//! Contract tests for `POST /v1/systemone` (TypeSafe SystemOne API) over the
//! Copilot decision models.

mod support;

use std::io::Write as _;

use axum::body::Body;
use http::{Request, StatusCode};
use http_body_util::BodyExt;
use serde_json::{Map, Value, json};
use tower::ServiceExt;

use copilot_proxy_rs::http::router;
use support::log_capture::with_event_capture;

const LUNA: &str = "gpt-6-luna-decisions";
const MICROSOFT: &str = "microsoft-decision-1";
const ROUTE: &str = "/v1/systemone";

fn mixed() -> Value {
    support::fixture_json("systemone_mixed.json")
}

fn request_for(model: &str, questions: Value) -> Value {
    json!({"model": model, "state": "Is it prime?", "questions": questions})
}

fn mixed_request(model: &str) -> Value {
    let mut request = mixed()["client_request"].clone();
    request["model"] = json!(model);
    request
}

fn prime_name(i: usize) -> String {
    format!("p_{i:03}")
}

fn noul_questions(count: usize) -> Value {
    let questions: Map<String, Value> = (0..count)
        .map(|i| {
            (
                prime_name(i),
                json!({"type": "noul", "instructions": format!("Is {} prime?", i + 2)}),
            )
        })
        .collect();
    Value::Object(questions)
}

fn decisions_predicate_answers(count: usize) -> Value {
    let answers: Vec<Value> = (0..count)
        .map(|i| json!({"type": "predicate", "name": prime_name(i), "probability": 0.5}))
        .collect();
    json!({"model": "gpt-6-luna", "answers": answers, "usage": {"input_tokens": 9, "output_tokens": 0, "total_tokens": 9}})
}

fn systemone_noul_answers(count: usize) -> Value {
    let answers: Map<String, Value> = (0..count)
        .map(|i| (prime_name(i), json!({"type": "noul", "noul": 0.25})))
        .collect();
    json!({"model": MICROSOFT, "answers": answers, "usage": {"input_tokens": 9, "output_tokens": count}})
}

async fn post(fixture: &support::AppFixture, body: Value) -> (StatusCode, Value) {
    let (status, _, text) = support::send_json(&fixture.state, ROUTE, body).await;
    let value = serde_json::from_str(&text).unwrap_or_else(|_| panic!("not JSON: {text}"));
    (status, value)
}

async fn upstream_posts(fixture: &support::AppFixture) -> (usize, usize) {
    (
        fixture.mock.hits("POST", "/v1/decisions").await,
        fixture.mock.hits("POST", "/v1/systemone").await,
    )
}

// AC-1: microsoft-decision-1 forwards the TypeSafe body once and passes answers through.

#[tokio::test]
async fn microsoft_mixed_questions_pass_through_in_one_upstream_call() {
    let fixture = support::AppFixture::with_nonchat_models().await;
    let recorded = mixed()["systemone_response"].clone();
    fixture
        .mock
        .respond_json("POST", "/v1/systemone", 200, recorded.clone())
        .await;

    let request = mixed_request(MICROSOFT);
    let (status, body) = post(&fixture, request.clone()).await;

    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(upstream_posts(&fixture).await, (0, 1));
    assert_eq!(
        fixture
            .mock
            .last_request_body_json("POST", "/v1/systemone")
            .await
            .unwrap(),
        request
    );
    assert_eq!(body, recorded);
    fixture.mock.assert_no_generation_calls().await;
}

#[tokio::test]
async fn microsoft_forty_noul_questions_take_one_upstream_call() {
    let fixture = support::AppFixture::with_nonchat_models().await;
    fixture
        .mock
        .respond_json("POST", "/v1/systemone", 200, systemone_noul_answers(40))
        .await;

    let (status, body) = post(&fixture, request_for(MICROSOFT, noul_questions(40))).await;

    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(upstream_posts(&fixture).await, (0, 1));
    assert_eq!(body["model"], MICROSOFT);
    assert_eq!(body["answers"].as_object().unwrap().len(), 40);
    assert_eq!(
        body["answers"]["p_039"],
        json!({"type": "noul", "noul": 0.25})
    );
}

#[tokio::test]
async fn microsoft_accepts_255_questions() {
    let fixture = support::AppFixture::with_nonchat_models().await;
    fixture
        .mock
        .respond_json("POST", "/v1/systemone", 200, systemone_noul_answers(255))
        .await;

    let (status, body) = post(&fixture, request_for(MICROSOFT, noul_questions(255))).await;

    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(upstream_posts(&fixture).await, (0, 1));
}

#[tokio::test]
async fn microsoft_malformed_answer_is_502() {
    let fixture = support::AppFixture::with_nonchat_models().await;
    fixture
        .mock
        .respond_json("POST", "/v1/systemone", 200, json!({"answers": []}))
        .await;

    let (status, body) = post(&fixture, mixed_request(MICROSOFT)).await;

    assert_eq!(status, StatusCode::BAD_GATEWAY, "{body}");
    assert_eq!(
        body["error"]["message"],
        "Copilot systemone response was malformed"
    );
}

// AC-2: gpt-6-luna-decisions is translated to one /v1/decisions call and back.

#[tokio::test]
async fn luna_mixed_questions_translate_to_one_decisions_call() {
    let fixture = support::AppFixture::with_nonchat_models().await;
    let recorded = mixed();
    fixture
        .mock
        .respond_json(
            "POST",
            "/v1/decisions",
            200,
            recorded["decisions_response"].clone(),
        )
        .await;

    let (status, body) = post(&fixture, mixed_request(LUNA)).await;

    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(upstream_posts(&fixture).await, (1, 0));
    assert_eq!(
        fixture
            .mock
            .last_request_body_json("POST", "/v1/decisions")
            .await
            .unwrap(),
        recorded["decisions_request"]
    );
    assert_eq!(body, recorded["decisions_answer"]);
    fixture.mock.assert_no_generation_calls().await;
}

#[tokio::test]
async fn luna_forty_noul_questions_take_one_upstream_call() {
    let fixture = support::AppFixture::with_nonchat_models().await;
    fixture
        .mock
        .respond_json(
            "POST",
            "/v1/decisions",
            200,
            decisions_predicate_answers(40),
        )
        .await;

    let (status, body) = post(&fixture, request_for(LUNA, noul_questions(40))).await;

    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(upstream_posts(&fixture).await, (1, 0));
    let upstream = fixture
        .mock
        .last_request_body_json("POST", "/v1/decisions")
        .await
        .unwrap();
    assert_eq!(upstream["questions"].as_array().unwrap().len(), 40);
    assert_eq!(
        upstream["questions"][0],
        json!({"type": "predicate", "name": "p_000", "instructions": "Is 2 prime?"})
    );
    assert_eq!(body["model"], LUNA);
    assert_eq!(body["answers"].as_object().unwrap().len(), 40);
    assert_eq!(
        body["answers"]["p_017"],
        json!({"type": "noul", "noul": 0.5})
    );
    assert_eq!(
        body["usage"],
        json!({"input_tokens": 9, "output_tokens": 0})
    );
}

#[tokio::test]
async fn luna_accepts_400_questions() {
    let fixture = support::AppFixture::with_nonchat_models().await;
    fixture
        .mock
        .respond_json(
            "POST",
            "/v1/decisions",
            200,
            decisions_predicate_answers(400),
        )
        .await;

    let (status, body) = post(&fixture, request_for(LUNA, noul_questions(400))).await;

    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(upstream_posts(&fixture).await, (1, 0));
}

#[tokio::test]
async fn luna_state_becomes_decisions_input() {
    let messages = json!([{"role": "user", "content": "Is 7 prime?"}]);
    let cases = [
        (json!("plain text"), json!("plain text")),
        (messages.clone(), messages),
        (json!({"ticket": 7}), json!("{\"ticket\":7}")),
        (json!(["a", "b"]), json!("[\"a\",\"b\"]")),
    ];
    for (state, expected_input) in cases {
        let fixture = support::AppFixture::with_nonchat_models().await;
        fixture
            .mock
            .respond_json("POST", "/v1/decisions", 200, decisions_predicate_answers(1))
            .await;
        let mut request = request_for(LUNA, noul_questions(1));
        request["state"] = state.clone();

        let (status, body) = post(&fixture, request).await;

        assert_eq!(status, StatusCode::OK, "{state}: {body}");
        let upstream = fixture
            .mock
            .last_request_body_json("POST", "/v1/decisions")
            .await
            .unwrap();
        assert_eq!(upstream["input"], expected_input, "{state}");
    }
}

// Partial refusals: declined questions become refusal entries; 422 only when
// every question is declined.

fn refusal(text: &str) -> Value {
    json!({"type": "refusal", "refusal": text})
}

async fn answer_with(model: &str, path: &str, upstream: Value) -> (StatusCode, Value) {
    let fixture = support::AppFixture::with_nonchat_models().await;
    fixture.mock.respond_json("POST", path, 200, upstream).await;
    post(&fixture, mixed_request(model)).await
}

fn assert_all_declined(status: StatusCode, body: &Value) {
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert_eq!(body["error"]["type"], "invalid_request_error");
    assert_eq!(
        body["error"]["message"],
        "model declined to answer question(s): intent, is_angry, is_billing, only_criteria, severity"
    );
    assert!(body.get("answers").is_none(), "{body}");
}

#[tokio::test]
async fn luna_partial_refusal_keeps_answers_and_marks_declined_questions() {
    let recorded = mixed();
    let (status, body) = answer_with(
        LUNA,
        "/v1/decisions",
        recorded["decisions_refusal_response"].clone(),
    )
    .await;

    assert_eq!(status, StatusCode::OK, "{body}");
    let mut expected = recorded["decisions_answer"].clone();
    expected["answers"]["intent"] = refusal("declined");
    expected["answers"]["severity"] = refusal("declined");
    expected["usage"] = json!({"input_tokens": 330, "output_tokens": 0});
    assert_eq!(body, expected);
}

#[tokio::test]
async fn luna_all_refused_is_422_naming_every_question() {
    let (status, body) = answer_with(
        LUNA,
        "/v1/decisions",
        mixed()["decisions_all_refused_response"].clone(),
    )
    .await;

    assert_all_declined(status, &body);
}

#[tokio::test]
async fn microsoft_partial_refusal_keeps_answers_and_marks_declined_questions() {
    let recorded = mixed();
    let (status, body) = answer_with(
        MICROSOFT,
        "/v1/systemone",
        recorded["systemone_refusal_response"].clone(),
    )
    .await;

    assert_eq!(status, StatusCode::OK, "{body}");
    let mut expected = recorded["systemone_refusal_response"].clone();
    expected["answers"]["intent"] = refusal("Cannot judge intent here.");
    expected["answers"]["severity"] = refusal("declined");
    assert_eq!(body, expected);
}

#[tokio::test]
async fn microsoft_refusal_text_is_truncated_to_200_chars() {
    let long = "é".repeat(250);
    let upstream = json!({
        "answers": {
            "a": {"type": "refusal", "refusal": long},
            "b": {"type": "refusal", "refusal": "   "},
            "c": {"type": "noul", "noul": 0.25}
        },
        "usage": {"input_tokens": 9, "output_tokens": 1}
    });
    let fixture = support::AppFixture::with_nonchat_models().await;
    fixture
        .mock
        .respond_json("POST", "/v1/systemone", 200, upstream)
        .await;

    let (status, body) = post(
        &fixture,
        request_for(MICROSOFT, noul_questions_named(&["a", "b", "c"])),
    )
    .await;

    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["answers"]["a"], refusal(&"é".repeat(200)));
    assert_eq!(body["answers"]["b"], refusal("declined"));
    assert_eq!(body["answers"]["c"], json!({"type": "noul", "noul": 0.25}));
}

#[tokio::test]
async fn microsoft_unrequested_refusal_is_dropped() {
    let upstream = json!({
        "answers": {
            "a": {"type": "refusal", "refusal": "no"},
            "b": {"type": "noul", "noul": 0.5},
            "extra": {"type": "refusal", "refusal": "x".repeat(250), "detail": "secret"}
        },
        "usage": {"input_tokens": 9, "output_tokens": 1}
    });
    let fixture = support::AppFixture::with_nonchat_models().await;
    fixture
        .mock
        .respond_json("POST", "/v1/systemone", 200, upstream)
        .await;

    let (status, body) = post(
        &fixture,
        request_for(MICROSOFT, noul_questions_named(&["a", "b"])),
    )
    .await;

    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(body["answers"].get("extra").is_none(), "{body}");
    assert_eq!(body["answers"]["a"], refusal("no"));
    assert_eq!(body["answers"]["b"], json!({"type": "noul", "noul": 0.5}));
}

fn noul_questions_named(names: &[&str]) -> Value {
    let questions: Map<String, Value> = names
        .iter()
        .map(|name| (name.to_string(), json!({"type": "noul"})))
        .collect();
    Value::Object(questions)
}

#[tokio::test]
async fn microsoft_all_refused_is_422_naming_every_question() {
    let (status, body) = answer_with(
        MICROSOFT,
        "/v1/systemone",
        mixed()["systemone_all_refused_response"].clone(),
    )
    .await;

    assert_all_declined(status, &body);
}

#[tokio::test]
async fn luna_missing_answer_is_502() {
    let fixture = support::AppFixture::with_nonchat_models().await;
    fixture
        .mock
        .respond_json("POST", "/v1/decisions", 200, decisions_predicate_answers(1))
        .await;

    let (status, body) = post(&fixture, request_for(LUNA, noul_questions(2))).await;

    assert_eq!(status, StatusCode::BAD_GATEWAY, "{body}");
    assert_eq!(
        body["error"]["message"],
        "Copilot decisions response was malformed"
    );
}

// AC-3: local validation rejects with 400 and sends nothing upstream.

fn oversized_choice() -> Value {
    let criteria: Map<String, Value> = (0..256).map(|i| (format!("o{i}"), Value::Null)).collect();
    json!({"q": {"type": "choice", "instructions": "Pick", "criteria": criteria}})
}

fn rejection_cases() -> Vec<(&'static str, Value, &'static str)> {
    vec![
        (
            "unknown model",
            request_for("no-such-model", noul_questions(1)),
            "model 'no-such-model' is not a SystemOne decision model; use gpt-6-luna-decisions or microsoft-decision-1",
        ),
        (
            "chat model",
            request_for("claude-sonnet-5.5", noul_questions(1)),
            "model 'claude-sonnet-5.5' is not a SystemOne decision model",
        ),
        (
            "image model",
            request_for("gpt-image-2.5-flare", noul_questions(1)),
            "model 'gpt-image-2.5-flare' is not a SystemOne decision model",
        ),
        (
            "missing model",
            json!({"state": "s", "questions": noul_questions(1)}),
            "model is required",
        ),
        (
            "box",
            request_for(
                MICROSOFT,
                json!({"where": {"type": "box", "instructions": "Find it"}}),
            ),
            "question 'where': unsupported type 'box'; use noul, choice or score",
        ),
        (
            "unknown type",
            request_for(LUNA, json!({"q": {"type": "maybe", "instructions": "?"}})),
            "question 'q': unsupported type 'maybe'; use noul, choice or score",
        ),
        (
            "missing type",
            request_for(LUNA, json!({"q": {"instructions": "?"}})),
            "question 'q': unsupported type ''; use noul, choice or score",
        ),
        (
            "empty questions",
            request_for(MICROSOFT, json!({})),
            "questions must contain at least one question",
        ),
        (
            "questions not an object",
            request_for(LUNA, json!([{"type": "noul"}])),
            "questions must be an object keyed by question name",
        ),
        (
            "microsoft over limit",
            request_for(MICROSOFT, noul_questions(256)),
            "request has 256 questions; the limit for model microsoft-decision-1 is 255",
        ),
        (
            "luna over limit",
            request_for(LUNA, noul_questions(401)),
            "request has 401 questions; the limit for model gpt-6-luna-decisions is 400",
        ),
        (
            "empty name",
            request_for(
                MICROSOFT,
                json!({"": {"type": "noul", "instructions": "?"}}),
            ),
            "question names must not be empty",
        ),
        (
            "one option",
            request_for(
                MICROSOFT,
                json!({"q": {"type": "choice", "criteria": {"a": null}}}),
            ),
            "question 'q': choice criteria must have 2-255 options",
        ),
        (
            "256 options",
            request_for(LUNA, oversized_choice()),
            "question 'q': choice criteria must have 2-255 options",
        ),
        (
            "choice criteria list",
            request_for(
                MICROSOFT,
                json!({"q": {"type": "choice", "criteria": ["a", "b"]}}),
            ),
            "question 'q': choice criteria must have 2-255 options",
        ),
        (
            "one level",
            request_for(LUNA, json!({"q": {"type": "score", "criteria": ["low"]}})),
            "question 'q': score criteria must have 2-10 levels",
        ),
        (
            "eleven levels",
            request_for(
                MICROSOFT,
                json!({"q": {"type": "score", "criteria": vec!["x"; 11]}}),
            ),
            "question 'q': score criteria must have 2-10 levels",
        ),
        (
            "empty instructions",
            request_for(
                MICROSOFT,
                json!({"q": {"type": "noul", "instructions": ""}}),
            ),
            "question 'q': instructions must not be empty",
        ),
        (
            "luna noul without instructions or criteria",
            request_for(LUNA, json!({"q": {"type": "noul"}})),
            "question 'q': noul needs instructions or criteria for model gpt-6-luna-decisions",
        ),
    ]
}

#[tokio::test]
async fn invalid_requests_are_400_without_upstream_calls() {
    for (case, request, message) in rejection_cases() {
        let fixture = support::AppFixture::with_nonchat_models().await;

        let (status, body) = post(&fixture, request).await;

        assert_eq!(status, StatusCode::BAD_REQUEST, "{case}: {body}");
        assert_eq!(body["error"]["type"], "invalid_request_error", "{case}");
        let actual = body["error"]["message"].as_str().unwrap();
        assert!(actual.starts_with(message), "{case}: {actual}");
        support::assert_no_upstream_posts(&fixture.mock).await;
    }
}

#[tokio::test]
async fn microsoft_allows_noul_without_instructions_or_criteria() {
    let fixture = support::AppFixture::with_nonchat_models().await;
    fixture
        .mock
        .respond_json(
            "POST",
            "/v1/systemone",
            200,
            json!({"answers": {"q": {"type": "noul", "noul": 0.25}}, "usage": {"input_tokens": 9, "output_tokens": 1}}),
        )
        .await;

    let (status, body) = post(
        &fixture,
        request_for(MICROSOFT, json!({"q": {"type": "noul"}})),
    )
    .await;

    assert_eq!(status, StatusCode::OK, "{body}");
}

#[tokio::test]
async fn local_model_is_rejected_without_fallback() {
    let fixture = support::AppFixture::with_mock_local().await;

    let (status, body) = post(
        &fixture,
        request_for("qwen3-coder-30b-local", noul_questions(1)),
    )
    .await;

    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert_eq!(
        body["error"]["message"],
        "model 'qwen3-coder-30b-local' is not a SystemOne decision model; use gpt-6-luna-decisions or microsoft-decision-1"
    );
    support::assert_no_upstream_posts(&fixture.mock).await;
    assert_eq!(fixture.mock.hits("POST", "/v1/chat/completions").await, 0);
}

#[tokio::test]
async fn multipart_bodies_are_rejected() {
    let fixture = support::AppFixture::with_nonchat_models().await;

    let (status, _, text) = support::send(
        &fixture.state,
        ROUTE,
        "multipart/form-data; boundary=b1",
        "--b1--\r\n",
    )
    .await;

    assert_eq!(status, StatusCode::BAD_REQUEST, "{text}");
    let body: Value = serde_json::from_str(&text).unwrap();
    assert_eq!(
        body["error"]["message"],
        "multipart bodies (assets) are not supported"
    );
    support::assert_no_upstream_posts(&fixture.mock).await;
}

#[tokio::test]
async fn microsoft_body_over_one_mib_is_413() {
    let fixture = support::AppFixture::with_nonchat_models().await;
    let mut request = request_for(MICROSOFT, noul_questions(1));
    request["state"] = json!("x".repeat(1024 * 1024));

    let (status, body) = post(&fixture, request).await;

    assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE, "{body}");
    support::assert_no_upstream_posts(&fixture.mock).await;
}

// AC-3: upstream errors map through the existing Copilot error envelope.

#[tokio::test]
async fn upstream_error_status_is_mapped() {
    for (model, path, status) in [
        (MICROSOFT, "/v1/systemone", 422u16),
        (LUNA, "/v1/decisions", 400u16),
    ] {
        let fixture = support::AppFixture::with_nonchat_models().await;
        fixture
            .mock
            .respond_json(
                "POST",
                path,
                status,
                json!({"detail": "secret upstream detail"}),
            )
            .await;

        let (actual, body) = post(&fixture, request_for(model, noul_questions(1))).await;

        assert_eq!(actual.as_u16(), status, "{model}: {body}");
        assert_eq!(fixture.mock.hits("POST", path).await, 1, "{model}");
        assert_eq!(
            body["error"]["message"],
            format!("Copilot request failed with HTTP {status}")
        );
    }
}

// Request bodies compressed by the TypeSafe SDK are decoded.

fn gzip(bytes: &[u8]) -> Vec<u8> {
    let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    encoder.write_all(bytes).unwrap();
    encoder.finish().unwrap()
}

#[tokio::test]
async fn compressed_request_bodies_are_accepted() {
    let raw = request_for(MICROSOFT, noul_questions(3)).to_string();
    for (encoding, bytes) in [
        ("zstd", zstd::bulk::compress(raw.as_bytes(), 0).unwrap()),
        ("gzip", gzip(raw.as_bytes())),
    ] {
        let fixture = support::AppFixture::with_nonchat_models().await;
        fixture
            .mock
            .respond_json("POST", "/v1/systemone", 200, systemone_noul_answers(3))
            .await;

        let response = router(fixture.state.clone())
            .oneshot(
                Request::post(ROUTE)
                    .header("content-type", "application/json")
                    .header("content-encoding", encoding)
                    .body(Body::from(bytes))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK, "{encoding}");
        assert_eq!(
            fixture
                .mock
                .last_request_body_json("POST", "/v1/systemone")
                .await
                .unwrap(),
            serde_json::from_str::<Value>(&raw).unwrap()
        );
    }
}

// AC-4: the models list advertises the route for both decision models only.

#[tokio::test]
async fn models_list_advertises_systemone_for_decision_models() {
    let fixture = support::AppFixture::with_nonchat_models().await;

    let response = router(fixture.state.clone())
        .oneshot(Request::get("/v1/models").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    let body: Value = serde_json::from_slice(&bytes).unwrap();
    let endpoints = |id: &str| -> Vec<String> {
        let entry = body["data"]
            .as_array()
            .unwrap()
            .iter()
            .find(|m| m["id"] == id)
            .unwrap_or_else(|| panic!("{id} not listed"));
        serde_json::from_value(entry["supported_endpoints"].clone()).unwrap()
    };

    assert_eq!(endpoints(LUNA), ["/v1/decisions", "/v1/systemone"]);
    assert_eq!(endpoints(MICROSOFT), ["/v1/systemone"]);
    assert!(!endpoints("claude-sonnet-5.5").contains(&ROUTE.to_string()));
    assert!(!endpoints("gpt-image-2.5-flare").contains(&ROUTE.to_string()));
}

// Safety: state and instructions never reach the logs.

#[tokio::test]
async fn state_and_instructions_are_not_logged() {
    let fixture = support::AppFixture::with_nonchat_models().await;
    fixture
        .mock
        .respond_json("POST", "/v1/decisions", 200, decisions_predicate_answers(1))
        .await;
    let request = json!({
        "model": LUNA,
        "state": "SENTINEL-STATE-7f3a",
        "questions": {"p_000": {"type": "noul", "instructions": "SENTINEL-INSTRUCTIONS-91c2"}}
    });

    let events = with_event_capture(|| async {
        let (status, body) = post(&fixture, request).await;
        assert_eq!(status, StatusCode::OK, "{body}");
    })
    .await;

    assert!(!events.is_empty());
    let logged = format!("{events:?}");
    assert!(!logged.contains("SENTINEL"), "{logged}");
}

#[tokio::test]
async fn partial_refusal_logs_count_and_names_only() {
    let fixture = support::AppFixture::with_nonchat_models().await;
    let upstream = json!({
        "answers": {
            "q_kept": {"type": "noul", "noul": 0.25},
            "q_declined": {"type": "refusal", "refusal": "SENTINEL-REFUSAL-55aa"}
        },
        "usage": {"input_tokens": 9, "output_tokens": 1}
    });
    fixture
        .mock
        .respond_json("POST", "/v1/systemone", 200, upstream)
        .await;
    let request = json!({
        "model": MICROSOFT,
        "state": "SENTINEL-STATE-7f3a",
        "questions": {
            "q_kept": {"type": "noul", "instructions": "SENTINEL-INSTRUCTIONS-91c2"},
            "q_declined": {"type": "noul", "instructions": "SENTINEL-INSTRUCTIONS-0b7d"}
        }
    });

    let events = with_event_capture(|| async {
        let (status, body) = post(&fixture, request).await;
        assert_eq!(status, StatusCode::OK, "{body}");
    })
    .await;

    let field = |event: &support::log_capture::CapturedEvent, key: &str| {
        event
            .fields
            .iter()
            .find(|(name, _)| name == key)
            .map(|(_, value)| value.clone())
    };
    let refusal_lines: Vec<_> = events
        .iter()
        .filter(|event| field(event, "systemone.refused").is_some())
        .collect();
    assert_eq!(refusal_lines.len(), 1, "{events:?}");
    let line = refusal_lines[0];
    assert_eq!(line.level, Some(tracing::Level::WARN));
    assert_eq!(field(line, "systemone.refused").as_deref(), Some("1"));
    let names = field(line, "systemone.refused_questions").unwrap_or_default();
    assert!(names.contains("q_declined"), "{line:?}");
    assert!(!names.contains("q_kept"), "{line:?}");
    let logged = format!("{events:?}");
    assert!(!logged.contains("SENTINEL"), "{logged}");
}

#[tokio::test]
async fn microsoft_incomplete_or_mismatched_results_are_502() {
    let good = json!({"type": "noul", "noul": 0.25});
    let usage = json!({"input_tokens": 9, "output_tokens": 1});
    let cases = [
        json!({"answers": {}, "usage": usage}),
        json!({"answers": {"other": good}, "usage": usage}),
        json!({"answers": {"q": {"type": "choice", "choice": "a"}}, "usage": usage}),
        json!({"answers": {"q": {"type": "noul"}}, "usage": usage}),
        json!({"answers": {"q": good}}),
        json!({"answers": {"q": good}, "usage": {"input_tokens": 9}}),
    ];
    for upstream in cases {
        let fixture = support::AppFixture::with_nonchat_models().await;
        fixture
            .mock
            .respond_json("POST", "/v1/systemone", 200, upstream.clone())
            .await;

        let (status, body) = post(
            &fixture,
            request_for(MICROSOFT, json!({"q": {"type": "noul"}})),
        )
        .await;

        assert_eq!(status, StatusCode::BAD_GATEWAY, "{upstream}: {body}");
        assert_eq!(
            body["error"]["message"],
            "Copilot systemone response was malformed"
        );
    }
}

#[tokio::test]
async fn invalid_questions_are_rejected_before_any_catalog_fetch() {
    let too_many = noul_questions(401);
    let cases = [
        json!({"q": {"type": "bogus"}}),
        json!({}),
        too_many,
        json!({"q": {"type": "choice", "criteria": {"only": "one"}}}),
    ];
    for model in [LUNA, MICROSOFT] {
        for questions in &cases {
            let fixture = support::AppFixture::with_nonchat_models().await;

            let (status, body) = post(&fixture, request_for(model, questions.clone())).await;

            assert_eq!(status, StatusCode::BAD_REQUEST, "{model}: {body}");
            assert_eq!(fixture.mock.hits("GET", "/models").await, 0, "{model}");
            assert_eq!(upstream_posts(&fixture).await, (0, 0));
        }
    }
}

#[tokio::test]
async fn cold_catalog_rejects_non_decision_models_without_upstream_calls() {
    for model in [
        "no-such-model",
        "gpt-4o",
        "claude-sonnet-5.5",
        "gpt-image-2.5-flare",
    ] {
        let fixture = support::AppFixture::with_nonchat_models().await;

        let (status, body) = post(&fixture, request_for(model, noul_questions(1))).await;

        assert_eq!(status, StatusCode::BAD_REQUEST, "{model}: {body}");
        assert_eq!(
            body["error"]["message"],
            format!(
                "model '{model}' is not a SystemOne decision model; use gpt-6-luna-decisions or microsoft-decision-1"
            )
        );
        assert_eq!(fixture.mock.hits("GET", "/models").await, 0, "{model}");
        assert_eq!(
            fixture.mock.hits("GET", "/copilot/token").await,
            0,
            "{model}"
        );
        assert_eq!(upstream_posts(&fixture).await, (0, 0), "{model}");
        support::assert_no_upstream_posts(&fixture.mock).await;
    }
}

#[tokio::test]
async fn cold_catalog_accepts_prefixed_decision_model_ids() {
    let fixture = support::AppFixture::with_nonchat_models().await;
    fixture
        .mock
        .respond_json("POST", "/v1/decisions", 200, decisions_predicate_answers(1))
        .await;
    let prefixed = format!("github-copilot/{LUNA}");

    let (status, body) = post(&fixture, request_for(&prefixed, noul_questions(1))).await;

    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(fixture.mock.hits("GET", "/models").await, 1);
}

#[tokio::test]
async fn cached_catalog_rejects_wrong_models_and_questions_without_upstream_calls() {
    let fixture = support::AppFixture::with_nonchat_models().await;
    let catalog = support::fixture_json("copilot_models_nonchat.json");
    let models = catalog["data"].as_array().unwrap().clone();
    fixture.state.models.set_copilot_models(models).await;

    for model in ["claude-sonnet-5.5", "gpt-image-2.5-flare"] {
        let (status, body) = post(&fixture, request_for(model, noul_questions(1))).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{model}: {body}");
    }
    let (status, body) = post(&fixture, request_for(LUNA, json!({}))).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");

    assert_eq!(fixture.mock.hits("GET", "/models").await, 0);
    assert_eq!(upstream_posts(&fixture).await, (0, 0));
}
