#!/usr/bin/env bash
set -euo pipefail

cd "$(dirname "${BASH_SOURCE[0]}")/.."
repo_root=$(pwd -P)
worktree_id=$(printf '%s' "$repo_root" | shasum -a 256 | cut -c1-16)
socket=${COPILOT_PROXY_RS_PC_SOCKET:-${TMPDIR:-/tmp}/cpr-${UID}-${worktree_id}.sock}
if [[ "$socket" != /* || ${#socket} -ge 100 ]]; then
    echo "COPILOT_PROXY_RS_PC_SOCKET must be an absolute path shorter than 100 characters" >&2
    exit 1
fi

# Never let a surrounding supervisor select our control socket or log files.
unset PC_SOCKET_PATH
export PC_LOG_FILE="${socket}.log"
export PC_CLIENT_LOG_FILE="${socket}.client.log"
pc=(process-compose -U --unix-socket "$socket")

# Version 1.124.0 has no client timeout flag; bound each control request in Bash.
bounded() {
    "$@" &
    local pid=$!
    local tick
    for ((tick = 0; tick < 100; tick++)); do
        if ! kill -0 "$pid" 2>/dev/null; then
            wait "$pid"
            return $?
        fi
        sleep 0.1
    done
    kill -KILL "$pid" 2>/dev/null || true
    wait "$pid" 2>/dev/null || true
    echo "process-compose control request timed out" >&2
    return 124
}

socket_active() {
    [[ -S "$socket" ]] || return 1
    local result=0
    curl --silent --output /dev/null --connect-timeout 1 --max-time 2 \
        --unix-socket "$socket" http://localhost/ || result=$?
    case "$result" in
        0) return 0 ;;
        7) rm -f -- "$socket"; return 1 ;;
        *)
            echo "Cannot probe process-compose socket (curl exit $result); leaving it intact" >&2
            exit "$result"
            ;;
    esac
}

wait_ready() {
    local deadline=$((SECONDS + 600))
    local state
    while ((SECONDS < deadline)); do
        if state=$(bounded "${pc[@]}" process list -o json); then
            if grep -Eq '"is_ready":[[:space:]]*"Ready"' <<< "$state" &&
                grep -Eq '"status":[[:space:]]*"Running"' <<< "$state"; then
                echo "proxy is Ready"
                return 0
            fi
        fi
        sleep 2
    done
    echo "proxy did not become Ready within 600 seconds; inspect proxy:logs or run proxy:down" >&2
    return 1
}

command=${1:-}
shift || true
case "$command" in
    up)
        if ! socket_active; then
            # Client subcommands reject -f; only startup loads configuration.
            bounded "${pc[@]}" -f process-compose.yaml -D --tui=false
        fi
        wait_ready
        ;;
    down)
        if socket_active; then
            bounded "${pc[@]}" down
            for ((attempt = 0; attempt < 50; attempt++)); do
                if ! socket_active; then
                    exit 0
                fi
                sleep 0.1
            done
            echo "process-compose did not release its socket after shutdown" >&2
            exit 1
        fi
        ;;
    restart)
        bounded "${pc[@]}" process restart proxy
        wait_ready
        ;;
    status) bounded "${pc[@]}" process list -o wide ;;
    tui) exec "${pc[@]}" attach 2> >(grep -v 'process compose config home' >&2) ;;
    logs)
        for arg in "$@"; do
            # Only an explicit follow may stream indefinitely.
            case "$arg" in
                -f | --follow | --follow=true) exec "${pc[@]}" process logs proxy --tail 100 "$@" ;;
            esac
        done
        bounded "${pc[@]}" process logs proxy --tail 100 "$@"
        ;;
    *)
        echo "Usage: $0 {up|down|restart|status|tui|logs [process-compose log options]}" >&2
        exit 2
        ;;
esac
