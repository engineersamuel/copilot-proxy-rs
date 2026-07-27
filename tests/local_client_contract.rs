mod support;

use std::time::Duration;

use copilot_proxy_rs::local::{LocalBackend, LocalModelError};
use copilot_proxy_rs::models::LocalModelTarget;

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
            "choices": [{
                "index": 0,
                "message": {"role": "assistant", "content": "hello"},
                "finish_reason": "stop"
            }],
            "usage": {"prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2}
        }),
    )
    .await;
    let target = LocalModelTarget {
        public_id: "qwen3-coder-30b-local".to_string(),
        base_url: format!("{}/v1", mock.base_url),
        upstream_model: r"models\Qwen3-Coder-30B-A3B-Instruct-IQ4_XS.gguf".to_string(),
    };
    let body = serde_json::json!({"messages": [{"role": "user", "content": "hello"}]})
        .as_object()
        .expect("test chat body must be an object")
        .clone();

    let response = LocalBackend::new()
        .post_chat(&target, body)
        .await
        .expect("local backend request must succeed");

    assert_eq!(response["choices"][0]["message"]["content"], "hello");
    let outbound = mock
        .last_request_body_json("POST", "/v1/chat/completions")
        .await
        .expect("mock must record the local backend request");
    assert_eq!(
        outbound["model"],
        r"models\Qwen3-Coder-30B-A3B-Instruct-IQ4_XS.gguf"
    );
}

#[tokio::test]
async fn invalid_success_json_returns_invalid_json_error() {
    let mock = support::MockServer::start().await;
    mock.respond_sse("POST", "/v1/chat/completions", 200, vec!["not-json"])
        .await;
    let target = LocalModelTarget {
        public_id: "local-model".to_string(),
        base_url: format!("{}/v1", mock.base_url),
        upstream_model: "upstream-model".to_string(),
    };
    let body = serde_json::json!({"messages": []})
        .as_object()
        .expect("test chat body must be an object")
        .clone();

    let error = LocalBackend::new()
        .post_chat(&target, body)
        .await
        .expect_err("invalid success JSON must fail");

    assert!(matches!(error, LocalModelError::InvalidJson));
}

#[tokio::test]
async fn large_http_error_returns_before_reading_the_full_body() {
    let mock = support::MockServer::start().await;
    let prefix = "local backend overloaded: ";
    let first_chunk = format!("{prefix}{}", "x".repeat(64 * 1024));
    let slow_tail = [b'x'];
    let mut chunks = Vec::with_capacity(1_001);
    chunks.push(first_chunk.as_bytes());
    chunks.extend(std::iter::repeat_n(slow_tail.as_slice(), 1_000));
    mock.respond_sse_split_chunks("POST", "/v1/chat/completions", 429, chunks)
        .await;
    let target = LocalModelTarget {
        public_id: "local-model".to_string(),
        base_url: format!("{}/v1", mock.base_url),
        upstream_model: "upstream-model".to_string(),
    };
    let body = serde_json::json!({"messages": []})
        .as_object()
        .expect("test chat body must be an object")
        .clone();

    let error = tokio::time::timeout(
        Duration::from_millis(500),
        LocalBackend::new().post_chat(&target, body),
    )
    .await
    .expect("bounded error reader must not wait for the slow tail")
    .expect_err("HTTP 429 must fail");

    let LocalModelError::Http { status, detail } = error else {
        panic!("expected HTTP error, got {error:?}");
    };
    assert_eq!(status, 429);
    assert!(detail.starts_with(prefix));
    assert!(detail.chars().count() <= 4096);
}
