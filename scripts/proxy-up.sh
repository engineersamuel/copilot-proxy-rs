#!/usr/bin/env bash
set -euo pipefail
for name in $(compgen -v PC_); do unset "$name"; done
cd "$(dirname "${BASH_SOURCE[0]}")/.."

socket="${COPILOT_PROXY_RS_PC_SOCKET:?COPILOT_PROXY_RS_PC_SOCKET is required}"
config="${COPILOT_PROXY_RS_PC_CONFIG:-process-compose.yaml}"
timeout_s="${COPILOT_PROXY_RS_UP_TIMEOUT:-60}"
pc() { process-compose -U -u "$socket" -L .process-compose/process-compose.log "$@"; }

[ "${COPILOT_PROXY_RS_SKIP_BUILD:-}" = 1 ] || cargo build --release --locked
mkdir -p .process-compose "$(dirname "$socket")"

if [ -S "$socket" ]; then
    if pc project is-ready >/dev/null 2>&1; then
        echo "proxy already running"
        exit 0
    fi
    if pc process list >/dev/null 2>&1; then
        pc down >/dev/null 2>&1 || true
    fi
    rm -f "$socket"
fi

pc -f "$config" up -D --tui=false --disable-dotenv

deadline=$((SECONDS + timeout_s))
until pc project is-ready >/dev/null 2>&1; do
    if [ "$SECONDS" -ge "$deadline" ]; then
        echo "proxy not ready after ${timeout_s}s" >&2
        pc down >/dev/null 2>&1 || true
        rm -f "$socket"
        exit 1
    fi
    sleep 1
done
