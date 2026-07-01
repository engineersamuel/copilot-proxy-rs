use axum::Json;
use axum::body::{Body, Bytes};
use axum::extract::State;
use axum::response::{IntoResponse, Response};
use futures_util::StreamExt;
use http::{HeaderMap, StatusCode};
use serde_json::Value;

use crate::copilot::request::{
    CopilotRequestMetadata, adapt_thinking_for_copilot, filter_anthropic_beta_header,
};
use crate::errors::anthropic_error;
use crate::http::errors::anthropic_copilot_error;
use crate::http::validation::validate_anthropic_messages_request;
use crate::request_body::parse_json_request_body_with_limit;
use crate::state::AppState;
use crate::telemetry::{ApiFamily, api_family_name, summarize_effective_request};

pub(crate) async fn messages(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    match messages_inner(state, headers, body).await {
        Ok(response) => response,
        Err((status, body)) => (status, body).into_response(),
    }
}

async fn messages_inner(
    state: AppState,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Response, (StatusCode, Json<crate::errors::AnthropicErrorResponse>)> {
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
        anthropic_error(
            StatusCode::BAD_REQUEST,
            "invalid_request_error",
            err.to_string(),
        )
    })?;
    validate_anthropic_messages_request(&body)?;
    state.copilot.refresh_models_if_stale().await;
    let stream = body.get("stream").and_then(Value::as_bool).unwrap_or(false);
    let requested_model = body
        .get("model")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let metadata = {
        let mut meta = CopilotRequestMetadata::default();
        if let Some(beta_value) = headers.get("anthropic-beta").and_then(|v| v.to_str().ok()) {
            if let Some(filtered) = filter_anthropic_beta_header(beta_value) {
                meta.extra_headers
                    .insert("anthropic-beta".to_string(), filtered);
            }
        }
        if meta.extra_headers.is_empty() {
            None
        } else {
            Some(meta)
        }
    };
    let copilot_model = state
        .models
        .get_copilot_openai_model(&requested_model)
        .await;
    let supported_efforts = state.models.supported_efforts(&copilot_model).await;
    let summary = summarize_effective_request(ApiFamily::Messages, Some(&requested_model), &body);
    tracing::info!(
        api.family = api_family_name(summary.api_family),
        model.requested = summary.requested_model.as_deref().unwrap_or(""),
        model.effective = copilot_model.as_str(),
        stream = summary.stream,
        tokens.input.estimated = summary.input_tokens_estimate as u64,
        messages.count = summary.message_count as u64,
        tools.definitions = summary.tool_definition_count as u64,
        tools.results = summary.tool_result_count as u64,
        max_tokens = summary.max_tokens.unwrap_or(0),
        effort = summary.effort.as_deref().unwrap_or(""),
        "messages request prepared"
    );
    if stream {
        let mut stream_body = body.clone();
        if state
            .models
            .model_supports_messages_api(&copilot_model)
            .await
        {
            stream_body.insert("model".to_string(), Value::String(copilot_model.clone()));
            stream_body.insert("stream".to_string(), Value::Bool(true));
            adapt_thinking_for_copilot(
                &mut stream_body,
                &copilot_model,
                supported_efforts.as_ref(),
            );
            let upstream = state
                .copilot
                .stream_messages(stream_body, metadata)
                .await
                .map_err(anthropic_copilot_error)?;
            let byte_stream = upstream
                .bytes_stream()
                .map(|chunk| chunk.map_err(std::io::Error::other));
            return Ok(Response::builder()
                .header(http::header::CONTENT_TYPE, "text/event-stream")
                .body(Body::from_stream(byte_stream))
                .unwrap());
        } else if state
            .models
            .model_supports_responses_api(&copilot_model)
            .await
        {
            let mut responses_body =
                crate::translate::responses_formats::anthropic_messages_to_responses_request(
                    &stream_body,
                    &copilot_model,
                );
            responses_body.insert("stream".to_string(), Value::Bool(true));
            let upstream = state
                .copilot
                .stream_responses(responses_body, metadata)
                .await
                .map_err(anthropic_copilot_error)?;
            let byte_stream = crate::http::sse::map_sse_lines(
                upstream.bytes_stream(),
                crate::translate::responses_formats::responses_sse_to_anthropic_sse_line,
            );
            return Ok(Response::builder()
                .header(http::header::CONTENT_TYPE, "text/event-stream")
                .body(Body::from_stream(byte_stream))
                .unwrap());
        } else {
            return Err(anthropic_error(
                StatusCode::BAD_REQUEST,
                "invalid_request_error",
                "streaming not supported for this model via OpenAI translation",
            ));
        }
    }
    let response = if state
        .models
        .model_supports_messages_api(&copilot_model)
        .await
    {
        body.insert("model".to_string(), Value::String(copilot_model.clone()));
        adapt_thinking_for_copilot(&mut body, &copilot_model, supported_efforts.as_ref());
        state
            .copilot
            .post_messages(body, metadata.clone())
            .await
            .map_err(anthropic_copilot_error)?
    } else if state
        .models
        .model_supports_responses_api(&copilot_model)
        .await
    {
        let responses_body =
            crate::translate::responses_formats::anthropic_messages_to_responses_request(
                &body,
                &copilot_model,
            );
        let responses = state
            .copilot
            .post_responses(responses_body, metadata.clone())
            .await
            .map_err(anthropic_copilot_error)?;
        crate::translate::responses_formats::responses_to_anthropic_message_response(
            &responses,
            &requested_model,
        )
    } else {
        let openai_body =
            crate::translate::anthropic::anthropic_to_openai_request(&body, &copilot_model);
        let openai_response = state
            .copilot
            .post_chat(openai_body, None)
            .await
            .map_err(anthropic_copilot_error)?;
        crate::translate::anthropic::openai_to_anthropic_response(
            &openai_response,
            &requested_model,
        )
    };
    Ok(Json(response).into_response())
}
