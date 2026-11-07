#!/usr/bin/env bash
set -euo pipefail

# shellcheck source=scripts/proxy-socket.sh
source "$(dirname "${BASH_SOURCE[0]}")/proxy-socket.sh"

port="${COPILOT_PROXY_RS_PORT:-8080}"
host="${COPILOT_PROXY_RS_HOST:-127.0.0.1}"
socket="$PC_SOCKET_PATH"

case "$host" in
  0.0.0.0 | :: | "[::]")
    echo "COPILOT_PROXY_RS_HOST=${host} is a wildcard address the readiness probe cannot reach; use a loopback address such as 127.0.0.1 or ::1." >&2
    exit 1
    ;;
esac

# sun_path is ~104 bytes on macOS and ~108 on Linux.
if ((${#socket} > 100)); then
  echo "Control socket path is ${#socket} bytes (limit ~100): ${socket}. Set COPILOT_PROXY_RS_PC_SOCKET to a shorter path, e.g. /tmp/copilot-proxy-rs.sock." >&2
  exit 1
fi

if [[ -S "$socket" ]]; then
  if process-compose process list -U >/dev/null 2>&1; then
    echo "A supervisor is already running on ${socket}; use mise run proxy:restart or mise run proxy:down first." >&2
    exit 1
  fi
  rm -f "$socket"
fi

if (exec 3<>"/dev/tcp/${host}/${port}") 2>/dev/null; then
  echo "${host}:${port} is already in use; the readiness probe would hit that listener instead of the managed proxy. Stop it or set COPILOT_PROXY_RS_PORT." >&2
  exit 1
fi
