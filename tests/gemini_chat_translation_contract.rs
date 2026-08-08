mod support;

use axum::body::Body;
use http::{Request, StatusCode};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use tower::ServiceExt;

use copilot_proxy_rs::http::router;

const GEMINI_MODELS: [&str; 3] = [
    "gemini-3.1-pro-preview",
    "gemini-3.5-flash",
    "gemini-3.6-flash",
];

/// Advertises a Gemini model that only supports chat completions upstream,
/// matching the Copilot catalog behavior that rejects the Responses API.
fn chat_only_models_payload() -> Value {
    json!({
        "data": GEMINI_MODELS
            .iter()
            .map(|id| json!({
                "id": id,
                "owned_by": "google",
                "supported_endpoints": ["/chat/completions"]
            }))
            .collect::<Vec<_>>()
    })
}

fn chat_completion_ok(model: &str) -> Value {
    json!({
        "id": "chatcmpl-1",
        "object": "chat.completion",
        "model": model,
        "choices": [{
            "index": 0,
            "finish_reason": "stop",
            "message": {"role": "assistant", "content": "OK"}
        }],
        "usage": {"prompt_tokens": 5, "completion_tokens": 1, "total_tokens": 6}
    })
}

const CHAT_STREAM_LINES: [&str; 4] = [
    r#"data: {"id":"chatcmpl-1","object":"chat.completion.chunk","choices":[{"index":0,"delta":{"role":"assistant"}}]}"#,
    r#"data: {"id":"chatcmpl-1","object":"chat.completion.chunk","choices":[{"index":0,"delta":{"content":"OK"}}]}"#,
    r#"data: {"id":"chatcmpl-1","object":"chat.completion.chunk","choices":[{"index":0,"delta":{},"finish_reason":"stop"}],"usage":{"prompt_tokens":5,"completion_tokens":1,"total_tokens":6}}"#,
    "data: [DONE]",
];

async fn response_json(response: axum::response::Response) -> Value {
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    serde_json::from_slice(&bytes).unwrap()
}

async fn response_text(response: axum::response::Response) -> String {
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    String::from_utf8(bytes.to_vec()).unwrap()
}

fn sse_events(text: &str) -> Vec<Value> {
    text.lines()
        .filter_map(|line| line.strip_prefix("data: "))
        .filter(|payload| *payload != "[DONE]")
        .filter_map(|payload| serde_json::from_str(payload).ok())
        .collect()
}

#[tokio::test]
async fn messages_non_streaming_returns_text_for_chat_only_models() {
    for model in GEMINI_MODELS {
        let fixture = support::AppFixture::with_mock_copilot().await;
        fixture
            .mock
            .respond_json("GET", "/models", 200, chat_only_models_payload())
            .await;
        fixture
            .mock
            .respond_json("POST", "/chat/completions", 200, chat_completion_ok(model))
            .await;

        let response = router(fixture.state.clone())
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/v1/messages")
                    .header("content-type", "application/json")
                    .body(Body::from(format!(
                        r#"{{"model":"{model}","max_tokens":16,"stream":false,"messages":[{{"role":"user","content":"Reply exactly OK"}}]}}"#
                    )))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK, "model {model}");
        let body = response_json(response).await;
        assert_eq!(body["model"], model);
        assert_eq!(body["content"][0]["type"], "text");
        assert_eq!(body["content"][0]["text"], "OK", "model {model}");
        assert_eq!(body["stop_reason"], "end_turn");
        assert_eq!(fixture.mock.hits("POST", "/responses").await, 0);
    }
}

#[tokio::test]
async fn messages_streaming_translates_chat_only_models_to_anthropic_events() {
    for model in GEMINI_MODELS {
        let fixture = support::AppFixture::with_mock_copilot().await;
        fixture
            .mock
            .respond_json("GET", "/models", 200, chat_only_models_payload())
            .await;
        fixture
            .mock
            .respond_sse("POST", "/chat/completions", 200, CHAT_STREAM_LINES.to_vec())
            .await;

        let response = router(fixture.state.clone())
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/v1/messages")
                    .header("content-type", "application/json")
                    .body(Body::from(format!(
                        r#"{{"model":"{model}","max_tokens":16,"stream":true,"messages":[{{"role":"user","content":"Reply exactly OK"}}]}}"#
                    )))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK, "model {model}");
        assert_eq!(
            response
                .headers()
                .get(http::header::CONTENT_TYPE)
                .and_then(|value| value.to_str().ok()),
            Some("text/event-stream")
        );
        let text = response_text(response).await;
        let events = sse_events(&text);
        let start = events
            .iter()
            .find(|event| event["type"] == "message_start")
            .unwrap_or_else(|| panic!("missing message_start for {model}: {text}"));
        assert_eq!(start["message"]["model"], model);
        let assembled: String = events
            .iter()
            .filter(|event| event["type"] == "content_block_delta")
            .filter_map(|event| event["delta"]["text"].as_str())
            .collect();
        assert_eq!(assembled, "OK", "model {model}");
        assert!(events.iter().any(|event| event["type"] == "message_stop"));
    }
}

#[tokio::test]
async fn responses_non_streaming_uses_chat_completions_for_chat_only_models() {
    for model in GEMINI_MODELS {
        let fixture = support::AppFixture::with_mock_copilot().await;
        fixture
            .mock
            .respond_json("GET", "/models", 200, chat_only_models_payload())
            .await;
        fixture
            .mock
            .respond_json("POST", "/chat/completions", 200, chat_completion_ok(model))
            .await;

        let response = router(fixture.state.clone())
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/v1/responses")
                    .header("content-type", "application/json")
                    .body(Body::from(format!(
                        r#"{{"model":"{model}","stream":false,"input":"Reply exactly OK"}}"#
                    )))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK, "model {model}");
        let body = response_json(response).await;
        assert_eq!(body["model"], model);
        assert_eq!(body["status"], "completed");
        assert_eq!(body["output"][0]["content"][0]["text"], "OK");
        assert_eq!(fixture.mock.hits("POST", "/responses").await, 0);
    }
}

#[tokio::test]
async fn responses_streaming_uses_chat_completions_for_chat_only_models() {
    for model in GEMINI_MODELS {
        let fixture = support::AppFixture::with_mock_copilot().await;
        fixture
            .mock
            .respond_json("GET", "/models", 200, chat_only_models_payload())
            .await;
        fixture
            .mock
            .respond_sse("POST", "/chat/completions", 200, CHAT_STREAM_LINES.to_vec())
            .await;

        let response = router(fixture.state.clone())
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/v1/responses")
                    .header("content-type", "application/json")
                    .body(Body::from(format!(
                        r#"{{"model":"{model}","stream":true,"input":"Reply exactly OK"}}"#
                    )))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK, "model {model}");
        let text = response_text(response).await;
        let events = sse_events(&text);
        let completed = events
            .iter()
            .find(|event| event["type"] == "response.completed")
            .unwrap_or_else(|| panic!("missing response.completed for {model}: {text}"));
        assert_eq!(completed["response"]["model"], model);
        assert_eq!(
            completed["response"]["output"][0]["content"][0]["text"], "OK",
            "model {model}"
        );
        assert!(text.ends_with("data: [DONE]\n\n"));
        assert_eq!(fixture.mock.hits("POST", "/responses").await, 0);
    }
}

/// Gemini spends its budget on reasoning tokens before emitting text, so a
/// small `max_tokens` truncates the turn. Clients must see `max_tokens` rather
/// than a `end_turn` that falsely implies a complete answer.
#[tokio::test]
async fn messages_reports_max_tokens_stop_reason_when_output_is_truncated() {
    let fixture = support::AppFixture::with_mock_copilot().await;
    fixture
        .mock
        .respond_json("GET", "/models", 200, chat_only_models_payload())
        .await;
    fixture
        .mock
        .respond_json(
            "POST",
            "/chat/completions",
            200,
            json!({
                "id": "chatcmpl-trunc",
                "object": "chat.completion",
                "model": "gemini-3.6-flash",
                "choices": [{
                    "index": 0,
                    "finish_reason": "length",
                    "message": {"role": "assistant", "content": null}
                }],
                "usage": {"prompt_tokens": 3, "completion_tokens": 16, "total_tokens": 19}
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
                    r#"{"model":"gemini-3.6-flash","max_tokens":16,"stream":false,"messages":[{"role":"user","content":"Reply exactly OK"}]}"#,
                ))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let body = response_json(response).await;
    assert_eq!(body["stop_reason"], "max_tokens");
}

#[tokio::test]
async fn messages_streaming_terminates_and_reports_truncation() {
    let fixture = support::AppFixture::with_mock_copilot().await;
    fixture
        .mock
        .respond_json("GET", "/models", 200, chat_only_models_payload())
        .await;
    fixture
        .mock
        .respond_sse(
            "POST",
            "/chat/completions",
            200,
            vec![
                r#"data: {"id":"c1","object":"chat.completion.chunk","choices":[{"index":0,"delta":{"role":"assistant"}}]}"#,
                r#"data: {"id":"c1","object":"chat.completion.chunk","choices":[{"index":0,"delta":{},"finish_reason":"length"}],"usage":{"prompt_tokens":3,"completion_tokens":16,"total_tokens":19}}"#,
                "data: [DONE]",
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
                    r#"{"model":"gemini-3.6-flash","max_tokens":16,"stream":true,"messages":[{"role":"user","content":"Reply exactly OK"}]}"#,
                ))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let text = response_text(response).await;
    let events = sse_events(&text);
    let delta = events
        .iter()
        .find(|event| event["type"] == "message_delta")
        .unwrap_or_else(|| panic!("missing message_delta: {text}"));
    assert_eq!(delta["delta"]["stop_reason"], "max_tokens");
    assert!(
        events.iter().any(|event| event["type"] == "message_stop"),
        "truncated stream must still terminate: {text}"
    );
}
