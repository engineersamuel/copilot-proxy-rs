# Configured Local Model Routing Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add `qwen3-coder-30b-local` to `/v1/models` and route chat completions plus Responses HTTP/SSE traffic to the configured Tailscale llama.cpp endpoint while preserving all Copilot behavior.

**Architecture:** Extend configuration and `ModelRegistry` with immutable local model definitions and resolve every requested ID to an explicit `ModelTarget`. A focused `LocalBackend` handles OpenAI-compatible chat transport, while a pure Responses/chat adapter handles request, response, tool, and SSE translation. Route handlers branch on `ModelTarget`; local failures never enter Copilot refresh, authentication, retries, or fallback.

**Tech Stack:** Rust 2024, Axum, Tokio, reqwest, serde/serde_json, futures, async-stream, thiserror, tower integration tests, cargo fmt/clippy/test.

---

## File map

- Modify `src/config.rs`: parse and validate `local_models`.
- Modify `src/models.rs`: merge configured models into both catalog views and resolve `ModelTarget`.
- Create `src/local/mod.rs`: local backend module boundary and exports.
- Create `src/local/client.rs`: outbound OpenAI-compatible chat transport and typed errors.
- Create `src/local/responses.rs`: pure Responses/chat request, response, tool, and stream translation.
- Modify `src/lib.rs`: register the local module.
- Modify `src/state.rs`: construct and expose `LocalBackend`.
- Modify `src/responses/request.rs`: extract provider-neutral previous-response expansion.
- Modify `src/http/chat.rs`: dispatch configured local chat requests.
- Modify `src/http/responses.rs`: dispatch configured local Responses requests and reject unsupported local resource operations.
- Modify `src/http/sse.rs`: support a stateful one-input-to-many-events SSE mapper.
- Modify `src/http/errors.rs`: map local transport and translation errors to OpenAI errors.
- Modify `tests/support/mod.rs`: build deterministic local-model fixtures.
- Create `tests/local_client_contract.rs`: direct local transport and error contracts.
- Modify `tests/config_contract.rs`: configuration defaults, loading, and validation.
- Modify `tests/http_contract.rs`: catalog merging and model-target resolution.
- Modify `tests/chat_completions_contract.rs`: local chat JSON/SSE routing and failures.
- Modify `tests/responses_contract.rs`: local Responses JSON/SSE routing, state, errors, and Copilot isolation.
- Modify `tests/responses_ws_contract.rs`: explicit local-model WebSocket rejection.
- Modify `config.example.json` and `README.md`: operator configuration and scope.

### Task 1: Parse and validate configured local models

**Files:**
- Modify: `src/config.rs:10-27,31-104,136-168,181-270,294-403`
- Test: `tests/config_contract.rs`

- [ ] **Step 1: Write failing configuration tests**

Add these tests to `tests/config_contract.rs`:

```rust
#[test]
fn local_models_default_to_empty() {
    assert!(AppConfig::default().local_models.is_empty());
}

#[test]
fn local_models_load_from_json() {
    let temp = repo_tempdir("config-local-model-");
    fs::write(
        temp.path().join("config.json"),
        r#"{
          "local_models": {
            "qwen3-coder-30b-local": {
              "base_url": "http://100.98.223.125:8080/v1",
              "upstream_model": "models\\Qwen3-Coder-30B-A3B-Instruct-IQ4_XS.gguf"
            }
          }
        }"#,
    )
    .unwrap();
    let env =
        EnvSource::from_pairs([("COPILOT_PROXY_RS_CONFIG_DIR", temp.path().to_str().unwrap())]);

    let config = AppConfig::load_from_env(&env).unwrap();
    let local = config.local_models.get("qwen3-coder-30b-local").unwrap();

    assert_eq!(local.base_url, "http://100.98.223.125:8080/v1");
    assert_eq!(
        local.upstream_model,
        r"models\Qwen3-Coder-30B-A3B-Instruct-IQ4_XS.gguf"
    );
}

#[test]
fn invalid_local_model_names_the_id_and_field() {
    let temp = repo_tempdir("config-invalid-local-model-");
    fs::write(
        temp.path().join("config.json"),
        r#"{"local_models":{"qwen-local":{"base_url":"ftp://host/v1","upstream_model":"model.gguf"}}}"#,
    )
    .unwrap();
    let env =
        EnvSource::from_pairs([("COPILOT_PROXY_RS_CONFIG_DIR", temp.path().to_str().unwrap())]);

    let error = AppConfig::load_from_env(&env).unwrap_err().to_string();

    assert!(error.contains("qwen-local"));
    assert!(error.contains("base_url"));
    assert!(error.contains("http or https"));
}
```

- [ ] **Step 2: Run the tests and verify RED**

Run:

```bash
rtk cargo test --test config_contract local_models -- --nocapture
rtk cargo test --test config_contract invalid_local_model_names_the_id_and_field -- --nocapture
```

Expected: compilation fails because `AppConfig::local_models` and `LocalModelConfig` do not exist.

- [ ] **Step 3: Add the configuration domain type and validation error**

Add beside `ModelOverrides` in `src/config.rs`:

```rust
#[derive(Debug, Clone, Default, Deserialize, Serialize, PartialEq, Eq)]
pub struct LocalModelConfig {
    pub base_url: String,
    pub upstream_model: String,
}
```

Add this `ConfigError` variant:

```rust
#[error("invalid local model {model_id:?} field {field}: {message}")]
InvalidLocalModel {
    model_id: String,
    field: &'static str,
    message: String,
},
```

Add `pub local_models: BTreeMap<String, LocalModelConfig>` to `AppConfig`, add the same field to `FileConfig`, initialize it with `BTreeMap::new()` in `Default`, and assign a non-empty file map in `merge_file_values`.

- [ ] **Step 4: Validate every configured definition after all merges**

Add this method and call `config.validate_local_models()?` after `apply_env_overrides` in `load_from_env`:

```rust
fn validate_local_models(&self) -> Result<(), ConfigError> {
    for (model_id, model) in &self.local_models {
        if model_id.is_empty() || model_id.trim() != model_id {
            return Err(ConfigError::InvalidLocalModel {
                model_id: model_id.clone(),
                field: "id",
                message: "must be non-empty and contain no surrounding whitespace".to_string(),
            });
        }
        if model.upstream_model.trim().is_empty() {
            return Err(ConfigError::InvalidLocalModel {
                model_id: model_id.clone(),
                field: "upstream_model",
                message: "must be non-empty".to_string(),
            });
        }
        let url = reqwest::Url::parse(&model.base_url).map_err(|error| {
            ConfigError::InvalidLocalModel {
                model_id: model_id.clone(),
                field: "base_url",
                message: error.to_string(),
            }
        })?;
        if !matches!(url.scheme(), "http" | "https") {
            return Err(ConfigError::InvalidLocalModel {
                model_id: model_id.clone(),
                field: "base_url",
                message: "scheme must be http or https".to_string(),
            });
        }
        if url.host_str().is_none() || url.query().is_some() || url.fragment().is_some() {
            return Err(ConfigError::InvalidLocalModel {
                model_id: model_id.clone(),
                field: "base_url",
                message: "must contain a host and no query or fragment".to_string(),
            });
        }
    }
    Ok(())
}
```

- [ ] **Step 5: Run focused and full configuration tests**

Run:

```bash
rtk cargo test --test config_contract local_models -- --nocapture
rtk cargo test --test config_contract invalid_local_model_names_the_id_and_field -- --nocapture
rtk cargo test --test config_contract
```

Expected: all `config_contract` tests pass.

- [ ] **Step 6: Commit the configuration slice**

```bash
rtk git add src/config.rs tests/config_contract.rs
rtk git commit -m "feat(config): define validated local models"
```

### Task 2: Merge local models into the catalog and resolve explicit targets

**Files:**
- Modify: `src/models.rs:82-152,251-398,506-576`
- Modify: `src/state.rs:21-49`
- Test: `tests/http_contract.rs`

- [ ] **Step 1: Write failing catalog and target-resolution tests**

Add imports for `LocalModelConfig` and `ModelTarget`, then add:

```rust
fn qwen_local_models() -> BTreeMap<String, LocalModelConfig> {
    BTreeMap::from([(
        "qwen3-coder-30b-local".to_string(),
        LocalModelConfig {
            base_url: "http://100.98.223.125:8080/v1".to_string(),
            upstream_model: r"models\Qwen3-Coder-30B-A3B-Instruct-IQ4_XS.gguf".to_string(),
        },
    )])
}

#[tokio::test]
async fn configured_local_model_is_listed_and_resolved() {
    let registry = ModelRegistry::with_models(BTreeMap::new(), qwen_local_models());
    let response = registry
        .list_for_snapshot(BackendSnapshot {
            primary: BackendKind::Copilot,
            fallback: None,
        })
        .await;

    assert!(response.data.iter().any(|model| model.id == "qwen3-coder-30b-local"));
    let rich = response
        .models
        .iter()
        .find(|model| model.slug == "qwen3-coder-30b-local")
        .unwrap();
    assert_eq!(rich.source, ModelMetadataSource::Local);
    assert_eq!(rich.supported_endpoints, vec!["/chat/completions", "/responses"]);

    let target = registry.resolve_target("qwen3-coder-30b-local").await;
    assert!(matches!(
        target,
        ModelTarget::Local(ref local)
            if local.public_id == "qwen3-coder-30b-local"
                && local.upstream_model == r"models\Qwen3-Coder-30B-A3B-Instruct-IQ4_XS.gguf"
    ));
}
```

- [ ] **Step 2: Run the test and verify RED**

Run:

```bash
rtk cargo test --test http_contract configured_local_model_is_listed_and_resolved -- --nocapture
```

Expected: compilation fails because `with_models`, `ModelTarget`, and `ModelMetadataSource::Local` do not exist.

- [ ] **Step 3: Add target types and immutable local definitions**

Add to `src/models.rs`:

```rust
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LocalModelTarget {
    pub public_id: String,
    pub base_url: String,
    pub upstream_model: String,
}

impl LocalModelTarget {
    pub fn chat_completions_url(&self) -> String {
        format!("{}/chat/completions", self.base_url.trim_end_matches('/'))
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ModelTarget {
    Copilot { model_id: String },
    Local(LocalModelTarget),
}
```

Add `Local` to `ModelMetadataSource` and derive `PartialEq, Eq` so catalog-source assertions remain direct. Add a `local_models: BTreeMap<String, LocalModelConfig>` field outside the registry lock. Replace the override-only constructor with:

```rust
pub fn with_models(
    copilot_overrides: BTreeMap<String, String>,
    local_models: BTreeMap<String, LocalModelConfig>,
) -> Self {
    Self {
        local_models,
        inner: RwLock::new(ModelRegistryInner {
            copilot_overrides,
            ..Default::default()
        }),
    }
}

pub fn configured_local_target(&self, model: &str) -> Option<LocalModelTarget> {
    let public_id = strip_model_prefix(model);
    let config = self.local_models.get(public_id)?;
    Some(LocalModelTarget {
        public_id: public_id.to_string(),
        base_url: config.base_url.clone(),
        upstream_model: config.upstream_model.clone(),
    })
}

pub async fn resolve_target(&self, model: &str) -> ModelTarget {
    if let Some(local) = self.configured_local_target(model) {
        return ModelTarget::Local(local);
    }
    ModelTarget::Copilot {
        model_id: self.get_copilot_openai_model(model).await,
    }
}
```

Keep `ModelRegistry::new()` and `with_copilot_overrides()` as compatibility wrappers that pass an empty local map.

- [ ] **Step 4: Merge local entries into both catalog views**

Add these builders:

```rust
fn local_model_entry(model_id: &str) -> ModelEntry {
    ModelEntry {
        id: model_id.to_string(),
        object: "model",
        created: 1_700_000_000,
        owned_by: "local".to_string(),
    }
}

fn local_rich_model_entry(model_id: &str) -> CodexModelEntry {
    CodexModelEntry {
        slug: model_id.to_string(),
        display_name: display_name(model_id),
        description: format!("Local OpenAI-compatible model {model_id}"),
        default_reasoning_level: None,
        supported_reasoning_levels: Vec::new(),
        shell_type: "shell_command".to_string(),
        visibility: "list".to_string(),
        supported_in_api: true,
        priority: 100,
        additional_speed_tiers: Vec::new(),
        service_tiers: Vec::new(),
        availability_nux: None,
        upgrade: None,
        base_instructions: String::new(),
        model_messages: serde_json::json!({}),
        supports_reasoning_summaries: false,
        default_reasoning_summary: "none".to_string(),
        support_verbosity: false,
        default_verbosity: "low".to_string(),
        apply_patch_tool_type: "freeform".to_string(),
        web_search_tool_type: "unsupported".to_string(),
        truncation_policy: serde_json::json!({"mode": "tokens", "limit": 10000}),
        supports_parallel_tool_calls: true,
        supports_image_detail_original: false,
        context_window: None,
        max_context_window: None,
        comp_hash: "local".to_string(),
        effective_context_window_percent: 95,
        experimental_supported_tools: Vec::new(),
        input_modalities: vec!["text".to_string()],
        supports_search_tool: false,
        use_responses_lite: false,
        context_window_modes: Vec::new(),
        supported_endpoints: vec!["/chat/completions".to_string(), "/responses".to_string()],
        source: ModelMetadataSource::Local,
    }
}
```

In `list_for_snapshot`, build `BTreeMap`s from the current Copilot views, insert every local entry last, and collect the maps into `data` and `models`. This makes local IDs deterministically win collisions.

- [ ] **Step 5: Wire definitions from `AppState` and run regression tests**

Change `AppState::new` to call:

```rust
let models = Arc::new(ModelRegistry::with_models(
    config.model_overrides.copilot.clone(),
    config.local_models.clone(),
));
```

Run:

```bash
rtk cargo test --test http_contract configured_local_model_is_listed_and_resolved -- --nocapture
rtk cargo test --test http_contract
```

Expected: the focused test and all existing catalog tests pass.

- [ ] **Step 6: Commit the model-catalog slice**

```bash
rtk git add src/models.rs src/state.rs tests/http_contract.rs
rtk git commit -m "feat(models): resolve configured local targets"
```

### Task 3: Add typed local HTTP transport

**Files:**
- Create: `src/local/mod.rs`
- Create: `src/local/client.rs`
- Modify: `src/lib.rs:1-15`
- Modify: `src/state.rs:12-63`
- Modify: `src/http/errors.rs:65-90`
- Create: `tests/local_client_contract.rs`
- Test support: `tests/support/mod.rs`

- [ ] **Step 1: Add a failing direct transport test**

Create `tests/local_client_contract.rs`, import `mod support;`, and add:

```rust
#[tokio::test]
async fn local_backend_posts_exact_upstream_model() {
    let mock = support::MockServer::start().await;
    mock.respond_json(
        "POST",
        "/v1/chat/completions",
        200,
        serde_json::json!({
            "id": "chatcmpl-local",
            "object": "chat.completion",
            "model": "models\\Qwen3-Coder-30B-A3B-Instruct-IQ4_XS.gguf",
            "choices": [{"index":0,"message":{"role":"assistant","content":"hello"},"finish_reason":"stop"}],
            "usage": {"prompt_tokens":1,"completion_tokens":1,"total_tokens":2}
        }),
    ).await;
    let target = LocalModelTarget {
        public_id: "qwen3-coder-30b-local".to_string(),
        base_url: format!("{}/v1", mock.base_url),
        upstream_model: r"models\Qwen3-Coder-30B-A3B-Instruct-IQ4_XS.gguf".to_string(),
    };
    let body = serde_json::json!({"messages":[{"role":"user","content":"hello"}]})
        .as_object().unwrap().clone();

    let response = LocalBackend::new().post_chat(&target, body).await.unwrap();

    assert_eq!(response["choices"][0]["message"]["content"], "hello");
    let outbound = mock.last_request_body_json("POST", "/v1/chat/completions").await.unwrap();
    assert_eq!(outbound["model"], r"models\Qwen3-Coder-30B-A3B-Instruct-IQ4_XS.gguf");
}
```

- [ ] **Step 2: Run the route test and verify RED**

Run:

```bash
rtk cargo test --test local_client_contract local_backend_posts_exact_upstream_model -- --nocapture
```

Expected: compilation fails because `LocalBackend` does not exist.

- [ ] **Step 3: Create the local backend and typed errors**

Create `src/local/mod.rs`:

```rust
pub mod client;

pub use client::{LocalBackend, LocalModelError};
```

Create `src/local/client.rs` with this public API and error contract:

```rust
use reqwest::{Client, Response};
use serde_json::{Map, Value};

use crate::models::LocalModelTarget;

const MAX_ERROR_CHARS: usize = 4096;

#[derive(Debug, thiserror::Error)]
pub enum LocalModelError {
    #[error("local model request timed out")]
    Timeout,
    #[error("local model connection failed")]
    Transport,
    #[error("local model returned HTTP {status}: {detail}")]
    Http { status: u16, detail: String },
    #[error("local model returned invalid JSON")]
    InvalidJson,
}

#[derive(Debug, Clone)]
pub struct LocalBackend {
    client: Client,
}

impl Default for LocalBackend {
    fn default() -> Self {
        Self::new()
    }
}

impl LocalBackend {
    pub fn new() -> Self {
        Self {
            client: Client::new(),
        }
    }

    pub async fn post_chat(
        &self,
        target: &LocalModelTarget,
        body: Map<String, Value>,
    ) -> Result<Value, LocalModelError> {
        let response = self.send(target, body).await?;
        response.json().await.map_err(|_| LocalModelError::InvalidJson)
    }

    pub async fn stream_chat(
        &self,
        target: &LocalModelTarget,
        body: Map<String, Value>,
    ) -> Result<Response, LocalModelError> {
        self.send(target, body).await
    }

    async fn send(
        &self,
        target: &LocalModelTarget,
        mut body: Map<String, Value>,
    ) -> Result<Response, LocalModelError> {
        body.insert("model".to_string(), Value::String(target.upstream_model.clone()));
        let response = self.client
            .post(target.chat_completions_url())
            .json(&body)
            .timeout(std::time::Duration::from_secs(300))
            .send()
            .await
            .map_err(map_transport_error)?;
        checked_response(response).await
    }
}

fn map_transport_error(error: reqwest::Error) -> LocalModelError {
    if error.is_timeout() {
        LocalModelError::Timeout
    } else {
        LocalModelError::Transport
    }
}

async fn checked_response(response: Response) -> Result<Response, LocalModelError> {
    if response.status().is_success() {
        return Ok(response);
    }
    let status = response.status();
    let body = response.text().await.unwrap_or_default();
    let detail: String = body.chars().take(MAX_ERROR_CHARS).collect();
    Err(LocalModelError::Http {
        status: status.as_u16(),
        detail: if detail.is_empty() { status.canonical_reason().unwrap_or("upstream error").to_string() } else { detail },
    })
}
```

Register `pub mod local;` in `src/lib.rs`. Add `pub local: Arc<LocalBackend>` to `AppState` and initialize it with `Arc::new(LocalBackend::new())` in both constructors.

- [ ] **Step 4: Add OpenAI error mapping for local failures**

Add to `src/http/errors.rs`:

```rust
pub(crate) fn openai_local_error(
    error: crate::local::LocalModelError,
) -> (StatusCode, Json<crate::errors::OpenAiErrorResponse>) {
    match error {
        crate::local::LocalModelError::Timeout => openai_error(
            StatusCode::GATEWAY_TIMEOUT,
            "server_error",
            "local model request timed out",
        ),
        crate::local::LocalModelError::Transport => openai_error(
            StatusCode::BAD_GATEWAY,
            "server_error",
            "local model connection failed",
        ),
        crate::local::LocalModelError::Http { status, detail } => openai_error(
            StatusCode::from_u16(status).unwrap_or(StatusCode::BAD_GATEWAY),
            "server_error",
            detail,
        ),
        crate::local::LocalModelError::InvalidJson => openai_error(
            StatusCode::BAD_GATEWAY,
            "server_error",
            "local model returned invalid JSON",
        ),
    }
}
```

- [ ] **Step 5: Run a compile check**

Run:

```bash
rtk cargo test --test local_client_contract local_backend_posts_exact_upstream_model -- --nocapture
rtk cargo check --all-targets
```

Expected: the direct transport test passes and all targets compile.

- [ ] **Step 6: Commit the transport boundary**

```bash
rtk git add src/lib.rs src/local/mod.rs src/local/client.rs src/state.rs src/http/errors.rs tests/support/mod.rs tests/local_client_contract.rs
rtk git commit -m "feat(local): add OpenAI-compatible transport"
```

### Task 4: Route chat completions to local targets

**Files:**
- Modify: `src/http/chat.rs:42-175`
- Modify: `src/http/sse.rs:4-38`
- Test: `tests/chat_completions_contract.rs`
- Test support: `tests/support/mod.rs`

- [ ] **Step 1: Add a failing local fixture and route test**

Add `AppFixture::with_mock_local()` in `tests/support/mod.rs`. It starts `MockServer`, inserts `qwen3-coder-30b-local` with `base_url: format!("{}/v1", mock.base_url)`, and constructs `AppState::new(config)`. Add the route-level version of `chat_completions_routes_configured_local_model_without_copilot_refresh` from Task 3's transport contract: send the public model through `router`, assert the response exposes the public ID, assert the mock receives the exact upstream ID, and assert `/models` and `/copilot/token` each have zero hits.

Run:

```bash
rtk cargo test --test chat_completions_contract chat_completions_routes_configured_local_model_without_copilot_refresh -- --nocapture
```

Expected: RED because `chat_completions_inner` still enters Copilot routing.

- [ ] **Step 2: Add a local chat handler and make the RED test pass**

Immediately after extracting `requested_model`, branch before Copilot refresh:

```rust
if let Some(local_target) = state.models.configured_local_target(&requested_model) {
    return handle_local_chat(state, body, requested_model, local_target, stream).await;
}
state.copilot.refresh_models_if_stale().await;
```

Move the current unconditional refresh below this branch. Add a focused `handle_local_chat` that:

1. Replaces `model` with `target.upstream_model` through `LocalBackend`.
2. Calls `post_chat` for buffered JSON and restores `response["model"]` to `requested_model`.
3. Calls `stream_chat` for SSE and maps every JSON `data:` line so its `model` field is the public ID.
4. Maps errors with `openai_local_error`.

Use this complete line mapper:

```rust
fn local_chat_sse_line(line: &str, public_model: &str) -> Option<String> {
    let payload = line.strip_prefix("data: ")?;
    if payload == "[DONE]" {
        return Some("data: [DONE]".to_string());
    }
    let mut value: Value = serde_json::from_str(payload).ok()?;
    if let Some(object) = value.as_object_mut() {
        object.insert("model".to_string(), Value::String(public_model.to_string()));
    }
    Some(format!("data: {value}"))
}
```

- [ ] **Step 3: Run the focused non-streaming test and verify GREEN**

```bash
rtk cargo test --test chat_completions_contract chat_completions_routes_configured_local_model_without_copilot_refresh -- --nocapture
```

Expected: PASS; the mock receives the exact Windows-style upstream ID and no Copilot refresh/token request.

- [ ] **Step 4: Write a failing local streaming test**

Configure the mock with chat SSE chunks containing the upstream model ID, request `stream: true`, collect the body, and assert all of:

```rust
assert_eq!(response.status(), StatusCode::OK);
assert_eq!(response.headers()["content-type"], "text/event-stream");
assert!(text.contains("qwen3-coder-30b-local"));
assert!(!text.contains("Qwen3-Coder-30B-A3B-Instruct-IQ4_XS.gguf"));
assert!(text.contains("data: [DONE]"));
```

- [ ] **Step 5: Run the streaming test and verify RED, then implement the mapper path**

Run the exact new test. Expected RED: raw upstream model ID remains visible. Capture the public ID in the existing `map_sse_lines` closure and call `local_chat_sse_line`.

- [ ] **Step 6: Add and satisfy no-fallback and error tests**

Add one test with `base_url: "http://127.0.0.1:1/v1"`; assert `502`, `error.type == "server_error"`, and no `/copilot/token` or `/models` hit. Add one mock HTTP `429` case and assert status `429` plus the bounded upstream detail.

Run:

```bash
rtk cargo test --test chat_completions_contract local -- --nocapture
rtk cargo test --test chat_completions_contract
```

Expected: all local chat tests and the full chat contract pass.

- [ ] **Step 7: Commit the chat vertical slice**

```bash
rtk git add src/http/chat.rs src/http/sse.rs tests/chat_completions_contract.rs
rtk git commit -m "feat(chat): route configured local models"
```

### Task 5: Translate Responses requests to chat completions

**Files:**
- Create: `src/local/responses.rs`
- Modify: `src/local/mod.rs`
- Modify: `src/responses/request.rs:20-110`
- Test: unit tests in `src/local/responses.rs`
- Test: existing unit tests in `src/responses/request.rs`

- [ ] **Step 1: Write failing translator tests**

Add unit tests for this exact public contract:

```rust
let translated = responses_to_chat(
    serde_json::json!({
        "model": "qwen3-coder-30b-local",
        "instructions": "Be concise",
        "input": [
            {"type":"message","role":"user","content":[{"type":"input_text","text":"weather"}]},
            {"type":"function_call","call_id":"call_1","name":"get_weather","arguments":"{\"city\":\"NYC\"}"},
            {"type":"function_call_output","call_id":"call_1","output":"sunny"}
        ],
        "tools": [{"type":"function","name":"get_weather","description":"Weather","parameters":{"type":"object"}}],
        "tool_choice": {"type":"function","name":"get_weather"},
        "max_output_tokens": 64,
        "stream": false
    }).as_object().unwrap().clone(),
    r"models\Qwen3-Coder-30B-A3B-Instruct-IQ4_XS.gguf",
).unwrap();

assert_eq!(translated.chat_body["model"], r"models\Qwen3-Coder-30B-A3B-Instruct-IQ4_XS.gguf");
assert_eq!(translated.chat_body["messages"][0], serde_json::json!({"role":"system","content":"Be concise"}));
assert_eq!(translated.chat_body["messages"][1]["role"], "user");
assert_eq!(translated.chat_body["messages"][2]["tool_calls"][0]["id"], "call_1");
assert_eq!(translated.chat_body["messages"][3], serde_json::json!({"role":"tool","tool_call_id":"call_1","content":"sunny"}));
assert_eq!(translated.chat_body["tools"][0]["function"]["name"], "get_weather");
assert_eq!(translated.chat_body["tool_choice"]["function"]["name"], "get_weather");
assert_eq!(translated.chat_body["max_tokens"], 64);
```

Add a second test proving a Responses custom/freeform tool becomes a chat function with one required string property named `input`, and that `custom_tool_call_output` becomes a `role: "tool"` message.

- [ ] **Step 2: Run translator tests and verify RED**

```bash
rtk cargo test local::responses::tests::responses_request -- --nocapture
```

Expected: compilation fails because `responses_to_chat` and its result type do not exist.

- [ ] **Step 3: Define translation types and the top-level conversion**

Add `pub mod responses;` and exports for the translation types to `src/local/mod.rs`.

Add:

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LocalToolKind {
    Function,
    Custom,
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum ResponsesTranslationError {
    #[error("unsupported Responses input: {0}")]
    UnsupportedInput(String),
    #[error("unsupported Responses tool: {0}")]
    UnsupportedTool(String),
    #[error("invalid Responses request: {0}")]
    InvalidRequest(String),
    #[error("invalid chat completion response: {0}")]
    InvalidResponse(String),
}

#[derive(Debug, Clone)]
pub struct TranslatedResponsesRequest {
    pub chat_body: Map<String, Value>,
    pub input_items: Vec<Value>,
    pub tool_kinds: BTreeMap<String, LocalToolKind>,
}
```

Implement `responses_to_chat(body, upstream_model)` as an allowlist conversion. It copies `temperature`, `top_p`, `seed`, `stop`, `parallel_tool_calls`, and `stream`; maps `max_output_tokens` to `max_tokens`; sets `stream_options.include_usage` when streaming; converts `instructions` to the first system message; converts normalized `input` items to chat messages; converts function and custom tools; and maps `tool_choice`. It intentionally consumes `reasoning`, `text`, `include`, `store`, `background`, `prompt_cache_key`, and `previous_response_id` because chat completions has no equivalent.

Use exact custom-tool encoding:

```rust
fn custom_tool_as_chat(tool: &Map<String, Value>) -> Result<Value, ResponsesTranslationError> {
    let name = required_string(tool, "name")?;
    Ok(serde_json::json!({
        "type": "function",
        "function": {
            "name": name,
            "description": tool.get("description").cloned().unwrap_or(Value::String(String::new())),
            "parameters": {
                "type": "object",
                "properties": {"input": {"type": "string"}},
                "required": ["input"],
                "additionalProperties": false
            }
        }
    }))
}
```

Reject hosted tool types such as `web_search_preview`, `image_generation`, and `computer_use_preview` with `UnsupportedTool(type_name)` before transport.

- [ ] **Step 4: Extract provider-neutral previous-response expansion under green tests**

In `src/responses/request.rs`, extract the initial cache lookup and transcript expansion into:

```rust
pub struct ExpandedResponsesInput {
    pub body: Map<String, Value>,
    pub previous_identity: Option<ResponsesTurnIdentity>,
    pub cache_status: PreviousResponseCacheStatus,
}

pub async fn expand_previous_response(
    store: &ResponsesStateStore,
    mut body: Map<String, Value>,
) -> ExpandedResponsesInput {
    let mut cache_status = PreviousResponseCacheStatus::NotRequested;
    let mut previous_identity = None;
    if let Some(previous) = body.get("previous_response_id").and_then(Value::as_str).map(str::to_string) {
        cache_status = PreviousResponseCacheStatus::Miss;
        if let Some(entry) = store.get_cached_response_state(&previous).await {
            cache_status = PreviousResponseCacheStatus::Hit;
            let mut expanded = entry.transcript;
            previous_identity = Some(entry.identity);
            if let Some(input) = normalize_input_items(body.get("input")) {
                expanded.extend(input);
            }
            body.insert("input".to_string(), Value::Array(expanded));
            body.remove("previous_response_id");
        }
    }
    ExpandedResponsesInput { body, previous_identity, cache_status }
}
```

Change `prepare_responses_request` to call this helper, then perform the existing Copilot adaptations and metadata construction without changing its output.

- [ ] **Step 5: Run translator and existing Responses request tests**

```bash
rtk cargo test local::responses::tests -- --nocapture
rtk cargo test responses::request::tests -- --nocapture
```

Expected: all translator tests pass and existing previous-response behavior is unchanged.

- [ ] **Step 6: Commit the request translator**

```bash
rtk git add src/local/mod.rs src/local/responses.rs src/responses/request.rs
rtk git commit -m "feat(responses): translate local requests to chat"
```

### Task 6: Translate non-streaming chat responses and route local Responses

**Files:**
- Modify: `src/local/responses.rs`
- Modify: `src/http/responses.rs:22-185`
- Modify: `src/http/errors.rs`
- Test: `tests/responses_contract.rs`

- [ ] **Step 1: Write failing buffered response translation tests**

Test text plus function/custom tool calls:

```rust
let response = chat_to_responses(
    serde_json::json!({
        "id":"chatcmpl-1",
        "model":"upstream",
        "choices":[{"index":0,"message":{"role":"assistant","content":"hello","tool_calls":[
            {"id":"call_1","type":"function","function":{"name":"get_weather","arguments":"{\"city\":\"NYC\"}"}}
        ]},"finish_reason":"tool_calls"}],
        "usage":{"prompt_tokens":7,"completion_tokens":3,"total_tokens":10}
    }),
    "resp_local_test",
    "qwen3-coder-30b-local",
    &BTreeMap::from([("get_weather".to_string(), LocalToolKind::Function)]),
).unwrap();

assert_eq!(response["id"], "resp_local_test");
assert_eq!(response["model"], "qwen3-coder-30b-local");
assert_eq!(response["status"], "completed");
assert_eq!(response["output"][0]["content"][0]["type"], "output_text");
assert_eq!(response["output"][1]["type"], "function_call");
assert_eq!(response["output"][1]["call_id"], "call_1");
assert_eq!(response["usage"], serde_json::json!({"input_tokens":7,"output_tokens":3,"total_tokens":10}));
```

- [ ] **Step 2: Run the test and verify RED**

```bash
rtk cargo test local::responses::tests::chat_response -- --nocapture
```

Expected: compilation fails because `chat_to_responses` does not exist.

- [ ] **Step 3: Implement buffered response conversion**

Add `chat_to_responses(chat, response_id, public_model, tool_kinds)`. It must:

- require `choices[0].message`;
- emit one Responses `message` output item when content is non-empty;
- emit `function_call` items for function tools;
- emit `custom_tool_call` items and unwrap `{"input":"text"}` for custom tools;
- preserve `call_id`, name, and argument text;
- map `prompt_tokens`, `completion_tokens`, and `total_tokens` to Responses usage names;
- use `resp_local_` IDs and public model metadata; and
- return `InvalidResponse` for malformed JSON rather than panic.

Use this response envelope:

```rust
serde_json::json!({
    "id": response_id,
    "object": "response",
    "created_at": current_epoch_seconds(),
    "status": "completed",
    "background": false,
    "error": null,
    "incomplete_details": null,
    "instructions": null,
    "max_output_tokens": null,
    "model": public_model,
    "output": output,
    "parallel_tool_calls": true,
    "previous_response_id": null,
    "reasoning": {"effort": null, "summary": null},
    "store": false,
    "temperature": null,
    "text": {"format":{"type":"text"},"verbosity":"low"},
    "tool_choice": "auto",
    "tools": [],
    "top_p": null,
    "truncation": "disabled",
    "usage": usage
})
```

- [ ] **Step 4: Write a failing local `/v1/responses` integration test**

Mock `POST /v1/chat/completions`, send `POST /v1/responses` with the public ID, text input, and function tool, then assert:

```rust
assert_eq!(response.status(), StatusCode::OK);
assert_eq!(body["model"], "qwen3-coder-30b-local");
assert!(body["id"].as_str().unwrap().starts_with("resp_local_"));
assert_eq!(body["output"][0]["content"][0]["text"], "hello");
assert_eq!(fixture.mock.hits("POST", "/v1/chat/completions").await, 1);
assert_eq!(fixture.mock.hits("POST", "/responses").await, 0);
assert_eq!(fixture.mock.hits("GET", "/models").await, 0);
```

- [ ] **Step 5: Add the local branch to the Responses handler**

After parsing `requested_model`, check `configured_local_target` before Copilot refresh. In a new `handle_local_responses` helper:

1. Call `expand_previous_response`.
2. Call `responses_to_chat` with the target upstream ID.
3. Generate `resp_local_{uuid}`.
4. For non-streaming requests, call `LocalBackend::post_chat` and `chat_to_responses`.
5. Cache `translated.input_items` plus the translated output items with the existing identity rules.
6. Return translation errors as `400 invalid_request_error` and transport errors through `openai_local_error`.

Extract the existing interaction/agent identity construction from `prepare_responses_request` into a provider-neutral helper so both branches use the same stable `ResponsesTurnIdentity` without manufacturing Copilot request metadata for local calls.

- [ ] **Step 6: Add unsupported-tool and previous-response tests**

Add a hosted-tool request that returns `400` and makes zero local/Copilot calls. Add a two-turn local test where the second request supplies `previous_response_id`; assert the second outbound chat `messages` includes the prior user input, prior assistant output, and new user input.

- [ ] **Step 7: Run focused and full non-streaming tests**

```bash
rtk cargo test --test responses_contract local_responses -- --nocapture
rtk cargo test --test responses_contract
```

Expected: local buffered Responses tests and all Copilot Responses contracts pass.

- [ ] **Step 8: Commit the buffered Responses slice**

```bash
rtk git add src/local/responses.rs src/responses/request.rs src/http/responses.rs src/http/errors.rs tests/responses_contract.rs
rtk git commit -m "feat(responses): route buffered local requests"
```

### Task 7: Translate local chat SSE to Responses SSE

**Files:**
- Modify: `src/local/responses.rs`
- Modify: `src/http/sse.rs:4-38`
- Modify: `src/http/responses.rs`
- Test: unit tests in `src/local/responses.rs`
- Test: `tests/responses_contract.rs`

- [ ] **Step 1: Write a failing stream-adapter unit test**

Feed these lines in order:

```rust
let lines = [
    r#"data: {"id":"chatcmpl-1","choices":[{"index":0,"delta":{"role":"assistant","content":"Hel"},"finish_reason":null}]}"#,
    r#"data: {"id":"chatcmpl-1","choices":[{"index":0,"delta":{"content":"lo"},"finish_reason":null}]}"#,
    r#"data: {"id":"chatcmpl-1","choices":[{"index":0,"delta":{},"finish_reason":"stop"}],"usage":{"prompt_tokens":2,"completion_tokens":1,"total_tokens":3}}"#,
    "data: [DONE]",
];
```

Assert the flattened output event types are exactly:

```rust
assert_eq!(
    event_types,
    vec![
        "response.created",
        "response.in_progress",
        "response.output_item.added",
        "response.content_part.added",
        "response.output_text.delta",
        "response.output_text.delta",
        "response.output_text.done",
        "response.content_part.done",
        "response.output_item.done",
        "response.completed",
    ]
);
assert_eq!(adapter.completed_response()["model"], "qwen3-coder-30b-local");
assert_eq!(adapter.completed_response()["usage"]["input_tokens"], 2);
```

- [ ] **Step 2: Run the stream-adapter test and verify RED**

```bash
rtk cargo test local::responses::tests::chat_sse -- --nocapture
```

Expected: compilation fails because `ChatToResponsesStream` does not exist.

- [ ] **Step 3: Add a stateful one-to-many SSE helper**

Add beside `map_sse_lines` in `src/http/sse.rs`:

```rust
pub(crate) fn map_sse_lines_many<S, F>(
    stream: S,
    mut mapper: F,
) -> impl Stream<Item = Result<Bytes, std::io::Error>>
where
    S: Stream<Item = Result<Bytes, reqwest::Error>> + Send + 'static,
    F: FnMut(&str) -> Vec<String> + Send + 'static,
{
    let mut src = Box::pin(stream);
    async_stream::stream! {
        let mut buf = String::new();
        while let Some(chunk_result) = src.next().await {
            match chunk_result {
                Err(error) => {
                    yield Err(std::io::Error::other(error));
                    return;
                }
                Ok(chunk) => buf.push_str(&String::from_utf8_lossy(&chunk)),
            }
            while let Some(newline) = buf.find('\n') {
                let line = buf[..newline].trim_end_matches('\r').to_string();
                buf.drain(..=newline);
                for mapped in mapper(&line) {
                    yield Ok(Bytes::from(format!("{mapped}\n\n")));
                }
            }
        }
        if !buf.is_empty() {
            let line = buf.trim_end_matches('\r').to_string();
            for mapped in mapper(&line) {
                yield Ok(Bytes::from(format!("{mapped}\n\n")));
            }
        }
    }
}
```

- [ ] **Step 4: Implement `ChatToResponsesStream`**

The state owns response/public IDs, accumulated text, accumulated tool calls by index, output items, usage, and terminal status. `map_line(&mut self, line) -> Vec<String>` parses only `data:` lines and emits `event: {type}\ndata: {json}` strings.

Required state transitions:

- first valid chunk: `response.created`, `response.in_progress`;
- first text delta: message `response.output_item.added`, content part added, then text delta;
- each tool index: one function/custom output item added, then argument/input delta events;
- finish: matching text/tool done events and output-item done events;
- final usage or `[DONE]`: exactly one `response.completed` containing accumulated output and mapped usage;
- malformed JSON: one `response.failed` with a bounded invalid-response error;
- duplicate `[DONE]`: no additional terminal event.

Use `response.function_call_arguments.delta/done` for function tools and `response.custom_tool_call_input.delta/done` for custom tools. Decode custom arguments from the accumulated `{"input":"text"}` object before the done event.

- [ ] **Step 5: Write a failing SSE route and streaming cache test**

Mock chunk boundaries that split one JSON `data:` line across two network chunks. Send `stream: true`, collect the proxy response, and assert ordered events, public model ID, `[DONE]`, mapped usage, and absence of the upstream model path. Extract `resp_local_` from `response.created`, send a follow-up with `previous_response_id`, and assert the outbound chat messages contain the streamed assistant output.

- [ ] **Step 6: Wire stream translation and terminal cache write**

In `handle_local_responses`, set `stream_options.include_usage`, call `LocalBackend::stream_chat`, and use an `async_stream::stream!` wrapper that:

1. feeds complete lines through `ChatToResponsesStream`;
2. yields mapped event bytes;
3. on terminal completion, caches `translated.input_items` and `adapter.output_items()` in `ResponsesStateStore`; and
4. yields exactly one final `data: [DONE]` marker.

The wrapper owns clones of the store, identity, and input items; it performs the cache write only after a completed terminal event.

- [ ] **Step 7: Run stream tests and all Responses contracts**

```bash
rtk cargo test local::responses::tests::chat_sse -- --nocapture
rtk cargo test --test responses_contract local_responses_stream -- --nocapture
rtk cargo test --test responses_contract
```

Expected: unit and integration stream tests pass, including split chunks and previous-response expansion.

- [ ] **Step 8: Commit the streaming slice**

```bash
rtk git add src/local/responses.rs src/http/sse.rs src/http/responses.rs tests/responses_contract.rs
rtk git commit -m "feat(responses): translate local SSE streams"
```

### Task 8: Reject unsupported local resource operations and document configuration

**Files:**
- Modify: `src/http/responses.rs:187-370`
- Modify: `tests/responses_contract.rs`
- Modify: `tests/responses_ws_contract.rs`
- Modify: `config.example.json`
- Modify: `README.md:180-206`

- [ ] **Step 1: Write failing retrieval and cancellation tests**

For `resp_local_example`, assert both `GET /v1/responses/resp_local_example` and `POST /v1/responses/resp_local_example/cancel` return `400`, use `invalid_request_error`, mention that local response resources are unsupported, and make zero Copilot upstream requests.

- [ ] **Step 2: Run the tests and verify RED**

```bash
rtk cargo test --test responses_contract local_response_resources -- --nocapture
```

Expected: requests are forwarded to Copilot.

- [ ] **Step 3: Reject local response IDs before Copilot dispatch**

Add:

```rust
fn is_local_response_id(response_id: &str) -> bool {
    response_id.starts_with("resp_local_")
}

fn unsupported_local_response_resource() -> Response {
    openai_error(
        StatusCode::BAD_REQUEST,
        "invalid_request_error",
        "retrieval and cancellation are not supported for local model responses",
    ).into_response()
}
```

Call this guard at the start of `responses_retrieve` and `responses_cancel`.

- [ ] **Step 4: Write and satisfy a Responses WebSocket rejection test**

Open an authenticated proxy WebSocket, send a `response.create` event whose model is `qwen3-coder-30b-local`, and assert the returned error event says local models do not support Responses WebSocket. In `handle_responses_ws`, inspect each `response.create` model before Copilot preparation and send the error frame without opening a Copilot WebSocket.

- [ ] **Step 5: Document the operator contract**

Add `"local_models": {}` to `config.example.json`. Add the approved JSON example to README's model-discovery section and state:

- definitions are listed without health probing;
- public IDs route before Copilot refresh;
- chat and Responses HTTP/SSE are supported;
- Responses WebSocket, retrieval, cancellation, auto-discovery, and outbound API keys are not supported;
- local failures never fall back to Copilot; and
- HTTP base URLs are appropriate only on a trusted private transport such as Tailscale.

- [ ] **Step 6: Run route and documentation checks**

```bash
rtk cargo test --test responses_contract local_response_resources -- --nocapture
rtk cargo test --test responses_ws_contract local_model -- --nocapture
rtk git diff --check
```

Expected: focused tests pass and the diff has no whitespace errors.

- [ ] **Step 7: Commit the scope and docs slice**

```bash
rtk git add src/http/responses.rs tests/responses_contract.rs tests/responses_ws_contract.rs config.example.json README.md
rtk git commit -m "docs(local): document model routing scope"
```

### Task 9: Full verification and live Tailscale proof

**Files:**
- Verify all changed files
- Temporary runtime config: `/tmp/copilot-proxy-rs-local-live/config.json` (never commit)

- [ ] **Step 1: Format and run the complete static/test gate**

```bash
rtk cargo fmt --all -- --check
rtk cargo clippy --all-targets --all-features --locked -- -D warnings
rtk cargo test --all-targets --all-features --locked
rtk git diff --check
```

Expected: every command exits `0`, with zero warnings and zero failing tests.

- [ ] **Step 2: Create an isolated runtime configuration**

Create `/tmp/copilot-proxy-rs-local-live/config.json` with `apply_patch`, containing:

```json
{
  "host": "127.0.0.1",
  "port": 19098,
  "local_models": {
    "qwen3-coder-30b-local": {
      "base_url": "http://100.98.223.125:8080/v1",
      "upstream_model": "models\\Qwen3-Coder-30B-A3B-Instruct-IQ4_XS.gguf"
    }
  }
}
```

- [ ] **Step 3: Start the isolated proxy and verify the catalog**

```bash
COPILOT_PROXY_RS_CONFIG_DIR=/tmp/copilot-proxy-rs-local-live rtk cargo run --bin copilot-proxy-rs -- --host 127.0.0.1 --port 19098
curl -sS http://127.0.0.1:19098/v1/models | jq '.data[] | select(.id == "qwen3-coder-30b-local")'
```

Run the proxy command in a dedicated PTY session, wait for `/health`, then run the curl command separately. Expected: one model entry with `id: "qwen3-coder-30b-local"` and `owned_by: "local"`.

- [ ] **Step 4: Prove non-streaming chat and Responses**

```bash
curl -sS http://127.0.0.1:19098/v1/chat/completions \
  -H 'Content-Type: application/json' \
  -d '{"model":"qwen3-coder-30b-local","messages":[{"role":"user","content":"Reply with exactly: local chat works"}],"temperature":0,"max_tokens":32}' | jq '{model, text: .choices[0].message.content}'

curl -sS http://127.0.0.1:19098/v1/responses \
  -H 'Content-Type: application/json' \
  -d '{"model":"qwen3-coder-30b-local","input":"Reply with exactly: local responses works","temperature":0,"max_output_tokens":32}' | jq '{id, model, status, output}'
```

Expected: both return HTTP `200`, public model IDs, and non-empty assistant text.

- [ ] **Step 5: Prove SSE and a tool-call round trip**

```bash
curl -sS -N http://127.0.0.1:19098/v1/responses \
  -H 'Content-Type: application/json' \
  -d '{"model":"qwen3-coder-30b-local","stream":true,"input":"Count from one to three."}'

local_tool_response=$(curl -sS http://127.0.0.1:19098/v1/responses \
  -H 'Content-Type: application/json' \
  -d '{"model":"qwen3-coder-30b-local","input":"What is the weather in New York? Use the tool.","tools":[{"type":"function","name":"get_weather","description":"Get weather","parameters":{"type":"object","properties":{"city":{"type":"string"}},"required":["city"]}}],"tool_choice":{"type":"function","name":"get_weather"}}')
local_response_id=$(jq -r '.id' <<<"$local_tool_response")
local_call_id=$(jq -r '.output[] | select(.type == "function_call") | .call_id' <<<"$local_tool_response")
jq -n \
  --arg model 'qwen3-coder-30b-local' \
  --arg previous_response_id "$local_response_id" \
  --arg call_id "$local_call_id" \
  '{model:$model,previous_response_id:$previous_response_id,input:[{type:"function_call_output",call_id:$call_id,output:"Sunny, 75 F"}]}' \
  | curl -sS http://127.0.0.1:19098/v1/responses -H 'Content-Type: application/json' -d @- \
  | jq '{status, model, output}'
```

Expected: the stream contains ordered `response.*` events, `response.completed`, and `[DONE]`; the first tool response contains a `get_weather` function call with valid JSON arguments; the follow-up accepts its call ID and returns a completed assistant answer.

- [ ] **Step 6: Inspect final repository state**

```bash
rtk git status --short --branch
rtk git log --oneline -10
```

Expected: only intended commits exist and the worktree is clean. Stop the isolated proxy. Report the exact live HTTP results, model ID, stream terminal event, tool-call result, and full verification counts.
