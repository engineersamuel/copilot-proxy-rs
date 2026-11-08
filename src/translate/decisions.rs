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
    /// The upstream answered with a `refusal` instead of a probability.
    Refused,
    /// Carries a low-cardinality reason that is safe to log.
    MalformedUpstream(&'static str),
}

/// One labeled choice found in the user's question, e.g. `Option B`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DecisionOption {
    pub label: String,
    pub text: String,
}

impl DecisionOption {
    /// Upstream question name, e.g. `option_b`.
    pub fn key(&self) -> String {
        self.label.to_lowercase().replace(' ', "_")
    }
}

/// Two or more distinct options plus the text that precedes them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DecisionOptions {
    pub context: String,
    pub options: Vec<DecisionOption>,
}

impl DecisionOptions {
    fn instructions(&self, option: &DecisionOption) -> String {
        format!(
            "{} Should we pick {}: {}?",
            self.context, option.label, option.text
        )
        .trim_start()
        .to_string()
    }
}

/// What the last user message asks: a yes/no question or a pick among options.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DecisionQuestion {
    YesNo(String),
    Options(DecisionOptions),
}

impl DecisionQuestion {
    fn parse(text: &str) -> Self {
        extract_options(text).map_or_else(|| Self::YesNo(text.to_string()), Self::Options)
    }

    /// `(name, instructions)` for each upstream question.
    fn upstream_questions(&self) -> Vec<(String, String)> {
        match self {
            Self::YesNo(text) => vec![("answer".to_string(), text.clone())],
            Self::Options(found) => found
                .options
                .iter()
                .map(|option| (option.key(), found.instructions(option)))
                .collect(),
        }
    }
}

/// Finds `Option X: ..` / `Option X - ..` labels, else a numbered list.
pub fn extract_options(text: &str) -> Option<DecisionOptions> {
    let found = labeled_options(text).or_else(|| numbered_options(text))?;
    let mut keys = found
        .options
        .iter()
        .map(DecisionOption::key)
        .collect::<Vec<_>>();
    keys.sort();
    keys.dedup();
    let usable = keys.len() == found.options.len()
        && found.options.len() >= 2
        && found.options.iter().all(|option| !option.text.is_empty());
    usable.then_some(found)
}

struct LabelMarker {
    start: usize,
    body: usize,
    label: String,
}

/// Parses `option <letter|number> [:|-]` at `start`, which must begin a word.
fn label_marker(text: &str, start: usize) -> Option<LabelMarker> {
    let word_start = text[..start]
        .chars()
        .next_back()
        .is_none_or(|before| !before.is_alphanumeric());
    let rest = text.get(start..start + 6)?;
    if !word_start || !rest.eq_ignore_ascii_case("option") {
        return None;
    }
    let after = &text[start + 6..];
    let token_text = after.trim_start_matches(' ');
    if token_text.len() == after.len() {
        return None;
    }
    let token_len = token_text
        .find(|c: char| !c.is_ascii_alphanumeric())
        .unwrap_or(token_text.len());
    let token = &token_text[..token_len];
    let single_letter = token.len() == 1 && token.chars().all(|c| c.is_ascii_alphabetic());
    let number = !token.is_empty() && token.chars().all(|c| c.is_ascii_digit());
    let separator = token_text[token_len..].trim_start_matches(' ');
    let body = separator.strip_prefix([':', '-'])?;
    (single_letter || number).then(|| LabelMarker {
        start,
        body: text.len() - body.len(),
        label: format!("Option {}", token.to_uppercase()),
    })
}

fn labeled_options(text: &str) -> Option<DecisionOptions> {
    let markers = text
        .char_indices()
        .filter_map(|(start, _)| label_marker(text, start))
        .collect::<Vec<_>>();
    let first = markers.first()?;
    let ends = markers.iter().skip(1).map(|marker| marker.start);
    let options = markers
        .iter()
        .zip(ends.chain([text.len()]))
        .map(|(marker, end)| DecisionOption {
            label: marker.label.clone(),
            text: first_sentence(&text[marker.body..end]).to_string(),
        })
        .collect();
    Some(DecisionOptions {
        context: text[..first.start].trim().to_string(),
        options,
    })
}

/// Parses `12. text` or `12) text` into its number and text.
fn numbered_item(line: &str) -> Option<(u64, &str)> {
    let line = line.trim_start();
    let digits = line.find(|c: char| !c.is_ascii_digit())?;
    let number = line[..digits].parse().ok()?;
    let item = line[digits..].strip_prefix(['.', ')'])?;
    item.starts_with(char::is_whitespace)
        .then(|| (number, first_sentence(item)))
}

fn numbered_options(text: &str) -> Option<DecisionOptions> {
    let lines = text.lines().collect::<Vec<_>>();
    let first = lines
        .iter()
        .position(|line| numbered_item(line).is_some())?;
    let (start, _) = numbered_item(lines[first])?;
    // Blank lines and continuation text between items do not end the list.
    let mut expected = start;
    let mut options = Vec::new();
    for line in &lines[first..] {
        if let Some((number, item)) = numbered_item(line).filter(|(n, _)| *n == expected) {
            options.push(DecisionOption {
                label: format!("Option {number}"),
                text: item.to_string(),
            });
            expected += 1;
        }
    }
    Some(DecisionOptions {
        context: lines[..first].join("\n").trim().to_string(),
        options,
    })
}

/// The first line's first sentence, without its closing punctuation.
fn first_sentence(segment: &str) -> &str {
    let line = segment.trim().lines().next().unwrap_or_default();
    let end = line
        .char_indices()
        .find(|&(index, c)| {
            matches!(c, '.' | '?' | '!') && line[index + 1..].starts_with(char::is_whitespace)
        })
        .map_or(line.len(), |(index, _)| index);
    line[..end]
        .trim()
        .trim_end_matches(['.', '?', '!'])
        .trim_end()
}

/// Clamps to [0, 1] and scales to sum 1; splits evenly when there is no signal.
pub fn normalize(probabilities: &[f64]) -> Vec<f64> {
    let clamped = probabilities
        .iter()
        .map(|probability| probability.clamp(0.0, 1.0))
        .collect::<Vec<_>>();
    let sum = clamped.iter().sum::<f64>();
    if sum > 0.0 && sum.is_finite() {
        clamped
            .iter()
            .map(|probability| probability / sum)
            .collect()
    } else {
        vec![1.0 / clamped.len() as f64; clamped.len()]
    }
}

/// `Option B (0.72); Option A 0.28` plus a reason line; ties keep listed order.
pub fn render_options(options: &[DecisionOption], probabilities: &[f64]) -> String {
    let mut ranked = options
        .iter()
        .map(|option| option.label.as_str())
        .zip(probabilities.iter().copied())
        .collect::<Vec<_>>();
    ranked.sort_by(|left, right| right.1.total_cmp(&left.1));
    let Some(((top, top_probability), others)) = ranked.split_first() else {
        return String::new();
    };
    let mut line = format!("{top} ({top_probability:.2})");
    for (label, probability) in others {
        line.push_str(&format!("; {label} {probability:.2}"));
    }
    if let Some((runner_up, probability)) = others.first() {
        line.push_str(&format!(
            "\nReason: {top} is favored over {runner_up} ({top_probability:.2} vs {probability:.2})."
        ));
    }
    line
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
) -> Result<(Map<String, Value>, DecisionQuestion), DecisionsError> {
    let question = turns
        .iter()
        .rev()
        .find(|turn| turn.role == "user")
        .map(|turn| turn.text.trim())
        .filter(|text| !text.is_empty())
        .map(DecisionQuestion::parse)
        .ok_or(DecisionsError::MissingUserMessage)?;
    let history = turns
        .iter()
        .filter(|turn| !turn.text.trim().is_empty())
        .map(|turn| json!({"role": turn.role, "content": turn.text}))
        .collect::<Vec<_>>();
    let named = question.upstream_questions().into_iter();
    let (history_key, questions) = match endpoint {
        DecisionsEndpoint::Decisions => (
            "input",
            named
                .map(|(name, instructions)| {
                    json!({"type": "predicate", "name": name, "instructions": instructions})
                })
                .collect::<Value>(),
        ),
        DecisionsEndpoint::SystemOne => (
            "state",
            Value::Object(
                named
                    .map(|(name, instructions)| {
                        (name, json!({"type": "noul", "instructions": instructions}))
                    })
                    .collect(),
            ),
        ),
    };
    let mut body = Map::new();
    body.insert("model".into(), json!(copilot_model));
    body.insert(history_key.into(), Value::Array(history));
    body.insert("questions".into(), questions);
    Ok((body, question))
}

/// Finds the named answer in an array or a name-keyed object.
fn named_answer<'a>(upstream: &'a Value, name: &str) -> Result<&'a Value, DecisionsError> {
    match upstream.get("answers") {
        Some(Value::Array(answers)) => answers
            .iter()
            .find(|answer| answer.get("name").and_then(Value::as_str) == Some(name)),
        Some(Value::Object(answers)) => answers.get(name),
        _ => return Err(DecisionsError::MalformedUpstream("no_answers")),
    }
    .ok_or(DecisionsError::MalformedUpstream("no_answer"))
}

/// Reads the named answer's probability, accepting a number or numeric string.
fn answer_probability(
    endpoint: DecisionsEndpoint,
    upstream: &Value,
    name: &str,
) -> Result<f64, DecisionsError> {
    let answer = named_answer(upstream, name)?;
    if answer.get("type").and_then(Value::as_str) == Some("refusal") {
        return Err(DecisionsError::Refused);
    }
    let key = match endpoint {
        DecisionsEndpoint::Decisions => "probability",
        DecisionsEndpoint::SystemOne => "noul",
    };
    match answer.get(key) {
        Some(Value::Number(number)) => number.as_f64(),
        Some(Value::String(text)) => text.trim().parse().ok(),
        _ => None,
    }
    .filter(|probability| probability.is_finite())
    .map(|probability| probability.clamp(0.0, 1.0))
    .ok_or(DecisionsError::MalformedUpstream("no_probability"))
}

pub fn answer_text(probability: f64) -> String {
    let verdict = if probability >= 0.5 { "Yes" } else { "No" };
    format!("{verdict} (probability {probability:.2})")
}

fn decision_text(
    endpoint: DecisionsEndpoint,
    upstream: &Value,
    question: &DecisionQuestion,
) -> Result<String, DecisionsError> {
    match question {
        DecisionQuestion::YesNo(_) => {
            answer_probability(endpoint, upstream, "answer").map(answer_text)
        }
        DecisionQuestion::Options(found) => {
            let probabilities = found
                .options
                .iter()
                .map(|option| answer_probability(endpoint, upstream, &option.key()))
                .collect::<Result<Vec<_>, _>>()?;
            Ok(render_options(&found.options, &normalize(&probabilities)))
        }
    }
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
    question: &DecisionQuestion,
    public_model: &str,
) -> Result<Value, DecisionsError> {
    let text = decision_text(endpoint, upstream, question)?;
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
            "content": [{"type": "output_text", "text": text, "annotations": []}],
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
    fn extract_options_numbered_list_survives_blank_lines_and_continuations() {
        let blank = extract_options("Choose a plan:\n1. Ship Friday\n\n2. Delay a week").unwrap();
        assert_eq!(
            option_pairs(&blank),
            [("Option 1", "Ship Friday"), ("Option 2", "Delay a week")]
        );
        let later = extract_options(
            "Plan?\n1. Ship Friday\n   more detail here\n2. Delay a week\n\nExtra paragraph.\n\n3. Cancel",
        )
        .unwrap();
        assert_eq!(
            option_pairs(&later),
            [
                ("Option 1", "Ship Friday"),
                ("Option 2", "Delay a week"),
                ("Option 3", "Cancel")
            ]
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
    fn render_options_leads_with_the_favorite_then_others_descending() {
        assert_eq!(
            render_options(
                &options(&["Option A", "Option B", "Option C"]),
                &[0.2, 0.5, 0.3]
            ),
            "Option B (0.50); Option C 0.30; Option A 0.20\nReason: Option B is favored over Option C (0.50 vs 0.30)."
        );
    }

    #[test]
    fn render_options_names_two_tied_options_and_defaults_to_the_first_listed() {
        assert_eq!(
            render_options(&options(&["Option A", "Option B"]), &[0.5, 0.5]),
            "Option A (0.50); Option B 0.50\nReason: Option A and Option B are tied (0.50 each); defaulting to the first listed."
        );
        assert_eq!(
            render_options(&options(&["Option A", "Option B"]), &[0.499, 0.501]),
            "Option A (0.50); Option B 0.50\nReason: Option A and Option B are tied (0.50 each); defaulting to the first listed."
        );
    }

    #[test]
    fn render_options_lists_every_tied_option() {
        let third = 1.0 / 3.0;
        assert_eq!(
            render_options(
                &options(&["Option A", "Option B", "Option C"]),
                &[third, third, third]
            ),
            "Option A (0.33); Option B 0.33; Option C 0.33\nReason: Option A, Option B and Option C are tied (0.33 each); defaulting to the first listed."
        );
        assert_eq!(
            render_options(
                &options(&["Option A", "Option B", "Option C"]),
                &[0.2, 0.4, 0.4]
            ),
            "Option B (0.40); Option C 0.40; Option A 0.20\nReason: Option B and Option C are tied (0.40 each); defaulting to the first listed."
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
        let all_zero = json!({"answers": {
            "option_a": {"noul": 0.0, "type": "noul"},
            "option_b": {"noul": -0.2, "type": "noul"}
        }});
        assert_eq!(
            answer(DecisionsEndpoint::SystemOne, &options_turns(), all_zero),
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
