# cmux-parity plan (revised after architectural discovery)

## Parity contract

`docs/cmux-parity-contract.md` is the required capability matrix for cmux
compatibility. Update that file whenever a change moves a cmux capability
between `missing`, `partial`, `blocked`, `deferred`, or `complete`.
`./scripts/check-cmux-parity.sh` enforces that every required capability stays
tracked.

## Architecture discovery

Limux has **two control servers**:

1. **Standalone `limux-control-server` binary** — uses `limux_core::Dispatcher`
   + `ControlState` and supports the **full** command vocabulary. Used for
   tests and for CLI calls when the GUI isn't running.

2. **Embedded bridge inside `limux-host-linux`** — `control_bridge.rs` only
   routes a narrow subset of methods to the GTK main loop. Supports
   `system.ping`, `system.identify`, `workspace.{current,list,create,
   select,rename,close}`, `pane.list`, `pane.surfaces`, `surface.list`,
   `pane.create` for terminal self-spawn, `surface.send_text`,
   `surface.send_key`, `surface.read_text`, `surface.health`, `surface.clear_history`, and
   `notification.create`, and `sidebar.state` for workspace cwd/git/PR/port/notification metadata. It now supports a first browser-command slice: `browser.open_split` with right-neighbor pane reuse, `browser.navigate`, `browser.back`, `browser.forward`, `browser.reload`, `browser.focus_webview`, `browser.is_webview_focused`, `browser.url.get`, `browser.get.title`, `browser.eval` with JSON-compatible value serialization, `browser.frame.{main,select}` for same-origin frame scoping, `browser.snapshot`, `browser.find.*`, `browser.click`, `browser.fill`, `browser.type`, `browser.check`, `browser.uncheck`, `browser.select`, `browser.focus`, `browser.hover`, `browser.dblclick`, `browser.scroll`, `browser.scroll_into_view`, `browser.press`, `browser.keydown`, `browser.keyup`, page-level `browser.cookies.{get,set,clear}`, native browser profile discovery via `limux browser profiles`, consent-gated raw profile-store staging plus bounded SQLite metadata inspection via `limux browser profile-data`, WebKitGTK cookie-manager import via `limux browser import-cookies --file <path>`, `browser.storage.{get,set,clear}`, `browser.tab.{list,new,switch,close}`, `browser.get.{text,html,value,attr,count,box,styles}`, visible viewport PNG screenshots via `browser.screenshot`, frame-aware stable `@eN` refs across snapshots within the loaded page/frame, and `browser.wait` polling readiness checks with timeout support. `scripts/xvfb-smoke-test.sh` now exercises a live browser split through open/wait/snapshot/find/click/fill/get-value/eval/storage/cookies/frame/tab/screenshot under Xvfb with a throwaway local HTTP fixture. The snapshot/find refs are DOM/ARIA-derived, in-memory, and reset on page load rather than full platform accessibility handles. It still does **NOT** support native Chrome/Firefox cookie decryption, Arc-specific profile discovery, actual history/session ingestion, full accessibility-tree parity, or full cross-frame/platform accessibility refs on the live GTK bridge.

When the GUI is running, the CLI targets the bridge via the runtime
socket. `list-panes` / `list-panels`, terminal `new-pane --command ...`,
text injection, key-level injection, `surface-health`, terminal
`read-screen`, exact-surface `clear-history`, project-command launch through `workspace.create`,
notification list/jump/clear, and `sidebar-state` now work against the running host.

## Delivery strategy (revised)

### Phase 1 — Env auto-wiring ✅ (shipped in 1295d12)

### Phase 2 — Make the bridge a full proxy (🚧 PARTIAL)

Bridge should route unknown methods to a local `Dispatcher` instance
seeded with live GTK state, OR to dedicated per-method `ControlCommand`
variants that interrogate the live state. The cleanest path:

- Maintain a `Arc<Mutex<ControlState>>` owned by the GTK app, kept in
  sync with live workspace/pane/surface state.
- Bridge falls through unknown methods to `Dispatcher::dispatch` on that
  shared state.
- Specific methods that need GTK side-effects (send_text, create_surface,
  notification.create) remain as `ControlCommand` variants.

The terminal introspection path is now bridged directly against live GTK state.
Remaining proxy work is for deferred browser surface commands and broader
dispatcher parity.

**Shipped so far (in 6b8eb1a and follow-up bridge work):**

- `surface.send_text` and `notification.create` now pass `allow_name=true`
  to `parse_optional_workspace_target`, so peers can address each other
  by workspace name (`--workspace claude`) without juggling runtime
  UUIDs. This is what made phase 5 practical.
- `pane.list`, `pane.surfaces`, and `surface.list` now route on the live
  GTK bridge, so agents can discover peer panes/surfaces in a running
  Limux window.
- `surface.send_key` now routes to the exact terminal surface when provided,
  so agents can send deterministic key-level control such as Ctrl-C.
- `surface.health`, `surface.read_text`, and `surface.clear_history` now route on the live GTK bridge,
  so agents can inspect peer terminal health, read visible screen text, and clear the resolved terminal surface.
- `pane.create` now routes through the GTK bridge for terminal panes. From
  inside an agent terminal, `limux new-pane --direction right --command claude`
  uses `LIMUX_WORKSPACE_ID`, `LIMUX_SURFACE_ID`, and `LIMUX_PANE_ID` to split
  the caller's pane, create a new terminal, and launch the command there.

**Still open (priority order):**

- Browser command bridge parity beyond the current slices: cookie decryption design after the consent-gated profile-data staging/inspection path, actual history/session import, full accessibility-tree parity, storage/cookie edge smokes, and full cross-frame/platform refs.
- Live terminal dispatcher parity for pane break/join/last and workspace next/previous/last/reorder/move-to-window.
- Remaining shortcut remapping parity decisions after the first-class `Ctrl+Shift+P` palette and `Ctrl+,` Settings/Keybindings shortcuts.

### Phase 3 — `limux notify` + GUI toast/sidebar integration ✅
`ControlCommand::CreateNotification` wired through the bridge into
`mark_workspace_unread_with_message` + libadwaita toast. The live GTK bridge
also exposes `notification.list`, `notification.clear`, and `notification.jump`,
with CLI coverage via `limux list-notifications [--unread]`,
`limux clear-notifications [--id <n>]`, and `limux jump-notification [--id <n>]`.
Notification rows retain workspace/pane/tab targets so jump can focus the latest
unread source or a specific notification by id. The native notifications panel
opens with `Ctrl+Alt+O` and can jump to, clear one, or clear all live rows.
Targeted notifications now also mark pane/tab attention CSS rings and clear them
when the source is focused, jumped, or notification state is cleared. `limux notify`
auto-targets `LIMUX_PANE_ID`/`LIMUX_SURFACE_ID` when called from a Limux terminal,
and `debug.attention.state` makes ring state observable in the Xvfb harness. The host shortcut
registry maps `Ctrl+Alt+J` to
the same latest-notification jump path, and the binding can be remapped through
`shortcuts.json`. Settings/Keybindings also has a first-class remappable
`Ctrl+,` shortcut so the settings surface is reachable without the pane toolbar.
CLI: `limux notify [--workspace <id|name>] [--pane <id|ref>] [--surface <id|ref>] [--subtitle <…>] [--body <…>] <title>`.

### Phase 4 — `limux claude-hook` / `opencode-hook` / `gemini-hook` ✅
Reads hook JSON from stdin, translates the agent-specific event vocabulary
into a `notify` (and, where useful, an inline `send`). Drop-in for
`~/.claude/settings.json` hooks blocks.

### Phase 5 — `limux agent-team` + `AGENTS.md` template ✅
`limux agent-team [--agents codex,claude[,opencode,gemini]] [--cwd <path>]
[--no-launch] [--dry-run]`:

- Calls `workspace.create` once per agent with `name=<agent>`, `cwd=<shared>`,
  `command=<agent CLI>` so each workspace launches the agent automatically.
- Bridge now passes `allow_name=true` to `parse_optional_workspace_target`
  for `surface.send_text` and `notification.create`, so peers address each
  other by workspace name (`limux send --workspace claude …`) instead of
  needing to swap UUIDs.
- Writes `AGENTS.md` in the shared cwd documenting:
    - the peers table (agent → workspace name → workspace ID → launch cmd),
    - the `<agent-msg from="…" to="…" id="…" reply-to="…" ts="…">` envelope,
    - the exact `limux send` invocation for sending and replying,
    - the `limux notify` escalation path for human input,
    - the `LIMUX_*` env contract every spawned terminal inherits,
    - editable Policies section (timeouts, size limits, destructive-action gating).

### Phase 6 — `limux ssh` remote workspace slice 🚧 PARTIAL
`limux ssh [--cwd <path>] [--name <workspace>] [--] <ssh-args...>` creates a
new workspace through the live `workspace.create` bridge and launches OpenSSH in
its first terminal. This matches the first-order cmux behavior of quickly opening
a remote terminal workspace. Remote browser network routing and image drag/upload
through `scp` remain open.

### Phase 7 — (deferred) `limux progress`, `limux log`, `limux markdown`
Nice polish, not blockers.

## Why phase 2 first

Without a real bridge, every subsequent feature ends up routing around
the same hole: the GUI owns the ground truth about surfaces/panes but
the CLI can't query it. Fixing this once, properly, makes phases 3–5
small.
