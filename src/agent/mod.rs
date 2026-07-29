//! Agent adapters.
//!
//! Every agent stores its conversations differently, and — importantly — they
//! do not offer the same information. Claude Code writes a generated title into
//! its transcript and maintains a live registry with a busy/idle status; Codex
//! has neither. Lifecycle hooks provide finer per-session states when
//! configured. Rather than reduce both to a lowest common denominator, each
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

/// How to invoke an agent: its absolute path, with its own directory put on
/// `PATH` first.
///
/// Relying on `$PATH` is not safe even inside a login shell. Node tools are
/// routinely installed under nvm, whose directory is added by an *interactive*
/// shell's rc file — so `sh -lc codex` fails on a machine where codex is
/// plainly installed.
///
/// The absolute path alone is still not enough. These tools start with
/// `#!/usr/bin/env node`, and the interpreter lives in the same directory as
/// the tool — so without it on `PATH` the kernel finds the script, `env` fails
/// to find node, and the session dies instantly with status 127. Prepending the
/// binary's own directory fixes that for any tool installed alongside its
/// runtime, which is most of them.
pub fn program(agent: &dyn Agent, fallback: &str) -> String {
    let Some(path) = agent.binary() else {
        return fallback.to_string();
    };
    let quoted = crate::util::shell_quote(&path.to_string_lossy());
    let own_dir = path.parent().map(|d| d.to_string_lossy().into_owned());
    // The captured PATH carries whatever the user's shell actually sets up —
    // nvm, pyenv, conda. The tool's own directory goes ahead of it as a
    // belt-and-braces for the case where nothing was ever captured.
    format!(
        "{} {quoted}",
        crate::hostenv::path_prefix(own_dir.as_deref())
    )
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
        .is_ok_and(|metadata| metadata.is_file() && metadata.permissions().mode() & 0o111 != 0)
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
    fn a_resolved_agent_is_launched_by_path_with_its_own_directory_first() {
        // Two separate failures are being avoided: a login shell that cannot
        // find the tool, and one that finds it but not the interpreter its
        // shebang names — which exits 127 the moment the session starts.
        let claude = claude::ClaudeAgent;
        let cmd = claude.launch_cmd(None);
        match claude.binary() {
            Some(path) => {
                assert!(cmd.contains(&path.to_string_lossy().into_owned()));
                assert!(cmd.starts_with("PATH="), "got: {cmd}");
                let dir = path.parent().unwrap().to_string_lossy().into_owned();
                assert!(cmd.contains(&dir), "the tool's own directory comes first");
            }
            None => assert_eq!(cmd, "claude", "fall back to the bare name when unresolved"),
        }
    }

    #[test]
    fn an_unresolvable_agent_falls_back_without_a_path_prefix() {
        struct Missing;
        impl Agent for Missing {
            fn kind(&self) -> AgentKind {
                AgentKind::Codex
            }
            fn caps(&self) -> Caps {
                Caps {
                    titles: false,
                    live_status: false,
                    resume: true,
                }
            }
            fn binary(&self) -> Option<std::path::PathBuf> {
                None
            }
            fn scan_chats(&self, _: &mut ChatIndex, _: &mut Vec<String>) -> Vec<Chat> {
                Vec::new()
            }
            fn live(&self) -> Vec<crate::model::LiveAgent> {
                Vec::new()
            }
            fn launch_cmd(&self, _: Option<&str>) -> String {
                program(self, "codex")
            }
        }
        assert_eq!(Missing.launch_cmd(None), "codex");
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
