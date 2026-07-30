//! Human-facing session commands.
//!
//! The dashboard and SSH protocol continue to use the exact UUID commands in
//! `cli.rs`. This module is a deliberately thin controller over those commands:
//! it resolves readable targets from one host probe, then sends the existing
//! machine-facing mutation to the host that owns the record.

use anyhow::{Context, Result, bail};
use clap::{Args, Subcommand};
use serde::Serialize;
use uuid::Uuid;

use crate::hostops::SpawnResult;
use crate::model::{AgentKind, Folder, Host, Probe, Session};
use crate::reconcile::{SessionView, State};
use crate::store::LocalStore;
use crate::{remote, tui};

const MIN_UUID_PREFIX: usize = 4;

#[derive(Args)]
pub(crate) struct SessionArgs {
    /// Configured host name; omit to operate on this machine
    #[arg(long, global = true, value_name = "NAME")]
    host: Option<String>,

    #[command(subcommand)]
    command: SessionCmd,
}

#[derive(Subcommand)]
enum SessionCmd {
    /// List sessions on one host
    List {
        /// Print a JSON array
        #[arg(long)]
        json: bool,
    },

    /// Show the session named by BZK_SESSION_ID
    Current {
        /// Print JSON
        #[arg(long)]
        json: bool,
    },

    /// Create a session in a marked folder
    New {
        /// Exact folder label/path or a unique UUID prefix
        folder: String,

        /// Agent to run: claude, codex or shell
        #[arg(long, default_value = "claude")]
        agent: String,

        /// Session title (a distinct default is generated when omitted)
        #[arg(long)]
        title: Option<String>,

        /// Agent-native conversation id to resume
        #[arg(long)]
        resume: Option<String>,

        /// Print JSON
        #[arg(long)]
        json: bool,
    },

    /// Start or resume a session in detached tmux
    Open {
        /// Exact title or a unique UUID prefix; omit for BZK_SESSION_ID
        target: Option<String>,

        /// Print JSON
        #[arg(long)]
        json: bool,
    },

    /// Rename a session
    Rename {
        /// New, task-specific title
        title: String,

        /// Exact current title or a unique UUID prefix; omit for BZK_SESSION_ID
        #[arg(long, short = 's', value_name = "TARGET")]
        session: Option<String>,

        /// Print JSON
        #[arg(long)]
        json: bool,
    },

    /// Stop a session while keeping its record and conversation
    Stop {
        /// Exact title or a unique UUID prefix; omit for BZK_SESSION_ID
        target: Option<String>,

        /// Print JSON
        #[arg(long)]
        json: bool,
    },

    /// Stop and forget a session record
    Remove {
        /// Exact title or a unique UUID prefix; omit for BZK_SESSION_ID
        target: Option<String>,

        /// Print JSON
        #[arg(long)]
        json: bool,
    },
}

#[derive(Serialize)]
struct SessionInfo {
    host: String,
    id: Uuid,
    title: String,
    agent: AgentKind,
    state: State,
    folder_id: Uuid,
    folder: String,
    path: String,
}

impl SessionInfo {
    fn from_view(host: &Host, probe: &Probe, view: &SessionView) -> Result<Self> {
        let folder = probe
            .folders
            .iter()
            .find(|folder| folder.id == view.session.folder_id)
            .with_context(|| {
                format!(
                    "session {} refers to missing folder {}",
                    view.session.id, view.session.folder_id
                )
            })?;
        Ok(Self {
            host: host.name.clone(),
            id: view.session.id,
            title: view.session.title.clone(),
            agent: view.session.agent,
            state: view.state,
            folder_id: folder.id,
            folder: folder.display_name(),
            path: folder.path.clone(),
        })
    }

    fn created(host: &Host, folder: &Folder, session: &Session) -> Self {
        Self {
            host: host.name.clone(),
            id: session.id,
            title: session.title.clone(),
            agent: session.agent,
            state: State::Down,
            folder_id: folder.id,
            folder: folder.display_name(),
            path: folder.path.clone(),
        }
    }
}

#[derive(Serialize)]
struct ActionInfo {
    action: &'static str,
    changed: bool,
    host: String,
    id: Uuid,
    title: String,
}

#[derive(Serialize)]
struct OpenInfo {
    action: &'static str,
    host: String,
    id: Uuid,
    title: String,
    tmux_name: String,
    started: bool,
    path: String,
}

pub(crate) fn run(args: SessionArgs) -> Result<()> {
    let host = resolve_host(args.host.as_deref())?;
    reject_remote_implicit_target(&host, &args.command)?;
    let probe = probe_host(&host)?;

    match args.command {
        SessionCmd::List { json } => list(&host, &probe, json),
        SessionCmd::Current { json } => {
            require_local_current(&host)?;
            let view = current_session(&probe)?;
            print_one(SessionInfo::from_view(&host, &probe, view)?, json)
        }
        SessionCmd::New {
            folder,
            agent,
            title,
            resume,
            json,
        } => new_session(&host, &probe, &folder, &agent, title, resume, json),
        SessionCmd::Open { target, json } => {
            let view = target_session(&host, &probe, target.as_deref())?;
            open(&host, view, json)
        }
        SessionCmd::Rename {
            title,
            session,
            json,
        } => {
            let view = target_session(&host, &probe, session.as_deref())?;
            rename(&host, &probe, view, &title, json)
        }
        SessionCmd::Stop { target, json } => {
            let view = target_session(&host, &probe, target.as_deref())?;
            mutate(
                &host,
                view,
                "stopped",
                "already stopped",
                json,
                |host, id| machine_bool(host, "stop", "stopped", id),
            )
        }
        SessionCmd::Remove { target, json } => {
            let view = target_session(&host, &probe, target.as_deref())?;
            mutate(
                &host,
                view,
                "removed",
                "already removed",
                json,
                |host, id| machine_bool(host, "rm-session", "removed", id),
            )
        }
    }
}

fn reject_remote_implicit_target(host: &Host, command: &SessionCmd) -> Result<()> {
    if host.is_local() {
        return Ok(());
    }
    match command {
        SessionCmd::Current { .. } => require_local_current(host),
        SessionCmd::Open { target: None, .. }
        | SessionCmd::Rename { session: None, .. }
        | SessionCmd::Stop { target: None, .. }
        | SessionCmd::Remove { target: None, .. } => bail!(
            "remote host {} requires an explicit session title or UUID prefix",
            host.name
        ),
        _ => Ok(()),
    }
}

fn resolve_host(name: Option<&str>) -> Result<Host> {
    let store = LocalStore::load()?;
    match name {
        Some(name) => store
            .host_by_name(name)
            .cloned()
            .with_context(|| format!("no configured host named {name}")),
        None => Ok(store
            .live_hosts()
            .into_iter()
            .find(|host| host.is_local())
            .cloned()
            .unwrap_or_else(|| Host::new("local".into(), None))),
    }
}

fn probe_host(host: &Host) -> Result<Probe> {
    let raw = remote::run_bzk(host, &["probe", "--json"])
        .with_context(|| format!("probing host {}", host.name))?;
    serde_json::from_str(raw.trim())
        .with_context(|| format!("host {} returned an invalid probe", host.name))
}

fn list(host: &Host, probe: &Probe, json: bool) -> Result<()> {
    let mut sessions: Vec<SessionInfo> = probe
        .sessions
        .iter()
        .map(|view| SessionInfo::from_view(host, probe, view))
        .collect::<Result<_>>()?;
    sessions.sort_by(|a, b| {
        a.title
            .cmp(&b.title)
            .then_with(|| a.id.as_bytes().cmp(b.id.as_bytes()))
    });

    if json {
        println!("{}", serde_json::to_string(&sessions)?);
    } else if sessions.is_empty() {
        println!("no sessions on {}", host.name);
    } else {
        for session in sessions {
            println!(
                "{:<8}  {:<10}  {:<6}  {}  [{}]",
                short_id(session.id),
                session.state.label(),
                session.agent,
                session.title,
                session.folder
            );
        }
    }
    Ok(())
}

fn print_one(session: SessionInfo, json: bool) -> Result<()> {
    if json {
        println!("{}", serde_json::to_string(&session)?);
    } else {
        println!(
            "{}  {}  {}  {}  [{}]",
            short_id(session.id),
            session.state.label(),
            session.agent,
            session.title,
            session.folder
        );
    }
    Ok(())
}

fn new_session(
    host: &Host,
    probe: &Probe,
    folder_target: &str,
    agent_name: &str,
    title: Option<String>,
    resume: Option<String>,
    json: bool,
) -> Result<()> {
    let folder = resolve_folder(&probe.folders, folder_target)?;
    let agent = AgentKind::parse(agent_name)
        .with_context(|| format!("unknown agent '{agent_name}' (claude, codex, shell)"))?;
    let session =
        tui::actions::create_session(host, folder.id, agent, title.as_deref(), resume.as_deref())?;
    let info = SessionInfo::created(host, folder, &session);
    if json {
        println!("{}", serde_json::to_string(&info)?);
    } else {
        println!(
            "created {} “{}” on {} [{}]",
            short_id(info.id),
            info.title,
            info.host,
            info.folder
        );
    }
    Ok(())
}

fn open(host: &Host, view: &SessionView, json: bool) -> Result<()> {
    let SpawnResult {
        session,
        tmux_name,
        started,
        cwd,
    } = tui::actions::start(host, view.session.id)?;
    let info = OpenInfo {
        action: "opened",
        host: host.name.clone(),
        id: session.id,
        title: session.title,
        tmux_name,
        started,
        path: cwd,
    };
    if json {
        println!("{}", serde_json::to_string(&info)?);
    } else {
        println!(
            "{} {} “{}” on {}",
            if info.started { "started" } else { "reused" },
            short_id(info.id),
            info.title,
            info.host
        );
    }
    Ok(())
}

fn rename(host: &Host, probe: &Probe, view: &SessionView, title: &str, json: bool) -> Result<()> {
    tui::actions::rename_session(host, view.session.id, title)?;
    let mut info = SessionInfo::from_view(host, probe, view)?;
    info.title = title.trim().to_string();
    if json {
        println!("{}", serde_json::to_string(&info)?);
    } else {
        println!(
            "renamed {} on {} to “{}”",
            short_id(info.id),
            info.host,
            info.title
        );
    }
    Ok(())
}

fn mutate(
    host: &Host,
    view: &SessionView,
    action: &'static str,
    unchanged: &'static str,
    json: bool,
    operation: impl FnOnce(&Host, Uuid) -> Result<bool>,
) -> Result<()> {
    let changed = operation(host, view.session.id)?;
    let info = ActionInfo {
        action,
        changed,
        host: host.name.clone(),
        id: view.session.id,
        title: view.session.title.clone(),
    };
    if json {
        println!("{}", serde_json::to_string(&info)?);
    } else {
        println!(
            "{} {} “{}” on {}",
            if changed { action } else { unchanged },
            short_id(info.id),
            info.title,
            info.host
        );
    }
    Ok(())
}

fn machine_bool(host: &Host, command: &str, field: &str, session: Uuid) -> Result<bool> {
    let id = session.to_string();
    let raw = remote::run_bzk(host, &[command, "--session", &id])?;
    let value: serde_json::Value =
        serde_json::from_str(raw.trim()).with_context(|| format!("parsing {command} result"))?;
    value
        .get(field)
        .and_then(serde_json::Value::as_bool)
        .with_context(|| format!("{command} result has no boolean '{field}'"))
}

fn require_local_current(host: &Host) -> Result<()> {
    if host.is_local() {
        Ok(())
    } else {
        bail!(
            "current-session lookup is local only; pass an explicit target to a remote command \
             with --host {}",
            host.name
        )
    }
}

fn current_session(probe: &Probe) -> Result<&SessionView> {
    let raw = std::env::var("BZK_SESSION_ID")
        .context("BZK_SESSION_ID is not set; pass a session title or UUID prefix")?;
    let id = Uuid::parse_str(&raw).context("BZK_SESSION_ID is not a valid session UUID")?;
    probe
        .sessions
        .iter()
        .find(|view| view.session.id == id)
        .with_context(|| format!("current session {id} is not tracked on this machine"))
}

fn target_session<'a>(
    host: &Host,
    probe: &'a Probe,
    target: Option<&str>,
) -> Result<&'a SessionView> {
    match target {
        Some(target) => resolve_session(&probe.sessions, target),
        None if host.is_local() => current_session(probe),
        None => bail!(
            "remote host {} requires an explicit session title or UUID prefix",
            host.name
        ),
    }
}

fn resolve_session<'a>(sessions: &'a [SessionView], target: &str) -> Result<&'a SessionView> {
    if let Ok(id) = Uuid::parse_str(target) {
        return sessions
            .iter()
            .find(|view| view.session.id == id)
            .with_context(|| format!("no session {id}"));
    }

    let title_matches: Vec<&SessionView> = sessions
        .iter()
        .filter(|view| view.session.title == target)
        .collect();
    match title_matches.as_slice() {
        [one] => return Ok(one),
        [] => {}
        many => return ambiguous_session(target, many),
    }

    let prefix = uuid_prefix(target)?;
    let matches: Vec<&SessionView> = sessions
        .iter()
        .filter(|view| view.session.id.simple().to_string().starts_with(&prefix))
        .collect();
    match matches.as_slice() {
        [one] => Ok(one),
        [] => bail!("no session matching '{target}'"),
        many => ambiguous_session(target, many),
    }
}

fn ambiguous_session<'a>(target: &str, matches: &[&'a SessionView]) -> Result<&'a SessionView> {
    let mut choices: Vec<String> = matches
        .iter()
        .map(|view| format!("{} “{}”", short_id(view.session.id), view.session.title))
        .collect();
    choices.sort();
    bail!(
        "session target '{target}' is ambiguous: {}",
        choices.join(", ")
    )
}

fn resolve_folder<'a>(folders: &'a [Folder], target: &str) -> Result<&'a Folder> {
    if let Ok(id) = Uuid::parse_str(target) {
        return folders
            .iter()
            .find(|folder| folder.id == id)
            .with_context(|| format!("no marked folder {id}"));
    }

    let named: Vec<&Folder> = folders
        .iter()
        .filter(|folder| folder.display_name() == target || folder.path == target)
        .collect();
    match named.as_slice() {
        [one] => return Ok(one),
        [] => {}
        many => return ambiguous_folder(target, many),
    }

    let prefix = uuid_prefix(target)?;
    let matches: Vec<&Folder> = folders
        .iter()
        .filter(|folder| folder.id.simple().to_string().starts_with(&prefix))
        .collect();
    match matches.as_slice() {
        [one] => Ok(one),
        [] => bail!("no marked folder matching '{target}'"),
        many => ambiguous_folder(target, many),
    }
}

fn ambiguous_folder<'a>(target: &str, matches: &[&'a Folder]) -> Result<&'a Folder> {
    let mut choices: Vec<String> = matches
        .iter()
        .map(|folder| format!("{} “{}”", short_id(folder.id), folder.display_name()))
        .collect();
    choices.sort();
    bail!(
        "folder target '{target}' is ambiguous: {}",
        choices.join(", ")
    )
}

fn uuid_prefix(target: &str) -> Result<String> {
    let compact = target.replace('-', "");
    if compact.len() < MIN_UUID_PREFIX || compact.len() > 32 {
        bail!(
            "'{target}' is not an exact title; UUID prefixes need {} to 32 hexadecimal digits",
            MIN_UUID_PREFIX
        );
    }
    if !compact.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        bail!("'{target}' is not an exact title or UUID prefix");
    }
    Ok(compact.to_ascii_lowercase())
}

fn short_id(id: Uuid) -> String {
    id.simple().to_string()[..8].to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn view(id: &str, title: &str) -> SessionView {
        SessionView {
            session: Session {
                id: Uuid::parse_str(id).expect("test UUID"),
                folder_id: Uuid::nil(),
                agent: AgentKind::Shell,
                title: title.into(),
                agent_session_id: None,
                created_at: 0,
                updated_at: 0,
                deleted_at: None,
                last_attached: None,
            },
            state: State::Down,
            preview: None,
            attention: None,
        }
    }

    #[test]
    fn exact_titles_and_unique_uuid_prefixes_resolve() {
        let sessions = vec![
            view("11111111-1111-4111-8111-111111111111", "API repair"),
            view("22222222-2222-4222-8222-222222222222", "docs"),
        ];
        assert_eq!(
            resolve_session(&sessions, "API repair")
                .expect("title")
                .session
                .id,
            sessions[0].session.id
        );
        assert_eq!(
            resolve_session(&sessions, "2222")
                .expect("prefix")
                .session
                .id,
            sessions[1].session.id
        );
    }

    #[test]
    fn ambiguous_titles_and_uuid_prefixes_are_rejected() {
        let sessions = vec![
            view("abcd1111-1111-4111-8111-111111111111", "same"),
            view("abcd2222-2222-4222-8222-222222222222", "same"),
        ];
        assert!(
            resolve_session(&sessions, "same")
                .expect_err("ambiguous title")
                .to_string()
                .contains("ambiguous")
        );
        assert!(
            resolve_session(&sessions, "abcd")
                .expect_err("ambiguous prefix")
                .to_string()
                .contains("abcd1111")
        );
    }

    #[test]
    fn short_or_non_hex_guesses_are_not_treated_as_uuid_prefixes() {
        let sessions = vec![view("abcd1111-1111-4111-8111-111111111111", "real title")];
        assert!(resolve_session(&sessions, "abc").is_err());
        assert!(resolve_session(&sessions, "almost").is_err());
    }
}
