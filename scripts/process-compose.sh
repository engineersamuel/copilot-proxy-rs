#!/usr/bin/env bash
# Operate the proxy under process-compose. Control is a unix socket only.
set -euo pipefail

cd "$(dirname "${BASH_SOURCE[0]}")/.."

port="${COPILOT_PROXY_RS_PORT:-8080}"
socket="${COPILOT_PROXY_RS_PC_SOCKET:-${TMPDIR:-/tmp}/copilot-proxy-rs-${port}.sock}"
export COPILOT_PROXY_RS_PORT="$port"
# Never reuse a socket inherited from another supervisor.
unset PC_SOCKET_PATH

pc() { process-compose "$@" -U -u "$socket"; }

# Run "$@" and kill it after $1 seconds; process-compose clients have no timeout flag.
bounded() {
  local secs="$1" pid watcher rc=0
  shift
  "$@" &
  pid=$!
  (sleep "$secs" && kill "$pid") >/dev/null 2>&1 &
  watcher=$!
  wait "$pid" || rc=$?
  kill "$watcher" >/dev/null 2>&1 || true
  return "$rc"
}

port_in_use() { (exec 3<>"/dev/tcp/127.0.0.1/$port") 2>/dev/null; }

up() {
  if port_in_use; then
    echo "error: 127.0.0.1:$port is already in use; refusing to start" >&2
    exit 1
  fi
  if [[ -S "$socket" ]]; then
    if bounded 5 pc process list >/dev/null 2>&1; then
      echo "error: process-compose already runs on $socket" >&2
      exit 1
    fi
    rm -f "$socket"
  fi
  cargo build --release --locked
  pc up -D -f process-compose.yaml
  for _ in $(seq 1 120); do
    if curl -fsS --max-time 2 "http://127.0.0.1:$port/health" >/dev/null 2>&1; then
      echo "copilot-proxy-rs is healthy on 127.0.0.1:$port (socket $socket)"
      return 0
    fi
    sleep 1
  done
  echo "error: /health on port $port not OK after 120s; stopping process-compose" >&2
  bounded 30 pc down || true
  rm -f "$socket"
  exit 1
}

down() {
  if [[ ! -S "$socket" ]]; then
    echo "process-compose is not running on $socket"
    return 0
  fi
  bounded 30 pc down || return 1
  rm -f "$socket"
}

case "${1:-}" in
  up) up ;;
  down) down ;;
  restart) down && up ;;
  status) bounded 10 pc process list -o wide ;;
  logs) pc process logs copilot-proxy-rs -n 200 -f ;;
  *)
    echo "usage: $0 {up|down|restart|status|logs}" >&2
    exit 2
    ;;
esac
