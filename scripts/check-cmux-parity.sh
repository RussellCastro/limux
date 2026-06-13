#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "$0")/.." && pwd)"
CONTRACT="$ROOT_DIR/docs/cmux-parity-contract.md"

if [[ ! -f "$CONTRACT" ]]; then
  echo "missing docs/cmux-parity-contract.md" >&2
  exit 1
fi

required_ids=(
  notification-rings
  notification-panel
  scriptable-browser
  browser-split
  tabs-and-splits
  sidebar-metadata
  ssh-workspaces
  agent-teams
  browser-import
  custom-commands
  scriptable-terminal-api
  native-linux-app
  ghostty-compatibility
  gpu-acceleration
  session-restore
  agent-resume-hooks
  keyboard-shortcuts
  distribution-updates
)

status_pattern='status=(complete|partial|missing|blocked|deferred)'
failed=0

for id in "${required_ids[@]}"; do
  count="$(grep -Ec "<!-- cmux-parity:${id} ${status_pattern} -->" "$CONTRACT" || true)"
  if [[ "$count" != "1" ]]; then
    echo "expected exactly one parity marker for ${id}, found ${count}" >&2
    failed=1
  fi
done

unknown_markers="$({ grep -Eo '<!-- cmux-parity:[a-z0-9-]+ status=[a-z]+ -->' "$CONTRACT" || true; } \
  | sed -E 's/^<!-- cmux-parity:([^ ]+) status=([^ ]+) -->$/\1 \2/' \
  | while read -r id status; do
      known=0
      for required in "${required_ids[@]}"; do
        if [[ "$id" == "$required" ]]; then
          known=1
          break
        fi
      done
      if [[ "$known" == "0" || ! "$status" =~ ^(complete|partial|missing|blocked|deferred)$ ]]; then
        printf '%s status=%s\n' "$id" "$status"
      fi
    done)"

if [[ -n "$unknown_markers" ]]; then
  echo "unknown or invalid cmux parity markers:" >&2
  echo "$unknown_markers" >&2
  failed=1
fi

exit "$failed"
