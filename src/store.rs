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
//! like any other change. Nothing calls [`merge`] yet, but the record shape and
//! the merge rule are what make a second laptop — or a shared config — a
//! configuration question rather than a rewrite.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
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
            Some(m) if t.updated_at() > m.updated_at() => *m = t.clone(),
            Some(_) => {}
            None => out.push(t.clone()),
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Host store
// ---------------------------------------------------------------------------

#[derive(Serialize, Deserialize, Debug)]
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
        let path = host_store_path();
        if !path.exists() {
            return Ok(Self::default());
        }
        let raw = std::fs::read_to_string(&path)
            .with_context(|| format!("reading {}", path.display()))?;
        serde_json::from_str(&raw).with_context(|| format!("parsing {}", path.display()))
    }

    pub fn save(&self) -> Result<()> {
        let bytes = serde_json::to_vec_pretty(self)?;
        atomic_write(&host_store_path(), &bytes)
    }

    /// Live folders, newest activity first.
    pub fn live_folders(&self) -> Vec<&Folder> {
        let mut v: Vec<&Folder> = self.folders.iter().filter(|f| f.deleted_at.is_none()).collect();
        v.sort_by_key(|f| std::cmp::Reverse(f.last_visit.unwrap_or(f.created_at)));
        v
    }

    pub fn live_sessions(&self) -> Vec<&Session> {
        self.sessions.iter().filter(|s| s.deleted_at.is_none()).collect()
    }

    pub fn folder_by_path(&self, path: &str) -> Option<&Folder> {
        self.folders
            .iter()
            .find(|f| f.path == path && f.deleted_at.is_none())
    }

    pub fn folder_mut(&mut self, id: Uuid) -> Option<&mut Folder> {
        self.folders.iter_mut().find(|f| f.id == id)
    }

    pub fn session(&self, id: Uuid) -> Option<&Session> {
        self.sessions.iter().find(|s| s.id == id)
    }

    pub fn session_mut(&mut self, id: Uuid) -> Option<&mut Session> {
        self.sessions.iter_mut().find(|s| s.id == id)
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

#[derive(Serialize, Deserialize, Debug)]
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
        let path = local_store_path();
        if !path.exists() {
            return Ok(Self::default());
        }
        let raw = std::fs::read_to_string(&path)
            .with_context(|| format!("reading {}", path.display()))?;
        serde_json::from_str(&raw).with_context(|| format!("parsing {}", path.display()))
    }

    pub fn save(&self) -> Result<()> {
        let bytes = serde_json::to_vec_pretty(self)?;
        atomic_write(&local_store_path(), &bytes)
    }

    pub fn live_hosts(&self) -> Vec<&Host> {
        self.hosts.iter().filter(|h| h.deleted_at.is_none()).collect()
    }

    pub fn live_layouts(&self) -> Vec<&Layout> {
        self.layouts.iter().filter(|l| l.deleted_at.is_none()).collect()
    }

    pub fn host_by_name(&self, name: &str) -> Option<&Host> {
        self.hosts
            .iter()
            .find(|h| h.name == name && h.deleted_at.is_none())
    }

    pub fn add_host(&mut self, name: &str, ssh: Option<String>) -> Result<Uuid> {
        if let Some(existing) = self.hosts.iter_mut().find(|h| h.name == name) {
            existing.ssh = ssh;
            existing.deleted_at = None;
            existing.updated_at = now_ms();
            return Ok(existing.id);
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
        if self.hosts.iter().any(|h| h.is_local() && h.deleted_at.is_none()) {
            return false;
        }
        self.hosts.push(Host::new("local".to_string(), None));
        true
    }

    pub fn merge_from(&mut self, other: &LocalStore) {
        self.hosts = merge(&self.hosts, &other.hosts);
        self.layouts = merge(&self.layouts, &other.layouts);
    }
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

        let merged = merge(&[a.clone()], &[b.clone(), other.clone()]);
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
        assert_eq!(first, second, "layouts referencing this folder must survive");
        assert!(s.live_folders().len() == 1);
    }

    #[test]
    fn removing_a_folder_buries_its_sessions() {
        let mut s = HostStore::default();
        let f = s.upsert_folder("/x");
        s.sessions.push(Session::new(f, AgentKind::Claude, "t".into()));
        s.remove_folder(f);
        assert!(s.live_sessions().is_empty());
    }
}
