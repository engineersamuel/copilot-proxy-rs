use serde_json::{Map, Value, json};

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
        .cloned()
        .unwrap_or_else(|| Value::Array(Vec::new()));
    out.insert("input".to_string(), input);
    if let Some(stream) = body.get("stream") {
        out.insert("stream".to_string(), stream.clone());
    }
    if let Some(effort) = body.get("reasoning_effort") {
        out.insert("reasoning".to_string(), json!({ "effort": effort }));
    }
    copy_prompt_cache_controls(body, &mut out);
    out
}

pub fn responses_to_openai_chat_response(response: &Value) -> Value {
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
    json!({
        "id": response.get("id").cloned().unwrap_or_else(|| json!("chatcmpl-responses")),
        "object": "chat.completion",
        "choices": [{"index": 0, "message": {"role": "assistant", "content": text}, "finish_reason": "stop"}],
        "usage": response.get("usage").cloned().unwrap_or_else(|| json!({}))
    })
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
        "stop_reason": if has_tool_use { "tool_use" } else { "end_turn" },
        "stop_sequence": null,
        "usage": {
            "input_tokens": usage.get("input_tokens").and_then(Value::as_u64).unwrap_or(0),
            "output_tokens": usage.get("output_tokens").and_then(Value::as_u64).unwrap_or(0)
        }
    })
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
        Some("response.completed") => {
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
                        "stop_reason": if has_tool_use { "tool_use" } else { "end_turn" },
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

pub fn responses_sse_to_openai_chat_sse_line(line: &str) -> Option<String> {
    if !line.starts_with("data: ") {
        return Some(line.to_string());
    }
    let payload = line.strip_prefix("data: ")?;
    if payload == "[DONE]" {
        return Some(line.to_string());
    }
    let value: Value = serde_json::from_str(payload).ok()?;
    match value.get("type").and_then(Value::as_str) {
        Some("response.output_text.delta") => {
            let content = value.get("delta").and_then(Value::as_str).unwrap_or("");
            Some(format!(
                "data: {}",
                json!({
                    "object": "chat.completion.chunk",
                    "choices": [{"index": 0, "delta": {"content": content}, "finish_reason": null}]
                })
            ))
        }
        Some("response.completed") => Some("data: [DONE]".to_string()),
        _ => None,
    }
}
