//! bizik — mark folders on the machines you work on, then launch and watch
//! agent sessions in all of them from one screen.
//!
//! The same binary plays both roles. On a server it answers `mark`, `probe` and
//! `spawn`; on a laptop it runs the dashboard and drives the servers over ssh.
//! One binary means an install is a single `scp`, and it is impossible for the
//! two sides to disagree about the data format.

mod agent;
mod attention;
mod hooks;
mod hostops;
mod model;
mod probe;
mod remote;
mod store;
mod tmux;
mod tui;
mod util;

use anyhow::{Context, Result, bail};
use clap::{Args, Parser, Subcommand};
use std::process::Command;
use uuid::Uuid;

use model::AgentKind;
use store::{HostStore, LocalStore};

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
        /// notification, permission, stop, prompt or end
        event: String,
    },

    /// Install, remove or inspect the hooks that report "waiting on you"
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
enum HooksCmd {
    /// Add the hooks to a machine's Claude Code settings
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

fn main() {
    if let Err(e) = run() {
        eprintln!("bzk: {e:#}");
        std::process::exit(1);
    }
}

fn run() -> Result<()> {
    let cli = Cli::parse();
    match cli.command {
        None | Some(Cmd::Tui) => cmd_tui(),
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
        Some(Cmd::Spawn { session }) => cmd_spawn(session),
        Some(Cmd::Stop { session }) => cmd_stop(session),
        Some(Cmd::RmSession { session }) => cmd_rm_session(session),
        Some(Cmd::Host(c)) => cmd_host(c),
        Some(Cmd::Install { host }) => cmd_install(host),
        Some(Cmd::Hook { event }) => cmd_hook(&event),
        Some(Cmd::Hooks(c)) => cmd_hooks(c),
        Some(Cmd::Export { folders }) => cmd_export(folders),
        Some(Cmd::Import { file, folders }) => cmd_import(&file, folders),
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
        return tui::run();
    }

    let exe = std::env::current_exe().context("locating own binary")?;
    let dash = format!("{}{} tui", config_env(), exe.display());

    // Reattaching must always land on a live dashboard. A previous run may have
    // been quit while its panes stayed open, leaving the session alive but with
    // no dashboard window in it — so recreate the window rather than attaching
    // into a session where nothing responds to keys.
    if tmux::has_session(SESSION) {
        match tmux::find_window_in(Some(SESSION), DASH_WINDOW) {
            Some(w) => tmux::select_window(&w)?,
            None => {
                tmux::new_window_in(SESSION, DASH_WINDOW, &dash)?;
            }
        }
        let status = Command::new("tmux")
            .args(["attach", "-t", &format!("={SESSION}")])
            .status()
            .context("attaching to tmux")?;
        if !status.success() {
            bail!("tmux exited with {status}");
        }
        return Ok(());
    }

    let status = Command::new("tmux")
        .args(["new-session", "-A", "-s", SESSION, "-n", DASH_WINDOW])
        .arg(&dash)
        .status()
        .context("starting tmux")?;
    if !status.success() {
        bail!("tmux exited with {status}");
    }
    Ok(())
}

/// The local tmux session that holds the dashboard and the panes it opens.
const SESSION: &str = "bizik";
const DASH_WINDOW: &str = "bzk-dash";

/// An `env …` prefix carrying the directory overrides into the relaunch.
///
/// The dashboard is restarted by the tmux server, which spawns it with *its
/// own* environment — whatever it inherited whenever it happened to start.
/// Without this, `BIZIK_CONFIG_DIR=… bzk` would silently read the default
/// configuration instead of the one that was asked for.
fn config_env() -> String {
    let vars: Vec<String> = ["BIZIK_CONFIG_DIR", "BIZIK_CACHE_DIR"]
        .iter()
        .filter_map(|name| {
            let value = std::env::var(name).ok()?;
            Some(format!("{name}={}", util::shell_quote(&value)))
        })
        .collect();
    if vars.is_empty() {
        String::new()
    } else {
        format!("env {} ", vars.join(" "))
    }
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
    let start = start.to_string_lossy().into_owned();

    let path = if repo_root {
        git(&start, &["rev-parse", "--show-toplevel"])
            .with_context(|| format!("{start} is not inside a git repository"))?
    } else {
        start.clone()
    };

    let mut store = HostStore::load()?;
    let id = store.upsert_folder(&path);

    if let Some(folder) = store.folder_mut(id) {
        if args.label.is_some() {
            folder.label = args.label;
        }
        folder.git_branch = git(&path, &["rev-parse", "--abbrev-ref", "HEAD"]);
        folder.git_remote = git(&path, &["remote", "get-url", "origin"]);
        folder.updated_at = util::now_ms();
    }
    store.save()?;

    println!("marked {path}");
    Ok(())
}

fn cmd_unmark(path: Option<String>) -> Result<()> {
    let path = match path {
        Some(p) => std::fs::canonicalize(&p)
            .with_context(|| format!("no such directory: {p}"))?
            .to_string_lossy()
            .into_owned(),
        None => std::env::current_dir()?.to_string_lossy().into_owned(),
    };

    let mut store = HostStore::load()?;
    let Some(folder) = store.folder_by_path(&path) else {
        bail!("{path} is not marked");
    };
    let id = folder.id;
    store.remove_folder(id);
    store.save()?;
    println!("unmarked {path}");
    Ok(())
}

fn cmd_marks(json: bool) -> Result<()> {
    let store = HostStore::load()?;
    let folders = store.live_folders();

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
    let out = Command::new("git").arg("-C").arg(dir).args(args).output().ok()?;
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
            p.agents.iter().map(|a| a.to_string()).collect::<Vec<_>>().join(", ")
        }
    );
    println!("folders: {}", p.folders.len());
    println!("sessions: {}", p.sessions.len());
    println!("conversations: {}", p.chats.len());
    println!("tmux sessions: {}", p.tmux.len());
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
    let session = hostops::create_session(folder, kind, title, resume)?;
    println!("{}", serde_json::to_string(&session)?);
    Ok(())
}

fn cmd_spawn(session: Uuid) -> Result<()> {
    let result = hostops::spawn(session)?;
    println!("{}", serde_json::to_string(&result)?);
    Ok(())
}

fn cmd_stop(session: Uuid) -> Result<()> {
    let killed = hostops::stop(session)?;
    println!("{}", serde_json::json!({ "stopped": killed }));
    Ok(())
}

fn cmd_rm_session(session: Uuid) -> Result<()> {
    let mut store = HostStore::load()?;
    // Stop it first, or the tmux session would outlive the record that names it.
    let _ = hostops::stop(session);
    let removed = store.remove_session(session);
    store.save()?;
    println!("{}", serde_json::json!({ "removed": removed }));
    Ok(())
}

// ---------------------------------------------------------------------------
// Hosts
// ---------------------------------------------------------------------------

fn cmd_host(cmd: HostCmd) -> Result<()> {
    let mut store = LocalStore::load()?;
    match cmd {
        HostCmd::Add { name, ssh } => {
            store.add_host(&name, ssh.clone())?;
            store.save()?;
            match ssh {
                Some(t) => println!("host {name} -> {t}\nnext: bzk install {name}"),
                None => println!("host {name} -> this machine"),
            }
        }
        HostCmd::Rm { name } => {
            if !store.remove_host(&name) {
                bail!("no host named {name}");
            }
            store.save()?;
            println!("removed {name}");
        }
        HostCmd::Ls => {
            store.ensure_local_host();
            store.save()?;
            for h in store.live_hosts() {
                println!("{:<16} {}", h.name, h.ssh.as_deref().unwrap_or("(this machine)"));
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
    let outcome = if event == "end" {
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
            println!("settings: {}", hooks::settings_path().display());
            println!("a running session picks these up on its next start");
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
                    .map(|l| l.trim_start_matches("local").trim())
                    .unwrap_or("?")
            ),
            Err(e) => println!("{e:#}"),
        }
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
        let mut mine = HostStore::load()?;
        mine.merge_from(&incoming);
        mine.save()?;
        println!(
            "merged: {} folders, {} sessions",
            mine.live_folders().len(),
            mine.live_sessions().len()
        );
    } else {
        let incoming: LocalStore =
            serde_json::from_str(&raw).context("this does not look like a bizik export")?;
        let mut mine = LocalStore::load()?;
        mine.merge_from(&incoming);
        mine.save()?;
        println!(
            "merged: {} hosts, {} layouts",
            mine.live_hosts().len(),
            mine.live_layouts().len()
        );
    }
    Ok(())
}

fn cmd_doctor() -> Result<()> {
    println!("local");
    println!(
        "  tmux            {}",
        if tmux::installed() {
            "ok"
        } else {
            "MISSING — the dashboard cannot open panes"
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
                    p.agents.iter().map(|a| a.to_string()).collect::<Vec<_>>().join("+")
                };
                println!(
                    "  {:<16} ok · bizik {} · {} folders · {} · hooks {}",
                    probed.host.name,
                    p.bzk_version,
                    p.folders.len(),
                    agents,
                    if p.hooks_installed { "on" } else { "off" }
                );
                // A stale binary is invisible until something behaves oddly, and
                // it is easy to leave behind after a rebuild.
                if p.bzk_version != mine {
                    println!(
                        "  {:<16} running {} but this is {mine} — run: bzk install {}",
                        "", p.bzk_version, probed.host.name
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
