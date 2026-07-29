//! Small helpers shared by every layer: paths, clocks, atomic writes, bounded
//! file reads and /proc introspection.

use anyhow::{Context, Result};
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};
use uuid::Uuid;

pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| {
            u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
        })
}

pub fn home() -> PathBuf {
    std::env::var_os("HOME").map_or_else(|| PathBuf::from("/"), PathBuf::from)
}

/// `$BIZIK_CONFIG_DIR` overrides everything — handy for tests and for running
/// two independent profiles on one machine.
pub fn config_dir() -> PathBuf {
    if let Some(d) = std::env::var_os("BIZIK_CONFIG_DIR") {
        return PathBuf::from(d);
    }
    std::env::var_os("XDG_CONFIG_HOME")
        .map_or_else(|| home().join(".config"), PathBuf::from)
        .join("bizik")
}

pub fn cache_dir() -> PathBuf {
    if let Some(d) = std::env::var_os("BIZIK_CACHE_DIR") {
        return PathBuf::from(d);
    }
    std::env::var_os("XDG_CACHE_HOME")
        .map_or_else(|| home().join(".cache"), PathBuf::from)
        .join("bizik")
}

/// Write via a temp file + rename so a crash mid-write can never leave a
/// truncated store behind.
pub fn atomic_write(path: &Path, bytes: &[u8]) -> Result<()> {
    let parent = path
        .parent()
        .with_context(|| format!("{} has no parent directory", path.display()))?;
    fs::create_dir_all(parent)
        .with_context(|| format!("creating directory {}", parent.display()))?;

    let name = path
        .file_name()
        .with_context(|| format!("{} has no file name", path.display()))?
        .to_string_lossy();
    // A pid+counter name can collide with a stale file after a crash and pid
    // reuse. A fresh UUID makes abandoned temp files harmless to later writes.
    let tmp = parent.join(format!(".{name}.tmp-{}", Uuid::new_v4().simple()));

    let result = (|| {
        #[cfg(unix)]
        use std::os::unix::fs::OpenOptionsExt;

        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        options.mode(0o600);

        let mut file = options
            .open(&tmp)
            .with_context(|| format!("creating {}", tmp.display()))?;

        // Replacing a file must not unexpectedly change its access policy.
        if let Ok(metadata) = fs::metadata(path) {
            file.set_permissions(metadata.permissions())
                .with_context(|| format!("preserving permissions of {}", path.display()))?;
        }

        file.write_all(bytes)
            .with_context(|| format!("writing {}", tmp.display()))?;
        file.sync_all()
            .with_context(|| format!("syncing {}", tmp.display()))?;
        fs::rename(&tmp, path).with_context(|| format!("renaming into {}", path.display()))?;

        // The rename itself is durable only after its directory entry is
        // synced. Directory fsync is supported on the Linux targets we ship.
        File::open(parent)
            .and_then(|dir| dir.sync_all())
            .with_context(|| format!("syncing directory {}", parent.display()))?;
        Ok(())
    })();

    if result.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    result
}

/// Read at most `max` bytes from the start of a file. Session transcripts reach
/// tens of megabytes, so nothing may ever read one whole.
pub fn read_head(path: &Path, max: usize) -> Result<String> {
    let mut f = File::open(path).with_context(|| format!("opening {}", path.display()))?;
    let mut buf = vec![0u8; max];
    let n = f.read(&mut buf)?;
    buf.truncate(n);
    Ok(String::from_utf8_lossy(&buf).into_owned())
}

/// Read at most `max` bytes from the end of a file. The first line is likely
/// truncated mid-way; callers must tolerate that.
pub fn read_tail(path: &Path, max: usize) -> Result<String> {
    let mut f = File::open(path).with_context(|| format!("opening {}", path.display()))?;
    let len = f.metadata()?.len();
    let start = len.saturating_sub(max as u64);
    f.seek(SeekFrom::Start(start))?;
    let file_len = usize::try_from(len).unwrap_or(usize::MAX);
    let mut buf = Vec::with_capacity(max.min(file_len));
    f.take(max as u64).read_to_end(&mut buf)?;
    Ok(String::from_utf8_lossy(&buf).into_owned())
}

pub fn mtime_ms(path: &Path) -> u64 {
    fs::metadata(path)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map_or(0, |duration| {
            u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
        })
}

pub fn file_size(path: &Path) -> u64 {
    fs::metadata(path).map_or(0, |metadata| metadata.len())
}

// ---------------------------------------------------------------------------
// /proc introspection
// ---------------------------------------------------------------------------

pub fn proc_alive(pid: u32) -> bool {
    Path::new(&format!("/proc/{pid}")).exists()
}

pub fn proc_comm(pid: u32) -> Option<String> {
    fs::read_to_string(format!("/proc/{pid}/comm"))
        .ok()
        .map(|s| s.trim().to_string())
}

pub fn proc_cwd(pid: u32) -> Option<String> {
    fs::read_link(format!("/proc/{pid}/cwd"))
        .ok()
        .map(|p| p.to_string_lossy().into_owned())
}

/// Field 22 of `/proc/<pid>/stat`, the process start time in clock ticks.
///
/// Claude Code records this alongside the pid precisely so a recycled pid can
/// be told apart from the original process. The comm field may contain spaces
/// and parentheses, so the split has to happen after the *last* `)`.
pub fn proc_starttime(pid: u32) -> Option<String> {
    let stat = fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let rest = &stat[stat.rfind(')')? + 1..];
    // After comm, fields resume at index 3 (state), so starttime (field 22) is
    // at offset 22 - 3 = 19 within this remainder.
    rest.split_whitespace().nth(19).map(str::to_string)
}

/// Field 4 of `/proc/<pid>/stat`, the parent pid.
pub fn proc_ppid(pid: u32) -> Option<u32> {
    let stat = fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let rest = &stat[stat.rfind(')')? + 1..];
    rest.split_whitespace().nth(1)?.parse().ok()
}

/// True when `pid` is alive, is the expected program, and — if a start time was
/// recorded — was not recycled.
pub fn proc_matches(pid: u32, comm_prefix: &str, started: Option<&str>) -> bool {
    if !proc_alive(pid) {
        return false;
    }
    match proc_comm(pid) {
        Some(c) if c.starts_with(comm_prefix) => {}
        _ => return false,
    }
    match (started, proc_starttime(pid)) {
        (Some(want), Some(got)) => want == got,
        _ => true,
    }
}

/// Every pid currently running a program whose comm matches `comm`.
pub fn pids_by_comm(comm: &str) -> Vec<u32> {
    let mut out = Vec::new();
    let Ok(entries) = fs::read_dir("/proc") else {
        return out;
    };
    for entry in entries.flatten() {
        let Ok(pid) = entry.file_name().to_string_lossy().parse::<u32>() else {
            continue;
        };
        if proc_comm(pid).as_deref() == Some(comm) {
            out.push(pid);
        }
    }
    out
}

/// Path to this binary, usable for spawning a copy of ourselves.
///
/// Not simply `current_exe()`. On Linux that reads `/proc/self/exe`, and once
/// the file has been replaced — by an upgrade, while this process is still
/// running — the link reads `/path/to/bzk (deleted)`. Spawning that fails with
/// "No such file or directory", which is a baffling way for a long-running
/// dashboard to start failing an hour after an install. Stripping the marker
/// lands on whatever occupies the path now, which is exactly what we want.
pub fn own_exe() -> Result<PathBuf> {
    let raw = std::env::current_exe().context("locating own binary")?;
    let path = strip_deleted(&raw);
    if !path.exists() {
        anyhow::bail!(
            "{} no longer exists — bizik was removed while running; restart it",
            path.display()
        );
    }
    Ok(path)
}

fn strip_deleted(path: &Path) -> PathBuf {
    match path.to_str().and_then(|s| s.strip_suffix(" (deleted)")) {
        Some(stripped) => PathBuf::from(stripped),
        None => path.to_path_buf(),
    }
}

/// This machine's short hostname, for when nobody supplied a better label.
pub fn hostname() -> String {
    fs::read_to_string("/proc/sys/kernel/hostname")
        .ok()
        .map(|h| h.trim().to_string())
        .filter(|h| !h.is_empty())
        .unwrap_or_else(|| "host".to_string())
}

/// Single-quote a string for POSIX shells.
pub fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

/// An `env …` prefix carrying this process's bizik settings into a child
/// started by tmux.
///
/// The tmux server may be older than this process and therefore have stale
/// environment variables. Carrying every `BIZIK_*` variable explicitly keeps
/// alternate config directories, cache directories and test sockets intact.
pub fn config_env() -> String {
    let mut vars: Vec<String> = std::env::vars()
        .filter(|(name, _)| name.starts_with("BIZIK_"))
        .map(|(name, value)| format!("{name}={}", shell_quote(&value)))
        .collect();
    if vars.is_empty() {
        return String::new();
    }
    vars.sort();
    format!("env {} ", vars.join(" "))
}

/// Collapse whitespace and clamp to `max` characters, for one-line previews.
pub fn one_line(s: &str, max: usize) -> String {
    let safe: String = s
        .chars()
        .map(|character| {
            if character.is_control() {
                ' '
            } else {
                character
            }
        })
        .collect();
    let flat = safe.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.chars().count() <= max {
        return flat;
    }
    let cut: String = flat.chars().take(max.saturating_sub(1)).collect();
    format!("{cut}…")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_replaced_binary_resolves_back_to_its_path() {
        // What /proc/self/exe reads after the file has been swapped underneath.
        assert_eq!(
            strip_deleted(Path::new("/home/me/.local/bin/bzk (deleted)")),
            Path::new("/home/me/.local/bin/bzk")
        );
        // An ordinary path, and one that merely looks suspicious, are untouched.
        assert_eq!(
            strip_deleted(Path::new("/usr/bin/bzk")),
            Path::new("/usr/bin/bzk")
        );
        assert_eq!(
            strip_deleted(Path::new("/opt/my (deleted) tools/bzk")),
            Path::new("/opt/my (deleted) tools/bzk")
        );
    }

    #[test]
    fn reads_are_bounded_at_both_ends() {
        // Transcripts reach tens of megabytes; nothing may ever read one whole.
        let dir = std::env::temp_dir().join(format!("bzk-bounded-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("big.jsonl");
        let body: String = (0..500).map(|i| format!("line-{i}\n")).collect();
        std::fs::write(&path, &body).unwrap();

        let head = read_head(&path, 64).unwrap();
        assert!(head.len() <= 64, "head must respect its budget");
        assert!(head.starts_with("line-0"));

        let tail = read_tail(&path, 64).unwrap();
        assert!(tail.len() <= 64);
        assert!(body.ends_with(&tail), "the tail must come from the end");

        // Asking for more than the file has is not an error.
        assert_eq!(read_head(&path, 10 * body.len()).unwrap(), body);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_completed_write_leaves_no_temporary_behind() {
        let dir = std::env::temp_dir().join(format!("bzk-tidy-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("store.json");

        atomic_write(&path, b"first").unwrap();
        atomic_write(&path, b"second").unwrap();

        assert_eq!(std::fs::read(&path).unwrap(), b"second");
        let leftovers: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .filter_map(std::result::Result::ok)
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .filter(|name| name != "store.json")
            .collect();
        assert!(leftovers.is_empty(), "stray temporaries: {leftovers:?}");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn one_line_collapses_whitespace_and_clamps_by_character() {
        assert_eq!(one_line("  a\n\tb   c ", 80), "a b c");
        assert_eq!(one_line("методичка", 5), "мето…");
        assert_eq!(one_line("safe\u{1b}[2Jtext", 80), "safe [2Jtext");
    }

    #[test]
    fn shell_quoting_survives_embedded_quotes() {
        assert_eq!(shell_quote("a'b"), r"'a'\''b'");
    }

    #[cfg(unix)]
    #[test]
    fn atomic_write_preserves_existing_permissions() {
        use std::os::unix::fs::PermissionsExt;

        let dir = std::env::temp_dir().join(format!("bzk-atomic-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("store.json");
        fs::write(&path, b"old").unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o640)).unwrap();

        atomic_write(&path, b"new").unwrap();

        assert_eq!(fs::read(&path).unwrap(), b"new");
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o640
        );
        fs::remove_dir_all(dir).ok();
    }
}
