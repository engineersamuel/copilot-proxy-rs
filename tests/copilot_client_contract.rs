mod support;

use copilot_proxy_rs::copilot::errors::CopilotError;
use support::log_capture::{field, with_event_capture};

#[tokio::test]
async fn post_chat_retries_429_then_returns_json() {
    let mock = support::MockServer::start().await;
    mock.respond_sequence_json(
        "POST",
        "/chat/completions",
        vec![
            (
                429,
                serde_json::json!({"error": "rate"}),
                vec![("retry-after", "0")],
            ),
            (
                200,
                serde_json::json!({"choices": [{"message": {"role": "assistant", "content": "hello"}}]}),
                vec![],
            ),
        ],
    )
    .await;
    mock.respond_json(
        "GET",
        "/copilot/token",
        200,
        serde_json::json!({"token": "copilot-token", "expires_at": 4_102_444_800u64}),
    )
    .await;

    let fixture = support::backend_fixture(mock).await;
    let response = fixture
        .backend
        .post_chat(
            serde_json::json!({"model": "gpt-5.5", "messages": []})
                .as_object()
                .unwrap()
                .clone(),
            None,
        )
        .await
        .unwrap();

    assert_eq!(response["choices"][0]["message"]["content"], "hello");
    assert_eq!(fixture.mock.hits("POST", "/chat/completions").await, 2);
}

#[tokio::test]
async fn post_chat_does_not_retry_with_different_model_when_effort_cannot_be_downgraded() {
    let mock = support::MockServer::start().await;
    mock.respond_sequence_json(
        "POST",
        "/chat/completions",
        vec![(
            400,
            serde_json::json!({"error": {"message": "model_not_supported"}}),
            vec![],
        )],
    )
    .await;
    mock.respond_json(
        "GET",
        "/copilot/token",
        200,
        serde_json::json!({"token": "copilot-token", "expires_at": 4_102_444_800u64}),
    )
    .await;

    let fixture = support::backend_fixture(mock).await;
    let events = with_event_capture(|| async {
        let response = fixture
            .backend
            .post_chat(
                serde_json::json!({"model": "gpt-5.4-mini", "messages": []})
                    .as_object()
                    .unwrap()
                    .clone(),
                None,
            )
            .await;

        assert!(
            response.is_err(),
            "unsupported model should return the original error instead of retrying with a different model"
        );
    })
    .await;
    assert_eq!(fixture.mock.hits("POST", "/chat/completions").await, 1);
    assert_eq!(
        field(&events, "chat model fallback suppressed", "model.requested").as_deref(),
        Some("gpt-5.4-mini")
    );
}

#[tokio::test]
async fn post_chat_sanitizes_raw_upstream_error_body() {
    let mock = support::MockServer::start().await;
    mock.respond_json(
        "POST",
        "/chat/completions",
        403,
        serde_json::json!({
            "error": {
                "message": "secret upstream diagnostic with prompt fragment"
            }
        }),
    )
    .await;
    mock.respond_json(
        "GET",
        "/copilot/token",
        200,
        serde_json::json!({"token": "copilot-token", "expires_at": 4_102_444_800u64}),
    )
    .await;

    let fixture = support::backend_fixture(mock).await;
    let err = fixture
        .backend
        .post_chat(
            serde_json::json!({"model": "gpt-5.5", "messages": []})
                .as_object()
                .unwrap()
                .clone(),
            None,
        )
        .await
        .unwrap_err();

    match err {
        CopilotError::Http(http) => {
            assert_eq!(http.status_code, 403);
            assert!(http.detail.contains("HTTP 403"));
            assert!(!http.detail.contains("secret upstream diagnostic"));
            assert!(!http.detail.contains("prompt fragment"));
        }
        other => panic!("expected HTTP error, got {other:?}"),
    }
}

#[tokio::test]
async fn post_chat_keeps_gpt55_and_downgrades_effort_on_unsupported_api() {
    let mock = support::MockServer::start().await;
    mock.respond_sequence_json(
        "POST",
        "/chat/completions",
        vec![
            (
                400,
                serde_json::json!({
                    "error": {
                        "code": "unsupported_api_for_model",
                        "message": "model \"gpt-5.5\" is not accessible via the /chat/completions endpoint"
                    }
                }),
                vec![],
            ),
            (
                200,
                serde_json::json!({"choices": [{"message": {"content": "gpt-5.5 downgraded ok"}}]}),
                vec![],
            ),
        ],
    )
    .await;
    mock.respond_json(
        "GET",
        "/copilot/token",
        200,
        serde_json::json!({"token": "copilot-token", "expires_at": 4_102_444_800u64}),
    )
    .await;

    let fixture = support::backend_fixture(mock).await;
    let events = with_event_capture(|| async {
        let response = fixture
            .backend
            .post_chat(
                serde_json::json!({
                    "model": "gpt-5.5",
                    "reasoning_effort": "xhigh",
                    "messages": []
                })
                .as_object()
                .unwrap()
                .clone(),
                None,
            )
            .await
            .unwrap();

        assert_eq!(
            response["choices"][0]["message"]["content"],
            "gpt-5.5 downgraded ok"
        );
    })
    .await;
    assert_eq!(fixture.mock.hits("POST", "/chat/completions").await, 2);

    let retry_body = fixture
        .mock
        .last_request_body_json("POST", "/chat/completions")
        .await
        .expect("retry body should have been captured");
    assert_eq!(retry_body["model"], "gpt-5.5");
    assert_eq!(retry_body["reasoning_effort"], "high");
    assert_eq!(
        field(
            &events,
            "chat effort downgraded for retry",
            "model.requested"
        )
        .as_deref(),
        Some("gpt-5.5")
    );
    assert_eq!(
        field(
            &events,
            "chat effort downgraded for retry",
            "effort.original"
        )
        .as_deref(),
        Some("xhigh")
    );
    assert_eq!(
        field(&events, "chat effort downgraded for retry", "effort.retry").as_deref(),
        Some("high")
    );
}

#[tokio::test]
async fn post_chat_keeps_gpt55_and_downgrades_max_effort_on_unsupported_api() {
    let fixture = support::AppFixture::with_mock_copilot().await;
    fixture
        .state
        .models
        .set_copilot_models(vec![serde_json::json!({
            "id": "gpt-5.5",
            "owned_by": "openai",
            "supported_endpoints": ["/responses"],
            "capabilities": {"supports": {"reasoning_effort": ["low", "medium", "high", "xhigh"]}}
        })])
        .await;
    fixture
        .mock
        .respond_sequence_json(
            "POST",
            "/chat/completions",
            vec![
                (
                    400,
                    serde_json::json!({
                        "error": {
                            "code": "unsupported_api_for_model",
                            "message": "model \"gpt-5.5\" is not accessible via the /chat/completions endpoint"
                        }
                    }),
                    vec![],
                ),
                (
                    200,
                    serde_json::json!({"choices": [{"message": {"content": "downgraded ok"}}]}),
                    vec![],
                ),
            ],
        )
        .await;

    let response = fixture
        .state
        .copilot
        .post_chat(
            serde_json::json!({
                "model": "gpt-5.5",
                "reasoning_effort": "max",
                "messages": [{"role": "user", "content": "Hello"}]
            })
            .as_object()
            .unwrap()
            .clone(),
            None,
        )
        .await
        .unwrap();

    assert_eq!(
        response["choices"][0]["message"]["content"],
        "downgraded ok"
    );
    let retry_body = fixture
        .mock
        .last_request_body_json("POST", "/chat/completions")
        .await
        .expect("retry body should have been captured");
    assert_eq!(retry_body["model"], "gpt-5.5");
    assert_eq!(retry_body["reasoning_effort"], "high");
}

#[tokio::test]
async fn stream_chat_retries_429_before_streaming() {
    let mock = support::MockServer::start().await;
    mock.respond_sequence_json(
        "POST",
        "/chat/completions",
        vec![(
            429,
            serde_json::json!({"error": "rate_limited"}),
            vec![("retry-after", "0")],
        )],
    )
    .await;
    mock.respond_sse("POST", "/chat/completions", 200, vec!["data: [DONE]"])
        .await;
    mock.respond_json(
        "GET",
        "/copilot/token",
        200,
        serde_json::json!({"token": "copilot-token", "expires_at": 4_102_444_800u64}),
    )
    .await;

    let fixture = support::backend_fixture(mock).await;
    let response = fixture
        .backend
        .stream_chat(
            serde_json::json!({"model": "gpt-5.5", "stream": true, "messages": []})
                .as_object()
                .unwrap()
                .clone(),
            None,
        )
        .await;

    assert!(
        response.is_ok(),
        "expected stream_chat to succeed after retry; got: {:?}",
        response.err()
    );
    assert_eq!(fixture.mock.hits("POST", "/chat/completions").await, 2);
}

#[tokio::test]
async fn stream_chat_keeps_gpt55_and_downgrades_effort_on_unsupported_api() {
    let fixture = support::AppFixture::with_mock_copilot().await;
    fixture
        .state
        .models
        .set_copilot_models(vec![serde_json::json!({
            "id": "gpt-5.5",
            "owned_by": "openai",
            "supported_endpoints": ["/responses"],
            "capabilities": {"supports": {"reasoning_effort": ["low", "medium", "high", "xhigh"]}}
        })])
        .await;
    fixture
        .mock
        .respond_sequence_json(
            "POST",
            "/chat/completions",
            vec![(
                400,
                serde_json::json!({
                    "error": {
                        "code": "unsupported_api_for_model",
                        "message": "model \"gpt-5.5\" is not accessible via the /chat/completions endpoint"
                    }
                }),
                vec![],
            )],
        )
        .await;
    fixture
        .mock
        .respond_sse("POST", "/chat/completions", 200, vec!["data: [DONE]"])
        .await;

    let response = fixture
        .state
        .copilot
        .stream_chat(
            serde_json::json!({
                "model": "gpt-5.5",
                "stream": true,
                "reasoning_effort": "xhigh",
                "messages": [{"role": "user", "content": "Hello"}]
            })
            .as_object()
            .unwrap()
            .clone(),
            None,
        )
        .await;

    assert!(
        response.is_ok(),
        "expected stream_chat to retry gpt-5.5 with downgraded effort; got: {:?}",
        response.err()
    );
    assert_eq!(fixture.mock.hits("POST", "/chat/completions").await, 2);
    let retry_body = fixture
        .mock
        .last_request_body_json("POST", "/chat/completions")
        .await
        .expect("retry body should have been captured");
    assert_eq!(retry_body["model"], "gpt-5.5");
    assert_eq!(retry_body["reasoning_effort"], "high");
}

#[tokio::test]
async fn copilot_backend_logs_retry_status_and_usage_metadata() {
    let fixture = support::AppFixture::with_mock_copilot().await;
    fixture.mock.respond_sequence_json(
        "POST",
        "/chat/completions",
        vec![
            (429, serde_json::json!({"error": "slow down"}), vec![("retry-after", "0")]),
            (200, serde_json::json!({
                "choices": [{"message": {"role": "assistant", "content": "private answer"}}],
                "usage": {"prompt_tokens": 8, "completion_tokens": 3, "total_tokens": 11}
            }), vec![]),
        ],
    ).await;

    let mut body = serde_json::Map::new();
    body.insert(
        "model".to_string(),
        serde_json::Value::String("gpt-5.5".to_string()),
    );
    body.insert(
        "messages".to_string(),
        serde_json::json!([{"role":"user","content":"private prompt"}]),
    );

    let events = with_event_capture(|| async {
        let response = fixture.state.copilot.post_chat(body, None).await.unwrap();
        assert!(response.get("choices").is_some());
    })
    .await;

    assert_eq!(
        field(&events, "copilot request retrying", "http.status_code").as_deref(),
        Some("429")
    );
    assert_eq!(
        field(&events, "copilot request completed", "http.status_code").as_deref(),
        Some("200")
    );
    assert_eq!(
        field(&events, "copilot usage", "tokens.input").as_deref(),
        Some("8")
    );
    assert_eq!(
        field(&events, "copilot usage", "tokens.output").as_deref(),
        Some("3")
    );
    let rendered = format!("{events:?}");
    assert!(!rendered.contains("private prompt"));
    assert!(!rendered.contains("private answer"));
}

#[tokio::test]
async fn stream_request_non_2xx_emits_completed_warn() {
    let mock = support::MockServer::start().await;
    mock.respond_json(
        "POST",
        "/chat/completions",
        403,
        serde_json::json!({"error": "forbidden"}),
    )
    .await;
    mock.respond_json(
        "GET",
        "/copilot/token",
        200,
        serde_json::json!({"token": "copilot-token", "expires_at": 4_102_444_800u64}),
    )
    .await;

    let fixture = support::backend_fixture(mock).await;
    let events = with_event_capture(|| async {
        let result = fixture
            .backend
            .stream_chat(
                serde_json::json!({"model": "gpt-5.5", "stream": true, "messages": []})
                    .as_object()
                    .unwrap()
                    .clone(),
                None,
            )
            .await;
        assert!(result.is_err(), "expected error on 403");
    })
    .await;

    assert_eq!(
        field(&events, "copilot request completed", "http.status_code").as_deref(),
        Some("403"),
        "completed warn must carry the 403 status"
    );
    assert_eq!(
        field(&events, "copilot request completed", "stream").as_deref(),
        Some("true"),
        "completed warn must carry stream=true"
    );
}

#[tokio::test]
async fn stream_request_retry_exhausted_emits_completed_warn() {
    // Default copilot_retry_max is 3, so 4 consecutive 429s exhaust retries.
    // The final attempt falls through to the non-retry return path.
    let mock = support::MockServer::start().await;
    mock.respond_sequence_json(
        "POST",
        "/chat/completions",
        vec![
            (
                429,
                serde_json::json!({"error": "slow"}),
                vec![("retry-after", "0")],
            ),
            (
                429,
                serde_json::json!({"error": "slow"}),
                vec![("retry-after", "0")],
            ),
            (
                429,
                serde_json::json!({"error": "slow"}),
                vec![("retry-after", "0")],
            ),
            (
                429,
                serde_json::json!({"error": "slow"}),
                vec![("retry-after", "0")],
            ),
        ],
    )
    .await;
    mock.respond_json(
        "GET",
        "/copilot/token",
        200,
        serde_json::json!({"token": "copilot-token", "expires_at": 4_102_444_800u64}),
    )
    .await;

    let fixture = support::backend_fixture(mock).await;
    let events = with_event_capture(|| async {
        let result = fixture
            .backend
            .stream_chat(
                serde_json::json!({"model": "gpt-5.5", "stream": true, "messages": []})
                    .as_object()
                    .unwrap()
                    .clone(),
                None,
            )
            .await;
        assert!(result.is_err(), "expected error when retries exhausted");
    })
    .await;

    assert_eq!(
        field(&events, "copilot request completed", "http.status_code").as_deref(),
        Some("429"),
        "completed warn must carry 429 when retries are exhausted"
    );
    assert_eq!(
        field(&events, "copilot request completed", "stream").as_deref(),
        Some("true"),
        "completed warn must carry stream=true"
    );
    assert_eq!(fixture.mock.hits("POST", "/chat/completions").await, 4);
}
