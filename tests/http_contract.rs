mod support;

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
fn copilot_model_list_contains_anthropic_and_openai_models() {
    let response = model_list_for_snapshot(BackendSnapshot {
        primary: BackendKind::Copilot,
        fallback: None,
    });

    assert_eq!(response.object, "list");
    assert!(
        response
            .data
            .iter()
            .any(|model| model.id == "claude-sonnet-4-6")
    );
    assert!(response.data.iter().any(|model| model.id == "gpt-5.4"));
    assert!(response.data.iter().all(|model| model.object == "model"));
}

#[test]
fn bedrock_model_list_contains_anthropic_models_only_without_copilot_fallback() {
    let response = model_list_for_snapshot(BackendSnapshot {
        primary: BackendKind::Bedrock,
        fallback: None,
    });

    assert!(
        response
            .data
            .iter()
            .any(|model| model.id == "claude-sonnet-4-6")
    );
    assert!(!response.data.iter().any(|model| model.id == "gpt-5.4"));
}

#[test]
fn bedrock_with_copilot_fallback_includes_non_claude_copilot_models() {
    let response = model_list_for_snapshot(BackendSnapshot {
        primary: BackendKind::Bedrock,
        fallback: Some(BackendKind::Copilot),
    });

    assert!(
        response
            .data
            .iter()
            .any(|model| model.id == "claude-sonnet-4-6")
    );
    assert!(response.data.iter().any(|model| model.id == "gpt-5.4"));
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
async fn models_route_returns_openai_model_list() {
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

    let public = registry
        .list_for_snapshot(BackendSnapshot {
            primary: BackendKind::Copilot,
            fallback: None,
        })
        .await;
    let public_json = serde_json::to_value(public.data.first().unwrap()).unwrap();
    assert!(
        public_json.get("capabilities").is_none(),
        "public /v1/models shape must not expose internal capabilities"
    );
}

#[tokio::test]
async fn static_claude_effort_fallbacks_match_python_supported_sets() {
    let registry = ModelRegistry::new();

    assert_eq!(
        registry
            .supported_efforts("claude-opus-4.8")
            .await
            .unwrap()
            .as_strings(),
        vec!["low", "medium", "high", "xhigh", "max"]
    );
    assert_eq!(
        registry
            .supported_efforts("claude-opus-4-7")
            .await
            .unwrap()
            .as_strings(),
        vec!["low", "medium", "high", "xhigh", "max"]
    );
    assert_eq!(
        registry
            .supported_efforts("claude-opus-4.6")
            .await
            .unwrap()
            .as_strings(),
        vec!["low", "medium", "high", "max"]
    );
    assert_eq!(
        registry
            .supported_efforts("claude-sonnet-4-6")
            .await
            .unwrap()
            .as_strings(),
        vec!["low", "medium", "high", "max"]
    );
    assert!(
        registry
            .supported_efforts("gpt-without-metadata")
            .await
            .is_none(),
        "models without dynamic metadata or static fallback should not support effort"
    );
}

#[tokio::test]
async fn dynamic_copilot_models_override_static_catalog_when_present() {
    let registry = ModelRegistry::new();
    registry
        .set_copilot_models(vec![serde_json::json!({
            "id": "gpt-dynamic",
            "created_at": 1_800_000_000u64,
            "owned_by": "openai",
            "supported_endpoints": ["/chat/completions"],
            "capabilities": {"limits": {"max_prompt_tokens": 1234, "max_output_tokens": 99}}
        })])
        .await;

    let response = registry
        .list_for_snapshot(BackendSnapshot {
            primary: BackendKind::Copilot,
            fallback: None,
        })
        .await;

    assert_eq!(response.data.len(), 1);
    assert_eq!(response.data[0].id, "gpt-dynamic");
}

#[tokio::test]
async fn models_route_uses_dynamic_copilot_models_when_refreshed() {
    let fixture = support::AppFixture::with_mock_copilot().await;
    fixture
        .mock
        .respond_json(
            "GET",
            "/models",
            200,
            serde_json::json!({
                "data": [{
                    "id": "gpt-live",
                    "created_at": 1800000000,
                    "owned_by": "openai",
                    "supported_endpoints": ["/chat/completions"]
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
            .any(|model| model["id"] == "gpt-live")
    );
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
