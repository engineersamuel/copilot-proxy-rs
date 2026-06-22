use serde_json::{Map, Value, json};

pub fn anthropic_to_openai_request(
    body: &Map<String, Value>,
    openai_model: &str,
) -> Map<String, Value> {
    let mut out = Map::new();
    out.insert("model".to_string(), Value::String(openai_model.to_string()));
    out.insert(
        "max_completion_tokens".to_string(),
        body.get("max_tokens")
            .cloned()
            .unwrap_or_else(|| json!(4096)),
    );
    let mut messages = Vec::new();
    if let Some(system) = body.get("system") {
        messages.push(json!({"role": "system", "content": system_to_text(system)}));
    }
    if let Some(items) = body.get("messages").and_then(Value::as_array) {
        for item in items {
            if let Some(object) = item.as_object() {
                let role = object.get("role").and_then(Value::as_str).unwrap_or("user");
                let content = object
                    .get("content")
                    .cloned()
                    .unwrap_or(Value::String(String::new()));
                messages.push(json!({"role": role, "content": content_to_openai(content)}));
            }
        }
    }
    out.insert("messages".to_string(), Value::Array(messages));
    if let Some(stream) = body.get("stream") {
        out.insert("stream".to_string(), stream.clone());
    }
    for key in ["temperature", "top_p"] {
        if let Some(value) = body.get(key) {
            out.insert(key.to_string(), value.clone());
        }
    }
    out
}

pub fn openai_to_anthropic_response(response: &Value, anthropic_model: &str) -> Value {
    let choice = response
        .get("choices")
        .and_then(Value::as_array)
        .and_then(|items| items.first());
    let message = choice
        .and_then(|choice| choice.get("message"))
        .unwrap_or(&Value::Null);
    let content = message
        .get("content")
        .and_then(Value::as_str)
        .map(|text| vec![json!({"type": "text", "text": text})])
        .unwrap_or_default();
    let usage = response.get("usage").cloned().unwrap_or_else(|| json!({}));
    json!({
        "id": format!("msg_{}", uuid::Uuid::new_v4().simple()),
        "type": "message",
        "role": "assistant",
        "model": anthropic_model,
        "content": content,
        "stop_reason": "end_turn",
        "stop_sequence": null,
        "usage": {
            "input_tokens": usage.get("prompt_tokens").and_then(Value::as_u64).unwrap_or(0),
            "output_tokens": usage.get("completion_tokens").and_then(Value::as_u64).unwrap_or(0)
        }
    })
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

fn content_to_openai(value: Value) -> Value {
    match value {
        Value::Array(items) => Value::Array(
            items
                .into_iter()
                .filter_map(|item| {
                    let object = item.as_object()?;
                    match object.get("type").and_then(Value::as_str) {
                        Some("text") => Some(json!({
                            "type": "text",
                            "text": object.get("text").cloned().unwrap_or(Value::String(String::new()))
                        })),
                        Some("image") | Some("tool_use") | Some("tool_result") => Some(item),
                        _ => None,
                    }
                })
                .collect(),
        ),
        other => other,
    }
}
