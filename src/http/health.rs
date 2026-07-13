use axum::Json;
use axum::body::Bytes;
use axum::extract::State;
use axum::extract::rejection::BytesRejection;
use http::{HeaderMap, StatusCode};
use serde::Serialize;

use crate::errors::{AnthropicErrorResponse, anthropic_error};
use crate::http::errors::{
    anthropic_request_body_error_type, request_body_error_details, request_body_rejection_details,
};
use crate::models::{CopilotModelsDebugSnapshot, ModelsListResponse};
use crate::request_body::parse_json_request_body_with_limit;
use crate::state::AppState;

#[derive(Debug, Serialize)]
pub(crate) struct RuntimeInfo {
    implementation: &'static str,
    pid: u32,
}

#[derive(Debug, Serialize)]
pub(crate) struct HealthResponse {
    status: &'static str,
    version: &'static str,
    backend: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    fallback: Option<&'static str>,
    runtime: RuntimeInfo,
}

#[derive(Debug, Serialize)]
pub(crate) struct VersionResponse {
    version: &'static str,
    runtime: RuntimeInfo,
}

#[derive(Debug, Serialize)]
pub(crate) struct CountTokensResponse {
    input_tokens: usize,
}

#[derive(Debug, Serialize)]
pub(crate) struct CopilotModelsDebugResponse {
    status: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    warning: Option<String>,
    snapshot: CopilotModelsDebugSnapshot,
}

pub(crate) async fn health(State(state): State<AppState>) -> Json<HealthResponse> {
    let snapshot = state.backend.snapshot().await;
    Json(HealthResponse {
        status: "ok",
        version: env!("CARGO_PKG_VERSION"),
        backend: snapshot.primary.as_str(),
        fallback: snapshot.fallback.map(|backend| backend.as_str()),
        runtime: runtime_info(),
    })
}

pub(crate) async fn version() -> Json<VersionResponse> {
    Json(VersionResponse {
        version: env!("CARGO_PKG_VERSION"),
        runtime: runtime_info(),
    })
}

pub(crate) async fn list_models(State(state): State<AppState>) -> Json<ModelsListResponse> {
    let snapshot = state.backend.snapshot().await;
    if snapshot.primary == crate::state::BackendKind::Copilot {
        state.copilot.refresh_models_if_stale().await;
    }
    Json(state.models.list_for_snapshot(snapshot).await)
}

pub(crate) async fn debug_copilot_models(
    State(state): State<AppState>,
) -> Result<Json<CopilotModelsDebugResponse>, (StatusCode, Json<AnthropicErrorResponse>)> {
    let refresh_result = state.copilot.refresh_models_if_stale_result().await;
    let snapshot = state.models.debug_snapshot().await;
    match refresh_result {
        Ok(()) => Ok(Json(CopilotModelsDebugResponse {
            status: "ok",
            warning: None,
            snapshot,
        })),
        Err(error) if snapshot.fetched => Ok(Json(CopilotModelsDebugResponse {
            status: "stale",
            warning: Some(error.to_string()),
            snapshot,
        })),
        Err(error) => Err(anthropic_error(
            StatusCode::BAD_GATEWAY,
            "api_error",
            format!("failed to refresh Copilot models: {error}"),
        )),
    }
}

pub(crate) async fn count_tokens(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Result<Bytes, BytesRejection>,
) -> Result<Json<CountTokensResponse>, (StatusCode, Json<crate::errors::AnthropicErrorResponse>)> {
    let body = body.map_err(|rejection| {
        let (status, message) = request_body_rejection_details(
            rejection,
            &headers,
            state.config.max_decoded_body_bytes,
            "messages_count_tokens",
        );
        anthropic_error(status, anthropic_request_body_error_type(status), message)
    })?;
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
        let (status, message) = request_body_error_details(&err);
        anthropic_error(status, anthropic_request_body_error_type(status), message)
    })?;
    Ok(Json(CountTokensResponse {
        input_tokens: crate::telemetry::estimate_request_tokens(&body),
    }))
}

pub(crate) fn runtime_info() -> RuntimeInfo {
    RuntimeInfo {
        implementation: "rust",
        pid: std::process::id(),
    }
}
