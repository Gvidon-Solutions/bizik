//! Host-side mutations: creating sessions, starting them, stopping them.
//!
//! These run on the machine that owns the folder — invoked directly on the
//! laptop, or over ssh when the laptop is driving a server. Keeping them here
//! rather than in the ssh layer means the local and remote paths cannot drift
//! apart.

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::agent;
use crate::application::SessionTerminator;
use crate::model::Session;
use crate::store::HostStore;
use crate::tmux::{self, SessionRef};
use crate::util::{now_ms, proc_ppid};

#[derive(Serialize, Deserialize, Debug)]
pub struct SpawnResult {
    pub session: Session,
    pub tmux_name: String,
    /// False when the session was already running and was left untouched.
    pub started: bool,
    pub cwd: String,
}

/// Start a session's detached tmux session, if it is not already up.
///
/// `host_label` is the name the driving laptop knows this machine by. The host
/// cannot work that out for itself, so it is passed in; without it the label
/// falls back to the machine's own hostname.
pub fn spawn(session_id: Uuid, host_label: Option<&str>) -> Result<SpawnResult> {
    let mut store = HostStore::load()?;

    let session = store
        .session(session_id)
        .cloned()
        .with_context(|| format!("no session {session_id} on this host"))?;
    let folder = store
        .folders
        .iter()
        .find(|f| f.id == session.folder_id)
        .with_context(|| "session points at a folder that no longer exists")?
        .clone();

    if !std::path::Path::new(&folder.path).is_dir() {
        bail!("{} no longer exists on this host", folder.path);
    }
    if !tmux::installed() {
        bail!("tmux is not installed on this host");
    }

    let adapter = agent::by_kind(session.agent);
    if !adapter.installed() {
        bail!("{} is not installed on this host", session.agent);
    }

    let raw = adapter.launch_cmd(session.agent_session_id.as_deref());
    let wrapped = tmux::wrap_command(session.agent.as_str(), &raw, &folder.path);
    let name = SessionRef::new(session.tmux_name());
    let started = tmux::spawn_detached(&name, &folder.path, &wrapped)?;
    // Identity goes onto the tmux session itself, so a probe can tell whose it
    // is without inferring anything from the name it happens to have.
    let _ = tmux::tag_session(&name, &session.id.to_string());

    // Applied every time, not only on creation, so an existing session picks up
    // a corrected label rather than keeping a stale one forever.
    let host = host_label.map_or_else(crate::util::hostname, str::to_string);
    tmux::label_session(&name, &host, &folder.display_name(), session.agent.as_str());

    // Bump the folder's recency so the dashboard floats what you actually use.
    let now = now_ms();
    if let Some(f) = store.folder_mut(folder.id) {
        f.visits += 1;
        f.last_visit = Some(now);
        f.updated_at = now;
    }
    if let Some(s) = store.session_mut(session_id) {
        s.last_attached = Some(now);
        s.updated_at = now;
    }
    store.save()?;

    Ok(SpawnResult {
        session,
        tmux_name: name.name().to_string(),
        started,
        cwd: folder.path,
    })
}

/// Stop a session's tmux session, leaving the record in place so it can be
/// started again later.
pub fn stop(session_id: Uuid) -> Result<bool> {
    let store = HostStore::load()?;
    let session = store
        .session(session_id)
        .with_context(|| format!("no session {session_id} on this host"))?;
    TmuxSessionTerminator.stop(session)
}

pub struct TmuxSessionTerminator;

impl SessionTerminator for TmuxSessionTerminator {
    fn stop(&self, session: &Session) -> Result<bool> {
        let name = SessionRef::new(session.tmux_name());
        if !tmux::has_session(&name) {
            return Ok(false);
        }
        tmux::kill_session(&name)?;
        Ok(true)
    }
}

/// Repair each session's pointer to the agent's own conversation id.
///
/// A session started fresh has no id to record — the agent mints one only once
/// it is running. Rather than guess, the running process is matched back to its
/// session by walking up from the agent's pid to the pid tmux started for that
/// pane. That is what makes a resume after a reboot land in the same
/// conversation instead of an empty one.
///
/// Returns the number of pointers repaired.
pub fn relink_sessions(store: &mut HostStore) -> usize {
    let live: Vec<crate::model::LiveAgent> = agent::all()
        .iter()
        .filter(|a| a.installed())
        .flat_map(|a| a.live())
        .collect();
    if live.is_empty() {
        return 0;
    }

    let mut fixed = 0;
    let ids: Vec<Uuid> = store.live_sessions().iter().map(|s| s.id).collect();
    let pids_by_session = tmux::all_pane_pids();

    for id in ids {
        let Some(session) = store.session(id) else {
            continue;
        };
        let Some(pane_pids) = pids_by_session.get(&session.tmux_name()) else {
            continue;
        };

        let found = live.iter().find(|l| {
            l.agent == session.agent
                && l.agent_session_id.is_some()
                && pane_pids.iter().any(|root| is_descendant(l.pid, *root))
        });

        if let Some(l) = found
            && session.agent_session_id != l.agent_session_id
            && let Some(s) = store.session_mut(id)
        {
            s.agent_session_id = l.agent_session_id.clone();
            s.updated_at = now_ms();
            fixed += 1;
        }
    }
    fixed
}

/// Whether `pid` sits under `root` in the process tree. The walk is bounded so
/// a malformed `/proc` cannot spin here.
fn is_descendant(pid: u32, root: u32) -> bool {
    let mut current = pid;
    for _ in 0..16 {
        if current == root {
            return true;
        }
        match proc_ppid(current) {
            Some(parent) if parent > 1 => current = parent,
            _ => return false,
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_process_is_its_own_descendant() {
        assert!(is_descendant(42, 42));
    }

    #[test]
    fn unrelated_pids_are_not_linked() {
        // pid 1 has no parent above it, so the walk terminates rather than
        // looping or matching by accident.
        assert!(!is_descendant(1, 999_999));
    }

    #[test]
    fn own_process_is_a_descendant_of_its_parent() {
        let me = std::process::id();
        let parent = proc_ppid(me).expect("this test needs /proc");
        assert!(is_descendant(me, parent));
    }
}
