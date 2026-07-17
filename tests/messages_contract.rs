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
async fn messages_refreshes_models_before_capability_routing() {
    let fixture = support::AppFixture::with_mock_copilot().await;
    fixture
        .mock
        .respond_json(
            "GET",
            "/models",
            200,
            serde_json::json!({
                "data": [{
                    "id": "gpt-live-messages",
                    "owned_by": "openai",
                    "supported_endpoints": ["/v1/messages"]
                }]
            }),
        )
        .await;
    fixture
        .mock
        .respond_json(
            "POST",
            "/v1/messages",
            200,
            serde_json::json!({
                "id": "msg_live",
                "type": "message",
                "role": "assistant",
                "model": "gpt-live-messages",
                "content": [{"type":"text","text":"live model ok"}],
                "stop_reason": "end_turn",
                "usage": {"input_tokens": 3, "output_tokens": 1}
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
                    r#"{"model":"gpt-live-messages","max_tokens":64,"messages":[{"role":"user","content":"hi"}]}"#,
                ))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(fixture.mock.hits("GET", "/models").await, 1);
    let outbound = fixture
        .mock
        .last_request_body_json("POST", "/v1/messages")
        .await
        .unwrap();
    assert_eq!(outbound["model"], "gpt-live-messages");
    let body = response_json(response).await;
    assert_eq!(body["content"][0]["text"], "live model ok");
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
async fn messages_stream_translates_responses_function_calls_to_anthropic_tool_use() {
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
data: {"type":"response.created","response":{"id":"resp_tool","model":"gpt-5.5","usage":{"input_tokens":4,"output_tokens":0}}}"#,
                r#"event: response.output_item.added
data: {"type":"response.output_item.added","output_index":0,"item":{"type":"function_call","call_id":"toolu_1","name":"Bash","arguments":""}}"#,
                r#"event: response.function_call_arguments.delta
data: {"type":"response.function_call_arguments.delta","output_index":0,"call_id":"toolu_1","delta":"{\"command\":\"pwd\"}"}"#,
                r#"event: response.output_item.done
data: {"type":"response.output_item.done","output_index":0,"item":{"type":"function_call","call_id":"toolu_1","name":"Bash","arguments":"{\"command\":\"pwd\"}"}}"#,
                r#"event: response.completed
data: {"type":"response.completed","response":{"id":"resp_tool","status":"completed","output":[{"type":"function_call","call_id":"toolu_1","name":"Bash","arguments":"{\"command\":\"pwd\"}"}],"usage":{"input_tokens":4,"output_tokens":3}}}"#,
            ],
        )
        .await;

    let response = router(fixture.state)
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/messages")
                .header("content-type", "application/json")
                .body(Body::from(
                    r#"{
                        "model":"gpt-5.5",
                        "stream":true,
                        "max_tokens":64,
                        "tools":[{"name":"Bash","input_schema":{"type":"object"}}],
                        "messages":[{"role":"user","content":"inspect"}]
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
    assert!(text.contains(r#""type":"tool_use""#), "{text}");
    assert!(text.contains(r#""id":"toolu_1""#), "{text}");
    assert!(text.contains(r#""name":"Bash""#), "{text}");
    assert!(text.contains(r#""type":"input_json_delta""#), "{text}");
    assert!(
        text.contains(r#""partial_json":"{\"command\":\"pwd\"}""#),
        "{text}"
    );
    assert!(text.contains(r#""stop_reason":"tool_use""#), "{text}");
    assert!(text.contains("event: message_stop"), "{text}");
}

#[tokio::test]
async fn messages_bridge_translates_responses_function_calls_to_anthropic_tool_use() {
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
                    "call_id":"toolu_1",
                    "name":"Bash",
                    "arguments":"{\"command\":\"pwd\"}"
                }],
                "usage": {"input_tokens": 4, "output_tokens": 3, "total_tokens": 7}
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
                    r#"{
                        "model":"gpt-5.5",
                        "max_tokens":64,
                        "tools":[{"name":"Bash","input_schema":{"type":"object"}}],
                        "messages":[{"role":"user","content":"inspect"}]
                    }"#,
                ))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let body = response_json(response).await;
    assert_eq!(body["content"][0]["type"], "tool_use");
    assert_eq!(body["content"][0]["id"], "toolu_1");
    assert_eq!(body["content"][0]["name"], "Bash");
    assert_eq!(body["content"][0]["input"]["command"], "pwd");
    assert_eq!(body["stop_reason"], "tool_use");
}

#[tokio::test]
async fn messages_routes_gpt55_to_responses_and_returns_anthropic_response() {
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
async fn messages_refreshes_models_before_endpoint_selection() {
    let fixture = support::AppFixture::with_mock_copilot().await;
    fixture
        .mock
        .respond_json(
            "GET",
            "/models",
            200,
            serde_json::json!({
                "data": [{
                    "id": "gpt-dynamic-responses",
                    "owned_by": "openai",
                    "supported_endpoints": ["/responses"]
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
                "id": "resp_messages_refresh",
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
                .uri("/v1/messages")
                .header("content-type", "application/json")
                .body(Body::from(
                    r#"{"model":"gpt-dynamic-responses","max_tokens":64,"messages":[{"role":"user","content":"hello"}]}"#,
                ))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(fixture.mock.hits("GET", "/models").await, 1);
    assert_eq!(fixture.mock.hits("POST", "/v1/messages").await, 0);
    assert_eq!(fixture.mock.hits("POST", "/chat/completions").await, 0);
    assert_eq!(fixture.mock.hits("POST", "/responses").await, 1);
    let body = response_json(response).await;
    assert_eq!(body["content"][0]["text"], "refreshed bridge");
}

#[tokio::test]
async fn messages_bridge_converts_anthropic_content_blocks_for_responses() {
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
async fn messages_bridge_encodes_follow_up_assistant_text_as_output_text() {
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
                "id": "resp_messages_follow_up",
                "object": "response",
                "status": "completed",
                "output": [{"type":"message","role":"assistant","content":[{"type":"output_text","text":"done"}]}],
                "usage": {"input_tokens": 8, "output_tokens": 1, "total_tokens": 9}
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
                        "messages":[
                            {"role":"user","content":"inspect the project"},
                            {"role":"assistant","content":"I will inspect it."},
                            {"role":"user","content":"continue"}
                        ]
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
    assert_eq!(outbound["input"][1]["role"], "assistant");
    assert_eq!(outbound["input"][1]["content"][0]["type"], "output_text");
}

#[tokio::test]
async fn messages_bridge_converts_anthropic_tools_and_tool_history_for_responses() {
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
                "id": "resp_messages_tools",
                "object": "response",
                "status": "completed",
                "output": [{"type":"message","role":"assistant","content":[{"type":"output_text","text":"done"}]}],
                "usage": {"input_tokens": 8, "output_tokens": 1, "total_tokens": 9}
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
                        "tools":[{
                            "name":"Bash",
                            "description":"Run a shell command",
                            "input_schema":{
                                "type":"object",
                                "properties":{"command":{"type":"string"}},
                                "required":["command"]
                            }
                        }],
                        "messages":[
                            {"role":"user","content":"inspect the project"},
                            {"role":"assistant","content":[
                                {"type":"text","text":"I will inspect it."},
                                {"type":"tool_use","id":"toolu_1","name":"Bash","input":{"command":"pwd"}}
                            ]},
                            {"role":"user","content":[
                                {"type":"tool_result","tool_use_id":"toolu_1","content":"project path"}
                            ]}
                        ]
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
    assert_eq!(outbound["tools"][0]["type"], "function");
    assert_eq!(outbound["tools"][0]["name"], "Bash");
    assert_eq!(outbound["tools"][0]["parameters"]["required"][0], "command");
    assert_eq!(outbound["input"][1]["content"][0]["type"], "output_text");
    assert_eq!(outbound["input"][2]["type"], "function_call");
    assert_eq!(outbound["input"][2]["call_id"], "toolu_1");
    assert_eq!(outbound["input"][2]["name"], "Bash");
    assert_eq!(outbound["input"][2]["arguments"], r#"{"command":"pwd"}"#);
    assert_eq!(outbound["input"][3]["type"], "function_call_output");
    assert_eq!(outbound["input"][3]["call_id"], "toolu_1");
    assert_eq!(outbound["input"][3]["output"], "project path");
}

#[tokio::test]
async fn messages_bridge_preserves_prompt_cache_controls_for_responses() {
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
        .state
        .models
        .set_copilot_models(vec![serde_json::json!({
            "id": "claude-sonnet-4.6",
            "owned_by": "anthropic",
            "supported_endpoints": ["/v1/messages"],
            "capabilities": {
                "supports": {
                    "reasoning_effort": ["low", "medium", "high"]
                }
            }
        })])
        .await;
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
