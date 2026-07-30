//! Data model.
//!
//! The split matters: **folders and sessions live on the server they belong
//! to**, while the laptop only owns its host list and its layouts. That makes a
//! second laptop a pure view — nothing to sync — and keeps a folder's identity
//! attached to the machine the folder actually exists on.
//!
//! Every record carries a uuid, an `updated_at` and a nullable `deleted_at`.
//! Deletion is a tombstone, never a removal, so a delete can propagate instead
//! of being resurrected by an older copy.

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::util::now_ms;

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[serde(rename_all = "lowercase")]
pub enum AgentKind {
    Claude,
    Codex,
    Shell,
}

impl AgentKind {
    pub const ALL: [AgentKind; 3] = [AgentKind::Claude, AgentKind::Codex, AgentKind::Shell];

    pub fn as_str(&self) -> &'static str {
        match self {
            AgentKind::Claude => "claude",
            AgentKind::Codex => "codex",
            AgentKind::Shell => "shell",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s.to_ascii_lowercase().as_str() {
            "claude" | "cc" => Some(AgentKind::Claude),
            "codex" | "cx" => Some(AgentKind::Codex),
            "shell" | "sh" => Some(AgentKind::Shell),
            _ => None,
        }
    }
}

impl std::fmt::Display for AgentKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A marked directory. Created by `bzk mark` on the machine that holds it, so
/// its uuid is minted once at the source and every laptop agrees on it.
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Folder {
    pub id: Uuid,
    pub path: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub git_remote: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub git_branch: Option<String>,
    pub created_at: u64,
    pub updated_at: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deleted_at: Option<u64>,
    #[serde(default)]
    pub visits: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_visit: Option<u64>,
    /// Pinned projects stay ahead of the normal recency ordering.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub pinned: bool,
    /// Hidden projects remain available without cluttering the default list.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub hidden: bool,
}

impl Folder {
    pub fn new(path: String) -> Self {
        let now = now_ms();
        Self {
            id: Uuid::new_v4(),
            path,
            label: None,
            git_remote: None,
            git_branch: None,
            created_at: now,
            updated_at: now,
            deleted_at: None,
            visits: 0,
            last_visit: None,
            pinned: false,
            hidden: false,
        }
    }

    /// What to show in a list: explicit label, else the last path component.
    pub fn display_name(&self) -> String {
        if let Some(l) = &self.label
            && !l.is_empty()
        {
            return l.clone();
        }
        self.path
            .rsplit('/')
            .find(|s| !s.is_empty())
            .unwrap_or(&self.path)
            .to_string()
    }
}

/// A long-lived named session: one agent, one folder, one conversation you
/// return to over days.
///
/// `agent_session_id` is a *pointer* that gets repaired after each run — never
/// a primary key. Agents keep their session ids stable across resume, but the
/// pointer can still be empty (session never started) or stale (transcript
/// deleted), and the record must survive both.
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Session {
    pub id: Uuid,
    pub folder_id: Uuid,
    pub agent: AgentKind,
    pub title: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_session_id: Option<String>,
    pub created_at: u64,
    pub updated_at: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deleted_at: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_attached: Option<u64>,
}

impl Session {
    pub fn new(folder_id: Uuid, agent: AgentKind, title: String) -> Self {
        let now = now_ms();
        Self {
            id: Uuid::new_v4(),
            folder_id,
            agent,
            title,
            agent_session_id: None,
            created_at: now,
            updated_at: now,
            deleted_at: None,
            last_attached: None,
        }
    }

    /// tmux session name on the remote host. Short enough to read in `tmux ls`,
    /// long enough not to collide, and reversible back to the uuid by prefix.
    pub fn tmux_name(&self) -> String {
        format!("bzk-{}", &self.id.simple().to_string()[..8])
    }
}

/// A machine. `ssh` is `None` for the local machine, so bizik manages the
/// laptop's own folders with exactly the same code path as a VPS.
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Host {
    pub id: Uuid,
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ssh: Option<String>,
    pub created_at: u64,
    pub updated_at: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deleted_at: Option<u64>,
}

impl Host {
    pub fn new(name: String, ssh: Option<String>) -> Self {
        let now = now_ms();
        Self {
            id: Uuid::new_v4(),
            name,
            ssh,
            created_at: now,
            updated_at: now,
            deleted_at: None,
        }
    }

    pub fn is_local(&self) -> bool {
        self.ssh.is_none()
    }
}

/// One pane of a saved layout.
///
/// The host is referenced by *logical name*, not by ssh target — so a layout
/// stays valid when a server changes address, and can be handed to someone else
/// who has a host of the same name.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq, Hash)]
pub struct PaneRef {
    pub host: String,
    pub session: Uuid,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Layout {
    pub id: Uuid,
    pub name: String,
    pub panes: Vec<PaneRef>,
    /// A tmux `window_layout` string, reapplied verbatim on restore to get the
    /// exact geometry back.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tmux_layout: Option<String>,
    pub created_at: u64,
    pub updated_at: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deleted_at: Option<u64>,
}

impl Layout {
    pub fn new(name: String, panes: Vec<PaneRef>, tmux_layout: Option<String>) -> Self {
        let now = now_ms();
        Self {
            id: Uuid::new_v4(),
            name,
            panes,
            tmux_layout,
            created_at: now,
            updated_at: now,
            deleted_at: None,
        }
    }
}

/// An agent-native conversation discovered on disk. Not owned by bizik and not
/// stored — rebuilt from the agent's own transcripts on every probe.
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Chat {
    pub agent: AgentKind,
    pub id: String,
    pub cwd: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_prompt: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub git_branch: Option<String>,
    pub last_active: u64,
    pub size: u64,
}

impl Chat {
    pub fn display_title(&self) -> String {
        self.title
            .clone()
            .or_else(|| self.last_prompt.clone())
            .unwrap_or_else(|| format!("({})", &self.id[..self.id.len().min(8)]))
    }
}

/// A running agent process seen on a host.
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct LiveAgent {
    pub agent: AgentKind,
    pub pid: u32,
    pub cwd: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_session_id: Option<String>,
    /// `idle`, `busy`, or `unknown` when the agent exposes no status.
    pub status: String,
    /// When `status` was last written. Used to decide whether a hook report or
    /// the agent's own status is the fresher account of what is happening.
    #[serde(default)]
    pub status_at: u64,
    /// What the agent's hooks last reported: `waiting`, `done` or `working`.
    /// Absent when hooks are not installed, which is why an idle session
    /// without this stays vaguely "your turn" rather than claiming to know.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attention: Option<String>,
}

/// A tmux session seen on a host.
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct TmuxSession {
    pub name: String,
    pub created: u64,
    pub attached: bool,
    pub windows: u32,
    /// Last non-empty lines of the first pane, for a preview in the dashboard.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preview: Option<String>,
    /// The bizik session uuid tmux itself is carrying for this session, when
    /// one was tagged. Identity read back from tmux rather than inferred from
    /// the session's name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owner: Option<String>,
}

/// Shape of the report a host sends back.
///
/// Bumped whenever the laptop could misread an older host's answer. Both sides
/// ship in one binary, so a mismatch means one of them was not updated — and
/// saying that outright beats a deserialisation error naming a field nobody
/// recognises.
pub const PROTOCOL: u32 = 1;

/// What one host reports in a single ssh round trip.
///
/// Sessions arrive already resolved: the host is the only place that can see
/// its own tmux and processes, so it is the only place that should be deciding
/// what state a session is in. The laptop displays this rather than re-deriving
/// it, which is what kept three copies of that logic disagreeing.
#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct Probe {
    #[serde(default)]
    pub protocol: u32,
    pub bzk_version: String,
    #[serde(default)]
    pub folders: Vec<Folder>,
    #[serde(default)]
    pub sessions: Vec<crate::reconcile::SessionView>,
    #[serde(default)]
    pub chats: Vec<Chat>,
    /// Sessions running here that no record claims.
    #[serde(default)]
    pub orphans: Vec<crate::reconcile::Orphan>,
    /// Agents whose binary is present on this host.
    #[serde(default)]
    pub agents: Vec<AgentKind>,
    /// Whether the hooks that distinguish "blocked on you" from "finished" are
    /// installed here.
    #[serde(default)]
    pub hooks_installed: bool,
    /// When this host's PATH was last captured, if ever.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub env_captured_at: Option<u64>,
    /// Non-fatal problems worth surfacing instead of hiding.
    #[serde(default)]
    pub warnings: Vec<String>,
}

impl Probe {
    /// Live records, for the places that only need the intent.
    pub fn records(&self) -> impl Iterator<Item = &Session> {
        self.sessions.iter().map(|v| &v.session)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn display_name_falls_back_to_last_path_component() {
        let mut f = Folder::new("/home/me/repos/thing/".into());
        assert_eq!(f.display_name(), "thing");
        f.label = Some("backend".into());
        assert_eq!(f.display_name(), "backend");
        f.label = Some(String::new());
        assert_eq!(f.display_name(), "thing");
    }

    #[test]
    fn tmux_name_is_short_and_derived_from_id() {
        let s = Session::new(Uuid::new_v4(), AgentKind::Claude, "t".into());
        let name = s.tmux_name();
        assert_eq!(name.len(), 12);
        assert!(s.id.simple().to_string().starts_with(&name[4..]));
    }

    #[test]
    fn agent_kind_roundtrips() {
        for a in AgentKind::ALL {
            assert_eq!(AgentKind::parse(a.as_str()), Some(a));
        }
    }
}
