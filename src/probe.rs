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

    // A probe is the natural moment to reattach session records to the
    // conversation ids their agents ended up creating.
    if tmux::installed()
        && crate::hostops::relink_sessions(&mut store) > 0
        && let Err(e) = store.save()
    {
        warnings.push(format!(
            "could not record resumable conversation ids: {e:#}"
        ));
    }

    let folders: Vec<Folder> = store.live_folders().into_iter().cloned().collect();
    let records: Vec<_> = store.live_sessions().into_iter().cloned().collect();

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

    // Keep the cache from growing without bound as transcripts are deleted.
    let still_there: Vec<String> = index
        .entries
        .keys()
        .filter(|p| std::path::Path::new(p).exists())
        .cloned()
        .collect();
    index.prune(&still_there);
    if let Err(e) = index.save() {
        warnings.push(format!("chat index not saved: {e:#}"));
    }

    let chats = select_chats(&folders, all_chats, &mut warnings);
    let live = live_agents();

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

/// Running agents, with whatever their hooks last reported joined in.
///
/// A hook report is only used when it is *newer* than the agent's own status
/// line. The two can disagree — an agent showing a permission prompt is
/// arguably still mid-turn — and rather than pick a winner by rule, the fresher
/// of the two accounts is taken.
fn live_agents() -> Vec<LiveAgent> {
    let mut live: Vec<LiveAgent> = agent::all()
        .iter()
        .filter(|a| a.installed())
        .flat_map(|a| a.live())
        .collect();

    let marks = crate::attention::read_all();
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
    use crate::model::Folder;

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
        chats.extend((0..10).map(|i| chat("/repo/sub", &format!("c{i}"), 100 + i as u64)));
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
