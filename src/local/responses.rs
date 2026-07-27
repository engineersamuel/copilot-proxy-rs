use std::collections::BTreeMap;

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
    messages.extend(
        input_items
            .iter()
            .map(input_item_to_chat_message)
            .collect::<Result<Vec<_>, _>>()?,
    );

    let mut tool_kinds = BTreeMap::new();
    let tools = body
        .remove("tools")
        .map(|tools| translate_tools(tools, &mut tool_kinds))
        .transpose()?;
    let tool_choice = body
        .remove("tool_choice")
        .map(translate_tool_choice)
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

fn normalize_input(input: Option<Value>) -> Result<Vec<Value>, ResponsesTranslationError> {
    match input {
        Some(Value::String(text)) => Ok(vec![json!({
            "type": "message",
            "role": "user",
            "content": [{"type": "input_text", "text": text}]
        })]),
        Some(Value::Array(items)) => Ok(items),
        Some(value) => Err(ResponsesTranslationError::UnsupportedInput(
            value_kind(&value).to_string(),
        )),
        None => Err(ResponsesTranslationError::InvalidRequest(
            "missing input".to_string(),
        )),
    }
}

fn input_item_to_chat_message(item: &Value) -> Result<Value, ResponsesTranslationError> {
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
    match item_type {
        "message" => translate_message(object),
        "function_call" => translate_function_call(object),
        "function_call_output" | "custom_tool_call_output" => translate_tool_call_output(object),
        "custom_tool_call" => translate_custom_tool_call(object),
        unsupported => Err(ResponsesTranslationError::UnsupportedInput(
            unsupported.to_string(),
        )),
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
        "role": "assistant",
        "tool_calls": [{
            "id": call_id,
            "type": "function",
            "function": {"name": name, "arguments": arguments}
        }]
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
        "role": "assistant",
        "tool_calls": [{
            "id": call_id,
            "type": "function",
            "function": {"name": name, "arguments": arguments}
        }]
    }))
}

fn translate_tool_call_output(
    output: &Map<String, Value>,
) -> Result<Value, ResponsesTranslationError> {
    let call_id = required_field_string(output, "call_id")?;
    let content = output.get("output").ok_or_else(|| {
        ResponsesTranslationError::InvalidRequest("tool output missing output".to_string())
    })?;
    let content = match content {
        Value::String(text) => text.clone(),
        value => value.to_string(),
    };
    Ok(json!({
        "role": "tool",
        "tool_call_id": call_id,
        "content": content
    }))
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
            tool_kinds.insert(name.to_string(), LocalToolKind::Function);
            Ok(json!({"type": "function", "function": function}))
        }
        "custom" | "freeform" => {
            let name = required_field_string(tool, "name")?;
            let description = tool
                .get("description")
                .map(|value| required_string(value, "tool description"))
                .transpose()?
                .unwrap_or_default();
            tool_kinds.insert(name.to_string(), LocalToolKind::Custom);
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

fn translate_tool_choice(choice: Value) -> Result<Value, ResponsesTranslationError> {
    match choice {
        Value::String(choice) => Ok(Value::String(choice)),
        Value::Object(choice) => {
            let choice_type = required_field_string(&choice, "type")?;
            match choice_type {
                "function" | "custom" | "freeform" => {
                    let name = required_field_string(&choice, "name")?;
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
    use serde_json::json;

    use super::{ResponsesTranslationError, responses_to_chat};

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
}
