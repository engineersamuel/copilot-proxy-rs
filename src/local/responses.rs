use std::collections::{BTreeMap, BTreeSet};
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::{Map, Value, json};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LocalToolKind {
    Function,
    Custom,
}

impl LocalToolKind {
    fn stream_item_id(self, response_id: &str, tool_index: usize) -> String {
        let prefix = match self {
            Self::Function => "fc",
            Self::Custom => "ctc",
        };
        format!("{prefix}_{response_id}_{tool_index}")
    }

    fn stream_delta_event(self) -> &'static str {
        match self {
            Self::Function => "response.function_call_arguments.delta",
            Self::Custom => "response.custom_tool_call_input.delta",
        }
    }

    fn stream_done_event(self) -> &'static str {
        match self {
            Self::Function => "response.function_call_arguments.done",
            Self::Custom => "response.custom_tool_call_input.done",
        }
    }
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

#[derive(Debug, Clone)]
struct StreamingToolCall {
    tool_index: usize,
    output_index: usize,
    call_id: Option<String>,
    name: Option<String>,
    arguments: String,
    kind: Option<LocalToolKind>,
    type_seen: bool,
    added: bool,
    done: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StreamStatus {
    InProgress,
    Completed,
    Incomplete,
    Failed,
}

impl StreamStatus {
    fn as_str(self) -> &'static str {
        match self {
            Self::InProgress => "in_progress",
            Self::Completed => "completed",
            Self::Incomplete => "incomplete",
            Self::Failed => "failed",
        }
    }
}

#[derive(Debug)]
pub struct ChatToResponsesStream {
    response_id: String,
    public_model: String,
    previous_response_id: Option<String>,
    created_at: u64,
    tool_kinds: BTreeMap<String, LocalToolKind>,
    text: String,
    text_output_index: Option<usize>,
    text_done: bool,
    tool_calls: BTreeMap<usize, StreamingToolCall>,
    next_output_index: usize,
    usage: Value,
    status: StreamStatus,
    incomplete_details: Value,
    started: bool,
    finish_seen: bool,
    terminal: bool,
}

impl ChatToResponsesStream {
    pub fn new(
        response_id: String,
        public_model: String,
        tool_kinds: BTreeMap<String, LocalToolKind>,
    ) -> Self {
        Self::new_with_previous_response_id(response_id, public_model, None, tool_kinds)
    }

    pub fn new_with_previous_response_id(
        response_id: String,
        public_model: String,
        previous_response_id: Option<String>,
        tool_kinds: BTreeMap<String, LocalToolKind>,
    ) -> Self {
        Self {
            response_id,
            public_model,
            previous_response_id,
            created_at: current_epoch_seconds(),
            tool_kinds,
            text: String::new(),
            text_output_index: None,
            text_done: false,
            tool_calls: BTreeMap::new(),
            next_output_index: 0,
            usage: Value::Null,
            status: StreamStatus::InProgress,
            incomplete_details: Value::Null,
            started: false,
            finish_seen: false,
            terminal: false,
        }
    }

    pub fn map_line(&mut self, line: &str) -> Vec<String> {
        let Some(data) = line.strip_prefix("data:").map(str::trim_start) else {
            return Vec::new();
        };
        if self.terminal {
            return Vec::new();
        }
        if data == "[DONE]" {
            if !self.finish_seen {
                return self.failed_event();
            }
            let mut events = Vec::new();
            match self.finish_events() {
                Ok(finish_events) => events.extend(finish_events),
                Err(()) => return self.failed_event(),
            }
            events.extend(self.complete_event());
            return events;
        }

        let chunk: Value = match serde_json::from_str(data) {
            Ok(chunk) => chunk,
            Err(_) => return self.failed_event(),
        };
        let Some(chunk) = chunk.as_object() else {
            return self.failed_event();
        };
        if self.finish_seen {
            return self.map_post_finish_chunk(chunk);
        }
        let mut events = self.start_events();

        if let Some(usage) = chunk.get("usage") {
            match translate_chat_usage(Some(usage)) {
                Ok(usage) => self.usage = usage,
                Err(_) => return self.failed_event(),
            }
        }

        if let Some(choices) = chunk.get("choices") {
            let Some(choices) = choices.as_array() else {
                return self.failed_event();
            };
            if let Some(choice) = choices.first() {
                let Some(choice) = choice.as_object() else {
                    return self.failed_event();
                };
                if let Some(delta) = choice.get("delta") {
                    let Some(delta) = delta.as_object() else {
                        return self.failed_event();
                    };
                    if let Some(content) = delta.get("content") {
                        match content {
                            Value::String(content) if !content.is_empty() => {
                                events.extend(self.text_delta_events(content));
                            }
                            Value::String(_) | Value::Null => {}
                            _ => return self.failed_event(),
                        }
                    }
                    if let Some(tool_calls) = delta.get("tool_calls") {
                        let Some(tool_calls) = tool_calls.as_array() else {
                            return self.failed_event();
                        };
                        for call in tool_calls {
                            match self.tool_delta_events(call) {
                                Ok(call_events) => events.extend(call_events),
                                Err(()) => return self.failed_event(),
                            }
                        }
                    }
                }
                if let Some(finish_reason) = choice.get("finish_reason") {
                    if !finish_reason.is_null() {
                        let Ok((status, incomplete_details)) =
                            translate_finish_reason(Some(finish_reason))
                        else {
                            return self.failed_event();
                        };
                        self.status = match status {
                            "completed" => StreamStatus::Completed,
                            "incomplete" => StreamStatus::Incomplete,
                            _ => return self.failed_event(),
                        };
                        self.incomplete_details = incomplete_details;
                        self.finish_seen = true;
                        match self.finish_events() {
                            Ok(finish_events) => events.extend(finish_events),
                            Err(()) => return self.failed_event(),
                        }
                    }
                }
            }
        } else if !chunk.contains_key("usage") {
            return self.failed_event();
        }

        if self.finish_seen && !self.usage.is_null() {
            events.extend(self.complete_event());
        }
        events
    }

    fn map_post_finish_chunk(&mut self, chunk: &Map<String, Value>) -> Vec<String> {
        let choices_are_empty = match chunk.get("choices") {
            None => true,
            Some(Value::Array(choices)) => choices.is_empty(),
            Some(_) => false,
        };
        if !choices_are_empty || !chunk.contains_key("usage") {
            return self.failed_event();
        }
        match translate_chat_usage(chunk.get("usage")) {
            Ok(usage) => self.usage = usage,
            Err(_) => return self.failed_event(),
        }
        if self.usage.is_null() {
            Vec::new()
        } else {
            self.complete_event()
        }
    }

    pub fn output_items(&self) -> Vec<Value> {
        let mut output = Vec::new();
        if let Some(index) = self.text_output_index {
            output.push((index, self.text_item()));
        }
        output.extend(self.tool_calls.values().filter_map(|call| {
            streaming_tool_item(&self.response_id, call).map(|item| (call.output_index, item))
        }));
        output.sort_by_key(|(index, _)| *index);
        output.into_iter().map(|(_, item)| item).collect()
    }

    pub fn is_completed(&self) -> bool {
        self.terminal && self.status == StreamStatus::Completed
    }

    pub fn completed_response(&self) -> Value {
        self.response_value(self.status.as_str())
    }

    pub fn fail(&mut self) -> Vec<String> {
        self.failed_event()
    }

    fn start_events(&mut self) -> Vec<String> {
        if self.started {
            return Vec::new();
        }
        self.started = true;
        let response = self.response_value("in_progress");
        vec![
            stream_event("response.created", json!({"response": response})),
            stream_event(
                "response.in_progress",
                json!({"response": self.response_value("in_progress")}),
            ),
        ]
    }

    fn text_delta_events(&mut self, delta: &str) -> Vec<String> {
        let output_index = *self.text_output_index.get_or_insert_with(|| {
            let index = self.next_output_index;
            self.next_output_index += 1;
            index
        });
        let first = self.text.is_empty();
        self.text.push_str(delta);
        let mut events = Vec::new();
        if first {
            events.push(stream_event(
                "response.output_item.added",
                json!({
                    "output_index": output_index,
                    "item": {
                        "id": format!("msg_{}", self.response_id),
                        "type": "message",
                        "status": "in_progress",
                        "role": "assistant",
                        "content": []
                    }
                }),
            ));
            events.push(stream_event(
                "response.content_part.added",
                json!({
                    "item_id": format!("msg_{}", self.response_id),
                    "output_index": output_index,
                    "content_index": 0,
                    "part": {"type": "output_text", "text": "", "annotations": []}
                }),
            ));
        }
        events.push(stream_event(
            "response.output_text.delta",
            json!({
                "item_id": format!("msg_{}", self.response_id),
                "output_index": output_index,
                "content_index": 0,
                "delta": delta
            }),
        ));
        events
    }

    fn tool_delta_events(&mut self, value: &Value) -> Result<Vec<String>, ()> {
        let fragment = value.as_object().ok_or(())?;
        let index = fragment
            .get("index")
            .and_then(Value::as_u64)
            .and_then(|index| usize::try_from(index).ok())
            .ok_or(())?;
        let call_type = match fragment.get("type") {
            None => None,
            Some(Value::String(call_type)) if call_type == "function" => Some(()),
            Some(_) => return Err(()),
        };
        let call_id = match fragment.get("id") {
            None => None,
            Some(Value::String(call_id)) if !call_id.is_empty() => Some(call_id.as_str()),
            Some(_) => return Err(()),
        };
        let (name, arguments_delta) = match fragment.get("function") {
            None => (None, None),
            Some(Value::Object(function)) => {
                let name = match function.get("name") {
                    None => None,
                    Some(Value::String(name)) if !name.is_empty() => Some(name.as_str()),
                    Some(_) => return Err(()),
                };
                let arguments = match function.get("arguments") {
                    None => None,
                    Some(Value::String(arguments)) => Some(arguments.as_str()),
                    Some(_) => return Err(()),
                };
                (name, arguments)
            }
            Some(_) => return Err(()),
        };
        let kind = name
            .map(|name| self.tool_kinds.get(name).copied().ok_or(()))
            .transpose()?;
        if !self.tool_calls.contains_key(&index) {
            if index != self.tool_calls.len() {
                return Err(());
            }
            let output_index = self.next_output_index;
            self.next_output_index += 1;
            self.tool_calls.insert(
                index,
                StreamingToolCall {
                    tool_index: index,
                    output_index,
                    call_id: None,
                    name: None,
                    arguments: String::new(),
                    kind: None,
                    type_seen: false,
                    added: false,
                    done: false,
                },
            );
        }

        let call = self.tool_calls.get_mut(&index).ok_or(())?;
        if call_type.is_some() {
            if call.type_seen {
                return Err(());
            }
            call.type_seen = true;
        }
        if let Some(call_id) = call_id {
            if call.call_id.is_some() {
                return Err(());
            }
            call.call_id = Some(call_id.to_string());
        }
        if let Some(name) = name {
            if call.name.is_some() {
                return Err(());
            }
            call.name = Some(name.to_string());
            call.kind = kind;
        }
        if let Some(arguments_delta) = arguments_delta {
            call.arguments.push_str(arguments_delta);
        }

        let mut events = Vec::new();
        let metadata = call
            .call_id
            .clone()
            .zip(call.name.clone())
            .zip(call.kind)
            .map(|((call_id, name), kind)| (call_id, name, kind));
        if !call.added && metadata.is_some() {
            call.added = true;
            let (call_id, name, kind) = metadata.ok_or(())?;
            let item_id = kind.stream_item_id(&self.response_id, call.tool_index);
            let item = match kind {
                LocalToolKind::Function => json!({
                    "id": item_id,
                    "type": "function_call",
                    "status": "in_progress",
                    "call_id": call_id,
                    "name": name,
                    "arguments": ""
                }),
                LocalToolKind::Custom => json!({
                    "id": item_id,
                    "type": "custom_tool_call",
                    "status": "in_progress",
                    "call_id": call_id,
                    "name": name,
                    "input": ""
                }),
            };
            events.push(stream_event(
                "response.output_item.added",
                json!({"output_index": call.output_index, "item": item}),
            ));
            if !call.arguments.is_empty() {
                events.push(stream_event(
                    kind.stream_delta_event(),
                    json!({
                        "item_id": item_id,
                        "output_index": call.output_index,
                        "delta": call.arguments
                    }),
                ));
            }
        } else if call.added && arguments_delta.is_some_and(|delta| !delta.is_empty()) {
            let kind = call.kind.ok_or(())?;
            events.push(stream_event(
                kind.stream_delta_event(),
                json!({
                    "item_id": kind.stream_item_id(&self.response_id, call.tool_index),
                    "output_index": call.output_index,
                    "delta": arguments_delta
                }),
            ));
        }
        Ok(events)
    }

    fn finish_events(&mut self) -> Result<Vec<String>, ()> {
        self.validate_tool_calls()?;
        let mut events = Vec::new();
        if let Some(output_index) = self.text_output_index {
            if !self.text_done {
                self.text_done = true;
                let item_id = format!("msg_{}", self.response_id);
                events.push(stream_event(
                    "response.output_text.done",
                    json!({
                        "item_id": item_id,
                        "output_index": output_index,
                        "content_index": 0,
                        "text": self.text
                    }),
                ));
                events.push(stream_event(
                    "response.content_part.done",
                    json!({
                        "item_id": item_id,
                        "output_index": output_index,
                        "content_index": 0,
                        "part": {"type": "output_text", "text": self.text, "annotations": []}
                    }),
                ));
                events.push(stream_event(
                    "response.output_item.done",
                    json!({"output_index": output_index, "item": self.text_item()}),
                ));
            }
        }
        for call in self.tool_calls.values_mut() {
            if call.done {
                continue;
            }
            let kind = call.kind.ok_or(())?;
            let custom_input = match kind {
                LocalToolKind::Function => String::new(),
                LocalToolKind::Custom => custom_tool_input(&call.arguments).ok_or(())?,
            };
            call.done = true;
            let item_id = kind.stream_item_id(&self.response_id, call.tool_index);
            let done_value = match kind {
                LocalToolKind::Function => json!({
                    "item_id": item_id,
                    "output_index": call.output_index,
                    "arguments": call.arguments
                }),
                LocalToolKind::Custom => json!({
                    "item_id": item_id,
                    "output_index": call.output_index,
                    "input": custom_input
                }),
            };
            events.push(stream_event(kind.stream_done_event(), done_value));
            events.push(stream_event(
                "response.output_item.done",
                json!({
                    "output_index": call.output_index,
                    "item": streaming_tool_item(&self.response_id, call).ok_or(())?
                }),
            ));
        }
        Ok(events)
    }

    fn validate_tool_calls(&self) -> Result<(), ()> {
        let mut call_ids = BTreeSet::new();
        let mut previous_output_index = None;
        for (expected_index, (index, call)) in self.tool_calls.iter().enumerate() {
            if *index != expected_index || call.tool_index != expected_index || !call.added {
                return Err(());
            }
            if previous_output_index.is_some_and(|previous| call.output_index <= previous) {
                return Err(());
            }
            previous_output_index = Some(call.output_index);

            let call_id = call
                .call_id
                .as_deref()
                .filter(|id| !id.is_empty())
                .ok_or(())?;
            let name = call
                .name
                .as_deref()
                .filter(|name| !name.is_empty())
                .ok_or(())?;
            let kind = call.kind.ok_or(())?;
            if self.tool_kinds.get(name).copied() != Some(kind) || !call_ids.insert(call_id) {
                return Err(());
            }
        }
        Ok(())
    }

    fn complete_event(&mut self) -> Vec<String> {
        if self.terminal {
            return Vec::new();
        }
        self.terminal = true;
        if self.status == StreamStatus::InProgress {
            self.status = StreamStatus::Completed;
        }
        let event_type = if self.status == StreamStatus::Incomplete {
            "response.incomplete"
        } else {
            "response.completed"
        };
        vec![stream_event(
            event_type,
            json!({"response": self.response_value(self.status.as_str())}),
        )]
    }

    fn failed_event(&mut self) -> Vec<String> {
        if self.terminal {
            return Vec::new();
        }
        self.terminal = true;
        self.status = StreamStatus::Failed;
        vec![stream_event(
            "response.failed",
            json!({"response": self.response_value("failed")}),
        )]
    }

    fn response_value(&self, status: &str) -> Value {
        let error = if status == "failed" {
            json!({"code": "invalid_response", "message": "local model returned invalid response"})
        } else {
            Value::Null
        };
        json!({
            "id": self.response_id,
            "object": "response",
            "created_at": self.created_at,
            "status": status,
            "background": false,
            "error": error,
            "incomplete_details": self.incomplete_details,
            "instructions": null,
            "max_output_tokens": null,
            "model": self.public_model,
            "output": self.output_items(),
            "parallel_tool_calls": true,
            "previous_response_id": self.previous_response_id,
            "reasoning": {"effort": null, "summary": null},
            "store": false,
            "temperature": null,
            "text": {"format": {"type": "text"}, "verbosity": "low"},
            "tool_choice": "auto",
            "tools": [],
            "top_p": null,
            "truncation": "disabled",
            "usage": self.usage
        })
    }

    fn text_item(&self) -> Value {
        json!({
            "id": format!("msg_{}", self.response_id),
            "type": "message",
            "status": if self.text_done { "completed" } else { "in_progress" },
            "role": "assistant",
            "content": [{
                "type": "output_text",
                "text": self.text,
                "annotations": []
            }]
        })
    }
}

fn streaming_tool_item(response_id: &str, call: &StreamingToolCall) -> Option<Value> {
    let call_id = call.call_id.as_deref()?;
    let name = call.name.as_deref()?;
    let kind = call.kind?;
    Some(match kind {
        LocalToolKind::Function => json!({
            "id": kind.stream_item_id(response_id, call.tool_index),
            "type": "function_call",
            "status": if call.done { "completed" } else { "in_progress" },
            "call_id": call_id,
            "name": name,
            "arguments": call.arguments
        }),
        LocalToolKind::Custom => json!({
            "id": kind.stream_item_id(response_id, call.tool_index),
            "type": "custom_tool_call",
            "status": if call.done { "completed" } else { "in_progress" },
            "call_id": call_id,
            "name": name,
            "input": custom_tool_input(&call.arguments).unwrap_or_default()
        }),
    })
}

fn custom_tool_input(arguments: &str) -> Option<String> {
    serde_json::from_str::<Value>(arguments)
        .ok()?
        .get("input")?
        .as_str()
        .map(str::to_string)
}

fn stream_event(event_type: &str, mut data: Value) -> String {
    if let Some(data) = data.as_object_mut() {
        data.insert("type".to_string(), Value::String(event_type.to_string()));
    }
    format!("event: {event_type}\ndata: {data}")
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
    let choice = chat
        .get("choices")
        .and_then(Value::as_array)
        .and_then(|choices| choices.first())
        .and_then(Value::as_object)
        .ok_or_else(|| invalid_response("choices[0] must be an object"))?;
    let (status, incomplete_details) = translate_finish_reason(choice.get("finish_reason"))?;
    let message = choice
        .get("message")
        .and_then(Value::as_object)
        .ok_or_else(|| invalid_response("choices[0].message must be an object"))?;
    if let Some(role) = message.get("role") {
        if role.as_str() != Some("assistant") {
            return Err(invalid_response(
                "choices[0].message.role must be assistant",
            ));
        }
    }
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
        validate_unique_tool_call_ids(calls)?;
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
        "status": status,
        "background": false,
        "error": null,
        "incomplete_details": incomplete_details,
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

fn translate_finish_reason(
    finish_reason: Option<&Value>,
) -> Result<(&'static str, Value), ResponsesTranslationError> {
    match finish_reason.and_then(Value::as_str) {
        Some("stop" | "tool_calls") => Ok(("completed", Value::Null)),
        Some("length") => Ok(("incomplete", json!({"reason": "max_output_tokens"}))),
        Some("content_filter") => Ok(("incomplete", json!({"reason": "content_filter"}))),
        Some(reason) => Err(invalid_response(format!(
            "unsupported choices[0].finish_reason {reason}"
        ))),
        None => Err(invalid_response(
            "choices[0].finish_reason must be a string",
        )),
    }
}

fn validate_unique_tool_call_ids(calls: &[Value]) -> Result<(), ResponsesTranslationError> {
    let mut call_ids = BTreeSet::new();
    for call in calls {
        let object = call
            .as_object()
            .ok_or_else(|| invalid_response("tool call must be an object"))?;
        let call_id = response_field_string(object, "id", "tool call")?;
        if !call_ids.insert(call_id) {
            return Err(invalid_response(format!(
                "duplicate tool call id {call_id}"
            )));
        }
    }
    Ok(())
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

    use serde_json::{Value, json};

    use super::{
        ChatToResponsesStream, LocalToolKind, ResponsesTranslationError, chat_to_responses,
        responses_to_chat,
    };

    #[test]
    fn chat_sse_translates_text_lifecycle_and_usage() {
        let mut adapter = ChatToResponsesStream::new(
            "resp_local_stream".to_string(),
            "qwen3-coder-30b-local".to_string(),
            BTreeMap::new(),
        );
        let lines = [
            r#"data: {"id":"chatcmpl-1","choices":[{"index":0,"delta":{"role":"assistant","content":"Hel"},"finish_reason":null}]}"#,
            r#"data: {"id":"chatcmpl-1","choices":[{"index":0,"delta":{"content":"lo"},"finish_reason":null}]}"#,
            r#"data: {"id":"chatcmpl-1","choices":[{"index":0,"delta":{},"finish_reason":"stop"}],"usage":{"prompt_tokens":2,"completion_tokens":1,"total_tokens":3}}"#,
            "data: [DONE]",
        ];

        let events = lines
            .into_iter()
            .flat_map(|line| adapter.map_line(line))
            .map(|event| {
                let data = event
                    .lines()
                    .find_map(|line| line.strip_prefix("data: "))
                    .unwrap();
                serde_json::from_str::<Value>(data).unwrap()
            })
            .collect::<Vec<_>>();
        let event_types = events
            .iter()
            .map(|event| event["type"].as_str().unwrap())
            .collect::<Vec<_>>();

        assert_eq!(
            event_types,
            [
                "response.created",
                "response.in_progress",
                "response.output_item.added",
                "response.content_part.added",
                "response.output_text.delta",
                "response.output_text.delta",
                "response.output_text.done",
                "response.content_part.done",
                "response.output_item.done",
                "response.completed",
            ]
        );
        assert_eq!(
            adapter.completed_response()["model"],
            "qwen3-coder-30b-local"
        );
        assert_eq!(adapter.completed_response()["usage"]["input_tokens"], 2);
    }

    #[test]
    fn chat_sse_translates_function_and_custom_tool_lifecycles_once() {
        let tool_kinds = BTreeMap::from([
            ("calculate".to_string(), LocalToolKind::Function),
            ("shell".to_string(), LocalToolKind::Custom),
        ]);
        let mut adapter = ChatToResponsesStream::new(
            "resp_local_tools".to_string(),
            "qwen3-coder-30b-local".to_string(),
            tool_kinds,
        );
        let chunks = [
            json!({
                "choices": [{"delta": {"tool_calls": [
                    {"index": 0, "id": "call_function", "function": {
                        "name": "calculate", "arguments": "{\"x\":"
                    }},
                    {"index": 1, "id": "call_custom", "function": {
                        "name": "shell", "arguments": "{\"input\":\"te"
                    }}
                ]}}]
            }),
            json!({
                "choices": [{"delta": {"tool_calls": [
                    {"index": 0, "function": {"arguments": "1}"}},
                    {"index": 1, "function": {"arguments": "xt\"}"}}
                ]}}]
            }),
            json!({"choices": [{"delta": {}, "finish_reason": "tool_calls"}]}),
        ];
        let mut events = chunks
            .into_iter()
            .flat_map(|chunk| adapter.map_line(&format!("data: {chunk}")))
            .collect::<Vec<_>>();
        events.extend(adapter.map_line("data: [DONE]"));
        let events = events
            .iter()
            .map(|event| {
                let data = event
                    .lines()
                    .find_map(|line| line.strip_prefix("data: "))
                    .unwrap();
                serde_json::from_str::<Value>(data).unwrap()
            })
            .collect::<Vec<_>>();

        assert_eq!(
            events
                .iter()
                .filter(|event| event["type"] == "response.output_item.added")
                .count(),
            2
        );
        assert_eq!(
            events
                .iter()
                .filter(|event| event["type"] == "response.output_item.done")
                .count(),
            2
        );
        let function_done = events
            .iter()
            .find(|event| event["type"] == "response.function_call_arguments.done")
            .unwrap();
        assert_eq!(function_done["arguments"], r#"{"x":1}"#);
        let custom_done = events
            .iter()
            .find(|event| event["type"] == "response.custom_tool_call_input.done")
            .unwrap();
        assert_eq!(custom_done["input"], "text");
    }

    #[test]
    fn chat_sse_malformed_json_fails_once() {
        let mut adapter = ChatToResponsesStream::new(
            "resp_local_bad_stream".to_string(),
            "qwen3-coder-30b-local".to_string(),
            BTreeMap::new(),
        );

        let failed = adapter.map_line("data: {");

        assert_eq!(failed.len(), 1);
        assert!(failed[0].starts_with("event: response.failed\n"));
        assert!(adapter.map_line("data: [DONE]").is_empty());
    }

    #[test]
    fn chat_sse_invalid_custom_tool_input_fails_once() {
        let mut adapter = ChatToResponsesStream::new(
            "resp_local_bad_custom".to_string(),
            "qwen3-coder-30b-local".to_string(),
            BTreeMap::from([("shell".to_string(), LocalToolKind::Custom)]),
        );
        adapter.map_line(&format!(
            "data: {}",
            json!({"choices": [{"delta": {"tool_calls": [{
                "index": 0,
                "id": "call_custom",
                "function": {"name": "shell", "arguments": "not-json"}
            }]}}]})
        ));

        let terminal =
            adapter.map_line(r#"data: {"choices":[{"delta":{},"finish_reason":"tool_calls"}]}"#);

        assert_eq!(terminal.len(), 1);
        assert!(terminal[0].starts_with("event: response.failed\n"));
        assert!(adapter.map_line("data: [DONE]").is_empty());
    }

    #[test]
    fn chat_sse_length_finish_emits_incomplete_terminal() {
        let mut adapter = ChatToResponsesStream::new(
            "resp_local_incomplete_stream".to_string(),
            "qwen3-coder-30b-local".to_string(),
            BTreeMap::new(),
        );
        adapter.map_line(
            r#"data: {"choices":[{"delta":{"content":"partial"},"finish_reason":null}]}"#,
        );

        let terminal = adapter.map_line(
            r#"data: {"choices":[{"delta":{},"finish_reason":"length"}],"usage":{"prompt_tokens":2,"completion_tokens":1,"total_tokens":3}}"#,
        );
        let terminal = terminal
            .last()
            .and_then(|event| event.lines().find_map(|line| line.strip_prefix("data: ")))
            .and_then(|data| serde_json::from_str::<Value>(data).ok())
            .unwrap();

        assert_eq!(terminal["type"], "response.incomplete");
        assert_eq!(terminal["response"]["status"], "incomplete");
        assert!(!adapter.is_completed());
        assert!(adapter.map_line("data: [DONE]").is_empty());
    }

    #[test]
    fn chat_sse_done_without_finish_reason_fails() {
        let mut adapter = ChatToResponsesStream::new(
            "resp_local_unfinished".to_string(),
            "qwen3-coder-30b-local".to_string(),
            BTreeMap::new(),
        );
        adapter.map_line(r#"data: {"choices":[{"delta":{"content":"partial"}}]}"#);

        let terminal = adapter.map_line("data: [DONE]");

        assert_eq!(terminal.len(), 1);
        assert!(terminal[0].starts_with("event: response.failed\n"));
        assert!(!adapter.is_completed());
    }

    #[test]
    fn chat_sse_delta_after_finish_reason_fails() {
        let mut adapter = ChatToResponsesStream::new(
            "resp_local_post_finish".to_string(),
            "qwen3-coder-30b-local".to_string(),
            BTreeMap::new(),
        );
        adapter.map_line(r#"data: {"choices":[{"delta":{"content":"done"}}]}"#);
        adapter.map_line(r#"data: {"choices":[{"delta":{},"finish_reason":"stop"}]}"#);

        let terminal = adapter.map_line(r#"data: {"choices":[{"delta":{"content":"late"}}]}"#);

        assert_eq!(terminal.len(), 1);
        assert!(terminal[0].starts_with("event: response.failed\n"));
        assert!(!adapter.is_completed());
    }

    #[test]
    fn chat_sse_accepts_id_then_name_tool_metadata() {
        let mut adapter = ChatToResponsesStream::new(
            "resp_local_staggered".to_string(),
            "qwen3-coder-30b-local".to_string(),
            BTreeMap::from([("calculate".to_string(), LocalToolKind::Function)]),
        );
        let lines = [
            json!({"choices": [{"delta": {"tool_calls": [{
                "index": 0, "id": "call_1", "type": "function"
            }]}}]}),
            json!({"choices": [{"delta": {"tool_calls": [{
                "index": 0, "function": {"name": "calculate", "arguments": "{\"x\":1}"}
            }]}}]}),
            json!({
                "choices": [{"delta": {}, "finish_reason": "tool_calls"}],
                "usage": {"prompt_tokens": 2, "completion_tokens": 1, "total_tokens": 3}
            }),
        ];

        let events = lines
            .into_iter()
            .flat_map(|line| adapter.map_line(&format!("data: {line}")))
            .collect::<Vec<_>>();

        assert!(
            events.iter().any(|event| {
                event.starts_with("event: response.function_call_arguments.delta\n")
            })
        );
        assert!(
            events
                .last()
                .is_some_and(|event| event.starts_with("event: response.completed\n"))
        );
        assert_eq!(adapter.output_items()[0]["call_id"], "call_1");
    }

    #[test]
    fn chat_sse_duplicate_tool_call_ids_fail_at_finish() {
        let mut adapter = ChatToResponsesStream::new(
            "resp_local_duplicate_calls".to_string(),
            "qwen3-coder-30b-local".to_string(),
            BTreeMap::from([("calculate".to_string(), LocalToolKind::Function)]),
        );
        adapter.map_line(&format!(
            "data: {}",
            json!({"choices": [{"delta": {"tool_calls": [
                {"index": 0, "id": "call_duplicate", "function": {
                    "name": "calculate", "arguments": "{}"
                }},
                {"index": 1, "id": "call_duplicate", "function": {
                    "name": "calculate", "arguments": "{}"
                }}
            ]}}]})
        ));

        let terminal =
            adapter.map_line(r#"data: {"choices":[{"delta":{},"finish_reason":"tool_calls"}]}"#);

        assert_eq!(terminal.len(), 1);
        assert!(terminal[0].starts_with("event: response.failed\n"));
    }

    #[test]
    fn chat_sse_conflicting_tool_metadata_fails() {
        let mut adapter = ChatToResponsesStream::new(
            "resp_local_conflicting_call".to_string(),
            "qwen3-coder-30b-local".to_string(),
            BTreeMap::from([("calculate".to_string(), LocalToolKind::Function)]),
        );
        adapter.map_line(&format!(
            "data: {}",
            json!({"choices": [{"delta": {"tool_calls": [{
                "index": 0, "id": "call_1", "function": {"name": "calculate"}
            }]}}]})
        ));

        let terminal = adapter.map_line(&format!(
            "data: {}",
            json!({"choices": [{"delta": {"tool_calls": [{
                "index": 0, "id": "call_2", "function": {"arguments": "{}"}
            }]}}]})
        ));

        assert_eq!(terminal.len(), 1);
        assert!(terminal[0].starts_with("event: response.failed\n"));
    }

    #[test]
    fn chat_sse_out_of_order_tool_index_fails() {
        let mut adapter = ChatToResponsesStream::new(
            "resp_local_ordered_calls".to_string(),
            "qwen3-coder-30b-local".to_string(),
            BTreeMap::from([("calculate".to_string(), LocalToolKind::Function)]),
        );

        let terminal = adapter.map_line(&format!(
            "data: {}",
            json!({"choices": [{"delta": {"tool_calls": [{
                "index": 1, "id": "call_1", "function": {
                    "name": "calculate", "arguments": "{}"
                }
            }]}}]})
        ));

        assert_eq!(terminal.len(), 1);
        assert!(terminal[0].starts_with("event: response.failed\n"));
    }

    #[test]
    fn chat_sse_created_at_is_stable_across_events() {
        let mut adapter = ChatToResponsesStream::new(
            "resp_local_stable_time".to_string(),
            "qwen3-coder-30b-local".to_string(),
            BTreeMap::new(),
        );
        let created = adapter
            .map_line(r#"data: {"choices":[{"delta":{"content":"hello"},"finish_reason":null}]}"#);
        let created_at = created[0]
            .lines()
            .find_map(|line| line.strip_prefix("data: "))
            .and_then(|data| serde_json::from_str::<Value>(data).ok())
            .and_then(|event| event["response"]["created_at"].as_u64())
            .unwrap();
        std::thread::sleep(std::time::Duration::from_millis(1_100));

        let completed = adapter.map_line(
            r#"data: {"choices":[{"delta":{},"finish_reason":"stop"}],"usage":{"prompt_tokens":1,"completion_tokens":1,"total_tokens":2}}"#,
        );
        let completed_at = completed
            .last()
            .and_then(|event| event.lines().find_map(|line| line.strip_prefix("data: ")))
            .and_then(|data| serde_json::from_str::<Value>(data).ok())
            .and_then(|event| event["response"]["created_at"].as_u64())
            .unwrap();

        assert_eq!(completed_at, created_at);
    }

    #[test]
    fn chat_response_translates_text_function_call_and_usage() {
        let tool_kinds = BTreeMap::from([("get_weather".to_string(), LocalToolKind::Function)]);
        let response = chat_to_responses(
            &json!({
                "choices": [{
                    "finish_reason": "tool_calls",
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
                    "finish_reason": "tool_calls",
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
    fn chat_response_maps_incomplete_finish_reasons() {
        for (finish_reason, expected_reason) in [
            ("length", "max_output_tokens"),
            ("content_filter", "content_filter"),
        ] {
            let response = chat_to_responses(
                &json!({
                    "choices": [{
                        "finish_reason": finish_reason,
                        "message": {"role": "assistant", "content": "partial"}
                    }]
                }),
                "resp_local_incomplete",
                "qwen3-coder-30b-local",
                &BTreeMap::new(),
            )
            .unwrap();

            assert_eq!(response["status"], "incomplete");
            assert_eq!(
                response["incomplete_details"],
                json!({"reason": expected_reason})
            );
        }
    }

    #[test]
    fn chat_response_rejects_invalid_finish_reasons() {
        for finish_reason in [Value::Null, json!("unknown"), json!(7)] {
            let error = chat_to_responses(
                &json!({
                    "choices": [{
                        "finish_reason": finish_reason,
                        "message": {"role": "assistant", "content": "hello"}
                    }]
                }),
                "resp_local_bad_finish",
                "qwen3-coder-30b-local",
                &BTreeMap::new(),
            )
            .unwrap_err();

            assert!(matches!(
                error,
                ResponsesTranslationError::InvalidResponse(_)
            ));
        }

        let missing = chat_to_responses(
            &json!({"choices": [{"message": {"content": "hello"}}]}),
            "resp_local_missing_finish",
            "qwen3-coder-30b-local",
            &BTreeMap::new(),
        )
        .unwrap_err();
        assert!(matches!(
            missing,
            ResponsesTranslationError::InvalidResponse(_)
        ));
    }

    #[test]
    fn chat_response_rejects_duplicate_tool_call_ids() {
        let tool_kinds = BTreeMap::from([
            ("first_tool".to_string(), LocalToolKind::Function),
            ("second_tool".to_string(), LocalToolKind::Function),
        ]);
        let error = chat_to_responses(
            &json!({
                "choices": [{
                    "finish_reason": "tool_calls",
                    "message": {
                        "content": null,
                        "tool_calls": [
                            {
                                "id": "call_duplicate",
                                "type": "function",
                                "function": {"name": "first_tool", "arguments": "{}"}
                            },
                            {
                                "id": "call_duplicate",
                                "type": "function",
                                "function": {"name": "second_tool", "arguments": "{}"}
                            }
                        ]
                    }
                }]
            }),
            "resp_local_duplicate",
            "qwen3-coder-30b-local",
            &tool_kinds,
        )
        .unwrap_err();

        assert!(matches!(
            error,
            ResponsesTranslationError::InvalidResponse(_)
        ));
    }

    #[test]
    fn chat_response_rejects_non_assistant_roles() {
        for role in [json!("user"), json!(7)] {
            let error = chat_to_responses(
                &json!({
                    "choices": [{
                        "finish_reason": "stop",
                        "message": {"role": role, "content": "hello"}
                    }]
                }),
                "resp_local_bad_role",
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
