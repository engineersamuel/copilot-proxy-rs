use axum::Json;
use axum::body::{Body, Bytes};
use axum::extract::State;
use axum::extract::rejection::BytesRejection;
use axum::response::{IntoResponse, Response};
use futures_util::StreamExt;
use http::{HeaderMap, StatusCode};
use serde_json::{Map, Value};

use crate::copilot::request::{
    CopilotRequestMetadata, adapt_thinking_for_copilot, filter_anthropic_beta_header,
};
use crate::errors::anthropic_error;
use crate::http::errors::{
    anthropic_copilot_error, anthropic_local_error, anthropic_request_body_error_type,
    anthropic_responses_translation_error, request_body_error_details,
    request_body_rejection_details,
};
use crate::http::validation::validate_anthropic_messages_request;
use crate::models::LocalModelTarget;
use crate::request_body::parse_json_request_body_with_limit;
use crate::state::AppState;
use crate::telemetry::{ApiFamily, api_family_name, summarize_effective_request};

pub(crate) async fn messages(
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
                "messages",
            );
            return anthropic_error(status, anthropic_request_body_error_type(status), message)
                .into_response();
        }
    };
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
        let (status, message) = request_body_error_details(&err);
        anthropic_error(status, anthropic_request_body_error_type(status), message)
    })?;
    validate_anthropic_messages_request(&body)?;
    normalize_message_level_system(&mut body)?;
    let stream = body.get("stream").and_then(Value::as_bool).unwrap_or(false);
    let requested_model = body
        .get("model")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    if let Some(local_target) = state.models.configured_local_target(&requested_model) {
        return handle_local_messages(state, body, local_target, stream).await;
    }
    state.copilot.refresh_models_if_stale().await;
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
    let has_web_search = crate::translate::responses_formats::has_anthropic_web_search_tool(&body);
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
        if has_web_search {
            let mut responses_body =
                crate::translate::responses_formats::anthropic_web_search_to_responses_request(
                    &stream_body,
                    &state.config.web_search_model,
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
        }
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
            return handle_copilot_chat_messages(
                state,
                stream_body,
                requested_model,
                copilot_model,
                metadata,
                true,
            )
            .await;
        }
    }
    let response = if has_web_search {
        let responses_body =
            crate::translate::responses_formats::anthropic_web_search_to_responses_request(
                &body,
                &state.config.web_search_model,
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
    } else if state
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
        return handle_copilot_chat_messages(
            state,
            body,
            requested_model,
            copilot_model,
            metadata,
            false,
        )
        .await;
    };
    Ok(Json(response).into_response())
}

fn normalize_message_level_system(
    body: &mut Map<String, Value>,
) -> Result<(), (StatusCode, Json<crate::errors::AnthropicErrorResponse>)> {
    let has_message_level_system =
        body.get("messages")
            .and_then(Value::as_array)
            .is_some_and(|messages| {
                messages
                    .iter()
                    .any(|message| message.get("role").and_then(Value::as_str) == Some("system"))
            });
    if !has_message_level_system {
        return Ok(());
    }

    let mut system_blocks = Vec::new();
    if let Some(system) = body.remove("system") {
        append_system_blocks(&mut system_blocks, system)?;
    }

    let Some(messages) = body.get_mut("messages").and_then(Value::as_array_mut) else {
        return Err(anthropic_error(
            StatusCode::BAD_REQUEST,
            "invalid_request_error",
            "messages must be an array",
        ));
    };
    let original_messages = std::mem::take(messages);
    for message in original_messages {
        if message.get("role").and_then(Value::as_str) == Some("system") {
            let content = message.get("content").cloned().ok_or_else(|| {
                anthropic_error(
                    StatusCode::BAD_REQUEST,
                    "invalid_request_error",
                    "system message content is required",
                )
            })?;
            append_system_blocks(&mut system_blocks, content)?;
        } else {
            messages.push(message);
        }
    }
    body.insert("system".to_string(), Value::Array(system_blocks));
    Ok(())
}

fn append_system_blocks(
    blocks: &mut Vec<Value>,
    content: Value,
) -> Result<(), (StatusCode, Json<crate::errors::AnthropicErrorResponse>)> {
    match content {
        Value::String(text) => {
            blocks.push(serde_json::json!({"type": "text", "text": text}));
            Ok(())
        }
        Value::Array(content_blocks) => {
            if content_blocks.iter().any(|block| {
                block.get("type").and_then(Value::as_str) != Some("text")
                    || block.get("text").and_then(Value::as_str).is_none()
            }) {
                return Err(anthropic_error(
                    StatusCode::BAD_REQUEST,
                    "invalid_request_error",
                    "system message content blocks must be text blocks",
                ));
            }
            blocks.extend(content_blocks);
            Ok(())
        }
        _ => Err(anthropic_error(
            StatusCode::BAD_REQUEST,
            "invalid_request_error",
            "system message content must be a string or array of text blocks",
        )),
    }
}

/// Serves an Anthropic Messages request through Copilot's chat completions API.
///
/// Models such as Gemini expose only `/chat/completions` upstream, so requests
/// are translated Anthropic -> Responses -> chat completions, and responses are
/// translated back chat completions -> Responses -> Anthropic.
async fn handle_copilot_chat_messages(
    state: AppState,
    body: Map<String, Value>,
    requested_model: String,
    copilot_model: String,
    metadata: Option<CopilotRequestMetadata>,
    stream: bool,
) -> Result<Response, (StatusCode, Json<crate::errors::AnthropicErrorResponse>)> {
    let mut responses_body =
        crate::translate::responses_formats::anthropic_messages_to_responses_request(
            &body,
            &copilot_model,
        );
    responses_body.insert("stream".to_string(), Value::Bool(stream));
    let translated = crate::local::responses_to_chat(responses_body, &copilot_model)
        .map_err(anthropic_responses_translation_error)?;

    if !stream {
        let chat = state
            .copilot
            .post_chat(translated.chat_body, metadata)
            .await
            .map_err(anthropic_copilot_error)?;
        let response_id = format!("msg_{}", uuid::Uuid::new_v4().simple());
        let responses = crate::local::responses::chat_to_responses_with_tool_names(
            &chat,
            &response_id,
            &requested_model,
            &translated.tool_kinds,
            &translated.tool_names,
        )
        .map_err(|_| {
            anthropic_error(
                StatusCode::BAD_GATEWAY,
                "server_error",
                "upstream model returned invalid response",
            )
        })?;
        let anthropic =
            crate::translate::responses_formats::responses_to_anthropic_message_response(
                &responses,
                &requested_model,
            );
        return Ok(Json(anthropic).into_response());
    }

    let upstream = state
        .copilot
        .stream_chat(translated.chat_body, metadata)
        .await
        .map_err(anthropic_copilot_error)?;
    if !has_event_stream_content_type(&upstream) {
        return Err(anthropic_error(
            StatusCode::BAD_GATEWAY,
            "server_error",
            "upstream model returned invalid response",
        ));
    }
    let response_id = format!("msg_{}", uuid::Uuid::new_v4().simple());
    let adapter = std::sync::Arc::new(std::sync::Mutex::new(
        crate::local::ChatToResponsesStream::new_with_tool_names(
            response_id,
            requested_model,
            translated.tool_kinds,
            translated.tool_names,
        ),
    ));
    let mapper_adapter = adapter.clone();
    let mapped = crate::http::sse::map_sse_lines_many(
        upstream.bytes_stream(),
        state.config.max_decoded_body_bytes as usize,
        move |line| {
            let responses_events = mapper_adapter
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .map_line(line);
            responses_events
                .into_iter()
                .flat_map(|event| responses_event_to_anthropic_frames(&event))
                .collect()
        },
    );
    let byte_stream = async_stream::stream! {
        futures_util::pin_mut!(mapped);
        let mut emitted_any = false;
        while let Some(event) = mapped.next().await {
            match event {
                Ok(event) => {
                    emitted_any = true;
                    yield Ok::<Bytes, std::io::Error>(event);
                }
                Err(_) => {
                    let failed_events = adapter
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner())
                        .fail();
                    for failed_event in failed_events {
                        for frame in responses_event_to_anthropic_frames(&failed_event) {
                            yield Ok::<Bytes, std::io::Error>(Bytes::from(format!("{frame}\n\n")));
                        }
                    }
                    if !emitted_any {
                        yield Err(std::io::Error::other(
                            "upstream model stream failed before first event",
                        ));
                    }
                    return;
                }
            }
        }
    };
    Ok(Response::builder()
        .header(http::header::CONTENT_TYPE, "text/event-stream")
        .body(Body::from_stream(byte_stream))
        .unwrap())
}

async fn handle_local_messages(
    state: AppState,
    body: Map<String, Value>,
    target: LocalModelTarget,
    stream: bool,
) -> Result<Response, (StatusCode, Json<crate::errors::AnthropicErrorResponse>)> {
    let summary = summarize_effective_request(ApiFamily::Messages, Some(&target.public_id), &body);
    tracing::info!(
        api.family = api_family_name(summary.api_family),
        model.requested = summary.requested_model.as_deref().unwrap_or(""),
        model.effective = target.public_id.as_str(),
        stream = summary.stream,
        tokens.input.estimated = summary.input_tokens_estimate as u64,
        messages.count = summary.message_count as u64,
        tools.definitions = summary.tool_definition_count as u64,
        tools.results = summary.tool_result_count as u64,
        max_tokens = summary.max_tokens.unwrap_or(0),
        effort = summary.effort.as_deref().unwrap_or(""),
        "messages local request prepared"
    );

    let responses_body =
        crate::translate::responses_formats::anthropic_messages_to_responses_request(
            &body,
            &target.public_id,
        );
    let translated = crate::local::responses_to_chat(responses_body, &target.upstream_model)
        .map_err(anthropic_responses_translation_error)?;

    if stream {
        let upstream = state
            .local
            .stream_chat(&target, translated.chat_body)
            .await
            .map_err(anthropic_local_error)?;
        if !has_event_stream_content_type(&upstream) {
            return Err(anthropic_error(
                StatusCode::BAD_GATEWAY,
                "server_error",
                "local model returned invalid response",
            ));
        }

        let response_id = format!("resp_local_{}", uuid::Uuid::new_v4().simple());
        let adapter = std::sync::Arc::new(std::sync::Mutex::new(
            crate::local::ChatToResponsesStream::new_with_tool_names(
                response_id,
                target.public_id.clone(),
                translated.tool_kinds,
                translated.tool_names,
            ),
        ));
        let mapper_adapter = adapter.clone();
        let mapped = crate::http::sse::map_sse_lines_many(
            upstream.bytes_stream(),
            state.config.max_decoded_body_bytes as usize,
            move |line| {
                let responses_events = mapper_adapter
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .map_line(line);
                responses_events
                    .into_iter()
                    .flat_map(|event| responses_event_to_anthropic_frames(&event))
                    .collect()
            },
        );
        let byte_stream = async_stream::stream! {
            futures_util::pin_mut!(mapped);
            let mut emitted_any = false;
            while let Some(event) = mapped.next().await {
                match event {
                    Ok(event) => {
                        emitted_any = true;
                        yield Ok::<Bytes, std::io::Error>(event);
                    }
                    Err(_) => {
                        let failed_events = adapter
                            .lock()
                            .unwrap_or_else(|poisoned| poisoned.into_inner())
                            .fail();
                        for failed_event in failed_events {
                            for frame in responses_event_to_anthropic_frames(&failed_event) {
                                yield Ok::<Bytes, std::io::Error>(Bytes::from(format!(
                                    "{frame}\n\n"
                                )));
                            }
                        }
                        if !emitted_any {
                            yield Err(std::io::Error::other(
                                "local model stream failed before first event",
                            ));
                        }
                        return;
                    }
                }
            }
        };
        return Ok(Response::builder()
            .header(http::header::CONTENT_TYPE, "text/event-stream")
            .body(Body::from_stream(byte_stream))
            .unwrap());
    }

    let chat = state
        .local
        .post_chat(&target, translated.chat_body)
        .await
        .map_err(anthropic_local_error)?;
    let response_id = format!("resp_local_{}", uuid::Uuid::new_v4().simple());
    let responses = crate::local::responses::chat_to_responses_with_tool_names(
        &chat,
        &response_id,
        &target.public_id,
        &translated.tool_kinds,
        &translated.tool_names,
    )
    .map_err(|_| {
        anthropic_error(
            StatusCode::BAD_GATEWAY,
            "server_error",
            "local model returned invalid response",
        )
    })?;
    let anthropic = crate::translate::responses_formats::responses_to_anthropic_message_response(
        &responses,
        &target.public_id,
    );
    Ok(Json(anthropic).into_response())
}

fn has_event_stream_content_type(response: &reqwest::Response) -> bool {
    response
        .headers()
        .get(http::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.split(';').next())
        .is_some_and(|media_type| media_type.trim().eq_ignore_ascii_case("text/event-stream"))
}

fn responses_event_to_anthropic_frames(event: &str) -> Vec<String> {
    let mut frames = Vec::new();
    for line in event.lines() {
        let Some(mapped) =
            crate::translate::responses_formats::responses_sse_to_anthropic_sse_line(line)
        else {
            continue;
        };
        for frame in mapped.split("\n\n") {
            if !frame.is_empty() {
                frames.push(frame.to_string());
            }
        }
    }
    frames
}
