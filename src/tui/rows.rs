//! What each screen shows.
//!
//! No state is decided here. The host that owns a session is the only place
//! that can see its tmux and its processes, so it resolves the state and this
//! module renders the answer — see [`crate::reconcile`]. Three copies of that
//! logic living in three layers is precisely what let them disagree.

use uuid::Uuid;

use crate::model::{AgentKind, Chat, Folder, Host, Layout, Probe};
use crate::reconcile::{SessionView, State};
use crate::remote::HostProbe;

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
        view: SessionView,
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
            Row::Session { host, view } => {
                format!("{host} {} {}", view.session.title, view.session.agent)
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
            Row::Session { host, view } => Some((host.clone(), view.session.id)),
            _ => None,
        }
    }
}

fn probe_of<'a>(probes: &'a [HostProbe], host: &str) -> Option<&'a Probe> {
    probes
        .iter()
        .find(|p| p.host.name == host)
        .and_then(|p| p.probe.as_ref())
}

/// The favourites screen: every marked folder on every host.
pub fn folders(hosts: &[Host], probes: &[HostProbe], show_hidden: bool) -> Vec<Row> {
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

        for folder in probe
            .folders
            .iter()
            .filter(|folder| show_hidden || !folder.hidden)
        {
            let here: Vec<&SessionView> = probe
                .sessions
                .iter()
                .filter(|v| v.session.folder_id == folder.id)
                .collect();

            rows.push(Row::Folder {
                host: host.name.clone(),
                folder: folder.clone(),
                sessions: here.len(),
                running: here.iter().filter(|v| v.state.is_running()).count(),
                attention: here.iter().filter(|v| v.state.wants_you()).count(),
                blocked: here.iter().filter(|v| v.state == State::NeedsYou).count(),
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
    let Some(probe) = probe_of(probes, host_name) else {
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

    let mut here: Vec<&SessionView> = probe
        .sessions
        .iter()
        .filter(|v| v.session.folder_id == folder_id)
        .collect();
    here.sort_by_key(|v| {
        std::cmp::Reverse(v.session.last_attached.unwrap_or(v.session.created_at))
    });

    for view in &here {
        rows.push(Row::Session {
            host: host_name.to_string(),
            view: (*view).clone(),
        });
    }

    // Conversations already adopted by a session would otherwise appear twice.
    let adopted: Vec<&str> = here
        .iter()
        .filter_map(|v| v.session.agent_session_id.as_deref())
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
        let Some(probe) = probe_of(probes, &host.name) else {
            continue;
        };
        for view in probe.sessions.iter().filter(|v| v.state.is_running()) {
            rows.push(Row::Session {
                host: host.name.clone(),
                view: view.clone(),
            });
        }
        // Something running that no record claims belongs on the screen that
        // lists what is running, not only in a warning nobody reads.
        for orphan in &probe.orphans {
            rows.push(Row::Note(format!(
                "{}: {} is running but bizik does not track it — tmux kill-session -t {}",
                host.name, orphan.tmux_name, orphan.tmux_name
            )));
        }
    }

    // Whatever wants you first; that is the only ordering that matters here.
    rows.sort_by_key(|r| match r {
        Row::Session { view, .. } => view.state.urgency(),
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
                    if probe.env_captured_at.is_none() && !probe.agents.is_empty() {
                        detail.push_str(" · no PATH captured");
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
                    !probe_of(probes, &p.host)
                        .is_some_and(|probe| probe.records().any(|s| s.id == p.session))
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
    use crate::model::Session;
    use crate::reconcile::Orphan;

    fn view(state: State) -> SessionView {
        SessionView {
            session: Session::new(Uuid::new_v4(), AgentKind::Claude, "t".into()),
            state,
            preview: None,
            attention: None,
        }
    }

    fn probed(host: &str, probe: Option<Probe>, error: Option<&str>) -> HostProbe {
        HostProbe {
            host: Host::new(host.into(), Some("h".into())),
            probe,
            error: error.map(str::to_string),
        }
    }

    #[test]
    fn unreachable_host_becomes_a_note_not_a_silent_gap() {
        let host = Host::new("back".into(), Some("h".into()));
        let rows = folders(
            &[host],
            &[probed("back", None, Some("connection refused"))],
            false,
        );
        assert!(matches!(&rows[0], Row::Note(t) if t.contains("connection refused")));
    }

    #[test]
    fn hidden_projects_only_appear_when_requested() {
        let host = Host::new("local".into(), None);
        let visible = Folder::new("/visible".into());
        let mut hidden = Folder::new("/hidden".into());
        hidden.hidden = true;
        let probe = Probe {
            folders: vec![visible, hidden],
            ..Probe::default()
        };
        let probes = [probed("local", Some(probe), None)];

        assert_eq!(
            folders(std::slice::from_ref(&host), &probes, false).len(),
            1
        );
        assert_eq!(folders(std::slice::from_ref(&host), &probes, true).len(), 2);
    }

    #[test]
    fn notes_are_never_selectable() {
        assert!(!Row::Note("x".into()).selectable());
    }

    #[test]
    fn the_running_screen_puts_blocked_sessions_first() {
        let host = Host::new("back".into(), Some("h".into()));
        let probe = Probe {
            sessions: vec![
                view(State::Working),
                view(State::NeedsYou),
                view(State::YourTurn),
            ],
            ..Default::default()
        };
        let rows = running(&[host], &[probed("back", Some(probe), None)]);
        match &rows[0] {
            Row::Session { view, .. } => assert_eq!(view.state, State::NeedsYou),
            other => panic!("expected a session first, got {other:?}"),
        }
    }

    #[test]
    fn stopped_sessions_are_not_on_the_running_screen() {
        let host = Host::new("back".into(), Some("h".into()));
        let probe = Probe {
            sessions: vec![view(State::Down)],
            ..Default::default()
        };
        let rows = running(&[host], &[probed("back", Some(probe), None)]);
        assert!(matches!(&rows[0], Row::Note(t) if t.contains("nothing is running")));
    }

    #[test]
    fn an_orphan_is_listed_where_running_things_are_listed() {
        let host = Host::new("back".into(), Some("h".into()));
        let _ = &host;
        let probe = Probe {
            orphans: vec![Orphan {
                tmux_name: "bzk-dead1234".into(),
                was_session: None,
            }],
            ..Default::default()
        };
        let rows = running(&[host], &[probed("back", Some(probe), None)]);
        assert!(
            rows.iter()
                .any(|r| matches!(r, Row::Note(t) if t.contains("bzk-dead1234"))),
            "an untracked session must not be visible only in a warning"
        );
    }

    #[test]
    fn a_layout_counts_panes_whose_sessions_are_gone() {
        let present = view(State::Down);
        let probe = Probe {
            sessions: vec![present.clone()],
            ..Default::default()
        };
        let layout = Layout::new(
            "two".into(),
            vec![
                crate::model::PaneRef {
                    host: "back".into(),
                    session: present.session.id,
                },
                crate::model::PaneRef {
                    host: "back".into(),
                    session: Uuid::new_v4(),
                },
            ],
            None,
        );
        let rows = layouts(&[layout], &[probed("back", Some(probe), None)]);
        match &rows[0] {
            Row::LayoutEntry { missing, .. } => assert_eq!(*missing, 1),
            other => panic!("expected a layout, got {other:?}"),
        }
    }
}
