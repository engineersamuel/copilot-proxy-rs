mod support;

use support::log_capture::{field, with_event_capture};

use axum::body::Body;
use http::{Request, StatusCode};
use http_body_util::BodyExt;
use serde_json::Value;
use tower::ServiceExt;

use copilot_proxy_rs::http::router;

#[tokio::test]
async fn chat_completions_routes_configured_local_model_without_copilot_refresh() {
    let fixture = support::AppFixture::with_mock_local().await;
    fixture
        .mock
        .respond_json(
            "POST",
            "/v1/chat/completions",
            200,
            serde_json::json!({
                "id": "chatcmpl-local",
                "object": "chat.completion",
                "model": r"models\Qwen3-Coder-30B-A3B-Instruct-IQ4_XS.gguf",
                "choices": [{"message": {"role": "assistant", "content": "local ok"}}]
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
                    r#"{"model":"qwen3-coder-30b-local","messages":[{"role":"user","content":"Hello"}]}"#,
                ))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let body = response_json(response).await;
    assert_eq!(body["model"], "qwen3-coder-30b-local");
    let outbound = fixture
        .mock
        .last_request_body_json("POST", "/v1/chat/completions")
        .await
        .unwrap();
    assert_eq!(
        outbound["model"],
        r"models\Qwen3-Coder-30B-A3B-Instruct-IQ4_XS.gguf"
    );
    assert_eq!(fixture.mock.hits("POST", "/v1/chat/completions").await, 1);
    assert_eq!(fixture.mock.hits("GET", "/models").await, 0);
    assert_eq!(fixture.mock.hits("GET", "/copilot/token").await, 0);
}

#[tokio::test]
async fn chat_completions_stream_rewrites_local_model_to_public_id() {
    let fixture = support::AppFixture::with_mock_local().await;
    let chunk_one = concat!(
        r#"data: {"id":"chatcmpl-local","object":"chat.completion.chunk","model":"models\\Qwen3-Coder-30B-A3B-Instruct-IQ4_XS.gguf","choices":[{"delta":{"content":"local"}}]}"#,
        "\r\n\r\n",
        r#"data: {"id":"chatcmpl-local","object":"chat.completion.chunk","model":"models\\Qwen3-Coder-30B-A3B-Instruct-IQ4_XS.gguf","choices":[{"delta":{"tool_calls":[{"index":0,"function":{"name":"wea"#,
    );
    let chunk_two = concat!(r#"ther"}}]}}]}"#, "\r\n\r\n", "data: [DONE]\r",);
    fixture
        .mock
        .respond_sse_split_chunks(
            "POST",
            "/v1/chat/completions",
            200,
            vec![chunk_one.as_bytes(), chunk_two.as_bytes(), b"\n\r\n"],
        )
        .await;

    let response = router(fixture.state.clone())
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/chat/completions")
                .header("content-type", "application/json")
                .body(Body::from(
                    r#"{"model":"qwen3-coder-30b-local","stream":true,"messages":[{"role":"user","content":"Hello"}]}"#,
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
    let events: Vec<_> = text
        .split("\n\n")
        .filter(|event| !event.is_empty())
        .collect();
    assert_eq!(
        events.len(),
        3,
        "every SSE event must retain its delimiter: {text:?}"
    );
    for event in &events[..2] {
        let payload = event.strip_prefix("data: ").unwrap();
        let chunk: Value = serde_json::from_str(payload).unwrap();
        assert_eq!(chunk["model"], "qwen3-coder-30b-local", "{event}");
        assert!(
            !event.contains("Qwen3-Coder-30B-A3B-Instruct-IQ4_XS.gguf"),
            "{event}"
        );
    }
    assert_eq!(events[2], "data: [DONE]");
    assert_eq!(text.matches("\n\n").count(), 3, "{text:?}");
    assert_eq!(fixture.mock.hits("GET", "/models").await, 0);
    assert_eq!(fixture.mock.hits("GET", "/copilot/token").await, 0);
}

#[tokio::test]
async fn local_chat_rejects_scalar_success_response() {
    let fixture = support::AppFixture::with_mock_local().await;
    fixture
        .mock
        .respond_json("POST", "/v1/chat/completions", 200, serde_json::json!("ok"))
        .await;

    let response = request_local_chat(fixture.state, false).await;

    assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
    let body = response_json(response).await;
    assert_eq!(body["error"]["type"], "server_error");
    assert_eq!(
        body["error"]["message"],
        "local model returned invalid response"
    );
}

#[tokio::test]
async fn local_chat_rejects_array_success_response() {
    let fixture = support::AppFixture::with_mock_local().await;
    fixture
        .mock
        .respond_json("POST", "/v1/chat/completions", 200, serde_json::json!([]))
        .await;

    let response = request_local_chat(fixture.state, false).await;

    assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
    let body = response_json(response).await;
    assert_eq!(body["error"]["type"], "server_error");
    assert_eq!(
        body["error"]["message"],
        "local model returned invalid response"
    );
}

#[tokio::test]
async fn local_chat_rejects_non_sse_stream_response() {
    let fixture = support::AppFixture::with_mock_local().await;
    fixture
        .mock
        .respond_json(
            "POST",
            "/v1/chat/completions",
            200,
            serde_json::json!({"model": "upstream-model"}),
        )
        .await;

    let response = request_local_chat(fixture.state, true).await;

    assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
    let body = response_json(response).await;
    assert_eq!(body["error"]["type"], "server_error");
    assert_eq!(
        body["error"]["message"],
        "local model returned invalid response"
    );
}

#[tokio::test]
async fn local_chat_accepts_sse_content_type_parameters() {
    let fixture = support::AppFixture::with_mock_local().await;
    fixture
        .mock
        .respond_text(
            "POST",
            "/v1/chat/completions",
            200,
            "text/event-stream; charset=utf-8",
            concat!(
                r#"data: {"model":"models\\Qwen3-Coder-30B-A3B-Instruct-IQ4_XS.gguf","choices":[]}"#,
                "\n\n",
                "data: [DONE]\n\n",
            )
            .to_string(),
        )
        .await;

    let response = request_local_chat(fixture.state, true).await;

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
    assert!(
        text.contains(r#""model":"qwen3-coder-30b-local""#),
        "{text}"
    );
    assert!(text.contains("data: [DONE]\n\n"), "{text:?}");
}

#[tokio::test]
async fn local_chat_stream_surfaces_malformed_data_json_as_body_error() {
    let fixture = support::AppFixture::with_mock_local().await;
    fixture
        .mock
        .respond_sse(
            "POST",
            "/v1/chat/completions",
            200,
            vec![r#"data: {"model":"models\\Qwen3-Coder-30B-A3B-Instruct-IQ4_XS.gguf"#],
        )
        .await;

    let response = request_local_chat(fixture.state, true).await;

    assert_eq!(response.status(), StatusCode::OK);
    assert!(response.into_body().collect().await.is_err());
}

#[tokio::test]
async fn local_chat_stream_surfaces_scalar_data_as_body_error() {
    let fixture = support::AppFixture::with_mock_local().await;
    fixture
        .mock
        .respond_sse(
            "POST",
            "/v1/chat/completions",
            200,
            vec![r#"data: "models\\Qwen3-Coder-30B-A3B-Instruct-IQ4_XS.gguf""#],
        )
        .await;

    let response = request_local_chat(fixture.state, true).await;

    assert_eq!(response.status(), StatusCode::OK);
    assert!(response.into_body().collect().await.is_err());
}

#[tokio::test]
async fn local_chat_stream_surfaces_array_data_as_body_error() {
    let fixture = support::AppFixture::with_mock_local().await;
    fixture
        .mock
        .respond_sse(
            "POST",
            "/v1/chat/completions",
            200,
            vec![r#"data: [{"model":"models\\Qwen3-Coder-30B-A3B-Instruct-IQ4_XS.gguf"}]"#],
        )
        .await;

    let response = request_local_chat(fixture.state, true).await;

    assert_eq!(response.status(), StatusCode::OK);
    assert!(response.into_body().collect().await.is_err());
}

#[tokio::test]
async fn local_chat_connection_failure_returns_502_without_copilot_fallback() {
    let fixture = support::AppFixture::with_local_base_url("http://127.0.0.1:1/v1").await;

    let response = router(fixture.state.clone())
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/chat/completions")
                .header("content-type", "application/json")
                .body(Body::from(
                    r#"{"model":"qwen3-coder-30b-local","messages":[{"role":"user","content":"Hello"}]}"#,
                ))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
    let body = response_json(response).await;
    assert_eq!(body["error"]["type"], "server_error");
    assert_eq!(fixture.mock.hits("GET", "/models").await, 0);
    assert_eq!(fixture.mock.hits("GET", "/copilot/token").await, 0);
}

#[tokio::test]
async fn local_chat_preserves_429_with_bounded_upstream_detail_without_copilot_fallback() {
    let fixture = support::AppFixture::with_mock_local().await;
    let prefix = "local backend overloaded: ";
    let upstream_detail = format!("{prefix}{}", "x".repeat(8_192));
    fixture
        .mock
        .respond_sse_split_chunks(
            "POST",
            "/v1/chat/completions",
            429,
            vec![upstream_detail.as_bytes()],
        )
        .await;

    let response = router(fixture.state.clone())
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/chat/completions")
                .header("content-type", "application/json")
                .body(Body::from(
                    r#"{"model":"qwen3-coder-30b-local","messages":[{"role":"user","content":"Hello"}]}"#,
                ))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
    let body = response_json(response).await;
    assert_eq!(body["error"]["type"], "server_error");
    let detail = body["error"]["message"].as_str().unwrap();
    assert!(detail.starts_with(prefix));
    assert!(detail.chars().count() <= 4_096);
    assert_eq!(fixture.mock.hits("POST", "/v1/chat/completions").await, 1);
    assert_eq!(fixture.mock.hits("GET", "/models").await, 0);
    assert_eq!(fixture.mock.hits("GET", "/copilot/token").await, 0);
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
    assert_eq!(body["model"], "gpt-5.5");
    assert!(body["created"].as_u64().is_some());
    assert_eq!(body["choices"][0]["message"]["content"], "bridged ok");
    assert_eq!(body["usage"]["prompt_tokens"], 3);
    assert_eq!(body["usage"]["completion_tokens"], 2);
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
async fn chat_completions_passes_through_gpt55_without_endpoint_metadata() {
    let fixture = support::AppFixture::with_mock_copilot().await;
    fixture
        .mock
        .respond_json(
            "POST",
            "/chat/completions",
            200,
            serde_json::json!({
                "choices": [{"message": {"role": "assistant", "content": "pass through ok"}}],
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
    assert_eq!(fixture.mock.hits("POST", "/chat/completions").await, 1);
    assert_eq!(fixture.mock.hits("POST", "/responses").await, 0);
    let body = response_json(response).await;
    assert_eq!(body["choices"][0]["message"]["content"], "pass through ok");
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
                r#"event: response.created
data: {"type":"response.created","response":{"id":"resp_stream","model":"gpt-5.5","usage":{"input_tokens":1,"output_tokens":0,"total_tokens":1}}}"#,
                r#"event: response.output_item.added
data: {"type":"response.output_item.added","output_index":0,"item":{"type":"message","role":"assistant","content":[]}}"#,
                r#"event: response.output_text.delta
data: {"type":"response.output_text.delta","output_index":0,"content_index":0,"delta":"bridge"}"#,
                r#"event: response.output_text.delta
data: {"type":"response.output_text.delta","output_index":0,"content_index":0,"delta":" stream"}"#,
                r#"event: response.completed
data: {"type":"response.completed","response":{"id":"resp_stream","object":"response","status":"completed","output":[],"usage":{"input_tokens":1,"output_tokens":2,"total_tokens":3}}}"#,
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
    assert!(
        !text.lines().any(|line| line.starts_with("event:")),
        "{text}"
    );
    assert_eq!(text.matches("data: [DONE]").count(), 1, "{text}");
    let chunks = chat_sse_chunks(&text);
    assert_eq!(chunks.len(), 4, "{text}");
    let chunk_id = chunks[0]["id"].as_str().unwrap();
    let created = chunks[0]["created"].as_u64().unwrap();
    assert!(!chunk_id.is_empty());
    assert!(created > 0);
    assert!(chunks.iter().all(|chunk| chunk["id"] == chunk_id));
    assert!(chunks.iter().all(|chunk| chunk["created"] == created));
    assert!(chunks.iter().all(|chunk| chunk["model"] == "gpt-5.5"));
    assert!(
        chunks
            .iter()
            .all(|chunk| chunk["object"] == "chat.completion.chunk")
    );
    assert_eq!(chunks[0]["choices"][0]["delta"]["role"], "assistant");
    assert_eq!(chunks[1]["choices"][0]["delta"]["content"], "bridge");
    assert_eq!(chunks[2]["choices"][0]["delta"]["content"], " stream");
    assert_eq!(chunks[3]["choices"][0]["finish_reason"], "stop");
    assert_eq!(chunks[3]["usage"]["prompt_tokens"], 1);
    assert_eq!(chunks[3]["usage"]["completion_tokens"], 2);
    assert_eq!(chunks[3]["usage"]["total_tokens"], 3);
    let outbound = fixture
        .mock
        .last_request_body_json("POST", "/responses")
        .await
        .unwrap();
    assert_eq!(outbound["model"], "gpt-5.5");
    assert_eq!(outbound["reasoning"]["effort"], "high");
}

#[tokio::test]
async fn chat_completions_stream_translates_responses_function_calls() {
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
        .respond_sse(
            "POST",
            "/responses",
            200,
            vec![
                r#"event: response.created
data: {"type":"response.created","response":{"id":"resp_tool","model":"gpt-5.5"}}"#,
                r#"event: response.output_item.added
data: {"type":"response.output_item.added","output_index":0,"item":{"type":"function_call","call_id":"call_1","name":"get_weather","arguments":""}}"#,
                r#"event: response.function_call_arguments.delta
data: {"type":"response.function_call_arguments.delta","output_index":0,"call_id":"call_1","delta":"{\"city\":"}"#,
                r#"event: response.function_call_arguments.delta
data: {"type":"response.function_call_arguments.delta","output_index":0,"call_id":"call_1","delta":"\"Seattle\"}"}"#,
                r#"event: response.completed
data: {"type":"response.completed","response":{"id":"resp_tool","status":"completed","output":[{"type":"function_call","call_id":"call_1","name":"get_weather","arguments":"{\"city\":\"Seattle\"}"}],"usage":{"input_tokens":8,"output_tokens":4,"total_tokens":12}}}"#,
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
                    r#"{
                        "model":"gpt-5.5",
                        "stream":true,
                        "max_tokens":64,
                        "messages":[{"role":"user","content":"weather"}],
                        "tools":[{
                            "type":"function",
                            "function":{
                                "name":"get_weather",
                                "description":"Get weather",
                                "parameters":{"type":"object","properties":{"city":{"type":"string"}}}
                            }
                        }],
                        "tool_choice":"auto",
                        "parallel_tool_calls":false
                    }"#,
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
    assert!(
        !text.lines().any(|line| line.starts_with("event:")),
        "{text}"
    );
    assert_eq!(text.matches("data: [DONE]").count(), 1, "{text}");
    let chunks = chat_sse_chunks(&text);
    assert_eq!(chunks.len(), 5, "{text}");
    for chunk in &chunks[1..4] {
        assert_eq!(chunk["choices"][0]["delta"]["tool_calls"][0]["index"], 0);
    }
    assert_eq!(
        chunks[1]["choices"][0]["delta"]["tool_calls"][0]["id"],
        "call_1"
    );
    assert_eq!(
        chunks[1]["choices"][0]["delta"]["tool_calls"][0]["function"]["name"],
        "get_weather"
    );
    assert_eq!(
        chunks[2]["choices"][0]["delta"]["tool_calls"][0]["function"]["arguments"],
        "{\"city\":"
    );
    assert_eq!(
        chunks[3]["choices"][0]["delta"]["tool_calls"][0]["function"]["arguments"],
        "\"Seattle\"}"
    );
    assert_eq!(chunks[4]["choices"][0]["finish_reason"], "tool_calls");
    assert_eq!(chunks[4]["usage"]["total_tokens"], 12);

    let outbound = fixture
        .mock
        .last_request_body_json("POST", "/responses")
        .await
        .unwrap();
    assert_eq!(outbound["tools"][0]["type"], "function");
    assert_eq!(outbound["tools"][0]["name"], "get_weather");
    assert_eq!(
        outbound["tools"][0]["parameters"]["properties"]["city"]["type"],
        "string"
    );
    assert_eq!(outbound["tool_choice"], "auto");
    assert_eq!(outbound["parallel_tool_calls"], false);
    assert_eq!(outbound["max_output_tokens"], 64);
}

#[tokio::test]
async fn chat_completions_stream_remaps_responses_tool_index_after_text() {
    let fixture = support::AppFixture::with_mock_copilot().await;
    fixture
        .state
        .models
        .set_copilot_models(vec![serde_json::json!({
            "id": "gpt-6.1-sol",
            "owned_by": "openai",
            "supported_endpoints": ["/responses"]
        })])
        .await;
    fixture
        .mock
        .respond_sse(
            "POST",
            "/responses",
            200,
            vec![
                r#"data: {"type":"response.created","response":{"id":"resp_preamble"}}"#,
                r#"data: {"type":"response.output_item.added","output_index":0,"item":{"type":"message","role":"assistant","content":[]}}"#,
                r#"data: {"type":"response.output_text.delta","output_index":0,"delta":"Checking the weather."}"#,
                r#"data: {"type":"response.output_item.added","output_index":1,"item":{"type":"function_call","call_id":"call_weather","name":"get_weather","arguments":""}}"#,
                r#"data: {"type":"response.function_call_arguments.delta","output_index":1,"delta":"{\"city\":"}"#,
                r#"data: {"type":"response.function_call_arguments.delta","output_index":1,"delta":"\"Seattle\"}"}"#,
                r#"data: {"type":"response.completed","response":{"status":"completed","output":[{"type":"function_call","call_id":"call_weather","name":"get_weather","arguments":"{\"city\":\"Seattle\"}"}],"usage":{"input_tokens":8,"output_tokens":4,"total_tokens":12}}}"#,
                "data: [DONE]",
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
                    serde_json::json!({
                        "model": "gpt-6.1-sol",
                        "stream": true,
                        "messages": [{"role":"user","content":"Check Seattle weather"}],
                        "tools": [{
                            "type":"function",
                            "function":{"name":"get_weather","parameters":{"type":"object"}}
                        }]
                    })
                    .to_string(),
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
    let chunks = chat_sse_chunks(&text);
    assert_eq!(chunks.len(), 6, "{text}");
    assert_eq!(chunks[0]["choices"][0]["delta"]["role"], "assistant");
    assert_eq!(
        chunks[1]["choices"][0]["delta"]["content"],
        "Checking the weather."
    );
    let mut arguments = String::new();
    for chunk in &chunks[2..5] {
        let call = &chunk["choices"][0]["delta"]["tool_calls"][0];
        assert_eq!(call["index"], 0, "{text}");
        assert_eq!(call["id"], "call_weather");
        assert_eq!(call["type"], "function");
        arguments.push_str(call["function"]["arguments"].as_str().unwrap());
    }
    assert_eq!(arguments, r#"{"city":"Seattle"}"#);
    assert_eq!(
        chunks[2]["choices"][0]["delta"]["tool_calls"][0]["function"]["name"],
        "get_weather"
    );
    assert!(chunks.iter().all(|chunk| chunk["model"] == "gpt-6.1-sol"));
    assert!(chunks.iter().all(|chunk| chunk["id"] == chunks[0]["id"]));
    assert!(
        chunks
            .iter()
            .all(|chunk| chunk["created"] == chunks[0]["created"])
    );
    assert_eq!(chunks[5]["choices"][0]["finish_reason"], "tool_calls");
    assert_eq!(
        chunks[5]["usage"],
        serde_json::json!({"prompt_tokens":8,"completion_tokens":4,"total_tokens":12})
    );
    assert_eq!(text.matches("data: [DONE]").count(), 1);
    assert_eq!(fixture.mock.hits("POST", "/responses").await, 1);
    assert_eq!(fixture.mock.hits("POST", "/chat/completions").await, 0);
}

#[tokio::test]
async fn chat_completions_stream_rejects_unknown_responses_tool_index() {
    let fixture = support::AppFixture::with_mock_copilot().await;
    fixture
        .state
        .models
        .set_copilot_models(vec![serde_json::json!({
            "id": "gpt-6.1-sol",
            "supported_endpoints": ["/responses"]
        })])
        .await;
    fixture
        .mock
        .respond_sse(
            "POST",
            "/responses",
            200,
            vec![
                r#"data: {"type":"response.output_item.added","output_index":7,"item":{"type":"function_call","call_id":"call_weather","name":"get_weather","arguments":""}}"#,
                r#"data: {"type":"response.function_call_arguments.delta","output_index":0,"call_id":"call_weather","delta":"{}"}"#,
                r#"data: {"type":"response.completed","response":{"output":[]}}"#,
                "data: [DONE]",
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
                    r#"{"model":"gpt-6.1-sol","stream":true,"messages":[{"role":"user","content":"weather"}],"tools":[{"type":"function","function":{"name":"get_weather","parameters":{"type":"object"}}}]}"#,
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let mut body = response.into_body();
    let mut text = String::new();
    let mut failed = false;
    while let Some(frame) = body.frame().await {
        match frame {
            Ok(frame) => {
                if let Some(bytes) = frame.data_ref() {
                    text.push_str(std::str::from_utf8(bytes).unwrap());
                }
            }
            Err(_) => {
                failed = true;
                break;
            }
        }
    }
    assert!(failed, "unknown upstream index must fail the body: {text}");
    assert!(body.frame().await.is_none());
    assert!(!text.contains("[DONE]"), "{text}");
    assert!(!text.contains(r#""finish_reason":"tool_calls""#), "{text}");
    let chunks = chat_sse_chunks(&text);
    assert_eq!(chunks.len(), 2, "{text}");
    assert_eq!(
        chunks[1]["choices"][0]["delta"]["tool_calls"][0]["index"],
        0
    );
    assert_eq!(
        chunks[1]["choices"][0]["delta"]["tool_calls"][0]["function"]["arguments"],
        ""
    );
    assert_eq!(fixture.mock.hits("POST", "/responses").await, 1);
    assert_eq!(fixture.mock.hits("POST", "/chat/completions").await, 0);
}

#[tokio::test]
async fn chat_completions_translates_responses_function_calls_without_streaming() {
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
                "id": "resp_tool",
                "object": "response",
                "status": "completed",
                "output": [{
                    "type":"function_call",
                    "call_id":"call_1",
                    "name":"get_weather",
                    "arguments":"{\"city\":\"Seattle\"}"
                }],
                "usage":{"input_tokens":8,"output_tokens":4,"total_tokens":12}
            }),
        )
        .await;

    let response = router(fixture.state)
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/chat/completions")
                .header("content-type", "application/json")
                .body(Body::from(
                    r#"{
                        "model":"gpt-5.5",
                        "messages":[
                            {"role":"assistant","content":null,"tool_calls":[{
                                "id":"call_previous",
                                "type":"function",
                                "function":{"name":"get_weather","arguments":"{\"city\":\"Portland\"}"}
                            }]},
                            {"role":"tool","tool_call_id":"call_previous","content":"rain"},
                            {"role":"user","content":"Check Seattle"}
                        ]
                    }"#,
                ))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let body = response_json(response).await;
    assert_eq!(body["model"], "gpt-5.5");
    assert_eq!(body["choices"][0]["finish_reason"], "tool_calls");
    assert_eq!(body["choices"][0]["message"]["content"], Value::Null);
    assert_eq!(
        body["choices"][0]["message"]["tool_calls"][0]["function"]["arguments"],
        "{\"city\":\"Seattle\"}"
    );
    let outbound = fixture
        .mock
        .last_request_body_json("POST", "/responses")
        .await
        .unwrap();
    assert_eq!(outbound["input"][0]["type"], "function_call");
    assert_eq!(outbound["input"][0]["call_id"], "call_previous");
    assert_eq!(outbound["input"][1]["type"], "function_call_output");
    assert_eq!(outbound["input"][1]["call_id"], "call_previous");
    assert_eq!(outbound["input"][1]["output"], "rain");
    assert_eq!(outbound["input"][2]["role"], "user");
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

fn chat_sse_chunks(text: &str) -> Vec<Value> {
    text.lines()
        .filter_map(|line| line.strip_prefix("data: "))
        .filter(|payload| *payload != "[DONE]")
        .map(|payload| serde_json::from_str(payload).unwrap())
        .collect()
}

async fn request_local_chat(
    state: copilot_proxy_rs::state::AppState,
    stream: bool,
) -> axum::response::Response {
    let body = serde_json::json!({
        "model": "qwen3-coder-30b-local",
        "stream": stream,
        "messages": [{"role": "user", "content": "Hello"}],
    });
    router(state)
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/chat/completions")
                .header("content-type", "application/json")
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap()
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

// ─── Decisions and image models ──────────────────────────────────────────────

fn sky_chat_request(model: &str, stream: bool) -> Value {
    decision_chat_request(model, support::SKY_QUESTION, stream)
}

fn decision_chat_request(model: &str, question: &str, stream: bool) -> Value {
    serde_json::json!({
        "model": model,
        "stream": stream,
        "temperature": 0.2,
        "messages": [
            {"role": "system", "content": "You are helpful"},
            {"role": "user", "content": question}
        ]
    })
}

async fn assert_chat_decision_options(model: &str, stream: bool) {
    let fixture = support::AppFixture::with_decision_option_answers().await;
    let (status, _, body) = support::send_json(
        &fixture.state,
        "/v1/chat/completions",
        decision_chat_request(model, support::OPTIONS_QUESTION, stream),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let text = if stream {
        support::sse_data(&body)
            .iter()
            .filter_map(|chunk| chunk["choices"][0]["delta"]["content"].as_str())
            .collect::<String>()
    } else {
        let body: Value = serde_json::from_str(&body).unwrap();
        body["choices"][0]["message"]["content"]
            .as_str()
            .unwrap_or_default()
            .to_string()
    };
    assert_eq!(text, support::expected_options_answer(model));
    support::assert_decision_options_upstream(&fixture.mock, model).await;
}

#[tokio::test]
async fn chat_decisions_options_non_stream_asks_one_question_per_option() {
    assert_chat_decision_options("gpt-6-luna-decisions", false).await;
}

#[tokio::test]
async fn chat_decisions_options_stream_asks_one_question_per_option() {
    assert_chat_decision_options("gpt-6-luna-decisions", true).await;
}

#[tokio::test]
async fn chat_systemone_options_non_stream_asks_one_question_per_option() {
    assert_chat_decision_options("microsoft-decision-1", false).await;
}

#[tokio::test]
async fn chat_systemone_options_stream_asks_one_question_per_option() {
    assert_chat_decision_options("microsoft-decision-1", true).await;
}

#[tokio::test]
async fn chat_decisions_options_missing_option_answer_is_502() {
    let fixture = support::AppFixture::with_nonchat_models().await;
    fixture
        .mock
        .respond_json(
            "POST",
            "/v1/decisions",
            200,
            serde_json::json!({"model": "gpt-6-luna", "answers": [
                {"type": "predicate", "name": "option_a", "probability": 0.4}
            ]}),
        )
        .await;
    let (status, _, body) = support::send_json(
        &fixture.state,
        "/v1/chat/completions",
        decision_chat_request("gpt-6-luna-decisions", support::OPTIONS_QUESTION, false),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_GATEWAY, "{body}");
    let body: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(
        body["error"]["message"],
        "Copilot decisions response was malformed"
    );
}

#[tokio::test]
async fn chat_decisions_refusal_is_explained_as_bad_request() {
    let fixture = support::AppFixture::with_nonchat_models().await;
    let recorded = support::fixture_json("copilot_decisions_predicate.json");
    fixture
        .mock
        .respond_json(
            "POST",
            "/v1/decisions",
            200,
            recorded["refusal_messages_input"]["response"].clone(),
        )
        .await;
    let (status, _, body) = support::send_json(
        &fixture.state,
        "/v1/chat/completions",
        decision_chat_request("gpt-6-luna-decisions", "Which lunch spot is best?", false),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    let body: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(body["error"]["type"], "invalid_request_error");
    assert_eq!(
        body["error"]["message"],
        "Copilot decisions model declined to answer; ask a yes/no question or name options such as 'Option A: ...' and 'Option B: ...'"
    );
}

async fn assert_chat_decision_answer(model: &str, expected_usage: Value) {
    let fixture = support::AppFixture::with_decision_models().await;
    let (status, _, body) = support::send_json(
        &fixture.state,
        "/v1/chat/completions",
        sky_chat_request(model, false),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let body: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(body["object"], "chat.completion");
    assert_eq!(body["model"], model);
    assert_eq!(body["choices"][0]["message"]["role"], "assistant");
    assert_eq!(
        body["choices"][0]["message"]["content"],
        "Yes (probability 0.99)"
    );
    assert_eq!(body["choices"][0]["finish_reason"], "stop");
    assert_eq!(body["usage"], expected_usage);
    support::assert_decision_upstream(&fixture.mock, model, 1).await;
}

async fn assert_chat_decision_stream(model: &str) {
    let fixture = support::AppFixture::with_decision_models().await;
    let (status, content_type, body) = support::send_json(
        &fixture.state,
        "/v1/chat/completions",
        sky_chat_request(model, true),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(
        content_type.starts_with("text/event-stream"),
        "{content_type}"
    );
    assert!(body.trim_end().ends_with("data: [DONE]"), "{body}");
    let chunks = support::sse_data(&body);
    assert!(chunks.iter().all(|chunk| chunk["model"] == model), "{body}");
    assert!(
        chunks
            .iter()
            .all(|chunk| chunk["object"] == "chat.completion.chunk")
    );
    assert_eq!(chunks[0]["choices"][0]["delta"]["role"], "assistant");
    let text: String = chunks
        .iter()
        .filter_map(|chunk| chunk["choices"][0]["delta"]["content"].as_str())
        .collect();
    assert_eq!(text, "Yes (probability 0.99)");
    let finish = chunks
        .iter()
        .find_map(|chunk| chunk["choices"][0]["finish_reason"].as_str());
    assert_eq!(finish, Some("stop"));
    support::assert_decision_upstream(&fixture.mock, model, 1).await;
}

#[tokio::test]
async fn chat_decisions_model_non_stream_answers_from_decisions_endpoint() {
    assert_chat_decision_answer(
        "gpt-6-luna-decisions",
        serde_json::json!({"prompt_tokens": 173, "completion_tokens": 0, "total_tokens": 173}),
    )
    .await;
}

#[tokio::test]
async fn chat_decisions_model_stream_single_chunk_sse() {
    assert_chat_decision_stream("gpt-6-luna-decisions").await;
}

#[tokio::test]
async fn chat_systemone_model_non_stream_answers_from_systemone_endpoint() {
    assert_chat_decision_answer(
        "microsoft-decision-1",
        serde_json::json!({"prompt_tokens": 51, "completion_tokens": 1, "total_tokens": 52}),
    )
    .await;
}

#[tokio::test]
async fn chat_systemone_model_stream_single_chunk_sse() {
    assert_chat_decision_stream("microsoft-decision-1").await;
}

#[tokio::test]
async fn chat_decisions_no_answer_probability_is_502() {
    let fixture = support::AppFixture::with_nonchat_models().await;
    fixture
        .mock
        .respond_json(
            "POST",
            "/v1/decisions",
            200,
            serde_json::json!({"model": "gpt-6-luna", "answers": []}),
        )
        .await;
    let (status, _, body) = support::send_json(
        &fixture.state,
        "/v1/chat/completions",
        sky_chat_request("gpt-6-luna-decisions", false),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_GATEWAY, "{body}");
    let body: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(body["error"]["type"], "server_error");
    assert_eq!(
        body["error"]["message"],
        "Copilot decisions response was malformed"
    );
}

#[tokio::test]
async fn chat_decisions_upstream_400_is_sanitized() {
    let fixture = support::AppFixture::with_nonchat_models().await;
    let recorded = support::fixture_json("copilot_decisions_predicate.json");
    fixture
        .mock
        .respond_json(
            "POST",
            "/v1/decisions",
            400,
            recorded["error_wrong_decision_model"]["response"].clone(),
        )
        .await;
    let (status, _, body) = support::send_json(
        &fixture.state,
        "/v1/chat/completions",
        sky_chat_request("gpt-6-luna-decisions", false),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert!(!body.contains("user constraints"), "{body}");
    let body: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(
        body["error"]["message"],
        "Copilot request failed with HTTP 400: no_available_model_endpoints"
    );
}

#[tokio::test]
async fn chat_decisions_requires_user_message() {
    let fixture = support::AppFixture::with_decision_models().await;
    let (status, _, body) = support::send_json(
        &fixture.state,
        "/v1/chat/completions",
        serde_json::json!({
            "model": "gpt-6-luna-decisions",
            "messages": [{"role": "system", "content": "You are helpful"}]
        }),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    let body: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(body["error"]["type"], "invalid_request_error");
    assert_eq!(
        body["error"]["message"],
        "decisions models require a user message"
    );
    support::assert_no_upstream_posts(&fixture.mock).await;
}

#[tokio::test]
async fn chat_image_model_returns_400_naming_images_endpoints() {
    let fixture = support::AppFixture::with_nonchat_models().await;
    for stream in [false, true] {
        let (status, _, body) = support::send_json(
            &fixture.state,
            "/v1/chat/completions",
            sky_chat_request("gpt-image-2.5-flare", stream),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
        let body: Value = serde_json::from_str(&body).unwrap();
        assert_eq!(body["error"]["type"], "invalid_request_error");
        assert_eq!(
            body["error"]["message"],
            support::image_model_message("/v1/chat/completions")
        );
    }
    support::assert_no_upstream_posts(&fixture.mock).await;
}

#[tokio::test]
async fn chat_existing_model_unchanged_with_nonchat_catalog() {
    let fixture = support::AppFixture::with_nonchat_models().await;
    fixture
        .mock
        .respond_json(
            "POST",
            "/chat/completions",
            200,
            serde_json::json!({
                "id": "chatcmpl-1",
                "object": "chat.completion",
                "model": "claude-sonnet-5.5",
                "choices": [{"index": 0, "message": {"role": "assistant", "content": "hi"}, "finish_reason": "stop"}]
            }),
        )
        .await;
    let (status, _, body) = support::send_json(
        &fixture.state,
        "/v1/chat/completions",
        sky_chat_request("claude-sonnet-5.5", false),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let body: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(body["choices"][0]["message"]["content"], "hi");
    assert_eq!(fixture.mock.hits("POST", "/chat/completions").await, 1);
    assert_eq!(fixture.mock.hits("POST", "/v1/decisions").await, 0);
    let upstream = fixture
        .mock
        .last_request_body_json("POST", "/chat/completions")
        .await
        .unwrap();
    assert_eq!(upstream["temperature"], 0.2);
    assert!(upstream.get("questions").is_none());
}

const RESPONSES_USAGE_JSON: &str = include_str!("fixtures/chat_usage_responses.json");
const RESPONSES_USAGE_SSE: &str = include_str!("fixtures/chat_usage_responses.sse");

fn fixture_json(text: &str) -> Value {
    serde_json::from_str(text).unwrap()
}

fn fixture_events(text: &'static str) -> Vec<&'static str> {
    text.split("\n\n")
        .filter(|event| !event.trim().is_empty())
        .collect()
}

async fn usage_fixture(model: &str, owned_by: &str, endpoint: &str) -> support::AppFixture {
    let fixture = support::AppFixture::with_mock_copilot().await;
    fixture
        .state
        .models
        .set_copilot_models(vec![serde_json::json!({
            "id": model,
            "owned_by": owned_by,
            "supported_endpoints": [endpoint]
        })])
        .await;
    fixture
}

async fn post_usage_chat(fixture: &support::AppFixture, model: &str, stream: bool) -> String {
    let mut body = serde_json::json!({
        "model": model,
        "stream": stream,
        "messages": [{"role": "user", "content": "Say hi"}]
    });
    if stream {
        body["stream_options"] = serde_json::json!({"include_usage": true});
    }
    let response = router(fixture.state.clone())
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/chat/completions")
                .header("content-type", "application/json")
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    String::from_utf8(bytes.to_vec()).unwrap()
}

fn final_usage_chunk(text: &str) -> Value {
    chat_sse_chunks(text)
        .into_iter()
        .rfind(|chunk| chunk.get("usage").is_some())
        .unwrap_or_else(|| panic!("no usage chunk: {text}"))
}

fn assert_translated_usage(body: &Value) {
    let upstream = fixture_json(RESPONSES_USAGE_JSON);
    assert_eq!(body["copilot_usage"], upstream["copilot_usage"]);
    assert_eq!(body["usage"]["prompt_tokens"], 3313);
    assert_eq!(body["usage"]["completion_tokens"], 5);
    assert_eq!(body["usage"]["total_tokens"], 3318);
    assert_eq!(
        body["usage"]["prompt_tokens_details"],
        serde_json::json!({"cached_tokens": 3310, "cache_write_tokens": 0})
    );
    assert_eq!(
        body["usage"]["completion_tokens_details"],
        serde_json::json!({"reasoning_tokens": 0})
    );
}

fn assert_no_usage_extras(body: &Value) {
    assert!(body.get("copilot_usage").is_none(), "{body}");
    assert!(
        body["usage"].get("prompt_tokens_details").is_none(),
        "{body}"
    );
    assert!(
        body["usage"].get("completion_tokens_details").is_none(),
        "{body}"
    );
}

#[tokio::test]
async fn chat_completions_translated_response_carries_copilot_usage_and_details() {
    let fixture = usage_fixture("gpt-6.1-sol", "openai", "/responses").await;
    fixture
        .mock
        .respond_json(
            "POST",
            "/responses",
            200,
            fixture_json(RESPONSES_USAGE_JSON),
        )
        .await;

    let body: Value =
        serde_json::from_str(&post_usage_chat(&fixture, "gpt-6.1-sol", false).await).unwrap();

    assert_eq!(fixture.mock.hits("POST", "/responses").await, 1);
    assert_eq!(body["choices"][0]["message"]["content"], "Hi");
    assert_translated_usage(&body);
}

#[tokio::test]
async fn chat_completions_translated_stream_final_chunk_carries_copilot_usage_and_details() {
    let fixture = usage_fixture("gpt-6.1-sol", "openai", "/responses").await;
    fixture
        .mock
        .respond_sse(
            "POST",
            "/responses",
            200,
            fixture_events(RESPONSES_USAGE_SSE),
        )
        .await;

    let text = post_usage_chat(&fixture, "gpt-6.1-sol", true).await;

    assert_eq!(fixture.mock.hits("POST", "/responses").await, 1);
    assert_translated_usage(&final_usage_chunk(&text));
}

#[tokio::test]
async fn chat_completions_translated_stream_reads_copilot_usage_inside_response() {
    let fixture = usage_fixture("gpt-6.1-sol", "openai", "/responses").await;
    fixture
        .mock
        .respond_sse(
            "POST",
            "/responses",
            200,
            vec![
                r#"event: response.completed
data: {"type":"response.completed","response":{"id":"resp_fixture","status":"completed","output":[],"usage":{"input_tokens":2,"output_tokens":1,"total_tokens":3},"copilot_usage":{"total_nano_aiu":42}}}"#,
            ],
        )
        .await;

    let chunk = final_usage_chunk(&post_usage_chat(&fixture, "gpt-6.1-sol", true).await);

    assert_eq!(
        chunk["copilot_usage"],
        serde_json::json!({"total_nano_aiu": 42})
    );
}

#[tokio::test]
async fn chat_completions_translated_response_omits_usage_fields_upstream_did_not_report() {
    let fixture = usage_fixture("gpt-6.1-sol", "openai", "/responses").await;
    fixture
        .mock
        .respond_json(
            "POST",
            "/responses",
            200,
            serde_json::json!({
                "id": "resp_fixture",
                "object": "response",
                "status": "completed",
                "output": [{"type": "message", "role": "assistant", "content": [{"type": "output_text", "text": "Hi"}]}],
                "usage": {"input_tokens": 9, "output_tokens": 1, "total_tokens": 10, "input_tokens_details": {"cached_tokens": 7}}
            }),
        )
        .await;

    let body: Value =
        serde_json::from_str(&post_usage_chat(&fixture, "gpt-6.1-sol", false).await).unwrap();

    assert_eq!(body["usage"]["prompt_tokens"], 9);
    assert_eq!(
        body["usage"]["prompt_tokens_details"],
        serde_json::json!({"cached_tokens": 7})
    );
    assert!(body["usage"].get("completion_tokens_details").is_none());
    assert!(body.get("copilot_usage").is_none());
}

#[tokio::test]
async fn chat_completions_translated_stream_omits_usage_fields_upstream_did_not_report() {
    let fixture = usage_fixture("gpt-6.1-sol", "openai", "/responses").await;
    fixture
        .mock
        .respond_sse(
            "POST",
            "/responses",
            200,
            vec![
                r#"event: response.completed
data: {"type":"response.completed","response":{"id":"resp_fixture","status":"completed","output":[],"usage":{"input_tokens":2,"output_tokens":1,"total_tokens":3}}}"#,
            ],
        )
        .await;

    let chunk = final_usage_chunk(&post_usage_chat(&fixture, "gpt-6.1-sol", true).await);

    assert_eq!(chunk["usage"]["total_tokens"], 3);
    assert_no_usage_extras(&chunk);
}

async fn assert_raw_usage_passthrough(
    model: &str,
    owned_by: &str,
    json_fixture: &str,
    sse_fixture: &'static str,
) {
    let fixture = usage_fixture(model, owned_by, "/chat/completions").await;
    let upstream = fixture_json(json_fixture);
    fixture
        .mock
        .respond_json("POST", "/chat/completions", 200, upstream.clone())
        .await;
    let body: Value = serde_json::from_str(&post_usage_chat(&fixture, model, false).await).unwrap();
    assert_eq!(body["usage"], upstream["usage"], "{model}");
    assert_eq!(body["copilot_usage"], upstream["copilot_usage"], "{model}");

    fixture
        .mock
        .respond_sse(
            "POST",
            "/chat/completions",
            200,
            fixture_events(sse_fixture),
        )
        .await;
    let text = post_usage_chat(&fixture, model, true).await;
    let upstream_final = final_usage_chunk(sse_fixture);
    let chunk = final_usage_chunk(&text);
    assert_eq!(chunk["usage"], upstream_final["usage"], "{model}");
    assert_eq!(
        chunk["copilot_usage"], upstream_final["copilot_usage"],
        "{model}"
    );
    assert_eq!(fixture.mock.hits("POST", "/responses").await, 0);
}

#[tokio::test]
async fn chat_completions_raw_gpt_passes_through_copilot_usage_and_details() {
    let upstream = fixture_json(include_str!("fixtures/chat_usage_gpt_derived.json"));
    assert!(upstream["usage"]["completion_tokens_details"].is_object());
    assert_raw_usage_passthrough(
        "gpt-6.1-sol",
        "openai",
        include_str!("fixtures/chat_usage_gpt_derived.json"),
        include_str!("fixtures/chat_usage_gpt_derived.sse"),
    )
    .await;
}

#[tokio::test]
async fn chat_completions_raw_claude_passes_through_copilot_usage_and_details() {
    assert_raw_usage_passthrough(
        "claude-haiku-5.5",
        "anthropic",
        include_str!("fixtures/chat_usage_claude.json"),
        include_str!("fixtures/chat_usage_claude.sse"),
    )
    .await;
}

#[tokio::test]
async fn chat_completions_raw_gemini_passes_through_copilot_usage_and_details() {
    assert_raw_usage_passthrough(
        "gemini-3.8-flash",
        "google",
        include_str!("fixtures/chat_usage_gemini.json"),
        include_str!("fixtures/chat_usage_gemini.sse"),
    )
    .await;
}

#[tokio::test]
async fn chat_completions_raw_omits_usage_fields_upstream_did_not_report() {
    let fixture = usage_fixture("claude-haiku-5.5", "anthropic", "/chat/completions").await;
    fixture
        .mock
        .respond_json(
            "POST",
            "/chat/completions",
            200,
            serde_json::json!({
                "id": "chatcmpl-fixture",
                "object": "chat.completion",
                "model": "claude-haiku-5.5",
                "choices": [{"index": 0, "finish_reason": "stop", "message": {"role": "assistant", "content": "Hi"}}],
                "usage": {"prompt_tokens": 2, "completion_tokens": 1, "total_tokens": 3}
            }),
        )
        .await;

    let body: Value =
        serde_json::from_str(&post_usage_chat(&fixture, "claude-haiku-5.5", false).await).unwrap();

    assert_no_usage_extras(&body);
}
