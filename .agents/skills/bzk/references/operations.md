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
bzk host add NAME [SSH_TARGET]       # add/update remote or local host
bzk host ls                          # list configured hosts
bzk install [NAME]                   # deploy current binary to one/all hosts
bzk hooks install [NAME...]          # install precise status hooks
bzk hooks status [NAME...]           # inspect hook coverage
bzk env capture [NAME...]            # capture agent/tool PATH
bzk env show                         # show captured environments
bzk doctor                           # check local setup and every host
```

For machine-readable work, also use:

```sh
bzk new-session --folder UUID --agent codex --title TITLE
bzk spawn --session UUID --host-label HOST_NAME
bzk stop --session UUID
bzk rm-session --session UUID
```

`new-session` prints the session record as JSON. Extract its `id` from JSON and
pass it to `spawn`; never scrape the human dashboard.

## FAST PATH: existing marked local project

Use this path only when the host is local, the project is already marked, and
the user explicitly requested `codex`, `claude`, or `shell`. Do not perform the
full preflight.

1. Reuse a folder UUID already established in the current context. If it is not
   known, make the only discovery call:

   ```sh
   bzk marks --json
   ```

2. Create the session, parse its JSON `id`, and spawn it locally:

   ```sh
   bzk new-session --folder FOLDER_UUID --agent AGENT --title TITLE
   bzk spawn --session SESSION_UUID --host-label local
   ```

3. Verify once:

   ```sh
   bzk probe --json --preview
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

5. Retrieve the folder UUID from remote `bzk marks --json`. Over the same SSH
   target, run remote `bzk new-session`, parse the returned JSON ID, then run
   remote `bzk spawn --host-label HOST_NAME`.

6. Verify remote `bzk probe --json --preview`, then run local `bzk doctor`.
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
