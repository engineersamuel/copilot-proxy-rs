#!/usr/bin/env bash
set -euo pipefail

deadline=$((SECONDS + 120))
while ((SECONDS < deadline)); do
  if status="$(process-compose process list -U --output wide)" &&
    awk '$2 == "copilot-proxy-rs" && $4 == "Running" && $6 == "Ready" { ready = 1 }
         END { exit !ready }' <<< "$status"; then
    printf '%s\n' "$status"
    exit 0
  fi
  sleep 1
done

echo "Timed out after 120s waiting for copilot-proxy-rs to be Running/Ready; inspect mise run proxy:logs." >&2
exit 1
