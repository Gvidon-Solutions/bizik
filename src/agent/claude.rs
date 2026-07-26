//! Claude Code adapter.
//!
//! Layout on disk:
//!
//! * `~/.claude/projects/<escaped-cwd>/<session-id>.jsonl` — transcripts. The
//!   directory name is the working directory with separators replaced, but the
//!   escaping rule is undocumented, so the `cwd` is read from inside the file
//!   instead of reconstructed from the directory name. That is authoritative
//!   and immune to the rule changing.
//! * `~/.claude/sessions/<pid>.json` — a registry of live sessions carrying a
//!   `status` of `idle` or `busy`.
//!
//! The registry is not self-cleaning: entries survive a crash still claiming to
//! be busy, so every pid is verified against `/proc` — including its start
//! time, which the registry records precisely so a recycled pid can be caught.

use anyhow::Result;
use serde::Deserialize;
use serde_json::Value;
use std::path::PathBuf;

use super::{Agent, Caps, ChatIndex, HEAD_BYTES, TAIL_BYTES, extract_text, looks_synthetic, parse_lines};
use crate::model::{AgentKind, Chat, LiveAgent};
use crate::util::{file_size, home, mtime_ms, one_line, proc_matches, read_head, read_tail};

pub struct ClaudeAgent;

fn projects_dir() -> PathBuf {
    home().join(".claude/projects")
}

fn sessions_dir() -> PathBuf {
    home().join(".claude/sessions")
}

/// One entry of `~/.claude/sessions/<pid>.json`.
#[derive(Deserialize, Debug)]
#[serde(rename_all = "camelCase")]
struct RegistryEntry {
    pid: u32,
    session_id: String,
    cwd: String,
    #[serde(default)]
    status: Option<String>,
    /// When the status was last written; falls back to the record's own stamp.
    #[serde(default)]
    status_updated_at: Option<u64>,
    #[serde(default)]
    updated_at: Option<u64>,
    /// Process start time in clock ticks; guards against pid reuse.
    #[serde(default)]
    proc_start: Option<String>,
}

impl Agent for ClaudeAgent {
    fn kind(&self) -> AgentKind {
        AgentKind::Claude
    }

    fn caps(&self) -> Caps {
        Caps {
            titles: true,
            live_status: true,
            resume: true,
        }
    }

    fn binary(&self) -> Option<PathBuf> {
        super::find_binary("claude")
    }

    fn scan_chats(&self, index: &mut ChatIndex, warnings: &mut Vec<String>) -> Vec<Chat> {
        let root = projects_dir();
        let Ok(projects) = std::fs::read_dir(&root) else {
            return Vec::new();
        };

        let mut chats = Vec::new();
        for project in projects.flatten() {
            let Ok(files) = std::fs::read_dir(project.path()) else {
                continue;
            };
            for file in files.flatten() {
                let path = file.path();
                if path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
                    continue;
                }
                let key = path.to_string_lossy().into_owned();
                let (mtime, size) = (mtime_ms(&path), file_size(&path));

                if let Some(cached) = index.get(&key, mtime, size) {
                    chats.push(cached.clone());
                    continue;
                }
                match parse_transcript(&path, mtime, size) {
                    Ok(Some(chat)) => {
                        index.put(key, mtime, size, chat.clone());
                        chats.push(chat);
                    }
                    // Background jobs leave tiny stub transcripts with no
                    // working directory. They are not conversations, and
                    // reporting them as damaged would be crying wolf.
                    Ok(None) => {}
                    Err(e) => warnings.push(format!("claude: cannot read {key}: {e}")),
                }
            }
        }
        chats
    }

    fn live(&self) -> Vec<LiveAgent> {
        let Ok(entries) = std::fs::read_dir(sessions_dir()) else {
            return Vec::new();
        };
        let mut out = Vec::new();
        for entry in entries.flatten() {
            let Ok(raw) = std::fs::read_to_string(entry.path()) else {
                continue;
            };
            let Ok(reg) = serde_json::from_str::<RegistryEntry>(&raw) else {
                continue;
            };
            // A stale record outlives its process and keeps claiming `busy`.
            if !proc_matches(reg.pid, "claude", reg.proc_start.as_deref()) {
                continue;
            }
            out.push(LiveAgent {
                agent: AgentKind::Claude,
                pid: reg.pid,
                cwd: reg.cwd,
                agent_session_id: Some(reg.session_id),
                status: reg.status.unwrap_or_else(|| "unknown".into()),
                status_at: reg.status_updated_at.or(reg.updated_at).unwrap_or(0),
                attention: None,
            });
        }
        out
    }

    fn launch_cmd(&self, resume: Option<&str>) -> String {
        let bin = super::program(self, "claude");
        match resume {
            Some(id) => format!("{bin} --resume {}", crate::util::shell_quote(id)),
            None => bin,
        }
    }
}

/// Read the bounded head and tail of a transcript and assemble a [`Chat`].
///
/// The head yields the working directory and the opening prompt; the tail
/// yields the freshest title (Claude rewrites it as the conversation evolves)
/// and the most recent prompt, which is what makes a list of chats readable.
///
/// `Ok(None)` means the file is valid but is not a conversation — the two cases
/// are kept apart so only a genuine read failure is reported as one.
fn parse_transcript(path: &std::path::Path, mtime: u64, size: u64) -> Result<Option<Chat>> {
    let Some(id) = path.file_stem().map(|s| s.to_string_lossy().into_owned()) else {
        return Ok(None);
    };

    let head = read_head(path, HEAD_BYTES)?;
    let mut cwd: Option<String> = None;
    let mut git_branch: Option<String> = None;
    let mut title: Option<String> = None;
    let mut first_prompt: Option<String> = None;

    for v in parse_lines(&head, false) {
        match v.get("type").and_then(Value::as_str) {
            Some("ai-title") => {
                if let Some(t) = v.get("aiTitle").and_then(Value::as_str) {
                    title = Some(t.to_string());
                }
            }
            Some("user") => {
                if cwd.is_none() {
                    cwd = v.get("cwd").and_then(Value::as_str).map(str::to_string);
                }
                if git_branch.is_none() {
                    git_branch = v
                        .get("gitBranch")
                        .and_then(Value::as_str)
                        .filter(|b| !b.is_empty() && *b != "HEAD")
                        .map(str::to_string);
                }
                // Subagent traffic shares the file; it is not the conversation.
                if first_prompt.is_none() && v.get("isSidechain") != Some(&Value::Bool(true))
                    && let Some(text) = v.get("message").and_then(|m| m.get("content")).and_then(extract_text)
                        && !looks_synthetic(&text)
                    {
                        first_prompt = Some(one_line(&text, 160));
                    }
            }
            _ => {
                if cwd.is_none() {
                    cwd = v.get("cwd").and_then(Value::as_str).map(str::to_string);
                }
            }
        }
    }

    // A transcript without a working directory cannot belong to a folder — it
    // is a background job's stub, not a conversation.
    let Some(cwd) = cwd else { return Ok(None) };

    let mut last_prompt = first_prompt.clone();
    if size > TAIL_BYTES as u64 {
        let tail = read_tail(path, TAIL_BYTES)?;
        for v in parse_lines(&tail, true) {
            match v.get("type").and_then(Value::as_str) {
                Some("ai-title") => {
                    if let Some(t) = v.get("aiTitle").and_then(Value::as_str) {
                        title = Some(t.to_string());
                    }
                }
                Some("user") if v.get("isSidechain") != Some(&Value::Bool(true)) => {
                    if let Some(text) = v.get("message").and_then(|m| m.get("content")).and_then(extract_text)
                        && !looks_synthetic(&text)
                    {
                        last_prompt = Some(one_line(&text, 160));
                    }
                }
                _ => {}
            }
        }
    }

    Ok(Some(Chat {
        agent: AgentKind::Claude,
        id,
        cwd,
        title,
        last_prompt,
        git_branch,
        last_active: mtime,
        size,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn write_transcript(dir: &std::path::Path, name: &str, lines: &[&str]) -> PathBuf {
        std::fs::create_dir_all(dir).unwrap();
        let p = dir.join(name);
        let mut f = std::fs::File::create(&p).unwrap();
        for l in lines {
            writeln!(f, "{l}").unwrap();
        }
        p
    }

    #[test]
    fn parses_title_cwd_and_skips_synthetic_and_sidechain_prompts() {
        let dir = std::env::temp_dir().join(format!("bzk-claude-{}", std::process::id()));
        let p = write_transcript(
            &dir,
            "abc.jsonl",
            &[
                r#"{"type":"mode","mode":"normal"}"#,
                r#"{"type":"user","isSidechain":false,"cwd":"/repo","gitBranch":"main","message":{"role":"user","content":"<environment_context>noise"}}"#,
                r#"{"type":"user","isSidechain":true,"cwd":"/repo","message":{"role":"user","content":"subagent chatter"}}"#,
                r#"{"type":"user","isSidechain":false,"cwd":"/repo","message":{"role":"user","content":"fix the login bug"}}"#,
                r#"{"type":"ai-title","aiTitle":"Login bug"}"#,
            ],
        );
        let chat = parse_transcript(&p, 42, file_size(&p)).unwrap().unwrap();
        assert_eq!(chat.id, "abc");
        assert_eq!(chat.cwd, "/repo");
        assert_eq!(chat.git_branch.as_deref(), Some("main"));
        assert_eq!(chat.title.as_deref(), Some("Login bug"));
        assert_eq!(chat.last_prompt.as_deref(), Some("fix the login bug"));
        assert_eq!(chat.last_active, 42);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn transcript_without_cwd_is_rejected() {
        let dir = std::env::temp_dir().join(format!("bzk-claude-nocwd-{}", std::process::id()));
        let p = write_transcript(&dir, "x.jsonl", &[r#"{"type":"mode","mode":"normal"}"#]);
        assert!(parse_transcript(&p, 1, file_size(&p)).unwrap().is_none(), "a stub is skipped, not an error");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn head_only_transcript_uses_first_prompt_as_last() {
        let dir = std::env::temp_dir().join(format!("bzk-claude-head-{}", std::process::id()));
        let p = write_transcript(
            &dir,
            "y.jsonl",
            &[r#"{"type":"user","isSidechain":false,"cwd":"/r","message":{"role":"user","content":"only prompt"}}"#],
        );
        let chat = parse_transcript(&p, 1, file_size(&p)).unwrap().unwrap();
        assert_eq!(chat.last_prompt.as_deref(), Some("only prompt"));
        assert!(chat.title.is_none(), "no ai-title means no invented title");
        std::fs::remove_dir_all(&dir).ok();
    }
}
