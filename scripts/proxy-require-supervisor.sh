#!/usr/bin/env bash
set -euo pipefail

# shellcheck source=scripts/proxy-socket.sh
source "$(dirname "${BASH_SOURCE[0]}")/proxy-socket.sh"

if ! process-compose process list -U >/dev/null 2>&1; then
  echo "proxy supervisor is not running; use mise run proxy:up (if a crash left a stale socket, proxy:up clears it)" >&2
  exit 1
fi
