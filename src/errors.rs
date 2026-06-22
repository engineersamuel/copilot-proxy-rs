use axum::Json;
use http::StatusCode;
use serde::Serialize;

#[derive(Debug, Serialize)]
pub struct AnthropicErrorResponse {
    #[serde(rename = "type")]
    pub response_type: &'static str,
    pub error: AnthropicErrorBody,
}

#[derive(Debug, Serialize)]
pub struct AnthropicErrorBody {
    #[serde(rename = "type")]
    pub error_type: String,
    pub message: String,
}

#[derive(Debug, Serialize)]
pub struct OpenAiErrorResponse {
    pub error: OpenAiErrorBody,
}

#[derive(Debug, Serialize)]
pub struct OpenAiErrorBody {
    pub message: String,
    #[serde(rename = "type")]
    pub error_type: String,
    pub param: Option<String>,
    pub code: Option<String>,
}

pub fn anthropic_error(
    status: StatusCode,
    error_type: impl Into<String>,
    message: impl Into<String>,
) -> (StatusCode, Json<AnthropicErrorResponse>) {
    (
        status,
        Json(AnthropicErrorResponse {
            response_type: "error",
            error: AnthropicErrorBody {
                error_type: error_type.into(),
                message: message.into(),
            },
        }),
    )
}

pub fn openai_error(
    status: StatusCode,
    error_type: impl Into<String>,
    message: impl Into<String>,
) -> (StatusCode, Json<OpenAiErrorResponse>) {
    (
        status,
        Json(OpenAiErrorResponse {
            error: OpenAiErrorBody {
                message: message.into(),
                error_type: error_type.into(),
                param: None,
                code: None,
            },
        }),
    )
}
