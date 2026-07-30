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
//!
//! Everything goes through [`SessionRef`] rather than raw strings. tmux's target
//! syntax is *not* uniform, and getting it wrong fails quietly — see that type
//! for the three spellings and which commands demand which.

use anyhow::{Context, Result, bail};
use std::collections::HashMap;
use std::process::Command;

use crate::model::TmuxSession;
use crate::util::shell_quote;

/// User option carrying the bizik session uuid, set on both the host's tmux
/// session and the laptop's viewing pane. Identity lives here rather than being
/// recovered by matching substrings of a command line.
pub const OPT_SESSION: &str = "@bzk_session";
/// User option carrying the host a pane is viewing.
pub const OPT_HOST: &str = "@bzk_host";
/// User option distinguishing the persistent sidebar from the active viewer.
pub const OPT_ROLE: &str = "@bzk_role";

// ---------------------------------------------------------------------------
// Targets
// ---------------------------------------------------------------------------

/// A tmux session, and the several ways tmux wants it spelled.
///
/// These are not stylistic variants. Each was found by a command failing:
///
/// * `=name` — the exact-match anchor. Without it a target is a *prefix*
///   pattern and `bzk-a3f9` resolves to `bzk-a3f91234`. Accepted by
///   `has-session`, `kill-session`, `list-panes -s`, `new-window -t`.
/// * `=name:` — the same anchor as a *pane* or *window* target. `capture-pane`
///   and `set-option -w` reject a bare `=name` with "can't find pane".
/// * `name` — no anchor at all. `set-option` (session scope) rejects `=name`
///   with "no such session".
///
/// Because a wrong spelling is a runtime error rather than a compile one, the
/// choice is made once here instead of at each of the twenty-odd call sites.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SessionRef(String);

impl SessionRef {
    pub fn new(name: impl Into<String>) -> Self {
        Self(name.into())
    }

    pub fn name(&self) -> &str {
        &self.0
    }

    /// `=name` — exact session match.
    pub fn anchored(&self) -> String {
        format!("={}", self.0)
    }

    /// `=name:` — this session's active pane, or its window.
    pub fn pane(&self) -> String {
        format!("={}:", self.0)
    }

    /// `name` — for `set-option`, which refuses the anchor.
    pub fn plain(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for SessionRef {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

// ---------------------------------------------------------------------------
// Running tmux
// ---------------------------------------------------------------------------

/// Alternate tmux socket. Set by the integration tests so they can create,
/// inspect and destroy sessions without ever touching the server the user is
/// working on — the single measure that makes a test suite safe to run here.
pub fn socket() -> Option<String> {
    std::env::var("BIZIK_TMUX_SOCKET")
        .ok()
        .filter(|s| !s.is_empty())
}

/// The tmux invocation as it must appear inside a shell command string.
pub fn cli() -> String {
    cli_on(socket().as_deref())
}

fn cli_on(socket: Option<&str>) -> String {
    match socket {
        Some(s) => format!("tmux -L {}", shell_quote(s)),
        None => "tmux".to_string(),
    }
}

fn command() -> Command {
    let mut cmd = Command::new("tmux");
    if let Some(s) = socket() {
        cmd.args(["-L", &s]);
    }
    cmd
}

/// Run tmux and capture stdout.
pub fn tmux(args: &[&str]) -> Result<String> {
    let out = command()
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
    command()
        .args(args)
        .output()
        .is_ok_and(|output| output.status.success())
}

pub fn installed() -> bool {
    crate::agent::find_binary("tmux").is_some()
}

/// tmux's own version string, e.g. `3.2a`.
///
/// Worth reporting because behaviour genuinely differs between versions, and a
/// silent difference is what a target-syntax bug looks like from the outside.
pub fn version() -> Option<String> {
    tmux(&["-V"])
        .ok()
        .map(|v| v.trim().trim_start_matches("tmux ").to_string())
        .filter(|v| !v.is_empty())
}

/// The oldest version bizik is known to work against.
pub const MIN_VERSION: &str = "3.2";

/// Whether a reported version is at least [`MIN_VERSION`]. Compares the leading
/// numeric parts only — tmux appends a letter to patch releases (`3.2a`).
pub fn version_supported(reported: &str) -> bool {
    fn parts(v: &str) -> (u32, u32) {
        let mut it = v.split('.');
        let major = it.next().unwrap_or("0");
        let minor = it.next().unwrap_or("0");
        let num = |s: &str| {
            s.chars()
                .take_while(char::is_ascii_digit)
                .collect::<String>()
                .parse()
                .unwrap_or(0)
        };
        (num(major), num(minor))
    }
    parts(reported) >= parts(MIN_VERSION)
}

/// Whether this process is itself running inside a tmux pane.
///
/// An empty `TMUX` means *not* in tmux — that is how tmux itself reads it, and
/// how a nested attach is forced (`TMUX= tmux attach`). Treating the empty
/// string as "inside" would make bizik skip creating its own session and draw
/// the dashboard straight into whatever pane it was started from.
pub fn inside_tmux() -> bool {
    let Ok(environment) = std::env::var("TMUX") else {
        return false;
    };
    if environment.is_empty() {
        return false;
    }
    if socket().is_none() {
        return true;
    }

    // Tests and isolated profiles deliberately use another tmux socket while
    // the caller may itself be running inside the user's normal tmux. In that
    // case `$TMUX` is real but belongs to a different server, and `new-window`
    // would otherwise land in whichever isolated session tmux guesses is
    // current. Compare the actual socket paths before treating this process as
    // nested in the target server.
    let environment_socket = environment.split(',').next().unwrap_or_default();
    tmux(&["display-message", "-p", "#{socket_path}"])
        .is_ok_and(|path| path.trim() == environment_socket)
}

// ---------------------------------------------------------------------------
// Session management (used on the host that holds the agent)
// ---------------------------------------------------------------------------

pub fn has_session(session: &SessionRef) -> bool {
    tmux_ok(&["has-session", "-t", &session.anchored()])
}

/// All tmux sessions on this machine, with a short preview of each one's
/// current pane and whatever bizik identity it carries.
pub fn list_sessions(with_preview: bool) -> Vec<TmuxSession> {
    let fmt = format!(
        "#{{session_name}}\t#{{session_created}}\t#{{session_attached}}\t#{{session_windows}}\t#{{{OPT_SESSION}}}"
    );
    let Ok(raw) = tmux(&["list-sessions", "-F", &fmt]) else {
        return Vec::new(); // No server running is the normal empty case.
    };

    raw.lines()
        .filter_map(|line| {
            let mut parts = line.split('\t');
            let name = parts.next()?.to_string();
            let created = parts.next()?.parse::<u64>().unwrap_or(0) * 1000;
            let attached = parts.next()? != "0";
            let windows = parts.next()?.parse::<u32>().unwrap_or(1);
            let owner = parts
                .next()
                .map(str::to_string)
                .filter(|s| !s.is_empty() && s != "0");
            let session = SessionRef::new(&name);
            let preview = with_preview.then(|| capture_preview(&session)).flatten();
            Some(TmuxSession {
                name,
                created,
                attached,
                windows,
                preview,
                owner,
            })
        })
        .collect()
}

/// Last few non-empty lines visible in a session's active pane.
pub fn capture_preview(session: &SessionRef) -> Option<String> {
    let raw = tmux(&["capture-pane", "-p", "-t", &session.pane()]).ok()?;
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
pub fn spawn_detached(session: &SessionRef, cwd: &str, cmd: &str) -> Result<bool> {
    if has_session(session) {
        return Ok(false);
    }
    tmux(&["new-session", "-d", "-s", session.plain(), "-c", cwd, cmd])?;
    // Without this, a session attached from a small pane stays clamped to that
    // size even after the viewer goes away. It is a *window* option, so it needs
    // `-w` and the pane-style target.
    let _ = tmux(&[
        "set-option",
        "-w",
        "-t",
        &session.pane(),
        "aggressive-resize",
        "on",
    ]);
    Ok(true)
}

pub fn kill_session(session: &SessionRef) -> Result<()> {
    tmux(&["kill-session", "-t", &session.anchored()]).map(|_| ())
}

/// Attach a bizik session uuid to a tmux session, so a probe can tell whose it
/// is without inferring anything from its name.
pub fn tag_session(session: &SessionRef, uuid: &str) -> Result<()> {
    set_session_option(session, OPT_SESSION, uuid)
}

/// Attach identity to a *pane* on the viewing side.
pub fn tag_pane(pane: &str, uuid: &str, host: &str) -> Result<()> {
    tmux(&["set-option", "-p", "-t", pane, OPT_SESSION, uuid])?;
    tmux(&["set-option", "-p", "-t", pane, OPT_HOST, host])?;
    Ok(())
}

pub fn tag_pane_role(pane: &str, role: &str) -> Result<()> {
    tmux(&["set-option", "-p", "-t", pane, OPT_ROLE, role]).map(|_| ())
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
    // A login shell, so agents that expect a login environment still get one.
    // The PATH they actually need is supplied explicitly by the caller; see
    // `agent::program`.
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
pub fn find_window_in(session: Option<&SessionRef>, name: &str) -> Option<String> {
    let target = session.map(SessionRef::anchored);
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
pub fn new_window_in(session: &SessionRef, name: &str, cmd: &str) -> Result<String> {
    let target = session.anchored();
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

/// Split a pane horizontally, placing the new pane before (to the left of) it.
pub fn split_window_left(pane: &str, width: u16, cmd: &str) -> Result<String> {
    let width = width.to_string();
    let out = tmux(&[
        "split-window",
        "-b",
        "-h",
        "-l",
        &width,
        "-t",
        pane,
        "-P",
        "-F",
        "#{pane_id}",
        cmd,
    ])?;
    Ok(out.trim().to_string())
}

/// Split a pane horizontally, placing the new pane after (to the right of) it.
pub fn split_window_right(pane: &str, cmd: &str) -> Result<String> {
    let out = tmux(&[
        "split-window",
        "-h",
        "-t",
        pane,
        "-P",
        "-F",
        "#{pane_id}",
        cmd,
    ])?;
    Ok(out.trim().to_string())
}

/// Split one viewer pane and return the new pane id.
///
/// Targeting a pane rather than the whole window preserves the outer
/// sidebar/viewer split while tmux grows the layout on the viewer side.
pub fn split_window(pane: &str, cmd: &str) -> Result<String> {
    let out = tmux(&["split-window", "-t", pane, "-P", "-F", "#{pane_id}", cmd])?;
    Ok(out.trim().to_string())
}

pub fn resize_pane_width(pane: &str, width: u16) -> Result<()> {
    let width = width.to_string();
    tmux(&["resize-pane", "-t", pane, "-x", &width]).map(|_| ())
}

pub fn pane_width(pane: &str) -> Option<u16> {
    tmux(&["display-message", "-p", "-t", pane, "#{pane_width}"])
        .ok()?
        .trim()
        .parse()
        .ok()
}

/// The key that jumps back to the dashboard from inside any pane.
pub fn return_key() -> String {
    std::env::var("BIZIK_RETURN_KEY").unwrap_or_else(|_| "F12".to_string())
}

/// The key that immediately detaches the client from bizik.
pub fn detach_key() -> String {
    std::env::var("BIZIK_DETACH_KEY").unwrap_or_else(|_| "F11".to_string())
}

/// Bind the return key on this tmux server.
///
/// Bindings are server-wide, and bizik shares the server with whatever else the
/// user runs — so the binding is conditional: inside bizik's own session it
/// switches to the dashboard, and everywhere else it passes the key straight
/// through to the application, as though it were not bound at all. The
/// condition is a format expression rather than a shell test, so no process is
/// spawned per keypress.
pub fn bind_return_key(session: &SessionRef, window: &str) -> Result<()> {
    let key = return_key();
    tmux(&[
        "bind-key",
        "-n",
        &key,
        "if-shell",
        "-F",
        &format!("#{{==:#{{session_name}},{session}}}"),
        &format!("select-window -t {}{window}", session.pane()),
        &format!("send-keys {key}"),
    ])
    .map(|_| ())
}

/// Bind a server-wide key that detaches only when pressed inside bizik.
///
/// Like the return binding, this must be conditional because tmux bindings are
/// shared by every session on the server. Outside bizik the application still
/// receives the original key.
pub fn bind_detach_key(session: &SessionRef) -> Result<()> {
    let key = detach_key();
    tmux(&[
        "bind-key",
        "-n",
        &key,
        "if-shell",
        "-F",
        &format!("#{{==:#{{session_name}},{session}}}"),
        "detach-client",
        &format!("send-keys {key}"),
    ])
    .map(|_| ())
}

/// Bind a global key that runs a command only inside bizik's own tmux session.
pub fn bind_workspace_key(session: &SessionRef, key: &str, command_to_run: &str) -> Result<()> {
    tmux(&[
        "bind-key",
        "-n",
        key,
        "if-shell",
        "-F",
        &format!("#{{==:#{{session_name}},{session}}}"),
        &format!("run-shell -b {}", shell_quote(command_to_run)),
        &format!("send-keys {key}"),
    ])
    .map(|_| ())
}

/// Create a detached session running `cmd` in a window called `window`.
///
/// Detached first, then attached separately, so session options can be set
/// while the session exists but nothing is looking at it yet.
pub fn new_detached_session(session: &SessionRef, window: &str, cmd: &str) -> Result<()> {
    tmux(&[
        "new-session",
        "-d",
        "-s",
        session.plain(),
        "-n",
        window,
        cmd,
    ])
    .map(|_| ())
}

/// Set an option on one session only.
///
/// Unlike key bindings, session options really are scoped: other sessions on
/// the same server and the global defaults are untouched.
pub fn set_session_option(session: &SessionRef, name: &str, value: &str) -> Result<()> {
    tmux(&["set-option", "-t", session.plain(), name, value]).map(|_| ())
}

pub fn set_window_option(window: &str, name: &str, value: &str) -> Result<()> {
    tmux(&["set-option", "-w", "-t", window, name, value]).map(|_| ())
}

/// Keep tmux's pane separator visually subordinate to the application.
pub fn style_app_window(window: &str) {
    for (option, value) in [
        ("pane-border-status", "off"),
        ("pane-border-style", "fg=colour250"),
        // Focus belongs in the application header, not in a full-height strip
        // that visually cuts the workspace in two.
        ("pane-active-border-style", "fg=colour250"),
        // Agent TUIs own their colours. A forced default foreground leaked
        // into Codex's black insertion background and made the inserted text
        // black-on-black. `default` also clears values left by older builds.
        ("window-style", "default"),
        ("window-active-style", "default"),
    ] {
        let _ = set_window_option(window, option, value);
    }
}

pub fn window_option(window: &str, name: &str) -> Option<String> {
    tmux(&["show-options", "-w", "-v", "-t", window, name])
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

/// Whether the mouse should be on. `BIZIK_MOUSE=off` turns it off.
pub fn mouse_wanted() -> bool {
    !matches!(
        std::env::var("BIZIK_MOUSE").as_deref(),
        Ok("off") | Ok("0") | Ok("false")
    )
}

/// Options applied to bizik's own session, and to no other.
pub fn apply_session_options(session: &SessionRef) {
    if mouse_wanted() {
        let _ = set_session_option(session, "mouse", "on");
    }
    // The application draws its own chrome. tmux's window list duplicates it
    // and creates a second status bar beneath attached agent sessions.
    for (option, value) in [
        ("status", "off"),
        // Scoped to bizik's session. Applications in its panes can now receive
        // terminal FocusGained/FocusLost without changing the user's other
        // tmux sessions.
        ("focus-events", "on"),
        ("message-style", "fg=colour237,bg=colour255"),
        ("mode-style", "fg=colour255,bg=colour134"),
    ] {
        let _ = set_session_option(session, option, value);
    }
}

/// Hide the status bar drawn by an attached agent session.
///
/// Each pane is attached to a tmux session on the host, and that tmux paints a
/// second status line inside the pane. The sidebar already carries the project,
/// session, agent and state, so the extra line is visual noise.
///
/// Set `BIZIK_PANE_STATUS=label` to restore the compact legacy label.
pub fn label_session(session: &SessionRef, host: &str, folder: &str, agent: &str) {
    let labelled = matches!(
        std::env::var("BIZIK_PANE_STATUS").as_deref(),
        Ok("label") | Ok("on") | Ok("1") | Ok("true")
    );
    if !labelled {
        let _ = set_session_option(session, "status", "off");
        return;
    }
    let left = format!(" #[bold]{host}#[nobold] · {folder} · #[fg=colour109]{agent}#[default] ");

    for (option, value) in [
        ("status", "on"),
        ("status-left", left.as_str()),
        ("status-left-length", "200"),
        // The window list and the clock are noise in a pane that only ever has
        // one window; the label carries everything worth reading.
        ("status-right", ""),
        ("window-status-format", ""),
        ("window-status-current-format", ""),
    ] {
        let _ = set_session_option(session, option, value);
    }
}

pub fn kill_pane(pane: &str) -> Result<()> {
    tmux(&["kill-pane", "-t", pane]).map(|_| ())
}

/// Name of the session this process is running inside.
pub fn current_session() -> Option<SessionRef> {
    let out = tmux(&["display-message", "-p", "#{session_name}"]).ok()?;
    let name = out.trim().to_string();
    (!name.is_empty()).then(|| SessionRef::new(name))
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

/// The window's geometry as a string that `select-layout` can replay.
pub fn capture_layout(window: &str) -> Result<String> {
    Ok(
        tmux(&["display-message", "-p", "-t", window, "#{window_layout}"])?
            .trim()
            .to_string(),
    )
}

/// Whether the pane running this process currently owns keyboard focus.
pub fn current_pane_active() -> bool {
    let Some(pane) = std::env::var("TMUX_PANE")
        .ok()
        .filter(|pane| !pane.is_empty())
    else {
        return false;
    };
    tmux(&["display-message", "-p", "-t", &pane, "#{pane_active}"])
        .is_ok_and(|value| value.trim() == "1")
}

/// What each pane of a window is: its id, and the bizik identity tagged onto
/// it. Read from tmux's own per-pane options rather than parsed out of a
/// command line, so a rename, a quoting change or a similar-looking session
/// cannot confuse it.
pub struct PaneInfo {
    pub pane: String,
    pub session: Option<String>,
    pub host: Option<String>,
    pub role: Option<String>,
    pub active: bool,
    pub last: bool,
    /// What the pane was started with. Only needed to recognise panes opened
    /// before identity was tagged onto them.
    pub start_command: String,
}

/// Every pane of every window with this name, on any session of this server.
///
/// `window_panes` resolves the *current* session, which needs a `TMUX` in the
/// environment. This one does not, so the pane mapping can be inspected from an
/// ordinary shell — which is the difference between diagnosing a mismatch and
/// guessing at it.
pub fn panes_of_window_anywhere(name: &str) -> Result<Vec<PaneInfo>> {
    let fmt = format!(
        "#{{window_name}}\t#{{pane_id}}\t#{{{OPT_SESSION}}}\t#{{{OPT_HOST}}}\t#{{{OPT_ROLE}}}\t#{{pane_active}}\t#{{pane_last}}\t#{{pane_start_command}}"
    );
    let raw = tmux(&["list-panes", "-a", "-F", &fmt])?;
    Ok(raw
        .lines()
        .filter_map(|l| {
            let mut parts = l.splitn(8, '\t');
            if parts.next()? != name {
                return None;
            }
            Some(parse_pane(parts))
        })
        .collect())
}

pub fn window_panes(window: &str) -> Result<Vec<PaneInfo>> {
    let fmt = format!(
        "#{{pane_id}}\t#{{{OPT_SESSION}}}\t#{{{OPT_HOST}}}\t#{{{OPT_ROLE}}}\t#{{pane_active}}\t#{{pane_last}}\t#{{pane_start_command}}"
    );
    let raw = tmux(&["list-panes", "-t", window, "-F", &fmt])?;
    Ok(raw.lines().map(|l| parse_pane(l.splitn(7, '\t'))).collect())
}

/// The command is last and taken whole: `splitn` keeps a tab inside it from
/// being read as another field.
fn parse_pane<'a>(mut parts: impl Iterator<Item = &'a str>) -> PaneInfo {
    let clean = |s: Option<&str>| s.map(str::to_string).filter(|v| !v.is_empty() && v != "0");
    PaneInfo {
        pane: parts.next().unwrap_or_default().to_string(),
        session: clean(parts.next()),
        host: clean(parts.next()),
        role: clean(parts.next()),
        active: parts.next().is_some_and(|value| value == "1"),
        last: parts.next().is_some_and(|value| value == "1"),
        start_command: parts.next().unwrap_or_default().to_string(),
    }
}

/// Attach this terminal to a session, blocking until the user detaches.
///
/// Goes through the same builder as everything else so the socket travels with
/// it. A raw `Command::new("tmux")` here quietly attached to the default server
/// while every other call in the process used the private one — which is the
/// sort of hole that makes an isolated test suite a comforting lie.
pub fn attach_interactively(session: &SessionRef) -> Result<()> {
    let status = command()
        .args(["attach", "-t", &session.anchored()])
        .status()
        .context("attaching to tmux")?;
    if !status.success() {
        bail!("tmux exited with {status}");
    }
    Ok(())
}

/// Let go of the terminal without stopping anything.
///
/// Quitting the dashboard used to close its window and leave the pane window
/// behind, still attached — so the user was dropped into somebody else's
/// arrangement and had to detach by hand. Detaching the whole client is what
/// "I am done looking" actually means: everything keeps running, and `bzk`
/// brings it straight back.
pub fn detach_current() -> Result<()> {
    let session = current_session().context("not inside a tmux session")?;
    tmux(&["detach-client", "-s", session.plain()]).map(|_| ())
}

/// Whether anyone is looking at this session.
///
/// Used to stop polling every host every few seconds for a dashboard nobody
/// has on screen.
pub fn current_session_attached() -> bool {
    tmux(&["display-message", "-p", "#{session_attached}"])
        .map_or(true, |value| value.trim() != "0")
}

pub fn select_window(window: &str) -> Result<()> {
    tmux(&["select-window", "-t", window]).map(|_| ())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_session_has_three_spellings_and_they_are_not_interchangeable() {
        let s = SessionRef::new("bzk-a3f9");
        // Each of these was discovered by a command failing on the others.
        assert_eq!(s.anchored(), "=bzk-a3f9", "has-session, kill-session");
        assert_eq!(s.pane(), "=bzk-a3f9:", "capture-pane, set-option -w");
        assert_eq!(s.plain(), "bzk-a3f9", "set-option rejects the anchor");
    }

    #[test]
    fn the_anchor_is_what_stops_a_prefix_from_matching() {
        assert_ne!(
            SessionRef::new("bzk-a3f9").anchored(),
            SessionRef::new("bzk-a3f91234").anchored()
        );
        assert!(SessionRef::new("bzk-a3f9").anchored().starts_with('='));
    }

    #[test]
    fn version_comparison_tolerates_tmux_patch_letters() {
        assert!(version_supported("3.2a"));
        assert!(version_supported("3.2"));
        assert!(version_supported("3.6"));
        assert!(version_supported("4.0"));
        assert!(!version_supported("3.1b"));
        assert!(!version_supported("2.8"));
        assert!(!version_supported("nonsense"));
    }

    #[test]
    fn a_socket_makes_every_invocation_private() {
        // Guards the property the test suite depends on: with a socket set,
        // nothing can reach the server the user is working on.
        assert_eq!(cli_on(Some("bzk-unit")), "tmux -L 'bzk-unit'");
        assert_eq!(cli_on(None), "tmux");
    }

    #[test]
    fn a_pane_line_keeps_its_command_whole() {
        // The command is the last field and may itself contain a tab. Splitting
        // on every tab would read part of it as another column and silently
        // mis-describe the pane.
        let info = parse_pane("%3\tabc-123\tback\tviewer\t1\t0\tssh host\t-t 'x'".splitn(7, '\t'));
        assert_eq!(info.pane, "%3");
        assert_eq!(info.session.as_deref(), Some("abc-123"));
        assert_eq!(info.host.as_deref(), Some("back"));
        assert_eq!(info.role.as_deref(), Some("viewer"));
        assert!(info.active);
        assert!(!info.last);
        assert_eq!(info.start_command, "ssh host\t-t 'x'");
    }

    #[test]
    fn an_untagged_pane_reports_no_identity_rather_than_an_empty_one() {
        // tmux prints an unset user option as an empty field, and older tmux
        // prints `0`. Either must read as "no tag", or a pane from an older
        // build looks tagged with nonsense.
        let empty = parse_pane("%1\t\t\t\t0\t0\tzsh".splitn(7, '\t'));
        assert_eq!(empty.session, None);
        assert_eq!(empty.host, None);
        assert_eq!(empty.role, None);
        assert_eq!(empty.start_command, "zsh");

        let zero = parse_pane("%1\t0\t0\t0\t0\t0\tzsh".splitn(7, '\t'));
        assert_eq!(zero.session, None);
    }

    #[test]
    fn wrapper_keeps_the_pane_and_never_restarts_by_itself() {
        let w = wrap_command("claude", "claude --resume x", "/repo");
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
        assert!(w.contains(r"'\''"));
    }
}
