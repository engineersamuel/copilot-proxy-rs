#!/usr/bin/env bash
# Usage: proxy-wait-ready.sh [old_pid]; with old_pid, a Ready row with that PID is stale.
set -euo pipefail

old_pid="${1:-0}"
grace="${PROXY_WAIT_GRACE:-10}"
deadline=$((SECONDS + 120))
while ((SECONDS < deadline)); do
  if status="$(process-compose process list -U -o json 2>/dev/null)"; then
    if jq -e --argjson old "$old_pid" '.[] | select(.name == "copilot-proxy-rs" and .status == "Running" and .is_ready == "Ready" and .pid != $old)' <<< "$status" >/dev/null; then
      process-compose process list -U --output wide
      exit 0
    fi
    if jq -e '.[] | select(.name == "copilot-proxy-rs" and (.status == "Error" or .status == "Completed"))' <<< "$status" >/dev/null; then
      echo "copilot-proxy-rs exited before becoming ready; inspect mise run proxy:logs." >&2
      exit 1
    fi
  elif ((SECONDS + 120 - deadline >= grace)); then
    echo "the supervisor is not answering on its control socket; it failed to start." >&2
    exit 1
  fi
  sleep 1
done

echo "Timed out after 120s waiting for copilot-proxy-rs to be Running/Ready; inspect mise run proxy:logs." >&2
exit 1
