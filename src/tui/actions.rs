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
    if let Some(pane) = existing_pane(&window, session) {
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
/// Read from the identity tmux carries on the pane, not from its command line.
/// Substring matching worked until it did not: two sessions whose names shared
/// a prefix, or a change to how the command is quoted, would silently pair a
/// pane with the wrong session.
fn existing_pane(window: &str, session: Uuid) -> Option<String> {
    let wanted = session.to_string();
    tmux::window_panes(window)
        .ok()?
        .into_iter()
        .find(|p| p.session.as_deref() == Some(wanted.as_str()))
        .map(|p| p.pane)
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
pub fn panes_in_work() -> Vec<OpenPane> {
    let Some(window) = tmux::find_window(WORK_WINDOW) else {
        return Vec::new();
    };
    let Ok(panes) = tmux::window_panes(&window) else {
        return Vec::new();
    };

    panes
        .into_iter()
        .filter_map(|p| {
            Some(OpenPane {
                session: p.session?.parse().ok()?,
                host: p.host?,
                pane: p.pane,
            })
        })
        .collect()
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
        let candidates = [("back".to_string(), s.clone())];
        let hit = candidates
            .iter()
            .find(|(_, c)| cmd.contains(&c.tmux_name()));
        assert_eq!(hit.map(|(h, _)| h.as_str()), Some("back"));
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
