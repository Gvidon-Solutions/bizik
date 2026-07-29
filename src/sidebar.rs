//! Compact project/session tree shown beside the active agent.
//!
//! The sidebar is a viewer, not an owner. Folders and sessions still live on
//! their hosts, and the active agent still lives in its detached tmux session.
//! Selecting another row only replaces the attach client in the viewer pane.

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
use uuid::Uuid;

use crate::hostops::SpawnResult;
use crate::model::{AgentKind, Folder, Host, Probe, Session};
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
    },
    NewSession {
        host: String,
        folder: Uuid,
        project: String,
    },
    Gap,
    Note(String),
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum TreeKey {
    Project(String, Uuid),
    Session(String, Uuid),
    NewSession(String, Uuid),
}

impl TreeRow {
    fn key(&self) -> Option<TreeKey> {
        match self {
            Self::Project { host, folder, .. } => Some(TreeKey::Project(host.clone(), *folder)),
            Self::Session { host, id, .. } => Some(TreeKey::Session(host.clone(), *id)),
            Self::NewSession { host, folder, .. } => {
                Some(TreeKey::NewSession(host.clone(), *folder))
            }
            Self::Gap | Self::Note(_) => None,
        }
    }

    fn matches_key(&self, key: &TreeKey) -> bool {
        self.key().as_ref() == Some(key)
    }
}

struct LaunchResult {
    host: Host,
    result: Result<SpawnResult, String>,
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

struct Sidebar {
    local: LocalStore,
    probes: Vec<HostProbe>,
    rows: Vec<TreeRow>,
    list: ListState,
    active: Option<(String, Uuid)>,
    focused: bool,
    collapsed: HashSet<(String, Uuid)>,
    show_hidden: bool,
    agent_picker: Option<AgentPicker>,
    session_overlay: Option<SessionOverlay>,
    refreshing: bool,
    starting: bool,
    creating: bool,
    mutating: bool,
    last_refresh: Instant,
    message: Option<(String, bool, Instant)>,
    probe_tx: Sender<Vec<HostProbe>>,
    probe_rx: Receiver<Vec<HostProbe>>,
    launch_tx: Sender<LaunchResult>,
    launch_rx: Receiver<LaunchResult>,
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
        let (create_tx, create_rx) = channel();
        let (mutation_tx, mutation_rx) = channel();
        let (project_mutation_tx, project_mutation_rx) = channel();
        let now = Instant::now();
        let mut sidebar = Self {
            local,
            probes: Vec::new(),
            rows: vec![TreeRow::Note("loading…".into())],
            list: ListState::default(),
            active: actions::active_session(),
            focused: tmux::current_pane_active(),
            collapsed: HashSet::new(),
            show_hidden: false,
            agent_picker: None,
            session_overlay: None,
            refreshing: false,
            starting: false,
            creating: false,
            mutating: false,
            last_refresh: now.checked_sub(REFRESH_EVERY).unwrap_or(now),
            message: None,
            probe_tx,
            probe_rx,
            launch_tx,
            launch_rx,
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
            self.active = actions::active_session();
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
                            KeyCode::Char('n' | 'т') => self.begin_new_session(),
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
                        match actions::open_pane(
                            &launch.host,
                            spawned.session.id,
                            &spawned.tmux_name,
                        ) {
                            Ok(_) => self.set_message("switched", false),
                            Err(error) => self.set_message(format!("{error:#}"), true),
                        }
                        self.kick_refresh();
                    }
                    Err(error) => self.set_message(error, true),
                }
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
                match mutation.result {
                    Ok(()) => {
                        let was_delete = matches!(&mutation.mutation, Mutation::Delete { .. });
                        let replace_deleted = match mutation.mutation {
                            Mutation::Rename => false,
                            Mutation::Delete { was_active } => {
                                was_active
                                    || self.active.as_ref().is_some_and(|active| {
                                        active.0 == mutation.target.host
                                            && active.1 == mutation.target.id
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
                            self.open_named(&host, session);
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
        let selected = self.current_key().or_else(|| {
            self.active
                .clone()
                .map(|(host, session)| TreeKey::Session(host, session))
        });
        self.rows = build_tree(
            &self.hosts(),
            &self.probes,
            &self.collapsed,
            self.show_hidden,
        );
        let target = selected
            .and_then(|key| self.rows.iter().position(|row| row.matches_key(&key)))
            .or_else(|| self.rows.iter().position(|row| row.key().is_some()));
        self.list.select(target);
    }

    fn current_key(&self) -> Option<TreeKey> {
        self.list
            .selected()
            .and_then(|index| self.rows.get(index))
            .and_then(TreeRow::key)
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

    /// Vim-style tree navigation: collapse a project, or move from one of its
    /// children back to the project row.
    fn collapse_or_parent(&mut self) {
        let Some(selected) = self.list.selected() else {
            return;
        };
        match self.rows.get(selected).and_then(TreeRow::key) {
            Some(TreeKey::Project(host, folder)) => {
                if self.collapsed.insert((host, folder)) {
                    self.rebuild();
                }
            }
            Some(TreeKey::Session(..) | TreeKey::NewSession(..)) => {
                if let Some(parent) = self.rows[..selected]
                    .iter()
                    .rposition(|row| matches!(row, TreeRow::Project { .. }))
                {
                    self.list.select(Some(parent));
                }
            }
            None => {}
        }
    }

    /// Vim-style tree navigation: expand a project, descend into an already
    /// expanded project, or open the selected session/action.
    fn expand_or_open(&mut self) {
        let Some(key) = self.current_key() else {
            return;
        };
        match key {
            TreeKey::Project(host, folder) => {
                if self.collapsed.remove(&(host, folder)) {
                    self.rebuild();
                } else {
                    self.move_cursor(1);
                }
            }
            TreeKey::Session(..) | TreeKey::NewSession(..) => self.open_selected(),
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
            }) => Some(SessionOverlay::Rename {
                target: SessionTarget {
                    host: host.clone(),
                    id: *id,
                    title: title.clone(),
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
            Some(TreeRow::Gap | TreeRow::Note(_)) | None => None,
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
        if self.starting || self.creating || self.mutating {
            return;
        }
        let Some(key) = self.current_key() else {
            return;
        };
        match key {
            TreeKey::Project(host, folder) => {
                let key = (host, folder);
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
        }
    }

    fn open_named(&mut self, host_name: &str, session: Uuid) {
        let Some(host) = self.local.host_by_name(host_name).cloned() else {
            self.set_message(format!("unknown host {host_name}"), true);
            return;
        };
        self.open(host, session);
    }

    fn open(&mut self, host: Host, session: Uuid) {
        self.starting = true;
        let tx = self.launch_tx.clone();
        std::thread::spawn(move || {
            let result = actions::start(&host, session).map_err(|error| format!("{error:#}"));
            let _ = tx.send(LaunchResult { host, result });
        });
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
            .is_some_and(|active| active.0 == target.host && active.1 == target.id);
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

fn build_tree(
    hosts: &[Host],
    probes: &[HostProbe],
    collapsed: &HashSet<(String, Uuid)>,
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
        let is_collapsed = collapsed.contains(&(host.name.clone(), folder.id));
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
            rows.extend(sessions.into_iter().map(|view| TreeRow::Session {
                host: host.name.clone(),
                id: view.session.id,
                agent: view.session.agent,
                title: view.session.title.clone(),
                state: view.state,
            }));
        }
    }
    rows.extend(unavailable);
    if rows.is_empty() {
        rows.push(TreeRow::Note("no marked projects".into()));
    }
    rows
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

    let width = usize::from(body.width.saturating_sub(4));
    if let Some(overlay) = &sidebar.session_overlay {
        draw_session_overlay(frame, body, overlay);
    } else if let Some(picker) = &sidebar.agent_picker {
        draw_agent_picker(frame, body, picker);
    } else {
        let items: Vec<ListItem> = sidebar
            .rows
            .iter()
            .map(|row| render_row(row, width, sidebar.active.as_ref()))
            .collect();
        let list = List::new(items)
            .style(Style::default().fg(FG).bg(BG))
            .highlight_symbol("  ")
            .highlight_spacing(HighlightSpacing::Always)
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
            " p pin  H hide  v hidden  d delete",
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

fn render_row(row: &TreeRow, width: usize, active: Option<&(String, Uuid)>) -> ListItem<'static> {
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
                        if *collapsed { "▸" } else { "▾" },
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
            host,
            id,
            agent,
            title,
            state,
        } => {
            let is_active = active
                .is_some_and(|(active_host, active_id)| active_host == host && active_id == id);
            let active_marker = if is_active { "›" } else { " " };
            let agent = match agent {
                AgentKind::Claude => "🧠",
                AgentKind::Codex => "🤖",
                AgentKind::Shell => "💻",
            };
            let title = util::one_line(title, width.saturating_sub(10).max(1));
            let label = format!("  {active_marker} {} {agent} {title}", state_symbol(*state));
            let style = if is_active {
                Style::default().fg(ACCENT).add_modifier(Modifier::BOLD)
            } else {
                state_style(*state)
            };
            ListItem::new(Line::from(Span::styled(label, style)))
        }
        TreeRow::NewSession { .. } => ListItem::new(Line::from(Span::styled(
            "  + new session",
            Style::default().fg(ACCENT),
        ))),
        TreeRow::Gap => ListItem::new(Line::default()),
        TreeRow::Note(text) => ListItem::new(Line::from(Span::styled(
            util::one_line(text, width.max(1)),
            Style::default().fg(DIM),
        ))),
    }
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
    use crate::model::{Folder, Probe, Session};
    use crate::reconcile::SessionView;

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

        let collapsed = HashSet::from([(
            "local".to_string(),
            match &rows[0] {
                TreeRow::Project { folder, .. } => *folder,
                _ => Uuid::nil(),
            },
        )]);
        let folded = build_tree(
            std::slice::from_ref(&host),
            std::slice::from_ref(&probe),
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
            &HashSet::new(),
            true,
        );
        assert!(with_hidden.iter().any(
            |row| matches!(row, TreeRow::Project { name, hidden: true, .. } if name == "hidden")
        ));
    }
}
