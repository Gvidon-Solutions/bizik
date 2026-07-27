# bizik

Mark folders on the machines you work on. From one screen, launch Claude Code or
Codex sessions in any of them, watch several at once, and see which one is
waiting on you.

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
tmux draws the panes. An agent's own full-screen interface keeps working
properly — scrollback, mouse, resize, copy mode — because nothing reimplements a
terminal.

**Folders and sessions live on their host.** `~/.config/bizik/host.json` on each
machine holds what is marked there. The laptop stores only its host list and its
layouts. That is why a second laptop needs no synchronisation to see everything.

**Status is never invented.** Claude Code publishes a busy/idle status; its
hooks say whether idle means *blocked on you* or *finished*. Codex publishes
nothing, so its sessions show *running* and nothing more. Where two agents share
a folder and cannot be told apart, the status stays vague on purpose.

## Install

Needs Rust, tmux, and ssh.

Write your servers into an untracked `hosts.mk`:

```make
HOSTS = back=168.119.201.8 front=example.com
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

<details>
<summary>The same thing by hand</summary>

```sh
rustup target add x86_64-unknown-linux-musl
cargo build --release --target x86_64-unknown-linux-musl
install -m755 target/x86_64-unknown-linux-musl/release/bzk ~/.local/bin/bzk

bzk host add back  168.119.201.8
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

`bzk install` puts the binary at `~/.local/bin/bzk` on each host. It is the same
binary on both sides, so the two can never disagree about the data format. If
the copy will not run there, install says so and repeats the musl command.

Each server needs tmux, and whichever agents you intend to run.

The status hooks — what makes `▲ needs you` possible — are part of `make setup`.
By hand:

```sh
bzk hooks install back front   # or with no names, for this machine
bzk hooks status               # where they are installed
bzk hooks uninstall back       # removes exactly what was added
```

This edits `~/.claude/settings.json` on that machine. It merges — every hook and
setting already there is kept — and the original is copied to
`settings.json.bzk-backup` the first time. Four events are added
(`Notification`, `Stop`, `UserPromptSubmit`, `SessionEnd`), each running a
fire-and-forget command that writes one small file. A session picks the hooks up
when it next starts.

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
| `enter` | start a session and open a pane |
| `b` | start it in the background and stay here |
| `space` | select · then `enter` opens them all at once |
| `x` | stop a session (its conversation is kept) |
| `d` | forget a session · unmark a folder |
| `e` | rename a folder |
| `w` | jump to the pane window |
| `S` | save the open panes as a layout |
| `i` | install bizik on the selected host |
| `r` | refresh now |
| `?` | this list |

Starting happens off the drawing thread — the header shows `starting N…` and the
list stays responsive while ssh does its work.

Restoring a layout wants the pane window to itself: if panes are open that the
layout does not know about, it offers to close them and replay the saved
geometry exactly, rather than silently tiling everything together.

Panes open in a window called `bzk-work`, and the dashboard stays in its own
window — so `S`, `x` and everything else are pressed there, not from inside a
pane where the agent owns the keyboard.

**`F12` gets you back to the dashboard from any pane.** bizik binds it when it
starts, and the binding is conditional: outside bizik's own tmux session the key
passes straight through to whatever is running, so nothing else is affected.
`BIZIK_RETURN_KEY=F9 bzk` picks a different one; tmux prefix then `0` always
works too.

Between panes: click one, or tmux prefix and an arrow key. `prefix z` zooms the
current pane to the whole window and back — with four panes open that is the one
worth remembering. From the dashboard, `enter` on a session that is already open
jumps to its pane rather than opening a second copy of it.

Each pane carries a label along its bottom edge — ` back · anogem · claude ` —
the machine, the directory and the agent. That is the tmux session on the host
drawing its own status line inside the pane; left alone it shows bizik's
bookkeeping (`bzk-6aaaf1:claude*`), which tells the reader nothing.
`BIZIK_PANE_STATUS=off` leaves that status line as tmux would have drawn it.

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
