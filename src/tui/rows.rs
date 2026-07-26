//! What each screen shows, and how a session's state is worked out.
//!
//! The status column is the reason this tool exists. Launching sessions is easy;
//! knowing which of them is grinding away and which is sitting there waiting for
//! an answer is what makes running six at once possible instead of merely
//! impressive. Where an agent cannot tell us, the status says so rather than
//! guessing — a fabricated "waiting" is worse than an honest "running".

use uuid::Uuid;

use crate::model::{AgentKind, Chat, Folder, Host, Layout, Probe, Session};
use crate::remote::HostProbe;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Status {
    /// Blocked on you — a permission prompt or a question. Known only when the
    /// agent's hooks are installed, and the single most useful thing this tool
    /// can tell you: a background session in this state will wait forever.
    NeedsYou,
    /// A turn ended and there is something to look at.
    Done,
    /// The agent is processing right now.
    Working,
    /// Idle, but without hooks there is no telling whether that means finished
    /// or blocked. Deliberately vaguer than the two above.
    YourTurn,
    /// The session is up but the agent exposes no status, or which process
    /// belongs to it is ambiguous.
    Up,
    /// Nothing is running; starting it will resume the conversation.
    Down,
}

impl Status {
    pub fn label(self) -> &'static str {
        match self {
            Status::NeedsYou => "needs you",
            Status::Done => "done",
            Status::Working => "working",
            Status::YourTurn => "your turn",
            Status::Up => "running",
            Status::Down => "stopped",
        }
    }

    pub fn glyph(self) -> &'static str {
        match self {
            Status::NeedsYou => "▲",
            Status::Done => "◆",
            Status::Working => "●",
            Status::YourTurn => "◇",
            Status::Up => "○",
            Status::Down => "·",
        }
    }

    /// The ball is in your court.
    pub fn wants_you(self) -> bool {
        matches!(self, Status::NeedsYou | Status::Done | Status::YourTurn)
    }

    /// Sort key for the mission-control list: whatever is blocked comes first.
    pub fn urgency(self) -> u8 {
        match self {
            Status::NeedsYou => 0,
            Status::Done => 1,
            Status::YourTurn => 2,
            Status::Working => 3,
            Status::Up => 4,
            Status::Down => 5,
        }
    }
}

#[derive(Clone, Debug)]
pub enum Row {
    Folder {
        host: String,
        folder: Folder,
        sessions: usize,
        running: usize,
        /// Sessions whose turn it is yours to take.
        attention: usize,
        /// Of those, the ones known to be blocked on you.
        blocked: usize,
    },
    /// The "start something new here" entry at the top of a folder.
    NewSession {
        host: String,
        folder: Uuid,
        agent: AgentKind,
    },
    Session {
        host: String,
        session: Session,
        status: Status,
        preview: Option<String>,
    },
    /// An agent conversation on disk that bizik does not yet track.
    Chat {
        host: String,
        folder: Uuid,
        chat: Chat,
    },
    HostEntry {
        host: Host,
        detail: String,
        ok: bool,
    },
    LayoutEntry {
        layout: Layout,
        missing: usize,
    },
    /// Guidance shown when a screen has nothing to list. Never selectable.
    Note(String),
}

impl Row {
    /// Rows that cannot be acted on are skipped by the cursor entirely, so
    /// there is never a selection that does nothing.
    pub fn selectable(&self) -> bool {
        !matches!(self, Row::Note(_))
    }

    /// Text the fuzzy filter matches against.
    pub fn haystack(&self) -> String {
        match self {
            Row::Folder { host, folder, .. } => {
                format!("{host} {} {}", folder.display_name(), folder.path)
            }
            Row::NewSession { agent, .. } => format!("new {agent}"),
            Row::Session { host, session, .. } => {
                format!("{host} {} {}", session.title, session.agent)
            }
            Row::Chat { host, chat, .. } => {
                format!("{host} {} {}", chat.display_title(), chat.agent)
            }
            Row::HostEntry { host, .. } => host.name.clone(),
            Row::LayoutEntry { layout, .. } => layout.name.clone(),
            Row::Note(_) => String::new(),
        }
    }

    /// Identity used by multi-select. Only sessions can be selected — a layout
    /// is made of running things, not of intentions.
    pub fn select_key(&self) -> Option<(String, Uuid)> {
        match self {
            Row::Session { host, session, .. } => Some((host.clone(), session.id)),
            _ => None,
        }
    }
}

/// Decide a session's status from what the host reported.
///
/// Matching prefers the agent's own conversation id, which is exact. Falling
/// back to the working directory is only safe when exactly one agent of that
/// kind is running there; with two, which is which is unknowable from here and
/// the status stays deliberately vague.
pub fn session_status(probe: &Probe, session: &Session, folder_path: &str) -> Status {
    let up = probe.tmux.iter().any(|t| t.name == session.tmux_name());
    if !up {
        return Status::Down;
    }

    let by_id = session.agent_session_id.as_ref().and_then(|id| {
        probe
            .live
            .iter()
            .find(|l| l.agent == session.agent && l.agent_session_id.as_ref() == Some(id))
    });

    let live = by_id.or_else(|| {
        let mut same_place = probe
            .live
            .iter()
            .filter(|l| l.agent == session.agent && l.cwd == folder_path);
        match (same_place.next(), same_place.next()) {
            (Some(only), None) => Some(only),
            _ => None,
        }
    });

    // `attention` is only present when a hook report was newer than the agent's
    // own status line, so where it exists it is the better account.
    match live {
        None => Status::Up,
        Some(l) => match (l.attention.as_deref(), l.status.as_str()) {
            (Some("waiting"), _) => Status::NeedsYou,
            (_, "busy") => Status::Working,
            (Some("done"), "idle") => Status::Done,
            (_, "idle") => Status::YourTurn,
            _ => Status::Up,
        },
    }
}

/// The favourites screen: every marked folder on every host.
pub fn folders(hosts: &[Host], probes: &[HostProbe]) -> Vec<Row> {
    let mut rows = Vec::new();

    for host in hosts {
        let Some(probed) = probes.iter().find(|p| p.host.name == host.name) else {
            continue;
        };
        let Some(probe) = &probed.probe else {
            rows.push(Row::Note(format!(
                "{} unreachable — {}",
                host.name,
                probed.error.as_deref().unwrap_or("unknown error")
            )));
            continue;
        };

        for folder in &probe.folders {
            let sessions: Vec<&Session> = probe
                .sessions
                .iter()
                .filter(|s| s.folder_id == folder.id)
                .collect();
            let statuses: Vec<Status> = sessions
                .iter()
                .map(|s| session_status(probe, s, &folder.path))
                .collect();

            rows.push(Row::Folder {
                host: host.name.clone(),
                folder: folder.clone(),
                sessions: sessions.len(),
                running: statuses.iter().filter(|s| **s != Status::Down).count(),
                attention: statuses.iter().filter(|s| s.wants_you()).count(),
                blocked: statuses
                    .iter()
                    .filter(|s| **s == Status::NeedsYou)
                    .count(),
            });
        }
    }

    if rows.iter().all(|r| matches!(r, Row::Note(_))) {
        rows.push(Row::Note(
            "no marked folders — run `bzk mark` in a directory on any host".into(),
        ));
    }
    rows
}

/// One folder: what you can start, what is already tracked, and what history
/// exists that bizik has not adopted yet.
pub fn folder_detail(host_name: &str, folder_id: Uuid, probes: &[HostProbe]) -> Vec<Row> {
    let mut rows = Vec::new();
    let Some(probe) = probes
        .iter()
        .find(|p| p.host.name == host_name)
        .and_then(|p| p.probe.as_ref())
    else {
        return vec![Row::Note(format!("{host_name} is not reachable right now"))];
    };
    let Some(folder) = probe.folders.iter().find(|f| f.id == folder_id) else {
        return vec![Row::Note("this folder is no longer marked".into())];
    };

    // Only agents actually installed on that host, plus a plain shell, which
    // always works.
    for agent in AgentKind::ALL {
        if agent == AgentKind::Shell || probe.agents.contains(&agent) {
            rows.push(Row::NewSession {
                host: host_name.to_string(),
                folder: folder_id,
                agent,
            });
        }
    }

    let mut sessions: Vec<&Session> = probe
        .sessions
        .iter()
        .filter(|s| s.folder_id == folder_id)
        .collect();
    sessions.sort_by_key(|s| std::cmp::Reverse(s.last_attached.unwrap_or(s.created_at)));

    for session in &sessions {
        let status = session_status(probe, session, &folder.path);
        let preview = probe
            .tmux
            .iter()
            .find(|t| t.name == session.tmux_name())
            .and_then(|t| t.preview.clone());
        rows.push(Row::Session {
            host: host_name.to_string(),
            session: (*session).clone(),
            status,
            preview,
        });
    }

    // Conversations already adopted by a session would otherwise appear twice.
    let adopted: Vec<&str> = sessions
        .iter()
        .filter_map(|s| s.agent_session_id.as_deref())
        .collect();

    // Only conversations that can actually be reopened are offered. Listing a
    // chat whose agent has no resume would be an entry that cannot do what it
    // appears to promise.
    let mut chats: Vec<&Chat> = probe
        .chats
        .iter()
        .filter(|c| {
            under(&c.cwd, &folder.path)
                && !adopted.contains(&c.id.as_str())
                && crate::agent::by_kind(c.agent).caps().resume
        })
        .collect();
    chats.sort_by_key(|c| std::cmp::Reverse(c.last_active));

    for chat in chats {
        rows.push(Row::Chat {
            host: host_name.to_string(),
            folder: folder_id,
            chat: chat.clone(),
        });
    }
    rows
}

/// Everything currently up, across every host — the mission-control view.
pub fn running(hosts: &[Host], probes: &[HostProbe]) -> Vec<Row> {
    let mut rows = Vec::new();

    for host in hosts {
        let Some(probe) = probes
            .iter()
            .find(|p| p.host.name == host.name)
            .and_then(|p| p.probe.as_ref())
        else {
            continue;
        };

        for session in &probe.sessions {
            let Some(folder) = probe.folders.iter().find(|f| f.id == session.folder_id) else {
                continue;
            };
            let status = session_status(probe, session, &folder.path);
            if status == Status::Down {
                continue;
            }
            rows.push(Row::Session {
                host: host.name.clone(),
                session: session.clone(),
                status,
                preview: probe
                    .tmux
                    .iter()
                    .find(|t| t.name == session.tmux_name())
                    .and_then(|t| t.preview.clone()),
            });
        }
    }

    // Whatever wants you first; that is the only ordering that matters here.
    rows.sort_by_key(|r| match r {
        Row::Session { status, .. } => status.urgency(),
        _ => u8::MAX,
    });

    if rows.is_empty() {
        rows.push(Row::Note(
            "nothing is running — start something from the folders screen".into(),
        ));
    }
    rows
}

pub fn hosts_screen(hosts: &[Host], probes: &[HostProbe]) -> Vec<Row> {
    if hosts.is_empty() {
        return vec![Row::Note(
            "no hosts — add one with: bzk host add <name> <ssh-target>".into(),
        )];
    }
    hosts
        .iter()
        .map(|host| {
            let probed = probes.iter().find(|p| p.host.name == host.name);
            match probed.and_then(|p| p.probe.as_ref()) {
                Some(probe) => {
                    let agents = if probe.agents.is_empty() {
                        "no agents installed".to_string()
                    } else {
                        probe
                            .agents
                            .iter()
                            .map(|a| a.to_string())
                            .collect::<Vec<_>>()
                            .join("+")
                    };
                    let mut detail = format!(
                        "bizik {} · {} folders · {}",
                        probe.bzk_version,
                        probe.folders.len(),
                        agents
                    );
                    // Say what is missing right where the host is listed, so a
                    // stale binary or absent hooks are not silent.
                    if probe.bzk_version != env!("CARGO_PKG_VERSION") {
                        detail.push_str(" · stale, press i");
                    }
                    if !probe.hooks_installed && !probe.agents.is_empty() {
                        detail.push_str(" · no hooks");
                    }
                    Row::HostEntry {
                        host: host.clone(),
                        detail,
                        ok: true,
                    }
                }
                None => Row::HostEntry {
                    host: host.clone(),
                    detail: probed
                        .and_then(|p| p.error.clone())
                        .unwrap_or_else(|| "not probed yet".into()),
                    ok: false,
                },
            }
        })
        .collect()
}

pub fn layouts(saved: &[Layout], probes: &[HostProbe]) -> Vec<Row> {
    if saved.is_empty() {
        return vec![Row::Note(
            "no layouts — open some panes, then press S to save this arrangement".into(),
        )];
    }
    saved
        .iter()
        .map(|layout| {
            let missing = layout
                .panes
                .iter()
                .filter(|p| {
                    !probes
                        .iter()
                        .filter(|hp| hp.host.name == p.host)
                        .filter_map(|hp| hp.probe.as_ref())
                        .any(|probe| probe.sessions.iter().any(|s| s.id == p.session))
                })
                .count();
            Row::LayoutEntry {
                layout: layout.clone(),
                missing,
            }
        })
        .collect()
}

fn under(path: &str, root: &str) -> bool {
    let p = path.trim_end_matches('/');
    let r = root.trim_end_matches('/');
    p == r || p.strip_prefix(r).is_some_and(|rest| rest.starts_with('/'))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{LiveAgent, TmuxSession};

    fn probe_with(session: &Session, live: Vec<LiveAgent>, up: bool) -> Probe {
        Probe {
            sessions: vec![session.clone()],
            tmux: if up { {
                    vec![TmuxSession {
                        name: session.tmux_name(),
                        created: 0,
                        attached: false,
                        windows: 1,
                        preview: None,
                    }]
                } } else { Default::default() },
            live,
            ..Default::default()
        }
    }

    fn live(agent: AgentKind, cwd: &str, id: Option<&str>, status: &str) -> LiveAgent {
        LiveAgent {
            agent,
            pid: 1,
            cwd: cwd.into(),
            agent_session_id: id.map(str::to_string),
            status: status.into(),
            status_at: 0,
            attention: None,
        }
    }

    fn with_attention(mut l: LiveAgent, state: &str) -> LiveAgent {
        l.attention = Some(state.into());
        l
    }

    #[test]
    fn no_tmux_session_means_stopped() {
        let s = Session::new(Uuid::new_v4(), AgentKind::Claude, "t".into());
        let p = probe_with(&s, vec![], false);
        assert_eq!(session_status(&p, &s, "/repo"), Status::Down);
    }

    #[test]
    fn busy_and_idle_map_to_working_and_your_turn() {
        let mut s = Session::new(Uuid::new_v4(), AgentKind::Claude, "t".into());
        s.agent_session_id = Some("abc".into());

        let busy = probe_with(&s, vec![live(AgentKind::Claude, "/repo", Some("abc"), "busy")], true);
        assert_eq!(session_status(&busy, &s, "/repo"), Status::Working);

        let idle = probe_with(&s, vec![live(AgentKind::Claude, "/repo", Some("abc"), "idle")], true);
        assert_eq!(session_status(&idle, &s, "/repo"), Status::YourTurn);
    }

    #[test]
    fn hooks_split_idle_into_blocked_and_finished() {
        // Without hooks, idle is ambiguous and stays vague. With them, the two
        // cases separate — which is the difference between a background session
        // you can leave alone and one that will wait forever.
        let mut s = Session::new(Uuid::new_v4(), AgentKind::Claude, "t".into());
        s.agent_session_id = Some("abc".into());
        let base = live(AgentKind::Claude, "/repo", Some("abc"), "idle");

        let vague = probe_with(&s, vec![base.clone()], true);
        assert_eq!(session_status(&vague, &s, "/repo"), Status::YourTurn);

        let blocked = probe_with(&s, vec![with_attention(base.clone(), "waiting")], true);
        assert_eq!(session_status(&blocked, &s, "/repo"), Status::NeedsYou);

        let finished = probe_with(&s, vec![with_attention(base, "done")], true);
        assert_eq!(session_status(&finished, &s, "/repo"), Status::Done);
    }

    #[test]
    fn a_blocked_agent_outranks_a_busy_status_line() {
        // An agent showing a permission prompt can still call itself busy. The
        // probe only attaches `waiting` when the hook was the fresher report,
        // so by the time it gets here it is the one to believe.
        let mut s = Session::new(Uuid::new_v4(), AgentKind::Claude, "t".into());
        s.agent_session_id = Some("abc".into());
        let busy = live(AgentKind::Claude, "/repo", Some("abc"), "busy");
        let p = probe_with(&s, vec![with_attention(busy, "waiting")], true);
        assert_eq!(session_status(&p, &s, "/repo"), Status::NeedsYou);
    }

    #[test]
    fn blocked_sorts_above_everything_else() {
        let order = [
            Status::NeedsYou,
            Status::Done,
            Status::YourTurn,
            Status::Working,
            Status::Up,
            Status::Down,
        ];
        let mut urgencies: Vec<u8> = order.iter().map(|s| s.urgency()).collect();
        let sorted = urgencies.clone();
        urgencies.sort();
        assert_eq!(urgencies, sorted, "urgency must already be in listed order");
        assert!(Status::NeedsYou.wants_you() && Status::Done.wants_you());
        assert!(!Status::Working.wants_you() && !Status::Down.wants_you());
    }

    #[test]
    fn two_agents_in_one_folder_are_not_guessed_apart() {
        // Without a conversation id there is no way to tell which process
        // belongs to this session, so the status must not claim to know.
        let s = Session::new(Uuid::new_v4(), AgentKind::Claude, "t".into());
        let p = probe_with(
            &s,
            vec![
                live(AgentKind::Claude, "/repo", None, "busy"),
                live(AgentKind::Claude, "/repo", None, "idle"),
            ],
            true,
        );
        assert_eq!(session_status(&p, &s, "/repo"), Status::Up);
    }

    #[test]
    fn a_lone_agent_in_the_folder_is_matched_by_directory() {
        let s = Session::new(Uuid::new_v4(), AgentKind::Claude, "t".into());
        let p = probe_with(&s, vec![live(AgentKind::Claude, "/repo", None, "busy")], true);
        assert_eq!(session_status(&p, &s, "/repo"), Status::Working);
    }

    #[test]
    fn codex_without_a_status_reports_running_not_a_guess() {
        let s = Session::new(Uuid::new_v4(), AgentKind::Codex, "t".into());
        let p = probe_with(&s, vec![live(AgentKind::Codex, "/repo", None, "unknown")], true);
        assert_eq!(session_status(&p, &s, "/repo"), Status::Up);
    }

    #[test]
    fn unreachable_host_becomes_a_note_not_a_silent_gap() {
        let host = Host::new("back".into(), Some("h".into()));
        let probes = vec![HostProbe {
            host: host.clone(),
            probe: None,
            error: Some("connection refused".into()),
        }];
        let rows = folders(&[host], &probes);
        assert!(matches!(&rows[0], Row::Note(t) if t.contains("connection refused")));
    }

    #[test]
    fn notes_are_never_selectable() {
        assert!(!Row::Note("x".into()).selectable());
    }
}
