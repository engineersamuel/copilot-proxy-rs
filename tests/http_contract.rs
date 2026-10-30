mod support;

use std::collections::BTreeMap;
use std::io::Write;
use std::sync::Arc;

use axum::body::Body;
use flate2::Compression;
use flate2::write::GzEncoder;
use http::{Request, StatusCode};
use http_body_util::BodyExt;
use serde_json::Value;
use tower::ServiceExt;

use copilot_proxy_rs::auth::{AuthEndpoints, CopilotAuth};
use copilot_proxy_rs::config::{AppConfig, EnvSource, LocalModelConfig};
use copilot_proxy_rs::copilot::client::{CopilotBackend, CopilotEndpoints};
use copilot_proxy_rs::http::router;
use copilot_proxy_rs::models::{ModelMetadataSource, ModelRegistry, ModelTarget};
use copilot_proxy_rs::state::{AppState, BackendKind, BackendSnapshot};

fn qwen_local_models() -> BTreeMap<String, LocalModelConfig> {
    BTreeMap::from([(
        "qwen3-coder-30b-local".to_string(),
        LocalModelConfig {
            base_url: "http://100.98.223.125:8080/v1".to_string(),
            upstream_model: r"models\Qwen3-Coder-30B-A3B-Instruct-IQ4_XS.gguf".to_string(),
        },
    )])
}

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

#[tokio::test]
async fn configured_local_model_is_listed_and_resolved() {
    let registry = ModelRegistry::with_models(BTreeMap::new(), qwen_local_models());
    let response = registry
        .list_for_snapshot(BackendSnapshot {
            primary: BackendKind::Copilot,
            fallback: None,
        })
        .await;

    assert!(
        response
            .data
            .iter()
            .any(|model| model.id == "qwen3-coder-30b-local")
    );
    let rich = response
        .models
        .iter()
        .find(|model| model.slug == "qwen3-coder-30b-local")
        .unwrap();
    assert_eq!(rich.source, ModelMetadataSource::Local);
    assert_eq!(
        rich.supported_endpoints,
        vec!["/chat/completions", "/responses", "/v1/messages"]
    );

    let target = registry.resolve_target("qwen3-coder-30b-local").await;
    assert!(matches!(
        target,
        ModelTarget::Local(ref local)
            if local.public_id == "qwen3-coder-30b-local"
                && local.upstream_model == r"models\Qwen3-Coder-30B-A3B-Instruct-IQ4_XS.gguf"
    ));
}

#[tokio::test]
async fn prefixed_configured_local_model_keeps_its_exact_public_id() {
    let registry = ModelRegistry::with_models(
        BTreeMap::new(),
        BTreeMap::from([(
            "github-copilot/foo".to_string(),
            LocalModelConfig {
                base_url: "http://127.0.0.1:8080/v1".to_string(),
                upstream_model: "exact-prefixed-target".to_string(),
            },
        )]),
    );

    let catalog = registry
        .list_for_snapshot(BackendSnapshot {
            primary: BackendKind::Copilot,
            fallback: None,
        })
        .await;
    assert!(
        catalog
            .data
            .iter()
            .any(|model| model.id == "github-copilot/foo")
    );
    assert!(catalog.models.iter().any(|model| {
        model.slug == "github-copilot/foo" && model.source == ModelMetadataSource::Local
    }));

    let target = registry.resolve_target("github-copilot/foo").await;
    assert!(matches!(
        target,
        ModelTarget::Local(ref local)
            if local.public_id == "github-copilot/foo"
                && local.upstream_model == "exact-prefixed-target"
    ));
}

#[tokio::test]
async fn prefixed_local_request_falls_back_to_unprefixed_key_but_exact_key_wins_collision() {
    let fallback_registry = ModelRegistry::with_models(
        BTreeMap::new(),
        BTreeMap::from([(
            "foo".to_string(),
            LocalModelConfig {
                base_url: "http://127.0.0.1:8080/v1".to_string(),
                upstream_model: "fallback-target".to_string(),
            },
        )]),
    );
    let fallback_catalog = fallback_registry
        .list_for_snapshot(BackendSnapshot {
            primary: BackendKind::Copilot,
            fallback: None,
        })
        .await;
    assert!(fallback_catalog.data.iter().any(|model| model.id == "foo"));
    assert!(
        !fallback_catalog
            .data
            .iter()
            .any(|model| model.id == "github-copilot/foo")
    );
    let fallback = fallback_registry.resolve_target("github-copilot/foo").await;
    assert!(matches!(
        fallback,
        ModelTarget::Local(ref local)
            if local.public_id == "foo" && local.upstream_model == "fallback-target"
    ));

    let collision_registry = ModelRegistry::with_models(
        BTreeMap::new(),
        BTreeMap::from([
            (
                "foo".to_string(),
                LocalModelConfig {
                    base_url: "http://127.0.0.1:8080/v1".to_string(),
                    upstream_model: "fallback-target".to_string(),
                },
            ),
            (
                "github-copilot/foo".to_string(),
                LocalModelConfig {
                    base_url: "http://127.0.0.1:8081/v1".to_string(),
                    upstream_model: "exact-target".to_string(),
                },
            ),
        ]),
    );
    let collision_catalog = collision_registry
        .list_for_snapshot(BackendSnapshot {
            primary: BackendKind::Copilot,
            fallback: None,
        })
        .await;
    for configured_id in ["foo", "github-copilot/foo"] {
        assert!(collision_catalog.models.iter().any(|model| {
            model.slug == configured_id && model.source == ModelMetadataSource::Local
        }));
    }
    let exact = collision_registry
        .resolve_target("github-copilot/foo")
        .await;
    assert!(matches!(
        exact,
        ModelTarget::Local(ref local)
            if local.public_id == "github-copilot/foo"
                && local.upstream_model == "exact-target"
    ));
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

    assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);
    let body = response_json(response).await;
    assert_eq!(body["error"]["type"], "invalid_request_error");
    let message = body["error"]["message"].as_str().unwrap();
    assert!(message.contains("8 bytes"));
    assert!(message.contains("COPILOT_PROXY_RS_MAX_DECODED_BODY_BYTES"));
}

#[tokio::test]
async fn compressed_body_within_decoded_limit_allows_encoding_overhead() {
    let payload = b"{}";
    let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
    encoder.write_all(payload).unwrap();
    let compressed = encoder.finish().unwrap();
    assert!(compressed.len() > payload.len());

    let config = AppConfig {
        max_decoded_body_bytes: payload.len() as u64,
        ..Default::default()
    };
    let response = router(AppState::new(config))
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/messages/count_tokens")
                .header("content-type", "application/json")
                .header("content-encoding", "gzip")
                .body(Body::from(compressed))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
}

#[tokio::test]
async fn messages_route_uses_anthropic_request_too_large_error_type() {
    let config = AppConfig {
        max_decoded_body_bytes: 8,
        ..Default::default()
    };
    let response = router(AppState::new(config))
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/messages")
                .header("content-type", "application/json")
                .body(Body::from(
                    r#"{"model":"claude-sonnet-5","messages":[{"role":"user","content":"hello"}]}"#,
                ))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);
    let body = response_json(response).await;
    assert_eq!(body["error"]["type"], "request_too_large");
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
async fn claude_family_aliases_resolve_to_latest_live_versions() {
    let registry = ModelRegistry::new();
    registry
        .set_copilot_models(
            [
                "claude-opus-4.8",
                "claude-opus-5.5",
                "claude-opus-5",
                "claude-opus-99-preview",
                "claude-sonnet-5.5",
                "claude-haiku-5.5",
            ]
            .into_iter()
            .map(|id| serde_json::json!({"id": id}))
            .collect(),
        )
        .await;

    for (alias, expected) in [
        ("opusplan", "claude-opus-5.5"),
        ("opusplan[1m]", "claude-opus-5.5"),
        ("github-copilot/opusplan", "claude-opus-5.5"),
        ("opus", "claude-opus-5.5"),
        ("opus[1m]", "claude-opus-5.5"),
        ("sonnet", "claude-sonnet-5.5"),
        ("sonnet[1m]", "claude-sonnet-5.5"),
        ("haiku", "claude-haiku-5.5"),
    ] {
        assert_eq!(
            registry.get_copilot_openai_model(alias).await,
            expected,
            "{alias}"
        );
    }
}

#[tokio::test]
async fn claude_family_aliases_follow_refreshes_and_compare_versions_numerically() {
    let registry = ModelRegistry::new();
    for (versions, expected) in [
        (
            vec!["claude-opus-5.5", "claude-opus-5.10", "claude-opus-5.9"],
            "claude-opus-5.10",
        ),
        (vec!["claude-opus-6", "claude-opus-5.10"], "claude-opus-6"),
        (vec!["claude-opus-5.5"], "claude-opus-5.5"),
    ] {
        registry
            .set_copilot_models(
                versions
                    .into_iter()
                    .map(|id| serde_json::json!({"id": id}))
                    .collect(),
            )
            .await;
        assert_eq!(
            registry.get_copilot_openai_model("opusplan").await,
            expected
        );
    }
}

#[tokio::test]
async fn claude_family_aliases_do_not_guess_unavailable_models() {
    let registry = ModelRegistry::new();
    assert_eq!(
        registry.get_copilot_openai_model("opusplan").await,
        "opusplan"
    );
    registry
        .set_copilot_models(vec![serde_json::json!({"id": "claude-sonnet-5.5"})])
        .await;
    assert_eq!(
        registry.get_copilot_openai_model("opusplan").await,
        "opusplan"
    );
    assert_eq!(
        registry.get_copilot_openai_model("claude-opus-4.8").await,
        "claude-opus-4.8"
    );
}

#[tokio::test]
async fn claude_family_aliases_preserve_overrides_and_live_exact_ids() {
    let registry = ModelRegistry::with_copilot_overrides(BTreeMap::from([(
        "opusplan".to_string(),
        "claude-opus-5".to_string(),
    )]));
    registry
        .set_copilot_models(vec![
            serde_json::json!({"id": "opus"}),
            serde_json::json!({"id": "claude-opus-5.5"}),
        ])
        .await;
    assert_eq!(
        registry.get_copilot_openai_model("opusplan").await,
        "claude-opus-5"
    );
    assert_eq!(registry.get_copilot_openai_model("opus").await, "opus");
}

#[tokio::test]
async fn claude_family_aliases_do_not_replace_configured_local_targets() {
    let local_config = LocalModelConfig {
        base_url: "http://127.0.0.1:9000/v1".to_string(),
        upstream_model: "local-opus".to_string(),
    };
    let registry = ModelRegistry::with_models(
        BTreeMap::new(),
        BTreeMap::from([("opusplan".to_string(), local_config)]),
    );
    registry
        .set_copilot_models(vec![serde_json::json!({"id": "claude-opus-5.5"})])
        .await;

    for alias in ["opusplan", "github-copilot/opusplan"] {
        assert!(matches!(
            registry.resolve_target(alias).await,
            ModelTarget::Local(target)
                if target.public_id == "opusplan" && target.upstream_model == "local-opus"
        ));
    }
}

#[tokio::test]
async fn gpt56_aliases_resolve_to_canonical_models() {
    let registry = ModelRegistry::new();

    assert_eq!(
        registry.get_copilot_openai_model("gpt-5-6-sol").await,
        "gpt-5.6-sol"
    );
    assert_eq!(
        registry.get_copilot_openai_model("gpt-5-6-luna").await,
        "gpt-5.6-luna"
    );
    assert_eq!(
        registry.get_copilot_openai_model("gpt-5-6-terra").await,
        "gpt-5.6-terra"
    );
    assert_eq!(
        registry.get_copilot_openai_model("gpt-5-6-tera").await,
        "gpt-5.6-terra"
    );
    assert_eq!(
        registry.get_copilot_openai_model("gpt-5.6-tera").await,
        "gpt-5.6-terra"
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
                    "id": "claude-sonnet-5.7",
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
            .any(|model| model["id"] == "claude-sonnet-5.7")
    );
    let sonnet5 = body["models"]
        .as_array()
        .unwrap()
        .iter()
        .find(|model| model["slug"] == "claude-sonnet-5.7")
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
            "id": "gpt-5.7",
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
    let rich_gpt57 = body["models"]
        .as_array()
        .unwrap()
        .iter()
        .find(|model| model["slug"] == "gpt-5.7")
        .unwrap();

    assert_eq!(rich_gpt57["context_window"], 400_000);
    assert_eq!(rich_gpt57["max_context_window"], 1_100_000);
    assert_eq!(
        rich_gpt57["supported_endpoints"],
        serde_json::json!(["/responses", "ws:/responses"])
    );
    assert_eq!(
        rich_gpt57["supported_reasoning_levels"],
        serde_json::json!([
            {"effort": "low", "description": "Fast responses with lighter reasoning"},
            {"effort": "medium", "description": "Balances speed and reasoning depth for everyday tasks"},
            {"effort": "high", "description": "Greater reasoning depth for complex problems"},
            {"effort": "xhigh", "description": "Extra high reasoning depth for complex problems"}
        ])
    );
    assert_eq!(rich_gpt57["shell_type"], "shell_command");
    assert_eq!(rich_gpt57["visibility"], "list");
    assert_eq!(rich_gpt57["supported_in_api"], true);
    assert_eq!(rich_gpt57["apply_patch_tool_type"], "freeform");
    assert_eq!(rich_gpt57["web_search_tool_type"], "text_and_image");
    assert_eq!(rich_gpt57["supports_parallel_tool_calls"], true);
    assert_eq!(
        rich_gpt57["input_modalities"],
        serde_json::json!(["text", "image"])
    );
    assert_eq!(rich_gpt57["source"], "dynamic");
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

async fn aa_mock(status: u16) -> String {
    aa_mock_ctl(status).await.0
}

/// Mock AA server whose status code can be changed after start.
async fn aa_mock_ctl(status: u16) -> (String, Arc<std::sync::atomic::AtomicU16>) {
    use axum::routing::get;
    use std::sync::atomic::{AtomicU16, Ordering};
    let fixture: Value =
        serde_json::from_str(include_str!("fixtures/aa_models_sample.json")).unwrap();
    let status = Arc::new(AtomicU16::new(status));
    let handle = status.clone();
    let app = axum::Router::new().route(
        "/language/models/free",
        get(move || {
            let fixture = fixture.clone();
            let code = status.load(Ordering::SeqCst);
            async move { (StatusCode::from_u16(code).unwrap(), axum::Json(fixture)) }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (format!("http://{addr}"), handle)
}

async fn models_json(state: AppState) -> Value {
    let response = router(state)
        .oneshot(Request::get("/v1/models").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    serde_json::from_slice(&bytes).unwrap()
}

fn find<'a>(body: &'a Value, id: &str) -> &'a Value {
    body["data"]
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["id"] == id)
        .unwrap_or_else(|| panic!("{id} not listed"))
}

fn copilot_model(id: &str) -> Value {
    serde_json::json!({"id": id, "owned_by": "anthropic"})
}

#[tokio::test]
async fn models_are_enriched_and_cache_survives_later_failure() {
    use copilot_proxy_rs::model_stats::ModelStatsStore;

    let mut state = state_with_no_token().await;
    state
        .models
        .set_copilot_models(vec![copilot_model("claude-opus-5.5")])
        .await;

    let (url, status) = aa_mock_ctl(200).await;
    let good = Arc::new(ModelStatsStore::with_base_url("k".into(), url));
    good.refresh().await;
    state.model_stats = good.clone();
    let body = models_json(state.clone()).await;
    let opus = find(&body, "claude-opus-5.5");
    assert_eq!(opus["stats"]["intelligence_index"], 57.6);
    assert!(opus["stats"].get("coding_index").is_none());
    assert_eq!(body["stats_attribution"]["name"], "Artificial Analysis");

    // Later failures must not drop cached data.
    for status_code in [429, 500] {
        status.store(status_code, std::sync::atomic::Ordering::SeqCst);
        good.refresh().await;
        let body = models_json(state.clone()).await;
        assert_eq!(
            find(&body, "claude-opus-5.5")["stats"]["intelligence_index"],
            57.6
        );
    }
}

#[tokio::test]
async fn local_models_never_get_stats() {
    use copilot_proxy_rs::model_stats::ModelStatsStore;

    let temp = repo_tempdir("http-contract-");
    let env =
        EnvSource::from_pairs([("COPILOT_PROXY_RS_CONFIG_DIR", temp.path().to_str().unwrap())]);
    let config = Arc::new(AppConfig::load_from_env(&env).unwrap());
    let auth = Arc::new(CopilotAuth::with_env_for_tests(
        config.clone(),
        env,
        AuthEndpoints::localhost_for_tests(),
        false,
    ));
    let local = BTreeMap::from([(
        "claude-opus-5.5".to_string(),
        LocalModelConfig {
            base_url: "http://127.0.0.1:1/v1".to_string(),
            upstream_model: "llama".to_string(),
        },
    )]);
    let models = Arc::new(ModelRegistry::with_models(BTreeMap::new(), local));
    let copilot = Arc::new(CopilotBackend::with_endpoints_for_tests(
        config.clone(),
        auth,
        models.clone(),
        CopilotEndpoints::default(),
    ));
    let mut state = AppState::with_parts_for_tests(config, models, copilot);
    let store = Arc::new(ModelStatsStore::with_base_url(
        "k".into(),
        aa_mock(200).await,
    ));
    store.refresh().await;
    state.model_stats = store;

    let body = models_json(state).await;
    let entry = find(&body, "claude-opus-5.5");
    for key in ["stats", "description", "excels_at", "curated"] {
        assert!(entry.get(key).is_none(), "{key} leaked onto local model");
    }
}

#[tokio::test]
async fn aa_client_does_not_follow_redirects_with_api_key() {
    use axum::routing::get;
    use copilot_proxy_rs::model_stats::ModelStatsStore;
    use std::sync::atomic::{AtomicBool, Ordering};

    let hit = Arc::new(AtomicBool::new(false));
    let target_hit = hit.clone();
    let target = axum::Router::new().route(
        "/steal",
        get(move |headers: http::HeaderMap| {
            let hit = target_hit.clone();
            async move {
                if headers.contains_key("x-api-key") {
                    hit.store(true, Ordering::SeqCst);
                }
                "x"
            }
        }),
    );
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let target_addr = l.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(l, target).await.unwrap() });
    let redirect = axum::Router::new().route(
        "/language/models/free",
        get(move || async move {
            (
                StatusCode::FOUND,
                [("location", format!("http://{target_addr}/steal"))],
            )
        }),
    );
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(l, redirect).await.unwrap() });

    let store = ModelStatsStore::with_base_url("k".into(), format!("http://{addr}"));
    store.refresh().await;
    assert!(!hit.load(Ordering::SeqCst), "redirect was followed");
    assert!(
        store
            .insights_for("claude-opus-5.5")
            .unwrap()
            .stats
            .is_none()
    );
}

#[tokio::test]
async fn overridden_copilot_ids_get_no_stats_and_empty_body_keeps_cache() {
    use copilot_proxy_rs::model_stats::ModelStatsStore;

    let temp = repo_tempdir("http-contract-");
    let env =
        EnvSource::from_pairs([("COPILOT_PROXY_RS_CONFIG_DIR", temp.path().to_str().unwrap())]);
    let config = Arc::new(AppConfig::load_from_env(&env).unwrap());
    let auth = Arc::new(CopilotAuth::with_env_for_tests(
        config.clone(),
        env,
        AuthEndpoints::localhost_for_tests(),
        false,
    ));
    let overrides = BTreeMap::from([("claude-opus-5.5".to_string(), "gpt-6.1-sol".to_string())]);
    let models = Arc::new(ModelRegistry::with_copilot_overrides(overrides));
    models
        .set_copilot_models(vec![copilot_model("claude-opus-5.5")])
        .await;
    let copilot = Arc::new(CopilotBackend::with_endpoints_for_tests(
        config.clone(),
        auth,
        models.clone(),
        CopilotEndpoints::default(),
    ));
    let mut state = AppState::with_parts_for_tests(config, models, copilot);
    let cache_dir = repo_tempdir("aa-cache-");
    let cache_path = cache_dir.path().join("cache.json");
    let store = Arc::new(
        ModelStatsStore::with_base_url("k".into(), aa_mock(200).await)
            .with_cache_path(cache_path.clone()),
    );
    state.model_stats = store.clone();
    let body = models_json(state).await;
    assert!(find(&body, "claude-opus-5.5").get("stats").is_none());
    // Seed the cache after the request (a listing syncs entries to listed ids).
    store.sync_models(&["gpt-6.1-sol".to_string()]);
    store.refresh().await;

    // A 200 with only empty entries must not replace the good cache.
    let empty = axum::Router::new().route(
        "/language/models/free",
        axum::routing::get(|| async { axum::Json(serde_json::json!({"data":[{}]})) }),
    );
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(l, empty).await.unwrap() });
    for _ in 0..100 {
        if std::fs::read_to_string(&cache_path).is_ok_and(|t| t.contains("intelligence_index")) {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    // Same cache file, new store pointed at the empty-entry server.
    let empty_store = ModelStatsStore::with_base_url("k".into(), format!("http://{addr}"))
        .with_cache_path(cache_path);
    empty_store.refresh().await;
    let store = Arc::new(empty_store);
    assert!(store.insights_for("gpt-6.1-sol").unwrap().stats.is_some());
}

#[tokio::test]
async fn models_return_promptly_with_cold_cache_and_slow_aa() {
    use axum::routing::get;
    use copilot_proxy_rs::model_stats::ModelStatsStore;

    let slow = axum::Router::new().route(
        "/language/models/free",
        get(|| async {
            tokio::time::sleep(std::time::Duration::from_secs(5)).await;
            axum::Json(serde_json::json!({"data": []}))
        }),
    );
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(l, slow).await.unwrap() });

    let mut state = state_with_no_token().await;
    state
        .models
        .set_copilot_models(vec![copilot_model("claude-opus-5.5")])
        .await;
    state.model_stats = Arc::new(ModelStatsStore::with_base_url(
        "k".into(),
        format!("http://{addr}"),
    ));
    let started = std::time::Instant::now();
    let body = models_json(state).await;
    assert!(started.elapsed() < std::time::Duration::from_secs(2));
    let opus = find(&body, "claude-opus-5.5");
    assert!(opus.get("stats").is_none());
    assert!(opus["description"].is_string());
}

#[tokio::test]
async fn excluded_ids_are_unlisted_but_others_stay() {
    let excluded_ids = [
        "claude-sonnet-5",
        "claude-haiku-4.5",
        "gpt-3.5-turbo",
        "gpt-3.5-turbo-0613",
        "gpt-4",
        "gpt-4-0125-preview",
        "gpt-4-0613",
        "gpt-4-o-preview",
        "gpt-4.1",
        "gpt-4.1-2025-04-14",
        "text-embedding-3-small",
        "text-embedding-3-small-inference",
        "text-embedding-ada-002",
    ];
    let state = state_with_no_token().await;
    state
        .models
        .set_copilot_models(
            excluded_ids
                .iter()
                .copied()
                .chain(["claude-opus-5.5"])
                .map(copilot_model)
                .collect(),
        )
        .await;
    let body = models_json(state).await;
    let ids = |list: &str, key: &str| -> Vec<String> {
        body[list]
            .as_array()
            .unwrap()
            .iter()
            .map(|m| m[key].as_str().unwrap().to_string())
            .collect()
    };
    for (list, key) in [("data", "id"), ("models", "slug")] {
        let got = ids(list, key);
        for id in excluded_ids {
            assert!(!got.iter().any(|listed| listed == id), "{list}: {id}");
        }
        assert!(got.contains(&"claude-opus-5.5".to_string()), "{list}");
    }
}

#[tokio::test]
async fn excluded_ids_never_reach_stats_pipeline_or_cache() {
    use axum::routing::get;
    use copilot_proxy_rs::model_stats::ModelStatsStore;
    use std::sync::atomic::{AtomicUsize, Ordering};

    let calls = Arc::new(AtomicUsize::new(0));
    let counter = calls.clone();
    let app = axum::Router::new().route(
        "/language/models/free",
        get(move || {
            counter.fetch_add(1, Ordering::SeqCst);
            async { axum::Json(serde_json::json!({"data": []})) }
        }),
    );
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(l, app).await.unwrap() });

    // An empty eligible list (all ids excluded) must trigger no AA call.
    let idle = Arc::new(ModelStatsStore::with_base_url(
        "k".into(),
        format!("http://{addr}"),
    ));
    idle.sync_models(&[]);
    idle.spawn_refresh_if_stale(&[]);
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    assert_eq!(calls.load(Ordering::SeqCst), 0);

    let dir = repo_tempdir("aa-excl-");
    let path = dir.path().join("cache.json");
    // Pre-existing cache holding an excluded id must be purged.
    std::fs::write(
        &path,
        r#"{"version":2,"resolved":["gpt-4-0613"],"entries":{"gpt-4-0613":{"stats":null}}}"#,
    )
    .unwrap();
    let mut state = state_with_no_token().await;
    state
        .models
        .set_copilot_models(vec![
            copilot_model("gpt-4-0613"),
            copilot_model("gpt-3.5-turbo"),
        ])
        .await;
    // Only excluded ids are listed by the Copilot side; static models may remain,
    // so check the excluded ids specifically.
    state.model_stats = Arc::new(
        ModelStatsStore::with_base_url("k".into(), format!("http://{addr}"))
            .with_cache_path(path.clone()),
    );
    let body = models_json(state).await;
    assert!(
        body["data"]
            .as_array()
            .unwrap()
            .iter()
            .all(|m| m["id"] != "gpt-4-0613" && m["id"] != "gpt-3.5-turbo")
    );
    for _ in 0..50 {
        if !std::fs::read_to_string(&path)
            .unwrap_or_default()
            .contains("gpt-4-0613")
        {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    let cache = std::fs::read_to_string(&path).unwrap();
    assert!(!cache.contains("gpt-4-0613") && !cache.contains("gpt-3.5-turbo"));
}

#[tokio::test]
async fn excluded_ids_are_hidden_from_listing_but_still_resolvable() {
    let state = state_with_no_token().await;
    state
        .models
        .set_copilot_models(vec![copilot_model("gpt-5.4")])
        .await;
    let body = models_json(state.clone()).await;
    assert!(
        body["data"]
            .as_array()
            .unwrap()
            .iter()
            .all(|m| m["id"] != "gpt-5.4")
    );
    assert!(
        body["models"]
            .as_array()
            .unwrap()
            .iter()
            .all(|m| m["slug"] != "gpt-5.4")
    );
    assert_eq!(
        state.models.get_copilot_openai_model("gpt-5.4").await,
        "gpt-5.4"
    );
}

/// Mock JEV: records the last request body, replies with `reply` and `status`.
async fn jev_mock(status: u16, reply: Value) -> (String, Arc<std::sync::Mutex<Option<Value>>>) {
    use axum::routing::post;
    let seen = Arc::new(std::sync::Mutex::new(None));
    let sink = seen.clone();
    let app = axum::Router::new().route(
        "/v1/systemone",
        post(move |axum::Json(body): axum::Json<Value>| {
            *sink.lock().unwrap() = Some(body);
            let reply = reply.clone();
            async move { (StatusCode::from_u16(status).unwrap(), axum::Json(reply)) }
        }),
    );
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
    (format!("http://{addr}"), seen)
}

async fn router_state(key: &str, jev_url: &str) -> AppState {
    let mut state = state_with_no_token().await;
    let mut config = (*state.config).clone();
    config.typesafe_api_key = copilot_proxy_rs::config::Secret(key.to_string());
    config.jev_base_url = jev_url.to_string();
    state.config = Arc::new(config);
    state
        .models
        .set_copilot_models(vec![
            copilot_model("claude-opus-5.5"),
            copilot_model("gpt-4-0613"),
        ])
        .await;
    state
}

async fn post_models(state: AppState, body: &str) -> (StatusCode, Value) {
    let response = router(state)
        .oneshot(
            Request::post("/v1/models")
                .header("content-type", "application/json")
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

#[tokio::test]
async fn model_router_happy_path_and_excluded_ids_not_offered() {
    let (url, seen) = jev_mock(
        200,
        serde_json::json!({"answers": {"pick": {
            "choice": "claude-opus-5.5",
            "probabilities": {"claude-opus-5.5": 0.8, "ghost-model": 0.9, "gpt-6.1-sol": 0.1},
            "confidence": 0.8
        }}}),
    )
    .await;
    let state = router_state("secret-key", &url).await;
    let (status, body) = post_models(state, r#"{"input":"design a landing page"}"#).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["recommended"]["id"], "claude-opus-5.5");
    assert!(body["recommended"]["description"].is_string());
    assert_eq!(body["confidence"], 0.8);
    assert_eq!(body["jev_model"], "jev-preview");
    let ranking = body["ranking"].as_array().unwrap();
    assert!(ranking.iter().all(|r| r["id"] != "ghost-model"));
    assert_eq!(ranking[0]["id"], "claude-opus-5.5");

    let sent = seen.lock().unwrap().clone().unwrap();
    assert_eq!(sent["model"], "jev-preview");
    assert_eq!(sent["state"], "design a landing page");
    let criteria = sent["questions"]["pick"]["criteria"].as_object().unwrap();
    assert!(criteria.contains_key("claude-opus-5.5"));
    assert!(!criteria.contains_key("gpt-4-0613"), "excluded id offered");
}

#[tokio::test]
async fn model_router_error_paths() {
    let ok_reply = serde_json::json!({"answers": {"pick": {"choice": "claude-opus-5.5"}}});
    // Missing key.
    let (url, _) = jev_mock(200, ok_reply.clone()).await;
    let (status, body) = post_models(router_state("", &url).await, r#"{"input":"x"}"#).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert!(body.to_string().contains("TYPESAFE_API_KEY"));
    // Upstream errors give a generic 502 without the upstream body.
    for code in [429, 500, 400] {
        let (url, _) = jev_mock(code, serde_json::json!({"leak": "upstream-body-text"})).await;
        let (status, body) = post_models(router_state("k", &url).await, r#"{"input":"x"}"#).await;
        assert_eq!(status, StatusCode::BAD_GATEWAY);
        assert!(!body.to_string().contains("upstream-body-text"));
    }
    // Unknown choice.
    let (url, _) = jev_mock(
        200,
        serde_json::json!({"answers": {"pick": {"choice": "not-listed"}}}),
    )
    .await;
    let (status, _) = post_models(router_state("k", &url).await, r#"{"input":"x"}"#).await;
    assert_eq!(status, StatusCode::BAD_GATEWAY);
    // Bad bodies.
    let big = format!(r#"{{"input":"{}"}}"#, "a".repeat(17 * 1024));
    for body in [
        r#"{"input":""}"#,
        r#"{"input":"   "}"#,
        "not json",
        r#"{}"#,
        big.as_str(),
    ] {
        let (status, _) = post_models(router_state("k", &url).await, body).await;
        assert_eq!(
            status,
            StatusCode::BAD_REQUEST,
            "{}",
            &body[..body.len().min(20)]
        );
    }
}

fn pick_reply(choice: &str, probs: Value) -> Value {
    serde_json::json!({"answers": {"pick": {"choice": choice, "probabilities": probs}}})
}

#[tokio::test]
async fn model_router_body_limits_and_decoded_input_cap() {
    let (url, _) = jev_mock(
        200,
        pick_reply(
            "claude-opus-5.5",
            serde_json::json!({"claude-opus-5.5": 0.9}),
        ),
    )
    .await;
    // Raw body over the hard cap is rejected before parsing.
    let huge = format!(r#"{{"input":"{}"}}"#, "a".repeat(200 * 1024));
    let (status, _) = post_models(router_state("k", &url).await, &huge).await;
    assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE);
    // 3,000 escapes decode to 3,000 bytes: allowed although the raw JSON is larger.
    let escaped = format!(r#"{{"input":"{}"}}"#, "\\u0061".repeat(3000));
    let (status, _) = post_models(router_state("k", &url).await, &escaped).await;
    assert_eq!(status, StatusCode::OK);
    // Decoded text over 16 KiB is a 400.
    let big = format!(r#"{{"input":"{}"}}"#, "a".repeat(17 * 1024));
    let (status, _) = post_models(router_state("k", &url).await, &big).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn model_router_validates_probabilities_and_keeps_true_confidence() {
    // Invalid chosen probability -> 502.
    let (url, _) = jev_mock(
        200,
        pick_reply(
            "claude-opus-5.5",
            serde_json::json!({"claude-opus-5.5": 7.0}),
        ),
    )
    .await;
    let (status, _) = post_models(router_state("k", &url).await, r#"{"input":"x"}"#).await;
    assert_eq!(status, StatusCode::BAD_GATEWAY);
    // Chosen id is 6th by probability; confidence keeps its true value and
    // other invalid entries are dropped.
    let state = router_state("k", "http://unused").await;
    let mut probs = serde_json::Map::new();
    for (i, id) in [
        "gpt-6.1-sol",
        "gpt-6-astra",
        "gpt-6-luna",
        "grok-4.7",
        "gemini-4-argon",
    ]
    .iter()
    .enumerate()
    {
        probs.insert(id.to_string(), serde_json::json!(0.5 - i as f64 * 0.01));
    }
    probs.insert("claude-opus-5.5".into(), serde_json::json!(0.1));
    probs.insert("gpt-5.6-sol-fast".into(), serde_json::json!(-3.0));
    let (url, _) = jev_mock(
        200,
        pick_reply("claude-opus-5.5", Value::Object(probs.clone())),
    )
    .await;
    drop(state);
    // These ids must be listed to count; list them via copilot models.
    let state = router_state("k", &url).await;
    state
        .models
        .set_copilot_models(
            [
                "claude-opus-5.5",
                "gpt-6.1-sol",
                "gpt-6-astra",
                "gpt-6-luna",
                "grok-4.7",
                "gemini-4-argon",
                "gpt-5.6-sol-fast",
            ]
            .iter()
            .map(|id| copilot_model(id))
            .collect(),
        )
        .await;
    let (status, body) = post_models(state, r#"{"input":"x"}"#).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["confidence"], 0.1);
    let ranking = body["ranking"].as_array().unwrap();
    assert_eq!(ranking.len(), 5);
    assert!(ranking.iter().all(|r| r["id"] != "gpt-5.6-sol-fast"));
}

#[tokio::test]
async fn model_router_upstream_cap_redirect_and_timeout() {
    use axum::routing::post;
    // Oversized upstream body -> generic 502.
    let big = serde_json::json!({"pad": "x".repeat(300 * 1024)});
    let (url, _) = jev_mock(200, big).await;
    let (status, _) = post_models(router_state("k", &url).await, r#"{"input":"x"}"#).await;
    assert_eq!(status, StatusCode::BAD_GATEWAY);

    // Redirect is not followed (key must not reach the target).
    let hit = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let h = hit.clone();
    let target = axum::Router::new().route(
        "/steal",
        post(move |headers: http::HeaderMap| {
            let h = h.clone();
            async move {
                if headers.contains_key("authorization") {
                    h.store(true, std::sync::atomic::Ordering::SeqCst);
                }
                "x"
            }
        }),
    );
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let taddr = l.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(l, target).await.unwrap() });
    let redirect = axum::Router::new().route(
        "/v1/systemone",
        post(move || async move {
            (
                StatusCode::TEMPORARY_REDIRECT,
                [("location", format!("http://{taddr}/steal"))],
            )
        }),
    );
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let raddr = l.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(l, redirect).await.unwrap() });
    let (status, _) = post_models(
        router_state("k", &format!("http://{raddr}")).await,
        r#"{"input":"x"}"#,
    )
    .await;
    assert_eq!(status, StatusCode::BAD_GATEWAY);
    assert!(!hit.load(std::sync::atomic::Ordering::SeqCst));

    // Timeout (paused clock would not affect reqwest; use a short real stall).
    let slow = axum::Router::new().route(
        "/v1/systemone",
        post(|| async {
            tokio::time::sleep(std::time::Duration::from_secs(30)).await;
            "x"
        }),
    );
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let saddr = l.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(l, slow).await.unwrap() });
    let started = std::time::Instant::now();
    let (status, _) = post_models(
        router_state("k", &format!("http://{saddr}")).await,
        r#"{"input":"x"}"#,
    )
    .await;
    assert_eq!(status, StatusCode::BAD_GATEWAY);
    assert!(started.elapsed() < std::time::Duration::from_secs(25));
}

#[tokio::test]
async fn model_router_requires_same_inbound_auth_as_get() {
    let (url, _) = jev_mock(
        200,
        pick_reply(
            "claude-opus-5.5",
            serde_json::json!({"claude-opus-5.5": 0.9}),
        ),
    )
    .await;
    let mut state = router_state("k", &url).await;
    let mut config = (*state.config).clone();
    config.api_key = "local-secret".to_string();
    state.config = Arc::new(config);
    let send = |auth: Option<&'static str>| {
        let state = state.clone();
        async move {
            let mut req = Request::post("/v1/models").header("content-type", "application/json");
            if let Some(a) = auth {
                req = req.header("authorization", a);
            }
            router(state)
                .oneshot(req.body(Body::from(r#"{"input":"x"}"#)).unwrap())
                .await
                .unwrap()
                .status()
        }
    };
    assert_eq!(send(None).await, StatusCode::UNAUTHORIZED);
    assert_eq!(send(Some("Bearer wrong")).await, StatusCode::UNAUTHORIZED);
    assert_eq!(send(Some("Bearer local-secret")).await, StatusCode::OK);
}

#[tokio::test]
async fn model_router_logs_never_contain_key_or_input() {
    use support::log_capture::with_event_capture;
    let (ok_url, _) = jev_mock(
        200,
        pick_reply(
            "claude-opus-5.5",
            serde_json::json!({"claude-opus-5.5": 0.9}),
        ),
    )
    .await;
    let (bad_url, _) = jev_mock(500, serde_json::json!({})).await;
    let (unlisted_url, _) = jev_mock(200, pick_reply("not-listed", serde_json::json!({}))).await;
    let events = with_event_capture(|| async {
        for url in [&ok_url, &bad_url, &unlisted_url] {
            let _ = post_models(
                router_state("sekrit-ts-key", url).await,
                r#"{"input":"unique-input-marker-123"}"#,
            )
            .await;
        }
    })
    .await;
    assert!(!events.is_empty(), "expected warning events");
    let dump = format!("{events:?}");
    assert!(!dump.contains("sekrit-ts-key") && !dump.contains("unique-input-marker-123"));
}

#[tokio::test]
async fn latest_opus_gets_ui_strength_in_listing_and_router_criteria() {
    let (url, seen) = jev_mock(
        200,
        pick_reply(
            "claude-opus-5.6",
            serde_json::json!({"claude-opus-5.6": 0.9}),
        ),
    )
    .await;
    let state = router_state("k", &url).await;
    state
        .models
        .set_copilot_models(vec![
            copilot_model("claude-opus-5.5"),
            copilot_model("claude-opus-5.6"),
            copilot_model("claude-haiku-5.5"),
        ])
        .await;
    let listing = models_json(state.clone()).await;
    let tags = |id: &str| find(&listing, id)["excels_at"].to_string();
    assert!(tags("claude-opus-5.6").contains("ui-design"));
    assert!(tags("claude-opus-5.6").contains("visual-work"));
    assert!(!tags("claude-opus-5.5").contains("ui-design"));
    assert!(!tags("claude-haiku-5.5").contains("ui-design"));
    assert!(
        find(&listing, "claude-opus-5.6")["description"]
            .as_str()
            .unwrap()
            .ends_with("Preferred for UI and visual work.")
    );

    let (status, _) = post_models(state, r#"{"input":"build a CSS layout"}"#).await;
    assert_eq!(status, StatusCode::OK);
    let sent = seen.lock().unwrap().clone().unwrap();
    let criteria = &sent["questions"]["pick"]["criteria"];
    assert!(
        criteria["claude-opus-5.6"]
            .as_str()
            .unwrap()
            .contains("ui-design")
    );
    assert!(
        !criteria["claude-opus-5.5"]
            .as_str()
            .unwrap()
            .contains("ui-design")
    );
    assert!(
        !criteria["claude-haiku-5.5"]
            .as_str()
            .unwrap()
            .contains("ui-design")
    );
    assert!(
        sent["questions"]["pick"]["instructions"]
            .as_str()
            .unwrap()
            .contains("latest Opus")
    );
}

async fn ui_rule_run(ui: Value, listed: &[&str], pick: &str) -> Value {
    let reply = serde_json::json!({"answers": {
        "pick": {"choice": pick, "probabilities": {pick: 0.7}},
        "is_ui_or_visual": ui,
    }});
    let (url, seen) = jev_mock(200, reply).await;
    let state = router_state("k", &url).await;
    state
        .models
        .set_copilot_models(listed.iter().map(|id| copilot_model(id)).collect())
        .await;
    let (status, body) = post_models(state, r#"{"input":"x"}"#).await;
    assert_eq!(status, StatusCode::OK);
    let sent = seen.lock().unwrap().clone().unwrap();
    assert!(sent["questions"]["is_ui_or_visual"]["criteria"]["true"].is_string());
    body
}

#[tokio::test]
async fn ui_rule_is_enforced_only_when_ui_true_and_opus_listed() {
    let ui_true = serde_json::json!({"choice":"true","probabilities":{"true":0.9,"false":0.1}});
    let ui_false = serde_json::json!({"choice":"false","probabilities":{"true":0.1,"false":0.9}});
    let list = [
        "claude-opus-5.5",
        "claude-opus-5.10",
        "claude-opus-5.9",
        "claude-haiku-5.5",
    ];
    let b = ui_rule_run(ui_true.clone(), &list, "claude-haiku-5.5").await;
    assert_eq!(b["recommended"]["id"], "claude-opus-5.10");
    assert_eq!(b["routed_by"], "ui-rule");
    let b = ui_rule_run(ui_false, &list, "claude-haiku-5.5").await;
    assert_eq!(b["recommended"]["id"], "claude-haiku-5.5");
    assert_eq!(b["routed_by"], "jev");
    let b = ui_rule_run(
        ui_true,
        &["claude-haiku-5.5", "grok-4.7"],
        "claude-haiku-5.5",
    )
    .await;
    assert_eq!(b["recommended"]["id"], "claude-haiku-5.5");
    assert_eq!(b["routed_by"], "jev");
}
