//! The seam between bizik and tmux, exercised for real.
//!
//! Every test here corresponds to something that actually broke and was found
//! by hand. That is the point of the file: to make the next one of those a
//! failing test rather than a confusing screenshot.

mod harness;
use harness::Sandbox;

#[test]
fn a_marked_folder_survives_a_round_trip_through_the_store() {
    let s = Sandbox::new("mark");
    let dir = s.dir("project");
    let folder = s.mark(&dir);

    let probe = s.probe();
    let folders = probe["folders"].as_array().unwrap();
    assert_eq!(folders.len(), 1);
    assert_eq!(folders[0]["id"], folder);
    assert_eq!(folders[0]["path"], dir.to_string_lossy().as_ref());
}

#[test]
fn spawning_starts_a_tmux_session_and_stopping_ends_it() {
    let s = Sandbox::new("lifecycle");
    let folder = s.mark(&s.dir("project"));
    let session = s.new_session(&folder, "one");
    let name = Sandbox::tmux_name(&session);

    assert_eq!(
        s.probe()["sessions"][0]["state"],
        "down",
        "a session that was never started is stopped"
    );

    s.spawn(&session).ok();
    s.eventually("the tmux session to exist", || {
        s.tmux_sessions().contains(&name)
    });

    s.bzk(&["stop", "--session", &session]).ok();
    assert!(
        !s.tmux_sessions().contains(&name),
        "stopping must actually kill it, not merely forget it"
    );
    assert_eq!(s.probe()["sessions"][0]["state"], "down");
}

#[test]
fn spawning_twice_does_not_start_a_second_copy() {
    let s = Sandbox::new("idempotent");
    let folder = s.mark(&s.dir("project"));
    let session = s.new_session(&folder, "one");

    s.spawn(&session).ok();
    s.eventually("the session to start", || s.tmux_sessions().len() == 1);
    s.spawn(&session).ok();

    assert_eq!(
        s.tmux_sessions().len(),
        1,
        "restoring a layout must reattach to work in progress, not duplicate it"
    );
}

#[test]
fn a_forgotten_session_cannot_be_started_again() {
    // The defect: `spawn` looked sessions up without checking for a tombstone,
    // so a layout referring to deleted sessions quietly brought them back —
    // running, but invisible to everything that lists live sessions.
    let s = Sandbox::new("tombstone");
    let folder = s.mark(&s.dir("project"));
    let session = s.new_session(&folder, "one");

    s.bzk(&["rm-session", "--session", &session]).ok();
    let out = s.spawn(&session);

    out.failed();
    assert!(
        out.stderr.contains("no session"),
        "and it must say why: {}",
        out.stderr
    );
}

#[test]
fn a_tmux_session_no_record_claims_is_reported_as_an_orphan() {
    // The same defect seen from the other side: a record deleted while its
    // tmux session kept running. Silence here is what made it baffling.
    let s = Sandbox::new("orphan");
    let folder = s.mark(&s.dir("project"));
    let session = s.new_session(&folder, "one");
    s.spawn(&session).ok();
    s.eventually("the session to start", || s.tmux_sessions().len() == 1);

    // Bury the record without touching tmux, exactly as the accident did.
    let mut store = s.host_store();
    store["sessions"][0]["deleted_at"] = serde_json::json!(1);
    s.write_host_store(&store);

    let probe = s.probe();
    let orphans = probe["orphans"].as_array().unwrap();
    assert_eq!(orphans.len(), 1, "an untracked session must not be silent");
    assert_eq!(orphans[0]["tmux_name"], Sandbox::tmux_name(&session));
    assert!(
        probe["warnings"]
            .as_array()
            .unwrap()
            .iter()
            .any(|w| w.as_str().unwrap().contains("no longer tracks")),
        "and it must be said in words too"
    );
}

#[test]
fn somebody_elses_tmux_sessions_are_left_alone() {
    let s = Sandbox::new("neighbour");
    s.mark(&s.dir("project"));
    s.tmux(&["new-session", "-d", "-s", "someones-work", "sleep 30"]);

    let probe = s.probe();
    assert!(
        probe["orphans"].as_array().unwrap().is_empty(),
        "a session bizik never created is none of its business"
    );
    assert!(s.tmux_sessions().contains(&"someones-work".to_string()));
}

#[test]
fn a_session_carries_its_identity_on_the_tmux_session_itself() {
    // Identity used to be recovered by matching substrings of a command line.
    // Reading it back from tmux is what makes pane mapping exact.
    let s = Sandbox::new("identity");
    let folder = s.mark(&s.dir("project"));
    let session = s.new_session(&folder, "one");
    s.spawn(&session).ok();
    s.eventually("the session to start", || s.tmux_sessions().len() == 1);

    let tagged = s.tmux(&["list-sessions", "-F", "#{session_name}\t#{@bzk_session}"]);
    assert!(
        tagged.contains(&session),
        "tmux should be carrying the uuid: {tagged}"
    );
}

#[test]
fn an_agent_that_exits_leaves_a_session_that_reads_as_exited() {
    // A crashed agent used to be indistinguishable from a working one: the
    // tmux session is alive either way, and only the process tells them apart.
    let s = Sandbox::new("exited");
    let folder = s.mark(&s.dir("project"));
    let session = s.new_session_of(&folder, "claude", "one");

    // Exactly what the wrapper leaves behind when an agent exits: the session
    // is up, but what is in it is a shell, not claude.
    s.tmux(&[
        "new-session",
        "-d",
        "-s",
        &Sandbox::tmux_name(&session),
        "sleep 60",
    ]);

    let probe = s.probe();
    assert_eq!(
        probe["sessions"][0]["state"], "exited",
        "alive but agent-less is its own state, not 'running'"
    );
}

#[test]
fn a_shell_session_reads_as_running_rather_than_exited() {
    // A shell has no agent process by design. Calling that "exited" would mark
    // every healthy terminal as broken.
    let s = Sandbox::new("shell-running");
    let folder = s.mark(&s.dir("project"));
    let session = s.new_session(&folder, "one");
    s.spawn(&session).ok();
    s.eventually("the session to start", || s.tmux_sessions().len() == 1);

    assert_eq!(s.probe()["sessions"][0]["state"], "up");
}

#[test]
fn several_sessions_in_one_folder_get_distinguishable_names() {
    let s = Sandbox::new("titles");
    let folder = s.mark(&s.dir("project"));

    let titles: Vec<String> = (0..3)
        .map(|_| {
            let out = s.bzk(&["new-session", "--folder", &folder, "--agent", "shell"]);
            out.ok();
            let parsed: serde_json::Value = serde_json::from_str(out.stdout.trim()).unwrap();
            parsed["title"].as_str().unwrap().to_string()
        })
        .collect();

    let unique: std::collections::HashSet<&String> = titles.iter().collect();
    assert_eq!(
        unique.len(),
        titles.len(),
        "four rows reading the same thing make the list useless: {titles:?}"
    );
}

#[test]
fn removing_a_folder_stops_being_able_to_start_its_sessions() {
    let s = Sandbox::new("unmark");
    let dir = s.dir("project");
    let folder = s.mark(&dir);
    let session = s.new_session(&folder, "one");

    s.bzk_in(&dir, &["unmark"]).ok();

    s.spawn(&session).failed();
    let probe = s.probe();
    assert!(probe["folders"].as_array().unwrap().is_empty());
    assert!(
        probe["sessions"].as_array().unwrap().is_empty(),
        "a session cannot outlive the folder it belongs to"
    );
}

#[test]
fn the_probe_announces_which_protocol_it_speaks() {
    let s = Sandbox::new("protocol");
    let probe = s.probe();
    assert_eq!(
        probe["protocol"], 1,
        "a laptop must be able to tell a stale host from a broken one"
    );
}

#[test]
fn a_broken_store_degrades_instead_of_taking_the_host_down() {
    let s = Sandbox::new("broken-store");
    s.mark(&s.dir("project"));
    std::fs::write(s.root.join("config/host.json"), "{ not json").unwrap();

    let probe = s.probe();
    assert!(
        probe["warnings"]
            .as_array()
            .unwrap()
            .iter()
            .any(|w| w.as_str().unwrap().contains("unreadable")),
        "the problem must be named, not swallowed"
    );
    assert!(probe["folders"].as_array().unwrap().is_empty());
}

#[test]
fn mark_rejects_a_file_without_mutating_the_store() {
    let s = Sandbox::new("mark-file");
    let file = s.root.join("not-a-directory");
    std::fs::write(&file, "content").unwrap();

    let out = s.bzk(&["mark", file.to_str().unwrap()]);

    out.failed();
    assert!(out.stderr.contains("not a directory"), "{}", out.stderr);
    assert!(
        !s.root.join("config/host.json").exists(),
        "validation must happen before persistence"
    );
}

#[test]
fn local_host_cannot_be_reinterpreted_as_a_remote_host() {
    let s = Sandbox::new("host-kind");
    s.bzk(&["host", "ls"]).ok();

    let out = s.bzk(&["host", "add", "local", "remote.example"]);

    out.failed();
    assert!(out.stderr.contains("cannot change"), "{}", out.stderr);
    let listed = s.bzk(&["host", "ls"]);
    listed.ok();
    assert_eq!(listed.stdout.lines().count(), 1);
    assert!(listed.stdout.contains("(this machine)"));
}

#[test]
fn unmarking_a_folder_stops_what_it_had_running() {
    // Tombstoning a session without stopping it strands the tmux session:
    // nothing can attach to it or stop it afterwards, because the record that
    // named it is gone. `rm-session` already refuses to do that; unmarking a
    // whole folder tombstones every session in it at once and owed the same
    // guarantee.
    let s = Sandbox::new("unmark-stops");
    let dir = s.dir("project");
    let folder = s.mark(&dir);
    let session = s.new_session(&folder, "one");
    s.spawn(&session).ok();
    s.eventually("the session to start", || s.tmux_sessions().len() == 1);

    s.bzk_in(&dir, &["unmark"]).ok();

    assert!(
        s.tmux_sessions().is_empty(),
        "unmarking must not leave a session nothing can manage: {:?}",
        s.tmux_sessions()
    );
    assert!(
        s.probe()["orphans"].as_array().unwrap().is_empty(),
        "and so must not manufacture an orphan"
    );
}

#[test]
fn a_folder_that_no_longer_exists_can_still_be_unmarked() {
    // The case where unmarking matters most. Requiring the directory to still
    // be there left the entry unremovable by any means short of editing the
    // store by hand.
    let s = Sandbox::new("unmark-gone");
    let dir = s.dir("doomed");
    s.mark(&dir);
    std::fs::remove_dir_all(&dir).unwrap();

    let out = s.bzk(&["unmark", dir.to_str().unwrap()]);
    out.ok();
    assert!(
        s.probe()["folders"].as_array().unwrap().is_empty(),
        "the entry has to go even though its directory did first"
    );
}
