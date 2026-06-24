use axum::Json;
use http::StatusCode;

use crate::errors::{anthropic_error, openai_error};

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
