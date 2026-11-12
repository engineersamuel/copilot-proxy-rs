use serde_json::Value;
use uuid::Uuid;

pub fn normalize_openai_response(response: &mut Value, streaming: bool) {
    let Some(object) = response.as_object_mut() else {
        return;
    };
    object.entry("object").or_insert_with(|| {
        Value::String(
            if streaming {
                "chat.completion.chunk"
            } else {
                "chat.completion"
            }
            .to_string(),
        )
    });
    object
        .entry("created")
        .or_insert_with(|| Value::Number(serde_json::Number::from(current_epoch_seconds())));
    object.entry("id").or_insert_with(|| {
        Value::String(format!(
            "chatcmpl-{}",
            &Uuid::new_v4().simple().to_string()[..29]
        ))
    });
    if let Some(choices) = object.get_mut("choices").and_then(Value::as_array_mut) {
        for (index, choice) in choices.iter_mut().enumerate() {
            if let Some(choice_object) = choice.as_object_mut() {
                choice_object
                    .entry("index")
                    .or_insert_with(|| Value::Number(serde_json::Number::from(index)));
            }
        }
    }
    if let Some(usage) = object.get_mut("usage").and_then(Value::as_object_mut) {
        mirror_reasoning_tokens(usage);
    }
}

// Gemini reports usage.reasoning_tokens instead of completion_tokens_details.reasoning_tokens.
fn mirror_reasoning_tokens(usage: &mut serde_json::Map<String, Value>) {
    let Some(reasoning) = usage
        .get("reasoning_tokens")
        .filter(|v| v.is_number())
        .cloned()
    else {
        return;
    };
    let Some(details) = usage
        .entry("completion_tokens_details")
        .or_insert_with(|| Value::Object(serde_json::Map::new()))
        .as_object_mut()
    else {
        return;
    };
    details.entry("reasoning_tokens").or_insert(reasoning);
}

pub fn normalize_openai_sse_line(line: &str) -> String {
    if !line.starts_with("data: ") || line.trim_end() == "data: [DONE]" {
        return line.to_string();
    }
    let json = &line[6..];
    match serde_json::from_str::<Value>(json) {
        Ok(mut value) => {
            normalize_openai_response(&mut value, true);
            format!("data: {}", serde_json::to_string(&value).unwrap())
        }
        Err(_) => line.to_string(),
    }
}

fn current_epoch_seconds() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}
