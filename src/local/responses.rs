use std::collections::{BTreeMap, BTreeSet};
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::{Map, Value, json};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LocalToolKind {
    Function,
    Custom,
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum ResponsesTranslationError {
    #[error("unsupported Responses input: {0}")]
    UnsupportedInput(String),
    #[error("unsupported Responses tool: {0}")]
    UnsupportedTool(String),
    #[error("invalid Responses request: {0}")]
    InvalidRequest(String),
    #[error("invalid chat completion response: {0}")]
    InvalidResponse(String),
}

#[derive(Debug, Clone)]
pub struct TranslatedResponsesRequest {
    pub chat_body: Map<String, Value>,
    pub input_items: Vec<Value>,
    pub tool_kinds: BTreeMap<String, LocalToolKind>,
}

pub fn responses_to_chat(
    mut body: Map<String, Value>,
    upstream_model: &str,
) -> Result<TranslatedResponsesRequest, ResponsesTranslationError> {
    let input_items = normalize_input(body.remove("input"))?;
    let mut messages = Vec::new();
    if let Some(instructions) = body.remove("instructions") {
        let instructions = required_string(&instructions, "instructions")?;
        messages.push(json!({"role": "system", "content": instructions}));
    }
    messages.extend(translate_input_items(&input_items)?);

    let mut tool_kinds = BTreeMap::new();
    let tools = body
        .remove("tools")
        .map(|tools| translate_tools(tools, &mut tool_kinds))
        .transpose()?;
    let tool_choice = body
        .remove("tool_choice")
        .map(|choice| translate_tool_choice(choice, &tool_kinds))
        .transpose()?;

    let mut chat_body = Map::new();
    chat_body.insert(
        "model".to_string(),
        Value::String(upstream_model.to_string()),
    );
    chat_body.insert("messages".to_string(), Value::Array(messages));
    for key in [
        "temperature",
        "top_p",
        "seed",
        "stop",
        "parallel_tool_calls",
        "stream",
    ] {
        if let Some(value) = body.remove(key) {
            chat_body.insert(key.to_string(), value);
        }
    }
    if let Some(max_tokens) = body.remove("max_output_tokens") {
        chat_body.insert("max_tokens".to_string(), max_tokens);
    }
    if chat_body.get("stream").and_then(Value::as_bool) == Some(true) {
        chat_body.insert("stream_options".to_string(), json!({"include_usage": true}));
    }
    if let Some(tools) = tools {
        chat_body.insert("tools".to_string(), Value::Array(tools));
    }
    if let Some(tool_choice) = tool_choice {
        chat_body.insert("tool_choice".to_string(), tool_choice);
    }

    Ok(TranslatedResponsesRequest {
        chat_body,
        input_items,
        tool_kinds,
    })
}

pub fn chat_to_responses(
    chat: &Value,
    response_id: &str,
    public_model: &str,
    tool_kinds: &BTreeMap<String, LocalToolKind>,
) -> Result<Value, ResponsesTranslationError> {
    let message = chat
        .get("choices")
        .and_then(Value::as_array)
        .and_then(|choices| choices.first())
        .and_then(|choice| choice.get("message"))
        .and_then(Value::as_object)
        .ok_or_else(|| invalid_response("choices[0].message must be an object"))?;
    let mut output = Vec::new();
    match message.get("content") {
        Some(Value::String(content)) if !content.is_empty() => {
            output.push(json!({
                "id": format!("msg_{response_id}"),
                "type": "message",
                "status": "completed",
                "role": "assistant",
                "content": [{
                    "type": "output_text",
                    "text": content,
                    "annotations": []
                }]
            }));
        }
        None | Some(Value::Null) | Some(Value::String(_)) => {}
        Some(_) => return Err(invalid_response("message.content must be a string or null")),
    }
    if let Some(tool_calls) = message.get("tool_calls") {
        let calls = tool_calls
            .as_array()
            .ok_or_else(|| invalid_response("message.tool_calls must be an array"))?;
        for (index, call) in calls.iter().enumerate() {
            output.push(translate_chat_tool_call(
                call,
                index,
                response_id,
                tool_kinds,
            )?);
        }
    }
    let usage = translate_chat_usage(chat.get("usage"))?;
    Ok(json!({
        "id": response_id,
        "object": "response",
        "created_at": current_epoch_seconds(),
        "status": "completed",
        "background": false,
        "error": null,
        "incomplete_details": null,
        "instructions": null,
        "max_output_tokens": null,
        "model": public_model,
        "output": output,
        "parallel_tool_calls": true,
        "previous_response_id": null,
        "reasoning": {"effort": null, "summary": null},
        "store": false,
        "temperature": null,
        "text": {"format": {"type": "text"}, "verbosity": "low"},
        "tool_choice": "auto",
        "tools": [],
        "top_p": null,
        "truncation": "disabled",
        "usage": usage
    }))
}

fn translate_chat_tool_call(
    call: &Value,
    index: usize,
    response_id: &str,
    tool_kinds: &BTreeMap<String, LocalToolKind>,
) -> Result<Value, ResponsesTranslationError> {
    let object = call
        .as_object()
        .ok_or_else(|| invalid_response("tool call must be an object"))?;
    if object.get("type").and_then(Value::as_str) != Some("function") {
        return Err(invalid_response("tool call type must be function"));
    }
    let call_id = response_field_string(object, "id", "tool call")?;
    let function = object
        .get("function")
        .and_then(Value::as_object)
        .ok_or_else(|| invalid_response("tool call function must be an object"))?;
    let name = response_field_string(function, "name", "tool call function")?;
    let arguments = response_field_string(function, "arguments", "tool call function")?;
    match tool_kinds.get(name) {
        Some(LocalToolKind::Function) => Ok(json!({
            "id": format!("fc_{response_id}_{index}"),
            "type": "function_call",
            "status": "completed",
            "call_id": call_id,
            "name": name,
            "arguments": arguments
        })),
        Some(LocalToolKind::Custom) => {
            let arguments: Value = serde_json::from_str(arguments)
                .map_err(|_| invalid_response("custom tool arguments must be valid JSON"))?;
            let input = arguments
                .as_object()
                .and_then(|arguments| arguments.get("input"))
                .and_then(Value::as_str)
                .ok_or_else(|| {
                    invalid_response("custom tool arguments must contain string input")
                })?;
            Ok(json!({
                "id": format!("ctc_{response_id}_{index}"),
                "type": "custom_tool_call",
                "status": "completed",
                "call_id": call_id,
                "name": name,
                "input": input
            }))
        }
        None => Err(invalid_response(format!(
            "tool call references unknown tool {name}"
        ))),
    }
}

fn translate_chat_usage(usage: Option<&Value>) -> Result<Value, ResponsesTranslationError> {
    let Some(usage) = usage else {
        return Ok(Value::Null);
    };
    if usage.is_null() {
        return Ok(Value::Null);
    }
    let usage = usage
        .as_object()
        .ok_or_else(|| invalid_response("usage must be an object or null"))?;
    let input_tokens = response_token_count(usage, "prompt_tokens")?;
    let output_tokens = response_token_count(usage, "completion_tokens")?;
    let total_tokens = response_token_count(usage, "total_tokens")?;
    Ok(json!({
        "input_tokens": input_tokens,
        "output_tokens": output_tokens,
        "total_tokens": total_tokens
    }))
}

fn response_token_count(
    usage: &Map<String, Value>,
    field: &str,
) -> Result<u64, ResponsesTranslationError> {
    usage
        .get(field)
        .ok_or_else(|| invalid_response(format!("usage.{field} is missing")))?
        .as_u64()
        .ok_or_else(|| invalid_response(format!("usage.{field} must be an unsigned integer")))
}

fn response_field_string<'a>(
    object: &'a Map<String, Value>,
    field: &str,
    context: &str,
) -> Result<&'a str, ResponsesTranslationError> {
    object
        .get(field)
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| invalid_response(format!("{context}.{field} must be a non-empty string")))
}

fn invalid_response(message: impl Into<String>) -> ResponsesTranslationError {
    ResponsesTranslationError::InvalidResponse(message.into())
}

fn current_epoch_seconds() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| duration.as_secs())
}

fn normalize_input(input: Option<Value>) -> Result<Vec<Value>, ResponsesTranslationError> {
    match input {
        Some(Value::String(text)) => Ok(vec![json!({
            "type": "message",
            "role": "user",
            "content": [{"type": "input_text", "text": text}]
        })]),
        Some(Value::Array(items)) => Ok(items),
        Some(_) => Err(ResponsesTranslationError::InvalidRequest(
            "input must be a string or array".to_string(),
        )),
        None => Err(ResponsesTranslationError::InvalidRequest(
            "missing input".to_string(),
        )),
    }
}

fn translate_input_items(input_items: &[Value]) -> Result<Vec<Value>, ResponsesTranslationError> {
    let mut messages = Vec::new();
    let mut pending_tool_calls = Vec::new();
    let mut pending_call_ids = BTreeSet::new();
    let mut outstanding_call_ids = BTreeSet::new();
    let mut call_ids = BTreeSet::new();
    let mut output_call_ids = BTreeSet::new();
    for item in input_items {
        let (object, item_type) = input_item(item)?;
        match item_type {
            "function_call" => {
                ensure_complete_tool_call_group(&outstanding_call_ids, "before new calls")?;
                let call_id = register_call_id(object, &mut call_ids)?;
                pending_call_ids.insert(call_id.to_string());
                pending_tool_calls.push(translate_function_call(object)?);
            }
            "custom_tool_call" => {
                ensure_complete_tool_call_group(&outstanding_call_ids, "before new calls")?;
                let call_id = register_call_id(object, &mut call_ids)?;
                pending_call_ids.insert(call_id.to_string());
                pending_tool_calls.push(translate_custom_tool_call(object)?);
            }
            "message" => {
                flush_tool_calls(
                    &mut messages,
                    &mut pending_tool_calls,
                    &mut pending_call_ids,
                    &mut outstanding_call_ids,
                );
                ensure_complete_tool_call_group(&outstanding_call_ids, "before message")?;
                messages.push(translate_message(object)?);
            }
            "function_call_output" | "custom_tool_call_output" => {
                flush_tool_calls(
                    &mut messages,
                    &mut pending_tool_calls,
                    &mut pending_call_ids,
                    &mut outstanding_call_ids,
                );
                validate_output_call_id(
                    object,
                    &call_ids,
                    &mut output_call_ids,
                    &mut outstanding_call_ids,
                )?;
                messages.push(translate_tool_call_output(object)?);
            }
            unsupported => {
                return Err(ResponsesTranslationError::UnsupportedInput(
                    unsupported.to_string(),
                ));
            }
        }
    }
    flush_tool_calls(
        &mut messages,
        &mut pending_tool_calls,
        &mut pending_call_ids,
        &mut outstanding_call_ids,
    );
    ensure_complete_tool_call_group(&outstanding_call_ids, "at end of input")?;
    Ok(messages)
}

fn register_call_id<'a>(
    call: &'a Map<String, Value>,
    call_ids: &mut BTreeSet<String>,
) -> Result<&'a str, ResponsesTranslationError> {
    let call_id = required_field_string(call, "call_id")?;
    if !call_ids.insert(call_id.to_string()) {
        return Err(ResponsesTranslationError::InvalidRequest(format!(
            "duplicate tool call id: {call_id}"
        )));
    }
    Ok(call_id)
}

fn validate_output_call_id(
    output: &Map<String, Value>,
    call_ids: &BTreeSet<String>,
    output_call_ids: &mut BTreeSet<String>,
    outstanding_call_ids: &mut BTreeSet<String>,
) -> Result<(), ResponsesTranslationError> {
    let call_id = required_field_string(output, "call_id")?;
    if !call_ids.contains(call_id) {
        return Err(ResponsesTranslationError::InvalidRequest(format!(
            "unmatched tool output call id: {call_id}"
        )));
    }
    if output_call_ids.contains(call_id) {
        return Err(ResponsesTranslationError::InvalidRequest(format!(
            "duplicate tool output call id: {call_id}"
        )));
    }
    if !outstanding_call_ids.remove(call_id) {
        return Err(ResponsesTranslationError::InvalidRequest(format!(
            "unmatched tool output call id: {call_id}"
        )));
    }
    output_call_ids.insert(call_id.to_string());
    Ok(())
}

fn ensure_complete_tool_call_group(
    outstanding_call_ids: &BTreeSet<String>,
    boundary: &str,
) -> Result<(), ResponsesTranslationError> {
    if outstanding_call_ids.is_empty() {
        return Ok(());
    }
    Err(ResponsesTranslationError::InvalidRequest(format!(
        "tool call group missing outputs {boundary}: {}",
        summarize_call_ids(outstanding_call_ids)
    )))
}

fn summarize_call_ids(call_ids: &BTreeSet<String>) -> String {
    const MAX_IDS: usize = 4;
    const MAX_ID_CHARS: usize = 64;

    let mut summary = call_ids
        .iter()
        .take(MAX_IDS)
        .map(|call_id| {
            let mut chars = call_id.chars();
            let prefix = chars.by_ref().take(MAX_ID_CHARS).collect::<String>();
            if chars.next().is_some() {
                format!("{prefix}...")
            } else {
                prefix
            }
        })
        .collect::<Vec<_>>();
    if call_ids.len() > MAX_IDS {
        summary.push(format!("+{} more", call_ids.len() - MAX_IDS));
    }
    summary.join(", ")
}

fn input_item(item: &Value) -> Result<(&Map<String, Value>, &str), ResponsesTranslationError> {
    let object = item
        .as_object()
        .ok_or_else(|| ResponsesTranslationError::UnsupportedInput(value_kind(item).to_string()))?;
    let item_type = match object.get("type") {
        Some(item_type) => required_string(item_type, "type")?,
        None if object.contains_key("role") => "message",
        None => {
            return Err(ResponsesTranslationError::InvalidRequest(
                "missing required field type".to_string(),
            ));
        }
    };
    Ok((object, item_type))
}

fn flush_tool_calls(
    messages: &mut Vec<Value>,
    pending_tool_calls: &mut Vec<Value>,
    pending_call_ids: &mut BTreeSet<String>,
    outstanding_call_ids: &mut BTreeSet<String>,
) {
    if !pending_tool_calls.is_empty() {
        messages.push(json!({
            "role": "assistant",
            "tool_calls": std::mem::take(pending_tool_calls)
        }));
        outstanding_call_ids.append(pending_call_ids);
    }
}

fn translate_message(message: &Map<String, Value>) -> Result<Value, ResponsesTranslationError> {
    let role = required_field_string(message, "role")?;
    if !matches!(role, "system" | "developer" | "user" | "assistant") {
        return Err(ResponsesTranslationError::InvalidRequest(format!(
            "invalid message role: {role}"
        )));
    }
    let content = message.get("content").ok_or_else(|| {
        ResponsesTranslationError::InvalidRequest("message missing content".to_string())
    })?;
    let content = translate_message_content(content)?;
    Ok(json!({"role": role, "content": content}))
}

fn translate_message_content(content: &Value) -> Result<Value, ResponsesTranslationError> {
    match content {
        Value::String(text) => Ok(Value::String(text.clone())),
        Value::Array(parts) => {
            let text = parts
                .iter()
                .map(|part| {
                    let part = part.as_object().ok_or_else(|| {
                        ResponsesTranslationError::UnsupportedInput(value_kind(part).to_string())
                    })?;
                    let part_type = required_field_string(part, "type")?;
                    if !matches!(part_type, "input_text" | "output_text" | "text") {
                        return Err(ResponsesTranslationError::UnsupportedInput(
                            part_type.to_string(),
                        ));
                    }
                    required_field_string(part, "text").map(str::to_string)
                })
                .collect::<Result<Vec<_>, _>>()?
                .join("");
            Ok(Value::String(text))
        }
        value => Err(ResponsesTranslationError::UnsupportedInput(
            value_kind(value).to_string(),
        )),
    }
}

fn translate_function_call(call: &Map<String, Value>) -> Result<Value, ResponsesTranslationError> {
    let call_id = required_field_string(call, "call_id")?;
    let name = required_field_string(call, "name")?;
    let arguments = required_field_string(call, "arguments")?;
    Ok(json!({
        "id": call_id,
        "type": "function",
        "function": {"name": name, "arguments": arguments}
    }))
}

fn translate_custom_tool_call(
    call: &Map<String, Value>,
) -> Result<Value, ResponsesTranslationError> {
    let call_id = required_field_string(call, "call_id")?;
    let name = required_field_string(call, "name")?;
    let input = required_field_string(call, "input")?;
    let arguments = json!({"input": input}).to_string();
    Ok(json!({
        "id": call_id,
        "type": "function",
        "function": {"name": name, "arguments": arguments}
    }))
}

fn translate_tool_call_output(
    output: &Map<String, Value>,
) -> Result<Value, ResponsesTranslationError> {
    let call_id = required_field_string(output, "call_id")?;
    let content = output.get("output").ok_or_else(|| {
        ResponsesTranslationError::InvalidRequest("tool output missing output".to_string())
    })?;
    let content = translate_tool_output_content(content)?;
    Ok(json!({
        "role": "tool",
        "tool_call_id": call_id,
        "content": content
    }))
}

fn translate_tool_output_content(output: &Value) -> Result<String, ResponsesTranslationError> {
    match output {
        Value::String(text) => Ok(text.clone()),
        Value::Array(parts) => parts
            .iter()
            .map(|part| {
                let part = part.as_object().ok_or_else(|| {
                    ResponsesTranslationError::InvalidRequest(
                        "tool output content block must be an object".to_string(),
                    )
                })?;
                let part_type = required_field_string(part, "type")?;
                if !matches!(part_type, "input_text" | "output_text" | "text") {
                    return Err(ResponsesTranslationError::InvalidRequest(format!(
                        "unsupported tool output content type: {part_type}"
                    )));
                }
                required_field_string(part, "text").map(str::to_string)
            })
            .collect::<Result<Vec<_>, _>>()
            .map(|text| text.join("")),
        _ => Err(ResponsesTranslationError::InvalidRequest(
            "tool output must be a string or array".to_string(),
        )),
    }
}

fn translate_tools(
    tools: Value,
    tool_kinds: &mut BTreeMap<String, LocalToolKind>,
) -> Result<Vec<Value>, ResponsesTranslationError> {
    let tools = tools.as_array().ok_or_else(|| {
        ResponsesTranslationError::InvalidRequest("tools must be an array".to_string())
    })?;
    tools
        .iter()
        .map(|tool| translate_tool(tool, tool_kinds))
        .collect()
}

fn translate_tool(
    tool: &Value,
    tool_kinds: &mut BTreeMap<String, LocalToolKind>,
) -> Result<Value, ResponsesTranslationError> {
    let tool = tool.as_object().ok_or_else(|| {
        ResponsesTranslationError::InvalidRequest("tool must be an object".to_string())
    })?;
    let tool_type = required_field_string(tool, "type")?;
    match tool_type {
        "function" => {
            let name = required_field_string(tool, "name")?;
            let parameters = tool.get("parameters").ok_or_else(|| {
                ResponsesTranslationError::InvalidRequest(format!(
                    "function tool {name} missing parameters"
                ))
            })?;
            if !parameters.is_object() {
                return Err(ResponsesTranslationError::InvalidRequest(format!(
                    "function tool {name} parameters must be an object"
                )));
            }
            let mut function = Map::new();
            function.insert("name".to_string(), Value::String(name.to_string()));
            if let Some(description) = tool.get("description") {
                let description = required_string(description, "tool description")?;
                function.insert(
                    "description".to_string(),
                    Value::String(description.to_string()),
                );
            }
            function.insert("parameters".to_string(), parameters.clone());
            if let Some(strict) = tool.get("strict") {
                let strict = strict.as_bool().ok_or_else(|| {
                    ResponsesTranslationError::InvalidRequest(format!(
                        "function tool {name} strict must be a boolean"
                    ))
                })?;
                function.insert("strict".to_string(), Value::Bool(strict));
            }
            register_tool_kind(tool_kinds, name, LocalToolKind::Function)?;
            Ok(json!({"type": "function", "function": function}))
        }
        "custom" | "freeform" => {
            let name = required_field_string(tool, "name")?;
            let description = tool
                .get("description")
                .map(|value| required_string(value, "tool description"))
                .transpose()?
                .unwrap_or_default();
            register_tool_kind(tool_kinds, name, LocalToolKind::Custom)?;
            Ok(json!({
                "type": "function",
                "function": {
                    "name": name,
                    "description": description,
                    "parameters": {
                        "type": "object",
                        "properties": {"input": {"type": "string"}},
                        "required": ["input"],
                        "additionalProperties": false
                    }
                }
            }))
        }
        "web_search_preview" | "image_generation" | "computer_use_preview" => Err(
            ResponsesTranslationError::UnsupportedTool(tool_type.to_string()),
        ),
        unsupported => Err(ResponsesTranslationError::UnsupportedTool(
            unsupported.to_string(),
        )),
    }
}

fn register_tool_kind(
    tool_kinds: &mut BTreeMap<String, LocalToolKind>,
    name: &str,
    kind: LocalToolKind,
) -> Result<(), ResponsesTranslationError> {
    if tool_kinds.insert(name.to_string(), kind).is_some() {
        return Err(ResponsesTranslationError::InvalidRequest(format!(
            "duplicate tool name: {name}"
        )));
    }
    Ok(())
}

fn translate_tool_choice(
    choice: Value,
    tool_kinds: &BTreeMap<String, LocalToolKind>,
) -> Result<Value, ResponsesTranslationError> {
    match choice {
        Value::String(choice) if matches!(choice.as_str(), "auto" | "none" | "required") => {
            Ok(Value::String(choice))
        }
        Value::String(choice) => Err(ResponsesTranslationError::InvalidRequest(format!(
            "invalid tool_choice string: {choice}"
        ))),
        Value::Object(choice) => {
            let choice_type = required_field_string(&choice, "type")?;
            match choice_type {
                "function" | "custom" | "freeform" => {
                    let name = required_field_string(&choice, "name")?;
                    let expected_kind = if choice_type == "function" {
                        LocalToolKind::Function
                    } else {
                        LocalToolKind::Custom
                    };
                    match tool_kinds.get(name) {
                        None => {
                            return Err(ResponsesTranslationError::InvalidRequest(format!(
                                "tool_choice references unknown tool: {name}"
                            )));
                        }
                        Some(kind) if *kind != expected_kind => {
                            return Err(ResponsesTranslationError::InvalidRequest(format!(
                                "tool_choice kind mismatch for tool: {name}"
                            )));
                        }
                        Some(_) => {}
                    }
                    Ok(json!({"type": "function", "function": {"name": name}}))
                }
                unsupported => Err(ResponsesTranslationError::UnsupportedTool(
                    unsupported.to_string(),
                )),
            }
        }
        value => Err(ResponsesTranslationError::InvalidRequest(format!(
            "tool_choice must be a string or object, got {}",
            value_kind(&value)
        ))),
    }
}

fn required_field_string<'a>(
    object: &'a Map<String, Value>,
    field: &str,
) -> Result<&'a str, ResponsesTranslationError> {
    object
        .get(field)
        .ok_or_else(|| {
            ResponsesTranslationError::InvalidRequest(format!("missing required field {field}"))
        })
        .and_then(|value| required_string(value, field))
}

fn required_string<'a>(
    value: &'a Value,
    field: &str,
) -> Result<&'a str, ResponsesTranslationError> {
    value.as_str().ok_or_else(|| {
        ResponsesTranslationError::InvalidRequest(format!("{field} must be a string"))
    })
}

fn value_kind(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "boolean",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use serde_json::json;

    use super::{LocalToolKind, ResponsesTranslationError, chat_to_responses, responses_to_chat};

    #[test]
    fn chat_response_translates_text_function_call_and_usage() {
        let tool_kinds = BTreeMap::from([("get_weather".to_string(), LocalToolKind::Function)]);
        let response = chat_to_responses(
            &json!({
                "choices": [{
                    "message": {
                        "role": "assistant",
                        "content": "hello",
                        "tool_calls": [{
                            "id": "call_1",
                            "type": "function",
                            "function": {
                                "name": "get_weather",
                                "arguments": "{\"city\":\"NYC\"}"
                            }
                        }]
                    }
                }],
                "usage": {
                    "prompt_tokens": 7,
                    "completion_tokens": 3,
                    "total_tokens": 10
                }
            }),
            "resp_local_test",
            "qwen3-coder-30b-local",
            &tool_kinds,
        )
        .unwrap();

        assert_eq!(
            response,
            json!({
                "id": "resp_local_test",
                "object": "response",
                "created_at": response["created_at"],
                "status": "completed",
                "background": false,
                "error": null,
                "incomplete_details": null,
                "instructions": null,
                "max_output_tokens": null,
                "model": "qwen3-coder-30b-local",
                "output": [
                    {
                        "id": "msg_resp_local_test",
                        "type": "message",
                        "status": "completed",
                        "role": "assistant",
                        "content": [{
                            "type": "output_text",
                            "text": "hello",
                            "annotations": []
                        }]
                    },
                    {
                        "id": "fc_resp_local_test_0",
                        "type": "function_call",
                        "status": "completed",
                        "call_id": "call_1",
                        "name": "get_weather",
                        "arguments": "{\"city\":\"NYC\"}"
                    }
                ],
                "parallel_tool_calls": true,
                "previous_response_id": null,
                "reasoning": {"effort": null, "summary": null},
                "store": false,
                "temperature": null,
                "text": {"format": {"type": "text"}, "verbosity": "low"},
                "tool_choice": "auto",
                "tools": [],
                "top_p": null,
                "truncation": "disabled",
                "usage": {"input_tokens": 7, "output_tokens": 3, "total_tokens": 10}
            })
        );
    }

    #[test]
    fn chat_response_translates_custom_tool_argument_input() {
        let tool_kinds = BTreeMap::from([("shell".to_string(), LocalToolKind::Custom)]);
        let response = chat_to_responses(
            &json!({
                "choices": [{
                    "message": {
                        "content": null,
                        "tool_calls": [{
                            "id": "call_custom",
                            "type": "function",
                            "function": {"name": "shell", "arguments": "{\"input\":\"text\"}"}
                        }]
                    }
                }]
            }),
            "resp_local_custom",
            "qwen3-coder-30b-local",
            &tool_kinds,
        )
        .unwrap();

        assert_eq!(
            response["output"],
            json!([{
                "id": "ctc_resp_local_custom_0",
                "type": "custom_tool_call",
                "status": "completed",
                "call_id": "call_custom",
                "name": "shell",
                "input": "text"
            }])
        );
    }

    #[test]
    fn chat_response_rejects_malformed_shapes() {
        for malformed in [
            json!({}),
            json!({"choices": []}),
            json!({"choices": [{"message": "bad"}]}),
            json!({"choices": [{"message": {"tool_calls": [{"id": "call_1"}]}}]}),
            json!({"choices": [{"message": {}}], "usage": {"prompt_tokens": "seven"}}),
        ] {
            let error = chat_to_responses(
                &malformed,
                "resp_local_bad",
                "qwen3-coder-30b-local",
                &BTreeMap::new(),
            )
            .unwrap_err();

            assert!(matches!(
                error,
                ResponsesTranslationError::InvalidResponse(_)
            ));
        }
    }

    #[test]
    fn responses_request_translates_messages_function_calls_tools_and_options() {
        let translated = responses_to_chat(
            json!({
                "model": "qwen3-coder-30b-local",
                "instructions": "Be concise",
                "input": [
                    {
                        "type": "message",
                        "role": "user",
                        "content": [{"type": "input_text", "text": "weather"}]
                    },
                    {
                        "type": "function_call",
                        "call_id": "call_1",
                        "name": "get_weather",
                        "arguments": "{\"city\":\"NYC\"}"
                    },
                    {
                        "type": "function_call_output",
                        "call_id": "call_1",
                        "output": "sunny"
                    }
                ],
                "tools": [{
                    "type": "function",
                    "name": "get_weather",
                    "description": "Weather",
                    "parameters": {"type": "object"}
                }],
                "tool_choice": {"type": "function", "name": "get_weather"},
                "max_output_tokens": 64,
                "stream": false
            })
            .as_object()
            .unwrap()
            .clone(),
            r"models\Qwen3-Coder-30B-A3B-Instruct-IQ4_XS.gguf",
        )
        .unwrap();

        assert_eq!(
            translated.chat_body,
            json!({
                "model": r"models\Qwen3-Coder-30B-A3B-Instruct-IQ4_XS.gguf",
                "messages": [
                    {"role": "system", "content": "Be concise"},
                    {"role": "user", "content": "weather"},
                    {
                        "role": "assistant",
                        "tool_calls": [{
                            "id": "call_1",
                            "type": "function",
                            "function": {
                                "name": "get_weather",
                                "arguments": "{\"city\":\"NYC\"}"
                            }
                        }]
                    },
                    {"role": "tool", "tool_call_id": "call_1", "content": "sunny"}
                ],
                "tools": [{
                    "type": "function",
                    "function": {
                        "name": "get_weather",
                        "description": "Weather",
                        "parameters": {"type": "object"}
                    }
                }],
                "tool_choice": {"type": "function", "function": {"name": "get_weather"}},
                "max_tokens": 64,
                "stream": false
            })
            .as_object()
            .unwrap()
            .clone()
        );
    }

    #[test]
    fn responses_request_translates_custom_tools_calls_and_outputs() {
        let translated = responses_to_chat(
            json!({
                "model": "custom-local",
                "input": [
                    {
                        "type": "custom_tool_call",
                        "call_id": "call_custom",
                        "name": "shell",
                        "input": "pwd"
                    },
                    {
                        "type": "custom_tool_call_output",
                        "call_id": "call_custom",
                        "output": "/workspace"
                    }
                ],
                "tools": [{"type": "custom", "name": "shell"}],
                "tool_choice": {"type": "custom", "name": "shell"}
            })
            .as_object()
            .unwrap()
            .clone(),
            "custom.gguf",
        )
        .unwrap();

        assert_eq!(
            translated.chat_body,
            json!({
                "model": "custom.gguf",
                "messages": [
                    {
                        "role": "assistant",
                        "tool_calls": [{
                            "id": "call_custom",
                            "type": "function",
                            "function": {
                                "name": "shell",
                                "arguments": "{\"input\":\"pwd\"}"
                            }
                        }]
                    },
                    {
                        "role": "tool",
                        "tool_call_id": "call_custom",
                        "content": "/workspace"
                    }
                ],
                "tools": [{
                    "type": "function",
                    "function": {
                        "name": "shell",
                        "description": "",
                        "parameters": {
                            "type": "object",
                            "properties": {"input": {"type": "string"}},
                            "required": ["input"],
                            "additionalProperties": false
                        }
                    }
                }],
                "tool_choice": {"type": "function", "function": {"name": "shell"}}
            })
            .as_object()
            .unwrap()
            .clone()
        );
    }

    #[test]
    fn responses_request_rejects_hosted_tools_before_transport() {
        let result = responses_to_chat(
            json!({
                "model": "local",
                "input": "search",
                "tools": [{"type": "web_search_preview"}]
            })
            .as_object()
            .unwrap()
            .clone(),
            "local.gguf",
        );

        assert_eq!(
            result.unwrap_err(),
            ResponsesTranslationError::UnsupportedTool("web_search_preview".to_string())
        );
    }

    #[test]
    fn responses_request_reports_malformed_input_as_invalid_request() {
        let result = responses_to_chat(
            json!({"model": "local", "input": [{"content": "hello"}]})
                .as_object()
                .unwrap()
                .clone(),
            "local.gguf",
        );

        assert_eq!(
            result.unwrap_err(),
            ResponsesTranslationError::InvalidRequest("missing required field type".to_string())
        );
    }

    #[test]
    fn responses_request_rejects_malformed_top_level_input() {
        let result = responses_to_chat(
            json!({"model": "local", "input": {"unexpected": true}})
                .as_object()
                .unwrap()
                .clone(),
            "local.gguf",
        );

        assert_eq!(
            result.unwrap_err(),
            ResponsesTranslationError::InvalidRequest(
                "input must be a string or array".to_string()
            )
        );
    }

    #[test]
    fn responses_request_rejects_missing_input() {
        let result = responses_to_chat(
            json!({"model": "local"}).as_object().unwrap().clone(),
            "local.gguf",
        );

        assert_eq!(
            result.unwrap_err(),
            ResponsesTranslationError::InvalidRequest("missing input".to_string())
        );
    }

    #[test]
    fn responses_request_groups_consecutive_calls_before_ordered_outputs_and_messages() {
        let translated = responses_to_chat(
            json!({
                "model": "local",
                "input": [
                    {
                        "type": "function_call",
                        "call_id": "call_1",
                        "name": "first",
                        "arguments": "{}"
                    },
                    {
                        "type": "custom_tool_call",
                        "call_id": "call_2",
                        "name": "second",
                        "input": "run"
                    },
                    {
                        "type": "function_call_output",
                        "call_id": "call_1",
                        "output": "one"
                    },
                    {
                        "type": "custom_tool_call_output",
                        "call_id": "call_2",
                        "output": "two"
                    },
                    {
                        "type": "message",
                        "role": "user",
                        "content": [{"type": "input_text", "text": "next"}]
                    }
                ]
            })
            .as_object()
            .unwrap()
            .clone(),
            "local.gguf",
        )
        .unwrap();

        assert_eq!(
            translated.chat_body["messages"],
            json!([
                {
                    "role": "assistant",
                    "tool_calls": [
                        {
                            "id": "call_1",
                            "type": "function",
                            "function": {"name": "first", "arguments": "{}"}
                        },
                        {
                            "id": "call_2",
                            "type": "function",
                            "function": {
                                "name": "second",
                                "arguments": "{\"input\":\"run\"}"
                            }
                        }
                    ]
                },
                {"role": "tool", "tool_call_id": "call_1", "content": "one"},
                {"role": "tool", "tool_call_id": "call_2", "content": "two"},
                {"role": "user", "content": "next"}
            ])
        );
    }

    #[test]
    fn responses_request_rejects_duplicate_call_ids() {
        let result = responses_to_chat(
            json!({
                "model": "local",
                "input": [
                    {
                        "type": "function_call",
                        "call_id": "call_1",
                        "name": "first",
                        "arguments": "{}"
                    },
                    {
                        "type": "custom_tool_call",
                        "call_id": "call_1",
                        "name": "second",
                        "input": "run"
                    }
                ]
            })
            .as_object()
            .unwrap()
            .clone(),
            "local.gguf",
        );

        assert_eq!(
            result.unwrap_err(),
            ResponsesTranslationError::InvalidRequest("duplicate tool call id: call_1".to_string())
        );
    }

    #[test]
    fn responses_request_rejects_unmatched_tool_outputs() {
        let result = responses_to_chat(
            json!({
                "model": "local",
                "input": [{
                    "type": "function_call_output",
                    "call_id": "call_missing",
                    "output": "orphaned"
                }]
            })
            .as_object()
            .unwrap()
            .clone(),
            "local.gguf",
        );

        assert_eq!(
            result.unwrap_err(),
            ResponsesTranslationError::InvalidRequest(
                "unmatched tool output call id: call_missing".to_string()
            )
        );
    }

    #[test]
    fn responses_request_rejects_duplicate_tool_outputs() {
        let result = responses_to_chat(
            json!({
                "model": "local",
                "input": [
                    {
                        "type": "function_call",
                        "call_id": "call_1",
                        "name": "first",
                        "arguments": "{}"
                    },
                    {
                        "type": "function_call_output",
                        "call_id": "call_1",
                        "output": "one"
                    },
                    {
                        "type": "custom_tool_call_output",
                        "call_id": "call_1",
                        "output": "again"
                    }
                ]
            })
            .as_object()
            .unwrap()
            .clone(),
            "local.gguf",
        );

        assert_eq!(
            result.unwrap_err(),
            ResponsesTranslationError::InvalidRequest(
                "duplicate tool output call id: call_1".to_string()
            )
        );
    }

    #[test]
    fn responses_request_flattens_multipart_tool_outputs_in_order() {
        let translated = responses_to_chat(
            json!({
                "model": "local",
                "input": [
                    {
                        "type": "function_call",
                        "call_id": "call_1",
                        "name": "first",
                        "arguments": "{}"
                    },
                    {
                        "type": "function_call_output",
                        "call_id": "call_1",
                        "output": [
                            {"type": "input_text", "text": "one"},
                            {"type": "output_text", "text": " two"},
                            {"type": "text", "text": " three"}
                        ]
                    }
                ]
            })
            .as_object()
            .unwrap()
            .clone(),
            "local.gguf",
        )
        .unwrap();

        assert_eq!(
            translated.chat_body["messages"][1],
            json!({"role": "tool", "tool_call_id": "call_1", "content": "one two three"})
        );
    }

    #[test]
    fn responses_request_rejects_unsupported_tool_output_blocks() {
        let result = responses_to_chat(
            json!({
                "model": "local",
                "input": [
                    {
                        "type": "function_call",
                        "call_id": "call_1",
                        "name": "first",
                        "arguments": "{}"
                    },
                    {
                        "type": "function_call_output",
                        "call_id": "call_1",
                        "output": [{"type": "input_image", "image_url": "data:image/png"}]
                    }
                ]
            })
            .as_object()
            .unwrap()
            .clone(),
            "local.gguf",
        );

        assert_eq!(
            result.unwrap_err(),
            ResponsesTranslationError::InvalidRequest(
                "unsupported tool output content type: input_image".to_string()
            )
        );
    }

    #[test]
    fn responses_request_rejects_duplicate_tool_names_across_kinds() {
        let result = responses_to_chat(
            json!({
                "model": "local",
                "input": "hello",
                "tools": [
                    {
                        "type": "function",
                        "name": "shared",
                        "parameters": {"type": "object"}
                    },
                    {"type": "custom", "name": "shared"}
                ]
            })
            .as_object()
            .unwrap()
            .clone(),
            "local.gguf",
        );

        assert_eq!(
            result.unwrap_err(),
            ResponsesTranslationError::InvalidRequest("duplicate tool name: shared".to_string())
        );
    }

    #[test]
    fn responses_request_rejects_unknown_named_tool_choice() {
        let result = responses_to_chat(
            json!({
                "model": "local",
                "input": "hello",
                "tools": [{
                    "type": "function",
                    "name": "known",
                    "parameters": {"type": "object"}
                }],
                "tool_choice": {"type": "function", "name": "missing"}
            })
            .as_object()
            .unwrap()
            .clone(),
            "local.gguf",
        );

        assert_eq!(
            result.unwrap_err(),
            ResponsesTranslationError::InvalidRequest(
                "tool_choice references unknown tool: missing".to_string()
            )
        );
    }

    #[test]
    fn responses_request_rejects_named_tool_choice_kind_mismatch() {
        let result = responses_to_chat(
            json!({
                "model": "local",
                "input": "hello",
                "tools": [{
                    "type": "function",
                    "name": "known",
                    "parameters": {"type": "object"}
                }],
                "tool_choice": {"type": "custom", "name": "known"}
            })
            .as_object()
            .unwrap()
            .clone(),
            "local.gguf",
        );

        assert_eq!(
            result.unwrap_err(),
            ResponsesTranslationError::InvalidRequest(
                "tool_choice kind mismatch for tool: known".to_string()
            )
        );
    }

    #[test]
    fn responses_request_copies_boolean_function_tool_strict() {
        let translated = responses_to_chat(
            json!({
                "model": "local",
                "input": "hello",
                "tools": [{
                    "type": "function",
                    "name": "known",
                    "parameters": {"type": "object"},
                    "strict": true
                }]
            })
            .as_object()
            .unwrap()
            .clone(),
            "local.gguf",
        )
        .unwrap();

        assert_eq!(translated.chat_body["tools"][0]["function"]["strict"], true);
    }

    #[test]
    fn responses_request_rejects_non_boolean_function_tool_strict() {
        let result = responses_to_chat(
            json!({
                "model": "local",
                "input": "hello",
                "tools": [{
                    "type": "function",
                    "name": "known",
                    "parameters": {"type": "object"},
                    "strict": "true"
                }]
            })
            .as_object()
            .unwrap()
            .clone(),
            "local.gguf",
        );

        assert_eq!(
            result.unwrap_err(),
            ResponsesTranslationError::InvalidRequest(
                "function tool known strict must be a boolean".to_string()
            )
        );
    }

    #[test]
    fn responses_request_rejects_arbitrary_string_tool_choice() {
        let result = responses_to_chat(
            json!({
                "model": "local",
                "input": "hello",
                "tool_choice": "sometimes"
            })
            .as_object()
            .unwrap()
            .clone(),
            "local.gguf",
        );

        assert_eq!(
            result.unwrap_err(),
            ResponsesTranslationError::InvalidRequest(
                "invalid tool_choice string: sometimes".to_string()
            )
        );
    }

    #[test]
    fn responses_request_rejects_message_interrupting_tool_call_group() {
        let result = responses_to_chat(
            json!({
                "model": "local",
                "input": [
                    {
                        "type": "function_call",
                        "call_id": "call_1",
                        "name": "first",
                        "arguments": "{}"
                    },
                    {
                        "type": "message",
                        "role": "user",
                        "content": [{"type": "input_text", "text": "interrupt"}]
                    }
                ]
            })
            .as_object()
            .unwrap()
            .clone(),
            "local.gguf",
        );

        assert_eq!(
            result.unwrap_err(),
            ResponsesTranslationError::InvalidRequest(
                "tool call group missing outputs before message: call_1".to_string()
            )
        );
    }

    #[test]
    fn responses_request_rejects_unanswered_tool_calls_at_end_of_input() {
        let result = responses_to_chat(
            json!({
                "model": "local",
                "input": [{
                    "type": "function_call",
                    "call_id": "call_1",
                    "name": "first",
                    "arguments": "{}"
                }]
            })
            .as_object()
            .unwrap()
            .clone(),
            "local.gguf",
        );

        assert_eq!(
            result.unwrap_err(),
            ResponsesTranslationError::InvalidRequest(
                "tool call group missing outputs at end of input: call_1".to_string()
            )
        );
    }

    #[test]
    fn responses_request_accepts_multi_call_outputs_in_any_order() {
        let translated = responses_to_chat(
            json!({
                "model": "local",
                "input": [
                    {
                        "type": "function_call",
                        "call_id": "call_1",
                        "name": "first",
                        "arguments": "{}"
                    },
                    {
                        "type": "custom_tool_call",
                        "call_id": "call_2",
                        "name": "second",
                        "input": "run"
                    },
                    {
                        "type": "custom_tool_call_output",
                        "call_id": "call_2",
                        "output": "two"
                    },
                    {
                        "type": "function_call_output",
                        "call_id": "call_1",
                        "output": "one"
                    }
                ]
            })
            .as_object()
            .unwrap()
            .clone(),
            "local.gguf",
        )
        .unwrap();

        assert_eq!(
            translated.chat_body["messages"],
            json!([
                {
                    "role": "assistant",
                    "tool_calls": [
                        {
                            "id": "call_1",
                            "type": "function",
                            "function": {"name": "first", "arguments": "{}"}
                        },
                        {
                            "id": "call_2",
                            "type": "function",
                            "function": {
                                "name": "second",
                                "arguments": "{\"input\":\"run\"}"
                            }
                        }
                    ]
                },
                {"role": "tool", "tool_call_id": "call_2", "content": "two"},
                {"role": "tool", "tool_call_id": "call_1", "content": "one"}
            ])
        );
    }
}
