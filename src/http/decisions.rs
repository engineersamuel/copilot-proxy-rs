//! Serves non-chat Copilot models (decisions, SystemOne, image) requested
//! through the conversational routes.

use std::collections::BTreeMap;

use axum::Json;
use axum::body::Body;
use axum::response::{IntoResponse, Response};
use http::StatusCode;
use serde_json::{Map, Value};

use crate::copilot::errors::CopilotError;
use crate::errors::{anthropic_error, openai_error};
use crate::http::errors::{anthropic_copilot_error, openai_copilot_error};
use crate::http::responses::completed_response_events;
use crate::models::CopilotModelKind;
use crate::responses::request::{
    PreviousResponseCacheStatus, expand_previous_response, normalize_input_items,
};
use crate::responses::state::ResponsesTurnIdentity;
use crate::state::AppState;
use crate::translate::decisions::{
    DecisionTurn, DecisionsEndpoint, build_upstream_request, decision_to_responses,
    turns_from_anthropic, turns_from_openai_chat, turns_from_responses,
};
use crate::translate::responses_formats::{
    ResponsesToChatStream, responses_sse_to_anthropic_sse_line,
    responses_to_anthropic_message_response, responses_to_openai_chat_response,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ClientApi {
    Chat,
    Messages,
    Responses,
}

impl ClientApi {
    fn route(self) -> &'static str {
        match self {
            Self::Chat => "/v1/chat/completions",
            Self::Messages => "/v1/messages",
            Self::Responses => "/v1/responses",
        }
    }

    fn error(self, status: StatusCode, openai_type: &str, message: String) -> Response {
        match self {
            Self::Messages => {
                let anthropic_type = match openai_type {
                    "server_error" => "api_error",
                    other => other,
                };
                anthropic_error(status, anthropic_type, message).into_response()
            }
            Self::Chat | Self::Responses => {
                openai_error(status, openai_type, message).into_response()
            }
        }
    }

    fn turns(self, body: &Map<String, Value>) -> Vec<DecisionTurn> {
        match self {
            Self::Chat => turns_from_openai_chat(body),
            Self::Messages => turns_from_anthropic(body),
            Self::Responses => turns_from_responses(body),
        }
    }

    fn copilot_error(self, error: CopilotError) -> Response {
        match self {
            Self::Messages => anthropic_copilot_error(error).into_response(),
            Self::Chat | Self::Responses => openai_copilot_error(error).into_response(),
        }
    }
}

/// Answers a request for a non-chat Copilot model, or returns `None` so the
/// caller continues with its normal chat path.
pub(crate) async fn nonchat_model_response(
    state: &AppState,
    api: ClientApi,
    body: &Map<String, Value>,
    copilot_model: &str,
    public_model: &str,
    stream: bool,
) -> Option<Response> {
    let endpoint = match state.models.copilot_model_kind(copilot_model).await? {
        CopilotModelKind::Decisions => DecisionsEndpoint::Decisions,
        CopilotModelKind::SystemOne => DecisionsEndpoint::SystemOne,
        CopilotModelKind::Image => return Some(image_model_rejection(api, public_model)),
        CopilotModelKind::Chat | CopilotModelKind::Other(_) => return None,
    };
    Some(
        handle_copilot_decisions(
            state,
            api,
            endpoint,
            body,
            copilot_model,
            public_model,
            stream,
        )
        .await,
    )
}

/// Explains why a non-chat model cannot run over the Responses WebSocket.
pub(crate) async fn responses_ws_rejection(
    state: &AppState,
    copilot_model: &str,
    public_model: &str,
) -> Option<String> {
    match state.models.copilot_model_kind(copilot_model).await? {
        CopilotModelKind::Decisions | CopilotModelKind::SystemOne => Some(format!(
            "model '{public_model}' does not support Responses WebSocket; use POST /v1/responses"
        )),
        CopilotModelKind::Image => Some(image_model_message(public_model, "Responses WebSocket")),
        CopilotModelKind::Chat | CopilotModelKind::Other(_) => None,
    }
}

pub(crate) fn image_model_message(public_model: &str, route: &str) -> String {
    format!(
        "model '{public_model}' is an image model and does not support {route}; use POST /v1/images/generations or POST /v1/images/edits"
    )
}

pub(crate) fn image_model_rejection(api: ClientApi, public_model: &str) -> Response {
    api.error(
        StatusCode::BAD_REQUEST,
        "invalid_request_error",
        image_model_message(public_model, api.route()),
    )
}

pub(crate) async fn handle_copilot_decisions(
    state: &AppState,
    api: ClientApi,
    endpoint: DecisionsEndpoint,
    body: &Map<String, Value>,
    copilot_model: &str,
    public_model: &str,
    stream: bool,
) -> Response {
    let (body, previous_identity) = match expand_previous(state, api, body).await {
        Ok(expanded) => expanded,
        Err(response) => return response,
    };
    let Ok(request) = build_upstream_request(endpoint, copilot_model, &api.turns(&body)) else {
        return api.error(
            StatusCode::BAD_REQUEST,
            "invalid_request_error",
            "decisions models require a user message".to_string(),
        );
    };
    tracing::info!(
        api.family = api.route(),
        model.requested = public_model,
        model.effective = copilot_model,
        stream,
        decisions.endpoint = ?endpoint,
        "decisions request prepared"
    );
    let upstream = match post_decision(state, endpoint, request).await {
        Ok(upstream) => upstream,
        Err(error) => return api.copilot_error(error),
    };
    match decision_to_responses(endpoint, &upstream, public_model) {
        Ok(response) => {
            if api == ClientApi::Responses {
                cache_decision_response(state, &body, &response, previous_identity).await;
            }
            if stream {
                sse_response(render_stream(api, &response))
            } else {
                Json(render_json(api, &response, public_model)).into_response()
            }
        }
        Err(_) => api.error(
            StatusCode::BAD_GATEWAY,
            "server_error",
            "Copilot decisions response was malformed".to_string(),
        ),
    }
}

/// Resolves `previous_response_id` for Responses requests; an unknown ID is
/// rejected rather than answered as a standalone turn.
async fn expand_previous(
    state: &AppState,
    api: ClientApi,
    body: &Map<String, Value>,
) -> Result<(Map<String, Value>, Option<ResponsesTurnIdentity>), Response> {
    if api != ClientApi::Responses {
        return Ok((body.clone(), None));
    }
    let expanded = expand_previous_response(&state.responses, body.clone()).await;
    if expanded.cache_status == PreviousResponseCacheStatus::Miss {
        return Err(api.error(
            StatusCode::BAD_REQUEST,
            "invalid_request_error",
            "previous_response_id was not found".to_string(),
        ));
    }
    Ok((expanded.body, expanded.previous_identity))
}

async fn cache_decision_response(
    state: &AppState,
    body: &Map<String, Value>,
    response: &Value,
    previous_identity: Option<ResponsesTurnIdentity>,
) {
    let (Some(id), Some(input), Some(output)) = (
        response.get("id").and_then(Value::as_str),
        normalize_input_items(body.get("input")),
        response.get("output").and_then(Value::as_array).cloned(),
    ) else {
        return;
    };
    let identity = previous_identity.unwrap_or_else(|| {
        let id = uuid::Uuid::new_v4().to_string();
        ResponsesTurnIdentity {
            interaction_id: id.clone(),
            agent_task_id: id,
        }
    });
    state
        .responses
        .cache_response_state(id, input, output, identity, false)
        .await;
}

async fn post_decision(
    state: &AppState,
    endpoint: DecisionsEndpoint,
    request: Map<String, Value>,
) -> Result<Value, CopilotError> {
    match endpoint {
        DecisionsEndpoint::Decisions => state.copilot.post_decisions(request).await,
        DecisionsEndpoint::SystemOne => state.copilot.post_systemone(request).await,
    }
}

fn render_json(api: ClientApi, response: &Value, public_model: &str) -> Value {
    match api {
        ClientApi::Chat => responses_to_openai_chat_response(response, public_model),
        ClientApi::Messages => responses_to_anthropic_message_response(response, public_model),
        ClientApi::Responses => response.clone(),
    }
}

/// Replays a completed Responses object as its full Responses SSE event list.
fn decision_responses_sse_events(response: &Value) -> Vec<String> {
    let mut adapter = crate::local::ChatToResponsesStream::new(
        text_of(response, "id"),
        text_of(response, "model"),
        BTreeMap::new(),
    );
    let mut events = adapter
        .begin_events_with_usage(response.get("usage").cloned().unwrap_or(Value::Null))
        .into_iter()
        .map(|event| format!("{event}\n\n"))
        .collect::<Vec<_>>();
    events.extend(completed_response_events(response));
    events
}

fn render_stream(api: ClientApi, response: &Value) -> String {
    let events = decision_responses_sse_events(response).concat();
    match api {
        ClientApi::Responses => events + "data: [DONE]\n\n",
        ClientApi::Chat => {
            let mut adapter = ResponsesToChatStream::new(text_of(response, "model"));
            events
                .lines()
                .flat_map(|line| adapter.map_line(line).unwrap_or_default())
                .map(|chunk| format!("{chunk}\n\n"))
                .collect()
        }
        ClientApi::Messages => events
            .lines()
            .filter_map(|line| match line {
                "" => Some("\n".to_string()),
                line => responses_sse_to_anthropic_sse_line(line).map(|mapped| mapped + "\n"),
            })
            .collect(),
    }
}

fn text_of(response: &Value, key: &str) -> String {
    response
        .get(key)
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string()
}

fn sse_response(body: String) -> Response {
    (
        [(http::header::CONTENT_TYPE, "text/event-stream")],
        Body::from(body),
    )
        .into_response()
}
