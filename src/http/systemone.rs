//! `POST /v1/systemone`: TypeSafe SystemOne-compatible multi-question
//! decisions answered by one upstream call on a Copilot decision model.

use axum::Json;
use axum::body::Bytes;
use axum::extract::State;
use axum::extract::rejection::BytesRejection;
use axum::response::{IntoResponse, Response};
use http::{HeaderMap, StatusCode, header};
use serde_json::{Map, Value};

use crate::errors::{OpenAiErrorResponse, openai_error};
use crate::http::errors::{
    openai_copilot_error, request_body_error_details, request_body_rejection_details,
};
use crate::models::CopilotModelKind;
use crate::request_body::parse_json_request_body_with_limit;
use crate::state::AppState;
use crate::translate::decisions::DecisionsEndpoint;
use crate::translate::systemone::{
    Questions, SystemOneError, from_decisions_response, from_systemone_response, parse_questions,
    requested_model, to_decisions_request, to_systemone_request,
};

const API_FAMILY: &str = "/v1/systemone";

type Failure = Box<(StatusCode, Json<OpenAiErrorResponse>)>;

fn bad_request(message: impl Into<String>) -> Failure {
    Box::new(openai_error(
        StatusCode::BAD_REQUEST,
        "invalid_request_error",
        message,
    ))
}

pub(crate) async fn systemone(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Result<Bytes, BytesRejection>,
) -> Response {
    match answer(&state, &headers, body).await {
        Ok(value) => Json(value).into_response(),
        Err(failure) => failure.into_response(),
    }
}

async fn answer(
    state: &AppState,
    headers: &HeaderMap,
    body: Result<Bytes, BytesRejection>,
) -> Result<Value, Failure> {
    reject_multipart(headers)?;
    let body = read_body(state, headers, body)?;
    let public_model = requested_model(&body).map_err(rejection)?;
    let (endpoint, copilot_model) = resolve(state, &public_model).await?;
    let questions = parse_questions(endpoint, &public_model, &body).map_err(rejection)?;
    tracing::info!(
        api.family = API_FAMILY,
        model.requested = public_model.as_str(),
        model.effective = copilot_model.as_str(),
        systemone.questions = questions.len() as u64,
        systemone.endpoint = ?endpoint,
        "systemone request prepared"
    );
    match endpoint {
        DecisionsEndpoint::SystemOne => {
            ask_systemone(state, body, &copilot_model, &public_model).await
        }
        DecisionsEndpoint::Decisions => {
            let input = body.get("state").unwrap_or(&Value::Null);
            ask_decisions(state, input, &questions, &copilot_model, &public_model).await
        }
    }
}

async fn ask_systemone(
    state: &AppState,
    body: Map<String, Value>,
    copilot_model: &str,
    public_model: &str,
) -> Result<Value, Failure> {
    let request = to_systemone_request(body, copilot_model).map_err(rejection)?;
    let upstream = state
        .copilot
        .post_systemone(request)
        .await
        .map_err(copilot)?;
    from_systemone_response(public_model, upstream).map_err(rejection)
}

async fn ask_decisions(
    state: &AppState,
    input: &Value,
    questions: &Questions,
    copilot_model: &str,
    public_model: &str,
) -> Result<Value, Failure> {
    let request = to_decisions_request(copilot_model, input, questions).map_err(rejection)?;
    let upstream = state
        .copilot
        .post_decisions(request)
        .await
        .map_err(copilot)?;
    from_decisions_response(public_model, questions, &upstream).map_err(rejection)
}

fn reject_multipart(headers: &HeaderMap) -> Result<(), Failure> {
    let content_type = headers
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default();
    let media_type = content_type.split(';').next().unwrap_or_default().trim();
    if media_type.eq_ignore_ascii_case("multipart/form-data") {
        return Err(bad_request("multipart bodies (assets) are not supported"));
    }
    Ok(())
}

fn read_body(
    state: &AppState,
    headers: &HeaderMap,
    body: Result<Bytes, BytesRejection>,
) -> Result<Map<String, Value>, Failure> {
    let limit = state.config.max_decoded_body_bytes;
    let body = body.map_err(|rejection| {
        let (status, message) =
            request_body_rejection_details(rejection, headers, limit, "systemone");
        Box::new(openai_error(status, "invalid_request_error", message))
    })?;
    let encoding = headers
        .get(header::CONTENT_ENCODING)
        .and_then(|value| value.to_str().ok())
        .unwrap_or("identity");
    parse_json_request_body_with_limit(&body, encoding, limit as usize).map_err(|err| {
        let (status, message) = request_body_error_details(&err);
        Box::new(openai_error(status, "invalid_request_error", message))
    })
}

/// Maps a public model to its decision endpoint; local, chat, image and
/// unknown models are rejected before anything is sent upstream.
async fn resolve(
    state: &AppState,
    public_model: &str,
) -> Result<(DecisionsEndpoint, String), Failure> {
    let not_decision = || {
        tracing::warn!(systemone.reject = "model", "systemone request rejected");
        bad_request(format!(
            "model '{public_model}' is not a SystemOne decision model; use gpt-6-luna-decisions or microsoft-decision-1"
        ))
    };
    if state.models.configured_local_target(public_model).is_some() {
        return Err(not_decision());
    }
    state.copilot.refresh_models_if_stale().await;
    let copilot_model = state.models.get_copilot_openai_model(public_model).await;
    match state.models.copilot_model_kind(&copilot_model).await {
        Some(CopilotModelKind::Decisions) => Ok((DecisionsEndpoint::Decisions, copilot_model)),
        Some(CopilotModelKind::SystemOne) => Ok((DecisionsEndpoint::SystemOne, copilot_model)),
        _ => Err(not_decision()),
    }
}

fn copilot(error: crate::copilot::errors::CopilotError) -> Failure {
    Box::new(openai_copilot_error(error))
}

fn rejection(error: SystemOneError) -> Failure {
    let (reason, status, error_type, message) = match error {
        SystemOneError::Invalid(message) => (
            "invalid",
            StatusCode::BAD_REQUEST,
            "invalid_request_error",
            message,
        ),
        SystemOneError::TooLarge(message) => (
            "too_large",
            StatusCode::PAYLOAD_TOO_LARGE,
            "invalid_request_error",
            message,
        ),
        SystemOneError::Refused(names) => (
            "refusal",
            StatusCode::UNPROCESSABLE_ENTITY,
            "invalid_request_error",
            format!("model declined to answer question(s): {}", names.join(", ")),
        ),
        SystemOneError::Malformed(endpoint) => (
            "malformed",
            StatusCode::BAD_GATEWAY,
            "server_error",
            format!(
                "Copilot {} response was malformed",
                endpoint_label(endpoint)
            ),
        ),
    };
    tracing::warn!(systemone.reject = reason, "systemone request rejected");
    Box::new(openai_error(status, error_type, message))
}

fn endpoint_label(endpoint: DecisionsEndpoint) -> &'static str {
    match endpoint {
        DecisionsEndpoint::Decisions => "decisions",
        DecisionsEndpoint::SystemOne => "systemone",
    }
}
