mod support;

use axum::body::Body;
use http::{Request, StatusCode};
use http_body_util::BodyExt;
use serde_json::Value;
use tower::ServiceExt;

use copilot_proxy_rs::http::router;

#[tokio::test]
async fn messages_returns_live_anthropic_response() {
    let fixture = support::AppFixture::with_mock_copilot().await;
    fixture
        .mock
        .respond_json(
            "POST",
            "/v1/messages",
            200,
            serde_json::json!({
                "id": "msg_1",
                "type": "message",
                "role": "assistant",
                "model": "claude-sonnet-4-6",
                "content": [{"type":"text","text":"hello"}],
                "stop_reason": "end_turn",
                "usage": {"input_tokens": 3, "output_tokens": 1}
            }),
        )
        .await;

    let response = router(fixture.state)
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/messages")
                .header("content-type", "application/json")
                .body(Body::from(
                    r#"{"model":"claude-sonnet-4-6","max_tokens":64,"messages":[{"role":"user","content":"hi"}]}"#,
                ))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let body = response_json(response).await;
    assert_eq!(body["content"][0]["text"], "hello");
}

#[tokio::test]
async fn messages_streams_anthropic_sse_from_copilot() {
    let fixture = support::AppFixture::with_mock_copilot().await;
    fixture
        .mock
        .respond_sse(
            "POST",
            "/v1/messages",
            200,
            vec![
                concat!(
                    "event: message_start\n",
                    r#"data: {"type":"message_start","message":{"id":"msg_1","type":"message","role":"assistant","content":[],"model":"gpt-messages-test","usage":{"input_tokens":1,"output_tokens":0}}}"#
                ),
                "event: done\ndata: [DONE]",
            ],
        )
        .await;

    // Pre-register non-claude model as supporting /v1/messages (dynamic registry, not heuristic)
    fixture
        .state
        .models
        .set_copilot_models(vec![serde_json::json!({
            "id": "gpt-messages-test",
            "owned_by": "openai",
            "supported_endpoints": ["/v1/messages"]
        })])
        .await;

    let response = router(fixture.state)
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/messages")
                .header("content-type", "application/json")
                .body(Body::from(
                    r#"{"model":"gpt-messages-test","stream":true,"max_tokens":64,"messages":[{"role":"user","content":"hi"}]}"#,
                ))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let text = String::from_utf8(
        response
            .into_body()
            .collect()
            .await
            .unwrap()
            .to_bytes()
            .to_vec(),
    )
    .unwrap();
    assert!(text.contains("event: message_start"));
    assert!(text.contains("data: [DONE]"));
}

#[tokio::test]
async fn messages_routes_gpt55_to_responses_and_returns_anthropic_response() {
    let fixture = support::AppFixture::with_mock_copilot().await;
    fixture
        .mock
        .respond_json(
            "POST",
            "/responses",
            200,
            serde_json::json!({
                "id": "resp_messages_bridge",
                "object": "response",
                "status": "completed",
                "output": [{"type":"message","role":"assistant","content":[{"type":"output_text","text":"hello from responses"}]}],
                "usage": {"input_tokens": 3, "output_tokens": 2, "total_tokens": 5}
            }),
        )
        .await;

    let response = router(fixture.state.clone())
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/messages")
                .header("content-type", "application/json")
                .body(Body::from(
                    r#"{"model":"gpt-5.5","max_tokens":64,"messages":[{"role":"user","content":"hello"}]}"#,
                ))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(fixture.mock.hits("POST", "/v1/messages").await, 0);
    assert_eq!(fixture.mock.hits("POST", "/chat/completions").await, 0);
    assert_eq!(fixture.mock.hits("POST", "/responses").await, 1);
    let body = response_json(response).await;
    assert_eq!(body["type"], "message");
    assert_eq!(body["content"][0]["text"], "hello from responses");
    let outbound = fixture
        .mock
        .last_request_body_json("POST", "/responses")
        .await
        .unwrap();
    assert_eq!(outbound["model"], "gpt-5.5");
    assert_eq!(outbound["input"][0]["role"], "user");
}

#[tokio::test]
async fn messages_bridge_converts_anthropic_content_blocks_for_responses() {
    let fixture = support::AppFixture::with_mock_copilot().await;
    fixture
        .mock
        .respond_json(
            "POST",
            "/responses",
            200,
            serde_json::json!({
                "id": "resp_messages_bridge",
                "object": "response",
                "status": "completed",
                "output": [{"type":"message","role":"assistant","content":[{"type":"output_text","text":"ok"}]}],
                "usage": {"input_tokens": 3, "output_tokens": 2, "total_tokens": 5}
            }),
        )
        .await;

    let response = router(fixture.state.clone())
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/messages")
                .header("content-type", "application/json")
                .body(Body::from(
                    r#"{
                        "model":"gpt-5.5",
                        "max_tokens":64,
                        "system":[{"type":"text","text":"You are concise."}],
                        "messages":[{"role":"user","content":[{"type":"text","text":"hello"}]}]
                    }"#,
                ))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let outbound = fixture
        .mock
        .last_request_body_json("POST", "/responses")
        .await
        .unwrap();
    assert_eq!(outbound["instructions"], "You are concise.");
    assert_eq!(outbound["input"][0]["content"][0]["type"], "input_text");
    assert_eq!(outbound["input"][0]["content"][0]["text"], "hello");
}

#[tokio::test]
async fn messages_bridge_preserves_prompt_cache_controls_for_responses() {
    let fixture = support::AppFixture::with_mock_copilot().await;
    fixture
        .mock
        .respond_json(
            "POST",
            "/responses",
            200,
            serde_json::json!({
                "id": "resp_messages_cache",
                "object": "response",
                "status": "completed",
                "output": [{"type":"message","role":"assistant","content":[{"type":"output_text","text":"ok"}]}],
                "usage": {"input_tokens": 3, "output_tokens": 2, "total_tokens": 5}
            }),
        )
        .await;

    let response = router(fixture.state.clone())
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/messages")
                .header("content-type", "application/json")
                .body(Body::from(
                    r#"{
                        "model":"gpt-5.5",
                        "max_tokens":64,
                        "prompt_cache_key":"messages-cache-key",
                        "prompt_cache_retention":"24h",
                        "messages":[{"role":"user","content":"hello"}]
                    }"#,
                ))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let outbound = fixture
        .mock
        .last_request_body_json("POST", "/responses")
        .await
        .unwrap();
    assert_eq!(outbound["prompt_cache_key"], "messages-cache-key");
    assert_eq!(outbound["prompt_cache_retention"], "24h");
}

#[tokio::test]
async fn messages_stream_routes_gpt55_to_responses_stream() {
    let fixture = support::AppFixture::with_mock_copilot().await;
    fixture
        .mock
        .respond_sse(
            "POST",
            "/responses",
            200,
            vec![
                r#"data: {"type":"response.output_text.delta","delta":"hello"}"#,
                r#"data: {"type":"response.output_text.delta","delta":" stream"}"#,
                r#"data: {"type":"response.completed","response":{"id":"resp_stream","object":"response","status":"completed","output":[],"usage":{"input_tokens":1,"output_tokens":2,"total_tokens":3}}}"#,
            ],
        )
        .await;

    let response = router(fixture.state.clone())
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/messages")
                .header("content-type", "application/json")
                .body(Body::from(
                    r#"{"model":"gpt-5.5","stream":true,"max_tokens":64,"messages":[{"role":"user","content":"hello"}]}"#,
                ))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(fixture.mock.hits("POST", "/v1/messages").await, 0);
    assert_eq!(fixture.mock.hits("POST", "/chat/completions").await, 0);
    assert_eq!(fixture.mock.hits("POST", "/responses").await, 1);
    assert_eq!(response.headers()["content-type"], "text/event-stream");
    let text = String::from_utf8(
        response
            .into_body()
            .collect()
            .await
            .unwrap()
            .to_bytes()
            .to_vec(),
    )
    .unwrap();
    assert!(text.contains("event: content_block_delta"), "{text}");
    assert!(text.contains("hello"), "{text}");
    assert!(text.contains(" stream"), "{text}");
    assert!(text.contains("data: [DONE]"), "{text}");
}

#[tokio::test]
async fn messages_filters_and_forwards_anthropic_beta_header() {
    let fixture = support::AppFixture::with_mock_copilot().await;
    fixture
        .mock
        .respond_json(
            "POST",
            "/v1/messages",
            200,
            serde_json::json!({
                "id": "msg_2",
                "type": "message",
                "role": "assistant",
                "model": "claude-sonnet-4-6",
                "content": [{"type": "text", "text": "beta ok"}],
                "stop_reason": "end_turn",
                "usage": {"input_tokens": 1, "output_tokens": 1}
            }),
        )
        .await;

    let response = router(fixture.state.clone())
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/messages")
                .header("content-type", "application/json")
                .header("anthropic-beta", "interleaved-thinking-2025-05-14, junk-beta")
                .body(Body::from(
                    r#"{"model":"claude-sonnet-4-6","max_tokens":64,"messages":[{"role":"user","content":"hi"}]}"#,
                ))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let forwarded = fixture
        .mock
        .last_request_header("POST", "/v1/messages", "anthropic-beta")
        .await;
    assert_eq!(
        forwarded.as_deref(),
        Some("interleaved-thinking-2025-05-14"),
        "expected filtered beta header forwarded; got: {forwarded:?}"
    );
}

#[tokio::test]
async fn messages_preserves_anthropic_cache_control_for_claude_requests() {
    let fixture = support::AppFixture::with_mock_copilot().await;
    fixture
        .mock
        .respond_json(
            "POST",
            "/v1/messages",
            200,
            serde_json::json!({
                "id": "msg_cache",
                "type": "message",
                "role": "assistant",
                "model": "claude-sonnet-4-6",
                "content": [{"type": "text", "text": "cache ok"}],
                "stop_reason": "end_turn",
                "usage": {"input_tokens": 1, "output_tokens": 1}
            }),
        )
        .await;

    let response = router(fixture.state.clone())
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/messages")
                .header("content-type", "application/json")
                .body(Body::from(
                    r#"{
                        "model":"claude-sonnet-4-6",
                        "max_tokens":64,
                        "system":[{
                            "type":"text",
                            "text":"Stable system context",
                            "cache_control":{"type":"ephemeral"}
                        }],
                        "messages":[{
                            "role":"user",
                            "content":[{
                                "type":"text",
                                "text":"Stable user context",
                                "cache_control":{"type":"ephemeral"}
                            }]
                        }]
                    }"#,
                ))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let outbound = fixture
        .mock
        .last_request_body_json("POST", "/v1/messages")
        .await
        .unwrap();
    assert_eq!(outbound["system"][0]["cache_control"]["type"], "ephemeral");
    assert_eq!(
        outbound["messages"][0]["content"][0]["cache_control"]["type"],
        "ephemeral"
    );
}

#[tokio::test]
async fn messages_stream_preserves_anthropic_cache_control_for_claude_requests() {
    let fixture = support::AppFixture::with_mock_copilot().await;
    fixture
        .mock
        .respond_sse(
            "POST",
            "/v1/messages",
            200,
            vec![
                concat!(
                    "event: message_start\n",
                    r#"data: {"type":"message_start","message":{"id":"msg_cache_stream","type":"message","role":"assistant","content":[],"model":"claude-sonnet-4-6","usage":{"input_tokens":1,"output_tokens":0}}}"#
                ),
                "event: done\ndata: [DONE]",
            ],
        )
        .await;

    let response = router(fixture.state.clone())
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/messages")
                .header("content-type", "application/json")
                .body(Body::from(
                    r#"{
                        "model":"claude-sonnet-4-6",
                        "stream":true,
                        "max_tokens":64,
                        "system":[{
                            "type":"text",
                            "text":"Stable system context",
                            "cache_control":{"type":"ephemeral"}
                        }],
                        "messages":[{
                            "role":"user",
                            "content":[{
                                "type":"text",
                                "text":"Stable user context",
                                "cache_control":{"type":"ephemeral"}
                            }]
                        }]
                    }"#,
                ))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let outbound = fixture
        .mock
        .last_request_body_json("POST", "/v1/messages")
        .await
        .unwrap();
    assert_eq!(outbound["system"][0]["cache_control"]["type"], "ephemeral");
    assert_eq!(
        outbound["messages"][0]["content"][0]["cache_control"]["type"],
        "ephemeral"
    );
}

async fn response_json(response: axum::response::Response) -> Value {
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    serde_json::from_slice(&bytes).unwrap()
}

#[tokio::test]
async fn messages_strips_structured_output_and_clamps_output_effort() {
    let fixture = support::AppFixture::with_mock_copilot().await;
    fixture
        .mock
        .respond_json(
            "POST",
            "/v1/messages",
            200,
            serde_json::json!({
                "id": "msg_effort",
                "type": "message",
                "role": "assistant",
                "model": "claude-sonnet-4.6",
                "content": [{"type":"text","text":"ok"}],
                "stop_reason": "end_turn",
                "usage": {"input_tokens": 1, "output_tokens": 1}
            }),
        )
        .await;

    let response = router(fixture.state.clone())
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/messages")
                .header("content-type", "application/json")
                .body(Body::from(
                    r#"{"model":"claude-sonnet-4-6","max_tokens":64,"output_config":{"format":{"type":"json_schema"},"effort":"xhigh"},"messages":[{"role":"user","content":"hi"}]}"#,
                ))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let outbound = fixture
        .mock
        .last_request_body_json("POST", "/v1/messages")
        .await
        .unwrap();
    assert_eq!(outbound["output_config"]["effort"], "high");
    assert!(outbound["output_config"].get("format").is_none());
}

#[tokio::test]
async fn messages_adaptive_only_models_convert_enabled_thinking_to_adaptive_and_remove_output_config()
 {
    let fixture = support::AppFixture::with_mock_copilot().await;
    fixture
        .mock
        .respond_json(
            "POST",
            "/v1/messages",
            200,
            serde_json::json!({
                "id": "msg_adaptive",
                "type": "message",
                "role": "assistant",
                "model": "claude-opus-4.8",
                "content": [{"type":"text","text":"ok"}],
                "stop_reason": "end_turn",
                "usage": {"input_tokens": 1, "output_tokens": 1}
            }),
        )
        .await;

    let response = router(fixture.state.clone())
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/messages")
                .header("content-type", "application/json")
                .body(Body::from(
                    r#"{"model":"claude-opus-4-8","max_tokens":64,"thinking":{"type":"enabled","budget_tokens":2048},"output_config":{"effort":"max"},"messages":[{"role":"user","content":"hi"}]}"#,
                ))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let outbound = fixture
        .mock
        .last_request_body_json("POST", "/v1/messages")
        .await
        .unwrap();
    assert_eq!(outbound["thinking"]["type"], "adaptive");
    assert!(outbound.get("output_config").is_none());
}

#[tokio::test]
async fn messages_non_adaptive_models_convert_adaptive_thinking_to_enabled_and_strip_effort() {
    let fixture = support::AppFixture::with_mock_copilot().await;
    fixture
        .state
        .models
        .set_copilot_models(vec![serde_json::json!({
            "id": "claude-3.5-sonnet",
            "owned_by": "anthropic",
            "supported_endpoints": ["/v1/messages"]
        })])
        .await;
    fixture
        .mock
        .respond_json(
            "POST",
            "/v1/messages",
            200,
            serde_json::json!({
                "id": "msg_enabled",
                "type": "message",
                "role": "assistant",
                "model": "claude-3.5-sonnet",
                "content": [{"type":"text","text":"ok"}],
                "stop_reason": "end_turn",
                "usage": {"input_tokens": 1, "output_tokens": 1}
            }),
        )
        .await;

    let response = router(fixture.state.clone())
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/messages")
                .header("content-type", "application/json")
                .body(Body::from(
                    r#"{"model":"claude-3.5-sonnet","max_tokens":64,"thinking":{"type":"adaptive"},"output_config":{"effort":"high"},"messages":[{"role":"user","content":"hi"}]}"#,
                ))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let outbound = fixture
        .mock
        .last_request_body_json("POST", "/v1/messages")
        .await
        .unwrap();
    assert_eq!(outbound["thinking"]["type"], "enabled");
    assert_eq!(outbound["thinking"]["budget_tokens"], 10000);
    assert!(outbound.get("output_config").is_none());
}
