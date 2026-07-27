//! Reaching other machines.
//!
//! Transport is plain ssh with connection multiplexing, and the multiplexing
//! options are passed on the command line rather than expected in
//! `~/.ssh/config` — bizik must work on a machine whose ssh config it has never
//! touched, and a dashboard that opens six panes would otherwise pay six full
//! handshakes.
//!
//! A host with no ssh target is the local machine, and takes the same code
//! path with the ssh prefix omitted. That is what lets bizik manage the laptop
//! it runs on without a special case anywhere above this module.

use anyhow::{Context, Result, bail};
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::model::{Host, Probe};
use crate::util::{cache_dir, shell_quote};

/// Where the installer puts the binary on a remote host.
pub const REMOTE_BIN: &str = ".local/bin/bzk";

/// A unix socket path cannot exceed 108 bytes, and ssh refuses the connection
/// outright rather than degrading. Leave room for the 40-character `%C` hash
/// and the filename.
const MAX_CONTROL_PATH: usize = 100;

/// Where connection-sharing sockets live.
///
/// Not the cache directory: `%C` alone is 40 characters, and a long `$HOME` or
/// `XDG_CACHE_HOME` pushes the socket past the limit. A short path under `/tmp`
/// — the same convention tmux uses for its own sockets — keeps it well clear
/// regardless of where the user's home happens to be.
fn control_dir() -> Option<PathBuf> {
    let dir = PathBuf::from(format!("/tmp/bzk-{}", uid()));
    private_dir(&dir).ok().map(|()| dir).or_else(|| {
        let fallback = cache_dir();
        std::fs::create_dir_all(&fallback).ok().map(|()| fallback)
    })
}

/// Create a directory only we can read, and refuse one that is already
/// somebody else's — a predictable name under `/tmp` is otherwise easy to
/// squat on.
fn private_dir(path: &Path) -> Result<()> {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};

    std::fs::create_dir_all(path)?;
    let meta = std::fs::symlink_metadata(path)?;
    if !meta.is_dir() {
        bail!("{} is not a directory", path.display());
    }
    if meta.uid() != uid() {
        bail!("{} belongs to another user", path.display());
    }
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))?;
    Ok(())
}

fn uid() -> u32 {
    use std::os::unix::fs::MetadataExt;
    std::fs::metadata("/proc/self")
        .map(|m| m.uid())
        .unwrap_or(0)
}

/// ssh options shared by every invocation.
///
/// Connection sharing is an optimisation — six panes opening at once should
/// cost one handshake, not six. It is therefore dropped silently when the
/// socket path will not fit, because a slower connection is a far better
/// outcome than no connection.
fn base_opts() -> Vec<String> {
    let mut opts = vec![
        "-o".into(),
        "ServerAliveInterval=15".into(),
        "-o".into(),
        "ServerAliveCountMax=3".into(),
    ];

    if let Some(dir) = control_dir() {
        let control = dir.join("cm-%C");
        if control.as_os_str().len() <= MAX_CONTROL_PATH {
            opts.extend([
                "-o".into(),
                "ControlMaster=auto".into(),
                "-o".into(),
                format!("ControlPath={}", control.display()),
                "-o".into(),
                "ControlPersist=10m".into(),
            ]);
        }
    }
    opts
}

fn ensure_control_dir() {
    let _ = control_dir();
}

/// Locate bizik on a remote host.
///
/// A command sent over ssh runs without a login shell, so `$PATH` frequently
/// lacks `~/.local/bin` — the very directory the installer writes to. Probing
/// the known location first and falling back to `$PATH` covers both a bizik
/// installed by us and one installed some other way.
fn remote_bzk(args: &[&str]) -> String {
    let quoted: Vec<String> = args.iter().map(|a| shell_quote(a)).collect();
    format!(
        "if [ -x \"$HOME/{REMOTE_BIN}\" ]; then exec \"$HOME/{REMOTE_BIN}\" {a}; else exec bzk {a}; fi",
        a = quoted.join(" ")
    )
}

/// Run `bzk <args>` on `host` and return stdout.
pub fn run_bzk(host: &Host, args: &[&str]) -> Result<String> {
    match &host.ssh {
        None => {
            let exe = crate::util::own_exe()?;
            let out = Command::new(exe)
                .args(args)
                .output()
                .context("running bzk locally")?;
            if !out.status.success() {
                bail!("{}", String::from_utf8_lossy(&out.stderr).trim());
            }
            Ok(String::from_utf8_lossy(&out.stdout).into_owned())
        }
        Some(target) => {
            ensure_control_dir();
            let mut cmd = Command::new("ssh");
            cmd.args(base_opts());
            // Fail fast instead of hanging on a password prompt: a probe runs
            // unattended and a stuck one would freeze the dashboard.
            cmd.args(["-o", "BatchMode=yes", "-o", "ConnectTimeout=8"]);
            cmd.arg(target);
            cmd.arg(remote_bzk(args));

            let out = cmd.output().context("running ssh")?;
            if !out.status.success() {
                let err = String::from_utf8_lossy(&out.stderr);
                let err = err.trim();
                if err.contains("not found") || err.contains("No such file") {
                    bail!(
                        "bzk is not installed on this host — run: bzk install {}",
                        host.name
                    );
                }
                bail!("{}", if err.is_empty() { "ssh failed" } else { err });
            }
            Ok(String::from_utf8_lossy(&out.stdout).into_owned())
        }
    }
}

/// The command a local tmux pane runs in order to show a session.
///
/// The pane holds nothing but a viewer. The agent itself is in a detached tmux
/// session on the host, which is why closing the laptop costs nothing.
///
/// The `=` that anchors a tmux target to an exact name **must stay quoted**.
/// tmux runs a pane's command through the user's login shell, and zsh expands a
/// word beginning with `=` to the path of a command by that name — so an
/// unquoted `-t =bzk-a3f9` dies with "bzk-a3f9 not found" before tmux ever sees
/// it. For a remote pane the quoting has to survive two shells: the double
/// quotes are consumed locally, and the single quotes travel to the login shell
/// on the far side, which would otherwise expand it all over again.
pub fn attach_command(host: &Host, tmux_name: &str) -> String {
    match &host.ssh {
        // Same tmux server: `TMUX` has to be cleared for tmux to allow the
        // nested attach at all.
        None => format!("env TMUX= {} attach -t '={tmux_name}'", crate::tmux::cli()),
        Some(target) => {
            let opts = base_opts().join(" ");
            format!("ssh {opts} -t {target} \"tmux attach -t '={tmux_name}'\"")
        }
    }
}

/// Copy this binary to a host and put it on `$PATH`.
pub fn install(host: &Host) -> Result<String> {
    let Some(target) = &host.ssh else {
        return Ok("local host needs no install".into());
    };
    let exe = crate::util::own_exe()?;
    ensure_control_dir();

    let mkdir = Command::new("ssh")
        .args(base_opts())
        .arg(target)
        .arg("mkdir -p ~/.local/bin")
        .output()
        .context("running ssh")?;
    if !mkdir.status.success() {
        bail!("{}", String::from_utf8_lossy(&mkdir.stderr).trim());
    }

    // Land on a temp name and rename, so an in-flight copy can never leave a
    // half-written binary that the next probe would try to execute.
    let scp = Command::new("scp")
        .args(base_opts())
        .arg(&exe)
        .arg(format!("{target}:.local/bin/bzk.new"))
        .output()
        .context("running scp")?;
    if !scp.status.success() {
        bail!("{}", String::from_utf8_lossy(&scp.stderr).trim());
    }

    let finish = Command::new("ssh")
        .args(base_opts())
        .arg(target)
        .arg("chmod +x ~/.local/bin/bzk.new && mv ~/.local/bin/bzk.new ~/.local/bin/bzk && ~/.local/bin/bzk --version")
        .output()
        .context("running ssh")?;
    if !finish.status.success() {
        let err = String::from_utf8_lossy(&finish.stderr);
        // The usual cause is a binary built against a newer glibc than the
        // server has. Say so, because "version `GLIBC_2.39' not found" on its
        // own does not suggest the fix.
        if err.contains("GLIBC") || err.contains("not found") {
            bail!(
                "the copied binary will not run there: {}\n\
                 build a static one and install again:\n  \
                 rustup target add x86_64-unknown-linux-musl\n  \
                 cargo build --release --target x86_64-unknown-linux-musl",
                err.trim()
            );
        }
        bail!("{}", err.trim());
    }
    Ok(String::from_utf8_lossy(&finish.stdout).trim().to_string())
}

/// The result of probing one host: either its report, or why there isn't one.
#[derive(Debug, Clone)]
pub struct HostProbe {
    pub host: Host,
    pub probe: Option<Probe>,
    pub error: Option<String>,
}

/// Probe every host at once.
///
/// Hosts are independent and a slow or unreachable one must not delay the rest,
/// so each gets a thread and a failure is recorded rather than propagated.
pub fn probe_all(hosts: &[Host]) -> Vec<HostProbe> {
    let mut results: Vec<HostProbe> = Vec::with_capacity(hosts.len());

    std::thread::scope(|scope| {
        let handles: Vec<_> = hosts
            .iter()
            .map(|host| scope.spawn(move || probe_one(host)))
            .collect();

        for (host, handle) in hosts.iter().zip(handles) {
            let probed = handle.join().unwrap_or_else(|_| HostProbe {
                host: host.clone(),
                probe: None,
                error: Some("probe thread panicked".into()),
            });
            results.push(probed);
        }
    });

    results
}

/// Just enough of a probe to know whether the rest can be read.
///
/// Deserialised on its own, and first. Checking the version *after* parsing the
/// whole payload is no check at all: the moment a field changes shape, parsing
/// fails before the version is ever looked at, and the user gets
/// "missing field `session` at line 1 column 712" instead of "update this
/// host". Every field here is optional, so this struct can always be read.
#[derive(serde::Deserialize)]
struct Envelope {
    #[serde(default)]
    protocol: u32,
}

pub fn probe_one(host: &Host) -> HostProbe {
    let fail = |error: String| HostProbe {
        host: host.clone(),
        probe: None,
        error: Some(error),
    };

    // `--preview` costs one `capture-pane` per session and is what puts the
    // last few lines of each pane in the dashboard.
    let raw = match run_bzk(host, &["probe", "--json", "--preview"]) {
        Ok(raw) => raw,
        Err(e) => return fail(format!("{e:#}")),
    };

    match serde_json::from_str::<Envelope>(&raw) {
        Ok(envelope) if envelope.protocol != crate::model::PROTOCOL => {
            return fail(format!(
                "speaks protocol {} but this bizik is {} — run: bzk install {}",
                envelope.protocol,
                crate::model::PROTOCOL,
                host.name
            ));
        }
        Ok(_) => {}
        Err(e) => {
            return fail(format!(
                "did not answer with a bizik probe ({e}): {}",
                crate::util::one_line(&raw, 100)
            ));
        }
    }

    match serde_json::from_str::<Probe>(&raw).with_context(|| {
        format!(
            "parsing probe from {} (got: {})",
            host.name,
            crate::util::one_line(&raw, 120)
        )
    }) {
        Ok(probe) => HostProbe {
            host: host.clone(),
            probe: Some(probe),
            error: None,
        },
        Err(e) => fail(format!("{e:#}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn local_attach_clears_tmux_so_nesting_is_allowed() {
        let h = Host::new("local".into(), None);
        assert_eq!(
            attach_command(&h, "bzk-a1"),
            "env TMUX= tmux attach -t '=bzk-a1'"
        );
    }

    #[test]
    fn remote_attach_requests_a_tty_and_reuses_connections() {
        let h = Host::new("back".into(), Some("root@1.2.3.4".into()));
        let cmd = attach_command(&h, "bzk-a1");
        assert!(cmd.contains("-t root@1.2.3.4"), "a TUI needs a tty");
        assert!(
            cmd.contains("ControlMaster=auto"),
            "six panes, one handshake"
        );
        assert!(cmd.contains("'=bzk-a1'"), "exact session match");
    }

    #[test]
    fn the_exact_match_anchor_is_never_left_bare_for_a_shell() {
        // A bare `=word` is command-path expansion in zsh, which is the login
        // shell tmux uses to run a pane. Both forms must keep it quoted.
        for host in [
            Host::new("local".into(), None),
            Host::new("back".into(), Some("h".into())),
        ] {
            let cmd = attach_command(&host, "bzk-a1");
            assert!(
                !cmd.contains(" =bzk-a1"),
                "unquoted anchor would be eaten by zsh: {cmd}"
            );
        }
    }

    #[test]
    fn remote_quoting_survives_both_shells() {
        let h = Host::new("back".into(), Some("h".into()));
        let cmd = attach_command(&h, "bzk-a1");
        // Outer double quotes are consumed locally; the inner single quotes
        // have to reach the remote login shell intact.
        assert!(cmd.ends_with("\"tmux attach -t '=bzk-a1'\""));
    }

    #[test]
    fn remote_invocation_falls_back_from_local_bin_to_path() {
        let cmd = remote_bzk(&["probe", "--json"]);
        assert!(cmd.contains(".local/bin/bzk"));
        assert!(cmd.contains("exec bzk"));
        assert!(cmd.contains("'probe' '--json'"), "arguments must be quoted");
    }
}
