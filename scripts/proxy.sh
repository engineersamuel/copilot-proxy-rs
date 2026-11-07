#!/usr/bin/env bash
# Operate the proxy under process-compose. Control goes only through a unix socket.
set -euo pipefail

# Ignore every inherited PC_* variable (an outer supervisor may export PC_SOCKET_PATH).
for var in $(compgen -e | grep '^PC_' || true); do
  unset "$var"
done

repo="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd -P)"
cd "$repo"
export COPILOT_PROXY_RS_PORT="${COPILOT_PROXY_RS_PORT:-8080}"
sock="${COPILOT_PROXY_RS_PC_SOCKET:-$repo/.run/pc-$COPILOT_PROXY_RS_PORT.sock}"

pc() {
  process-compose -U -u "$sock" "$@"
}

# Run pc with a time bound; process-compose has no client timeout flag.
pc_bounded() {
  local rc=0 pid watcher
  pc "$@" &
  pid=$!
  (sleep 5 && kill "$pid") >/dev/null 2>&1 &
  watcher=$!
  wait "$pid" || rc=$?
  kill "$watcher" 2>/dev/null || true
  return "$rc"
}

is_running() {
  [[ -S "$sock" ]] && pc_bounded list >/dev/null 2>&1
}

up() {
  if is_running; then
    echo "proxy already running (socket $sock)"
    return 0
  fi
  cargo build --locked
  mkdir -p .run
  rm -f "$sock"
  pc up -D -f process-compose.yaml -L ".run/pc-$COPILOT_PROXY_RS_PORT.log"
  for _ in $(seq 60); do
    if pc_bounded process list -o json 2>/dev/null | grep -q '"is_ready": "Ready"'; then
      echo "proxy ready on 127.0.0.1:$COPILOT_PROXY_RS_PORT"
      return 0
    fi
    sleep 1
  done
  echo "proxy not ready after 60s; see .run/proxy-$COPILOT_PROXY_RS_PORT.log" >&2
  return 1
}

down() {
  pc down 2>/dev/null || true
}

case "${1:-}" in
  up) up ;;
  down) down ;;
  restart) down && up ;;
  status) pc process list -o wide ;;
  logs) pc process logs proxy ;;
  *)
    echo "usage: $0 {up|down|restart|status|logs}" >&2
    exit 2
    ;;
esac
