//! The dashboard.
//!
//! Navigation obeys one rule, everywhere: **Esc always goes back**. It closes an
//! overlay, then leaves a filter, then pops a screen, and only quits from the
//! top. Combined with `?` for the key list on every screen, that makes it
//! impossible to reach a state with no way out — which is the failure mode
//! terminal UIs fall into most often.
//!
//! Quitting is always safe and never asks: sessions run on their hosts, not
//! here, so closing the dashboard stops nothing.

pub mod actions;
pub mod rows;
mod state;
mod ui;

use anyhow::Result;
use ratatui::DefaultTerminal;
use ratatui::crossterm::event::{self, Event, KeyEvent, KeyEventKind};
use std::collections::HashSet;
use std::sync::mpsc::{Receiver, Sender, channel};
use std::time::{Duration, Instant};
use uuid::Uuid;

use crate::model::{AgentKind, Host, Layout, PaneRef};
use crate::remote::{self, HostProbe};
use crate::store::LocalStore;
use crate::tmux;
use rows::Row;
pub use state::{Confirm, InputKind, Overlay, Screen, ToastKind};
use state::{Intent, State};

/// How often hosts are re-probed. Short enough that a status change is noticed
/// while you are looking at it, long enough not to hammer six ssh connections.
const REFRESH_EVERY: Duration = Duration::from_secs(4);
const TOAST_FOR: Duration = Duration::from_secs(6);

/// A finished round of session starts, handed back from the worker thread.
struct LaunchBatch {
    background: bool,
    /// Geometry to replay once the panes are open, for a layout restore.
    geometry: Option<String>,
    /// Layout name, when this batch came from a restore.
    label: Option<String>,
    results: Vec<(Host, Result<crate::hostops::SpawnResult, String>)>,
}

struct App {
    local: LocalStore,
    probes: Vec<HostProbe>,
    state: State,
    last_refresh: Instant,
    quit: bool,
    tx: Sender<Vec<HostProbe>>,
    rx: Receiver<Vec<HostProbe>>,
    launch_tx: Sender<LaunchBatch>,
    launch_rx: Receiver<LaunchBatch>,
}

pub fn run() -> Result<()> {
    let mut app = App::new()?;
    let mut terminal = ratatui::init();
    let result = app.main_loop(&mut terminal);
    ratatui::restore();
    result
}

impl App {
    fn new() -> Result<Self> {
        let mut local = LocalStore::load()?;
        // A fresh install can drive the machine it is on without any setup.
        if local.ensure_local_host() {
            local.save()?;
        }
        let (tx, rx) = channel();
        let (launch_tx, launch_rx) = channel();
        let now = Instant::now();
        Ok(Self {
            local,
            probes: Vec::new(),
            state: State::default(),
            last_refresh: now.checked_sub(REFRESH_EVERY).unwrap_or(now),
            quit: false,
            tx,
            rx,
            launch_tx,
            launch_rx,
        })
    }

    fn main_loop(&mut self, terminal: &mut DefaultTerminal) -> Result<()> {
        while !self.quit {
            self.rebuild();
            terminal.draw(|frame| ui::draw(frame, self))?;
            self.pump()?;
        }
        Ok(())
    }

    // -----------------------------------------------------------------------
    // Data
    // -----------------------------------------------------------------------

    fn screen(&self) -> &Screen {
        self.state.screen()
    }

    pub fn hosts(&self) -> Vec<Host> {
        self.local.live_hosts().into_iter().cloned().collect()
    }

    fn host(&self, name: &str) -> Option<Host> {
        self.local.host_by_name(name).cloned()
    }

    /// Every known session paired with its host. Needed only to recognise panes
    /// opened by a build that predates identity tagging.
    fn all_sessions(&self) -> Vec<(String, crate::model::Session)> {
        self.probes
            .iter()
            .filter_map(|p| p.probe.as_ref().map(|pr| (p.host.name.clone(), pr)))
            .flat_map(|(host, pr)| {
                pr.records()
                    .map(move |s| (host.clone(), s.clone()))
                    .collect::<Vec<_>>()
            })
            .collect()
    }

    fn rebuild(&mut self) {
        let hosts = self.hosts();
        let mut rows = match self.screen().clone() {
            Screen::Folders => rows::folders(&hosts, &self.probes),
            Screen::Folder { host, folder, .. } => rows::folder_detail(&host, folder, &self.probes),
            Screen::Running => rows::running(&hosts, &self.probes),
            Screen::Layouts => {
                let saved: Vec<Layout> = self.local.live_layouts().into_iter().cloned().collect();
                rows::layouts(&saved, &self.probes)
            }
            Screen::Hosts => rows::hosts_screen(&hosts, &self.probes),
        };

        if !self.state.filter.is_empty() {
            rows = fuzzy_filter(rows, &self.state.filter);
            if rows.is_empty() {
                rows.push(Row::Note(format!(
                    "nothing matches “{}”",
                    self.state.filter
                )));
            }
        }
        self.state.rows = rows;

        // Keep the cursor on something actionable.
        let selectable: Vec<usize> = self
            .state
            .rows
            .iter()
            .enumerate()
            .filter(|(_, r)| r.selectable())
            .map(|(i, _)| i)
            .collect();
        match self.state.list.selected() {
            _ if selectable.is_empty() => self.state.list.select(None),
            Some(i) if selectable.contains(&i) => {}
            _ => self.state.list.select(selectable.first().copied()),
        }
    }

    fn pump(&mut self) -> Result<()> {
        if event::poll(Duration::from_millis(200))?
            && let Event::Key(key) = event::read()?
            && key.kind == KeyEventKind::Press
        {
            self.on_key(key);
        }

        while let Ok(probes) = self.rx.try_recv() {
            self.probes = probes;
            self.state.refreshing = false;
            self.last_refresh = Instant::now();
        }

        while let Ok(batch) = self.launch_rx.try_recv() {
            self.finish_batch(batch);
        }

        // Polling every host every few seconds for a dashboard nobody is
        // looking at is pure waste, on this machine and on theirs.
        if !self.state.refreshing
            && self.last_refresh.elapsed() >= REFRESH_EVERY
            && tmux::current_session_attached()
        {
            self.kick_refresh();
        }

        if let Some((_, _, at)) = &self.state.toast
            && at.elapsed() > TOAST_FOR
        {
            self.state.toast = None;
        }
        Ok(())
    }

    fn kick_refresh(&mut self) {
        if self.state.refreshing {
            return;
        }
        self.state.refreshing = true;
        self.last_refresh = Instant::now();
        let hosts = self.hosts();
        let tx = self.tx.clone();
        // Probing blocks on ssh, so it never runs on the thread that draws.
        std::thread::spawn(move || {
            let _ = tx.send(remote::probe_all(&hosts));
        });
    }

    pub fn info(&mut self, msg: impl Into<String>) {
        self.state.info(msg);
    }

    pub fn error(&mut self, msg: impl Into<String>) {
        self.state.error(msg);
    }

    fn current(&self) -> Option<Row> {
        self.state.current()
    }

    // -----------------------------------------------------------------------
    // Input
    // -----------------------------------------------------------------------

    fn on_key(&mut self, key: KeyEvent) {
        let Some(intent) = self.state.reduce_key(key) else {
            return;
        };
        match intent {
            Intent::Activate { background } => self.activate(background),
            Intent::AskDelete => self.ask_delete(),
            Intent::AskStop => self.ask_stop(),
            Intent::BeginRelabel => self.begin_relabel(),
            Intent::Detach => self.detach(),
            Intent::FocusWork => {
                if let Err(error) = actions::focus_work() {
                    self.error(format!("{error:#}"));
                }
            }
            Intent::InstallHost => self.install_host(),
            Intent::Refresh => self.kick_refresh(),
            Intent::RunConfirmed(action) => self.run_confirmed(action),
            Intent::RunInput(kind, value) => self.run_input(kind, value),
        }
    }

    /// Hand the terminal back, leaving everything running.
    ///
    /// The dashboard process stays alive in its window, so reattaching is
    /// instant and the statuses are already there.
    fn detach(&mut self) {
        if let Err(e) = tmux::detach_current() {
            // Nothing to detach from means this is not the session bizik
            // manages, and quitting outright is the only sensible reading.
            let _ = e;
            self.quit = true;
        }
    }

    // -----------------------------------------------------------------------
    // Doing things
    // -----------------------------------------------------------------------

    /// Enter, or `b` for the background variant.
    ///
    /// With sessions selected, this acts on the selection rather than the row
    /// under the cursor — which is how several panes get opened in one go.
    fn activate(&mut self, background: bool) {
        if !self.state.selected.is_empty() {
            self.launch_selected(background);
            return;
        }
        let Some(row) = self.current() else { return };

        match row {
            Row::Folder { host, folder, .. } => {
                if background {
                    self.info("open the folder to choose what to start");
                } else {
                    let name = folder.display_name();
                    self.state.stack.push(Screen::Folder {
                        host,
                        folder: folder.id,
                        name,
                    });
                    self.state.list.select(None);
                    // A filter belongs to the list it was typed into. Carried
                    // into a folder it hides that folder's own actions — you
                    // search for "anogem", open it, and the entries for
                    // starting something are gone.
                    self.state.filter.clear();
                }
            }
            Row::NewSession {
                host,
                folder,
                agent,
            } => self.start_new(&host, folder, agent, None, background),
            Row::Chat { host, folder, chat } => {
                // Adopting a conversation records it, then resumes it — the
                // history stays exactly where the agent put it.
                let title = crate::util::one_line(&chat.display_title(), 60);
                self.start_new(
                    &host,
                    folder,
                    chat.agent,
                    Some((chat.id, title)),
                    background,
                )
            }
            Row::Session { host, view } => self.launch_one(&host, view.session.id, background),
            Row::LayoutEntry { layout, .. } => self.restore_layout(&layout),
            Row::HostEntry { host, .. } => {
                if let Some(p) = self.probes.iter().find(|p| p.host.name == host.name)
                    && let Some(e) = &p.error
                {
                    self.error(e.clone());
                } else {
                    self.info(format!("{} is reachable", host.name));
                }
            }
            Row::Note(_) => {}
        }
    }

    fn launch_one(&mut self, host_name: &str, session: Uuid, background: bool) {
        let Some(host) = self.host(host_name) else {
            self.error(format!("unknown host {host_name}"));
            return;
        };
        self.spawn_batch(vec![(host, session)], background, None, None);
    }

    /// Start sessions off the drawing thread.
    ///
    /// Each start is an ssh round trip. Done inline, a batch of six would freeze
    /// the dashboard for as long as the slowest one — and on a cold connection
    /// that is seconds. The starts run concurrently on a worker; the panes are
    /// opened here when the results arrive, in order, so the arrangement stays
    /// predictable.
    fn spawn_batch(
        &mut self,
        jobs: Vec<(Host, Uuid)>,
        background: bool,
        geometry: Option<String>,
        label: Option<String>,
    ) {
        if jobs.is_empty() {
            return;
        }
        self.state.starting += jobs.len();
        let tx = self.launch_tx.clone();

        std::thread::spawn(move || {
            let results: Vec<(Host, Result<crate::hostops::SpawnResult, String>)> =
                std::thread::scope(|scope| {
                    let handles: Vec<_> = jobs
                        .iter()
                        .map(|(host, id)| scope.spawn(move || actions::start(host, *id)))
                        .collect();
                    jobs.iter()
                        .cloned()
                        .zip(handles)
                        .map(|((host, _), h)| {
                            let outcome = match h.join() {
                                Ok(Ok(r)) => Ok(r),
                                Ok(Err(e)) => Err(format!("{e:#}")),
                                Err(_) => Err("start panicked".into()),
                            };
                            (host, outcome)
                        })
                        .collect()
                });
            let _ = tx.send(LaunchBatch {
                background,
                geometry,
                label,
                results,
            });
        });
    }

    /// Show a finished batch in the sidebar workspace. Runs on the drawing
    /// thread, where every call is a local tmux command and therefore fast.
    fn finish_batch(&mut self, batch: LaunchBatch) {
        self.state.starting = self.state.starting.saturating_sub(batch.results.len());

        let mut opened = 0;
        let mut failed = 0;
        for (host, result) in &batch.results {
            match result {
                Ok(spawned) => {
                    if batch.background {
                        opened += 1;
                    } else if let Err(e) =
                        actions::open_pane(host, spawned.session.id, &spawned.tmux_name)
                    {
                        failed += 1;
                        self.error(format!("{e:#}"));
                    } else {
                        opened += 1;
                    }
                }
                Err(e) => {
                    failed += 1;
                    self.error(e.clone());
                }
            }
        }

        if !batch.background && opened > 0 {
            match &batch.geometry {
                Some(geometry) => {
                    if actions::apply_geometry(geometry).is_err() {
                        actions::tile();
                    }
                }
                None => actions::tile(),
            }
            let _ = actions::focus_work();
        }

        // The moment the workspace opens is when the dashboard goes off screen, so
        // that is where the way back belongs — not only in the help overlay.
        let back = if batch.background {
            String::new()
        } else {
            format!(" · {} returns here", tmux::return_key())
        };

        if failed > 0 {
            self.error(format!("{opened} started, {failed} failed"));
        } else if let Some(name) = batch.label {
            self.info(format!("restored {name}{back}"));
        } else {
            self.info(format!(
                "{opened} session{} {}{back}",
                if opened == 1 { "" } else { "s" },
                if batch.background {
                    "started"
                } else {
                    "available in sidebar"
                }
            ));
        }
        self.kick_refresh();
    }

    fn start_new(
        &mut self,
        host_name: &str,
        folder: Uuid,
        agent: AgentKind,
        resume: Option<(String, String)>,
        background: bool,
    ) {
        let Some(host) = self.host(host_name) else {
            self.error(format!("unknown host {host_name}"));
            return;
        };
        let (resume_id, title) = match resume {
            Some((id, title)) => (Some(id), Some(title)),
            None => (None, None),
        };
        match actions::create_session(&host, folder, agent, title.as_deref(), resume_id.as_deref())
        {
            Ok(session) => self.launch_one(host_name, session.id, background),
            Err(e) => self.error(format!("{e:#}")),
        }
    }

    fn launch_selected(&mut self, background: bool) {
        let picked: Vec<(String, Uuid)> = self.state.selected.iter().cloned().collect();
        let mut jobs: Vec<(Host, Uuid)> = Vec::new();
        for (host_name, session) in &picked {
            match self.host(host_name) {
                Some(h) => jobs.push((h, *session)),
                None => self.error(format!("unknown host {host_name}")),
            }
        }
        self.state.selected.clear();
        self.spawn_batch(jobs, background, None, None);
    }

    /// Restore a saved arrangement.
    ///
    /// A saved geometry describes that layout's panes and no others, so
    /// replaying it over unrelated panes would rearrange work the layout knows
    /// nothing about. Rather than silently tile instead, the panes that do not
    /// belong are named and closing them is offered as a choice.
    fn restore_layout(&mut self, layout: &Layout) {
        let open = actions::panes_in_work(&self.all_sessions());
        let close = panes_to_close(&open, &layout.panes);

        if close.is_empty() {
            // Say what was counted. "It offered to close nothing" and "it never
            // looked" are indistinguishable otherwise, and telling them apart
            // by reasoning about the code wasted an afternoon.
            if open.len() > layout.panes.len() {
                self.error(format!(
                    "{} panes open, {} identified, none judged surplus — bzk panes shows why",
                    tmux::find_window(actions::WORK_WINDOW)
                        .and_then(|w| tmux::window_panes(&w).ok())
                        .map_or(0, |panes| panes.len()),
                    open.len()
                ));
            }
            self.do_restore(layout, &[]);
            return;
        }
        self.state.overlay = Some(Overlay::Confirm {
            prompt: format!(
                "“{}” wants the pane window to itself. Close {} pane{} that do not belong to it — duplicates and strangers — and restore its exact arrangement?",
                layout.name,
                close.len(),
                if close.len() == 1 { "" } else { "s" }
            ),
            action: Confirm::RestoreLayout {
                id: layout.id,
                close,
            },
        });
    }

    fn do_restore(&mut self, layout: &Layout, close: &[String]) {
        for pane in close {
            let _ = tmux::kill_pane(pane);
        }

        let mut jobs: Vec<(Host, Uuid)> = Vec::new();
        for pane in &layout.panes {
            match self.host(&pane.host) {
                Some(h) => jobs.push((h, pane.session)),
                None => self.error(format!(
                    "layout needs host “{}”, which is not configured",
                    pane.host
                )),
            }
        }
        self.spawn_batch(
            jobs,
            false,
            layout.tmux_layout.clone(),
            Some(layout.name.clone()),
        );
    }

    fn begin_relabel(&mut self) {
        match self.current() {
            Some(Row::Folder { host, folder, .. }) => {
                self.state.overlay = Some(Overlay::Input {
                    prompt: format!("new label for {}", folder.path),
                    value: folder.label.clone().unwrap_or_default(),
                    kind: InputKind::Relabel {
                        host,
                        path: folder.path,
                    },
                })
            }
            _ => self.info("select a folder to rename it"),
        }
    }

    fn ask_stop(&mut self) {
        match self.current() {
            Some(Row::Session { host, view }) => {
                let session = view.session;
                self.state.overlay = Some(Overlay::Confirm {
                    prompt: format!(
                        "stop “{}” on {host}? the conversation is kept and can be resumed",
                        session.title
                    ),
                    action: Confirm::Stop {
                        host,
                        session: session.id,
                    },
                })
            }
            _ => self.info("nothing here to stop"),
        }
    }

    fn ask_delete(&mut self) {
        match self.current() {
            Some(Row::Session { host, view }) => {
                let session = view.session;
                self.state.overlay = Some(Overlay::Confirm {
                    prompt: format!(
                        "forget “{}”? it stops, and bizik drops its record — the agent's own history stays on disk",
                        session.title
                    ),
                    action: Confirm::Forget {
                        host,
                        session: session.id,
                    },
                })
            }
            Some(Row::Folder { host, folder, .. }) => {
                self.state.overlay = Some(Overlay::Confirm {
                    prompt: format!(
                        "unmark {}? its sessions are forgotten too — nothing on disk is touched",
                        folder.path
                    ),
                    action: Confirm::Unmark {
                        host,
                        path: folder.path,
                    },
                })
            }
            Some(Row::LayoutEntry { layout, .. }) => {
                self.state.overlay = Some(Overlay::Confirm {
                    prompt: format!("delete layout “{}”?", layout.name),
                    action: Confirm::DeleteLayout { id: layout.id },
                })
            }
            _ => self.info("nothing here to remove"),
        }
    }

    fn install_host(&mut self) {
        let Some(Row::HostEntry { host, .. }) = self.current() else {
            self.info("press i on a host to install bizik there");
            return;
        };
        match remote::install(&host) {
            Ok(v) => {
                self.info(format!("installed on {}: {v}", host.name));
                self.kick_refresh();
            }
            Err(e) => self.error(format!("{e:#}")),
        }
    }

    fn run_confirmed(&mut self, action: Confirm) {
        let outcome = match action {
            Confirm::Stop { host, session } => self
                .host(&host)
                .ok_or_else(|| format!("unknown host {host}"))
                .and_then(|h| actions::stop(&h, session).map_err(|e| format!("{e:#}")))
                .map(|_| "stopped".to_string()),
            Confirm::Forget { host, session } => self
                .host(&host)
                .ok_or_else(|| format!("unknown host {host}"))
                .and_then(|h| actions::forget(&h, session).map_err(|e| format!("{e:#}")))
                .map(|_| "forgotten".to_string()),
            Confirm::Unmark { host, path } => self
                .host(&host)
                .ok_or_else(|| format!("unknown host {host}"))
                .and_then(|h| actions::unmark(&h, &path).map_err(|e| format!("{e:#}")))
                .map(|_| "unmarked".to_string()),
            Confirm::DeleteLayout { id } => {
                self.local.remove_layout(id);
                self.local
                    .save()
                    .map(|_| "layout deleted".to_string())
                    .map_err(|e| format!("{e:#}"))
            }
            Confirm::CloseViewer => {
                // Panes are only viewers; closing them costs nothing that is
                // not still running on its host.
                let session = tmux::current_session();
                self.quit = true;
                match session {
                    Some(s) => tmux::kill_session(&s)
                        .map(|_| String::new())
                        .map_err(|e| format!("{e:#}")),
                    None => Ok(String::new()),
                }
            }
            Confirm::RestoreLayout { id, close } => {
                match self
                    .local
                    .live_layouts()
                    .into_iter()
                    .find(|l| l.id == id)
                    .cloned()
                {
                    Some(layout) => {
                        self.do_restore(&layout, &close);
                        return;
                    }
                    None => Err("that layout is gone".to_string()),
                }
            }
        };
        match outcome {
            Ok(msg) => {
                self.info(msg);
                self.kick_refresh();
            }
            Err(e) => self.error(e),
        }
    }

    fn run_input(&mut self, kind: InputKind, value: String) {
        if value.is_empty() {
            self.info("cancelled");
            return;
        }
        match kind {
            InputKind::SaveLayout => self.save_layout(value),
            InputKind::Relabel { host, path } => {
                match self
                    .host(&host)
                    .ok_or_else(|| format!("unknown host {host}"))
                    .and_then(|h| actions::relabel(&h, &path, &value).map_err(|e| format!("{e:#}")))
                {
                    Ok(()) => {
                        self.info("renamed");
                        self.kick_refresh();
                    }
                    Err(e) => self.error(e),
                }
            }
        }
    }

    fn save_layout(&mut self, name: String) {
        let panes = actions::panes_in_work(&self.all_sessions());
        if panes.is_empty() {
            self.error("no bizik panes are open — open some first, then save");
            return;
        }
        let refs: Vec<PaneRef> = panes
            .into_iter()
            .map(|p| PaneRef {
                host: p.host,
                session: p.session,
            })
            .collect();
        let count = refs.len();

        self.local
            .layouts
            .push(Layout::new(name.clone(), refs, actions::work_layout()));
        match self.local.save() {
            Ok(()) => self.info(format!("saved “{name}” with {count} panes")),
            Err(e) => self.error(format!("{e:#}")),
        }
    }
}

/// Panes a layout restore must remove: strangers and every duplicate after the
/// first matching pane. Kept pure so the exact destructive decision is tested
/// without touching tmux.
fn panes_to_close(open: &[actions::OpenPane], wanted: &[PaneRef]) -> Vec<String> {
    let wanted: HashSet<(String, Uuid)> = wanted
        .iter()
        .map(|pane| (pane.host.clone(), pane.session))
        .collect();
    let mut seen: HashSet<(String, Uuid)> = HashSet::new();
    open.iter()
        .filter_map(|pane| {
            let key = (pane.host.clone(), pane.session);
            (!wanted.contains(&key) || !seen.insert(key)).then(|| pane.pane.clone())
        })
        .collect()
}

/// Rank rows by a fuzzy match, keeping notes out of the way.
fn fuzzy_filter(rows: Vec<Row>, needle: &str) -> Vec<Row> {
    use nucleo_matcher::pattern::{CaseMatching, Normalization, Pattern};
    use nucleo_matcher::{Config, Matcher, Utf32Str};

    let mut matcher = Matcher::new(Config::DEFAULT);
    let pattern = Pattern::parse(needle, CaseMatching::Ignore, Normalization::Smart);

    let mut scored: Vec<(u32, Row)> = rows
        .into_iter()
        .filter(|r| r.selectable())
        .filter_map(|row| {
            let mut buf = Vec::new();
            let haystack = row.haystack();
            let score = pattern.score(Utf32Str::new(&haystack, &mut buf), &mut matcher)?;
            Some((score, row))
        })
        .collect();

    scored.sort_by_key(|(score, _)| std::cmp::Reverse(*score));
    scored.into_iter().map(|(_, row)| row).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Folder;

    fn folder_row(name: &str, host: &str) -> Row {
        let mut f = Folder::new(format!("/srv/{name}"));
        f.label = Some(name.to_string());
        Row::Folder {
            host: host.into(),
            folder: f,
            sessions: 0,
            running: 0,
            attention: 0,
            blocked: 0,
        }
    }

    #[test]
    fn filter_matches_across_host_and_name_and_ranks() {
        let rows = vec![
            folder_row("frontend", "gvidon"),
            folder_row("backend", "hetzner"),
        ];
        let got = fuzzy_filter(rows, "front");
        assert_eq!(got.len(), 1);
        assert!(got[0].haystack().contains("frontend"));
    }

    #[test]
    fn filter_drops_notes_so_the_cursor_never_lands_on_one() {
        let rows = vec![Row::Note("some warning".into()), folder_row("app", "h")];
        let got = fuzzy_filter(rows, "app");
        assert!(got.iter().all(|r| r.selectable()));
    }

    #[test]
    fn a_restore_closes_strangers_and_second_copies_alike() {
        let a = Uuid::new_v4();
        let b = Uuid::new_v4();
        let stranger = Uuid::new_v4();
        let wanted = vec![
            PaneRef {
                host: "back".into(),
                session: a,
            },
            PaneRef {
                host: "back".into(),
                session: b,
            },
        ];
        let pane = |host: &str, session, pane: &str| actions::OpenPane {
            host: host.into(),
            session,
            pane: pane.into(),
        };

        // Exactly the shape that left eight panes for four entries: every pane
        // belongs to the layout, so nothing looked out of place.
        let doubled = panes_to_close(
            &[
                pane("back", a, "%1"),
                pane("back", b, "%2"),
                pane("back", a, "%3"),
                pane("back", b, "%4"),
            ],
            &wanted,
        );
        assert_eq!(doubled, vec!["%3", "%4"], "second copies have to go too");

        let mixed = panes_to_close(
            &[pane("back", a, "%1"), pane("back", stranger, "%2")],
            &wanted,
        );
        assert_eq!(mixed, vec!["%2"]);

        let clean = panes_to_close(&[pane("back", a, "%1"), pane("back", b, "%2")], &wanted);
        assert!(clean.is_empty(), "a window already right is left alone");
    }
}
