//! Installing the agent hooks that make "waiting on you" knowable.
//!
//! This edits a file the user owns — `~/.claude/settings.json` — so it is never
//! done implicitly. It is an explicit `bzk hooks install`, it merges rather than
//! replaces, it keeps a backup of the original, and `bzk hooks uninstall`
//! removes exactly what was added and nothing else.

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

/// Claude Code event name paired with the argument `bzk hook` receives.
///
/// `PermissionRequest` is deliberately not used: it fires before a prompt is
/// shown, including when permission is granted automatically, which would
/// report a session as blocked when nothing ever blocked. `Notification` fires
/// only when the agent actually wants the user.
const EVENTS: [(&str, &str); 4] = [
    ("Notification", "notification"),
    ("Stop", "stop"),
    ("UserPromptSubmit", "prompt"),
    ("SessionEnd", "end"),
];

pub fn settings_path() -> PathBuf {
    home().join(".claude/settings.json")
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

fn our_group(exe: &str, arg: &str) -> Value {
    json!({
        "matcher": "",
        "hooks": [{
            "type": "command",
            // An absolute path, because a hook command runs through a plain
            // shell whose PATH need not contain ~/.local/bin.
            "command": format!("{MARKER} {exe} hook {arg}"),
            // Fire and forget: this writes one small file and must never sit in
            // front of the agent doing real work.
            "async": true,
            "timeout": 5
        }]
    })
}

pub struct Report {
    pub added: Vec<String>,
    pub removed: Vec<String>,
}

/// Add (or refresh) bizik's hooks, leaving every other hook untouched.
pub fn install() -> Result<Report> {
    install_at(&settings_path())
}

pub fn install_at(path: &Path) -> Result<Report> {
    let exe = std::env::current_exe()
        .context("locating own binary")?
        .to_string_lossy()
        .into_owned();

    let mut settings = read_settings(path)?;
    let mut hooks = match settings.remove("hooks") {
        Some(Value::Object(m)) => m,
        Some(other) => bail!("`hooks` in settings.json is not an object: {other}"),
        None => Map::new(),
    };

    let mut added = Vec::new();
    for (event, arg) in EVENTS {
        let mut groups = match hooks.remove(event) {
            Some(Value::Array(a)) => a,
            Some(other) => bail!("`hooks.{event}` is not an array: {other}"),
            None => Vec::new(),
        };
        // Drop a previous version of our entry so reinstalling refreshes the
        // path rather than stacking duplicates.
        groups.retain(|g| !is_ours(g));
        groups.push(our_group(&exe, arg));
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
    uninstall_at(&settings_path())
}

pub fn uninstall_at(path: &Path) -> Result<Report> {
    let mut settings = read_settings(path)?;
    let Some(Value::Object(mut hooks)) = settings.remove("hooks") else {
        return Ok(Report {
            added: Vec::new(),
            removed: Vec::new(),
        });
    };

    let mut removed = Vec::new();
    for (event, _) in EVENTS {
        let Some(Value::Array(mut groups)) = hooks.remove(event) else {
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
    installed_events_at(&settings_path())
}

pub fn installed_events_at(path: &Path) -> Vec<String> {
    let Ok(settings) = read_settings(path) else {
        return Vec::new();
    };
    let Some(Value::Object(hooks)) = settings.get("hooks") else {
        return Vec::new();
    };
    EVENTS
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
    all_installed_at(&settings_path())
}

pub fn all_installed_at(path: &Path) -> bool {
    installed_events_at(path).len() == EVENTS.len()
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
}
