//! Reconciling what bizik intends with what is actually running.
//!
//! There are four sources of truth about a session, and they can disagree:
//!
//! * the **record** in this host's store — the intent
//! * the **tmux session** — whether something is running
//! * the **agent process** — whether the thing running is the agent, or the
//!   shell it fell back to when the agent exited
//! * the **hook marker** — whether the agent is blocked on the user
//!
//! Every disagreement between them used to be resolved wherever it happened to
//! be noticed, by whichever join was convenient — the probe matched on names,
//! the dashboard matched on working directories, the pane mapper matched on
//! command-line substrings. Combinations nobody had thought about simply fell
//! through, and one of them was a real defect: a deleted record whose tmux
//! session was still alive showed panes that worked while the dashboard called
//! them missing, and nothing could stop them.
//!
//! So the join happens once, here, over an exhaustive set of states. A new
//! combination becomes a compile error in the `match` rather than a bug report.

use serde::{Deserialize, Serialize};

use crate::model::{AgentKind, LiveAgent, Session, TmuxSession};

/// What is actually true of one session, decided once on the host that owns it.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum State {
    /// Blocked on the user — a permission prompt or a question. Known only
    /// where the agent's hooks are installed.
    NeedsYou,
    /// A turn ended and there is something to look at.
    Done,
    /// The agent is working.
    Working,
    /// Idle, but with no hooks there is no telling whether that means finished
    /// or blocked. Deliberately vaguer than the two above.
    YourTurn,
    /// Running, but the agent exposes no status, or two of them share the
    /// directory and which is which cannot be known from here.
    Up,
    /// The tmux session is alive but the agent is not in it — it exited and
    /// left the fallback shell behind. The pane still holds its scrollback.
    Exited,
    /// Nothing is running. Starting it resumes the conversation.
    Down,
}

impl State {
    pub fn label(self) -> &'static str {
        match self {
            State::NeedsYou => "needs you",
            State::Done => "done",
            State::Working => "working",
            State::YourTurn => "your turn",
            State::Up => "running",
            State::Exited => "exited",
            State::Down => "stopped",
        }
    }

    pub fn glyph(self) -> &'static str {
        match self {
            State::NeedsYou => "▲",
            State::Done => "◆",
            State::Working => "●",
            State::YourTurn => "◇",
            State::Up => "○",
            State::Exited => "✕",
            State::Down => "·",
        }
    }

    /// The ball is in your court.
    pub fn wants_you(self) -> bool {
        matches!(self, State::NeedsYou | State::Done | State::YourTurn)
    }

    pub fn is_running(self) -> bool {
        !matches!(self, State::Down)
    }

    /// Sort key for the mission-control list: whatever is blocked comes first.
    pub fn urgency(self) -> u8 {
        match self {
            State::NeedsYou => 0,
            State::Done => 1,
            State::YourTurn => 2,
            State::Working => 3,
            State::Exited => 4,
            State::Up => 5,
            State::Down => 6,
        }
    }
}

/// A session and everything the host could determine about it, resolved.
///
/// The laptop displays this; it does not recompute it. One place decides, so
/// there is one answer.
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct SessionView {
    pub session: Session,
    pub state: State,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preview: Option<String>,
    /// What the agent's hook last said, when it said anything.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attention: Option<String>,
}

/// A tmux session that looks like ours but answers to no record.
///
/// Left unreported these accumulate invisibly: they cannot be attached,
/// stopped, or reasoned about, and they keep running until the machine
/// reboots. Naming them turns a leak into a chore.
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Orphan {
    pub tmux_name: String,
    /// Set when tmux still carries a bizik uuid — meaning the record was
    /// deleted out from under a running session rather than never existing.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub was_session: Option<String>,
}

pub struct Inputs<'a> {
    pub sessions: &'a [Session],
    pub folder_path: &'a dyn Fn(uuid::Uuid) -> Option<String>,
    pub tmux: &'a [TmuxSession],
    pub live: &'a [LiveAgent],
}

/// Join every source into one answer per session, plus whatever is running
/// that no session claims.
pub fn reconcile(input: Inputs<'_>) -> (Vec<SessionView>, Vec<Orphan>) {
    let mut views = Vec::with_capacity(input.sessions.len());

    for session in input.sessions {
        let name = session.tmux_name();
        let tmux = input.tmux.iter().find(|t| t.name == name);
        let folder = (input.folder_path)(session.folder_id);
        let agent = match folder.as_deref() {
            Some(path) => match_agent(input.live, session, path),
            // Without a folder there is nothing to match a process against, and
            // saying so beats claiming the agent died.
            None => Attribution::Ambiguous,
        };
        let attention = match &agent {
            Attribution::Agent(a) => a.attention.clone(),
            _ => None,
        };

        views.push(SessionView {
            state: decide(tmux.is_some(), &agent, session.agent),
            preview: tmux.and_then(|t| t.preview.clone()),
            attention,
            session: session.clone(),
        });
    }

    let claimed: Vec<String> = input.sessions.iter().map(|s| s.tmux_name()).collect();
    let orphans = input
        .tmux
        .iter()
        .filter(|t| looks_like_ours(t) && !claimed.iter().any(|c| c == &t.name))
        .map(|t| Orphan {
            tmux_name: t.name.clone(),
            was_session: t.owner.clone(),
        })
        .collect();

    (views, orphans)
}

/// Agent sessions always carry the `bzk-` name.
///
/// The owner format can inherit a pane option when it is read while listing a
/// session. The dashboard's viewer pane intentionally carries that option, so
/// treating an owner value alone as proof would report the `bizik` dashboard
/// itself as an orphaned agent session.
fn looks_like_ours(t: &TmuxSession) -> bool {
    t.name.starts_with("bzk-")
}

/// Which running agent belongs to this session.
///
/// The conversation id is exact. Falling back to the working directory is only
/// safe when exactly one agent of that kind is there; with two, which is which
/// is genuinely unknowable from here and guessing would put a confident wrong
/// answer on the screen.
fn match_agent<'a>(live: &'a [LiveAgent], session: &Session, folder: &str) -> Attribution<'a> {
    if let Some(id) = &session.agent_session_id
        && let Some(exact) = live
            .iter()
            .find(|l| l.agent == session.agent && l.agent_session_id.as_ref() == Some(id))
    {
        return Attribution::Agent(exact);
    }
    let mut here = live
        .iter()
        .filter(|l| l.agent == session.agent && l.cwd == folder);
    match (here.next(), here.next()) {
        (Some(only), None) => Attribution::Agent(only),
        (Some(_), Some(_)) => Attribution::Ambiguous,
        _ => Attribution::Absent,
    }
}

/// The outcome of trying to pair a session with a running process.
///
/// Three outcomes, not two. "No agent found" and "several agents, none of them
/// identifiable as this one" look identical to an `Option` and mean opposite
/// things: the first is a crashed session, the second a perfectly healthy one
/// we simply cannot say much about. Collapsing them reported working sessions
/// as dead — which a test caught the moment the states became exhaustive.
enum Attribution<'a> {
    Agent(&'a LiveAgent),
    Ambiguous,
    Absent,
}

/// The whole decision, in one exhaustive place.
fn decide(tmux_alive: bool, agent: &Attribution<'_>, kind: AgentKind) -> State {
    match (tmux_alive, agent) {
        (false, _) => State::Down,
        // A plain shell has no agent process by design — the shell *is* what is
        // running. Only a session that was supposed to hold an agent can be
        // said to have lost it.
        (true, Attribution::Absent) if kind == AgentKind::Shell => State::Up,
        // Alive with no agent process anywhere in the folder: the agent exited
        // and the wrapper left a shell behind. Reporting this as "running" is
        // how a crashed session used to hide in plain sight.
        (true, Attribution::Absent) => State::Exited,
        // Running, but which process is this session's cannot be known.
        (true, Attribution::Ambiguous) => State::Up,
        (true, Attribution::Agent(a)) => match (a.attention.as_deref(), a.status.as_str()) {
            // `attention` reaches us only when the hook was the fresher report,
            // so where it exists it is the better account.
            (Some("waiting"), _) => State::NeedsYou,
            (_, "busy") => State::Working,
            (Some("done"), "idle") => State::Done,
            (_, "idle") => State::YourTurn,
            _ => State::Up,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use uuid::Uuid;

    fn session(agent: AgentKind, chat: Option<&str>) -> Session {
        let mut s = Session::new(Uuid::new_v4(), agent, "t".into());
        s.agent_session_id = chat.map(str::to_string);
        s
    }

    fn tmux_for(s: &Session) -> TmuxSession {
        TmuxSession {
            name: s.tmux_name(),
            created: 0,
            attached: false,
            windows: 1,
            preview: None,
            owner: Some(s.id.to_string()),
        }
    }

    fn agent(kind: AgentKind, cwd: &str, chat: Option<&str>, status: &str) -> LiveAgent {
        LiveAgent {
            agent: kind,
            pid: 1,
            cwd: cwd.into(),
            agent_session_id: chat.map(str::to_string),
            status: status.into(),
            status_at: 0,
            attention: None,
        }
    }

    fn run(
        sessions: &[Session],
        tmux: &[TmuxSession],
        live: &[LiveAgent],
    ) -> (Vec<SessionView>, Vec<Orphan>) {
        reconcile(Inputs {
            sessions,
            folder_path: &|_| Some("/repo".to_string()),
            tmux,
            live,
        })
    }

    #[test]
    fn no_tmux_session_is_stopped() {
        let s = session(AgentKind::Claude, None);
        let (views, _) = run(std::slice::from_ref(&s), &[], &[]);
        assert_eq!(views[0].state, State::Down);
    }

    #[test]
    fn a_shell_session_is_running_not_exited() {
        // A shell has no agent behind it and never will; calling that "exited"
        // marks every healthy terminal as broken.
        let s = session(AgentKind::Shell, None);
        let (views, _) = run(std::slice::from_ref(&s), &[tmux_for(&s)], &[]);
        assert_eq!(views[0].state, State::Up);
    }

    #[test]
    fn a_live_tmux_with_no_agent_in_it_is_exited_not_running() {
        // The case that used to hide: the agent crashed, the wrapper left a
        // shell, and the row claimed everything was fine.
        let s = session(AgentKind::Claude, None);
        let (views, _) = run(std::slice::from_ref(&s), &[tmux_for(&s)], &[]);
        assert_eq!(views[0].state, State::Exited);
    }

    #[test]
    fn hooks_split_idle_into_blocked_and_finished() {
        let s = session(AgentKind::Claude, Some("abc"));
        let base = agent(AgentKind::Claude, "/repo", Some("abc"), "idle");

        let (vague, _) = run(
            std::slice::from_ref(&s),
            &[tmux_for(&s)],
            std::slice::from_ref(&base),
        );
        assert_eq!(vague[0].state, State::YourTurn);

        let mut waiting = base.clone();
        waiting.attention = Some("waiting".into());
        let (blocked, _) = run(std::slice::from_ref(&s), &[tmux_for(&s)], &[waiting]);
        assert_eq!(blocked[0].state, State::NeedsYou);

        let mut done = base;
        done.attention = Some("done".into());
        let (finished, _) = run(std::slice::from_ref(&s), &[tmux_for(&s)], &[done]);
        assert_eq!(finished[0].state, State::Done);
    }

    #[test]
    fn a_blocked_agent_outranks_a_busy_status_line() {
        let s = session(AgentKind::Claude, Some("abc"));
        let mut busy = agent(AgentKind::Claude, "/repo", Some("abc"), "busy");
        busy.attention = Some("waiting".into());
        let (views, _) = run(std::slice::from_ref(&s), &[tmux_for(&s)], &[busy]);
        assert_eq!(views[0].state, State::NeedsYou);
    }

    #[test]
    fn two_agents_in_one_folder_are_not_guessed_apart() {
        let s = session(AgentKind::Claude, None);
        let live = [
            agent(AgentKind::Claude, "/repo", None, "busy"),
            agent(AgentKind::Claude, "/repo", None, "idle"),
        ];
        let (views, _) = run(std::slice::from_ref(&s), &[tmux_for(&s)], &live);
        assert_eq!(views[0].state, State::Up, "ambiguity must not be resolved");
    }

    #[test]
    fn a_lone_agent_in_the_folder_is_matched_by_directory() {
        let s = session(AgentKind::Claude, None);
        let live = [agent(AgentKind::Claude, "/repo", None, "busy")];
        let (views, _) = run(std::slice::from_ref(&s), &[tmux_for(&s)], &live);
        assert_eq!(views[0].state, State::Working);
    }

    #[test]
    fn a_tmux_session_no_record_claims_is_reported_as_an_orphan() {
        // Exactly the shape of the defect: a record deleted while its tmux
        // session kept running. Silence here is what made it baffling.
        let gone = session(AgentKind::Claude, None);
        let (views, orphans) = run(&[], &[tmux_for(&gone)], &[]);
        assert!(views.is_empty());
        assert_eq!(orphans.len(), 1);
        assert_eq!(orphans[0].tmux_name, gone.tmux_name());
        assert_eq!(
            orphans[0].was_session.as_deref(),
            Some(gone.id.to_string()).as_deref()
        );
    }

    #[test]
    fn somebody_elses_tmux_sessions_are_not_ours_to_report() {
        let theirs = TmuxSession {
            name: "work".into(),
            created: 0,
            attached: true,
            windows: 3,
            preview: None,
            owner: None,
        };
        let (_, orphans) = run(&[], &[theirs], &[]);
        assert!(orphans.is_empty());
    }

    #[test]
    fn the_dashboard_is_not_an_orphan_when_its_viewer_pane_exposes_an_owner() {
        let dashboard = TmuxSession {
            name: "bizik".into(),
            created: 0,
            attached: true,
            windows: 2,
            preview: None,
            owner: Some(uuid::Uuid::new_v4().to_string()),
        };
        let (_, orphans) = run(&[], &[dashboard], &[]);
        assert!(orphans.is_empty());
    }

    #[test]
    fn urgency_puts_blocked_first_and_stopped_last() {
        let mut order = [
            State::Up,
            State::Down,
            State::NeedsYou,
            State::Working,
            State::Done,
            State::Exited,
            State::YourTurn,
        ];
        order.sort_by_key(|s| s.urgency());
        assert_eq!(order[0], State::NeedsYou);
        assert_eq!(order[1], State::Done);
        assert_eq!(*order.last().unwrap(), State::Down);
    }
}
