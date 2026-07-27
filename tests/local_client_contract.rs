mod support;

use copilot_proxy_rs::local::LocalBackend;
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
