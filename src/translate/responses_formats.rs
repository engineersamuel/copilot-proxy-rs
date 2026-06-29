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
    copy_prompt_cache_controls(body, &mut out);
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
    Value::Array(
        messages
            .iter()
            .filter_map(|message| {
                let object = message.as_object()?;
                let role = object.get("role").and_then(Value::as_str).unwrap_or("user");
                let content = object
                    .get("content")
                    .map(anthropic_content_to_responses_content)
                    .unwrap_or_else(|| Value::Array(Vec::new()));
                Some(json!({"role": role, "content": content}))
            })
            .collect(),
    )
}

fn anthropic_content_to_responses_content(value: &Value) -> Value {
    match value {
        Value::String(text) => Value::Array(vec![json!({"type": "input_text", "text": text})]),
        Value::Array(items) => Value::Array(
            items
                .iter()
                .filter_map(|item| {
                    let object = item.as_object()?;
                    match object.get("type").and_then(Value::as_str) {
                        Some("text") => Some(json!({
                            "type": "input_text",
                            "text": object.get("text").cloned().unwrap_or_else(|| json!(""))
                        })),
                        _ => None,
                    }
                })
                .collect(),
        ),
        _ => Value::Array(Vec::new()),
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
    let usage = response.get("usage").cloned().unwrap_or_else(|| json!({}));
    json!({
        "id": response.get("id").cloned().unwrap_or_else(|| json!(format!("msg_{}", uuid::Uuid::new_v4().simple()))),
        "type": "message",
        "role": "assistant",
        "model": anthropic_model,
        "content": [{"type": "text", "text": text}],
        "stop_reason": "end_turn",
        "stop_sequence": null,
        "usage": {
            "input_tokens": usage.get("input_tokens").and_then(Value::as_u64).unwrap_or(0),
            "output_tokens": usage.get("output_tokens").and_then(Value::as_u64).unwrap_or(0)
        }
    })
}

pub fn responses_sse_to_anthropic_sse_line(line: &str) -> Option<String> {
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
            let text = value.get("delta").and_then(Value::as_str).unwrap_or("");
            Some(format!(
                "event: content_block_delta\ndata: {}",
                json!({
                    "type": "content_block_delta",
                    "index": 0,
                    "delta": {"type": "text_delta", "text": text}
                })
            ))
        }
        Some("response.completed") => Some("event: done\ndata: [DONE]".to_string()),
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
