use axum::Json;
use axum::body::{Body, Bytes};
use axum::extract::rejection::BytesRejection;
use axum::extract::{Path, State};
use axum::response::{IntoResponse, Response};
use futures_util::{SinkExt, StreamExt};
use http::HeaderMap;
use serde_json::{Map, Value};

use crate::copilot::request::{
    ENCRYPTED_FUNCTION_OUTPUT_DECRYPTION_ERROR, strip_agent_message_encrypted_content,
};
use crate::errors::openai_error;
use crate::http::errors::{
    openai_copilot_error, openai_local_error, openai_responses_translation_error,
    request_body_error_details, request_body_rejection_details,
};
use crate::models::LocalModelTarget;
use crate::request_body::parse_json_request_body_with_limit;
use crate::responses::request::PreviousResponseCacheStatus;
use crate::state::AppState;
use crate::telemetry::{
    ApiFamily, CacheOperation, api_family_name, summarize_cache, summarize_effective_request,
    summarize_request_sizes,
};

type CopilotResponseByteStream =
    std::pin::Pin<Box<dyn futures_util::Stream<Item = Result<Bytes, reqwest::Error>> + Send>>;

#[derive(Debug, Default)]
struct ResponsesStreamDiagnostics {
    events_seen: u64,
    terminal_seen: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ResponsesStreamPrefixDecision {
    Forward,
    RetryWithoutAgentMessageEncryptedContent,
}

async fn buffer_copilot_responses_stream_prefix<S>(
    stream: &mut std::pin::Pin<Box<S>>,
    max_buffer_bytes: usize,
) -> Result<(Vec<Bytes>, ResponsesStreamPrefixDecision), std::io::Error>
where
    S: futures_util::Stream<Item = Result<Bytes, reqwest::Error>>,
{
    let mut chunks = Vec::new();
    let mut buffered_bytes = 0usize;
    let mut line = Vec::new();

    while let Some(chunk_result) = stream.next().await {
        let chunk = chunk_result.map_err(std::io::Error::other)?;
        buffered_bytes = buffered_bytes
            .checked_add(chunk.len())
            .filter(|size| *size <= max_buffer_bytes)
            .ok_or_else(|| {
                std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "SSE prefix exceeds configured limit",
                )
            })?;

        for byte in &chunk {
            if *byte != b'\n' {
                line.push(*byte);
                if line.len() > max_buffer_bytes {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::InvalidData,
                        "SSE line exceeds configured limit",
                    ));
                }
                continue;
            }

            if line.last() == Some(&b'\r') {
                line.pop();
            }
            let decision =
                classify_copilot_responses_stream_prefix_line(&String::from_utf8_lossy(&line));
            line.clear();
            if let Some(decision) = decision {
                chunks.push(chunk);
                return Ok((chunks, decision));
            }
        }
        chunks.push(chunk);
    }

    if line.last() == Some(&b'\r') {
        line.pop();
    }
    let decision = classify_copilot_responses_stream_prefix_line(&String::from_utf8_lossy(&line))
        .unwrap_or(ResponsesStreamPrefixDecision::Forward);
    Ok((chunks, decision))
}

fn classify_copilot_responses_stream_prefix_line(
    line: &str,
) -> Option<ResponsesStreamPrefixDecision> {
    let payload = line.strip_prefix("data:")?;
    let event = serde_json::from_str::<Value>(payload.trim_start()).ok()?;
    match event.get("type").and_then(Value::as_str)? {
        "response.created" | "response.in_progress" | "response.queued" => None,
        "response.failed" if is_retryable_early_responses_failure(&event) => {
            Some(ResponsesStreamPrefixDecision::RetryWithoutAgentMessageEncryptedContent)
        }
        _ => Some(ResponsesStreamPrefixDecision::Forward),
    }
}

fn is_retryable_early_responses_failure(event: &Value) -> bool {
    let response = event.get("response").unwrap_or(&Value::Null);
    let error = response
        .get("error")
        .or_else(|| event.get("error"))
        .unwrap_or(&Value::Null);
    let code = error.get("code").and_then(Value::as_str).unwrap_or("");
    let error_type = error.get("type").and_then(Value::as_str).unwrap_or("");
    let message = error.get("message").and_then(Value::as_str).unwrap_or("");
    (code.is_empty() && error_type.is_empty() && message.is_empty())
        || message
            .to_ascii_lowercase()
            .contains(ENCRYPTED_FUNCTION_OUTPUT_DECRYPTION_ERROR)
}

fn copilot_response_request_id(response: &reqwest::Response) -> String {
    ["x-request-id", "x-github-request-id", "request-id"]
        .into_iter()
        .find_map(|name| response.headers().get(name))
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default()
        .to_string()
}

fn observe_copilot_responses_stream_line(
    line: &str,
    diagnostics: &mut ResponsesStreamDiagnostics,
    request_id: &str,
    upstream_request_id: &str,
    started: std::time::Instant,
) {
    let Some(payload) = line.strip_prefix("data:") else {
        return;
    };
    let Ok(event) = serde_json::from_str::<Value>(payload.trim_start()) else {
        return;
    };
    let Some(event_type) = event.get("type").and_then(Value::as_str) else {
        return;
    };
    diagnostics.events_seen += 1;
    diagnostics.terminal_seen |= matches!(
        event_type,
        "response.completed" | "response.incomplete" | "response.failed"
    );
    if event_type != "response.failed" {
        return;
    }
    let response = event.get("response").unwrap_or(&Value::Null);
    let error = response
        .get("error")
        .or_else(|| event.get("error"))
        .unwrap_or(&Value::Null);
    let response_id = response.get("id").and_then(Value::as_str).unwrap_or("");
    let response_status = response
        .get("status")
        .and_then(Value::as_str)
        .unwrap_or("failed");
    let error_code = error.get("code").and_then(Value::as_str).unwrap_or("");
    let error_type = error.get("type").and_then(Value::as_str).unwrap_or("");
    let error_message = error.get("message").and_then(Value::as_str).unwrap_or("");
    tracing::warn!(
        api.family = "responses",
        request.id = request_id,
        upstream.request_id = upstream_request_id,
        response.id = response_id,
        response.status = response_status,
        stream.events = diagnostics.events_seen,
        elapsed.ms = started.elapsed().as_millis() as u64,
        upstream.error.code = error_code,
        upstream.error.type = error_type,
        upstream.error.message_bytes = error_message.len() as u64,
        "copilot responses stream failed"
    );
}

fn log_unterminated_copilot_responses_stream(
    diagnostics: &ResponsesStreamDiagnostics,
    stream_end: crate::http::sse::SseStreamEnd,
    request_id: &str,
    upstream_request_id: &str,
    started: std::time::Instant,
) {
    if diagnostics.terminal_seen {
        return;
    }
    let end = match stream_end {
        crate::http::sse::SseStreamEnd::Eof => "eof",
        crate::http::sse::SseStreamEnd::TransportError => "transport_error",
    };
    tracing::warn!(
        api.family = "responses",
        request.id = request_id,
        upstream.request_id = upstream_request_id,
        stream.end = end,
        stream.events = diagnostics.events_seen,
        elapsed.ms = started.elapsed().as_millis() as u64,
        "copilot responses stream ended without terminal event"
    );
}

pub(crate) async fn responses(
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
                "responses",
            );
            return openai_error(status, "invalid_request_error", message).into_response();
        }
    };
    let request_body_bytes = body.len();
    let request_id = headers
        .get("x-request-id")
        .and_then(|value| value.to_str().ok())
        .map(str::to_string)
        .unwrap_or_else(|| format!("resp-{}", uuid::Uuid::new_v4().simple()));
    let encoding = headers
        .get(http::header::CONTENT_ENCODING)
        .and_then(|value| value.to_str().ok())
        .unwrap_or("identity");
    let body = match parse_json_request_body_with_limit(
        &body,
        encoding,
        state.config.max_decoded_body_bytes as usize,
    ) {
        Ok(body) => body,
        Err(err) => {
            let (status, message) = request_body_error_details(&err);
            return openai_error(status, "invalid_request_error", message).into_response();
        }
    };
    let requested_model = body
        .get("model")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    if let Some(local_target) = state.models.configured_local_target(&requested_model) {
        return handle_local_responses(state, headers, body, local_target).await;
    }
    state.copilot.refresh_models_if_stale().await;
    let stream = body.get("stream").and_then(Value::as_bool).unwrap_or(false);
    let copilot_model = state
        .models
        .get_copilot_openai_model(&requested_model)
        .await;
    let supported_efforts = state.models.supported_efforts(&copilot_model).await;
    let copilot_model_id = copilot_model.clone();
    let prepared = crate::responses::request::prepare_responses_request(
        &state.responses,
        body,
        request_id,
        &headers,
        copilot_model,
        supported_efforts.as_ref(),
    )
    .await;
    let summary = summarize_effective_request(
        ApiFamily::Responses,
        Some(&requested_model),
        &prepared.effective_body,
    );
    let size_summary = summarize_request_sizes(&prepared.effective_body);
    tracing::info!(
        api.family = api_family_name(summary.api_family),
        model.requested = summary.requested_model.as_deref().unwrap_or(""),
        model.effective = summary.effective_model.as_deref().unwrap_or(""),
        stream = summary.stream,
        request.body.bytes = request_body_bytes as u64,
        request.body.limit = state.config.max_decoded_body_bytes,
        request.body.effective_bytes = size_summary.body_bytes as u64,
        tokens.input.estimated = summary.input_tokens_estimate as u64,
        input.items = summary.input_item_count as u64,
        input.bytes = size_summary.input_bytes as u64,
        input.tools.bytes = size_summary.input_tool_bytes as u64,
        input.reasoning.bytes = size_summary.input_reasoning_bytes as u64,
        input.largest_item.bytes = size_summary.largest_input_item_bytes as u64,
        input.largest_item.index = size_summary.largest_input_item_index.unwrap_or(0) as u64,
        input.largest_item.type = size_summary.largest_input_item_type.unwrap_or(""),
        input.largest_item.role = size_summary.largest_input_item_role.unwrap_or(""),
        tools.bytes = size_summary.tools_bytes as u64,
        tools.definitions = summary.tool_definition_count as u64,
        tools.results = summary.tool_result_count as u64,
        effort = summary.effort.as_deref().unwrap_or(""),
        "responses request prepared"
    );
    match prepared.cache_status {
        PreviousResponseCacheStatus::Hit => {
            let cache = summarize_cache(
                CacheOperation::Hit,
                summary.input_item_count.checked_sub(1),
                None,
            );
            tracing::info!(
                cache.operation = "hit",
                cache.transcript_items = cache.transcript_items.unwrap_or(0) as u64,
                "responses cache event"
            );
        }
        PreviousResponseCacheStatus::Miss => {
            tracing::info!(cache.operation = "miss", "responses cache event");
        }
        PreviousResponseCacheStatus::NotRequested => {}
    }
    if !state
        .models
        .model_supports_responses_api(&copilot_model_id)
        .await
        && state
            .models
            .model_supports_chat_completions_api(&copilot_model_id)
            .await
    {
        return handle_copilot_chat_responses(
            state,
            prepared.effective_body,
            requested_model,
            copilot_model_id,
            Some(prepared.request_metadata),
            stream,
        )
        .await;
    }
    if stream {
        let stream_started = std::time::Instant::now();
        let mut fallback_body = prepared.effective_body.clone();
        let stripped_encrypted_content = strip_agent_message_encrypted_content(&mut fallback_body);
        let diagnostic_request_id = prepared
            .request_metadata
            .request_id
            .clone()
            .unwrap_or_default();
        let retry_metadata = prepared.request_metadata.clone();
        let upstream = match state
            .copilot
            .stream_responses(prepared.effective_body, Some(prepared.request_metadata))
            .await
        {
            Ok(upstream) => upstream,
            Err(err) => return openai_copilot_error(err).into_response(),
        };
        let initial_upstream_request_id = copilot_response_request_id(&upstream);
        let mut upstream_stream = Box::pin(upstream.bytes_stream());
        let (byte_stream, upstream_request_id) = if stripped_encrypted_content > 0 {
            let (prefix, decision) = match buffer_copilot_responses_stream_prefix(
                &mut upstream_stream,
                state.config.max_decoded_body_bytes as usize,
            )
            .await
            {
                Ok(result) => result,
                Err(error) => {
                    return openai_error(
                        http::StatusCode::BAD_GATEWAY,
                        "server_error",
                        format!("Copilot response stream failed: {error}"),
                    )
                    .into_response();
                }
            };
            match decision {
                ResponsesStreamPrefixDecision::Forward => {
                    let prefix_stream = futures_util::stream::iter(prefix.into_iter().map(Ok));
                    (
                        Box::pin(prefix_stream.chain(upstream_stream)) as CopilotResponseByteStream,
                        initial_upstream_request_id,
                    )
                }
                ResponsesStreamPrefixDecision::RetryWithoutAgentMessageEncryptedContent => {
                    tracing::warn!(
                        api.family = "responses",
                        stream = true,
                        retry.trigger = "early_response_failed",
                        input.encrypted_content.stripped = stripped_encrypted_content as u64,
                        "copilot responses retrying without agent message encrypted content"
                    );
                    let retry = match state
                        .copilot
                        .stream_responses(fallback_body, Some(retry_metadata))
                        .await
                    {
                        Ok(retry) => retry,
                        Err(error) => return openai_copilot_error(error).into_response(),
                    };
                    let retry_upstream_request_id = copilot_response_request_id(&retry);
                    (
                        Box::pin(retry.bytes_stream()) as CopilotResponseByteStream,
                        retry_upstream_request_id,
                    )
                }
            }
        } else {
            (
                upstream_stream as CopilotResponseByteStream,
                initial_upstream_request_id,
            )
        };
        let diagnostics =
            std::sync::Arc::new(std::sync::Mutex::new(ResponsesStreamDiagnostics::default()));
        let observer_diagnostics = diagnostics.clone();
        let observer_request_id = diagnostic_request_id.clone();
        let observer_upstream_request_id = upstream_request_id.clone();
        let end_diagnostics = diagnostics.clone();
        let byte_stream = crate::http::sse::inspect_sse_lines(
            byte_stream,
            state.config.max_decoded_body_bytes as usize,
            move |line| {
                let mut diagnostics = observer_diagnostics
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                observe_copilot_responses_stream_line(
                    line,
                    &mut diagnostics,
                    &observer_request_id,
                    &observer_upstream_request_id,
                    stream_started,
                );
            },
            move |stream_end| {
                let diagnostics = end_diagnostics
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                log_unterminated_copilot_responses_stream(
                    &diagnostics,
                    stream_end,
                    &diagnostic_request_id,
                    &upstream_request_id,
                    stream_started,
                );
            },
        );
        return Response::builder()
            .header(http::header::CONTENT_TYPE, "text/event-stream")
            .body(Body::from_stream(byte_stream))
            .unwrap();
    }
    let response = match state
        .copilot
        .post_responses(
            prepared.effective_body.clone(),
            Some(prepared.request_metadata),
        )
        .await
    {
        Ok(response) => response,
        Err(err) => return openai_copilot_error(err).into_response(),
    };
    if let (Some(id), Some(input), Some(output)) = (
        response.get("id").and_then(Value::as_str),
        crate::responses::request::normalize_input_items(prepared.effective_body.get("input")),
        response.get("output").and_then(Value::as_array).cloned(),
    ) {
        let has_tool_calls = output.iter().any(|item| {
            item.get("type")
                .and_then(Value::as_str)
                .is_some_and(|kind| matches!(kind, "function_call" | "custom_tool_call"))
        });
        let transcript_items = input.len() + output.len();
        state
            .responses
            .cache_response_state(id, input, output, prepared.identity, has_tool_calls)
            .await;
        let cache = summarize_cache(
            CacheOperation::Write,
            Some(transcript_items),
            Some(has_tool_calls),
        );
        tracing::info!(
            cache.operation = "write",
            cache.transcript_items = cache.transcript_items.unwrap_or(0) as u64,
            cache.last_response_had_tool_calls =
                cache.last_response_had_tool_calls.unwrap_or(false),
            "responses cache event"
        );
    }
    Json(response).into_response()
}

/// Serves an OpenAI Responses request through Copilot's chat completions API.
///
/// Models such as Gemini reject the upstream Responses API, so requests are
/// translated Responses -> chat completions and responses are translated back.
async fn handle_copilot_chat_responses(
    state: AppState,
    body: Map<String, Value>,
    requested_model: String,
    copilot_model: String,
    metadata: Option<crate::copilot::request::CopilotRequestMetadata>,
    stream: bool,
) -> Response {
    let translated = match crate::local::responses_to_chat(body, &copilot_model) {
        Ok(translated) => translated,
        Err(error) => return openai_responses_translation_error(error).into_response(),
    };
    let response_id = format!("resp_{}", uuid::Uuid::new_v4().simple());

    if !stream {
        let chat = match state
            .copilot
            .post_chat(translated.chat_body, metadata)
            .await
        {
            Ok(chat) => chat,
            Err(error) => return openai_copilot_error(error).into_response(),
        };
        return match crate::local::responses::chat_to_responses_with_tool_names(
            &chat,
            &response_id,
            &requested_model,
            &translated.tool_kinds,
            &translated.tool_names,
        ) {
            Ok(response) => Json(response).into_response(),
            Err(_) => openai_error(
                http::StatusCode::BAD_GATEWAY,
                "server_error",
                "upstream model returned invalid response",
            )
            .into_response(),
        };
    }

    let upstream = match state
        .copilot
        .stream_chat(translated.chat_body, metadata)
        .await
    {
        Ok(upstream) => upstream,
        Err(error) => return openai_copilot_error(error).into_response(),
    };
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
            mapper_adapter
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .map_line(line)
        },
    );
    let byte_stream = async_stream::stream! {
        futures_util::pin_mut!(mapped);
        while let Some(event) = mapped.next().await {
            match event {
                Ok(event) => yield Ok::<Bytes, std::io::Error>(event),
                Err(_) => {
                    let failed_events = adapter
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner())
                        .fail();
                    for failed_event in failed_events {
                        yield Ok::<Bytes, std::io::Error>(Bytes::from(format!(
                            "{failed_event}\n\n"
                        )));
                    }
                    yield Ok::<Bytes, std::io::Error>(Bytes::from_static(b"data: [DONE]\n\n"));
                    return;
                }
            }
        }
        let failed_events = adapter
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .fail();
        for failed_event in failed_events {
            yield Ok::<Bytes, std::io::Error>(Bytes::from(format!("{failed_event}\n\n")));
        }
        yield Ok::<Bytes, std::io::Error>(Bytes::from_static(b"data: [DONE]\n\n"));
    };
    Response::builder()
        .header(http::header::CONTENT_TYPE, "text/event-stream")
        .body(Body::from_stream(byte_stream))
        .unwrap()
}

async fn handle_local_responses(
    state: AppState,
    headers: HeaderMap,
    body: Map<String, Value>,
    target: LocalModelTarget,
) -> Response {
    let requested_previous_response_id = match body.get("previous_response_id") {
        None | Some(Value::Null) => None,
        Some(Value::String(previous_response_id)) => Some(previous_response_id.clone()),
        Some(_) => {
            return openai_error(
                http::StatusCode::BAD_REQUEST,
                "invalid_request_error",
                "previous_response_id must be a string",
            )
            .into_response();
        }
    };
    let expanded =
        crate::responses::request::expand_previous_response(&state.responses, body).await;
    if expanded.cache_status == PreviousResponseCacheStatus::Miss {
        return openai_error(
            http::StatusCode::BAD_REQUEST,
            "invalid_request_error",
            "previous_response_id was not found",
        )
        .into_response();
    }
    let mut effective_body = expanded.body;
    let identity = crate::responses::request::prepare_responses_turn_identity(
        &mut effective_body,
        &headers,
        expanded.previous_identity.as_ref(),
    );
    let stream = effective_body
        .get("stream")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let translated = match crate::local::responses_to_chat(effective_body, &target.upstream_model) {
        Ok(translated) => translated,
        Err(error) => return openai_responses_translation_error(error).into_response(),
    };
    if stream {
        let upstream = match state.local.stream_chat(&target, translated.chat_body).await {
            Ok(upstream) => upstream,
            Err(error) => return openai_local_error(error).into_response(),
        };
        let is_event_stream = upstream
            .headers()
            .get(http::header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .is_some_and(|value| {
                value.split(';').next().is_some_and(|media_type| {
                    media_type.trim().eq_ignore_ascii_case("text/event-stream")
                })
            });
        if !is_event_stream {
            return openai_error(
                http::StatusCode::BAD_GATEWAY,
                "server_error",
                "local model returned invalid response",
            )
            .into_response();
        }

        let response_id = format!("resp_local_{}", uuid::Uuid::new_v4().simple());
        let adapter = std::sync::Arc::new(std::sync::Mutex::new(
            crate::local::responses::ChatToResponsesStream::new_with_previous_response_id_and_tool_names(
                response_id.clone(),
                target.public_id.clone(),
                (expanded.cache_status == PreviousResponseCacheStatus::Hit)
                    .then(|| requested_previous_response_id.clone())
                    .flatten(),
                translated.tool_kinds,
                translated.tool_names,
            ),
        ));
        let mapper_adapter = adapter.clone();
        let mapped = crate::http::sse::map_sse_lines_many(
            upstream.bytes_stream(),
            state.config.max_decoded_body_bytes as usize,
            move |line| {
                mapper_adapter
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .map_line(line)
            },
        );
        let cache_state = state.clone();
        let cache_response_id = response_id;
        let mut cache_payload = Some((translated.input_items, identity));
        let byte_stream = async_stream::stream! {
            futures_util::pin_mut!(mapped);
            while let Some(event) = mapped.next().await {
                let event = match event {
                    Ok(event) => event,
                    Err(_) => {
                        let failed_events = adapter
                            .lock()
                            .unwrap_or_else(|poisoned| poisoned.into_inner())
                            .fail();
                        for failed_event in failed_events {
                            yield Ok::<Bytes, std::io::Error>(Bytes::from(format!(
                                "{failed_event}\n\n"
                            )));
                        }
                        yield Ok::<Bytes, std::io::Error>(Bytes::from_static(
                            b"data: [DONE]\n\n",
                        ));
                        return;
                    }
                };
                let terminal_event = std::str::from_utf8(&event).ok().and_then(|text| {
                    if text.starts_with("event: response.completed\n") {
                        Some(true)
                    } else if text.starts_with("event: response.incomplete\n")
                        || text.starts_with("event: response.failed\n")
                    {
                        Some(false)
                    } else {
                        None
                    }
                });
                let Some(completed_event) = terminal_event else {
                    yield Ok::<Bytes, std::io::Error>(event);
                    continue;
                };
                let (completed, output) = {
                    let adapter = adapter
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner());
                    (
                        completed_event && adapter.is_completed(),
                        adapter.output_items(),
                    )
                };
                if completed {
                    if let Some((input, identity)) = cache_payload.take() {
                        let has_tool_calls = output.iter().any(|item| {
                            item.get("type")
                                .and_then(Value::as_str)
                                .is_some_and(|kind| {
                                    matches!(kind, "function_call" | "custom_tool_call")
                                })
                        });
                        cache_state
                            .responses
                            .cache_response_state(
                                &cache_response_id,
                                input,
                                output,
                                identity,
                                has_tool_calls,
                            )
                            .await;
                    }
                }
                yield Ok::<Bytes, std::io::Error>(event);
                yield Ok::<Bytes, std::io::Error>(Bytes::from_static(b"data: [DONE]\n\n"));
                return;
            }
            let failed_events = adapter
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .fail();
            for failed_event in failed_events {
                yield Ok::<Bytes, std::io::Error>(Bytes::from(format!("{failed_event}\n\n")));
            }
            yield Ok::<Bytes, std::io::Error>(Bytes::from_static(b"data: [DONE]\n\n"));
        };
        return Response::builder()
            .header(http::header::CONTENT_TYPE, "text/event-stream")
            .body(Body::from_stream(byte_stream))
            .unwrap();
    }

    let chat = match state.local.post_chat(&target, translated.chat_body).await {
        Ok(chat) => chat,
        Err(error) => return openai_local_error(error).into_response(),
    };
    let response_id = format!("resp_local_{}", uuid::Uuid::new_v4().simple());
    let mut response = match crate::local::responses::chat_to_responses_with_tool_names(
        &chat,
        &response_id,
        &target.public_id,
        &translated.tool_kinds,
        &translated.tool_names,
    ) {
        Ok(response) => response,
        Err(_) => {
            return openai_error(
                http::StatusCode::BAD_GATEWAY,
                "server_error",
                "local model returned invalid response",
            )
            .into_response();
        }
    };
    if expanded.cache_status == PreviousResponseCacheStatus::Hit {
        if let (Some(response), Some(previous_response_id)) =
            (response.as_object_mut(), requested_previous_response_id)
        {
            response.insert(
                "previous_response_id".to_string(),
                Value::String(previous_response_id),
            );
        }
    }
    if let (Some("completed"), Some(output)) = (
        response.get("status").and_then(Value::as_str),
        response.get("output").and_then(Value::as_array).cloned(),
    ) {
        let has_tool_calls = output.iter().any(|item| {
            item.get("type")
                .and_then(Value::as_str)
                .is_some_and(|kind| matches!(kind, "function_call" | "custom_tool_call"))
        });
        state
            .responses
            .cache_response_state(
                response_id,
                translated.input_items,
                output,
                identity,
                has_tool_calls,
            )
            .await;
    }
    Json(response).into_response()
}

pub(crate) async fn responses_ws(
    State(state): State<AppState>,
    headers: HeaderMap,
    ws: axum::extract::WebSocketUpgrade,
) -> Response {
    if !crate::http::auth::request_has_valid_api_key(&headers, &state.config.api_key) {
        return crate::http::auth::unauthorized_ws_response();
    }
    if !crate::http::auth::request_has_allowed_origin(&headers, &state.config.allowed_origins) {
        return crate::http::auth::forbidden_ws_response("WebSocket origin is not allowed");
    }
    ws.on_upgrade(move |socket| handle_responses_ws(state, socket))
}

async fn handle_responses_ws(state: AppState, mut client_ws: axum::extract::ws::WebSocket) {
    use axum::extract::ws::Message;

    tracing::info!(api.family = "responses_ws", "responses websocket connected");
    while let Some(Ok(message)) = client_ws.recv().await {
        let Message::Text(raw) = message else {
            continue;
        };
        let mut body: Map<String, Value> = match serde_json::from_str::<Value>(&raw) {
            Ok(Value::Object(map)) => map,
            _ => {
                let _ = client_ws
                    .send(Message::Text(
                        serde_json::json!({
                            "type": "error",
                            "error": {"type": "invalid_request_error", "message": "Invalid JSON"}
                        })
                        .to_string()
                        .into(),
                    ))
                    .await;
                continue;
            }
        };
        let is_typed_response_create =
            body.get("type").and_then(Value::as_str) == Some("response.create");
        let requested_model = body
            .get("model")
            .and_then(Value::as_str)
            .unwrap_or_default();
        if state
            .models
            .configured_local_target(requested_model)
            .is_some()
        {
            let _ = client_ws
                .send(Message::Text(
                    serde_json::json!({
                        "type": "error",
                        "error": {
                            "type": "invalid_request_error",
                            "message": "Local models do not support Responses WebSocket"
                        }
                    })
                    .to_string()
                    .into(),
                ))
                .await;
            continue;
        }
        if is_typed_response_create {
            body.remove("type");
        }
        if body.get("generate").and_then(Value::as_bool) == Some(false) {
            let response_id = format!("resp_prewarm_{}", uuid::Uuid::new_v4().simple());
            let _ = client_ws
                .send(Message::Text(
                    serde_json::json!({
                        "type": "response.created",
                        "response": {"id": response_id, "object": "response", "status": "completed", "output": []}
                    })
                    .to_string()
                    .into(),
                ))
                .await;
            let _ = client_ws
                .send(Message::Text(
                    serde_json::json!({
                        "type": "response.completed",
                        "response": {
                            "id": response_id,
                            "object": "response",
                            "status": "completed",
                            "output": [],
                            "usage": {"input_tokens": 0, "output_tokens": 0, "total_tokens": 0}
                        }
                    })
                    .to_string()
                    .into(),
                ))
                .await;
            continue;
        }
        let headers = HeaderMap::new();
        state.copilot.refresh_models_if_stale().await;
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
        let prepared = crate::responses::request::prepare_responses_request(
            &state.responses,
            body.clone(),
            format!("ws-{}", uuid::Uuid::new_v4().simple()),
            &headers,
            copilot_model,
            supported_efforts.as_ref(),
        )
        .await;
        let mut backend_body = prepared.effective_body.clone();
        backend_body.insert("stream".to_string(), Value::Bool(true));
        match state
            .copilot
            .connect_responses_websocket(&backend_body, Some(prepared.request_metadata))
            .await
        {
            Ok(mut backend_ws) => {
                let _ = backend_ws
                    .send(tokio_tungstenite::tungstenite::Message::Text(
                        serde_json::to_string(&backend_body).unwrap().into(),
                    ))
                    .await;
                while let Some(next) = backend_ws.next().await {
                    match next {
                        Ok(tokio_tungstenite::tungstenite::Message::Text(text)) => {
                            let text = text.to_string();
                            let kind = serde_json::from_str::<Value>(&text).ok().and_then(|v| {
                                v.get("type").and_then(Value::as_str).map(str::to_string)
                            });
                            let terminal = kind
                                .as_deref()
                                .is_some_and(|k| k == "response.completed" || k == "error");
                            if client_ws.send(Message::Text(text.into())).await.is_err() {
                                return;
                            }
                            if terminal {
                                if let Some(ref kind) = kind {
                                    tracing::info!(
                                        api.family = "responses_ws",
                                        event.type = kind.as_str(),
                                        "responses websocket completed"
                                    );
                                }
                                break;
                            }
                        }
                        Ok(tokio_tungstenite::tungstenite::Message::Close(_)) => break,
                        Err(err) => {
                            let _ = client_ws
                                .send(Message::Text(
                                    serde_json::json!({
                                        "type": "error",
                                        "status": 502,
                                        "error": {"type": "connection_error", "message": err.to_string()}
                                    })
                                    .to_string()
                                    .into(),
                                ))
                                .await;
                            break;
                        }
                        _ => {}
                    }
                }
            }
            Err(err) => {
                let _ = client_ws
                    .send(Message::Text(
                        serde_json::json!({
                            "type": "error",
                            "status": 502,
                            "error": {"type": "connection_error", "message": err.to_string()}
                        })
                        .to_string()
                        .into(),
                    ))
                    .await;
            }
        }
    }
}

fn is_local_response_id(response_id: &str) -> bool {
    response_id.starts_with("resp_local_")
}

fn unsupported_local_response_resource() -> Response {
    openai_error(
        http::StatusCode::BAD_REQUEST,
        "invalid_request_error",
        "Retrieval and cancellation of local response resources are unsupported",
    )
    .into_response()
}

pub(crate) async fn responses_retrieve(
    State(state): State<AppState>,
    Path(response_id): Path<String>,
) -> Response {
    if is_local_response_id(&response_id) {
        return unsupported_local_response_resource();
    }
    match state.copilot.get_response(&response_id, None).await {
        Ok(response) => Json(response).into_response(),
        Err(err) => openai_copilot_error(err).into_response(),
    }
}

pub(crate) async fn responses_cancel(
    State(state): State<AppState>,
    Path(response_id): Path<String>,
) -> Response {
    if is_local_response_id(&response_id) {
        return unsupported_local_response_resource();
    }
    match state.copilot.cancel_response(&response_id, None).await {
        Ok(response) => Json(response).into_response(),
        Err(err) => openai_copilot_error(err).into_response(),
    }
}

pub(crate) async fn responses_compact() -> Response {
    Json(serde_json::json!({
        "id": format!("resp_compact_{}", uuid::Uuid::new_v4().simple()),
        "object": "response.compaction",
        "created_at": std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs(),
        "output": [],
        "usage": {"input_tokens": 0, "output_tokens": 0, "total_tokens": 0}
    }))
    .into_response()
}
