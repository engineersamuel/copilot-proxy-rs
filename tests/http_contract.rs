mod support;

use std::collections::BTreeMap;
use std::sync::Arc;

use axum::body::Body;
use http::{Request, StatusCode};
use http_body_util::BodyExt;
use serde_json::Value;
use tower::ServiceExt;

use copilot_proxy_rs::auth::{AuthEndpoints, CopilotAuth};
use copilot_proxy_rs::config::{AppConfig, EnvSource};
use copilot_proxy_rs::copilot::client::{CopilotBackend, CopilotEndpoints};
use copilot_proxy_rs::http::router;
use copilot_proxy_rs::models::ModelRegistry;
use copilot_proxy_rs::models::{infer_owned_by, model_list_for_snapshot};
use copilot_proxy_rs::state::{AppState, BackendKind, BackendSnapshot};

fn repo_tempdir(prefix: &str) -> tempfile::TempDir {
    tempfile::Builder::new()
        .prefix(prefix)
        .tempdir_in(env!("CARGO_MANIFEST_DIR"))
        .unwrap()
}

/// Creates an AppState with no token available (isolated temp dir with no token file).
async fn state_with_no_token() -> AppState {
    let temp = repo_tempdir("http-contract-");
    // Intentionally no github_token file written
    let env =
        EnvSource::from_pairs([("COPILOT_PROXY_RS_CONFIG_DIR", temp.path().to_str().unwrap())]);
    let config = Arc::new(AppConfig::load_from_env(&env).unwrap());
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
    AppState::with_parts_for_tests(config, models, copilot)
}

async fn state_with_no_token_and_api_key(api_key: &str) -> AppState {
    let temp = repo_tempdir("http-contract-");
    let env =
        EnvSource::from_pairs([("COPILOT_PROXY_RS_CONFIG_DIR", temp.path().to_str().unwrap())]);
    let mut config = AppConfig::load_from_env(&env).unwrap();
    config.api_key = api_key.to_string();
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
    AppState::with_parts_for_tests(config, models, copilot)
}

#[test]
fn infer_owned_by_matches_python_prefix_rules() {
    assert_eq!(infer_owned_by("gpt-5.4"), "openai");
    assert_eq!(infer_owned_by("gemini-3-pro-preview"), "google");
    assert_eq!(infer_owned_by("grok-code-fast-1"), "xai");
    assert_eq!(infer_owned_by("claude-sonnet-4-6"), "anthropic");
    assert_eq!(infer_owned_by("raptor-mini"), "other");
}

#[test]
fn copilot_model_list_is_empty_without_live_metadata() {
    let response = model_list_for_snapshot(BackendSnapshot {
        primary: BackendKind::Copilot,
        fallback: None,
    });

    assert_eq!(response.object, "list");
    assert!(response.data.is_empty());
    assert!(response.models.is_empty());
}

#[tokio::test]
async fn health_returns_status_version_backend_and_runtime() {
    let app = router(AppState::new(AppConfig::default()));
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
    let body = response_json(response).await;
    assert_eq!(body["status"], "ok");
    assert_eq!(body["version"], env!("CARGO_PKG_VERSION"));
    assert_eq!(body["backend"], "copilot");
    assert_eq!(body["runtime"]["implementation"], "rust");
}

#[tokio::test]
async fn version_returns_version_and_runtime() {
    let app = router(AppState::new(AppConfig::default()));
    let response = app
        .oneshot(
            Request::builder()
                .uri("/version")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let body = response_json(response).await;
    assert_eq!(body["version"], env!("CARGO_PKG_VERSION"));
    assert_eq!(body["runtime"]["implementation"], "rust");
}

#[tokio::test]
async fn models_route_returns_empty_live_catalog_when_refresh_is_unavailable() {
    let app = router(state_with_no_token().await);
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
    assert_eq!(body["data"], serde_json::json!([]));
    assert_eq!(body["models"], serde_json::json!([]));
}

#[tokio::test]
async fn count_tokens_returns_simple_input_token_estimate() {
    let app = router(AppState::new(AppConfig::default()));
    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/messages/count_tokens")
                .header("content-type", "application/json")
                .body(Body::from(
                    r#"{"system":"hello","messages":[{"role":"user","content":"world"}]}"#,
                ))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let body = response_json(response).await;
    assert_eq!(body["input_tokens"], 2);
}

#[tokio::test]
async fn anthropic_messages_returns_auth_error_when_no_token_available() {
    let app = router(state_with_no_token().await);
    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/messages")
                .header("content-type", "application/json")
                .body(Body::from(
                    r#"{"model":"claude-sonnet-4-6","max_tokens":100,"messages":[{"role":"user","content":"hello"}]}"#,
                ))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    let body = response_json(response).await;
    assert_eq!(body["type"], "error");
    assert_eq!(body["error"]["type"], "authentication_error");
    assert!(body["error"]["message"].as_str().is_some());
}

#[tokio::test]
async fn openai_chat_returns_auth_error_when_no_token_available() {
    let app = router(state_with_no_token().await);
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
    assert!(body["error"]["param"].is_null());
    assert!(body["error"]["code"].is_null());
}

#[tokio::test]
async fn count_tokens_unsupported_encoding_returns_anthropic_error() {
    let app = router(AppState::new(AppConfig::default()));
    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/messages/count_tokens")
                .header("content-type", "application/json")
                .header("content-encoding", "br")
                .body(Body::from(b"\x1b\x28\x00\x04\x22hello world\x03".as_ref()))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let body = response_json(response).await;
    assert_eq!(body["type"], "error");
    assert_eq!(body["error"]["type"], "invalid_request_error");
    assert!(body["error"]["message"].as_str().is_some());
}

#[tokio::test]
async fn count_tokens_non_object_json_returns_anthropic_error() {
    let app = router(AppState::new(AppConfig::default()));
    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/messages/count_tokens")
                .header("content-type", "application/json")
                .body(Body::from(r#"[1, 2, 3]"#))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let body = response_json(response).await;
    assert_eq!(body["type"], "error");
    assert_eq!(body["error"]["type"], "invalid_request_error");
    assert!(body["error"]["message"].as_str().is_some());
}

#[tokio::test]
async fn chat_route_rejects_body_over_decoded_limit() {
    let config = AppConfig {
        max_decoded_body_bytes: 8,
        ..Default::default()
    };
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

#[tokio::test]
async fn dynamic_copilot_models_store_supported_reasoning_efforts_internally() {
    let registry = ModelRegistry::new();
    registry
        .set_copilot_models(vec![serde_json::json!({
            "id": "gpt-effort-live",
            "created_at": 1_800_000_000u64,
            "owned_by": "openai",
            "supported_endpoints": ["/chat/completions"],
            "capabilities": {
                "supports": {
                    "reasoning_effort": ["low", "medium", "high"]
                }
            }
        })])
        .await;

    let efforts = registry
        .supported_efforts("gpt-effort-live")
        .await
        .expect("dynamic model should expose internal supported efforts");

    assert_eq!(efforts.as_strings(), vec!["low", "medium", "high"]);
}

#[tokio::test]
async fn known_models_do_not_get_static_effort_fallbacks_without_metadata() {
    let registry = ModelRegistry::new();

    assert!(
        registry
            .supported_efforts("claude-opus-4.8")
            .await
            .is_none(),
        "known models without dynamic metadata should not advertise static effort support"
    );
    assert!(
        registry
            .supported_efforts("claude-sonnet-4-6")
            .await
            .is_none(),
        "aliases without dynamic metadata should not advertise static effort support"
    );
}

#[tokio::test]
async fn models_route_includes_cached_upstream_only_model_ids() {
    let state = state_with_no_token().await;
    state
        .models
        .set_copilot_models(vec![serde_json::json!({
            "id": "gpt-upstream-only",
            "created_at": 1_800_000_000u64,
            "owned_by": "openai",
            "supported_endpoints": ["/responses"]
        })])
        .await;

    let response = router(state)
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
    let ids: Vec<&str> = body["data"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|model| model["id"].as_str())
        .collect();
    assert!(
        ids.contains(&"gpt-upstream-only"),
        "public /v1/models should include live Copilot model IDs"
    );
    assert!(
        body["models"]
            .as_array()
            .unwrap()
            .iter()
            .any(|model| model["slug"] == "gpt-upstream-only")
    );
    let rich = body["models"]
        .as_array()
        .unwrap()
        .iter()
        .find(|model| model["slug"] == "gpt-upstream-only")
        .unwrap();
    assert_eq!(rich["context_window"], Value::Null);
    assert_eq!(rich["max_context_window"], Value::Null);
    assert_eq!(rich["context_window_modes"], serde_json::json!([]));
}

#[tokio::test]
async fn models_route_refreshes_and_exposes_upstream_model_ids() {
    let fixture = support::AppFixture::with_mock_copilot().await;
    fixture
        .mock
        .respond_json(
            "GET",
            "/models",
            200,
            serde_json::json!({
                "data": [{
                    "id": "claude-sonnet-5",
                    "owned_by": "anthropic",
                    "created_at": 1_800_000_000u64,
                    "supported_endpoints": ["/v1/messages"]
                }]
            }),
        )
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
    assert!(
        body["data"]
            .as_array()
            .unwrap()
            .iter()
            .any(|model| model["id"] == "claude-sonnet-5")
    );
    let sonnet5 = body["models"]
        .as_array()
        .unwrap()
        .iter()
        .find(|model| model["slug"] == "claude-sonnet-5")
        .unwrap();
    assert_eq!(sonnet5["source"], "dynamic");
    assert_eq!(
        sonnet5["supported_endpoints"],
        serde_json::json!(["/v1/messages"])
    );
}

#[tokio::test]
async fn app_state_wires_copilot_model_overrides() {
    let mut config = AppConfig::default();
    config
        .model_overrides
        .copilot
        .insert("sonnet-latest".to_string(), "claude-sonnet-5".to_string());

    let state = AppState::new(config);
    assert_eq!(
        state.models.get_copilot_openai_model("sonnet-latest").await,
        "claude-sonnet-5"
    );
}

#[tokio::test]
async fn copilot_model_overrides_take_precedence_over_live_exact_ids() {
    let mut overrides = BTreeMap::new();
    overrides.insert("preferred-model".to_string(), "claude-sonnet-5".to_string());
    let registry = ModelRegistry::with_copilot_overrides(overrides);
    registry
        .set_copilot_models(vec![serde_json::json!({
            "id": "preferred-model",
            "owned_by": "openai"
        })])
        .await;

    assert_eq!(
        registry.get_copilot_openai_model("preferred-model").await,
        "claude-sonnet-5"
    );
}

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
    let gpt55 = snapshot
        .models
        .iter()
        .find(|model| model.slug == "gpt-5.5")
        .unwrap();
    assert_eq!(
        gpt55.source,
        copilot_proxy_rs::models::ModelMetadataSource::Dynamic
    );
}

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
    let state = state_with_no_token().await;
    state
        .models
        .set_copilot_models(vec![serde_json::json!({
            "id": "gpt-5.5",
            "owned_by": "openai",
            "supported_endpoints": ["/responses"],
            "authorization": "Bearer secret"
        })])
        .await;

    let response = router(state)
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
    assert!(
        body["snapshot"]["models"]
            .as_array()
            .unwrap()
            .iter()
            .any(|model| model["slug"] == "gpt-5.5")
    );
}

#[tokio::test]
async fn models_route_uses_dynamic_context_and_reasoning_metadata() {
    let state = state_with_no_token().await;
    state
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

    let response = router(state)
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
    assert_eq!(
        rich_gpt55["supported_endpoints"],
        serde_json::json!(["/responses", "ws:/responses"])
    );
    assert_eq!(
        rich_gpt55["supported_reasoning_levels"],
        serde_json::json!([
            {"effort": "low", "description": "Fast responses with lighter reasoning"},
            {"effort": "medium", "description": "Balances speed and reasoning depth for everyday tasks"},
            {"effort": "high", "description": "Greater reasoning depth for complex problems"},
            {"effort": "xhigh", "description": "Extra high reasoning depth for complex problems"}
        ])
    );
    assert_eq!(rich_gpt55["shell_type"], "shell_command");
    assert_eq!(rich_gpt55["visibility"], "list");
    assert_eq!(rich_gpt55["supported_in_api"], true);
    assert_eq!(rich_gpt55["apply_patch_tool_type"], "freeform");
    assert_eq!(rich_gpt55["web_search_tool_type"], "text_and_image");
    assert_eq!(rich_gpt55["supports_parallel_tool_calls"], true);
    assert_eq!(
        rich_gpt55["input_modalities"],
        serde_json::json!(["text", "image"])
    );
    assert_eq!(rich_gpt55["source"], "dynamic");
}

async fn response_json(response: axum::response::Response) -> Value {
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    serde_json::from_slice(&bytes).unwrap()
}

#[tokio::test]
async fn health_does_not_require_inbound_auth() {
    let app = router(AppState::new(AppConfig {
        api_key: "local-secret".to_string(),
        ..AppConfig::default()
    }));
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
    let app = router(state_with_no_token_and_api_key("local-secret").await);
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
    let app = router(state_with_no_token_and_api_key("local-secret").await);
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

    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    let body = response_json(response).await;
    assert_eq!(body["error"]["type"], "authentication_error");
    assert_ne!(body["error"]["message"], "Missing or invalid proxy API key");
}

#[tokio::test]
async fn copilot_routes_accept_lowercase_bearer_inbound_auth_when_configured() {
    let app = router(state_with_no_token_and_api_key("local-secret").await);
    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/chat/completions")
                .header("content-type", "application/json")
                .header("authorization", "bearer local-secret")
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
    assert_ne!(body["error"]["message"], "Missing or invalid proxy API key");
}

#[tokio::test]
async fn copilot_routes_accept_x_api_key_inbound_auth_when_configured() {
    let app = router(state_with_no_token_and_api_key("local-secret").await);
    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/messages/count_tokens")
                .header("content-type", "application/json")
                .header("x-api-key", "local-secret")
                .body(Body::from(
                    r#"{"messages":[{"role":"user","content":"hello"}]}"#,
                ))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
}
