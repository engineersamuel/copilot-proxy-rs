# Inbound Auth Hardening Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add optional inbound authentication, WebSocket origin checks, and decompressed request-size limits so the local proxy is safer when users intentionally expose it beyond a single trusted client.

**Architecture:** Keep the default zero-friction local workflow unchanged: no inbound auth is required unless configured. Add a focused HTTP middleware module for Bearer/API-key validation and origin policy, and keep request-body expansion limits in the existing request-body module because all JSON parsing already flows through it.

**Tech Stack:** Rust 1.85+, Axum 0.8 middleware, Tokio, serde/serde_json, flate2, zstd, existing contract tests.

## Global Constraints

- Preserve default local behavior: existing requests without `COPILOT_PROXY_RS_API_KEY` must continue to work.
- Do not log raw prompt bodies, GitHub tokens, Copilot tokens, or configured inbound API keys.
- All Copilot-backed routes use the server operator's Copilot credentials; default bind host remains `127.0.0.1`.
- WebSocket clients must honor the same inbound policy as HTTP routes.
- Use existing test commands: `cargo test --quiet` and `cargo clippy --all-targets --quiet -- -D warnings`.
- Do not add new runtime dependencies unless Axum/Tower already in `Cargo.toml` cannot cover the implementation.

---

## File Structure

- Create `src/http/auth.rs`: optional inbound auth middleware and reusable WebSocket/origin validation helpers.
- Modify `src/http/mod.rs`: expose the new `auth` submodule.
- Modify `src/http/routes.rs`: layer the middleware around the router and enforce origin policy during WebSocket upgrade.
- Modify `src/config.rs`: add `api_key`, `allowed_origins`, and `max_decoded_body_bytes` config fields plus file/env loading.
- Modify `src/request_body.rs`: add bounded decompression/parsing functions and keep existing unbounded functions as wrappers for tests and non-route callers.
- Modify `config.example.json`, `.env.example`, and `README.md`: document the new hardening knobs.
- Modify `tests/http_contract.rs`, `tests/responses_ws_contract.rs`, `tests/request_body_contract.rs`, and `tests/config_contract.rs`: cover defaults, enabled auth, origin checks, and decompression limits.

---

### Task 1: Optional HTTP Bearer/API-Key Authentication

**Files:**
- Create: `src/http/auth.rs`
- Modify: `src/http/mod.rs`
- Modify: `src/http/routes.rs`
- Modify: `src/config.rs`
- Test: `tests/config_contract.rs`
- Test: `tests/http_contract.rs`

**Interfaces:**
- Consumes: `AppConfig` from `src/config.rs`, `AppState` from `src/state.rs`.
- Produces: `pub async fn require_inbound_auth(State(state): State<AppState>, request: Request, next: Next) -> Response`.
- Produces: `pub fn request_has_valid_api_key(headers: &HeaderMap, configured_api_key: &str) -> bool`.

- [ ] **Step 1: Write failing config tests**

Append these tests to `tests/config_contract.rs`:

```rust
#[test]
fn default_inbound_auth_config_is_disabled() {
    let temp = repo_tempdir("config-default-auth-");
    let env = EnvSource::from_pairs([
        ("HOME", temp.path().to_str().unwrap()),
        ("COPILOT_PROXY_RS_CONFIG_DIR", temp.path().to_str().unwrap()),
    ]);

    let config = AppConfig::load_from_env(&env).unwrap();

    assert_eq!(config.api_key, "");
    assert!(config.allowed_origins.is_empty());
    assert_eq!(config.max_decoded_body_bytes, 16 * 1024 * 1024);
}

#[test]
fn env_overrides_inbound_auth_config() {
    let temp = repo_tempdir("config-env-auth-");
    let env = EnvSource::from_pairs([
        ("HOME", temp.path().to_str().unwrap()),
        ("COPILOT_PROXY_RS_CONFIG_DIR", temp.path().to_str().unwrap()),
        ("COPILOT_PROXY_RS_API_KEY", "local-secret"),
        ("COPILOT_PROXY_RS_ALLOWED_ORIGINS", "http://localhost:3000,https://example.test"),
        ("COPILOT_PROXY_RS_MAX_DECODED_BODY_BYTES", "4096"),
    ]);

    let config = AppConfig::load_from_env(&env).unwrap();

    assert_eq!(config.api_key, "local-secret");
    assert_eq!(
        config.allowed_origins,
        vec![
            "http://localhost:3000".to_string(),
            "https://example.test".to_string()
        ]
    );
    assert_eq!(config.max_decoded_body_bytes, 4096);
}
```

- [ ] **Step 2: Run config tests to verify they fail**

Run:

```bash
cargo test --quiet --test config_contract default_inbound_auth_config_is_disabled env_overrides_inbound_auth_config
```

Expected: FAIL with missing `api_key`, `allowed_origins`, or `max_decoded_body_bytes` fields on `AppConfig`.

- [ ] **Step 3: Implement config fields**

Modify `src/config.rs`:

```rust
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(default)]
pub struct AppConfig {
    #[serde(deserialize_with = "deserialize_string")]
    pub backend: String,
    #[serde(deserialize_with = "deserialize_string")]
    pub fallback_backend: String,
    #[serde(deserialize_with = "deserialize_string")]
    pub host: String,
    #[serde(deserialize_with = "deserialize_u16")]
    pub port: u16,
    #[serde(deserialize_with = "deserialize_u64")]
    pub copilot_timeout: u64,
    #[serde(deserialize_with = "deserialize_u64")]
    pub copilot_models_ttl: u64,
    #[serde(deserialize_with = "deserialize_u32")]
    pub copilot_retry_max: u32,
    #[serde(deserialize_with = "deserialize_f64")]
    pub copilot_retry_base_delay: f64,
    #[serde(deserialize_with = "deserialize_u32")]
    pub copilot_max_rate: u32,
    #[serde(deserialize_with = "deserialize_f64")]
    pub context_guard_threshold: f64,
    #[serde(deserialize_with = "deserialize_string")]
    pub bedrock_region_prefix: String,
    #[serde(deserialize_with = "deserialize_string")]
    pub aws_region: String,
    #[serde(deserialize_with = "deserialize_u64")]
    pub bedrock_read_timeout: u64,
    #[serde(default = "default_true", deserialize_with = "deserialize_bool")]
    pub update_check: bool,
    #[serde(default = "default_true", deserialize_with = "deserialize_bool")]
    pub auto_restart: bool,
    #[serde(deserialize_with = "deserialize_bool")]
    pub auto_update: bool,
    #[serde(deserialize_with = "deserialize_bool")]
    pub allow_non_loopback_bind: bool,
    #[serde(deserialize_with = "deserialize_bool")]
    pub container_loopback_only: bool,
    #[serde(deserialize_with = "deserialize_string")]
    pub api_key: String,
    #[serde(deserialize_with = "deserialize_string_vec")]
    pub allowed_origins: Vec<String>,
    #[serde(deserialize_with = "deserialize_u64")]
    pub max_decoded_body_bytes: u64,
    #[serde(deserialize_with = "deserialize_string")]
    pub log_level: String,
    #[serde(deserialize_with = "deserialize_string")]
    pub cowork_host: String,
    #[serde(deserialize_with = "deserialize_u16")]
    pub cowork_port: u16,
    #[serde(deserialize_with = "deserialize_bool")]
    pub hide_getting_started: bool,
    pub enabled_mcp_servers: BTreeMap<String, bool>,
    #[serde(deserialize_with = "deserialize_string_vec")]
    pub search_provider_order: Vec<String>,
    pub model_overrides: ModelOverrides,
}
```

Add defaults in `impl Default for AppConfig`:

```rust
api_key: String::new(),
allowed_origins: Vec::new(),
max_decoded_body_bytes: 16 * 1024 * 1024,
```

Add fields to `FileConfig`:

```rust
#[serde(deserialize_with = "deserialize_opt_string")]
api_key: Option<String>,
#[serde(deserialize_with = "deserialize_opt_string_vec")]
allowed_origins: Option<Vec<String>>,
#[serde(deserialize_with = "deserialize_opt_u64")]
max_decoded_body_bytes: Option<u64>,
```

Add merge logic in `merge_file_values` after `container_loopback_only`:

```rust
if let Some(v) = file.api_key {
    self.api_key = v;
}
if let Some(v) = file.allowed_origins {
    self.allowed_origins = v;
}
if let Some(v) = file.max_decoded_body_bytes {
    if v > 0 {
        self.max_decoded_body_bytes = v;
    }
}
```

Add env overrides in `apply_env_overrides` after `COPILOT_PROXY_RS_CONTAINER_LOOPBACK_ONLY`:

```rust
apply_string(env, "COPILOT_PROXY_RS_API_KEY", &mut config.api_key);
if let Some(value) = env.get("COPILOT_PROXY_RS_ALLOWED_ORIGINS") {
    config.allowed_origins = value
        .split(',')
        .map(str::trim)
        .filter(|origin| !origin.is_empty())
        .map(str::to_string)
        .collect();
}
apply_parse(
    env,
    "COPILOT_PROXY_RS_MAX_DECODED_BODY_BYTES",
    &mut config.max_decoded_body_bytes,
);
if config.max_decoded_body_bytes == 0 {
    config.max_decoded_body_bytes = 16 * 1024 * 1024;
}
```

- [ ] **Step 4: Run config tests to verify they pass**

Run:

```bash
cargo test --quiet --test config_contract default_inbound_auth_config_is_disabled env_overrides_inbound_auth_config
```

Expected: PASS.

- [ ] **Step 5: Write failing HTTP auth tests**

Append this helper and tests to `tests/http_contract.rs`:

```rust
async fn authed_test_state(api_key: &str) -> AppState {
    let mut config = AppConfig::default();
    config.api_key = api_key.to_string();
    AppState::new(config)
}

#[tokio::test]
async fn health_does_not_require_inbound_auth() {
    let app = router(authed_test_state("local-secret").await);
    let response = app
        .oneshot(
            Request::builder()
                .uri("/health")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
}

#[tokio::test]
async fn copilot_routes_reject_missing_inbound_auth_when_configured() {
    let app = router(authed_test_state("local-secret").await);
    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/chat/completions")
                .header("content-type", "application/json")
                .body(Body::from(
                    r#"{"model":"gpt-5.5","messages":[{"role":"user","content":"hello"}]}"#,
                ))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    let body = response_json(response).await;
    assert_eq!(body["error"]["type"], "authentication_error");
}

#[tokio::test]
async fn copilot_routes_accept_bearer_inbound_auth_when_configured() {
    let temp = repo_tempdir("http-contract-bearer-auth-");
    let env =
        EnvSource::from_pairs([("COPILOT_PROXY_RS_CONFIG_DIR", temp.path().to_str().unwrap())]);
    let mut config = AppConfig::load_from_env(&env).unwrap();
    config.api_key = "local-secret".to_string();
    let config = Arc::new(config);
    let auth = Arc::new(CopilotAuth::with_env_for_tests(
        config.clone(),
        env,
        AuthEndpoints::localhost_for_tests(),
        false,
    ));
    let models = Arc::new(ModelRegistry::new());
    let copilot = Arc::new(CopilotBackend::with_endpoints_for_tests(
        config.clone(),
        auth,
        models.clone(),
        CopilotEndpoints::default(),
    ));
    let app = router(AppState::with_parts_for_tests(config, models, copilot));
    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/chat/completions")
                .header("content-type", "application/json")
                .header("authorization", "Bearer local-secret")
                .body(Body::from(
                    r#"{"model":"gpt-5.5","messages":[{"role":"user","content":"hello"}]}"#,
                ))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(
        response.status(),
        StatusCode::UNAUTHORIZED,
        "valid inbound auth should allow the request through to Copilot auth"
    );
    let body = response_json(response).await;
    assert_eq!(body["error"]["type"], "authentication_error");
    assert_ne!(body["error"]["message"], "Missing or invalid proxy API key");
}

#[tokio::test]
async fn copilot_routes_accept_x_api_key_inbound_auth_when_configured() {
    let mut config = AppConfig::default();
    config.api_key = "local-secret".to_string();
    let app = router(AppState::new(config));
    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/messages/count_tokens")
                .header("content-type", "application/json")
                .header("x-api-key", "local-secret")
                .body(Body::from(r#"{"messages":[{"role":"user","content":"hello"}]}"#))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
}
```

- [ ] **Step 6: Run HTTP auth tests to verify they fail**

Run:

```bash
cargo test --quiet --test http_contract health_does_not_require_inbound_auth copilot_routes_reject_missing_inbound_auth_when_configured copilot_routes_accept_bearer_inbound_auth_when_configured copilot_routes_accept_x_api_key_inbound_auth_when_configured
```

Expected: FAIL because no inbound middleware exists.

- [ ] **Step 7: Implement HTTP auth middleware**

Create `src/http/auth.rs`:

```rust
use axum::body::Body;
use axum::extract::{Request, State};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use http::{HeaderMap, StatusCode};

use crate::errors::openai_error;
use crate::state::AppState;

pub fn request_has_valid_api_key(headers: &HeaderMap, configured_api_key: &str) -> bool {
    if configured_api_key.is_empty() {
        return true;
    }
    let bearer_matches = headers
        .get(http::header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
        .is_some_and(|token| token == configured_api_key);
    let api_key_matches = headers
        .get("x-api-key")
        .and_then(|value| value.to_str().ok())
        .is_some_and(|token| token == configured_api_key);
    bearer_matches || api_key_matches
}

pub async fn require_inbound_auth(
    State(state): State<AppState>,
    request: Request,
    next: Next,
) -> Response {
    if request_has_valid_api_key(request.headers(), &state.config.api_key) {
        return next.run(request).await;
    }
    openai_error(
        StatusCode::UNAUTHORIZED,
        "authentication_error",
        "Missing or invalid proxy API key",
    )
    .into_response()
}

pub fn unauthorized_ws_response() -> Response<Body> {
    openai_error(
        StatusCode::UNAUTHORIZED,
        "authentication_error",
        "Missing or invalid proxy API key",
    )
    .into_response()
}
```

Modify `src/http/mod.rs`:

```rust
pub mod auth;
pub mod routes;
```

Modify `router` in `src/http/routes.rs`:

```rust
use axum::middleware;
```

Replace the current router construction with this shape:

```rust
pub fn router(state: AppState) -> Router {
    let protected = Router::new()
        .route("/v1/models", get(list_models))
        .route("/v1/messages/count_tokens", post(count_tokens))
        .route("/v1/messages", post(messages))
        .route("/v1/chat/completions", post(chat_completions))
        .route("/v1/responses", post(responses).get(responses_ws))
        .route("/v1/responses/compact", post(responses_compact))
        .route("/v1/responses/{response_id}", get(responses_retrieve))
        .route("/v1/responses/{response_id}/cancel", post(responses_cancel))
        .route_layer(middleware::from_fn_with_state(
            state.clone(),
            crate::http::auth::require_inbound_auth,
        ));

    Router::new()
        .route("/health", get(health))
        .route("/version", get(version))
        .merge(protected)
        .with_state(state)
}
```

- [ ] **Step 8: Run HTTP auth tests to verify they pass**

Run:

```bash
cargo test --quiet --test http_contract health_does_not_require_inbound_auth copilot_routes_reject_missing_inbound_auth_when_configured copilot_routes_accept_bearer_inbound_auth_when_configured copilot_routes_accept_x_api_key_inbound_auth_when_configured
```

Expected: PASS.

- [ ] **Step 9: Run auth-related full tests**

Run:

```bash
cargo test --quiet --test http_contract --test config_contract
```

Expected: PASS.

- [ ] **Step 10: Commit**

```bash
git add src/http/auth.rs src/http/mod.rs src/http/routes.rs src/config.rs tests/http_contract.rs tests/config_contract.rs
git commit -m "feat: add optional inbound proxy authentication"
```

---

### Task 2: WebSocket Origin Checks

**Files:**
- Modify: `src/http/auth.rs`
- Modify: `src/http/routes.rs`
- Test: `tests/responses_ws_contract.rs`

**Interfaces:**
- Consumes: `request_has_valid_api_key(headers: &HeaderMap, configured_api_key: &str) -> bool`.
- Produces: `pub fn request_has_allowed_origin(headers: &HeaderMap, allowed_origins: &[String]) -> bool`.
- Produces: `pub fn forbidden_ws_response(message: &'static str) -> Response<Body>`.

- [ ] **Step 1: Write failing WebSocket auth/origin tests**

Append these tests to `tests/responses_ws_contract.rs`:

```rust
async fn start_proxy_with_config(config: AppConfig) -> SocketAddr {
    let fixture = support::AppFixture::with_mock_copilot().await;
    let state = AppState {
        config: Arc::new(config),
        backend: fixture.state.backend,
        models: fixture.state.models,
        copilot: fixture.state.copilot,
        responses: fixture.state.responses,
    };
    start_proxy(state).await
}

#[tokio::test]
async fn responses_websocket_rejects_missing_inbound_auth_when_configured() {
    let fixture = support::AppFixture::with_mock_copilot().await;
    let mut config = (*fixture.state.config).clone();
    config.api_key = "local-secret".to_string();
    let state = AppState {
        config: Arc::new(config),
        backend: fixture.state.backend,
        models: fixture.state.models,
        copilot: fixture.state.copilot,
        responses: fixture.state.responses,
    };
    let addr = start_proxy(state).await;

    let url = format!("ws://{addr}/v1/responses");
    let err = connect_async(&url).await.unwrap_err();

    assert!(
        err.to_string().contains("HTTP error"),
        "expected rejected websocket upgrade, got {err}"
    );
}

#[tokio::test]
async fn responses_websocket_rejects_disallowed_origin_when_configured() {
    use tokio_tungstenite::tungstenite::client::IntoClientRequest;

    let fixture = support::AppFixture::with_mock_copilot().await;
    let mut config = (*fixture.state.config).clone();
    config.api_key = "local-secret".to_string();
    config.allowed_origins = vec!["http://localhost:3000".to_string()];
    let state = AppState {
        config: Arc::new(config),
        backend: fixture.state.backend,
        models: fixture.state.models,
        copilot: fixture.state.copilot,
        responses: fixture.state.responses,
    };
    let addr = start_proxy(state).await;

    let mut request = format!("ws://{addr}/v1/responses")
        .into_client_request()
        .unwrap();
    request
        .headers_mut()
        .insert("authorization", "Bearer local-secret".parse().unwrap());
    request
        .headers_mut()
        .insert("origin", "https://evil.example".parse().unwrap());

    let err = connect_async(request).await.unwrap_err();

    assert!(
        err.to_string().contains("HTTP error"),
        "expected rejected websocket upgrade, got {err}"
    );
}
```

- [ ] **Step 2: Run WebSocket tests to verify they fail**

Run:

```bash
cargo test --quiet --test responses_ws_contract responses_websocket_rejects_missing_inbound_auth_when_configured responses_websocket_rejects_disallowed_origin_when_configured
```

Expected: FAIL because WebSocket upgrade currently accepts unauthenticated clients and does not check `Origin`.

- [ ] **Step 3: Implement origin helpers**

Extend `src/http/auth.rs`:

```rust
pub fn request_has_allowed_origin(headers: &HeaderMap, allowed_origins: &[String]) -> bool {
    if allowed_origins.is_empty() {
        return true;
    }
    headers
        .get(http::header::ORIGIN)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|origin| allowed_origins.iter().any(|allowed| allowed == origin))
}

pub fn forbidden_ws_response(message: &'static str) -> Response<Body> {
    openai_error(StatusCode::FORBIDDEN, "invalid_request_error", message).into_response()
}
```

- [ ] **Step 4: Apply checks in WebSocket handler**

Modify the `responses_ws` function in `src/http/routes.rs` to extract headers:

```rust
async fn responses_ws(
    State(state): State<AppState>,
    headers: HeaderMap,
    ws: axum::extract::WebSocketUpgrade,
) -> Response {
    if !crate::http::auth::request_has_valid_api_key(&headers, &state.config.api_key) {
        return crate::http::auth::unauthorized_ws_response();
    }
    if !crate::http::auth::request_has_allowed_origin(&headers, &state.config.allowed_origins) {
        return crate::http::auth::forbidden_ws_response("WebSocket origin is not allowed");
    }
    ws.on_upgrade(move |socket| handle_responses_ws(state, socket))
}
```

If the route middleware from Task 1 already handles HTTP auth before this function, keep the explicit check here anyway so WebSocket auth remains obvious and testable at the upgrade boundary.

- [ ] **Step 5: Run WebSocket tests to verify they pass**

Run:

```bash
cargo test --quiet --test responses_ws_contract responses_websocket_rejects_missing_inbound_auth_when_configured responses_websocket_rejects_disallowed_origin_when_configured
```

Expected: PASS.

- [ ] **Step 6: Run full WebSocket contract**

Run:

```bash
cargo test --quiet --test responses_ws_contract
```

Expected: PASS.

- [ ] **Step 7: Commit**

```bash
git add src/http/auth.rs src/http/routes.rs tests/responses_ws_contract.rs
git commit -m "feat: enforce websocket proxy origin policy"
```

---

### Task 3: Decoded Request Body Size Limits

**Files:**
- Modify: `src/request_body.rs`
- Modify: `src/http/routes.rs`
- Test: `tests/request_body_contract.rs`
- Test: `tests/http_contract.rs`

**Interfaces:**
- Consumes: `AppConfig.max_decoded_body_bytes: u64`.
- Produces: `pub fn parse_json_request_body_with_limit(raw_body: &[u8], content_encoding: &str, max_decoded_bytes: usize) -> Result<Map<String, Value>, RequestBodyError>`.
- Produces: `RequestBodyError::DecodedBodyTooLarge { limit: usize }`.

- [ ] **Step 1: Write failing request-body limit tests**

Append to `tests/request_body_contract.rs`:

```rust
#[test]
fn identity_body_over_limit_is_rejected() {
    let err = parse_json_request_body_with_limit(br#"{"message":"hello"}"#, "identity", 8)
        .unwrap_err();

    assert!(matches!(err, RequestBodyError::DecodedBodyTooLarge { limit } if limit == 8));
}

#[test]
fn gzip_body_over_decoded_limit_is_rejected() {
    let payload = gzip_bytes(br#"{"message":"hello"}"#);
    let err = parse_json_request_body_with_limit(&payload, "gzip", 8).unwrap_err();

    assert!(matches!(err, RequestBodyError::DecodedBodyTooLarge { limit } if limit == 8));
}

#[test]
fn zstd_body_over_decoded_limit_is_rejected() {
    let payload = zstd::bulk::compress(br#"{"message":"hello"}"#, 0).unwrap();
    let err = parse_json_request_body_with_limit(&payload, "zstd", 8).unwrap_err();

    assert!(matches!(err, RequestBodyError::DecodedBodyTooLarge { limit } if limit == 8));
}
```

Also update the import:

```rust
use copilot_proxy_rs::request_body::{
    RequestBodyError, decode_request_body, parse_json_request_body,
    parse_json_request_body_with_limit,
};
```

- [ ] **Step 2: Run request-body tests to verify they fail**

Run:

```bash
cargo test --quiet --test request_body_contract identity_body_over_limit_is_rejected gzip_body_over_decoded_limit_is_rejected zstd_body_over_decoded_limit_is_rejected
```

Expected: FAIL because the bounded parser and error variant do not exist.

- [ ] **Step 3: Implement bounded parser**

Modify `src/request_body.rs`:

```rust
#[derive(Debug, Error, PartialEq, Eq)]
pub enum RequestBodyError {
    #[error("unsupported content-encoding: {encoding}")]
    UnsupportedContentEncoding { encoding: String },
    #[error("invalid {encoding} request body: {message}")]
    InvalidCompressedBody { encoding: String, message: String },
    #[error("decoded request body exceeds {limit} bytes")]
    DecodedBodyTooLarge { limit: usize },
    #[error("{message}")]
    InvalidJson { message: String },
}
```

Add these functions and update existing wrappers:

```rust
pub fn decode_request_body_with_limit(
    raw_body: &[u8],
    content_encoding: &str,
    max_decoded_bytes: usize,
) -> Result<Vec<u8>, RequestBodyError> {
    let encoding = content_encoding.trim().to_ascii_lowercase();
    if encoding.contains("gzip") {
        return decompress_gzip_with_limit(raw_body, max_decoded_bytes);
    }
    if encoding.contains("zstd") {
        let mut decoder = zstd::stream::read::Decoder::new(raw_body).map_err(|source| {
            RequestBodyError::InvalidCompressedBody {
                encoding: "zstd".to_string(),
                message: source.to_string(),
            }
        })?;
        return read_to_limit(&mut decoder, max_decoded_bytes, "zstd");
    }
    if encoding.is_empty() || encoding == "identity" {
        if raw_body.len() > max_decoded_bytes {
            return Err(RequestBodyError::DecodedBodyTooLarge {
                limit: max_decoded_bytes,
            });
        }
        return Ok(raw_body.to_vec());
    }
    Err(RequestBodyError::UnsupportedContentEncoding { encoding })
}

pub fn parse_json_request_body_with_limit(
    raw_body: &[u8],
    content_encoding: &str,
    max_decoded_bytes: usize,
) -> Result<Map<String, Value>, RequestBodyError> {
    let decoded = decode_request_body_with_limit(raw_body, content_encoding, max_decoded_bytes)?;
    parse_json_value(decoded)
}

pub fn decode_request_body(
    raw_body: &[u8],
    content_encoding: &str,
) -> Result<Vec<u8>, RequestBodyError> {
    decode_request_body_with_limit(raw_body, content_encoding, usize::MAX)
}

pub fn parse_json_request_body(
    raw_body: &[u8],
    content_encoding: &str,
) -> Result<Map<String, Value>, RequestBodyError> {
    parse_json_request_body_with_limit(raw_body, content_encoding, usize::MAX)
}

fn parse_json_value(decoded: Vec<u8>) -> Result<Map<String, Value>, RequestBodyError> {
    let value: Value =
        serde_json::from_slice(&decoded).map_err(|source| RequestBodyError::InvalidJson {
            message: source.to_string(),
        })?;
    match value {
        Value::Object(object) => Ok(object),
        _ => Err(RequestBodyError::InvalidJson {
            message: "Top-level JSON body must be an object".to_string(),
        }),
    }
}

fn decompress_gzip_with_limit(
    raw_body: &[u8],
    max_decoded_bytes: usize,
) -> Result<Vec<u8>, RequestBodyError> {
    let mut decoder = GzDecoder::new(raw_body);
    read_to_limit(&mut decoder, max_decoded_bytes, "gzip")
}

fn read_to_limit<R: Read>(
    reader: &mut R,
    max_decoded_bytes: usize,
    encoding: &'static str,
) -> Result<Vec<u8>, RequestBodyError> {
    let mut decoded = Vec::new();
    let limit = max_decoded_bytes.saturating_add(1) as u64;
    reader
        .take(limit)
        .read_to_end(&mut decoded)
        .map_err(|source| RequestBodyError::InvalidCompressedBody {
            encoding: encoding.to_string(),
            message: source.to_string(),
        })?;
    if decoded.len() > max_decoded_bytes {
        return Err(RequestBodyError::DecodedBodyTooLarge {
            limit: max_decoded_bytes,
        });
    }
    Ok(decoded)
}
```

Remove the old `decompress_gzip` function after replacing callers.

- [ ] **Step 4: Run request-body tests to verify they pass**

Run:

```bash
cargo test --quiet --test request_body_contract
```

Expected: PASS.

- [ ] **Step 5: Write failing route-level limit test**

Append to `tests/http_contract.rs`:

```rust
#[tokio::test]
async fn chat_route_rejects_body_over_decoded_limit() {
    let mut config = AppConfig::default();
    config.max_decoded_body_bytes = 8;
    let app = router(AppState::new(config));
    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/chat/completions")
                .header("content-type", "application/json")
                .body(Body::from(
                    r#"{"model":"gpt-5.5","messages":[{"role":"user","content":"hello"}]}"#,
                ))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let body = response_json(response).await;
    assert_eq!(body["error"]["type"], "invalid_request_error");
    assert!(
        body["error"]["message"]
            .as_str()
            .unwrap()
            .contains("decoded request body exceeds 8 bytes")
    );
}
```

- [ ] **Step 6: Run route-level test to verify it fails**

Run:

```bash
cargo test --quiet --test http_contract chat_route_rejects_body_over_decoded_limit
```

Expected: FAIL because route handlers still call the unbounded parser.

- [ ] **Step 7: Use bounded parser in routes**

Modify the import in `src/http/routes.rs`:

```rust
use crate::request_body::parse_json_request_body_with_limit;
```

In every handler that currently calls `parse_json_request_body(&body, encoding)`, replace it with:

```rust
parse_json_request_body_with_limit(
    &body,
    encoding,
    state.config.max_decoded_body_bytes as usize,
)
```

For `count_tokens`, this means the function already has `State(state): State<AppState>` available if it does not today. Update its signature from:

```rust
async fn count_tokens(
    headers: HeaderMap,
    body: Bytes,
) -> Result<Json<CountTokensResponse>, (StatusCode, Json<crate::errors::AnthropicErrorResponse>)>
```

to:

```rust
async fn count_tokens(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Json<CountTokensResponse>, (StatusCode, Json<crate::errors::AnthropicErrorResponse>)>
```

- [ ] **Step 8: Run route-level test to verify it passes**

Run:

```bash
cargo test --quiet --test http_contract chat_route_rejects_body_over_decoded_limit
```

Expected: PASS.

- [ ] **Step 9: Run request parsing and HTTP contracts**

Run:

```bash
cargo test --quiet --test request_body_contract --test http_contract
```

Expected: PASS.

- [ ] **Step 10: Commit**

```bash
git add src/request_body.rs src/http/routes.rs tests/request_body_contract.rs tests/http_contract.rs
git commit -m "feat: bound decoded request body size"
```

---

### Task 4: Documentation and Examples

**Files:**
- Modify: `README.md`
- Modify: `config.example.json`
- Modify: `.env.example`

**Interfaces:**
- Consumes: `COPILOT_PROXY_RS_API_KEY`, `COPILOT_PROXY_RS_ALLOWED_ORIGINS`, `COPILOT_PROXY_RS_MAX_DECODED_BODY_BYTES`.
- Produces: User-facing setup guidance for optional auth and request limits.

- [ ] **Step 1: Update example config**

Modify `config.example.json`:

```json
{
  "backend": "copilot",
  "fallback_backend": "",
  "host": "127.0.0.1",
  "port": 8080,
  "allow_non_loopback_bind": false,
  "container_loopback_only": false,
  "api_key": "",
  "allowed_origins": [],
  "max_decoded_body_bytes": 16777216,
  "copilot_timeout": 300,
  "copilot_models_ttl": 300,
  "copilot_retry_max": 3,
  "copilot_retry_base_delay": 1.0,
  "copilot_max_rate": 15,
  "log_level": "INFO"
}
```

- [ ] **Step 2: Update environment example**

Ensure `.env.example` contains:

```bash
COPILOT_PROXY_RS_API_KEY=
COPILOT_PROXY_RS_ALLOWED_ORIGINS=
COPILOT_PROXY_RS_MAX_DECODED_BODY_BYTES=16777216
```

- [ ] **Step 3: Update README configuration table**

Add rows under `README.md` "Important variables":

```markdown
| `COPILOT_PROXY_RS_API_KEY` | Optional inbound API key. When set, Copilot-backed routes require `Authorization: Bearer <key>` or `x-api-key: <key>`. |
| `COPILOT_PROXY_RS_ALLOWED_ORIGINS` | Optional comma-separated WebSocket origin allowlist for `/v1/responses`. Empty means no origin filtering. |
| `COPILOT_PROXY_RS_MAX_DECODED_BODY_BYTES` | Maximum decoded JSON request body size after gzip/zstd decompression. Defaults to `16777216` bytes. |
```

- [ ] **Step 4: Update README safety model**

Replace the WebSocket alpha sentence with:

```markdown
WebSocket clients can be protected with `COPILOT_PROXY_RS_API_KEY` and
`COPILOT_PROXY_RS_ALLOWED_ORIGINS`. Keep the proxy local-only unless you have
configured inbound authentication, origin controls, and trusted network
boundaries.
```

- [ ] **Step 5: Run full verification**

Run:

```bash
cargo test --quiet && cargo clippy --all-targets --quiet -- -D warnings
```

Expected: PASS.

- [ ] **Step 6: Commit**

```bash
git add README.md config.example.json .env.example
git commit -m "docs: document inbound proxy hardening"
```

---

## Self-Review Notes

- Spec coverage: HTTP auth, WebSocket origin checks, and request-size hardening are each covered by a task with tests.
- Placeholder scan: no TODO/TBD placeholders remain.
- Type consistency: `AppConfig.api_key`, `AppConfig.allowed_origins`, and `AppConfig.max_decoded_body_bytes` are used consistently across config, middleware, routes, and tests.
