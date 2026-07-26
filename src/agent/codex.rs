//! Codex adapter.
//!
//! Layout on disk: `~/.codex/sessions/YYYY/MM/DD/rollout-<ts>-<uuid>.jsonl`.
//! The first record is `session_meta`, whose payload carries the working
//! directory and the session id — cheaper to read than Claude's format.
//!
//! What Codex does *not* have is any equivalent of Claude's live registry or
//! its generated titles. So [`Caps`] reports `titles: false` and
//! `live_status: false`, liveness is recovered by walking `/proc`, and the UI
//! shows those sessions as running-or-not rather than pretending to know
//! whether one is waiting on input.

use anyhow::Result;
use serde_json::Value;
use std::path::{Path, PathBuf};

use super::{Agent, Caps, ChatIndex, HEAD_BYTES, TAIL_BYTES, extract_text, looks_synthetic, parse_lines};
use crate::model::{AgentKind, Chat, LiveAgent};
use crate::util::{
    file_size, home, mtime_ms, one_line, pids_by_comm, proc_cwd, read_head, read_tail,
};

pub struct CodexAgent;

fn sessions_root() -> PathBuf {
    home().join(".codex/sessions")
}

impl Agent for CodexAgent {
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

    fn binary(&self) -> Option<PathBuf> {
        super::find_binary("codex")
    }

    fn scan_chats(&self, index: &mut ChatIndex, warnings: &mut Vec<String>) -> Vec<Chat> {
        let mut files = Vec::new();
        collect_rollouts(&sessions_root(), 0, &mut files);

        let mut chats = Vec::new();
        for path in files {
            let key = path.to_string_lossy().into_owned();
            let (mtime, size) = (mtime_ms(&path), file_size(&path));

            if let Some(cached) = index.get(&key, mtime, size) {
                chats.push(cached.clone());
                continue;
            }
            match parse_rollout(&path, mtime, size) {
                Ok(Some(chat)) => {
                    index.put(key, mtime, size, chat.clone());
                    chats.push(chat);
                }
                // A rollout with no meta record is not a conversation; saying
                // it is damaged would be a false alarm.
                Ok(None) => {}
                Err(e) => warnings.push(format!("codex: cannot read {key}: {e}")),
            }
        }
        chats
    }

    /// Without a registry, a running Codex is just a process. Its working
    /// directory comes from `/proc/<pid>/cwd`; its conversation id is not
    /// recoverable this way, and is left empty rather than guessed.
    fn live(&self) -> Vec<LiveAgent> {
        pids_by_comm("codex")
            .into_iter()
            .filter_map(|pid| {
                Some(LiveAgent {
                    agent: AgentKind::Codex,
                    pid,
                    cwd: proc_cwd(pid)?,
                    agent_session_id: None,
                    status: "unknown".into(),
                })
            })
            .collect()
    }

    fn launch_cmd(&self, resume: Option<&str>) -> String {
        let bin = super::program(self, "codex");
        match resume {
            Some(id) => format!("{bin} resume {}", crate::util::shell_quote(id)),
            None => bin,
        }
    }
}

/// Walk the `YYYY/MM/DD` tree gathering rollout files. Depth is bounded so a
/// symlink loop cannot hang a probe.
fn collect_rollouts(dir: &Path, depth: usize, out: &mut Vec<PathBuf>) {
    if depth > 4 {
        return;
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_rollouts(&path, depth + 1, out);
        } else if path.extension().and_then(|e| e.to_str()) == Some("jsonl")
            && path
                .file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with("rollout-"))
        {
            out.push(path);
        }
    }
}

fn parse_rollout(path: &Path, mtime: u64, size: u64) -> Result<Option<Chat>> {
    let head = read_head(path, HEAD_BYTES)?;

    let mut id: Option<String> = None;
    let mut cwd: Option<String> = None;
    let mut first_prompt: Option<String> = None;

    for v in parse_lines(&head, false) {
        let payload = v.get("payload");
        if v.get("type").and_then(Value::as_str) == Some("session_meta")
            && let Some(p) = payload
        {
            cwd = p.get("cwd").and_then(Value::as_str).map(str::to_string);
            id = p
                .get("session_id")
                .or_else(|| p.get("id"))
                .and_then(Value::as_str)
                .map(str::to_string);
        }
        if first_prompt.is_none()
            && let Some(text) = user_text(payload)
            && !looks_synthetic(&text)
        {
            first_prompt = Some(one_line(&text, 160));
        }
    }

    // No meta record means no id and no working directory, and a rollout that
    // cannot be tied to a folder is not something we can offer to resume.
    let (Some(id), Some(cwd)) = (id, cwd) else {
        return Ok(None);
    };

    let mut last_prompt = first_prompt;
    if size > TAIL_BYTES as u64 {
        let tail = read_tail(path, TAIL_BYTES)?;
        for v in parse_lines(&tail, true) {
            if let Some(text) = user_text(v.get("payload"))
                && !looks_synthetic(&text)
            {
                last_prompt = Some(one_line(&text, 160));
            }
        }
    }

    Ok(Some(Chat {
        agent: AgentKind::Codex,
        id,
        cwd,
        // Codex generates no title; the UI falls back to the last prompt rather
        // than displaying something invented here.
        title: None,
        last_prompt,
        git_branch: None,
        last_active: mtime,
        size,
    }))
}

/// Text of a user message, if this payload is one.
fn user_text(payload: Option<&Value>) -> Option<String> {
    let p = payload?;
    if p.get("role").and_then(Value::as_str) != Some("user") {
        return None;
    }
    extract_text(p.get("content")?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn write(dir: &Path, name: &str, lines: &[&str]) -> PathBuf {
        std::fs::create_dir_all(dir).unwrap();
        let p = dir.join(name);
        let mut f = std::fs::File::create(&p).unwrap();
        for l in lines {
            writeln!(f, "{l}").unwrap();
        }
        p
    }

    #[test]
    fn parses_meta_and_skips_the_agents_md_preamble() {
        let dir = std::env::temp_dir().join(format!("bzk-codex-{}", std::process::id()));
        let p = write(
            &dir,
            "rollout-2026-07-15T18-31-31-019f6667.jsonl",
            &[
                r#"{"type":"session_meta","payload":{"session_id":"019f6667","cwd":"/home/me/research"}}"#,
                // Doubled hashes: the payload itself contains `"#`.
                r##"{"type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"# AGENTS.md instructions for /home/me/research"}]}}"##,
                r#"{"type":"response_item","payload":{"type":"message","role":"assistant","content":[{"type":"output_text","text":"ok"}]}}"#,
                r#"{"type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"проверь расчёты"}]}}"#,
            ],
        );
        let chat = parse_rollout(&p, 7, file_size(&p)).unwrap().unwrap();
        assert_eq!(chat.id, "019f6667");
        assert_eq!(chat.cwd, "/home/me/research");
        assert_eq!(chat.last_prompt.as_deref(), Some("проверь расчёты"));
        assert!(chat.title.is_none());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn rollout_without_meta_is_rejected() {
        let dir = std::env::temp_dir().join(format!("bzk-codex-bad-{}", std::process::id()));
        let p = write(&dir, "rollout-x.jsonl", &[r#"{"type":"event_msg"}"#]);
        assert!(parse_rollout(&p, 1, file_size(&p)).unwrap().is_none(), "skipped, not reported as damaged");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn collect_only_picks_rollout_jsonl() {
        let dir = std::env::temp_dir().join(format!("bzk-codex-walk-{}", std::process::id()));
        write(&dir.join("2026/07/15"), "rollout-a.jsonl", &["{}"]);
        write(&dir.join("2026/07/15"), "notes.txt", &["x"]);
        write(&dir.join("2026/07/15"), "other.jsonl", &["{}"]);
        let mut out = Vec::new();
        collect_rollouts(&dir, 0, &mut out);
        assert_eq!(out.len(), 1);
        std::fs::remove_dir_all(&dir).ok();
    }
}
