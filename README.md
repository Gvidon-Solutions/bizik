# bizik

Mark folders on the machines you work on. From one screen, launch Claude Code or
Codex sessions in any of them, switch between them from a project sidebar, and
see which one is waiting on you.

The module boundaries, persistence invariants, quality gates, and current
refactoring roadmap are documented in [ARCHITECTURE.md](ARCHITECTURE.md).

```
 bizik  folders  running  layouts  hosts
▌▲ needs you   gvidon   claude   payments API        Should I drop the old column?
 ◆ done        hetzner  claude   frontend rewrite    All 48 tests pass.
 ● working     hetzner  claude   docs sweep          Editing src/App.tsx…
 ○ running     hetzner  codex    migration script    running tests…
 · stopped     local    claude   notes
```

`▲` is the one that matters: that session is blocked on a question and will wait
forever. Without it, launching things in the background is a way of quietly
accumulating stuck work.

## Why it is built this way

**Agents run on their host, not on your laptop.** Each session is a detached
tmux session on the machine that owns the code. Your laptop only ever runs a
viewer. Close the lid, lose the wifi, reboot — the work carries on, and a second
laptop is just another viewer.

**tmux does the multiplexing.** bizik decides what to start and where to put it;
tmux draws the sidebar and active viewer. An agent's own full-screen interface keeps working
properly — scrollback, mouse, resize, copy mode — because nothing reimplements a
terminal.

**Folders and sessions live on their host.** `~/.config/bizik/host.json` on each
machine holds what is marked there. The laptop stores only its host list and its
layouts. That is why a second laptop needs no synchronisation to see everything.

**Status is never invented.** Lifecycle hooks attach every event to bizik's
exact session id, so both Codex and Claude sessions can show *working*,
*needs you*, and *done* even when several agents share one project. Without
hooks, the status stays vague on purpose.

## Install

Needs Rust, tmux, and ssh.

Write your servers into an untracked `hosts.mk`:

```make
HOSTS = back=203.0.113.10 front=example.com
```

Then:

```sh
make setup
```

That builds a static binary, installs it to `~/.local/bin`, registers the
hosts, copies the binary to each of them, installs the status hooks, and
finishes with `make doctor`. `make` on its own lists everything else.

After changing the code, `make deploy` rebuilds and pushes to every host —
`make doctor` says which hosts are still on an older binary.

## Feature worktrees

Development uses [Worktrunk](https://worktrunk.dev): every feature gets its own
branch and sibling worktree, so several features and agent sessions can proceed
without sharing a working directory.

Install Worktrunk, then enable its shell integration once:

```sh
cargo install worktrunk
wt config shell install zsh
exec zsh
```

Start a feature from the default branch:

```sh
wt switch --create feature/session-export --base main
```

For this repository that creates a sibling directory such as
`../bizik.feature-session-export` and changes into it. The project Worktrunk
hook copies ignored local configuration such as `hosts.mk`, while deliberately
leaving out the reproducible `target/` build directory.

Useful lifecycle commands:

```sh
wt list                              # all feature worktrees and their state
wt switch feature/session-export     # return to an existing feature
wt switch main                       # return to the default branch worktree
wt remove feature/session-export     # remove it after the feature is merged
```

The repository's `AGENTS.md` applies the same one-feature/one-worktree rule to
automated coding sessions; `CLAUDE.md` is a symbolic link to the same file for
Claude Code.

<details>
<summary>The same thing by hand</summary>

```sh
rustup target add x86_64-unknown-linux-musl
cargo build --release --target x86_64-unknown-linux-musl
install -m755 target/x86_64-unknown-linux-musl/release/bzk ~/.local/bin/bzk

bzk host add back  203.0.113.10
bzk host add front example.com
bzk install                 # scp's this binary to every host
bzk hooks install back front
bzk doctor                  # checks tmux, ssh, agents, and every host
```

**Build against musl.** `bzk install` copies the binary you are running to each
server, and a default build links against your laptop's glibc — which is
routinely newer than the server's, so the copy refuses to start. The musl
target produces a genuinely static binary that runs on any Linux, and needs no
C toolchain.

</details>

`bzk install` puts the binary at `~/.local/bin/bzk` on each host. A normal
deploy therefore runs the same protocol version on both sides; if one host was
missed, probing refuses the mismatch and `doctor` names the stale side. If the
copy will not run there, install says so and repeats the musl command.

Each server needs tmux, and whichever agents you intend to run.

The status hooks — what makes `▲ needs you` possible — are part of `make setup`.
By hand:

```sh
bzk hooks install back front   # or with no names, for this machine
bzk hooks status               # where they are installed
bzk hooks uninstall back       # removes exactly what was added
```

This merges into `~/.claude/settings.json` and Codex's existing hook
representation (`~/.codex/config.toml` or `~/.codex/hooks.json`) on that
machine; every hook and setting already there is kept, and each original is
backed up once. The handlers only write a small local state file. Restart agent
sessions after installing. Codex also asks you to review and trust the exact
commands before it will run them.

`bzk doctor` reports which hosts have them, and flags a host still running an
older binary after you rebuild.

Agents also need to be findable. A non-interactive shell does not have the PATH
you have when you type — nvm, pyenv and friends are set up by an interactive rc
file — so codex would find its own binary and then die because
`#!/usr/bin/env node` could not find node. Capture the real thing once per host:

```sh
bzk env capture back front   # or with no names, for this machine
bzk env show
```

It runs your login shell interactively and remembers the PATH it ends up with,
which covers nvm, bun, pyenv, conda and whatever comes next. `doctor` says when
a host has never had it done.

## Use

On any machine, inside a directory you work in:

```sh
bzk mark                    # this directory
bzk mark -r                 # the enclosing git repository (or: --repo, or mark-repo)
bzk mark -l "payments API"  # with a name for the list
bzk unmark
```

Add a shortcut to your shell rc so marking is one keystroke:

```sh
alias m='bzk mark'
alias mr='bzk mark --repo'
```

Then, on your laptop:

```sh
bzk
```

That opens the dashboard in a tmux session named `bizik`. Run it again from any
terminal and you land back where you were — instantly, because `q` detaches
rather than shutting anything down, and the dashboard is still there with its
statuses current. While nobody is attached it stops polling, so an unwatched
dashboard costs nothing.

### Keys

| | |
|---|---|
| `↑ ↓` / `j k`, `g` `G` | move |
| `tab` / `⇧tab` | switch screen |
| `/` | fuzzy filter |
| `esc` | back — always, at every depth |
| `q` | detach — hands the terminal back, everything keeps running |
| `Q` | close the panes and the dashboard on this machine |
| `F10` | show or hide the project/session sidebar |
| `Ctrl+h` / `Ctrl+l` | focus the sidebar / active session |
| `enter` | start a session and show it beside the sidebar |
| `b` | start it in the background and stay here |
| `space` | select · then `enter` starts them all; the sidebar lists each one |
| `x` | stop a session (its conversation is kept) |
| `d` / `Delete` | forget a session · delete a project after confirmation |
| `e` | rename a folder |
| `p` | pin or unpin the selected project |
| `H` | hide or restore the selected project |
| `v` | show or hide hidden projects |
| `w` | jump to the sidebar workspace |
| `S` | save the active workspace as a layout |
| `i` | install bizik on the selected host |
| `r` | refresh now |
| `?` | this list |
| `F11` | detach immediately from anywhere in bizik |

Starting happens off the drawing thread — the header shows `starting N…` and the
list stays responsive while ssh does its work.

Codex and Claude sessions started by bizik bypass their normal approval and
sandbox checks by default (`--dangerously-bypass-approvals-and-sandbox` and
`--dangerously-skip-permissions`). Only mark projects whose contents you trust.

The workspace lives in a window called `bzk-work`: a project/session tree on
the left and one active agent on the right. Opening another session replaces
only the viewer on the right; every agent keeps running in its own detached
tmux session. Click a session in the tree to switch. Right-click a session or
project to open its management menu; every menu action displays its shortcut.
Deleting a session keeps the agent's conversation on disk, while deleting a
project removes it from bizik but leaves its files untouched.
Click a project name to fold or unfold its sessions, and click
`＋ New session` to choose Codex, Claude or a shell for that project. `F10`
hides or restores the tree; `BIZIK_SIDEBAR_KEY=F9 bzk` picks another key and
`BIZIK_SIDEBAR_WIDTH=36 bzk` changes its width.

The dashboard stays in its own window, so `S`, `x` and everything else are
pressed there, not from inside the agent where it owns the keyboard.

**`F12` gets you back to the dashboard from the workspace.** bizik binds it when it
starts, and the binding is conditional: outside bizik's own tmux session the key
passes straight through to whatever is running, so nothing else is affected.
`BIZIK_RETURN_KEY=F9 bzk` picks a different one; tmux prefix then `0` always
works too.

**`F11` exits bizik immediately from any pane.** It only detaches the current
tmux client: the dashboard and every agent keep running. Start `bzk` again to
return exactly where you were. `BIZIK_DETACH_KEY=F8 bzk` picks another key.

Click the sidebar, use `Ctrl+h` / `Ctrl+l`, or use tmux prefix plus an arrow key
to move between the tree and the active agent. In the sidebar, `j`/`k` moves,
`h` collapses or moves to the parent project, and `l` expands or opens. The same
physical keys work in Russian layout: `о`/`л` and `р`/`д`. `e` (`у`) changes a
project's display label or renames a session without touching its directory;
`n` (`т`) creates a session, `p` pins a project, `H` hides or restores it, and
`v` reveals hidden projects. `d` (`в`) deletes the selected project or session
after confirmation. Arrow keys and `Enter` work too. `prefix z` zooms the
active agent to the whole window and back.

The workspace and attached agent sessions hide tmux's own status bars: the
sidebar already shows the project, session, agent and state, so duplicated
window lists only add visual noise. `BIZIK_PANE_STATUS=label` restores the
compact legacy label inside agent panes if you prefer it.

Codex chooses its code and diff colours when it starts. A background agent
starts before a terminal is attached, so there is no terminal to report a light
background; when `tui.theme` is not already set in Codex's config, bizik supplies
the light `catppuccin-latte` syntax theme. Pick and persist another one with
Codex's `/theme`, or use `BIZIK_CODEX_THEME=<theme-name>` on the host that starts
the agent. `BIZIK_CODEX_THEME=inherit` leaves detection entirely to Codex.

The mouse is enabled for bizik's own tmux session only — clicking a pane selects
it and the wheel scrolls its history, while every other session on your tmux
server keeps whatever you had. Two things follow from it: dragging selects into
tmux's buffer instead of the terminal's, so hold `Shift` for the terminal's own
selection; and a mouse-aware program inside a pane no longer sees the wheel,
since the outer tmux takes it first. `BIZIK_MOUSE=off bzk` turns it back off.

### Nesting

Your laptop's tmux and each server's tmux both want a prefix key. Leave the
servers on the default `C-b` so hand-rolled ssh sessions keep working as before,
and give the laptop a different one:

```tmux
# ~/.tmux.conf on the laptop only
unbind C-b
set -g prefix C-a
bind C-a send-prefix
```

Then `C-a` is "my pane manager" and `C-b` is "inside that machine".

## Sessions and conversations

A **session** is a long-lived, named thing you come back to: one agent, one
folder, one conversation. Several per folder is normal — one doing the work,
one for questions.

Opening a folder shows what you can start, the sessions bizik already tracks,
and the agent's own past conversations in that directory. Picking one of those
adopts it: the record is created and the conversation resumed, with its history
left exactly where the agent put it.

bizik stores a *pointer* to the agent's conversation id, never treats it as an
identity, and repairs it after each run by matching the running process back to
the pane tmux started for it. That is what makes resuming after a reboot land in
the same conversation rather than an empty one.

## When something breaks

* **An agent exits** — the pane stays, prints the exit code, and drops you into a
  shell in the same directory. Nothing restarts by itself: a background agent
  silently re-running could repeat work that already had effects.
* **A host goes away** — its row says so, and the other hosts carry on. Probes
  run in parallel with a short timeout.
* **tmux or the server restarts** — the sessions are gone but the records are
  not. Restore a layout and everything is started and resumed in one step.
* **Quitting the dashboard** — stops nothing. The sessions are on their hosts.

## More than one laptop, or sharing

Layouts refer to hosts by name, not by address, so they survive a server moving
and can be handed to someone else who has a host of that name.

```sh
bzk export > bizik.json          # hosts and layouts
bzk import bizik.json            # merge them in on the other laptop
bzk export --folders > srv.json  # a machine's own folders and sessions
```

Merging is last-write-wins per record and respects deletions, so importing twice
is harmless and a delete made on one machine is not resurrected by the other.

## Commands

```
bzk                     open the dashboard
bzk mark [--repo] [-l]  mark a directory here
bzk unmark [path]       unmark it
bzk marks [--json]      list this machine's marks
bzk probe [--json]      what this machine has: folders, sessions, chats, status
bzk host add|rm|ls      manage the hosts this laptop drives
bzk install [host]      copy this binary to a host
bzk hooks install|uninstall|status [hosts...]
bzk export|import       move a configuration between machines
bzk doctor              check the local setup and every host
```

`bzk new-session`, `bzk spawn`, `bzk stop` and `bzk rm-session` also exist; the
dashboard calls them over ssh, and they are useful by hand or from a script.

`BIZIK_CONFIG_DIR` and `BIZIK_CACHE_DIR` override where state is kept, which is
handy for keeping two independent profiles on one machine.

## How it is put together

Four things decide what a session is: the record on its host, whether a tmux
session is alive, whether the agent process is in it, and what the agent's hooks
last reported. They can disagree, and each disagreement used to be settled
wherever it was noticed — which is how a layout came to show four missing panes
that were plainly on screen.

They are joined once, on the host that owns the session, over an exhaustive set
of states. The laptop displays that answer rather than deriving its own. A
combination nobody thought about is a compile error rather than a bug report.

Identity is carried by tmux itself (`@bzk_session` on the session and on the
pane viewing it), not recovered by matching command-line substrings. Targets go
through a type that knows tmux wants `=name`, `=name:` or `name` depending on
the command — each spelling was found by a command failing quietly.

`make test` runs the unit tests and an integration suite that drives the real
binary against a **private tmux socket** and a temporary config. That isolation
is not a nicety: testing by hand against a live tmux server once destroyed
sessions that were being worked in.

`bzk doctor` reports the things that go stale silently — an older binary on a
host, missing hooks, an uncaptured PATH, a tmux too old, a protocol mismatch.

## Notes on the agents

Claude Code keeps transcripts in `~/.claude/projects/<escaped-cwd>/<id>.jsonl`
and a live registry in `~/.claude/sessions/<pid>.json`. Both are internal
formats with no compatibility promise, so bizik parses them defensively: reads
are bounded (transcripts reach tens of megabytes), results are cached by mtime
and size, and anything unparseable degrades to a path and a timestamp rather
than an error. Registry entries outlive crashed processes, so every pid is
checked against `/proc` — including its start time, to catch a recycled pid.

Codex keeps `~/.codex/sessions/YYYY/MM/DD/rollout-*.jsonl`, whose first record
carries the working directory and session id. It has no live registry, so
liveness comes from walking `/proc`.
