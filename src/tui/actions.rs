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
use crate::{tmux, util};

/// Panes live in their own window, so the dashboard never has to shrink itself
/// to make room for the work it launches.
pub const WORK_WINDOW: &str = "bzk-work";
pub const SIDEBAR_ROLE: &str = "sidebar";
pub const VIEWER_ROLE: &str = "viewer";
const DEFAULT_SIDEBAR_WIDTH: u16 = 30;
const SIDEBAR_HIDDEN: &str = "@bzk_sidebar_hidden";

fn work_window() -> Option<String> {
    tmux::find_window(WORK_WINDOW)
        .or_else(|| tmux::find_window_in(Some(&tmux::SessionRef::new("bizik")), WORK_WINDOW))
}

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

pub fn rename_session(host: &Host, session: Uuid, title: &str) -> Result<()> {
    let id = session.to_string();
    remote::run_bzk(
        host,
        &["rename-session", "--session", &id, "--title", title],
    )
    .map(|_| ())
}

pub fn unmark(host: &Host, path: &str) -> Result<()> {
    remote::run_bzk(host, &["unmark", path]).map(|_| ())
}

pub fn relabel(host: &Host, path: &str, label: &str) -> Result<()> {
    remote::run_bzk(host, &["mark", path, "--label", label]).map(|_| ())
}

/// Put one viewer for `tmux_name` beside the project sidebar.
///
/// There is deliberately only one viewer pane. Switching sessions respawns
/// that viewer while every agent keeps running in its detached host-side tmux
/// session. New sessions therefore appear in the tree instead of repeatedly
/// splitting the terminal.
pub fn open_pane(host: &Host, session: Uuid, tmux_name: &str) -> Result<String> {
    let cmd = remote::attach_command(host, tmux_name);
    let (window, panes, created) = if let Some(window) = work_window() {
        let panes = tmux::window_panes(&window)?;
        (window, panes, None)
    } else {
        let viewer = tmux::new_window(WORK_WINDOW, &cmd)?;
        let window = work_window().context("the work window was not created")?;
        (window, Vec::new(), Some(viewer))
    };

    let existing = panes
        .iter()
        .find(|pane| identifies(pane, &session.to_string(), tmux_name))
        .map(|pane| pane.pane.clone());
    let reusable = panes
        .iter()
        .find(|pane| pane.role.as_deref() == Some(VIEWER_ROLE))
        .map(|pane| pane.pane.clone());
    let sidebar = panes
        .iter()
        .find(|pane| pane.role.as_deref() == Some(SIDEBAR_ROLE))
        .map(|pane| pane.pane.clone());

    let viewer = if let Some(pane) = created {
        pane
    } else if let Some(pane) = existing {
        pane
    } else if let Some(pane) = reusable {
        tmux::respawn_pane(&pane, &cmd)?;
        pane
    } else if let Some(sidebar) = sidebar {
        // Closing the active session lets its attach pane exit before the next
        // tab is opened. At that point the sidebar owns the whole lower row;
        // split it horizontally to rebuild the viewer beside it.
        tmux::split_window_right(&sidebar, &cmd)?
    } else if let Some(pane) = panes.first() {
        // Migrate a workspace created by a build that still had a top tab
        // strip. The pane itself is only a navigator, so it can become the
        // viewer without touching any detached agent session.
        tmux::respawn_pane(&pane.pane, &cmd)?;
        pane.pane.clone()
    } else {
        anyhow::bail!("the work window exists but has no panes")
    };

    // Old builds may have left a top tab strip or several tiled viewers. They
    // are only clients; closing them never stops host-side agent sessions.
    for pane in panes {
        if pane.pane != viewer && pane.role.as_deref() != Some(SIDEBAR_ROLE) {
            let _ = tmux::kill_pane(&pane.pane);
        }
    }

    tmux::tag_pane(&viewer, &session.to_string(), &host.name)?;
    tmux::tag_pane_role(&viewer, VIEWER_ROLE)?;
    if !sidebar_hidden(&window) {
        let _ = ensure_sidebar(&window, &viewer)?;
    }
    fit_workspace(&window)?;
    tmux::select_pane(&viewer)?;
    Ok(viewer)
}

fn ensure_sidebar(window: &str, viewer: &str) -> Result<String> {
    if let Some(sidebar) = tmux::window_panes(window)?
        .iter()
        .find(|pane| pane.role.as_deref() == Some(SIDEBAR_ROLE))
    {
        return Ok(sidebar.pane.clone());
    }
    let sidebar = tmux::split_window_left(viewer, sidebar_width(), &navigator_command()?)?;
    tmux::tag_pane_role(&sidebar, SIDEBAR_ROLE)?;
    Ok(sidebar)
}

fn navigator_command() -> Result<String> {
    let exe = util::own_exe()?;
    Ok(format!(
        "{}{} sidebar",
        util::config_env(),
        util::shell_quote(&exe.to_string_lossy())
    ))
}

fn sidebar_width() -> u16 {
    std::env::var("BIZIK_SIDEBAR_WIDTH")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(DEFAULT_SIDEBAR_WIDTH)
        .clamp(20, 60)
}

fn sidebar_hidden(window: &str) -> bool {
    tmux::window_option(window, SIDEBAR_HIDDEN).as_deref() == Some("1")
}

pub fn sidebar_key() -> String {
    std::env::var("BIZIK_SIDEBAR_KEY").unwrap_or_else(|_| "F10".into())
}

pub fn toggle_sidebar() -> Result<()> {
    let window = work_window().context("the workspace is not open yet")?;
    let panes = tmux::window_panes(&window)?;
    if let Some(sidebar) = panes
        .iter()
        .find(|pane| pane.role.as_deref() == Some(SIDEBAR_ROLE))
    {
        tmux::set_window_option(&window, SIDEBAR_HIDDEN, "1")?;
        tmux::kill_pane(&sidebar.pane)?;
    } else {
        let viewer = panes
            .iter()
            .find(|pane| pane.role.as_deref() == Some(VIEWER_ROLE))
            .context("open a session before showing the sidebar")?;
        tmux::set_window_option(&window, SIDEBAR_HIDDEN, "0")?;
        ensure_sidebar(&window, &viewer.pane)?;
    }
    fit_workspace(&window)
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
    let window = work_window().context("no panes are open yet")?;
    tmux::select_window(&window)
}

/// Session currently occupying the single viewer pane.
pub fn active_session() -> Option<(String, Uuid)> {
    let window = work_window()?;
    tmux::window_panes(&window)
        .ok()?
        .into_iter()
        .find(|pane| pane.role.as_deref() == Some(VIEWER_ROLE))
        .and_then(|pane| Some((pane.host?, pane.session?.parse().ok()?)))
}

/// A sidebar workspace has a fixed shape, not a user-managed pane geometry.
pub fn work_layout() -> Option<String> {
    None
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
    let Some(window) = work_window() else {
        return Vec::new();
    };
    let Ok(panes) = tmux::window_panes(&window) else {
        return Vec::new();
    };

    panes
        .into_iter()
        .filter(|pane| pane.role.as_deref() == Some(VIEWER_ROLE) || pane.role.is_none())
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

/// Old saved layouts may contain tiled geometry. The current workspace always
/// keeps the sidebar at a stable width and shows one active session.
pub fn apply_geometry(_geometry: &str) -> Result<()> {
    fit_sidebar()
}

pub fn tile() {
    let _ = fit_sidebar();
}

fn fit_sidebar() -> Result<()> {
    let window = work_window().context("the pane window is gone")?;
    fit_workspace(&window)
}

fn fit_workspace(window: &str) -> Result<()> {
    let panes = tmux::window_panes(window)?;
    if let Some(sidebar) = panes
        .iter()
        .find(|pane| pane.role.as_deref() == Some(SIDEBAR_ROLE))
    {
        let _ = tmux::resize_pane_width(&sidebar.pane, sidebar_width());
    }
    Ok(())
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
            role: Some(VIEWER_ROLE.into()),
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
