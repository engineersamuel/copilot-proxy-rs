use copilot_proxy_rs::telemetry::{
    ApiFamily, summarize_effective_request, summarize_request, summarize_request_sizes,
    summarize_usage,
};

#[test]
fn request_summary_extracts_safe_chat_metadata_without_content() {
    let body = serde_json::json!({
        "model": "gpt-5.5",
        "stream": true,
        "reasoning_effort": "high",
        "max_completion_tokens": 123,
        "messages": [
            {"role": "system", "content": "secret system prompt"},
            {"role": "user", "content": "private prompt text"},
            {"role": "tool", "content": "private tool result"}
        ],
        "tools": [
            {"type": "function", "function": {"name": "secret_tool", "parameters": {"type": "object"}}}
        ]
    });

    let summary = summarize_request(ApiFamily::ChatCompletions, body.as_object().unwrap());

    assert_eq!(summary.api_family, ApiFamily::ChatCompletions);
    assert_eq!(summary.requested_model.as_deref(), Some("gpt-5.5"));
    assert_eq!(summary.effective_model, None);
    assert!(summary.stream);
    assert_eq!(summary.message_count, 3);
    assert_eq!(summary.tool_definition_count, 1);
    assert_eq!(summary.tool_result_count, 1);
    assert_eq!(summary.max_tokens, Some(123));
    assert_eq!(summary.effort.as_deref(), Some("high"));
    let debug = format!("{summary:?}");
    assert!(!debug.contains("private prompt text"));
    assert!(!debug.contains("secret system prompt"));
    assert!(!debug.contains("secret_tool"));
    assert!(!debug.contains("private tool result"));
}

#[test]
fn effective_request_summary_keeps_requested_and_effective_models() {
    let body = serde_json::json!({
        "model": "gpt-5.5-copilot",
        "messages": [{"role":"user","content":"hello"}]
    });

    let summary = summarize_effective_request(
        ApiFamily::ChatCompletions,
        Some("gpt-5.5"),
        body.as_object().unwrap(),
    );

    assert_eq!(summary.requested_model.as_deref(), Some("gpt-5.5"));
    assert_eq!(summary.effective_model.as_deref(), Some("gpt-5.5-copilot"));
}

#[test]
fn usage_summary_reads_openai_responses_and_anthropic_cache_fields() {
    let openai = serde_json::json!({
        "usage": {
            "input_tokens": 10,
            "output_tokens": 4,
            "total_tokens": 14,
            "input_tokens_details": {"cached_tokens": 6}
        }
    });
    let usage = summarize_usage(&openai);
    assert_eq!(usage.input_tokens, Some(10));
    assert_eq!(usage.output_tokens, Some(4));
    assert_eq!(usage.cached_tokens, Some(6));
    assert_eq!(usage.total_tokens, Some(14));

    let anthropic = serde_json::json!({
        "usage": {
            "input_tokens": 7,
            "output_tokens": 3,
            "cache_read_input_tokens": 2,
            "cache_creation_input_tokens": 1
        }
    });
    let usage = summarize_usage(&anthropic);
    assert_eq!(usage.input_tokens, Some(7));
    assert_eq!(usage.output_tokens, Some(3));
    assert_eq!(usage.cached_tokens, Some(3));
    assert_eq!(usage.total_tokens, Some(10));
}

#[test]
fn responses_summary_counts_input_items_and_reasoning_effort() {
    let body = serde_json::json!({
        "model": "gpt-5.5",
        "input": [
            {"role": "user", "content": [{"type": "input_text", "text": "private"}]},
            {"type": "function_call", "arguments": "{\"secret\":true}"}
        ],
        "reasoning": {"effort": "medium"},
        "tools": [{"type": "function", "name": "hidden"}]
    });

    let summary = summarize_request(ApiFamily::Responses, body.as_object().unwrap());

    assert_eq!(summary.input_item_count, 2);
    assert_eq!(summary.tool_definition_count, 1);
    assert_eq!(summary.tool_result_count, 1);
    assert_eq!(summary.effort.as_deref(), Some("medium"));
    let debug = format!("{summary:?}");
    assert!(!debug.contains("private"));
    assert!(!debug.contains("secret"));
    assert!(!debug.contains("hidden"));
}

#[test]
fn request_size_summary_reports_structure_without_content() {
    let body = serde_json::json!({
        "model": "gpt-5.5",
        "input": [
            {"type": "reasoning", "encrypted_content": "private reasoning"},
            {"type": "function_call_output", "call_id": "secret-call", "output": "private output"},
            {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "private prompt"}]}
        ],
        "tools": [{"type": "function", "name": "private_tool"}]
    });

    let summary = summarize_request_sizes(body.as_object().unwrap());

    assert!(summary.body_bytes >= summary.input_bytes);
    assert!(summary.input_tool_bytes > 0);
    assert!(summary.input_reasoning_bytes > 0);
    assert!(summary.tools_bytes > 0);
    assert!(summary.largest_input_item_bytes > 0);
    assert!(summary.largest_input_item_index.is_some());
    let debug = format!("{summary:?}");
    assert!(!debug.contains("private reasoning"));
    assert!(!debug.contains("private output"));
    assert!(!debug.contains("private prompt"));
    assert!(!debug.contains("private_tool"));
    assert!(!debug.contains("secret-call"));
}

#[test]
fn request_size_summary_allowlists_logged_item_type_and_role() {
    let body = serde_json::json!({
        "input": [{
            "type": "private type value",
            "role": "private role value",
            "content": "x".repeat(100)
        }]
    });

    let summary = summarize_request_sizes(body.as_object().unwrap());

    assert_eq!(summary.largest_input_item_type, Some("unknown"));
    assert_eq!(summary.largest_input_item_role, Some("unknown"));
    let debug = format!("{summary:?}");
    assert!(!debug.contains("private type value"));
    assert!(!debug.contains("private role value"));
}
