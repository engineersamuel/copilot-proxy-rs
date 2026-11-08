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
        let (body, question) =
            build_upstream_request(DecisionsEndpoint::SystemOne, "m", &turns).unwrap();
        assert_eq!(question, DecisionQuestion::YesNo("last".into()));
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

    fn option_pairs(options: &DecisionOptions) -> Vec<(&str, &str)> {
        options
            .options
            .iter()
            .map(|option| (option.label.as_str(), option.text.as_str()))
            .collect()
    }

    fn options(labels: &[&str]) -> Vec<DecisionOption> {
        labels
            .iter()
            .map(|label| DecisionOption {
                label: (*label).to_string(),
                text: String::new(),
            })
            .collect()
    }

    #[test]
    fn extract_options_reads_colon_and_dash_labels_and_cuts_the_trailing_question() {
        let found = extract_options(
            "Ship by Friday? Option A: ship now. Option B - wait a week. Which option and why?",
        )
        .unwrap();
        assert_eq!(found.context, "Ship by Friday?");
        assert_eq!(
            option_pairs(&found),
            [("Option A", "ship now"), ("Option B", "wait a week")]
        );
    }

    #[test]
    fn extract_options_accepts_lowercase_and_numeric_labels() {
        let lower = extract_options("option a: tea\noption b: coffee").unwrap();
        assert_eq!(lower.context, "");
        assert_eq!(
            option_pairs(&lower),
            [("Option A", "tea"), ("Option B", "coffee")]
        );
        let numeric = extract_options("Pick. Option 1: red. Option 2: blue.").unwrap();
        assert_eq!(
            option_pairs(&numeric),
            [("Option 1", "red"), ("Option 2", "blue")]
        );
        assert_eq!(numeric.options[1].key(), "option_2");
        assert_eq!(lower.options[0].key(), "option_a");
    }

    #[test]
    fn extract_options_reads_a_numbered_list() {
        let found = extract_options("Where should we eat?\n1. tacos\n2) noodles\nThanks").unwrap();
        assert_eq!(found.context, "Where should we eat?");
        assert_eq!(
            option_pairs(&found),
            [("Option 1", "tacos"), ("Option 2", "noodles")]
        );
    }

    #[test]
    fn extract_options_needs_two_distinct_options() {
        assert_eq!(extract_options("Is the sky blue on a clear day?"), None);
        assert_eq!(
            extract_options("Option A: go. Which option should we pick?"),
            None
        );
        assert_eq!(extract_options("Option A: go. Option A: stay."), None);
        assert_eq!(extract_options("Steps:\n1. only one"), None);
        assert_eq!(
            extract_options("Our options are many. Adoption is high."),
            None
        );
    }

    #[test]
    fn normalize_scales_to_one_clamps_and_splits_evenly_without_signal() {
        assert_eq!(normalize(&[0.2, 0.6]), [0.25, 0.75]);
        assert_eq!(normalize(&[-1.0, 2.0]), [0.0, 1.0]);
        assert_eq!(normalize(&[0.0, 0.0]), [0.5, 0.5]);
        assert_eq!(normalize(&[f64::NAN, 0.3]), [0.5, 0.5]);
    }

    #[test]
    fn render_options_leads_with_the_favorite_then_others_descending() {
        assert_eq!(
            render_options(
                &options(&["Option A", "Option B", "Option C"]),
                &[0.2, 0.5, 0.3]
            ),
            "Option B (0.50); Option C 0.30; Option A 0.20\nReason: Option B is favored over Option C (0.50 vs 0.30)."
        );
        assert_eq!(
            render_options(&options(&["Option A", "Option B"]), &[0.5, 0.5]),
            "Option A (0.50); Option B 0.50\nReason: Option A is favored over Option B (0.50 vs 0.50)."
        );
    }

    fn options_turns() -> [DecisionTurn; 1] {
        [turn(
            "user",
            "Lunch? Option A: tacos. Option B: noodles. Which one?".into(),
        )]
    }

    #[test]
    fn options_request_asks_one_question_per_option_on_both_endpoints() {
        let (luna, _) =
            build_upstream_request(DecisionsEndpoint::Decisions, "m", &options_turns()).unwrap();
        assert_eq!(
            luna["questions"],
            json!([
                {"type": "predicate", "name": "option_a", "instructions": "Lunch? Should we pick Option A: tacos?"},
                {"type": "predicate", "name": "option_b", "instructions": "Lunch? Should we pick Option B: noodles?"}
            ])
        );
        let (systemone, _) =
            build_upstream_request(DecisionsEndpoint::SystemOne, "m", &options_turns()).unwrap();
        assert_eq!(
            systemone["questions"],
            json!({
                "option_a": {"type": "noul", "instructions": "Lunch? Should we pick Option A: tacos?"},
                "option_b": {"type": "noul", "instructions": "Lunch? Should we pick Option B: noodles?"}
            })
        );
    }

    fn answer(
        endpoint: DecisionsEndpoint,
        turns: &[DecisionTurn],
        upstream: Value,
    ) -> Result<Value, DecisionsError> {
        let (_, question) = build_upstream_request(endpoint, "m", turns).unwrap();
        decision_to_responses(endpoint, &upstream, &question, "public")
            .map(|response| response["output"][0]["content"][0]["text"].clone())
    }

    #[test]
    fn decisions_answers_match_by_name_and_tolerate_keyed_objects_and_string_numbers() {
        let sky = [turn("user", "Is it blue?".into())];
        let keyed = json!({"answers": {"answer": {"probability": "0.8"}}});
        assert_eq!(
            answer(DecisionsEndpoint::Decisions, &sky, keyed),
            Ok(json!("Yes (probability 0.80)"))
        );
        let clamped = json!({"answers": [
            {"name": "other", "probability": 0.1},
            {"name": "answer", "probability": 1.7}
        ]});
        assert_eq!(
            answer(DecisionsEndpoint::Decisions, &sky, clamped),
            Ok(json!("Yes (probability 1.00)"))
        );
        let options = json!({"answers": [
            {"name": "option_b", "probability": 0.6},
            {"name": "option_a", "probability": 0.2}
        ]});
        assert_eq!(
            answer(DecisionsEndpoint::Decisions, &options_turns(), options),
            Ok(json!(
                "Option B (0.75); Option A 0.25\nReason: Option B is favored over Option A (0.75 vs 0.25)."
            ))
        );
    }

    #[test]
    fn refusals_and_missing_answers_are_distinct_errors() {
        let sky = [turn("user", "Which is best?".into())];
        let refusal = json!({"answers": [{"type": "refusal", "name": "answer"}]});
        assert_eq!(
            answer(DecisionsEndpoint::Decisions, &sky, refusal),
            Err(DecisionsError::Refused)
        );
        let missing_option = json!({"answers": {"option_a": {"noul": 0.4, "type": "noul"}}});
        assert_eq!(
            answer(
                DecisionsEndpoint::SystemOne,
                &options_turns(),
                missing_option
            ),
            Err(DecisionsError::MalformedUpstream("no_answer"))
        );
        let text = json!({"answers": {"answer": {"noul": "high"}}});
        assert_eq!(
            answer(DecisionsEndpoint::SystemOne, &sky, text),
            Err(DecisionsError::MalformedUpstream("no_probability"))
        );
        assert_eq!(
            answer(DecisionsEndpoint::Decisions, &sky, json!({"answers": []})),
            Err(DecisionsError::MalformedUpstream("no_answer"))
        );
        assert_eq!(
            answer(DecisionsEndpoint::Decisions, &sky, json!({})),
            Err(DecisionsError::MalformedUpstream("no_answers"))
        );
    }

    #[test]
    fn usage_total_falls_back_to_input_plus_output() {
        let upstream = json!({
            "answers": {"answer": {"noul": 0.2}},
            "usage": {"input_tokens": 41, "output_tokens": 1}
        });
        let question = DecisionQuestion::YesNo("q".into());
        let response =
            decision_to_responses(DecisionsEndpoint::SystemOne, &upstream, &question, "public")
                .unwrap();
        assert_eq!(
            response["usage"],
            json!({"input_tokens": 41, "output_tokens": 1, "total_tokens": 42})
        );
        assert_eq!(response["model"], "public");
        let without_usage = json!({"answers": {"answer": {"noul": 0.2}}});
        assert_eq!(
            decision_to_responses(
                DecisionsEndpoint::SystemOne,
                &without_usage,
                &question,
                "public"
            )
            .unwrap()["usage"],
            json!({"input_tokens": 0, "output_tokens": 0, "total_tokens": 0})
        );
        assert_eq!(
            response["output"][0]["content"][0]["text"],
            "No (probability 0.20)"
        );
    }
}
