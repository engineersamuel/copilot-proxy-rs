use axum::Json;
use axum::extract::rejection::BytesRejection;
use http::{HeaderMap, StatusCode};

use crate::errors::{anthropic_error, openai_error};
use crate::request_body::RequestBodyError;

pub(crate) fn request_body_rejection_details(
    rejection: BytesRejection,
    headers: &HeaderMap,
    limit: u64,
    api_family: &'static str,
) -> (StatusCode, String) {
    let status = rejection.status();
    let content_length = headers
        .get(http::header::CONTENT_LENGTH)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<u64>().ok());
    let message = if status == StatusCode::PAYLOAD_TOO_LARGE {
        body_limit_message(limit, content_length)
    } else {
        rejection.body_text()
    };
    tracing::warn!(
        api.family = api_family,
        http.status_code = status.as_u16() as u64,
        request.content_length = content_length.unwrap_or(0),
        request.content_length.present = content_length.is_some(),
        request.body.limit = limit,
        error = %rejection,
        "request body rejected"
    );
    (status, message)
}

pub(crate) fn request_body_error_details(error: &RequestBodyError) -> (StatusCode, String) {
    match error {
        RequestBodyError::DecodedBodyTooLarge { limit } => (
            StatusCode::PAYLOAD_TOO_LARGE,
            body_limit_message(u64::try_from(*limit).unwrap_or(u64::MAX), None),
        ),
        _ => (StatusCode::BAD_REQUEST, error.to_string()),
    }
}

pub(crate) fn anthropic_request_body_error_type(status: StatusCode) -> &'static str {
    if status == StatusCode::PAYLOAD_TOO_LARGE {
        "request_too_large"
    } else {
        "invalid_request_error"
    }
}

fn body_limit_message(limit: u64, content_length: Option<u64>) -> String {
    let size = content_length
        .map(|bytes| format!(" ({bytes} bytes)"))
        .unwrap_or_default();
    format!(
        "request body{size} exceeds the configured limit of {limit} bytes. \
         Increase COPILOT_PROXY_RS_MAX_DECODED_BODY_BYTES and restart the proxy before retrying \
         the same conversation."
    )
}

pub(crate) fn openai_copilot_error(
    error: crate::copilot::errors::CopilotError,
) -> (StatusCode, Json<crate::errors::OpenAiErrorResponse>) {
    match error {
        crate::copilot::errors::CopilotError::Auth(err) => openai_error(
            StatusCode::UNAUTHORIZED,
            "authentication_error",
            err.to_string(),
        ),
        crate::copilot::errors::CopilotError::Transient(err) => openai_error(
            StatusCode::from_u16(err.status_code).unwrap_or(StatusCode::BAD_GATEWAY),
            err.error_type,
            err.message,
        ),
        crate::copilot::errors::CopilotError::Http(err) => openai_error(
            StatusCode::from_u16(err.status_code).unwrap_or(StatusCode::BAD_GATEWAY),
            "server_error",
            err.detail,
        ),
        other => openai_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "server_error",
            other.to_string(),
        ),
    }
}

pub(crate) fn anthropic_copilot_error(
    error: crate::copilot::errors::CopilotError,
) -> (StatusCode, Json<crate::errors::AnthropicErrorResponse>) {
    match error {
        crate::copilot::errors::CopilotError::Auth(err) => anthropic_error(
            StatusCode::UNAUTHORIZED,
            "authentication_error",
            err.to_string(),
        ),
        crate::copilot::errors::CopilotError::Transient(err) => anthropic_error(
            StatusCode::from_u16(err.status_code).unwrap_or(StatusCode::BAD_GATEWAY),
            err.error_type,
            err.message,
        ),
        crate::copilot::errors::CopilotError::Http(err) => anthropic_error(
            StatusCode::from_u16(err.status_code).unwrap_or(StatusCode::BAD_GATEWAY),
            "server_error",
            err.detail,
        ),
        other => anthropic_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "server_error",
            other.to_string(),
        ),
    }
}
