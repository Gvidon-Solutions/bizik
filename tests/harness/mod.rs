//! A disposable bizik: its own tmux socket, its own config, its own home.
//!
//! Every one of these tests creates real tmux sessions and real files. That is
//! the point — the defects this suite exists to catch were all in the seam
//! between bizik and tmux, and none of them were reachable from a pure unit
//! test.
//!
//! It is also why isolation is not optional. Earlier, testing by hand against
//! the developer's own tmux server destroyed sessions that were being worked
//! in. `BIZIK_TMUX_SOCKET` puts every command on a private socket, and the
//! config and cache directories are temporary, so a test physically cannot
//! reach anything real.

#![allow(dead_code)]

use std::path::{Path, PathBuf};
use std::process::Command;

pub struct Sandbox {
    pub root: PathBuf,
    pub socket: String,
}

impl Sandbox {
    /// A fresh sandbox, named after the test using it so failures are
    /// attributable and parallel tests cannot collide.
    pub fn new(name: &str) -> Self {
        let unique = format!("bzk-it-{name}-{}", std::process::id());
        let root = std::env::temp_dir().join(&unique);
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("config")).expect("creating the sandbox");
        std::fs::create_dir_all(root.join("cache")).expect("creating the sandbox");

        let sandbox = Self {
            root,
            socket: unique,
        };
        sandbox.kill_server();
        sandbox
    }

    fn bin() -> &'static str {
        env!("CARGO_BIN_EXE_bzk")
    }

    /// Run bizik inside the sandbox.
    pub fn bzk(&self, args: &[&str]) -> Output {
        let out = Command::new(Self::bin())
            .args(args)
            .env("BIZIK_TMUX_SOCKET", &self.socket)
            .env("BIZIK_CONFIG_DIR", self.root.join("config"))
            .env("BIZIK_CACHE_DIR", self.root.join("cache"))
            // Nothing here should reach the user's real agent state.
            .env("HOME", &self.root)
            .env("BIZIK_PANE_STATUS", "off")
            .current_dir(&self.root)
            .output()
            .expect("running bzk");
        Output {
            code: out.status.code().unwrap_or(-1),
            stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
        }
    }

    /// Run bizik with a working directory, for the commands that use it.
    pub fn bzk_in(&self, dir: &Path, args: &[&str]) -> Output {
        let out = Command::new(Self::bin())
            .args(args)
            .env("BIZIK_TMUX_SOCKET", &self.socket)
            .env("BIZIK_CONFIG_DIR", self.root.join("config"))
            .env("BIZIK_CACHE_DIR", self.root.join("cache"))
            .env("HOME", &self.root)
            .env("BIZIK_PANE_STATUS", "off")
            .current_dir(dir)
            .output()
            .expect("running bzk");
        Output {
            code: out.status.code().unwrap_or(-1),
            stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
        }
    }

    /// Talk to the sandbox's own tmux server, never the ambient one.
    pub fn tmux(&self, args: &[&str]) -> String {
        let out = Command::new("tmux")
            .args(["-L", &self.socket])
            .args(args)
            .output()
            .expect("running tmux");
        String::from_utf8_lossy(&out.stdout).into_owned()
    }

    pub fn tmux_ok(&self, args: &[&str]) -> bool {
        Command::new("tmux")
            .args(["-L", &self.socket])
            .args(args)
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
    }

    pub fn tmux_sessions(&self) -> Vec<String> {
        self.tmux(&["list-sessions", "-F", "#{session_name}"])
            .lines()
            .map(str::to_string)
            .collect()
    }

    /// A directory inside the sandbox, created on demand.
    pub fn dir(&self, name: &str) -> PathBuf {
        let p = self.root.join(name);
        std::fs::create_dir_all(&p).expect("creating a directory");
        p
    }

    pub fn kill_server(&self) {
        let _ = Command::new("tmux")
            .args(["-L", &self.socket, "kill-server"])
            .output();
    }

    /// The host store as bizik wrote it.
    pub fn host_store(&self) -> serde_json::Value {
        let raw = std::fs::read_to_string(self.root.join("config/host.json")).unwrap_or_default();
        serde_json::from_str(&raw).unwrap_or(serde_json::Value::Null)
    }

    pub fn write_host_store(&self, value: &serde_json::Value) {
        std::fs::write(
            self.root.join("config/host.json"),
            serde_json::to_vec_pretty(value).unwrap(),
        )
        .expect("writing the host store");
    }

    pub fn probe(&self) -> serde_json::Value {
        let out = self.bzk(&["probe", "--json"]);
        assert_eq!(out.code, 0, "probe failed: {}", out.stderr);
        serde_json::from_str(out.stdout.trim()).expect("probe was not JSON")
    }

    /// Mark a directory and return the folder's uuid.
    pub fn mark(&self, dir: &Path) -> String {
        let out = self.bzk_in(dir, &["mark"]);
        assert_eq!(out.code, 0, "mark failed: {}", out.stderr);
        let marks = self.bzk(&["marks", "--json"]);
        let parsed: serde_json::Value = serde_json::from_str(marks.stdout.trim()).unwrap();
        parsed
            .as_array()
            .and_then(|a| {
                a.iter()
                    .find(|f| f["path"] == dir.to_string_lossy().as_ref())
            })
            .and_then(|f| f["id"].as_str())
            .expect("the folder that was just marked")
            .to_string()
    }

    /// Create a shell session and return its uuid. A shell needs no API access
    /// and starts instantly, which is what makes these tests cheap to run.
    pub fn new_session(&self, folder: &str, title: &str) -> String {
        self.new_session_of(folder, "shell", title)
    }

    /// A session record for a given agent. Useful for reproducing states an
    /// agent gets into without needing the agent installed.
    pub fn new_session_of(&self, folder: &str, agent: &str, title: &str) -> String {
        let out = self.bzk(&[
            "new-session",
            "--folder",
            folder,
            "--agent",
            agent,
            "--title",
            title,
        ]);
        assert_eq!(out.code, 0, "new-session failed: {}", out.stderr);
        let parsed: serde_json::Value = serde_json::from_str(out.stdout.trim()).unwrap();
        parsed["id"].as_str().expect("a session id").to_string()
    }

    pub fn spawn(&self, session: &str) -> Output {
        self.bzk(&["spawn", "--session", session])
    }

    /// tmux session name for a bizik session uuid, mirroring `Session::tmux_name`.
    pub fn tmux_name(session: &str) -> String {
        format!("bzk-{}", &session.replace('-', "")[..8])
    }

    /// Wait for a condition, so a test never races tmux's own start-up.
    pub fn eventually(&self, what: &str, mut check: impl FnMut() -> bool) {
        for _ in 0..50 {
            if check() {
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
        panic!("timed out waiting for {what}");
    }
}

impl Drop for Sandbox {
    fn drop(&mut self) {
        // Leaving a tmux server behind would make the next run flaky and the
        // machine gradually messier.
        self.kill_server();
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

pub struct Output {
    pub code: i32,
    pub stdout: String,
    pub stderr: String,
}

impl Output {
    pub fn ok(&self) -> &Self {
        assert_eq!(self.code, 0, "expected success, got: {}", self.stderr);
        self
    }

    pub fn failed(&self) -> &Self {
        assert_ne!(self.code, 0, "expected failure, got: {}", self.stdout);
        self
    }
}
