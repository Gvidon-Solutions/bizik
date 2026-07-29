//! Whether a session wants you, and why.
//!
//! An agent's own registry only says *busy* or *idle*, and idle covers two very
//! different situations: it finished and there is something to look at, or it is
//! blocked on a question and will sit there forever. Launching five sessions in
//! the background is only safe if those two can be told apart.
//!
//! The distinction comes from agent lifecycle hooks. A prompt submission marks
//! work as active, a permission request marks it as blocked, and `Stop` marks a
//! completed turn. Each writes a small file here; the probe reads those files
//! and refines the idle case. No hooks installed means no refinement — the
//! status stays honestly vague rather than becoming a guess.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

use crate::util::{atomic_write, cache_dir, now_ms, one_line};

/// Files older than this are ignored: a stale marker from a session that
/// crashed days ago must not keep claiming your attention.
const STALE_AFTER_MS: u64 = 24 * 60 * 60 * 1000;

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum State {
    /// Blocked on the user — a permission prompt or a question.
    Waiting,
    /// A turn ended; there is something to look at.
    Done,
    /// The user replied and it is off again.
    Working,
}

impl State {
    pub fn as_str(&self) -> &'static str {
        match self {
            State::Waiting => "waiting",
            State::Done => "done",
            State::Working => "working",
        }
    }
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Mark {
    pub state: State,
    /// Native Claude/Codex conversation id from the hook payload.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_session_id: Option<String>,
    /// The directory is useful for diagnostics and migration of old records.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    pub at: u64,
}

fn dir() -> PathBuf {
    cache_dir().join("attention")
}

/// Keyed by directory so the module can be exercised without swapping
/// environment variables under a running process.
fn path_in(dir: &Path, session_id: &str) -> PathBuf {
    // A session id comes from the agent, so it is not trusted as a filename.
    let safe: String = session_id
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_')
        .take(64)
        .collect();
    dir.join(format!("{safe}.json"))
}

/// Which hook events map to which state. Unknown events are rejected rather
/// than silently ignored, so a typo in a settings file is visible.
pub fn state_for_event(event: &str) -> Option<State> {
    match event {
        "notification" | "permission" => Some(State::Waiting),
        "stop" => Some(State::Done),
        "prompt" => Some(State::Working),
        _ => None,
    }
}

/// Record a hook firing. `payload` is the JSON the agent wrote to stdin.
pub fn record(event: &str, payload: &str) -> Result<Option<String>> {
    let bzk_session = std::env::var("BZK_SESSION_ID").ok();
    record_for_in(&dir(), event, payload, bzk_session.as_deref())
}

#[cfg(test)]
pub fn record_in(dir: &Path, event: &str, payload: &str) -> Result<Option<String>> {
    record_for_in(dir, event, payload, None)
}

fn record_for_in(
    dir: &Path,
    event: &str,
    payload: &str,
    bzk_session_id: Option<&str>,
) -> Result<Option<String>> {
    let Some(state) = state_for_event(event) else {
        anyhow::bail!("unknown hook event '{event}' (notification, permission, stop, prompt, end)");
    };

    let value: serde_json::Value = serde_json::from_str(payload.trim())
        .with_context(|| format!("hook payload was not JSON: {}", one_line(payload, 80)))?;
    let Some(agent_session_id) = value.get("session_id").and_then(|v| v.as_str()) else {
        // Nothing to key on; drop it rather than write a file nobody can find.
        return Ok(None);
    };
    let key = bzk_session_id
        .filter(|id| !id.trim().is_empty())
        .unwrap_or(agent_session_id);

    let message = value
        .get("message")
        .and_then(|v| v.as_str())
        .map(|m| one_line(m, 120));

    let mark = Mark {
        state,
        agent_session_id: Some(agent_session_id.to_string()),
        cwd: value.get("cwd").and_then(Value::as_str).map(str::to_string),
        message,
        at: now_ms(),
    };
    atomic_write(&path_in(dir, key), &serde_json::to_vec(&mark)?)?;
    Ok(Some(key.to_string()))
}

/// Forget a session's marker, at session end.
pub fn clear(payload: &str) -> Result<Option<String>> {
    let bzk_session = std::env::var("BZK_SESSION_ID").ok();
    clear_for_in(&dir(), payload, bzk_session.as_deref())
}

#[cfg(test)]
pub fn clear_in(dir: &Path, payload: &str) -> Result<Option<String>> {
    clear_for_in(dir, payload, None)
}

fn clear_for_in(dir: &Path, payload: &str, bzk_session_id: Option<&str>) -> Result<Option<String>> {
    let value: serde_json::Value = serde_json::from_str(payload.trim()).unwrap_or_default();
    let Some(agent_session_id) = value.get("session_id").and_then(|v| v.as_str()) else {
        return Ok(None);
    };
    let key = bzk_session_id
        .filter(|id| !id.trim().is_empty())
        .unwrap_or(agent_session_id);
    let _ = std::fs::remove_file(path_in(dir, key));
    // Remove a marker left by an older build that keyed the same conversation
    // by the agent-native id.
    if key != agent_session_id {
        let _ = std::fs::remove_file(path_in(dir, agent_session_id));
    }
    Ok(Some(key.to_string()))
}

/// Every current marker, keyed by the agent's session id.
///
/// Stale files are dropped as they are read, which keeps the directory from
/// accumulating markers for sessions that died without a `SessionEnd`.
pub fn read_all() -> HashMap<String, Mark> {
    read_all_in(&dir())
}

pub fn read_all_in(dir: &Path) -> HashMap<String, Mark> {
    let mut out = HashMap::new();
    let Ok(entries) = std::fs::read_dir(dir) else {
        return out;
    };
    let now = now_ms();

    for entry in entries.flatten() {
        let path = entry.path();
        let Some(id) = path.file_stem().map(|s| s.to_string_lossy().into_owned()) else {
            continue;
        };
        let Ok(raw) = std::fs::read_to_string(&path) else {
            continue;
        };
        let Ok(mark) = serde_json::from_str::<Mark>(&raw) else {
            let _ = std::fs::remove_file(&path);
            continue;
        };
        if now.saturating_sub(mark.at) > STALE_AFTER_MS {
            let _ = std::fs::remove_file(&path);
            continue;
        }
        out.insert(id, mark);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("bzk-att-{}-{}", name, std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn events_map_to_states_and_unknowns_are_rejected() {
        assert_eq!(state_for_event("notification"), Some(State::Waiting));
        assert_eq!(state_for_event("permission"), Some(State::Waiting));
        assert_eq!(state_for_event("stop"), Some(State::Done));
        assert_eq!(state_for_event("prompt"), Some(State::Working));
        assert_eq!(state_for_event("wat"), None);
    }

    #[test]
    fn a_notification_is_recorded_and_read_back() {
        let d = scratch("record");
        let payload = r#"{"session_id":"abc-123","message":"Claude needs your permission"}"#;
        assert_eq!(
            record_in(&d, "notification", payload).unwrap().as_deref(),
            Some("abc-123")
        );

        let all = read_all_in(&d);
        let mark = all.get("abc-123").expect("marker written");
        assert_eq!(mark.state, State::Waiting);
        assert!(mark.message.as_deref().unwrap().contains("permission"));
        std::fs::remove_dir_all(&d).ok();
    }

    #[test]
    fn a_later_event_replaces_an_earlier_one() {
        let d = scratch("replace");
        record_in(&d, "notification", r#"{"session_id":"s1"}"#).unwrap();
        record_in(&d, "prompt", r#"{"session_id":"s1"}"#).unwrap();
        assert_eq!(read_all_in(&d).get("s1").unwrap().state, State::Working);
        std::fs::remove_dir_all(&d).ok();
    }

    #[test]
    fn bzk_session_id_keys_codex_without_losing_native_thread_id() {
        let d = scratch("codex-key");
        let payload = r#"{"session_id":"codex-thread","cwd":"/repo"}"#;
        assert_eq!(
            record_for_in(&d, "prompt", payload, Some("bzk-session"))
                .unwrap()
                .as_deref(),
            Some("bzk-session")
        );
        let mark = read_all_in(&d).remove("bzk-session").unwrap();
        assert_eq!(mark.agent_session_id.as_deref(), Some("codex-thread"));
        assert_eq!(mark.cwd.as_deref(), Some("/repo"));
        std::fs::remove_dir_all(&d).ok();
    }

    #[test]
    fn session_end_removes_the_marker() {
        let d = scratch("clear");
        record_in(&d, "stop", r#"{"session_id":"s2"}"#).unwrap();
        assert!(read_all_in(&d).contains_key("s2"));
        clear_in(&d, r#"{"session_id":"s2"}"#).unwrap();
        assert!(!read_all_in(&d).contains_key("s2"));
        std::fs::remove_dir_all(&d).ok();
    }

    #[test]
    fn a_payload_without_a_session_id_writes_nothing() {
        let d = scratch("nosession");
        assert_eq!(record_in(&d, "stop", r#"{"cwd":"/x"}"#).unwrap(), None);
        assert!(read_all_in(&d).is_empty());
        std::fs::remove_dir_all(&d).ok();
    }

    #[test]
    fn a_session_id_cannot_escape_the_directory() {
        let d = PathBuf::from("/tmp/bzk-att-traversal");
        let p = path_in(&d, "../../etc/passwd");
        assert_eq!(p.parent().unwrap(), d, "must stay in the directory");
        assert_eq!(p.file_name().unwrap().to_string_lossy(), "etcpasswd.json");
    }

    #[test]
    fn stale_markers_are_dropped_rather_than_nagging_forever() {
        let d = scratch("stale");
        record_in(&d, "notification", r#"{"session_id":"old"}"#).unwrap();
        let p = path_in(&d, "old");
        let mut mark: Mark = serde_json::from_str(&std::fs::read_to_string(&p).unwrap()).unwrap();
        mark.at = now_ms() - STALE_AFTER_MS - 1;
        std::fs::write(&p, serde_json::to_vec(&mark).unwrap()).unwrap();

        assert!(read_all_in(&d).is_empty());
        assert!(!p.exists(), "and the file is cleaned up");
        std::fs::remove_dir_all(&d).ok();
    }
}
