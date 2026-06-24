use axum::Json;
use http::StatusCode;
use serde_json::{Map, Value};

use crate::errors::{anthropic_error, openai_error};

pub(crate) fn validate_openai_chat_request(
    body: &Map<String, Value>,
) -> Result<(), (StatusCode, Json<crate::errors::OpenAiErrorResponse>)> {
    if !body.contains_key("model") {
        return Err(openai_error(
            StatusCode::BAD_REQUEST,
            "invalid_request_error",
            "Missing required field: model",
        ));
    }
    match body.get("messages") {
        Some(Value::Array(messages)) if !messages.is_empty() => Ok(()),
        Some(Value::Array(_)) => Err(openai_error(
            StatusCode::BAD_REQUEST,
            "invalid_request_error",
            "messages must not be empty",
        )),
        Some(_) => Err(openai_error(
            StatusCode::BAD_REQUEST,
            "invalid_request_error",
            "messages must be an array",
        )),
        None => Err(openai_error(
            StatusCode::BAD_REQUEST,
            "invalid_request_error",
            "Missing required field: messages",
        )),
    }
}

pub(crate) fn validate_anthropic_messages_request(
    body: &Map<String, Value>,
) -> Result<(), (StatusCode, Json<crate::errors::AnthropicErrorResponse>)> {
    if !body.contains_key("model") {
        return Err(anthropic_error(
            StatusCode::BAD_REQUEST,
            "invalid_request_error",
            "Missing required field: model",
        ));
    }
    if !body.contains_key("max_tokens") {
        return Err(anthropic_error(
            StatusCode::BAD_REQUEST,
            "invalid_request_error",
            "Missing required field: max_tokens",
        ));
    }
    match body.get("messages") {
        Some(Value::Array(_)) => Ok(()),
        Some(_) => Err(anthropic_error(
            StatusCode::BAD_REQUEST,
            "invalid_request_error",
            "messages must be an array",
        )),
        None => Err(anthropic_error(
            StatusCode::BAD_REQUEST,
            "invalid_request_error",
            "Missing required field: messages",
        )),
    }
}
