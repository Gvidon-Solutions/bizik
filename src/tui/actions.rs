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
const WORK_SESSION: &str = "bizik";
const DEFAULT_SIDEBAR_WIDTH: u16 = 30;
const SIDEBAR_HIDDEN: &str = "@bzk_sidebar_hidden";
const SIDEBAR_WIDTH: &str = "@bzk_sidebar_width";

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

pub fn set_project_pinned(host: &Host, folder: Uuid, pinned: bool) -> Result<()> {
    let folder = folder.to_string();
    let pinned = pinned.to_string();
    remote::run_bzk(
        host,
        &["update-folder", "--folder", &folder, "--pinned", &pinned],
    )
    .map(|_| ())
}

pub fn set_project_hidden(host: &Host, folder: Uuid, hidden: bool) -> Result<()> {
    let folder = folder.to_string();
    let hidden = hidden.to_string();
    remote::run_bzk(
        host,
        &["update-folder", "--folder", &folder, "--hidden", &hidden],
    )
    .map(|_| ())
}

/// Put one layout viewer for `tmux_name` beside the project sidebar.
///
/// This is the additive primitive used while restoring a layout or an explicit
/// multi-selection. Ordinary session navigation must use `open_standalone` so
/// switching sessions replaces the previous workspace instead of fragmenting
/// it. Reopening a session focuses its existing viewer instead of duplicating
/// it.
pub fn open_pane(host: &Host, session: Uuid, tmux_name: &str) -> Result<String> {
    let cmd = remote::attach_command(host, tmux_name);
    let (window, panes, created) = if let Some(window) = work_window() {
        let panes = tmux::window_panes(&window)?;
        remember_sidebar_width(&window, &panes);
        (window, panes, None)
    } else {
        let viewer = create_work_window(&cmd)?;
        let window = work_window().context("the work window was not created")?;
        (window, Vec::new(), Some(viewer))
    };

    let existing = panes
        .iter()
        .find(|pane| identifies(pane, &session.to_string(), tmux_name))
        .map(|pane| pane.pane.clone());
    let sidebar = panes
        .iter()
        .find(|pane| pane.role.as_deref() == Some(SIDEBAR_ROLE))
        .map(|pane| pane.pane.clone());

    let viewer = if let Some(pane) = created {
        pane
    } else if let Some(pane) = existing {
        tmux::select_pane(&pane)?;
        return Ok(pane);
    } else if let Some(pane) = panes
        .iter()
        .find(|pane| pane.role.as_deref() == Some(VIEWER_ROLE))
    {
        tmux::split_window(&pane.pane, &cmd)?
    } else if let Some(sidebar) = sidebar {
        tmux::split_window_right(&sidebar, &cmd)?
    } else if let Some(pane) = panes.first() {
        // Migrate a workspace created by a build that predates pane roles.
        tmux::split_window(&pane.pane, &cmd)?
    } else {
        anyhow::bail!("the work window exists but has no panes")
    };

    tmux::tag_pane(&viewer, &session.to_string(), &host.name)?;
    tmux::tag_pane_role(&viewer, VIEWER_ROLE)?;
    if !sidebar_hidden(&window) {
        let _ = ensure_sidebar(&window, &viewer)?;
    }
    tile_window(&window)?;
    tmux::style_app_window(&window);
    tmux::select_pane(&viewer)?;
    Ok(viewer)
}

fn create_work_window(cmd: &str) -> Result<String> {
    if tmux::inside_tmux() {
        return tmux::new_window(WORK_WINDOW, cmd);
    }
    let session = tmux::SessionRef::new(WORK_SESSION);
    if tmux::has_session(&session) {
        return tmux::new_window_in(&session, WORK_WINDOW, cmd);
    }
    tmux::new_detached_session(&session, WORK_WINDOW, cmd)?;
    let window = tmux::find_window_in(Some(&session), WORK_WINDOW)
        .context("the detached work window was not created")?;
    tmux::window_panes(&window)?
        .into_iter()
        .next()
        .map(|pane| pane.pane)
        .context("the detached work window has no pane")
}

/// Show exactly one session, removing only local viewer clients.
///
/// The detached sessions on their hosts remain untouched. This is the
/// "standalone" escape hatch from a busy layout.
pub fn open_standalone(host: &Host, session: Uuid, tmux_name: &str) -> Result<String> {
    if let Some(window) = work_window() {
        for pane in tmux::window_panes(&window)? {
            if pane.role.as_deref() == Some(VIEWER_ROLE)
                && !(pane.session.as_deref() == Some(&session.to_string())
                    && pane.host.as_deref() == Some(host.name.as_str()))
            {
                tmux::kill_pane(&pane.pane)?;
            }
        }
    }
    open_pane(host, session, tmux_name)
}

/// Focus an existing viewer without changing the workspace composition.
///
/// Layout children use this when their complete layout is already visible.
/// Returning `false` tells the caller that the layout needs restoring first.
pub fn focus_open_session(host: &str, session: Uuid) -> Result<bool> {
    let Some(window) = work_window() else {
        return Ok(false);
    };
    let session = session.to_string();
    let Some(pane) = tmux::window_panes(&window)?.into_iter().find(|pane| {
        pane.role.as_deref() == Some(VIEWER_ROLE)
            && pane.host.as_deref() == Some(host)
            && pane.session.as_deref() == Some(session.as_str())
    }) else {
        return Ok(false);
    };
    tmux::select_window(&window)?;
    tmux::select_pane(&pane.pane)?;
    Ok(true)
}

/// Whether the workspace currently contains exactly this layout's viewers.
pub fn workspace_has_exact_panes(wanted: &[crate::model::PaneRef]) -> bool {
    let Some(window) = work_window() else {
        return false;
    };
    let Ok(panes) = tmux::window_panes(&window) else {
        return false;
    };
    let wanted: std::collections::HashSet<(String, Uuid)> = wanted
        .iter()
        .map(|pane| (pane.host.clone(), pane.session))
        .collect();
    let viewers: Vec<_> = panes
        .iter()
        .filter(|pane| pane.role.as_deref() == Some(VIEWER_ROLE))
        .collect();
    if viewers.len() != wanted.len() {
        return false;
    }
    viewers.iter().all(|pane| {
        let Some(host) = pane.host.as_deref() else {
            return false;
        };
        let Some(session) = pane.session.as_deref().and_then(|id| id.parse().ok()) else {
            return false;
        };
        wanted.contains(&(host.to_string(), session))
    })
}

/// Close viewer clients that do not belong to the layout being restored.
///
/// A duplicate is surplus after the first matching pane. The sidebar and the
/// detached agent sessions are never touched.
pub fn retain_layout_panes(wanted: &[crate::model::PaneRef]) -> Result<()> {
    let Some(window) = work_window() else {
        return Ok(());
    };
    let wanted: std::collections::HashSet<(String, Uuid)> = wanted
        .iter()
        .map(|pane| (pane.host.clone(), pane.session))
        .collect();
    let mut seen = std::collections::HashSet::new();
    for pane in tmux::window_panes(&window)? {
        if pane.role.as_deref() != Some(VIEWER_ROLE) {
            continue;
        }
        let Some(host) = pane.host.clone() else {
            tmux::kill_pane(&pane.pane)?;
            continue;
        };
        let Some(session) = pane.session.as_deref().and_then(|id| id.parse().ok()) else {
            tmux::kill_pane(&pane.pane)?;
            continue;
        };
        let key = (host, session);
        if !wanted.contains(&key) || !seen.insert(key) {
            tmux::kill_pane(&pane.pane)?;
        }
    }
    Ok(())
}

fn ensure_sidebar(window: &str, viewer: &str) -> Result<String> {
    if let Some(sidebar) = tmux::window_panes(window)?
        .iter()
        .find(|pane| pane.role.as_deref() == Some(SIDEBAR_ROLE))
    {
        return Ok(sidebar.pane.clone());
    }
    let sidebar = tmux::split_window_left(
        viewer,
        remembered_sidebar_width(window),
        &navigator_command()?,
    )?;
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

fn configured_sidebar_width() -> u16 {
    std::env::var("BIZIK_SIDEBAR_WIDTH")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(DEFAULT_SIDEBAR_WIDTH)
        .clamp(20, 60)
}

fn remembered_sidebar_width(window: &str) -> u16 {
    tmux::window_option(window, SIDEBAR_WIDTH)
        .and_then(|value| value.parse().ok())
        .unwrap_or_else(configured_sidebar_width)
        .max(20)
}

/// Persist a user-adjusted width only while both workspace panes exist.
///
/// When an active agent session is deleted, its attach client exits first and
/// tmux briefly stretches the sidebar across the entire window. That transient
/// geometry must never replace the user's real sidebar width.
fn remember_sidebar_width(window: &str, panes: &[tmux::PaneInfo]) {
    let Some(sidebar) = stable_sidebar_pane(panes) else {
        return;
    };
    if let Some(width) = tmux::pane_width(sidebar) {
        let _ = tmux::set_window_option(window, SIDEBAR_WIDTH, &width.to_string());
    }
}

fn stable_sidebar_pane(panes: &[tmux::PaneInfo]) -> Option<&str> {
    let has_viewer = panes
        .iter()
        .any(|pane| pane.role.as_deref() == Some(VIEWER_ROLE));
    if !has_viewer {
        return None;
    }
    panes
        .iter()
        .find(|pane| pane.role.as_deref() == Some(SIDEBAR_ROLE))
        .map(|pane| pane.pane.as_str())
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
        remember_sidebar_width(&window, &panes);
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

/// Show the sidebar when necessary and move keyboard focus into it.
pub fn focus_sidebar() -> Result<()> {
    let window = work_window().context("the workspace is not open yet")?;
    let panes = tmux::window_panes(&window)?;
    let sidebar = if let Some(sidebar) = panes
        .iter()
        .find(|pane| pane.role.as_deref() == Some(SIDEBAR_ROLE))
    {
        sidebar.pane.clone()
    } else {
        let viewer = panes
            .iter()
            .find(|pane| pane.role.as_deref() == Some(VIEWER_ROLE))
            .context("open a session before focusing the sidebar")?;
        tmux::set_window_option(&window, SIDEBAR_HIDDEN, "0")?;
        ensure_sidebar(&window, &viewer.pane)?
    };
    tmux::select_window(&window)?;
    tmux::select_pane(&sidebar)
}

/// Move keyboard focus from the sidebar back to the active agent.
pub fn focus_viewer() -> Result<()> {
    let window = work_window().context("the workspace is not open yet")?;
    let panes = tmux::window_panes(&window)?;
    let viewer = panes
        .iter()
        .find(|pane| pane.role.as_deref() == Some(VIEWER_ROLE) && pane.active)
        .or_else(|| {
            panes
                .iter()
                .find(|pane| pane.role.as_deref() == Some(VIEWER_ROLE) && pane.last)
        })
        .or_else(|| {
            panes
                .iter()
                .find(|pane| pane.role.as_deref() == Some(VIEWER_ROLE))
        })
        .context("no active session is open")?;
    tmux::select_window(&window)?;
    tmux::select_pane(&viewer.pane)
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

/// Session currently occupying the focused viewer pane.
pub fn active_session() -> Option<(String, Uuid)> {
    let window = work_window()?;
    let panes = tmux::window_panes(&window).ok()?;
    panes
        .iter()
        .find(|pane| pane.role.as_deref() == Some(VIEWER_ROLE) && pane.active)
        .or_else(|| {
            panes
                .iter()
                .find(|pane| pane.role.as_deref() == Some(VIEWER_ROLE) && pane.last)
        })
        .or_else(|| {
            panes
                .iter()
                .find(|pane| pane.role.as_deref() == Some(VIEWER_ROLE))
        })
        .and_then(|pane| Some((pane.host.clone()?, pane.session.as_deref()?.parse().ok()?)))
}

/// Geometry of the whole workspace, including the stable sidebar pane.
pub fn work_layout() -> Option<String> {
    let window = work_window()?;
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

/// Apply an exact saved geometry, including its sidebar/viewer split.
pub fn apply_geometry(geometry: &str) -> Result<()> {
    let window = work_window().context("the pane window is gone")?;
    tmux::select_layout(&window, geometry)?;
    remember_current_sidebar_width(&window);
    Ok(())
}

pub fn tile() {
    if let Some(window) = work_window() {
        let _ = tile_window(&window);
    }
}

fn tile_window(window: &str) -> Result<()> {
    let panes = tmux::window_panes(window)?;
    let sidebar = panes
        .iter()
        .find(|pane| pane.role.as_deref() == Some(SIDEBAR_ROLE));
    if let Some(sidebar) = sidebar {
        // `main-vertical` keeps one full-height pane on the left and stacks the
        // remaining viewers on the right. Selecting the sidebar first makes it
        // that main pane on supported tmux versions.
        tmux::select_pane(&sidebar.pane)?;
        tmux::select_layout(window, "main-vertical")?;
    } else {
        tmux::select_layout(window, "tiled")?;
    }
    fit_workspace(window)
}

fn remember_current_sidebar_width(window: &str) {
    if let Ok(panes) = tmux::window_panes(window) {
        remember_sidebar_width(window, &panes);
    }
}

fn fit_workspace(window: &str) -> Result<()> {
    let panes = tmux::window_panes(window)?;
    if let Some(sidebar) = panes
        .iter()
        .find(|pane| pane.role.as_deref() == Some(SIDEBAR_ROLE))
    {
        let _ = tmux::resize_pane_width(&sidebar.pane, remembered_sidebar_width(window));
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

    #[test]
    fn a_temporarily_full_width_sidebar_is_not_remembered() {
        let sidebar = pane_with_role("%1", SIDEBAR_ROLE);
        assert_eq!(stable_sidebar_pane(&[sidebar]), None);

        let sidebar = pane_with_role("%1", SIDEBAR_ROLE);
        let viewer = pane_with_role("%2", VIEWER_ROLE);
        assert_eq!(stable_sidebar_pane(&[sidebar, viewer]), Some("%1"));
    }

    fn pane_with_role(id: &str, role: &str) -> tmux::PaneInfo {
        tmux::PaneInfo {
            pane: id.into(),
            session: None,
            host: None,
            role: Some(role.into()),
            active: false,
            last: false,
            start_command: String::new(),
        }
    }

    fn pane(tag: Option<&str>, command: &str) -> tmux::PaneInfo {
        tmux::PaneInfo {
            pane: "%1".into(),
            session: tag.map(str::to_string),
            host: tag.map(|_| "back".to_string()),
            role: Some(VIEWER_ROLE.into()),
            active: false,
            last: false,
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
