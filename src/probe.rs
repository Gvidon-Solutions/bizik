//! The host-side collector.
//!
//! Everything a laptop needs to know about one machine is gathered here and
//! returned in a single JSON document, so a dashboard refresh costs one ssh
//! round trip per host rather than one per question.
//!
//! Crucially the answer is *resolved*, not raw. Only this machine can see its
//! own tmux sessions and processes, so only this machine should be deciding
//! what state a session is in — see [`crate::reconcile`].

use crate::agent::{self, ChatIndex};
use crate::model::{AgentKind, Chat, Folder, LiveAgent, PROTOCOL, Probe};
use crate::reconcile::{self, Inputs};
use crate::store::HostStore;
use crate::tmux;
use crate::util::{now_ms, one_line};

/// Most chats reported per marked folder. Old conversations pile up and the
/// full list is neither useful nor cheap to ship over ssh.
const MAX_CHATS_PER_FOLDER: usize = 40;

pub fn collect(with_preview: bool) -> Probe {
    let mut warnings = Vec::new();

    let mut store = match HostStore::load() {
        Ok(s) => s,
        Err(e) => {
            // A broken store must not take the whole host offline: report what
            // can still be seen and say what is wrong.
            warnings.push(format!("host store unreadable: {e:#}"));
            HostStore::default()
        }
    };

    let marks = crate::attention::read_all();
    // A probe is the natural moment to reattach session records to the
    // conversation ids their agents ended up creating. Delay the save until
    // chats have also been scanned, so a newly discovered Codex title lands in
    // the same atomic store update as its thread id.
    let relinked = if tmux::installed() {
        crate::hostops::relink_sessions(&mut store)
            + crate::hostops::relink_sessions_from_hooks(&mut store, &marks)
    } else {
        0
    };

    let mut index = ChatIndex::load();
    let mut all_chats = Vec::new();
    let mut installed = Vec::new();

    for a in agent::all() {
        // A shell is always available and keeps no transcripts.
        if a.kind() == AgentKind::Shell || !a.installed() {
            continue;
        }
        installed.push(a.kind());
        all_chats.extend(a.scan_chats(&mut index, &mut warnings));
    }

    let retitled = synchronize_codex_session_titles(&mut store, &all_chats, &mut index);
    if relinked + retitled > 0
        && let Err(e) = store.save()
    {
        warnings.push(format!(
            "could not record resumable conversation ids or automatic titles: {e:#}"
        ));
    }

    let folders: Vec<Folder> = store.live_folders().into_iter().cloned().collect();
    let records: Vec<_> = store.live_sessions().into_iter().cloned().collect();

    // Keep the cache from growing without bound as transcripts are deleted.
    let still_there: Vec<String> = index
        .entries
        .keys()
        .filter(|p| std::path::Path::new(p).exists())
        .cloned()
        .collect();
    index.prune(&still_there);
    let live_session_ids: std::collections::HashSet<String> = records
        .iter()
        .map(|session| session.id.to_string())
        .collect();
    index
        .automatic_session_titles
        .retain(|id, _| live_session_ids.contains(id));
    if let Err(e) = index.save() {
        warnings.push(format!("chat index not saved: {e:#}"));
    }

    let chats = select_chats(&folders, all_chats, &mut warnings);
    let live = live_agents(&marks);
    let hook_states: std::collections::HashMap<String, String> = marks
        .iter()
        .map(|(key, mark)| (key.clone(), mark.state.as_str().to_string()))
        .collect();

    let tmux_sessions = if tmux::installed() {
        tmux::list_sessions(with_preview)
    } else {
        warnings.push("tmux is not installed on this host — sessions cannot be started".into());
        Vec::new()
    };

    let by_id: std::collections::HashMap<uuid::Uuid, String> =
        folders.iter().map(|f| (f.id, f.path.clone())).collect();
    let (sessions, orphans) = reconcile::reconcile(Inputs {
        sessions: &records,
        folder_path: &|id| by_id.get(&id).cloned(),
        tmux: &tmux_sessions,
        live: &live,
        hook_states: &hook_states,
    });

    if !orphans.is_empty() {
        warnings.push(format!(
            "{} tmux session(s) bizik no longer tracks: {} — close with: tmux kill-session -t <name>",
            orphans.len(),
            orphans
                .iter()
                .map(|o| o.tmux_name.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }

    Probe {
        protocol: PROTOCOL,
        bzk_version: env!("CARGO_PKG_VERSION").to_string(),
        folders,
        sessions,
        chats,
        orphans,
        agents: installed,
        hooks_installed: crate::hooks::all_installed(),
        env_captured_at: crate::hostenv::load().map(|e| e.captured_at),
        warnings,
    }
}

/// Copy Codex's own generated thread names onto sessions that still follow
/// automatic naming.
///
/// A generic legacy title is eligible for its first automatic rename. After
/// that, the disposable chat cache remembers the last title copied. If the
/// session still has that exact value, a later native rename may follow it; if
/// the user changed the title in bizik, the values diverge and synchronization
/// stops. The durable store needs no schema change and manual names always win.
fn synchronize_codex_session_titles(
    store: &mut HostStore,
    chats: &[Chat],
    index: &mut ChatIndex,
) -> usize {
    let native_titles: std::collections::HashMap<&str, String> = chats
        .iter()
        .filter(|chat| chat.agent == AgentKind::Codex)
        .filter_map(|chat| {
            let title = one_line(chat.title.as_deref()?, 160);
            (!title.is_empty()).then_some((chat.id.as_str(), title))
        })
        .collect();

    let candidates: Vec<_> = store
        .live_sessions()
        .iter()
        .filter(|session| session.agent == AgentKind::Codex)
        .filter_map(|session| {
            let agent_id = session.agent_session_id.as_deref()?;
            let native = native_titles.get(agent_id)?;
            let folder_name = store
                .folders
                .iter()
                .find(|folder| folder.id == session.folder_id)
                .map(|folder| folder.display_name())?;
            Some((
                session.id,
                session.title.clone(),
                native.clone(),
                folder_name,
            ))
        })
        .collect();

    let mut changed = 0;
    for (id, current, native, folder_name) in candidates {
        let key = id.to_string();
        let followed_previous = index
            .automatic_session_titles
            .get(&key)
            .is_some_and(|previous| previous == &current);
        let eligible = current == native
            || followed_previous
            || is_legacy_default_title(&current, &folder_name);

        if !eligible {
            // A different value is a manual rename. Forget provenance so a
            // future Codex title change can never overwrite it.
            index.automatic_session_titles.remove(&key);
            continue;
        }

        index.automatic_session_titles.insert(key, native.clone());
        if current != native
            && let Some(session) = store.session_mut(id)
        {
            session.title = native;
            session.updated_at = now_ms();
            changed += 1;
        }
    }
    changed
}

fn is_legacy_default_title(title: &str, folder_name: &str) -> bool {
    let base = format!("codex · {folder_name}");
    if title == base {
        return true;
    }
    title
        .strip_prefix(&format!("{base} "))
        .and_then(|suffix| suffix.parse::<usize>().ok())
        .is_some_and(|number| number >= 2)
}

/// Running agents, with whatever their hooks last reported joined in.
///
/// A hook report is only used when it is *newer* than the agent's own status
/// line. The two can disagree — an agent showing a permission prompt is
/// arguably still mid-turn — and rather than pick a winner by rule, the fresher
/// of the two accounts is taken.
fn live_agents(
    marks: &std::collections::HashMap<String, crate::attention::Mark>,
) -> Vec<LiveAgent> {
    let mut live: Vec<LiveAgent> = agent::all()
        .iter()
        .filter(|a| a.installed())
        .flat_map(|a| a.live())
        .collect();

    for l in &mut live {
        if let Some(id) = &l.agent_session_id
            && let Some(mark) = marks.get(id)
            && mark.at >= l.status_at
        {
            l.attention = Some(mark.state.as_str().to_string());
        }
    }
    live
}

/// Keep only conversations that belong to a marked folder, newest first, and
/// say out loud when the per-folder cap drops any — a silent truncation would
/// read as "this folder has no more history".
fn select_chats(folders: &[Folder], mut chats: Vec<Chat>, warnings: &mut Vec<String>) -> Vec<Chat> {
    // Deduplicate before capping, or the cap silently yields fewer than it
    // promises. One conversation can appear under two project directories when
    // its working directory changed part-way through.
    chats.sort_by(|a, b| a.id.cmp(&b.id).then(a.agent.as_str().cmp(b.agent.as_str())));
    chats.dedup_by(|a, b| a.id == b.id && a.agent == b.agent);
    chats.sort_by_key(|c| std::cmp::Reverse(c.last_active));

    let mut out: Vec<Chat> = Vec::new();
    for folder in folders {
        let matching: Vec<&Chat> = chats
            .iter()
            .filter(|c| under(&c.cwd, &folder.path))
            .collect();
        if matching.len() > MAX_CHATS_PER_FOLDER {
            warnings.push(format!(
                "{}: showing the {} most recent of {} conversations",
                folder.display_name(),
                MAX_CHATS_PER_FOLDER,
                matching.len()
            ));
        }
        for chat in matching.into_iter().take(MAX_CHATS_PER_FOLDER) {
            // Nested marks overlap; a chat under both belongs in the list once.
            if !out.iter().any(|c| c.id == chat.id && c.agent == chat.agent) {
                out.push(chat.clone());
            }
        }
    }
    out.sort_by_key(|c| std::cmp::Reverse(c.last_active));
    out
}

/// Whether `path` is `root` or lives inside it. Compared component-wise so
/// `/repo/app` is not treated as being inside `/repo/ap`.
fn under(path: &str, root: &str) -> bool {
    let p = path.trim_end_matches('/');
    let r = root.trim_end_matches('/');
    p == r || p.strip_prefix(r).is_some_and(|rest| rest.starts_with('/'))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Folder, Session};

    fn chat(cwd: &str, id: &str, when: u64) -> Chat {
        Chat {
            agent: AgentKind::Claude,
            id: id.into(),
            cwd: cwd.into(),
            title: None,
            last_prompt: None,
            git_branch: None,
            last_active: when,
            size: 1,
        }
    }

    fn codex_chat(id: &str, title: &str) -> Chat {
        Chat {
            agent: AgentKind::Codex,
            id: id.into(),
            cwd: "/repo".into(),
            title: Some(title.into()),
            last_prompt: None,
            git_branch: None,
            last_active: 1,
            size: 1,
        }
    }

    fn codex_session_store(title: &str, thread: &str) -> (HostStore, uuid::Uuid) {
        let folder = Folder::new("/repo".into());
        let mut session = Session::new(folder.id, AgentKind::Codex, title.into());
        session.agent_session_id = Some(thread.into());
        let id = session.id;
        let mut store = HostStore::default();
        store.folders.push(folder);
        store.sessions.push(session);
        (store, id)
    }

    #[test]
    fn legacy_codex_titles_follow_native_renames_until_the_user_renames_them() {
        let (mut store, session_id) = codex_session_store("codex · repo 2", "thread");
        let mut index = ChatIndex::default();

        assert_eq!(
            synchronize_codex_session_titles(
                &mut store,
                &[codex_chat("thread", "Fix login flow")],
                &mut index,
            ),
            1
        );
        assert_eq!(store.session(session_id).unwrap().title, "Fix login flow");
        assert_eq!(
            index
                .automatic_session_titles
                .get(&session_id.to_string())
                .map(String::as_str),
            Some("Fix login flow")
        );

        assert_eq!(
            synchronize_codex_session_titles(
                &mut store,
                &[codex_chat("thread", "Repair authentication")],
                &mut index,
            ),
            1
        );
        assert_eq!(
            store.session(session_id).unwrap().title,
            "Repair authentication"
        );

        store.session_mut(session_id).unwrap().title = "my manual name".into();
        assert_eq!(
            synchronize_codex_session_titles(
                &mut store,
                &[codex_chat("thread", "Another native name")],
                &mut index,
            ),
            0
        );
        assert_eq!(store.session(session_id).unwrap().title, "my manual name");
        assert!(
            !index
                .automatic_session_titles
                .contains_key(&session_id.to_string())
        );
    }

    #[test]
    fn a_custom_legacy_codex_title_is_never_replaced() {
        let (mut store, session_id) = codex_session_store("release blocker", "thread");
        let mut index = ChatIndex::default();

        assert_eq!(
            synchronize_codex_session_titles(
                &mut store,
                &[codex_chat("thread", "Generated title")],
                &mut index,
            ),
            0
        );
        assert_eq!(store.session(session_id).unwrap().title, "release blocker");
        assert!(index.automatic_session_titles.is_empty());
    }

    #[test]
    fn an_adopted_codex_title_is_tracked_for_later_native_updates() {
        let (mut store, session_id) = codex_session_store("Existing title", "thread");
        let mut index = ChatIndex::default();

        assert_eq!(
            synchronize_codex_session_titles(
                &mut store,
                &[codex_chat("thread", "Existing title")],
                &mut index,
            ),
            0
        );
        assert_eq!(
            synchronize_codex_session_titles(
                &mut store,
                &[codex_chat("thread", "Updated title")],
                &mut index,
            ),
            1
        );
        assert_eq!(store.session(session_id).unwrap().title, "Updated title");
    }

    #[test]
    fn descendants_count_as_inside_but_prefixes_do_not() {
        assert!(under("/repo", "/repo"));
        assert!(under("/repo/app", "/repo"));
        assert!(under("/repo/", "/repo"));
        assert!(!under("/repository", "/repo"));
        assert!(!under("/other", "/repo"));
    }

    #[test]
    fn chats_outside_marked_folders_are_dropped_and_order_is_newest_first() {
        let folders = vec![Folder::new("/repo".into())];
        let chats = vec![
            chat("/repo", "old", 10),
            chat("/elsewhere", "ignored", 99),
            chat("/repo/sub", "new", 50),
        ];
        let mut w = Vec::new();
        let got = select_chats(&folders, chats, &mut w);
        assert_eq!(
            got.iter().map(|c| c.id.as_str()).collect::<Vec<_>>(),
            ["new", "old"]
        );
        assert!(w.is_empty());
    }

    #[test]
    fn a_chat_under_two_marked_folders_is_reported_once() {
        let folders = vec![Folder::new("/repo".into()), Folder::new("/repo/sub".into())];
        let mut w = Vec::new();
        let got = select_chats(&folders, vec![chat("/repo/sub", "x", 1)], &mut w);
        assert_eq!(got.len(), 1);
    }

    #[test]
    fn truncation_is_announced_rather_than_silent() {
        let folders = vec![Folder::new("/repo".into())];
        let chats: Vec<Chat> = (0..MAX_CHATS_PER_FOLDER + 5)
            .map(|i| chat("/repo", &format!("c{i}"), i as u64))
            .collect();
        let mut w = Vec::new();
        let got = select_chats(&folders, chats, &mut w);
        assert_eq!(got.len(), MAX_CHATS_PER_FOLDER);
        assert_eq!(w.len(), 1, "the user must be told some were dropped");
    }

    #[test]
    fn duplicates_are_removed_before_the_cap_is_applied() {
        let folders = vec![Folder::new("/repo".into())];
        let mut chats: Vec<Chat> = (0..MAX_CHATS_PER_FOLDER)
            .map(|i| chat("/repo", &format!("c{i}"), 100 + i as u64))
            .collect();
        chats.extend((0_u64..10).map(|i| chat("/repo/sub", &format!("c{i}"), 100 + i)));
        let mut w = Vec::new();
        let got = select_chats(&folders, chats, &mut w);
        assert_eq!(
            got.len(),
            MAX_CHATS_PER_FOLDER,
            "a full cap, not a short one"
        );
        assert!(w.is_empty(), "nothing was actually dropped");
    }
}
