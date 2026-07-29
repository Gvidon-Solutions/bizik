//! Command-line parsing and dispatch.

use anyhow::{Context, Result, bail};
use clap::{Args, Parser, Subcommand};
use std::process::Command;
use uuid::Uuid;

use crate::application::{self, FsHostStateRepository, FsLocalStateRepository};
use crate::model::AgentKind;
use crate::store::{HostStore, LocalStore};
use crate::{agent, attention, hooks, hostenv, hostops, model, probe, remote, tmux, tui, util};

#[derive(Parser)]
#[command(name = "bzk", version, about = "Remote agent session manager", long_about = None)]
struct Cli {
    #[command(subcommand)]
    command: Option<Cmd>,
}

#[derive(Subcommand)]
enum Cmd {
    /// Open the dashboard (default when run with no arguments)
    Tui,

    /// Draw the project/session tree inside the work window
    #[command(hide = true)]
    Sidebar,

    /// Show or hide the project/session sidebar
    #[command(hide = true)]
    ToggleSidebar,

    /// Move focus to the project/session sidebar
    #[command(hide = true)]
    FocusSidebar,

    /// Move focus to the active session viewer
    #[command(hide = true)]
    FocusViewer,

    /// Start and show one session in the workspace
    #[command(hide = true)]
    View {
        #[arg(long)]
        host: String,
        #[arg(long)]
        session: Uuid,
    },

    /// Add the current directory to favourites
    #[command(visible_alias = "m")]
    Mark(MarkArgs),

    /// Add the enclosing git repository to favourites
    #[command(visible_alias = "mr")]
    MarkRepo(MarkArgs),

    /// Remove a directory from favourites
    Unmark {
        /// Directory to unmark (default: current)
        path: Option<String>,
    },

    /// List this machine's favourites
    Marks {
        #[arg(long)]
        json: bool,
    },

    /// Report this machine's folders, sessions, conversations and status
    Probe {
        #[arg(long)]
        json: bool,
        /// Include a preview of each tmux session's screen
        #[arg(long)]
        preview: bool,
    },

    /// Create a session record on this machine
    NewSession {
        #[arg(long)]
        folder: Uuid,
        #[arg(long, default_value = "claude")]
        agent: String,
        #[arg(long)]
        title: Option<String>,
        /// Conversation id to resume rather than starting a fresh one
        #[arg(long)]
        resume: Option<String>,
    },

    /// Start a session's detached tmux session on this machine
    Spawn {
        #[arg(long)]
        session: Uuid,
        /// Name the driving laptop knows this host by, shown in the pane's label
        #[arg(long)]
        host_label: Option<String>,
    },

    /// Stop a session's tmux session, keeping the record
    Stop {
        #[arg(long)]
        session: Uuid,
    },

    /// Forget a session record on this machine
    RmSession {
        #[arg(long)]
        session: Uuid,
    },

    /// Rename a session record on this machine
    #[command(hide = true)]
    RenameSession {
        #[arg(long)]
        session: Uuid,
        #[arg(long)]
        title: String,
    },

    /// Change project presentation preferences on this machine
    #[command(hide = true)]
    UpdateFolder {
        #[arg(long)]
        folder: Uuid,
        #[arg(long)]
        pinned: Option<bool>,
        #[arg(long)]
        hidden: Option<bool>,
    },

    /// Manage the hosts this laptop drives
    #[command(subcommand)]
    Host(HostCmd),

    /// Copy this binary to a host
    Install {
        /// Host name, or all hosts when omitted
        host: Option<String>,
    },

    /// Record an agent hook firing. Called by the agent; reads JSON on stdin.
    #[command(hide = true)]
    Hook {
        /// notification, permission, stop, prompt, start or end
        event: String,
    },

    /// Install, remove or inspect Claude Code and Codex status hooks
    #[command(subcommand)]
    Hooks(HooksCmd),

    /// Print this machine's configuration as JSON
    Export {
        /// Export this machine's folders and sessions instead of hosts and layouts
        #[arg(long)]
        folders: bool,
    },

    /// Merge a configuration exported elsewhere into this one
    Import {
        /// File to read, or `-` for stdin
        file: String,
        /// Merge folders and sessions instead of hosts and layouts
        #[arg(long)]
        folders: bool,
    },

    /// Capture this machine's real PATH, or a host's
    #[command(subcommand)]
    Env(EnvCmd),

    /// Show which session each open pane is viewing
    Panes,

    /// Check the local setup and every host
    Doctor,
}

#[derive(Args)]
struct MarkArgs {
    /// Directory to mark (default: current)
    path: Option<String>,
    /// Name to show in the list instead of the directory name
    #[arg(long, short)]
    label: Option<String>,
    /// Mark the enclosing git repository instead of this directory
    #[arg(long, short)]
    repo: bool,
}

#[derive(Subcommand)]
enum EnvCmd {
    /// Ask the login shell what PATH it really has, and remember the answer
    Capture {
        /// Host names; omit for this machine
        hosts: Vec<String>,
    },
    /// Show what was captured, and when
    Show,
}

#[derive(Subcommand)]
enum HooksCmd {
    /// Add status hooks to Claude Code and Codex
    Install {
        /// Host names; omit for this machine
        hosts: Vec<String>,
    },
    /// Remove only the hooks bizik added
    Uninstall {
        /// Host names; omit for this machine
        hosts: Vec<String>,
    },
    /// Show where the hooks are installed
    Status {
        /// Host names; omit for every configured host
        hosts: Vec<String>,
    },
}

#[derive(Subcommand)]
enum HostCmd {
    /// Add or update a host
    Add {
        /// Short name used in the dashboard and in layouts
        name: String,
        /// ssh target, e.g. root@1.2.3.4 or an ssh config alias. Omit for this machine.
        ssh: Option<String>,
    },
    /// Remove a host
    Rm { name: String },
    /// List hosts
    Ls,
}

pub fn run() -> Result<()> {
    let cli = Cli::parse();
    match cli.command {
        None | Some(Cmd::Tui) => cmd_tui(),
        Some(Cmd::Sidebar) => crate::sidebar::run(),
        Some(Cmd::ToggleSidebar) => tui::actions::toggle_sidebar(),
        Some(Cmd::FocusSidebar) => tui::actions::focus_sidebar(),
        Some(Cmd::FocusViewer) => tui::actions::focus_viewer(),
        Some(Cmd::View { host, session }) => cmd_view(&host, session),
        Some(Cmd::Mark(a)) => cmd_mark(a, false),
        Some(Cmd::MarkRepo(a)) => cmd_mark(a, true),
        Some(Cmd::Unmark { path }) => cmd_unmark(path),
        Some(Cmd::Marks { json }) => cmd_marks(json),
        Some(Cmd::Probe { json, preview }) => cmd_probe(json, preview),
        Some(Cmd::NewSession {
            folder,
            agent,
            title,
            resume,
        }) => cmd_new_session(folder, &agent, title, resume),
        Some(Cmd::Spawn {
            session,
            host_label,
        }) => cmd_spawn(session, host_label.as_deref()),
        Some(Cmd::Stop { session }) => cmd_stop(session),
        Some(Cmd::RmSession { session }) => cmd_rm_session(session),
        Some(Cmd::RenameSession { session, title }) => cmd_rename_session(session, &title),
        Some(Cmd::UpdateFolder {
            folder,
            pinned,
            hidden,
        }) => cmd_update_folder(folder, pinned, hidden),
        Some(Cmd::Host(c)) => cmd_host(c),
        Some(Cmd::Install { host }) => cmd_install(host),
        Some(Cmd::Hook { event }) => cmd_hook(&event),
        Some(Cmd::Hooks(c)) => cmd_hooks(c),
        Some(Cmd::Export { folders }) => cmd_export(folders),
        Some(Cmd::Import { file, folders }) => cmd_import(&file, folders),
        Some(Cmd::Env(c)) => cmd_env(c),
        Some(Cmd::Panes) => cmd_panes(),
        Some(Cmd::Doctor) => cmd_doctor(),
    }
}

// ---------------------------------------------------------------------------
// Dashboard
// ---------------------------------------------------------------------------

/// Launch the dashboard, putting it inside tmux first if it is not already.
///
/// The dashboard opens panes, and there is nowhere to put a pane without a
/// multiplexer — so rather than fail with advice, it re-executes itself inside
/// a tmux session named `bizik`. `-A` attaches to that session if it already
/// exists, which makes `bzk` from any terminal land back where you were.
fn cmd_tui() -> Result<()> {
    if !tmux::installed() {
        bail!("tmux is required — install it with: sudo apt install tmux");
    }
    if tmux::inside_tmux() {
        // Started inside a session already — bind the way back to wherever the
        // dashboard actually is, which need not be bizik's own session.
        if let (Some(session), Some(window)) =
            (tmux::current_session(), tmux::current_window_name())
        {
            let _ = tmux::bind_return_key(&session, &window);
            let _ = bind_workspace_keys(&session);
        }
        return tui::run();
    }

    let exe = util::own_exe()?;
    let dash = format!(
        "{}{} tui",
        util::config_env(),
        util::shell_quote(&exe.to_string_lossy())
    );

    // The session is created detached and attached separately, so options and
    // bindings can be applied to a session that exists but nobody is looking at
    // yet — the same path whether this is a first run or a reattach.
    if !tmux::has_session(&session_ref()) {
        tmux::new_detached_session(&session_ref(), DASH_WINDOW, &dash)?;
    }

    // Reattaching must always land on a live dashboard. A previous run may have
    // been quit while its panes stayed open, leaving the session alive but with
    // no dashboard window in it — so recreate the window rather than attaching
    // into a session where nothing responds to keys.
    match tmux::find_window_in(Some(&session_ref()), DASH_WINDOW) {
        Some(w) => tmux::select_window(&w)?,
        None => {
            tmux::new_window_in(&session_ref(), DASH_WINDOW, &dash)?;
        }
    }

    // Both re-applied every launch: bindings live on the tmux server and
    // options on the session, either of which may have gone away since.
    let _ = tmux::bind_return_key(&session_ref(), DASH_WINDOW);
    let _ = bind_workspace_keys(&session_ref());
    tmux::apply_session_options(&session_ref());

    tmux::attach_interactively(&session_ref())
}

/// The local tmux session that holds the dashboard and the panes it opens.
const SESSION: &str = "bizik";
const DASH_WINDOW: &str = "bzk-dash";

fn session_ref() -> tmux::SessionRef {
    tmux::SessionRef::new(SESSION)
}

fn bind_workspace_keys(session: &tmux::SessionRef) -> Result<()> {
    let exe = util::own_exe()?;
    let toggle = format!(
        "{}{} toggle-sidebar",
        util::config_env(),
        util::shell_quote(&exe.to_string_lossy())
    );
    let focus_sidebar = format!(
        "{}{} focus-sidebar",
        util::config_env(),
        util::shell_quote(&exe.to_string_lossy())
    );
    let focus_viewer = format!(
        "{}{} focus-viewer",
        util::config_env(),
        util::shell_quote(&exe.to_string_lossy())
    );
    tmux::bind_detach_key(session)?;
    tmux::bind_workspace_key(session, &tui::actions::sidebar_key(), &toggle)?;
    tmux::bind_workspace_key(session, "C-h", &focus_sidebar)?;
    tmux::bind_workspace_key(session, "C-l", &focus_viewer)
}

fn cmd_view(host_name: &str, session: Uuid) -> Result<()> {
    let store = LocalStore::load()?;
    let host = store
        .host_by_name(host_name)
        .with_context(|| format!("no host named {host_name}"))?;
    let spawned = tui::actions::start(host, session)?;
    tui::actions::open_pane(host, spawned.session.id, &spawned.tmux_name)?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Favourites
// ---------------------------------------------------------------------------

/// Marking the current directory and marking its repository are the same
/// operation with one switch, reachable either as `mark --repo` or as the
/// separate `mark-repo`, because both spellings get typed.
fn cmd_mark(args: MarkArgs, repo_root: bool) -> Result<()> {
    let repo_root = repo_root || args.repo;
    let start = match args.path {
        Some(p) => std::fs::canonicalize(&p).with_context(|| format!("no such directory: {p}"))?,
        None => std::env::current_dir().context("reading current directory")?,
    };
    if !start.is_dir() {
        bail!("{} is not a directory", start.display());
    }
    let start = start.to_string_lossy().into_owned();

    let path = if repo_root {
        git(&start, &["rev-parse", "--show-toplevel"])
            .with_context(|| format!("{start} is not inside a git repository"))?
    } else {
        start
    };

    application::mark_folder(
        &FsHostStateRepository::default(),
        application::MarkFolder {
            git_branch: git(&path, &["rev-parse", "--abbrev-ref", "HEAD"]),
            git_remote: git(&path, &["remote", "get-url", "origin"]),
            label: args.label,
            path: path.clone(),
        },
    )?;

    println!("marked {path}");
    Ok(())
}

fn cmd_unmark(path: Option<String>) -> Result<()> {
    let path = match path {
        // Deliberately not `canonicalize`: a folder that has been deleted is
        // exactly the one you most want to unmark, and requiring it to still
        // exist left the entry unremovable by any means short of editing the
        // store by hand.
        Some(p) => resolve_removable(&p)?,
        None => std::env::current_dir()?.to_string_lossy().into_owned(),
    };

    application::unmark_folder(
        &FsHostStateRepository::default(),
        &hostops::TmuxSessionTerminator,
        &path,
    )?;
    println!("unmarked {path}");
    Ok(())
}

/// An absolute path for something that may no longer exist.
///
/// Resolved through the filesystem when it can be, so symlinks and `..` match
/// what `mark` recorded; resolved lexically when it cannot, so a deleted
/// directory still names its own entry.
fn resolve_removable(path: &str) -> Result<String> {
    if let Ok(real) = std::fs::canonicalize(path) {
        return Ok(real.to_string_lossy().into_owned());
    }
    Ok(std::path::absolute(path)
        .with_context(|| format!("resolving {path}"))?
        .to_string_lossy()
        .into_owned())
}

fn cmd_marks(json: bool) -> Result<()> {
    let folders = application::marked_folders(&FsHostStateRepository::default())?;

    if json {
        println!("{}", serde_json::to_string_pretty(&folders)?);
        return Ok(());
    }
    if folders.is_empty() {
        println!("no marked folders here — run `bzk mark` inside one");
        return Ok(());
    }
    for f in folders {
        let branch = f.git_branch.as_deref().unwrap_or("-");
        println!("{:<24} {:<12} {}", f.display_name(), branch, f.path);
    }
    Ok(())
}

/// Run a git query, returning `None` for anything that is not a clean success.
fn git(dir: &str, args: &[&str]) -> Option<String> {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
    (!s.is_empty()).then_some(s)
}

// ---------------------------------------------------------------------------
// Host-side operations
// ---------------------------------------------------------------------------

fn cmd_probe(json: bool, preview: bool) -> Result<()> {
    let p = probe::collect(preview);
    if json {
        println!("{}", serde_json::to_string(&p)?);
        return Ok(());
    }
    println!("bizik {}", p.bzk_version);
    println!(
        "agents: {}",
        if p.agents.is_empty() {
            "none installed".to_string()
        } else {
            p.agents
                .iter()
                .map(|a| a.to_string())
                .collect::<Vec<_>>()
                .join(", ")
        }
    );
    println!("folders: {}", p.folders.len());
    println!("sessions: {}", p.sessions.len());
    println!("conversations: {}", p.chats.len());
    println!("orphaned tmux sessions: {}", p.orphans.len());
    for w in &p.warnings {
        println!("warning: {w}");
    }
    Ok(())
}

fn cmd_new_session(
    folder: Uuid,
    agent_name: &str,
    title: Option<String>,
    resume: Option<String>,
) -> Result<()> {
    let kind = AgentKind::parse(agent_name)
        .with_context(|| format!("unknown agent '{agent_name}' (claude, codex, shell)"))?;
    let session = application::create_session(
        &FsHostStateRepository::default(),
        application::CreateSession {
            folder,
            agent: kind,
            title,
            resume,
        },
    )?;
    println!("{}", serde_json::to_string(&session)?);
    Ok(())
}

fn cmd_spawn(session: Uuid, host_label: Option<&str>) -> Result<()> {
    let result = hostops::spawn(session, host_label)?;
    println!("{}", serde_json::to_string(&result)?);
    Ok(())
}

fn cmd_stop(session: Uuid) -> Result<()> {
    let killed = hostops::stop(session)?;
    println!("{}", serde_json::json!({ "stopped": killed }));
    Ok(())
}

fn cmd_rm_session(session: Uuid) -> Result<()> {
    let removed = application::forget_session(
        &FsHostStateRepository::default(),
        &hostops::TmuxSessionTerminator,
        session,
    )?;
    println!("{}", serde_json::json!({ "removed": removed }));
    Ok(())
}

fn cmd_update_folder(folder: Uuid, pinned: Option<bool>, hidden: Option<bool>) -> Result<()> {
    let folder = application::update_folder_preferences(
        &FsHostStateRepository::default(),
        application::UpdateFolderPreferences {
            folder,
            pinned,
            hidden,
        },
    )?;
    println!("{}", serde_json::to_string(&folder)?);
    Ok(())
}

fn cmd_rename_session(session: Uuid, title: &str) -> Result<()> {
    let renamed = application::rename_session(&FsHostStateRepository::default(), session, title)?;
    println!("{}", serde_json::to_string(&renamed)?);
    Ok(())
}

// ---------------------------------------------------------------------------
// Hosts
// ---------------------------------------------------------------------------

fn cmd_host(cmd: HostCmd) -> Result<()> {
    let repository = FsLocalStateRepository::default();
    match cmd {
        HostCmd::Add { name, ssh } => {
            application::add_host(&repository, &name, ssh.clone())?;
            match ssh {
                Some(t) => println!("host {name} -> {t}\nnext: bzk install {name}"),
                None => println!("host {name} -> this machine"),
            }
        }
        HostCmd::Rm { name } => {
            application::remove_host(&repository, &name)?;
            println!("removed {name}");
        }
        HostCmd::Ls => {
            for h in application::configured_hosts(&repository)? {
                println!(
                    "{:<16} {}",
                    h.name,
                    h.ssh.as_deref().unwrap_or("(this machine)")
                );
            }
        }
    }
    Ok(())
}

fn cmd_install(host: Option<String>) -> Result<()> {
    let store = LocalStore::load()?;
    let targets: Vec<model::Host> = match host {
        Some(name) => vec![
            store
                .host_by_name(&name)
                .with_context(|| format!("no host named {name}"))?
                .clone(),
        ],
        None => store.live_hosts().into_iter().cloned().collect(),
    };
    if targets.is_empty() {
        bail!("no hosts configured — add one with: bzk host add <name> <ssh-target>");
    }

    for h in targets {
        if h.is_local() {
            continue;
        }
        print!("{} ... ", h.name);
        use std::io::Write;
        std::io::stdout().flush().ok();
        match remote::install(&h) {
            Ok(v) => println!("ok ({v})"),
            Err(e) => println!("failed: {e:#}"),
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Hooks
// ---------------------------------------------------------------------------

/// Called by the agent itself, many times a session.
///
/// It must never fail in a way the agent notices: a hook that exits non-zero
/// can interrupt the very work it is reporting on. Problems go to stderr and
/// the exit status stays zero.
fn cmd_hook(event: &str) -> Result<()> {
    let payload = std::io::read_to_string(std::io::stdin()).unwrap_or_default();
    let outcome = if matches!(event, "start" | "end") {
        attention::clear(&payload)
    } else {
        attention::record(event, &payload)
    };
    if let Err(e) = outcome {
        eprintln!("bzk hook: {e:#}");
    }
    Ok(())
}

fn cmd_hooks(cmd: HooksCmd) -> Result<()> {
    match cmd {
        HooksCmd::Install { hosts } if hosts.is_empty() => {
            let report = hooks::install()?;
            println!("installed on this machine: {}", report.added.join(", "));
            println!("Claude: {}", hooks::claude_settings_path().display());
            println!("Codex:  {}", hooks::codex_hook_source_path().display());
            println!("restart agent sessions to load hooks");
            println!("Codex: run /hooks once to review and trust the installed commands");
        }
        HooksCmd::Uninstall { hosts } if hosts.is_empty() => {
            let report = hooks::uninstall()?;
            if report.removed.is_empty() {
                println!("nothing of ours was installed here");
            } else {
                println!("removed: {}", report.removed.join(", "));
            }
        }
        HooksCmd::Install { hosts } => remote_hooks(&hosts, "install")?,
        HooksCmd::Uninstall { hosts } => remote_hooks(&hosts, "uninstall")?,
        HooksCmd::Status { hosts } => cmd_hooks_status(&hosts)?,
    }
    Ok(())
}

fn remote_hooks(names: &[String], action: &str) -> Result<()> {
    let store = LocalStore::load()?;
    for name in names {
        let host = store
            .host_by_name(name)
            .with_context(|| format!("no host named {name}"))?;
        print!("{name} ... ");
        use std::io::Write;
        std::io::stdout().flush().ok();
        match remote::run_bzk(host, &["hooks", action]) {
            Ok(out) => println!("{}", util::one_line(&out, 100)),
            Err(e) => println!("failed: {e:#}"),
        }
    }
    Ok(())
}

fn cmd_hooks_status(names: &[String]) -> Result<()> {
    let events = hooks::installed_events();
    println!(
        "local            {}",
        if events.is_empty() {
            "not installed".to_string()
        } else {
            events.join(", ")
        }
    );

    let mut store = LocalStore::load()?;
    if store.ensure_local_host() {
        store.save()?;
    }
    let hosts: Vec<model::Host> = store
        .live_hosts()
        .into_iter()
        .filter(|h| !h.is_local() && (names.is_empty() || names.contains(&h.name)))
        .cloned()
        .collect();

    for host in hosts {
        print!("{:<16} ", host.name);
        use std::io::Write;
        std::io::stdout().flush().ok();
        match remote::run_bzk(&host, &["hooks", "status"]) {
            Ok(out) => println!(
                "{}",
                out.lines()
                    .next()
                    .map_or("?", |line| { line.trim_start_matches("local").trim() })
            ),
            Err(e) => println!("{e:#}"),
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Host environment
// ---------------------------------------------------------------------------

fn cmd_env(cmd: EnvCmd) -> Result<()> {
    match cmd {
        EnvCmd::Capture { hosts } if hosts.is_empty() => {
            let env = hostenv::capture()?;
            println!("captured from {} -i", env.shell);
            println!("{}", env.path);
        }
        EnvCmd::Capture { hosts } => {
            let store = LocalStore::load()?;
            for name in hosts {
                let host = store
                    .host_by_name(&name)
                    .with_context(|| format!("no host named {name}"))?;
                print!("{name} ... ");
                use std::io::Write;
                std::io::stdout().flush().ok();
                match remote::run_bzk(host, &["env", "capture"]) {
                    Ok(out) => println!("{}", util::one_line(&out, 100)),
                    Err(e) => println!("failed: {e:#}"),
                }
            }
        }
        EnvCmd::Show => match hostenv::load() {
            Some(e) => {
                println!("captured from {}", e.shell);
                println!("{}", e.path);
            }
            None => println!("nothing captured — run: bzk env capture"),
        },
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Moving a configuration between machines
// ---------------------------------------------------------------------------

/// Export and import exist so a second laptop, or a colleague, is a copy
/// operation rather than a rebuild. The merge is last-write-wins per record and
/// respects tombstones, so importing twice is harmless and a deletion made on
/// one machine survives a merge from another that still had the record.
///
/// A layout refers to hosts by name, so importing one on a machine that has a
/// host of the same name simply works; where it does not, the dashboard says
/// which host is missing rather than failing silently.
fn cmd_export(folders: bool) -> Result<()> {
    let json = if folders {
        serde_json::to_string_pretty(&HostStore::load()?)?
    } else {
        serde_json::to_string_pretty(&LocalStore::load()?)?
    };
    println!("{json}");
    Ok(())
}

fn cmd_import(file: &str, folders: bool) -> Result<()> {
    let raw = if file == "-" {
        std::io::read_to_string(std::io::stdin()).context("reading stdin")?
    } else {
        std::fs::read_to_string(file).with_context(|| format!("reading {file}"))?
    };

    if folders {
        let incoming: HostStore =
            serde_json::from_str(&raw).context("this does not look like a folders export")?;
        let summary = application::import_host_state(&FsHostStateRepository::default(), &incoming)?;
        println!(
            "merged: {} folders, {} sessions",
            summary.primary, summary.secondary
        );
    } else {
        let incoming: LocalStore =
            serde_json::from_str(&raw).context("this does not look like a bizik export")?;
        let summary =
            application::import_local_state(&FsLocalStateRepository::default(), &incoming)?;
        println!(
            "merged: {} hosts, {} layouts",
            summary.primary, summary.secondary
        );
    }
    Ok(())
}

/// What is in the pane window, and what bizik makes of it.
///
/// A pane whose session cannot be identified is one a layout restore will
/// neither reuse nor clean up, so being able to see that directly is worth a
/// command.
fn cmd_panes() -> Result<()> {
    let panes = tmux::panes_of_window_anywhere(tui::actions::WORK_WINDOW)?;
    if panes.is_empty() {
        println!("no panes open");
        return Ok(());
    }

    let mut store = LocalStore::load()?;
    if store.ensure_local_host() {
        store.save()?;
    }
    let hosts: Vec<model::Host> = store.live_hosts().into_iter().cloned().collect();
    let known: Vec<(String, model::Session)> = remote::probe_all(&hosts)
        .into_iter()
        .filter_map(|p| p.probe.map(|pr| (p.host.name, pr)))
        .flat_map(|(host, pr)| {
            pr.records()
                .map(|s| (host.clone(), s.clone()))
                .collect::<Vec<_>>()
        })
        .collect();

    for p in &panes {
        let identified = tui::actions::identify_pane(p, &known);
        match identified {
            Some((host, session)) => {
                let title = known
                    .iter()
                    .find(|(_, s)| s.id == session)
                    .map(|(_, s)| s.title.clone())
                    .unwrap_or_default();
                println!(
                    "{:<5} {:<10} {:<28} {}",
                    p.pane,
                    host,
                    title,
                    if p.session.is_some() {
                        "tagged"
                    } else {
                        "recovered from its command"
                    }
                );
            }
            None => println!(
                "{:<5} {:<10} {:<28} {}",
                p.pane,
                "?",
                "unidentified",
                util::one_line(&p.start_command, 60)
            ),
        }
    }

    // What a restore would do, without doing it. "It does not offer to close
    // anything" is otherwise impossible to tell apart from "it did not run".
    let open: Vec<(String, Uuid)> = panes
        .iter()
        .filter_map(|p| tui::actions::identify_pane(p, &known))
        .collect();
    for layout in store.live_layouts() {
        let wanted: std::collections::HashSet<(String, Uuid)> = layout
            .panes
            .iter()
            .map(|p| (p.host.clone(), p.session))
            .collect();
        let mut seen = std::collections::HashSet::new();
        let close = open
            .iter()
            .filter(|key| !wanted.contains(key) || !seen.insert((*key).clone()))
            .count();
        println!(
            "\nlayout “{}”: {} panes wanted, {} open would be closed on restore",
            layout.name,
            layout.panes.len(),
            close
        );
    }
    Ok(())
}

fn cmd_doctor() -> Result<()> {
    println!("local");
    println!(
        "  tmux            {}",
        match (tmux::installed(), tmux::version()) {
            (false, _) => "MISSING — the dashboard cannot open panes".to_string(),
            (true, Some(v)) if tmux::version_supported(&v) => format!("ok ({v})"),
            // Behaviour genuinely differs between versions, and the way that
            // shows up is a target syntax quietly doing nothing.
            (true, Some(v)) => format!("{v} — older than the supported {}", tmux::MIN_VERSION),
            (true, None) => "ok (version unknown)".to_string(),
        }
    );
    println!(
        "  ssh             {}",
        if agent::find_binary("ssh").is_some() {
            "ok"
        } else {
            "MISSING"
        }
    );
    for a in agent::all() {
        if a.kind() == AgentKind::Shell {
            continue;
        }
        println!(
            "  {:<15} {}",
            a.kind().to_string(),
            match a.binary() {
                Some(p) => format!("ok ({})", p.display()),
                None => "not installed".into(),
            }
        );
    }
    println!(
        "  PATH capture    {}",
        match hostenv::load() {
            Some(_) => "ok".to_string(),
            None => "not captured — run: bzk env capture".to_string(),
        }
    );
    println!("  config          {}", util::config_dir().display());

    let mut store = LocalStore::load()?;
    if store.ensure_local_host() {
        store.save()?;
    }
    let hosts: Vec<model::Host> = store.live_hosts().into_iter().cloned().collect();
    let mine = env!("CARGO_PKG_VERSION");
    println!("\nhosts ({})", hosts.len());
    for probed in remote::probe_all(&hosts) {
        match (&probed.probe, &probed.error) {
            (Some(p), _) => {
                let agents = if p.agents.is_empty() {
                    "no agents".to_string()
                } else {
                    p.agents
                        .iter()
                        .map(|a| a.to_string())
                        .collect::<Vec<_>>()
                        .join("+")
                };
                println!(
                    "  {:<16} ok · bizik {} · {} folders · {} · hooks {}",
                    probed.host.name,
                    p.bzk_version,
                    p.folders.len(),
                    agents,
                    if p.hooks_installed {
                        "configured"
                    } else {
                        "missing"
                    }
                );
                // A stale binary is invisible until something behaves oddly, and
                // it is easy to leave behind after a rebuild.
                if p.bzk_version != mine {
                    println!(
                        "  {:<16} running {} but this is {mine} — run: bzk install {}",
                        "", p.bzk_version, probed.host.name
                    );
                }
                if p.protocol != model::PROTOCOL {
                    println!(
                        "  {:<16} speaks protocol {} but this is {} — run: bzk install {}",
                        "",
                        p.protocol,
                        model::PROTOCOL,
                        probed.host.name
                    );
                }
                if p.env_captured_at.is_none() && !p.agents.is_empty() {
                    println!(
                        "  {:<16} PATH never captured — agents may not start: bzk env capture {}",
                        "", probed.host.name
                    );
                }
                if !p.hooks_installed && !p.agents.is_empty() {
                    println!(
                        "  {:<16} no hooks — “needs you” cannot be told from “done” here",
                        ""
                    );
                    println!("  {:<16} run: bzk hooks install {}", "", probed.host.name);
                }
                for w in &p.warnings {
                    println!("  {:<16} warning: {w}", "");
                }
            }
            (None, Some(e)) => println!("  {:<16} {e}", probed.host.name),
            (None, None) => println!("  {:<16} unknown failure", probed.host.name),
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    #[test]
    fn every_bizik_setting_travels_into_the_relaunch() {
        // The hand-written list was wrong within a day: BIZIK_TMUX_SOCKET was
        // added to the program and not to the list, so a dashboard told to use
        // a private tmux server quietly drove the default one instead. Matching
        // on the prefix means a new knob cannot be forgotten.
        let rendered = |pairs: &[(&str, &str)]| -> String {
            let mut vars: Vec<String> = pairs
                .iter()
                .filter(|(name, _)| name.starts_with("BIZIK_"))
                .map(|(name, value)| format!("{name}={}", crate::util::shell_quote(value)))
                .collect();
            if vars.is_empty() {
                return String::new();
            }
            vars.sort();
            format!("env {} ", vars.join(" "))
        };

        let out = rendered(&[
            ("BIZIK_TMUX_SOCKET", "bzk-test"),
            ("PATH", "/usr/bin"),
            ("BIZIK_CONFIG_DIR", "/tmp/cfg"),
        ]);
        assert!(out.contains("BIZIK_TMUX_SOCKET='bzk-test'"), "{out}");
        assert!(out.contains("BIZIK_CONFIG_DIR='/tmp/cfg'"), "{out}");
        assert!(
            !out.contains("PATH="),
            "unrelated variables stay behind: {out}"
        );

        assert_eq!(rendered(&[("PATH", "/usr/bin")]), "");
    }
}
