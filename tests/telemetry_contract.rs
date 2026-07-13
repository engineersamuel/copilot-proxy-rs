use copilot_proxy_rs::telemetry::{
    ApiFamily, CacheOperation, summarize_cache, summarize_effective_request, summarize_request,
    summarize_request_sizes, summarize_usage,
};

#[test]
fn request_summary_extracts_safe_chat_metadata_without_content() {
    let body = serde_json::json!({
        "model": "gpt-5.5",
        "stream": true,
        "reasoning_effort": "high",
        "max_completion_tokens": 123,
        "messages": [
            {"role": "system", "content": "secret system prompt"},
            {"role": "user", "content": "private prompt text"},
            {"role": "tool", "content": "private tool result"}
        ],
        "tools": [
            {"type": "function", "function": {"name": "secret_tool", "parameters": {"type": "object"}}}
        ]
    });

    let summary = summarize_request(ApiFamily::ChatCompletions, body.as_object().unwrap());

    assert_eq!(summary.api_family, ApiFamily::ChatCompletions);
    assert_eq!(summary.requested_model.as_deref(), Some("gpt-5.5"));
    assert_eq!(summary.effective_model, None);
    assert!(summary.stream);
    assert_eq!(summary.message_count, 3);
    assert_eq!(summary.tool_definition_count, 1);
    assert_eq!(summary.tool_result_count, 1);
    assert_eq!(summary.max_tokens, Some(123));
    assert_eq!(summary.effort.as_deref(), Some("high"));
    let debug = format!("{summary:?}");
    assert!(!debug.contains("private prompt text"));
    assert!(!debug.contains("secret system prompt"));
    assert!(!debug.contains("secret_tool"));
    assert!(!debug.contains("private tool result"));
}

#[test]
fn effective_request_summary_keeps_requested_and_effective_models() {
    let body = serde_json::json!({
        "model": "gpt-5.5-copilot",
        "messages": [{"role":"user","content":"hello"}]
    });

    let summary = summarize_effective_request(
        ApiFamily::ChatCompletions,
        Some("gpt-5.5"),
        body.as_object().unwrap(),
    );

    assert_eq!(summary.requested_model.as_deref(), Some("gpt-5.5"));
    assert_eq!(summary.effective_model.as_deref(), Some("gpt-5.5-copilot"));
}

#[test]
fn usage_summary_reads_openai_responses_and_anthropic_cache_fields() {
    let openai = serde_json::json!({
        "usage": {
            "input_tokens": 10,
            "output_tokens": 4,
            "total_tokens": 14,
            "input_tokens_details": {"cached_tokens": 6}
        }
    });
    let usage = summarize_usage(&openai);
    assert_eq!(usage.input_tokens, Some(10));
    assert_eq!(usage.output_tokens, Some(4));
    assert_eq!(usage.cached_tokens, Some(6));
    assert_eq!(usage.total_tokens, Some(14));

    let anthropic = serde_json::json!({
        "usage": {
            "input_tokens": 7,
            "output_tokens": 3,
            "cache_read_input_tokens": 2,
            "cache_creation_input_tokens": 1
        }
    });
    let usage = summarize_usage(&anthropic);
    assert_eq!(usage.input_tokens, Some(7));
    assert_eq!(usage.output_tokens, Some(3));
    assert_eq!(usage.cached_tokens, Some(3));
    assert_eq!(usage.total_tokens, Some(10));
}

#[test]
fn responses_summary_counts_input_items_and_reasoning_effort() {
    let body = serde_json::json!({
        "model": "gpt-5.5",
        "input": [
            {"role": "user", "content": [{"type": "input_text", "text": "private"}]},
            {"type": "function_call", "arguments": "{\"secret\":true}"}
        ],
        "reasoning": {"effort": "medium"},
        "tools": [{"type": "function", "name": "hidden"}]
    });

    let summary = summarize_request(ApiFamily::Responses, body.as_object().unwrap());

    assert_eq!(summary.input_item_count, 2);
    assert_eq!(summary.tool_definition_count, 1);
    assert_eq!(summary.tool_result_count, 1);
    assert_eq!(summary.effort.as_deref(), Some("medium"));
    let debug = format!("{summary:?}");
    assert!(!debug.contains("private"));
    assert!(!debug.contains("secret"));
    assert!(!debug.contains("hidden"));
}

#[test]
fn request_size_summary_reports_structure_without_content() {
    let body = serde_json::json!({
        "model": "gpt-5.5",
        "input": [
            {"type": "reasoning", "encrypted_content": "private reasoning"},
            {"type": "function_call_output", "call_id": "secret-call", "output": "private output"},
            {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "private prompt"}]}
        ],
        "tools": [{"type": "function", "name": "private_tool"}]
    });

    let summary = summarize_request_sizes(body.as_object().unwrap());

    assert!(summary.body_bytes >= summary.input_bytes);
    assert!(summary.input_tool_bytes > 0);
    assert!(summary.input_reasoning_bytes > 0);
    assert!(summary.tools_bytes > 0);
    assert!(summary.largest_input_item_bytes > 0);
    assert!(summary.largest_input_item_index.is_some());
    let debug = format!("{summary:?}");
    assert!(!debug.contains("private reasoning"));
    assert!(!debug.contains("private output"));
    assert!(!debug.contains("private prompt"));
    assert!(!debug.contains("private_tool"));
    assert!(!debug.contains("secret-call"));
}

#[test]
fn request_size_summary_allowlists_logged_item_type_and_role() {
    let body = serde_json::json!({
        "input": [{
            "type": "private type value",
            "role": "private role value",
            "content": "x".repeat(100)
        }]
    });

    let summary = summarize_request_sizes(body.as_object().unwrap());

    assert_eq!(summary.largest_input_item_type, Some("unknown"));
    assert_eq!(summary.largest_input_item_role, Some("unknown"));
    let debug = format!("{summary:?}");
    assert!(!debug.contains("private type value"));
    assert!(!debug.contains("private role value"));
}

#[test]
fn cache_summary_is_metadata_only() {
    let summary = summarize_cache(CacheOperation::Write, Some(5), Some(true));

    assert_eq!(summary.operation, CacheOperation::Write);
    assert_eq!(summary.transcript_items, Some(5));
    assert_eq!(summary.last_response_had_tool_calls, Some(true));
}

use std::sync::{Arc, Mutex};
use tracing_subscriber::layer::SubscriberExt;

#[derive(Debug, Default, Clone)]
struct CapturedEvent {
    message: String,
    fields: Vec<(String, String)>,
}

struct EventCaptureLayer {
    events: Arc<Mutex<Vec<CapturedEvent>>>,
}

#[derive(Default)]
struct EventVisitor {
    event: CapturedEvent,
}

impl tracing::field::Visit for EventVisitor {
    fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
        if field.name() == "message" {
            self.event.message = value.to_string();
        } else {
            self.event
                .fields
                .push((field.name().to_string(), value.to_string()));
        }
    }

    fn record_u64(&mut self, field: &tracing::field::Field, value: u64) {
        self.event
            .fields
            .push((field.name().to_string(), value.to_string()));
    }

    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
        if field.name() == "message" {
            self.event.message = format!("{value:?}");
        } else {
            self.event
                .fields
                .push((field.name().to_string(), format!("{value:?}")));
        }
    }
}

impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for EventCaptureLayer {
    fn on_event(
        &self,
        event: &tracing::Event<'_>,
        _ctx: tracing_subscriber::layer::Context<'_, S>,
    ) {
        let mut visitor = EventVisitor::default();
        event.record(&mut visitor);
        self.events.lock().unwrap().push(visitor.event);
    }
}

fn with_event_capture<F: FnOnce()>(f: F) -> Vec<CapturedEvent> {
    let events = Arc::new(Mutex::new(Vec::new()));
    let subscriber = tracing_subscriber::registry().with(EventCaptureLayer {
        events: events.clone(),
    });
    tracing::subscriber::with_default(subscriber, f);
    Arc::try_unwrap(events).unwrap().into_inner().unwrap()
}

fn field(events: &[CapturedEvent], message: &str, name: &str) -> Option<String> {
    events
        .iter()
        .find(|event| event.message == message)
        .and_then(|event| {
            event
                .fields
                .iter()
                .find(|(field, _)| field == name)
                .map(|(_, value)| value.clone())
        })
}

#[test]
fn api_family_names_are_stable_for_log_fields() {
    use copilot_proxy_rs::telemetry::api_family_name;

    assert_eq!(
        api_family_name(ApiFamily::ChatCompletions),
        "chat_completions"
    );
    assert_eq!(api_family_name(ApiFamily::Messages), "messages");
    assert_eq!(api_family_name(ApiFamily::Responses), "responses");
    assert_eq!(
        api_family_name(ApiFamily::ResponsesWebSocket),
        "responses_ws"
    );
    assert_eq!(api_family_name(ApiFamily::Models), "models");
    assert_eq!(api_family_name(ApiFamily::Other("health")), "health");
}

#[test]
fn startup_log_emits_service_backend_and_bind_metadata() {
    let events = with_event_capture(|| {
        copilot_proxy_rs::telemetry::log_startup(
            "http://127.0.0.1:4141",
            "0.1.0",
            "copilot",
            Some("bedrock"),
            1234,
        );
    });

    assert_eq!(
        field(&events, "proxy listening", "service.name").as_deref(),
        Some("copilot-proxy-rs")
    );
    assert_eq!(
        field(&events, "proxy listening", "backend.primary").as_deref(),
        Some("copilot")
    );
    assert_eq!(
        field(&events, "proxy listening", "backend.fallback").as_deref(),
        Some("bedrock")
    );
    assert_eq!(
        field(&events, "proxy listening", "bind.address").as_deref(),
        Some("http://127.0.0.1:4141")
    );
    assert_eq!(
        field(&events, "proxy listening", "process.pid").as_deref(),
        Some("1234")
    );
}
