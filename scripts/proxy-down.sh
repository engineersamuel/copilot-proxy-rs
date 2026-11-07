#!/usr/bin/env bash
set -euo pipefail

# shellcheck source=scripts/proxy-socket.sh
source "$(dirname "${BASH_SOURCE[0]}")/proxy-socket.sh"

if ! process-compose process list -U >/dev/null 2>&1; then
  rm -f "$PC_SOCKET_PATH"
  echo "proxy is not running"
  exit 0
fi
process-compose down -U
