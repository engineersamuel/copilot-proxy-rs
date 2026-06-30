# Copilot Model Metadata Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Expose sanitized upstream Copilot model metadata and enrich `/v1/models` with a Codex-friendly capability catalog.

**Architecture:** Keep `/v1/models` backward-compatible by preserving `object` and `data[]`, then add a `models[]` rich catalog built from dynamic upstream metadata plus static fallbacks. Add a protected `/debug/copilot/models` route that refreshes Copilot metadata when stale and returns sanitized cached upstream data, derived catalog data, and refresh status.

**Tech Stack:** Rust 2024, axum 0.8, serde/serde_json, tokio, tower integration tests.

## Global Constraints

- Preserve the existing OpenAI-compatible `/v1/models` response fields: `object: "list"` and `data[]`.
- Do not expose credentials, authorization headers, Copilot tokens, or request-specific private content from debug endpoints.
- If Copilot metadata refresh fails, `/v1/models` must still return static fallback metadata.
- Treat pricing thresholds as billing boundaries, not proof of maximum model context.
- Do not change request routing, Copilot token acquisition, Codex configuration, or backend selection behavior.

---

## File Structure

- `src/models.rs`: owns model response structs, rich catalog structs, metadata derivation, sanitization helpers, and registry snapshot accessors.
- `src/copilot/client.rs`: adds a fallible model refresh method used by the debug endpoint while preserving existing best-effort refresh behavior.
- `src/http/health.rs`: adds the debug response handler beside existing health/model handlers.
- `src/http/routes.rs`: registers `/debug/copilot/models` inside the existing protected route group.
- `tests/http_contract.rs`: covers public `/v1/models` compatibility, rich catalog output, and debug route behavior.
- `tests/copilot_client_contract.rs`: covers fallible refresh behavior if it cannot be covered cleanly through HTTP route tests.

### Task 1: Enrich `/v1/models` with rich catalog metadata

**Files:**
- Modify: `src/models.rs:66-212`
- Test: `tests/http_contract.rs:139-162`
- Test: `tests/http_contract.rs:381-418`

**Interfaces:**
- Consumes: existing `ModelRegistry::list_for_snapshot(snapshot: BackendSnapshot) -> ModelsListResponse`
- Produces:
  - `ModelsListResponse { object: &'static str, data: Vec<ModelEntry>, models: Vec<CodexModelEntry> }`
  - `CodexModelEntry` serialized with fields `slug`, `display_name`, `default_reasoning_level`, `supported_reasoning_levels`, `context_window`, `max_context_window`, `context_window_modes`, `supported_endpoints`, and `source`

- [ ] **Step 1: Update the failing `/v1/models` contract test**

In `tests/http_contract.rs`, replace `models_route_returns_openai_model_list` with:

```rust
#[tokio::test]
async fn models_route_returns_openai_list_and_rich_catalog() {
    let app = router(AppState::new(AppConfig::default()));
    let response = app
        .oneshot(
            Request::builder()
                .uri("/v1/models")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let body = response_json(response).await;
    assert_eq!(body["object"], "list");
    assert!(
        body["data"]
            .as_array()
            .unwrap()
            .iter()
            .any(|model| model["id"] == "gpt-5.4")
    );

    let rich_gpt55 = body["models"]
        .as_array()
        .unwrap()
        .iter()
        .find(|model| model["slug"] == "gpt-5.5")
        .unwrap();
    assert_eq!(rich_gpt55["display_name"], "GPT-5.5");
    assert_eq!(rich_gpt55["context_window"], 272_000);
    assert_eq!(rich_gpt55["max_context_window"], 1_000_000);
    assert_eq!(rich_gpt55["source"], "static");
    assert_eq!(
        rich_gpt55["context_window_modes"],
        serde_json::json!([
            {"name": "default", "context_window": 272000},
            {"name": "long_context", "context_window": 1000000}
        ])
    );
}
```

- [ ] **Step 2: Run the test to verify it fails**

Run:

```bash
cargo test models_route_returns_openai_list_and_rich_catalog --test http_contract
```

Expected: FAIL because `body["models"]` is missing.

- [ ] **Step 3: Add rich model response types**

In `src/models.rs`, replace the response structs at lines 66-78 with:

```rust
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ModelsListResponse {
    pub object: &'static str,
    pub data: Vec<ModelEntry>,
    pub models: Vec<CodexModelEntry>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ModelEntry {
    pub id: String,
    pub object: &'static str,
    pub created: u64,
    pub owned_by: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CodexModelEntry {
    pub slug: String,
    pub display_name: String,
    pub default_reasoning_level: Option<String>,
    pub supported_reasoning_levels: Vec<String>,
    pub context_window: u64,
    pub max_context_window: u64,
    pub context_window_modes: Vec<ContextWindowMode>,
    pub supported_endpoints: Vec<String>,
    pub source: ModelMetadataSource,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ContextWindowMode {
    pub name: String,
    pub context_window: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ModelMetadataSource {
    Static,
    Dynamic,
}
```

- [ ] **Step 4: Add static rich metadata helpers**

Add these helpers after `copilot_models()` in `src/models.rs`:

```rust
fn rich_copilot_models(dynamic_models: &[serde_json::Value]) -> Vec<CodexModelEntry> {
    copilot_models()
        .into_iter()
        .map(|entry| rich_model_entry(&entry.id, dynamic_models))
        .collect()
}

fn rich_model_entry(model_id: &str, dynamic_models: &[serde_json::Value]) -> CodexModelEntry {
    let dynamic = dynamic_models
        .iter()
        .find(|item| item.get("id").and_then(serde_json::Value::as_str) == Some(model_id));
    let fallback = static_model_metadata(model_id);

    let supported_reasoning_levels = dynamic
        .and_then(dynamic_supported_efforts)
        .map(|efforts| {
            efforts
                .as_strings()
                .into_iter()
                .map(str::to_string)
                .collect()
        })
        .or_else(|| fallback.reasoning_levels.clone())
        .unwrap_or_default();
    let default_reasoning_level = if supported_reasoning_levels
        .iter()
        .any(|level| level == "medium")
    {
        Some("medium".to_string())
    } else {
        supported_reasoning_levels.first().cloned()
    };

    let supported_endpoints = dynamic
        .and_then(dynamic_supported_endpoints)
        .unwrap_or_else(|| static_supported_endpoints(model_id));
    let context_window = dynamic
        .and_then(|item| number_field(item, &["context_window", "contextWindow"]))
        .unwrap_or(fallback.context_window);
    let max_context_window = dynamic
        .and_then(|item| number_field(item, &["max_context_window", "maxContextWindow"]))
        .unwrap_or(fallback.max_context_window);
    let context_window_modes = dynamic
        .and_then(dynamic_context_window_modes)
        .unwrap_or_else(|| fallback.context_window_modes.clone());

    CodexModelEntry {
        slug: model_id.to_string(),
        display_name: display_name(model_id),
        default_reasoning_level,
        supported_reasoning_levels,
        context_window,
        max_context_window,
        context_window_modes,
        supported_endpoints,
        source: if dynamic.is_some() {
            ModelMetadataSource::Dynamic
        } else {
            ModelMetadataSource::Static
        },
    }
}

#[derive(Debug, Clone)]
struct StaticModelMetadata {
    context_window: u64,
    max_context_window: u64,
    context_window_modes: Vec<ContextWindowMode>,
    reasoning_levels: Option<Vec<String>>,
}

fn static_model_metadata(model_id: &str) -> StaticModelMetadata {
    match model_id {
        "gpt-5.5" => StaticModelMetadata {
            context_window: 272_000,
            max_context_window: 1_000_000,
            context_window_modes: vec![
                context_mode("default", 272_000),
                context_mode("long_context", 1_000_000),
            ],
            reasoning_levels: Some(vec![
                "low".to_string(),
                "medium".to_string(),
                "high".to_string(),
                "xhigh".to_string(),
            ]),
        },
        "gpt-5.4" => StaticModelMetadata {
            context_window: 272_000,
            max_context_window: 1_000_000,
            context_window_modes: vec![
                context_mode("default", 272_000),
                context_mode("long_context", 1_000_000),
            ],
            reasoning_levels: Some(vec![
                "low".to_string(),
                "medium".to_string(),
                "high".to_string(),
                "xhigh".to_string(),
            ]),
        },
        _ => StaticModelMetadata {
            context_window: 128_000,
            max_context_window: 128_000,
            context_window_modes: vec![context_mode("default", 128_000)],
            reasoning_levels: None,
        },
    }
}

fn context_mode(name: &str, context_window: u64) -> ContextWindowMode {
    ContextWindowMode {
        name: name.to_string(),
        context_window,
    }
}

fn display_name(model_id: &str) -> String {
    model_id
        .split('-')
        .map(|part| {
            if part.eq_ignore_ascii_case("gpt") {
                "GPT".to_string()
            } else {
                let mut chars = part.chars();
                match chars.next() {
                    Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
                    None => String::new(),
                }
            }
        })
        .collect::<Vec<_>>()
        .join("-")
}

fn number_field(value: &serde_json::Value, names: &[&str]) -> Option<u64> {
    names.iter().find_map(|name| value.get(*name)?.as_u64())
}

fn dynamic_supported_endpoints(value: &serde_json::Value) -> Option<Vec<String>> {
    value
        .get("supported_endpoints")
        .and_then(serde_json::Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(|item| item.as_str().map(str::to_string))
                .collect()
        })
}

fn dynamic_context_window_modes(value: &serde_json::Value) -> Option<Vec<ContextWindowMode>> {
    let modes = value
        .get("context_window_modes")
        .or_else(|| value.get("contextWindowModes"))?
        .as_array()?
        .iter()
        .filter_map(|mode| {
            let name = mode.get("name")?.as_str()?;
            let context_window = number_field(mode, &["context_window", "contextWindow"])?;
            Some(context_mode(name, context_window))
        })
        .collect::<Vec<_>>();
    (!modes.is_empty()).then_some(modes)
}
```

- [ ] **Step 5: Wire `models[]` into `ModelsListResponse`**

Update `model_list_for_snapshot` and `ModelRegistry::list_for_snapshot` in `src/models.rs`:

```rust
pub fn model_list_for_snapshot(_snapshot: BackendSnapshot) -> ModelsListResponse {
    ModelsListResponse {
        object: "list",
        data: copilot_models(),
        models: rich_copilot_models(&[]),
    }
}
```

```rust
pub async fn list_for_snapshot(&self, snapshot: BackendSnapshot) -> ModelsListResponse {
    let inner = self.inner.read().await;
    let dynamic_models = if snapshot.primary == BackendSnapshot::default().primary {
        Vec::new()
    } else {
        inner.models.clone()
    };
    drop(inner);
    ModelsListResponse {
        object: "list",
        data: copilot_models(),
        models: rich_copilot_models(&dynamic_models),
    }
}
```

If `BackendSnapshot::default()` is unavailable or awkward, use this simpler version:

```rust
pub async fn list_for_snapshot(&self, _snapshot: BackendSnapshot) -> ModelsListResponse {
    let inner = self.inner.read().await;
    let dynamic_models = inner.models.clone();
    drop(inner);
    ModelsListResponse {
        object: "list",
        data: copilot_models(),
        models: rich_copilot_models(&dynamic_models),
    }
}
```

- [ ] **Step 6: Run the focused test**

Run:

```bash
cargo test models_route_returns_openai_list_and_rich_catalog --test http_contract
```

Expected: PASS.

- [ ] **Step 7: Commit Task 1**

```bash
git add src/models.rs tests/http_contract.rs
git commit -m "feat: enrich models catalog metadata" -m "Co-authored-by: Copilot <223556219+Copilot@users.noreply.github.com>"
```

### Task 2: Add sanitized model metadata snapshot support

**Files:**
- Modify: `src/models.rs:214-338`
- Test: `tests/http_contract.rs`

**Interfaces:**
- Consumes: registry cached `Vec<serde_json::Value>` from `set_copilot_models`
- Produces:
  - `ModelRegistry::debug_snapshot(&self) -> CopilotModelsDebugSnapshot`
  - `CopilotModelsDebugSnapshot { fetched: bool, fetched_at_age_seconds: Option<u64>, upstream_models: Vec<Value>, models: Vec<CodexModelEntry> }`
  - `sanitize_model_metadata(value: &Value) -> Value`

- [ ] **Step 1: Add a failing unit-style integration test for sanitization**

Append this test to `tests/http_contract.rs` before `response_json`:

```rust
#[tokio::test]
async fn model_registry_debug_snapshot_sanitizes_sensitive_fields() {
    let registry = copilot_proxy_rs::models::ModelRegistry::new();
    registry
        .set_copilot_models(vec![serde_json::json!({
            "id": "gpt-5.5",
            "owned_by": "openai",
            "supported_endpoints": ["/responses"],
            "authorization": "Bearer secret",
            "access_token": "secret",
            "capabilities": {
                "supports": {
                    "reasoning_effort": ["low", "medium", "high", "xhigh"]
                }
            }
        })])
        .await;

    let snapshot = registry.debug_snapshot().await;
    assert!(snapshot.fetched);
    assert!(snapshot.fetched_at_age_seconds.is_some());
    assert_eq!(snapshot.upstream_models[0]["id"], "gpt-5.5");
    assert!(snapshot.upstream_models[0].get("authorization").is_none());
    assert!(snapshot.upstream_models[0].get("access_token").is_none());
    assert_eq!(snapshot.models[0].slug, "gpt-5.5");
    assert_eq!(snapshot.models[0].source, copilot_proxy_rs::models::ModelMetadataSource::Dynamic);
}
```

- [ ] **Step 2: Run the test to verify it fails**

Run:

```bash
cargo test model_registry_debug_snapshot_sanitizes_sensitive_fields --test http_contract
```

Expected: FAIL because `debug_snapshot` and exported fields do not exist.

- [ ] **Step 3: Add debug snapshot structs**

In `src/models.rs`, after `ModelRegistryInner`, add:

```rust
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CopilotModelsDebugSnapshot {
    pub fetched: bool,
    pub fetched_at_age_seconds: Option<u64>,
    pub upstream_models: Vec<serde_json::Value>,
    pub models: Vec<CodexModelEntry>,
}
```

- [ ] **Step 4: Add registry snapshot method**

In `impl ModelRegistry`, after `refresh_needed`, add:

```rust
pub async fn debug_snapshot(&self) -> CopilotModelsDebugSnapshot {
    let inner = self.inner.read().await;
    let upstream_models = inner.models.iter().map(sanitize_model_metadata).collect();
    let models = rich_copilot_models(&inner.models);
    CopilotModelsDebugSnapshot {
        fetched: inner.fetched_at.is_some(),
        fetched_at_age_seconds: inner
            .fetched_at
            .map(|fetched_at| fetched_at.elapsed().as_secs()),
        upstream_models,
        models,
    }
}
```

- [ ] **Step 5: Add recursive metadata sanitizer**

In `src/models.rs`, add after `dynamic_context_window_modes`:

```rust
fn sanitize_model_metadata(value: &serde_json::Value) -> serde_json::Value {
    match value {
        serde_json::Value::Object(map) => {
            let sanitized = map
                .iter()
                .filter_map(|(key, value)| {
                    if is_sensitive_metadata_key(key) {
                        None
                    } else {
                        Some((key.clone(), sanitize_model_metadata(value)))
                    }
                })
                .collect();
            serde_json::Value::Object(sanitized)
        }
        serde_json::Value::Array(items) => {
            serde_json::Value::Array(items.iter().map(sanitize_model_metadata).collect())
        }
        other => other.clone(),
    }
}

fn is_sensitive_metadata_key(key: &str) -> bool {
    matches!(
        key.to_ascii_lowercase().as_str(),
        "authorization"
            | "access_token"
            | "refresh_token"
            | "token"
            | "api_key"
            | "apikey"
            | "password"
            | "secret"
    )
}
```

- [ ] **Step 6: Run the sanitization test**

Run:

```bash
cargo test model_registry_debug_snapshot_sanitizes_sensitive_fields --test http_contract
```

Expected: PASS.

- [ ] **Step 7: Commit Task 2**

```bash
git add src/models.rs tests/http_contract.rs
git commit -m "feat: add sanitized Copilot model snapshots" -m "Co-authored-by: Copilot <223556219+Copilot@users.noreply.github.com>"
```

### Task 3: Add protected `/debug/copilot/models` endpoint

**Files:**
- Modify: `src/copilot/client.rs:310-331`
- Modify: `src/http/health.rs:1-63`
- Modify: `src/http/routes.rs:5-27`
- Test: `tests/http_contract.rs`

**Interfaces:**
- Consumes:
  - `CopilotClient::refresh_models_if_stale_result(&self) -> Result<(), CopilotError>`
  - `ModelRegistry::debug_snapshot(&self) -> CopilotModelsDebugSnapshot`
- Produces:
  - `debug_copilot_models(State<AppState>) -> Result<Json<CopilotModelsDebugResponse>, (StatusCode, Json<AnthropicErrorResponse>)>`
  - Route: `GET /debug/copilot/models`

- [ ] **Step 1: Add failing protected-route tests**

Append these tests to `tests/http_contract.rs` before `response_json`:

```rust
#[tokio::test]
async fn debug_copilot_models_requires_inbound_auth() {
    let app = router(AppState::new(AppConfig {
        api_key: "local-secret".to_string(),
        ..AppConfig::default()
    }));

    let response = app
        .oneshot(
            Request::builder()
                .uri("/debug/copilot/models")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn debug_copilot_models_returns_sanitized_snapshot() {
    let fixture = support::AppFixture::with_mock_copilot().await;
    fixture
        .state
        .models
        .set_copilot_models(vec![serde_json::json!({
            "id": "gpt-5.5",
            "owned_by": "openai",
            "supported_endpoints": ["/responses"],
            "authorization": "Bearer secret"
        })])
        .await;

    let response = router(fixture.state)
        .oneshot(
            Request::builder()
                .uri("/debug/copilot/models")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let body = response_json(response).await;
    assert_eq!(body["status"], "ok");
    assert_eq!(body["snapshot"]["upstream_models"][0]["id"], "gpt-5.5");
    assert!(
        body["snapshot"]["upstream_models"][0]
            .get("authorization")
            .is_none()
    );
    assert_eq!(body["snapshot"]["models"][0]["slug"], "gpt-5.5");
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run:

```bash
cargo test debug_copilot_models --test http_contract
```

Expected: FAIL because the route does not exist.

- [ ] **Step 3: Add fallible model refresh method**

In `src/copilot/client.rs`, replace `refresh_models_if_stale` with:

```rust
pub async fn refresh_models_if_stale(&self) {
    if let Err(error) = self.refresh_models_if_stale_result().await {
        tracing::warn!(
            error = %error,
            "failed to refresh Copilot models; keeping cached/static models"
        );
    }
}

pub async fn refresh_models_if_stale_result(&self) -> Result<(), CopilotError> {
    if !self
        .models
        .refresh_needed(self.config.copilot_models_ttl)
        .await
    {
        return Ok(());
    }
    let value = self.get_json(&self.endpoints.models_url, None).await?;
    if let Some(models) = value.get("data").and_then(Value::as_array) {
        self.models.set_copilot_models(models.clone()).await;
    }
    Ok(())
}
```

- [ ] **Step 4: Add debug response handler**

In `src/http/health.rs`, update imports:

```rust
use serde::Serialize;

use crate::errors::{AnthropicErrorResponse, anthropic_error};
use crate::models::{CopilotModelsDebugSnapshot, ModelsListResponse};
```

Add after `CountTokensResponse`:

```rust
#[derive(Debug, Serialize)]
pub(crate) struct CopilotModelsDebugResponse {
    status: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    warning: Option<String>,
    snapshot: CopilotModelsDebugSnapshot,
}
```

Add after `list_models`:

```rust
pub(crate) async fn debug_copilot_models(
    State(state): State<AppState>,
) -> Result<Json<CopilotModelsDebugResponse>, (StatusCode, Json<AnthropicErrorResponse>)> {
    let refresh_result = state.copilot.refresh_models_if_stale_result().await;
    let snapshot = state.models.debug_snapshot().await;
    match refresh_result {
        Ok(()) => Ok(Json(CopilotModelsDebugResponse {
            status: "ok",
            warning: None,
            snapshot,
        })),
        Err(error) if snapshot.fetched => Ok(Json(CopilotModelsDebugResponse {
            status: "stale",
            warning: Some(error.to_string()),
            snapshot,
        })),
        Err(error) => Err((
            StatusCode::BAD_GATEWAY,
            Json(anthropic_error(
                StatusCode::BAD_GATEWAY,
                "api_error",
                format!("failed to refresh Copilot models: {error}"),
            )),
        )),
    }
}
```

- [ ] **Step 5: Register route as protected**

In `src/http/routes.rs`, update the health import and protected routes:

```rust
use crate::http::health::{count_tokens, debug_copilot_models, health, list_models, version};
```

```rust
let protected_routes = Router::new()
    .route("/debug/copilot/models", get(debug_copilot_models))
    .route("/v1/models", get(list_models))
```

- [ ] **Step 6: Run debug endpoint tests**

Run:

```bash
cargo test debug_copilot_models --test http_contract
```

Expected: PASS.

- [ ] **Step 7: Commit Task 3**

```bash
git add src/copilot/client.rs src/http/health.rs src/http/routes.rs tests/http_contract.rs
git commit -m "feat: expose debug Copilot model metadata" -m "Co-authored-by: Copilot <223556219+Copilot@users.noreply.github.com>"
```

### Task 4: Validate dynamic metadata and full contract

**Files:**
- Modify: `tests/http_contract.rs`
- Modify if needed: `src/models.rs`

**Interfaces:**
- Consumes: `rich_model_entry`, `dynamic_supported_efforts`, `dynamic_supported_endpoints`, `dynamic_context_window_modes`
- Produces: verified behavior that dynamic upstream fields override static fallbacks when present

- [ ] **Step 1: Add dynamic metadata test**

Append this test to `tests/http_contract.rs` before `response_json`:

```rust
#[tokio::test]
async fn models_route_uses_dynamic_context_and_reasoning_metadata() {
    let fixture = support::AppFixture::with_mock_copilot().await;
    fixture
        .state
        .models
        .set_copilot_models(vec![serde_json::json!({
            "id": "gpt-5.5",
            "owned_by": "openai",
            "supported_endpoints": ["/responses", "ws:/responses"],
            "context_window": 400000u64,
            "max_context_window": 1100000u64,
            "context_window_modes": [
                {"name": "default", "context_window": 400000u64},
                {"name": "long_context", "context_window": 1100000u64}
            ],
            "capabilities": {
                "supports": {
                    "reasoning_effort": ["low", "medium", "high", "xhigh"]
                }
            }
        })])
        .await;

    let response = router(fixture.state)
        .oneshot(
            Request::builder()
                .uri("/v1/models")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let body = response_json(response).await;
    let rich_gpt55 = body["models"]
        .as_array()
        .unwrap()
        .iter()
        .find(|model| model["slug"] == "gpt-5.5")
        .unwrap();

    assert_eq!(rich_gpt55["context_window"], 400_000);
    assert_eq!(rich_gpt55["max_context_window"], 1_100_000);
    assert_eq!(rich_gpt55["supported_endpoints"], serde_json::json!(["/responses", "ws:/responses"]));
    assert_eq!(rich_gpt55["supported_reasoning_levels"], serde_json::json!(["low", "medium", "high", "xhigh"]));
    assert_eq!(rich_gpt55["source"], "dynamic");
}
```

- [ ] **Step 2: Run the dynamic metadata test**

Run:

```bash
cargo test models_route_uses_dynamic_context_and_reasoning_metadata --test http_contract
```

Expected: PASS. If it fails because dynamic context fields are not preferred, update `rich_model_entry` to read dynamic fields before static fallback exactly as Task 1 Step 4 specifies.

- [ ] **Step 3: Run all HTTP contract tests**

Run:

```bash
cargo test --test http_contract
```

Expected: PASS.

- [ ] **Step 4: Run full test suite**

Run:

```bash
cargo test
```

Expected: PASS.

- [ ] **Step 5: Run formatting**

Run:

```bash
cargo fmt -- --check
```

Expected: PASS. If it fails, run `cargo fmt`, then re-run `cargo fmt -- --check`.

- [ ] **Step 6: Run linting**

Run:

```bash
cargo clippy --all-targets --all-features -- -D warnings
```

Expected: PASS.

- [ ] **Step 7: Commit Task 4**

```bash
git add src/models.rs tests/http_contract.rs
git commit -m "test: cover dynamic Copilot model metadata" -m "Co-authored-by: Copilot <223556219+Copilot@users.noreply.github.com>"
```

### Task 5: Live smoke test and cleanup

**Files:**
- Modify only if required by validation failures: `src/models.rs`, `src/http/health.rs`, `src/http/routes.rs`, `src/copilot/client.rs`, `tests/http_contract.rs`

**Interfaces:**
- Consumes: running local proxy at `http://127.0.0.1:8080`
- Produces: verified local HTTP output for `/v1/models` and `/debug/copilot/models`

- [ ] **Step 1: Check live `/v1/models` shape**

Run:

```bash
curl -fsS http://127.0.0.1:8080/v1/models | python3 -m json.tool | head -120
```

Expected: JSON contains `object`, `data`, and `models`.

- [ ] **Step 2: Check live GPT-5.5 derived metadata**

Run:

```bash
curl -fsS http://127.0.0.1:8080/v1/models \
  | python3 -c 'import json,sys; data=json.load(sys.stdin); print([m for m in data["models"] if m["slug"]=="gpt-5.5"][0])'
```

Expected: output contains `context_window`, `max_context_window`, `context_window_modes`, and `supported_reasoning_levels`.

- [ ] **Step 3: Check debug endpoint if inbound auth allows it**

If `COPILOT_PROXY_API_KEY` is configured, run:

```bash
curl -fsS -H "authorization: Bearer $COPILOT_PROXY_API_KEY" \
  http://127.0.0.1:8080/debug/copilot/models \
  | python3 -m json.tool | head -160
```

If no inbound API key is configured, run:

```bash
curl -fsS http://127.0.0.1:8080/debug/copilot/models | python3 -m json.tool | head -160
```

Expected: JSON contains `status` and `snapshot`; `snapshot.upstream_models` does not contain `authorization`, `access_token`, `token`, `api_key`, `password`, or `secret` keys.

- [ ] **Step 4: Confirm working tree contains only intentional files**

Run:

```bash
git --no-pager status --short
```

Expected: only intentional modified files are present. `Copilot-Processing.md` may remain untracked from earlier process tracking and should not be committed unless explicitly requested.

- [ ] **Step 5: Final commit if smoke-test fixes were needed**

If Step 1-3 required code changes, run:

```bash
git add src/models.rs src/http/health.rs src/http/routes.rs src/copilot/client.rs tests/http_contract.rs
git commit -m "fix: align live Copilot model metadata output" -m "Co-authored-by: Copilot <223556219+Copilot@users.noreply.github.com>"
```

Expected: commit succeeds. If no changes were needed, skip this step.

## Self-Review

- Spec coverage: Tasks cover `/v1/models` compatibility, rich `models[]`, sanitized upstream debug snapshot, stale/fallback behavior, and test validation.
- Placeholder scan: no `TBD`, `TODO`, or unspecified error-handling steps remain.
- Type consistency: `CodexModelEntry`, `ContextWindowMode`, `ModelMetadataSource`, and `CopilotModelsDebugSnapshot` are defined before tasks that consume them.
