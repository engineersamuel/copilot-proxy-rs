# Retry and Fallback Behavior Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make Copilot retry behavior match the exposed configuration and remove misleading backend fallback behavior until a real non-Copilot backend exists.

**Architecture:** Centralize retry classification and delay calculation in `src/copilot/client.rs`, then apply it consistently to JSON, GET, and streaming requests. Keep model/effort fallback separate from transient retries, and make the public config/docs accurately state that this release is Copilot-backed only.

**Tech Stack:** Rust 1.85+, reqwest, Tokio timers, existing mock server contract tests, existing tracing capture helpers.

## Global Constraints

- Retry only idempotent-safe setup/transport failures and upstream transient statuses where the current client already resends whole request bodies.
- Preserve `Retry-After` support and the existing `copilot_retry_max` count semantics: default `3` means up to 4 total attempts.
- Do not retry client/request errors such as 400, 401, 403, or model unsupported responses except the existing explicit gpt-5.5 reasoning-effort downgrade path.
- Keep request/response bodies out of logs.
- Use existing test commands: `cargo test --quiet` and `cargo clippy --all-targets --quiet -- -D warnings`.

---

## File Structure

- Modify `src/copilot/client.rs`: centralize transient detection, retry delay, and retry logging; apply to POST JSON, GET JSON, and streaming requests.
- Modify `src/state.rs`: remove or disable the public `Bedrock` backend variant until a real backend implementation exists.
- Modify `src/models.rs`: remove Bedrock-only model-list behavior or clearly keep it behind tests if the product scope still requires it.
- Modify `src/config.rs`: stop advertising `fallback_backend` as an active runtime fallback, or parse it only as a future-reserved no-op with warnings.
- Modify `src/main.rs`, `README.md`, `config.example.json`: align wording to Copilot-backed proxy.
- Modify `tests/copilot_client_contract.rs`, `tests/http_contract.rs`, `tests/state_contract.rs`, `tests/config_contract.rs`: assert transient retry behavior and honest backend scope.

---

### Task 1: Unified Transient Retry for JSON POST Requests

**Files:**
- Modify: `src/copilot/client.rs`
- Test: `tests/copilot_client_contract.rs`

**Interfaces:**
- Consumes: existing `retry_delay(headers: &HeaderMap, attempt: u32, base_delay_seconds: f64) -> Duration`.
- Produces: `fn retryable_status(status: u16) -> bool`.
- Produces: `fn transient_error_for_status(status: u16, detail: String) -> TransientBackendError`.

- [ ] **Step 1: Write failing JSON 5xx retry test**

Append to `tests/copilot_client_contract.rs`:

```rust
#[tokio::test]
async fn post_chat_retries_503_then_returns_json() {
    let mock = support::MockServer::start().await;
    mock.respond_sequence_json(
        "POST",
        "/chat/completions",
        vec![
            (
                503,
                serde_json::json!({"error": "temporarily unavailable"}),
                vec![("retry-after", "0")],
            ),
            (
                200,
                serde_json::json!({"choices": [{"message": {"role": "assistant", "content": "recovered"}}]}),
                vec![],
            ),
        ],
    )
    .await;
    mock.respond_json(
        "GET",
        "/copilot/token",
        200,
        serde_json::json!({"token": "copilot-token", "expires_at": 4_102_444_800u64}),
    )
    .await;

    let fixture = support::backend_fixture(mock).await;
    let response = fixture
        .backend
        .post_chat(
            serde_json::json!({"model": "gpt-5.5", "messages": []})
                .as_object()
                .unwrap()
                .clone(),
            None,
        )
        .await
        .unwrap();

    assert_eq!(response["choices"][0]["message"]["content"], "recovered");
    assert_eq!(fixture.mock.hits("POST", "/chat/completions").await, 2);
}
```

- [ ] **Step 2: Run test to verify it fails**

Run:

```bash
cargo test --quiet --test copilot_client_contract post_chat_retries_503_then_returns_json
```

Expected: FAIL because `post_json` currently returns a transient error on the first 503.

- [ ] **Step 3: Implement shared transient retry classification**

In `src/copilot/client.rs`, add:

```rust
fn retryable_status(status: u16) -> bool {
    matches!(status, 429 | 500 | 502 | 503 | 504)
}

fn transient_error_for_status(status: u16, detail: String) -> TransientBackendError {
    TransientBackendError {
        status_code: status,
        error_type: error_type_for_status(status).to_string(),
        message: detail,
        backend: "copilot".to_string(),
    }
}
```

In `post_json`, replace the current 429-only retry block plus final transient return with this structure after `let response = ...`:

```rust
let status = response.status().as_u16();
if response.status().is_success() {
    let value = response.json().await.map_err(map_reqwest_error)?;
    let usage = crate::telemetry::summarize_usage(&value);
    tracing::info!(
        api.family = family,
        http.status_code = status as u64,
        elapsed.ms = started.elapsed().as_millis() as u64,
        attempt = attempt as u64,
        "copilot request completed"
    );
    if usage.input_tokens.is_some()
        || usage.output_tokens.is_some()
        || usage.cached_tokens.is_some()
        || usage.total_tokens.is_some()
    {
        tracing::info!(
            api.family = family,
            tokens.input = usage.input_tokens.unwrap_or(0),
            tokens.output = usage.output_tokens.unwrap_or(0),
            tokens.cached = usage.cached_tokens.unwrap_or(0),
            tokens.total = usage.total_tokens.unwrap_or(0),
            "copilot usage"
        );
    }
    return Ok(value);
}

last_response_text = response.text().await.unwrap_or_default();
let sanitized_detail = sanitize_upstream_error(status, &last_response_text);
if retryable_status(status) && attempt < self.config.copilot_retry_max {
    let delay = retry_delay(response.headers(), attempt, self.config.copilot_retry_base_delay);
    tracing::warn!(
        api.family = family,
        http.status_code = status as u64,
        attempt = attempt as u64,
        retry.delay_ms = delay.as_millis() as u64,
        "copilot request retrying"
    );
    tokio::time::sleep(delay).await;
    continue;
}
tracing::warn!(
    api.family = family,
    http.status_code = status as u64,
    elapsed.ms = started.elapsed().as_millis() as u64,
    attempt = attempt as u64,
    "copilot request completed"
);
if retryable_status(status) {
    return Err(transient_error_for_status(status, sanitized_detail).into());
}
return Err(CopilotHttpError {
    status_code: status,
    detail: sanitized_detail,
}
.into());
```

Make sure `response.headers()` is read before `response.text()` consumes the response. If the compiler reports a move error, compute `delay` before reading text:

```rust
let delay = retry_delay(response.headers(), attempt, self.config.copilot_retry_base_delay);
last_response_text = response.text().await.unwrap_or_default();
```

- [ ] **Step 4: Run JSON retry test to verify it passes**

Run:

```bash
cargo test --quiet --test copilot_client_contract post_chat_retries_503_then_returns_json
```

Expected: PASS.

- [ ] **Step 5: Run existing retry tests**

Run:

```bash
cargo test --quiet --test copilot_client_contract post_chat_retries_429_then_returns_json copilot_backend_logs_retry_status_and_usage_metadata
```

Expected: PASS.

- [ ] **Step 6: Commit**

```bash
git add src/copilot/client.rs tests/copilot_client_contract.rs
git commit -m "fix: retry transient copilot json failures"
```

---

### Task 2: Unified Transient Retry for GET and Streaming Requests

**Files:**
- Modify: `src/copilot/client.rs`
- Test: `tests/copilot_client_contract.rs`

**Interfaces:**
- Consumes: `retryable_status(status: u16) -> bool`.
- Consumes: `transient_error_for_status(status: u16, detail: String) -> TransientBackendError`.
- Produces: consistent retry behavior in `get_json` and `stream_request`.

- [ ] **Step 1: Write failing stream 503 retry test**

Append to `tests/copilot_client_contract.rs`:

```rust
#[tokio::test]
async fn stream_chat_retries_503_then_returns_stream() {
    let mock = support::MockServer::start().await;
    mock.respond_sequence_json(
        "POST",
        "/chat/completions",
        vec![
            (
                503,
                serde_json::json!({"error": "temporarily unavailable"}),
                vec![("retry-after", "0")],
            ),
            (
                200,
                serde_json::json!({"ok": true}),
                vec![],
            ),
        ],
    )
    .await;
    mock.respond_json(
        "GET",
        "/copilot/token",
        200,
        serde_json::json!({"token": "copilot-token", "expires_at": 4_102_444_800u64}),
    )
    .await;

    let fixture = support::backend_fixture(mock).await;
    let response = fixture
        .backend
        .stream_chat(
            serde_json::json!({"model": "gpt-5.5", "stream": true, "messages": []})
                .as_object()
                .unwrap()
                .clone(),
            None,
        )
        .await
        .unwrap();

    assert!(response.status().is_success());
    assert_eq!(fixture.mock.hits("POST", "/chat/completions").await, 2);
}
```

- [ ] **Step 2: Write failing GET 503 retry test**

Append to `tests/copilot_client_contract.rs`:

```rust
#[tokio::test]
async fn refresh_models_retries_503_then_caches_models() {
    let mock = support::MockServer::start().await;
    mock.respond_sequence_json(
        "GET",
        "/models",
        vec![
            (
                503,
                serde_json::json!({"error": "temporarily unavailable"}),
                vec![("retry-after", "0")],
            ),
            (
                200,
                serde_json::json!({"data": [{"id": "gpt-transient-ok", "owned_by": "openai"}]}),
                vec![],
            ),
        ],
    )
    .await;
    mock.respond_json(
        "GET",
        "/copilot/token",
        200,
        serde_json::json!({"token": "copilot-token", "expires_at": 4_102_444_800u64}),
    )
    .await;

    let fixture = support::backend_fixture(mock).await;
    fixture.backend.refresh_models_if_stale().await;

    let models = fixture
        .models
        .list_for_snapshot(copilot_proxy_rs::state::BackendSnapshot {
            primary: copilot_proxy_rs::state::BackendKind::Copilot,
            fallback: None,
        })
        .await;
    assert!(models.data.iter().any(|model| model.id == "gpt-transient-ok"));
    assert_eq!(fixture.mock.hits("GET", "/models").await, 2);
}
```

- [ ] **Step 3: Run stream and GET retry tests to verify they fail**

Run:

```bash
cargo test --quiet --test copilot_client_contract stream_chat_retries_503_then_returns_stream refresh_models_retries_503_then_caches_models
```

Expected: FAIL because stream and GET retry only handle 429.

- [ ] **Step 4: Implement retries in `get_json`**

In `get_json`, replace 429-only retry logic with:

```rust
let status = response.status().as_u16();
if response.status().is_success() {
    let value = response.json().await.map_err(map_reqwest_error)?;
    tracing::info!(
        api.family = family,
        http.status_code = status as u64,
        elapsed.ms = started.elapsed().as_millis() as u64,
        attempt = attempt as u64,
        "copilot request completed"
    );
    return Ok(value);
}

let delay = retry_delay(response.headers(), attempt, self.config.copilot_retry_base_delay);
let detail = sanitize_upstream_error(status, &response.text().await.unwrap_or_default());
if retryable_status(status) && attempt < self.config.copilot_retry_max {
    tracing::warn!(
        api.family = family,
        http.status_code = status as u64,
        attempt = attempt as u64,
        retry.delay_ms = delay.as_millis() as u64,
        "copilot request retrying"
    );
    tokio::time::sleep(delay).await;
    continue;
}
tracing::warn!(
    api.family = family,
    http.status_code = status as u64,
    elapsed.ms = started.elapsed().as_millis() as u64,
    attempt = attempt as u64,
    "copilot request completed"
);
if retryable_status(status) {
    return Err(transient_error_for_status(status, detail).into());
}
return Err(CopilotHttpError {
    status_code: status,
    detail,
}
.into());
```

- [ ] **Step 5: Implement retries in `stream_request`**

In `stream_request`, replace 429-only retry logic with:

```rust
let status = response.status().as_u16();
if response.status().is_success() {
    tracing::info!(
        api.family = family,
        http.status_code = status as u64,
        elapsed.ms = started.elapsed().as_millis() as u64,
        stream = true,
        "copilot request completed"
    );
    return Ok(response);
}

let delay = retry_delay(response.headers(), attempt, self.config.copilot_retry_base_delay);
let detail = sanitize_upstream_error(status, &response.text().await.unwrap_or_default());
if retryable_status(status) && attempt < self.config.copilot_retry_max {
    tracing::warn!(
        api.family = family,
        http.status_code = status as u64,
        attempt = attempt as u64,
        retry.delay_ms = delay.as_millis() as u64,
        "copilot request retrying"
    );
    tokio::time::sleep(delay).await;
    last_err = Some(transient_error_for_status(status, detail).into());
    continue;
}
tracing::warn!(
    api.family = family,
    http.status_code = status as u64,
    elapsed.ms = started.elapsed().as_millis() as u64,
    stream = true,
    "copilot request completed"
);
return if retryable_status(status) {
    Err(transient_error_for_status(status, detail).into())
} else {
    Err(CopilotHttpError {
        status_code: status,
        detail,
    }
    .into())
};
```

- [ ] **Step 6: Run stream and GET retry tests to verify they pass**

Run:

```bash
cargo test --quiet --test copilot_client_contract stream_chat_retries_503_then_returns_stream refresh_models_retries_503_then_caches_models
```

Expected: PASS.

- [ ] **Step 7: Run full Copilot client contract**

Run:

```bash
cargo test --quiet --test copilot_client_contract
```

Expected: PASS.

- [ ] **Step 8: Commit**

```bash
git add src/copilot/client.rs tests/copilot_client_contract.rs
git commit -m "fix: retry transient copilot stream and model requests"
```

---

### Task 3: Retry Delay Jitter Without Breaking Zero Retry-After Tests

**Files:**
- Modify: `src/copilot/client.rs`
- Test: `tests/copilot_client_contract.rs`

**Interfaces:**
- Consumes: existing `retry_delay` function.
- Produces: `fn retry_delay_with_jitter(headers: &HeaderMap, attempt: u32, base_delay_seconds: f64) -> Duration`.

- [ ] **Step 1: Write deterministic delay tests**

Append to the internal unit test module in `src/copilot/client.rs`, or create it if absent:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use http::HeaderMap;

    #[test]
    fn retry_delay_honors_zero_retry_after_without_jitter() {
        let mut headers = HeaderMap::new();
        headers.insert("retry-after", "0".parse().unwrap());

        assert_eq!(
            retry_delay_with_jitter(&headers, 2, 1.0),
            Duration::from_secs(0)
        );
    }

    #[test]
    fn retry_delay_adds_bounded_jitter_when_retry_after_absent() {
        let headers = HeaderMap::new();
        let delay = retry_delay_with_jitter(&headers, 2, 1.0);

        assert!(delay >= Duration::from_secs(4));
        assert!(delay <= Duration::from_millis(4400));
    }
}
```

- [ ] **Step 2: Run delay tests to verify they fail**

Run:

```bash
cargo test --quiet retry_delay_honors_zero_retry_after_without_jitter retry_delay_adds_bounded_jitter_when_retry_after_absent
```

Expected: FAIL because `retry_delay_with_jitter` does not exist.

- [ ] **Step 3: Implement deterministic bounded jitter**

Add to `src/copilot/client.rs`:

```rust
fn retry_delay_with_jitter(
    headers: &http::HeaderMap,
    attempt: u32,
    base_delay_seconds: f64,
) -> Duration {
    if headers.get("retry-after").is_some() {
        return retry_delay(headers, attempt, base_delay_seconds);
    }
    let base = retry_delay(headers, attempt, base_delay_seconds);
    let jitter_seed = (attempt as u64).wrapping_mul(37) % 100;
    let jitter = Duration::from_millis(jitter_seed * 4);
    base + jitter
}
```

Replace retry wait calculations in `post_json`, `get_json`, and `stream_request`:

```rust
let delay = retry_delay_with_jitter(
    response.headers(),
    attempt,
    self.config.copilot_retry_base_delay,
);
```

- [ ] **Step 4: Run delay tests to verify they pass**

Run:

```bash
cargo test --quiet retry_delay_honors_zero_retry_after_without_jitter retry_delay_adds_bounded_jitter_when_retry_after_absent
```

Expected: PASS.

- [ ] **Step 5: Run all retry tests**

Run:

```bash
cargo test --quiet --test copilot_client_contract
```

Expected: PASS.

- [ ] **Step 6: Commit**

```bash
git add src/copilot/client.rs
git commit -m "fix: add bounded jitter to copilot retries"
```

---

### Task 4: Align Backend Scope With Implemented Copilot-Only Runtime

**Files:**
- Modify: `src/state.rs`
- Modify: `src/models.rs`
- Modify: `src/main.rs`
- Modify: `src/config.rs`
- Modify: `README.md`
- Modify: `config.example.json`
- Test: `tests/http_contract.rs`
- Test: `tests/state_contract.rs`
- Test: `tests/config_contract.rs`

**Interfaces:**
- Produces: `BackendKind` with only `Copilot` until a real Bedrock request path exists.
- Produces: `/health` response with `backend: "copilot"` and no misleading fallback unless a real fallback exists.

- [ ] **Step 1: Write/update scope tests**

In `tests/state_contract.rs`, replace Bedrock-specific expectations with:

```rust
#[test]
fn backend_kind_only_parses_copilot_for_active_runtime() {
    assert_eq!(BackendKind::parse("copilot"), Some(BackendKind::Copilot));
    assert_eq!(BackendKind::parse("bedrock"), None);
    assert_eq!(BackendKind::parse(""), None);
}
```

In `tests/http_contract.rs`, remove or rewrite tests named `bedrock_model_list_contains_anthropic_models_only_without_copilot_fallback` and `bedrock_with_copilot_fallback_includes_non_claude_copilot_models` to this single test:

```rust
#[test]
fn copilot_model_list_contains_supported_static_models() {
    let response = model_list_for_snapshot(BackendSnapshot {
        primary: BackendKind::Copilot,
        fallback: None,
    });

    assert_eq!(response.object, "list");
    assert!(response.data.iter().any(|model| model.id == "claude-sonnet-4-6"));
    assert!(response.data.iter().any(|model| model.id == "gpt-5.4"));
    assert!(response.data.iter().all(|model| model.object == "model"));
}
```

- [ ] **Step 2: Run scope tests to verify they fail**

Run:

```bash
cargo test --quiet --test state_contract backend_kind_only_parses_copilot_for_active_runtime
```

Expected: FAIL because `BackendKind::parse("bedrock")` currently returns `Some(Bedrock)`.

- [ ] **Step 3: Remove inactive Bedrock runtime variant**

Modify `src/state.rs`:

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BackendKind {
    Copilot,
}

impl BackendKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Copilot => "copilot",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "copilot" => Some(Self::Copilot),
            _ => None,
        }
    }

    pub fn parse_optional(value: &str) -> Option<Self> {
        let value = value.trim();
        if value.is_empty() {
            None
        } else {
            Self::parse(value)
        }
    }
}
```

Modify `src/models.rs` so `model_list_for_snapshot` no longer matches on Bedrock:

```rust
pub fn model_list_for_snapshot(_snapshot: BackendSnapshot) -> ModelsListResponse {
    ModelsListResponse {
        object: "list",
        data: copilot_models(),
    }
}
```

Remove `BEDROCK_MODEL_MAP`, `bedrock_models`, and `append_non_claude_copilot_models` if no tests or callers remain.

- [ ] **Step 4: Align CLI and config wording**

Modify `src/main.rs` command description:

```rust
about = "Rust API proxy for GitHub Copilot-backed OpenAI, Anthropic, and Responses clients"
```

In `src/config.rs`, keep `fallback_backend` parsing as a reserved no-op for compatibility, but warn through `AppState::new` when it is set and unrecognized. Do not add active fallback routing until a real backend exists.

- [ ] **Step 5: Run scope tests to verify they pass**

Run:

```bash
cargo test --quiet --test state_contract --test http_contract --test config_contract
```

Expected: PASS.

- [ ] **Step 6: Update docs/config examples**

In `README.md`, ensure the opening description remains:

```markdown
`copilot-proxy-rs` is an experimental local Rust proxy that exposes OpenAI,
Anthropic Messages, and OpenAI Responses-style routes backed by GitHub Copilot.
```

In `config.example.json`, remove `"fallback_backend": ""` if compatibility is not needed in examples:

```json
{
  "backend": "copilot",
  "host": "127.0.0.1",
  "port": 8080,
  "allow_non_loopback_bind": false,
  "container_loopback_only": false,
  "copilot_timeout": 300,
  "copilot_models_ttl": 300,
  "copilot_retry_max": 3,
  "copilot_retry_base_delay": 1.0,
  "copilot_max_rate": 15,
  "log_level": "INFO"
}
```

- [ ] **Step 7: Run full verification**

Run:

```bash
cargo test --quiet && cargo clippy --all-targets --quiet -- -D warnings
```

Expected: PASS.

- [ ] **Step 8: Commit**

```bash
git add src/state.rs src/models.rs src/main.rs src/config.rs README.md config.example.json tests/state_contract.rs tests/http_contract.rs tests/config_contract.rs
git commit -m "refactor: align backend scope with copilot runtime"
```

---

## Self-Review Notes

- Spec coverage: JSON, GET, and streaming transient retries are covered; stale backend/fallback scope is covered.
- Placeholder scan: no TODO/TBD placeholders remain.
- Type consistency: retry helpers return `Duration` and reuse existing `TransientBackendError`; backend scope remains `BackendKind::Copilot`.
