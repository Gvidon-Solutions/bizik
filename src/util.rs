//! Small helpers shared by every layer: paths, clocks, atomic writes, bounded
//! file reads and /proc introspection.

use anyhow::{Context, Result};
use std::fs::{self, File};
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

pub fn home() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/"))
}

/// `$BIZIK_CONFIG_DIR` overrides everything — handy for tests and for running
/// two independent profiles on one machine.
pub fn config_dir() -> PathBuf {
    if let Some(d) = std::env::var_os("BIZIK_CONFIG_DIR") {
        return PathBuf::from(d);
    }
    std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| home().join(".config"))
        .join("bizik")
}

pub fn cache_dir() -> PathBuf {
    if let Some(d) = std::env::var_os("BIZIK_CACHE_DIR") {
        return PathBuf::from(d);
    }
    std::env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| home().join(".cache"))
        .join("bizik")
}

/// Write via a temp file + rename so a crash mid-write can never leave a
/// truncated store behind.
pub fn atomic_write(path: &Path, bytes: &[u8]) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("creating directory {}", parent.display()))?;
    }
    let tmp = path.with_extension(format!("tmp.{}", std::process::id()));
    fs::write(&tmp, bytes).with_context(|| format!("writing {}", tmp.display()))?;
    fs::rename(&tmp, path).with_context(|| format!("renaming into {}", path.display()))?;
    Ok(())
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
    let mut buf = Vec::with_capacity(max.min(len as usize));
    f.take(max as u64).read_to_end(&mut buf)?;
    Ok(String::from_utf8_lossy(&buf).into_owned())
}

pub fn mtime_ms(path: &Path) -> u64 {
    fs::metadata(path)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

pub fn file_size(path: &Path) -> u64 {
    fs::metadata(path).map(|m| m.len()).unwrap_or(0)
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

/// Collapse whitespace and clamp to `max` characters, for one-line previews.
pub fn one_line(s: &str, max: usize) -> String {
    let flat = s.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.chars().count() <= max {
        return flat;
    }
    let cut: String = flat.chars().take(max.saturating_sub(1)).collect();
    format!("{cut}…")
}
