use axum::body::{Body, Bytes};
use axum::extract::State;
use axum::middleware;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use bytes::Bytes as RawBytes;
use futures_util::{SinkExt, StreamExt};
use http::{HeaderMap, StatusCode};
use serde::Serialize;
use serde_json::{Map, Value};

use crate::copilot::request::{
    CopilotRequestMetadata, adapt_openai_reasoning_effort, adapt_thinking_for_copilot,
    filter_anthropic_beta_header,
};
use crate::errors::{anthropic_error, openai_error};
use crate::models::ModelsListResponse;
use crate::request_body::parse_json_request_body_with_limit;
use crate::responses::request::PreviousResponseCacheStatus;
use crate::state::AppState;
use crate::telemetry::{
    ApiFamily, CacheOperation, api_family_name, summarize_cache, summarize_effective_request,
};

pub fn router(state: AppState) -> Router {
    let protected = Router::new()
        .route("/v1/models", get(list_models))
        .route("/v1/messages/count_tokens", post(count_tokens))
        .route("/v1/messages", post(messages))
        .route("/v1/chat/completions", post(chat_completions))
        .route("/v1/responses", post(responses).get(responses_ws))
        .route("/v1/responses/compact", post(responses_compact))
        .route("/v1/responses/{response_id}", get(responses_retrieve))
        .route("/v1/responses/{response_id}/cancel", post(responses_cancel))
        .route_layer(middleware::from_fn_with_state(
            state.clone(),
            crate::http::auth::require_inbound_auth,
        ));

    Router::new()
        .route("/health", get(health))
        .route("/version", get(version))
        .merge(protected)
        .with_state(state)
}

#[derive(Debug, Serialize)]
struct RuntimeInfo {
    implementation: &'static str,
    pid: u32,
}

#[derive(Debug, Serialize)]
struct HealthResponse {
    status: &'static str,
    version: &'static str,
    backend: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    fallback: Option<&'static str>,
    runtime: RuntimeInfo,
}

#[derive(Debug, Serialize)]
struct VersionResponse {
    version: &'static str,
    runtime: RuntimeInfo,
}

#[derive(Debug, Serialize)]
struct CountTokensResponse {
    input_tokens: usize,
}

async fn health(State(state): State<AppState>) -> Json<HealthResponse> {
    let snapshot = state.backend.snapshot().await;
    Json(HealthResponse {
        status: "ok",
        version: env!("CARGO_PKG_VERSION"),
        backend: snapshot.primary.as_str(),
        fallback: snapshot.fallback.map(|backend| backend.as_str()),
        runtime: runtime_info(),
    })
}

async fn version() -> Json<VersionResponse> {
    Json(VersionResponse {
        version: env!("CARGO_PKG_VERSION"),
        runtime: runtime_info(),
    })
}

async fn list_models(State(state): State<AppState>) -> Json<ModelsListResponse> {
    let snapshot = state.backend.snapshot().await;
    if snapshot.primary == crate::state::BackendKind::Copilot {
        state.copilot.refresh_models_if_stale().await;
    }
    Json(state.models.list_for_snapshot(snapshot).await)
}

async fn count_tokens(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Json<CountTokensResponse>, (StatusCode, Json<crate::errors::AnthropicErrorResponse>)> {
    let encoding = headers
        .get(http::header::CONTENT_ENCODING)
        .and_then(|value| value.to_str().ok())
        .unwrap_or("identity");
    let body = parse_json_request_body_with_limit(
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
    Ok(Json(CountTokensResponse {
        input_tokens: crate::telemetry::estimate_request_tokens(&body),
    }))
}

async fn chat_completions(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
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
        openai_error(
            StatusCode::BAD_REQUEST,
            "invalid_request_error",
            err.to_string(),
        )
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
            let byte_stream = {
                let mut src = Box::pin(upstream.bytes_stream());
                async_stream::stream! {
                    let mut buf = String::new();
                    while let Some(chunk_result) = src.next().await {
                        match chunk_result {
                            Err(e) => {
                                yield Err(std::io::Error::other(e));
                                return;
                            }
                            Ok(chunk) => buf.push_str(&String::from_utf8_lossy(&chunk)),
                        }
                        while let Some(nl) = buf.find('\n') {
                            let line = buf[..nl].trim_end_matches('\r').to_string();
                            buf.drain(..=nl);
                            if let Some(normalized) =
                                crate::translate::responses_formats::responses_sse_to_openai_chat_sse_line(&line)
                            {
                                yield Ok::<RawBytes, std::io::Error>(RawBytes::from(
                                    format!("{normalized}\n"),
                                ));
                            }
                        }
                    }
                    if !buf.is_empty() {
                        let line = buf.trim_end_matches('\r').to_string();
                        if let Some(normalized) =
                            crate::translate::responses_formats::responses_sse_to_openai_chat_sse_line(&line)
                        {
                            yield Ok(RawBytes::from(format!("{normalized}\n")));
                        }
                    }
                }
            };
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
        let byte_stream = {
            let mut src = Box::pin(upstream.bytes_stream());
            async_stream::stream! {
                let mut buf = String::new();
                while let Some(chunk_result) = src.next().await {
                    match chunk_result {
                        Err(e) => {
                            yield Err(std::io::Error::other(e));
                            return;
                        }
                        Ok(chunk) => buf.push_str(&String::from_utf8_lossy(&chunk)),
                    }
                    // Emit every complete line (those terminated by \n).
                    while let Some(nl) = buf.find('\n') {
                        let line = buf[..nl].trim_end_matches('\r').to_string();
                        buf.drain(..=nl);
                        let normalized =
                            crate::translate::openai::normalize_openai_sse_line(&line);
                        yield Ok::<RawBytes, std::io::Error>(RawBytes::from(
                            format!("{normalized}\n"),
                        ));
                    }
                }
                // Flush any partial line left at stream end (shouldn't occur in valid SSE).
                if !buf.is_empty() {
                    let line = buf.trim_end_matches('\r').to_string();
                    let normalized = crate::translate::openai::normalize_openai_sse_line(&line);
                    yield Ok(RawBytes::from(format!("{normalized}\n")));
                }
            }
        };
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

async fn messages(State(state): State<AppState>, headers: HeaderMap, body: Bytes) -> Response {
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
            let byte_stream = {
                let mut src = Box::pin(upstream.bytes_stream());
                async_stream::stream! {
                    let mut buf = String::new();
                    while let Some(chunk_result) = src.next().await {
                        match chunk_result {
                            Err(e) => {
                                yield Err(std::io::Error::other(e));
                                return;
                            }
                            Ok(chunk) => buf.push_str(&String::from_utf8_lossy(&chunk)),
                        }
                        while let Some(nl) = buf.find('\n') {
                            let line = buf[..nl].trim_end_matches('\r').to_string();
                            buf.drain(..=nl);
                            if let Some(normalized) =
                                crate::translate::responses_formats::responses_sse_to_anthropic_sse_line(&line)
                            {
                                yield Ok::<RawBytes, std::io::Error>(RawBytes::from(
                                    format!("{normalized}\n"),
                                ));
                            }
                        }
                    }
                    if !buf.is_empty() {
                        let line = buf.trim_end_matches('\r').to_string();
                        if let Some(normalized) =
                            crate::translate::responses_formats::responses_sse_to_anthropic_sse_line(&line)
                        {
                            yield Ok(RawBytes::from(format!("{normalized}\n")));
                        }
                    }
                }
            };
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

async fn responses(State(state): State<AppState>, headers: HeaderMap, body: Bytes) -> Response {
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
            return openai_error(
                StatusCode::BAD_REQUEST,
                "invalid_request_error",
                err.to_string(),
            )
            .into_response();
        }
    };
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
    tracing::info!(
        api.family = api_family_name(summary.api_family),
        model.requested = summary.requested_model.as_deref().unwrap_or(""),
        model.effective = summary.effective_model.as_deref().unwrap_or(""),
        stream = summary.stream,
        tokens.input.estimated = summary.input_tokens_estimate as u64,
        input.items = summary.input_item_count as u64,
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
    if stream {
        let upstream = match state
            .copilot
            .stream_responses(prepared.effective_body, Some(prepared.request_metadata))
            .await
        {
            Ok(upstream) => upstream,
            Err(err) => return openai_copilot_error(err).into_response(),
        };
        let byte_stream = upstream
            .bytes_stream()
            .map(|chunk| chunk.map_err(std::io::Error::other));
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

async fn responses_ws(
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
        if body.get("type").and_then(Value::as_str) == Some("response.create") {
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

async fn responses_retrieve(
    State(state): State<AppState>,
    axum::extract::Path(response_id): axum::extract::Path<String>,
) -> Response {
    match state.copilot.get_response(&response_id, None).await {
        Ok(response) => Json(response).into_response(),
        Err(err) => openai_copilot_error(err).into_response(),
    }
}

async fn responses_cancel(
    State(state): State<AppState>,
    axum::extract::Path(response_id): axum::extract::Path<String>,
) -> Response {
    match state.copilot.cancel_response(&response_id, None).await {
        Ok(response) => Json(response).into_response(),
        Err(err) => openai_copilot_error(err).into_response(),
    }
}

async fn responses_compact() -> Response {
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

fn runtime_info() -> RuntimeInfo {
    RuntimeInfo {
        implementation: "rust",
        pid: std::process::id(),
    }
}

fn validate_openai_chat_request(
    body: &serde_json::Map<String, Value>,
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

fn validate_anthropic_messages_request(
    body: &serde_json::Map<String, Value>,
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

fn openai_copilot_error(
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

fn anthropic_copilot_error(
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
