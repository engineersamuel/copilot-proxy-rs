use http::HeaderMap;
use serde_json::{Map, Value};
use uuid::Uuid;

use crate::copilot::request::{
    CopilotRequestMetadata, adapt_responses_reasoning_effort, adapt_responses_tools_for_copilot,
    compute_initiator,
};
use crate::models::SupportedEfforts;
use crate::responses::state::{ResponsesStateStore, ResponsesTurnIdentity};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PreviousResponseCacheStatus {
    NotRequested,
    Hit,
    Miss,
}

#[derive(Debug, Clone)]
pub struct PreparedResponsesRequest {
    pub effective_body: Map<String, Value>,
    pub request_metadata: CopilotRequestMetadata,
    pub identity: ResponsesTurnIdentity,
    pub cache_status: PreviousResponseCacheStatus,
}

#[derive(Debug, Clone)]
pub struct ExpandedResponsesInput {
    pub body: Map<String, Value>,
    pub previous_identity: Option<ResponsesTurnIdentity>,
    pub cache_status: PreviousResponseCacheStatus,
}

pub async fn expand_previous_response(
    store: &ResponsesStateStore,
    mut body: Map<String, Value>,
) -> ExpandedResponsesInput {
    let mut cache_status = PreviousResponseCacheStatus::NotRequested;
    let mut previous_identity = None;
    if let Some(previous) = body
        .get("previous_response_id")
        .and_then(Value::as_str)
        .map(str::to_string)
    {
        cache_status = PreviousResponseCacheStatus::Miss;
        if let Some(entry) = store.get_cached_response_state(&previous).await {
            cache_status = PreviousResponseCacheStatus::Hit;
            previous_identity = Some(entry.identity);
            if let Some(input) = normalize_input_items(body.get("input")) {
                let mut expanded = entry.transcript;
                expanded.extend(input);
                body.insert("input".to_string(), Value::Array(expanded));
                body.remove("previous_response_id");
            }
        }
    }
    ExpandedResponsesInput {
        body,
        previous_identity,
        cache_status,
    }
}

pub async fn prepare_responses_request(
    store: &ResponsesStateStore,
    body: Map<String, Value>,
    request_id: String,
    headers: &HeaderMap,
    copilot_model: String,
    supported_efforts: Option<&SupportedEfforts>,
) -> PreparedResponsesRequest {
    let expanded = expand_previous_response(store, body).await;
    let mut effective_body = expanded.body;
    let previous_identity = expanded.previous_identity;
    let cache_status = expanded.cache_status;
    effective_body.insert("model".to_string(), Value::String(copilot_model));
    adapt_responses_reasoning_effort(&mut effective_body, supported_efforts);
    adapt_responses_tools_for_copilot(&mut effective_body);
    let identity =
        prepare_responses_turn_identity(&mut effective_body, headers, previous_identity.as_ref());
    let initiator = compute_initiator(&effective_body, true).to_string();
    let request_metadata = CopilotRequestMetadata {
        request_id: Some(request_id),
        initiator: Some(initiator),
        openai_intent: Some("conversation-agent".to_string()),
        interaction_id: Some(identity.interaction_id.clone()),
        interaction_type: Some("conversation-agent".to_string()),
        agent_task_id: Some(identity.agent_task_id.clone()),
        extra_headers: Default::default(),
    };
    PreparedResponsesRequest {
        effective_body,
        request_metadata,
        identity,
        cache_status,
    }
}

pub fn prepare_responses_turn_identity(
    effective_body: &mut Map<String, Value>,
    headers: &HeaderMap,
    previous_identity: Option<&ResponsesTurnIdentity>,
) -> ResponsesTurnIdentity {
    let incoming_interaction_id = header_value(headers, "x-interaction-id")
        .or_else(|| header_value(headers, "x-client-request-id"));
    let prompt_cache_identity = incoming_interaction_id
        .as_deref()
        .or_else(|| previous_identity.map(|identity| identity.interaction_id.as_str()));
    if !effective_body.contains_key("prompt_cache_key") {
        if let Some(cache_identity) = prompt_cache_identity {
            let model = effective_body
                .get("model")
                .and_then(Value::as_str)
                .unwrap_or_default();
            effective_body.insert(
                "prompt_cache_key".to_string(),
                Value::String(format!("{cache_identity}:{model}")),
            );
        }
    }
    let interaction_id = incoming_interaction_id
        .or_else(|| previous_identity.map(|identity| identity.interaction_id.clone()))
        .unwrap_or_else(|| Uuid::new_v4().to_string());
    ResponsesTurnIdentity {
        interaction_id: interaction_id.clone(),
        agent_task_id: header_value(headers, "x-agent-task-id")
            .or_else(|| previous_identity.map(|identity| identity.agent_task_id.clone()))
            .unwrap_or_else(|| interaction_id.clone()),
    }
}

pub fn normalize_input_items(value: Option<&Value>) -> Option<Vec<Value>> {
    match value {
        Some(Value::Array(items)) => Some(items.clone()),
        Some(Value::String(text)) => Some(vec![serde_json::json!({
            "role": "user",
            "content": [{"type": "input_text", "text": text}]
        })]),
        _ => None,
    }
}

fn header_value(headers: &HeaderMap, key: &str) -> Option<String> {
    headers
        .get(key)
        .and_then(|value| value.to_str().ok())
        .filter(|value| !value.is_empty())
        .map(str::to_string)
}

#[cfg(test)]
mod tests {
    use http::HeaderMap;
    use serde_json::{Value, json};

    use super::{
        PreviousResponseCacheStatus, expand_previous_response, normalize_input_items,
        prepare_responses_request,
    };
    use crate::responses::state::{ResponsesStateStore, ResponsesTurnIdentity};

    fn parse_body(s: &str) -> serde_json::Map<String, Value> {
        serde_json::from_str(s).unwrap()
    }

    #[tokio::test]
    async fn previous_response_id_expands_transcript_and_is_stripped() {
        let store = ResponsesStateStore::default();

        let prior_input = vec![json!({
            "role": "user",
            "content": [{"type": "input_text", "text": "first message"}]
        })];
        let prior_output = vec![json!({
            "type": "message",
            "role": "assistant",
            "content": [{"type": "output_text", "text": "first reply"}]
        })];
        let identity = ResponsesTurnIdentity {
            interaction_id: "iid-1".to_string(),
            agent_task_id: "atid-1".to_string(),
        };
        store
            .cache_response_state("resp_abc", prior_input, prior_output, identity, false)
            .await;

        let body = parse_body(
            r#"{"model":"gpt-5.5","input":"follow-up","previous_response_id":"resp_abc"}"#,
        );
        let result = prepare_responses_request(
            &store,
            body,
            "req-1".to_string(),
            &HeaderMap::new(),
            "gpt-5.5".to_string(),
            None,
        )
        .await;

        // previous_response_id must be stripped before forwarding upstream
        assert!(
            !result.effective_body.contains_key("previous_response_id"),
            "previous_response_id should be stripped"
        );

        // input must be the expanded transcript: prior input + prior output + normalized new turn
        let input = result
            .effective_body
            .get("input")
            .and_then(Value::as_array)
            .expect("input should be an array after expansion");
        assert_eq!(
            input.len(),
            3,
            "expected 1 prior input + 1 prior output + 1 new message, got {input:?}"
        );

        // New message should be normalized from the string "follow-up"
        let new_msg = &input[2];
        assert_eq!(new_msg["role"], "user");
        assert_eq!(new_msg["content"][0]["type"], "input_text");
        assert_eq!(new_msg["content"][0]["text"], "follow-up");
        assert_eq!(result.cache_status, PreviousResponseCacheStatus::Hit);
    }

    #[tokio::test]
    async fn unknown_previous_response_id_leaves_body_unchanged() {
        let store = ResponsesStateStore::default();
        let body = parse_body(
            r#"{"model":"gpt-5.5","input":"hello","previous_response_id":"nonexistent"}"#,
        );
        let result = prepare_responses_request(
            &store,
            body,
            "req-2".to_string(),
            &HeaderMap::new(),
            "gpt-5.5".to_string(),
            None,
        )
        .await;

        // When the id is not cached, the body is passed through unmodified
        assert_eq!(
            result.effective_body["previous_response_id"],
            json!("nonexistent")
        );
        assert_eq!(result.effective_body["input"], json!("hello"));
        assert_eq!(result.cache_status, PreviousResponseCacheStatus::Miss);
    }

    #[test]
    fn normalize_string_input_produces_user_message() {
        let items = normalize_input_items(Some(&json!("hello world"))).unwrap();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0]["role"], "user");
        assert_eq!(items[0]["content"][0]["type"], "input_text");
        assert_eq!(items[0]["content"][0]["text"], "hello world");
    }

    #[test]
    fn normalize_array_input_passes_through() {
        let arr = json!([{"role": "user", "content": "hi"}]);
        let items = normalize_input_items(Some(&arr)).unwrap();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0]["role"], "user");
    }

    #[test]
    fn normalize_missing_input_returns_none() {
        assert!(normalize_input_items(None).is_none());
    }

    #[tokio::test]
    async fn expand_previous_response_reports_not_requested_without_an_id() {
        let body = parse_body(r#"{"model":"local","input":"hello"}"#);

        let expanded =
            expand_previous_response(&ResponsesStateStore::default(), body.clone()).await;

        assert_eq!(expanded.body, body);
        assert_eq!(expanded.previous_identity, None);
        assert_eq!(
            expanded.cache_status,
            PreviousResponseCacheStatus::NotRequested
        );
    }

    #[tokio::test]
    async fn expand_previous_response_reports_miss_and_retains_the_id() {
        let body =
            parse_body(r#"{"model":"local","input":"hello","previous_response_id":"missing"}"#);

        let expanded =
            expand_previous_response(&ResponsesStateStore::default(), body.clone()).await;

        assert_eq!(expanded.body, body);
        assert_eq!(expanded.previous_identity, None);
        assert_eq!(expanded.cache_status, PreviousResponseCacheStatus::Miss);
    }

    #[tokio::test]
    async fn expand_previous_response_reports_hit_and_expands_the_transcript() {
        let store = ResponsesStateStore::default();
        let identity = ResponsesTurnIdentity {
            interaction_id: "iid-expanded".to_string(),
            agent_task_id: "atid-expanded".to_string(),
        };
        store
            .cache_response_state(
                "resp_expand",
                vec![json!({"role": "user", "content": "first"})],
                vec![json!({"role": "assistant", "content": "reply"})],
                identity.clone(),
                false,
            )
            .await;
        let body = parse_body(
            r#"{"model":"local","input":"follow-up","previous_response_id":"resp_expand"}"#,
        );

        let expanded = expand_previous_response(&store, body).await;

        assert_eq!(expanded.previous_identity, Some(identity));
        assert_eq!(expanded.cache_status, PreviousResponseCacheStatus::Hit);
        assert!(!expanded.body.contains_key("previous_response_id"));
        assert_eq!(
            expanded.body["input"],
            json!([
                {"role": "user", "content": "first"},
                {"role": "assistant", "content": "reply"},
                {
                    "role": "user",
                    "content": [{"type": "input_text", "text": "follow-up"}]
                }
            ])
        );
    }

    #[tokio::test]
    async fn expand_previous_response_preserves_malformed_current_input_on_cache_hit() {
        let store = ResponsesStateStore::default();
        let identity = ResponsesTurnIdentity {
            interaction_id: "iid-malformed".to_string(),
            agent_task_id: "atid-malformed".to_string(),
        };
        store
            .cache_response_state(
                "resp_malformed",
                vec![json!({"role": "user", "content": "old"})],
                vec![json!({"role": "assistant", "content": "old reply"})],
                identity.clone(),
                false,
            )
            .await;
        let body = parse_body(
            r#"{"model":"local","input":{"unexpected":true},"previous_response_id":"resp_malformed"}"#,
        );

        let expanded = expand_previous_response(&store, body.clone()).await;

        assert_eq!(expanded.body, body);
        assert_eq!(expanded.previous_identity, Some(identity));
        assert_eq!(expanded.cache_status, PreviousResponseCacheStatus::Hit);
    }

    #[tokio::test]
    async fn expand_previous_response_preserves_missing_current_input_on_cache_hit() {
        let store = ResponsesStateStore::default();
        let identity = ResponsesTurnIdentity {
            interaction_id: "iid-missing".to_string(),
            agent_task_id: "atid-missing".to_string(),
        };
        store
            .cache_response_state(
                "resp_missing_input",
                vec![json!({"role": "user", "content": "old"})],
                vec![json!({"role": "assistant", "content": "old reply"})],
                identity.clone(),
                false,
            )
            .await;
        let body = parse_body(r#"{"model":"local","previous_response_id":"resp_missing_input"}"#);

        let expanded = expand_previous_response(&store, body.clone()).await;

        assert_eq!(expanded.body, body);
        assert_eq!(expanded.previous_identity, Some(identity));
        assert_eq!(expanded.cache_status, PreviousResponseCacheStatus::Hit);
    }
}
