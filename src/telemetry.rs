use serde_json::{Map, Value};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApiFamily {
    ChatCompletions,
    Messages,
    Responses,
    ResponsesWebSocket,
    Models,
    Other(&'static str),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CacheOperation {
    Hit,
    Miss,
    Write,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RequestLogSummary {
    pub api_family: ApiFamily,
    pub requested_model: Option<String>,
    pub effective_model: Option<String>,
    pub stream: bool,
    pub input_tokens_estimate: usize,
    pub message_count: usize,
    pub input_item_count: usize,
    pub tool_definition_count: usize,
    pub tool_result_count: usize,
    pub max_tokens: Option<u64>,
    pub effort: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UsageLogSummary {
    pub input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
    pub cached_tokens: Option<u64>,
    pub total_tokens: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CacheLogSummary {
    pub operation: CacheOperation,
    pub transcript_items: Option<usize>,
    pub last_response_had_tool_calls: Option<bool>,
}

pub fn api_family_name(api_family: ApiFamily) -> &'static str {
    match api_family {
        ApiFamily::ChatCompletions => "chat_completions",
        ApiFamily::Messages => "messages",
        ApiFamily::Responses => "responses",
        ApiFamily::ResponsesWebSocket => "responses_ws",
        ApiFamily::Models => "models",
        ApiFamily::Other(name) => name,
    }
}

pub fn summarize_request(api_family: ApiFamily, body: &Map<String, Value>) -> RequestLogSummary {
    summarize_effective_request(api_family, None, body)
}

pub fn summarize_effective_request(
    api_family: ApiFamily,
    requested_model: Option<&str>,
    body: &Map<String, Value>,
) -> RequestLogSummary {
    let body_model = body
        .get("model")
        .and_then(Value::as_str)
        .map(str::to_string);
    RequestLogSummary {
        api_family,
        requested_model: requested_model
            .map(str::to_string)
            .or_else(|| body_model.clone()),
        effective_model: requested_model.and(body_model),
        stream: body.get("stream").and_then(Value::as_bool).unwrap_or(false),
        input_tokens_estimate: estimate_request_tokens(body),
        message_count: body
            .get("messages")
            .and_then(Value::as_array)
            .map_or(0, Vec::len),
        input_item_count: input_item_count(body.get("input")),
        tool_definition_count: body
            .get("tools")
            .and_then(Value::as_array)
            .map_or(0, Vec::len),
        tool_result_count: count_tool_results(body),
        max_tokens: body
            .get("max_completion_tokens")
            .or_else(|| body.get("max_tokens"))
            .and_then(Value::as_u64),
        effort: body
            .get("reasoning_effort")
            .or_else(|| body.get("reasoning").and_then(|v| v.get("effort")))
            .or_else(|| body.get("output_config").and_then(|v| v.get("effort")))
            .and_then(Value::as_str)
            .map(str::to_string),
    }
}

pub fn summarize_usage(value: &Value) -> UsageLogSummary {
    let usage = value.get("usage").unwrap_or(value);
    let input_tokens = usage
        .get("input_tokens")
        .or_else(|| usage.get("prompt_tokens"))
        .and_then(Value::as_u64);
    let output_tokens = usage
        .get("output_tokens")
        .or_else(|| usage.get("completion_tokens"))
        .and_then(Value::as_u64);
    let cached_tokens = usage
        .get("input_tokens_details")
        .and_then(|details| details.get("cached_tokens"))
        .and_then(Value::as_u64)
        .or_else(|| {
            let read = usage
                .get("cache_read_input_tokens")
                .and_then(Value::as_u64)
                .unwrap_or(0);
            let created = usage
                .get("cache_creation_input_tokens")
                .and_then(Value::as_u64)
                .unwrap_or(0);
            (read + created > 0).then_some(read + created)
        });
    let total_tokens = usage
        .get("total_tokens")
        .and_then(Value::as_u64)
        .or_else(|| {
            Some(input_tokens.unwrap_or(0) + output_tokens.unwrap_or(0)).filter(|total| *total > 0)
        });
    UsageLogSummary {
        input_tokens,
        output_tokens,
        cached_tokens,
        total_tokens,
    }
}

pub fn summarize_cache(
    operation: CacheOperation,
    transcript_items: Option<usize>,
    last_response_had_tool_calls: Option<bool>,
) -> CacheLogSummary {
    CacheLogSummary {
        operation,
        transcript_items,
        last_response_had_tool_calls,
    }
}

pub fn estimate_request_tokens(body: &Map<String, Value>) -> usize {
    let mut total = 0;
    if let Some(system) = body.get("system") {
        total += count_content_tokens(system);
    }
    if let Some(messages) = body.get("messages").and_then(Value::as_array) {
        for message in messages {
            if let Some(content) = message.get("content") {
                total += count_content_tokens(content);
            }
        }
    }
    if let Some(input) = body.get("input") {
        total += count_content_tokens(input);
    }
    if let Some(tools) = body.get("tools") {
        total += tools.to_string().split_whitespace().count();
    }
    total
}

fn input_item_count(value: Option<&Value>) -> usize {
    match value {
        Some(Value::Array(items)) => items.len(),
        Some(Value::String(_)) => 1,
        _ => 0,
    }
}

fn count_tool_results(body: &Map<String, Value>) -> usize {
    let messages_count = body
        .get("messages")
        .and_then(Value::as_array)
        .map(|messages| messages.iter().map(count_message_tool_results).sum())
        .unwrap_or(0);
    let input_count = body
        .get("input")
        .and_then(Value::as_array)
        .map(|items| items.iter().filter(|item| is_tool_item(item)).count())
        .unwrap_or(0);
    messages_count + input_count
}

fn count_message_tool_results(message: &Value) -> usize {
    if message.get("role").and_then(Value::as_str) == Some("tool") {
        return 1;
    }
    match message.get("content") {
        Some(Value::Array(items)) => items.iter().filter(|item| is_tool_item(item)).count(),
        Some(content) if is_tool_item(content) => 1,
        _ => 0,
    }
}

fn is_tool_item(value: &Value) -> bool {
    value
        .get("type")
        .and_then(Value::as_str)
        .is_some_and(|kind| {
            matches!(
                kind,
                "tool_result"
                    | "tool_use"
                    | "function_call"
                    | "custom_tool_call"
                    | "function_call_output"
                    | "web_search_tool_result"
                    | "web_fetch_tool_result"
                    | "server_tool_use"
            )
        })
}

pub fn init_logging() -> Result<(), tracing_subscriber::util::TryInitError> {
    use tracing_subscriber::{EnvFilter, fmt, layer::SubscriberExt, util::SubscriberInitExt};
    tracing_subscriber::registry()
        .with(EnvFilter::from_default_env())
        .with(fmt::layer().compact().with_ansi(true).with_target(false))
        .try_init()
}

pub fn log_startup(address: &str, version: &str, backend: &str, fallback: Option<&str>, pid: u32) {
    tracing::info!(
        service.name = "copilot-proxy-rs",
        service.version = version,
        bind.address = address,
        backend.primary = backend,
        backend.fallback = fallback.unwrap_or(""),
        process.pid = pid,
        "proxy listening"
    );
}

fn count_content_tokens(value: &Value) -> usize {
    match value {
        Value::String(text) => text.split_whitespace().count(),
        Value::Array(items) => items.iter().map(count_content_tokens).sum(),
        Value::Object(object) => object
            .get("text")
            .or_else(|| object.get("content"))
            .map(count_content_tokens)
            .unwrap_or_default(),
        _ => 0,
    }
}
