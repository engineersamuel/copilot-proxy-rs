mod support;

use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use futures_util::{SinkExt, StreamExt};
use tokio::net::TcpListener;
use tokio_tungstenite::{connect_async, tungstenite::Message};

use copilot_proxy_rs::auth::CopilotAuth;
use copilot_proxy_rs::config::{AppConfig, EnvSource, LocalModelConfig};
use copilot_proxy_rs::copilot::client::{CopilotBackend, CopilotEndpoints};
use copilot_proxy_rs::http::router;
use copilot_proxy_rs::local::LocalBackend;
use copilot_proxy_rs::models::ModelRegistry;
use copilot_proxy_rs::state::AppState;

async fn start_proxy(state: AppState) -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, router(state)).await.unwrap();
    });
    addr
}

async fn start_proxy_with_config(config: AppConfig) -> SocketAddr {
    let fixture = support::AppFixture::with_mock_copilot().await;
    let state = AppState {
        config: Arc::new(config),
        backend: fixture.state.backend,
        models: fixture.state.models,
        copilot: fixture.state.copilot,
        local: Arc::new(LocalBackend::new()),
        responses: fixture.state.responses,
    };
    start_proxy(state).await
}

async fn mock_ws_backend_addr() -> (SocketAddr, Arc<AtomicUsize>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let open_count = Arc::new(AtomicUsize::new(0));
    let handler_open_count = open_count.clone();
    tokio::spawn(async move {
        let app = axum::Router::new()
            .route("/responses", axum::routing::get(backend_ws_handler))
            .with_state(handler_open_count);
        axum::serve(listener, app).await.unwrap();
    });
    (addr, open_count)
}

async fn backend_ws_handler(
    axum::extract::State(open_count): axum::extract::State<Arc<AtomicUsize>>,
    ws: axum::extract::WebSocketUpgrade,
) -> axum::response::Response {
    open_count.fetch_add(1, Ordering::SeqCst);
    ws.on_upgrade(|mut socket| async move {
        use axum::extract::ws::Message as AxMessage;
        if let Some(Ok(message)) = socket.recv().await {
            let text = match message {
                AxMessage::Text(text) => text.to_string(),
                _ => String::new(),
            };
            let parsed: serde_json::Value =
                serde_json::from_str(&text).unwrap_or_else(|_| serde_json::json!({}));
            if parsed["model"] == "gpt-ws-effort" {
                assert_eq!(parsed["reasoning"]["effort"], "high");
                assert_eq!(parsed["reasoning"]["summary"], "auto");
            }
            let _ = socket
                .send(AxMessage::Text(
                    serde_json::json!({
                        "type": "response.completed",
                        "response": {
                            "id": "resp_backend_1",
                            "object": "response",
                            "status": "completed",
                            "output": []
                        }
                    })
                    .to_string()
                    .into(),
                ))
                .await;
        }
    })
}

async fn state_with_ws_backend(
    ws_backend_addr: SocketAddr,
) -> (AppState, tempfile::TempDir, support::MockServer) {
    let mock = support::MockServer::start().await;
    mock.respond_json(
        "GET",
        "/copilot/token",
        200,
        serde_json::json!({
            "token": "copilot-token",
            "expires_at": 4_102_444_800u64
        }),
    )
    .await;

    let temp = tempfile::Builder::new()
        .prefix("ws-test-")
        .tempdir_in(env!("CARGO_MANIFEST_DIR"))
        .unwrap();
    std::fs::write(temp.path().join("github_token"), "github-token").unwrap();
    let env =
        EnvSource::from_pairs([("COPILOT_PROXY_RS_CONFIG_DIR", temp.path().to_str().unwrap())]);
    let mut config = AppConfig::load_from_env(&env).unwrap();
    config.local_models.insert(
        "qwen3-coder-30b-local".to_string(),
        LocalModelConfig {
            base_url: "http://127.0.0.1:1/v1".to_string(),
            upstream_model: r"models\Qwen3-Coder-30B-A3B-Instruct-IQ4_XS.gguf".to_string(),
        },
    );
    let config = Arc::new(config);
    let auth = Arc::new(CopilotAuth::with_env_for_tests(
        config.clone(),
        env,
        mock.auth_endpoints(),
        false,
    ));
    let models = Arc::new(ModelRegistry::with_models(
        config.model_overrides.copilot.clone(),
        config.local_models.clone(),
    ));
    let endpoints = CopilotEndpoints {
        responses_ws_url: format!("ws://{}/responses", ws_backend_addr),
        ..mock.copilot_endpoints()
    };
    let copilot = Arc::new(CopilotBackend::with_endpoints_for_tests(
        config.clone(),
        auth,
        models.clone(),
        endpoints,
    ));
    let state = AppState::with_parts_for_tests(config, models, copilot);
    (state, temp, mock)
}

// ─── Tests ────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn responses_websocket_rejects_missing_inbound_auth_when_configured() {
    let fixture = support::AppFixture::with_mock_copilot().await;
    let mut config = (*fixture.state.config).clone();
    config.api_key = "local-secret".to_string();
    let addr = start_proxy_with_config(config).await;

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
    config.allowed_origins = vec!["https://trusted.example".to_string()];
    let addr = start_proxy_with_config(config).await;

    let mut request = format!("ws://{addr}/v1/responses")
        .into_client_request()
        .unwrap();
    request
        .headers_mut()
        .insert("authorization", "Bearer local-secret".parse().unwrap());
    request
        .headers_mut()
        .insert("origin", "https://trusted.example.evil".parse().unwrap());

    let err = connect_async(request).await.unwrap_err();

    assert!(
        err.to_string().contains("HTTP error"),
        "expected rejected websocket upgrade, got {err}"
    );
}

#[tokio::test]
async fn responses_websocket_rejects_origin_when_allowlist_is_empty() {
    use tokio_tungstenite::tungstenite::client::IntoClientRequest;

    let addr = start_proxy_with_config(AppConfig::default()).await;
    let mut request = format!("ws://{addr}/v1/responses")
        .into_client_request()
        .unwrap();
    request
        .headers_mut()
        .insert("origin", "https://evil.example".parse().unwrap());

    let err = connect_async(request).await.unwrap_err();
    let tokio_tungstenite::tungstenite::Error::Http(response) = err else {
        panic!("expected rejected websocket upgrade");
    };

    assert_eq!(response.status(), http::StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn responses_websocket_allows_missing_origin_when_allowlist_is_empty() {
    let addr = start_proxy_with_config(AppConfig::default()).await;
    let url = format!("ws://{addr}/v1/responses");

    let (_, response) = connect_async(&url).await.unwrap();

    assert_eq!(response.status(), http::StatusCode::SWITCHING_PROTOCOLS);
}

#[tokio::test]
async fn responses_websocket_allows_missing_origin_when_allowlist_is_configured() {
    let config = AppConfig {
        allowed_origins: vec!["https://trusted.example".to_string()],
        ..AppConfig::default()
    };
    let addr = start_proxy_with_config(config).await;
    let url = format!("ws://{addr}/v1/responses");

    let (_, response) = connect_async(&url).await.unwrap();

    assert_eq!(response.status(), http::StatusCode::SWITCHING_PROTOCOLS);
}

#[tokio::test]
async fn responses_websocket_allows_exact_matching_origin() {
    use tokio_tungstenite::tungstenite::client::IntoClientRequest;

    let config = AppConfig {
        allowed_origins: vec!["https://trusted.example".to_string()],
        ..AppConfig::default()
    };
    let addr = start_proxy_with_config(config).await;
    let mut request = format!("ws://{addr}/v1/responses")
        .into_client_request()
        .unwrap();
    request
        .headers_mut()
        .insert("origin", "https://trusted.example".parse().unwrap());

    let (_, response) = connect_async(request).await.unwrap();

    assert_eq!(response.status(), http::StatusCode::SWITCHING_PROTOCOLS);
}

#[tokio::test]
async fn responses_websocket_rejects_invalid_json_with_error_frame() {
    let fixture = support::AppFixture::with_mock_copilot().await;
    let addr = start_proxy(fixture.state).await;

    let url = format!("ws://{addr}/v1/responses");
    let (mut ws, _) = connect_async(&url).await.unwrap();

    ws.send(Message::Text("not-json".to_string().into()))
        .await
        .unwrap();

    let msg = ws.next().await.unwrap().unwrap();
    let text = match msg {
        Message::Text(t) => t.to_string(),
        other => panic!("expected text frame, got {other:?}"),
    };

    assert!(
        text.contains(r#""type":"error""#),
        "expected type:error in: {text}"
    );
    assert!(
        text.contains("invalid_request_error"),
        "expected invalid_request_error in: {text}"
    );
}

#[tokio::test]
async fn responses_websocket_rejects_local_model_without_copilot_work() {
    use tokio_tungstenite::tungstenite::client::IntoClientRequest;

    let mut fixture = support::AppFixture::with_mock_local().await;
    Arc::make_mut(&mut fixture.state.config).api_key = "local-secret".to_string();
    let addr = start_proxy(fixture.state.clone()).await;
    let mut request = format!("ws://{addr}/v1/responses")
        .into_client_request()
        .unwrap();
    request
        .headers_mut()
        .insert("authorization", "Bearer local-secret".parse().unwrap());
    let (mut ws, response) = connect_async(request).await.unwrap();
    assert_eq!(response.status(), http::StatusCode::SWITCHING_PROTOCOLS);

    ws.send(Message::Text(
        serde_json::json!({
            "type": "response.create",
            "model": "qwen3-coder-30b-local",
            "input": "hello"
        })
        .to_string()
        .into(),
    ))
    .await
    .unwrap();

    let message = ws.next().await.unwrap().unwrap();
    let Message::Text(text) = message else {
        panic!("expected text frame, got {message:?}");
    };
    let event: serde_json::Value = serde_json::from_str(&text).unwrap();
    assert_eq!(event["type"], "error");
    assert_eq!(event["error"]["type"], "invalid_request_error");
    assert_eq!(
        event["error"]["message"],
        "Local models do not support Responses WebSocket"
    );
    assert_eq!(fixture.mock.hits("GET", "/responses").await, 0);
    assert_eq!(fixture.mock.hits("GET", "/models").await, 0);
    assert_eq!(fixture.mock.hits("GET", "/copilot/token").await, 0);
}

#[tokio::test]
async fn responses_websocket_rejects_bare_prefixed_local_model_and_keeps_socket_usable() {
    use tokio_tungstenite::tungstenite::client::IntoClientRequest;

    let (ws_backend_addr, backend_open_count) = mock_ws_backend_addr().await;
    let (mut state, _temp, mock) = state_with_ws_backend(ws_backend_addr).await;
    Arc::make_mut(&mut state.config).api_key = "local-secret".to_string();
    let addr = start_proxy(state).await;
    let mut request = format!("ws://{addr}/v1/responses")
        .into_client_request()
        .unwrap();
    request
        .headers_mut()
        .insert("authorization", "Bearer local-secret".parse().unwrap());
    let (mut ws, _) = connect_async(request).await.unwrap();

    ws.send(Message::Text(
        serde_json::json!({
            "model": "github-copilot/qwen3-coder-30b-local",
            "input": "private local input"
        })
        .to_string()
        .into(),
    ))
    .await
    .unwrap();

    let message = ws.next().await.unwrap().unwrap();
    let Message::Text(text) = message else {
        panic!("expected text frame, got {message:?}");
    };
    let event: serde_json::Value = serde_json::from_str(&text).unwrap();
    assert_eq!(event["type"], "error");
    assert_eq!(event["error"]["type"], "invalid_request_error");
    assert_eq!(
        event["error"]["message"],
        "Local models do not support Responses WebSocket"
    );
    assert_eq!(backend_open_count.load(Ordering::SeqCst), 0);
    assert_eq!(mock.hits("GET", "/models").await, 0);
    assert_eq!(mock.hits("GET", "/copilot/token").await, 0);

    ws.send(Message::Text(
        serde_json::json!({
            "model": "gpt-5.5",
            "input": "non-local input"
        })
        .to_string()
        .into(),
    ))
    .await
    .unwrap();
    let message = ws.next().await.unwrap().unwrap();
    let Message::Text(text) = message else {
        panic!("expected text frame, got {message:?}");
    };
    let event: serde_json::Value = serde_json::from_str(&text).unwrap();
    assert_eq!(event["type"], "response.completed");
    assert_eq!(backend_open_count.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn responses_websocket_rejects_malformed_local_event_types_without_copilot_work() {
    use tokio_tungstenite::tungstenite::client::IntoClientRequest;

    let (ws_backend_addr, backend_open_count) = mock_ws_backend_addr().await;
    let (mut state, _temp, mock) = state_with_ws_backend(ws_backend_addr).await;
    Arc::make_mut(&mut state.config).api_key = "local-secret".to_string();
    let addr = start_proxy(state).await;
    let mut request = format!("ws://{addr}/v1/responses")
        .into_client_request()
        .unwrap();
    request
        .headers_mut()
        .insert("authorization", "Bearer local-secret".parse().unwrap());
    let (mut ws, _) = connect_async(request).await.unwrap();

    for (event_type, model) in [
        (serde_json::Value::Null, "qwen3-coder-30b-local"),
        (serde_json::json!(7), "github-copilot/qwen3-coder-30b-local"),
        (
            serde_json::json!({"unexpected": true}),
            "qwen3-coder-30b-local",
        ),
        (
            serde_json::json!("response.cancel"),
            "github-copilot/qwen3-coder-30b-local",
        ),
    ] {
        ws.send(Message::Text(
            serde_json::json!({
                "type": event_type,
                "model": model,
                "input": "private local input"
            })
            .to_string()
            .into(),
        ))
        .await
        .unwrap();

        let message = ws.next().await.unwrap().unwrap();
        let Message::Text(text) = message else {
            panic!("expected text frame, got {message:?}");
        };
        let event: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(event["type"], "error");
        assert_eq!(event["error"]["type"], "invalid_request_error");
        assert_eq!(
            event["error"]["message"],
            "Local models do not support Responses WebSocket"
        );
        assert_eq!(backend_open_count.load(Ordering::SeqCst), 0);
        assert_eq!(mock.hits("GET", "/models").await, 0);
        assert_eq!(mock.hits("GET", "/copilot/token").await, 0);
    }

    ws.send(Message::Text(
        serde_json::json!({
            "type": "response.create",
            "model": "gpt-5.5",
            "input": "non-local input"
        })
        .to_string()
        .into(),
    ))
    .await
    .unwrap();
    let message = ws.next().await.unwrap().unwrap();
    let Message::Text(text) = message else {
        panic!("expected text frame, got {message:?}");
    };
    let event: serde_json::Value = serde_json::from_str(&text).unwrap();
    assert_eq!(event["type"], "response.completed");
    assert_eq!(backend_open_count.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn responses_websocket_prewarm_generate_false_returns_created_and_completed() {
    let fixture = support::AppFixture::with_mock_copilot().await;
    let addr = start_proxy(fixture.state).await;

    let url = format!("ws://{addr}/v1/responses");
    let (mut ws, _) = connect_async(&url).await.unwrap();

    ws.send(Message::Text(
        serde_json::json!({
            "type": "response.create",
            "model": "gpt-5.5",
            "input": "warmup",
            "generate": false
        })
        .to_string()
        .into(),
    ))
    .await
    .unwrap();

    let first = ws.next().await.unwrap().unwrap();
    let first_text = match first {
        Message::Text(t) => t.to_string(),
        other => panic!("expected text frame, got {other:?}"),
    };
    assert!(
        first_text.contains(r#""type":"response.created""#),
        "first frame should be response.created, got: {first_text}"
    );

    let second = ws.next().await.unwrap().unwrap();
    let second_text = match second {
        Message::Text(t) => t.to_string(),
        other => panic!("expected text frame, got {other:?}"),
    };
    assert!(
        second_text.contains(r#""type":"response.completed""#),
        "second frame should be response.completed, got: {second_text}"
    );
}

#[tokio::test]
async fn responses_websocket_forwards_prepared_effort_adapted_body() {
    let (ws_backend_addr, _backend_open_count) = mock_ws_backend_addr().await;
    let (state, _temp, _mock) = state_with_ws_backend(ws_backend_addr).await;
    state
        .models
        .set_copilot_models(vec![serde_json::json!({
            "id": "gpt-ws-effort",
            "owned_by": "openai",
            "supported_endpoints": ["ws:/responses"],
            "capabilities": {"supports": {"reasoning_effort": ["low", "medium", "high"]}}
        })])
        .await;
    let addr = start_proxy(state).await;

    let url = format!("ws://{addr}/v1/responses");
    let (mut ws, _) = connect_async(&url).await.unwrap();

    ws.send(Message::Text(
        serde_json::json!({
            "model": "gpt-ws-effort",
            "input": "hello",
            "reasoning": {"effort": "max", "summary": "auto"}
        })
        .to_string()
        .into(),
    ))
    .await
    .unwrap();

    let msg = ws.next().await.unwrap().unwrap();
    let text = match msg {
        Message::Text(t) => t.to_string(),
        other => panic!("expected text frame, got {other:?}"),
    };
    assert!(
        text.contains(r#""type":"response.completed""#),
        "expected response.completed from bridge, got: {text}"
    );
}

#[tokio::test]
async fn responses_websocket_bridges_backend_completed_event() {
    let (ws_backend_addr, _backend_open_count) = mock_ws_backend_addr().await;
    let (state, _temp, _mock) = state_with_ws_backend(ws_backend_addr).await;
    let addr = start_proxy(state).await;

    let url = format!("ws://{addr}/v1/responses");
    let (mut ws, _) = connect_async(&url).await.unwrap();

    ws.send(Message::Text(
        serde_json::json!({
            "model": "gpt-5.5",
            "input": "hello"
        })
        .to_string()
        .into(),
    ))
    .await
    .unwrap();

    let msg = ws.next().await.unwrap().unwrap();
    let text = match msg {
        Message::Text(t) => t.to_string(),
        other => panic!("expected text frame, got {other:?}"),
    };
    assert!(
        text.contains(r#""type":"response.completed""#),
        "expected response.completed from bridge, got: {text}"
    );
}
