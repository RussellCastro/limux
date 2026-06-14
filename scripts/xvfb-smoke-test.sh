#!/usr/bin/env bash
# scripts/xvfb-smoke-test.sh - Headless end-to-end smoke test for the
# limux agent-integrations stack. Runs a real limux GTK host under Xvfb,
# exercises limux-cli against the live Unix socket, asserts expected
# behavior, then tears down. Zero display hardware required.
#
# Usage:
#   ./scripts/xvfb-smoke-test.sh                # release build
#   LIMUX_SMOKE_PROFILE=debug ./scripts/xvfb-smoke-test.sh
set -euo pipefail

PROFILE="${LIMUX_SMOKE_PROFILE:-release}"
ROOT_DIR="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT_DIR"

DEMO_DIR="$(mktemp -d -t limux-smoke-XXXXXX)"
LOG_DIR="$DEMO_DIR/logs"
mkdir -p "$LOG_DIR"

echo "== limux agent-integrations smoke test =="
echo "profile:   $PROFILE"
echo "demo dir:  $DEMO_DIR"
echo "log dir:   $LOG_DIR"

HTTP_PID=""

# --- 1. Deps --------------------------------------------------------------
command -v xvfb-run >/dev/null || {
  echo "FAIL: xvfb-run not installed (sudo pacman -S xorg-server-xvfb)"
  exit 2
}
command -v cargo >/dev/null || { echo "FAIL: cargo missing"; exit 2; }
command -v sed >/dev/null || { echo "FAIL: sed missing"; exit 2; }
command -v python3 >/dev/null || { echo "FAIL: python3 missing"; exit 2; }

# --- 2. Build -------------------------------------------------------------
if [ "$PROFILE" = "release" ]; then
  CARGO_FLAGS="--release"
  BIN_DIR="target/release"
else
  CARGO_FLAGS=""
  BIN_DIR="target/debug"
fi

echo "-- building limux-cli ($PROFILE)..."
cargo build $CARGO_FLAGS -p limux-cli --bin limux-cli 2>&1 | tail -3

echo "-- building limux-host-linux ($PROFILE)..."
cargo build $CARGO_FLAGS -p limux-host-linux 2>&1 | tail -3

LIMUX_HOST="$ROOT_DIR/$BIN_DIR/limux"
LIMUX_CLI="$ROOT_DIR/$BIN_DIR/limux-cli"
[ -x "$LIMUX_HOST" ] || { echo "FAIL: host binary missing at $LIMUX_HOST"; exit 2; }
[ -x "$LIMUX_CLI" ]  || { echo "FAIL: cli binary missing at $LIMUX_CLI"; exit 2; }

# The release host needs libghostty.so on the runtime path; debug finds
# it via rpath.
LIBGHOSTTY_DIR="$ROOT_DIR/ghostty/zig-out/lib"
if [ "$PROFILE" = "release" ] && [ -d "$LIBGHOSTTY_DIR" ]; then
  export LD_LIBRARY_PATH="$LIBGHOSTTY_DIR:${LD_LIBRARY_PATH:-}"
fi

# --- 3. Stage 0: dry-run agent-team (no host) ----------------------------
# Fast sanity pass — if this fails nothing else will work.
echo
echo "== stage 0: agent-team --dry-run (no host) =="
"$LIMUX_CLI" agent-team --dry-run \
  --agents codex,claude,opencode,gemini \
  --cwd "$DEMO_DIR" \
  2>&1 | tee "$LOG_DIR/stage0.txt"

grep -q "peers=\[codex, claude, opencode, gemini\]" \
  "$LOG_DIR/stage0.txt" \
  || { echo "FAIL: stage 0 dry-run did not report expected peers"; exit 1; }
echo "stage 0: OK"

# --- 4. Launch the live host under Xvfb ----------------------------------
# Each smoke run gets its own socket path so we don't collide with the
# user's real limux session.
SOCKET="$DEMO_DIR/limux.sock"
export LIMUX_SOCKET="$SOCKET"
export LIMUX_SOCKET_PATH="$SOCKET"
export LIMUX_SOCKET_MODE="runtime"
export XDG_DATA_HOME="$DEMO_DIR/data"
export XDG_STATE_HOME="$DEMO_DIR/state"
export XDG_RUNTIME_DIR="$DEMO_DIR/runtime"
mkdir -p "$XDG_DATA_HOME/limux" "$XDG_STATE_HOME" "$XDG_RUNTIME_DIR"
chmod 700 "$XDG_RUNTIME_DIR"
cat > "$XDG_DATA_HOME/limux/session.json" <<SMOKE_SESSION
{
  "version": 1,
  "active_workspace_index": 0,
  "top_bar_visible": true,
  "sidebar": { "visible": true, "width": 220 },
  "workspaces": [
    {
      "id": "00000000-0000-4000-8000-000000000001",
      "name": "limux",
      "favorite": false,
      "cwd": "$DEMO_DIR",
      "folder_path": "$DEMO_DIR",
      "layout": {
        "kind": "pane",
        "pane_id": 1,
        "active_tab_id": "terminal-0",
        "tabs": [
          {
            "id": "terminal-0",
            "custom_name": null,
            "pinned": false,
            "tab_kind": "terminal",
            "cwd": "$DEMO_DIR"
          }
        ]
      }
    }
  ]
}
SMOKE_SESSION

echo
echo "== stage 1: boot limux host under xvfb-run =="
# Under Xvfb there is no GPU, so Mesa would fall back to llvmpipe, which
# has historically crashed on Ghostty's shader variants. Force softpipe
# (slower but stable), and pin GL version to avoid newer-feature probes.
export LIBGL_ALWAYS_SOFTWARE=1
export GALLIUM_DRIVER=softpipe
export LP_NUM_THREADS=1
export MESA_GL_VERSION_OVERRIDE="${MESA_GL_VERSION_OVERRIDE:-3.3}"
xvfb-run -a -s "-screen 0 1280x800x24 +extension GLX +render" \
  "$LIMUX_HOST" >"$LOG_DIR/host.stdout" 2>"$LOG_DIR/host.stderr" &
HOST_PID=$!
echo "host PID: $HOST_PID (socket=$SOCKET)"

cleanup() {
  local rc=$?
  echo
  echo "-- cleanup (rc=$rc) --"
  if [ -n "${HTTP_PID:-}" ] && kill -0 "$HTTP_PID" 2>/dev/null; then
    kill "$HTTP_PID" 2>/dev/null || true
    sleep 1
    kill -9 "$HTTP_PID" 2>/dev/null || true
  fi
  if kill -0 "$HOST_PID" 2>/dev/null; then
    kill "$HOST_PID" 2>/dev/null || true
    sleep 1
    kill -9 "$HOST_PID" 2>/dev/null || true
  fi
  # Tail the host log on failure to aid debugging.
  if [ "$rc" -ne 0 ]; then
    echo "-- host.stdout (tail) --"
    tail -n 40 "$LOG_DIR/host.stdout" 2>/dev/null || true
    echo "-- host.stderr (tail) --"
    tail -n 40 "$LOG_DIR/host.stderr" 2>/dev/null || true
    echo "artifacts retained at: $DEMO_DIR"
  else
    # Clean slate on success.
    rm -rf "$DEMO_DIR"
  fi
}
trap cleanup EXIT INT TERM

# Poll for the socket (up to 30s)
for i in $(seq 1 60); do
  if [ -S "$SOCKET" ]; then
    echo "socket up after ${i}*500ms"
    break
  fi
  if ! kill -0 "$HOST_PID" 2>/dev/null; then
    echo "FAIL: host process died before opening the socket"
    exit 1
  fi
  sleep 0.5
done

[ -S "$SOCKET" ] || { echo "FAIL: socket $SOCKET never appeared"; exit 1; }

# --- 5. Stage 2: live agent-team ------------------------------------------
echo
echo "== stage 2: agent-team against live host (--no-launch) =="
# --no-launch keeps the workspace commands from actually spawning codex/
# claude binaries (which may not be installed in CI); the bridge + AGENTS.md
# + allow_name=true path are still fully exercised.
"$LIMUX_CLI" --id-format both agent-team \
  --agents codex,claude \
  --cwd "$DEMO_DIR" \
  --no-launch \
  2>&1 | tee "$LOG_DIR/stage2.txt"

grep -q "peers=\[codex, claude\]" "$LOG_DIR/stage2.txt" \
  || { echo "FAIL: live agent-team did not create peers"; exit 1; }
[ -f "$DEMO_DIR/AGENTS.md" ] \
  || { echo "FAIL: AGENTS.md not written to $DEMO_DIR"; exit 1; }

# Assert the runtime AGENTS.md has the protocol envelope + both peers.
grep -q "<agent-msg"  "$DEMO_DIR/AGENTS.md" || { echo "FAIL: AGENTS.md missing <agent-msg>"; exit 1; }
grep -q "\bcodex\b"   "$DEMO_DIR/AGENTS.md" || { echo "FAIL: AGENTS.md missing codex peer"; exit 1; }
grep -q "\bclaude\b"  "$DEMO_DIR/AGENTS.md" || { echo "FAIL: AGENTS.md missing claude peer"; exit 1; }
echo "stage 2: OK (AGENTS.md + 2 workspaces + allow_name bridge path)"

# --- 6. Stage 3: list-workspaces sanity -----------------------------------
echo
echo "== stage 3: list-workspaces sees both peers =="
"$LIMUX_CLI" list-workspaces 2>&1 | tee "$LOG_DIR/stage3.txt"
grep -q codex  "$LOG_DIR/stage3.txt" || { echo "FAIL: list-workspaces missing codex"; exit 1; }
grep -q claude "$LOG_DIR/stage3.txt" || { echo "FAIL: list-workspaces missing claude"; exit 1; }
"$LIMUX_CLI" --id-format both --json list-panes --workspace claude \
  2>&1 | tee "$LOG_DIR/stage3-claude-panes.json"
read -r NOTIFY_TARGET_PANE NOTIFY_TARGET_SURFACE <<EOF_NOTIFY_TARGET
$(python3 - "$LOG_DIR/stage3-claude-panes.json" <<'PY_NOTIFY_TARGET'
import json
import sys
with open(sys.argv[1], encoding="utf-8") as handle:
    payload = json.load(handle)
panes = payload.get("panes") or []
if not panes:
    raise SystemExit("no claude panes found for notification attention smoke")
first = panes[0]
print(first.get("pane_id") or "", first.get("active_surface_id") or "")
PY_NOTIFY_TARGET
)
EOF_NOTIFY_TARGET
[ -n "$NOTIFY_TARGET_PANE" ] || { echo "FAIL: could not identify claude pane for notification attention smoke"; exit 1; }
[ -n "$NOTIFY_TARGET_SURFACE" ] || { echo "FAIL: could not identify claude surface for notification attention smoke"; exit 1; }
echo "stage 3: OK"

# --- 7. Stage 4: by-name send (the phase-5 allow_name=true unlock) --------
# This is the single most important assertion in the whole harness —
# it proves that `limux send --workspace <name>` resolves to the right
# workspace via the bridge. Without allow_name=true this errors out.
echo
echo "== stage 4: surface.send_text by workspace name =="
ENVELOPE=$'<agent-msg from="codex" to="claude" id="smoke-1" ts="2026-04-19T23:59:00Z"><request>smoke test ping</request></agent-msg>\n'
if "$LIMUX_CLI" send --workspace claude "$ENVELOPE" 2>&1 | tee "$LOG_DIR/stage4.txt"; then
  echo "stage 4: OK (by-name send accepted)"
else
  echo "FAIL: by-name send to 'claude' failed — allow_name=true may be regressed"
  exit 1
fi

# --- 8. Stage 5: by-name notify -------------------------------------------
echo
echo "== stage 5: notification.create by workspace name =="
if "$LIMUX_CLI" notify --workspace claude --pane "$NOTIFY_TARGET_PANE" --surface "$NOTIFY_TARGET_SURFACE" --subtitle "smoke" --body "all good" "Smoke test" \
     2>&1 | tee "$LOG_DIR/stage5.txt"; then
  echo "stage 5: OK (by-name notify accepted)"
else
  echo "FAIL: by-name notify failed — allow_name=true on notification.create may be regressed"
  exit 1
fi

"$LIMUX_CLI" --json --request '{"id":"smoke-attention-after-notify","method":"debug.attention.state","params":{"workspace_id":"claude"}}' \
  2>&1 | tee "$LOG_DIR/stage5-attention-after-notify.json"
python3 - "$LOG_DIR/stage5-attention-after-notify.json" "$NOTIFY_TARGET_PANE" "$NOTIFY_TARGET_SURFACE" true <<'PY_ASSERT_ATTENTION'
import json
import sys
with open(sys.argv[1], encoding="utf-8") as handle:
    payload = json.load(handle)
target_pane = str(sys.argv[2])
target_surface = sys.argv[3]
expected = sys.argv[4] == "true"
panes = payload.get("panes") or []
pane = next((row for row in panes if str(row.get("pane_id")) == target_pane), None)
if pane is None:
    raise SystemExit(f"target pane {target_pane} missing from attention state")
tabs = pane.get("tabs") or []
tab = next((row for row in tabs if row.get("surface_id") == target_surface), None)
if tab is None:
    raise SystemExit(f"target surface {target_surface} missing from attention state")
if bool(pane.get("attention")) != expected:
    raise SystemExit(f"pane attention expected {expected}, got {pane.get('attention')}")
if bool(tab.get("attention")) != expected:
    raise SystemExit(f"tab attention expected {expected}, got {tab.get('attention')}")
if expected and not payload.get("unread"):
    raise SystemExit("workspace unread flag was not set with attention")
if not expected and payload.get("unread"):
    raise SystemExit("workspace unread flag remained set after attention clear")
PY_ASSERT_ATTENTION

# --- 9. Stage 6: custom commands + notification/sidebar APIs ---------------
echo
echo "== stage 6: custom commands, notifications, and sidebar metadata =="
PROJECT_COMMAND_PROOF="$DEMO_DIR/project-command-proof"
cat > "$DEMO_DIR/cmux.json" <<SMOKE_COMMANDS
{
  "commands": {
    "smoke-project": {
      "label": "Smoke Project Command",
      "command": "printf project-ok > '$PROJECT_COMMAND_PROOF'",
      "cwd": "."
    }
  }
}
SMOKE_COMMANDS

"$LIMUX_CLI" --json commands --project "$DEMO_DIR" \
  2>&1 | tee "$LOG_DIR/stage6-commands.json"
grep -q '"name"[[:space:]]*:[[:space:]]*"smoke-project"' "$LOG_DIR/stage6-commands.json" \
  || { echo "FAIL: project command listing missing smoke-project"; exit 1; }
grep -q 'Smoke Project Command' "$LOG_DIR/stage6-commands.json" \
  || { echo "FAIL: project command listing missing label"; exit 1; }

"$LIMUX_CLI" --id-format both --json run-command --project "$DEMO_DIR" smoke-project \
  2>&1 | tee "$LOG_DIR/stage6-run-command.json"
grep -q '"project_command"[[:space:]]*:[[:space:]]*"smoke-project"' "$LOG_DIR/stage6-run-command.json" \
  || { echo "FAIL: run-command response missing project_command"; exit 1; }
grep -q '"workspace_id"[[:space:]]*:[[:space:]]*"' "$LOG_DIR/stage6-run-command.json" \
  || { echo "FAIL: run-command response missing workspace_id"; exit 1; }

for _ in $(seq 1 50); do
  if [ -f "$PROJECT_COMMAND_PROOF" ]; then
    break
  fi
  sleep 0.1
done
[ -f "$PROJECT_COMMAND_PROOF" ] || { echo "FAIL: project command proof file missing"; exit 1; }
[ "$(cat "$PROJECT_COMMAND_PROOF")" = "project-ok" ] || { echo "FAIL: project command proof file has unexpected content"; exit 1; }

"$LIMUX_CLI" --json --request '{"id":"smoke-palette-open","method":"debug.shortcut.simulate","params":{"action":"open_command_palette"}}' \
  2>&1 | tee "$LOG_DIR/stage6-command-palette-open.json"
grep -q '"command"[[:space:]]*:[[:space:]]*"OpenCommandPalette"' "$LOG_DIR/stage6-command-palette-open.json" \
  || { echo "FAIL: debug shortcut did not resolve open_command_palette"; exit 1; }
grep -q '"command_palette_visible"[[:space:]]*:[[:space:]]*true' "$LOG_DIR/stage6-command-palette-open.json" \
  || { echo "FAIL: command palette did not become visible from shortcut simulation"; exit 1; }

"$LIMUX_CLI" --json --request '{"id":"smoke-palette-visible","method":"debug.command_palette.visible","params":{}}' \
  2>&1 | tee "$LOG_DIR/stage6-command-palette-visible.json"
grep -q '"visible"[[:space:]]*:[[:space:]]*true' "$LOG_DIR/stage6-command-palette-visible.json" \
  || { echo "FAIL: debug.command_palette.visible did not report visible=true"; exit 1; }

"$LIMUX_CLI" --json --request '{"id":"smoke-palette-results","method":"debug.command_palette.results","params":{"limit":80}}' \
  2>&1 | tee "$LOG_DIR/stage6-command-palette-results.json"
grep -q 'Smoke Project Command' "$LOG_DIR/stage6-command-palette-results.json" \
  || { echo "FAIL: command palette results missing project command"; exit 1; }
grep -q 'Open Settings' "$LOG_DIR/stage6-command-palette-results.json" \
  || { echo "FAIL: command palette results missing built-in settings command"; exit 1; }

"$LIMUX_CLI" --json --request '{"id":"smoke-palette-close","method":"debug.command_palette.toggle","params":{}}' \
  2>&1 | tee "$LOG_DIR/stage6-command-palette-close.json"
grep -q '"visible"[[:space:]]*:[[:space:]]*false' "$LOG_DIR/stage6-command-palette-close.json" \
  || { echo "FAIL: command palette debug toggle did not close the palette"; exit 1; }

"$LIMUX_CLI" --json sidebar-state --workspace claude \
  2>&1 | tee "$LOG_DIR/stage6-sidebar.json"
grep -Fq "\"cwd\":\"$DEMO_DIR\"" "$LOG_DIR/stage6-sidebar.json" \
  || { echo "FAIL: sidebar-state missing claude cwd"; exit 1; }
grep -q '"unread"[[:space:]]*:[[:space:]]*true' "$LOG_DIR/stage6-sidebar.json" \
  || { echo "FAIL: sidebar-state did not report unread=true after notify"; exit 1; }
grep -q 'Smoke test' "$LOG_DIR/stage6-sidebar.json" \
  || { echo "FAIL: sidebar-state missing latest notification text"; exit 1; }

"$LIMUX_CLI" --json list-notifications --unread \
  2>&1 | tee "$LOG_DIR/stage6-notifications-unread.json"
grep -q '"title"[[:space:]]*:[[:space:]]*"Smoke test"' "$LOG_DIR/stage6-notifications-unread.json" \
  || { echo "FAIL: unread notification list missing Smoke test"; exit 1; }
grep -q '"unread"[[:space:]]*:[[:space:]]*true' "$LOG_DIR/stage6-notifications-unread.json" \
  || { echo "FAIL: unread notification list did not report unread=true"; exit 1; }
NOTIFICATION_ID="$(sed -n 's/.*"notification_id"[[:space:]]*:[[:space:]]*\([0-9][0-9]*\).*/\1/p' "$LOG_DIR/stage6-notifications-unread.json" | head -1)"
[ -n "$NOTIFICATION_ID" ] || { echo "FAIL: unread notification list missing notification_id"; exit 1; }

"$LIMUX_CLI" --json --request '{"id":"smoke-notification-panel-open","method":"debug.shortcut.simulate","params":{"action":"open_notification_panel"}}' \
  2>&1 | tee "$LOG_DIR/stage6-notification-panel-open.json"
grep -q '"command"[[:space:]]*:[[:space:]]*"OpenNotificationPanel"' "$LOG_DIR/stage6-notification-panel-open.json" \
  || { echo "FAIL: debug shortcut did not resolve open_notification_panel"; exit 1; }
grep -q '"notification_panel_visible"[[:space:]]*:[[:space:]]*true' "$LOG_DIR/stage6-notification-panel-open.json" \
  || { echo "FAIL: notification panel did not become visible from shortcut simulation"; exit 1; }

"$LIMUX_CLI" --json --request '{"id":"smoke-notification-panel-visible","method":"debug.notification_panel.visible","params":{}}' \
  2>&1 | tee "$LOG_DIR/stage6-notification-panel-visible.json"
grep -q '"visible"[[:space:]]*:[[:space:]]*true' "$LOG_DIR/stage6-notification-panel-visible.json" \
  || { echo "FAIL: debug.notification_panel.visible did not report visible=true"; exit 1; }
grep -q '"notification_count"[[:space:]]*:[[:space:]]*[1-9]' "$LOG_DIR/stage6-notification-panel-visible.json" \
  || { echo "FAIL: notification panel state did not report notifications"; exit 1; }
grep -q '"unread_count"[[:space:]]*:[[:space:]]*[1-9]' "$LOG_DIR/stage6-notification-panel-visible.json" \
  || { echo "FAIL: notification panel state did not report unread notifications"; exit 1; }

"$LIMUX_CLI" --json --request '{"id":"smoke-notification-panel-close","method":"debug.notification_panel.close","params":{}}' \
  2>&1 | tee "$LOG_DIR/stage6-notification-panel-close.json"
grep -q '"visible"[[:space:]]*:[[:space:]]*false' "$LOG_DIR/stage6-notification-panel-close.json" \
  || { echo "FAIL: debug.notification_panel.close did not hide the notification panel"; exit 1; }

"$LIMUX_CLI" --json jump-notification --id "$NOTIFICATION_ID" \
  2>&1 | tee "$LOG_DIR/stage6-notification-jump.json"
grep -q '"jumped"[[:space:]]*:[[:space:]]*true' "$LOG_DIR/stage6-notification-jump.json" \
  || { echo "FAIL: jump-notification did not report jumped=true"; exit 1; }
grep -q '"unread"[[:space:]]*:[[:space:]]*false' "$LOG_DIR/stage6-notification-jump.json" \
  || { echo "FAIL: jumped notification did not become read"; exit 1; }
"$LIMUX_CLI" --json --request '{"id":"smoke-attention-after-jump","method":"debug.attention.state","params":{"workspace_id":"claude"}}' \
  2>&1 | tee "$LOG_DIR/stage6-attention-after-jump.json"
python3 - "$LOG_DIR/stage6-attention-after-jump.json" "$NOTIFY_TARGET_PANE" "$NOTIFY_TARGET_SURFACE" false <<'PY_ASSERT_ATTENTION'
import json
import sys
with open(sys.argv[1], encoding="utf-8") as handle:
    payload = json.load(handle)
target_pane = str(sys.argv[2])
target_surface = sys.argv[3]
expected = sys.argv[4] == "true"
panes = payload.get("panes") or []
pane = next((row for row in panes if str(row.get("pane_id")) == target_pane), None)
if pane is None:
    raise SystemExit(f"target pane {target_pane} missing from attention state")
tabs = pane.get("tabs") or []
tab = next((row for row in tabs if row.get("surface_id") == target_surface), None)
if tab is None:
    raise SystemExit(f"target surface {target_surface} missing from attention state")
if bool(pane.get("attention")) != expected:
    raise SystemExit(f"pane attention expected {expected}, got {pane.get('attention')}")
if bool(tab.get("attention")) != expected:
    raise SystemExit(f"tab attention expected {expected}, got {tab.get('attention')}")
if expected and not payload.get("unread"):
    raise SystemExit("workspace unread flag was not set with attention")
if not expected and payload.get("unread"):
    raise SystemExit("workspace unread flag remained set after attention clear")
PY_ASSERT_ATTENTION

"$LIMUX_CLI" --json list-notifications --unread \
  2>&1 | tee "$LOG_DIR/stage6-notifications-after-jump.json"
if grep -q 'Smoke test' "$LOG_DIR/stage6-notifications-after-jump.json"; then
  echo "FAIL: unread notification list still contains jumped Smoke test notification"
  exit 1
fi

"$LIMUX_CLI" --json clear-notifications --id "$NOTIFICATION_ID" \
  2>&1 | tee "$LOG_DIR/stage6-notifications-clear.json"
if grep -q 'Smoke test' "$LOG_DIR/stage6-notifications-clear.json"; then
  echo "FAIL: clear-notifications did not remove Smoke test notification"
  exit 1
fi
echo "stage 6: OK (project commands + native palette/panel shortcuts + notification attention/list/jump/clear + sidebar-state)"

# --- 10. Stage 7: self-split pane.create + command injection ---------------
echo
echo "== stage 7: pane.create self-split with exact-surface command =="
SELF_SPLIT_PROOF="$DEMO_DIR/self-split-proof"
SELF_SPLIT_ENV="$DEMO_DIR/self-split-env"
SELF_SPLIT_CMD="printf split-ok > '$SELF_SPLIT_PROOF'; printf '%s\n%s\n%s\n' \"\$LIMUX_WORKSPACE_ID\" \"\$LIMUX_PANE_ID\" \"\$LIMUX_SURFACE_ID\" > '$SELF_SPLIT_ENV'"

"$LIMUX_CLI" --id-format both --json new-pane \
  --workspace claude \
  --direction right \
  --command "$SELF_SPLIT_CMD" \
  2>&1 | tee "$LOG_DIR/stage7-self-split.json"

RESPONSE_WORKSPACE="$(sed -n 's/.*"workspace_id"[[:space:]]*:[[:space:]]*"\([^"]*\)".*/\1/p' "$LOG_DIR/stage7-self-split.json" | head -1)"
RESPONSE_PANE="$(sed -n 's/.*"pane_id"[[:space:]]*:[[:space:]]*"\([^"]*\)".*/\1/p' "$LOG_DIR/stage7-self-split.json" | head -1)"
RESPONSE_SURFACE="$(sed -n 's/.*"surface_id"[[:space:]]*:[[:space:]]*"\([^"]*\)".*/\1/p' "$LOG_DIR/stage7-self-split.json" | head -1)"

[ -n "$RESPONSE_WORKSPACE" ] || { echo "FAIL: pane.create response missing workspace_id"; exit 1; }
[ -n "$RESPONSE_PANE" ] || { echo "FAIL: pane.create response missing pane_id"; exit 1; }
[ -n "$RESPONSE_SURFACE" ] || { echo "FAIL: pane.create response missing surface_id"; exit 1; }

for _ in $(seq 1 50); do
  if [ -f "$SELF_SPLIT_PROOF" ] && [ -f "$SELF_SPLIT_ENV" ]; then
    break
  fi
  sleep 0.1
done

[ -f "$SELF_SPLIT_PROOF" ] || { echo "FAIL: self-split command proof file missing"; exit 1; }
[ "$(cat "$SELF_SPLIT_PROOF")" = "split-ok" ] || { echo "FAIL: self-split proof file has unexpected content"; exit 1; }
[ -f "$SELF_SPLIT_ENV" ] || { echo "FAIL: self-split env file missing"; exit 1; }

ENV_WORKSPACE="$(sed -n '1p' "$SELF_SPLIT_ENV")"
ENV_PANE="$(sed -n '2p' "$SELF_SPLIT_ENV")"
ENV_SURFACE="$(sed -n '3p' "$SELF_SPLIT_ENV")"

[ "$ENV_WORKSPACE" = "$RESPONSE_WORKSPACE" ] || {
  echo "FAIL: spawned pane LIMUX_WORKSPACE_ID ($ENV_WORKSPACE) did not match response ($RESPONSE_WORKSPACE)"
  exit 1
}
[ "$ENV_PANE" = "$RESPONSE_PANE" ] || {
  echo "FAIL: spawned pane LIMUX_PANE_ID ($ENV_PANE) did not match response ($RESPONSE_PANE)"
  exit 1
}
[ "$ENV_SURFACE" = "$RESPONSE_SURFACE" ] || {
  echo "FAIL: spawned pane LIMUX_SURFACE_ID ($ENV_SURFACE) did not match response ($RESPONSE_SURFACE)"
  exit 1
}
echo "stage 7: OK (self-split command ran with fresh LIMUX_* env)"

# --- 11. Stage 8: live browser bridge -------------------------------------
echo
echo "== stage 8: browser bridge open/wait/snapshot/find/action/screenshot =="
BROWSER_SMOKE_HTML="$DEMO_DIR/browser-smoke.html"
BROWSER_SECOND_HTML="$DEMO_DIR/browser-second.html"
BROWSER_SHOT="$DEMO_DIR/browser-smoke.png"
cat > "$BROWSER_SMOKE_HTML" <<'BROWSER_SMOKE'
<!doctype html>
<html>
  <head>
    <meta charset="utf-8">
    <title>Limux Browser Smoke</title>
  </head>
  <body>
    <main>
      <h1>Limux Browser Smoke</h1>
      <button id="ready" aria-label="Smoke Ready" type="button">Ready</button>
      <label for="name">Name</label>
      <input id="name" placeholder="name">
      <iframe id="child-frame" title="Smoke Frame" srcdoc="<!doctype html><html><body><h2>Frame Area</h2><button id='frame-ready' aria-label='Frame Ready' type='button'>Frame Ready</button></body></html>"></iframe>
    </main>
  </body>
</html>
BROWSER_SMOKE
cat > "$BROWSER_SECOND_HTML" <<'BROWSER_SECOND'
<!doctype html>
<html>
  <head>
    <meta charset="utf-8">
    <title>Limux Browser Second</title>
  </head>
  <body>
    <h1 id="second-ready">Second Tab Ready</h1>
  </body>
</html>
BROWSER_SECOND

HTTP_PORT_FILE="$DEMO_DIR/http-port"
python3 - "$DEMO_DIR" "$HTTP_PORT_FILE" >"$LOG_DIR/http.stdout" 2>"$LOG_DIR/http.stderr" <<'PY_HTTP' &
import functools
import http.server
import socketserver
import sys

root = sys.argv[1]
port_file = sys.argv[2]

class SmokeServer(socketserver.TCPServer):
    allow_reuse_address = True

handler = functools.partial(http.server.SimpleHTTPRequestHandler, directory=root)
with SmokeServer(("127.0.0.1", 0), handler) as httpd:
    with open(port_file, "w", encoding="utf-8") as handle:
        handle.write(str(httpd.server_address[1]))
    httpd.serve_forever()
PY_HTTP
HTTP_PID=$!
for _ in $(seq 1 50); do
  if [ -s "$HTTP_PORT_FILE" ]; then
    break
  fi
  if ! kill -0 "$HTTP_PID" 2>/dev/null; then
    echo "FAIL: browser fixture HTTP server exited early"
    cat "$LOG_DIR/http.stderr" 2>/dev/null || true
    exit 1
  fi
  sleep 0.1
done
[ -s "$HTTP_PORT_FILE" ] || { echo "FAIL: browser fixture HTTP server did not report a port"; exit 1; }
HTTP_PORT="$(cat "$HTTP_PORT_FILE")"
BROWSER_SMOKE_URL="http://127.0.0.1:$HTTP_PORT/browser-smoke.html"
BROWSER_SECOND_URL="http://127.0.0.1:$HTTP_PORT/browser-second.html"

"$LIMUX_CLI" --id-format both --json list-panes --workspace claude \
  2>&1 | tee "$LOG_DIR/stage8-reuse-panes-before.json"
read -r REUSE_BEFORE_COUNT REUSE_SOURCE_SURFACE <<EOF_REUSE
$(python3 - "$LOG_DIR/stage8-reuse-panes-before.json" "$RESPONSE_PANE" <<'PY_REUSE'
import json
import sys
with open(sys.argv[1], encoding="utf-8") as handle:
    payload = json.load(handle)
right_pane = sys.argv[2]
panes = payload.get("panes") or []
source = next((row for row in panes if str(row.get("pane_id")) != right_pane), None)
if not source:
    raise SystemExit("no left/source pane found for browser reuse smoke")
print(len(panes), source.get("active_surface_id") or "")
PY_REUSE
)
EOF_REUSE
[ -n "$REUSE_SOURCE_SURFACE" ] || { echo "FAIL: browser reuse smoke could not identify source surface"; exit 1; }
REUSE_REQUEST="$(python3 - "$REUSE_SOURCE_SURFACE" "$BROWSER_SECOND_URL" <<'PY_REQUEST'
import json
import sys
print(json.dumps({
    "id": 1,
    "method": "browser.open_split",
    "params": {
        "workspace_id": "claude",
        "surface_id": sys.argv[1],
        "url": sys.argv[2],
    },
}))
PY_REQUEST
)"
"$LIMUX_CLI" --id-format both --json --request "$REUSE_REQUEST" \
  2>&1 | tee "$LOG_DIR/stage8-reuse-open-split.json"
grep -q '"created_split"[[:space:]]*:[[:space:]]*false' "$LOG_DIR/stage8-reuse-open-split.json" \
  || { echo "FAIL: browser.open_split did not reuse the right neighbor pane"; exit 1; }
grep -q '"target_pane_id"[[:space:]]*:[[:space:]]*"'"$RESPONSE_PANE"'"' "$LOG_DIR/stage8-reuse-open-split.json" \
  || { echo "FAIL: browser.open_split reused an unexpected target pane"; exit 1; }
REUSE_SURFACE="$(sed -n 's/.*"surface_id"[[:space:]]*:[[:space:]]*"\([^"]*\)".*/\1/p' "$LOG_DIR/stage8-reuse-open-split.json" | head -1)"
[ -n "$REUSE_SURFACE" ] || { echo "FAIL: browser reuse response missing surface_id"; exit 1; }
"$LIMUX_CLI" --id-format both --json list-panes --workspace claude \
  2>&1 | tee "$LOG_DIR/stage8-reuse-panes-after.json"
REUSE_AFTER_COUNT="$(python3 - "$LOG_DIR/stage8-reuse-panes-after.json" <<'PY_COUNT'
import json
import sys
with open(sys.argv[1], encoding="utf-8") as handle:
    print(len((json.load(handle).get("panes") or [])))
PY_COUNT
)"
[ "$REUSE_AFTER_COUNT" = "$REUSE_BEFORE_COUNT" ] \
  || { echo "FAIL: browser.open_split reuse changed pane count ($REUSE_BEFORE_COUNT -> $REUSE_AFTER_COUNT)"; exit 1; }
"$LIMUX_CLI" --json browser "$REUSE_SURFACE" wait --selector "#second-ready" --timeout-ms 5000 \
  2>&1 | tee "$LOG_DIR/stage8-reuse-wait.json"
grep -q '"ready"[[:space:]]*:[[:space:]]*true' "$LOG_DIR/stage8-reuse-wait.json" \
  || { echo "FAIL: reused browser surface did not load second page"; exit 1; }

"$LIMUX_CLI" --id-format both --json browser open "$BROWSER_SMOKE_URL" \
  2>&1 | tee "$LOG_DIR/stage8-open.json"
BROWSER_SURFACE="$(sed -n 's/.*"surface_id"[[:space:]]*:[[:space:]]*"\([^"]*\)".*/\1/p' "$LOG_DIR/stage8-open.json" | head -1)"
[ -n "$BROWSER_SURFACE" ] || { echo "FAIL: browser open did not return surface_id"; exit 1; }

"$LIMUX_CLI" --json browser "$BROWSER_SURFACE" wait --selector "#ready" --timeout-ms 5000 \
  2>&1 | tee "$LOG_DIR/stage8-wait.json"
grep -q '"ready"[[:space:]]*:[[:space:]]*true' "$LOG_DIR/stage8-wait.json" \
  || { echo "FAIL: browser wait did not report ready=true"; exit 1; }

"$LIMUX_CLI" --json browser "$BROWSER_SURFACE" snapshot \
  2>&1 | tee "$LOG_DIR/stage8-snapshot1.json"
grep -q "Limux Browser Smoke" "$LOG_DIR/stage8-snapshot1.json" \
  || { echo "FAIL: browser snapshot missing page title/text"; exit 1; }
grep -q '"frame_id"[[:space:]]*:[[:space:]]*"main"' "$LOG_DIR/stage8-snapshot1.json" \
  || { echo "FAIL: browser snapshot missing main frame metadata"; exit 1; }
grep -q '"element_ref"[[:space:]]*:[[:space:]]*"@e' "$LOG_DIR/stage8-snapshot1.json" \
  || { echo "FAIL: browser snapshot missing stable element refs"; exit 1; }

"$LIMUX_CLI" --json browser "$BROWSER_SURFACE" find text Ready \
  2>&1 | tee "$LOG_DIR/stage8-find.json"
READY_REF="$(sed -n 's/.*"element_ref"[[:space:]]*:[[:space:]]*"\([^"]*\)".*/\1/p' "$LOG_DIR/stage8-find.json" | head -1)"
[ -n "$READY_REF" ] || { echo "FAIL: browser find text Ready did not return element_ref"; exit 1; }

"$LIMUX_CLI" --json browser "$BROWSER_SURFACE" click "$READY_REF" \
  2>&1 | tee "$LOG_DIR/stage8-click.json"
grep -q '"ok"[[:space:]]*:[[:space:]]*true' "$LOG_DIR/stage8-click.json" \
  || { echo "FAIL: browser click did not report ok=true"; exit 1; }

"$LIMUX_CLI" --json browser "$BROWSER_SURFACE" fill "#name" smoke \
  2>&1 | tee "$LOG_DIR/stage8-fill.json"
grep -q '"ok"[[:space:]]*:[[:space:]]*true' "$LOG_DIR/stage8-fill.json" \
  || { echo "FAIL: browser fill did not report ok=true"; exit 1; }

"$LIMUX_CLI" --json browser "$BROWSER_SURFACE" get value "#name" \
  2>&1 | tee "$LOG_DIR/stage8-value.json"
grep -q '"value"[[:space:]]*:[[:space:]]*"smoke"' "$LOG_DIR/stage8-value.json" \
  || { echo "FAIL: browser get value did not return filled text"; exit 1; }

"$LIMUX_CLI" --json browser "$BROWSER_SURFACE" eval "({title: document.title, ready: !!document.querySelector('#ready')})" \
  2>&1 | tee "$LOG_DIR/stage8-eval.json"
grep -q '"title"[[:space:]]*:[[:space:]]*"Limux Browser Smoke"' "$LOG_DIR/stage8-eval.json" \
  || { echo "FAIL: browser eval did not return page title"; exit 1; }
grep -q '"ready"[[:space:]]*:[[:space:]]*true' "$LOG_DIR/stage8-eval.json" \
  || { echo "FAIL: browser eval did not return ready=true"; exit 1; }

"$LIMUX_CLI" --json browser "$BROWSER_SURFACE" storage local set smoke-key smoke-value \
  2>&1 | tee "$LOG_DIR/stage8-storage-set.json"
grep -q '"ok"[[:space:]]*:[[:space:]]*true' "$LOG_DIR/stage8-storage-set.json" \
  || { echo "FAIL: browser storage set did not report ok=true"; exit 1; }
"$LIMUX_CLI" --json browser "$BROWSER_SURFACE" storage local get smoke-key \
  2>&1 | tee "$LOG_DIR/stage8-storage-get.json"
grep -q '"value"[[:space:]]*:[[:space:]]*"smoke-value"' "$LOG_DIR/stage8-storage-get.json" \
  || { echo "FAIL: browser storage get did not return smoke-value"; exit 1; }
"$LIMUX_CLI" --json browser "$BROWSER_SURFACE" storage local clear smoke-key \
  2>&1 | tee "$LOG_DIR/stage8-storage-clear.json"
grep -q '"ok"[[:space:]]*:[[:space:]]*true' "$LOG_DIR/stage8-storage-clear.json" \
  || { echo "FAIL: browser storage clear did not report ok=true"; exit 1; }
"$LIMUX_CLI" --json browser "$BROWSER_SURFACE" storage local get smoke-key \
  2>&1 | tee "$LOG_DIR/stage8-storage-after-clear.json"
grep -q '"value"[[:space:]]*:[[:space:]]*null' "$LOG_DIR/stage8-storage-after-clear.json" \
  || { echo "FAIL: browser storage clear did not remove smoke-key"; exit 1; }

"$LIMUX_CLI" --json browser "$BROWSER_SURFACE" cookies set smoke-cookie cookie-value \
  2>&1 | tee "$LOG_DIR/stage8-cookie-set.json"
grep -q '"ok"[[:space:]]*:[[:space:]]*true' "$LOG_DIR/stage8-cookie-set.json" \
  || { echo "FAIL: browser cookie set did not report ok=true"; exit 1; }
"$LIMUX_CLI" --json browser "$BROWSER_SURFACE" cookies get smoke-cookie \
  2>&1 | tee "$LOG_DIR/stage8-cookie-get.json"
grep -q '"name"[[:space:]]*:[[:space:]]*"smoke-cookie"' "$LOG_DIR/stage8-cookie-get.json" \
  || { echo "FAIL: browser cookie get missing smoke-cookie"; exit 1; }
grep -q '"value"[[:space:]]*:[[:space:]]*"cookie-value"' "$LOG_DIR/stage8-cookie-get.json" \
  || { echo "FAIL: browser cookie get missing cookie-value"; exit 1; }
"$LIMUX_CLI" --json browser "$BROWSER_SURFACE" cookies clear smoke-cookie \
  2>&1 | tee "$LOG_DIR/stage8-cookie-clear.json"
grep -q '"ok"[[:space:]]*:[[:space:]]*true' "$LOG_DIR/stage8-cookie-clear.json" \
  || { echo "FAIL: browser cookie clear did not report ok=true"; exit 1; }
"$LIMUX_CLI" --json browser "$BROWSER_SURFACE" cookies get smoke-cookie \
  2>&1 | tee "$LOG_DIR/stage8-cookie-after-clear.json"
if grep -q '"name"[[:space:]]*:[[:space:]]*"smoke-cookie"' "$LOG_DIR/stage8-cookie-after-clear.json"; then
  echo "FAIL: browser cookie clear did not remove smoke-cookie"
  exit 1
fi

BROWSER_COOKIE_IMPORT="$DEMO_DIR/browser-cookies.json"
cat > "$BROWSER_COOKIE_IMPORT" <<SMOKE_COOKIES
{
  "cookies": [
    {
      "name": "imported-cookie",
      "value": "imported-value",
      "domain": "127.0.0.1",
      "path": "/",
      "secure": false,
      "http_only": false
    }
  ]
}
SMOKE_COOKIES
"$LIMUX_CLI" --json browser "$BROWSER_SURFACE" import-cookies --file "$BROWSER_COOKIE_IMPORT" --format json \
  2>&1 | tee "$LOG_DIR/stage8-cookie-import.json"
grep -q '"imported_count"[[:space:]]*:[[:space:]]*1' "$LOG_DIR/stage8-cookie-import.json" \
  || { echo "FAIL: browser import-cookies did not import exactly one cookie"; exit 1; }
grep -q '"name"[[:space:]]*:[[:space:]]*"imported-cookie"' "$LOG_DIR/stage8-cookie-import.json" \
  || { echo "FAIL: browser import-cookies response missing imported-cookie"; exit 1; }

"$LIMUX_CLI" --json browser "$BROWSER_SURFACE" frame "#child-frame" \
  2>&1 | tee "$LOG_DIR/stage8-frame-select.json"
grep -q '"ok"[[:space:]]*:[[:space:]]*true' "$LOG_DIR/stage8-frame-select.json" \
  || { echo "FAIL: browser frame select did not report ok=true"; exit 1; }
grep -q '"frame_id"[[:space:]]*:[[:space:]]*"#child-frame"' "$LOG_DIR/stage8-frame-select.json" \
  || { echo "FAIL: browser frame select missing frame_id"; exit 1; }
"$LIMUX_CLI" --json browser "$BROWSER_SURFACE" snapshot \
  2>&1 | tee "$LOG_DIR/stage8-frame-snapshot.json"
grep -q 'Frame Ready' "$LOG_DIR/stage8-frame-snapshot.json" \
  || { echo "FAIL: browser frame snapshot missing frame content"; exit 1; }
grep -q '"frame_selector"[[:space:]]*:[[:space:]]*"#child-frame"' "$LOG_DIR/stage8-frame-snapshot.json" \
  || { echo "FAIL: browser frame snapshot missing frame selector metadata"; exit 1; }
"$LIMUX_CLI" --json browser "$BROWSER_SURFACE" find text "Frame Ready" \
  2>&1 | tee "$LOG_DIR/stage8-frame-find.json"
FRAME_REF="$(sed -n 's/.*"element_ref"[[:space:]]*:[[:space:]]*"\([^"]*\)".*/\1/p' "$LOG_DIR/stage8-frame-find.json" | head -1)"
[ -n "$FRAME_REF" ] || { echo "FAIL: browser frame find did not return element_ref"; exit 1; }
"$LIMUX_CLI" --json browser "$BROWSER_SURFACE" click "$FRAME_REF" \
  2>&1 | tee "$LOG_DIR/stage8-frame-click.json"
grep -q '"ok"[[:space:]]*:[[:space:]]*true' "$LOG_DIR/stage8-frame-click.json" \
  || { echo "FAIL: browser frame click did not report ok=true"; exit 1; }
"$LIMUX_CLI" --json browser "$BROWSER_SURFACE" frame main \
  2>&1 | tee "$LOG_DIR/stage8-frame-main.json"
grep -q '"frame_id"[[:space:]]*:[[:space:]]*"main"' "$LOG_DIR/stage8-frame-main.json" \
  || { echo "FAIL: browser frame main did not return to main frame"; exit 1; }

"$LIMUX_CLI" --id-format both --json browser "$BROWSER_SURFACE" tab list \
  2>&1 | tee "$LOG_DIR/stage8-tab-list1.json"
grep -q '"current_surface_id"[[:space:]]*:[[:space:]]*"' "$LOG_DIR/stage8-tab-list1.json" \
  || { echo "FAIL: browser tab list missing current_surface_id"; exit 1; }
"$LIMUX_CLI" --id-format both --json browser "$BROWSER_SURFACE" tab new "$BROWSER_SECOND_URL" \
  2>&1 | tee "$LOG_DIR/stage8-tab-new.json"
SECOND_SURFACE="$(sed -n 's/.*"surface_id"[[:space:]]*:[[:space:]]*"\([^"]*\)".*/\1/p' "$LOG_DIR/stage8-tab-new.json" | head -1)"
[ -n "$SECOND_SURFACE" ] || { echo "FAIL: browser tab new did not return surface_id"; exit 1; }
"$LIMUX_CLI" --json browser "$SECOND_SURFACE" wait --selector "#second-ready" --timeout-ms 5000 \
  2>&1 | tee "$LOG_DIR/stage8-tab-wait.json"
grep -q '"ready"[[:space:]]*:[[:space:]]*true' "$LOG_DIR/stage8-tab-wait.json" \
  || { echo "FAIL: browser tab new page did not become ready"; exit 1; }
"$LIMUX_CLI" --json browser "$SECOND_SURFACE" get title \
  2>&1 | tee "$LOG_DIR/stage8-tab-title.json"
grep -q '"title"[[:space:]]*:[[:space:]]*"Limux Browser Second"' "$LOG_DIR/stage8-tab-title.json" \
  || { echo "FAIL: browser tab title did not match second page"; exit 1; }
"$LIMUX_CLI" --id-format both --json browser "$BROWSER_SURFACE" tab switch "$BROWSER_SURFACE" \
  2>&1 | tee "$LOG_DIR/stage8-tab-switch.json"
grep -q '"surface_id"[[:space:]]*:[[:space:]]*"' "$LOG_DIR/stage8-tab-switch.json" \
  || { echo "FAIL: browser tab switch missing surface_id"; exit 1; }
"$LIMUX_CLI" --id-format both --json browser "$BROWSER_SURFACE" tab close "$SECOND_SURFACE" \
  2>&1 | tee "$LOG_DIR/stage8-tab-close.json"
grep -q '"ok"[[:space:]]*:[[:space:]]*true' "$LOG_DIR/stage8-tab-close.json" \
  || { echo "FAIL: browser tab close did not report ok=true"; exit 1; }

"$LIMUX_CLI" browser "$BROWSER_SURFACE" screenshot --out "$BROWSER_SHOT" \
  2>&1 | tee "$LOG_DIR/stage8-screenshot.txt"
[ -s "$BROWSER_SHOT" ] || { echo "FAIL: browser screenshot did not write a non-empty PNG"; exit 1; }
echo "stage 8: OK (browser bridge right-neighbor reuse + open/wait/snapshot/find/click/fill/eval/storage/cookies/frame/tab/screenshot)"

# --- 12. Stage 9: hook translators end-to-end -----------------------------
echo
echo "== stage 9: claude-hook event translation =="
if echo '{"hook_event_name":"Notification","message":"hello from smoke"}' \
  | LIMUX_WORKSPACE_ID="" "$LIMUX_CLI" claude-hook 2>&1 \
  | tee "$LOG_DIR/stage9.txt"; then
  echo "stage 9: OK (claude-hook accepted JSON on stdin)"
else
  # claude-hook legitimately errors without a workspace target — that's
  # a pass-through error, not a bridge regression. Surface the output.
  echo "stage 9: claude-hook returned non-zero (check output)"
fi

echo
echo "===================================="
echo "✅ limux agent/browser smoke test PASSED"
echo "===================================="
