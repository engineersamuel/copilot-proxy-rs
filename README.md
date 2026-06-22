# copilot-proxy-rs

`copilot-proxy-rs` is an experimental local Rust proxy that exposes OpenAI,
Anthropic Messages, and OpenAI Responses-style routes backed by GitHub Copilot.

> **Alpha status:** this project is intended for local development on trusted
> machines. It forwards requests using your GitHub/Copilot credentials. Keep it
> bound to loopback (`127.0.0.1`) unless you have added network controls and
> understand the credential-exposure risk.

## Features

- Config defaults and loading from `~/.config/copilot-proxy-rs/config.json`.
- Environment overrides with `COPILOT_PROXY_RS_*` variables.
- Static `/health`, `/version`, `/v1/models`, and `/v1/messages/count_tokens` routes.
- Live Copilot-backed routes for `/v1/chat/completions`, `/v1/messages`, `/v1/responses`, response retrieval/cancellation, and Responses WebSocket.
- Safe metadata logging that avoids raw prompt/body/token logging.
- Fail-closed startup for non-loopback binds unless explicitly opted in.

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

## Quickstart

Prerequisites:

- Rust 1.85 or newer
- A GitHub account with Copilot access

Run locally:

```bash
git clone https://github.com/smendenhall/copilot-proxy-rs.git
cd copilot-proxy-rs
export GITHUB_TOKEN=your-token-with-copilot-access
cargo run
```

Use an isolated config directory and port:

```bash
COPILOT_PROXY_RS_CONFIG_DIR=~/.config/copilot-proxy-rs-dev COPILOT_PROXY_RS_PORT=19090 cargo run
```

Verify:

```bash
curl -fsS http://127.0.0.1:8080/health
curl -fsS http://127.0.0.1:8080/v1/models
```

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
| `RUST_LOG` | Rust logging filter. Docker defaults to `info`. |

## Safety model

All Copilot-backed routes use the server operator's Copilot credentials. Do not
expose this process directly to a network or the public internet. For alpha
releases, the service refuses non-loopback bind addresses unless
`COPILOT_PROXY_RS_ALLOW_NON_LOOPBACK=true` is set.

WebSocket clients are not separately authenticated in this alpha. Keep the proxy
local-only or place it behind trusted inbound authentication and origin controls.

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

## License

MIT. See `LICENSE`.
