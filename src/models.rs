use std::collections::{BTreeMap, BTreeSet};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use tokio::sync::RwLock;

use crate::state::BackendSnapshot;

pub const COPILOT_MODEL_MAP: &[(&str, &str)] = &[
    ("claude-opus-4-8", "claude-opus-4.8"),
    ("claude-opus-4-7", "claude-opus-4.7"),
    ("claude-opus-4-6-20250515", "claude-opus-4.6"),
    ("claude-opus-4-6", "claude-opus-4.6"),
    ("claude-sonnet-4-6", "claude-sonnet-4.6"),
    ("claude-opus-4-5-20251101", "claude-opus-4.5"),
    ("claude-opus-4-5", "claude-opus-4.5"),
    ("claude-opus-4-1-20250805", "claude-opus-4.1"),
    ("claude-opus-4-1", "claude-opus-4.1"),
    ("claude-opus-4-20250514", "claude-opus-4"),
    ("claude-opus-4", "claude-opus-4"),
    ("claude-sonnet-4-5-20250929", "claude-sonnet-4.5"),
    ("claude-sonnet-4-5", "claude-sonnet-4.5"),
    ("claude-sonnet-4-20250514", "claude-sonnet-4"),
    ("claude-sonnet-4", "claude-sonnet-4"),
    ("claude-haiku-4-5-20251001", "claude-haiku-4.5"),
    ("claude-haiku-4-5", "claude-haiku-4.5"),
    ("claude-3-7-sonnet-20250219", "claude-3.7-sonnet"),
    ("claude-3-7-sonnet", "claude-3.7-sonnet"),
    ("claude-3-5-sonnet-20241022", "claude-3.5-sonnet"),
    ("claude-3-5-sonnet", "claude-3.5-sonnet"),
    ("claude-3-5-haiku-20241022", "claude-3.5-haiku"),
    ("claude-3-5-haiku", "claude-3.5-haiku"),
];

pub const COPILOT_OPENAI_MODEL_MAP: &[(&str, &str)] = &[
    ("claude-opus-4.8", "claude-opus-4.8"),
    ("claude-opus-4.7", "claude-opus-4.7"),
    ("claude-opus-4.6", "claude-opus-4.6"),
    ("claude-opus-4.5", "claude-opus-4.5"),
    ("claude-opus-4.1", "claude-opus-4.1"),
    ("claude-sonnet-4.5", "claude-sonnet-4.5"),
    ("claude-sonnet-4", "claude-sonnet-4"),
    ("claude-haiku-4.5", "claude-haiku-4.5"),
    ("gpt-5.5", "gpt-5.5"),
    ("gpt-5.4", "gpt-5.4"),
    ("gpt-5.3-codex", "gpt-5.3-codex"),
    ("gpt-5.2-codex", "gpt-5.2-codex"),
    ("gpt-5.2", "gpt-5.2"),
    ("gpt-5.1-codex-max", "gpt-5.1-codex-max"),
    ("gpt-5.1-codex-mini", "gpt-5.1-codex-mini"),
    ("gpt-5.1-codex", "gpt-5.1-codex"),
    ("gpt-5.1", "gpt-5.1"),
    ("gpt-5-codex", "gpt-5-codex"),
    ("gpt-5-mini", "gpt-5-mini"),
    ("gpt-5", "gpt-5"),
    ("gpt-4.1", "gpt-4.1"),
    ("gpt-4o", "gpt-4o"),
    ("gemini-3-pro-preview", "gemini-3-pro-preview"),
    ("gemini-3-flash-preview", "gemini-3-flash-preview"),
    ("gemini-2.5-pro", "gemini-2.5-pro"),
    ("grok-code-fast-1", "grok-code-fast-1"),
    ("raptor-mini", "raptor-mini"),
];

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ModelsListResponse {
    pub object: &'static str,
    pub data: Vec<ModelEntry>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ModelEntry {
    pub id: String,
    pub object: &'static str,
    pub created: u64,
    pub owned_by: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum EffortLevel {
    Low,
    Medium,
    High,
    #[serde(rename = "xhigh")]
    XHigh,
    Max,
}

impl EffortLevel {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
            Self::XHigh => "xhigh",
            Self::Max => "max",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "low" => Some(Self::Low),
            "medium" => Some(Self::Medium),
            "high" => Some(Self::High),
            "xhigh" => Some(Self::XHigh),
            "max" => Some(Self::Max),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SupportedEfforts(Vec<EffortLevel>);

impl SupportedEfforts {
    pub fn new(mut efforts: Vec<EffortLevel>) -> Option<Self> {
        efforts.sort();
        efforts.dedup();
        (!efforts.is_empty()).then_some(Self(efforts))
    }

    pub fn as_strings(&self) -> Vec<&'static str> {
        self.0.iter().map(|effort| effort.as_str()).collect()
    }

    pub fn highest(&self) -> Option<EffortLevel> {
        self.0.last().copied()
    }

    pub fn lowest(&self) -> Option<EffortLevel> {
        self.0.first().copied()
    }

    pub fn contains(&self, effort: EffortLevel) -> bool {
        self.0.contains(&effort)
    }

    pub fn clamp(&self, requested: &str) -> Option<EffortLevel> {
        let requested = EffortLevel::parse(requested)?;
        self.0
            .iter()
            .rev()
            .copied()
            .find(|supported| *supported <= requested)
            .or_else(|| self.lowest())
    }
}

pub fn infer_owned_by(model_id: &str) -> &'static str {
    if model_id.starts_with("gpt-")
        || model_id.starts_with("o1-")
        || model_id.starts_with("o3-")
        || model_id.starts_with("o4-")
    {
        "openai"
    } else if model_id.starts_with("gemini-") {
        "google"
    } else if model_id.starts_with("grok-") {
        "xai"
    } else if model_id.starts_with("claude-") {
        "anthropic"
    } else {
        "other"
    }
}

pub fn is_claude_model(model_id: &str) -> bool {
    model_id.starts_with("claude-")
        || model_id.starts_with("claude ")
        || model_id.contains("anthropic.")
}

pub fn model_list_for_snapshot(_snapshot: BackendSnapshot) -> ModelsListResponse {
    ModelsListResponse {
        object: "list",
        data: copilot_models(),
    }
}

fn copilot_models() -> Vec<ModelEntry> {
    let mut owners = BTreeMap::new();
    let copilot_target_ids: BTreeSet<&str> = COPILOT_MODEL_MAP
        .iter()
        .map(|(_, target)| *target)
        .collect();

    for (model_id, _) in COPILOT_MODEL_MAP {
        owners.insert((*model_id).to_string(), "anthropic".to_string());
    }
    for (model_id, _) in COPILOT_OPENAI_MODEL_MAP {
        if !owners.contains_key(*model_id) && !copilot_target_ids.contains(model_id) {
            owners.insert(
                (*model_id).to_string(),
                infer_owned_by(model_id).to_string(),
            );
        }
    }

    owners
        .into_iter()
        .map(|(id, owned_by)| ModelEntry {
            id,
            object: "model",
            created: 1_700_000_000,
            owned_by,
        })
        .collect()
}

#[derive(Debug, Default)]
pub struct ModelRegistry {
    inner: RwLock<ModelRegistryInner>,
}

#[derive(Debug, Default)]
struct ModelRegistryInner {
    models: Vec<serde_json::Value>,
    fetched_at: Option<Instant>,
}

impl ModelRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub async fn set_copilot_models(&self, models: Vec<serde_json::Value>) {
        let mut inner = self.inner.write().await;
        inner.models = models;
        inner.fetched_at = Some(Instant::now());
    }

    pub async fn refresh_needed(&self, ttl_seconds: u64) -> bool {
        let inner = self.inner.read().await;
        match inner.fetched_at {
            Some(fetched_at) => fetched_at.elapsed() >= Duration::from_secs(ttl_seconds),
            None => true,
        }
    }

    pub async fn list_for_snapshot(&self, snapshot: BackendSnapshot) -> ModelsListResponse {
        model_list_for_snapshot(snapshot)
    }

    pub async fn get_copilot_openai_model(&self, model: &str) -> String {
        let model = strip_model_prefix(model);
        if model == "default" {
            return "claude-sonnet-4.6".to_string();
        }
        let inner = self.inner.read().await;
        let ids: std::collections::BTreeSet<String> = inner
            .models
            .iter()
            .filter_map(|m| {
                m.get("id")
                    .and_then(serde_json::Value::as_str)
                    .map(str::to_string)
            })
            .collect();
        if !ids.is_empty() && ids.contains(model) {
            return model.to_string();
        }
        drop(inner);
        COPILOT_OPENAI_MODEL_MAP
            .iter()
            .chain(COPILOT_MODEL_MAP.iter())
            .find_map(|(source, target)| (*source == model).then(|| (*target).to_string()))
            .unwrap_or_else(|| model.to_string())
    }

    pub async fn model_supports_responses_api(&self, model: &str) -> bool {
        self.supported_endpoints(model)
            .await
            .iter()
            .any(|endpoint| endpoint == "/responses")
    }

    pub async fn model_supports_chat_completions_api(&self, model: &str) -> bool {
        self.supported_endpoints(model)
            .await
            .iter()
            .any(|endpoint| endpoint == "/chat/completions")
    }

    pub async fn model_supports_responses_ws(&self, model: &str) -> bool {
        self.supported_endpoints(model)
            .await
            .iter()
            .any(|endpoint| endpoint == "ws:/responses")
    }

    pub async fn model_supports_messages_api(&self, model: &str) -> bool {
        let endpoints = self.supported_endpoints(model).await;
        if !endpoints.is_empty() {
            return endpoints.iter().any(|endpoint| endpoint == "/v1/messages");
        }
        // Default: Claude models support the /v1/messages endpoint
        model.starts_with("claude-")
    }

    pub async fn supported_efforts(&self, model: &str) -> Option<SupportedEfforts> {
        let model = strip_model_prefix(model);
        let inner = self.inner.read().await;
        let dynamic = inner
            .models
            .iter()
            .find(|item| item.get("id").and_then(serde_json::Value::as_str) == Some(model))
            .and_then(dynamic_supported_efforts);
        drop(inner);
        dynamic.or_else(|| static_supported_efforts(model))
    }

    async fn supported_endpoints(&self, model: &str) -> Vec<String> {
        let model = strip_model_prefix(model);
        let inner = self.inner.read().await;
        let dynamic: Vec<String> = inner
            .models
            .iter()
            .find(|item| item.get("id").and_then(serde_json::Value::as_str) == Some(model))
            .and_then(|item| item.get("supported_endpoints"))
            .and_then(serde_json::Value::as_array)
            .map(|items| {
                items
                    .iter()
                    .filter_map(|item| item.as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default();
        if dynamic.is_empty() {
            static_supported_endpoints(model)
        } else {
            dynamic
        }
    }
}

fn static_supported_endpoints(model: &str) -> Vec<String> {
    match model {
        "gpt-5.5" | "gpt-5.4" | "gpt-5.4-mini" | "gpt-5.4-nano" | "gpt-5.3-codex" => {
            vec!["/responses".to_string()]
        }
        _ => Vec::new(),
    }
}

fn strip_model_prefix(model: &str) -> &str {
    model.strip_prefix("github-copilot/").unwrap_or(model)
}

fn dynamic_supported_efforts(value: &serde_json::Value) -> Option<SupportedEfforts> {
    let efforts = value
        .get("capabilities")
        .and_then(|capabilities| capabilities.get("supports"))
        .and_then(|supports| supports.get("reasoning_effort"))
        .and_then(serde_json::Value::as_array)?
        .iter()
        .filter_map(|item| item.as_str().and_then(EffortLevel::parse))
        .collect();
    SupportedEfforts::new(efforts)
}

fn static_supported_efforts(model: &str) -> Option<SupportedEfforts> {
    let efforts = match model {
        "claude-opus-4.8" | "claude-opus-4-8" | "claude-opus-4.7" | "claude-opus-4-7" => {
            vec![
                EffortLevel::Low,
                EffortLevel::Medium,
                EffortLevel::High,
                EffortLevel::XHigh,
                EffortLevel::Max,
            ]
        }
        "claude-opus-4.6" | "claude-opus-4-6" | "claude-sonnet-4.6" | "claude-sonnet-4-6" => {
            vec![
                EffortLevel::Low,
                EffortLevel::Medium,
                EffortLevel::High,
                EffortLevel::Max,
            ]
        }
        _ => return None,
    };
    SupportedEfforts::new(efforts)
}
