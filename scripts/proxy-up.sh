#!/usr/bin/env bash
set -euo pipefail
for name in $(compgen -v PC_); do unset "$name"; done
cd "$(dirname "${BASH_SOURCE[0]}")/.."

socket="${COPILOT_PROXY_RS_PC_SOCKET:?COPILOT_PROXY_RS_PC_SOCKET is required}"
config="${COPILOT_PROXY_RS_PC_CONFIG:-process-compose.yaml}"
timeout_s="${COPILOT_PROXY_RS_UP_TIMEOUT:-60}"
pc=(process-compose -U -u "$socket" -L .process-compose/process-compose.log)

# process-compose clients have no timeout flag: run one for at most 5s, 124 on timeout.
probe() {
    "${pc[@]}" "$@" >/dev/null 2>&1 &
    local pid=$! ticks=0
    while kill -0 "$pid" 2>/dev/null; do
        if [ "$ticks" -ge 50 ]; then
            kill "$pid" 2>/dev/null || true
            wait "$pid" 2>/dev/null || true
            return 124
        fi
        sleep 0.1
        ticks=$((ticks + 1))
    done
    wait "$pid"
}

mkdir -p .process-compose "$(dirname "$socket")"

if [ -S "$socket" ]; then
    rc=0
    probe project is-ready || rc=$?
    if [ "$rc" -eq 0 ]; then
        echo "proxy already running"
        exit 0
    fi
    # A socket that accepts but never answers is likely held by a live, hung
    # supervisor; replacing it could start a second one, so stop instead.
    if [ "$rc" -eq 124 ]; then
        echo "supervisor socket $socket is unresponsive; stop its process-compose, then remove it" >&2
        exit 1
    fi
    if probe process list; then
        probe down || true
    fi
    rm -f "$socket"
fi

[ "${COPILOT_PROXY_RS_SKIP_BUILD:-}" = 1 ] || cargo build --release --locked

"${pc[@]}" -f "$config" up -D --tui=false --disable-dotenv

deadline=$((SECONDS + timeout_s))
until probe project is-ready; do
    if [ "$SECONDS" -ge "$deadline" ]; then
        echo "proxy not ready after ${timeout_s}s" >&2
        probe down || true
        rm -f "$socket"
        exit 1
    fi
    sleep 1
done
