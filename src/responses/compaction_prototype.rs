//! PROTOTYPE: compare proxy-managed OpenAI and Anthropic compaction state.
//!
//! Question: can one ordinary LLM summary be wrapped in each client's native
//! compaction shape, then expanded by the proxy into model-readable context?
//! The OpenAI carrier is only hex-encoded here; production must authenticate or
//! encrypt proxy-owned state before placing it in `encrypted_content`.

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use thiserror::Error;

const OPENAI_CARRIER_PREFIX: &str = "copilot-proxy-rs.compaction.v1:";
const SUMMARY_INSTRUCTIONS: &str = "\
Summarize the conversation for another coding agent that will continue without \
the original transcript. Preserve instructions, decisions, constraints, exact \
identifiers, tool results, current state, unresolved work, and next steps. \
Return only one <summary>...</summary> block.";

/// Client contract being prototyped.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ClientProtocol {
    OpenAiResponses,
    AnthropicMessages,
}

/// Protocol-neutral conversation item used by the prototype.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ConversationItem {
    Message {
        role: Role,
        text: String,
    },
    ToolCall {
        id: String,
        name: String,
        arguments: String,
    },
    ToolResult {
        call_id: String,
        output: String,
    },
}

/// Message role used by [`ConversationItem`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    System,
    Developer,
    User,
    Assistant,
}

/// Pending internal LLM call plus the client context that will survive it.
#[derive(Debug, Clone, Serialize)]
pub struct CompactionDraft {
    pub protocol: ClientProtocol,
    pub model: String,
    pub summarized_items: Vec<ConversationItem>,
    pub retained_items: Vec<ConversationItem>,
    pub summary_request: Value,
    pinned_instructions: Vec<String>,
}

/// Compaction result returned to the client and round-tripped on its next call.
#[derive(Debug, Clone, Serialize)]
pub struct CompactedConversation {
    pub protocol: ClientProtocol,
    pub model: String,
    pub client_response: Value,
    pub next_request_context: Value,
}

/// Errors surfaced while decoding a client-round-tripped compaction.
#[derive(Debug, Error)]
pub enum PrototypeError {
    #[error("conversation has no history available to summarize")]
    NothingToSummarize,
    #[error("missing field `{0}` in compacted client context")]
    MissingField(&'static str),
    #[error("invalid proxy compaction carrier prefix")]
    InvalidCarrierPrefix,
    #[error("invalid hexadecimal compaction carrier")]
    InvalidCarrierHex,
    #[error("invalid proxy compaction carrier: {0}")]
    InvalidCarrierJson(#[from] serde_json::Error),
    #[error("unsupported compacted client item: {0}")]
    UnsupportedClientItem(String),
}

#[derive(Debug, Serialize, Deserialize)]
struct OpenAiCarrier {
    version: u8,
    summary: String,
}

/// Starts compaction and builds the ordinary Responses API call used to ask the
/// upstream LLM for a summary.
pub fn begin_compaction(
    transcript: &[ConversationItem],
    protocol: ClientProtocol,
    model: &str,
) -> Result<CompactionDraft, PrototypeError> {
    let (pinned, history): (Vec<_>, Vec<_>) =
        transcript.iter().cloned().partition(is_pinned_instruction);
    let groups = atomic_groups(&history);
    let boundary = retention_boundary(&groups, protocol);
    let summarized_items = flatten_groups(&groups[..boundary]);
    let retained_items = flatten_groups(&groups[boundary..]);

    if summarized_items.is_empty() {
        return Err(PrototypeError::NothingToSummarize);
    }

    let pinned_instructions = pinned
        .iter()
        .filter_map(message_text)
        .map(str::to_string)
        .collect::<Vec<_>>();
    let summary_request = json!({
        "endpoint": "POST /v1/responses",
        "purpose": "proxy-internal compaction summary",
        "body": {
            "model": model,
            "store": false,
            "instructions": SUMMARY_INSTRUCTIONS,
            "input": [{
                "role": "user",
                "content": [{
                    "type": "input_text",
                    "text": render_summary_source(&pinned, &summarized_items)
                }]
            }]
        }
    });

    Ok(CompactionDraft {
        protocol,
        model: model.to_string(),
        summarized_items,
        retained_items,
        summary_request,
        pinned_instructions,
    })
}

/// Wraps an upstream LLM summary in the selected client's compaction contract.
pub fn finish_compaction(
    draft: CompactionDraft,
    model_summary: &str,
) -> Result<CompactedConversation, PrototypeError> {
    let summary = normalized_summary(model_summary);

    match draft.protocol {
        ClientProtocol::OpenAiResponses => finish_openai(draft, &summary),
        ClientProtocol::AnthropicMessages => Ok(finish_anthropic(draft, &summary)),
    }
}

/// Expands client-round-tripped compaction state into a normal Responses API
/// request that the upstream LLM can read.
pub fn expand_for_upstream(
    compacted: &CompactedConversation,
    follow_up: &str,
) -> Result<Value, PrototypeError> {
    match compacted.protocol {
        ClientProtocol::OpenAiResponses => expand_openai(compacted, follow_up),
        ClientProtocol::AnthropicMessages => expand_anthropic(compacted, follow_up),
    }
}

/// Small transcript with a tool pair and enough turns to exercise retention.
pub fn sample_transcript() -> Vec<ConversationItem> {
    vec![
        message(Role::System, "You are a coding agent. Keep edits surgical."),
        message(
            Role::Developer,
            "Never proxy Copilot CLI; support only Codex and Claude Code.",
        ),
        message(Role::User, "Find why /v1/responses/compact is a no-op."),
        message(
            Role::Assistant,
            "I will inspect the compact route and client expectations.",
        ),
        ConversationItem::ToolCall {
            id: "call_1".to_string(),
            name: "read_file".to_string(),
            arguments: r#"{"path":"src/http/responses.rs"}"#.to_string(),
        },
        ConversationItem::ToolResult {
            call_id: "call_1".to_string(),
            output: "responses_compact returns output: []".to_string(),
        },
        message(
            Role::Assistant,
            "The successful empty output can erase canonical client context.",
        ),
        message(
            Role::User,
            "Prototype both OpenAI-style and Claude-style compaction.",
        ),
        message(
            Role::Assistant,
            "Use one LLM summary and protocol-specific client envelopes.",
        ),
        message(
            Role::User,
            "Preserve tool pairs and show what is sent upstream next.",
        ),
    ]
}

/// Representative result from the internal summarization call.
pub fn sample_model_summary() -> &'static str {
    "<summary>The compact endpoint currently returns an empty successful output, \
which risks replacing canonical context with nothing. The proxy should make one \
ordinary LLM call to summarize old history, preserve system/developer rules and \
recent complete turns, return a protocol-native compaction item, and expand that \
item before forwarding the next request upstream. OpenAI requires exactly one \
opaque compaction item; Anthropic returns a plaintext compaction block. Copilot \
CLI is out of scope. Preserve tool call/result pairs atomically.</summary>"
}

fn finish_openai(
    draft: CompactionDraft,
    summary: &str,
) -> Result<CompactedConversation, PrototypeError> {
    let carrier = encode_openai_carrier(summary)?;
    let retained = draft
        .retained_items
        .iter()
        .map(to_openai_item)
        .collect::<Vec<_>>();
    let mut output = Vec::with_capacity(retained.len() + 1);
    output.push(json!({
        "type": "compaction",
        "encrypted_content": carrier
    }));
    output.extend(retained);
    let client_response = json!({
        "id": "resp_compact_prototype",
        "object": "response.compaction",
        "created_at": 0,
        "output": output.clone(),
        "usage": {
            "input_tokens": 0,
            "output_tokens": 0,
            "total_tokens": 0
        }
    });
    let next_request_context = json!({
        "instructions": draft.pinned_instructions.join("\n\n"),
        "input": output
    });

    Ok(CompactedConversation {
        protocol: draft.protocol,
        model: draft.model,
        client_response,
        next_request_context,
    })
}

fn finish_anthropic(draft: CompactionDraft, summary: &str) -> CompactedConversation {
    let block = json!({
        "type": "compaction",
        "content": summary,
        "encrypted_content": null
    });
    let mut messages = Vec::with_capacity(draft.retained_items.len() + 1);
    messages.push(json!({
        "role": "assistant",
        "content": [block.clone()]
    }));
    messages.extend(draft.retained_items.iter().map(to_anthropic_message));
    let client_response = json!({
        "id": "msg_compact_prototype",
        "type": "message",
        "role": "assistant",
        "model": draft.model.as_str(),
        "content": [block],
        "stop_reason": "compaction",
        "stop_sequence": null,
        "usage": {
            "input_tokens": 0,
            "output_tokens": 0
        }
    });
    let next_request_context = json!({
        "system": draft.pinned_instructions.join("\n\n"),
        "messages": messages,
        "context_management": {
            "edits": [{"type": "compact_20260112"}]
        }
    });

    CompactedConversation {
        protocol: draft.protocol,
        model: draft.model,
        client_response,
        next_request_context,
    }
}

fn expand_openai(
    compacted: &CompactedConversation,
    follow_up: &str,
) -> Result<Value, PrototypeError> {
    let context = &compacted.next_request_context;
    let input = context
        .get("input")
        .and_then(Value::as_array)
        .ok_or(PrototypeError::MissingField("input"))?;
    let mut expanded = Vec::with_capacity(input.len() + 1);

    for item in input {
        if item.get("type").and_then(Value::as_str) == Some("compaction") {
            let carrier = item
                .get("encrypted_content")
                .and_then(Value::as_str)
                .ok_or(PrototypeError::MissingField("encrypted_content"))?;
            let summary = decode_openai_carrier(carrier)?;
            expanded.push(summary_as_openai_item(&summary));
        } else {
            expanded.push(item.clone());
        }
    }
    expanded.push(to_openai_item(&message(Role::User, follow_up)));

    Ok(json!({
        "model": compacted.model.as_str(),
        "instructions": context.get("instructions").cloned().unwrap_or_default(),
        "input": expanded
    }))
}

fn expand_anthropic(
    compacted: &CompactedConversation,
    follow_up: &str,
) -> Result<Value, PrototypeError> {
    let context = &compacted.next_request_context;
    let messages = context
        .get("messages")
        .and_then(Value::as_array)
        .ok_or(PrototypeError::MissingField("messages"))?;
    let mut expanded = Vec::with_capacity(messages.len() + 1);

    for client_message in messages {
        let role = client_message
            .get("role")
            .and_then(Value::as_str)
            .ok_or(PrototypeError::MissingField("role"))?;
        let content = client_message
            .get("content")
            .and_then(Value::as_array)
            .ok_or(PrototypeError::MissingField("content"))?;

        for block in content {
            match block.get("type").and_then(Value::as_str) {
                Some("compaction") => {
                    let summary = block
                        .get("content")
                        .and_then(Value::as_str)
                        .ok_or(PrototypeError::MissingField("compaction.content"))?;
                    expanded.push(summary_as_openai_item(summary));
                }
                Some("text") => {
                    let text = block
                        .get("text")
                        .and_then(Value::as_str)
                        .ok_or(PrototypeError::MissingField("text"))?;
                    expanded.push(to_openai_item(&message(parse_message_role(role)?, text)));
                }
                Some("tool_use") => {
                    let arguments = block
                        .get("input")
                        .cloned()
                        .unwrap_or_else(|| json!({}))
                        .to_string();
                    expanded.push(json!({
                        "type": "function_call",
                        "call_id": required_string(block, "id")?,
                        "name": required_string(block, "name")?,
                        "arguments": arguments
                    }));
                }
                Some("tool_result") => expanded.push(json!({
                    "type": "function_call_output",
                    "call_id": required_string(block, "tool_use_id")?,
                    "output": required_string(block, "content")?
                })),
                other => {
                    return Err(PrototypeError::UnsupportedClientItem(format!("{other:?}")));
                }
            }
        }
    }
    expanded.push(to_openai_item(&message(Role::User, follow_up)));

    Ok(json!({
        "model": compacted.model.as_str(),
        "instructions": context.get("system").cloned().unwrap_or_default(),
        "input": expanded
    }))
}

fn atomic_groups(items: &[ConversationItem]) -> Vec<Vec<ConversationItem>> {
    let mut groups = Vec::new();
    let mut index = 0;

    while index < items.len() {
        let ConversationItem::ToolCall { .. } = &items[index] else {
            groups.push(vec![items[index].clone()]);
            index += 1;
            continue;
        };

        let mut group = Vec::new();
        let mut call_ids = Vec::new();
        while let Some(ConversationItem::ToolCall { id, .. }) = items.get(index) {
            call_ids.push(id.clone());
            group.push(items[index].clone());
            index += 1;
        }
        while let Some(ConversationItem::ToolResult { call_id, .. }) = items.get(index) {
            if !call_ids.iter().any(|id| id == call_id) {
                break;
            }
            group.push(items[index].clone());
            index += 1;
        }

        groups.push(group);
    }

    groups
}

fn retention_boundary(groups: &[Vec<ConversationItem>], protocol: ClientProtocol) -> usize {
    let mut boundary = groups.len().saturating_sub(3);

    if protocol == ClientProtocol::AnthropicMessages {
        while boundary > 0 && !starts_user_turn(&groups[boundary]) {
            boundary -= 1;
        }
    }

    boundary
}

fn starts_user_turn(group: &[ConversationItem]) -> bool {
    matches!(
        group.first(),
        Some(ConversationItem::Message {
            role: Role::User,
            ..
        }) | Some(ConversationItem::ToolResult { .. })
    )
}

fn flatten_groups(groups: &[Vec<ConversationItem>]) -> Vec<ConversationItem> {
    groups.iter().flatten().cloned().collect()
}

fn is_pinned_instruction(item: &ConversationItem) -> bool {
    matches!(
        item,
        ConversationItem::Message {
            role: Role::System | Role::Developer,
            ..
        }
    )
}

fn message_text(item: &ConversationItem) -> Option<&str> {
    match item {
        ConversationItem::Message { text, .. } => Some(text),
        ConversationItem::ToolCall { .. } | ConversationItem::ToolResult { .. } => None,
    }
}

fn render_summary_source(pinned: &[ConversationItem], summarized: &[ConversationItem]) -> String {
    pinned
        .iter()
        .chain(summarized)
        .map(render_item)
        .collect::<Vec<_>>()
        .join("\n")
}

fn render_item(item: &ConversationItem) -> String {
    match item {
        ConversationItem::Message { role, text } => {
            format!("[{}] {text}", role_name(*role))
        }
        ConversationItem::ToolCall {
            id,
            name,
            arguments,
        } => format!("[tool_call id={id} name={name}] {arguments}"),
        ConversationItem::ToolResult { call_id, output } => {
            format!("[tool_result call_id={call_id}] {output}")
        }
    }
}

fn to_openai_item(item: &ConversationItem) -> Value {
    match item {
        ConversationItem::Message { role, text } => match role {
            Role::Assistant => json!({
                "type": "message",
                "role": "assistant",
                "content": [{"type": "output_text", "text": text}]
            }),
            _ => json!({
                "role": role_name(*role),
                "content": [{"type": "input_text", "text": text}]
            }),
        },
        ConversationItem::ToolCall {
            id,
            name,
            arguments,
        } => json!({
            "type": "function_call",
            "call_id": id,
            "name": name,
            "arguments": arguments
        }),
        ConversationItem::ToolResult { call_id, output } => json!({
            "type": "function_call_output",
            "call_id": call_id,
            "output": output
        }),
    }
}

fn to_anthropic_message(item: &ConversationItem) -> Value {
    match item {
        ConversationItem::Message { role, text } => json!({
            "role": role_name(*role),
            "content": [{"type": "text", "text": text}]
        }),
        ConversationItem::ToolCall {
            id,
            name,
            arguments,
        } => json!({
            "role": "assistant",
            "content": [{
                "type": "tool_use",
                "id": id,
                "name": name,
                "input": serde_json::from_str::<Value>(arguments).unwrap_or_else(|_| json!({
                    "raw": arguments
                }))
            }]
        }),
        ConversationItem::ToolResult { call_id, output } => json!({
            "role": "user",
            "content": [{
                "type": "tool_result",
                "tool_use_id": call_id,
                "content": output
            }]
        }),
    }
}

fn summary_as_openai_item(summary: &str) -> Value {
    json!({
        "role": "user",
        "content": [{
            "type": "input_text",
            "text": format!("Compacted conversation state:\n{summary}")
        }]
    })
}

fn normalized_summary(summary: &str) -> String {
    let trimmed = summary.trim();
    if trimmed.starts_with("<summary>") && trimmed.ends_with("</summary>") {
        trimmed.to_string()
    } else {
        format!("<summary>{trimmed}</summary>")
    }
}

fn encode_openai_carrier(summary: &str) -> Result<String, PrototypeError> {
    let payload = serde_json::to_vec(&OpenAiCarrier {
        version: 1,
        summary: summary.to_string(),
    })?;
    Ok(format!("{OPENAI_CARRIER_PREFIX}{}", encode_hex(&payload)))
}

fn decode_openai_carrier(carrier: &str) -> Result<String, PrototypeError> {
    let encoded = carrier
        .strip_prefix(OPENAI_CARRIER_PREFIX)
        .ok_or(PrototypeError::InvalidCarrierPrefix)?;
    let bytes = decode_hex(encoded)?;
    let payload: OpenAiCarrier = serde_json::from_slice(&bytes)?;
    Ok(payload.summary)
}

fn encode_hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut encoded = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        encoded.push(HEX[(byte >> 4) as usize] as char);
        encoded.push(HEX[(byte & 0x0f) as usize] as char);
    }
    encoded
}

fn decode_hex(encoded: &str) -> Result<Vec<u8>, PrototypeError> {
    let chunks = encoded.as_bytes().chunks_exact(2);
    if !chunks.remainder().is_empty() {
        return Err(PrototypeError::InvalidCarrierHex);
    }

    chunks
        .map(|pair| {
            let high = hex_value(pair[0]).ok_or(PrototypeError::InvalidCarrierHex)?;
            let low = hex_value(pair[1]).ok_or(PrototypeError::InvalidCarrierHex)?;
            Ok((high << 4) | low)
        })
        .collect()
}

fn hex_value(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

fn parse_message_role(role: &str) -> Result<Role, PrototypeError> {
    match role {
        "user" => Ok(Role::User),
        "assistant" => Ok(Role::Assistant),
        other => Err(PrototypeError::UnsupportedClientItem(format!(
            "message role {other}"
        ))),
    }
}

fn required_string<'a>(value: &'a Value, key: &'static str) -> Result<&'a str, PrototypeError> {
    value
        .get(key)
        .and_then(Value::as_str)
        .ok_or(PrototypeError::MissingField(key))
}

fn role_name(role: Role) -> &'static str {
    match role {
        Role::System => "system",
        Role::Developer => "developer",
        Role::User => "user",
        Role::Assistant => "assistant",
    }
}

fn message(role: Role, text: &str) -> ConversationItem {
    ConversationItem::Message {
        role,
        text: text.to_string(),
    }
}
