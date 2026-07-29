//! Installing the Claude Code and Codex hooks that make live state knowable.
//!
//! This edits files the user owns, so it is never done implicitly. It is an
//! explicit `bzk hooks install`, it merges rather than replaces, it keeps a
//! backup of each original, and `bzk hooks uninstall` removes exactly what was
//! added and nothing else.

use anyhow::{Context, Result, bail};
use serde_json::{Map, Value, json};
use std::path::{Path, PathBuf};

use crate::util::{atomic_write, home};

/// Marks a hook entry as ours. Anything else in the file is left alone.
///
/// A leading environment assignment rather than the binary's name: the name is
/// whatever the file was installed as, and matching on it would leave a build
/// named anything else unable to find — or remove — its own entries.
const MARKER: &str = "BZK_HOOK=1";
const INLINE_BEGIN: &str = "# >>> bizik lifecycle hooks >>>";
const INLINE_END: &str = "# <<< bizik lifecycle hooks <<<";

/// Claude Code event name paired with the argument `bzk hook` receives.
///
/// `PermissionRequest` is deliberately not used: it fires before a prompt is
/// shown, including when permission is granted automatically, which would
/// report a session as blocked when nothing ever blocked. `Notification` fires
/// only when the agent actually wants the user.
const CLAUDE_EVENTS: [(&str, &str); 4] = [
    ("Notification", "notification"),
    ("Stop", "stop"),
    ("UserPromptSubmit", "prompt"),
    ("SessionEnd", "end"),
];

/// Codex lifecycle events. `PostToolUse` resets a permission marker after an
/// automatically approved tool call, so a transient request cannot leave the
/// sidebar claiming that the agent is blocked.
const CODEX_EVENTS: [(&str, &str); 6] = [
    ("SessionStart", "start"),
    ("UserPromptSubmit", "prompt"),
    ("PermissionRequest", "permission"),
    ("PostToolUse", "prompt"),
    ("Stop", "stop"),
    ("SessionEnd", "end"),
];

pub fn claude_settings_path() -> PathBuf {
    home().join(".claude/settings.json")
}

pub fn codex_hooks_path() -> PathBuf {
    home().join(".codex/hooks.json")
}

pub fn codex_config_path() -> PathBuf {
    home().join(".codex/config.toml")
}

/// Codex warns when one config layer contains both inline hooks and a
/// `hooks.json`. Follow the representation already in use on this machine.
pub fn codex_hook_source_path() -> PathBuf {
    let config = codex_config_path();
    if codex_has_inline_hooks(&config) {
        config
    } else {
        codex_hooks_path()
    }
}

fn backup_for(path: &Path) -> PathBuf {
    path.with_extension("json.bzk-backup")
}

/// Every operation takes the settings path explicitly. That keeps the module
/// testable without reaching for `$HOME`, which cannot be swapped safely while
/// other threads are running.
fn read_settings(path: &Path) -> Result<Map<String, Value>> {
    if !path.exists() {
        return Ok(Map::new());
    }
    let raw =
        std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    if raw.trim().is_empty() {
        return Ok(Map::new());
    }
    match serde_json::from_str::<Value>(&raw) {
        Ok(Value::Object(map)) => Ok(map),
        Ok(_) => bail!("{} is not a JSON object", path.display()),
        // Refusing here matters: a malformed settings file silently disables
        // every setting in it, and overwriting one would destroy whatever the
        // user was in the middle of editing.
        Err(e) => bail!("{} is not valid JSON ({e}) — fix it first", path.display()),
    }
}

fn write_settings(path: &Path, map: Map<String, Value>) -> Result<()> {
    let backup = backup_for(path);
    if path.exists() && !backup.exists() {
        let _ = std::fs::copy(path, &backup);
    }
    let bytes = serde_json::to_vec_pretty(&Value::Object(map))?;
    atomic_write(path, &bytes)
}

fn is_ours(group: &Value) -> bool {
    group
        .get("hooks")
        .and_then(Value::as_array)
        .is_some_and(|hooks| {
            !hooks.is_empty()
                && hooks.iter().all(|h| {
                    h.get("command")
                        .and_then(Value::as_str)
                        .is_some_and(|c| c.contains(MARKER))
                })
        })
}

fn our_group(exe: &str, arg: &str, async_supported: bool, timeout: u64) -> Value {
    let mut hook = json!({
        "type": "command",
        // An absolute path, because a hook command runs through a plain
        // shell whose PATH need not contain ~/.local/bin.
        "command": format!("{MARKER} {exe} hook {arg}"),
        "timeout": timeout
    });
    if async_supported {
        hook["async"] = Value::Bool(true);
    }
    json!({
        "matcher": "",
        "hooks": [hook]
    })
}

pub struct Report {
    pub added: Vec<String>,
    pub removed: Vec<String>,
}

/// Add (or refresh) bizik's hooks for both agents.
pub fn install() -> Result<Report> {
    let exe = crate::util::own_exe()?.to_string_lossy().into_owned();
    let mut claude = install_agent_at(&claude_settings_path(), &exe, &CLAUDE_EVENTS, true)?;
    let codex = if codex_has_inline_hooks(&codex_config_path()) {
        migrate_our_codex_json()?;
        install_codex_inline_at(&codex_config_path(), &exe)?
    } else {
        install_agent_at(&codex_hooks_path(), &exe, &CODEX_EVENTS, false)?
    };
    claude
        .added
        .iter_mut()
        .for_each(|event| *event = format!("Claude:{event}"));
    claude.added.extend(
        codex
            .added
            .into_iter()
            .map(|event| format!("Codex:{event}")),
    );
    Ok(claude)
}

/// Claude-only path kept explicit for isolated tests.
#[cfg(test)]
pub fn install_at(path: &Path) -> Result<Report> {
    let exe = crate::util::own_exe()?.to_string_lossy().into_owned();
    install_agent_at(path, &exe, &CLAUDE_EVENTS, true)
}

fn install_agent_at(
    path: &Path,
    exe: &str,
    events: &[(&str, &str)],
    async_supported: bool,
) -> Result<Report> {
    let mut settings = read_settings(path)?;
    let mut hooks = match settings.remove("hooks") {
        Some(Value::Object(m)) => m,
        Some(other) => bail!("`hooks` in settings.json is not an object: {other}"),
        None => Map::new(),
    };

    let mut added = Vec::new();
    for (event, arg) in events {
        let mut groups = match hooks.remove(*event) {
            Some(Value::Array(a)) => a,
            Some(other) => bail!("`hooks.{event}` is not an array: {other}"),
            None => Vec::new(),
        };
        // Drop a previous version of our entry so reinstalling refreshes the
        // path rather than stacking duplicates.
        groups.retain(|g| !is_ours(g));
        let timeout = if *event == "SessionEnd" { 3 } else { 5 };
        groups.push(our_group(exe, arg, async_supported, timeout));
        hooks.insert(event.to_string(), Value::Array(groups));
        added.push(event.to_string());
    }

    settings.insert("hooks".into(), Value::Object(hooks));
    write_settings(path, settings)?;
    Ok(Report {
        added,
        removed: Vec::new(),
    })
}

/// Remove only the entries bizik added.
pub fn uninstall() -> Result<Report> {
    let mut claude = uninstall_agent_at(&claude_settings_path(), &CLAUDE_EVENTS)?;
    let mut codex = uninstall_agent_at(&codex_hooks_path(), &CODEX_EVENTS)?;
    codex
        .removed
        .extend(uninstall_codex_inline_at(&codex_config_path())?.removed);
    codex.removed.sort();
    codex.removed.dedup();
    claude
        .removed
        .iter_mut()
        .for_each(|event| *event = format!("Claude:{event}"));
    claude.removed.extend(
        codex
            .removed
            .into_iter()
            .map(|event| format!("Codex:{event}")),
    );
    Ok(claude)
}

#[cfg(test)]
pub fn uninstall_at(path: &Path) -> Result<Report> {
    uninstall_agent_at(path, &CLAUDE_EVENTS)
}

fn uninstall_agent_at(path: &Path, events: &[(&str, &str)]) -> Result<Report> {
    let mut settings = read_settings(path)?;
    let Some(Value::Object(mut hooks)) = settings.remove("hooks") else {
        return Ok(Report {
            added: Vec::new(),
            removed: Vec::new(),
        });
    };

    let mut removed = Vec::new();
    for (event, _) in events {
        let Some(Value::Array(mut groups)) = hooks.remove(*event) else {
            continue;
        };
        let before = groups.len();
        groups.retain(|g| !is_ours(g));
        if groups.len() != before {
            removed.push(event.to_string());
        }
        // An event left with no groups is dropped entirely, so uninstalling
        // returns the file to how it looked before.
        if !groups.is_empty() {
            hooks.insert(event.to_string(), Value::Array(groups));
        }
    }

    if !hooks.is_empty() {
        settings.insert("hooks".into(), Value::Object(hooks));
    }
    write_settings(path, settings)?;
    Ok(Report {
        added: Vec::new(),
        removed,
    })
}

/// Which of our hooks are currently present.
pub fn installed_events() -> Vec<String> {
    let mut events: Vec<String> =
        installed_agent_events_at(&claude_settings_path(), &CLAUDE_EVENTS)
            .into_iter()
            .map(|event| format!("Claude:{event}"))
            .collect();
    let mut codex = installed_agent_events_at(&codex_hooks_path(), &CODEX_EVENTS);
    codex.extend(installed_codex_inline_events_at(&codex_config_path()));
    codex.sort();
    codex.dedup();
    events.extend(codex.into_iter().map(|event| format!("Codex:{event}")));
    events
}

#[cfg(test)]
pub fn installed_events_at(path: &Path) -> Vec<String> {
    installed_agent_events_at(path, &CLAUDE_EVENTS)
}

fn installed_agent_events_at(path: &Path, events: &[(&str, &str)]) -> Vec<String> {
    let Ok(settings) = read_settings(path) else {
        return Vec::new();
    };
    let Some(Value::Object(hooks)) = settings.get("hooks") else {
        return Vec::new();
    };
    events
        .iter()
        .filter(|(event, _)| {
            hooks
                .get(*event)
                .and_then(Value::as_array)
                .is_some_and(|groups| groups.iter().any(is_ours))
        })
        .map(|(event, _)| event.to_string())
        .collect()
}

pub fn all_installed() -> bool {
    installed_agent_events_at(&claude_settings_path(), &CLAUDE_EVENTS).len() == CLAUDE_EVENTS.len()
        && {
            let mut codex = installed_agent_events_at(&codex_hooks_path(), &CODEX_EVENTS);
            codex.extend(installed_codex_inline_events_at(&codex_config_path()));
            codex.sort();
            codex.dedup();
            codex.len() == CODEX_EVENTS.len()
        }
}

#[cfg(test)]
pub fn all_installed_at(path: &Path) -> bool {
    installed_events_at(path).len() == CLAUDE_EVENTS.len()
}

fn codex_has_inline_hooks(path: &Path) -> bool {
    std::fs::read_to_string(path)
        .is_ok_and(|raw| raw.contains("[[hooks.") || raw.contains(INLINE_BEGIN))
}

fn codex_inline_block(exe: &str) -> Result<String> {
    let mut out = String::from(INLINE_BEGIN);
    out.push('\n');
    for (event, arg) in CODEX_EVENTS {
        out.push_str(&codex_inline_event_block(exe, event, arg)?);
    }
    out.push_str(INLINE_END);
    out.push('\n');
    Ok(out)
}

fn codex_inline_event_block(exe: &str, event: &str, arg: &str) -> Result<String> {
    let command = serde_json::to_string(&format!("{MARKER} {exe} hook {arg}"))?;
    let timeout = if event == "SessionEnd" { 3 } else { 5 };
    Ok(format!(
        "[[hooks.{event}]]\nmatcher = \"\"\n\n[[hooks.{event}.hooks]]\ntype = \"command\"\ncommand = {command}\ntimeout = {timeout}\n\n"
    ))
}

fn codex_inline_is_current(raw: &str, exe: &str) -> Result<bool> {
    if raw.matches(INLINE_BEGIN).count() != 1 || raw.matches(INLINE_END).count() != 1 {
        return Ok(false);
    }
    for (event, arg) in CODEX_EVENTS {
        if !raw.contains(&codex_inline_event_block(exe, event, arg)?) {
            return Ok(false);
        }
    }
    Ok(true)
}

fn without_inline_block(raw: &str) -> String {
    let Some(start) = raw.find(INLINE_BEGIN) else {
        return raw.to_string();
    };
    let Some(relative_end) = raw[start..].find(INLINE_END) else {
        return raw.to_string();
    };
    let end = start + relative_end + INLINE_END.len();
    let end = end + usize::from(raw.as_bytes().get(end) == Some(&b'\n'));
    let managed = &raw[start + INLINE_BEGIN.len()..start + relative_end];
    // Codex persists trust hashes under `[hooks.state]` immediately before the
    // trailing marker comment. Those hashes include hooks bizik does not own,
    // so keep the state tables when refreshing or uninstalling our definitions.
    let trust_state = managed.find("[hooks.state]").map(|at| &managed[at..]);
    let mut clean = String::with_capacity(raw.len().saturating_sub(end - start));
    clean.push_str(raw[..start].trim_end());
    clean.push('\n');
    if let Some(trust_state) = trust_state {
        clean.push('\n');
        clean.push_str(trust_state.trim());
        clean.push('\n');
    }
    clean.push_str(raw[end..].trim_start_matches('\n'));
    clean
}

fn install_codex_inline_at(path: &Path, exe: &str) -> Result<Report> {
    let raw = if path.exists() {
        std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?
    } else {
        String::new()
    };
    let desired = codex_inline_block(exe)?;
    let report = || Report {
        added: CODEX_EVENTS
            .iter()
            .map(|(event, _)| event.to_string())
            .collect(),
        removed: Vec::new(),
    };
    // Codex stores trust metadata next to inline hooks. Even an equivalent
    // rewrite can make it ask the user to review every hook again, so a
    // reinstall with unchanged commands must be a byte-for-byte no-op.
    if codex_inline_is_current(&raw, exe)? {
        return Ok(report());
    }
    let mut updated = without_inline_block(&raw);
    if !updated.is_empty() && !updated.ends_with('\n') {
        updated.push('\n');
    }
    updated.push('\n');
    updated.push_str(&desired);
    if path.exists() && !backup_for(path).exists() {
        let _ = std::fs::copy(path, backup_for(path));
    }
    atomic_write(path, updated.as_bytes())?;
    Ok(report())
}

fn uninstall_codex_inline_at(path: &Path) -> Result<Report> {
    if !path.exists() {
        return Ok(Report {
            added: Vec::new(),
            removed: Vec::new(),
        });
    }
    let raw =
        std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    if !raw.contains(INLINE_BEGIN) {
        return Ok(Report {
            added: Vec::new(),
            removed: Vec::new(),
        });
    }
    atomic_write(path, without_inline_block(&raw).as_bytes())?;
    Ok(Report {
        added: Vec::new(),
        removed: CODEX_EVENTS
            .iter()
            .map(|(event, _)| event.to_string())
            .collect(),
    })
}

fn installed_codex_inline_events_at(path: &Path) -> Vec<String> {
    if std::fs::read_to_string(path).is_ok_and(|raw| raw.contains(INLINE_BEGIN)) {
        CODEX_EVENTS
            .iter()
            .map(|(event, _)| event.to_string())
            .collect()
    } else {
        Vec::new()
    }
}

/// Remove the JSON written by an older bizik build only when it contains
/// nothing except bizik's own hook groups. User-owned JSON stays untouched.
fn migrate_our_codex_json() -> Result<()> {
    let path = codex_hooks_path();
    if !path.exists() {
        return Ok(());
    }
    let settings = read_settings(&path)?;
    let only_ours = settings.len() == 1
        && settings
            .get("hooks")
            .and_then(Value::as_object)
            .is_some_and(|hooks| {
                !hooks.is_empty()
                    && hooks.values().all(|groups| {
                        groups
                            .as_array()
                            .is_some_and(|groups| !groups.is_empty() && groups.iter().all(is_ours))
                    })
            });
    if only_ours {
        std::fs::remove_file(&path)
            .with_context(|| format!("removing migrated {}", path.display()))?;
    } else {
        // A mixed file must be preserved; remove only our groups. Codex will
        // retain its pre-existing two-source warning, but bizik does not add a
        // second copy of its own hooks.
        uninstall_agent_at(&path, &CODEX_EVENTS)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A settings file in its own directory, so these tests can run in
    /// parallel — swapping `$HOME` under a running process cannot.
    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("bzk-hooks-{}-{}", name, std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir.join("settings.json")
    }

    fn write(path: &Path, value: Value) {
        std::fs::write(path, serde_json::to_vec_pretty(&value).unwrap()).unwrap();
    }

    #[test]
    fn install_preserves_unrelated_settings_and_hooks() {
        let path = scratch("preserve");
        write(
            &path,
            json!({
                "model": "opus[1m]",
                "theme": "dark",
                "hooks": {
                    "PostToolUse": [{
                        "matcher": "Write|Edit",
                        "hooks": [{"type": "command", "command": "prettier --write $FILE"}]
                    }],
                    "Stop": [{
                        "matcher": "",
                        "hooks": [{"type": "command", "command": "echo mine"}]
                    }]
                }
            }),
        );

        install_at(&path).unwrap();
        let after = read_settings(&path).unwrap();

        assert_eq!(after.get("model").unwrap(), "opus[1m]");
        assert_eq!(after.get("theme").unwrap(), "dark");

        let hooks = after.get("hooks").unwrap();
        assert!(
            hooks.get("PostToolUse").is_some(),
            "somebody else's hook must survive"
        );
        let stop = hooks.get("Stop").unwrap().as_array().unwrap();
        assert_eq!(
            stop.len(),
            2,
            "the user's own Stop hook is kept alongside ours"
        );
        assert!(stop.iter().any(|g| !is_ours(g)));
        assert!(stop.iter().any(is_ours));

        std::fs::remove_dir_all(path.parent().unwrap()).ok();
    }

    #[test]
    fn reinstalling_refreshes_rather_than_duplicates() {
        let path = scratch("refresh");
        install_at(&path).unwrap();
        install_at(&path).unwrap();
        let after = read_settings(&path).unwrap();
        let stop = after
            .get("hooks")
            .unwrap()
            .get("Stop")
            .unwrap()
            .as_array()
            .unwrap();
        assert_eq!(stop.len(), 1);
        std::fs::remove_dir_all(path.parent().unwrap()).ok();
    }

    #[test]
    fn uninstall_removes_only_ours_and_restores_the_shape() {
        let path = scratch("uninstall");
        write(
            &path,
            json!({
                "hooks": {
                    "Stop": [{"matcher": "", "hooks": [{"type": "command", "command": "echo mine"}]}]
                }
            }),
        );

        install_at(&path).unwrap();
        uninstall_at(&path).unwrap();

        let after = read_settings(&path).unwrap();
        let hooks = after.get("hooks").unwrap();
        let stop = hooks.get("Stop").unwrap().as_array().unwrap();
        assert_eq!(stop.len(), 1);
        assert_eq!(stop[0]["hooks"][0]["command"], "echo mine");
        assert!(
            hooks.get("Notification").is_none(),
            "an event we emptied is removed, not left as []"
        );
        std::fs::remove_dir_all(path.parent().unwrap()).ok();
    }

    #[test]
    fn ownership_does_not_depend_on_what_the_binary_is_called() {
        // The installed file can be named anything; matching on its name would
        // leave a differently-named build unable to remove its own entries.
        let ours = json!({
            "hooks": [{"type": "command", "command": format!("{MARKER} /opt/weird-name-9f hook stop")}]
        });
        let theirs = json!({"hooks": [{"type": "command", "command": "/usr/bin/bzk hook stop"}]});
        assert!(is_ours(&ours));
        assert!(!is_ours(&theirs), "a lookalike command is not ours");
    }

    #[test]
    fn a_malformed_settings_file_is_refused_not_overwritten() {
        let path = scratch("malformed");
        std::fs::write(&path, "{ this is not json").unwrap();
        assert!(install_at(&path).is_err());
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "{ this is not json",
            "the user's file is exactly as they left it"
        );
        std::fs::remove_dir_all(path.parent().unwrap()).ok();
    }

    #[test]
    fn install_keeps_a_backup_of_the_original() {
        let path = scratch("backup");
        std::fs::write(&path, r#"{"theme":"dark"}"#).unwrap();
        install_at(&path).unwrap();
        assert_eq!(
            std::fs::read_to_string(backup_for(&path)).unwrap(),
            r#"{"theme":"dark"}"#
        );
        std::fs::remove_dir_all(path.parent().unwrap()).ok();
    }

    #[test]
    fn status_reports_what_is_installed() {
        let path = scratch("status");
        assert!(installed_events_at(&path).is_empty());
        install_at(&path).unwrap();
        assert!(all_installed_at(&path));
        uninstall_at(&path).unwrap();
        assert!(installed_events_at(&path).is_empty());
        std::fs::remove_dir_all(path.parent().unwrap()).ok();
    }

    #[test]
    fn codex_gets_every_lifecycle_hook_without_unsupported_async() {
        let path = scratch("codex");
        let report = install_agent_at(&path, "/opt/bzk", &CODEX_EVENTS, false).unwrap();
        assert_eq!(report.added.len(), CODEX_EVENTS.len());

        let after = read_settings(&path).unwrap();
        let hooks = after.get("hooks").unwrap();
        for (event, arg) in CODEX_EVENTS {
            let groups = hooks.get(event).unwrap().as_array().unwrap();
            let ours = groups.iter().find(|group| is_ours(group)).unwrap();
            let handler = &ours["hooks"][0];
            assert_eq!(handler["command"], format!("{MARKER} /opt/bzk hook {arg}"));
            assert!(
                handler.get("async").is_none(),
                "Codex parses async but does not support it"
            );
        }

        uninstall_agent_at(&path, &CODEX_EVENTS).unwrap();
        assert!(
            installed_agent_events_at(&path, &CODEX_EVENTS).is_empty(),
            "uninstall removes every Codex event"
        );
        std::fs::remove_dir_all(path.parent().unwrap()).ok();
    }

    #[test]
    fn codex_inline_install_preserves_existing_hooks_and_is_reversible() {
        let path = scratch("codex-inline").with_file_name("config.toml");
        let original = concat!(
            "model = \"gpt-5\"\n\n",
            "[[hooks.SessionStart]]\n",
            "hooks = [{ type = \"command\", command = \"node existing.js\" }]\n",
        );
        std::fs::write(&path, original).unwrap();

        install_codex_inline_at(&path, "/opt/bzk").unwrap();
        install_codex_inline_at(&path, "/opt/bzk").unwrap();
        let installed = std::fs::read_to_string(&path).unwrap();
        assert_eq!(installed.matches(INLINE_BEGIN).count(), 1);
        assert!(installed.contains("node existing.js"));
        assert!(installed.contains("BZK_HOOK=1 /opt/bzk hook prompt"));
        assert!(
            installed.contains(
                "[[hooks.SessionEnd.hooks]]\ntype = \"command\"\ncommand = \"BZK_HOOK=1 /opt/bzk hook end\"\ntimeout = 3"
            ),
            "SessionEnd uses Codex's supported timeout"
        );
        assert_eq!(
            installed_codex_inline_events_at(&path).len(),
            CODEX_EVENTS.len()
        );

        let with_trust_state = installed.replace(
            INLINE_END,
            concat!(
                "[hooks.state]\n\n",
                "[hooks.state.\"trusted hook\"]\n",
                "trusted_hash = \"sha256:test\"\n\n",
                "# <<< bizik lifecycle hooks <<<"
            ),
        );
        std::fs::write(&path, &with_trust_state).unwrap();
        install_codex_inline_at(&path, "/opt/bzk").unwrap();
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            with_trust_state,
            "an unchanged reinstall must preserve Codex trust metadata byte for byte"
        );

        uninstall_codex_inline_at(&path).unwrap();
        let restored = std::fs::read_to_string(&path).unwrap();
        assert!(restored.contains("node existing.js"));
        assert!(restored.contains("trusted_hash = \"sha256:test\""));
        assert!(!restored.contains(MARKER));
        assert!(!restored.contains(INLINE_BEGIN));
        std::fs::remove_dir_all(path.parent().unwrap()).ok();
    }
}
