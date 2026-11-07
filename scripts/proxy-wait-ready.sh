#!/usr/bin/env bash
# Usage: proxy-wait-ready.sh [old_pid]; with old_pid, a Ready row with that PID is stale.
set -euo pipefail

old_pid="${1:-0}"
deadline=$((SECONDS + 120))
while ((SECONDS < deadline)); do
  if status="$(process-compose process list -U -o json)" &&
    jq -e --argjson old "$old_pid" '.[] | select(.name == "copilot-proxy-rs" and .status == "Running" and .is_ready == "Ready" and .pid != $old)' <<< "$status" >/dev/null; then
    process-compose process list -U --output wide
    exit 0
  fi
  sleep 1
done

echo "Timed out after 120s waiting for copilot-proxy-rs to be Running/Ready; inspect mise run proxy:logs." >&2
exit 1
