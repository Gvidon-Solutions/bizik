//! Agent adapters.
//!
//! Every agent stores its conversations differently, and — importantly — they
//! do not offer the same information. Claude Code writes a generated title into
//! its transcript and maintains a live registry with a busy/idle status; Codex
//! has neither. Rather than reduce both to a lowest common denominator, each
//! adapter declares [`Caps`] and the UI degrades *visibly*: a missing status is
//! shown as unknown, never invented.
//!
//! Transcripts reach tens of megabytes, so no adapter may read a whole file.
//! Parsing is bounded to a head and a tail slice, and results are cached in
//! [`ChatIndex`] keyed by mtime and size — steady state re-reads nothing.

pub mod claude;
pub mod codex;

use anyhow::Result;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

use crate::model::{AgentKind, Chat, LiveAgent};
use crate::util::{atomic_write, cache_dir, home};

/// Bytes read from the start of a transcript. Enough to reach the first real
/// exchange even when a system preamble is large.
pub const HEAD_BYTES: usize = 512 * 1024;
/// Bytes read from the end of a transcript, for the most recent title and the
/// last prompt.
pub const TAIL_BYTES: usize = 128 * 1024;

/// What an adapter can actually deliver. Anything false must degrade in the UI
/// rather than be faked.
#[derive(Serialize, Deserialize, Clone, Copy, Debug)]
pub struct Caps {
    /// Supplies a human-readable conversation title of its own.
    pub titles: bool,
    /// Reports a live busy/idle status per session.
    pub live_status: bool,
    /// Can reopen a specific past conversation by id.
    pub resume: bool,
}

pub trait Agent {
    fn kind(&self) -> AgentKind;
    fn caps(&self) -> Caps;

    /// Absolute path to the binary, if this agent is installed here.
    fn binary(&self) -> Option<PathBuf>;

    /// All conversations found on disk, using and updating the cache.
    fn scan_chats(&self, index: &mut ChatIndex, warnings: &mut Vec<String>) -> Vec<Chat>;

    /// Agent processes running right now.
    fn live(&self) -> Vec<LiveAgent>;

    /// Shell command that starts this agent, optionally resuming a conversation.
    fn launch_cmd(&self, resume: Option<&str>) -> String;

    fn installed(&self) -> bool {
        self.binary().is_some()
    }
}

/// How an agent should be named in a command: by absolute path when we know it.
///
/// Relying on `$PATH` is not safe even inside a login shell. Node tools are
/// routinely installed under nvm, whose directory is added by an *interactive*
/// shell's rc file — so `sh -lc codex` fails on a machine where codex is
/// plainly installed. Resolving the path here, on the host that will run it,
/// removes the guesswork entirely.
pub fn program(agent: &dyn Agent, fallback: &str) -> String {
    agent
        .binary()
        .map(|p| crate::util::shell_quote(&p.to_string_lossy()))
        .unwrap_or_else(|| fallback.to_string())
}

pub fn all() -> Vec<Box<dyn Agent>> {
    vec![
        Box::new(claude::ClaudeAgent),
        Box::new(codex::CodexAgent),
        Box::new(ShellAgent),
    ]
}

pub fn by_kind(kind: AgentKind) -> Box<dyn Agent> {
    match kind {
        AgentKind::Claude => Box::new(claude::ClaudeAgent),
        AgentKind::Codex => Box::new(codex::CodexAgent),
        AgentKind::Shell => Box::new(ShellAgent),
    }
}

/// A plain login shell. Not really an agent, but it makes "just give me a
/// terminal in that folder" work through the same machinery as everything else.
pub struct ShellAgent;

impl Agent for ShellAgent {
    fn kind(&self) -> AgentKind {
        AgentKind::Shell
    }

    fn caps(&self) -> Caps {
        Caps {
            titles: false,
            live_status: false,
            resume: false,
        }
    }

    fn binary(&self) -> Option<PathBuf> {
        Some(PathBuf::from(
            std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".into()),
        ))
    }

    fn scan_chats(&self, _index: &mut ChatIndex, _warnings: &mut Vec<String>) -> Vec<Chat> {
        Vec::new()
    }

    fn live(&self) -> Vec<LiveAgent> {
        Vec::new()
    }

    fn launch_cmd(&self, _resume: Option<&str>) -> String {
        "exec \"$SHELL\" -l".to_string()
    }
}

// ---------------------------------------------------------------------------
// Binary lookup
// ---------------------------------------------------------------------------

/// Locate a binary by name.
///
/// `$PATH` alone is not enough: a probe arrives over a non-interactive ssh
/// session, which does not source login files, so the very directories these
/// tools install into are usually missing. The extra candidates cover the
/// common per-user install locations.
pub fn find_binary(name: &str) -> Option<PathBuf> {
    if let Some(path) = std::env::var_os("PATH") {
        for dir in std::env::split_paths(&path) {
            let candidate = dir.join(name);
            if is_executable(&candidate) {
                return Some(candidate);
            }
        }
    }
    let h = home();
    let extra = [
        h.join(".local/bin"),
        h.join(".bun/bin"),
        h.join(".npm-global/bin"),
        h.join(".volta/bin"),
        h.join(".cargo/bin"),
        PathBuf::from("/usr/local/bin"),
        PathBuf::from("/opt/homebrew/bin"),
    ];
    for dir in extra {
        let candidate = dir.join(name);
        if is_executable(&candidate) {
            return Some(candidate);
        }
    }
    // nvm keeps binaries under a per-version directory.
    if let Ok(versions) = std::fs::read_dir(h.join(".nvm/versions/node")) {
        for v in versions.flatten() {
            let candidate = v.path().join("bin").join(name);
            if is_executable(&candidate) {
                return Some(candidate);
            }
        }
    }
    None
}

fn is_executable(p: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(p)
        .map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

// ---------------------------------------------------------------------------
// Chat index cache
// ---------------------------------------------------------------------------

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct ChatEntry {
    pub mtime: u64,
    pub size: u64,
    pub chat: Chat,
}

/// Cache of parsed transcripts, keyed by absolute file path.
///
/// A transcript is re-parsed only when its mtime or size changed, which keeps a
/// probe cheap no matter how large the history gets.
#[derive(Serialize, Deserialize, Default, Debug)]
pub struct ChatIndex {
    #[serde(default)]
    pub entries: HashMap<String, ChatEntry>,
}

impl ChatIndex {
    pub fn path() -> PathBuf {
        cache_dir().join("chats.json")
    }

    pub fn load() -> Self {
        std::fs::read_to_string(Self::path())
            .ok()
            .and_then(|raw| serde_json::from_str(&raw).ok())
            .unwrap_or_default()
    }

    pub fn save(&self) -> Result<()> {
        atomic_write(&Self::path(), &serde_json::to_vec(self)?)
    }

    pub fn get(&self, path: &str, mtime: u64, size: u64) -> Option<&Chat> {
        self.entries
            .get(path)
            .filter(|e| e.mtime == mtime && e.size == size)
            .map(|e| &e.chat)
    }

    pub fn put(&mut self, path: String, mtime: u64, size: u64, chat: Chat) {
        self.entries.insert(path, ChatEntry { mtime, size, chat });
    }

    /// Drop entries whose file no longer exists, so the cache cannot grow
    /// without bound.
    pub fn prune(&mut self, seen: &[String]) {
        self.entries.retain(|k, _| seen.contains(k));
    }
}

// ---------------------------------------------------------------------------
// JSON helpers shared by the adapters
// ---------------------------------------------------------------------------

/// Pull plain text out of a message body that may be either a bare string or an
/// array of content blocks.
pub fn extract_text(v: &Value) -> Option<String> {
    match v {
        Value::String(s) => Some(s.clone()),
        Value::Array(items) => {
            let joined: Vec<String> = items
                .iter()
                .filter_map(|item| item.get("text").and_then(Value::as_str).map(str::to_string))
                .collect();
            if joined.is_empty() {
                None
            } else {
                Some(joined.join(" "))
            }
        }
        _ => None,
    }
}

/// Both agents prepend machine-generated context to a conversation. Those must
/// never be mistaken for the user's first prompt.
pub fn looks_synthetic(text: &str) -> bool {
    let t = text.trim_start();
    t.starts_with('<')
        || t.starts_with("# AGENTS.md")
        || t.starts_with("Caveat:")
        || t.starts_with("[Request interrupted")
        // Written by the agent when a conversation is compacted and resumed.
        // It reads like a prompt but says nothing about what the work is.
        || t.starts_with("This session is being continued")
        || t.contains("<system-reminder>")
        || t.contains("<INSTRUCTIONS>")
        || t.contains("<environment_context>")
        || t.contains("<user-prompt-submit-hook>")
}

/// Parse a slice of JSONL, skipping the first line when it may be a fragment of
/// a longer one (as it always is for a tail read).
pub fn parse_lines(raw: &str, skip_first: bool) -> impl Iterator<Item = Value> + '_ {
    raw.lines()
        .skip(usize::from(skip_first))
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn extract_text_handles_both_shapes() {
        assert_eq!(extract_text(&json!("hi")).as_deref(), Some("hi"));
        let blocks =
            json!([{"type":"text","text":"a"},{"type":"image"},{"type":"text","text":"b"}]);
        assert_eq!(extract_text(&blocks).as_deref(), Some("a b"));
        assert_eq!(extract_text(&json!([{"type":"image"}])), None);
        assert_eq!(extract_text(&json!(7)), None);
    }

    #[test]
    fn synthetic_preambles_are_recognised() {
        assert!(looks_synthetic("<environment_context>\nfoo"));
        assert!(looks_synthetic("# AGENTS.md instructions for /x"));
        assert!(looks_synthetic("real text <system-reminder> hidden"));
        assert!(!looks_synthetic("Please fix the login bug"));
    }

    #[test]
    fn tail_parsing_drops_the_partial_first_line() {
        let raw = "n-of-a-line\n{\"a\":1}\n{\"b\":2}";
        let got: Vec<Value> = parse_lines(raw, true).collect();
        assert_eq!(got.len(), 2);
    }

    #[test]
    fn a_resolved_agent_is_launched_by_absolute_path() {
        // `$PATH` is not to be trusted here: nvm-installed tools are missing
        // from a login shell's environment even when plainly installed.
        let claude = claude::ClaudeAgent;
        let cmd = claude.launch_cmd(None);
        if let Some(path) = claude.binary() {
            assert!(cmd.contains(&path.to_string_lossy().into_owned()));
            assert!(cmd.starts_with('\''), "the path must be quoted");
        } else {
            assert_eq!(cmd, "claude", "fall back to the bare name when unresolved");
        }
    }

    #[test]
    fn index_entry_is_invalidated_by_mtime_or_size() {
        let mut idx = ChatIndex::default();
        let chat = Chat {
            agent: AgentKind::Claude,
            id: "x".into(),
            cwd: "/tmp".into(),
            title: None,
            last_prompt: None,
            git_branch: None,
            last_active: 1,
            size: 10,
        };
        idx.put("/p".into(), 100, 10, chat);
        assert!(idx.get("/p", 100, 10).is_some());
        assert!(idx.get("/p", 101, 10).is_none());
        assert!(idx.get("/p", 100, 11).is_none());
    }
}
