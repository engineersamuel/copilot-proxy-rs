use std::collections::BTreeMap;

use serde_json::{Map, Value};

use crate::models::{EffortLevel, SupportedEfforts};

pub const COPILOT_REQUEST_API_VERSION: &str = "2026-06-01";

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CopilotRequestMetadata {
    pub request_id: Option<String>,
    pub initiator: Option<String>,
    pub openai_intent: Option<String>,
    pub interaction_id: Option<String>,
    pub interaction_type: Option<String>,
    pub agent_task_id: Option<String>,
    pub extra_headers: BTreeMap<String, String>,
}

pub fn base_copilot_request_headers(token: &str) -> BTreeMap<String, String> {
    let mut headers = BTreeMap::new();
    headers.insert("Authorization".to_string(), format!("Bearer {token}"));
    headers.insert(
        "Copilot-Integration-Id".to_string(),
        "vscode-chat".to_string(),
    );
    headers.insert("Editor-Version".to_string(), "vscode/1.100.0".to_string());
    headers.insert(
        "Editor-Plugin-Version".to_string(),
        "copilot-chat/0.27.2025040201".to_string(),
    );
    headers.insert(
        "User-Agent".to_string(),
        "GithubCopilot/1.155.0".to_string(),
    );
    headers.insert("Accept".to_string(), "application/json".to_string());
    headers.insert(
        "X-GitHub-Api-Version".to_string(),
        COPILOT_REQUEST_API_VERSION.to_string(),
    );
    headers
}

pub fn compute_initiator(body: &Map<String, Value>, strict_continuation: bool) -> &'static str {
    if let Some(messages) = body.get("messages").and_then(Value::as_array) {
        if let Some(last) = messages.last().and_then(Value::as_object) {
            match last.get("role").and_then(Value::as_str).unwrap_or("") {
                "assistant" | "tool" => return "agent",
                "user" => {
                    if content_has_agent_tool_result(last.get("content"))
                        || is_suggestion_mode(last.get("content"))
                    {
                        return "agent";
                    }
                    return "user";
                }
                _ => {}
            }
        }
        return "user";
    }
    if let Some(input) = body.get("input") {
        if input.is_string() {
            return "user";
        }
        if let Some(items) = input.as_array() {
            if input_has_tool_outputs(items) {
                return "agent";
            }
            if let Some(last) = items.last().and_then(Value::as_object) {
                if matches!(
                    last.get("type").and_then(Value::as_str),
                    Some("function_call" | "custom_tool_call")
                ) || last.get("role").and_then(Value::as_str) == Some("assistant")
                {
                    return "agent";
                }
            }
            if strict_continuation
                && body.get("previous_response_id").is_some()
                && !input_has_user_message(items)
            {
                return "agent";
            }
            return "user";
        }
    }
    if strict_continuation && body.get("previous_response_id").is_some() {
        return "agent";
    }
    "user"
}

pub fn filter_anthropic_beta_header(beta: &str) -> Option<String> {
    let supported = [
        "interleaved-thinking",
        "context-management",
        "advanced-tool-use",
    ];
    let filtered: Vec<&str> = beta
        .split(',')
        .map(str::trim)
        .filter(|part| !part.is_empty() && supported.iter().any(|prefix| part.starts_with(prefix)))
        .collect();
    if filtered.is_empty() {
        None
    } else {
        Some(filtered.join(", "))
    }
}

pub fn clamp_effort(effort: &str, supported: &SupportedEfforts) -> Option<&'static str> {
    supported
        .clamp(effort)
        .or_else(|| supported.highest())
        .map(EffortLevel::as_str)
}

pub fn strip_structured_output(body: &mut Map<String, Value>) {
    let remove_output_config = body
        .get_mut("output_config")
        .and_then(Value::as_object_mut)
        .is_some_and(|output_config| {
            output_config.remove("format");
            output_config.is_empty()
        });
    if remove_output_config {
        body.remove("output_config");
    }
}

pub fn adapt_openai_reasoning_effort(
    body: &mut Map<String, Value>,
    supported_efforts: Option<&SupportedEfforts>,
) {
    let Some(requested) = body
        .get("reasoning_effort")
        .and_then(Value::as_str)
        .map(str::to_string)
    else {
        return;
    };
    match supported_efforts.and_then(|supported| clamp_effort(&requested, supported)) {
        Some(clamped) => {
            body.insert(
                "reasoning_effort".to_string(),
                Value::String(clamped.to_string()),
            );
        }
        None => {
            body.remove("reasoning_effort");
        }
    }
}

pub fn adapt_responses_reasoning_effort(
    body: &mut Map<String, Value>,
    supported_efforts: Option<&SupportedEfforts>,
) {
    let Some(reasoning) = body.get_mut("reasoning").and_then(Value::as_object_mut) else {
        return;
    };
    let Some(requested) = reasoning
        .get("effort")
        .and_then(Value::as_str)
        .map(str::to_string)
    else {
        return;
    };

    match supported_efforts.and_then(|supported| clamp_effort(&requested, supported)) {
        Some(clamped) => {
            reasoning.insert("effort".to_string(), Value::String(clamped.to_string()));
        }
        None => {
            reasoning.remove("effort");
        }
    }
    if reasoning.is_empty() {
        body.remove("reasoning");
    }
}

pub fn adapt_responses_tools_for_copilot(body: &mut Map<String, Value>) {
    strip_unsupported_responses_tools(body);
    strip_unsupported_responses_tool_choice(body);
    strip_unsupported_responses_includes(body);
}

fn strip_unsupported_responses_tools(body: &mut Map<String, Value>) {
    let remove_tools_key = if let Some(tools) = body.get_mut("tools").and_then(Value::as_array_mut)
    {
        tools.retain(|tool| !is_unsupported_responses_tool(tool));
        tools.is_empty()
    } else {
        false
    };

    if remove_tools_key {
        body.remove("tools");
        body.remove("tool_choice");
    }
}

fn strip_unsupported_responses_tool_choice(body: &mut Map<String, Value>) {
    let remove_tool_choice = body
        .get("tool_choice")
        .is_some_and(is_unsupported_responses_tool_choice);
    if remove_tool_choice {
        body.remove("tool_choice");
    }
}

fn strip_unsupported_responses_includes(body: &mut Map<String, Value>) {
    let remove_include_key =
        if let Some(includes) = body.get_mut("include").and_then(Value::as_array_mut) {
            includes.retain(|include| {
                include
                    .as_str()
                    .is_none_or(|value| !value.starts_with("image_generation_call."))
            });
            includes.is_empty()
        } else {
            false
        };
    if remove_include_key {
        body.remove("include");
    }
}

fn is_unsupported_responses_tool(tool: &Value) -> bool {
    tool.get("type")
        .and_then(Value::as_str)
        .is_some_and(is_unsupported_responses_tool_type)
}

fn is_unsupported_responses_tool_choice(tool_choice: &Value) -> bool {
    match tool_choice {
        Value::String(choice) => is_unsupported_responses_tool_type(choice),
        Value::Object(choice) => choice
            .get("type")
            .and_then(Value::as_str)
            .is_some_and(is_unsupported_responses_tool_type),
        _ => false,
    }
}

fn is_unsupported_responses_tool_type(tool_type: &str) -> bool {
    tool_type == "image_generation"
}

pub fn adapt_thinking_for_copilot(
    body: &mut Map<String, Value>,
    model: &str,
    supported_efforts: Option<&SupportedEfforts>,
) {
    strip_structured_output(body);
    adapt_output_config_effort(body, supported_efforts);

    let Some(thinking) = body.get_mut("thinking").and_then(Value::as_object_mut) else {
        return;
    };
    match thinking.get("type").and_then(Value::as_str) {
        Some("enabled") if is_adaptive_only_model(model) => {
            thinking.clear();
            thinking.insert("type".to_string(), Value::String("adaptive".to_string()));
            body.remove("output_config");
        }
        Some("adaptive") if !is_adaptive_capable_model(model) => {
            let budget_tokens = thinking
                .get("budget_tokens")
                .and_then(Value::as_u64)
                .unwrap_or(10_000);
            thinking.clear();
            thinking.insert("type".to_string(), Value::String("enabled".to_string()));
            thinking.insert(
                "budget_tokens".to_string(),
                Value::Number(serde_json::Number::from(budget_tokens)),
            );
            body.remove("output_config");
        }
        _ => {}
    }
}

fn adapt_output_config_effort(
    body: &mut Map<String, Value>,
    supported_efforts: Option<&SupportedEfforts>,
) {
    let Some(output_config) = body.get_mut("output_config").and_then(Value::as_object_mut) else {
        return;
    };
    let Some(requested) = output_config
        .get("effort")
        .and_then(Value::as_str)
        .map(str::to_string)
    else {
        if output_config.is_empty() {
            body.remove("output_config");
        }
        return;
    };

    match supported_efforts.and_then(|supported| clamp_effort(&requested, supported)) {
        Some(clamped) => {
            output_config.insert("effort".to_string(), Value::String(clamped.to_string()));
        }
        None => {
            output_config.remove("effort");
        }
    }
    if output_config.is_empty() {
        body.remove("output_config");
    }
}

fn is_adaptive_only_model(model: &str) -> bool {
    matches!(
        model,
        "claude-opus-4.7" | "claude-opus-4-7" | "claude-opus-4.8" | "claude-opus-4-8"
    )
}

fn is_adaptive_capable_model(model: &str) -> bool {
    is_adaptive_only_model(model)
        || matches!(
            model,
            "claude-opus-4.6" | "claude-opus-4-6" | "claude-sonnet-4.6" | "claude-sonnet-4-6"
        )
}

fn content_has_agent_tool_result(value: Option<&Value>) -> bool {
    value.and_then(Value::as_array).is_some_and(|items| {
        items.iter().any(|item| {
            item.as_object()
                .and_then(|object| object.get("type"))
                .and_then(Value::as_str)
                .is_some_and(|kind| {
                    matches!(
                        kind,
                        "tool_result"
                            | "web_search_tool_result"
                            | "web_fetch_tool_result"
                            | "server_tool_use"
                    )
                })
        })
    })
}

fn is_suggestion_mode(value: Option<&Value>) -> bool {
    fn starts_marker(text: &str) -> bool {
        text.trim_start().starts_with("[SUGGESTION MODE:")
    }
    match value {
        Some(Value::String(text)) => starts_marker(text),
        Some(Value::Array(items)) => items.iter().any(|item| {
            item.as_object()
                .and_then(|object| object.get("text"))
                .and_then(Value::as_str)
                .is_some_and(starts_marker)
        }),
        _ => false,
    }
}

pub fn input_has_tool_outputs(items: &[Value]) -> bool {
    items.iter().any(|item| {
        item.as_object()
            .and_then(|object| object.get("type"))
            .and_then(Value::as_str)
            .is_some_and(|kind| matches!(kind, "function_call_output" | "custom_tool_call_output"))
    })
}

pub fn input_has_user_message(items: &[Value]) -> bool {
    items.iter().any(|item| {
        item.as_object()
            .and_then(|object| object.get("role"))
            .and_then(Value::as_str)
            == Some("user")
    })
}
