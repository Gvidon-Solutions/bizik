//! The host's real PATH, captured rather than guessed.
//!
//! Agents get launched from a non-interactive context, and the environment
//! there is not the one the user has when they type. Node tools live under nvm,
//! Python under pyenv, Rust under rustup, and each of those is put on `PATH` by
//! an *interactive* shell's rc file. Codex demonstrated the whole class: its
//! absolute path was found, but its `#!/usr/bin/env node` shebang could not find
//! node, and the session died with status 127 the instant it started.
//!
//! Pointing at each tool's own directory fixes that tool. Capturing the shell's
//! actual `PATH` fixes the class — bun, pyenv, rbenv, conda, volta, and whatever
//! the user installs next.
//!
//! It is a deliberate, explicit step rather than something done on every
//! launch: sourcing an interactive rc runs the user's code, which is not a thing
//! to do behind their back several times a minute. Captured once, shown in
//! `doctor`, re-captured when they say so.

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::process::Command;

use crate::util::{atomic_write, config_dir, now_ms};

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct HostEnv {
    pub path: String,
    pub shell: String,
    pub captured_at: u64,
}

fn store_path() -> PathBuf {
    config_dir().join("env.json")
}

pub fn load() -> Option<HostEnv> {
    let raw = std::fs::read_to_string(store_path()).ok()?;
    serde_json::from_str(raw.trim()).ok()
}

/// Ask the user's login shell, interactively, what `PATH` it ends up with.
///
/// `-i` is the point: without it nvm and friends are never sourced and the
/// answer is the same impoverished PATH we already had. A timeout guards
/// against an rc file that waits for input — with `-i` that is a real
/// possibility, and hanging a probe forever would be worse than failing.
pub fn capture() -> Result<HostEnv> {
    let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/bash".to_string());

    let out = Command::new("timeout")
        .args(["10", &shell, "-ic", "printf %s \"$PATH\""])
        .output()
        .with_context(|| format!("running {shell} -ic"))?;

    // Interactive shells print banners and warnings to stderr; only stdout is
    // the answer, and `printf` keeps it to exactly one line.
    let path = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if path.is_empty() {
        bail!(
            "{shell} -ic produced no PATH{}",
            match String::from_utf8_lossy(&out.stderr).trim() {
                "" => String::new(),
                e => format!(" ({e})"),
            }
        );
    }

    let env = HostEnv {
        path,
        shell,
        captured_at: now_ms(),
    };
    atomic_write(&store_path(), &serde_json::to_vec_pretty(&env)?)?;
    Ok(env)
}

/// The captured PATH, or the current one when nothing has been captured.
///
/// Falling back rather than failing matters: a host that has never run
/// `bzk env capture` must still be able to start agents, just with the older,
/// guessier behaviour.
pub fn effective_path() -> String {
    load()
        .map(|e| e.path)
        .unwrap_or_else(|| std::env::var("PATH").unwrap_or_default())
}

/// A `PATH=…` assignment to prefix a launch command with, with `extra` first.
///
/// Returned as a prefix rather than applied to a `Command` because the launch
/// travels as a shell string through tmux.
pub fn path_prefix(extra: Option<&str>) -> String {
    let base = effective_path();
    let joined = match extra {
        Some(dir) if !dir.is_empty() && !base.split(':').any(|p| p == dir) => {
            format!("{dir}:{base}")
        }
        _ => base,
    };
    format!("PATH={}", crate::util::shell_quote(&joined))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_tools_own_directory_goes_first() {
        // SAFETY: no other thread reads PATH during this test.
        unsafe { std::env::set_var("BIZIK_CONFIG_DIR", "/nonexistent-for-test") };
        unsafe { std::env::set_var("PATH", "/usr/bin:/bin") };

        let prefix = path_prefix(Some("/home/me/.nvm/versions/node/v25/bin"));
        assert!(prefix.starts_with("PATH="));
        assert!(
            prefix.contains("'/home/me/.nvm/versions/node/v25/bin:/usr/bin:/bin'"),
            "got: {prefix}"
        );
    }

    #[test]
    fn a_directory_already_present_is_not_added_twice() {
        unsafe { std::env::set_var("BIZIK_CONFIG_DIR", "/nonexistent-for-test") };
        unsafe { std::env::set_var("PATH", "/usr/bin:/bin") };

        let prefix = path_prefix(Some("/usr/bin"));
        assert_eq!(prefix, "PATH='/usr/bin:/bin'");
    }

    #[test]
    fn a_path_with_a_quote_in_it_cannot_break_out_of_the_assignment() {
        unsafe { std::env::set_var("BIZIK_CONFIG_DIR", "/nonexistent-for-test") };
        unsafe { std::env::set_var("PATH", "/od'd/bin") };
        assert_eq!(path_prefix(None), r"PATH='/od'\''d/bin'");
    }
}
