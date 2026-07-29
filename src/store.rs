//! Persistence.
//!
//! Two stores, deliberately separate:
//!
//! * [`HostStore`] (`<config>/host.json`) — folders and sessions that belong to
//!   *this* machine. Present on every server, and on the laptop too, since the
//!   laptop is just another host.
//! * [`LocalStore`] (`<config>/local.json`) — the host list and the layouts.
//!   Only meaningful on a machine you drive from.
//!
//! Both merge by last-write-wins on `updated_at`, with tombstones taking part
//! like any other change. Every serialized save rereads and merges while
//! holding the store lock, and explicit imports use the same rule.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::fs::OpenOptions;
use std::path::{Path, PathBuf};
use uuid::Uuid;

use crate::model::{Folder, Host, Layout, Session};
use crate::util::{atomic_write, config_dir, now_ms};

const STORE_VERSION: u32 = 1;

pub fn host_store_path() -> PathBuf {
    config_dir().join("host.json")
}

pub fn local_store_path() -> PathBuf {
    config_dir().join("local.json")
}

/// Records with a uuid and a last-modified stamp can be merged generically.
pub trait Record {
    fn id(&self) -> Uuid;
    fn updated_at(&self) -> u64;
}

macro_rules! impl_record {
    ($t:ty) => {
        impl Record for $t {
            fn id(&self) -> Uuid {
                self.id
            }
            fn updated_at(&self) -> u64 {
                self.updated_at
            }
        }
    };
}
impl_record!(Folder);
impl_record!(Session);
impl_record!(Host);
impl_record!(Layout);

/// Last-write-wins union of two record sets, keyed by uuid.
///
/// A tombstone is an ordinary update, so a delete propagates as long as it is
/// newer — which is exactly why deletes are never physical removals.
pub fn merge<T: Record + Clone>(mine: &[T], theirs: &[T]) -> Vec<T> {
    let mut out: Vec<T> = mine.to_vec();
    for t in theirs {
        match out.iter_mut().find(|m| m.id() == t.id()) {
            // On an exact millisecond tie, the second input wins. For a save,
            // that is the writer currently holding the lock; refusing it would
            // silently discard a legitimate edit made in the same millisecond.
            Some(m) if t.updated_at() >= m.updated_at() => *m = t.clone(),
            Some(_) => {}
            None => out.push(t.clone()),
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Host store
// ---------------------------------------------------------------------------

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct HostStore {
    pub version: u32,
    #[serde(default)]
    pub folders: Vec<Folder>,
    #[serde(default)]
    pub sessions: Vec<Session>,
}

impl Default for HostStore {
    fn default() -> Self {
        Self {
            version: STORE_VERSION,
            folders: Vec::new(),
            sessions: Vec::new(),
        }
    }
}

impl HostStore {
    pub fn load() -> Result<Self> {
        Self::load_from(&host_store_path())
    }

    pub(crate) fn load_from(path: &Path) -> Result<Self> {
        if !path.exists() {
            return Ok(Self::default());
        }
        let raw =
            std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
        let store: Self =
            serde_json::from_str(&raw).with_context(|| format!("parsing {}", path.display()))?;
        store.validate()?;
        Ok(store)
    }

    pub fn save(&self) -> Result<()> {
        self.save_to(&host_store_path())
    }

    pub(crate) fn save_to(&self, path: &Path) -> Result<()> {
        self.validate()?;
        with_store_lock(path, || {
            let mut combined = if path.exists() {
                Self::load_from(path)?
            } else {
                self.clone()
            };
            if path.exists() {
                combined.merge_from(self);
            }
            combined.validate()?;
            let bytes = serde_json::to_vec_pretty(&combined)?;
            atomic_write(path, &bytes)
        })
    }

    /// Refuse an on-disk format this binary does not understand.
    ///
    /// Silently accepting a newer version is unsafe even when Serde can happen
    /// to deserialize it: saving afterwards would discard fields this binary
    /// has never heard of.
    pub fn validate(&self) -> Result<()> {
        validate_version("host", self.version)?;
        validate_unique_ids("folder", &self.folders)?;
        validate_unique_ids("session", &self.sessions)?;

        let folders: std::collections::HashMap<Uuid, bool> = self
            .folders
            .iter()
            .map(|folder| {
                validate_text("folder path", &folder.path, 4096, false)?;
                if let Some(label) = folder.label.as_deref() {
                    validate_text("folder label", label, 256, true)?;
                }
                if let Some(remote) = folder.git_remote.as_deref() {
                    validate_text("git remote", remote, 4096, true)?;
                }
                if let Some(branch) = folder.git_branch.as_deref() {
                    validate_text("git branch", branch, 256, true)?;
                }
                Ok((folder.id, folder.deleted_at.is_none()))
            })
            .collect::<Result<_>>()?;
        for session in &self.sessions {
            validate_text("session title", &session.title, 256, false)?;
            if let Some(agent_id) = session.agent_session_id.as_deref() {
                validate_text("agent session id", agent_id, 1024, false)?;
            }
        }

        for session in &self.sessions {
            let Some(folder_is_live) = folders.get(&session.folder_id) else {
                anyhow::bail!(
                    "session {} refers to missing folder {}",
                    session.id,
                    session.folder_id
                );
            };
            if session.deleted_at.is_none() && !folder_is_live {
                anyhow::bail!(
                    "live session {} refers to deleted folder {}",
                    session.id,
                    session.folder_id
                );
            }
        }
        Ok(())
    }

    /// Live folders, pinned first and newest activity first within each group.
    pub fn live_folders(&self) -> Vec<&Folder> {
        let mut v: Vec<&Folder> = self
            .folders
            .iter()
            .filter(|f| f.deleted_at.is_none())
            .collect();
        v.sort_by_key(|f| {
            (
                std::cmp::Reverse(f.pinned),
                std::cmp::Reverse(f.last_visit.unwrap_or(f.created_at)),
            )
        });
        v
    }

    pub fn live_sessions(&self) -> Vec<&Session> {
        self.sessions
            .iter()
            .filter(|s| s.deleted_at.is_none())
            .collect()
    }

    pub fn folder_by_path(&self, path: &str) -> Option<&Folder> {
        self.folders
            .iter()
            .find(|f| f.path == path && f.deleted_at.is_none())
    }

    pub fn folder_mut(&mut self, id: Uuid) -> Option<&mut Folder> {
        self.folders.iter_mut().find(|f| f.id == id)
    }

    /// A session that has not been forgotten.
    ///
    /// Tombstones must not be visible here. Returning one let `spawn` start a
    /// session that had been deleted: the tmux session came back and its panes
    /// worked, but nothing that reports live sessions would ever mention it —
    /// so it showed up as a layout with missing panes that nevertheless opened
    /// fine, and as a session no dashboard could stop.
    pub fn session(&self, id: Uuid) -> Option<&Session> {
        self.sessions
            .iter()
            .find(|s| s.id == id && s.deleted_at.is_none())
    }

    pub fn session_mut(&mut self, id: Uuid) -> Option<&mut Session> {
        self.sessions
            .iter_mut()
            .find(|s| s.id == id && s.deleted_at.is_none())
    }

    /// Mark a path, or revive its tombstone if it was marked before — so
    /// re-marking a folder keeps its uuid, and therefore keeps every layout
    /// that referenced it working.
    pub fn upsert_folder(&mut self, path: &str) -> Uuid {
        if let Some(existing) = self.folders.iter_mut().find(|f| f.path == path) {
            existing.deleted_at = None;
            existing.updated_at = now_ms();
            return existing.id;
        }
        let folder = Folder::new(path.to_string());
        let id = folder.id;
        self.folders.push(folder);
        id
    }

    pub fn remove_folder(&mut self, id: Uuid) -> bool {
        let now = now_ms();
        let mut hit = false;
        if let Some(f) = self.folders.iter_mut().find(|f| f.id == id) {
            f.deleted_at = Some(now);
            f.updated_at = now;
            hit = true;
        }
        // Sessions cannot outlive their folder.
        for s in self.sessions.iter_mut().filter(|s| s.folder_id == id) {
            s.deleted_at = Some(now);
            s.updated_at = now;
        }
        hit
    }

    pub fn remove_session(&mut self, id: Uuid) -> bool {
        let now = now_ms();
        match self.sessions.iter_mut().find(|s| s.id == id) {
            Some(s) => {
                s.deleted_at = Some(now);
                s.updated_at = now;
                true
            }
            None => false,
        }
    }

    pub fn merge_from(&mut self, other: &HostStore) {
        self.folders = merge(&self.folders, &other.folders);
        self.sessions = merge(&self.sessions, &other.sessions);
    }
}

// ---------------------------------------------------------------------------
// Local store
// ---------------------------------------------------------------------------

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct LocalStore {
    pub version: u32,
    /// Identifies this laptop in a future multi-device merge.
    pub device_id: Uuid,
    #[serde(default)]
    pub hosts: Vec<Host>,
    #[serde(default)]
    pub layouts: Vec<Layout>,
}

impl Default for LocalStore {
    fn default() -> Self {
        Self {
            version: STORE_VERSION,
            device_id: Uuid::new_v4(),
            hosts: Vec::new(),
            layouts: Vec::new(),
        }
    }
}

impl LocalStore {
    pub fn load() -> Result<Self> {
        Self::load_from(&local_store_path())
    }

    pub(crate) fn load_from(path: &Path) -> Result<Self> {
        if !path.exists() {
            return Ok(Self::default());
        }
        let raw =
            std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
        let store: Self =
            serde_json::from_str(&raw).with_context(|| format!("parsing {}", path.display()))?;
        store.validate()?;
        Ok(store)
    }

    pub fn save(&self) -> Result<()> {
        self.save_to(&local_store_path())
    }

    pub(crate) fn save_to(&self, path: &Path) -> Result<()> {
        self.validate()?;
        with_store_lock(path, || {
            let mut combined = if path.exists() {
                Self::load_from(path)?
            } else {
                self.clone()
            };
            if path.exists() {
                combined.merge_from(self);
            }
            combined.validate()?;
            let bytes = serde_json::to_vec_pretty(&combined)?;
            atomic_write(path, &bytes)
        })
    }

    /// Refuse to read or overwrite a store from an incompatible release.
    pub fn validate(&self) -> Result<()> {
        validate_version("local", self.version)?;
        validate_unique_ids("host", &self.hosts)?;
        validate_unique_ids("layout", &self.layouts)?;

        let mut live_names = HashSet::new();
        let mut live_local_hosts = 0;
        for host in &self.hosts {
            validate_host_name(&host.name)?;
            if let Some(target) = host.ssh.as_deref() {
                validate_ssh_target(target)?;
            }
            if host.deleted_at.is_none() {
                if !live_names.insert(host.name.as_str()) {
                    anyhow::bail!("duplicate live host name {}", host.name);
                }
                if host.is_local() {
                    live_local_hosts += 1;
                }
            }
        }
        if live_local_hosts > 1 {
            anyhow::bail!("configuration contains more than one local host");
        }

        for layout in &self.layouts {
            validate_text("layout name", &layout.name, 256, false)
                .with_context(|| format!("invalid layout {}", layout.id))?;
            if let Some(geometry) = layout.tmux_layout.as_deref() {
                validate_text("tmux layout", geometry, 8192, false)
                    .with_context(|| format!("invalid layout {}", layout.id))?;
            }
            for pane in &layout.panes {
                validate_host_name(&pane.host).with_context(|| {
                    format!("layout {} has an invalid host reference", layout.id)
                })?;
            }
        }
        Ok(())
    }

    pub fn live_hosts(&self) -> Vec<&Host> {
        self.hosts
            .iter()
            .filter(|h| h.deleted_at.is_none())
            .collect()
    }

    pub fn live_layouts(&self) -> Vec<&Layout> {
        self.layouts
            .iter()
            .filter(|l| l.deleted_at.is_none())
            .collect()
    }

    pub fn host_by_name(&self, name: &str) -> Option<&Host> {
        self.hosts
            .iter()
            .find(|h| h.name == name && h.deleted_at.is_none())
    }

    pub fn add_host(&mut self, name: &str, ssh: Option<String>) -> Result<Uuid> {
        validate_host_name(name)?;
        if let Some(target) = ssh.as_deref() {
            validate_ssh_target(target)?;
        }

        if let Some(index) = self.hosts.iter().position(|h| h.name == name) {
            let existing_id = self.hosts[index].id;
            if self.hosts[index].is_local() != ssh.is_none() {
                anyhow::bail!(
                    "host {name} cannot change between local and remote; remove it and choose a \
                     different name"
                );
            }
            if ssh.is_none()
                && self
                    .hosts
                    .iter()
                    .any(|h| h.id != existing_id && h.is_local() && h.deleted_at.is_none())
            {
                anyhow::bail!("this configuration already has a local host");
            }
            let existing = &mut self.hosts[index];
            existing.ssh = ssh;
            existing.deleted_at = None;
            existing.updated_at = now_ms();
            return Ok(existing.id);
        }

        if ssh.is_none()
            && self
                .hosts
                .iter()
                .any(|h| h.is_local() && h.deleted_at.is_none())
        {
            anyhow::bail!("this configuration already has a local host");
        }
        let host = Host::new(name.to_string(), ssh);
        let id = host.id;
        self.hosts.push(host);
        Ok(id)
    }

    pub fn remove_host(&mut self, name: &str) -> bool {
        let now = now_ms();
        match self.hosts.iter_mut().find(|h| h.name == name) {
            Some(h) => {
                h.deleted_at = Some(now);
                h.updated_at = now;
                true
            }
            None => false,
        }
    }

    pub fn remove_layout(&mut self, id: Uuid) -> bool {
        let now = now_ms();
        match self.layouts.iter_mut().find(|l| l.id == id) {
            Some(l) => {
                l.deleted_at = Some(now);
                l.updated_at = now;
                true
            }
            None => false,
        }
    }

    /// Ensure a `local` host exists, so a fresh install can drive the machine
    /// it is installed on without any setup.
    pub fn ensure_local_host(&mut self) -> bool {
        if self
            .hosts
            .iter()
            .any(|h| h.is_local() && h.deleted_at.is_none())
        {
            return false;
        }
        let live_names: std::collections::HashSet<String> = self
            .hosts
            .iter()
            .filter(|h| h.deleted_at.is_none())
            .map(|h| h.name.clone())
            .collect();
        if let Some(local) = self
            .hosts
            .iter_mut()
            .find(|host| host.is_local() && !live_names.contains(&host.name))
        {
            local.deleted_at = None;
            local.updated_at = now_ms();
            return true;
        }
        let name = (1..)
            .map(|suffix| {
                if suffix == 1 {
                    "local".to_string()
                } else {
                    format!("local-{suffix}")
                }
            })
            .find(|candidate| !live_names.contains(candidate))
            .unwrap_or_else(|| format!("local-{}", Uuid::new_v4().simple()));
        self.hosts.push(Host::new(name, None));
        true
    }

    pub fn merge_from(&mut self, other: &LocalStore) {
        self.hosts = merge(&self.hosts, &other.hosts);
        self.layouts = merge(&self.layouts, &other.layouts);
    }
}

fn validate_version(kind: &str, version: u32) -> Result<()> {
    if version == STORE_VERSION {
        return Ok(());
    }
    anyhow::bail!(
        "unsupported {kind} store version {version}; this bzk supports version \
         {STORE_VERSION} — update bzk before modifying this configuration"
    )
}

fn validate_unique_ids<T: Record>(kind: &str, records: &[T]) -> Result<()> {
    let mut ids = HashSet::with_capacity(records.len());
    for record in records {
        if !ids.insert(record.id()) {
            anyhow::bail!("duplicate {kind} id {}", record.id());
        }
    }
    Ok(())
}

fn validate_text(kind: &str, value: &str, max: usize, allow_empty: bool) -> Result<()> {
    if !allow_empty && value.trim().is_empty() {
        anyhow::bail!("{kind} cannot be empty");
    }
    if value.chars().count() > max {
        anyhow::bail!("{kind} is longer than {max} characters");
    }
    if value.chars().any(char::is_control) {
        anyhow::bail!("{kind} cannot contain control characters");
    }
    Ok(())
}

fn validate_host_name(name: &str) -> Result<()> {
    if name.is_empty() {
        anyhow::bail!("host name cannot be empty");
    }
    if name.trim() != name {
        anyhow::bail!("host name cannot start or end with whitespace");
    }
    if name.chars().count() > 64 {
        anyhow::bail!("host name is longer than 64 characters");
    }
    if name.chars().any(char::is_control) {
        anyhow::bail!("host name cannot contain control characters");
    }
    Ok(())
}

fn validate_ssh_target(target: &str) -> Result<()> {
    if target.is_empty() {
        anyhow::bail!("ssh target cannot be empty");
    }
    if target.trim() != target {
        anyhow::bail!("ssh target cannot start or end with whitespace");
    }
    if target.chars().count() > 512 {
        anyhow::bail!("ssh target is longer than 512 characters");
    }
    if target.chars().any(char::is_control) {
        anyhow::bail!("ssh target cannot contain control characters");
    }
    Ok(())
}

/// Serialize every read-merge-write cycle for a store.
///
/// Atomic rename prevents torn JSON, but without this lock two healthy bzk
/// processes can both read the same revision and the later save can erase the
/// earlier process's unrelated change.
fn with_store_lock<T>(path: &Path, operation: impl FnOnce() -> Result<T>) -> Result<T> {
    let parent = path
        .parent()
        .with_context(|| format!("{} has no parent directory", path.display()))?;
    std::fs::create_dir_all(parent)
        .with_context(|| format!("creating directory {}", parent.display()))?;
    let lock_path = path.with_extension("lock");

    #[cfg(unix)]
    use std::os::unix::fs::OpenOptionsExt;

    let mut options = OpenOptions::new();
    options.read(true).write(true).create(true);
    #[cfg(unix)]
    options.mode(0o600);
    let lock = options
        .open(&lock_path)
        .with_context(|| format!("opening store lock {}", lock_path.display()))?;
    lock.lock()
        .with_context(|| format!("locking {}", lock_path.display()))?;
    operation()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::AgentKind;

    #[test]
    fn merge_prefers_newer_and_keeps_unseen() {
        let mut a = Folder::new("/a".into());
        let mut b = a.clone();
        b.label = Some("newer".into());
        b.updated_at = a.updated_at + 10;
        let other = Folder::new("/other".into());

        let merged = merge(&[a.clone()], &[b.clone(), other]);
        assert_eq!(merged.len(), 2);
        let got = merged.iter().find(|f| f.id == a.id).unwrap();
        assert_eq!(got.label.as_deref(), Some("newer"));

        // Older loses even when it arrives second.
        a.label = Some("older".into());
        let merged = merge(&[b], &[a]);
        assert_eq!(merged[0].label.as_deref(), Some("newer"));
    }

    #[test]
    fn tombstone_propagates_as_an_ordinary_update() {
        let live = Folder::new("/a".into());
        let mut dead = live.clone();
        dead.deleted_at = Some(live.updated_at + 5);
        dead.updated_at = live.updated_at + 5;

        let merged = merge(&[live], &[dead]);
        assert!(merged[0].deleted_at.is_some());
    }

    #[test]
    fn remarking_a_path_revives_the_same_uuid() {
        let mut s = HostStore::default();
        let first = s.upsert_folder("/x");
        s.remove_folder(first);
        let second = s.upsert_folder("/x");
        assert_eq!(
            first, second,
            "layouts referencing this folder must survive"
        );
        assert!(s.live_folders().len() == 1);
    }

    #[test]
    fn a_forgotten_session_cannot_be_looked_up_and_started_again() {
        let mut s = HostStore::default();
        let f = s.upsert_folder("/x");
        let session = Session::new(f, AgentKind::Claude, "t".into());
        let id = session.id;
        s.sessions.push(session);
        assert!(s.session(id).is_some());

        s.remove_session(id);
        assert!(s.session(id).is_none(), "a tombstone is not a session");
        assert!(s.session_mut(id).is_none());
    }

    #[test]
    fn removing_a_folder_buries_its_sessions() {
        let mut s = HostStore::default();
        let f = s.upsert_folder("/x");
        s.sessions
            .push(Session::new(f, AgentKind::Claude, "t".into()));
        s.remove_folder(f);
        assert!(s.live_sessions().is_empty());
    }

    #[test]
    fn an_unknown_store_version_is_never_silently_overwritten() {
        let mut host = HostStore::default();
        host.version += 1;
        let error = host
            .validate()
            .expect_err("a future format must be refused");
        assert!(error.to_string().contains("update bzk"), "{error:#}");

        let local = LocalStore {
            version: 0,
            ..LocalStore::default()
        };
        assert!(local.validate().is_err());
    }

    #[test]
    fn duplicate_ids_and_broken_references_are_refused() {
        let mut host = HostStore::default();
        let folder = Folder::new("/repo".into());
        host.folders.push(folder.clone());
        host.folders.push(folder);
        assert!(
            host.validate()
                .expect_err("ambiguous ids cannot be merged safely")
                .to_string()
                .contains("duplicate folder id")
        );

        let mut host = HostStore::default();
        host.sessions.push(Session::new(
            Uuid::new_v4(),
            AgentKind::Claude,
            "orphan".into(),
        ));
        assert!(
            host.validate()
                .expect_err("a live session needs its folder")
                .to_string()
                .contains("missing folder")
        );
    }

    #[test]
    fn local_store_validates_names_and_single_local_host_invariant() {
        let mut local = LocalStore::default();
        local.hosts.push(Host::new("same".into(), None));
        local.hosts.push(Host::new("same".into(), None));
        let error = local
            .validate()
            .expect_err("duplicate names and local hosts are ambiguous");
        assert!(
            error.to_string().contains("duplicate live host name"),
            "{error:#}"
        );

        local.hosts[1].name = "other".into();
        assert!(
            local
                .validate()
                .expect_err("only one host can represent this machine")
                .to_string()
                .contains("more than one local host")
        );
    }

    #[test]
    fn terminal_control_sequences_are_rejected_in_importable_text() {
        let mut host = HostStore::default();
        let folder_id = host.upsert_folder("/repo");
        host.folder_mut(folder_id).unwrap().label = Some("safe\u{1b}[2J".into());
        assert!(
            host.validate()
                .expect_err("stored labels are rendered in a terminal")
                .to_string()
                .contains("control characters")
        );

        let mut local = LocalStore::default();
        local
            .layouts
            .push(Layout::new("layout\nspoof".into(), Vec::new(), None));
        let error = local
            .validate()
            .expect_err("layout names are rendered in a terminal");
        assert!(format!("{error:#}").contains("control characters"));
    }

    #[test]
    fn stale_writers_do_not_erase_each_others_records() {
        let dir = std::env::temp_dir().join(format!("bzk-store-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("host.json");

        let seed = HostStore::default();
        seed.save_to(&path).unwrap();
        let mut first = HostStore::load_from(&path).unwrap();
        let mut second = HostStore::load_from(&path).unwrap();

        first.upsert_folder("/first");
        first.save_to(&path).unwrap();
        second.upsert_folder("/second");
        second.save_to(&path).unwrap();

        let saved = HostStore::load_from(&path).unwrap();
        assert!(saved.folder_by_path("/first").is_some());
        assert!(saved.folder_by_path("/second").is_some());
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn a_stale_writer_cannot_create_a_live_session_under_a_deleted_folder() {
        let dir =
            std::env::temp_dir().join(format!("bzk-store-conflict-{}", Uuid::new_v4().simple()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("host.json");

        let mut seed = HostStore::default();
        let folder_id = seed.upsert_folder("/repo");
        seed.save_to(&path).unwrap();
        let mut stale = HostStore::load_from(&path).unwrap();
        let mut current = HostStore::load_from(&path).unwrap();

        let old_stamp = stale.folder_mut(folder_id).unwrap().updated_at;
        current.remove_folder(folder_id);
        let deleted = current.folder_mut(folder_id).unwrap();
        deleted.updated_at = old_stamp + 10;
        deleted.deleted_at = Some(old_stamp + 10);
        current.save_to(&path).unwrap();

        stale.sessions.push(Session::new(
            folder_id,
            AgentKind::Claude,
            "late writer".into(),
        ));
        let error = stale
            .save_to(&path)
            .expect_err("the merged state, not only each input, must be valid");
        assert!(error.to_string().contains("deleted folder"), "{error:#}");

        let saved = HostStore::load_from(&path).unwrap();
        assert!(saved.live_sessions().is_empty());
        assert!(saved.folder_by_path("/repo").is_none());
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn a_local_host_cannot_be_silently_converted_to_remote() {
        let mut store = LocalStore::default();
        assert!(store.ensure_local_host());

        let error = store
            .add_host("local", Some("server".into()))
            .expect_err("changing host kind breaks layout identity");

        assert!(error.to_string().contains("cannot change"), "{error:#}");
        assert_eq!(store.live_hosts().len(), 1);
        assert!(store.live_hosts()[0].is_local());
    }

    #[test]
    fn local_host_uses_a_unique_name_when_remote_already_owns_local() {
        let mut store = LocalStore::default();
        store.add_host("local", Some("server".into())).unwrap();

        assert!(store.ensure_local_host());

        assert!(store.host_by_name("local").is_some_and(|h| !h.is_local()));
        assert!(store.host_by_name("local-2").is_some_and(Host::is_local));
    }

    #[test]
    fn host_inputs_are_validated_at_the_domain_boundary() {
        let mut store = LocalStore::default();
        for name in ["", " leading", "trailing ", "line\nbreak"] {
            assert!(store.add_host(name, Some("server".into())).is_err());
        }
        for target in ["", " server", "server ", "server\ncommand"] {
            assert!(store.add_host("remote", Some(target.into())).is_err());
        }
    }

    #[test]
    fn ensuring_local_revives_its_tombstone_instead_of_changing_identity() {
        let mut store = LocalStore::default();
        assert!(store.ensure_local_host());
        let id = store.host_by_name("local").unwrap().id;
        assert!(store.remove_host("local"));

        assert!(store.ensure_local_host());

        assert_eq!(store.host_by_name("local").unwrap().id, id);
        assert_eq!(
            store.hosts.len(),
            1,
            "a second UUID would break old layouts"
        );
    }
}
