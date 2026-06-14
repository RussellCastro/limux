# cmux parity contract

This document is the source of truth for this personal fork's cmux parity
work against `manaflow-ai/cmux`. Keep it current when adding, removing, or
re-scoping cmux-compatible behavior.

The tracked capability set is based on the cmux README feature surface as of
2026-06-13. Re-check upstream cmux before changing the required set. Use these
status values:

- `complete`: Limux has the user-visible behavior and a regression check or
  smoke path.
- `partial`: Limux has meaningful behavior, but important cmux semantics are
  missing.
- `missing`: Limux does not have the behavior.
- `blocked`: Work needs an upstream dependency, platform decision, or design
  decision before implementation.
- `deferred`: Intentionally out of scope for the current parity push.

Each tracked capability must keep a marker in this exact form so
`scripts/check-cmux-parity.sh` can enforce coverage:

```text
<!-- cmux-parity:<capability-id> status=<status> -->
```

## Capability Matrix

| Capability | Limux status | cmux behavior to match | Current Limux behavior | Next parity step |
|---|---|---|---|---|
| Notification rings | partial <!-- cmux-parity:notification-rings status=partial --> | Panes get attention rings and sidebar tabs light up when agents need attention. | Limux has `limux notify`, libadwaita toast, sidebar unread badge plumbing, and live pane/tab attention CSS rings for targeted notifications. | Add an Xvfb smoke assertion for pane/tab attention visuals before marking complete. |
| Notification panel | partial <!-- cmux-parity:notification-panel status=partial --> | Dedicated panel lists pending notifications and can jump to the latest unread item. | Limux exposes toasts, unread workspace state, pane/tab-targeted live notification rows via `notification.list`/`notification.clear`, and `notification.jump`/`limux jump-notification` for latest-unread or by-id focusing. A dedicated in-app panel UI remains open. | Add panel UI and live smoke coverage. |
| Scriptable browser | partial <!-- cmux-parity:scriptable-browser status=partial --> | Browser panes can be driven through CLI/socket APIs for accessibility tree snapshots, element refs, click/fill/type/select/check, JS eval, cookies/storage, tab lifecycle, screenshots, and URL control. | The live GTK bridge now supports browser split creation, navigation, back/forward/reload, webview focus/focus-state lookup, URL lookup, title lookup, JS eval with JSON-compatible value serialization, same-origin frame selection, DOM/ARIA-derived snapshots, visible viewport PNG screenshots via `browser.screenshot`, `browser.find.*`, selector or in-memory `@eN` ref click/fill/type/check/uncheck/select/focus/hover/dblclick/scroll/scroll-into-view/keyboard actions, page-level `browser.cookies.{get,set,clear}`, `browser.storage.{get,set,clear}`, `browser.tab.{list,new,switch,close}`, `browser.get.{text,html,value,attr,count,box,styles}`, and `browser.wait` polling readiness checks with timeout support for WebKitGTK browser surfaces. Full accessibility-tree parity, full frame-stable refs, and browser profile import remain open. | Add profile import, richer smokes, and full accessibility-tree parity. |
| Browser split | partial <!-- cmux-parity:browser-split status=partial --> | Browser can open alongside terminals as a split surface. | Limux documents browser splits and the live GTK bridge now routes `browser.open_split` to create a browser-only split pane. | Add live smoke coverage and right-neighbor reuse parity before marking complete. |
| Vertical and horizontal tabs | partial <!-- cmux-parity:tabs-and-splits status=partial --> | Sidebar vertical workspace tabs plus horizontal/vertical split panes and surface tabs. | Limux has workspaces, split panes, and tabbed terminals. | Confirm cmux navigation semantics and add shortcut-level regression coverage where feasible. |
| Sidebar metadata | partial <!-- cmux-parity:sidebar-metadata status=partial --> | Sidebar shows git branch, linked PR status/number, working directory, listening ports, and latest notification text. | Limux live bridge exposes `sidebar.state` and `limux sidebar-state` prefers it, returning workspace cwd, best-effort git branch, linked PR number/status via `gh pr view`, listening ports tied to workspace processes via `ss` + `/proc/<pid>/cwd`, unread state, and latest sidebar notification text. Workspace rows now render cwd, git branch, and command-refreshed PR/port summaries in a compact metadata line. Live smoke coverage remains open. | Add live smoke coverage for metadata rendering and decide whether PR/port refresh should run periodically. |
| SSH workspaces | partial <!-- cmux-parity:ssh-workspaces status=partial --> | `cmux ssh user@remote` creates a remote workspace; browser panes route through the remote network; image drag uploads via `scp`. | `limux ssh [--cwd <path>] [--name <workspace>] [--] <ssh-args...>` creates a workspace and launches OpenSSH in the first terminal. Remote browser routing and image drag/upload are not implemented. | Add remote browser network routing, `scp` upload flow, and live smoke coverage. |
| Agent teams | partial <!-- cmux-parity:agent-teams status=partial --> | Claude Code Teams spawn as native splits with sidebar metadata and notifications. | Limux has `limux agent-team --agents codex,claude[,opencode,gemini]`, workspace spawning, generated `AGENTS.md`, and by-name send. | Align UX with cmux team launch semantics and add missing sidebar metadata. |
| Browser import | missing <!-- cmux-parity:browser-import status=missing --> | Browser panes can import cookies, history, and sessions from Chrome, Firefox, Arc, and other browsers. | No documented Limux equivalent. | Scope browser profile storage, import permissions, and WebKitGTK cookie/session APIs. |
| Custom commands | missing <!-- cmux-parity:custom-commands status=missing --> | Project-specific commands in `cmux.json` launch from the command palette. | No documented Limux equivalent. | Define a Linux-compatible config schema only after command palette/settings direction is settled. |
| Scriptable terminal API | partial <!-- cmux-parity:scriptable-terminal-api status=partial --> | CLI and socket API can create workspaces, split panes, send keystrokes, and inspect/control terminal surfaces. | Limux live bridge supports workspace list/create/select/rename/close, pane/surface list, terminal pane create, text/key send, read text, health, and notifications. | Fill remaining dispatcher parity gaps and document stable API compatibility. |
| Native Linux app | complete <!-- cmux-parity:native-linux-app status=complete --> | cmux is a native macOS AppKit app rather than Electron. | Limux is GTK4/libadwaita, not Electron. | Keep UI work native and avoid adding web-shell dependencies. |
| Ghostty compatibility | partial <!-- cmux-parity:ghostty-compatibility status=partial --> | Reads existing Ghostty config for themes, fonts, and colors. | Limux embeds Ghostty rendering through `libghostty.so`; Ghostty config parity is not documented as complete. | Audit Ghostty config loading and document supported keys. |
| GPU acceleration | complete <!-- cmux-parity:gpu-acceleration status=complete --> | Powered by libghostty for GPU-accelerated terminal rendering. | Limux uses embedded Ghostty with OpenGL rendering. | Preserve GPU path in packaging and smoke tests. |
| Session restore | partial <!-- cmux-parity:session-restore status=partial --> | Restores layout, working directories, terminal scrollback best effort, browser URL/history, and supported agent sessions. | Limux documents workspace persistence and hook/session behavior, but not full cmux restore semantics. | Split restore requirements into app layout, terminal scrollback, browser state, and agent resume tests. |
| Agent resume hooks | partial <!-- cmux-parity:agent-resume-hooks status=partial --> | Hooks/resume integrations cover Claude Code, Codex, Grok, OpenCode, Pi, Amp, Cursor CLI, Gemini, Rovo Dev, Copilot, CodeBuddy, Factory, and Qoder. | Limux README documents Codex, Claude Code, and Gemini hooks; OpenCode templates are omitted until ready. | Expand supported agents deliberately, with dry-run tests and generated hook template checks. |
| Keyboard shortcuts | partial <!-- cmux-parity:keyboard-shortcuts status=partial --> | cmux provides workspace, surface, split, browser, notification, find, terminal, and window shortcuts with macOS conventions and customization. | Limux maps default behavior to Linux `Ctrl`/`Alt`/`Meta` conventions and documents app/browser/find/notification/terminal/workspace shortcuts, including `Ctrl+W` for focused-tab close, `Ctrl+Alt+W` for focused-pane close, and `Ctrl+Alt+J` for latest-notification jump. | Add command palette/settings and remaining remapping parity decisions. |
| Distribution and updates | partial <!-- cmux-parity:distribution-updates status=partial --> | DMG/Homebrew installs and Sparkle auto-updates; nightly app has separate bundle/update feed. | Limux ships `.deb`, AppImage, tarball, and AUR package, with no documented auto-update channel. | Decide Linux update story: distro packages only, AppImage update metadata, Flatpak, or none. |

## Parity Rules

- Keep changes rebaseable onto upstream Limux, but optimize this branch for
  the personal cmux-style Linux workflow.
- Upstream PRs are optional and should be limited to mature, self-contained
  changes with clear maintainer traction.
- Reimplement cmux behavior from public behavior and docs; do not copy GPL
  cmux implementation code into Limux's MIT-licensed codebase.
- A feature is not `complete` until the user-visible behavior is documented
  and has a regression test, smoke test, or explicit manual verification note.
- Browser automation is the highest-impact missing capability because it blocks
  agent workflows that depend on inspecting and controlling web UIs from inside
  the terminal workspace.
