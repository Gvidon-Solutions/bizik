//! Side effects the dashboard performs: starting sessions on hosts and putting
//! panes on the screen.
//!
//! Starting and viewing are deliberately separate steps. A session is started
//! detached on its host and keeps running whether or not anything is looking at
//! it — which is what makes launching several at once, or closing the laptop,
//! harmless.

use anyhow::{Context, Result};
use uuid::Uuid;

use crate::hostops::SpawnResult;
use crate::model::{AgentKind, Host, Session};
use crate::remote;
use crate::tmux;

/// Panes live in their own window, so the dashboard never has to shrink itself
/// to make room for the work it launches.
pub const WORK_WINDOW: &str = "bzk-work";

/// Create a session record on a host.
pub fn create_session(
    host: &Host,
    folder: Uuid,
    agent: AgentKind,
    title: Option<&str>,
    resume: Option<&str>,
) -> Result<Session> {
    let folder = folder.to_string();
    let mut args: Vec<&str> = vec![
        "new-session",
        "--folder",
        &folder,
        "--agent",
        agent.as_str(),
    ];
    if let Some(t) = title {
        args.extend(["--title", t]);
    }
    if let Some(r) = resume {
        args.extend(["--resume", r]);
    }
    let out = remote::run_bzk(host, &args)?;
    serde_json::from_str(out.trim()).context("parsing the new session record")
}

/// Start a session on its host if it is not already up.
pub fn start(host: &Host, session: Uuid) -> Result<SpawnResult> {
    let id = session.to_string();
    // The host label travels with the request: a server has no way to know what
    // this laptop calls it, and that is the name worth showing in the pane.
    let out = remote::run_bzk(
        host,
        &["spawn", "--session", &id, "--host-label", &host.name],
    )?;
    serde_json::from_str(out.trim()).context("parsing the spawn result")
}

pub fn stop(host: &Host, session: Uuid) -> Result<()> {
    let id = session.to_string();
    remote::run_bzk(host, &["stop", "--session", &id]).map(|_| ())
}

pub fn forget(host: &Host, session: Uuid) -> Result<()> {
    let id = session.to_string();
    remote::run_bzk(host, &["rm-session", "--session", &id]).map(|_| ())
}

pub fn unmark(host: &Host, path: &str) -> Result<()> {
    remote::run_bzk(host, &["unmark", path]).map(|_| ())
}

pub fn relabel(host: &Host, path: &str, label: &str) -> Result<()> {
    remote::run_bzk(host, &["mark", path, "--label", label]).map(|_| ())
}

/// Put a viewer for `tmux_name` on screen and return the new pane's id.
///
/// Every pane is tiled into one window so that several sessions really are
/// visible at once, which is the whole point — a list of what is running is not
/// a substitute for watching it run.
pub fn open_pane(host: &Host, session: Uuid, tmux_name: &str) -> Result<String> {
    let cmd = remote::attach_command(host, tmux_name);
    let tag = |pane: &str| {
        let _ = tmux::tag_pane(pane, &session.to_string(), &host.name);
    };

    let Some(window) = tmux::find_window(WORK_WINDOW) else {
        let pane = tmux::new_window(WORK_WINDOW, &cmd)?;
        tag(&pane);
        return Ok(pane);
    };

    // A session already on screen is focused rather than opened again. Two
    // panes showing one conversation is never what was meant, and it is easy to
    // ask for by selecting a session that was already open.
    if let Some(pane) = existing_pane(&window, session, tmux_name, &host.name) {
        tmux::select_pane(&pane)?;
        return Ok(pane);
    }

    let pane = tmux::split_window(&window, &cmd)?;
    tag(&pane);
    tmux::select_layout(&window, "tiled")?;
    Ok(pane)
}

/// The pane already viewing this session, if any.
///
/// The tag tmux carries on the pane is the answer when it is there. A pane
/// opened by an older build has no tag, and ignoring those was not harmless:
/// restoring a layout over them saw an empty window, opened its own panes
/// alongside, and left eight where four were meant. So an untagged pane is
/// recognised by the session name still visible in its start command, and
/// tagged on the spot — after which it is exact like the rest.
fn existing_pane(window: &str, session: Uuid, tmux_name: &str, host: &str) -> Option<String> {
    let wanted = session.to_string();
    let panes = tmux::window_panes(window).ok()?;
    let found = panes.iter().find(|p| identifies(p, &wanted, tmux_name))?;

    if found.session.is_none() {
        let _ = tmux::tag_pane(&found.pane, &wanted, host);
    }
    Some(found.pane.clone())
}

/// Whether this pane is showing that session, by tag or by what it was started
/// with.
fn identifies(pane: &tmux::PaneInfo, session_uuid: &str, tmux_name: &str) -> bool {
    if let Some(tagged) = &pane.session {
        return tagged == session_uuid;
    }
    pane.start_command.contains(&format!("'={tmux_name}'"))
}

/// Switch the terminal to the pane window.
pub fn focus_work() -> Result<()> {
    let window = tmux::find_window(WORK_WINDOW).context("no panes are open yet")?;
    tmux::select_window(&window)
}

/// Geometry of the pane window, for saving a layout.
pub fn work_layout() -> Option<String> {
    let window = tmux::find_window(WORK_WINDOW)?;
    tmux::capture_layout(&window).ok()
}

/// One open pane and the session it is showing.
pub struct OpenPane {
    pub pane: String,
    pub host: String,
    pub session: Uuid,
}

/// Which session each open pane is showing.
///
/// tmux carries the answer on the pane itself, tagged when the pane was opened.
/// It survives a dashboard restart, so a layout can still be saved from panes
/// opened by an earlier run.
pub fn panes_in_work(candidates: &[(String, Session)]) -> Vec<OpenPane> {
    let Some(window) = tmux::find_window(WORK_WINDOW) else {
        return Vec::new();
    };
    let Ok(panes) = tmux::window_panes(&window) else {
        return Vec::new();
    };

    panes
        .into_iter()
        .filter_map(|p| {
            let (host, session) = identify_pane(&p, candidates)?;
            // Recovered panes are tagged as they are found, so this is the last
            // time any of them needs recovering.
            if p.session.is_none() {
                let _ = tmux::tag_pane(&p.pane, &session.to_string(), &host);
            }
            Some(OpenPane {
                pane: p.pane,
                host,
                session,
            })
        })
        .collect()
}

/// Which session a pane is showing.
///
/// The tag tmux carries answers for itself. A pane opened by an older build has
/// none, and is recovered from the session name still visible in its start
/// command.
pub fn identify_pane(
    p: &tmux::PaneInfo,
    candidates: &[(String, Session)],
) -> Option<(String, Uuid)> {
    if let (Some(session), Some(host)) = (&p.session, &p.host)
        && let Ok(id) = session.parse()
    {
        return Some((host.clone(), id));
    }
    candidates
        .iter()
        .find(|(_, s)| p.start_command.contains(&format!("'={}'", s.tmux_name())))
        .map(|(host, s)| (host.clone(), s.id))
}

/// Apply a saved geometry to the pane window.
pub fn apply_geometry(geometry: &str) -> Result<()> {
    let window = tmux::find_window(WORK_WINDOW).context("the pane window is gone")?;
    tmux::select_layout(&window, geometry)
}

pub fn tile() {
    if let Some(window) = tmux::find_window(WORK_WINDOW) {
        let _ = tmux::select_layout(&window, "tiled");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pane_mapping_matches_on_tmux_session_name() {
        let s = Session::new(Uuid::new_v4(), AgentKind::Claude, "t".into());
        let host = Host::new("back".into(), Some("root@h".into()));
        // The real pane command, so the test breaks if its shape changes.
        let cmd = crate::remote::attach_command(&host, &s.tmux_name());
        let candidates = [("back".to_string(), s)];
        let hit = candidates
            .iter()
            .find(|(_, c)| cmd.contains(&c.tmux_name()));
        assert_eq!(hit.map(|(h, _)| h.as_str()), Some("back"));
    }

    fn pane(tag: Option<&str>, command: &str) -> tmux::PaneInfo {
        tmux::PaneInfo {
            pane: "%1".into(),
            session: tag.map(str::to_string),
            host: tag.map(|_| "back".to_string()),
            start_command: command.to_string(),
        }
    }

    #[test]
    fn a_pane_from_an_older_build_is_still_recognised() {
        // It carries no tag, only the command it was started with. Ignoring
        // those made a layout restore open its panes alongside the ones already
        // there — eight where four were meant.
        let s = Session::new(Uuid::new_v4(), AgentKind::Claude, "t".into());
        let host = Host::new("back".into(), Some("root@h".into()));
        let legacy = pane(None, &crate::remote::attach_command(&host, &s.tmux_name()));

        assert!(identifies(&legacy, &s.id.to_string(), &s.tmux_name()));
    }

    #[test]
    fn a_tag_is_believed_over_the_command_line() {
        // Once tagged, the tag is the answer — a stale command line cannot
        // reassign a pane to somebody else's session.
        let mine = Session::new(Uuid::new_v4(), AgentKind::Claude, "t".into());
        let other = Session::new(Uuid::new_v4(), AgentKind::Claude, "u".into());
        let host = Host::new("back".into(), Some("root@h".into()));

        let tagged = pane(
            Some(&other.id.to_string()),
            &crate::remote::attach_command(&host, &mine.tmux_name()),
        );
        assert!(!identifies(
            &tagged,
            &mine.id.to_string(),
            &mine.tmux_name()
        ));
        assert!(identifies(
            &tagged,
            &other.id.to_string(),
            &other.tmux_name()
        ));
    }

    #[test]
    fn an_unrelated_pane_matches_nothing() {
        let s = Session::new(Uuid::new_v4(), AgentKind::Claude, "t".into());
        let shell = pane(None, "zsh");
        assert!(!identifies(&shell, &s.id.to_string(), &s.tmux_name()));
    }

    #[test]
    fn an_already_open_session_is_recognised_in_a_pane_command() {
        let s = Session::new(Uuid::new_v4(), AgentKind::Claude, "t".into());
        let other = Session::new(Uuid::new_v4(), AgentKind::Claude, "u".into());
        let host = Host::new("back".into(), Some("root@h".into()));
        let cmd = crate::remote::attach_command(&host, &s.tmux_name());

        assert!(cmd.contains(&format!("'={}'", s.tmux_name())));
        assert!(
            !cmd.contains(&format!("'={}'", other.tmux_name())),
            "a different session must not be mistaken for this one"
        );
    }
}
