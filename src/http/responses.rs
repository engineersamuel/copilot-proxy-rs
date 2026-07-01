use axum::Json;
use axum::body::{Body, Bytes};
use axum::extract::{Path, State};
use axum::response::{IntoResponse, Response};
use futures_util::{SinkExt, StreamExt};
use http::{HeaderMap, StatusCode};
use serde_json::{Map, Value};

use crate::errors::openai_error;
use crate::http::errors::openai_copilot_error;
use crate::request_body::parse_json_request_body_with_limit;
use crate::responses::request::PreviousResponseCacheStatus;
use crate::state::AppState;
use crate::telemetry::{
    ApiFamily, CacheOperation, api_family_name, summarize_cache, summarize_effective_request,
};

pub(crate) async fn responses(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
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

pub(crate) async fn responses_retrieve(
    State(state): State<AppState>,
    Path(response_id): Path<String>,
) -> Response {
    match state.copilot.get_response(&response_id, None).await {
        Ok(response) => Json(response).into_response(),
        Err(err) => openai_copilot_error(err).into_response(),
    }
}

pub(crate) async fn responses_cancel(
    State(state): State<AppState>,
    Path(response_id): Path<String>,
) -> Response {
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
