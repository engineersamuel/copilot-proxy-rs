use axum::Json;
use axum::body::{Body, Bytes};
use axum::extract::State;
use axum::extract::rejection::BytesRejection;
use axum::response::{IntoResponse, Response};
use http::{HeaderMap, StatusCode};
use serde_json::Value;

use crate::copilot::request::adapt_openai_reasoning_effort;
use crate::errors::openai_error;
use crate::http::errors::{
    openai_copilot_error, request_body_error_details, request_body_rejection_details,
};
use crate::http::validation::validate_openai_chat_request;
use crate::request_body::parse_json_request_body_with_limit;
use crate::state::AppState;
use crate::telemetry::{ApiFamily, api_family_name, summarize_effective_request};

pub(crate) async fn chat_completions(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Result<Bytes, BytesRejection>,
) -> Response {
    let body = match body {
        Ok(body) => body,
        Err(rejection) => {
            let (status, message) = request_body_rejection_details(
                rejection,
                &headers,
                state.config.max_decoded_body_bytes,
                "chat_completions",
            );
            return openai_error(status, "invalid_request_error", message).into_response();
        }
    };
    match chat_completions_inner(state, headers, body).await {
        Ok(response) => response,
        Err((status, body)) => (status, body).into_response(),
    }
}

async fn chat_completions_inner(
    state: AppState,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Response, (StatusCode, Json<crate::errors::OpenAiErrorResponse>)> {
    let encoding = headers
        .get(http::header::CONTENT_ENCODING)
        .and_then(|value| value.to_str().ok())
        .unwrap_or("identity");
    let mut body = parse_json_request_body_with_limit(
        &body,
        encoding,
        state.config.max_decoded_body_bytes as usize,
    )
    .map_err(|err| {
        let (status, message) = request_body_error_details(&err);
        openai_error(status, "invalid_request_error", message)
    })?;
    validate_openai_chat_request(&body)?;
    state.copilot.refresh_models_if_stale().await;
    let stream = body.get("stream").and_then(Value::as_bool).unwrap_or(false);
    let requested_model = body
        .get("model")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let copilot_model = state
        .models
        .get_copilot_openai_model(&requested_model)
        .await;
    let supported_efforts = state.models.supported_efforts(&copilot_model).await;
    body.insert("model".to_string(), Value::String(copilot_model));
    let original_effort = body
        .get("reasoning_effort")
        .and_then(Value::as_str)
        .map(str::to_string);
    adapt_openai_reasoning_effort(&mut body, supported_efforts.as_ref());
    let retry_effort = body.get("reasoning_effort").and_then(Value::as_str);
    if let (Some(original), Some(retry)) = (original_effort.as_deref(), retry_effort) {
        if original != retry {
            tracing::warn!(
                model.requested = requested_model.as_str(),
                model.effective = body
                    .get("model")
                    .and_then(|value| value.as_str())
                    .unwrap_or(""),
                effort.original = original,
                effort.retry = retry,
                "chat effort downgraded"
            );
        }
    }
    if let Some(max_tokens) = body.remove("max_tokens") {
        body.entry("max_completion_tokens".to_string())
            .or_insert(max_tokens);
    }
    let use_responses_endpoint = state
        .models
        .model_supports_responses_api(&requested_model)
        .await
        && !state
            .models
            .model_supports_chat_completions_api(&requested_model)
            .await;
    let summary =
        summarize_effective_request(ApiFamily::ChatCompletions, Some(&requested_model), &body);
    tracing::info!(
        api.family = api_family_name(summary.api_family),
        model.requested = summary.requested_model.as_deref().unwrap_or(""),
        model.effective = summary.effective_model.as_deref().unwrap_or(""),
        stream = summary.stream,
        tokens.input.estimated = summary.input_tokens_estimate as u64,
        messages.count = summary.message_count as u64,
        tools.definitions = summary.tool_definition_count as u64,
        tools.results = summary.tool_result_count as u64,
        max_tokens = summary.max_tokens.unwrap_or(0),
        effort = summary.effort.as_deref().unwrap_or(""),
        "chat request prepared"
    );
    if use_responses_endpoint {
        let mut responses_body =
            crate::translate::responses_formats::openai_chat_to_responses_request(&body);
        if stream {
            responses_body.insert("stream".to_string(), Value::Bool(true));
            let upstream = state
                .copilot
                .stream_responses(responses_body, None)
                .await
                .map_err(openai_copilot_error)?;
            let byte_stream = crate::http::sse::map_sse_lines(
                upstream.bytes_stream(),
                crate::translate::responses_formats::responses_sse_to_openai_chat_sse_line,
            );
            return Ok(Response::builder()
                .header(http::header::CONTENT_TYPE, "text/event-stream")
                .body(Body::from_stream(byte_stream))
                .unwrap());
        }
        let responses = state
            .copilot
            .post_responses(responses_body, None)
            .await
            .map_err(openai_copilot_error)?;
        let chat_response =
            crate::translate::responses_formats::responses_to_openai_chat_response(&responses);
        return Ok(Json(chat_response).into_response());
    }
    if stream {
        body.insert("stream".to_string(), Value::Bool(true));
        body.insert(
            "stream_options".to_string(),
            serde_json::json!({"include_usage": true}),
        );
        let upstream = state
            .copilot
            .stream_chat(body, None)
            .await
            .map_err(openai_copilot_error)?;
        let byte_stream = crate::http::sse::map_sse_lines(upstream.bytes_stream(), |line| {
            Some(crate::translate::openai::normalize_openai_sse_line(line))
        });
        return Ok(Response::builder()
            .header(http::header::CONTENT_TYPE, "text/event-stream")
            .body(Body::from_stream(byte_stream))
            .unwrap());
    }
    let mut response = state
        .copilot
        .post_chat(body, None)
        .await
        .map_err(openai_copilot_error)?;
    crate::translate::openai::normalize_openai_response(&mut response, false);
    Ok(Json(response).into_response())
}
