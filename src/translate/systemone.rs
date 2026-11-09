//! Pure translation and validation for the TypeSafe SystemOne API
//! (`POST /v1/systemone`) over the Copilot decision models.
//!
//! Glossary: a *question* is one named TypeSafe primitive (`noul`, `choice`,
//! `score`); the *state* is the content the questions are asked about; an
//! *answer* is the per-question result keyed by question name.

use serde_json::{Map, Value, json};

use crate::translate::decisions::DecisionsEndpoint;

const DEFAULT_CHOICE_INSTRUCTIONS: &str = "Which option best fits?";
const DEFAULT_SCORE_INSTRUCTIONS: &str = "Which level best fits?";
const MESSAGE_ROLES: [&str; 4] = ["system", "developer", "user", "assistant"];
const MAX_REFUSAL_CHARS: usize = 200;
const DEFAULT_REFUSAL: &str = "declined";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SystemOneError {
    /// Local validation failure (HTTP 400).
    Invalid(String),
    /// Translated body exceeds the upstream limit (HTTP 413).
    TooLarge(String),
    /// The model declined every question, named in request order (HTTP 422).
    Refused(Vec<String>),
    /// Upstream answered 200 with an unusable body (HTTP 502).
    Malformed(DecisionsEndpoint),
}

fn invalid(message: impl Into<String>) -> SystemOneError {
    SystemOneError::Invalid(message.into())
}

/// A validated TypeSafe question. `instructions` is never an empty string.
#[derive(Debug, Clone, PartialEq)]
pub enum Question {
    Noul {
        instructions: Option<Value>,
        yes: Option<Value>,
        no: Option<Value>,
    },
    Choice {
        instructions: Option<Value>,
        criteria: Map<String, Value>,
    },
    Score {
        instructions: Option<Value>,
        criteria: Vec<Value>,
    },
}

/// Questions in request-map order, each paired with its non-empty name.
pub type Questions = Vec<(String, Question)>;

/// A TypeSafe result plus the questions the model declined, in request order.
/// Declined questions appear in `response.answers` as refusal entries.
#[derive(Debug, Clone, PartialEq)]
pub struct Translated {
    pub response: Value,
    pub refused: Vec<String>,
}

pub fn question_limit(endpoint: DecisionsEndpoint) -> usize {
    match endpoint {
        DecisionsEndpoint::Decisions => 400,
        DecisionsEndpoint::SystemOne => 255,
    }
}

pub fn body_limit(endpoint: DecisionsEndpoint) -> usize {
    match endpoint {
        DecisionsEndpoint::Decisions => 5 * 1024 * 1024,
        DecisionsEndpoint::SystemOne => 1024 * 1024,
    }
}

pub fn requested_model(body: &Map<String, Value>) -> Result<String, SystemOneError> {
    body.get("model")
        .and_then(Value::as_str)
        .filter(|model| !model.is_empty())
        .map(str::to_string)
        .ok_or_else(|| invalid("model is required"))
}

/// Validates `questions` for the target endpoint; `model` names it in messages.
pub fn parse_questions(
    endpoint: DecisionsEndpoint,
    model: &str,
    body: &Map<String, Value>,
) -> Result<Questions, SystemOneError> {
    let questions = body
        .get("questions")
        .and_then(Value::as_object)
        .ok_or_else(|| invalid("questions must be an object keyed by question name"))?;
    if questions.is_empty() {
        return Err(invalid("questions must contain at least one question"));
    }
    let limit = question_limit(endpoint);
    if questions.len() > limit {
        return Err(invalid(format!(
            "request has {} questions; the limit for model {model} is {limit}",
            questions.len()
        )));
    }
    questions
        .iter()
        .map(|(name, raw)| {
            if name.is_empty() {
                return Err(invalid("question names must not be empty"));
            }
            let question = parse_question(raw)
                .and_then(|question| check_for_endpoint(endpoint, model, question))
                .map_err(|reason| invalid(format!("question '{name}': {reason}")))?;
            Ok((name.clone(), question))
        })
        .collect()
}

fn parse_question(raw: &Value) -> Result<Question, String> {
    let instructions = parse_instructions(raw.get("instructions"))?;
    let criteria = raw.get("criteria");
    match raw.get("type").and_then(Value::as_str).unwrap_or_default() {
        "noul" => parse_noul(instructions, criteria),
        "choice" => Ok(Question::Choice {
            criteria: bounded(
                criteria.and_then(Value::as_object),
                Map::len,
                2..=255,
                "choice criteria must have 2-255 options",
            )?,
            instructions,
        }),
        "score" => Ok(Question::Score {
            criteria: bounded(
                criteria.and_then(Value::as_array),
                Vec::len,
                2..=10,
                "score criteria must have 2-10 levels",
            )?,
            instructions,
        }),
        other => Err(format!(
            "unsupported type '{other}'; use noul, choice or score"
        )),
    }
}

/// Clones `criteria` when its size is within `range`.
fn bounded<T: Clone>(
    criteria: Option<&T>,
    len: fn(&T) -> usize,
    range: std::ops::RangeInclusive<usize>,
    message: &str,
) -> Result<T, String> {
    criteria
        .filter(|criteria| range.contains(&len(criteria)))
        .cloned()
        .ok_or_else(|| message.to_string())
}

fn parse_instructions(raw: Option<&Value>) -> Result<Option<Value>, String> {
    match raw {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(text)) if text.is_empty() => {
            Err("instructions must not be empty".to_string())
        }
        Some(value) => Ok(Some(value.clone())),
    }
}

fn parse_noul(instructions: Option<Value>, criteria: Option<&Value>) -> Result<Question, String> {
    let outcome = |key: &str| {
        criteria
            .and_then(|c| c.get(key))
            .filter(|v| !v.is_null())
            .cloned()
    };
    match criteria {
        None | Some(Value::Null | Value::Object(_)) => Ok(Question::Noul {
            instructions,
            yes: outcome("true"),
            no: outcome("false"),
        }),
        Some(_) => Err("noul criteria must be an object with true/false".to_string()),
    }
}

fn check_for_endpoint(
    endpoint: DecisionsEndpoint,
    model: &str,
    question: Question,
) -> Result<Question, String> {
    let unanswerable = matches!(
        (&question, endpoint),
        (
            Question::Noul {
                instructions: None,
                yes: None,
                no: None
            },
            DecisionsEndpoint::Decisions
        )
    );
    if unanswerable {
        return Err(format!(
            "noul needs instructions or criteria for model {model}"
        ));
    }
    Ok(question)
}

fn ensure_size(
    endpoint: DecisionsEndpoint,
    body: &Map<String, Value>,
) -> Result<(), SystemOneError> {
    let size = serde_json::to_vec(body)
        .map(|bytes| bytes.len())
        .unwrap_or(0);
    let limit = body_limit(endpoint);
    if size > limit {
        return Err(SystemOneError::TooLarge(format!(
            "translated request is {size} bytes; the upstream limit is {limit} bytes"
        )));
    }
    Ok(())
}

/// The client body forwarded to `/v1/systemone` with only `model` rewritten.
pub fn to_systemone_request(
    mut body: Map<String, Value>,
    copilot_model: &str,
) -> Result<Map<String, Value>, SystemOneError> {
    body.insert("model".to_string(), json!(copilot_model));
    ensure_size(DecisionsEndpoint::SystemOne, &body)?;
    Ok(body)
}

/// One `/v1/decisions` request answering every question.
pub fn to_decisions_request(
    copilot_model: &str,
    state: &Value,
    questions: &Questions,
) -> Result<Map<String, Value>, SystemOneError> {
    let questions: Vec<Value> = questions
        .iter()
        .map(|(name, question)| decisions_question(name, question))
        .collect();
    let mut body = Map::new();
    body.insert("model".to_string(), json!(copilot_model));
    body.insert("input".to_string(), decisions_input(state));
    body.insert("questions".to_string(), Value::Array(questions));
    ensure_size(DecisionsEndpoint::Decisions, &body)?;
    Ok(body)
}

/// Strings and `[{role, content}]` messages are native decisions input;
/// anything else is sent as compact JSON text.
fn decisions_input(state: &Value) -> Value {
    match state {
        Value::String(_) => state.clone(),
        Value::Array(items) if !items.is_empty() && items.iter().all(is_message) => state.clone(),
        other => json!(other.to_string()),
    }
}

fn is_message(item: &Value) -> bool {
    let role = item.get("role").and_then(Value::as_str);
    role.is_some_and(|role| MESSAGE_ROLES.contains(&role)) && item.get("content").is_some()
}

fn text(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        other => other.to_string(),
    }
}

fn instructions_or(instructions: &Option<Value>, default: &str) -> String {
    instructions
        .as_ref()
        .map(text)
        .unwrap_or_else(|| default.to_string())
}

fn decisions_question(name: &str, question: &Question) -> Value {
    match question {
        Question::Noul {
            instructions,
            yes,
            no,
        } => json!({
            "type": "predicate",
            "name": name,
            "instructions": noul_instructions(instructions, yes, no),
        }),
        Question::Choice {
            instructions,
            criteria,
        } => json!({
            "type": "choice",
            "name": name,
            "instructions": instructions_or(instructions, DEFAULT_CHOICE_INSTRUCTIONS),
            "choices": criteria.iter().map(|(value, description)| choice(value, description)).collect::<Vec<_>>(),
        }),
        Question::Score {
            instructions,
            criteria,
        } => json!({
            "type": "score",
            "name": name,
            "instructions": instructions_or(instructions, DEFAULT_SCORE_INSTRUCTIONS),
            "levels": criteria.iter().enumerate().map(|(i, level)| json!({"label": level_label(i, level)})).collect::<Vec<_>>(),
        }),
    }
}

/// Folds noul criteria into the instructions text, which decisions requires.
fn noul_instructions(
    instructions: &Option<Value>,
    yes: &Option<Value>,
    no: &Option<Value>,
) -> String {
    let parts: Vec<String> = [
        instructions.as_ref().map(text),
        yes.as_ref().map(|v| format!("Yes means: {}.", text(v))),
        no.as_ref().map(|v| format!("No means: {}.", text(v))),
    ]
    .into_iter()
    .flatten()
    .collect();
    parts.join(" ")
}

fn choice(value: &str, description: &Value) -> Value {
    match description {
        Value::Null => json!({"value": value}),
        other => json!({"value": value, "description": text(other)}),
    }
}

fn level_label(index: usize, level: &Value) -> String {
    match level {
        Value::Null => index.to_string(),
        other => text(other),
    }
}

/// Re-keys a `/v1/decisions` answer array into the TypeSafe result shape.
pub fn from_decisions_response(
    public_model: &str,
    questions: &Questions,
    upstream: &Value,
) -> Result<Translated, SystemOneError> {
    let malformed = SystemOneError::Malformed(DecisionsEndpoint::Decisions);
    let by_name = answers_by_name(upstream).ok_or(malformed.clone())?;
    let mut answers = Map::new();
    let mut refused = Vec::new();
    for (name, question) in questions {
        let answer = by_name.get(name.as_str()).ok_or(malformed.clone())?;
        let answer = if is_refusal(answer) {
            refused.push(name.clone());
            refusal_entry(answer)
        } else {
            typesafe_answer(question, answer).ok_or(malformed.clone())?
        };
        answers.insert(name.clone(), answer);
    }
    let usage = upstream.get("usage");
    let tokens = |key: &str| {
        usage
            .and_then(|u| u.get(key))
            .cloned()
            .unwrap_or(Value::Null)
    };
    let response = json!({
        "model": public_model,
        "answers": answers,
        "usage": {"input_tokens": tokens("input_tokens"), "output_tokens": tokens("output_tokens")},
    });
    settle(questions, response, refused)
}

/// Rejects the batch only when every question was declined.
fn settle(
    questions: &Questions,
    response: Value,
    refused: Vec<String>,
) -> Result<Translated, SystemOneError> {
    if refused.len() == questions.len() {
        return Err(SystemOneError::Refused(refused));
    }
    Ok(Translated { response, refused })
}

fn is_refusal(answer: &Value) -> bool {
    answer.get("type").and_then(Value::as_str) == Some("refusal")
}

/// `{"type":"refusal","refusal":text}` from an upstream refusal; only its
/// string `refusal` field is read, trimmed and truncated.
fn refusal_entry(answer: &Value) -> Value {
    let text = answer
        .get("refusal")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|text| !text.is_empty())
        .map(|text| text.chars().take(MAX_REFUSAL_CHARS).collect())
        .unwrap_or_else(|| DEFAULT_REFUSAL.to_string());
    json!({"type": "refusal", "refusal": text})
}

/// Answers keyed by name; `None` when the array is missing or names repeat.
fn answers_by_name(upstream: &Value) -> Option<std::collections::HashMap<&str, &Value>> {
    let answers = upstream.get("answers")?.as_array()?;
    let mut by_name = std::collections::HashMap::new();
    for answer in answers {
        let name = answer.get("name")?.as_str()?;
        if by_name.insert(name, answer).is_some() {
            return None;
        }
    }
    Some(by_name)
}

fn typesafe_answer(question: &Question, answer: &Value) -> Option<Value> {
    let kind = answer.get("type")?.as_str()?;
    let number = |key: &str| answer.get(key).filter(|v| v.is_number()).cloned();
    match (question, kind) {
        (Question::Noul { .. }, "predicate") => {
            Some(json!({"type": "noul", "noul": number("probability")?}))
        }
        (Question::Choice { .. }, "choice") => Some(json!({
            "type": "choice",
            "choice": answer.get("choice")?.as_str()?,
            "confidence": number("confidence")?,
            "probabilities": probability_map(answer, |value| value.as_str().map(str::to_string))?,
        })),
        (Question::Score { criteria, .. }, "score") => Some(json!({
            "type": "score",
            "score": number("score")?,
            "confidence": number("confidence")?,
            "legend": criteria.iter().enumerate().map(|(i, level)| (i.to_string(), level.clone())).collect::<Map<_, _>>(),
            "probabilities": probability_map(answer, |value| value.as_u64().map(|i| i.to_string()))?,
        })),
        _ => None,
    }
}

/// `[{value, probability}]` to `{key(value): probability}`.
fn probability_map(answer: &Value, key: impl Fn(&Value) -> Option<String>) -> Option<Value> {
    answer
        .get("probabilities")?
        .as_array()?
        .iter()
        .map(|entry| {
            let probability = entry.get("probability").filter(|v| v.is_number())?;
            Some((key(entry.get("value")?)?, probability.clone()))
        })
        .collect::<Option<Map<_, _>>>()
        .map(Value::Object)
}

/// Passes a `/v1/systemone` result through with the public model id after
/// checking every requested question has a well-formed answer and usage.
/// Refusal answers are normalized to the refusal entry shape.
pub fn from_systemone_response(
    public_model: &str,
    questions: &Questions,
    upstream: Value,
) -> Result<Translated, SystemOneError> {
    let malformed = SystemOneError::Malformed(DecisionsEndpoint::SystemOne);
    let Value::Object(mut body) = upstream else {
        return Err(malformed);
    };
    let usage_ok = body.get("usage").is_some_and(|usage| {
        ["input_tokens", "output_tokens"]
            .iter()
            .all(|key| usage.get(key).is_some_and(Value::is_number))
    });
    let answers = body
        .get_mut("answers")
        .and_then(Value::as_object_mut)
        .filter(|_| usage_ok)
        .ok_or(malformed.clone())?;
    let mut refused = Vec::new();
    for (name, question) in questions {
        let answer = answers.get_mut(name).ok_or(malformed.clone())?;
        if is_refusal(answer) {
            *answer = refusal_entry(answer);
            refused.push(name.clone());
        } else if !answer_matches(question, answer) {
            return Err(malformed);
        }
    }
    body.insert("model".to_string(), json!(public_model));
    settle(questions, Value::Object(body), refused)
}

fn answer_matches(question: &Question, answer: &Value) -> bool {
    let has = |key: &str, ok: fn(&Value) -> bool| answer.get(key).is_some_and(ok);
    let kind = answer.get("type").and_then(Value::as_str);
    match (question, kind) {
        (Question::Noul { .. }, Some("noul")) => has("noul", Value::is_number),
        (Question::Choice { .. }, Some("choice")) => {
            has("choice", Value::is_string)
                && has("confidence", Value::is_number)
                && has("probabilities", Value::is_object)
        }
        (Question::Score { .. }, Some("score")) => {
            has("score", Value::is_number)
                && has("confidence", Value::is_number)
                && has("legend", Value::is_object)
                && has("probabilities", Value::is_object)
        }
        _ => false,
    }
}
