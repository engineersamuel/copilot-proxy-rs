#!/usr/bin/env bash
set -euo pipefail

port="${COPILOT_PROXY_RS_BENCH_PORT:-19091}"
runs="${COPILOT_PROXY_RS_BENCH_RUNS:-10}"
warmup="${COPILOT_PROXY_RS_BENCH_WARMUP:-2}"
bin="${COPILOT_PROXY_RS_BENCH_BIN:-target/release/copilot-proxy-rs}"
base_url="http://127.0.0.1:${port}"

if ! command -v curl >/dev/null 2>&1; then
  echo "curl is required" >&2
  exit 1
fi

if ! command -v python3 >/dev/null 2>&1; then
  echo "python3 is required for timing and summary statistics" >&2
  exit 1
fi

if curl -fsS --max-time 1 "${base_url}/health" >/dev/null 2>&1; then
  echo "benchmark port ${port} already has a responsive proxy; choose another port with COPILOT_PROXY_RS_BENCH_PORT" >&2
  exit 1
fi

cargo build --release --locked >/dev/null

if [[ ! -x "${bin}" ]]; then
  echo "benchmark binary not found or not executable: ${bin}" >&2
  exit 1
fi

tmpdir="$(mktemp -d)"
pid=""

cleanup() {
  if [[ -n "${pid}" ]] && kill -0 "${pid}" >/dev/null 2>&1; then
    kill "${pid}" >/dev/null 2>&1 || true
    wait "${pid}" >/dev/null 2>&1 || true
  fi
  rm -rf "${tmpdir}"
}
trap cleanup EXIT

timestamp_ms() {
  python3 - <<'PY'
import time
print(time.perf_counter_ns() // 1_000_000)
PY
}

total=$((runs + warmup))
csv="${tmpdir}/results.csv"
printf "run,startup_ms,rss_kib\n" >"${csv}"

for run in $(seq 1 "${total}"); do
  run_config="${tmpdir}/config-${run}"
  mkdir -p "${run_config}"
  start_ms="$(timestamp_ms)"
  COPILOT_PROXY_RS_CONFIG_DIR="${run_config}" \
    COPILOT_PROXY_RS_PORT="${port}" \
    RUST_LOG=error \
    "${bin}" >"${tmpdir}/run-${run}.log" 2>&1 &
  pid="$!"

  ready="false"
  for _ in $(seq 1 500); do
    if curl -fsS --max-time 1 "${base_url}/health" >/dev/null 2>&1; then
      ready="true"
      break
    fi
    sleep 0.01
  done

  if [[ "${ready}" != "true" ]]; then
    echo "proxy did not become ready; log follows" >&2
    cat "${tmpdir}/run-${run}.log" >&2
    exit 1
  fi

  ready_ms="$(timestamp_ms)"
  startup_ms=$((ready_ms - start_ms))
  sleep 0.2
  rss_kib="$(ps -o rss= -p "${pid}" | tr -d '[:space:]')"
  printf "%s,%s,%s\n" "${run}" "${startup_ms}" "${rss_kib}" >>"${csv}"

  kill "${pid}" >/dev/null 2>&1 || true
  wait "${pid}" >/dev/null 2>&1 || true
  pid=""
done

python3 - "${csv}" "${warmup}" "${runs}" "${port}" <<'PY'
import csv
import math
import platform
import statistics
import subprocess
import sys

csv_path, warmup, runs, port = sys.argv[1], int(sys.argv[2]), int(sys.argv[3]), sys.argv[4]
rows = []
with open(csv_path, newline="") as handle:
    for row in csv.DictReader(handle):
        if int(row["run"]) > warmup:
            rows.append({
                "startup_ms": int(row["startup_ms"]),
                "rss_kib": int(row["rss_kib"]),
            })

startup = [row["startup_ms"] for row in rows]
rss_mib = [row["rss_kib"] / 1024 for row in rows]
rustc = subprocess.run(["rustc", "--version"], check=False, text=True, capture_output=True).stdout.strip()

def p95(values):
    return sorted(values)[max(0, math.ceil(len(values) * 0.95) - 1)]

print("## Benchmark result")
print()
print(f"- Command: `COPILOT_PROXY_RS_BENCH_PORT={port} ./scripts/benchmark-proxy.sh`")
print(f"- Runs: {runs} measured after {warmup} warmup")
print(f"- Platform: {platform.platform()}")
print(f"- Rust: {rustc}")
print()
print("| Metric | Median | Mean | p95 | Min | Max |")
print("| --- | ---: | ---: | ---: | ---: | ---: |")
print(f"| Startup to `/health` | {statistics.median(startup):.0f} ms | {statistics.mean(startup):.1f} ms | {p95(startup):.0f} ms | {min(startup):.0f} ms | {max(startup):.0f} ms |")
print(f"| Idle RSS after readiness | {statistics.median(rss_mib):.1f} MiB | {statistics.mean(rss_mib):.1f} MiB | {p95(rss_mib):.1f} MiB | {min(rss_mib):.1f} MiB | {max(rss_mib):.1f} MiB |")
PY
