# Module Decomposition Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Split oversized route/config code into focused modules without changing externally observable API behavior.

**Architecture:** Preserve the existing router and handler behavior while moving cohesive pieces into smaller modules: static routes, Chat Completions, Anthropic Messages, Responses HTTP/WebSocket, validation/error mapping, and streaming line transforms. Refactor by moving code first, then deduplicate repeated helpers only after contract tests prove behavior is unchanged.

**Tech Stack:** Rust 1.85+, Axum 0.8, existing integration/contract tests, no new dependencies.

## Global Constraints

- This is a behavior-preserving refactor; route paths, status codes, response shapes, headers, logging fields, and auth behavior must not intentionally change.
- Keep public crate exports stable unless a symbol is private to `src/http`.
- Do not combine this refactor with feature work from the inbound-auth or retry plans.
- Use existing test commands: `cargo test --quiet` and `cargo clippy --all-targets --quiet -- -D warnings`.
- Commit after each independently passing module move.

---

## File Structure

- Modify `src/http/routes.rs`: shrink to router assembly plus shared route registration only.
- Create `src/http/health.rs`: `/health`, `/version`, `/v1/models`, `/v1/messages/count_tokens`.
- Create `src/http/chat.rs`: `/v1/chat/completions` handlers.
- Create `src/http/messages.rs`: `/v1/messages` handlers.
- Create `src/http/responses.rs`: `/v1/responses`, retrieve, cancel, compact, and WebSocket handlers.
- Create `src/http/errors.rs`: `openai_copilot_error` and `anthropic_copilot_error`.
- Create `src/http/validation.rs`: OpenAI and Anthropic request validators.
- Create `src/http/sse.rs`: reusable SSE line-buffer mapping helpers.
- Modify `src/http/mod.rs`: expose new modules as `pub(crate)` or `pub` only where tests need access.
- Optionally create `src/config/env.rs` in a later task only if `src/config.rs` remains difficult to read after route decomposition.

---

### Task 1: Extract Static Health, Version, Models, and Count-Tokens Routes

**Files:**
- Create: `src/http/health.rs`
- Modify: `src/http/mod.rs`
- Modify: `src/http/routes.rs`
- Test: `tests/http_contract.rs`

**Interfaces:**
- Consumes: `AppState`, `ModelsListResponse`, `parse_json_request_body`.
- Produces: `pub(crate) async fn health(State(state): State<AppState>) -> Json<HealthResponse>`.
- Produces: `pub(crate) async fn version() -> Json<VersionResponse>`.
- Produces: `pub(crate) async fn list_models(State(state): State<AppState>) -> Json<ModelsListResponse>`.
- Produces: `pub(crate) async fn count_tokens(headers: HeaderMap, body: Bytes) -> Result<Json<CountTokensResponse>, (StatusCode, Json<AnthropicErrorResponse>)>`.

- [ ] **Step 1: Run current static route tests as baseline**

Run:

```bash
cargo test --quiet --test http_contract health_returns_status_version_backend_and_runtime version_returns_version_and_runtime models_route_returns_openai_model_list count_tokens_returns_simple_input_token_estimate
```

Expected: PASS before edits.

- [ ] **Step 2: Create `src/http/health.rs`**

Move the following structs and functions from `src/http/routes.rs` into `src/http/health.rs`:

```rust
use axum::body::Bytes;
use axum::extract::State;
use axum::Json;
use http::{HeaderMap, StatusCode};
use serde::Serialize;

use crate::errors::anthropic_error;
use crate::models::ModelsListResponse;
use crate::request_body::parse_json_request_body;
use crate::state::AppState;

#[derive(Debug, Serialize)]
pub(crate) struct RuntimeInfo {
    implementation: &'static str,
    pid: u32,
}

#[derive(Debug, Serialize)]
pub(crate) struct HealthResponse {
    status: &'static str,
    version: &'static str,
    backend: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    fallback: Option<&'static str>,
    runtime: RuntimeInfo,
}

#[derive(Debug, Serialize)]
pub(crate) struct VersionResponse {
    version: &'static str,
    runtime: RuntimeInfo,
}

#[derive(Debug, Serialize)]
pub(crate) struct CountTokensResponse {
    input_tokens: usize,
}

pub(crate) async fn health(State(state): State<AppState>) -> Json<HealthResponse> {
    let snapshot = state.backend.snapshot().await;
    Json(HealthResponse {
        status: "ok",
        version: env!("CARGO_PKG_VERSION"),
        backend: snapshot.primary.as_str(),
        fallback: snapshot.fallback.map(|backend| backend.as_str()),
        runtime: runtime_info(),
    })
}

pub(crate) async fn version() -> Json<VersionResponse> {
    Json(VersionResponse {
        version: env!("CARGO_PKG_VERSION"),
        runtime: runtime_info(),
    })
}

pub(crate) async fn list_models(State(state): State<AppState>) -> Json<ModelsListResponse> {
    let snapshot = state.backend.snapshot().await;
    if snapshot.primary == crate::state::BackendKind::Copilot
        || snapshot.fallback == Some(crate::state::BackendKind::Copilot)
    {
        state.copilot.refresh_models_if_stale().await;
    }
    Json(state.models.list_for_snapshot(snapshot).await)
}

pub(crate) async fn count_tokens(
    headers: HeaderMap,
    body: Bytes,
) -> Result<Json<CountTokensResponse>, (StatusCode, Json<crate::errors::AnthropicErrorResponse>)> {
    let encoding = headers
        .get(http::header::CONTENT_ENCODING)
        .and_then(|value| value.to_str().ok())
        .unwrap_or("identity");
    let body = parse_json_request_body(&body, encoding).map_err(|err| {
        anthropic_error(
            StatusCode::BAD_REQUEST,
            "invalid_request_error",
            err.to_string(),
        )
    })?;
    Ok(Json(CountTokensResponse {
        input_tokens: crate::telemetry::estimate_request_tokens(&body),
    }))
}

pub(crate) fn runtime_info() -> RuntimeInfo {
    RuntimeInfo {
        implementation: "rust",
        pid: std::process::id(),
    }
}
```

If inbound-auth hardening has already changed `count_tokens` to accept `State(state)`, preserve that newer signature and use the bounded parser from that plan.

- [ ] **Step 3: Wire the module**

Modify `src/http/mod.rs`:

```rust
pub(crate) mod health;
pub mod routes;
```

Modify imports and route registrations in `src/http/routes.rs`:

```rust
use crate::http::health::{count_tokens, health, list_models, version};
```

Remove the moved structs/functions from `routes.rs`.

- [ ] **Step 4: Run static route tests**

Run:

```bash
cargo test --quiet --test http_contract health_returns_status_version_backend_and_runtime version_returns_version_and_runtime models_route_returns_openai_model_list count_tokens_returns_simple_input_token_estimate
```

Expected: PASS.

- [ ] **Step 5: Run formatter**

Run:

```bash
cargo fmt --check
```

Expected: PASS. If it fails, run `cargo fmt` and then `cargo fmt --check`.

- [ ] **Step 6: Commit**

```bash
git add src/http/health.rs src/http/mod.rs src/http/routes.rs
git commit -m "refactor: extract static http routes"
```

---

### Task 2: Extract Error Mapping and Request Validation

**Files:**
- Create: `src/http/errors.rs`
- Create: `src/http/validation.rs`
- Modify: `src/http/mod.rs`
- Modify: `src/http/routes.rs`
- Test: `tests/http_contract.rs`
- Test: `tests/messages_contract.rs`
- Test: `tests/chat_completions_contract.rs`

**Interfaces:**
- Produces: `pub(crate) fn openai_copilot_error(error: crate::copilot::errors::CopilotError) -> (StatusCode, Json<OpenAiErrorResponse>)`.
- Produces: `pub(crate) fn anthropic_copilot_error(error: crate::copilot::errors::CopilotError) -> (StatusCode, Json<AnthropicErrorResponse>)`.
- Produces: `pub(crate) fn validate_openai_chat_request(body: &Map<String, Value>) -> Result<(), (StatusCode, Json<OpenAiErrorResponse>)>`.
- Produces: `pub(crate) fn validate_anthropic_messages_request(body: &Map<String, Value>) -> Result<(), (StatusCode, Json<AnthropicErrorResponse>)>`.

- [ ] **Step 1: Run current validation/error tests as baseline**

Run:

```bash
cargo test --quiet --test http_contract --test messages_contract --test chat_completions_contract
```

Expected: PASS.

- [ ] **Step 2: Create `src/http/errors.rs`**

Move `openai_copilot_error` and `anthropic_copilot_error` from `routes.rs` into:

```rust
use axum::Json;
use http::StatusCode;

use crate::errors::{anthropic_error, openai_error};

pub(crate) fn openai_copilot_error(
    error: crate::copilot::errors::CopilotError,
) -> (StatusCode, Json<crate::errors::OpenAiErrorResponse>) {
    match error {
        crate::copilot::errors::CopilotError::Auth(err) => openai_error(
            StatusCode::UNAUTHORIZED,
            "authentication_error",
            err.to_string(),
        ),
        crate::copilot::errors::CopilotError::Transient(err) => openai_error(
            StatusCode::from_u16(err.status_code).unwrap_or(StatusCode::BAD_GATEWAY),
            err.error_type,
            err.message,
        ),
        crate::copilot::errors::CopilotError::Http(err) => openai_error(
            StatusCode::from_u16(err.status_code).unwrap_or(StatusCode::BAD_GATEWAY),
            "server_error",
            err.detail,
        ),
        other => openai_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "server_error",
            other.to_string(),
        ),
    }
}

pub(crate) fn anthropic_copilot_error(
    error: crate::copilot::errors::CopilotError,
) -> (StatusCode, Json<crate::errors::AnthropicErrorResponse>) {
    match error {
        crate::copilot::errors::CopilotError::Auth(err) => anthropic_error(
            StatusCode::UNAUTHORIZED,
            "authentication_error",
            err.to_string(),
        ),
        crate::copilot::errors::CopilotError::Transient(err) => anthropic_error(
            StatusCode::from_u16(err.status_code).unwrap_or(StatusCode::BAD_GATEWAY),
            err.error_type,
            err.message,
        ),
        crate::copilot::errors::CopilotError::Http(err) => anthropic_error(
            StatusCode::from_u16(err.status_code).unwrap_or(StatusCode::BAD_GATEWAY),
            "server_error",
            err.detail,
        ),
        other => anthropic_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "server_error",
            other.to_string(),
        ),
    }
}
```

- [ ] **Step 3: Create `src/http/validation.rs`**

Move validators from `routes.rs` into:

```rust
use axum::Json;
use http::StatusCode;
use serde_json::{Map, Value};

use crate::errors::{anthropic_error, openai_error};

pub(crate) fn validate_openai_chat_request(
    body: &Map<String, Value>,
) -> Result<(), (StatusCode, Json<crate::errors::OpenAiErrorResponse>)> {
    if !body.contains_key("model") {
        return Err(openai_error(
            StatusCode::BAD_REQUEST,
            "invalid_request_error",
            "Missing required field: model",
        ));
    }
    match body.get("messages") {
        Some(Value::Array(messages)) if !messages.is_empty() => Ok(()),
        Some(Value::Array(_)) => Err(openai_error(
            StatusCode::BAD_REQUEST,
            "invalid_request_error",
            "messages must not be empty",
        )),
        Some(_) => Err(openai_error(
            StatusCode::BAD_REQUEST,
            "invalid_request_error",
            "messages must be an array",
        )),
        None => Err(openai_error(
            StatusCode::BAD_REQUEST,
            "invalid_request_error",
            "Missing required field: messages",
        )),
    }
}

pub(crate) fn validate_anthropic_messages_request(
    body: &Map<String, Value>,
) -> Result<(), (StatusCode, Json<crate::errors::AnthropicErrorResponse>)> {
    if !body.contains_key("model") {
        return Err(anthropic_error(
            StatusCode::BAD_REQUEST,
            "invalid_request_error",
            "Missing required field: model",
        ));
    }
    if !body.contains_key("max_tokens") {
        return Err(anthropic_error(
            StatusCode::BAD_REQUEST,
            "invalid_request_error",
            "Missing required field: max_tokens",
        ));
    }
    match body.get("messages") {
        Some(Value::Array(_)) => Ok(()),
        Some(_) => Err(anthropic_error(
            StatusCode::BAD_REQUEST,
            "invalid_request_error",
            "messages must be an array",
        )),
        None => Err(anthropic_error(
            StatusCode::BAD_REQUEST,
            "invalid_request_error",
            "Missing required field: messages",
        )),
    }
}
```

- [ ] **Step 4: Wire modules and imports**

Modify `src/http/mod.rs`:

```rust
pub(crate) mod errors;
pub(crate) mod health;
pub mod routes;
pub(crate) mod validation;
```

Modify `src/http/routes.rs` imports:

```rust
use crate::http::errors::{anthropic_copilot_error, openai_copilot_error};
use crate::http::validation::{validate_anthropic_messages_request, validate_openai_chat_request};
```

Remove the moved functions from `routes.rs`.

- [ ] **Step 5: Run validation/error contracts**

Run:

```bash
cargo test --quiet --test http_contract --test messages_contract --test chat_completions_contract
```

Expected: PASS.

- [ ] **Step 6: Commit**

```bash
git add src/http/errors.rs src/http/validation.rs src/http/mod.rs src/http/routes.rs
git commit -m "refactor: extract http error and validation helpers"
```

---

### Task 3: Extract Reusable SSE Line Mapping

**Files:**
- Create: `src/http/sse.rs`
- Modify: `src/http/mod.rs`
- Modify: `src/http/routes.rs`
- Test: `tests/chat_completions_contract.rs`
- Test: `tests/messages_contract.rs`

**Interfaces:**
- Produces: `pub(crate) fn map_sse_lines<S, F>(stream: S, mapper: F) -> impl Stream<Item = Result<bytes::Bytes, std::io::Error>>`.
- Consumes mapper closures such as `normalize_openai_sse_line`, `responses_sse_to_openai_chat_sse_line`, and `responses_sse_to_anthropic_sse_line`.

- [ ] **Step 1: Run current streaming tests as baseline**

Run:

```bash
cargo test --quiet --test chat_completions_contract --test messages_contract
```

Expected: PASS.

- [ ] **Step 2: Create `src/http/sse.rs`**

Create:

```rust
use bytes::Bytes;
use futures_util::{Stream, StreamExt};

pub(crate) fn map_sse_lines<S, F>(
    stream: S,
    mapper: F,
) -> impl Stream<Item = Result<Bytes, std::io::Error>>
where
    S: Stream<Item = Result<Bytes, reqwest::Error>> + Send + 'static,
    F: Fn(&str) -> Option<String> + Send + Sync + 'static,
{
    let mut src = Box::pin(stream);
    async_stream::stream! {
        let mut buf = String::new();
        while let Some(chunk_result) = src.next().await {
            match chunk_result {
                Err(e) => {
                    yield Err(std::io::Error::other(e));
                    return;
                }
                Ok(chunk) => buf.push_str(&String::from_utf8_lossy(&chunk)),
            }
            while let Some(nl) = buf.find('\n') {
                let line = buf[..nl].trim_end_matches('\r').to_string();
                buf.drain(..=nl);
                if let Some(normalized) = mapper(&line) {
                    yield Ok::<Bytes, std::io::Error>(Bytes::from(format!("{normalized}\n")));
                }
            }
        }
        if !buf.is_empty() {
            let line = buf.trim_end_matches('\r').to_string();
            if let Some(normalized) = mapper(&line) {
                yield Ok(Bytes::from(format!("{normalized}\n")));
            }
        }
    }
}
```

- [ ] **Step 3: Wire the module**

Modify `src/http/mod.rs`:

```rust
pub(crate) mod errors;
pub(crate) mod health;
pub mod routes;
pub(crate) mod sse;
pub(crate) mod validation;
```

- [ ] **Step 4: Replace duplicated SSE buffering in routes**

For OpenAI chat SSE normalization, replace the inline `async_stream::stream!` block with:

```rust
let byte_stream = crate::http::sse::map_sse_lines(
    upstream.bytes_stream(),
    |line| Some(crate::translate::openai::normalize_openai_sse_line(line)),
);
```

For Responses-to-OpenAI normalization:

```rust
let byte_stream = crate::http::sse::map_sse_lines(
    upstream.bytes_stream(),
    crate::translate::responses_formats::responses_sse_to_openai_chat_sse_line,
);
```

For Responses-to-Anthropic normalization:

```rust
let byte_stream = crate::http::sse::map_sse_lines(
    upstream.bytes_stream(),
    crate::translate::responses_formats::responses_sse_to_anthropic_sse_line,
);
```

Keep pass-through streams unchanged:

```rust
let byte_stream = upstream
    .bytes_stream()
    .map(|chunk| chunk.map_err(std::io::Error::other));
```

- [ ] **Step 5: Run streaming contracts**

Run:

```bash
cargo test --quiet --test chat_completions_contract --test messages_contract
```

Expected: PASS.

- [ ] **Step 6: Commit**

```bash
git add src/http/sse.rs src/http/mod.rs src/http/routes.rs
git commit -m "refactor: extract sse line mapping"
```

---

### Task 4: Extract Chat Completions Handler

**Files:**
- Create: `src/http/chat.rs`
- Modify: `src/http/mod.rs`
- Modify: `src/http/routes.rs`
- Test: `tests/chat_completions_contract.rs`
- Test: `tests/http_contract.rs`

**Interfaces:**
- Produces: `pub(crate) async fn chat_completions(State(state): State<AppState>, headers: HeaderMap, body: Bytes) -> Response`.
- Consumes: `openai_copilot_error`, `validate_openai_chat_request`, `map_sse_lines`.

- [ ] **Step 1: Run chat contracts as baseline**

Run:

```bash
cargo test --quiet --test chat_completions_contract --test http_contract openai_chat_returns_auth_error_when_no_token_available
```

Expected: PASS.

- [ ] **Step 2: Create `src/http/chat.rs`**

Move `chat_completions` and `chat_completions_inner` from `routes.rs` into `src/http/chat.rs`. Start the file with:

```rust
use axum::body::{Body, Bytes};
use axum::extract::State;
use axum::response::{IntoResponse, Response};
use axum::Json;
use bytes::Bytes as RawBytes;
use futures_util::StreamExt;
use http::{HeaderMap, StatusCode};
use serde_json::Value;

use crate::copilot::request::adapt_openai_reasoning_effort;
use crate::errors::openai_error;
use crate::http::errors::openai_copilot_error;
use crate::http::validation::validate_openai_chat_request;
use crate::request_body::parse_json_request_body;
use crate::state::AppState;
use crate::telemetry::{api_family_name, summarize_effective_request, ApiFamily};
```

If the retry/fallback plan has already removed Bedrock/fallback behavior, keep the current post-retry imports and model-list logic rather than restoring fallback-specific code from this example. If Task 3 already removed `RawBytes` or direct `StreamExt` use, do not reintroduce unused imports.

- [ ] **Step 3: Wire route to extracted handler**

Modify `src/http/mod.rs`:

```rust
pub(crate) mod chat;
pub(crate) mod errors;
pub(crate) mod health;
pub mod routes;
pub(crate) mod sse;
pub(crate) mod validation;
```

Modify `src/http/routes.rs` imports:

```rust
use crate::http::chat::chat_completions;
```

Remove moved chat functions from `routes.rs`.

- [ ] **Step 4: Run chat contracts**

Run:

```bash
cargo test --quiet --test chat_completions_contract --test http_contract openai_chat_returns_auth_error_when_no_token_available
```

Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add src/http/chat.rs src/http/mod.rs src/http/routes.rs
git commit -m "refactor: extract chat completions route"
```

---

### Task 5: Extract Anthropic Messages Handler

**Files:**
- Create: `src/http/messages.rs`
- Modify: `src/http/mod.rs`
- Modify: `src/http/routes.rs`
- Test: `tests/messages_contract.rs`
- Test: `tests/http_contract.rs`

**Interfaces:**
- Produces: `pub(crate) async fn messages(State(state): State<AppState>, headers: HeaderMap, body: Bytes) -> Response`.
- Consumes: `anthropic_copilot_error`, `validate_anthropic_messages_request`, `map_sse_lines`.

- [ ] **Step 1: Run messages contracts as baseline**

Run:

```bash
cargo test --quiet --test messages_contract --test http_contract anthropic_messages_returns_auth_error_when_no_token_available
```

Expected: PASS.

- [ ] **Step 2: Create `src/http/messages.rs`**

Move `messages` and `messages_inner` from `routes.rs` into `src/http/messages.rs`. Start the file with:

```rust
use axum::body::{Body, Bytes};
use axum::extract::State;
use axum::response::{IntoResponse, Response};
use axum::Json;
use futures_util::StreamExt;
use http::{HeaderMap, StatusCode};
use serde_json::Value;

use crate::copilot::request::{
    adapt_thinking_for_copilot, filter_anthropic_beta_header, CopilotRequestMetadata,
};
use crate::errors::anthropic_error;
use crate::http::errors::anthropic_copilot_error;
use crate::http::validation::validate_anthropic_messages_request;
use crate::request_body::parse_json_request_body;
use crate::state::AppState;
use crate::telemetry::{api_family_name, summarize_effective_request, ApiFamily};
```

If the retry/fallback plan has already removed Bedrock/fallback behavior, keep the current post-retry route and model-list logic rather than restoring fallback-specific code from this example. If Task 3 has already replaced direct stream buffering, use `crate::http::sse::map_sse_lines` in the moved code.

- [ ] **Step 3: Wire route to extracted handler**

Modify `src/http/mod.rs`:

```rust
pub(crate) mod chat;
pub(crate) mod errors;
pub(crate) mod health;
pub(crate) mod messages;
pub mod routes;
pub(crate) mod sse;
pub(crate) mod validation;
```

Modify `src/http/routes.rs` imports:

```rust
use crate::http::messages::messages;
```

Remove moved messages functions from `routes.rs`.

- [ ] **Step 4: Run messages contracts**

Run:

```bash
cargo test --quiet --test messages_contract --test http_contract anthropic_messages_returns_auth_error_when_no_token_available
```

Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add src/http/messages.rs src/http/mod.rs src/http/routes.rs
git commit -m "refactor: extract anthropic messages route"
```

---

### Task 6: Extract Responses HTTP and WebSocket Handlers

**Files:**
- Create: `src/http/responses.rs`
- Modify: `src/http/mod.rs`
- Modify: `src/http/routes.rs`
- Test: `tests/responses_contract.rs`
- Test: `tests/responses_ws_contract.rs`

**Interfaces:**
- Produces: `pub(crate) async fn responses(State(state): State<AppState>, headers: HeaderMap, body: Bytes) -> Response`.
- Produces: `pub(crate) async fn responses_ws(State(state): State<AppState>, ws: WebSocketUpgrade) -> Response`.
- Produces: `pub(crate) async fn responses_retrieve(State(state): State<AppState>, Path(response_id): Path<String>) -> Response`.
- Produces: `pub(crate) async fn responses_cancel(State(state): State<AppState>, Path(response_id): Path<String>) -> Response`.
- Produces: `pub(crate) async fn responses_compact() -> Response`.
- Consumes: `openai_copilot_error`, `summarize_cache`, `prepare_responses_request`.

- [ ] **Step 1: Run responses contracts as baseline**

Run:

```bash
cargo test --quiet --test responses_contract --test responses_ws_contract
```

Expected: PASS.

- [ ] **Step 2: Create `src/http/responses.rs`**

Move these functions from `routes.rs` into `src/http/responses.rs`:

```rust
responses
responses_ws
handle_responses_ws
responses_retrieve
responses_cancel
responses_compact
```

Start the new file with:

```rust
use axum::body::{Body, Bytes};
use axum::extract::{Path, State};
use axum::response::{IntoResponse, Response};
use axum::Json;
use futures_util::{SinkExt, StreamExt};
use http::{HeaderMap, StatusCode};
use serde_json::{Map, Value};

use crate::errors::openai_error;
use crate::http::errors::openai_copilot_error;
use crate::request_body::parse_json_request_body;
use crate::responses::request::PreviousResponseCacheStatus;
use crate::state::AppState;
use crate::telemetry::{
    api_family_name, summarize_cache, summarize_effective_request, ApiFamily, CacheOperation,
};
```

If inbound-auth hardening has already changed the `responses_ws` signature to include `headers: HeaderMap`, preserve that newer signature. If the retry/fallback plan has already removed Bedrock/fallback behavior, keep the current post-retry logic rather than restoring fallback-specific code from this example.

- [ ] **Step 3: Wire routes to extracted handlers**

Modify `src/http/mod.rs`:

```rust
pub(crate) mod chat;
pub(crate) mod errors;
pub(crate) mod health;
pub(crate) mod messages;
pub(crate) mod responses;
pub mod routes;
pub(crate) mod sse;
pub(crate) mod validation;
```

Modify `src/http/routes.rs` imports:

```rust
use crate::http::responses::{
    responses, responses_cancel, responses_compact, responses_retrieve, responses_ws,
};
```

Remove moved responses functions from `routes.rs`.

- [ ] **Step 4: Run responses contracts**

Run:

```bash
cargo test --quiet --test responses_contract --test responses_ws_contract
```

Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add src/http/responses.rs src/http/mod.rs src/http/routes.rs
git commit -m "refactor: extract responses routes"
```

---

### Task 7: Final Router Cleanup and Documentation

**Files:**
- Modify: `src/http/routes.rs`
- Modify: `README.md`
- Test: all tests

**Interfaces:**
- Produces: `pub fn router(state: AppState) -> Router` as the sole public responsibility of `src/http/routes.rs`.

- [ ] **Step 1: Simplify `src/http/routes.rs`**

After prior tasks, `src/http/routes.rs` should be reduced to:

```rust
use axum::routing::{get, post};
use axum::Router;

use crate::http::chat::chat_completions;
use crate::http::health::{count_tokens, health, list_models, version};
use crate::http::messages::messages;
use crate::http::responses::{
    responses, responses_cancel, responses_compact, responses_retrieve, responses_ws,
};
use crate::state::AppState;

pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/health", get(health))
        .route("/version", get(version))
        .route("/v1/models", get(list_models))
        .route("/v1/messages/count_tokens", post(count_tokens))
        .route("/v1/messages", post(messages))
        .route("/v1/chat/completions", post(chat_completions))
        .route("/v1/responses", post(responses).get(responses_ws))
        .route("/v1/responses/compact", post(responses_compact))
        .route("/v1/responses/{response_id}", get(responses_retrieve))
        .route("/v1/responses/{response_id}/cancel", post(responses_cancel))
        .with_state(state)
}
```

If inbound-auth hardening has already introduced a protected sub-router and middleware, keep that newer router structure and only remove unused imports/code.

- [ ] **Step 2: Add architecture note to README**

Add a short "Code structure" subsection after the Features list:

```markdown
## Code structure

- `src/http/` owns route wiring and HTTP/WebSocket handlers.
- `src/copilot/` owns GitHub/Copilot authentication, upstream requests, retries, and model refresh.
- `src/translate/` owns OpenAI, Anthropic, and Responses format conversion.
- `src/responses/` owns Responses API request preparation and in-memory response-state caching.
```

- [ ] **Step 3: Run full verification**

Run:

```bash
cargo fmt --check && cargo test --quiet && cargo clippy --all-targets --quiet -- -D warnings
```

Expected: PASS.

- [ ] **Step 4: Inspect route file size**

Run:

```bash
wc -l src/http/routes.rs src/http/*.rs
```

Expected: `src/http/routes.rs` is under 80 lines; the extracted modules hold the moved behavior.

- [ ] **Step 5: Commit**

```bash
git add src/http/routes.rs README.md
git commit -m "docs: describe proxy code structure"
```

---

## Self-Review Notes

- Spec coverage: static routes, validation/error helpers, SSE helpers, chat, messages, responses, WebSocket, router cleanup, and docs are covered.
- Placeholder scan: no TODO/TBD placeholders remain.
- Type consistency: route handler signatures match Axum extraction patterns currently used by the repository.
