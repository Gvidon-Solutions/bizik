# Architecture

bizik is one executable with two runtime roles: a host-side session service and
a local dashboard. Keeping one wire-compatible binary is intentional; keeping
all responsibilities in one compilation unit is not an architectural
requirement.

## Boundaries

The current modules form these layers:

- **Domain:** `model`, `reconcile`, the pure row-building logic in `tui/rows`,
  and the keyboard reducer in `tui/state`. These define records and derive
  session or presentation state without performing I/O.
- **Application:** `application`, plus the orchestration portions of `hostops`
  and `probe`. Repository and session-runtime ports live here; filesystem and
  tmux implementations do not.
- **Ports and adapters:** the `Agent` trait is the port for agent-specific
  behavior; `agent/claude` and `agent/codex` are adapters.
- **Infrastructure:** `store`, `remote`, `tmux`, `hooks`, `attention`,
  `hostenv`, and the process/filesystem helpers in `util`.
- **Presentation:** command dispatch in `cli`, rendering in `tui/ui`, and the
  dashboard effect executor in `tui`. `main` is only the process entry point
  and error-to-exit-code adapter.

Dependencies should point toward the domain. In particular, `model` and
`reconcile` must not invoke tmux, ssh, the filesystem, or terminal rendering.
New agent-specific behavior belongs behind `Agent`, not in `probe` or the TUI.

## Data ownership and consistency

`host.json` belongs to the machine that owns its folders and sessions.
`local.json` belongs to the machine running the dashboard and contains hosts and
layouts.

Every mutation keeps a tombstone and an `updated_at` timestamp. Saving a store
is a serialized read-merge-write transaction:

1. take the store lock;
2. reread and validate the on-disk version;
3. merge records by UUID and update time;
4. write and fsync a private temporary file;
5. atomically rename it and fsync the parent directory.

This prevents torn JSON and prevents a stale dashboard or concurrent CLI
process from erasing an unrelated update. Both input stores and the merged
result are validated for version, unique identities, host invariants, and live
session references before any rename. An older binary cannot discard newer
fields, and two individually valid concurrent edits cannot persist an invalid
combined state.

The merge clock is still wall-clock based. It is sufficient for concurrent
processes on one host, but true multi-device automatic synchronization will need
a logical revision or hybrid logical clock before it can safely replace the
explicit export/import workflow.

## External command safety

Values passed through `std::process::Command` are positional arguments, with
`--` before user-configured SSH targets. Commands that tmux must run through a
shell quote every data argument with `util::shell_quote`. Do not interpolate a
host, path, session identifier, or environment value into a shell command
without going through that boundary.

## Verification

The local gates are:

```sh
make check       # rustfmt, strict Clippy, unit and tmux integration tests
make audit       # RustSec, licenses, duplicate versions, trusted sources
make coverage    # LLVM source coverage with a ratcheted line floor
make verify      # all three local quality gates above
make build       # static x86_64 Linux release artifact
```

CI repeats these on stable Rust, treats rustdoc warnings as errors, verifies the
declared Rust 1.89 MSRV, builds the musl target, and refreshes the advisory scan
weekly. Production code forbids `unsafe`, `unwrap`, `expect`, `panic`, `todo`,
and `unimplemented`. The measured line coverage is 65.6% and the enforced floor
is 65%; it must move upward as the orchestration boundaries below become
testable. Correctness-focused Clippy restrictions also reject lossy integer
casts, unchecked time subtraction, redundant clones, and ambiguous
`map(...).unwrap_or(...)` fallback chains.

## Remaining structural work

The next refactors are ordered by leverage:

1. Continue moving session spawning, probing, remote installation, and pane
   restore behind narrow runtime traits. Store use cases and safe session
   deletion already use ports; process-heavy branches remain the largest
   deterministic-testing gap.
2. Extend the TUI reducer beyond keyboard input to consume probe/launch
   completion events. Keyboard navigation is pure and thoroughly tested;
   worker completion still mutates `App` in the effect executor.
3. Split the command parser/formatter from the remaining infrastructure-heavy
   CLI handlers once those runtime ports exist. `main` is already a thin
   library entry point, and store-oriented commands call typed use cases.
4. Replace lossy string paths in domain records with an explicit serialized
   path type, or document UTF-8-only paths as a product constraint. Current
   `to_string_lossy` calls can make two distinct non-UTF-8 paths
   indistinguishable.
5. Add an explicit store migration registry before incrementing the store
   format. Version rejection is safe, but it is not yet an upgrade path.

These are architectural constraints and tracked debt, not reasons to weaken a
quality gate or hide an unsupported state.
