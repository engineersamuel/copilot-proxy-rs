use std::collections::BTreeMap;

use serde_json::{Map, Value};

use crate::models::{EffortLevel, SupportedEfforts};

pub const COPILOT_REQUEST_API_VERSION: &str = "2026-06-01";
pub const ENCRYPTED_FUNCTION_OUTPUT_DECRYPTION_ERROR: &str =
    "encrypted function output content could not be decrypted or decoded";

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
        return messages_initiator(messages);
    }
    let continuing = strict_continuation && body.get("previous_response_id").is_some();
    if let Some(input) = body.get("input") {
        if input.is_string() {
            return "user";
        }
        if let Some(items) = input.as_array() {
            return input_items_initiator(items, continuing);
        }
    }
    if continuing {
        return "agent";
    }
    "user"
}

fn messages_initiator(messages: &[Value]) -> &'static str {
    let Some(last) = messages.last().and_then(Value::as_object) else {
        return "user";
    };
    match last.get("role").and_then(Value::as_str) {
        Some("assistant" | "tool") => "agent",
        Some("user")
            if content_has_agent_tool_result(last.get("content"))
                || is_suggestion_mode(last.get("content")) =>
        {
            "agent"
        }
        _ => "user",
    }
}

fn input_items_initiator(items: &[Value], continuing: bool) -> &'static str {
    if input_has_tool_outputs(items) {
        return "agent";
    }
    let last_is_agent = items.last().and_then(Value::as_object).is_some_and(|last| {
        matches!(
            last.get("type").and_then(Value::as_str),
            Some("function_call" | "custom_tool_call")
        ) || last.get("role").and_then(Value::as_str) == Some("assistant")
    });
    if last_is_agent || (continuing && !input_has_user_message(items)) {
        return "agent";
    }
    "user"
}

/// Normalize and forward client `anthropic-beta` values.
///
/// Trims CSV parts and drops empties, but does not allowlist prefixes so new
/// Anthropic betas (e.g. extended cache TTL) reach Copilot without a proxy bump.
/// Unsupported betas may be rejected or ignored upstream.
pub fn filter_anthropic_beta_header(beta: &str) -> Option<String> {
    let filtered: Vec<&str> = beta
        .split(',')
        .map(str::trim)
        .filter(|part| !part.is_empty())
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
    normalize_input_namespace_descriptions(body);
}

pub fn strip_agent_message_encrypted_content(body: &mut Map<String, Value>) -> usize {
    let Some(Value::Array(items)) = body.get_mut("input") else {
        return 0;
    };
    let mut stripped = 0;
    for item in items {
        if item.get("type").and_then(Value::as_str) != Some("agent_message") {
            continue;
        }
        let Some(content) = item.get_mut("content").and_then(Value::as_array_mut) else {
            continue;
        };
        if !content
            .iter()
            .any(|part| part.get("type").and_then(Value::as_str) != Some("encrypted_content"))
        {
            continue;
        }
        content.retain(|part| {
            let keep = part.get("type").and_then(Value::as_str) != Some("encrypted_content");
            if !keep {
                stripped += 1;
            }
            keep
        });
    }
    stripped
}

/// Removes `agent_message` encrypted parts that use the `RT\x02` CBOR envelope
/// (base64 prefix `UlQC`). Copilot rejects that format on every attempt, so
/// sending it only costs a failed round trip. Parts are removed only when the
/// same message keeps readable content; Fernet blobs and reasoning ciphertext
/// stay untouched.
pub fn strip_rejected_agent_message_encrypted_content(body: &mut Map<String, Value>) -> usize {
    let Some(Value::Array(items)) = body.get_mut("input") else {
        return 0;
    };
    let mut stripped = 0;
    for item in items {
        if item.get("type").and_then(Value::as_str) != Some("agent_message") {
            continue;
        }
        let Some(content) = item.get_mut("content").and_then(Value::as_array_mut) else {
            continue;
        };
        if !content
            .iter()
            .any(|part| part.get("type").and_then(Value::as_str) != Some("encrypted_content"))
        {
            continue;
        }
        content.retain(|part| {
            let keep = !is_rejected_encrypted_envelope_part(part);
            if !keep {
                stripped += 1;
            }
            keep
        });
    }
    stripped
}

fn is_rejected_encrypted_envelope_part(part: &Value) -> bool {
    part.get("type").and_then(Value::as_str) == Some("encrypted_content")
        && part
            .get("encrypted_content")
            .and_then(Value::as_str)
            .is_some_and(is_rejected_encrypted_envelope)
}

/// Matches `RT`, envelope version `0x02`, a CBOR map header, and a first
/// text key of `version`.
fn is_rejected_encrypted_envelope(blob: &str) -> bool {
    const PREFIX_CHARS: usize = 16;
    let Some(prefix) = blob.get(..PREFIX_CHARS) else {
        return false;
    };
    let Some(bytes) = decode_base64_prefix(prefix) else {
        return false;
    };
    bytes.len() == 12
        && bytes[..3] == *b"RT\x02"
        && (0xa1..=0xb7).contains(&bytes[3])
        && bytes[4] == 0x67
        && bytes[5..12] == *b"version"
}

fn decode_base64_prefix(chars: &str) -> Option<Vec<u8>> {
    fn sextet(byte: u8) -> Option<u32> {
        match byte {
            b'A'..=b'Z' => Some(u32::from(byte - b'A')),
            b'a'..=b'z' => Some(u32::from(byte - b'a') + 26),
            b'0'..=b'9' => Some(u32::from(byte - b'0') + 52),
            b'+' | b'-' => Some(62),
            b'/' | b'_' => Some(63),
            _ => None,
        }
    }
    let mut out = Vec::with_capacity(chars.len() / 4 * 3);
    for chunk in chars.as_bytes().chunks_exact(4) {
        let mut word = 0u32;
        for &byte in chunk {
            word = (word << 6) | sextet(byte)?;
        }
        out.extend_from_slice(&word.to_be_bytes()[1..]);
    }
    Some(out)
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

/// Codex 0.147.0 embeds default tools under `input[*].tools` namespaces and
/// deliberately sends an empty description for the default `functions`
/// namespace. Copilot rejects blank namespace descriptions, so fill a
/// deterministic fallback before the request leaves the proxy.
fn normalize_input_namespace_descriptions(body: &mut Map<String, Value>) {
    let Some(Value::Array(items)) = body.get_mut("input") else {
        return;
    };
    for item in items {
        let Some(tools) = item.get_mut("tools").and_then(Value::as_array_mut) else {
            continue;
        };
        for tool in tools {
            normalize_namespace_description(tool);
        }
    }
}

fn normalize_namespace_description(tool: &mut Value) {
    let Some(tool_obj) = tool.as_object_mut() else {
        return;
    };
    if tool_obj.get("type").and_then(Value::as_str) != Some("namespace") {
        return;
    }
    let Some(name) = tool_obj.get("name").and_then(Value::as_str) else {
        return;
    };
    if name.trim().is_empty() {
        return;
    }
    let needs_fallback = tool_obj
        .get("description")
        .and_then(Value::as_str)
        .is_none_or(|text| text.trim().is_empty());
    if needs_fallback {
        let description = format!("Tools in the {name} namespace.");
        tool_obj.insert("description".to_string(), Value::String(description));
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
    body.remove("context_management");
    strip_structured_output(body);
    adapt_output_config_effort(body, supported_efforts);

    let Some(thinking) = body.get_mut("thinking").and_then(Value::as_object_mut) else {
        return;
    };
    match thinking.get("type").and_then(Value::as_str) {
        Some("enabled") if is_adaptive_only_model(model) => {
            thinking.clear();
            thinking.insert("type".to_string(), Value::String("adaptive".to_string()));
            if !matches!(
                model,
                "claude-opus-5.5" | "claude-opus-5-5" | "claude-sonnet-5.5" | "claude-sonnet-5-5"
            ) {
                body.remove("output_config");
            }
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
        "claude-opus-4.7"
            | "claude-opus-4-7"
            | "claude-opus-4.8"
            | "claude-opus-4-8"
            | "claude-opus-5"
            | "claude-opus-5.5"
            | "claude-opus-5-5"
            | "claude-sonnet-5.5"
            | "claude-sonnet-5-5"
    )
}

fn is_adaptive_capable_model(model: &str) -> bool {
    is_adaptive_only_model(model)
        || matches!(
            model,
            "claude-opus-4.6"
                | "claude-opus-4-6"
                | "claude-sonnet-4.6"
                | "claude-sonnet-4-6"
                | "claude-sonnet-5"
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

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const REJECTED_ENVELOPE: &str =
        "UlQCpmd2ZXJzaW9uYzEuMGpjaXBoZXJ0ZXh0AAECAwQFBgcICQoLDA0ODxAREhM=";
    const OTHER_ENVELOPE_VERSION: &str =
        "UlQDpmd2ZXJzaW9uYzEuMGpjaXBoZXJ0ZXh0AAECAwQFBgcICQoLDA0ODxAREhM=";
    const FERNET: &str = "gAABAgMEBQYHCAkKCwwNDg8QERITFBUWFxgZGhscHR4fICEiIyQlJic=";

    fn body_with(content: Value) -> Map<String, Value> {
        json!({"input": [{"type": "agent_message", "content": content}]})
            .as_object()
            .unwrap()
            .clone()
    }

    #[test]
    fn detects_only_the_rejected_envelope_format() {
        assert!(is_rejected_encrypted_envelope(REJECTED_ENVELOPE));
        assert!(is_rejected_encrypted_envelope(
            &REJECTED_ENVELOPE.replace('+', "-").replace('/', "_")
        ));
        assert!(!is_rejected_encrypted_envelope(OTHER_ENVELOPE_VERSION));
        assert!(!is_rejected_encrypted_envelope(FERNET));
        assert!(!is_rejected_encrypted_envelope("UlQC"));
        assert!(!is_rejected_encrypted_envelope("opaque"));
        assert!(!is_rejected_encrypted_envelope(
            "UlQC\u{e9}\u{e9}\u{e9}\u{e9}\u{e9}\u{e9}"
        ));
    }

    #[test]
    fn strips_rejected_envelope_and_keeps_fernet_and_text() {
        let mut body = body_with(json!([
            {"type": "input_text", "text": "result"},
            {"type": "encrypted_content", "encrypted_content": FERNET},
            {"type": "encrypted_content", "encrypted_content": REJECTED_ENVELOPE}
        ]));

        assert_eq!(strip_rejected_agent_message_encrypted_content(&mut body), 1);
        assert_eq!(
            body["input"][0]["content"],
            json!([
                {"type": "input_text", "text": "result"},
                {"type": "encrypted_content", "encrypted_content": FERNET}
            ])
        );
    }

    #[test]
    fn keeps_rejected_envelope_without_readable_sibling() {
        let content = json!([
            {"type": "encrypted_content", "encrypted_content": REJECTED_ENVELOPE}
        ]);
        let mut body = body_with(content.clone());

        assert_eq!(strip_rejected_agent_message_encrypted_content(&mut body), 0);
        assert_eq!(body["input"][0]["content"], content);
    }

    #[test]
    fn ignores_non_agent_message_items() {
        let mut body = json!({"input": [{
            "type": "reasoning",
            "encrypted_content": REJECTED_ENVELOPE,
            "content": [
                {"type": "input_text", "text": "x"},
                {"type": "encrypted_content", "encrypted_content": REJECTED_ENVELOPE}
            ]
        }]})
        .as_object()
        .unwrap()
        .clone();

        assert_eq!(strip_rejected_agent_message_encrypted_content(&mut body), 0);
    }
}
