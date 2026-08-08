use std::collections::HashMap;
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::{Map, Value, json};
use uuid::Uuid;

pub fn openai_chat_to_responses_request(body: &Map<String, Value>) -> Map<String, Value> {
    let mut out = Map::new();
    out.insert(
        "model".to_string(),
        body.get("model")
            .cloned()
            .unwrap_or(Value::String(String::new())),
    );
    let input = body
        .get("messages")
        .map(openai_chat_messages_to_responses_input)
        .unwrap_or_else(|| Value::Array(Vec::new()));
    out.insert("input".to_string(), input);
    if let Some(stream) = body.get("stream") {
        out.insert("stream".to_string(), stream.clone());
    }
    if let Some(effort) = body.get("reasoning_effort") {
        out.insert("reasoning".to_string(), json!({ "effort": effort }));
    }
    if let Some(max_tokens) = body.get("max_completion_tokens") {
        out.insert("max_output_tokens".to_string(), max_tokens.clone());
    }
    if let Some(tools) = body.get("tools").and_then(Value::as_array) {
        out.insert(
            "tools".to_string(),
            Value::Array(
                tools
                    .iter()
                    .filter_map(openai_chat_tool_to_responses_tool)
                    .collect(),
            ),
        );
    }
    if let Some(tool_choice) = body
        .get("tool_choice")
        .and_then(openai_chat_tool_choice_to_responses)
    {
        out.insert("tool_choice".to_string(), tool_choice);
    }
    if let Some(parallel_tool_calls) = body.get("parallel_tool_calls") {
        out.insert(
            "parallel_tool_calls".to_string(),
            parallel_tool_calls.clone(),
        );
    }
    copy_prompt_cache_controls(body, &mut out);
    crate::responses::request::normalize_legacy_message_content_parts(&mut out);
    out
}

fn openai_chat_messages_to_responses_input(value: &Value) -> Value {
    let Some(messages) = value.as_array() else {
        return Value::Array(Vec::new());
    };
    let mut input = Vec::new();
    for message in messages {
        let Some(object) = message.as_object() else {
            continue;
        };
        let role = object.get("role").and_then(Value::as_str).unwrap_or("user");
        if role == "tool" {
            input.push(json!({
                "type": "function_call_output",
                "call_id": object
                    .get("tool_call_id")
                    .and_then(Value::as_str)
                    .unwrap_or(""),
                "output": openai_chat_message_content_text(object.get("content"))
            }));
            continue;
        }
        if let Some(content) = object.get("content")
            && !content.is_null()
            && content.as_str() != Some("")
        {
            let mut translated_message = Map::new();
            translated_message.insert("role".to_string(), Value::String(role.to_string()));
            translated_message.insert("content".to_string(), content.clone());
            input.push(Value::Object(translated_message));
        }
        for tool_call in object
            .get("tool_calls")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            let function = tool_call.get("function").unwrap_or(&Value::Null);
            input.push(json!({
                "type": "function_call",
                "call_id": tool_call.get("id").and_then(Value::as_str).unwrap_or(""),
                "name": function.get("name").and_then(Value::as_str).unwrap_or(""),
                "arguments": function
                    .get("arguments")
                    .and_then(Value::as_str)
                    .unwrap_or("")
            }));
        }
    }
    Value::Array(input)
}

fn openai_chat_message_content_text(value: Option<&Value>) -> String {
    match value {
        Some(Value::String(text)) => text.clone(),
        Some(Value::Array(items)) => items
            .iter()
            .filter_map(|item| item.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join("\n"),
        Some(value) if !value.is_null() => value.to_string(),
        _ => String::new(),
    }
}

pub fn responses_to_openai_chat_response(response: &Value, public_model: &str) -> Value {
    let text = response
        .get("output")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .flat_map(|item| {
            item.get("content")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
        })
        .filter_map(|content| content.get("text").and_then(Value::as_str))
        .collect::<Vec<_>>()
        .join("\n");
    let tool_calls = response
        .get("output")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter(|item| item.get("type").and_then(Value::as_str) == Some("function_call"))
        .map(|item| {
            json!({
                "id": item.get("call_id").and_then(Value::as_str).unwrap_or(""),
                "type": "function",
                "function": {
                    "name": item.get("name").and_then(Value::as_str).unwrap_or(""),
                    "arguments": item.get("arguments").and_then(Value::as_str).unwrap_or("")
                }
            })
        })
        .collect::<Vec<_>>();
    let finish_reason = if tool_calls.is_empty() {
        "stop"
    } else {
        "tool_calls"
    };
    let mut message = Map::new();
    message.insert("role".to_string(), json!("assistant"));
    message.insert(
        "content".to_string(),
        if text.is_empty() && !tool_calls.is_empty() {
            Value::Null
        } else {
            Value::String(text)
        },
    );
    if !tool_calls.is_empty() {
        message.insert("tool_calls".to_string(), Value::Array(tool_calls));
    }
    json!({
        "id": response.get("id").cloned().unwrap_or_else(|| json!("chatcmpl-responses")),
        "object": "chat.completion",
        "created": response
            .get("created_at")
            .and_then(Value::as_u64)
            .unwrap_or_else(unix_timestamp),
        "model": public_model,
        "choices": [{"index": 0, "message": message, "finish_reason": finish_reason}],
        "usage": openai_chat_usage(response.get("usage"))
    })
}

fn openai_chat_tool_to_responses_tool(value: &Value) -> Option<Value> {
    let function = value.get("function")?.as_object()?;
    let name = function.get("name")?.as_str()?;
    let mut tool = Map::new();
    tool.insert("type".to_string(), json!("function"));
    tool.insert("name".to_string(), Value::String(name.to_string()));
    if let Some(description) = function.get("description") {
        tool.insert("description".to_string(), description.clone());
    }
    tool.insert(
        "parameters".to_string(),
        function
            .get("parameters")
            .cloned()
            .unwrap_or_else(|| json!({"type": "object"})),
    );
    if let Some(strict) = function.get("strict") {
        tool.insert("strict".to_string(), strict.clone());
    }
    Some(Value::Object(tool))
}

fn openai_chat_tool_choice_to_responses(value: &Value) -> Option<Value> {
    match value {
        Value::String(choice) => Some(Value::String(choice.clone())),
        Value::Object(choice) if choice.get("type").and_then(Value::as_str) == Some("function") => {
            Some(json!({
                "type": "function",
                "name": choice
                    .get("function")
                    .and_then(|function| function.get("name"))
                    .and_then(Value::as_str)
                    .unwrap_or("")
            }))
        }
        _ => None,
    }
}

fn openai_chat_usage(usage: Option<&Value>) -> Value {
    let input_tokens = usage
        .and_then(|value| value.get("input_tokens"))
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let output_tokens = usage
        .and_then(|value| value.get("output_tokens"))
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let total_tokens = usage
        .and_then(|value| value.get("total_tokens"))
        .and_then(Value::as_u64)
        .unwrap_or(input_tokens + output_tokens);
    json!({
        "prompt_tokens": input_tokens,
        "completion_tokens": output_tokens,
        "total_tokens": total_tokens
    })
}

fn unix_timestamp() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| duration.as_secs())
}

pub fn anthropic_messages_to_responses_request(
    body: &Map<String, Value>,
    model: &str,
) -> Map<String, Value> {
    let mut out = Map::new();
    out.insert("model".to_string(), Value::String(model.to_string()));
    let input = body
        .get("messages")
        .map(anthropic_messages_to_responses_input)
        .unwrap_or_else(|| Value::Array(Vec::new()));
    out.insert("input".to_string(), input);
    if let Some(system) = body.get("system") {
        out.insert(
            "instructions".to_string(),
            Value::String(system_to_text(system)),
        );
    }
    if let Some(max_tokens) = body.get("max_tokens") {
        out.insert("max_output_tokens".to_string(), max_tokens.clone());
    }
    if let Some(stream) = body.get("stream") {
        out.insert("stream".to_string(), stream.clone());
    }
    if let Some(output_config) = body.get("output_config").and_then(Value::as_object) {
        if let Some(effort) = output_config.get("effort") {
            out.insert("reasoning".to_string(), json!({ "effort": effort }));
        }
    }
    if let Some(tools) = body.get("tools").and_then(Value::as_array) {
        out.insert(
            "tools".to_string(),
            Value::Array(
                tools
                    .iter()
                    .filter_map(anthropic_tool_to_responses_tool)
                    .collect(),
            ),
        );
    }
    if let Some(tool_choice) = body
        .get("tool_choice")
        .and_then(anthropic_tool_choice_to_responses)
    {
        out.insert("tool_choice".to_string(), tool_choice);
    }
    copy_prompt_cache_controls(body, &mut out);
    out
}

pub fn has_anthropic_web_search_tool(body: &Map<String, Value>) -> bool {
    body.get("tools")
        .and_then(Value::as_array)
        .is_some_and(|tools| tools.iter().any(is_anthropic_web_search_tool))
}

pub fn anthropic_web_search_to_responses_request(
    body: &Map<String, Value>,
    model: &str,
) -> Map<String, Value> {
    let mut out = anthropic_messages_to_responses_request(body, model);
    let tools = body
        .get("tools")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(anthropic_tool_to_web_search_responses_tool)
        .collect();
    out.insert("tools".to_string(), Value::Array(tools));
    out.insert(
        "tool_choice".to_string(),
        Value::String("required".to_string()),
    );
    out
}

fn copy_prompt_cache_controls(source: &Map<String, Value>, target: &mut Map<String, Value>) {
    for key in ["prompt_cache_key", "prompt_cache_retention"] {
        if let Some(value) = source.get(key) {
            target.insert(key.to_string(), value.clone());
        }
    }
}

fn anthropic_messages_to_responses_input(value: &Value) -> Value {
    let Some(messages) = value.as_array() else {
        return Value::Array(Vec::new());
    };
    let mut input = Vec::new();
    for message in messages {
        let Some(object) = message.as_object() else {
            continue;
        };
        let role = object.get("role").and_then(Value::as_str).unwrap_or("user");
        append_anthropic_message_items(
            &mut input,
            role,
            object.get("content").unwrap_or(&Value::Null),
        );
    }
    Value::Array(input)
}

fn append_anthropic_message_items(input: &mut Vec<Value>, role: &str, value: &Value) {
    match value {
        Value::String(text) => push_text_message(input, role, vec![text_content(role, text)]),
        Value::Array(items) => {
            let mut message_content = Vec::new();
            for item in items {
                let Some(object) = item.as_object() else {
                    continue;
                };
                match object.get("type").and_then(Value::as_str) {
                    Some("text") => {
                        let text = object.get("text").and_then(Value::as_str).unwrap_or("");
                        message_content.push(text_content(role, text));
                    }
                    Some("tool_use") => {
                        flush_text_message(input, role, &mut message_content);
                        let call_id = object.get("id").and_then(Value::as_str).unwrap_or("");
                        let name = object.get("name").and_then(Value::as_str).unwrap_or("");
                        let arguments =
                            serde_json::to_string(object.get("input").unwrap_or(&json!({})))
                                .unwrap_or_else(|_| "{}".to_string());
                        input.push(json!({
                            "type": "function_call",
                            "call_id": call_id,
                            "name": name,
                            "arguments": arguments
                        }));
                    }
                    Some("tool_result") => {
                        flush_text_message(input, role, &mut message_content);
                        input.push(json!({
                            "type": "function_call_output",
                            "call_id": object
                                .get("tool_use_id")
                                .and_then(Value::as_str)
                                .unwrap_or(""),
                            "output": anthropic_tool_result_text(object.get("content"))
                        }));
                    }
                    _ => {}
                }
            }
            flush_text_message(input, role, &mut message_content);
        }
        _ => {}
    }
}

fn text_content(role: &str, text: &str) -> Value {
    let content_type = if role == "assistant" {
        "output_text"
    } else {
        "input_text"
    };
    json!({"type": content_type, "text": text})
}

fn flush_text_message(input: &mut Vec<Value>, role: &str, content: &mut Vec<Value>) {
    if !content.is_empty() {
        push_text_message(input, role, std::mem::take(content));
    }
}

fn push_text_message(input: &mut Vec<Value>, role: &str, content: Vec<Value>) {
    input.push(json!({"type": "message", "role": role, "content": content}));
}

fn anthropic_tool_result_text(value: Option<&Value>) -> String {
    match value {
        Some(Value::String(text)) => text.clone(),
        Some(Value::Array(items)) => items
            .iter()
            .filter_map(|item| item.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join("\n"),
        Some(other) => other.to_string(),
        None => String::new(),
    }
}

fn anthropic_tool_to_responses_tool(value: &Value) -> Option<Value> {
    let object = value.as_object()?;
    let name = object.get("name")?.as_str()?;
    let mut tool = Map::new();
    tool.insert("type".to_string(), Value::String("function".to_string()));
    tool.insert("name".to_string(), Value::String(name.to_string()));
    if let Some(description) = object.get("description") {
        tool.insert("description".to_string(), description.clone());
    }
    tool.insert(
        "parameters".to_string(),
        object
            .get("input_schema")
            .cloned()
            .unwrap_or_else(|| json!({"type": "object"})),
    );
    Some(Value::Object(tool))
}

fn anthropic_tool_to_web_search_responses_tool(value: &Value) -> Option<Value> {
    if !is_anthropic_web_search_tool(value) {
        return anthropic_tool_to_responses_tool(value);
    }
    let mut tool = Map::new();
    tool.insert("type".to_string(), Value::String("web_search".to_string()));
    if let Some(allowed_domains) = value.get("allowed_domains").and_then(Value::as_array) {
        tool.insert(
            "filters".to_string(),
            json!({"allowed_domains": allowed_domains}),
        );
    }
    if let Some(user_location) = value.get("user_location") {
        tool.insert("user_location".to_string(), user_location.clone());
    }
    Some(Value::Object(tool))
}

fn is_anthropic_web_search_tool(value: &Value) -> bool {
    value
        .get("type")
        .and_then(Value::as_str)
        .is_some_and(|tool_type| tool_type.starts_with("web_search_"))
}

fn anthropic_tool_choice_to_responses(value: &Value) -> Option<Value> {
    let object = value.as_object()?;
    match object.get("type").and_then(Value::as_str) {
        Some("auto") => Some(json!("auto")),
        Some("any") => Some(json!("required")),
        Some("none") => Some(json!("none")),
        Some("tool") => Some(json!({
            "type": "function",
            "name": object.get("name").and_then(Value::as_str).unwrap_or("")
        })),
        _ => None,
    }
}

fn system_to_text(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        Value::Array(items) => items
            .iter()
            .filter_map(|item| item.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

pub fn responses_to_anthropic_message_response(response: &Value, anthropic_model: &str) -> Value {
    let mut content = Vec::new();
    let mut has_tool_use = false;
    for item in response
        .get("output")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        match item.get("type").and_then(Value::as_str) {
            Some("message") => {
                content.extend(
                    item.get("content")
                        .and_then(Value::as_array)
                        .into_iter()
                        .flatten()
                        .filter_map(|part| {
                            let text = part.get("text").and_then(Value::as_str)?;
                            Some(json!({"type": "text", "text": text}))
                        }),
                );
            }
            Some("function_call") => {
                has_tool_use = true;
                let arguments = item
                    .get("arguments")
                    .and_then(Value::as_str)
                    .and_then(|arguments| serde_json::from_str(arguments).ok())
                    .unwrap_or_else(|| json!({}));
                content.push(json!({
                    "type": "tool_use",
                    "id": item.get("call_id").cloned().unwrap_or_else(|| json!("")),
                    "name": item.get("name").cloned().unwrap_or_else(|| json!("")),
                    "input": arguments
                }));
            }
            _ => {}
        }
    }
    let usage = response.get("usage").cloned().unwrap_or_else(|| json!({}));
    json!({
        "id": response.get("id").cloned().unwrap_or_else(|| json!(format!("msg_{}", uuid::Uuid::new_v4().simple()))),
        "type": "message",
        "role": "assistant",
        "model": anthropic_model,
        "content": content,
        "stop_reason": anthropic_stop_reason(response, has_tool_use),
        "stop_sequence": null,
        "usage": {
            "input_tokens": usage.get("input_tokens").and_then(Value::as_u64).unwrap_or(0),
            "output_tokens": usage.get("output_tokens").and_then(Value::as_u64).unwrap_or(0)
        }
    })
}

/// Maps a Responses status to an Anthropic `stop_reason`.
///
/// Truncated output must surface as `max_tokens` so clients can distinguish a
/// budget-exhausted turn from a completed one.
fn anthropic_stop_reason(response: &Value, has_tool_use: bool) -> &'static str {
    if has_tool_use {
        return "tool_use";
    }
    let truncated = response.get("status").and_then(Value::as_str) == Some("incomplete")
        && response
            .get("incomplete_details")
            .and_then(|details| details.get("reason"))
            .and_then(Value::as_str)
            == Some("max_output_tokens");
    if truncated { "max_tokens" } else { "end_turn" }
}

pub fn responses_sse_to_anthropic_sse_line(line: &str) -> Option<String> {
    if line.starts_with("event:") {
        return None;
    }
    if !line.starts_with("data: ") {
        return Some(line.to_string());
    }
    let payload = line.strip_prefix("data: ")?;
    if payload == "[DONE]" {
        return Some(line.to_string());
    }
    let value: Value = serde_json::from_str(payload).ok()?;
    match value.get("type").and_then(Value::as_str) {
        Some("response.created") => {
            let response = value.get("response").unwrap_or(&Value::Null);
            Some(format!(
                "event: message_start\ndata: {}",
                json!({
                    "type": "message_start",
                    "message": {
                        "id": response.get("id").cloned().unwrap_or_else(|| json!("")),
                        "type": "message",
                        "role": "assistant",
                        "model": response.get("model").cloned().unwrap_or_else(|| json!("")),
                        "content": [],
                        "stop_reason": null,
                        "stop_sequence": null,
                        "usage": {
                            "input_tokens": response
                                .get("usage")
                                .and_then(|usage| usage.get("input_tokens"))
                                .and_then(Value::as_u64)
                                .unwrap_or(0),
                            "output_tokens": 0
                        }
                    }
                })
            ))
        }
        Some("response.content_part.added")
            if value
                .get("part")
                .and_then(|part| part.get("type"))
                .and_then(Value::as_str)
                == Some("output_text") =>
        {
            let index = value
                .get("content_index")
                .and_then(Value::as_u64)
                .unwrap_or(0);
            Some(format!(
                "event: content_block_start\ndata: {}",
                json!({
                    "type": "content_block_start",
                    "index": index,
                    "content_block": {"type": "text", "text": ""}
                })
            ))
        }
        Some("response.output_item.added")
            if value
                .get("item")
                .and_then(|item| item.get("type"))
                .and_then(Value::as_str)
                == Some("function_call") =>
        {
            let item = value.get("item").unwrap_or(&Value::Null);
            let index = value
                .get("output_index")
                .and_then(Value::as_u64)
                .unwrap_or(0);
            Some(format!(
                "event: content_block_start\ndata: {}",
                json!({
                    "type": "content_block_start",
                    "index": index,
                    "content_block": {
                        "type": "tool_use",
                        "id": item.get("call_id").cloned().unwrap_or_else(|| json!("")),
                        "name": item.get("name").cloned().unwrap_or_else(|| json!("")),
                        "input": {}
                    }
                })
            ))
        }
        Some("response.output_text.delta") => {
            let text = value.get("delta").and_then(Value::as_str).unwrap_or("");
            let index = value
                .get("content_index")
                .and_then(Value::as_u64)
                .or_else(|| value.get("output_index").and_then(Value::as_u64))
                .unwrap_or(0);
            Some(format!(
                "event: content_block_delta\ndata: {}",
                json!({
                    "type": "content_block_delta",
                    "index": index,
                    "delta": {"type": "text_delta", "text": text}
                })
            ))
        }
        Some("response.function_call_arguments.delta") => {
            let index = value
                .get("output_index")
                .and_then(Value::as_u64)
                .unwrap_or(0);
            let delta = value.get("delta").and_then(Value::as_str).unwrap_or("");
            Some(format!(
                "event: content_block_delta\ndata: {}",
                json!({
                    "type": "content_block_delta",
                    "index": index,
                    "delta": {"type": "input_json_delta", "partial_json": delta}
                })
            ))
        }
        Some("response.content_part.done") => {
            let index = value
                .get("content_index")
                .and_then(Value::as_u64)
                .unwrap_or(0);
            Some(format!(
                "event: content_block_stop\ndata: {}",
                json!({"type": "content_block_stop", "index": index})
            ))
        }
        Some("response.output_item.done")
            if value
                .get("item")
                .and_then(|item| item.get("type"))
                .and_then(Value::as_str)
                == Some("function_call") =>
        {
            let index = value
                .get("output_index")
                .and_then(Value::as_u64)
                .unwrap_or(0);
            Some(format!(
                "event: content_block_stop\ndata: {}",
                json!({"type": "content_block_stop", "index": index})
            ))
        }
        Some("response.completed") | Some("response.incomplete") => {
            let response = value.get("response").unwrap_or(&Value::Null);
            let has_tool_use = response
                .get("output")
                .and_then(Value::as_array)
                .is_some_and(|output| {
                    output.iter().any(|item| {
                        item.get("type").and_then(Value::as_str) == Some("function_call")
                    })
                });
            let output_tokens = response
                .get("usage")
                .and_then(|usage| usage.get("output_tokens"))
                .and_then(Value::as_u64)
                .unwrap_or(0);
            Some(format!(
                "event: message_delta\ndata: {}\n\nevent: message_stop\ndata: {}\n\nevent: done\ndata: [DONE]",
                json!({
                    "type": "message_delta",
                    "delta": {
                        "stop_reason": anthropic_stop_reason(response, has_tool_use),
                        "stop_sequence": null
                    },
                    "usage": {"output_tokens": output_tokens}
                }),
                json!({"type": "message_stop"})
            ))
        }
        _ => None,
    }
}

#[derive(Debug, Clone)]
struct ChatToolCall {
    id: String,
}

#[derive(Debug)]
pub struct ResponsesToChatStream {
    id: String,
    created: u64,
    model: String,
    role_emitted: bool,
    completed: bool,
    saw_tool_call: bool,
    tool_calls: HashMap<u64, ChatToolCall>,
}

impl ResponsesToChatStream {
    pub fn new(model: String) -> Self {
        Self {
            id: format!("chatcmpl-{}", Uuid::new_v4().simple()),
            created: unix_timestamp(),
            model,
            role_emitted: false,
            completed: false,
            saw_tool_call: false,
            tool_calls: HashMap::new(),
        }
    }

    pub fn map_line(&mut self, line: &str) -> Result<Vec<String>, std::io::Error> {
        let Some(payload) = line.strip_prefix("data: ") else {
            return Ok(Vec::new());
        };
        if payload == "[DONE]" {
            return Ok(self.finish(None));
        }
        let value: Value = serde_json::from_str(payload).map_err(|error| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("invalid Responses SSE JSON: {error}"),
            )
        })?;
        match value.get("type").and_then(Value::as_str) {
            Some("response.created") => Ok(self.role_chunk().into_iter().collect()),
            Some("response.output_text.delta") => {
                let mut events = self.role_chunk().into_iter().collect::<Vec<_>>();
                let content = value.get("delta").and_then(Value::as_str).unwrap_or("");
                events.push(self.chunk(json!({
                    "choices": [{
                        "index": 0,
                        "delta": {"content": content},
                        "finish_reason": null
                    }]
                })));
                Ok(events)
            }
            Some("response.output_item.added")
                if value
                    .get("item")
                    .and_then(|item| item.get("type"))
                    .and_then(Value::as_str)
                    == Some("function_call") =>
            {
                let mut events = self.role_chunk().into_iter().collect::<Vec<_>>();
                let index = value
                    .get("output_index")
                    .and_then(Value::as_u64)
                    .unwrap_or(0);
                let item = value.get("item").unwrap_or(&Value::Null);
                let call_id = item
                    .get("call_id")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string();
                let name = item.get("name").and_then(Value::as_str).unwrap_or("");
                self.saw_tool_call = true;
                self.tool_calls.insert(
                    index,
                    ChatToolCall {
                        id: call_id.clone(),
                    },
                );
                events.push(self.chunk(json!({
                    "choices": [{
                        "index": 0,
                        "delta": {
                            "tool_calls": [{
                                "index": index,
                                "id": call_id,
                                "type": "function",
                                "function": {"name": name, "arguments": ""}
                            }]
                        },
                        "finish_reason": null
                    }]
                })));
                Ok(events)
            }
            Some("response.function_call_arguments.delta") => {
                let mut events = self.role_chunk().into_iter().collect::<Vec<_>>();
                let index = value
                    .get("output_index")
                    .and_then(Value::as_u64)
                    .unwrap_or(0);
                let event_call_id = value.get("call_id").and_then(Value::as_str).unwrap_or("");
                let call_id = self
                    .tool_calls
                    .get(&index)
                    .map(|tool_call| tool_call.id.as_str())
                    .filter(|call_id| !call_id.is_empty())
                    .unwrap_or(event_call_id);
                let arguments = value.get("delta").and_then(Value::as_str).unwrap_or("");
                self.saw_tool_call = true;
                events.push(self.chunk(json!({
                    "choices": [{
                        "index": 0,
                        "delta": {
                            "tool_calls": [{
                                "index": index,
                                "id": call_id,
                                "type": "function",
                                "function": {"arguments": arguments}
                            }]
                        },
                        "finish_reason": null
                    }]
                })));
                Ok(events)
            }
            Some("response.completed") => Ok(self.finish(value.get("response"))),
            _ => Ok(Vec::new()),
        }
    }

    fn role_chunk(&mut self) -> Option<String> {
        if self.role_emitted {
            return None;
        }
        self.role_emitted = true;
        Some(self.chunk(json!({
            "choices": [{
                "index": 0,
                "delta": {"role": "assistant", "content": ""},
                "finish_reason": null
            }]
        })))
    }

    fn finish(&mut self, response: Option<&Value>) -> Vec<String> {
        if self.completed {
            return Vec::new();
        }
        self.completed = true;
        let mut events = self.role_chunk().into_iter().collect::<Vec<_>>();
        let has_response_tool_call = response
            .and_then(|value| value.get("output"))
            .and_then(Value::as_array)
            .is_some_and(|output| {
                output
                    .iter()
                    .any(|item| item.get("type").and_then(Value::as_str) == Some("function_call"))
            });
        let finish_reason = if self.saw_tool_call || has_response_tool_call {
            "tool_calls"
        } else {
            "stop"
        };
        let usage = response
            .map(|value| openai_chat_usage(value.get("usage")))
            .unwrap_or_else(|| openai_chat_usage(None));
        events.push(self.chunk(json!({
            "choices": [{
                "index": 0,
                "delta": {},
                "finish_reason": finish_reason
            }],
            "usage": usage
        })));
        events.push("data: [DONE]".to_string());
        events
    }

    fn chunk(&self, fields: Value) -> String {
        let mut object = fields.as_object().cloned().unwrap_or_default();
        object.insert("id".to_string(), Value::String(self.id.clone()));
        object.insert("object".to_string(), json!("chat.completion.chunk"));
        object.insert("created".to_string(), json!(self.created));
        object.insert("model".to_string(), Value::String(self.model.clone()));
        format!("data: {}", Value::Object(object))
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::openai_chat_to_responses_request;

    #[test]
    fn openai_chat_bridge_normalizes_multimodal_content_for_responses() {
        let body = json!({
            "model": "gpt-5.6-sol",
            "messages": [{
                "role": "user",
                "content": [
                    {
                        "type": "image_url",
                        "image_url": {"url": "data:image/png;base64,aGVsbG8="}
                    },
                    {"type": "text", "text": "Describe this image"}
                ]
            }]
        })
        .as_object()
        .cloned()
        .unwrap();

        let translated = openai_chat_to_responses_request(&body);

        assert_eq!(
            translated["input"][0]["content"],
            json!([
                {
                    "type": "input_image",
                    "image_url": "data:image/png;base64,aGVsbG8="
                },
                {"type": "input_text", "text": "Describe this image"}
            ])
        );
    }
}
