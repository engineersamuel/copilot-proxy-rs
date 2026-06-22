use std::sync::{Arc, Mutex};

use axum::Json;
use http::StatusCode;
use serde_json::Value;
use tracing_subscriber::layer::SubscriberExt;

use copilot_proxy_rs::config::AppConfig;
use copilot_proxy_rs::errors::{anthropic_error, openai_error};
use copilot_proxy_rs::responses::state::{
    ResponsesStateEntry, ResponsesStateStore, ResponsesTurnIdentity,
};
use copilot_proxy_rs::state::{AppState, BackendKind};

// --- Warning-capture helpers ---

struct WarnLayer {
    messages: Arc<Mutex<Vec<String>>>,
}

struct MessageVisitor(String);

impl tracing::field::Visit for MessageVisitor {
    fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
        if field.name() == "message" {
            self.0 = value.to_string();
        }
    }

    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
        if field.name() == "message" {
            self.0 = format!("{value:?}");
        }
    }
}

impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for WarnLayer {
    fn on_event(
        &self,
        event: &tracing::Event<'_>,
        _ctx: tracing_subscriber::layer::Context<'_, S>,
    ) {
        if *event.metadata().level() == tracing::Level::WARN {
            let mut v = MessageVisitor(String::new());
            event.record(&mut v);
            self.messages.lock().unwrap().push(v.0);
        }
    }
}

fn with_warn_capture<F: FnOnce()>(f: F) -> Vec<String> {
    let messages = Arc::new(Mutex::new(Vec::<String>::new()));
    let layer = WarnLayer {
        messages: messages.clone(),
    };
    let subscriber = tracing_subscriber::registry().with(layer);
    tracing::subscriber::with_default(subscriber, f);
    Arc::try_unwrap(messages).unwrap().into_inner().unwrap()
}

#[tokio::test]
async fn backend_state_snapshots_primary_and_fallback() {
    let config = AppConfig {
        backend: "bedrock".to_string(),
        fallback_backend: "copilot".to_string(),
        ..AppConfig::default()
    };
    let state = AppState::new(config);

    let snapshot = state.backend.snapshot().await;

    assert_eq!(snapshot.primary, BackendKind::Bedrock);
    assert_eq!(snapshot.fallback, Some(BackendKind::Copilot));
}

#[tokio::test]
async fn runtime_switch_affects_new_snapshots_only() {
    let state = AppState::new(AppConfig::default());
    let before = state.backend.snapshot().await;

    state
        .backend
        .set(BackendKind::Bedrock, Some(BackendKind::Copilot))
        .await;
    let after = state.backend.snapshot().await;

    assert_eq!(before.primary, BackendKind::Copilot);
    assert_eq!(before.fallback, None);
    assert_eq!(after.primary, BackendKind::Bedrock);
    assert_eq!(after.fallback, Some(BackendKind::Copilot));
}

#[test]
fn anthropic_error_shape_matches_python() {
    let (status, Json(body)) = anthropic_error(
        StatusCode::BAD_REQUEST,
        "invalid_request_error",
        "bad request",
    );
    let body = serde_json::to_value(body).unwrap();

    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["type"], "error");
    assert_eq!(body["error"]["type"], "invalid_request_error");
    assert_eq!(body["error"]["message"], "bad request");
}

#[test]
fn openai_error_shape_matches_python() {
    let (status, Json(body)) = openai_error(
        StatusCode::BAD_REQUEST,
        "invalid_request_error",
        "bad request",
    );
    let body: Value = serde_json::to_value(body).unwrap();

    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["error"]["message"], "bad request");
    assert_eq!(body["error"]["type"], "invalid_request_error");
    assert!(body["error"]["param"].is_null());
    assert!(body["error"]["code"].is_null());
}

#[test]
fn warn_on_unrecognized_primary_backend() {
    let warnings = with_warn_capture(|| {
        let config = AppConfig {
            backend: "bedroc".to_string(),
            ..AppConfig::default()
        };
        let _state = AppState::new(config);
    });

    assert!(
        warnings
            .iter()
            .any(|m| m.contains("bedroc") || m.contains("unrecognized")),
        "expected a WARN mentioning the bad backend value; got: {warnings:?}"
    );
}

#[test]
fn warn_on_unrecognized_fallback_backend() {
    let warnings = with_warn_capture(|| {
        let config = AppConfig {
            backend: "copilot".to_string(),
            fallback_backend: "bedrok".to_string(),
            ..AppConfig::default()
        };
        let _state = AppState::new(config);
    });

    assert!(
        warnings
            .iter()
            .any(|m| m.contains("bedrok") || m.contains("unrecognized")),
        "expected a WARN mentioning the bad fallback value; got: {warnings:?}"
    );
}

#[tokio::test]
async fn responses_state_cache_is_lru_and_copies_transcripts() {
    let store = ResponsesStateStore::new(2, 3);
    let identity = ResponsesTurnIdentity {
        interaction_id: "interaction-1".to_string(),
        agent_task_id: "agent-1".to_string(),
    };

    store
        .cache_response_state(
            "resp_1",
            vec![serde_json::json!({"role":"user","content":"a"})],
            vec![serde_json::json!({"role":"assistant","content":"b"})],
            identity.clone(),
            false,
        )
        .await;
    store
        .cache_response_state(
            "resp_2",
            vec![serde_json::json!({"role":"user","content":"c"})],
            vec![serde_json::json!({"role":"assistant","content":"d"})],
            identity.clone(),
            true,
        )
        .await;
    store
        .cache_response_state(
            "resp_3",
            vec![serde_json::json!({"role":"user","content":"e"})],
            vec![serde_json::json!({"role":"assistant","content":"f"})],
            identity,
            false,
        )
        .await;

    assert!(store.get_cached_response_state("resp_1").await.is_none());
    let entry: ResponsesStateEntry = store.get_cached_response_state("resp_2").await.unwrap();
    assert!(entry.last_response_had_tool_calls);
    assert_eq!(entry.transcript.len(), 2);
    assert_eq!(entry.identity.agent_task_id, "agent-1");
}
