# Repository workflow

## Feature worktrees

- Use Worktrunk for every new feature that changes the repository. One feature
  must have one dedicated branch and one dedicated worktree.
- If the current checkout is already the dedicated worktree for that feature,
  continue there instead of creating another one. Treat it as dedicated only
  when `wt list` shows it as a secondary worktree and its branch already follows
  the `feature/...` or `fix/...` naming rule for the requested work. The primary
  checkout is never a feature worktree.
- Name feature branches `feature/<short-kebab-case-name>` and fixes
  `fix/<short-kebab-case-name>`.
- Create new work from `main`:

  ```sh
  wt switch --create feature/<name> --base main
  wt switch --create fix/<name> --base main
  ```

- Reuse an existing feature worktree with `wt switch feature/<name>`.
- In a non-interactive environment where Worktrunk cannot change the parent
  shell's directory or ask for project-hook approval, add
  `--no-cd --format=json --yes`, read the `path` field from the result, and run
  all subsequent commands with that directory as their working directory:

  ```sh
  wt switch --create fix/<name> --base main --no-cd --format=json --yes
  ```

  `--yes` approves only the repository's configured Worktrunk hooks; never use
  it as a substitute for resolving destructive-operation prompts.
- Do not implement a feature in the primary checkout. Configuration-only
  maintenance of the worktree workflow itself is the exception.
- Inspect `wt list` before creating a worktree so an existing feature branch is
  never duplicated.
- Do not use `--force`, `--force-delete`, or delete a worktree with uncommitted
  changes. After a feature is integrated, remove its clean worktree with
  `wt remove feature/<name>`.
- Treat every other worktree as independently owned: never discard, move, or
  overwrite its changes.
