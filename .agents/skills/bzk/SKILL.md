---
name: bzk
description: Operate and explain bizik (bzk), including hotkeys, installation, SSH hosts, marked projects, agent sessions, status hooks, diagnostics, and monitored execution. Use when the user invokes $bzk, asks how to use or launch bizik projects, or asks Codex to create, register, launch, inspect, or manage a local or remote bizik project.
---

# BZK

Act as the user's bizik operator. Answer questions directly; when the user asks
for an operation, run it through completion and keep the user informed.

## Route the request

- For keys and navigation, read [references/hotkeys.md](references/hotkeys.md).
- For commands, setup, SSH projects, sessions, monitoring, and recovery, read
  [references/operations.md](references/operations.md).
- When working inside a bizik checkout, treat its current `README.md`,
  `bzk --help`, and subcommand help as newer than bundled references.

## Keep the user in control

1. State the resolved host, path, project source, and agent before mutating
   anything. Infer obvious values and ask only when a missing value materially
   changes the result.
2. Run ordinary requested setup steps without repeated confirmation. Report
   each meaningful checkpoint and any assumption. During long-running work,
   send a brief progress update at least once per minute.
3. Pause before deleting files or configuration, overwriting a non-empty
   directory, replacing an SSH host target, stopping unrelated sessions, or
   changing authentication/security settings not explicitly requested.
4. Never print private keys, passwords, tokens, or secret file contents. Do not
   weaken SSH host-key checking. Treat a changed host key as a blocker.
5. Quote paths and user-provided values as data. Avoid `eval`, interpolated
   remote shell fragments, destructive globs, and broad recursive commands.
6. Verify the observable result after every mutation. On failure, preserve
   completed safe work, explain the failing stage, and continue with a safe
   repair when possible.

## Execute by intent

- **Explain:** answer in the user's language with the relevant keys or commands.
- **Inspect or diagnose:** begin with read-only checks and use actual output as
  evidence. Do not repair unless the request includes repair or execution.
- **Create or launch:** use the fast path below for an already marked local
  project and an explicit agent. Otherwise perform the full preflight, create
  or reuse the exact directory, register it with bizik, configure visibility
  hooks when appropriate, create the requested Codex/Claude/shell session,
  start it, and verify its state.
- **Monitor:** keep polling at a reasonable interval, report state changes and
  questions from the agent, and yield control when user input is needed. A
  running session is progress, not completion.

## FAST PATH: launch in an existing local project

Use this path when all three conditions hold: the host is local, the project is
already marked, and the user explicitly chose `codex`, `claude`, or `shell`.

1. Reuse the folder UUID if it is already known; otherwise get it with
   `bzk marks --json`.
2. Run `bzk new-session` for that folder and agent, parse the returned session
   ID, then run `bzk spawn`.
3. Run `bzk probe --json --preview` exactly once to verify the result.

Do not run `bzk doctor`, `bzk hooks status`, `bzk host ls`, `bzk env capture`,
any `--help`, or extra probes on this path. Use targeted recovery after an
actual error, or the full preflight when the user separately asks for
diagnostics.

Finish with a compact handoff: host, path, bizik project label, session/agent,
current state, and the command or key that returns the user to it.
