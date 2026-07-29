//! Application use cases.
//!
//! This layer coordinates domain records through repository ports. It knows
//! neither CLI syntax nor terminal output, and tests can replace filesystem
//! repositories with in-memory ones.

use anyhow::{Context, Result, bail};
use std::path::PathBuf;

use crate::model::{AgentKind, Folder, Host, Session};
use crate::store::{HostStore, LocalStore, host_store_path, local_store_path};
use crate::util::now_ms;

pub trait HostStateRepository {
    fn load(&self) -> Result<HostStore>;
    fn save(&self, store: &HostStore) -> Result<()>;
}

pub trait LocalStateRepository {
    fn load(&self) -> Result<LocalStore>;
    fn save(&self, store: &LocalStore) -> Result<()>;
}

/// Runtime boundary needed when a session record is forgotten.
pub trait SessionTerminator {
    fn stop(&self, session: &Session) -> Result<bool>;
}

#[derive(Clone, Debug)]
pub struct FsHostStateRepository {
    path: PathBuf,
}

impl FsHostStateRepository {
    #[cfg(test)]
    fn at(path: PathBuf) -> Self {
        Self { path }
    }
}

impl Default for FsHostStateRepository {
    fn default() -> Self {
        Self {
            path: host_store_path(),
        }
    }
}

impl HostStateRepository for FsHostStateRepository {
    fn load(&self) -> Result<HostStore> {
        HostStore::load_from(&self.path)
    }

    fn save(&self, store: &HostStore) -> Result<()> {
        store.save_to(&self.path)
    }
}

#[derive(Clone, Debug)]
pub struct FsLocalStateRepository {
    path: PathBuf,
}

impl FsLocalStateRepository {
    #[cfg(test)]
    fn at(path: PathBuf) -> Self {
        Self { path }
    }
}

impl Default for FsLocalStateRepository {
    fn default() -> Self {
        Self {
            path: local_store_path(),
        }
    }
}

impl LocalStateRepository for FsLocalStateRepository {
    fn load(&self) -> Result<LocalStore> {
        LocalStore::load_from(&self.path)
    }

    fn save(&self, store: &LocalStore) -> Result<()> {
        store.save_to(&self.path)
    }
}

#[derive(Clone, Debug)]
pub struct MarkFolder {
    pub path: String,
    pub label: Option<String>,
    pub git_remote: Option<String>,
    pub git_branch: Option<String>,
}

pub fn mark_folder(repository: &impl HostStateRepository, request: MarkFolder) -> Result<Folder> {
    let mut store = repository.load()?;
    let id = store.upsert_folder(&request.path);
    let folder = store
        .folder_mut(id)
        .context("the folder inserted into the store disappeared")?;
    if request.label.is_some() {
        folder.label = request.label;
    }
    folder.git_remote = request.git_remote;
    folder.git_branch = request.git_branch;
    folder.updated_at = now_ms();
    let result = folder.clone();
    repository.save(&store)?;
    Ok(result)
}

/// Unmark a folder, stopping whatever it still has running.
///
/// `remove_folder` tombstones the folder's sessions, and a tombstoned session
/// is one nothing can attach to or stop — so leaving their tmux sessions alive
/// would strand them the moment the record went away. `forget_session` already
/// refuses to tombstone a session it could not stop; unmarking a whole folder
/// owes the same guarantee, since it tombstones every session in it at once.
pub fn unmark_folder(
    repository: &impl HostStateRepository,
    terminator: &impl SessionTerminator,
    path: &str,
) -> Result<Folder> {
    let mut store = repository.load()?;
    let folder = store
        .folder_by_path(path)
        .cloned()
        .with_context(|| format!("{path} is not marked"))?;

    let running: Vec<Session> = store
        .live_sessions()
        .into_iter()
        .filter(|session| session.folder_id == folder.id)
        .cloned()
        .collect();
    // Stop everything before touching the store: a failure here must leave the
    // folder marked rather than half-removed.
    for session in &running {
        terminator
            .stop(session)
            .with_context(|| format!("stopping {} before unmarking", session.title))?;
    }

    if !store.remove_folder(folder.id) {
        bail!("folder {} disappeared while removing it", folder.id);
    }
    repository.save(&store)?;
    Ok(folder)
}

pub fn marked_folders(repository: &impl HostStateRepository) -> Result<Vec<Folder>> {
    Ok(repository
        .load()?
        .live_folders()
        .into_iter()
        .cloned()
        .collect())
}

#[derive(Clone, Debug)]
pub struct CreateSession {
    pub folder: uuid::Uuid,
    pub agent: AgentKind,
    pub title: Option<String>,
    pub resume: Option<String>,
}

pub fn create_session(
    repository: &impl HostStateRepository,
    request: CreateSession,
) -> Result<Session> {
    let mut store = repository.load()?;
    let folder = store
        .folders
        .iter()
        .find(|folder| folder.id == request.folder && folder.deleted_at.is_none())
        .with_context(|| format!("no marked folder {} on this host", request.folder))?;

    let base = format!("{} · {}", request.agent, folder.display_name());
    let title = request
        .title
        .unwrap_or_else(|| unique_session_title(&store, request.folder, &base));
    let mut session = Session::new(request.folder, request.agent, title);
    session.agent_session_id = request.resume;
    store.sessions.push(session.clone());
    repository.save(&store)?;
    Ok(session)
}

pub fn forget_session(
    repository: &impl HostStateRepository,
    terminator: &impl SessionTerminator,
    id: uuid::Uuid,
) -> Result<bool> {
    let mut store = repository.load()?;
    let Some(session) = store.session(id).cloned() else {
        return Ok(false);
    };

    // A failed stop must abort the transaction. Tombstoning the record anyway
    // would leave a running tmux session with no live identity to manage it.
    terminator.stop(&session)?;
    if !store.remove_session(id) {
        bail!("session {id} disappeared while removing it");
    }
    repository.save(&store)?;
    Ok(true)
}

pub fn rename_session(
    repository: &impl HostStateRepository,
    id: uuid::Uuid,
    title: &str,
) -> Result<Session> {
    let mut store = repository.load()?;
    let session = store
        .session_mut(id)
        .with_context(|| format!("no session {id} on this host"))?;
    session.title = title.trim().to_string();
    session.updated_at = now_ms();
    let renamed = session.clone();
    // Saving validates the title length, emptiness and control characters
    // before replacing the on-disk store.
    repository.save(&store)?;
    Ok(renamed)
}

fn unique_session_title(store: &HostStore, folder: uuid::Uuid, base: &str) -> String {
    let taken: Vec<&str> = store
        .live_sessions()
        .iter()
        .filter(|session| session.folder_id == folder)
        .map(|session| session.title.as_str())
        .collect();
    if !taken.contains(&base) {
        return base.to_string();
    }

    (2..)
        .map(|number| format!("{base} {number}"))
        .find(|candidate| !taken.contains(&candidate.as_str()))
        .unwrap_or_else(|| base.to_string())
}

pub fn add_host(
    repository: &impl LocalStateRepository,
    name: &str,
    ssh: Option<String>,
) -> Result<Host> {
    let mut store = repository.load()?;
    let id = store.add_host(name, ssh)?;
    let host = store
        .hosts
        .iter()
        .find(|host| host.id == id)
        .cloned()
        .context("the host inserted into the store disappeared")?;
    repository.save(&store)?;
    Ok(host)
}

pub fn remove_host(repository: &impl LocalStateRepository, name: &str) -> Result<Host> {
    let mut store = repository.load()?;
    let host = store
        .host_by_name(name)
        .cloned()
        .with_context(|| format!("no host named {name}"))?;
    if !store.remove_host(name) {
        bail!("host {} disappeared while removing it", host.id);
    }
    repository.save(&store)?;
    Ok(host)
}

pub fn configured_hosts(repository: &impl LocalStateRepository) -> Result<Vec<Host>> {
    let mut store = repository.load()?;
    if store.ensure_local_host() {
        repository.save(&store)?;
    }
    Ok(store.live_hosts().into_iter().cloned().collect())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ImportSummary {
    pub primary: usize,
    pub secondary: usize,
}

pub fn import_host_state(
    repository: &impl HostStateRepository,
    incoming: &HostStore,
) -> Result<ImportSummary> {
    incoming.validate()?;
    let mut store = repository.load()?;
    store.merge_from(incoming);
    repository.save(&store)?;
    Ok(ImportSummary {
        primary: store.live_folders().len(),
        secondary: store.live_sessions().len(),
    })
}

pub fn import_local_state(
    repository: &impl LocalStateRepository,
    incoming: &LocalStore,
) -> Result<ImportSummary> {
    incoming.validate()?;
    let mut store = repository.load()?;
    store.merge_from(incoming);
    repository.save(&store)?;
    Ok(ImportSummary {
        primary: store.live_hosts().len(),
        secondary: store.live_layouts().len(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;
    use uuid::Uuid;

    fn temp_path(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "bzk-application-{name}-{}-{}",
            std::process::id(),
            Uuid::new_v4()
        ))
    }

    fn cleanup(path: &std::path::Path) {
        std::fs::remove_file(path).ok();
        std::fs::remove_file(path.with_extension("lock")).ok();
    }

    #[test]
    fn folder_use_cases_preserve_identity_and_optional_label() {
        let path = temp_path("folders");
        let repository = FsHostStateRepository::at(path.clone());
        let first = mark_folder(
            &repository,
            MarkFolder {
                path: "/repo".into(),
                label: Some("backend".into()),
                git_remote: Some("git@example/repo".into()),
                git_branch: Some("main".into()),
            },
        )
        .unwrap();

        let second = mark_folder(
            &repository,
            MarkFolder {
                path: "/repo".into(),
                label: None,
                git_remote: None,
                git_branch: Some("feature".into()),
            },
        )
        .unwrap();

        assert_eq!(first.id, second.id);
        assert_eq!(second.label.as_deref(), Some("backend"));
        assert_eq!(second.git_branch.as_deref(), Some("feature"));
        assert_eq!(marked_folders(&repository).unwrap().len(), 1);

        let terminator = FakeTerminator {
            calls: Cell::new(0),
            fail: false,
        };
        unmark_folder(&repository, &terminator, "/repo").unwrap();
        assert!(marked_folders(&repository).unwrap().is_empty());
        cleanup(&path);
    }

    #[test]
    fn host_use_cases_keep_exactly_one_local_host() {
        let path = temp_path("hosts");
        let repository = FsLocalStateRepository::at(path.clone());

        let hosts = configured_hosts(&repository).unwrap();
        assert_eq!(hosts.iter().filter(|host| host.is_local()).count(), 1);

        add_host(&repository, "server", Some("root@example".into())).unwrap();
        let hosts = configured_hosts(&repository).unwrap();
        assert_eq!(hosts.len(), 2);
        assert_eq!(hosts.iter().filter(|host| host.is_local()).count(), 1);

        remove_host(&repository, "server").unwrap();
        assert_eq!(configured_hosts(&repository).unwrap().len(), 1);
        cleanup(&path);
    }

    struct FakeTerminator {
        calls: Cell<usize>,
        fail: bool,
    }

    impl SessionTerminator for FakeTerminator {
        fn stop(&self, _session: &Session) -> Result<bool> {
            self.calls.set(self.calls.get() + 1);
            if self.fail {
                bail!("tmux refused");
            }
            Ok(true)
        }
    }

    #[test]
    fn unmarking_stops_live_sessions_and_aborts_if_one_will_not_stop() {
        let path = temp_path("unmark-stops");
        let repository = FsHostStateRepository::at(path.clone());
        let folder = mark_folder(
            &repository,
            MarkFolder {
                path: "/repo".into(),
                label: None,
                git_remote: None,
                git_branch: None,
            },
        )
        .unwrap();
        for _ in 0..2 {
            create_session(
                &repository,
                CreateSession {
                    folder: folder.id,
                    agent: AgentKind::Shell,
                    title: None,
                    resume: None,
                },
            )
            .unwrap();
        }

        // A refusal must leave the folder marked: half-removing it would strand
        // sessions that are still running.
        let refuses = FakeTerminator {
            calls: Cell::new(0),
            fail: true,
        };
        assert!(unmark_folder(&repository, &refuses, "/repo").is_err());
        assert_eq!(marked_folders(&repository).unwrap().len(), 1);

        let accepts = FakeTerminator {
            calls: Cell::new(0),
            fail: false,
        };
        unmark_folder(&repository, &accepts, "/repo").unwrap();
        assert_eq!(accepts.calls.get(), 2, "every live session must be stopped");
        assert!(marked_folders(&repository).unwrap().is_empty());
        cleanup(&path);
    }

    #[test]
    fn session_use_cases_number_titles_and_stop_before_forgetting() {
        let path = temp_path("sessions");
        let repository = FsHostStateRepository::at(path.clone());
        let folder = mark_folder(
            &repository,
            MarkFolder {
                path: "/repo".into(),
                label: None,
                git_remote: None,
                git_branch: None,
            },
        )
        .unwrap();

        let create = || {
            create_session(
                &repository,
                CreateSession {
                    folder: folder.id,
                    agent: AgentKind::Shell,
                    title: None,
                    resume: None,
                },
            )
            .unwrap()
        };
        assert_eq!(create().title, "shell · repo");
        let second = create();
        assert_eq!(second.title, "shell · repo 2");

        let renamed = rename_session(&repository, second.id, "  build API  ").unwrap();
        assert_eq!(renamed.title, "build API");
        assert_eq!(
            repository.load().unwrap().session(second.id).unwrap().title,
            "build API"
        );
        assert!(rename_session(&repository, second.id, "   ").is_err());

        let terminator = FakeTerminator {
            calls: Cell::new(0),
            fail: false,
        };
        assert!(forget_session(&repository, &terminator, second.id).unwrap());
        assert_eq!(terminator.calls.get(), 1);
        assert!(repository.load().unwrap().session(second.id).is_none());
        cleanup(&path);
    }

    #[test]
    fn failed_stop_keeps_the_session_record_live() {
        let path = temp_path("failed-stop");
        let repository = FsHostStateRepository::at(path.clone());
        let folder = mark_folder(
            &repository,
            MarkFolder {
                path: "/repo".into(),
                label: None,
                git_remote: None,
                git_branch: None,
            },
        )
        .unwrap();
        let session = create_session(
            &repository,
            CreateSession {
                folder: folder.id,
                agent: AgentKind::Shell,
                title: Some("important".into()),
                resume: None,
            },
        )
        .unwrap();
        let terminator = FakeTerminator {
            calls: Cell::new(0),
            fail: true,
        };

        assert!(forget_session(&repository, &terminator, session.id).is_err());
        assert!(repository.load().unwrap().session(session.id).is_some());
        cleanup(&path);
    }

    #[test]
    fn imports_validate_versions_before_touching_the_repository() {
        let path = temp_path("import");
        let repository = FsHostStateRepository::at(path.clone());
        let incoming = HostStore {
            version: 99,
            ..HostStore::default()
        };

        assert!(import_host_state(&repository, &incoming).is_err());
        assert!(!path.exists(), "an invalid import must not create a store");
    }
}
