//! tmux driver.
//!
//! bizik does not render panes. tmux does that far better than a hand-rolled
//! multiplexer ever would, and an agent TUI nested inside one behaves correctly
//! because tmux owns the terminal properly — alternate screen, mouse, resize,
//! scrollback and copy mode all keep working.
//!
//! Two roles, deliberately not mixed:
//!
//! * **Remote** — one detached tmux session per bizik session, holding the
//!   agent process. It lives on the server, so it survives a dropped link, a
//!   closed laptop, and a second laptop attaching later.
//! * **Local** — the viewport. Panes here run nothing but an `ssh … tmux
//!   attach`, and the window's `window_layout` string is what a saved layout
//!   stores.

use anyhow::{Context, Result, bail};
use std::collections::HashMap;
use std::process::Command;

use crate::model::TmuxSession;
use crate::util::shell_quote;

/// Run tmux and capture stdout.
pub fn tmux(args: &[&str]) -> Result<String> {
    let out = Command::new("tmux")
        .args(args)
        .output()
        .context("running tmux (is it installed?)")?;
    if !out.status.success() {
        bail!(
            "tmux {}: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Run tmux, treating failure as "no". Used where an error simply means the
/// server is not running yet.
fn tmux_ok(args: &[&str]) -> bool {
    Command::new("tmux")
        .args(args)
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

pub fn installed() -> bool {
    crate::agent::find_binary("tmux").is_some()
}

/// Whether this process is itself running inside a tmux pane.
///
/// An empty `TMUX` means *not* in tmux — that is how tmux itself reads it, and
/// how a nested attach is forced (`TMUX= tmux attach`). Treating the empty
/// string as "inside" would make bizik skip creating its own session and draw
/// the dashboard straight into whatever pane it was started from.
pub fn inside_tmux() -> bool {
    std::env::var("TMUX").is_ok_and(|v| !v.is_empty())
}

// ---------------------------------------------------------------------------
// Session management (used on the host that holds the agent)
// ---------------------------------------------------------------------------

pub fn has_session(name: &str) -> bool {
    tmux_ok(&["has-session", "-t", &exact(name)])
}

/// All tmux sessions on this machine, with a short preview of each one's
/// current pane.
pub fn list_sessions(with_preview: bool) -> Vec<TmuxSession> {
    let fmt = "#{session_name}\t#{session_created}\t#{session_attached}\t#{session_windows}";
    let Ok(raw) = tmux(&["list-sessions", "-F", fmt]) else {
        return Vec::new(); // No server running is the normal empty case.
    };

    raw.lines()
        .filter_map(|line| {
            let mut parts = line.split('\t');
            let name = parts.next()?.to_string();
            let created = parts.next()?.parse::<u64>().unwrap_or(0) * 1000;
            let attached = parts.next()? != "0";
            let windows = parts.next()?.parse::<u32>().unwrap_or(1);
            let preview = with_preview.then(|| capture_preview(&name)).flatten();
            Some(TmuxSession {
                name,
                created,
                attached,
                windows,
                preview,
            })
        })
        .collect()
}

/// Last few non-empty lines visible in a session's active pane.
///
/// The target needs a trailing colon: `capture-pane` resolves a *pane*, and a
/// bare `=name` is rejected by that parser even though the same string is a
/// valid session target elsewhere. `=name:` keeps the exact-match anchor and
/// selects the session's active pane.
pub fn capture_preview(session: &str) -> Option<String> {
    let raw = tmux(&["capture-pane", "-p", "-t", &format!("{}:", exact(session))]).ok()?;
    let lines: Vec<&str> = raw
        .lines()
        .map(str::trim_end)
        // An agent's own interface ends in box drawing — its input frame sits at
        // the bottom of every screen. Lines with no words in them say nothing
        // about what the session is doing.
        .filter(|l| l.chars().any(char::is_alphanumeric))
        .collect();
    let tail = lines
        .iter()
        .rev()
        .take(3)
        .rev()
        .copied()
        .collect::<Vec<_>>();
    (!tail.is_empty()).then(|| tail.join(" ⏎ "))
}

/// Root pid of every pane, grouped by session — the processes tmux itself
/// started. Agents launched inside show up somewhere below these in the process
/// tree.
///
/// Every session is fetched in one call rather than one call each: this runs on
/// every probe, and a probe happens every few seconds for every host.
pub fn all_pane_pids() -> HashMap<String, Vec<u32>> {
    let Ok(raw) = tmux(&["list-panes", "-a", "-F", "#{session_name}\t#{pane_pid}"]) else {
        return HashMap::new();
    };
    let mut out: HashMap<String, Vec<u32>> = HashMap::new();
    for line in raw.lines() {
        if let Some((session, pid)) = line.split_once('\t')
            && let Ok(pid) = pid.trim().parse::<u32>()
        {
            out.entry(session.to_string()).or_default().push(pid);
        }
    }
    out
}

/// Start a detached session running `cmd` in `cwd`.
///
/// Idempotent: an existing session of that name is left alone, so relaunching a
/// layout reattaches to the work already in progress instead of starting a
/// second copy of it.
pub fn spawn_detached(name: &str, cwd: &str, cmd: &str) -> Result<bool> {
    if has_session(name) {
        return Ok(false);
    }
    tmux(&["new-session", "-d", "-s", name, "-c", cwd, cmd])?;
    // Without this, a session attached from a small pane stays clamped to that
    // size even after the viewer goes away.
    //
    // `-w` and the trailing colon are both required: this is a *window* option,
    // and `set-option -t <session>` rejects it with "no such window" — silently,
    // if the result is discarded.
    let _ = tmux(&[
        "set-option",
        "-w",
        "-t",
        &format!("{}:", exact(name)),
        "aggressive-resize",
        "on",
    ]);
    Ok(true)
}

pub fn kill_session(name: &str) -> Result<()> {
    tmux(&["kill-session", "-t", &exact(name)]).map(|_| ())
}

/// Wrap an agent command so the pane outlives it.
///
/// When an agent exits — crash or a clean `/exit` — the pane must not vanish,
/// taking its scrollback with it. Instead the exit code is printed and a login
/// shell takes over in the same directory, leaving the reason on screen and the
/// user exactly where they were. Restarting is never automatic: a background
/// agent that silently re-runs could repeat work that already had effects.
pub fn wrap_command(agent: &str, cmd: &str, cwd: &str) -> String {
    let inner = format!(
        "{cmd}; __bzk_code=$?; \
         printf '\\n\\033[2m[bizik] {agent} exited (%s) — shell in {cwd}. Ctrl-D closes this pane.\\033[0m\\n' \"$__bzk_code\"; \
         exec \"${{SHELL:-/bin/bash}}\" -l"
    );
    // A login shell, so PATH includes the per-user directories that agents
    // install into — a non-interactive ssh would otherwise not find them.
    format!("exec /bin/sh -lc {}", shell_quote(&inner))
}

// ---------------------------------------------------------------------------
// Local viewport
// ---------------------------------------------------------------------------

/// Create a window running `cmd` and return the id of its pane.
pub fn new_window(name: &str, cmd: &str) -> Result<String> {
    let out = tmux(&["new-window", "-P", "-F", "#{pane_id}", "-n", name, cmd])?;
    Ok(out.trim().to_string())
}

/// Find a window by name in the current session.
pub fn find_window(name: &str) -> Option<String> {
    find_window_in(None, name)
}

/// Find a window by name, optionally in a named session rather than the
/// current one.
pub fn find_window_in(session: Option<&str>, name: &str) -> Option<String> {
    let target = session.map(exact);
    let mut args: Vec<&str> = vec!["list-windows", "-F", "#{window_id}\t#{window_name}"];
    if let Some(t) = &target {
        args.extend(["-t", t]);
    }
    let raw = tmux(&args).ok()?;
    raw.lines().find_map(|l| {
        let (id, n) = l.split_once('\t')?;
        (n == name).then(|| id.to_string())
    })
}

/// Create a window in a named session, running `cmd`.
pub fn new_window_in(session: &str, name: &str, cmd: &str) -> Result<String> {
    let target = exact(session);
    let out = tmux(&[
        "new-window",
        "-t",
        &target,
        "-P",
        "-F",
        "#{pane_id}",
        "-n",
        name,
        cmd,
    ])?;
    Ok(out.trim().to_string())
}

/// Split `window` and return the new pane id.
pub fn split_window(window: &str, cmd: &str) -> Result<String> {
    let out = tmux(&["split-window", "-t", window, "-P", "-F", "#{pane_id}", cmd])?;
    Ok(out.trim().to_string())
}

/// The key that jumps back to the dashboard from inside any pane.
///
/// A pane is showing another machine's tmux, and the agent inside it owns the
/// keyboard — so the way back has to be a binding, not a key the dashboard
/// listens for. `F12` is used by neither Claude Code nor Codex.
pub fn return_key() -> String {
    std::env::var("BIZIK_RETURN_KEY").unwrap_or_else(|_| "F12".to_string())
}

/// Bind the return key on this tmux server.
///
/// Bindings are server-wide, and bizik shares the server with whatever else the
/// user runs — so the binding is conditional: inside bizik's own session it
/// switches to the dashboard, and everywhere else it passes the key straight
/// through to the application, as though it were not bound at all. The
/// condition is a format expression rather than a shell test, so no process is
/// spawned per keypress.
pub fn bind_return_key(session: &str, window: &str) -> Result<()> {
    let key = return_key();
    tmux(&[
        "bind-key",
        "-n",
        &key,
        "if-shell",
        "-F",
        &format!("#{{==:#{{session_name}},{session}}}"),
        &format!("select-window -t ={session}:{window}"),
        &format!("send-keys {key}"),
    ])
    .map(|_| ())
}

pub fn kill_pane(pane: &str) -> Result<()> {
    tmux(&["kill-pane", "-t", pane]).map(|_| ())
}

/// Name of the session this process is running inside.
pub fn current_session() -> Option<String> {
    let out = tmux(&["display-message", "-p", "#{session_name}"]).ok()?;
    let name = out.trim().to_string();
    (!name.is_empty()).then_some(name)
}

/// Name of the window this process is running inside.
pub fn current_window_name() -> Option<String> {
    let out = tmux(&["display-message", "-p", "#{window_name}"]).ok()?;
    let name = out.trim().to_string();
    (!name.is_empty()).then_some(name)
}

pub fn select_pane(pane: &str) -> Result<()> {
    tmux(&["select-pane", "-t", pane]).map(|_| ())
}

pub fn select_layout(window: &str, layout: &str) -> Result<()> {
    tmux(&["select-layout", "-t", window, layout]).map(|_| ())
}

/// The window's geometry as a string that `select-layout` can replay verbatim.
pub fn capture_layout(window: &str) -> Result<String> {
    Ok(
        tmux(&["display-message", "-p", "-t", window, "#{window_layout}"])?
            .trim()
            .to_string(),
    )
}

/// Commands running in each pane of a window, paired with the pane id. Used to
/// work out which sessions an open window is showing when saving a layout.
pub fn window_panes(window: &str) -> Result<Vec<(String, String)>> {
    let raw = tmux(&[
        "list-panes",
        "-t",
        window,
        "-F",
        "#{pane_id}\t#{pane_start_command}",
    ])?;
    Ok(raw
        .lines()
        .filter_map(|l| {
            let (id, cmd) = l.split_once('\t')?;
            Some((id.to_string(), cmd.to_string()))
        })
        .collect())
}

pub fn select_window(window: &str) -> Result<()> {
    tmux(&["select-window", "-t", window]).map(|_| ())
}

/// `=name` pins tmux to an exact match. Without it a target is a prefix
/// pattern, and `bzk-a3f9` would happily resolve to `bzk-a3f91234`.
fn exact(name: &str) -> String {
    format!("={name}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn targets_are_anchored_to_exact_names() {
        // Without the anchor a target is a prefix pattern, and `bzk-a3f9` would
        // happily resolve to a different session named `bzk-a3f91234`.
        assert_eq!(exact("bzk-a3f9"), "=bzk-a3f9");
    }

    #[test]
    fn pane_targets_keep_the_anchor_and_add_a_colon() {
        // `capture-pane` rejects a bare `=name`; the colon makes it a pane
        // target while the anchor still prevents a prefix match.
        assert_eq!(format!("{}:", exact("bzk-a3f9")), "=bzk-a3f9:");
    }

    #[test]
    fn wrapper_keeps_the_pane_and_uses_a_login_shell() {
        let w = wrap_command("claude", "claude --resume x", "/repo");
        assert!(w.starts_with("exec /bin/sh -lc "), "login shell for PATH");
        assert!(w.contains("exited"), "exit reason must stay on screen");
        assert!(
            w.contains("SHELL:-/bin/bash"),
            "falls back to a shell, never dies"
        );
        assert!(!w.contains("while"), "no automatic restart loop");
    }

    #[test]
    fn wrapper_quotes_commands_containing_quotes() {
        let w = wrap_command("claude", "claude --resume 'a b'", "/r");
        // The inner single quotes must be escaped, not left to terminate early.
        assert!(w.contains(r"'\''"));
    }
}
