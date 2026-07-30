//! Codex adapter.
//!
//! Layout on disk: `~/.codex/sessions/YYYY/MM/DD/rollout-<ts>-<uuid>.jsonl`.
//! The first record is `session_meta`, whose payload carries the working
//! directory and the session id — cheaper to read than Claude's format.
//! Modern Codex stores thread names in `~/.codex/state_<version>.sqlite`;
//! bizik reads them through Codex's app-server API. Older releases used
//! `~/.codex/session_index.jsonl`, which remains a fallback. Both are read
//! separately because a title can change without touching the rollout file.
//!
//! Codex has no process registry, so [`Caps`] still reports
//! `live_status: false`, and liveness is recovered by walking `/proc`. When
//! bizik's lifecycle hooks are installed, their exact per-session reports
//! refine that process-level view into working, waiting, and done.

use anyhow::{Context, Result, anyhow, bail};
use serde::Deserialize;
use serde_json::Value;
use std::collections::HashMap;
use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::thread;
use std::time::{Duration, Instant};

use super::{
    Agent, Caps, ChatIndex, CodexTitleSource, HEAD_BYTES, TAIL_BYTES, extract_text,
    looks_synthetic, parse_lines,
};
use crate::model::{AgentKind, Chat, LiveAgent};
use crate::util::{
    file_size, home, mtime_ms, one_line, pids_by_comm, proc_cwd, read_head, read_tail,
};

pub struct CodexAgent;

const DEFAULT_THEME: &str = "github";
const TITLE_REFRESH_INTERVAL_MS: u64 = 30_000;

fn codex_home() -> PathBuf {
    std::env::var_os("CODEX_HOME").map_or_else(|| home().join(".codex"), PathBuf::from)
}

fn sessions_root() -> PathBuf {
    codex_home().join("sessions")
}

/// Codex chooses a syntax-highlighting theme when its TUI starts. A bizik agent
/// starts in a detached tmux session, before any terminal is attached, so the
/// terminal background query has nobody to answer it and Codex falls back to a
/// dark code/diff palette even when the eventual viewer is light.
///
/// Respect an explicit Codex theme. Otherwise use a light theme that defines
/// its own inserted/deleted backgrounds. Merely selecting a light syntax
/// palette is not enough: when OSC background detection fails, Codex otherwise
/// applies its dark fallback to diffs. The environment knob is useful for
/// one-off launches and `inherit` restores Codex's automatic choice.
fn launch_theme() -> Option<String> {
    let requested = std::env::var("BIZIK_CODEX_THEME").ok();
    choose_theme(config_has_theme(), requested.as_deref())
}

fn choose_theme(configured: bool, requested: Option<&str>) -> Option<String> {
    match requested.map(str::trim) {
        Some("inherit" | "off") => None,
        Some(theme) if valid_theme_name(theme) => Some(theme.to_string()),
        Some(_) => Some(DEFAULT_THEME.to_string()),
        None if configured => None,
        None => Some(DEFAULT_THEME.to_string()),
    }
}

fn valid_theme_name(theme: &str) -> bool {
    !theme.is_empty()
        && theme
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
}

fn config_has_theme() -> bool {
    fs::read_to_string(codex_home().join("config.toml"))
        .is_ok_and(|raw| config_text_has_theme(&raw))
}

fn config_text_has_theme(raw: &str) -> bool {
    let mut in_tui = false;
    for raw_line in raw.lines() {
        let line = raw_line.split('#').next().unwrap_or("").trim();
        if line.starts_with('[') && line.ends_with(']') {
            in_tui = line[1..line.len() - 1].trim() == "tui";
            continue;
        }
        let Some((key, _)) = line.split_once('=') else {
            continue;
        };
        let key = key.trim().trim_matches(['\'', '"']);
        if key == "tui.theme" || (in_tui && key == "theme") {
            return true;
        }
    }
    false
}

impl Agent for CodexAgent {
    fn kind(&self) -> AgentKind {
        AgentKind::Codex
    }

    fn caps(&self) -> Caps {
        Caps {
            titles: true,
            live_status: false,
            resume: true,
        }
    }

    fn binary(&self) -> Option<PathBuf> {
        super::find_binary("codex")
    }

    fn scan_chats(&self, index: &mut ChatIndex, warnings: &mut Vec<String>) -> Vec<Chat> {
        let binary = self.binary();
        let native_titles = read_native_titles(&codex_home(), binary.as_deref(), index, warnings);
        let mut files = Vec::new();
        collect_rollouts(&sessions_root(), 0, &mut files);

        let mut chats = Vec::new();
        for path in files {
            let key = path.to_string_lossy().into_owned();
            let (mtime, size) = (mtime_ms(&path), file_size(&path));

            if let Some(cached) = index.get(&key, mtime, size) {
                let mut chat = cached.clone();
                apply_native_title(&mut chat, &native_titles);
                chats.push(chat);
                continue;
            }
            match parse_rollout(&path, mtime, size) {
                Ok(Some(mut chat)) => {
                    index.put(key, mtime, size, chat.clone());
                    apply_native_title(&mut chat, &native_titles);
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
                    status_at: 0,
                    attention: None,
                })
            })
            .collect()
    }

    fn launch_cmd(&self, resume: Option<&str>) -> String {
        let bin = super::program(self, "codex");
        let mut bin = format!("{bin} --dangerously-bypass-approvals-and-sandbox");
        if let Some(theme) = launch_theme() {
            let setting = format!("tui.theme=\"{theme}\"");
            bin.push_str(" -c ");
            bin.push_str(&crate::util::shell_quote(&setting));
        }
        match resume {
            Some(id) => format!("{bin} resume {}", crate::util::shell_quote(id)),
            None => bin,
        }
    }
}

#[derive(Deserialize)]
struct SessionIndexEntry {
    id: String,
    thread_name: String,
}

/// Codex rewrites this small append-only index independently from rollouts.
///
/// Malformed entries are counted and skipped: one damaged line must not hide
/// every healthy conversation. Later entries win if Codex ever writes the
/// same id again.
fn read_native_titles(
    home: &Path,
    codex_binary: Option<&Path>,
    index: &mut ChatIndex,
    warnings: &mut Vec<String>,
) -> HashMap<String, String> {
    let legacy_path = home.join("session_index.jsonl");
    let mut titles = read_legacy_session_titles(&legacy_path, warnings);

    let Some(database_path) = newest_state_database(home) else {
        return titles;
    };
    let source = codex_title_source(&database_path);
    let now = crate::util::now_ms();
    let recently_checked = index.codex_titles_checked_at != 0
        && now.saturating_sub(index.codex_titles_checked_at) < TITLE_REFRESH_INTERVAL_MS;
    if index.codex_title_source.as_ref() == Some(&source) || recently_checked {
        titles.extend(index.codex_titles.clone());
        return titles;
    }

    let Some(codex_binary) = codex_binary else {
        titles.extend(index.codex_titles.clone());
        return titles;
    };
    index.codex_titles_checked_at = now;
    match read_app_server_titles(codex_binary) {
        Ok(app_server_titles) => {
            index.codex_titles = app_server_titles;
            index.codex_title_source = Some(source);
        }
        Err(error) => {
            warnings.push(format!(
                "codex: cannot read current session titles through app-server: {error:#}"
            ));
        }
    }
    titles.extend(index.codex_titles.clone());
    titles
}

/// Find the newest versioned Codex state database without depending on the
/// current schema number. Files such as `state_5.sqlite-wal` are ignored.
fn newest_state_database(home: &Path) -> Option<PathBuf> {
    fs::read_dir(home)
        .ok()?
        .flatten()
        .filter_map(|entry| {
            let name = entry.file_name();
            let name = name.to_str()?;
            let version = name
                .strip_prefix("state_")?
                .strip_suffix(".sqlite")?
                .parse::<u32>()
                .ok()?;
            entry.path().is_file().then_some((version, entry.path()))
        })
        .max_by_key(|(version, _)| *version)
        .map(|(_, path)| path)
}

fn codex_title_source(database_path: &Path) -> CodexTitleSource {
    let mut wal_name = database_path.as_os_str().to_os_string();
    wal_name.push("-wal");
    let wal_path = PathBuf::from(wal_name);
    CodexTitleSource {
        database_path: database_path.to_string_lossy().into_owned(),
        database_mtime: mtime_ms(database_path),
        database_size: file_size(database_path),
        wal_mtime: mtime_ms(&wal_path),
        wal_size: file_size(&wal_path),
    }
}

/// Ask Codex for its current titles instead of parsing its private SQLite
/// schema. The child is bounded and killed after the metadata response.
fn read_app_server_titles(codex_binary: &Path) -> Result<HashMap<String, String>> {
    let mut command = Command::new(codex_binary);
    command
        .args(["app-server", "--stdio"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    if let Some(binary_dir) = codex_binary.parent() {
        command.env(
            "PATH",
            format!(
                "{}:{}",
                binary_dir.to_string_lossy(),
                crate::hostenv::effective_path()
            ),
        );
    }
    let mut child = command
        .spawn()
        .with_context(|| format!("starting {}", codex_binary.display()))?;
    let stdout = child.stdout.take().context("capturing app-server stdout")?;
    let mut stdin = child.stdin.take().context("opening app-server stdin")?;
    let (sender, receiver) = mpsc::channel();
    let reader = thread::spawn(move || {
        for line in BufReader::new(stdout).lines().take(4096) {
            let Ok(line) = line else {
                break;
            };
            if let Ok(message) = serde_json::from_str::<Value>(&line)
                && sender.send(message).is_err()
            {
                break;
            }
        }
    });

    let result = (|| {
        write_app_server_message(
            &mut stdin,
            &serde_json::json!({
                "method": "initialize",
                "id": 0,
                "params": {
                    "clientInfo": {
                        "name": "bizik",
                        "title": "bizik",
                        "version": env!("CARGO_PKG_VERSION"),
                    }
                }
            }),
        )?;
        response_result(receive_app_server_response(&receiver, 0)?)?;
        write_app_server_message(
            &mut stdin,
            &serde_json::json!({"method": "initialized", "params": {}}),
        )?;

        let mut request_id = 1_u64;
        let mut cursor: Option<String> = None;
        let mut titles = HashMap::new();
        loop {
            write_app_server_message(
                &mut stdin,
                &serde_json::json!({
                    "method": "thread/list",
                    "id": request_id,
                    "params": {
                        "cursor": cursor,
                        "limit": 1000,
                        "useStateDbOnly": true,
                    }
                }),
            )?;
            let result = response_result(receive_app_server_response(&receiver, request_id)?)?;
            let (page_titles, next_cursor) = parse_thread_page(result)?;
            titles.extend(page_titles);
            let Some(next_cursor) = next_cursor else {
                break;
            };
            cursor = Some(next_cursor);
            request_id += 1;
            if request_id > 64 {
                bail!("thread list exceeded 64 pages");
            }
        }
        Ok(titles)
    })();

    drop(stdin);
    let _ = child.kill();
    let _ = child.wait();
    let _ = reader.join();
    result
}

fn write_app_server_message(stdin: &mut impl Write, message: &Value) -> Result<()> {
    serde_json::to_writer(&mut *stdin, message)?;
    stdin.write_all(b"\n")?;
    stdin.flush()?;
    Ok(())
}

fn receive_app_server_response(receiver: &Receiver<Value>, id: u64) -> Result<Value> {
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        let message = receiver
            .recv_timeout(remaining)
            .map_err(|error| anyhow!("waiting for app-server response {id}: {error}"))?;
        if message.get("id").and_then(Value::as_u64) == Some(id) {
            return Ok(message);
        }
    }
}

fn response_result(response: Value) -> Result<Value> {
    if let Some(error) = response.get("error")
        && !error.is_null()
    {
        bail!("app-server returned {error}");
    }
    response
        .get("result")
        .cloned()
        .context("app-server response has no result")
}

fn parse_thread_page(result: Value) -> Result<(HashMap<String, String>, Option<String>)> {
    let threads = result
        .get("data")
        .and_then(Value::as_array)
        .context("thread/list result has no data array")?;
    let mut titles = HashMap::new();
    for thread in threads {
        let Some(id) = thread.get("id").and_then(Value::as_str).map(str::trim) else {
            continue;
        };
        let raw_title = thread
            .get("name")
            .and_then(Value::as_str)
            .filter(|name| !name.trim().is_empty())
            .or_else(|| thread.get("preview").and_then(Value::as_str))
            .unwrap_or("");
        let title = one_line(raw_title, 160);
        if !id.is_empty() && !title.is_empty() {
            titles.insert(id.to_string(), title);
        }
    }
    let next_cursor = result
        .get("nextCursor")
        .and_then(Value::as_str)
        .map(str::to_string);
    Ok((titles, next_cursor))
}

fn read_legacy_session_titles(path: &Path, warnings: &mut Vec<String>) -> HashMap<String, String> {
    let file = match fs::File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return HashMap::new(),
        Err(error) => {
            warnings.push(format!(
                "codex: cannot read session title index {}: {error}",
                path.display()
            ));
            return HashMap::new();
        }
    };

    let mut titles = HashMap::new();
    let mut rejected = 0_usize;
    for line in BufReader::new(file).lines() {
        let Ok(line) = line else {
            rejected += 1;
            continue;
        };
        let Ok(entry) = serde_json::from_str::<SessionIndexEntry>(&line) else {
            rejected += 1;
            continue;
        };
        let id = entry.id.trim();
        let title = one_line(&entry.thread_name, 160);
        if id.is_empty() || title.is_empty() {
            rejected += 1;
            continue;
        }
        titles.insert(id.to_string(), title);
    }
    if rejected > 0 {
        warnings.push(format!(
            "codex: skipped {rejected} invalid session title index entr{} in {}",
            if rejected == 1 { "y" } else { "ies" },
            path.display()
        ));
    }
    titles
}

fn apply_native_title(chat: &mut Chat, titles: &HashMap<String, String>) {
    chat.title = titles.get(&chat.id).cloned();
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
        // Added from Codex's title store after parsing. Keeping it out of the
        // rollout cache lets a renamed thread refresh immediately.
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
    fn reads_generated_titles_from_the_legacy_session_index() {
        let dir = std::env::temp_dir().join(format!("bzk-codex-titles-{}", std::process::id()));
        let path = write(
            &dir,
            "session_index.jsonl",
            &[
                r#"{"id":"one","thread_name":"Initial name","updated_at":"2026-07-30T12:00:00Z"}"#,
                "not json",
                r#"{"id":"two","thread_name":"  Multi\nline\tname  ","updated_at":"2026-07-30T12:01:00Z"}"#,
                r#"{"id":"one","thread_name":"Fresh name","updated_at":"2026-07-30T12:02:00Z"}"#,
                r#"{"id":"","thread_name":"No identity","updated_at":"2026-07-30T12:03:00Z"}"#,
            ],
        );
        let mut warnings = Vec::new();

        let titles = read_legacy_session_titles(&path, &mut warnings);

        assert_eq!(titles.get("one").map(String::as_str), Some("Fresh name"));
        assert_eq!(
            titles.get("two").map(String::as_str),
            Some("Multi line name")
        );
        assert_eq!(titles.len(), 2);
        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].contains("2 invalid"));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn fingerprints_the_newest_codex_database_and_its_wal() {
        let dir = std::env::temp_dir().join(format!("bzk-codex-sqlite-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let stale_path = dir.join("state_4.sqlite");
        std::fs::write(&stale_path, b"older").unwrap();
        let current_path = dir.join("state_5.sqlite");
        std::fs::write(&current_path, b"database").unwrap();
        std::fs::write(dir.join("state_5.sqlite-wal"), b"wal").unwrap();

        assert_eq!(
            newest_state_database(&dir).as_deref(),
            Some(current_path.as_path())
        );
        let source = codex_title_source(&current_path);
        assert_eq!(source.database_size, 8);
        assert_eq!(source.wal_size, 3);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn app_server_pages_prefer_names_and_fall_back_to_previews() {
        let result = serde_json::json!({
            "data": [
                {"id": "generated", "name": null, "preview": "  Generated\n  title "},
                {"id": "named", "name": "My Codex name", "preview": "Generated fallback"},
                {"id": "", "name": null, "preview": "No identity"},
                {"id": "empty", "name": null, "preview": ""},
            ],
            "nextCursor": "next-page",
        });

        let (titles, cursor) = parse_thread_page(result).unwrap();

        assert_eq!(
            titles.get("generated").map(String::as_str),
            Some("Generated title")
        );
        assert_eq!(
            titles.get("named").map(String::as_str),
            Some("My Codex name")
        );
        assert_eq!(titles.len(), 2);
        assert_eq!(cursor.as_deref(), Some("next-page"));
    }

    #[test]
    fn a_native_title_is_applied_without_losing_prompt_fallback_data() {
        let mut chat = Chat {
            agent: AgentKind::Codex,
            id: "thread".into(),
            cwd: "/repo".into(),
            title: None,
            last_prompt: Some("the user's latest request".into()),
            git_branch: None,
            last_active: 1,
            size: 2,
        };
        let titles = HashMap::from([("thread".into(), "Generated title".into())]);

        apply_native_title(&mut chat, &titles);

        assert_eq!(chat.title.as_deref(), Some("Generated title"));
        assert_eq!(
            chat.last_prompt.as_deref(),
            Some("the user's latest request")
        );
    }

    #[test]
    fn rollout_without_meta_is_rejected() {
        let dir = std::env::temp_dir().join(format!("bzk-codex-bad-{}", std::process::id()));
        let p = write(&dir, "rollout-x.jsonl", &[r#"{"type":"event_msg"}"#]);
        assert!(
            parse_rollout(&p, 1, file_size(&p)).unwrap().is_none(),
            "skipped, not reported as damaged"
        );
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

    #[test]
    fn launch_always_bypasses_approvals_and_sandbox() {
        let agent = CodexAgent;
        let fresh = agent.launch_cmd(None);
        let resumed = agent.launch_cmd(Some("thread id"));

        assert!(fresh.contains(" --dangerously-bypass-approvals-and-sandbox"));
        assert!(resumed.contains(" resume 'thread id'"));
    }

    #[test]
    fn detached_launch_defaults_to_a_light_diff_capable_theme() {
        assert_eq!(choose_theme(false, None).as_deref(), Some("github"));
    }

    #[test]
    fn an_explicit_codex_theme_is_not_overridden() {
        assert_eq!(choose_theme(true, None), None);
        assert_eq!(choose_theme(false, Some("inherit")), None);
        assert_eq!(
            choose_theme(true, Some("solarized-light")).as_deref(),
            Some("solarized-light")
        );
    }

    #[test]
    fn only_safe_kebab_case_theme_names_reach_the_shell() {
        assert!(valid_theme_name("base16-ocean-light"));
        assert!(!valid_theme_name("theme'; touch /tmp/nope"));
        assert_eq!(
            choose_theme(false, Some("theme'; touch /tmp/nope")).as_deref(),
            Some(DEFAULT_THEME)
        );
    }

    #[test]
    fn codex_theme_is_found_in_both_supported_config_forms() {
        assert!(config_text_has_theme(
            "[tui]\nanimations = true\ntheme = \"catppuccin-latte\"\n"
        ));
        assert!(config_text_has_theme(
            "model = \"gpt-5\"\ntui.theme = \"catppuccin-latte\"\n"
        ));
        assert!(!config_text_has_theme(
            "[tui]\nanimations = true\n\n[projects.\"/tmp/theme\"]\ntrust_level = \"trusted\"\n"
        ));
    }
}
