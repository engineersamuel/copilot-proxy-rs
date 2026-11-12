//! `POST /v1/models`: ask the TypeSafe JEV API which listed model best fits a task.
//!
//! The candidate list is the same enriched, exclusion-filtered list that
//! `GET /v1/models` returns. Neither the key nor the input text is logged.

use std::sync::OnceLock;
use std::time::Duration;

use axum::Json;
use axum::body::Bytes;
use axum::extract::State;
use http::StatusCode;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::errors::{AnthropicErrorResponse, anthropic_error};
use crate::http::health::build_models_value;
use crate::state::AppState;

const MAX_INPUT_BYTES: usize = 16 * 1024;
/// Raw request cap, enforced by the route's body limit before buffering.
pub(crate) const MAX_BODY_BYTES: usize = 64 * 1024;
const MAX_JEV_RESPONSE_BYTES: usize = 256 * 1024;
const JEV_TIMEOUT: Duration = Duration::from_secs(20);
const MAX_CRITERIA_CHARS: usize = 400;
const NEUTRAL_TEXT: &str = "General-purpose model; no further details known.";
macro_rules! pick_instructions {
    () => {
        "Pick the model that best fits the task described in the input. \
Capability fit comes first: choose the model whose strengths match what the task needs. \
Prefer cheaper or faster models only when the task is clearly simple."
    };
}
const PICK_INSTRUCTIONS: &str = pick_instructions!();
const INSTRUCTIONS: &str = concat!(
    pick_instructions!(),
    " Any UI, frontend, CSS, layout, design, visual or image-related task must go to the latest Opus model, \
the one whose strengths mention ui-design and visual-work."
);

const UI_INSTRUCTIONS: &str =
    "Decide whether the task is a UI, frontend, CSS, layout, design, visual or image-related task.";
const UI_TRUE: &str = "The task is about UI, frontend, CSS, layout, design, visuals or images.";
const UI_FALSE: &str =
    "The task is not about UI, frontend, CSS, layout, design, visuals or images.";

type ApiError = (StatusCode, Json<AnthropicErrorResponse>);

#[derive(Deserialize)]
struct RouteRequest {
    input: String,
    #[serde(default, rename = "type", deserialize_with = "present")]
    model_type: Option<Value>,
}

/// Keeps an explicit `null` as `Some(Null)` so only an absent field means the default.
fn present<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Option<Value>, D::Error> {
    Value::deserialize(d).map(Some)
}

/// Model `type` the router chooses between, as listed by `GET /v1/models`.
#[derive(Clone, Copy, PartialEq, Eq)]
enum RouteType {
    Chat,
    Image,
    Decisions,
}

impl RouteType {
    fn from_value(value: Option<&Value>) -> Option<Self> {
        match value.map(Value::as_str) {
            None => Some(Self::Chat),
            Some(Some("chat")) => Some(Self::Chat),
            Some(Some("image")) => Some(Self::Image),
            Some(Some("decisions")) => Some(Self::Decisions),
            Some(_) => None,
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::Chat => "chat",
            Self::Image => "image",
            Self::Decisions => "decisions",
        }
    }
}

#[derive(Deserialize)]
struct JevResponse {
    answers: JevAnswers,
}

#[derive(Deserialize)]
struct JevAnswers {
    pick: JevPick,
    #[serde(default)]
    is_ui_or_visual: Option<JevPick>,
}

#[derive(Deserialize)]
struct JevPick {
    choice: String,
    #[serde(default)]
    probabilities: serde_json::Map<String, Value>,
    #[serde(default)]
    confidence: Option<f64>,
}

fn client() -> &'static reqwest::Client {
    static CLIENT: OnceLock<reqwest::Client> = OnceLock::new();
    CLIENT.get_or_init(|| {
        reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(JEV_TIMEOUT)
            .build()
            .unwrap_or_default()
    })
}

/// Read the upstream body incrementally, failing once it exceeds the cap.
async fn read_capped_json<T: serde::de::DeserializeOwned>(
    mut response: reqwest::Response,
) -> Result<T, ApiError> {
    if response
        .content_length()
        .is_some_and(|len| len > MAX_JEV_RESPONSE_BYTES as u64)
    {
        return Err(upstream_error());
    }
    let mut buf = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(|_| upstream_error())? {
        if buf.len() + chunk.len() > MAX_JEV_RESPONSE_BYTES {
            return Err(upstream_error());
        }
        buf.extend_from_slice(&chunk);
    }
    serde_json::from_slice(&buf).map_err(|_| upstream_error())
}

fn valid_probability(value: &Value) -> Option<f64> {
    value
        .as_f64()
        .filter(|p| p.is_finite() && (0.0..=1.0).contains(p))
}

fn bad_request(message: &str) -> ApiError {
    anthropic_error(StatusCode::BAD_REQUEST, "invalid_request_error", message)
}

fn upstream_error() -> ApiError {
    anthropic_error(
        StatusCode::BAD_GATEWAY,
        "api_error",
        "model router upstream request failed",
    )
}

fn parse_request(body: &[u8]) -> Result<(String, RouteType), ApiError> {
    let request: RouteRequest = serde_json::from_slice(body)
        .map_err(|_| bad_request("body must be JSON like {\"input\": \"<text>\"}"))?;
    let route_type = RouteType::from_value(request.model_type.as_ref())
        .ok_or_else(|| bad_request("type must be one of: chat, image, decisions"))?;
    let input = request.input.trim();
    if input.is_empty() {
        return Err(bad_request("input must not be empty"));
    }
    if input.len() > MAX_INPUT_BYTES {
        return Err(bad_request("input exceeds 16 KiB"));
    }
    Ok((input.to_string(), route_type))
}

fn compact(value: f64) -> String {
    format!("{value:.2}")
        .trim_end_matches('0')
        .trim_end_matches('.')
        .to_string()
}

/// One criteria text per listed model: description, strengths, compact stats.
pub(crate) fn criteria_text(entry: &Value) -> String {
    let mut text = entry
        .get("description")
        .and_then(Value::as_str)
        .unwrap_or(NEUTRAL_TEXT)
        .to_string();
    let tags: Vec<&str> = entry
        .get("excels_at")
        .and_then(Value::as_array)
        .map(|a| a.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default();
    if !tags.is_empty() {
        text.push_str(" Strengths: ");
        text.push_str(&tags.join(", "));
        text.push('.');
    }
    if let Some(stats) = entry.get("stats") {
        let mut hints = Vec::new();
        if let Some(v) = stats.get("intelligence_index").and_then(Value::as_f64) {
            hints.push(format!("intelligence {}", compact(v)));
        }
        if let Some(v) = stats
            .get("output_price_per_1m_tokens")
            .and_then(Value::as_f64)
        {
            hints.push(format!("output ${}/1M", compact(v)));
        }
        if let Some(v) = stats
            .get("median_output_tokens_per_second")
            .and_then(Value::as_f64)
        {
            hints.push(format!("{} tok/s", compact(v)));
        }
        if !hints.is_empty() {
            text.push_str(&format!(" ({})", hints.join(", ")));
        }
    }
    if text.chars().count() > MAX_CRITERIA_CHARS {
        text = text.chars().take(MAX_CRITERIA_CHARS).collect();
    }
    text
}

/// Chat asks the UI question too; other types get neutral pick instructions only.
fn jev_payload(
    jev_model: &str,
    input: &str,
    criteria: &serde_json::Map<String, Value>,
    route_type: RouteType,
) -> Value {
    if route_type != RouteType::Chat {
        return json!({
            "model": jev_model,
            "state": input,
            "questions": {
                "pick": {"type": "choice", "instructions": PICK_INSTRUCTIONS, "criteria": criteria},
            },
        });
    }
    json!({
        "model": jev_model,
        "state": input,
        "questions": {
            "pick": {
                "type": "choice",
                "instructions": INSTRUCTIONS,
                "criteria": criteria,
            },
            "is_ui_or_visual": {
                "type": "choice",
                "instructions": UI_INSTRUCTIONS,
                "criteria": {"true": UI_TRUE, "false": UI_FALSE},
            },
        },
    })
}

/// Listed entries whose `type` equals the requested one; missing types match nothing.
fn entries_of_type(list: &Value, route_type: RouteType) -> Vec<&Value> {
    list.get("data")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter(|e| e.get("type").and_then(Value::as_str) == Some(route_type.as_str()))
                .collect()
        })
        .unwrap_or_default()
}

fn criteria_map(entries: &[&Value]) -> serde_json::Map<String, Value> {
    entries
        .iter()
        .filter_map(|entry| {
            let id = entry.get("id").and_then(Value::as_str)?;
            Some((id.to_string(), Value::String(criteria_text(entry))))
        })
        .collect()
}

async fn call_jev(base_url: &str, key: &str, payload: &Value) -> Result<JevResponse, ApiError> {
    let url = format!("{}/v1/systemone", base_url.trim_end_matches('/'));
    let response = client()
        .post(url)
        .bearer_auth(key)
        .json(payload)
        .send()
        .await
        .map_err(|_| {
            tracing::warn!("model router request failed");
            upstream_error()
        })?;
    if !response.status().is_success() {
        tracing::warn!(
            status = response.status().as_u16(),
            "model router upstream error"
        );
        return Err(upstream_error());
    }
    read_capped_json(response).await
}

/// Top-5 listed ids by probability, plus the confidence in the pick.
fn rank_pick(
    pick: &JevPick,
    criteria: &serde_json::Map<String, Value>,
) -> Result<(Vec<(String, f64)>, f64), ApiError> {
    let mut ranking: Vec<(String, f64)> = pick
        .probabilities
        .iter()
        .filter(|(id, _)| criteria.contains_key(id.as_str()))
        .filter_map(|(id, p)| valid_probability(p).map(|p| (id.clone(), p)))
        .collect();
    // Confidence comes from the full distribution, before top-5 truncation.
    let chosen_probability = ranking
        .iter()
        .find(|(id, _)| *id == pick.choice)
        .map(|(_, p)| *p);
    let confidence = match (
        pick.confidence.filter(|c| c.is_finite()),
        chosen_probability,
    ) {
        (Some(c), _) => c.clamp(0.0, 1.0),
        (None, Some(p)) => p,
        (None, None) => return Err(upstream_error()),
    };
    if chosen_probability.is_none() && !pick.probabilities.is_empty() {
        return Err(upstream_error());
    }
    ranking.sort_by(|a, b| b.1.total_cmp(&a.1));
    ranking.truncate(5);
    Ok((ranking, confidence))
}

fn ui_confident(ui: Option<&JevPick>) -> bool {
    ui.is_some_and(|ui| {
        ui.choice == "true"
            && ui
                .probabilities
                .get("true")
                .and_then(valid_probability)
                .or(ui.confidence.filter(|c| c.is_finite()))
                .is_some_and(|p| p >= 0.5)
    })
}

fn router_key(state: &AppState) -> Result<String, ApiError> {
    let key = state.config.typesafe_api_key.0.trim();
    if key.is_empty() {
        return Err(anthropic_error(
            StatusCode::SERVICE_UNAVAILABLE,
            "api_error",
            "model router is not configured: set TYPESAFE_API_KEY",
        ));
    }
    Ok(key.to_string())
}

fn body_error(rejection: axum::extract::rejection::BytesRejection) -> ApiError {
    if rejection.status() == StatusCode::PAYLOAD_TOO_LARGE {
        anthropic_error(
            StatusCode::PAYLOAD_TOO_LARGE,
            "request_too_large",
            "request body too large",
        )
    } else {
        bad_request("could not read request body")
    }
}

/// UI rule (user-specified, chat only): a confident UI/visual answer forces the latest Opus.
fn ui_rule_target<'a>(
    entries: &[&'a Value],
    route_type: RouteType,
    ui: Option<&JevPick>,
) -> Option<&'a Value> {
    if route_type != RouteType::Chat || !ui_confident(ui) {
        return None;
    }
    let id_of = |e: &&'a Value| e.get("id").and_then(Value::as_str);
    let opus = crate::http::health::latest_opus_id(entries.iter().filter_map(id_of))?;
    entries.iter().copied().find(|e| id_of(e) == Some(opus))
}

pub(crate) async fn route_model(
    State(state): State<AppState>,
    body: Result<Bytes, axum::extract::rejection::BytesRejection>,
) -> Result<Json<Value>, ApiError> {
    let body = body.map_err(body_error)?;
    let (input, route_type) = parse_request(&body)?;
    let key = router_key(&state)?;

    let list = build_models_value(&state).await;
    let entries = entries_of_type(&list, route_type);
    let criteria = criteria_map(&entries);
    if criteria.is_empty() {
        return Err(anthropic_error(
            StatusCode::SERVICE_UNAVAILABLE,
            "api_error",
            "no models available to route between",
        ));
    }

    let payload = jev_payload(&state.config.jev_model, &input, &criteria, route_type);
    let parsed = call_jev(&state.config.jev_base_url, &key, &payload).await?;
    let reply = recommend(
        &entries,
        &criteria,
        route_type,
        parsed.answers,
        &state.config.jev_model,
    )?;
    Ok(Json(reply))
}

/// Pure decision step: validates Jev's answers against the listed entries.
fn recommend(
    entries: &[&Value],
    criteria: &serde_json::Map<String, Value>,
    route_type: RouteType,
    answers: JevAnswers,
    jev_model: &str,
) -> Result<Value, ApiError> {
    let pick = answers.pick;
    let chosen = entries
        .iter()
        .find(|e| e.get("id").and_then(Value::as_str) == Some(pick.choice.as_str()))
        .ok_or_else(|| {
            tracing::warn!("model router returned an unlisted choice");
            upstream_error()
        })?;

    let (ranking, confidence) = rank_pick(&pick, criteria)?;

    let ui = answers.is_ui_or_visual;
    let (final_entry, routed_by) = match ui_rule_target(entries, route_type, ui.as_ref()) {
        Some(entry) => (entry, "ui-rule"),
        None => (*chosen, "jev"),
    };

    Ok(json!({
        "routed_by": routed_by,
        "recommended": {
            "id": final_entry.get("id").cloned().unwrap_or(Value::Null),
            "description": final_entry.get("description").cloned().unwrap_or(Value::Null),
            "excels_at": final_entry.get("excels_at").cloned().unwrap_or(json!([])),
        },
        "confidence": confidence,
        "ranking": ranking.iter().map(|(id, p)| json!({"id": id, "probability": p})).collect::<Vec<_>>(),
        "jev_model": jev_model,
        "type": route_type.as_str(),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn input_validation() {
        assert!(parse_request(br#"{"input":"hi"}"#).is_ok());
        // Escapes inflate the raw size, not the decoded text.
        let escaped = format!(r#"{{"input":"{}"}}"#, "\\u0061".repeat(3000));
        assert!(escaped.len() > 16 * 1024 && parse_request(escaped.as_bytes()).is_ok());
        assert!(parse_request(br#"{"input":"  "}"#).is_err());
        assert!(parse_request(b"nope").is_err());
        assert!(parse_request(br#"{"nope":1}"#).is_err());
        let big = format!(r#"{{"input":"{}"}}"#, "a".repeat(MAX_INPUT_BYTES + 1));
        assert!(parse_request(big.as_bytes()).is_err());
    }

    #[test]
    fn criteria_is_neutral_without_description_and_compact_with_stats() {
        assert_eq!(criteria_text(&json!({"id": "x"})), NEUTRAL_TEXT);
        let t = criteria_text(&json!({"description": "D", "excels_at": ["a", "b"],
            "stats": {"intelligence_index": 57.6, "output_price_per_1m_tokens": 20.0}}));
        assert_eq!(t, "D Strengths: a, b. (intelligence 57.6, output $20/1M)");
    }

    #[test]
    fn criteria_truncates_on_char_boundary() {
        let t = criteria_text(&json!({"description": "é".repeat(500)}));
        assert_eq!(t.chars().count(), MAX_CRITERIA_CHARS);
        assert!(t.is_char_boundary(t.len()));
    }

    #[test]
    fn probability_validation() {
        assert_eq!(valid_probability(&json!(0.5)), Some(0.5));
        for bad in [json!(1.5), json!(-0.1), json!("x"), json!(null)] {
            assert!(valid_probability(&bad).is_none());
        }
    }
}
