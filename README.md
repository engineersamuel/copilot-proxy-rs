# copilot-proxy-rs

<p align="center">
  <img src="logo.svg" width="160" alt="copilot-proxy-rs logo">
</p>

`copilot-proxy-rs` is an experimental local Rust proxy that exposes OpenAI, Anthropic Messages, and OpenAI Responses-style routes backed by GitHub Copilot.

> **Alpha status:** This project is intended for local development on trusted machines. Keep it bound to loopback (`127.0.0.1`) unless you have added network controls and understand the credential-exposure risk.

## Quickstart & Installation

### Option 1: Run with Cargo (Native)
1. Ensure Rust 1.85+ is installed.
2. Start the proxy server:
   ```bash
   RUST_LOG=info cargo run
   ```
   *If you don't have a token saved yet, the terminal will prompt you to complete GitHub's browser-based device flow on first startup.*

### Option 2: Run with Docker (Containerized)
Run the proxy as a detached background container with your host-persisted GitHub token securely mounted:
```bash
docker run -d --name copilot-proxy-rs \
  --user "$(id -u):$(id -g)" \
  -p 127.0.0.1:8080:8080 \
  -e COPILOT_PROXY_RS_HOST=0.0.0.0 \
  -e COPILOT_PROXY_RS_CONFIG_DIR=/config \
  -e COPILOT_PROXY_RS_CONTAINER_LOOPBACK_ONLY=true \
  -e RUST_LOG=info \
  -v "$HOME/.config/copilot-proxy-rs:/config:ro" \
  copilot-proxy-rs
```
*(Or simply run `docker compose up -d`)*

### Verify
Check health:
```bash
curl -fsS http://127.0.0.1:8080/health
```

Run a simple "Hello World" query (e.g., against `gpt-5.5`):
```bash
curl -fsS http://127.0.0.1:8080/v1/chat/completions -H "Content-Type: application/json" -d '{"model":"gpt-5.5","messages":[{"role":"user","content":"Say Hello World!"}]}'
```

Run a streaming query:
```bash
curl -fsS -N http://127.0.0.1:8080/v1/chat/completions -H "Content-Type: application/json" -d '{"model":"gpt-5.5","stream":true,"messages":[{"role":"user","content":"Write me poem"}]}'
```

## Features

- Config defaults and loading from `~/.config/copilot-proxy-rs/config.json`.
- Environment overrides with `COPILOT_PROXY_RS_*` variables.
- Static `/health`, `/version`, and `/v1/messages/count_tokens` routes.
- Copilot-backed `/v1/models` sourced from live upstream model discovery.
- Live Copilot-backed routes for `/v1/chat/completions`, `/v1/messages`, `/v1/responses`, response retrieval/cancellation, and Responses WebSocket.
- Safe metadata logging that avoids raw prompt/body/token logging.
- Fail-closed startup for non-loopback binds unless explicitly opted in.

## Code structure

- `src/http/` owns route wiring and HTTP/WebSocket handlers.
- `src/copilot/` owns GitHub/Copilot authentication, upstream requests, retries, and model refresh.
- `src/translate/` owns OpenAI, Anthropic, and Responses format conversion.
- `src/responses/` owns Responses API request preparation and in-memory response-state caching.

## Benchmarks

The proxy is a single Rust process and starts quickly without requiring a live
Copilot call for readiness. The benchmark below measures a release binary from
process spawn until `/health` responds, then samples idle resident memory.

Run it on an isolated loopback port so it does not interfere with a real proxy:

```bash
COPILOT_PROXY_RS_BENCH_PORT=19091 ./scripts/benchmark-proxy.sh
```

Latest local result:

<!-- benchmark:start -->
## Benchmark result

- Command: `COPILOT_PROXY_RS_BENCH_PORT=19091 ./scripts/benchmark-proxy.sh`
- Runs: 10 measured after 2 warmup
- Platform: macOS-26.5.1-arm64-arm-64bit-Mach-O
- Rust: rustc 1.96.0 (ac68faa20 2026-05-25)

| Metric | Median | Mean | p95 | Min | Max |
| --- | ---: | ---: | ---: | ---: | ---: |
| Startup to `/health` | 37 ms | 37.4 ms | 43 ms | 32 ms | 43 ms |
| Idle RSS after readiness | 4.0 MiB | 4.0 MiB | 4.0 MiB | 3.9 MiB | 4.0 MiB |
<!-- benchmark:end -->

The benchmark uses `/health` only. It does not send prompts or call live
Copilot-backed routes, so no GitHub token is required for these startup and
idle-memory numbers.

## Copilot authentication

The Rust service uses `GITHUB_TOKEN` when set. Otherwise it reads
`github_token` from the configured copilot-proxy-rs config directory. If no
usable token is available and the process is interactive, it prints a GitHub
device-flow URL and code in the server terminal.

By default, the persisted OAuth token lives at:

```text
~/.config/copilot-proxy-rs/github_token
```

If `COPILOT_PROXY_RS_CONFIG_DIR` is set, the token is read from:

```text
$COPILOT_PROXY_RS_CONFIG_DIR/github_token
```

You can create that token with a normal interactive run:

```bash
cargo run
```

After the browser device flow completes, future `cargo run`, Docker, and
Compose runs can reuse the same file without setting `GITHUB_TOKEN`.

## Configuration

Copy `config.example.json` to `~/.config/copilot-proxy-rs/config.json` and edit
as needed. Environment variables override file values.

Important variables:

| Variable | Purpose |
| --- | --- |
| `GITHUB_TOKEN` | GitHub token used to request Copilot tokens. |
| `COPILOT_PROXY_RS_CONFIG_DIR` | Directory containing `config.json` and `github_token`. |
| `COPILOT_PROXY_RS_HOST` | Bind host. Defaults to `127.0.0.1`. |
| `COPILOT_PROXY_RS_PORT` | Bind port. Defaults to `8080`. |
| `COPILOT_PROXY_RS_ALLOW_NON_LOOPBACK` | Required for `0.0.0.0`, `::`, or other non-loopback binds. |
| `COPILOT_PROXY_RS_CONTAINER_LOOPBACK_ONLY` | Allows container-internal `0.0.0.0` binds when the host port is published only on loopback. |
| `COPILOT_PROXY_RS_API_KEY` | Optional inbound API key. When set, Copilot-backed routes require `Authorization: Bearer <key>` or `x-api-key: <key>`. |
| `COPILOT_PROXY_RS_ALLOWED_ORIGINS` | Optional comma-separated WebSocket origin allowlist for `/v1/responses`. Empty means no origin filtering. |
| `COPILOT_PROXY_RS_MAX_DECODED_BODY_BYTES` | Maximum decoded JSON request body size after gzip/zstd decompression. Defaults to `16777216` bytes. |
| `COPILOT_MODELS_TTL` | Seconds to cache GitHub Copilot `/models` metadata. Defaults to `300`. |
| `RUST_LOG` | Rust logging filter. Docker defaults to `info`. |

## Model discovery

The proxy starts a background GitHub Copilot model metadata refresh at startup,
then caches successful refreshes for `COPILOT_MODELS_TTL` seconds. `/v1/models`,
`/v1/chat/completions`, `/v1/messages`, `/v1/responses`, and Responses
WebSocket requests also refresh the cache when it is stale, so newly advertised
Copilot model IDs can appear and route without rebuilding the container.

If metadata refresh fails, `/v1/models` returns the last cached live model list,
plus static GPT-5.6 fallback entries for `gpt-5.6-sol`, `gpt-5.6-terra`, and
`gpt-5.6-luna` when no refresh has succeeded yet. Those fallbacks route through
the Responses API and advertise `low`, `medium`, `high`, `xhigh`, and `max`
reasoning efforts; live Copilot metadata overrides them when available.
Context-window fields are reported only when Copilot advertises them; otherwise
they are `null` and `context_window_modes` is empty.

You can define local request aliases in `config.json` with
`model_overrides.copilot`, for example:

```json
{
  "model_overrides": {
    "copilot": {
      "sonnet-latest": "claude-sonnet-5"
    }
  }
}
```

## Safety model

All Copilot-backed routes use the server operator's Copilot credentials. Do not
expose this process directly to a network or the public internet. For alpha
releases, the service refuses non-loopback bind addresses unless
`COPILOT_PROXY_RS_ALLOW_NON_LOOPBACK=true` is set.

WebSocket clients can be protected with `COPILOT_PROXY_RS_API_KEY` and
`COPILOT_PROXY_RS_ALLOWED_ORIGINS`. Keep the proxy local-only unless you have
configured inbound authentication, origin controls, and trusted network
boundaries.

## Docker

Build and run locally:

```bash
docker build -t copilot-proxy-rs .
docker run -d --name copilot-proxy-rs \
  --user "$(id -u):$(id -g)" \
  -p 127.0.0.1:8080:8080 \
  -e COPILOT_PROXY_RS_HOST=0.0.0.0 \
  -e COPILOT_PROXY_RS_CONFIG_DIR=/config \
  -e COPILOT_PROXY_RS_CONTAINER_LOOPBACK_ONLY=true \
  -e RUST_LOG=info \
  -v "$HOME/.config/copilot-proxy-rs:/config:ro" \
  copilot-proxy-rs
```

Follow logs or stop the container:

```bash
docker logs -f copilot-proxy-rs
docker stop copilot-proxy-rs
```

With Compose:

```bash
HOST_UID="$(id -u)" HOST_GID="$(id -g)" docker compose up --build -d
docker compose logs -f
docker compose down
```

The image binds to `0.0.0.0` inside the container, but the documented port
mapping exposes it only on host loopback. Do not publish the container with
`-p 8080:8080` unless you add your own inbound authentication and network
controls.

Docker and Compose run the container as your host UID/GID and mount your host
`~/.config/copilot-proxy-rs` directory read-only at `/config`, so the container
can read a `0600` `github_token` created by `cargo run` without weakening file
permissions.

## API examples

Chat Completions:

```bash
curl -fsS http://127.0.0.1:8080/v1/chat/completions \
  -H 'Content-Type: application/json' \
  -d '{"model":"gpt-5.5","messages":[{"role":"user","content":"Say hello"}]}'
```

Anthropic Messages:

```bash
curl -fsS http://127.0.0.1:8080/v1/messages \
  -H 'Content-Type: application/json' \
  -d '{"model":"claude-sonnet-4-6","max_tokens":128,"messages":[{"role":"user","content":"Say hello"}]}'
```

Anthropic-style prompt caching metadata is preserved for Claude-family
Messages requests that are forwarded to Copilot's Anthropic-compatible
`/v1/messages` endpoint. For example, `cache_control: {"type":"ephemeral"}`
on `system` text blocks or message content blocks remains in the outbound
provider request. This depends on the upstream Copilot-hosted Claude endpoint
honoring the metadata.

OpenAI-style prompt caching controls are preserved for GPT/OpenAI-style
Chat Completions and Responses requests. `prompt_cache_key` and
`prompt_cache_retention` pass through when supplied by the client, including
when Chat Completions or Messages requests are translated to Copilot's
`/responses` endpoint. Direct Responses requests without an explicit
`prompt_cache_key` derive one from `x-interaction-id` or
`x-client-request-id` plus the effective model, when either header is present,
to improve upstream cache locality without inventing a random per-request key.

Responses:

```bash
curl -fsS http://127.0.0.1:8080/v1/responses \
  -H 'Content-Type: application/json' \
  -d '{"model":"gpt-5.5","input":"Say hello"}'
```

## Test

```bash
cargo test
cargo clippy --all-targets -- -D warnings
```

## Live smoke test

After starting the server with valid Copilot credentials:

```bash
COPILOT_PROXY_RS_CONFIG_DIR=~/.config/copilot-proxy-rs-dev COPILOT_PROXY_RS_PORT=19090 cargo run
./scripts/live-smoke.sh http://127.0.0.1:19090
```

The smoke test calls real Copilot endpoints and is intentionally not part of
`cargo test`.

## Streaming benchmark

Use the reusable streaming benchmark script to measure TTFT and approximate
streaming throughput for each model exposed by the proxy:

```bash
python3 scripts/benchmark_streaming.py \
  --base-url http://127.0.0.1:8080 \
  --runs 3 \
  --output-json /tmp/streaming-bench.json \
  --output-csv /tmp/streaming-bench.csv
```

The script benchmarks the proxy's `/v1/chat/completions` streaming endpoint,
prints a summary table, and writes JSON/CSV output for later comparison.

## License

MIT. See `LICENSE`.
