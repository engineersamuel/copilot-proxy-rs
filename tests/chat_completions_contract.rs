mod support;

use support::log_capture::{field, with_event_capture};

use axum::body::Body;
use http::{Request, StatusCode};
use http_body_util::BodyExt;
use serde_json::Value;
use tower::ServiceExt;

use copilot_proxy_rs::http::router;

#[tokio::test]
async fn chat_completions_returns_live_copilot_response() {
    let fixture = support::AppFixture::with_mock_copilot().await;
    fixture
        .mock
        .respond_json(
            "POST",
            "/chat/completions",
            200,
            serde_json::json!({
                "choices": [{"message": {"role": "assistant", "content": "Funny quote!"}}],
                "usage": {"prompt_tokens": 4, "completion_tokens": 2, "total_tokens": 6}
            }),
        )
        .await;

    let app = router(fixture.state);
    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/chat/completions")
                .header("content-type", "application/json")
                .body(Body::from(
                    r#"{"model":"gpt-5-mini","messages":[{"role":"user","content":"Hello"}]}"#,
                ))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let body = response_json(response).await;
    assert_eq!(body["object"], "chat.completion");
    assert_eq!(body["choices"][0]["message"]["content"], "Funny quote!");
}

#[tokio::test]
async fn chat_completions_streams_openai_sse() {
    let fixture = support::AppFixture::with_mock_copilot().await;
    fixture
        .mock
        .respond_sse(
            "POST",
            "/chat/completions",
            200,
            vec![
                r#"data: {"choices":[{"delta":{"content":"hi"}}]}"#,
                "data: [DONE]",
            ],
        )
        .await;

    let app = router(fixture.state);
    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/chat/completions")
                .header("content-type", "application/json")
                .body(Body::from(
                    r#"{"model":"gpt-5-mini","stream":true,"messages":[{"role":"user","content":"Hello"}]}"#,
                ))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
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
    assert!(
        text.contains("chat.completion.chunk"),
        "missing chunk object type; got: {text}"
    );
    assert!(text.contains("[DONE]"));
}

#[tokio::test]
async fn chat_completions_routes_responses_only_model_to_responses_endpoint() {
    let fixture = support::AppFixture::with_mock_copilot().await;
    fixture
        .state
        .models
        .set_copilot_models(vec![serde_json::json!({
            "id": "gpt-5.5",
            "owned_by": "openai",
            "supported_endpoints": ["/responses"],
            "capabilities": {"supports": {"reasoning_effort": ["low", "medium", "high"]}}
        })])
        .await;
    fixture
        .mock
        .respond_json(
            "POST",
            "/responses",
            200,
            serde_json::json!({
                "id": "resp_chat_bridge",
                "object": "response",
                "status": "completed",
                "output": [{"type":"message","role":"assistant","content":[{"type":"output_text","text":"bridged ok"}]}],
                "usage": {"input_tokens": 3, "output_tokens": 2, "total_tokens": 5}
            }),
        )
        .await;

    let response = router(fixture.state.clone())
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/chat/completions")
                .header("content-type", "application/json")
                .body(Body::from(
                    r#"{"model":"gpt-5.5","reasoning_effort":"max","messages":[{"role":"user","content":"Hello"}]}"#,
                ))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(fixture.mock.hits("POST", "/chat/completions").await, 0);
    assert_eq!(fixture.mock.hits("POST", "/responses").await, 1);
    let body = response_json(response).await;
    assert_eq!(body["object"], "chat.completion");
    assert_eq!(body["choices"][0]["message"]["content"], "bridged ok");
    assert_eq!(body["usage"]["total_tokens"], 5);
    let outbound = fixture
        .mock
        .last_request_body_json("POST", "/responses")
        .await
        .unwrap();
    assert_eq!(outbound["model"], "gpt-5.5");
    assert_eq!(outbound["reasoning"]["effort"], "high");
    assert_eq!(outbound["input"][0]["role"], "user");
}

#[tokio::test]
async fn chat_completions_preserves_prompt_cache_controls_when_routed_to_responses() {
    let fixture = support::AppFixture::with_mock_copilot().await;
    fixture
        .state
        .models
        .set_copilot_models(vec![serde_json::json!({
            "id": "gpt-5.5",
            "owned_by": "openai",
            "supported_endpoints": ["/responses"]
        })])
        .await;
    fixture
        .mock
        .respond_json(
            "POST",
            "/responses",
            200,
            serde_json::json!({
                "id": "resp_chat_cache",
                "object": "response",
                "status": "completed",
                "output": [{"type":"message","role":"assistant","content":[{"type":"output_text","text":"cached bridge"}]}],
                "usage": {"input_tokens": 3, "output_tokens": 2, "total_tokens": 5}
            }),
        )
        .await;

    let response = router(fixture.state.clone())
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/chat/completions")
                .header("content-type", "application/json")
                .body(Body::from(
                    r#"{
                        "model":"gpt-5.5",
                        "prompt_cache_key":"chat-cache-key",
                        "prompt_cache_retention":"24h",
                        "messages":[{"role":"user","content":"Hello"}]
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
    assert_eq!(outbound["prompt_cache_key"], "chat-cache-key");
    assert_eq!(outbound["prompt_cache_retention"], "24h");
}

#[tokio::test]
async fn chat_completions_routes_static_gpt55_to_responses_endpoint_without_metadata() {
    let fixture = support::AppFixture::with_mock_copilot().await;
    fixture
        .mock
        .respond_json(
            "POST",
            "/responses",
            200,
            serde_json::json!({
                "id": "resp_static_chat_bridge",
                "object": "response",
                "status": "completed",
                "output": [{"type":"message","role":"assistant","content":[{"type":"output_text","text":"static bridge ok"}]}],
                "usage": {"input_tokens": 3, "output_tokens": 2, "total_tokens": 5}
            }),
        )
        .await;

    let response = router(fixture.state.clone())
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/chat/completions")
                .header("content-type", "application/json")
                .body(Body::from(
                    r#"{"model":"gpt-5.5","messages":[{"role":"user","content":"Hello"}]}"#,
                ))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(fixture.mock.hits("POST", "/chat/completions").await, 0);
    assert_eq!(fixture.mock.hits("POST", "/responses").await, 1);
    let body = response_json(response).await;
    assert_eq!(body["choices"][0]["message"]["content"], "static bridge ok");
}

#[tokio::test]
async fn chat_completions_refreshes_models_before_endpoint_selection() {
    let fixture = support::AppFixture::with_mock_copilot().await;
    fixture
        .mock
        .respond_json(
            "GET",
            "/models",
            200,
            serde_json::json!({
                "data": [{
                    "id": "gpt-5.5",
                    "owned_by": "openai",
                    "supported_endpoints": ["/responses"],
                    "capabilities": {"supports": {"reasoning_effort": ["low", "medium", "high"]}}
                }]
            }),
        )
        .await;
    fixture
        .mock
        .respond_json(
            "POST",
            "/responses",
            200,
            serde_json::json!({
                "id": "resp_chat_bridge",
                "object": "response",
                "status": "completed",
                "output": [{"type":"message","role":"assistant","content":[{"type":"output_text","text":"refreshed bridge"}]}],
                "usage": {"input_tokens": 3, "output_tokens": 2, "total_tokens": 5}
            }),
        )
        .await;

    let response = router(fixture.state.clone())
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/chat/completions")
                .header("content-type", "application/json")
                .body(Body::from(
                    r#"{"model":"gpt-5.5","messages":[{"role":"user","content":"Hello"}]}"#,
                ))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(fixture.mock.hits("GET", "/models").await, 1);
    assert_eq!(fixture.mock.hits("POST", "/chat/completions").await, 0);
    assert_eq!(fixture.mock.hits("POST", "/responses").await, 1);
    let body = response_json(response).await;
    assert_eq!(body["choices"][0]["message"]["content"], "refreshed bridge");
}

#[tokio::test]
async fn chat_completions_stream_routes_responses_only_model_to_responses_stream() {
    let fixture = support::AppFixture::with_mock_copilot().await;
    fixture
        .state
        .models
        .set_copilot_models(vec![serde_json::json!({
            "id": "gpt-5.5",
            "owned_by": "openai",
            "supported_endpoints": ["/responses"],
            "capabilities": {"supports": {"reasoning_effort": ["low", "medium", "high"]}}
        })])
        .await;
    fixture
        .mock
        .respond_sse(
            "POST",
            "/responses",
            200,
            vec![
                r#"data: {"type":"response.output_text.delta","delta":"bridge"}"#,
                r#"data: {"type":"response.output_text.delta","delta":" stream"}"#,
                r#"data: {"type":"response.completed","response":{"id":"resp_stream","object":"response","status":"completed","output":[],"usage":{"input_tokens":1,"output_tokens":2,"total_tokens":3}}}"#,
            ],
        )
        .await;

    let response = router(fixture.state.clone())
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/chat/completions")
                .header("content-type", "application/json")
                .body(Body::from(
                    r#"{"model":"gpt-5.5","stream":true,"reasoning_effort":"max","messages":[{"role":"user","content":"Hello"}]}"#,
                ))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
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
    assert!(text.contains(r#""content":"bridge""#), "{text}");
    assert!(text.contains(r#""content":" stream""#), "{text}");
    assert!(text.contains("data: [DONE]"), "{text}");
    let outbound = fixture
        .mock
        .last_request_body_json("POST", "/responses")
        .await
        .unwrap();
    assert_eq!(outbound["model"], "gpt-5.5");
    assert_eq!(outbound["reasoning"]["effort"], "high");
}

#[test]
fn anthropic_tool_use_and_tool_result_blocks_are_preserved_in_openai_translation() {
    use copilot_proxy_rs::translate::anthropic::anthropic_to_openai_request;

    let body = serde_json::json!({
        "model": "gpt-5.5",
        "max_tokens": 100,
        "messages": [
            {
                "role": "assistant",
                "content": [
                    {"type": "text", "text": "Calling tool"},
                    {"type": "tool_use", "id": "tu_1", "name": "my_func", "input": {"arg": 1}}
                ]
            },
            {
                "role": "user",
                "content": [
                    {"type": "tool_result", "tool_use_id": "tu_1", "content": [{"type": "text", "text": "result"}]}
                ]
            }
        ]
    });

    let result = anthropic_to_openai_request(body.as_object().unwrap(), "gpt-5.5");
    let messages = result["messages"].as_array().unwrap();

    let asst_content = messages[0]["content"].as_array().unwrap();
    assert_eq!(
        asst_content.len(),
        2,
        "Expected 2 content blocks in assistant message, got {}",
        asst_content.len()
    );
    assert!(
        asst_content.iter().any(|c| c["type"] == "text"),
        "text block missing"
    );
    assert!(
        asst_content.iter().any(|c| c["type"] == "tool_use"),
        "tool_use block missing"
    );

    let user_content = messages[1]["content"].as_array().unwrap();
    assert_eq!(
        user_content.len(),
        1,
        "Expected 1 content block in user message, got {}",
        user_content.len()
    );
    assert!(
        user_content.iter().any(|c| c["type"] == "tool_result"),
        "tool_result block missing"
    );
}

async fn response_json(response: axum::response::Response) -> Value {
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    serde_json::from_slice(&bytes).unwrap()
}

#[tokio::test]
async fn chat_completions_forwards_clamped_reasoning_effort_for_supported_model() {
    let fixture = support::AppFixture::with_mock_copilot().await;
    fixture
        .state
        .models
        .set_copilot_models(vec![serde_json::json!({
            "id": "gpt-effort",
            "owned_by": "openai",
            "supported_endpoints": ["/chat/completions"],
            "capabilities": {"supports": {"reasoning_effort": ["low", "medium", "high"]}}
        })])
        .await;
    fixture
        .mock
        .respond_json(
            "POST",
            "/chat/completions",
            200,
            serde_json::json!({"choices": [{"message": {"role": "assistant", "content": "ok"}}]}),
        )
        .await;

    let events = with_event_capture(|| async {
        let response = router(fixture.state.clone())
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/v1/chat/completions")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        r#"{"model":"gpt-effort","reasoning_effort":"max","messages":[{"role":"user","content":"Hello"}]}"#,
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
    })
    .await;

    let outbound = fixture
        .mock
        .last_request_body_json("POST", "/chat/completions")
        .await
        .unwrap();
    assert_eq!(outbound["reasoning_effort"], "high");
    assert_eq!(
        field(&events, "chat effort downgraded", "model.requested").as_deref(),
        Some("gpt-effort")
    );
    assert_eq!(
        field(&events, "chat effort downgraded", "effort.original").as_deref(),
        Some("max")
    );
    assert_eq!(
        field(&events, "chat effort downgraded", "effort.retry").as_deref(),
        Some("high")
    );
}

#[tokio::test]
async fn chat_completions_strips_reasoning_effort_for_unsupported_model() {
    let fixture = support::AppFixture::with_mock_copilot().await;
    fixture
        .mock
        .respond_json(
            "POST",
            "/chat/completions",
            200,
            serde_json::json!({"choices": [{"message": {"role": "assistant", "content": "ok"}}]}),
        )
        .await;

    let response = router(fixture.state.clone())
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/chat/completions")
                .header("content-type", "application/json")
                .body(Body::from(
                    r#"{"model":"gpt-no-effort","reasoning_effort":"high","messages":[{"role":"user","content":"Hello"}]}"#,
                ))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let outbound = fixture
        .mock
        .last_request_body_json("POST", "/chat/completions")
        .await
        .unwrap();
    assert!(outbound.get("reasoning_effort").is_none());
}

#[tokio::test]
async fn chat_completions_sse_data_line_split_across_chunks_is_correctly_assembled() {
    // Arrange: split "data: [DONE]" across two network chunks.
    // chunk1 ends mid-token; chunk2 completes the line.
    // The bug (processing lines per-chunk) emits "data: [DON\n\nE]\n\n" instead of "data: [DONE]\n\n".
    let fixture = support::AppFixture::with_mock_copilot().await;
    let chunk1 =
        b"data: {\"id\":\"1\",\"choices\":[{\"delta\":{\"content\":\"hi\"}}]}\n\ndata: [DON"
            as &[u8];
    let chunk2 = b"E]\n\n" as &[u8];
    fixture
        .mock
        .respond_sse_split_chunks("POST", "/chat/completions", 200, vec![chunk1, chunk2])
        .await;

    let app = router(fixture.state);
    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/chat/completions")
                .header("content-type", "application/json")
                .body(Body::from(
                    r#"{"model":"gpt-5-mini","stream":true,"messages":[{"role":"user","content":"Hi"}]}"#,
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

    // With the bug, the DONE marker gets corrupted into two separate fragments.
    // With the fix, the line buffer reassembles it into a single complete event.
    assert!(
        text.contains("data: [DONE]"),
        "data: [DONE] was corrupted by chunk split; got:\n{text}"
    );
}

#[tokio::test]
async fn chat_completions_logs_safe_request_metadata() {
    let fixture = support::AppFixture::with_mock_copilot().await;
    fixture
        .mock
        .respond_json(
            "POST",
            "/chat/completions",
            200,
            serde_json::json!({
                "choices": [{"message": {"role": "assistant", "content": "private answer"}}],
                "usage": {"prompt_tokens": 4, "completion_tokens": 2, "total_tokens": 6}
            }),
        )
        .await;

    let events = with_event_capture(|| async {
        let response = router(fixture.state.clone())
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/v1/chat/completions")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        r#"{"model":"gpt-5-mini","stream":false,"reasoning_effort":"high","messages":[{"role":"user","content":"private prompt"}],"tools":[{"type":"function","function":{"name":"secret_tool","parameters":{"type":"object"}}}]}"#,
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
    }).await;

    assert_eq!(
        field(&events, "chat request prepared", "api.family").as_deref(),
        Some("chat_completions")
    );
    assert_eq!(
        field(&events, "chat request prepared", "model.requested").as_deref(),
        Some("gpt-5-mini")
    );
    assert_eq!(
        field(&events, "chat request prepared", "model.effective").as_deref(),
        Some("gpt-5-mini")
    );
    assert_eq!(
        field(&events, "chat request prepared", "stream").as_deref(),
        Some("false")
    );
    assert_eq!(
        field(&events, "chat request prepared", "tools.definitions").as_deref(),
        Some("1")
    );
    let rendered = format!("{events:?}");
    assert!(!rendered.contains("private prompt"));
    assert!(!rendered.contains("private answer"));
    assert!(!rendered.contains("secret_tool"));
}
