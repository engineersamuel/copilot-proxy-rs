mod support;

use std::net::SocketAddr;
use std::sync::Arc;

use futures_util::{SinkExt, StreamExt};
use tokio::net::TcpListener;
use tokio_tungstenite::{connect_async, tungstenite::Message};

use copilot_proxy_rs::auth::CopilotAuth;
use copilot_proxy_rs::config::{AppConfig, EnvSource};
use copilot_proxy_rs::copilot::client::{CopilotBackend, CopilotEndpoints};
use copilot_proxy_rs::http::router;
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

async fn mock_ws_backend_addr() -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let app = axum::Router::new().route("/responses", axum::routing::get(backend_ws_handler));
        axum::serve(listener, app).await.unwrap();
    });
    addr
}

async fn backend_ws_handler(ws: axum::extract::WebSocketUpgrade) -> axum::response::Response {
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

async fn state_with_ws_backend(ws_backend_addr: SocketAddr) -> (AppState, tempfile::TempDir) {
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
    let config = Arc::new(AppConfig::load_from_env(&env).unwrap());
    let auth = Arc::new(CopilotAuth::with_env_for_tests(
        config.clone(),
        env,
        mock.auth_endpoints(),
        false,
    ));
    let models = Arc::new(ModelRegistry::new());
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
    (state, temp)
}

// ─── Tests ────────────────────────────────────────────────────────────────────

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
    let ws_backend_addr = mock_ws_backend_addr().await;
    let (state, _temp) = state_with_ws_backend(ws_backend_addr).await;
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
    let ws_backend_addr = mock_ws_backend_addr().await;
    let (state, _temp) = state_with_ws_backend(ws_backend_addr).await;
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
