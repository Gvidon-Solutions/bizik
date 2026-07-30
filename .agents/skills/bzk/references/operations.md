# bizik operations

Use this file for installation, project creation, SSH, sessions, monitoring,
and troubleshooting.

## Core commands

```sh
bzk                                  # open or reattach dashboard
bzk mark [PATH] [--label LABEL]      # mark a directory
bzk mark --repo [PATH]               # mark enclosing git repository
bzk unmark [PATH]                    # remove from bizik, not from disk
bzk marks --json                     # list local marked folders with IDs
bzk probe --json --preview           # local projects, sessions, chats, states
bzk session|s list [--json]          # concise session operations
bzk s current --json                 # BZK_SESSION_ID on this machine
bzk s new FOLDER --agent AGENT       # exact folder name or UUID prefix
bzk s open|stop|remove [TARGET]      # exact title or unique UUID prefix
bzk s rename TITLE [-s TARGET]       # current session when TARGET is omitted
bzk host add NAME [SSH_TARGET]       # add/update remote or local host
bzk host ls                          # list configured hosts
bzk install [NAME]                   # deploy current binary to one/all hosts
bzk hooks install [NAME...]          # install precise status hooks
bzk hooks status [NAME...]           # inspect hook coverage
bzk env capture [NAME...]            # capture agent/tool PATH
bzk env show                         # show captured environments
bzk layout ls --json                # list saved terminal layouts
bzk layout save NAME                # save panes and exact geometry
bzk layout open NAME                # start and restore all layout sessions
bzk doctor                           # check local setup and every host
```

Add `--host NAME` to compact commands for a configured remote host. Remote
mutations require an explicit target and `current` is local-only; never treat a
local `BZK_SESSION_ID` as a remote identity.

The exact UUID protocol remains available for scripts and compatibility:

```sh
bzk new-session --folder UUID --agent codex --title TITLE
bzk spawn --session UUID --host-label HOST_NAME
bzk stop --session UUID
bzk rm-session --session UUID
bzk layout create NAME --pane HOST:SESSION_UUID [--pane ...]
bzk layout add NAME --host HOST --session SESSION_UUID
bzk layout remove NAME --host HOST --session SESSION_UUID
bzk layout rename NAME NEW_NAME
bzk layout rm NAME
```

`new-session` prints the session record as JSON. Existing scripts may continue
to extract its `id` and pass it to `spawn`; never scrape the human dashboard.

Layouts are local viewing records; project sessions remain owned by their
hosts. A one-project layout is shown inline inside that project in the sidebar,
while a cross-project layout is shown in the global tree below all projects.
Removing a session from a layout or deleting the layout never stops or forgets
the session.

## FAST PATH: existing marked local project

Use this path only when the host is local, the project is already marked, and
the user explicitly requested `codex`, `claude`, or `shell`. Do not perform the
full preflight.

1. Create the session by exact project label/path (or a known UUID prefix), then
   parse the returned JSON `id`:

   ```sh
   bzk s new PROJECT --agent AGENT --title TITLE --json
   bzk s open SESSION_UUID
   ```

2. Verify once:

   ```sh
   bzk s list --json
   ```

Do not run `bzk doctor`, `bzk hooks status`, `bzk host ls`, `bzk env capture`,
`bzk --help`, subcommand help, or repeated probes on this path. An actual error
may justify a targeted check; switch to full diagnostics only for recovery or
when the user asks for it.

## Full preflight

Use the full preflight for remote work, a new or unregistered project,
installation, explicit diagnosis, and recovery. Do not use it for the fast
path above.

1. Resolve whether the target is local or remote. For remote work, resolve a
   short bizik host name and the SSH target separately.
2. Inspect existing state before changing it:

   ```sh
   command -v bzk
   bzk --version
   bzk host ls
   bzk hooks status
   bzk doctor
   ```

3. For SSH, use the user's existing SSH config and credentials. Test the exact
   target without disabling host-key verification. If authentication needs an
   interactive password, key passphrase, VPN, or first-use fingerprint
   decision, pause and tell the user exactly what is needed.
4. On the target, verify `tmux` and the requested `codex`, `claude`, or shell.
   Capture the host environment when the agent comes from nvm, pyenv, conda,
   bun, or another interactive-shell setup.

Do not dump whole config files during preflight; select only the required
fields and redact secrets from output.

## Create or register a local project

1. Inspect the destination. Reuse it only when its contents match the request.
2. If a repository URL is given, clone it into a missing/empty destination. If
   the directory exists, verify its origin instead of cloning over it.
3. If no repository URL is given, create only the requested directory. Do not
   invent a framework or run `git init` unless requested or clearly implied.
4. Register the project:

   ```sh
   bzk mark "/absolute/project/path" --label "project label"
   ```

5. Install local hooks and capture the environment when missing:

   ```sh
   bzk hooks install
   bzk env capture
   ```

6. Find the folder UUID in `bzk marks --json`, create the requested agent
   session, spawn it, then verify with `bzk probe --json --preview`.

## Create or register a project over SSH

1. If the host name is absent, register it:

   ```sh
   bzk host add HOST_NAME SSH_TARGET
   ```

   If the name already points elsewhere, pause before replacing it.

2. Ensure the same compatible binary is available remotely:

   ```sh
   bzk install HOST_NAME
   bzk hooks install HOST_NAME
   bzk env capture HOST_NAME
   ```

   A normal install places the remote binary at `~/.local/bin/bzk`. If install
   reports a libc mismatch, build/deploy the documented static musl target
   rather than copying an incompatible binary.

3. Create or clone the project through the exact SSH target. Pass paths, labels,
   URLs, and titles as positional arguments to a small quoted remote command;
   never concatenate raw user input into a remote shell string. Refuse to
   overwrite a non-empty destination or a repository with a different origin.

4. On the remote host, mark the absolute path with:

   ```sh
   "$HOME/.local/bin/bzk" mark "/absolute/project/path" --label "project label"
   ```

5. Create and open the session through the configured logical host name:

   ```sh
   bzk s new PROJECT --agent AGENT --title TITLE --host HOST_NAME --json
   bzk s open SESSION_UUID --host HOST_NAME
   ```

6. Verify with `bzk s list --host HOST_NAME --json`, then run local `bzk doctor`.
   Report both the session state and how to open it from the dashboard.

## Choosing the agent

- Honor an explicit `codex`, `claude`, or `shell`.
- If none is stated, reuse the user's recent/default agent when discoverable.
- If there is no safe signal, ask before launching; agent choice determines
  permissions, conversation storage, and available commands.

bizik starts Codex and Claude with their approval/sandbox bypass flags. Treat
marked project contents as trusted code. Call this out before the first launch
on an unfamiliar repository.

## Monitoring and control

- Use local or remote `bzk probe --json --preview` to inspect state.
- Distinguish `working`, `needs you`, `done`, `exited`, `running`, and
  `stopped`; do not report completion merely because tmux is alive.
- When asked to wait or babysit, poll at a moderate interval and report only
  changes plus periodic heartbeats. Surface `needs you` immediately with the
  question or screen preview.
- Do not answer an agent's consequential question on the user's behalf unless
  the request already supplies that decision. Let the user respond, then pass
  the response to the session.
- Stop or forget a session only when explicitly requested. `stop` preserves the
  conversation; `rm-session` forgets the bizik record.

## Safe local reload

Use this low-level workflow only for an explicit `$bzk reload`. It updates the
local installed executable without signaling, restarting, detaching, or killing
the dashboard, sidebar, agent tmux sessions, or any unrelated process.

1. Resolve the source checkout and installed executable with `pwd`,
   `git status --short --branch`, and `command -v bzk`. Refuse a remote target,
   a directory, or a path outside the user's local binary location. Do not run
   `make deploy`, `bzk install`, hook commands, tmux kill commands, `pkill`, or
   `kill`.
2. Before building, capture the installed binary's canonical path, SHA-256,
   device/inode from `stat -Lc '%d:%i'`, and version. Capture every exact
   `bzk` PID from `pgrep -x bzk`; for each existing `/proc/PID`, record its
   `/proc/PID/stat` start time plus the device/inode and link target of
   `/proc/PID/exe`. Keep the snapshot in a temporary directory created with
   `mktemp -d`.
3. Run `make check`, then `make build`. Building must finish before anything at
   the installed path changes. Verify the release artifact exists, is
   executable, and runs `--version`.
4. Stage the release artifact in the installed binary's directory with
   `mktemp`, mode `0755`, and the same owner. Compare the staged file to the
   build artifact. Atomically rename that one staged file over the installed
   binary. Do not alter configuration, hooks, tmux state, or any other file.
5. Verify the installed file has the staged SHA-256 and a different inode from
   the captured installed inode. Run the installed path with `--version` and
   compare it with the build artifact's output; also compare the two files byte
   for byte so the invocation path is proven to contain the new build.
6. Verify every captured PID still exists with the same `/proc/PID/stat` start
   time and the same executable device/inode captured before replacement.
   Linux may append ` (deleted)` to its `/proc/PID/exe` link after the atomic
   rename; that is expected and proves the live process kept its old image.
   Treat a missing/reused PID, changed executable inode, or mismatched new
   binary as failure and report it without stopping anything.
7. Remove only the temporary snapshot/staging files and report old/new
   inode/hash/version plus the surviving PID count.

## Recovery

- Agent exited: inspect the pane preview and exit code; do not auto-restart work
  that may have produced side effects.
- Host unavailable: verify SSH/DNS/VPN while leaving other hosts untouched.
- Version mismatch: deploy the current binary to the named stale host.
- Vague status: install hooks, restart only the affected agent session when the
  user authorizes it, and complete Codex's one-time `/hooks` trust review.
- Agent command not found: run `bzk env capture HOST_NAME`, then verify the
  captured PATH and requested agent binary.
- tmux/server restart: session records remain; restore or spawn the requested
  session to resume its stored conversation.
