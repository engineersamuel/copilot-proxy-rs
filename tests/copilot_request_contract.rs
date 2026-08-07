use copilot_proxy_rs::copilot::request::{
    adapt_openai_reasoning_effort, adapt_responses_reasoning_effort,
    adapt_responses_tools_for_copilot, adapt_thinking_for_copilot, base_copilot_request_headers,
    clamp_effort, compute_initiator, filter_anthropic_beta_header, strip_structured_output,
};
use copilot_proxy_rs::models::{EffortLevel, SupportedEfforts};

#[test]
fn base_headers_match_python_copilot_fingerprint() {
    let headers = base_copilot_request_headers("token-123");

    assert_eq!(headers.get("Authorization").unwrap(), "Bearer token-123");
    assert_eq!(
        headers.get("Copilot-Integration-Id").unwrap(),
        "vscode-chat"
    );
    assert_eq!(headers.get("Editor-Version").unwrap(), "vscode/1.100.0");
    assert_eq!(
        headers.get("Editor-Plugin-Version").unwrap(),
        "copilot-chat/0.27.2025040201"
    );
    assert_eq!(headers.get("User-Agent").unwrap(), "GithubCopilot/1.155.0");
    assert_eq!(headers.get("X-GitHub-Api-Version").unwrap(), "2026-06-01");
}

#[test]
fn initiator_is_agent_for_tool_continuations_and_user_for_plain_user_turns() {
    let plain = serde_json::json!({
        "messages": [{"role": "user", "content": "hello"}]
    });
    assert_eq!(compute_initiator(plain.as_object().unwrap(), false), "user");

    let tool = serde_json::json!({
        "messages": [{"role": "tool", "content": "tool result"}]
    });
    assert_eq!(compute_initiator(tool.as_object().unwrap(), false), "agent");
}

#[test]
fn anthropic_beta_header_normalizes_and_forwards_all_tokens() {
    assert_eq!(
        filter_anthropic_beta_header(
            "interleaved-thinking-2025-05-14, context-management-2025-06-27, unknown-beta, advanced-tool-use-2025-01-01, extended-cache-ttl-2025-04-11"
        ),
        Some(
            "interleaved-thinking-2025-05-14, context-management-2025-06-27, unknown-beta, advanced-tool-use-2025-01-01, extended-cache-ttl-2025-04-11"
                .to_string()
        )
    );
    assert_eq!(
        filter_anthropic_beta_header("unknown-beta"),
        Some("unknown-beta".to_string())
    );
    assert_eq!(
        filter_anthropic_beta_header("  a , , b  "),
        Some("a, b".to_string())
    );
    assert_eq!(filter_anthropic_beta_header(""), None);
    assert_eq!(filter_anthropic_beta_header("  ,  , "), None);
}

fn supported(efforts: &[EffortLevel]) -> SupportedEfforts {
    SupportedEfforts::new(efforts.to_vec()).unwrap()
}

#[test]
fn clamp_effort_uses_python_rank_order_and_unknown_values_clamp_to_highest() {
    let efforts = supported(&[EffortLevel::Low, EffortLevel::Medium, EffortLevel::High]);

    assert_eq!(clamp_effort("medium", &efforts), Some("medium"));
    assert_eq!(clamp_effort("max", &efforts), Some("high"));
    assert_eq!(clamp_effort("turbo", &efforts), Some("high"));
}

#[test]
fn clamp_effort_below_supported_range_clamps_to_lowest_supported() {
    let efforts = supported(&[EffortLevel::Medium, EffortLevel::High]);

    assert_eq!(clamp_effort("low", &efforts), Some("medium"));
}

#[test]
fn openai_reasoning_effort_is_clamped_or_removed() {
    let efforts = supported(&[EffortLevel::Low, EffortLevel::Medium, EffortLevel::High]);
    let mut supported_body = serde_json::json!({
        "model": "gpt-effort",
        "reasoning_effort": "max",
        "messages": [{"role": "user", "content": "hi"}]
    })
    .as_object()
    .unwrap()
    .clone();

    adapt_openai_reasoning_effort(&mut supported_body, Some(&efforts));
    assert_eq!(supported_body["reasoning_effort"], "high");

    let mut unsupported_body = serde_json::json!({
        "model": "gpt-no-effort",
        "reasoning_effort": "high",
        "messages": [{"role": "user", "content": "hi"}]
    })
    .as_object()
    .unwrap()
    .clone();
    adapt_openai_reasoning_effort(&mut unsupported_body, None);
    assert!(unsupported_body.get("reasoning_effort").is_none());
}

#[test]
fn structured_output_strip_preserves_output_config_effort() {
    let mut body = serde_json::json!({
        "model": "claude-sonnet-4.6",
        "output_config": {
            "format": {"type": "json_schema", "schema": {"type": "object"}},
            "effort": "max"
        }
    })
    .as_object()
    .unwrap()
    .clone();

    strip_structured_output(&mut body);

    assert_eq!(body["output_config"]["effort"], "max");
    assert!(body["output_config"].get("format").is_none());
}

#[test]
fn anthropic_thinking_adapts_effort_and_mode_for_copilot() {
    let efforts = supported(&[
        EffortLevel::Low,
        EffortLevel::Medium,
        EffortLevel::High,
        EffortLevel::XHigh,
        EffortLevel::Max,
    ]);
    let mut adaptive_only = serde_json::json!({
        "model": "claude-opus-4.8",
        "thinking": {"type": "enabled", "budget_tokens": 2048},
        "output_config": {
            "format": {"type": "json_schema"},
            "effort": "max"
        }
    })
    .as_object()
    .unwrap()
    .clone();

    adapt_thinking_for_copilot(&mut adaptive_only, "claude-opus-4.8", Some(&efforts));

    assert_eq!(adaptive_only["thinking"]["type"], "adaptive");
    assert!(adaptive_only.get("output_config").is_none());

    let mut non_adaptive = serde_json::json!({
        "model": "claude-3.5-sonnet",
        "thinking": {"type": "adaptive"}
    })
    .as_object()
    .unwrap()
    .clone();
    adapt_thinking_for_copilot(&mut non_adaptive, "claude-3.5-sonnet", None);
    assert_eq!(non_adaptive["thinking"]["type"], "enabled");
    assert_eq!(non_adaptive["thinking"]["budget_tokens"], 10000);
    assert!(non_adaptive.get("output_config").is_none());

    let mut sonnet_five = serde_json::json!({
        "model": "claude-sonnet-5",
        "thinking": {"type": "adaptive", "display": "omitted"},
        "context_management": {
            "edits": [{"type": "clear_thinking_20251015", "keep": "all"}]
        },
        "output_config": {"effort": "medium"}
    })
    .as_object()
    .unwrap()
    .clone();
    adapt_thinking_for_copilot(&mut sonnet_five, "claude-sonnet-5", Some(&efforts));
    assert_eq!(sonnet_five["thinking"]["type"], "adaptive");
    assert_eq!(sonnet_five["thinking"]["display"], "omitted");
    assert_eq!(sonnet_five["output_config"]["effort"], "medium");
    assert!(sonnet_five.get("context_management").is_none());
}

#[test]
fn claude_opus_five_enabled_thinking_becomes_adaptive() {
    let mut body = serde_json::json!({
        "thinking": {"type": "enabled", "budget_tokens": 2048}
    })
    .as_object()
    .unwrap()
    .clone();

    adapt_thinking_for_copilot(&mut body, "claude-opus-5", None);

    assert_eq!(body["thinking"], serde_json::json!({"type": "adaptive"}));
}

#[test]
fn claude_opus_five_adaptive_thinking_remains_adaptive() {
    let mut body = serde_json::json!({
        "thinking": {"type": "adaptive"}
    })
    .as_object()
    .unwrap()
    .clone();

    adapt_thinking_for_copilot(&mut body, "claude-opus-5", None);

    assert_eq!(body["thinking"], serde_json::json!({"type": "adaptive"}));
}

#[test]
fn responses_reasoning_effort_is_clamped_or_removed_without_losing_other_fields() {
    let efforts = supported(&[EffortLevel::Low, EffortLevel::Medium, EffortLevel::High]);
    let mut supported_body = serde_json::json!({
        "model": "gpt-effort",
        "input": "hi",
        "reasoning": {"effort": "max", "summary": "auto"},
        "metadata": {"keep": true}
    })
    .as_object()
    .unwrap()
    .clone();

    adapt_responses_reasoning_effort(&mut supported_body, Some(&efforts));
    assert_eq!(supported_body["reasoning"]["effort"], "high");
    assert_eq!(supported_body["reasoning"]["summary"], "auto");
    assert_eq!(supported_body["metadata"]["keep"], true);

    let mut unsupported_body = serde_json::json!({
        "model": "gpt-no-effort",
        "input": "hi",
        "reasoning": {"effort": "high"}
    })
    .as_object()
    .unwrap()
    .clone();
    adapt_responses_reasoning_effort(&mut unsupported_body, None);
    assert!(unsupported_body.get("reasoning").is_none());
}

#[test]
fn responses_image_generation_tool_is_stripped_for_copilot() {
    let mut body = serde_json::json!({
        "model": "gpt-5.5",
        "input": "hello",
        "tools": [
            {"type": "image_generation", "partial_images": 1},
            {"type": "function", "name": "safe_tool", "parameters": {"type": "object"}}
        ],
        "tool_choice": "auto",
        "include": ["image_generation_call.results", "reasoning.encrypted_content"]
    })
    .as_object()
    .unwrap()
    .clone();

    adapt_responses_tools_for_copilot(&mut body);

    assert_eq!(
        body["tools"],
        serde_json::json!([
            {"type": "function", "name": "safe_tool", "parameters": {"type": "object"}}
        ])
    );
    assert_eq!(body["tool_choice"], "auto");
    assert_eq!(
        body["include"],
        serde_json::json!(["reasoning.encrypted_content"])
    );
}

#[test]
fn responses_image_generation_only_request_removes_tool_controls() {
    let mut body = serde_json::json!({
        "model": "gpt-5.5",
        "input": "hello",
        "tools": [{"type": "image_generation"}],
        "tool_choice": "auto",
        "include": ["image_generation_call.results"]
    })
    .as_object()
    .unwrap()
    .clone();

    adapt_responses_tools_for_copilot(&mut body);

    assert!(body.get("tools").is_none());
    assert!(body.get("tool_choice").is_none());
    assert!(body.get("include").is_none());
}

#[test]
fn responses_image_generation_tool_choice_is_removed_without_tools() {
    let mut body = serde_json::json!({
        "model": "gpt-5.5",
        "input": "hello",
        "tool_choice": {"type": "image_generation"}
    })
    .as_object()
    .unwrap()
    .clone();

    adapt_responses_tools_for_copilot(&mut body);

    assert!(body.get("tool_choice").is_none());
}

#[test]
fn responses_input_namespace_empty_description_gets_copilot_fallback() {
    let mut body = serde_json::json!({
        "model": "gpt-5.6-sol",
        "input": [{
            "type": "additional_tools",
            "role": "developer",
            "tools": [{
                "type": "namespace",
                "name": "functions",
                "description": "",
                "tools": [{
                    "type": "custom",
                    "name": "exec",
                    "description": "Run JavaScript code",
                    "format": {
                        "type": "grammar",
                        "syntax": "lark",
                        "definition": "start: SOURCE"
                    }
                }, {
                    "type": "function",
                    "name": "wait",
                    "description": "Wait for a running operation",
                    "strict": false,
                    "parameters": {
                        "type": "object",
                        "properties": {"id": {"type": "string"}},
                        "required": ["id"]
                    }
                }]
            }],
            "content": [{"type": "input_text", "text": "Use available tools."}]
        }, {
            "role": "user",
            "content": [{"type": "input_text", "text": "Reply exactly OK"}]
        }],
        "stream": true
    })
    .as_object()
    .unwrap()
    .clone();

    let nested_before = body["input"][0]["tools"][0]["tools"].clone();
    adapt_responses_tools_for_copilot(&mut body);

    assert_eq!(
        body["input"][0]["tools"][0]["description"],
        "Tools in the functions namespace."
    );
    assert_eq!(body["input"][0]["tools"][0]["type"], "namespace");
    assert_eq!(body["input"][0]["tools"][0]["name"], "functions");
    assert_eq!(body["input"][0]["tools"][0]["tools"], nested_before);
    assert_eq!(body["stream"], true);
    assert_eq!(body["model"], "gpt-5.6-sol");
}

#[test]
fn responses_input_namespace_missing_or_blank_description_gets_fallback() {
    let mut body = serde_json::json!({
        "input": [{
            "tools": [
                {"type": "namespace", "name": "functions"},
                {"type": "namespace", "name": "browser", "description": "   \t\n"},
                {"type": "namespace", "name": "other", "description": 12}
            ]
        }]
    })
    .as_object()
    .unwrap()
    .clone();

    adapt_responses_tools_for_copilot(&mut body);

    assert_eq!(
        body["input"][0]["tools"][0]["description"],
        "Tools in the functions namespace."
    );
    assert_eq!(
        body["input"][0]["tools"][1]["description"],
        "Tools in the browser namespace."
    );
    assert_eq!(
        body["input"][0]["tools"][2]["description"],
        "Tools in the other namespace."
    );
}

#[test]
fn responses_input_namespace_nonempty_description_is_preserved() {
    let mut body = serde_json::json!({
        "input": [{
            "tools": [{
                "type": "namespace",
                "name": "browser",
                "description": "Tools in the browser namespace."
            }]
        }]
    })
    .as_object()
    .unwrap()
    .clone();

    adapt_responses_tools_for_copilot(&mut body);

    assert_eq!(
        body["input"][0]["tools"][0]["description"]
            .as_str()
            .unwrap(),
        "Tools in the browser namespace."
    );
}

#[test]
fn responses_input_namespace_malformed_entries_are_unchanged() {
    let mut body = serde_json::json!({
        "input": [{
            "tools": [
                {"type": "namespace", "name": "", "description": ""},
                {"type": "namespace", "name": "   ", "description": ""},
                {"type": "namespace", "name": 7, "description": ""},
                {"type": "namespace", "description": ""},
                {"type": "function", "name": "wait", "description": ""},
                "not-an-object",
                {"type": "namespace", "name": "ok", "description": ""}
            ]
        }, {
            "tools": "not-an-array"
        }, "not-an-object"],
        "tools": [{
            "type": "namespace",
            "name": "functions",
            "description": ""
        }]
    })
    .as_object()
    .unwrap()
    .clone();
    let before = body.clone();

    adapt_responses_tools_for_copilot(&mut body);

    // Malformed / non-namespace / top-level tools stay put; only the valid
    // nested namespace at input[0].tools[6] is filled.
    assert_eq!(body["input"][0]["tools"][0], before["input"][0]["tools"][0]);
    assert_eq!(body["input"][0]["tools"][1], before["input"][0]["tools"][1]);
    assert_eq!(body["input"][0]["tools"][2], before["input"][0]["tools"][2]);
    assert_eq!(body["input"][0]["tools"][3], before["input"][0]["tools"][3]);
    assert_eq!(body["input"][0]["tools"][4], before["input"][0]["tools"][4]);
    assert_eq!(body["input"][0]["tools"][5], before["input"][0]["tools"][5]);
    assert_eq!(
        body["input"][0]["tools"][6]["description"],
        "Tools in the ok namespace."
    );
    assert_eq!(body["input"][1]["tools"], "not-an-array");
    assert_eq!(body["input"][2], "not-an-object");
    assert_eq!(
        body["tools"][0]["description"], "",
        "top-level tools must not be mutated by input namespace normalization"
    );
}

#[test]
fn responses_input_namespace_normalization_is_idempotent() {
    let mut body = serde_json::json!({
        "input": [{
            "tools": [{
                "type": "namespace",
                "name": "functions",
                "description": ""
            }]
        }]
    })
    .as_object()
    .unwrap()
    .clone();

    adapt_responses_tools_for_copilot(&mut body);
    let once = body.clone();
    adapt_responses_tools_for_copilot(&mut body);
    assert_eq!(body, once);
}
