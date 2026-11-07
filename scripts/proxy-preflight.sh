#!/usr/bin/env bash
set -euo pipefail

port="${COPILOT_PROXY_RS_PORT:-8080}"
if (exec 3<>"/dev/tcp/127.0.0.1/${port}") 2>/dev/null; then
  echo "Port ${port} is already in use; the readiness probe would hit that listener instead of the managed proxy. Stop it or set COPILOT_PROXY_RS_PORT." >&2
  exit 1
fi
