mod support;

use support::log_capture::{field, with_event_capture};

use axum::body::Body;
use http::{Request, StatusCode};
use http_body_util::BodyExt;
use serde_json::Value;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tower::ServiceExt;

use copilot_proxy_rs::config::{AppConfig, LocalModelConfig};
use copilot_proxy_rs::http::router;
use copilot_proxy_rs::state::AppState;

#[tokio::test]
async fn local_responses_routes_buffered_request_without_copilot_work() {
    let fixture = support::AppFixture::with_mock_local().await;
    fixture
        .mock
        .respond_json(
            "POST",
            "/v1/chat/completions",
            200,
            serde_json::json!({
                "choices": [{
                    "finish_reason": "tool_calls",
                    "message": {
                        "role": "assistant",
                        "content": "hello",
                        "tool_calls": [{
                            "id": "call_1",
                            "type": "function",
                            "function": {
                                "name": "get_weather",
                                "arguments": "{\"city\":\"NYC\"}"
                            }
                        }]
                    }
                }],
                "usage": {"prompt_tokens": 7, "completion_tokens": 3, "total_tokens": 10}
            }),
        )
        .await;

    let response = router(fixture.state.clone())
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/responses")
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::json!({
                        "model": "qwen3-coder-30b-local",
                        "input": "weather",
                        "tools": [{
                            "type": "function",
                            "name": "get_weather",
                            "parameters": {"type": "object"}
                        }]
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let body = response_json(response).await;
    assert_eq!(body["model"], "qwen3-coder-30b-local");
    assert!(body["id"].as_str().unwrap().starts_with("resp_local_"));
    assert_eq!(body["output"][0]["content"][0]["text"], "hello");
    assert_eq!(fixture.mock.hits("POST", "/v1/chat/completions").await, 1);
    assert_eq!(fixture.mock.hits("POST", "/responses").await, 0);
    assert_eq!(fixture.mock.hits("GET", "/models").await, 0);
    assert_eq!(fixture.mock.hits("GET", "/copilot/token").await, 0);
}

#[tokio::test]
async fn local_responses_rejects_hosted_tools_before_transport() {
    let fixture = support::AppFixture::with_mock_local().await;
    let response = router(fixture.state.clone())
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/responses")
                .header("content-type", "application/json")
                .body(Body::from(
                    r#"{"model":"qwen3-coder-30b-local","input":"search","tools":[{"type":"web_search_preview"}]}"#,
                ))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let body = response_json(response).await;
    assert_eq!(body["error"]["type"], "invalid_request_error");
    assert_eq!(fixture.mock.hits("POST", "/v1/chat/completions").await, 0);
    assert_eq!(fixture.mock.hits("POST", "/responses").await, 0);
    assert_eq!(fixture.mock.hits("GET", "/models").await, 0);
    assert_eq!(fixture.mock.hits("GET", "/copilot/token").await, 0);
}

#[tokio::test]
async fn local_responses_expands_cached_transcript_for_second_turn() {
    let fixture = support::AppFixture::with_mock_local().await;
    fixture
        .mock
        .respond_sequence_json(
            "POST",
            "/v1/chat/completions",
            vec![
                (
                    200,
                    serde_json::json!({
                        "choices": [{
                            "finish_reason": "stop",
                            "message": {"content": "first reply"}
                        }],
                        "usage": {"prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2}
                    }),
                    vec![],
                ),
                (
                    200,
                    serde_json::json!({
                        "choices": [{
                            "finish_reason": "stop",
                            "message": {"content": "second reply"}
                        }],
                        "usage": {"prompt_tokens": 3, "completion_tokens": 1, "total_tokens": 4}
                    }),
                    vec![],
                ),
            ],
        )
        .await;

    let first = router(fixture.state.clone())
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/responses")
                .header("content-type", "application/json")
                .header("x-interaction-id", "interaction-local")
                .body(Body::from(
                    r#"{"model":"qwen3-coder-30b-local","input":"first input"}"#,
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(first.status(), StatusCode::OK);
    let first_body = response_json(first).await;
    let first_id = first_body["id"].as_str().unwrap();

    let second = router(fixture.state.clone())
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/responses")
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::json!({
                        "model": "qwen3-coder-30b-local",
                        "input": "second input",
                        "previous_response_id": first_id
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(second.status(), StatusCode::OK);
    let second_body = response_json(second).await;
    assert_eq!(second_body["previous_response_id"], first_id);

    let upstream = fixture
        .mock
        .last_request_body_json("POST", "/v1/chat/completions")
        .await
        .unwrap();
    assert_eq!(
        upstream["messages"],
        serde_json::json!([
            {"role": "user", "content": "first input"},
            {"role": "assistant", "content": "first reply"},
            {"role": "user", "content": "second input"}
        ])
    );
}

#[tokio::test]
async fn local_buffered_incomplete_response_is_not_available_for_follow_up() {
    for finish_reason in ["length", "content_filter"] {
        let fixture = support::AppFixture::with_mock_local().await;
        fixture
            .mock
            .respond_json(
                "POST",
                "/v1/chat/completions",
                200,
                serde_json::json!({
                    "choices": [{
                        "finish_reason": finish_reason,
                        "message": {"content": "partial reply"}
                    }],
                    "usage": {"prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2}
                }),
            )
            .await;

        let first = router(fixture.state.clone())
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/v1/responses")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        r#"{"model":"qwen3-coder-30b-local","input":"first input"}"#,
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(first.status(), StatusCode::OK);
        let first_body = response_json(first).await;
        assert_eq!(first_body["status"], "incomplete");
        let first_id = first_body["id"].as_str().unwrap();

        let follow_up = router(fixture.state.clone())
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/v1/responses")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        serde_json::json!({
                            "model": "qwen3-coder-30b-local",
                            "input": "second input",
                            "previous_response_id": first_id
                        })
                        .to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(follow_up.status(), StatusCode::BAD_REQUEST);
        assert_eq!(fixture.mock.hits("POST", "/v1/chat/completions").await, 1);
        assert_eq!(fixture.mock.hits("POST", "/responses").await, 0);
        assert_eq!(fixture.mock.hits("GET", "/models").await, 0);
        assert_eq!(fixture.mock.hits("GET", "/copilot/token").await, 0);
    }
}

#[tokio::test]
async fn local_responses_rejects_unknown_previous_response_before_transport() {
    let fixture = support::AppFixture::with_mock_local().await;
    let response = router(fixture.state.clone())
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/responses")
                .header("content-type", "application/json")
                .body(Body::from(
                    r#"{"model":"qwen3-coder-30b-local","input":"follow-up","previous_response_id":"resp_local_missing"}"#,
                ))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let body = response_json(response).await;
    assert_eq!(body["error"]["type"], "invalid_request_error");
    assert_eq!(fixture.mock.hits("POST", "/v1/chat/completions").await, 0);
    assert_eq!(fixture.mock.hits("POST", "/responses").await, 0);
    assert_eq!(fixture.mock.hits("GET", "/models").await, 0);
    assert_eq!(fixture.mock.hits("GET", "/copilot/token").await, 0);
}

#[tokio::test]
async fn local_responses_rejects_non_string_previous_response_id_before_transport() {
    for invalid_previous_response_id in [serde_json::json!(42), serde_json::json!({"id": 42})] {
        let fixture = support::AppFixture::with_mock_local().await;
        let response = router(fixture.state.clone())
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/v1/responses")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        serde_json::json!({
                            "model": "qwen3-coder-30b-local",
                            "input": "follow-up",
                            "previous_response_id": invalid_previous_response_id
                        })
                        .to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        let body = response_json(response).await;
        assert_eq!(body["error"]["type"], "invalid_request_error");
        assert_eq!(fixture.mock.hits("POST", "/v1/chat/completions").await, 0);
        assert_eq!(fixture.mock.hits("POST", "/responses").await, 0);
        assert_eq!(fixture.mock.hits("GET", "/models").await, 0);
        assert_eq!(fixture.mock.hits("GET", "/copilot/token").await, 0);
    }
}

#[tokio::test]
async fn local_responses_maps_malformed_upstream_response_without_copilot_fallback() {
    let fixture = support::AppFixture::with_mock_local().await;
    fixture
        .mock
        .respond_json(
            "POST",
            "/v1/chat/completions",
            200,
            serde_json::json!({"choices": []}),
        )
        .await;

    let response = router(fixture.state.clone())
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/responses")
                .header("content-type", "application/json")
                .body(Body::from(
                    r#"{"model":"qwen3-coder-30b-local","input":"hello"}"#,
                ))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
    let body = response_json(response).await;
    assert_eq!(body["error"]["type"], "server_error");
    assert_eq!(fixture.mock.hits("POST", "/v1/chat/completions").await, 1);
    assert_eq!(fixture.mock.hits("POST", "/responses").await, 0);
    assert_eq!(fixture.mock.hits("GET", "/models").await, 0);
}

async fn request_local_responses_stream(fixture: &support::AppFixture) -> String {
    let response = router(fixture.state.clone())
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/responses")
                .header("content-type", "application/json")
                .body(Body::from(
                    r#"{"model":"qwen3-coder-30b-local","input":"first input","stream":true}"#,
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.headers()["content-type"], "text/event-stream");
    String::from_utf8(
        response
            .into_body()
            .collect()
            .await
            .unwrap()
            .to_bytes()
            .to_vec(),
    )
    .unwrap()
}

fn streamed_response_id(text: &str) -> String {
    text.split("\n\n")
        .find_map(|frame| {
            let data = frame.lines().find_map(|line| line.strip_prefix("data: "))?;
            let event = serde_json::from_str::<Value>(data).ok()?;
            (event["type"] == "response.created")
                .then(|| event["response"]["id"].as_str().map(str::to_string))?
        })
        .unwrap()
}

async fn assert_local_response_not_cached(fixture: &support::AppFixture, response_id: &str) {
    let follow_up = router(fixture.state.clone())
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/responses")
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::json!({
                        "model": "qwen3-coder-30b-local",
                        "input": "second input",
                        "previous_response_id": response_id
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(follow_up.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn local_responses_stream_translates_split_chat_sse_and_caches_output() {
    let fixture = support::AppFixture::with_mock_local().await;
    let first_chunk = concat!(
        r#"data: {"choices":[{"delta":{"content":"Hel"}}]}"#,
        "\r\n\r\n",
        r#"data: {"choices":[{"delta":{"content":"l"#,
    );
    let second_chunk = concat!(
        r#"o"}}]}"#,
        "\r\n\r\n",
        r#"data: {"choices":[{"delta":{},"finish_reason":"stop"}],"usage":{"prompt_tokens":2,"completion_tokens":1,"total_tokens":3}}"#,
        "\r\n\r\n",
        "data: [DONE]\r\n\r\n",
    );
    fixture
        .mock
        .respond_sse_split_chunks(
            "POST",
            "/v1/chat/completions",
            200,
            vec![first_chunk.as_bytes(), second_chunk.as_bytes()],
        )
        .await;
    let text = request_local_responses_stream(&fixture).await;
    let frames = text
        .split("\n\n")
        .filter(|frame| !frame.is_empty())
        .collect::<Vec<_>>();
    let events = frames
        .iter()
        .filter_map(|frame| {
            frame
                .lines()
                .find_map(|line| line.strip_prefix("data: "))
                .filter(|data| *data != "[DONE]")
                .map(|data| serde_json::from_str::<Value>(data).unwrap())
        })
        .collect::<Vec<_>>();
    let event_types = events
        .iter()
        .map(|event| event["type"].as_str().unwrap())
        .collect::<Vec<_>>();
    assert_eq!(
        event_types,
        [
            "response.created",
            "response.in_progress",
            "response.output_item.added",
            "response.content_part.added",
            "response.output_text.delta",
            "response.output_text.delta",
            "response.output_text.done",
            "response.content_part.done",
            "response.output_item.done",
            "response.completed",
        ]
    );
    assert_eq!(text.matches("data: [DONE]").count(), 1, "{text:?}");
    assert!(!text.contains(r#"models\\Qwen3-Coder"#), "{text:?}");
    let created = &events[0]["response"];
    let response_id = created["id"].as_str().unwrap();
    assert!(response_id.starts_with("resp_local_"));
    assert_eq!(created["model"], "qwen3-coder-30b-local");
    let completed = &events.last().unwrap()["response"];
    assert_eq!(completed["model"], "qwen3-coder-30b-local");
    assert_eq!(completed["usage"]["input_tokens"], 2);
    assert_eq!(completed["output"][0]["content"][0]["text"], "Hello");
    let streamed_upstream = fixture
        .mock
        .last_request_body_json("POST", "/v1/chat/completions")
        .await
        .unwrap();
    assert_eq!(streamed_upstream["stream_options"]["include_usage"], true);

    fixture
        .mock
        .respond_json(
            "POST",
            "/v1/chat/completions",
            200,
            serde_json::json!({
                "choices": [{
                    "finish_reason": "stop",
                    "message": {"role": "assistant", "content": "second reply"}
                }],
                "usage": {"prompt_tokens": 4, "completion_tokens": 1, "total_tokens": 5}
            }),
        )
        .await;
    let follow_up = router(fixture.state.clone())
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/responses")
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::json!({
                        "model": "qwen3-coder-30b-local",
                        "input": "second input",
                        "previous_response_id": response_id
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(follow_up.status(), StatusCode::OK);
    let outbound = fixture
        .mock
        .last_request_body_json("POST", "/v1/chat/completions")
        .await
        .unwrap();
    assert_eq!(
        outbound["messages"],
        serde_json::json!([
            {"role": "user", "content": "first input"},
            {"role": "assistant", "content": "Hello"},
            {"role": "user", "content": "second input"}
        ])
    );
    assert_eq!(fixture.mock.hits("POST", "/v1/chat/completions").await, 2);
    assert_eq!(fixture.mock.hits("POST", "/responses").await, 0);
    assert_eq!(fixture.mock.hits("GET", "/models").await, 0);
    assert_eq!(fixture.mock.hits("GET", "/copilot/token").await, 0);
}

#[tokio::test]
async fn local_responses_stream_eof_fails_with_done_and_does_not_cache() {
    let fixture = support::AppFixture::with_mock_local().await;
    fixture
        .mock
        .respond_sse_split_chunks(
            "POST",
            "/v1/chat/completions",
            200,
            vec![
                br#"data: {"choices":[{"delta":{"content":"partial"}}]}

"#,
            ],
        )
        .await;

    let text = request_local_responses_stream(&fixture).await;
    assert_eq!(
        text.matches("event: response.failed").count(),
        1,
        "{text:?}"
    );
    assert_eq!(text.matches("data: [DONE]").count(), 1, "{text:?}");
    let response_id = streamed_response_id(&text);
    assert_local_response_not_cached(&fixture, &response_id).await;
    assert_eq!(fixture.mock.hits("POST", "/v1/chat/completions").await, 1);
    assert_eq!(fixture.mock.hits("POST", "/responses").await, 0);
    assert_eq!(fixture.mock.hits("GET", "/models").await, 0);
    assert_eq!(fixture.mock.hits("GET", "/copilot/token").await, 0);
}

#[tokio::test]
async fn local_responses_stream_transport_error_fails_with_done_and_does_not_cache() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let mut request = [0; 4096];
        let _ = socket.read(&mut request).await;
        socket
            .write_all(
                concat!(
                    "HTTP/1.1 200 OK\r\n",
                    "content-type: text/event-stream\r\n",
                    "content-length: 1000\r\n",
                    "connection: close\r\n\r\n",
                    r#"data: {"choices":[{"delta":{"content":"partial"}}]}"#,
                    "\n\n"
                )
                .as_bytes(),
            )
            .await
            .unwrap();
        socket.shutdown().await.unwrap();
    });
    let fixture = support::AppFixture::with_local_base_url(&format!("http://{address}/v1")).await;

    let text = request_local_responses_stream(&fixture).await;
    assert_eq!(
        text.matches("event: response.failed").count(),
        1,
        "{text:?}"
    );
    assert_eq!(text.matches("data: [DONE]").count(), 1, "{text:?}");
    let response_id = streamed_response_id(&text);
    assert_local_response_not_cached(&fixture, &response_id).await;
    assert_eq!(fixture.mock.hits("POST", "/responses").await, 0);
    assert_eq!(fixture.mock.hits("GET", "/models").await, 0);
    assert_eq!(fixture.mock.hits("GET", "/copilot/token").await, 0);
}

#[tokio::test]
async fn local_responses_stream_incomplete_finishes_with_done_and_does_not_cache() {
    let fixture = support::AppFixture::with_mock_local().await;
    fixture
        .mock
        .respond_sse(
            "POST",
            "/v1/chat/completions",
            200,
            vec![
                r#"data: {"choices":[{"delta":{"content":"partial"},"finish_reason":null}]}"#,
                r#"data: {"choices":[{"delta":{},"finish_reason":"length"}],"usage":{"prompt_tokens":2,"completion_tokens":1,"total_tokens":3}}"#,
                "data: [DONE]",
            ],
        )
        .await;

    let text = request_local_responses_stream(&fixture).await;
    assert_eq!(text.matches("event: response.incomplete").count(), 1);
    assert_eq!(text.matches("event: response.completed").count(), 0);
    assert_eq!(text.matches("data: [DONE]").count(), 1);
    let response_id = streamed_response_id(&text);
    assert_local_response_not_cached(&fixture, &response_id).await;
    assert_eq!(fixture.mock.hits("POST", "/v1/chat/completions").await, 1);
    assert_eq!(fixture.mock.hits("POST", "/responses").await, 0);
    assert_eq!(fixture.mock.hits("GET", "/models").await, 0);
    assert_eq!(fixture.mock.hits("GET", "/copilot/token").await, 0);
}

#[tokio::test]
async fn local_responses_stream_caches_before_completed_is_visible() {
    let fixture = support::AppFixture::with_mock_local().await;
    fixture
        .mock
        .respond_sse(
            "POST",
            "/v1/chat/completions",
            200,
            vec![
                r#"data: {"choices":[{"delta":{"content":"first reply"},"finish_reason":null}]}"#,
                r#"data: {"choices":[{"delta":{},"finish_reason":"stop"}],"usage":{"prompt_tokens":2,"completion_tokens":1,"total_tokens":3}}"#,
                "data: [DONE]",
            ],
        )
        .await;
    let response = router(fixture.state.clone())
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/responses")
                .header("content-type", "application/json")
                .body(Body::from(
                    r#"{"model":"qwen3-coder-30b-local","input":"first input","stream":true}"#,
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    let mut body = response.into_body();
    let mut visible = String::new();
    while let Some(frame) = body.frame().await {
        let frame = frame.unwrap();
        if let Ok(data) = frame.into_data() {
            visible.push_str(&String::from_utf8_lossy(&data));
        }
        if visible.contains("event: response.completed\n") {
            break;
        }
    }
    let response_id = streamed_response_id(&visible);
    fixture
        .mock
        .respond_json(
            "POST",
            "/v1/chat/completions",
            200,
            serde_json::json!({
                "choices": [{
                    "finish_reason": "stop",
                    "message": {"role": "assistant", "content": "second reply"}
                }],
                "usage": {"prompt_tokens": 4, "completion_tokens": 1, "total_tokens": 5}
            }),
        )
        .await;

    let follow_up = router(fixture.state.clone())
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/responses")
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::json!({
                        "model": "qwen3-coder-30b-local",
                        "input": "second input",
                        "previous_response_id": response_id
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(follow_up.status(), StatusCode::OK);
    let outbound = fixture
        .mock
        .last_request_body_json("POST", "/v1/chat/completions")
        .await
        .unwrap();
    assert_eq!(outbound["messages"][1]["content"], "first reply");
}

#[tokio::test]
async fn local_responses_stream_echoes_previous_response_id() {
    let fixture = support::AppFixture::with_mock_local().await;
    fixture
        .mock
        .respond_json(
            "POST",
            "/v1/chat/completions",
            200,
            serde_json::json!({
                "choices": [{
                    "finish_reason": "stop",
                    "message": {"role": "assistant", "content": "first reply"}
                }],
                "usage": {"prompt_tokens": 2, "completion_tokens": 1, "total_tokens": 3}
            }),
        )
        .await;
    let first = router(fixture.state.clone())
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/responses")
                .header("content-type", "application/json")
                .body(Body::from(
                    r#"{"model":"qwen3-coder-30b-local","input":"first input"}"#,
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    let first_id = response_json(first).await["id"]
        .as_str()
        .unwrap()
        .to_string();
    fixture
        .mock
        .respond_sse(
            "POST",
            "/v1/chat/completions",
            200,
            vec![
                r#"data: {"choices":[{"delta":{"content":"second reply"},"finish_reason":null}]}"#,
                r#"data: {"choices":[{"delta":{},"finish_reason":"stop"}],"usage":{"prompt_tokens":4,"completion_tokens":1,"total_tokens":5}}"#,
                "data: [DONE]",
            ],
        )
        .await;
    let response = router(fixture.state.clone())
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/responses")
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::json!({
                        "model": "qwen3-coder-30b-local",
                        "input": "second input",
                        "stream": true,
                        "previous_response_id": first_id
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
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
    let snapshots = text
        .split("\n\n")
        .filter_map(|frame| {
            let data = frame.lines().find_map(|line| line.strip_prefix("data: "))?;
            let event = serde_json::from_str::<Value>(data).ok()?;
            event.get("response").cloned()
        })
        .collect::<Vec<_>>();

    assert!(!snapshots.is_empty());
    assert!(
        snapshots
            .iter()
            .all(|response| response["previous_response_id"] == first_id)
    );
}

#[tokio::test]
async fn local_responses_stream_rejects_oversized_sse_line_without_cache() {
    let mock = support::MockServer::start().await;
    let content = "x".repeat(700);
    let line = format!(
        "data: {}\n\n",
        serde_json::json!({
            "choices": [{"delta": {"content": content}, "finish_reason": "stop"}],
            "usage": {"prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2}
        })
    );
    let (first, second) = line.as_bytes().split_at(600);
    mock.respond_sse_split_chunks("POST", "/v1/chat/completions", 200, vec![first, second])
        .await;
    let mut config = AppConfig {
        max_decoded_body_bytes: 512,
        ..Default::default()
    };
    config.local_models.insert(
        "qwen3-coder-30b-local".to_string(),
        LocalModelConfig {
            base_url: format!("{}/v1", mock.base_url),
            upstream_model: "upstream-local".to_string(),
        },
    );
    let state = AppState::new(config);
    let response = router(state.clone())
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/responses")
                .header("content-type", "application/json")
                .body(Body::from(
                    r#"{"model":"qwen3-coder-30b-local","input":"hello","stream":true}"#,
                ))
                .unwrap(),
        )
        .await
        .unwrap();
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

    assert_eq!(
        text.matches("event: response.failed").count(),
        1,
        "{text:?}"
    );
    assert_eq!(
        text.matches("event: response.completed").count(),
        0,
        "{text:?}"
    );
    assert_eq!(text.matches("data: [DONE]").count(), 1, "{text:?}");
    let failed_id = text
        .split("\n\n")
        .find_map(|frame| {
            let data = frame.lines().find_map(|line| line.strip_prefix("data: "))?;
            let event = serde_json::from_str::<Value>(data).ok()?;
            (event["type"] == "response.failed")
                .then(|| event["response"]["id"].as_str().map(str::to_string))?
        })
        .unwrap();
    let follow_up = router(state)
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/responses")
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::json!({
                        "model": "qwen3-coder-30b-local",
                        "input": "again",
                        "previous_response_id": failed_id
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(follow_up.status(), StatusCode::BAD_REQUEST);
    assert_eq!(mock.hits("POST", "/v1/chat/completions").await, 1);
}

#[tokio::test]
async fn local_responses_stream_accepts_case_insensitive_content_type() {
    let fixture = support::AppFixture::with_mock_local().await;
    fixture
        .mock
        .respond_text(
            "POST",
            "/v1/chat/completions",
            200,
            "Text/Event-Stream; Charset=UTF-8",
            concat!(
                r#"data: {"choices":[{"delta":{"content":"hello"},"finish_reason":null}]}"#,
                "\n\n",
                r#"data: {"choices":[{"delta":{},"finish_reason":"stop"}],"usage":{"prompt_tokens":1,"completion_tokens":1,"total_tokens":2}}"#,
                "\n\n",
                "data: [DONE]\n\n"
            )
            .to_string(),
        )
        .await;

    let text = request_local_responses_stream(&fixture).await;

    assert!(text.contains("event: response.completed\n"));
    assert_eq!(text.matches("data: [DONE]").count(), 1);
}

#[tokio::test]
async fn local_responses_stream_malformed_tool_calls_fail_without_cache() {
    let fixture = support::AppFixture::with_mock_local().await;
    fixture
        .mock
        .respond_sse(
            "POST",
            "/v1/chat/completions",
            200,
            vec![
                r#"data: {"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call_duplicate","function":{"name":"calculate","arguments":"{}"}},{"index":1,"id":"call_duplicate","function":{"name":"calculate","arguments":"{}"}}]},"finish_reason":null}]}"#,
                r#"data: {"choices":[{"delta":{},"finish_reason":"tool_calls"}]}"#,
                "data: [DONE]",
            ],
        )
        .await;
    let response = router(fixture.state.clone())
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/responses")
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::json!({
                        "model": "qwen3-coder-30b-local",
                        "input": "calculate",
                        "stream": true,
                        "tools": [{
                            "type": "function",
                            "name": "calculate",
                            "parameters": {"type": "object"}
                        }]
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
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

    assert_eq!(
        text.matches("event: response.failed").count(),
        1,
        "{text:?}"
    );
    assert_eq!(
        text.matches("event: response.completed").count(),
        0,
        "{text:?}"
    );
    let failed_id = text
        .split("\n\n")
        .find_map(|frame| {
            let data = frame.lines().find_map(|line| line.strip_prefix("data: "))?;
            let event = serde_json::from_str::<Value>(data).ok()?;
            (event["type"] == "response.failed")
                .then(|| event["response"]["id"].as_str().map(str::to_string))?
        })
        .unwrap();
    assert_local_response_not_cached(&fixture, &failed_id).await;
    assert_eq!(fixture.mock.hits("POST", "/v1/chat/completions").await, 1);
    assert_eq!(fixture.mock.hits("POST", "/responses").await, 0);
    assert_eq!(fixture.mock.hits("GET", "/models").await, 0);
    assert_eq!(fixture.mock.hits("GET", "/copilot/token").await, 0);
}

#[tokio::test]
async fn responses_accepts_body_between_axum_default_and_configured_limit() {
    let fixture = support::AppFixture::with_mock_copilot().await;
    fixture
        .mock
        .respond_json(
            "POST",
            "/responses",
            200,
            serde_json::json!({
                "id": "resp_large",
                "object": "response",
                "status": "completed",
                "output": [],
                "usage": {"input_tokens": 1, "output_tokens": 1, "total_tokens": 2}
            }),
        )
        .await;

    let request_body = serde_json::to_vec(&serde_json::json!({
        "model": "gpt-5.5",
        "input": "x".repeat(2 * 1024 * 1024)
    }))
    .unwrap();
    assert!(request_body.len() > 2 * 1024 * 1024);
    assert!(request_body.len() < fixture.state.config.max_decoded_body_bytes as usize);

    let response = router(fixture.state)
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/responses")
                .header("content-type", "application/json")
                .body(Body::from(request_body))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let body = response_json(response).await;
    assert_eq!(body["id"], "resp_large");
}

#[tokio::test]
async fn responses_rejects_body_over_configured_limit_with_actionable_error() {
    let config = copilot_proxy_rs::AppConfig {
        max_decoded_body_bytes: 64,
        ..Default::default()
    };
    let response = router(copilot_proxy_rs::AppState::new(config))
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/responses")
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::to_vec(&serde_json::json!({
                        "model": "gpt-5.5",
                        "input": "x".repeat(128)
                    }))
                    .unwrap(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);
    let body = response_json(response).await;
    let message = body["error"]["message"].as_str().unwrap();
    assert!(message.contains("64 bytes"));
    assert!(message.contains("COPILOT_PROXY_RS_MAX_DECODED_BODY_BYTES"));
}

#[tokio::test]
async fn responses_passthrough_returns_live_response_and_caches_state() {
    let fixture = support::AppFixture::with_mock_copilot().await;
    fixture.mock.respond_json("POST", "/responses", 200, serde_json::json!({
        "id": "resp_1",
        "object": "response",
        "status": "completed",
        "output": [{"type":"message","role":"assistant","content":[{"type":"output_text","text":"hi"}]}],
        "usage": {"input_tokens": 1, "output_tokens": 1, "total_tokens": 2}
    })).await;

    let response = router(fixture.state.clone())
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/responses")
                .header("content-type", "application/json")
                .body(Body::from(r#"{"model":"gpt-5.5","input":"hello"}"#))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let body = response_json(response).await;
    assert_eq!(body["id"], "resp_1");

    // Verify the response was cached: a follow-up request with previous_response_id should
    // produce an upstream body where input is the expanded transcript (prior turn + new message),
    // not just the raw string "follow-up".
    fixture.mock.respond_json("POST", "/responses", 200, serde_json::json!({
        "id": "resp_2",
        "object": "response",
        "status": "completed",
        "output": [{"type":"message","role":"assistant","content":[{"type":"output_text","text":"there"}]}],
        "usage": {"input_tokens": 3, "output_tokens": 1, "total_tokens": 4}
    })).await;

    let response2 = router(fixture.state.clone())
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/responses")
                .header("content-type", "application/json")
                .body(Body::from(
                    r#"{"model":"gpt-5.5","input":"follow-up","previous_response_id":"resp_1"}"#,
                ))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response2.status(), StatusCode::OK);
    let body2 = response_json(response2).await;
    assert_eq!(body2["id"], "resp_2");

    // The upstream request should have received the full expanded transcript in `input`,
    // not a bare string, and `previous_response_id` should have been stripped.
    let upstream_body = fixture
        .mock
        .last_request_body_json("POST", "/responses")
        .await
        .expect("upstream did not receive a request body");
    assert!(
        upstream_body.get("previous_response_id").is_none(),
        "previous_response_id should be stripped before forwarding"
    );
    let input = upstream_body["input"]
        .as_array()
        .expect("upstream input should be an expanded array");
    // Prior turn: 1 user input + 1 assistant output, plus the new "follow-up" user message = 3 items.
    assert!(
        input.len() >= 3,
        "expanded input should contain prior transcript plus new message; got {} items",
        input.len()
    );
    let last_item = input.last().unwrap();
    assert_eq!(last_item["role"], "user");
    let text = last_item["content"][0]["text"].as_str().unwrap_or("");
    assert_eq!(text, "follow-up");
}

#[tokio::test]
async fn responses_refreshes_models_before_reasoning_adaptation() {
    let fixture = support::AppFixture::with_mock_copilot().await;
    fixture
        .mock
        .respond_json(
            "GET",
            "/models",
            200,
            serde_json::json!({
                "data": [{
                    "id": "gpt-live-responses",
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
                "id": "resp_live_reasoning",
                "object": "response",
                "status": "completed",
                "output": [],
                "usage": {"input_tokens": 1, "output_tokens": 1, "total_tokens": 2}
            }),
        )
        .await;

    let response = router(fixture.state.clone())
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/responses")
                .header("content-type", "application/json")
                .body(Body::from(
                    r#"{"model":"gpt-live-responses","input":"hello","reasoning":{"effort":"max"}}"#,
                ))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(fixture.mock.hits("GET", "/models").await, 1);
    let outbound = fixture
        .mock
        .last_request_body_json("POST", "/responses")
        .await
        .unwrap();
    assert_eq!(outbound["model"], "gpt-live-responses");
    assert_eq!(outbound["reasoning"]["effort"], "high");
}

#[tokio::test]
async fn responses_retrieve_and_cancel_proxy_to_copilot() {
    let fixture = support::AppFixture::with_mock_copilot().await;
    fixture
        .mock
        .respond_json(
            "GET",
            "/responses/resp_1",
            200,
            serde_json::json!({"id":"resp_1","status":"completed"}),
        )
        .await;
    fixture
        .mock
        .respond_json(
            "POST",
            "/responses/resp_1/cancel",
            200,
            serde_json::json!({"id":"resp_1","status":"cancelled"}),
        )
        .await;

    let app = router(fixture.state);
    let retrieve = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/v1/responses/resp_1")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(retrieve.status(), StatusCode::OK);
    let cancel = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/responses/resp_1/cancel")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(cancel.status(), StatusCode::OK);
}

#[tokio::test]
async fn local_response_resources_retrieval_is_rejected_without_copilot_work() {
    let fixture = support::AppFixture::with_mock_local().await;

    let response = router(fixture.state.clone())
        .oneshot(
            Request::builder()
                .uri("/v1/responses/resp_local_example")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let body = response_json(response).await;
    assert_eq!(body["error"]["type"], "invalid_request_error");
    assert_eq!(
        body["error"]["message"],
        "Retrieval and cancellation of local response resources are unsupported"
    );
    assert_eq!(
        fixture
            .mock
            .hits("GET", "/responses/resp_local_example")
            .await,
        0
    );
    assert_eq!(fixture.mock.hits("GET", "/models").await, 0);
    assert_eq!(fixture.mock.hits("GET", "/copilot/token").await, 0);
}

#[tokio::test]
async fn local_response_resources_cancellation_is_rejected_without_copilot_work() {
    let fixture = support::AppFixture::with_mock_local().await;

    let response = router(fixture.state.clone())
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/responses/resp_local_example/cancel")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let body = response_json(response).await;
    assert_eq!(body["error"]["type"], "invalid_request_error");
    assert_eq!(
        body["error"]["message"],
        "Retrieval and cancellation of local response resources are unsupported"
    );
    assert_eq!(
        fixture
            .mock
            .hits("POST", "/responses/resp_local_example/cancel")
            .await,
        0
    );
    assert_eq!(fixture.mock.hits("GET", "/models").await, 0);
    assert_eq!(fixture.mock.hits("GET", "/copilot/token").await, 0);
}

#[tokio::test]
async fn responses_streams_sse_passthrough() {
    let fixture = support::AppFixture::with_mock_copilot().await;
    fixture
        .mock
        .respond_sse(
            "POST",
            "/responses",
            200,
            vec![
                r#"event: response.created
data: {"type":"response.created","response":{"id":"resp_1","status":"in_progress"}}"#,
                r#"event: response.completed
data: {"type":"response.completed","response":{"id":"resp_1","status":"completed","output":[]}}"#,
            ],
        )
        .await;
    let response = router(fixture.state)
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/responses")
                .header("content-type", "application/json")
                .body(Body::from(
                    r#"{"model":"gpt-5.5","stream":true,"input":"hello"}"#,
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
    assert!(text.contains("response.completed"));
}

#[tokio::test]
async fn responses_http_clamps_reasoning_effort_and_preserves_unrelated_fields() {
    let fixture = support::AppFixture::with_mock_copilot().await;
    fixture
        .state
        .models
        .set_copilot_models(vec![serde_json::json!({
            "id": "gpt-responses-effort",
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
                "id": "resp_effort",
                "object": "response",
                "status": "completed",
                "output": [],
                "usage": {"input_tokens": 1, "output_tokens": 1, "total_tokens": 2}
            }),
        )
        .await;

    let response = router(fixture.state.clone())
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/responses")
                .header("content-type", "application/json")
                .body(Body::from(
                    r#"{"model":"gpt-responses-effort","input":"hello","reasoning":{"effort":"max","summary":"auto"},"metadata":{"keep":true}}"#,
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
    assert_eq!(outbound["model"], "gpt-responses-effort");
    assert_eq!(outbound["reasoning"]["effort"], "high");
    assert_eq!(outbound["reasoning"]["summary"], "auto");
    assert_eq!(outbound["metadata"]["keep"], true);
}

#[tokio::test]
async fn responses_preserves_gpt56_static_efforts_through_max() {
    let fixture = support::AppFixture::with_mock_copilot().await;
    fixture.state.models.set_copilot_models(vec![]).await;
    fixture
        .mock
        .respond_json(
            "POST",
            "/responses",
            200,
            serde_json::json!({
                "id": "resp_gpt56_effort",
                "object": "response",
                "status": "completed",
                "output": [],
                "usage": {"input_tokens": 1, "output_tokens": 1, "total_tokens": 2}
            }),
        )
        .await;

    let app = router(fixture.state.clone());
    let models = ["gpt-5.6-sol", "gpt-5.6-terra", "gpt-5.6-luna"];
    let efforts = ["low", "medium", "high", "xhigh", "max"];

    for model in models {
        for effort in efforts {
            let body = serde_json::json!({
                "model": model,
                "input": "hello",
                "reasoning": {"effort": effort}
            });
            let response = app
                .clone()
                .oneshot(
                    Request::builder()
                        .method("POST")
                        .uri("/v1/responses")
                        .header("content-type", "application/json")
                        .body(Body::from(body.to_string()))
                        .unwrap(),
                )
                .await
                .unwrap();

            assert_eq!(response.status(), StatusCode::OK, "{model} {effort}");
            let outbound = fixture
                .mock
                .last_request_body_json("POST", "/responses")
                .await
                .unwrap();
            assert_eq!(outbound["model"], model, "{model} {effort}");
            assert_eq!(outbound["reasoning"]["effort"], effort, "{model} {effort}");
        }
    }

    assert_eq!(fixture.mock.hits("GET", "/models").await, 0);
    assert_eq!(fixture.mock.hits("POST", "/responses").await, 15);
}

#[tokio::test]
async fn responses_http_strips_codex_image_generation_tool_before_copilot() {
    let fixture = support::AppFixture::with_mock_copilot().await;
    fixture
        .mock
        .respond_json(
            "POST",
            "/responses",
            200,
            serde_json::json!({
                "id": "resp_image_tool_stripped",
                "object": "response",
                "status": "completed",
                "output": [{"type":"message","role":"assistant","content":[{"type":"output_text","text":"ok"}]}],
                "usage": {"input_tokens": 1, "output_tokens": 1, "total_tokens": 2}
            }),
        )
        .await;

    let response = router(fixture.state.clone())
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/responses")
                .header("content-type", "application/json")
                .body(Body::from(
                    r#"{
                        "model":"gpt-5.5",
                        "input":"hello",
                        "tools":[
                            {"type":"image_generation","partial_images":1},
                            {"type":"function","name":"safe_tool","parameters":{"type":"object"}}
                        ],
                        "tool_choice":"auto",
                        "include":["image_generation_call.results","reasoning.encrypted_content"]
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
    assert_eq!(
        outbound["tools"],
        serde_json::json!([
            {"type": "function", "name": "safe_tool", "parameters": {"type": "object"}}
        ])
    );
    assert_eq!(outbound["tool_choice"], "auto");
    assert_eq!(
        outbound["include"],
        serde_json::json!(["reasoning.encrypted_content"])
    );
}

#[tokio::test]
async fn responses_preserves_explicit_prompt_cache_controls() {
    let fixture = support::AppFixture::with_mock_copilot().await;
    fixture
        .mock
        .respond_json(
            "POST",
            "/responses",
            200,
            serde_json::json!({
                "id": "resp_cache_controls",
                "object": "response",
                "status": "completed",
                "output": [],
                "usage": {"input_tokens": 1, "output_tokens": 1, "total_tokens": 2}
            }),
        )
        .await;

    let response = router(fixture.state.clone())
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/responses")
                .header("content-type", "application/json")
                .header("x-interaction-id", "conv-ignored")
                .body(Body::from(
                    r#"{
                        "model":"gpt-5.5",
                        "input":"hello",
                        "prompt_cache_key":"client-cache-key",
                        "prompt_cache_retention":"24h"
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
    assert_eq!(outbound["prompt_cache_key"], "client-cache-key");
    assert_eq!(outbound["prompt_cache_retention"], "24h");
}

#[tokio::test]
async fn responses_adds_stable_prompt_cache_key_from_conversation_header() {
    let fixture = support::AppFixture::with_mock_copilot().await;
    fixture
        .mock
        .respond_json(
            "POST",
            "/responses",
            200,
            serde_json::json!({
                "id": "resp_cache_key",
                "object": "response",
                "status": "completed",
                "output": [],
                "usage": {"input_tokens": 1, "output_tokens": 1, "total_tokens": 2}
            }),
        )
        .await;

    let response = router(fixture.state.clone())
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/responses")
                .header("content-type", "application/json")
                .header("x-interaction-id", "conv-123")
                .body(Body::from(r#"{"model":"gpt-5.5","input":"hello"}"#))
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
    assert_eq!(outbound["prompt_cache_key"], "conv-123:gpt-5.5");
}

#[tokio::test]
async fn responses_http_strips_reasoning_effort_for_unsupported_model() {
    let fixture = support::AppFixture::with_mock_copilot().await;
    fixture
        .mock
        .respond_json(
            "POST",
            "/responses",
            200,
            serde_json::json!({
                "id": "resp_no_effort",
                "object": "response",
                "status": "completed",
                "output": [],
                "usage": {"input_tokens": 1, "output_tokens": 1, "total_tokens": 2}
            }),
        )
        .await;

    let response = router(fixture.state.clone())
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/responses")
                .header("content-type", "application/json")
                .body(Body::from(
                    r#"{"model":"gpt-no-effort","input":"hello","reasoning":{"effort":"high"}}"#,
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
    assert_eq!(outbound["model"], "gpt-no-effort");
    assert!(outbound.get("reasoning").is_none());
}

async fn response_json(response: axum::response::Response) -> Value {
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    serde_json::from_slice(&bytes).unwrap()
}

#[tokio::test]
async fn responses_logs_cache_and_usage_safe_metadata() {
    let fixture = support::AppFixture::with_mock_copilot().await;
    fixture
        .mock
        .respond_json(
            "GET",
            "/models",
            200,
            serde_json::json!({"data": [{"id": "gpt-5.5", "owned_by": "openai"}]}),
        )
        .await;
    fixture.mock.respond_json("POST", "/responses", 200, serde_json::json!({
        "id": "resp_log_1",
        "object": "response",
        "status": "completed",
        "output": [{"type":"function_call","name":"hidden","arguments":"{\"secret\":true}"}],
        "usage": {
            "input_tokens": 5,
            "output_tokens": 2,
            "total_tokens": 7,
            "input_tokens_details": {"cached_tokens": 3}
        }
    })).await;

    let request_body = r#"{"model":"gpt-5.5","input":"private prompt"}"#;
    let events = with_event_capture(|| async {
        let response = router(fixture.state.clone())
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/v1/responses")
                    .header("content-type", "application/json")
                    .body(Body::from(request_body))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
    })
    .await;

    let request_body_bytes = request_body.len().to_string();
    let request_body_limit = (16 * 1024 * 1024).to_string();
    assert_eq!(
        field(&events, "responses request prepared", "api.family").as_deref(),
        Some("responses")
    );
    assert_eq!(
        field(&events, "responses request prepared", "request.body.bytes").as_deref(),
        Some(request_body_bytes.as_str())
    );
    assert_eq!(
        field(&events, "responses request prepared", "request.body.limit").as_deref(),
        Some(request_body_limit.as_str())
    );
    assert!(
        field(
            &events,
            "responses request prepared",
            "request.body.effective_bytes"
        )
        .and_then(|value| value.parse::<usize>().ok())
        .is_some_and(|bytes| bytes >= request_body.len())
    );
    assert_eq!(
        field(&events, "responses cache event", "cache.operation").as_deref(),
        Some("write")
    );
    assert_eq!(
        field(&events, "copilot request completed", "http.status_code").as_deref(),
        Some("200")
    );
    assert_eq!(
        field(&events, "copilot usage", "tokens.input").as_deref(),
        Some("5")
    );
    let rendered = format!("{events:?}");
    assert!(!rendered.contains("private prompt"));
    assert!(!rendered.contains("secret"));
    assert!(!rendered.contains("hidden"));
}
