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
use crate::models::CopilotModelsDebugSnapshot;
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

pub(crate) async fn list_models(State(state): State<AppState>) -> Json<serde_json::Value> {
    Json(build_models_value(&state).await)
}

/// The enriched, exclusion-filtered model list shared by `GET` and `POST /v1/models`.
pub(crate) async fn build_models_value(state: &AppState) -> serde_json::Value {
    let snapshot = state.backend.snapshot().await;
    if snapshot.primary == crate::state::BackendKind::Copilot {
        state.copilot.refresh_models_if_stale().await;
    }
    let list = state.models.list_for_snapshot(snapshot).await;
    let mut value = serde_json::to_value(&list).unwrap_or_default();
    exclude_models(
        &mut value,
        &crate::models::model_exclude_set(&state.config.model_excludes),
    );
    if merge_model_insights(&mut value, &state.model_stats, &state.models).await {
        value["stats_attribution"] = crate::model_stats::attribution();
    }
    mark_latest_opus_for_ui(&mut value);
    value
}

/// Numeric version segments and `-fast` flag from `claude-opus-5.5[-fast]`.
fn opus_version(id: &str) -> Option<(Vec<u32>, bool)> {
    let rest = id.strip_prefix("claude-opus-")?;
    let (rest, fast) = match rest.strip_suffix("-fast") {
        Some(r) => (r, true),
        None => (rest, false),
    };
    let version = rest
        .split(['.', '-'])
        .map(|p| p.parse::<u32>().ok())
        .collect::<Option<Vec<_>>>()?;
    Some((version, fast))
}

/// Id of the latest listed Opus: highest numeric version, non-fast on ties.
pub(crate) fn latest_opus_id<'a>(ids: impl Iterator<Item = &'a str>) -> Option<&'a str> {
    ids.filter_map(|id| {
        let (v, fast) = opus_version(id)?;
        Some(((v, !fast), id))
    })
    .max_by(|a, b| a.0.cmp(&b.0))
    .map(|(_, id)| id)
}

/// User-specified preference: the latest listed Opus is the UI/visual model.
fn mark_latest_opus_for_ui(value: &mut serde_json::Value) {
    let Some(entries) = value.get_mut("data").and_then(|d| d.as_array_mut()) else {
        return;
    };
    let latest = latest_opus_id(
        entries
            .iter()
            .filter(|e| e.get("ui_eligible").is_some())
            .filter_map(|e| e.get("id")?.as_str()),
    )
    .map(str::to_string);
    for entry in entries.iter_mut() {
        if let Some(obj) = entry.as_object_mut() {
            obj.remove("ui_eligible");
        }
    }
    let Some(latest) = latest else { return };
    let Some(entry) = entries
        .iter_mut()
        .find(|e| e.get("id").and_then(|i| i.as_str()) == Some(latest.as_str()))
        .and_then(|e| e.as_object_mut())
    else {
        return;
    };
    let description = entry
        .get("description")
        .and_then(|d| d.as_str())
        .map(|d| format!("{} Preferred for UI and visual work.", d.trim_end()))
        .unwrap_or_else(|| "Preferred for UI and visual work.".to_string());
    entry.insert("description".into(), description.into());
    let tags = entry
        .entry("excels_at")
        .or_insert_with(|| serde_json::json!([]));
    if let Some(tags) = tags.as_array_mut() {
        for tag in ["ui-design", "visual-work"] {
            if !tags.iter().any(|t| t == tag) {
                tags.push(tag.into());
            }
        }
    }
}

/// Drop excluded ids from both listings. Listing only; routing is unchanged.
fn exclude_models(value: &mut serde_json::Value, excluded: &crate::models::ModelExcludes) {
    for (list, key) in [("data", "id"), ("models", "slug")] {
        if let Some(items) = value.get_mut(list).and_then(|v| v.as_array_mut()) {
            items.retain(|item| {
                !item
                    .get(key)
                    .and_then(|v| v.as_str())
                    .is_some_and(|id| excluded.contains(id))
            });
        }
    }
}

/// Add optional stats fields to each Copilot-routed `data` entry. Returns true
/// when any entry carries Artificial Analysis stats.
async fn merge_model_insights(
    value: &mut serde_json::Value,
    store: &std::sync::Arc<crate::model_stats::ModelStatsStore>,
    registry: &crate::models::ModelRegistry,
) -> bool {
    let Some(entries) = value.get_mut("data").and_then(|d| d.as_array_mut()) else {
        return false;
    };
    let mut eligible = Vec::new();
    for entry in entries.iter() {
        let Some(id) = entry.get("id").and_then(|id| id.as_str()) else {
            continue;
        };
        if registry.configured_local_target(id).is_some() || registry.has_copilot_override(id).await
        {
            continue;
        }
        eligible.push(id.to_string());
    }
    store.sync_models(&eligible);
    store.spawn_refresh_if_stale(&eligible);
    let mut any_stats = false;
    for entry in entries {
        let Some(id) = entry.get("id").and_then(|id| id.as_str()) else {
            continue;
        };
        if !eligible.iter().any(|e| e == id) {
            continue;
        }
        let Some(insights) = store.insights_for(id) else {
            continue;
        };
        any_stats |= insights.stats.is_some();
        if let Some(obj) = entry.as_object_mut() {
            obj.insert("ui_eligible".into(), true.into());
        }
        if let (Some(target), Ok(serde_json::Value::Object(extra))) =
            (entry.as_object_mut(), serde_json::to_value(&insights))
        {
            for (key, field) in extra {
                target.entry(key).or_insert(field);
            }
        }
    }
    any_stats
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

#[cfg(test)]
mod tests {
    use super::latest_opus_id;

    #[test]
    fn latest_opus_numeric_and_variants() {
        fn pick<'a>(ids: &[&'a str]) -> Option<&'a str> {
            latest_opus_id(ids.iter().copied())
        }
        assert_eq!(
            pick(&["claude-opus-5.9", "claude-opus-5.10"]),
            Some("claude-opus-5.10")
        );
        assert_eq!(
            pick(&["claude-opus-5.5-fast", "claude-opus-5.5"]),
            Some("claude-opus-5.5")
        );
        assert_eq!(
            pick(&["claude-opus-4.8", "claude-opus-5.5-fast"]),
            Some("claude-opus-5.5-fast")
        );
        assert_eq!(
            pick(&["claude-opus-4.8-fast"]),
            Some("claude-opus-4.8-fast")
        );
        assert_eq!(pick(&["claude-sonnet-5.5"]), None);
    }
}
