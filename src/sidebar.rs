//! Compact project/session tree shown beside the active agent.
//!
//! The sidebar is a viewer, not an owner. Folders and sessions still live on
//! their hosts, and every agent still lives in its detached tmux session.
//! Layout rows arrange local attach clients without taking ownership of them.

use anyhow::Result;
use ratatui::DefaultTerminal;
use ratatui::crossterm::event::{
    self, DisableFocusChange, DisableMouseCapture, EnableFocusChange, EnableMouseCapture, Event,
    KeyCode, KeyEventKind, MouseButton, MouseEventKind,
};
use ratatui::crossterm::execute;
use ratatui::layout::{Constraint, Layout};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{
    Block, Borders, Clear, HighlightSpacing, List, ListItem, ListState, Paragraph,
};
use std::collections::HashSet;
use std::io::stdout;
use std::sync::mpsc::{Receiver, Sender, channel};
use std::time::{Duration, Instant};
use unicode_width::UnicodeWidthStr;
use uuid::Uuid;

use crate::hostops::SpawnResult;
use crate::model::{AgentKind, Folder, Host, Layout as SavedLayout, Probe, Session};
use crate::reconcile::State;
use crate::remote::{self, HostProbe};
use crate::store::LocalStore;
use crate::tmux;
use crate::tui::actions;
use crate::util;

const REFRESH_EVERY: Duration = Duration::from_secs(4);
const MESSAGE_FOR: Duration = Duration::from_secs(5);
const DASH_WINDOW: &str = "bzk-dash";
// Catppuccin Latte keeps the Neovim feel in a light palette. Every cell gets
// an explicit background so stale tmux contents cannot show through redraws.
const BG: Color = Color::Rgb(239, 241, 245);
const SURFACE: Color = Color::Rgb(230, 233, 239);
const SELECTED: Color = Color::Rgb(220, 224, 232);
const FOCUSED_SELECTED: Color = Color::Rgb(216, 204, 255);
const FG: Color = Color::Rgb(76, 79, 105);
const DIM: Color = Color::Rgb(140, 143, 161);
const ACCENT: Color = Color::Rgb(136, 57, 239);
const RED: Color = Color::Rgb(210, 15, 57);
const GREEN: Color = Color::Rgb(79, 122, 91);
const YELLOW: Color = Color::Rgb(223, 142, 29);
const BLUE: Color = Color::Rgb(30, 102, 245);
const BORDER: Color = Color::Rgb(188, 192, 204);

#[derive(Clone, Debug)]
enum TreeRow {
    Project {
        host: String,
        folder: Uuid,
        path: String,
        name: String,
        attention: usize,
        collapsed: bool,
        pinned: bool,
        hidden: bool,
    },
    Session {
        host: String,
        id: Uuid,
        agent: AgentKind,
        title: String,
        state: State,
        depth: u8,
    },
    NewSession {
        host: String,
        folder: Uuid,
        project: String,
    },
    Layout {
        id: Uuid,
        name: String,
        panes: usize,
        missing: usize,
        collapsed: bool,
        depth: u8,
    },
    LayoutProject {
        layout: Uuid,
        host: String,
        folder: Uuid,
        name: String,
        collapsed: bool,
        depth: u8,
    },
    LayoutSession {
        layout: Uuid,
        host: String,
        id: Uuid,
        folder: Option<Uuid>,
        agent: Option<AgentKind>,
        title: String,
        state: State,
        missing: bool,
        last: bool,
        occurrence: usize,
        depth: u8,
    },
    Section(String),
    Gap,
    Note(String),
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
enum TreeKey {
    Project(String, Uuid),
    Session(String, Uuid),
    NewSession(String, Uuid),
    Layout(Uuid),
    LayoutProject(Uuid, String, Uuid),
    LayoutSession(Uuid, String, Uuid, usize),
}

impl TreeRow {
    fn key(&self) -> Option<TreeKey> {
        match self {
            Self::Project { host, folder, .. } => Some(TreeKey::Project(host.clone(), *folder)),
            Self::Session { host, id, .. } => Some(TreeKey::Session(host.clone(), *id)),
            Self::NewSession { host, folder, .. } => {
                Some(TreeKey::NewSession(host.clone(), *folder))
            }
            Self::Layout { id, .. } => Some(TreeKey::Layout(*id)),
            Self::LayoutProject {
                layout,
                host,
                folder,
                ..
            } => Some(TreeKey::LayoutProject(*layout, host.clone(), *folder)),
            Self::LayoutSession {
                layout,
                host,
                id,
                occurrence,
                ..
            } => Some(TreeKey::LayoutSession(
                *layout,
                host.clone(),
                *id,
                *occurrence,
            )),
            Self::Section(_) | Self::Gap | Self::Note(_) => None,
        }
    }

    fn matches_key(&self, key: &TreeKey) -> bool {
        self.key().as_ref() == Some(key)
    }

    fn depth(&self) -> Option<u8> {
        match self {
            Self::Project { .. } => Some(0),
            Self::Session { depth, .. }
            | Self::Layout { depth, .. }
            | Self::LayoutProject { depth, .. }
            | Self::LayoutSession { depth, .. } => Some(*depth),
            Self::NewSession { .. } => Some(1),
            Self::Section(_) | Self::Gap | Self::Note(_) => None,
        }
    }
}

struct LaunchResult {
    host: Host,
    focus_sidebar: bool,
    standalone: bool,
    result: Result<SpawnResult, String>,
}

struct LayoutLaunchResult {
    layout: SavedLayout,
    focus: Option<(String, Uuid)>,
    results: Vec<(Host, Result<SpawnResult, String>)>,
}

struct CreateResult {
    host: Host,
    result: Result<Session, String>,
}

#[derive(Clone)]
struct SessionTarget {
    host: String,
    id: Uuid,
    title: String,
}

#[derive(Clone)]
struct ProjectTarget {
    host: String,
    folder: Uuid,
    path: String,
    name: String,
    pinned: bool,
    hidden: bool,
}

#[derive(Clone)]
struct LayoutTarget {
    id: Uuid,
    name: String,
    panes: usize,
}

enum SessionOverlay {
    Menu {
        target: SessionTarget,
        selected: usize,
    },
    Rename {
        target: SessionTarget,
        value: String,
    },
    RenameProject {
        target: ProjectTarget,
        value: String,
    },
    RenameLayout {
        target: LayoutTarget,
        value: String,
    },
    SaveLayout {
        value: String,
    },
    ProjectMenu {
        target: ProjectTarget,
        selected: usize,
    },
    ConfirmDelete {
        target: SessionTarget,
    },
    ConfirmDeleteProject {
        target: ProjectTarget,
    },
    ConfirmDeleteLayout {
        target: LayoutTarget,
    },
    ConfirmRemoveFromLayout {
        layout: LayoutTarget,
        target: SessionTarget,
    },
}

enum Mutation {
    Rename,
    Delete { was_active: bool },
}

struct MutationResult {
    target: SessionTarget,
    mutation: Mutation,
    result: Result<(), String>,
}

enum ProjectMutation {
    Rename(String),
    Pin(bool),
    Hide(bool),
    Delete,
}

struct ProjectMutationResult {
    mutation: ProjectMutation,
    result: Result<(), String>,
}

#[derive(Clone)]
struct AgentPicker {
    host: String,
    folder: Uuid,
    project: String,
    selected: usize,
}

#[derive(Clone)]
struct LayoutPicker {
    target: SessionTarget,
    layouts: Vec<LayoutTarget>,
    selected: usize,
}

struct Sidebar {
    local: LocalStore,
    probes: Vec<HostProbe>,
    rows: Vec<TreeRow>,
    list: ListState,
    active: Option<actions::ActiveWorkspace>,
    focused: bool,
    collapsed: HashSet<TreeKey>,
    show_hidden: bool,
    agent_picker: Option<AgentPicker>,
    layout_picker: Option<LayoutPicker>,
    session_overlay: Option<SessionOverlay>,
    refreshing: bool,
    starting: bool,
    creating: bool,
    mutating: bool,
    restoring: bool,
    last_refresh: Instant,
    message: Option<(String, bool, Instant)>,
    probe_tx: Sender<Vec<HostProbe>>,
    probe_rx: Receiver<Vec<HostProbe>>,
    launch_tx: Sender<LaunchResult>,
    launch_rx: Receiver<LaunchResult>,
    layout_launch_tx: Sender<LayoutLaunchResult>,
    layout_launch_rx: Receiver<LayoutLaunchResult>,
    create_tx: Sender<CreateResult>,
    create_rx: Receiver<CreateResult>,
    mutation_tx: Sender<MutationResult>,
    mutation_rx: Receiver<MutationResult>,
    project_mutation_tx: Sender<ProjectMutationResult>,
    project_mutation_rx: Receiver<ProjectMutationResult>,
}

pub fn run() -> Result<()> {
    let mut sidebar = Sidebar::new()?;
    execute!(stdout(), EnableMouseCapture, EnableFocusChange)?;
    let mut terminal = ratatui::init();
    let result = sidebar.main_loop(&mut terminal);
    ratatui::restore();
    let _ = execute!(stdout(), DisableFocusChange, DisableMouseCapture);
    result
}

impl Sidebar {
    fn new() -> Result<Self> {
        let mut local = LocalStore::load()?;
        if local.ensure_local_host() {
            local.save()?;
        }
        let (probe_tx, probe_rx) = channel();
        let (launch_tx, launch_rx) = channel();
        let (layout_launch_tx, layout_launch_rx) = channel();
        let (create_tx, create_rx) = channel();
        let (mutation_tx, mutation_rx) = channel();
        let (project_mutation_tx, project_mutation_rx) = channel();
        let now = Instant::now();
        let active = normalized_active_workspace(&local);
        let mut sidebar = Self {
            local,
            probes: Vec::new(),
            rows: vec![TreeRow::Note("loading…".into())],
            list: ListState::default(),
            active,
            focused: tmux::current_pane_active(),
            collapsed: HashSet::new(),
            show_hidden: false,
            agent_picker: None,
            layout_picker: None,
            session_overlay: None,
            refreshing: false,
            starting: false,
            creating: false,
            mutating: false,
            restoring: false,
            last_refresh: now.checked_sub(REFRESH_EVERY).unwrap_or(now),
            message: None,
            probe_tx,
            probe_rx,
            launch_tx,
            launch_rx,
            layout_launch_tx,
            layout_launch_rx,
            create_tx,
            create_rx,
            mutation_tx,
            mutation_rx,
            project_mutation_tx,
            project_mutation_rx,
        };
        sidebar.kick_refresh();
        Ok(sidebar)
    }

    fn main_loop(&mut self, terminal: &mut DefaultTerminal) -> Result<()> {
        loop {
            self.active = normalized_active_workspace(&self.local);
            self.follow_active_session();
            terminal.draw(|frame| draw(frame, self))?;

            if event::poll(Duration::from_millis(150))? {
                let event = event::read()?;
                match event {
                    Event::FocusGained => {
                        self.focused = true;
                        continue;
                    }
                    Event::FocusLost => {
                        self.focused = false;
                        continue;
                    }
                    _ => {}
                }
                if self.session_overlay.is_some() {
                    self.handle_session_overlay(event);
                } else if self.layout_picker.is_some() {
                    self.handle_layout_picker(event);
                } else if self.agent_picker.is_some() {
                    self.handle_agent_picker(event);
                } else {
                    match event {
                        Event::Key(key) if key.kind == KeyEventKind::Press => match key.code {
                            KeyCode::Up | KeyCode::Char('k' | 'л') => self.move_cursor(-1),
                            KeyCode::Down | KeyCode::Char('j' | 'о') => self.move_cursor(1),
                            KeyCode::Left | KeyCode::Char('h' | 'р') => self.collapse_or_parent(),
                            KeyCode::Right | KeyCode::Char('l' | 'д') => self.expand_or_open(),
                            KeyCode::Home | KeyCode::Char('g' | 'п') => self.jump(true),
                            KeyCode::End | KeyCode::Char('G' | 'П') => self.jump(false),
                            KeyCode::Enter => self.open_selected(),
                            KeyCode::Char('o' | 'щ') => self.open_selected_standalone(),
                            KeyCode::Char('n' | 'т') => self.begin_new_session(),
                            KeyCode::Char('a' | 'ф') => self.begin_add_to_layout(),
                            KeyCode::Char('S' | 'Ы') => {
                                self.session_overlay = Some(SessionOverlay::SaveLayout {
                                    value: String::new(),
                                });
                            }
                            KeyCode::Char('e' | 'у') => self.begin_rename(),
                            KeyCode::Char('d' | 'в') | KeyCode::Delete => self.begin_delete(),
                            KeyCode::Char('p' | 'з') => self.toggle_project_pinned(),
                            KeyCode::Char('H' | 'Р') => self.toggle_project_hidden(),
                            KeyCode::Char('v' | 'м') => self.toggle_hidden_visibility(),
                            KeyCode::Char('r' | 'к') => self.kick_refresh(),
                            KeyCode::Char('q' | 'й') | KeyCode::Esc => {
                                if let Some(window) = tmux::find_window(DASH_WINDOW) {
                                    let _ = tmux::select_window(&window);
                                }
                            }
                            _ => {}
                        },
                        Event::Mouse(mouse)
                            if matches!(mouse.kind, MouseEventKind::Down(MouseButton::Left))
                                && mouse.row >= 2 =>
                        {
                            let index = self.row_at(mouse.row);
                            if self.rows.get(index).is_some_and(|row| row.key().is_some()) {
                                self.list.select(Some(index));
                                self.open_selected();
                            }
                        }
                        Event::Mouse(mouse)
                            if matches!(mouse.kind, MouseEventKind::Down(MouseButton::Right))
                                && mouse.row >= 2 =>
                        {
                            let index = self.row_at(mouse.row);
                            self.show_context_menu(index);
                        }
                        Event::Mouse(mouse) if matches!(mouse.kind, MouseEventKind::ScrollUp) => {
                            self.move_cursor(-1);
                        }
                        Event::Mouse(mouse) if matches!(mouse.kind, MouseEventKind::ScrollDown) => {
                            self.move_cursor(1);
                        }
                        _ => {}
                    }
                }
            }

            while let Ok(probes) = self.probe_rx.try_recv() {
                self.probes = probes;
                self.refreshing = false;
                self.last_refresh = Instant::now();
                self.rebuild();
            }

            while let Ok(launch) = self.launch_rx.try_recv() {
                self.starting = false;
                match launch.result {
                    Ok(spawned) => {
                        let opened = if launch.standalone {
                            actions::open_standalone(
                                &launch.host,
                                spawned.session.id,
                                &spawned.tmux_name,
                            )
                        } else {
                            actions::open_pane(&launch.host, spawned.session.id, &spawned.tmux_name)
                        };
                        if launch.focus_sidebar {
                            self.focus_sidebar();
                        }
                        match opened {
                            Ok(_) => self.set_message("switched", false),
                            Err(error) => self.set_message(format!("{error:#}"), true),
                        }
                        self.kick_refresh();
                    }
                    Err(error) => {
                        if launch.focus_sidebar {
                            self.focus_sidebar();
                        }
                        self.set_message(error, true);
                    }
                }
            }

            while let Ok(batch) = self.layout_launch_rx.try_recv() {
                self.restoring = false;
                let mut opened = 0usize;
                let mut failed = 0usize;
                if let Err(error) = actions::retain_layout_panes(&batch.layout.panes) {
                    failed += 1;
                    self.set_message(format!("{error:#}"), true);
                }
                for (host, result) in &batch.results {
                    match result {
                        Ok(spawned) => {
                            match actions::open_pane(host, spawned.session.id, &spawned.tmux_name) {
                                Ok(_) => opened += 1,
                                Err(error) => {
                                    failed += 1;
                                    self.set_message(format!("{error:#}"), true);
                                }
                            }
                        }
                        Err(error) => {
                            failed += 1;
                            self.set_message(error.clone(), true);
                        }
                    }
                }
                if opened > 0 {
                    if batch
                        .layout
                        .tmux_layout
                        .as_deref()
                        .is_none_or(|geometry| actions::apply_geometry(geometry).is_err())
                    {
                        actions::tile();
                    }
                    let active_layout = actions::workspace_has_exact_panes(&batch.layout.panes)
                        .then_some(batch.layout.id);
                    if let Err(error) = actions::set_active_layout(active_layout) {
                        failed += 1;
                        self.set_message(format!("{error:#}"), true);
                    }
                    if let Some((host, session)) = &batch.focus {
                        let _ = actions::focus_open_session(host, *session);
                    } else {
                        let _ = actions::focus_work();
                    }
                }
                if failed == 0 {
                    self.set_message(
                        format!("opened {} · {opened} panes", batch.layout.name),
                        false,
                    );
                } else {
                    self.set_message(format!("{opened} opened · {failed} failed"), true);
                }
                self.kick_refresh();
            }

            while let Ok(created) = self.create_rx.try_recv() {
                self.creating = false;
                match created.result {
                    Ok(session) => {
                        self.set_message("session created", false);
                        self.open(created.host, session.id);
                        self.kick_refresh();
                    }
                    Err(error) => self.set_message(error, true),
                }
            }

            while let Ok(mutation) = self.mutation_rx.try_recv() {
                self.mutating = false;
                let mut focus_sidebar = true;
                match mutation.result {
                    Ok(()) => {
                        let was_delete = matches!(&mutation.mutation, Mutation::Delete { .. });
                        let replace_deleted = match mutation.mutation {
                            Mutation::Rename => false,
                            Mutation::Delete { was_active } => {
                                was_active
                                    || self.active.as_ref().is_some_and(|active| {
                                        active.host == mutation.target.host
                                            && active.session == mutation.target.id
                                    })
                            }
                        };
                        if replace_deleted
                            && let Some((host, session)) =
                                self.rows.iter().find_map(|row| match row {
                                    TreeRow::Session { host, id, .. }
                                        if host != &mutation.target.host
                                            || id != &mutation.target.id =>
                                    {
                                        Some((host.clone(), *id))
                                    }
                                    _ => None,
                                })
                        {
                            focus_sidebar = !self.open_named_with_focus(&host, session, true);
                        }
                        self.set_message(
                            if was_delete {
                                format!("deleted {}", mutation.target.title)
                            } else {
                                "session renamed".to_string()
                            },
                            false,
                        );
                        self.kick_refresh();
                    }
                    Err(error) => self.set_message(error, true),
                }
                if focus_sidebar {
                    self.focus_sidebar();
                }
            }

            while let Ok(mutation) = self.project_mutation_rx.try_recv() {
                self.mutating = false;
                match mutation.result {
                    Ok(()) => {
                        self.set_message(
                            match mutation.mutation {
                                ProjectMutation::Rename(_) => "project renamed",
                                ProjectMutation::Pin(true) => "project pinned",
                                ProjectMutation::Pin(false) => "project unpinned",
                                ProjectMutation::Hide(true) => "project hidden",
                                ProjectMutation::Hide(false) => "project restored",
                                ProjectMutation::Delete => "project deleted",
                            },
                            false,
                        );
                        self.kick_refresh();
                    }
                    Err(error) => self.set_message(error, true),
                }
            }

            if !self.refreshing && self.last_refresh.elapsed() >= REFRESH_EVERY {
                self.kick_refresh();
            }
            if self
                .message
                .as_ref()
                .is_some_and(|(_, _, at)| at.elapsed() > MESSAGE_FOR)
            {
                self.message = None;
            }
        }
    }

    fn hosts(&self) -> Vec<Host> {
        self.local.live_hosts().into_iter().cloned().collect()
    }

    fn kick_refresh(&mut self) {
        if self.refreshing {
            return;
        }
        self.refreshing = true;
        self.last_refresh = Instant::now();
        let hosts = self.hosts();
        let tx = self.probe_tx.clone();
        std::thread::spawn(move || {
            let _ = tx.send(remote::probe_all(&hosts));
        });
    }

    fn rebuild(&mut self) {
        if let Ok(local) = LocalStore::load() {
            self.local = local;
        }
        let selected_position = self
            .list
            .selected()
            .and_then(|selected| selectable_position(&self.rows, selected));
        let selected = self.current_key().or_else(|| {
            self.active.clone().map(|active| match active.layout {
                Some(layout) => TreeKey::LayoutSession(layout, active.host, active.session, 0),
                None => TreeKey::Session(active.host, active.session),
            })
        });
        self.rows = build_tree(
            &self.hosts(),
            &self.probes,
            &self
                .local
                .live_layouts()
                .into_iter()
                .cloned()
                .collect::<Vec<_>>(),
            &self.collapsed,
            self.show_hidden,
        );
        let target = selection_after_rebuild(&self.rows, selected.as_ref(), selected_position);
        self.list.select(target);
    }

    fn current_key(&self) -> Option<TreeKey> {
        self.list
            .selected()
            .and_then(|index| self.rows.get(index))
            .and_then(TreeRow::key)
    }

    /// Outside the sidebar, its cursor represents what the viewer is showing.
    /// Once focus returns, navigation is deliberately independent again so the
    /// user can browse the tree without moving the active session.
    fn follow_active_session(&mut self) {
        if self.focused {
            return;
        }
        self.list
            .select(active_session_selection(&self.rows, self.active.as_ref()));
    }

    fn row_at(&self, mouse_row: u16) -> usize {
        self.list
            .offset()
            .saturating_add(usize::from(mouse_row.saturating_sub(2)))
    }

    fn move_cursor(&mut self, delta: isize) {
        let selectable: Vec<usize> = self
            .rows
            .iter()
            .enumerate()
            .filter_map(|(index, row)| row.key().map(|_| index))
            .collect();
        if selectable.is_empty() {
            return;
        }
        let at = self
            .list
            .selected()
            .and_then(|selected| selectable.iter().position(|index| *index == selected))
            .unwrap_or(0);
        let next = at.saturating_add_signed(delta).min(selectable.len() - 1);
        self.list.select(Some(selectable[next]));
    }

    fn jump(&mut self, first: bool) {
        let mut selectable = self
            .rows
            .iter()
            .enumerate()
            .filter(|(_, row)| row.key().is_some());
        let target = if first {
            selectable.next()
        } else {
            selectable.next_back()
        };
        self.list.select(target.map(|(index, _)| index));
    }

    /// Vim-style tree navigation: collapse a branch, or move to its immediate
    /// parent in the visible tree.
    fn collapse_or_parent(&mut self) {
        let Some(selected) = self.list.selected() else {
            return;
        };
        let Some(row) = self.rows.get(selected) else {
            return;
        };
        match row.key() {
            Some(
                key @ (TreeKey::Project(..) | TreeKey::Layout(..) | TreeKey::LayoutProject(..)),
            ) => {
                if self.collapsed.insert(key) {
                    self.rebuild();
                }
            }
            Some(_) => {
                let Some(depth) = row.depth() else {
                    return;
                };
                if let Some(parent) = self.rows[..selected].iter().rposition(|candidate| {
                    candidate
                        .depth()
                        .is_some_and(|candidate_depth| candidate_depth < depth)
                }) {
                    self.list.select(Some(parent));
                }
            }
            None => {}
        }
    }

    /// Vim-style tree navigation: expand a branch, descend into an expanded
    /// branch, or open the selected leaf.
    fn expand_or_open(&mut self) {
        let Some(key) = self.current_key() else {
            return;
        };
        match key {
            key @ (TreeKey::Project(..) | TreeKey::Layout(..) | TreeKey::LayoutProject(..)) => {
                if self.collapsed.remove(&key) {
                    self.rebuild();
                } else {
                    self.move_cursor(1);
                }
            }
            TreeKey::Session(..) | TreeKey::NewSession(..) | TreeKey::LayoutSession(..) => {
                self.open_selected()
            }
        }
    }

    /// Rename the selected project's display label or the selected session.
    /// Neither operation renames a directory on disk.
    fn begin_rename(&mut self) {
        if self.mutating {
            return;
        }
        let Some(index) = self.list.selected() else {
            return;
        };
        self.session_overlay = match self.rows.get(index) {
            Some(TreeRow::Session {
                host, id, title, ..
            })
            | Some(TreeRow::LayoutSession {
                host, id, title, ..
            }) => Some(SessionOverlay::Rename {
                target: SessionTarget {
                    host: host.clone(),
                    id: *id,
                    title: title.clone(),
                },
                value: String::new(),
            }),
            Some(TreeRow::Layout {
                id, name, panes, ..
            }) => Some(SessionOverlay::RenameLayout {
                target: LayoutTarget {
                    id: *id,
                    name: name.clone(),
                    panes: *panes,
                },
                value: String::new(),
            }),
            Some(TreeRow::Project {
                host,
                folder,
                path,
                name,
                pinned,
                hidden,
                ..
            }) => Some(SessionOverlay::RenameProject {
                target: ProjectTarget {
                    host: host.clone(),
                    folder: *folder,
                    path: path.clone(),
                    name: name.clone(),
                    pinned: *pinned,
                    hidden: *hidden,
                },
                value: String::new(),
            }),
            _ => {
                self.set_message("select a project or session to rename", true);
                None
            }
        };
    }

    /// Delete the selected bizik session record after confirmation. The
    /// agent's conversation remains in its own transcript store.
    fn begin_delete(&mut self) {
        if self.mutating {
            return;
        }
        let Some(index) = self.list.selected() else {
            return;
        };
        self.session_overlay = match self.rows.get(index) {
            Some(TreeRow::Session {
                host, id, title, ..
            }) => Some(SessionOverlay::ConfirmDelete {
                target: SessionTarget {
                    host: host.clone(),
                    id: *id,
                    title: title.clone(),
                },
            }),
            Some(TreeRow::LayoutSession {
                layout,
                host,
                id,
                title,
                ..
            }) => {
                let layout = self
                    .local
                    .live_layouts()
                    .into_iter()
                    .find(|candidate| candidate.id == *layout)
                    .map(|candidate| LayoutTarget {
                        id: candidate.id,
                        name: candidate.name.clone(),
                        panes: candidate.panes.len(),
                    });
                layout.map(|layout| SessionOverlay::ConfirmRemoveFromLayout {
                    layout,
                    target: SessionTarget {
                        host: host.clone(),
                        id: *id,
                        title: title.clone(),
                    },
                })
            }
            Some(TreeRow::Layout {
                id, name, panes, ..
            }) => Some(SessionOverlay::ConfirmDeleteLayout {
                target: LayoutTarget {
                    id: *id,
                    name: name.clone(),
                    panes: *panes,
                },
            }),
            Some(TreeRow::Project {
                host,
                folder,
                path,
                name,
                pinned,
                hidden,
                ..
            }) => Some(SessionOverlay::ConfirmDeleteProject {
                target: ProjectTarget {
                    host: host.clone(),
                    folder: *folder,
                    path: path.clone(),
                    name: name.clone(),
                    pinned: *pinned,
                    hidden: *hidden,
                },
            }),
            _ => {
                self.set_message("select a project or session to delete", true);
                None
            }
        };
    }

    fn selected_project(&self) -> Option<ProjectTarget> {
        let index = self.list.selected()?;
        let TreeRow::Project {
            host,
            folder,
            path,
            name,
            pinned,
            hidden,
            ..
        } = self.rows.get(index)?
        else {
            return None;
        };
        Some(ProjectTarget {
            host: host.clone(),
            folder: *folder,
            path: path.clone(),
            name: name.clone(),
            pinned: *pinned,
            hidden: *hidden,
        })
    }

    fn toggle_project_pinned(&mut self) {
        let Some(target) = self.selected_project() else {
            self.set_message("select a project to pin or unpin", true);
            return;
        };
        self.submit_project_mutation(target.clone(), ProjectMutation::Pin(!target.pinned));
    }

    fn toggle_project_hidden(&mut self) {
        let Some(target) = self.selected_project() else {
            self.set_message("select a project to hide or restore", true);
            return;
        };
        self.submit_project_mutation(target.clone(), ProjectMutation::Hide(!target.hidden));
    }

    fn toggle_hidden_visibility(&mut self) {
        self.show_hidden = !self.show_hidden;
        self.rebuild();
        self.set_message(
            if self.show_hidden {
                "hidden projects are visible"
            } else {
                "hidden projects are hidden"
            },
            false,
        );
    }

    fn begin_add_to_layout(&mut self) {
        let Some(index) = self.list.selected() else {
            return;
        };
        let target = match self.rows.get(index) {
            Some(TreeRow::Session {
                host, id, title, ..
            })
            | Some(TreeRow::LayoutSession {
                host, id, title, ..
            }) => SessionTarget {
                host: host.clone(),
                id: *id,
                title: title.clone(),
            },
            _ => {
                self.set_message("select a session to add to a layout", true);
                return;
            }
        };
        let layouts: Vec<LayoutTarget> = self
            .local
            .live_layouts()
            .into_iter()
            .map(|layout| LayoutTarget {
                id: layout.id,
                name: layout.name.clone(),
                panes: layout.panes.len(),
            })
            .collect();
        if layouts.is_empty() {
            self.set_message("no layouts yet · save one with S in the dashboard", true);
            return;
        }
        self.layout_picker = Some(LayoutPicker {
            target,
            layouts,
            selected: 0,
        });
    }

    /// Create in the selected project, including when one of that project's
    /// sessions currently owns the cursor.
    fn begin_new_session(&mut self) {
        if self.starting || self.creating || self.mutating {
            return;
        }
        let Some(selected) = self.list.selected() else {
            return;
        };
        let project = match self.rows.get(selected) {
            Some(TreeRow::Project {
                host, folder, name, ..
            }) => Some((host.clone(), *folder, name.clone())),
            Some(TreeRow::NewSession {
                host,
                folder,
                project,
            }) => Some((host.clone(), *folder, project.clone())),
            Some(TreeRow::Session { .. }) => self.rows[..selected].iter().rev().find_map(|row| {
                if let TreeRow::Project {
                    host, folder, name, ..
                } = row
                {
                    Some((host.clone(), *folder, name.clone()))
                } else {
                    None
                }
            }),
            Some(TreeRow::LayoutProject {
                host, folder, name, ..
            }) => Some((host.clone(), *folder, name.clone())),
            Some(TreeRow::LayoutSession {
                host,
                folder: Some(folder),
                ..
            }) => self
                .probes
                .iter()
                .find(|probed| probed.host.name == *host)
                .and_then(|probed| probed.probe.as_ref())
                .and_then(|probe| {
                    probe
                        .folders
                        .iter()
                        .find(|candidate| candidate.id == *folder)
                })
                .map(|project| (host.clone(), *folder, project.display_name())),
            Some(
                TreeRow::Layout { .. }
                | TreeRow::LayoutSession { .. }
                | TreeRow::Section(_)
                | TreeRow::Gap
                | TreeRow::Note(_),
            )
            | None => None,
        };
        let Some((host, folder, project)) = project else {
            self.set_message("select a project to create a session", true);
            return;
        };
        self.agent_picker = Some(AgentPicker {
            host,
            folder,
            project,
            selected: 0,
        });
    }

    fn open_selected(&mut self) {
        if self.starting || self.creating || self.mutating || self.restoring {
            return;
        }
        let Some(key) = self.current_key() else {
            return;
        };
        match key {
            TreeKey::Project(host, folder) => {
                let key = TreeKey::Project(host, folder);
                if !self.collapsed.remove(&key) {
                    self.collapsed.insert(key);
                }
                self.rebuild();
            }
            TreeKey::Layout(id) => self.open_layout(id),
            TreeKey::LayoutProject(layout, host, folder) => {
                let key = TreeKey::LayoutProject(layout, host, folder);
                if !self.collapsed.remove(&key) {
                    self.collapsed.insert(key);
                }
                self.rebuild();
            }
            TreeKey::NewSession(host, folder) => {
                let project = self
                    .rows
                    .iter()
                    .find_map(|row| match row {
                        TreeRow::NewSession {
                            host: row_host,
                            folder: row_folder,
                            project,
                        } if row_host == &host && row_folder == &folder => Some(project.clone()),
                        _ => None,
                    })
                    .unwrap_or_else(|| "project".to_string());
                self.agent_picker = Some(AgentPicker {
                    host,
                    folder,
                    project,
                    selected: 0,
                });
            }
            TreeKey::Session(host, session) => self.open_named(&host, session),
            TreeKey::LayoutSession(layout, host, session, _) => {
                let saved = self
                    .local
                    .live_layouts()
                    .into_iter()
                    .find(|saved| saved.id == layout)
                    .cloned();
                if let Some(saved) = saved
                    && actions::workspace_has_exact_panes(&saved.panes)
                {
                    match actions::focus_open_session(&host, session) {
                        Ok(true) => self.set_message("switched", false),
                        Ok(false) => self.open_layout_with_focus(layout, Some((host, session))),
                        Err(error) => self.set_message(format!("{error:#}"), true),
                    }
                } else {
                    self.open_layout_with_focus(layout, Some((host, session)));
                }
            }
        }
    }

    fn open_layout(&mut self, id: Uuid) {
        self.open_layout_with_focus(id, None);
    }

    fn open_layout_with_focus(&mut self, id: Uuid, focus: Option<(String, Uuid)>) {
        let Some(layout) = self
            .local
            .live_layouts()
            .into_iter()
            .find(|layout| layout.id == id)
            .cloned()
        else {
            self.set_message("that layout is gone", true);
            return;
        };
        let mut jobs = Vec::new();
        for pane in &layout.panes {
            let Some(host) = self.local.host_by_name(&pane.host).cloned() else {
                self.set_message(format!("unknown host {}", pane.host), true);
                return;
            };
            jobs.push((host, pane.session));
        }
        if jobs.is_empty() {
            self.set_message("that layout has no sessions", true);
            return;
        }

        self.restoring = true;
        let tx = self.layout_launch_tx.clone();
        std::thread::spawn(move || {
            let results = jobs
                .into_iter()
                .map(|(host, session)| {
                    let result =
                        actions::start(&host, session).map_err(|error| format!("{error:#}"));
                    (host, result)
                })
                .collect();
            let _ = tx.send(LayoutLaunchResult {
                layout,
                focus,
                results,
            });
        });
    }

    fn open_selected_standalone(&mut self) {
        if self.starting || self.creating || self.mutating || self.restoring {
            return;
        }
        match self.current_key() {
            Some(TreeKey::Session(host, session))
            | Some(TreeKey::LayoutSession(_, host, session, _)) => {
                let Some(host) = self.local.host_by_name(&host).cloned() else {
                    self.set_message(format!("unknown host {host}"), true);
                    return;
                };
                self.open_with_mode(host, session, false, true);
            }
            _ => self.set_message("select a session to open standalone", true),
        }
    }

    fn open_named(&mut self, host_name: &str, session: Uuid) {
        self.open_named_with_focus(host_name, session, false);
    }

    fn open_named_with_focus(
        &mut self,
        host_name: &str,
        session: Uuid,
        focus_sidebar: bool,
    ) -> bool {
        let Some(host) = self.local.host_by_name(host_name).cloned() else {
            self.set_message(format!("unknown host {host_name}"), true);
            return false;
        };
        self.open_with_mode(host, session, focus_sidebar, true);
        true
    }

    fn open(&mut self, host: Host, session: Uuid) {
        self.open_with_mode(host, session, false, true);
    }

    fn open_with_mode(&mut self, host: Host, session: Uuid, focus_sidebar: bool, standalone: bool) {
        self.starting = true;
        let tx = self.launch_tx.clone();
        std::thread::spawn(move || {
            let result = actions::start(&host, session).map_err(|error| format!("{error:#}"));
            let _ = tx.send(LaunchResult {
                host,
                focus_sidebar,
                standalone,
                result,
            });
        });
    }

    fn focus_sidebar(&mut self) {
        if actions::focus_sidebar().is_ok() {
            self.focused = true;
        }
    }

    fn handle_agent_picker(&mut self, event: Event) {
        let Some(mut picker) = self.agent_picker.take() else {
            return;
        };
        match event {
            Event::Key(key) if key.kind == KeyEventKind::Press => match key.code {
                KeyCode::Esc => {}
                KeyCode::Up | KeyCode::Char('k' | 'л') => {
                    picker.selected = picker.selected.saturating_sub(1);
                    self.agent_picker = Some(picker);
                }
                KeyCode::Down | KeyCode::Char('j' | 'о') => {
                    picker.selected = (picker.selected + 1).min(2);
                    self.agent_picker = Some(picker);
                }
                KeyCode::Char('1') => {
                    picker.selected = 0;
                    self.create_from_picker(picker);
                }
                KeyCode::Char('2') => {
                    picker.selected = 1;
                    self.create_from_picker(picker);
                }
                KeyCode::Char('3') => {
                    picker.selected = 2;
                    self.create_from_picker(picker);
                }
                KeyCode::Enter => self.create_from_picker(picker),
                _ => self.agent_picker = Some(picker),
            },
            Event::Mouse(mouse)
                if matches!(mouse.kind, MouseEventKind::Down(MouseButton::Left)) =>
            {
                if (3..=5).contains(&mouse.row) {
                    picker.selected = usize::from(mouse.row - 3);
                    self.create_from_picker(picker);
                } else {
                    self.agent_picker = Some(picker);
                }
            }
            _ => self.agent_picker = Some(picker),
        }
    }

    fn handle_layout_picker(&mut self, event: Event) {
        let Some(mut picker) = self.layout_picker.take() else {
            return;
        };
        match event {
            Event::Key(key) if key.kind == KeyEventKind::Press => match key.code {
                KeyCode::Esc => {}
                KeyCode::Up | KeyCode::Char('k' | 'л') => {
                    picker.selected = picker.selected.saturating_sub(1);
                    self.layout_picker = Some(picker);
                }
                KeyCode::Down | KeyCode::Char('j' | 'о') => {
                    picker.selected =
                        (picker.selected + 1).min(picker.layouts.len().saturating_sub(1));
                    self.layout_picker = Some(picker);
                }
                KeyCode::Enter => self.add_from_layout_picker(picker),
                _ => self.layout_picker = Some(picker),
            },
            _ => self.layout_picker = Some(picker),
        }
    }

    fn add_from_layout_picker(&mut self, picker: LayoutPicker) {
        let Some(layout) = picker.layouts.get(picker.selected) else {
            return;
        };
        let pane = crate::model::PaneRef {
            host: picker.target.host,
            session: picker.target.id,
        };
        match self.local.add_layout_pane(&layout.id.to_string(), pane) {
            Ok(changed) => match self.local.save() {
                Ok(()) => {
                    self.set_message(
                        if changed {
                            format!("added to {}", layout.name)
                        } else {
                            format!("already in {}", layout.name)
                        },
                        false,
                    );
                    self.rebuild();
                }
                Err(error) => self.set_message(format!("{error:#}"), true),
            },
            Err(error) => self.set_message(format!("{error:#}"), true),
        }
    }

    fn create_from_picker(&mut self, picker: AgentPicker) {
        let Some(host) = self.local.host_by_name(&picker.host).cloned() else {
            self.set_message(format!("unknown host {}", picker.host), true);
            return;
        };
        let agent = [AgentKind::Codex, AgentKind::Claude, AgentKind::Shell][picker.selected];
        self.creating = true;
        let tx = self.create_tx.clone();
        std::thread::spawn(move || {
            let result = actions::create_session(&host, picker.folder, agent, None, None)
                .map_err(|error| format!("{error:#}"));
            let _ = tx.send(CreateResult { host, result });
        });
    }

    fn show_context_menu(&mut self, index: usize) {
        if self.mutating {
            return;
        }
        let overlay = match self.rows.get(index) {
            Some(TreeRow::Session {
                host, id, title, ..
            }) => SessionOverlay::Menu {
                target: SessionTarget {
                    host: host.clone(),
                    id: *id,
                    title: title.clone(),
                },
                selected: 0,
            },
            Some(TreeRow::Project {
                host,
                folder,
                path,
                name,
                pinned,
                hidden,
                ..
            }) => SessionOverlay::ProjectMenu {
                target: ProjectTarget {
                    host: host.clone(),
                    folder: *folder,
                    path: path.clone(),
                    name: name.clone(),
                    pinned: *pinned,
                    hidden: *hidden,
                },
                selected: 0,
            },
            _ => return,
        };
        self.list.select(Some(index));
        self.session_overlay = Some(overlay);
    }

    fn handle_session_overlay(&mut self, event: Event) {
        let Some(overlay) = self.session_overlay.take() else {
            return;
        };
        match overlay {
            SessionOverlay::Menu {
                target,
                mut selected,
            } => match event {
                Event::Key(key) if key.kind == KeyEventKind::Press => match key.code {
                    KeyCode::Esc => {}
                    KeyCode::Up | KeyCode::Char('k' | 'л') => {
                        selected = selected.saturating_sub(1);
                        self.session_overlay = Some(SessionOverlay::Menu { target, selected });
                    }
                    KeyCode::Down | KeyCode::Char('j' | 'о') => {
                        selected = (selected + 1).min(2);
                        self.session_overlay = Some(SessionOverlay::Menu { target, selected });
                    }
                    KeyCode::Char('e' | 'у') => self.choose_session_menu(target, 0),
                    KeyCode::Char('d' | 'в') | KeyCode::Delete => {
                        self.choose_session_menu(target, 1);
                    }
                    KeyCode::Enter => self.choose_session_menu(target, selected),
                    _ => {
                        self.session_overlay = Some(SessionOverlay::Menu { target, selected });
                    }
                },
                Event::Mouse(mouse)
                    if matches!(mouse.kind, MouseEventKind::Down(MouseButton::Left)) =>
                {
                    match mouse.row {
                        3 => self.choose_session_menu(target, 0),
                        4 => self.choose_session_menu(target, 1),
                        _ => {}
                    }
                }
                Event::Mouse(mouse)
                    if matches!(mouse.kind, MouseEventKind::Down(MouseButton::Right)) => {}
                _ => {
                    self.session_overlay = Some(SessionOverlay::Menu { target, selected });
                }
            },
            SessionOverlay::ProjectMenu {
                target,
                mut selected,
            } => match event {
                Event::Key(key) if key.kind == KeyEventKind::Press => match key.code {
                    KeyCode::Esc => {}
                    KeyCode::Up | KeyCode::Char('k' | 'л') => {
                        selected = selected.saturating_sub(1);
                        self.session_overlay =
                            Some(SessionOverlay::ProjectMenu { target, selected });
                    }
                    KeyCode::Down | KeyCode::Char('j' | 'о') => {
                        selected = (selected + 1).min(4);
                        self.session_overlay =
                            Some(SessionOverlay::ProjectMenu { target, selected });
                    }
                    KeyCode::Char('e' | 'у') => self.choose_project_menu(target, 0),
                    KeyCode::Char('p' | 'з') => self.choose_project_menu(target, 1),
                    KeyCode::Char('h' | 'H' | 'р' | 'Р') => {
                        self.choose_project_menu(target, 2);
                    }
                    KeyCode::Char('d' | 'в') | KeyCode::Delete => {
                        self.choose_project_menu(target, 3);
                    }
                    KeyCode::Enter => self.choose_project_menu(target, selected),
                    _ => {
                        self.session_overlay =
                            Some(SessionOverlay::ProjectMenu { target, selected });
                    }
                },
                Event::Mouse(mouse)
                    if matches!(mouse.kind, MouseEventKind::Down(MouseButton::Left)) =>
                {
                    match mouse.row {
                        3 => self.choose_project_menu(target, 0),
                        4 => self.choose_project_menu(target, 1),
                        5 => self.choose_project_menu(target, 2),
                        6 => self.choose_project_menu(target, 3),
                        _ => {}
                    }
                }
                Event::Mouse(mouse)
                    if matches!(mouse.kind, MouseEventKind::Down(MouseButton::Right)) => {}
                _ => {
                    self.session_overlay = Some(SessionOverlay::ProjectMenu { target, selected });
                }
            },
            SessionOverlay::Rename { target, mut value } => match event {
                Event::Key(key) if key.kind == KeyEventKind::Press => match key.code {
                    KeyCode::Esc => {}
                    KeyCode::Enter => self.submit_rename(target, &value),
                    KeyCode::Backspace => {
                        value.pop();
                        self.session_overlay = Some(SessionOverlay::Rename { target, value });
                    }
                    KeyCode::Char(character) if value.chars().count() < 256 => {
                        value.push(character);
                        self.session_overlay = Some(SessionOverlay::Rename { target, value });
                    }
                    _ => {
                        self.session_overlay = Some(SessionOverlay::Rename { target, value });
                    }
                },
                Event::Paste(text) => {
                    value.extend(
                        text.chars()
                            .filter(|character| !character.is_control())
                            .take(256usize.saturating_sub(value.chars().count())),
                    );
                    self.session_overlay = Some(SessionOverlay::Rename { target, value });
                }
                _ => {
                    self.session_overlay = Some(SessionOverlay::Rename { target, value });
                }
            },
            SessionOverlay::RenameProject { target, mut value } => match event {
                Event::Key(key) if key.kind == KeyEventKind::Press => match key.code {
                    KeyCode::Esc => {}
                    KeyCode::Enter => self.submit_project_rename(target, &value),
                    KeyCode::Backspace => {
                        value.pop();
                        self.session_overlay =
                            Some(SessionOverlay::RenameProject { target, value });
                    }
                    KeyCode::Char(character) if value.chars().count() < 256 => {
                        value.push(character);
                        self.session_overlay =
                            Some(SessionOverlay::RenameProject { target, value });
                    }
                    _ => {
                        self.session_overlay =
                            Some(SessionOverlay::RenameProject { target, value });
                    }
                },
                Event::Paste(text) => {
                    value.extend(
                        text.chars()
                            .filter(|character| !character.is_control())
                            .take(256usize.saturating_sub(value.chars().count())),
                    );
                    self.session_overlay = Some(SessionOverlay::RenameProject { target, value });
                }
                _ => {
                    self.session_overlay = Some(SessionOverlay::RenameProject { target, value });
                }
            },
            SessionOverlay::RenameLayout { target, mut value } => match event {
                Event::Key(key) if key.kind == KeyEventKind::Press => match key.code {
                    KeyCode::Esc => {}
                    KeyCode::Enter => self.submit_layout_rename(target, &value),
                    KeyCode::Backspace => {
                        value.pop();
                        self.session_overlay = Some(SessionOverlay::RenameLayout { target, value });
                    }
                    KeyCode::Char(character) if value.chars().count() < 256 => {
                        value.push(character);
                        self.session_overlay = Some(SessionOverlay::RenameLayout { target, value });
                    }
                    _ => {
                        self.session_overlay = Some(SessionOverlay::RenameLayout { target, value });
                    }
                },
                Event::Paste(text) => {
                    value.extend(
                        text.chars()
                            .filter(|character| !character.is_control())
                            .take(256usize.saturating_sub(value.chars().count())),
                    );
                    self.session_overlay = Some(SessionOverlay::RenameLayout { target, value });
                }
                _ => {
                    self.session_overlay = Some(SessionOverlay::RenameLayout { target, value });
                }
            },
            SessionOverlay::SaveLayout { mut value } => match event {
                Event::Key(key) if key.kind == KeyEventKind::Press => match key.code {
                    KeyCode::Esc => {}
                    KeyCode::Enter => self.submit_save_layout(&value),
                    KeyCode::Backspace => {
                        value.pop();
                        self.session_overlay = Some(SessionOverlay::SaveLayout { value });
                    }
                    KeyCode::Char(character) if value.chars().count() < 256 => {
                        value.push(character);
                        self.session_overlay = Some(SessionOverlay::SaveLayout { value });
                    }
                    _ => {
                        self.session_overlay = Some(SessionOverlay::SaveLayout { value });
                    }
                },
                Event::Paste(text) => {
                    value.extend(
                        text.chars()
                            .filter(|character| !character.is_control())
                            .take(256usize.saturating_sub(value.chars().count())),
                    );
                    self.session_overlay = Some(SessionOverlay::SaveLayout { value });
                }
                _ => {
                    self.session_overlay = Some(SessionOverlay::SaveLayout { value });
                }
            },
            SessionOverlay::ConfirmDelete { target } => match event {
                Event::Key(key) if key.kind == KeyEventKind::Press => match key.code {
                    KeyCode::Enter | KeyCode::Char('y' | 'Y' | 'н' | 'Н') => {
                        self.submit_delete(target);
                    }
                    KeyCode::Esc | KeyCode::Char('n' | 'N' | 'т' | 'Т') => {}
                    _ => {
                        self.session_overlay = Some(SessionOverlay::ConfirmDelete { target });
                    }
                },
                Event::Mouse(mouse)
                    if matches!(mouse.kind, MouseEventKind::Down(MouseButton::Left)) =>
                {
                    if mouse.row == 4 && mouse.column < 12 {
                        self.submit_delete(target);
                    }
                }
                _ => {
                    self.session_overlay = Some(SessionOverlay::ConfirmDelete { target });
                }
            },
            SessionOverlay::ConfirmDeleteProject { target } => match event {
                Event::Key(key) if key.kind == KeyEventKind::Press => match key.code {
                    KeyCode::Enter | KeyCode::Char('y' | 'Y' | 'н' | 'Н') => {
                        self.submit_project_mutation(target, ProjectMutation::Delete);
                    }
                    KeyCode::Esc | KeyCode::Char('n' | 'N' | 'т' | 'Т') => {}
                    _ => {
                        self.session_overlay =
                            Some(SessionOverlay::ConfirmDeleteProject { target });
                    }
                },
                Event::Mouse(mouse)
                    if matches!(mouse.kind, MouseEventKind::Down(MouseButton::Left)) =>
                {
                    if mouse.row == 6 && mouse.column < 12 {
                        self.submit_project_mutation(target, ProjectMutation::Delete);
                    }
                }
                _ => {
                    self.session_overlay = Some(SessionOverlay::ConfirmDeleteProject { target });
                }
            },
            SessionOverlay::ConfirmDeleteLayout { target } => match event {
                Event::Key(key) if key.kind == KeyEventKind::Press => match key.code {
                    KeyCode::Enter | KeyCode::Char('y' | 'Y' | 'н' | 'Н') => {
                        self.submit_layout_delete(target);
                    }
                    KeyCode::Esc | KeyCode::Char('n' | 'N' | 'т' | 'Т') => {}
                    _ => {
                        self.session_overlay = Some(SessionOverlay::ConfirmDeleteLayout { target });
                    }
                },
                _ => {
                    self.session_overlay = Some(SessionOverlay::ConfirmDeleteLayout { target });
                }
            },
            SessionOverlay::ConfirmRemoveFromLayout { layout, target } => match event {
                Event::Key(key) if key.kind == KeyEventKind::Press => match key.code {
                    KeyCode::Enter | KeyCode::Char('y' | 'Y' | 'н' | 'Н') => {
                        self.submit_remove_from_layout(layout, target);
                    }
                    KeyCode::Esc | KeyCode::Char('n' | 'N' | 'т' | 'Т') => {}
                    _ => {
                        self.session_overlay =
                            Some(SessionOverlay::ConfirmRemoveFromLayout { layout, target });
                    }
                },
                _ => {
                    self.session_overlay =
                        Some(SessionOverlay::ConfirmRemoveFromLayout { layout, target });
                }
            },
        }
    }

    fn choose_session_menu(&mut self, target: SessionTarget, selected: usize) {
        self.session_overlay = match selected {
            0 => Some(SessionOverlay::Rename {
                value: String::new(),
                target,
            }),
            1 => Some(SessionOverlay::ConfirmDelete { target }),
            _ => None,
        };
    }

    fn choose_project_menu(&mut self, target: ProjectTarget, selected: usize) {
        match selected {
            0 => {
                self.session_overlay = Some(SessionOverlay::RenameProject {
                    value: String::new(),
                    target,
                });
            }
            1 => {
                let pinned = !target.pinned;
                self.submit_project_mutation(target, ProjectMutation::Pin(pinned));
            }
            2 => {
                let hidden = !target.hidden;
                self.submit_project_mutation(target, ProjectMutation::Hide(hidden));
            }
            3 => {
                self.session_overlay = Some(SessionOverlay::ConfirmDeleteProject { target });
            }
            _ => {}
        }
    }

    fn submit_rename(&mut self, target: SessionTarget, value: &str) {
        let title = value.trim().to_string();
        if title.is_empty() {
            self.set_message("session name cannot be empty", true);
            self.session_overlay = Some(SessionOverlay::Rename {
                target,
                value: value.to_string(),
            });
            return;
        }
        let Some(host) = self.local.host_by_name(&target.host).cloned() else {
            self.set_message(format!("unknown host {}", target.host), true);
            return;
        };
        self.mutating = true;
        let tx = self.mutation_tx.clone();
        std::thread::spawn(move || {
            let result =
                actions::rename_session(&host, target.id, &title).map_err(|e| format!("{e:#}"));
            let _ = tx.send(MutationResult {
                target,
                mutation: Mutation::Rename,
                result,
            });
        });
    }

    fn submit_project_rename(&mut self, target: ProjectTarget, value: &str) {
        let label = value.trim().to_string();
        if label.is_empty() {
            self.set_message("project name cannot be empty", true);
            self.session_overlay = Some(SessionOverlay::RenameProject {
                target,
                value: value.to_string(),
            });
            return;
        }
        self.submit_project_mutation(target, ProjectMutation::Rename(label));
    }

    fn submit_layout_rename(&mut self, target: LayoutTarget, value: &str) {
        let name = value.trim().to_string();
        if name.is_empty() {
            self.set_message("layout name cannot be empty", true);
            self.session_overlay = Some(SessionOverlay::RenameLayout {
                target,
                value: value.to_string(),
            });
            return;
        }
        match self.local.rename_layout(&target.id.to_string(), name) {
            Ok(_) => match self.local.save() {
                Ok(()) => {
                    self.set_message("layout renamed", false);
                    self.rebuild();
                }
                Err(error) => self.set_message(format!("{error:#}"), true),
            },
            Err(error) => self.set_message(format!("{error:#}"), true),
        }
    }

    fn submit_save_layout(&mut self, value: &str) {
        let name = value.trim().to_string();
        if name.is_empty() {
            self.set_message("layout name cannot be empty", true);
            self.session_overlay = Some(SessionOverlay::SaveLayout {
                value: value.to_string(),
            });
            return;
        }
        let candidates: Vec<(String, Session)> = self
            .probes
            .iter()
            .filter_map(|probed| {
                probed
                    .probe
                    .as_ref()
                    .map(|probe| (probed.host.name.clone(), probe))
            })
            .flat_map(|(host, probe)| {
                probe
                    .records()
                    .map(|session| (host.clone(), session.clone()))
                    .collect::<Vec<_>>()
            })
            .collect();
        let panes: Vec<crate::model::PaneRef> = actions::panes_in_work(&candidates)
            .into_iter()
            .map(|pane| crate::model::PaneRef {
                host: pane.host,
                session: pane.session,
            })
            .collect();
        if panes.is_empty() {
            self.set_message("no session panes are open", true);
            return;
        }
        let count = panes.len();
        match self
            .local
            .create_layout(name.clone(), panes, actions::work_layout())
        {
            Ok(_) => match self.local.save() {
                Ok(()) => {
                    self.set_message(format!("saved {name} · {count} panes"), false);
                    self.rebuild();
                }
                Err(error) => self.set_message(format!("{error:#}"), true),
            },
            Err(error) => self.set_message(format!("{error:#}"), true),
        }
    }

    fn submit_layout_delete(&mut self, target: LayoutTarget) {
        self.local.remove_layout(target.id);
        match self.local.save() {
            Ok(()) => {
                self.set_message(
                    format!(
                        "layout deleted · {} sessions were not stopped",
                        target.panes
                    ),
                    false,
                );
                self.rebuild();
            }
            Err(error) => self.set_message(format!("{error:#}"), true),
        }
    }

    fn submit_remove_from_layout(&mut self, layout: LayoutTarget, target: SessionTarget) {
        let pane = crate::model::PaneRef {
            host: target.host,
            session: target.id,
        };
        match self.local.remove_layout_pane(&layout.id.to_string(), &pane) {
            Ok(_) => match self.local.save() {
                Ok(()) => {
                    self.set_message(
                        format!("removed from {} · session kept", layout.name),
                        false,
                    );
                    self.rebuild();
                }
                Err(error) => self.set_message(format!("{error:#}"), true),
            },
            Err(error) => self.set_message(format!("{error:#}"), true),
        }
    }

    fn submit_project_mutation(&mut self, target: ProjectTarget, mutation: ProjectMutation) {
        if self.mutating {
            return;
        }
        let Some(host) = self.local.host_by_name(&target.host).cloned() else {
            self.set_message(format!("unknown host {}", target.host), true);
            return;
        };
        self.mutating = true;
        let tx = self.project_mutation_tx.clone();
        std::thread::spawn(move || {
            let result = match &mutation {
                ProjectMutation::Rename(label) => actions::relabel(&host, &target.path, label),
                ProjectMutation::Pin(pinned) => {
                    actions::set_project_pinned(&host, target.folder, *pinned)
                }
                ProjectMutation::Hide(hidden) => {
                    actions::set_project_hidden(&host, target.folder, *hidden)
                }
                ProjectMutation::Delete => actions::unmark(&host, &target.path),
            }
            .map_err(|error| format!("{error:#}"));
            let _ = tx.send(ProjectMutationResult { mutation, result });
        });
    }

    fn submit_delete(&mut self, target: SessionTarget) {
        let Some(host) = self.local.host_by_name(&target.host).cloned() else {
            self.set_message(format!("unknown host {}", target.host), true);
            return;
        };
        let was_active = self
            .active
            .as_ref()
            .is_some_and(|active| active.host == target.host && active.session == target.id);
        self.mutating = true;
        let tx = self.mutation_tx.clone();
        std::thread::spawn(move || {
            let result = actions::forget(&host, target.id).map_err(|e| format!("{e:#}"));
            let _ = tx.send(MutationResult {
                target,
                mutation: Mutation::Delete { was_active },
                result,
            });
        });
    }

    fn set_message(&mut self, text: impl Into<String>, error: bool) {
        self.message = Some((text.into(), error, Instant::now()));
    }
}

fn selectable_position(rows: &[TreeRow], selected: usize) -> Option<usize> {
    rows.iter()
        .enumerate()
        .filter(|(_, row)| row.key().is_some())
        .position(|(index, _)| index == selected)
}

/// Preserve identity across ordinary refreshes. If the selected row vanished,
/// keep its ordinal position so deletion lands on the following row, or the
/// preceding row when the deleted row was last.
fn selection_after_rebuild(
    rows: &[TreeRow],
    preferred: Option<&TreeKey>,
    previous_position: Option<usize>,
) -> Option<usize> {
    if let Some(index) = preferred.and_then(|key| rows.iter().position(|row| row.matches_key(key)))
    {
        return Some(index);
    }

    let selectable: Vec<usize> = rows
        .iter()
        .enumerate()
        .filter_map(|(index, row)| row.key().map(|_| index))
        .collect();
    previous_position
        .and_then(|position| selectable.get(position).or_else(|| selectable.last()))
        .copied()
        .or_else(|| selectable.first().copied())
}

#[derive(Clone)]
struct ResolvedLayoutSession {
    host: String,
    folder: Uuid,
    project: String,
    id: Uuid,
    agent: AgentKind,
    title: String,
    state: State,
}

struct LayoutProjectGroup {
    host: String,
    folder: Uuid,
    project: String,
    sessions: Vec<(usize, ResolvedLayoutSession)>,
}

fn resolve_layout_session(
    host: &str,
    session: Uuid,
    probes: &[HostProbe],
) -> Option<ResolvedLayoutSession> {
    let probe = probes
        .iter()
        .find(|probed| probed.host.name == host)?
        .probe
        .as_ref()?;
    let view = probe
        .sessions
        .iter()
        .find(|view| view.session.id == session)?;
    let folder = probe
        .folders
        .iter()
        .find(|folder| folder.id == view.session.folder_id)?;
    Some(ResolvedLayoutSession {
        host: host.to_string(),
        folder: folder.id,
        project: folder.display_name(),
        id: session,
        agent: view.session.agent,
        title: view.session.title.clone(),
        state: view.state,
    })
}

/// A layout is shown inside a project only while every pane resolves to that
/// same project. Adding a pane from another project therefore promotes it to
/// the global section without changing the persisted layout record.
fn layout_project(layout: &SavedLayout, probes: &[HostProbe]) -> Option<(String, Uuid)> {
    let mut owner: Option<(String, Uuid)> = None;
    for pane in &layout.panes {
        let resolved = resolve_layout_session(&pane.host, pane.session, probes)?;
        let candidate = (resolved.host, resolved.folder);
        if owner.as_ref().is_some_and(|current| current != &candidate) {
            return None;
        }
        owner = Some(candidate);
    }
    owner
}

fn append_layout(
    rows: &mut Vec<TreeRow>,
    layout: &SavedLayout,
    depth: u8,
    global: bool,
    probes: &[HostProbe],
    collapsed: &HashSet<TreeKey>,
) {
    let key = TreeKey::Layout(layout.id);
    let is_collapsed = collapsed.contains(&key);
    let resolved: Vec<Option<ResolvedLayoutSession>> = layout
        .panes
        .iter()
        .map(|pane| resolve_layout_session(&pane.host, pane.session, probes))
        .collect();
    rows.push(TreeRow::Layout {
        id: layout.id,
        name: layout.name.clone(),
        panes: layout.panes.len(),
        missing: resolved.iter().filter(|session| session.is_none()).count(),
        collapsed: is_collapsed,
        depth,
    });
    if is_collapsed {
        return;
    }

    if !global {
        let last = layout.panes.len().saturating_sub(1);
        for (occurrence, (pane, session)) in
            layout.panes.iter().zip(resolved.into_iter()).enumerate()
        {
            rows.push(layout_session_row(
                layout.id,
                pane,
                session,
                occurrence,
                depth.saturating_add(1),
                occurrence == last,
            ));
        }
        return;
    }

    let mut groups: Vec<LayoutProjectGroup> = Vec::new();
    let mut missing = Vec::new();
    for (occurrence, (pane, session)) in layout.panes.iter().zip(resolved.into_iter()).enumerate() {
        let Some(session) = session else {
            missing.push((occurrence, pane));
            continue;
        };
        let key = (session.host.clone(), session.folder);
        match groups
            .iter_mut()
            .find(|group| (group.host.as_str(), group.folder) == (key.0.as_str(), key.1))
        {
            Some(group) => group.sessions.push((occurrence, session)),
            None => groups.push(LayoutProjectGroup {
                host: key.0,
                folder: key.1,
                project: session.project.clone(),
                sessions: vec![(occurrence, session)],
            }),
        }
    }

    let layout_children = groups.len() + missing.len();
    let mut child_index = 0usize;
    for group in groups {
        let key = TreeKey::LayoutProject(layout.id, group.host.clone(), group.folder);
        let project_collapsed = collapsed.contains(&key);
        rows.push(TreeRow::LayoutProject {
            layout: layout.id,
            host: group.host,
            folder: group.folder,
            name: group.project,
            collapsed: project_collapsed,
            depth: depth.saturating_add(1),
        });
        child_index += 1;
        if !project_collapsed {
            let last = group.sessions.len().saturating_sub(1);
            rows.extend(group.sessions.into_iter().enumerate().map(
                |(index, (occurrence, session))| TreeRow::LayoutSession {
                    layout: layout.id,
                    host: session.host,
                    id: session.id,
                    folder: Some(session.folder),
                    agent: Some(session.agent),
                    title: session.title,
                    state: session.state,
                    missing: false,
                    last: index == last,
                    occurrence,
                    depth: depth.saturating_add(2),
                },
            ));
        }
    }
    rows.extend(missing.into_iter().map(|(occurrence, pane)| {
        let last = child_index + 1 == layout_children;
        child_index += 1;
        layout_session_row(
            layout.id,
            pane,
            None,
            occurrence,
            depth.saturating_add(1),
            last,
        )
    }));
}

fn layout_session_row(
    layout: Uuid,
    pane: &crate::model::PaneRef,
    session: Option<ResolvedLayoutSession>,
    occurrence: usize,
    depth: u8,
    last: bool,
) -> TreeRow {
    match session {
        Some(session) => TreeRow::LayoutSession {
            layout,
            host: session.host,
            id: session.id,
            folder: Some(session.folder),
            agent: Some(session.agent),
            title: session.title,
            state: session.state,
            missing: false,
            last,
            occurrence,
            depth,
        },
        None => TreeRow::LayoutSession {
            layout,
            host: pane.host.clone(),
            id: pane.session,
            folder: None,
            agent: None,
            title: format!("missing {}", pane.session),
            state: State::Down,
            missing: true,
            last,
            occurrence,
            depth,
        },
    }
}

fn build_tree(
    hosts: &[Host],
    probes: &[HostProbe],
    layouts: &[SavedLayout],
    collapsed: &HashSet<TreeKey>,
    show_hidden: bool,
) -> Vec<TreeRow> {
    let mut rows = Vec::new();
    let mut projects: Vec<(&Host, &Folder, &Probe, usize, usize)> = Vec::new();
    let mut unavailable = Vec::new();

    for (host_index, host) in hosts.iter().enumerate() {
        let Some(probed) = probes.iter().find(|probe| probe.host.name == host.name) else {
            continue;
        };
        let Some(probe) = &probed.probe else {
            unavailable.push(TreeRow::Note(format!("{} offline", host.name)));
            continue;
        };
        projects.extend(
            probe
                .folders
                .iter()
                .enumerate()
                .filter(|(_, folder)| show_hidden || !folder.hidden)
                .map(|(folder_index, folder)| (host, folder, probe, host_index, folder_index)),
        );
    }

    projects.sort_by_key(|(_, folder, _, host_index, folder_index)| {
        (
            folder.hidden,
            std::cmp::Reverse(folder.pinned),
            *host_index,
            *folder_index,
        )
    });

    for (project_index, (host, folder, probe, _, _)) in projects.into_iter().enumerate() {
        if project_index > 0 {
            rows.push(TreeRow::Gap);
        }
        let mut sessions: Vec<_> = probe
            .sessions
            .iter()
            .filter(|view| view.session.folder_id == folder.id)
            .collect();
        sessions.sort_by_key(|view| view.session.created_at);
        let project_key = TreeKey::Project(host.name.clone(), folder.id);
        let is_collapsed = collapsed.contains(&project_key);
        rows.push(TreeRow::Project {
            host: host.name.clone(),
            folder: folder.id,
            path: folder.path.clone(),
            name: folder.display_name(),
            attention: sessions
                .iter()
                .filter(|view| view.state.wants_you())
                .count(),
            collapsed: is_collapsed,
            pinned: folder.pinned,
            hidden: folder.hidden,
        });
        if !is_collapsed {
            rows.push(TreeRow::NewSession {
                host: host.name.clone(),
                folder: folder.id,
                project: folder.display_name(),
            });
            for layout in layouts.iter().filter(|layout| {
                layout_project(layout, probes).as_ref() == Some(&(host.name.clone(), folder.id))
            }) {
                append_layout(&mut rows, layout, 1, false, probes, collapsed);
            }
            rows.extend(sessions.into_iter().map(|view| TreeRow::Session {
                host: host.name.clone(),
                id: view.session.id,
                agent: view.session.agent,
                title: view.session.title.clone(),
                state: view.state,
                depth: 1,
            }));
        }
    }
    rows.extend(unavailable);

    let global: Vec<_> = layouts
        .iter()
        .filter(|layout| layout_project(layout, probes).is_none())
        .collect();
    if !global.is_empty() {
        if !rows.is_empty() {
            rows.push(TreeRow::Gap);
        }
        rows.push(TreeRow::Section("LAYOUTS".into()));
        for layout in global {
            append_layout(&mut rows, layout, 0, true, probes, collapsed);
        }
    }
    if rows.is_empty() {
        rows.push(TreeRow::Note("no marked projects or layouts".into()));
    }
    rows
}

fn normalized_active_workspace(local: &LocalStore) -> Option<actions::ActiveWorkspace> {
    let mut active = actions::active_workspace()?;
    if active.layout.is_some_and(|id| {
        !local
            .live_layouts()
            .into_iter()
            .any(|layout| layout.id == id)
    }) {
        active.layout = None;
    }
    Some(active)
}

fn active_session_selection(
    rows: &[TreeRow],
    active: Option<&actions::ActiveWorkspace>,
) -> Option<usize> {
    let active = active?;
    rows.iter().position(|row| match row {
        TreeRow::Session { host, id, .. } => {
            active.layout.is_none() && active.host == *host && active.session == *id
        }
        TreeRow::LayoutSession {
            layout, host, id, ..
        } => active.layout == Some(*layout) && active.host == *host && active.session == *id,
        _ => false,
    })
}

fn draw(frame: &mut ratatui::Frame, sidebar: &mut Sidebar) {
    // Clear first, then paint every cell. This prevents remnants of the
    // previous tmux client from surviving a resize or session switch.
    frame.render_widget(Clear, frame.area());
    frame.render_widget(
        Block::default().style(Style::default().fg(FG).bg(BG)),
        frame.area(),
    );

    let [header, body, footer] = Layout::vertical([
        Constraint::Length(2),
        Constraint::Min(3),
        Constraint::Length(2),
    ])
    .areas(frame.area());

    let activity = if sidebar.mutating {
        "  saving…"
    } else if sidebar.restoring {
        "  restoring layout…"
    } else if sidebar.creating {
        "  creating…"
    } else if sidebar.starting {
        "  starting…"
    } else if sidebar.refreshing {
        "  refreshing…"
    } else {
        ""
    };
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(
                " PROJECTS",
                Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
            ),
            Span::styled(activity, Style::default().fg(DIM)),
        ]))
        .style(Style::default().fg(FG).bg(SURFACE))
        .block(
            Block::default()
                .borders(Borders::BOTTOM)
                .border_style(Style::default().fg(BORDER)),
        ),
        header,
    );

    let width = usize::from(body.width.saturating_sub(2));
    if let Some(overlay) = &sidebar.session_overlay {
        draw_session_overlay(frame, body, overlay);
    } else if let Some(picker) = &sidebar.layout_picker {
        draw_layout_picker(frame, body, picker);
    } else if let Some(picker) = &sidebar.agent_picker {
        draw_agent_picker(frame, body, picker);
    } else {
        let items: Vec<ListItem> = sidebar
            .rows
            .iter()
            .map(|row| render_row(row, width))
            .collect();
        let list = List::new(items)
            .style(Style::default().fg(FG).bg(BG))
            .highlight_spacing(HighlightSpacing::Never)
            .highlight_style(selection_style(sidebar.focused));
        frame.render_stateful_widget(list, body, &mut sidebar.list);
    }

    let footer_line = match &sidebar.message {
        Some((text, error, _)) => Line::from(Span::styled(
            format!(" {}", util::one_line(text, width.max(4))),
            if *error {
                Style::default()
                    .fg(RED)
                    .bg(SURFACE)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default()
                    .fg(GREEN)
                    .bg(SURFACE)
                    .add_modifier(Modifier::BOLD)
            },
        )),
        None => Line::from(Span::styled(
            " enter open  S save  a add  o standalone  d remove",
            Style::default().fg(DIM).bg(SURFACE),
        )),
    };
    frame.render_widget(
        Paragraph::new(footer_line)
            .style(Style::default().fg(DIM).bg(SURFACE))
            .block(
                Block::default()
                    .borders(Borders::TOP)
                    .border_style(Style::default().fg(BORDER)),
            ),
        footer,
    );
}

fn selection_style(focused: bool) -> Style {
    Style::default()
        .fg(FG)
        .bg(if focused { FOCUSED_SELECTED } else { SELECTED })
        .add_modifier(Modifier::BOLD)
}

fn draw_session_overlay(
    frame: &mut ratatui::Frame,
    area: ratatui::layout::Rect,
    overlay: &SessionOverlay,
) {
    let lines = match overlay {
        SessionOverlay::Menu { target, selected } => vec![
            Line::styled(
                format!("  {}", util::one_line(&target.title, 24)),
                Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
            ),
            menu_line("  [E] Rename", *selected == 0),
            menu_line("  [D] Delete session", *selected == 1),
            menu_line("  [Esc] Cancel", *selected == 2),
            Line::styled("  Enter select · Esc close", Style::default().fg(DIM)),
        ],
        SessionOverlay::ProjectMenu { target, selected } => vec![
            Line::styled(
                format!("  {}", util::one_line(&target.name, 24)),
                Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
            ),
            menu_line("  [E] Rename", *selected == 0),
            menu_line(
                if target.pinned {
                    "  [P] Unpin"
                } else {
                    "  [P] Pin"
                },
                *selected == 1,
            ),
            menu_line(
                if target.hidden {
                    "  [H] Restore"
                } else {
                    "  [H] Hide"
                },
                *selected == 2,
            ),
            menu_line("  [D] Delete project", *selected == 3),
            menu_line("  [Esc] Cancel", *selected == 4),
        ],
        SessionOverlay::Rename { target, value } => vec![
            Line::styled(
                format!("  Rename {}", util::one_line(&target.title, 18)),
                Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
            ),
            Line::from(vec![
                Span::styled("  Name: ", Style::default().fg(DIM)),
                Span::styled(
                    util::one_line(value, usize::from(area.width.saturating_sub(8))),
                    Style::default().add_modifier(Modifier::BOLD),
                ),
            ]),
            Line::styled("  Enter save · Esc cancel", Style::default().fg(DIM)),
        ],
        SessionOverlay::RenameProject { target, value } => vec![
            Line::styled(
                format!("  Project {}", util::one_line(&target.name, 18)),
                Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
            ),
            Line::from(vec![
                Span::styled("  Label: ", Style::default().fg(DIM)),
                Span::styled(
                    util::one_line(value, usize::from(area.width.saturating_sub(9))),
                    Style::default().add_modifier(Modifier::BOLD),
                ),
            ]),
            Line::styled("  Folder path stays unchanged", Style::default().fg(DIM)),
            Line::styled("  Enter save · Esc cancel", Style::default().fg(DIM)),
        ],
        SessionOverlay::RenameLayout { target, value } => vec![
            Line::styled(
                format!("  Layout {}", util::one_line(&target.name, 18)),
                Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
            ),
            Line::from(vec![
                Span::styled("  Name: ", Style::default().fg(DIM)),
                Span::styled(
                    util::one_line(value, usize::from(area.width.saturating_sub(8))),
                    Style::default().add_modifier(Modifier::BOLD),
                ),
            ]),
            Line::styled("  Enter save · Esc cancel", Style::default().fg(DIM)),
        ],
        SessionOverlay::SaveLayout { value } => vec![
            Line::styled(
                "  Save current workspace",
                Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
            ),
            Line::from(vec![
                Span::styled("  Name: ", Style::default().fg(DIM)),
                Span::styled(
                    util::one_line(value, usize::from(area.width.saturating_sub(8))),
                    Style::default().add_modifier(Modifier::BOLD),
                ),
            ]),
            Line::styled("  Enter save · Esc cancel", Style::default().fg(DIM)),
        ],
        SessionOverlay::ConfirmDelete { target } => vec![
            Line::styled(
                format!("  Delete {}?", util::one_line(&target.title, 19)),
                Style::default().fg(RED).add_modifier(Modifier::BOLD),
            ),
            Line::styled("  Conversation stays on disk.", Style::default().fg(DIM)),
            Line::from(vec![
                Span::styled(
                    "  [Y] Delete ",
                    Style::default().fg(BG).bg(RED).add_modifier(Modifier::BOLD),
                ),
                Span::raw(" [N] Cancel "),
            ]),
        ],
        SessionOverlay::ConfirmDeleteProject { target } => vec![
            Line::styled(
                format!("  Delete {}?", util::one_line(&target.name, 19)),
                Style::default().fg(RED).add_modifier(Modifier::BOLD),
            ),
            Line::styled("  Sessions are forgotten.", Style::default().fg(DIM)),
            Line::styled("  Project files stay on disk.", Style::default().fg(DIM)),
            Line::from(vec![
                Span::styled(
                    "  [Y] Delete ",
                    Style::default().fg(BG).bg(RED).add_modifier(Modifier::BOLD),
                ),
                Span::raw(" [N] Cancel "),
            ]),
        ],
        SessionOverlay::ConfirmDeleteLayout { target } => vec![
            Line::styled(
                format!("  Delete {}?", util::one_line(&target.name, 19)),
                Style::default().fg(RED).add_modifier(Modifier::BOLD),
            ),
            Line::styled(
                format!("  Its {} sessions keep running.", target.panes),
                Style::default().fg(DIM),
            ),
            Line::from(vec![
                Span::styled(
                    "  [Y] Delete ",
                    Style::default().fg(BG).bg(RED).add_modifier(Modifier::BOLD),
                ),
                Span::raw(" [N] Cancel "),
            ]),
        ],
        SessionOverlay::ConfirmRemoveFromLayout { layout, target } => vec![
            Line::styled(
                format!("  Remove {}?", util::one_line(&target.title, 19)),
                Style::default().fg(RED).add_modifier(Modifier::BOLD),
            ),
            Line::styled(
                format!("  From {} only.", util::one_line(&layout.name, 21)),
                Style::default().fg(DIM),
            ),
            Line::styled("  The session keeps running.", Style::default().fg(DIM)),
            Line::from(vec![
                Span::styled(
                    "  [Y] Remove ",
                    Style::default().fg(BG).bg(RED).add_modifier(Modifier::BOLD),
                ),
                Span::raw(" [N] Cancel "),
            ]),
        ],
    };
    frame.render_widget(
        Paragraph::new(lines).style(Style::default().fg(FG).bg(BG)),
        area,
    );
}

fn menu_line(label: &str, selected: bool) -> Line<'static> {
    Line::styled(
        label.to_string(),
        if selected {
            Style::default()
                .fg(BG)
                .bg(ACCENT)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(FG).bg(BG)
        },
    )
}

fn render_row(row: &TreeRow, width: usize) -> ListItem<'static> {
    match row {
        TreeRow::Project {
            host,
            name,
            attention,
            collapsed,
            pinned,
            hidden,
            ..
        } => {
            let suffix = if *attention > 0 {
                format!("  !{attention}")
            } else {
                String::new()
            };
            let available = width.saturating_sub(suffix.chars().count() + 3);
            ListItem::new(Line::from(vec![
                Span::styled(
                    format!(
                        "{} {}{}{}",
                        disclosure_symbol(*collapsed),
                        if *pinned { "📌 " } else { "" },
                        util::one_line(name, available.max(1)),
                        if *hidden { "  hidden" } else { "" }
                    ),
                    Style::default().add_modifier(Modifier::BOLD),
                ),
                Span::styled(suffix, Style::default().fg(RED)),
                Span::styled(
                    if host == "local" {
                        String::new()
                    } else {
                        format!(" @{host}")
                    },
                    Style::default().fg(DIM),
                ),
            ]))
        }
        TreeRow::Session {
            agent,
            title,
            state,
            depth,
            ..
        } => {
            let label = session_label(*depth, None, *state, Some(*agent), title, width);
            ListItem::new(Line::from(Span::styled(label, state_style(*state))))
        }
        TreeRow::NewSession { .. } => ListItem::new(Line::from(Span::styled(
            "  + new session",
            Style::default().fg(ACCENT),
        ))),
        TreeRow::Layout {
            name,
            panes,
            missing,
            collapsed,
            depth,
            ..
        } => {
            let indent = tree_indent(*depth);
            let suffix = format!(
                "  {panes}{}",
                if *missing > 0 {
                    format!(" · {missing} missing")
                } else {
                    String::new()
                }
            );
            let available = width.saturating_sub(indent.len() + suffix.chars().count() + 2);
            ListItem::new(Line::from(vec![
                Span::styled(
                    format!(
                        "{indent}{} {}",
                        disclosure_symbol(*collapsed),
                        util::one_line(name, available.max(1))
                    ),
                    Style::default().fg(FG).add_modifier(Modifier::BOLD),
                ),
                Span::styled(
                    suffix,
                    if *missing > 0 {
                        Style::default().fg(RED)
                    } else {
                        Style::default().fg(DIM)
                    },
                ),
            ]))
        }
        TreeRow::LayoutProject {
            host,
            name,
            collapsed,
            depth,
            ..
        } => {
            let indent = tree_indent(*depth);
            let remote = if host == "local" {
                String::new()
            } else {
                format!(" @{host}")
            };
            let available = width.saturating_sub(indent.len() + remote.chars().count() + 3);
            ListItem::new(Line::from(vec![
                Span::styled(
                    format!(
                        "{indent}{} {}",
                        disclosure_symbol(*collapsed),
                        util::one_line(name, available.max(1))
                    ),
                    Style::default().add_modifier(Modifier::BOLD),
                ),
                Span::styled(remote, Style::default().fg(DIM)),
            ]))
        }
        TreeRow::LayoutSession {
            agent,
            title,
            state,
            missing,
            last,
            depth,
            ..
        } => {
            let label = session_label(*depth, Some(*last), *state, *agent, title, width);
            let style = if *missing {
                Style::default().fg(RED)
            } else {
                state_style(*state)
            };
            ListItem::new(Line::from(Span::styled(label, style)))
        }
        TreeRow::Section(title) => ListItem::new(Line::from(Span::styled(
            format!(" {title}"),
            Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
        ))),
        TreeRow::Gap => ListItem::new(Line::default()),
        TreeRow::Note(text) => ListItem::new(Line::from(Span::styled(
            util::one_line(text, width.max(1)),
            Style::default().fg(DIM),
        ))),
    }
}

fn tree_indent(depth: u8) -> String {
    "  ".repeat(usize::from(depth))
}

fn disclosure_symbol(collapsed: bool) -> &'static str {
    if collapsed { "›" } else { "⌄" }
}

fn session_label(
    depth: u8,
    last: Option<bool>,
    state: State,
    agent: Option<AgentKind>,
    title: &str,
    width: usize,
) -> String {
    let agent = match agent {
        Some(AgentKind::Claude) => "🧠",
        Some(AgentKind::Codex) => "🤖",
        Some(AgentKind::Shell) => "💻",
        None => "?",
    };
    let agent_width = UnicodeWidthStr::width(agent);
    let agent_cell = format!("{agent}{}", " ".repeat(2usize.saturating_sub(agent_width)));
    let indent = tree_indent(if last.is_some() {
        depth.saturating_sub(1)
    } else {
        depth
    });
    let prefix = match last {
        Some(true) => format!("{indent}└─ {} {agent_cell} ", state_symbol(state)),
        Some(false) => format!("{indent}├─ {} {agent_cell} ", state_symbol(state)),
        None => format!("{indent}{} {agent_cell} ", state_symbol(state)),
    };
    let available = width
        .saturating_sub(UnicodeWidthStr::width(prefix.as_str()))
        .max(1);
    format!("{prefix}{}", util::one_line(title, available))
}

fn draw_layout_picker(
    frame: &mut ratatui::Frame,
    area: ratatui::layout::Rect,
    picker: &LayoutPicker,
) {
    let mut lines = vec![Line::styled(
        format!("  Add {}", util::one_line(&picker.target.title, 20)),
        Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
    )];
    for (index, layout) in picker.layouts.iter().enumerate() {
        lines.push(Line::styled(
            format!("  ▦ {}  {}", util::one_line(&layout.name, 18), layout.panes),
            if picker.selected == index {
                Style::default()
                    .fg(BG)
                    .bg(ACCENT)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(FG).bg(BG)
            },
        ));
    }
    lines.push(Line::styled(
        "  Enter add · Esc cancel",
        Style::default().fg(DIM),
    ));
    frame.render_widget(
        Paragraph::new(lines).style(Style::default().fg(FG).bg(BG)),
        area,
    );
}

fn draw_agent_picker(
    frame: &mut ratatui::Frame,
    area: ratatui::layout::Rect,
    picker: &AgentPicker,
) {
    let mut lines = vec![Line::styled(
        format!("  New in {}", util::one_line(&picker.project, 19)),
        Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
    )];
    for (index, label) in ["[1] 🤖 Codex", "[2] 🧠 Claude", "[3] 💻 Shell"]
        .iter()
        .enumerate()
    {
        lines.push(Line::styled(
            format!("  {label}"),
            if picker.selected == index {
                Style::default()
                    .fg(BG)
                    .bg(ACCENT)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(FG).bg(BG)
            },
        ));
    }
    lines.push(Line::styled("  Esc cancel", Style::default().fg(DIM)));
    frame.render_widget(
        Paragraph::new(lines).style(Style::default().fg(FG).bg(BG)),
        area,
    );
}

fn state_style(state: State) -> Style {
    match state {
        State::NeedsYou | State::Exited => Style::default().fg(RED),
        State::Done | State::YourTurn => Style::default().fg(GREEN),
        State::Working => Style::default().fg(YELLOW),
        State::Up => Style::default().fg(BLUE),
        State::Down => Style::default().fg(DIM),
    }
}

fn state_symbol(state: State) -> &'static str {
    match state {
        State::NeedsYou => "!",
        State::Done => "✓",
        State::Working => "↻",
        State::YourTurn => "→",
        State::Up => "↑",
        State::Exited => "×",
        State::Down => "−",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Folder, Layout as SavedLayout, PaneRef, Probe, Session};
    use crate::reconcile::SessionView;

    fn session_row(id: Uuid, title: &str) -> TreeRow {
        TreeRow::Session {
            host: "local".into(),
            id,
            agent: AgentKind::Codex,
            title: title.into(),
            state: State::Down,
            depth: 1,
        }
    }

    #[test]
    fn deleted_selection_stays_at_its_position_or_moves_to_previous_row() {
        let first = Uuid::new_v4();
        let middle = Uuid::new_v4();
        let last = Uuid::new_v4();
        let before = vec![
            session_row(first, "first"),
            TreeRow::Gap,
            session_row(middle, "middle"),
            session_row(last, "last"),
        ];

        let middle_position =
            selectable_position(&before, 2).expect("middle session is selectable");
        let after_middle = vec![
            session_row(first, "first"),
            TreeRow::Gap,
            session_row(last, "last"),
        ];
        assert_eq!(
            selection_after_rebuild(
                &after_middle,
                Some(&TreeKey::Session("local".into(), middle)),
                Some(middle_position),
            ),
            Some(2),
            "the following session should occupy the deleted session's position",
        );

        let last_position = selectable_position(&before, 3).expect("last session is selectable");
        let after_last = vec![
            session_row(first, "first"),
            TreeRow::Gap,
            session_row(middle, "middle"),
        ];
        assert_eq!(
            selection_after_rebuild(
                &after_last,
                Some(&TreeKey::Session("local".into(), last)),
                Some(last_position),
            ),
            Some(2),
            "deleting the last session should move to the preceding session",
        );
    }

    #[test]
    fn selected_row_is_brighter_only_while_the_sidebar_is_focused() {
        assert_eq!(
            selection_style(true),
            Style::default()
                .fg(FG)
                .bg(FOCUSED_SELECTED)
                .add_modifier(Modifier::BOLD)
        );
        assert_eq!(
            selection_style(false),
            Style::default()
                .fg(FG)
                .bg(SELECTED)
                .add_modifier(Modifier::BOLD)
        );
    }

    #[test]
    fn inactive_sidebar_selection_follows_layout_or_standalone_context() {
        let layout = Uuid::new_v4();
        let active = Uuid::new_v4();
        let rows = vec![
            TreeRow::Note("offline".into()),
            TreeRow::Layout {
                id: layout,
                name: "workspace".into(),
                panes: 1,
                missing: 0,
                collapsed: false,
                depth: 1,
            },
            TreeRow::LayoutSession {
                layout,
                host: "server".into(),
                id: active,
                folder: None,
                agent: Some(AgentKind::Claude),
                title: "active in layout".into(),
                state: State::Working,
                missing: false,
                last: true,
                occurrence: 0,
                depth: 2,
            },
            TreeRow::Session {
                host: "server".into(),
                id: active,
                agent: AgentKind::Claude,
                title: "active standalone".into(),
                state: State::Working,
                depth: 1,
            },
        ];

        assert_eq!(
            active_session_selection(
                &rows,
                Some(&actions::ActiveWorkspace {
                    host: "server".into(),
                    session: active,
                    layout: Some(layout),
                }),
            ),
            Some(2)
        );
        assert_eq!(
            active_session_selection(
                &rows,
                Some(&actions::ActiveWorkspace {
                    host: "server".into(),
                    session: active,
                    layout: None,
                }),
            ),
            Some(3)
        );
        assert_eq!(
            active_session_selection(
                &rows,
                Some(&actions::ActiveWorkspace {
                    host: "local".into(),
                    session: active,
                    layout: Some(layout),
                }),
            ),
            None
        );
        assert_eq!(active_session_selection(&rows, None), None);
    }

    #[test]
    fn session_labels_use_tree_connectors_without_extra_active_markers() {
        assert_eq!(
            session_label(
                2,
                Some(false),
                State::Done,
                Some(AgentKind::Codex),
                "first",
                80,
            ),
            "  ├─ ✓ 🤖 first"
        );
        assert_eq!(
            session_label(
                2,
                Some(true),
                State::Done,
                Some(AgentKind::Codex),
                "last",
                80,
            ),
            "  └─ ✓ 🤖 last"
        );
        assert_eq!(
            session_label(
                1,
                None,
                State::Done,
                Some(AgentKind::Codex),
                "standalone",
                80,
            ),
            "  ✓ 🤖 standalone"
        );
    }

    #[test]
    fn disclosure_chevrons_share_one_cell() {
        assert_eq!(disclosure_symbol(true), "›");
        assert_eq!(disclosure_symbol(false), "⌄");
        assert_eq!(UnicodeWidthStr::width(disclosure_symbol(true)), 1);
        assert_eq!(UnicodeWidthStr::width(disclosure_symbol(false)), 1);
    }

    #[test]
    fn tree_groups_sessions_under_their_project() {
        let host = Host::new("local".into(), None);
        let folder = Folder::new("/work/project".into());
        let mut first = Session::new(folder.id, AgentKind::Codex, "session 1".into());
        first.created_at = 10;
        first.last_attached = Some(20);
        let mut second = Session::new(folder.id, AgentKind::Claude, "session 2".into());
        second.created_at = 20;
        second.last_attached = Some(10);
        let probe = HostProbe {
            host: host.clone(),
            probe: Some(Probe {
                folders: vec![folder],
                sessions: vec![
                    SessionView {
                        session: second,
                        state: State::Down,
                        preview: None,
                        attention: None,
                    },
                    SessionView {
                        session: first,
                        state: State::Working,
                        preview: None,
                        attention: None,
                    },
                ],
                ..Probe::default()
            }),
            error: None,
        };

        let rows = build_tree(
            std::slice::from_ref(&host),
            std::slice::from_ref(&probe),
            &[],
            &HashSet::new(),
            false,
        );
        assert!(matches!(
            &rows[0],
            TreeRow::Project { name, .. } if name == "project"
        ));
        assert!(matches!(
            &rows[1],
            TreeRow::NewSession { project, .. } if project == "project"
        ));
        assert!(matches!(
            &rows[2],
            TreeRow::Session { title, .. } if title == "session 1"
        ));
        assert!(matches!(
            &rows[3],
            TreeRow::Session { title, .. } if title == "session 2"
        ));

        let collapsed = HashSet::from([TreeKey::Project(
            "local".to_string(),
            match &rows[0] {
                TreeRow::Project { folder, .. } => *folder,
                _ => Uuid::nil(),
            },
        )]);
        let folded = build_tree(
            std::slice::from_ref(&host),
            std::slice::from_ref(&probe),
            &[],
            &collapsed,
            false,
        );
        assert_eq!(folded.len(), 1);
        assert!(matches!(
            &folded[0],
            TreeRow::Project {
                collapsed: true,
                ..
            }
        ));
    }

    #[test]
    fn project_layouts_are_inline_and_cross_project_layouts_form_a_global_tree() {
        let local = Host::new("local".into(), None);
        let back = Host::new("back".into(), Some("back.example".into()));
        let payments = Folder::new("/work/payments".into());
        let frontend = Folder::new("/work/frontend".into());
        let api = Session::new(payments.id, AgentKind::Codex, "deploy API".into());
        let tests = Session::new(payments.id, AgentKind::Claude, "smoke tests".into());
        let build = Session::new(frontend.id, AgentKind::Codex, "production build".into());
        let probes = vec![
            HostProbe {
                host: local.clone(),
                probe: Some(Probe {
                    folders: vec![payments.clone()],
                    sessions: vec![
                        SessionView {
                            session: api.clone(),
                            state: State::Working,
                            preview: None,
                            attention: None,
                        },
                        SessionView {
                            session: tests.clone(),
                            state: State::NeedsYou,
                            preview: None,
                            attention: None,
                        },
                    ],
                    ..Probe::default()
                }),
                error: None,
            },
            HostProbe {
                host: back.clone(),
                probe: Some(Probe {
                    folders: vec![frontend],
                    sessions: vec![SessionView {
                        session: build.clone(),
                        state: State::Up,
                        preview: None,
                        attention: None,
                    }],
                    ..Probe::default()
                }),
                error: None,
            },
        ];
        let project_layout = SavedLayout::new(
            "release prep".into(),
            vec![
                PaneRef {
                    host: "local".into(),
                    session: api.id,
                },
                PaneRef {
                    host: "local".into(),
                    session: tests.id,
                },
            ],
            None,
        );
        let global_layout = SavedLayout::new(
            "production release".into(),
            vec![
                PaneRef {
                    host: "local".into(),
                    session: api.id,
                },
                PaneRef {
                    host: "back".into(),
                    session: build.id,
                },
            ],
            None,
        );

        assert_eq!(
            layout_project(&project_layout, &probes),
            Some(("local".into(), payments.id))
        );
        assert_eq!(layout_project(&global_layout, &probes), None);

        let rows = build_tree(
            &[local, back],
            &probes,
            &[project_layout, global_layout],
            &HashSet::new(),
            false,
        );
        assert!(rows.iter().any(
            |row| matches!(row, TreeRow::Layout { name, depth: 1, .. } if name == "release prep")
        ));
        let section = rows
            .iter()
            .position(|row| matches!(row, TreeRow::Section(title) if title == "LAYOUTS"))
            .expect("global layouts section");
        assert!(rows[section + 1..].iter().any(
            |row| matches!(row, TreeRow::Layout { name, depth: 0, .. } if name == "production release")
        ));
        assert_eq!(
            rows[section + 1..]
                .iter()
                .filter(|row| matches!(row, TreeRow::LayoutProject { .. }))
                .count(),
            2
        );
    }

    #[test]
    fn tree_pins_projects_adds_air_and_hides_hidden_projects() {
        let host = Host::new("local".into(), None);
        let normal = Folder::new("/work/normal".into());
        let mut pinned = Folder::new("/work/pinned".into());
        pinned.pinned = true;
        let mut hidden = Folder::new("/work/hidden".into());
        hidden.hidden = true;
        let probe = HostProbe {
            host: host.clone(),
            probe: Some(Probe {
                folders: vec![normal, pinned, hidden],
                ..Probe::default()
            }),
            error: None,
        };

        let visible = build_tree(
            std::slice::from_ref(&host),
            std::slice::from_ref(&probe),
            &[],
            &HashSet::new(),
            false,
        );
        assert!(matches!(
            &visible[0],
            TreeRow::Project {
                name,
                pinned: true,
                ..
            } if name == "pinned"
        ));
        assert!(visible.iter().any(|row| matches!(row, TreeRow::Gap)));
        assert!(
            !visible
                .iter()
                .any(|row| matches!(row, TreeRow::Project { name, .. } if name == "hidden"))
        );

        let with_hidden = build_tree(
            std::slice::from_ref(&host),
            std::slice::from_ref(&probe),
            &[],
            &HashSet::new(),
            true,
        );
        assert!(with_hidden.iter().any(
            |row| matches!(row, TreeRow::Project { name, hidden: true, .. } if name == "hidden")
        ));
    }
}
