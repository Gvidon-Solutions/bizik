# bizik hotkeys

Use this file for shortcut questions. Mention only the keys relevant to the
question unless the user asks for the complete list.

## Dashboard

| Key | Action |
|---|---|
| `↑` / `↓`, `j` / `k`, `g` / `G` | Move; jump to first/last |
| `Tab` / `Shift+Tab` | Switch dashboard screen |
| `/` | Fuzzy filter |
| `Esc` | Go back at any depth |
| `Enter` | Start and open the selected session |
| `b` | Start in the background |
| `Space` | Select sessions; `Enter` starts all selected |
| `x` | Stop a session but keep its conversation |
| `d` / `Delete` | Forget a session or remove a project from bizik after confirmation; project files remain |
| `e` | Rename a project label or session |
| `p` | Pin or unpin a project |
| `H` | Hide or restore a project |
| `v` | Show or hide hidden projects |
| `w` | Jump to the sidebar workspace |
| `S` | Save the active workspace and exact pane geometry as a layout |
| `i` | Install bizik on the selected host |
| `r` | Refresh immediately |
| `?` | Show built-in key help |
| `q` | Detach; dashboard and sessions keep running |
| `Q` | Close local dashboard panes; remote sessions keep running |

## Workspace and sidebar

| Key | Action |
|---|---|
| `F10` | Show or hide the project/session sidebar |
| `Ctrl+h` / `Ctrl+l` | Focus sidebar / active session |
| `F12` | Return from workspace to dashboard |
| `F11` | Detach immediately from any bizik pane |
| `j` / `k` | Move in the sidebar |
| `h` / `l` | Collapse or move to parent / expand or open |
| `n` | Create a session |
| `a` | Add the selected session to an existing layout |
| `o` | Open the selected session standalone; other viewers close, agents keep running |
| `S` | Save the current multi-pane workspace as a layout |
| `d` / `Delete` | On a nested layout session, remove only its reference |
| tmux prefix, then `z` | Zoom active agent pane and restore |
| tmux prefix, then `0` | Return to the dashboard window |

Russian-layout equivalents in the sidebar are `о`/`л` for `j`/`k`,
`р`/`д` for `h`/`l`, `у` for `e`, `т` for `n`, `ф` for `a`, `щ` for `o`,
and `в` for `d`.

## Overrides

- `BIZIK_SIDEBAR_KEY=F9 bzk` changes `F10`.
- `BIZIK_RETURN_KEY=F9 bzk` changes `F12`.
- `BIZIK_DETACH_KEY=F8 bzk` changes `F11`.
- `BIZIK_SIDEBAR_WIDTH=36 bzk` changes sidebar width.
- `BIZIK_MOUSE=off bzk` disables bizik's tmux mouse support.

`F11` and `q` detach instead of stopping agents. Starting `bzk` again returns
to the existing dashboard.
