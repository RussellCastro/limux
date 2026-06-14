# Scriptable terminal API

Limux exposes a JSON control socket used by `limux-cli` and by agent workflows
running inside terminal surfaces. When the GTK host is running, these methods are
handled against live workspace, pane, tab, and Ghostty terminal state.

## Stable live terminal methods

| Method | CLI path | Live GTK behavior |
|---|---|---|
| `workspace.current` | `limux current-workspace` | Returns the focused workspace handle and metadata. |
| `workspace.list` | `limux list-workspaces` | Lists live workspaces with ids, refs, names, cwd, and selection state. |
| `workspace.create` | `limux new-workspace`, `limux run-command`, `limux ssh` | Creates a workspace, optionally with cwd and startup command. |
| `workspace.select` | `limux select-workspace` | Selects a workspace by id/ref/name/index. |
| `workspace.rename` | `limux rename-workspace` | Renames a workspace. |
| `workspace.close` | `limux close-workspace` | Closes a workspace. |
| `pane.list` | `limux list-panes` | Lists live pane ids, active surface ids, and focused pane state. |
| `pane.surfaces` | `limux pane-surfaces` | Lists surfaces/tabs for a pane. |
| `surface.list` | `limux list-panels` | Lists terminal/browser/keybinding surfaces in the workspace. |
| `pane.create` | `limux new-pane` | Creates a terminal split from an explicit source surface/pane or active fallback. |
| `surface.send_text` | `limux send` | Pastes text into the resolved terminal surface. |
| `surface.send_key` | `limux send-key` | Sends a normalized key chord to the resolved terminal surface. |
| `surface.read_text` | `limux read-screen`, `limux capture-pane` | Reads visible viewport text from the resolved terminal surface. |
| `surface.health` | `limux surface-health` | Reports terminal realization, process state, and size. |
| `surface.clear_history` | `limux clear-history` | Runs Ghostty's `clear_screen` action on the resolved terminal surface. |

Workspace targets accept `workspace:<id>` refs, raw ids, names where the route
allows names, and indices where supported. Surface targets accept raw composite
surface ids and `surface:<id>` refs. Inside a Limux-spawned terminal, the CLI
auto-fills `LIMUX_WORKSPACE_ID`, `LIMUX_SURFACE_ID`, and `LIMUX_PANE_ID` for
commands that operate on the caller's workspace or terminal.

## Regression coverage

- `cargo test -p limux-host-linux control_bridge` covers parser contracts for
  live terminal routes, including `surface.clear_history` surface refs.
- `cargo test -p limux-cli` covers CLI request construction for env-targeted
  pane creation and notifications.
- `scripts/xvfb-smoke-test.sh` exercises live `send`, `read-screen`,
  `clear-history`, `new-pane --command`, surface env propagation, and exact
  surface targeting when `xvfb-run` is available.

## Current boundaries

The live GTK bridge does not yet implement every standalone dispatcher method.
Known open terminal/window methods include pane break/join/last and workspace
next/previous/last/reorder/move-to-window actions. Browser automation has its
own parity track in `docs/cmux-parity-contract.md`.
