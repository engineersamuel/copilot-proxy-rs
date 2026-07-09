use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use tokio::sync::RwLock;

use crate::state::BackendSnapshot;

pub const COPILOT_MODEL_ALIASES: &[(&str, &str)] = &[
    ("gpt-5-6-sol", "gpt-5.6-sol"),
    ("gpt-5-6-luna", "gpt-5.6-luna"),
    ("gpt-5-6-terra", "gpt-5.6-terra"),
    ("gpt-5-6-tera", "gpt-5.6-terra"),
    ("gpt-5.6-tera", "gpt-5.6-terra"),
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
    ("claude-sonnet-4-5-20250929", "claude-sonnet-4.5"),
    ("claude-sonnet-4-5", "claude-sonnet-4.5"),
    ("claude-sonnet-4-20250514", "claude-sonnet-4"),
    ("claude-haiku-4-5-20251001", "claude-haiku-4.5"),
    ("claude-haiku-4-5", "claude-haiku-4.5"),
    ("claude-3-7-sonnet-20250219", "claude-3.7-sonnet"),
    ("claude-3-7-sonnet", "claude-3.7-sonnet"),
    ("claude-3-5-sonnet-20241022", "claude-3.5-sonnet"),
    ("claude-3-5-sonnet", "claude-3.5-sonnet"),
    ("claude-3-5-haiku-20241022", "claude-3.5-haiku"),
    ("claude-3-5-haiku", "claude-3.5-haiku"),
];

const GPT56_MODEL_CREATED: u64 = 1_783_555_200;
const GPT56_SUPPORTED_ENDPOINTS: &[&str] = &["/responses", "ws:/responses"];
const GPT56_REASONING_EFFORTS: &[EffortLevel] = &[
    EffortLevel::Low,
    EffortLevel::Medium,
    EffortLevel::High,
    EffortLevel::XHigh,
    EffortLevel::Max,
];

#[derive(Debug, Clone, Copy)]
struct StaticCopilotModel {
    id: &'static str,
    created: u64,
    owned_by: &'static str,
    supported_endpoints: &'static [&'static str],
    supported_efforts: &'static [EffortLevel],
}

const STATIC_COPILOT_MODELS: &[StaticCopilotModel] = &[
    StaticCopilotModel {
        id: "gpt-5.6-luna",
        created: GPT56_MODEL_CREATED,
        owned_by: "openai",
        supported_endpoints: GPT56_SUPPORTED_ENDPOINTS,
        supported_efforts: GPT56_REASONING_EFFORTS,
    },
    StaticCopilotModel {
        id: "gpt-5.6-sol",
        created: GPT56_MODEL_CREATED,
        owned_by: "openai",
        supported_endpoints: GPT56_SUPPORTED_ENDPOINTS,
        supported_efforts: GPT56_REASONING_EFFORTS,
    },
    StaticCopilotModel {
        id: "gpt-5.6-terra",
        created: GPT56_MODEL_CREATED,
        owned_by: "openai",
        supported_endpoints: GPT56_SUPPORTED_ENDPOINTS,
        supported_efforts: GPT56_REASONING_EFFORTS,
    },
];

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ModelsListResponse {
    pub object: &'static str,
    pub data: Vec<ModelEntry>,
    pub models: Vec<CodexModelEntry>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ModelEntry {
    pub id: String,
    pub object: &'static str,
    pub created: u64,
    pub owned_by: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CodexModelEntry {
    pub slug: String,
    pub display_name: String,
    pub description: String,
    pub default_reasoning_level: Option<String>,
    pub supported_reasoning_levels: Vec<ReasoningEffortPreset>,
    pub shell_type: String,
    pub visibility: String,
    pub supported_in_api: bool,
    pub priority: u64,
    pub additional_speed_tiers: Vec<serde_json::Value>,
    pub service_tiers: Vec<serde_json::Value>,
    pub availability_nux: Option<serde_json::Value>,
    pub upgrade: Option<serde_json::Value>,
    pub base_instructions: String,
    pub model_messages: serde_json::Value,
    pub supports_reasoning_summaries: bool,
    pub default_reasoning_summary: String,
    pub support_verbosity: bool,
    pub default_verbosity: String,
    pub apply_patch_tool_type: String,
    pub web_search_tool_type: String,
    pub truncation_policy: serde_json::Value,
    pub supports_parallel_tool_calls: bool,
    pub supports_image_detail_original: bool,
    pub context_window: Option<u64>,
    pub max_context_window: Option<u64>,
    pub comp_hash: String,
    pub effective_context_window_percent: u64,
    pub experimental_supported_tools: Vec<serde_json::Value>,
    pub input_modalities: Vec<String>,
    pub supports_search_tool: bool,
    pub use_responses_lite: bool,
    pub context_window_modes: Vec<ContextWindowMode>,
    pub supported_endpoints: Vec<String>,
    pub source: ModelMetadataSource,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ReasoningEffortPreset {
    pub effort: String,
    pub description: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ContextWindowMode {
    pub name: String,
    pub context_window: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ModelMetadataSource {
    Dynamic,
    Static,
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
    } else if model_id.starts_with("mai-") {
        "microsoft"
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
        data: copilot_models(&[]),
        models: rich_copilot_models(&[]),
    }
}

fn copilot_models(dynamic_models: &[serde_json::Value]) -> Vec<ModelEntry> {
    let mut entries = BTreeMap::new();
    for static_model in STATIC_COPILOT_MODELS {
        let entry = static_model_entry(static_model);
        entries.insert(entry.id.clone(), entry);
    }
    for dynamic in dynamic_models {
        if let Some(entry) = dynamic_model_entry(dynamic) {
            entries.insert(entry.id.clone(), entry);
        }
    }

    entries.into_values().collect()
}

fn static_model_entry(model: &StaticCopilotModel) -> ModelEntry {
    ModelEntry {
        id: model.id.to_string(),
        object: "model",
        created: model.created,
        owned_by: model.owned_by.to_string(),
    }
}

fn dynamic_model_entry(value: &serde_json::Value) -> Option<ModelEntry> {
    let id = value.get("id").and_then(serde_json::Value::as_str)?;
    Some(ModelEntry {
        id: id.to_string(),
        object: "model",
        created: number_field(value, &["created", "created_at", "createdAt"])
            .unwrap_or(1_700_000_000),
        owned_by: value
            .get("owned_by")
            .or_else(|| value.get("ownedBy"))
            .and_then(serde_json::Value::as_str)
            .unwrap_or_else(|| infer_owned_by(id))
            .to_string(),
    })
}

fn rich_copilot_models(dynamic_models: &[serde_json::Value]) -> Vec<CodexModelEntry> {
    copilot_models(dynamic_models)
        .into_iter()
        .map(|entry| rich_model_entry(&entry.id, dynamic_models))
        .collect()
}

fn rich_model_entry(model_id: &str, dynamic_models: &[serde_json::Value]) -> CodexModelEntry {
    let dynamic = dynamic_models
        .iter()
        .find(|item| item.get("id").and_then(serde_json::Value::as_str) == Some(model_id));
    let static_model = static_copilot_model(model_id);
    let source = if dynamic.is_some() {
        ModelMetadataSource::Dynamic
    } else {
        ModelMetadataSource::Static
    };

    let supported_reasoning_level_names: Vec<String> = dynamic
        .and_then(dynamic_supported_efforts)
        .or_else(|| static_supported_efforts(model_id))
        .map(|efforts| {
            efforts
                .as_strings()
                .into_iter()
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default();
    let default_reasoning_level = if supported_reasoning_level_names
        .iter()
        .any(|level| level == "medium")
    {
        Some("medium".to_string())
    } else {
        supported_reasoning_level_names.first().cloned()
    };
    let supported_reasoning_levels = supported_reasoning_level_names
        .into_iter()
        .map(reasoning_effort_preset)
        .collect();

    let supported_endpoints = dynamic
        .and_then(dynamic_supported_endpoints)
        .filter(|endpoints| !endpoints.is_empty())
        .or_else(|| static_model.map(static_supported_endpoints))
        .unwrap_or_default();
    let context_window =
        dynamic.and_then(|item| number_field(item, &["context_window", "contextWindow"]));
    let max_context_window =
        dynamic.and_then(|item| number_field(item, &["max_context_window", "maxContextWindow"]));
    let context_window_modes = dynamic
        .and_then(dynamic_context_window_modes)
        .unwrap_or_default();

    CodexModelEntry {
        slug: model_id.to_string(),
        display_name: display_name(model_id),
        description: format!("Copilot model {model_id}"),
        default_reasoning_level,
        supported_reasoning_levels,
        shell_type: "shell_command".to_string(),
        visibility: "list".to_string(),
        supported_in_api: true,
        priority: 100,
        additional_speed_tiers: Vec::new(),
        service_tiers: Vec::new(),
        availability_nux: None,
        upgrade: None,
        base_instructions: String::new(),
        model_messages: serde_json::json!({}),
        supports_reasoning_summaries: true,
        default_reasoning_summary: "none".to_string(),
        support_verbosity: true,
        default_verbosity: "low".to_string(),
        apply_patch_tool_type: "freeform".to_string(),
        web_search_tool_type: "text_and_image".to_string(),
        truncation_policy: serde_json::json!({
            "mode": "tokens",
            "limit": 10000
        }),
        supports_parallel_tool_calls: true,
        supports_image_detail_original: true,
        context_window,
        max_context_window,
        comp_hash: match source {
            ModelMetadataSource::Dynamic => "dynamic",
            ModelMetadataSource::Static => "static",
        }
        .to_string(),
        effective_context_window_percent: 95,
        experimental_supported_tools: Vec::new(),
        input_modalities: vec!["text".to_string(), "image".to_string()],
        supports_search_tool: true,
        use_responses_lite: false,
        context_window_modes,
        supported_endpoints,
        source,
    }
}

fn reasoning_effort_preset(effort: String) -> ReasoningEffortPreset {
    let description = match effort.as_str() {
        "low" => "Fast responses with lighter reasoning",
        "medium" => "Balances speed and reasoning depth for everyday tasks",
        "high" => "Greater reasoning depth for complex problems",
        "xhigh" => "Extra high reasoning depth for complex problems",
        "max" => "Maximum reasoning depth for the hardest problems",
        _ => "Reasoning effort preset",
    };
    ReasoningEffortPreset {
        effort,
        description: description.to_string(),
    }
}

fn context_mode(name: &str, context_window: u64) -> ContextWindowMode {
    ContextWindowMode {
        name: name.to_string(),
        context_window,
    }
}

fn display_name(model_id: &str) -> String {
    model_id
        .split('-')
        .map(|part| {
            if part.eq_ignore_ascii_case("gpt") {
                "GPT".to_string()
            } else {
                let mut chars = part.chars();
                match chars.next() {
                    Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
                    None => String::new(),
                }
            }
        })
        .collect::<Vec<_>>()
        .join("-")
}

fn number_field(value: &serde_json::Value, names: &[&str]) -> Option<u64> {
    names.iter().find_map(|name| value.get(*name)?.as_u64())
}

fn dynamic_supported_endpoints(value: &serde_json::Value) -> Option<Vec<String>> {
    value
        .get("supported_endpoints")
        .and_then(serde_json::Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(|item| item.as_str().map(str::to_string))
                .collect()
        })
}

fn dynamic_context_window_modes(value: &serde_json::Value) -> Option<Vec<ContextWindowMode>> {
    let modes = value
        .get("context_window_modes")
        .or_else(|| value.get("contextWindowModes"))?
        .as_array()?
        .iter()
        .filter_map(|mode| {
            let name = mode.get("name")?.as_str()?;
            let context_window = number_field(mode, &["context_window", "contextWindow"])?;
            Some(context_mode(name, context_window))
        })
        .collect::<Vec<_>>();
    (!modes.is_empty()).then_some(modes)
}

fn sanitize_model_metadata(value: &serde_json::Value) -> serde_json::Value {
    match value {
        serde_json::Value::Object(map) => serde_json::Value::Object(
            map.iter()
                .filter_map(|(key, value)| {
                    if is_sensitive_metadata_key(key) {
                        None
                    } else {
                        Some((key.clone(), sanitize_model_metadata(value)))
                    }
                })
                .collect(),
        ),
        serde_json::Value::Array(items) => {
            serde_json::Value::Array(items.iter().map(sanitize_model_metadata).collect())
        }
        other => other.clone(),
    }
}

fn is_sensitive_metadata_key(key: &str) -> bool {
    matches!(
        key.to_ascii_lowercase().as_str(),
        "authorization"
            | "access_token"
            | "refresh_token"
            | "token"
            | "api_key"
            | "apikey"
            | "password"
            | "secret"
    )
}

#[derive(Debug, Default)]
pub struct ModelRegistry {
    inner: RwLock<ModelRegistryInner>,
}

#[derive(Debug, Default)]
struct ModelRegistryInner {
    models: Vec<serde_json::Value>,
    fetched_at: Option<Instant>,
    copilot_overrides: BTreeMap<String, String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CopilotModelsDebugSnapshot {
    pub fetched: bool,
    pub fetched_at_age_seconds: Option<u64>,
    pub upstream_models: Vec<serde_json::Value>,
    pub models: Vec<CodexModelEntry>,
}

impl ModelRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_copilot_overrides(overrides: BTreeMap<String, String>) -> Self {
        Self {
            inner: RwLock::new(ModelRegistryInner {
                copilot_overrides: overrides,
                ..Default::default()
            }),
        }
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

    pub async fn debug_snapshot(&self) -> CopilotModelsDebugSnapshot {
        let inner = self.inner.read().await;
        let upstream_models = inner.models.iter().map(sanitize_model_metadata).collect();
        let models = rich_copilot_models(&inner.models);
        CopilotModelsDebugSnapshot {
            fetched: inner.fetched_at.is_some(),
            fetched_at_age_seconds: inner
                .fetched_at
                .map(|fetched_at| fetched_at.elapsed().as_secs()),
            upstream_models,
            models,
        }
    }

    pub async fn list_for_snapshot(&self, _snapshot: BackendSnapshot) -> ModelsListResponse {
        let inner = self.inner.read().await;
        let dynamic_models = inner.models.clone();
        drop(inner);
        ModelsListResponse {
            object: "list",
            data: copilot_models(&dynamic_models),
            models: rich_copilot_models(&dynamic_models),
        }
    }

    pub async fn get_copilot_openai_model(&self, model: &str) -> String {
        let model = strip_model_prefix(model);
        if model == "default" {
            return "claude-sonnet-4.6".to_string();
        }
        let inner = self.inner.read().await;
        if let Some(target) = inner.copilot_overrides.get(model) {
            return target.clone();
        }
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
        COPILOT_MODEL_ALIASES
            .iter()
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
        let model = canonical_model_id(model);
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
        let model = canonical_model_id(model);
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
            static_copilot_model(model)
                .map(static_supported_endpoints)
                .unwrap_or_default()
        } else {
            dynamic
        }
    }
}

fn strip_model_prefix(model: &str) -> &str {
    model.strip_prefix("github-copilot/").unwrap_or(model)
}

fn canonical_model_id(model: &str) -> &str {
    let model = strip_model_prefix(model);
    COPILOT_MODEL_ALIASES
        .iter()
        .find_map(|(source, target)| (*source == model).then_some(*target))
        .unwrap_or(model)
}

fn static_copilot_model(model: &str) -> Option<&'static StaticCopilotModel> {
    let model = canonical_model_id(model);
    STATIC_COPILOT_MODELS
        .iter()
        .find(|static_model| static_model.id == model)
}

fn static_supported_efforts(model: &str) -> Option<SupportedEfforts> {
    let static_model = static_copilot_model(model)?;
    SupportedEfforts::new(static_model.supported_efforts.to_vec())
}

fn static_supported_endpoints(model: &StaticCopilotModel) -> Vec<String> {
    model
        .supported_endpoints
        .iter()
        .map(|endpoint| (*endpoint).to_string())
        .collect()
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
