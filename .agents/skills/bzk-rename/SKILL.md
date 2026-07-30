---
name: bzk-rename
description: Infer and apply a concise, specific title to the current Bizik session from the actual conversation and task. Use when the user invokes $bzk-rename, asks to rename or title this Bizik session, or wants the current session name to reflect the work being done.
---

# Rename the current Bizik session

Infer the title from the work actually discussed here, rename only the session
identified by `BZK_SESSION_ID`, and verify it once.

1. Read the conversation and identify the concrete object plus outcome: for
   example, `Add compact session CLI` or `Fix OAuth callback race`. Prefer
   roughly 3–8 words.
2. Do not invent a generic title such as `Current task`, `Bizik work`,
   `Development session`, or `Working on changes`. If the conversation does
   not establish a specific task, ask for one instead of renaming.
3. Require a non-empty `BZK_SESSION_ID`. Do not search for another session and
   do not substitute a title match.
4. Run exactly one mutation:

   ```sh
   bzk s rename "INFERRED TITLE"
   ```

   Let the compact command consume `BZK_SESSION_ID`; never call `stop`, `open`,
   `remove`, raw `rename-session`, or tmux process commands.
5. Verify exactly once:

   ```sh
   bzk s current --json
   ```

   Confirm both that `id` equals `BZK_SESSION_ID` and that `title` equals the
   inferred title. Do not run a second list, current, or probe. Report the final
   title and session's short ID.
