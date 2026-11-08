//! Pure translation between conversational client APIs and the Copilot
//! decisions (`/v1/decisions`) and SystemOne (`/v1/systemone`) endpoints.

use serde_json::{Map, Value, json};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DecisionsEndpoint {
    Decisions,
    SystemOne,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DecisionTurn {
    pub role: &'static str,
    pub text: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DecisionsError {
    MissingUserMessage,
    MalformedUpstream,
}

fn turn(role: &'static str, text: String) -> DecisionTurn {
    DecisionTurn { role, text }
}

/// Joins the text of a string or an array of typed text parts.
fn content_text(content: Option<&Value>, text_types: &[&str]) -> String {
    match content {
        Some(Value::String(text)) => text.clone(),
        Some(Value::Array(parts)) => parts
            .iter()
            .filter(|part| {
                part.get("type")
                    .and_then(Value::as_str)
                    .is_some_and(|kind| text_types.contains(&kind))
            })
            .filter_map(|part| part.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

fn conversational_role(role: Option<&str>) -> Option<&'static str> {
    match role? {
        "system" | "developer" => Some("system"),
        "user" => Some("user"),
        "assistant" => Some("assistant"),
        _ => None,
    }
}

fn message_turns(messages: Option<&Value>, text_types: &[&str]) -> Vec<DecisionTurn> {
    messages
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|message| {
            let role = conversational_role(message.get("role").and_then(Value::as_str))?;
            Some(turn(role, content_text(message.get("content"), text_types)))
        })
        .collect()
}

pub fn turns_from_openai_chat(body: &Map<String, Value>) -> Vec<DecisionTurn> {
    message_turns(body.get("messages"), &["text"])
}

pub fn turns_from_anthropic(body: &Map<String, Value>) -> Vec<DecisionTurn> {
    let mut turns = vec![turn("system", content_text(body.get("system"), &["text"]))];
    turns.extend(message_turns(body.get("messages"), &["text"]));
    turns
}

pub fn turns_from_responses(body: &Map<String, Value>) -> Vec<DecisionTurn> {
    let mut turns = vec![turn("system", content_text(body.get("instructions"), &[]))];
    match body.get("input") {
        Some(Value::String(text)) => turns.push(turn("user", text.clone())),
        input => {
            let messages = input
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter(|item| {
                    item.get("type")
                        .and_then(Value::as_str)
                        .is_none_or(|kind| kind == "message")
                })
                .cloned()
                .collect::<Vec<_>>();
            turns.extend(message_turns(
                Some(&Value::Array(messages)),
                &["input_text", "output_text"],
            ));
        }
    }
    turns
}

pub fn build_upstream_request(
    endpoint: DecisionsEndpoint,
    copilot_model: &str,
    turns: &[DecisionTurn],
) -> Result<Map<String, Value>, DecisionsError> {
    let question = turns
        .iter()
        .rev()
        .find(|turn| turn.role == "user")
        .map(|turn| turn.text.trim())
        .filter(|text| !text.is_empty())
        .ok_or(DecisionsError::MissingUserMessage)?;
    let history = turns
        .iter()
        .filter(|turn| !turn.text.trim().is_empty())
        .map(|turn| json!({"role": turn.role, "content": turn.text}))
        .collect::<Vec<_>>();
    let (history_key, questions) = match endpoint {
        DecisionsEndpoint::Decisions => (
            "input",
            json!([{"type": "predicate", "name": "answer", "instructions": question}]),
        ),
        DecisionsEndpoint::SystemOne => (
            "state",
            json!({"answer": {"type": "noul", "instructions": question}}),
        ),
    };
    let mut body = Map::new();
    body.insert("model".into(), json!(copilot_model));
    body.insert(history_key.into(), Value::Array(history));
    body.insert("questions".into(), questions);
    Ok(body)
}

pub fn answer_probability(endpoint: DecisionsEndpoint, upstream: &Value) -> Option<f64> {
    let value = match endpoint {
        DecisionsEndpoint::Decisions => {
            let answers = upstream.get("answers")?.as_array()?;
            answers
                .iter()
                .find(|answer| answer.get("name").and_then(Value::as_str) == Some("answer"))
                .or_else(|| answers.first())?
                .get("probability")
        }
        DecisionsEndpoint::SystemOne => upstream.pointer("/answers/answer/noul"),
    };
    value
        .and_then(Value::as_f64)
        .filter(|probability| probability.is_finite())
        .map(|probability| probability.clamp(0.0, 1.0))
}

pub fn answer_text(probability: f64) -> String {
    let verdict = if probability >= 0.5 { "Yes" } else { "No" };
    format!("{verdict} (probability {probability:.2})")
}

fn usage(upstream: &Value) -> Value {
    let count = |key: &str| {
        upstream
            .pointer(&format!("/usage/{key}"))
            .and_then(Value::as_u64)
    };
    let input = count("input_tokens").unwrap_or(0);
    let output = count("output_tokens").unwrap_or(0);
    let total = count("total_tokens").unwrap_or(input + output);
    json!({"input_tokens": input, "output_tokens": output, "total_tokens": total})
}

/// Builds a completed Responses API object carrying the decision as text.
pub fn decision_to_responses(
    endpoint: DecisionsEndpoint,
    upstream: &Value,
    public_model: &str,
) -> Result<Value, DecisionsError> {
    let probability =
        answer_probability(endpoint, upstream).ok_or(DecisionsError::MalformedUpstream)?;
    let created_at = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs())
        .unwrap_or_default();
    Ok(json!({
        "id": format!("resp_{}", uuid::Uuid::new_v4().simple()),
        "object": "response",
        "created_at": created_at,
        "status": "completed",
        "model": public_model,
        "output": [{
            "type": "message",
            "id": format!("msg_{}", uuid::Uuid::new_v4().simple()),
            "status": "completed",
            "role": "assistant",
            "content": [{"type": "output_text", "text": answer_text(probability), "annotations": []}],
        }],
        "usage": usage(upstream),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn object(value: Value) -> Map<String, Value> {
        value.as_object().cloned().unwrap_or_default()
    }

    fn pairs(turns: &[DecisionTurn]) -> Vec<(&str, &str)> {
        turns
            .iter()
            .map(|turn| (turn.role, turn.text.as_str()))
            .collect()
    }

    #[test]
    fn answer_text_is_yes_from_half_inclusive_and_rounds_to_two_places() {
        assert_eq!(answer_text(0.5), "Yes (probability 0.50)");
        assert_eq!(answer_text(0.4999), "No (probability 0.50)");
        assert_eq!(answer_text(0.0), "No (probability 0.00)");
        assert_eq!(answer_text(1.0), "Yes (probability 1.00)");
        assert_eq!(answer_text(0.9940889), "Yes (probability 0.99)");
    }

    #[test]
    fn chat_turns_map_developer_to_system_and_keep_only_text_parts() {
        let body = object(json!({"messages": [
            {"role": "developer", "content": "be brief"},
            {"role": "user", "content": [
                {"type": "text", "text": "Is it blue?"},
                {"type": "image_url", "image_url": {"url": "data:"}}
            ]},
            {"role": "tool", "content": "ignored"}
        ]}));
        assert_eq!(
            pairs(&turns_from_openai_chat(&body)),
            [("system", "be brief"), ("user", "Is it blue?")]
        );
    }

    #[test]
    fn anthropic_turns_include_system_blocks() {
        let body = object(json!({
            "system": [{"type": "text", "text": "sys"}],
            "messages": [{"role": "user", "content": [{"type": "text", "text": "q"}]}]
        }));
        assert_eq!(
            pairs(&turns_from_anthropic(&body)),
            [("system", "sys"), ("user", "q")]
        );
    }

    #[test]
    fn responses_turns_accept_string_or_message_items_and_drop_other_items() {
        let text = object(json!({"instructions": "sys", "input": "q"}));
        assert_eq!(
            pairs(&turns_from_responses(&text)),
            [("system", "sys"), ("user", "q")]
        );
        let items = object(json!({"input": [
            {"role": "user", "content": [{"type": "input_text", "text": "a"}]},
            {"type": "message", "role": "assistant", "content": [{"type": "output_text", "text": "b"}]},
            {"type": "function_call_output", "role": "user", "output": "x"}
        ]}));
        assert_eq!(
            pairs(&turns_from_responses(&items)),
            [("system", ""), ("user", "a"), ("assistant", "b")]
        );
    }

    #[test]
    fn upstream_request_uses_last_user_turn_and_drops_empty_turns() {
        let turns = [
            turn("system", String::new()),
            turn("user", "first".into()),
            turn("assistant", "ok".into()),
            turn("user", "  last  ".into()),
        ];
        let body = build_upstream_request(DecisionsEndpoint::SystemOne, "m", &turns).unwrap();
        assert_eq!(
            Value::Object(body),
            json!({
                "model": "m",
                "state": [
                    {"role": "user", "content": "first"},
                    {"role": "assistant", "content": "ok"},
                    {"role": "user", "content": "  last  "}
                ],
                "questions": {"answer": {"type": "noul", "instructions": "last"}}
            })
        );
        assert_eq!(
            build_upstream_request(
                DecisionsEndpoint::Decisions,
                "m",
                &[turn("user", " ".into())]
            ),
            Err(DecisionsError::MissingUserMessage)
        );
    }

    #[test]
    fn answer_probability_prefers_named_answer_and_clamps() {
        let upstream = json!({"answers": [
            {"name": "other", "probability": 0.1},
            {"name": "answer", "probability": 1.7}
        ]});
        assert_eq!(
            answer_probability(DecisionsEndpoint::Decisions, &upstream),
            Some(1.0)
        );
        let unnamed = json!({"answers": [{"probability": -2.0}]});
        assert_eq!(
            answer_probability(DecisionsEndpoint::Decisions, &unnamed),
            Some(0.0)
        );
        let text = json!({"answers": {"answer": {"noul": "high"}}});
        assert_eq!(
            answer_probability(DecisionsEndpoint::SystemOne, &text),
            None
        );
    }

    #[test]
    fn usage_total_falls_back_to_input_plus_output() {
        let upstream = json!({
            "answers": {"answer": {"noul": 0.2}},
            "usage": {"input_tokens": 41, "output_tokens": 1}
        });
        let response =
            decision_to_responses(DecisionsEndpoint::SystemOne, &upstream, "public").unwrap();
        assert_eq!(
            response["usage"],
            json!({"input_tokens": 41, "output_tokens": 1, "total_tokens": 42})
        );
        assert_eq!(response["model"], "public");
        let without_usage = json!({"answers": {"answer": {"noul": 0.2}}});
        assert_eq!(
            decision_to_responses(DecisionsEndpoint::SystemOne, &without_usage, "public").unwrap()
                ["usage"],
            json!({"input_tokens": 0, "output_tokens": 0, "total_tokens": 0})
        );
        assert_eq!(
            response["output"][0]["content"][0]["text"],
            "No (probability 0.20)"
        );
    }
}
