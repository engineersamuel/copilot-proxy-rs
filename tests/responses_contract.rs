mod support;

use support::log_capture::{field, with_event_capture};

use axum::body::Body;
use http::{Request, StatusCode};
use http_body_util::BodyExt;
use serde_json::Value;
use tower::ServiceExt;

use copilot_proxy_rs::http::router;

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
