//! Optional Artificial Analysis (AA) stats for `/models`.
//!
//! The catalog is fetched in the background, cached in memory, and the last
//! good copy is served when AA fails or rate-limits. Nothing here can fail a
//! `/models` request.

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

const AA_BASE_URL: &str = "https://artificialanalysis.ai/api/v2";

const FAILURE_BACKOFF: Duration = Duration::from_secs(15 * 60);
const MAX_BACKOFF: Duration = Duration::from_secs(24 * 60 * 60);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(15);
const MAX_PAGES: u32 = 10;

/// Copilot id (normalized) -> AA slug, for names the generic rules miss.
const AA_ALIASES: &[(&str, &str)] = &[
    ("gpt-6-1-sol", "gpt-6-1-sol-xhigh"),
    ("gpt-5-4-mini", "gpt-5-4-mini-medium"),
    ("gpt-5-6-tera", "gpt-5-6-terra"),
    ("claude-opus-4-8-fast", "claude-opus-4-8"),
];

/// Coding/agentic tags need real signal: absolute floors plus a ratio to the
/// intelligence index (tuned so only the top ~20% of the AA catalog qualify).
/// Fixed cut (AA intelligence index v4.x) for the `high-intelligence` tag.
const HIGH_INTELLIGENCE_CUT: f64 = 45.0;
const MIN_INTELLIGENCE_FOR_TAGS: f64 = 25.0;
const CODING_RATIO: f64 = 1.9;
const MIN_CODING_INDEX: f64 = 70.0;
const AGENTIC_RATIO: f64 = 1.1;
const MIN_AGENTIC_INDEX: f64 = 45.0;

/// Suffixes AA appends to a slug to name a reasoning-effort variant.
const VARIANT_SUFFIXES: &[&str] = &[
    "minimal",
    "low",
    "medium",
    "high",
    "xhigh",
    "max",
    "reasoning",
    "non-reasoning",
    "thinking",
];

/// Descriptions and tags taken from the GitHub Copilot model comparison page
/// (see docs/model-stats.md), keyed by normalized model id.
struct CuratedEntry {
    id: &'static str,
    description: &'static str,
    excels_at: &'static [&'static str],
}

const CURATED: &[CuratedEntry] = &[
    CuratedEntry {
        id: "gpt-5-mini",
        description: "General-purpose coding and writing. Fast, accurate code completions and explanations.",
        excels_at: &["code-completion", "explanations"],
    },
    CuratedEntry {
        id: "gpt-5-3-codex",
        description: "Agentic software development.",
        excels_at: &["agentic-coding"],
    },
    CuratedEntry {
        id: "gpt-5-4",
        description: "Deep reasoning and debugging. Multi-step problem solving and architecture-level code analysis.",
        excels_at: &["deep-reasoning", "debugging", "architecture-analysis"],
    },
    CuratedEntry {
        id: "gpt-5-4-mini",
        description: "Agentic software development. Codebase exploration, especially with grep-style tools.",
        excels_at: &["agentic-coding", "codebase-exploration"],
    },
    CuratedEntry {
        id: "gpt-5-5",
        description: "Deep reasoning and debugging. Multi-step problem solving and architecture-level code analysis.",
        excels_at: &["deep-reasoning", "debugging", "architecture-analysis"],
    },
    CuratedEntry {
        id: "gpt-5-6-luna",
        description: "Fast help with simple or repetitive tasks. Quick, cost-efficient responses for smaller coding tasks.",
        excels_at: &["quick-tasks", "cost-efficiency"],
    },
    CuratedEntry {
        id: "gpt-5-6-sol",
        description: "Deep reasoning and debugging. Complex reasoning over large codebases and long-running agentic work.",
        excels_at: &[
            "deep-reasoning",
            "large-codebases",
            "long-running-agentic-work",
        ],
    },
    CuratedEntry {
        id: "gpt-5-6-terra",
        description: "General-purpose coding and agent tasks. Balanced everyday interactive and agentic coding.",
        excels_at: &["everyday-coding", "agentic-coding"],
    },
    CuratedEntry {
        id: "gpt-6-astra",
        description: "Long-horizon, autonomous coding and agentic tasks. Continuous planning, batched diagnosis and verification, and independent result confirmation.",
        excels_at: &["long-horizon-coding", "autonomous-agents", "verification"],
    },
    CuratedEntry {
        id: "gpt-6-luna",
        description: "Fast help with simple or repetitive tasks. Quick, cost-efficient responses for smaller, faster tasks.",
        excels_at: &["quick-tasks", "cost-efficiency"],
    },
    CuratedEntry {
        id: "gpt-6-sol",
        description: "Interactive and agentic coding. All-round development tasks that benefit from careful, multistep validation.",
        excels_at: &[
            "interactive-coding",
            "agentic-coding",
            "multistep-validation",
        ],
    },
    CuratedEntry {
        id: "gpt-6-1-sol",
        description: "Complex coding tasks. Advanced, efficient reasoning.",
        excels_at: &["complex-coding", "efficient-reasoning"],
    },
    CuratedEntry {
        id: "claude-fable-5",
        description: "Long-horizon, autonomous coding and knowledge work. First-attempt correctness through upfront reasoning, parallel tool batching, and proactive verification.",
        excels_at: &[
            "long-horizon-coding",
            "knowledge-work",
            "first-attempt-correctness",
        ],
    },
    CuratedEntry {
        id: "claude-fable-5-1",
        description: "Long-horizon, autonomous coding and knowledge work. Substantial long-running tasks: deep codebase research, feature development, complex agentic workflows.",
        excels_at: &[
            "long-horizon-coding",
            "codebase-research",
            "agentic-workflows",
        ],
    },
    CuratedEntry {
        id: "claude-haiku-4-5",
        description: "Fast help with simple or repetitive tasks. Fast, reliable answers to lightweight coding questions.",
        excels_at: &["quick-tasks", "lightweight-coding-questions"],
    },
    CuratedEntry {
        id: "claude-haiku-5-5",
        description: "Fast help with simple or repetitive tasks. Fast, reliable answers to lightweight coding questions.",
        excels_at: &["quick-tasks", "lightweight-coding-questions"],
    },
    CuratedEntry {
        id: "claude-opus-4-8",
        description: "Deep reasoning and debugging. Complex problem-solving and sophisticated reasoning.",
        excels_at: &["deep-reasoning", "debugging"],
    },
    CuratedEntry {
        id: "claude-opus-5",
        description: "Deep reasoning and debugging. Complex problem-solving and sophisticated reasoning.",
        excels_at: &["deep-reasoning", "debugging"],
    },
    CuratedEntry {
        id: "claude-opus-5-5",
        description: "Long-running agentic coding and knowledge work. Efficient multistep tasks, error recovery, and collaboration.",
        excels_at: &["agentic-coding", "knowledge-work", "error-recovery"],
    },
    CuratedEntry {
        id: "claude-sonnet-4-6",
        description: "General-purpose coding and agent tasks. Complex problem-solving and sophisticated reasoning.",
        excels_at: &["everyday-coding", "agentic-coding"],
    },
    CuratedEntry {
        id: "claude-sonnet-5",
        description: "General-purpose coding and agent tasks. Complex problem-solving and sophisticated reasoning.",
        excels_at: &["everyday-coding", "agentic-coding"],
    },
    CuratedEntry {
        id: "claude-sonnet-5-5",
        description: "General-purpose coding and agent tasks. Efficient task completion with fewer steps, tokens, and tool calls.",
        excels_at: &["everyday-coding", "agentic-coding", "token-efficiency"],
    },
    CuratedEntry {
        id: "gemini-3-7-flash",
        description: "Fast help with simple or repetitive tasks. Fast, reliable answers to lightweight coding questions.",
        excels_at: &["quick-tasks", "lightweight-coding-questions"],
    },
    CuratedEntry {
        id: "gemini-3-8-flash",
        description: "Fast help with simple or repetitive tasks. Fast, reliable answers to lightweight coding questions.",
        excels_at: &["quick-tasks", "lightweight-coding-questions"],
    },
    CuratedEntry {
        id: "gemini-4-argon",
        description: "Complex coding, agentic, and reasoning tasks. Thorough work that tests edge cases before finishing.",
        excels_at: &["complex-coding", "agentic-coding", "edge-case-testing"],
    },
    CuratedEntry {
        id: "mai-code-1-1-flash",
        description: "General-purpose coding and writing, image understanding. Fast code completions and explanations, instruction following, tool use.",
        excels_at: &["code-completion", "instruction-following", "tool-use"],
    },
    CuratedEntry {
        id: "grok-4-5",
        description: "General-purpose coding and agent tasks. Complex problem-solving and sophisticated reasoning.",
        excels_at: &["everyday-coding", "agentic-coding"],
    },
    CuratedEntry {
        id: "grok-4-6",
        description: "General-purpose coding and agent tasks. Complex problem-solving and sophisticated reasoning.",
        excels_at: &["everyday-coding", "agentic-coding"],
    },
    CuratedEntry {
        id: "grok-4-7",
        description: "Agentic coding and complex, multistep workflows.",
        excels_at: &["agentic-coding", "multistep-workflows"],
    },
    CuratedEntry {
        id: "kimi-k3",
        description: "Agentic coding and long-context work. Multi-step agent tasks across large codebases.",
        excels_at: &["agentic-coding", "long-context", "large-codebases"],
    },
];

const GENERIC_DESCRIPTION: &str = "Frontier model for complex work";
const GENERIC_FAST_DESCRIPTION: &str = "Frontier model for quick and fast work";
const FAST_NAME_PARTS: &[&str] = &["haiku", "flash", "mini", "nano", "luna", "lite", "fast"];

/// Ids that are not chat models: no description, no tags.
fn is_non_chat(key: &str) -> bool {
    [
        "embedding",
        "embed",
        "compaction",
        "whisper",
        "tts",
        "moderation",
    ]
    .iter()
    .any(|part| key.contains(part))
}

/// Older generations we have no sourced text for: no description.
fn is_legacy(key: &str) -> bool {
    [
        "gpt-3", "gpt-4", "o1", "o3", "o4", "text-", "claude-3", "claude-2",
    ]
    .iter()
    .any(|prefix| {
        key.strip_prefix(prefix).is_some_and(|rest| {
            rest.is_empty()
                || !rest.starts_with(|c: char| c.is_ascii_alphabetic())
                || *prefix == "text-"
        })
    })
}

/// Major version digits right after `prefix` (`gemini-1.5-pro` -> 1).
fn major_after(key: &str, prefix: &str) -> Option<u32> {
    let rest = key.strip_prefix(prefix)?;
    let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
    digits.parse().ok()
}

/// Generic text only for current-generation chat families.
fn generic_description(key: &str) -> Option<&'static str> {
    let current = key.starts_with("gpt-5")
        || key.starts_with("gpt-6")
        || key.starts_with("kimi")
        || key.starts_with("mai-")
        || major_after(key, "gemini-").is_some_and(|v| v >= 3)
        || major_after(key, "grok-").is_some_and(|v| v >= 4)
        || ["opus", "sonnet", "haiku", "fable"]
            .iter()
            .any(|family| major_after(key, &format!("claude-{family}-")).is_some_and(|v| v >= 4));
    if is_non_chat(key) || is_legacy(key) || !current {
        return None;
    }
    Some(if FAST_NAME_PARTS.iter().any(|part| key.contains(part)) {
        GENERIC_FAST_DESCRIPTION
    } else {
        GENERIC_DESCRIPTION
    })
}

/// Drop a trailing snapshot date (`-2024-08-06` or `-20240806`).
fn strip_snapshot_date(key: &str) -> &str {
    let num =
        |part: &str, len: usize| part.len() == len && part.bytes().all(|c| c.is_ascii_digit());
    let parts: Vec<&str> = key.rsplitn(4, '-').collect();
    if let [d, m, y, rest] = parts[..] {
        if num(d, 2) && num(m, 2) && num(y, 4) {
            return rest;
        }
    }
    match key.rsplit_once('-') {
        Some((rest, d)) if num(d, 8) => rest,
        _ => key,
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AaStats {
    pub aa_slug: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub intelligence_index: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub coding_index: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agentic_index: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub input_price_per_1m_tokens: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub output_price_per_1m_tokens: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub median_output_tokens_per_second: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub creator: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub release_date: Option<String>,
}

/// Extra per-model fields, flattened into a `/models` `data` entry.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ModelInsights {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stats: Option<AaStats>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub excels_at: Vec<String>,
    /// True when `description` / `excels_at` include hand-curated claims.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub curated: bool,
}

/// Data credit required by the Artificial Analysis terms.
pub fn attribution() -> serde_json::Value {
    serde_json::json!({"name": "Artificial Analysis", "url": "https://artificialanalysis.ai"})
}

#[derive(Debug, Default, Deserialize)]
struct AaPage {
    #[serde(default)]
    data: Vec<AaModel>,
    #[serde(default)]
    pagination: AaPagination,
}

#[derive(Debug, Default, Deserialize)]
struct AaPagination {
    #[serde(default)]
    has_more: bool,
}

#[derive(Debug, Default, Deserialize)]
struct AaModel {
    #[serde(default)]
    name: String,
    #[serde(default)]
    slug: String,
    release_date: Option<String>,
    model_creator: Option<AaCreator>,
    #[serde(default)]
    evaluations: AaEvaluations,
    #[serde(default)]
    pricing: AaPricing,
    #[serde(default)]
    performance: AaPerformance,
}

#[derive(Debug, Default, Deserialize)]
struct AaCreator {
    name: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
struct AaEvaluations {
    artificial_analysis_intelligence_index: Option<f64>,
    artificial_analysis_coding_index: Option<f64>,
    artificial_analysis_agentic_index: Option<f64>,
}

#[derive(Debug, Default, Deserialize)]
struct AaPricing {
    price_1m_input_tokens: Option<f64>,
    price_1m_output_tokens: Option<f64>,
}

#[derive(Debug, Default, Deserialize)]
struct AaPerformance {
    median_output_tokens_per_second: Option<f64>,
}

impl AaModel {
    fn intelligence(&self) -> f64 {
        self.evaluations
            .artificial_analysis_intelligence_index
            .unwrap_or(f64::MIN)
    }

    fn stats(&self) -> AaStats {
        AaStats {
            aa_slug: self.slug.clone(),
            intelligence_index: self.evaluations.artificial_analysis_intelligence_index,
            coding_index: self.evaluations.artificial_analysis_coding_index,
            agentic_index: self.evaluations.artificial_analysis_agentic_index,
            input_price_per_1m_tokens: self.pricing.price_1m_input_tokens,
            output_price_per_1m_tokens: self.pricing.price_1m_output_tokens,
            median_output_tokens_per_second: self.performance.median_output_tokens_per_second,
            creator: self.model_creator.as_ref().and_then(|c| c.name.clone()),
            release_date: self.release_date.clone(),
        }
    }
}

/// Lowercase, turn every non-alphanumeric run into one hyphen.
fn normalize(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for ch in value.chars() {
        if ch.is_ascii_alphanumeric() {
            out.push(ch.to_ascii_lowercase());
        } else if !out.is_empty() && !out.ends_with('-') {
            out.push('-');
        }
    }
    out.trim_end_matches('-').to_string()
}

fn strip_parentheticals(name: &str) -> String {
    let mut depth = 0u32;
    name.chars()
        .filter(|ch| match ch {
            '(' | '[' => {
                depth += 1;
                false
            }
            ')' | ']' => {
                depth = depth.saturating_sub(1);
                false
            }
            _ => depth == 0,
        })
        .collect()
}

/// Parsed AA catalog with match indexes. Variants of one model (effort
/// levels) share a key; the highest intelligence index wins.
#[derive(Debug, Default)]
struct Catalog {
    models: Vec<AaModel>,
    by_slug: BTreeMap<String, usize>,
    by_name: BTreeMap<String, usize>,
}

impl Catalog {
    fn from_models(models: Vec<AaModel>) -> Self {
        let mut by_slug = BTreeMap::new();
        let mut by_name: BTreeMap<String, usize> = BTreeMap::new();
        for (index, model) in models.iter().enumerate() {
            by_slug.insert(normalize(&model.slug), index);
            let key = normalize(&strip_parentheticals(&model.name));
            if key.is_empty() {
                continue;
            }
            match by_name.get(&key) {
                Some(&best) if models[best].intelligence() >= model.intelligence() => {}
                _ => {
                    by_name.insert(key, index);
                }
            }
        }
        Self {
            models,
            by_slug,
            by_name,
        }
    }

    /// Slug, normalized name, alias map, then conservative variant-suffix match.
    fn find(&self, model_id: &str) -> Option<&AaModel> {
        let key = normalize(model_id);
        if key.is_empty() {
            return None;
        }
        let index = self
            .by_slug
            .get(&key)
            .or_else(|| self.by_name.get(&key))
            .or_else(|| {
                AA_ALIASES
                    .iter()
                    .find(|(from, _)| *from == key)
                    .and_then(|(_, slug)| self.by_slug.get(*slug))
            })
            .copied()
            .or_else(|| self.find_variant(&key))?;
        self.models.get(index)
    }

    /// Match `<key>-<effort suffix>` slugs only; never looser prefixes.
    fn find_variant(&self, key: &str) -> Option<usize> {
        let prefix = format!("{key}-");
        self.by_slug
            .iter()
            .filter(|(slug, _)| {
                slug.strip_prefix(&prefix)
                    .is_some_and(|rest| VARIANT_SUFFIXES.contains(&rest))
            })
            .map(|(_, &index)| index)
            .max_by(|&a, &b| {
                self.models[a]
                    .intelligence()
                    .total_cmp(&self.models[b].intelligence())
            })
    }
}

fn curated_entry(model_id: &str) -> Option<&'static CuratedEntry> {
    let key = normalize(model_id);
    let find = |k: &str| CURATED.iter().find(|entry| entry.id == k);
    find(&key)
        .or_else(|| find(strip_snapshot_date(&key)))
        .or_else(|| {
            AA_ALIASES
                .iter()
                .find(|(from, _)| *from == key)
                .and_then(|(_, target)| find(target))
        })
}

/// Tags derived from index ratios and price/speed. Heuristic thresholds.
fn derived_tags(stats: &AaStats) -> Vec<&'static str> {
    let mut tags = Vec::new();
    let intelligence = stats.intelligence_index.filter(|v| *v > 0.0);
    if let Some(intelligence) = intelligence.filter(|v| *v >= MIN_INTELLIGENCE_FOR_TAGS) {
        if stats
            .coding_index
            .is_some_and(|v| v >= MIN_CODING_INDEX && v >= intelligence * CODING_RATIO)
        {
            tags.push("strong-coding-index");
        }
        if stats
            .agentic_index
            .is_some_and(|v| v >= MIN_AGENTIC_INDEX && v >= intelligence * AGENTIC_RATIO)
        {
            tags.push("strong-agentic-index");
        }
        if intelligence >= HIGH_INTELLIGENCE_CUT {
            tags.push("high-intelligence");
        }
    }
    if stats
        .median_output_tokens_per_second
        .is_some_and(|v| v >= 150.0)
    {
        tags.push("high-throughput");
    }
    if stats
        .output_price_per_1m_tokens
        .is_some_and(|v| (0.0..=2.0).contains(&v))
    {
        tags.push("low-price");
    }
    tags
}

/// Description, tags and curated flag are computed from the in-code table on
/// every call, never cached, so table or rule changes apply immediately.
fn build_insights(model_id: &str, stats: Option<AaStats>) -> ModelInsights {
    let key = normalize(model_id);
    if is_non_chat(&key) {
        return ModelInsights {
            stats: None,
            description: None,
            excels_at: Vec::new(),
            curated: false,
        };
    }
    let curated = curated_entry(model_id);
    // Legacy ids keep stats but get no tags: we have nothing sourced to say.
    let tagged = curated.is_some() || (!is_legacy(&key) && generic_description(&key).is_some());
    let mut excels_at: Vec<String> = curated
        .map(|entry| entry.excels_at.iter().map(|tag| tag.to_string()).collect())
        .unwrap_or_default();
    if let Some(stats) = stats.as_ref().filter(|_| tagged) {
        for tag in derived_tags(stats) {
            if !excels_at.iter().any(|existing| existing == tag) {
                excels_at.push(tag.to_string());
            }
        }
    }
    ModelInsights {
        stats,
        description: curated
            .map(|entry| entry.description)
            .or_else(|| generic_description(&key))
            .map(str::to_string),
        excels_at,
        curated: curated.is_some(),
    }
}

#[derive(Debug, Default)]
struct Timing {
    next_attempt: Option<Instant>,
}

/// Bumped to 2: v1 files cached description text, which is now ignored.
const CACHE_VERSION: u32 = 2;

/// Only AA-derived data is cached.
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
struct CachedEntry {
    #[serde(default)]
    stats: Option<AaStats>,
}

/// Per-model AA data, persisted so restarts skip the AA round trip.
#[derive(Debug, Default, Serialize, Deserialize)]
struct CacheState {
    #[serde(default)]
    version: u32,
    /// Unix seconds of the last successful AA fetch.
    #[serde(default)]
    fetched_at_unix: Option<u64>,
    /// Ids already matched against a real AA catalog (even if unmatched).
    #[serde(default)]
    resolved: BTreeSet<String>,
    #[serde(default)]
    entries: BTreeMap<String, CachedEntry>,
}

fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

pub struct ModelStatsStore {
    api_key: String,
    base_url: String,
    client: reqwest::Client,
    catalog: RwLock<Option<Arc<Catalog>>>,
    state: Mutex<CacheState>,
    cache_path: Option<PathBuf>,
    write_seq: Arc<AtomicU64>,
    write_lock: Arc<Mutex<u64>>,
    timing: Mutex<Timing>,
    in_flight: AtomicBool,
}

impl std::fmt::Debug for ModelStatsStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ModelStatsStore")
            .field("api_key", &"<redacted>")
            .finish_non_exhaustive()
    }
}

impl ModelStatsStore {
    pub fn new(api_key: String) -> Self {
        Self::with_base_url(api_key, AA_BASE_URL.to_string())
    }

    pub fn with_base_url(api_key: String, base_url: String) -> Self {
        Self {
            api_key: api_key.trim().to_string(),
            base_url,
            client: reqwest::Client::builder()
                .redirect(reqwest::redirect::Policy::none())
                .timeout(REQUEST_TIMEOUT)
                .build()
                .unwrap_or_default(),
            catalog: RwLock::new(None),
            state: Mutex::new(CacheState::default()),
            cache_path: None,
            write_seq: Arc::new(AtomicU64::new(0)),
            write_lock: Arc::new(Mutex::new(0)),
            timing: Mutex::new(Timing::default()),
            in_flight: AtomicBool::new(false),
        }
    }

    /// Persist enriched data at `path`; loads an existing cache, ignoring a
    /// missing or corrupt file.
    pub fn with_cache_path(mut self, path: PathBuf) -> Self {
        let loaded = std::fs::read(&path)
            .ok()
            .and_then(|bytes| serde_json::from_slice::<CacheState>(&bytes).ok())
            .filter(|state| state.version == CACHE_VERSION);
        if let Some(mut state) = loaded {
            // `resolved` is only trustworthy for ids that still have an entry.
            let entries = &state.entries;
            state.resolved.retain(|id| entries.contains_key(id));
            *self.state.lock().unwrap_or_else(|e| e.into_inner()) = state;
        }
        self.cache_path = Some(path);
        self
    }

    fn enabled(&self) -> bool {
        !self.api_key.is_empty()
    }

    fn compute(&self, model_id: &str) -> CachedEntry {
        let catalog = self
            .catalog
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        CachedEntry {
            stats: catalog
                .as_deref()
                .and_then(|catalog| catalog.find(model_id))
                .map(AaModel::stats),
        }
    }

    /// Align cached entries with the listed ids: drop removed ids, compute
    /// only added ones. Persists only when something changed.
    pub fn sync_models(&self, ids: &[String]) {
        let wanted: BTreeSet<&String> = ids.iter().collect();
        let has_catalog = self
            .catalog
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .is_some();
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let before = state.entries.len();
        state.entries.retain(|id, _| wanted.contains(id));
        state.resolved.retain(|id| wanted.contains(id));
        let mut changed = state.entries.len() != before;
        for id in ids {
            if !state.entries.contains_key(id) {
                let insights = self.compute(id);
                state.entries.insert(id.clone(), insights);
                if has_catalog {
                    state.resolved.insert(id.clone());
                }
                changed = true;
            }
        }
        state.version = CACHE_VERSION;
        if changed {
            self.persist(&state);
        }
    }

    fn needs_refresh(&self, ids: &[String]) -> bool {
        if self
            .timing
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .next_attempt
            .is_some_and(|at| Instant::now() < at)
        {
            return false;
        }
        // Refetch only when a listed id has never been resolved against AA.
        let state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        ids.iter().any(|id| !state.resolved.contains(id))
    }

    /// Start a background refresh when needed. Never blocks.
    pub fn spawn_refresh_if_stale(self: &Arc<Self>, ids: &[String]) {
        if !self.enabled() || !self.needs_refresh(ids) {
            return;
        }
        if self.in_flight.swap(true, Ordering::AcqRel) {
            return;
        }
        let store = Arc::clone(self);
        tokio::spawn(async move {
            store.refresh().await;
            store.in_flight.store(false, Ordering::Release);
        });
    }

    /// Fetch every page; on any failure keep the last good data.
    pub async fn refresh(&self) {
        if !self.enabled() {
            return;
        }
        let outcome = self.fetch_all().await;
        match outcome {
            Ok(catalog) => {
                tracing::info!(
                    models = catalog.models.len(),
                    "refreshed Artificial Analysis catalog"
                );
                *self.catalog.write().unwrap_or_else(|e| e.into_inner()) = Some(Arc::new(catalog));
                let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
                let pending: Vec<String> = state
                    .entries
                    .keys()
                    .filter(|id| !state.resolved.contains(*id))
                    .cloned()
                    .collect();
                for id in pending {
                    let insights = self.compute(&id);
                    state.entries.insert(id.clone(), insights);
                    state.resolved.insert(id);
                }
                state.version = CACHE_VERSION;
                state.fetched_at_unix = Some(unix_now());
                self.persist(&state);
                self.timing
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .next_attempt = None;
            }
            Err(failure) => {
                tracing::warn!(
                    reason = failure.reason,
                    "failed to refresh Artificial Analysis catalog; keeping cached data"
                );
                self.timing
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .next_attempt = Some(Instant::now() + failure.backoff);
            }
        }
    }

    /// Serialize under the caller's lock, then write off-thread. The caller
    /// must drop its state guard before the write happens; only bytes are
    /// handed over. Without a runtime (sync tests) the write is inline.
    fn persist(&self, state: &CacheState) {
        let Some(path) = self.cache_path.clone() else {
            return;
        };
        let Ok(bytes) = serde_json::to_vec(state) else {
            return;
        };
        let seq = self.write_seq.fetch_add(1, Ordering::SeqCst) + 1;
        let last = Arc::clone(&self.write_lock);
        let job = move || write_cache_file(&path, &bytes, seq, &last);
        match tokio::runtime::Handle::try_current() {
            Ok(handle) => {
                handle.spawn_blocking(job);
            }
            Err(_) => job(),
        }
    }

    async fn fetch_all(&self) -> Result<Catalog, FetchFailure> {
        let url = format!("{}/language/models/free", self.base_url);
        let mut models = Vec::new();
        let mut complete = false;
        for page in 1..=MAX_PAGES {
            let response = self
                .client
                .get(&url)
                .header("x-api-key", &self.api_key)
                .query(&[("page", page)])
                .timeout(REQUEST_TIMEOUT)
                .send()
                .await
                .map_err(|_| FetchFailure::new("request failed", FAILURE_BACKOFF))?;
            let status = response.status();
            if !status.is_success() {
                let backoff = if status.as_u16() == 429 {
                    retry_after(response.headers()).unwrap_or(FAILURE_BACKOFF)
                } else {
                    FAILURE_BACKOFF
                };
                return Err(FetchFailure::new("non-success status", backoff));
            }
            let body: AaPage = response
                .json()
                .await
                .map_err(|_| FetchFailure::new("invalid response body", FAILURE_BACKOFF))?;
            models.extend(body.data);
            if !body.pagination.has_more {
                complete = true;
                break;
            }
        }
        if !complete {
            return Err(FetchFailure::new("too many pages", FAILURE_BACKOFF));
        }
        models.retain(|m| !m.slug.is_empty() && !m.name.is_empty());
        if models.is_empty() {
            return Err(FetchFailure::new("empty catalog", FAILURE_BACKOFF));
        }
        Ok(Catalog::from_models(models))
    }

    /// Insights for one public model id. AA stats come from the synced cache
    /// (computed on the fly for unsynced ids); text is always computed here.
    pub fn insights_for(&self, model_id: &str) -> Option<ModelInsights> {
        let cached = self
            .state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .entries
            .get(model_id)
            .cloned();
        let entry = cached.unwrap_or_else(|| self.compute(model_id));
        Some(build_insights(model_id, entry.stats))
    }

    #[cfg(test)]
    fn set_catalog_for_tests(&self, catalog: Catalog) {
        *self.catalog.write().unwrap() = Some(Arc::new(catalog));
    }
}

struct FetchFailure {
    reason: &'static str,
    backoff: Duration,
}

impl FetchFailure {
    fn new(reason: &'static str, backoff: Duration) -> Self {
        Self { reason, backoff }
    }
}

/// Atomic write (temp file + rename). Stale writes (older `seq`) are skipped.
/// Failures are logged without paths or contents.
fn write_cache_file(path: &std::path::Path, bytes: &[u8], seq: u64, last: &Mutex<u64>) {
    let mut last = last.lock().unwrap_or_else(|e| e.into_inner());
    if seq < *last {
        return;
    }
    let result = (|| -> std::io::Result<()> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let tmp = path.with_extension(format!("tmp-{}", std::process::id()));
        std::fs::write(&tmp, bytes)?;
        std::fs::rename(&tmp, path).inspect_err(|_| {
            let _ = std::fs::remove_file(&tmp);
        })
    })();
    match result {
        Ok(()) => *last = seq,
        Err(_) => tracing::warn!("failed to write model stats cache"),
    }
}

fn retry_after(headers: &reqwest::header::HeaderMap) -> Option<Duration> {
    let value = headers
        .get(reqwest::header::RETRY_AFTER)?
        .to_str()
        .ok()?
        .trim();
    let wait = match value.parse::<u64>() {
        Ok(seconds) => Duration::from_secs(seconds),
        Err(_) => httpdate::parse_http_date(value)
            .ok()?
            .duration_since(std::time::SystemTime::now())
            .unwrap_or_default(),
    };
    Some(wait.min(MAX_BACKOFF))
}

#[cfg(test)]
mod tests {
    use super::*;

    const FIXTURE: &str = include_str!("../tests/fixtures/aa_models_sample.json");

    fn catalog() -> Catalog {
        let page: AaPage = serde_json::from_str(FIXTURE).unwrap();
        Catalog::from_models(page.data)
    }

    #[test]
    fn retry_after_parses_seconds_date_and_caps() {
        use reqwest::header::{HeaderMap, HeaderValue, RETRY_AFTER};
        let h = |v: &str| {
            let mut m = HeaderMap::new();
            m.insert(RETRY_AFTER, HeaderValue::from_str(v).unwrap());
            m
        };
        assert_eq!(retry_after(&h("120")), Some(Duration::from_secs(120)));
        assert_eq!(retry_after(&h("999999999")), Some(MAX_BACKOFF));
        assert_eq!(
            retry_after(&h("Wed, 21 Oct 2015 07:28:00 GMT")),
            Some(Duration::ZERO)
        );
        let future =
            httpdate::fmt_http_date(std::time::SystemTime::now() + Duration::from_secs(3600));
        let got = retry_after(&h(&future)).unwrap();
        assert!(got > Duration::from_secs(3500) && got <= Duration::from_secs(3600));
        assert!(retry_after(&h("soon")).is_none());
    }

    #[test]
    fn debug_redacts_api_key() {
        let store = ModelStatsStore::new("sekrit-key-123".into());
        assert!(!format!("{store:?}").contains("sekrit-key-123"));
    }

    #[test]
    fn empty_entries_are_dropped() {
        let page: AaPage = serde_json::from_str(r#"{"data":[{}]}"#).unwrap();
        let usable: Vec<_> = page
            .data
            .into_iter()
            .filter(|m| !m.slug.is_empty() && !m.name.is_empty())
            .collect();
        assert!(usable.is_empty());
    }

    #[test]
    fn normalize_collapses_punctuation() {
        assert_eq!(normalize("Claude Opus 5.5"), "claude-opus-5-5");
        assert_eq!(normalize("gpt-6.1-sol"), "gpt-6-1-sol");
    }

    #[test]
    fn matches_by_slug_and_dotted_id() {
        let catalog = catalog();
        assert_eq!(
            catalog.find("claude-opus-5.5").unwrap().slug,
            "claude-opus-5-5"
        );
    }

    #[test]
    fn matches_by_alias() {
        let catalog = catalog();
        assert_eq!(
            catalog.find("gpt-6.1-sol").unwrap().slug,
            "gpt-6-1-sol-xhigh"
        );
    }

    #[test]
    fn variant_match_picks_highest_intelligence() {
        let catalog = catalog();
        assert_eq!(
            catalog.find("claude-sonnet-5.5").unwrap().slug,
            "claude-sonnet-5-5-high"
        );
    }

    #[test]
    fn unrelated_ids_do_not_match() {
        let catalog = catalog();
        assert!(catalog.find("claude-opus").is_none());
        assert!(catalog.find("totally-unknown").is_none());
    }

    #[test]
    fn null_measurements_are_omitted_not_zero() {
        let store = ModelStatsStore::new("k".into());
        store.set_catalog_for_tests(catalog());
        let value = serde_json::to_value(store.insights_for("claude-opus-5.5").unwrap()).unwrap();
        let stats = &value["stats"];
        assert_eq!(stats["intelligence_index"], 57.6);
        assert!(stats.get("coding_index").is_none());
        assert_eq!(stats["output_price_per_1m_tokens"], 20.0);
        assert_eq!(value["curated"], true);
        assert!(
            value["excels_at"]
                .as_array()
                .unwrap()
                .iter()
                .any(|t| t == "agentic-coding")
        );
    }

    #[test]
    fn unmatched_uncurated_model_has_no_insights() {
        let store = ModelStatsStore::new("k".into());
        store.set_catalog_for_tests(catalog());
        let insights = store.insights_for("totally-unknown").unwrap();
        assert!(insights.stats.is_none());
        assert!(!insights.curated);
        assert!(insights.excels_at.is_empty());
        assert_eq!(insights.description, None, "unknown family: no claim");
        assert_eq!(
            store
                .insights_for("gpt-5.7-sol")
                .unwrap()
                .description
                .as_deref(),
            Some(GENERIC_DESCRIPTION)
        );
        assert_eq!(
            store
                .insights_for("gemini-9-flash")
                .unwrap()
                .description
                .as_deref(),
            Some(GENERIC_FAST_DESCRIPTION)
        );
    }

    #[test]
    fn curated_entry_works_without_catalog() {
        let store = ModelStatsStore::new(String::new());
        let insights = store.insights_for("gpt-6.1-sol").unwrap();
        assert!(insights.stats.is_none());
        assert!(insights.excels_at.contains(&"complex-coding".to_string()));
    }

    #[test]
    fn cache_persists_and_syncs_added_and_removed_ids() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("cache.json");
        let store = ModelStatsStore::new("k".into()).with_cache_path(path.clone());
        store.set_catalog_for_tests(catalog());
        store.sync_models(&["claude-opus-5.5".into(), "gpt-6.1-sol".into()]);
        assert!(path.exists());
        let first = std::fs::read(&path).unwrap();

        // Same ids from a fresh store: served from file, no catalog needed.
        let reloaded = ModelStatsStore::new("k".into()).with_cache_path(path.clone());
        assert!(
            reloaded
                .insights_for("claude-opus-5.5")
                .unwrap()
                .stats
                .is_some()
        );
        reloaded.sync_models(&["claude-opus-5.5".into(), "gpt-6.1-sol".into()]);
        assert_eq!(
            std::fs::read(&path).unwrap(),
            first,
            "unchanged ids rewrite nothing"
        );

        // One removed, one added: only those change.
        reloaded.sync_models(&["claude-opus-5.5".into(), "grok-4.7".into()]);
        let value: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        let entries = value["entries"].as_object().unwrap();
        assert!(entries.contains_key("grok-4.7") && !entries.contains_key("gpt-6.1-sol"));
        assert!(entries["claude-opus-5.5"]["stats"].is_object());
        assert!(entries["claude-opus-5.5"].get("description").is_none());
        assert!(!format!("{value}").contains("api_key"));
    }

    #[test]
    fn corrupt_cache_is_ignored() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("cache.json");
        std::fs::write(&path, b"{not json").unwrap();
        let store = ModelStatsStore::new("k".into()).with_cache_path(path);
        assert!(store.insights_for("claude-opus-5.5").unwrap().curated);
    }

    #[test]
    fn curated_lookup_uses_alias_map() {
        assert!(curated_entry("claude-opus-4.8-fast").is_some());
        assert!(curated_entry("claude-opus-4.8").is_some());
    }

    #[test]
    fn derived_tags_have_floors_and_are_selective() {
        let base = AaStats {
            aa_slug: String::new(),
            intelligence_index: Some(5.0),
            coding_index: Some(30.0),
            agentic_index: Some(30.0),
            input_price_per_1m_tokens: None,
            output_price_per_1m_tokens: None,
            median_output_tokens_per_second: None,
            creator: None,
            release_date: None,
        };
        assert!(derived_tags(&base).is_empty());
        let page: AaPage = serde_json::from_str(FIXTURE).unwrap();
        let tagged = page
            .data
            .iter()
            .filter(|m| derived_tags(&m.stats()).contains(&"strong-coding-index"))
            .count();
        assert!(tagged < page.data.len(), "coding must not tag every model");
    }

    #[test]
    fn no_refetch_when_ids_resolved_and_only_new_ids_trigger_one() {
        let store = ModelStatsStore::new("k".into());
        store.set_catalog_for_tests(catalog());
        let ids = vec!["claude-opus-5.5".to_string()];
        store.sync_models(&ids);
        assert!(!store.needs_refresh(&ids), "resolved ids never go stale");
        let more = vec!["claude-opus-5.5".to_string(), "grok-4.7".to_string()];
        // Catalog is in memory, so the new id resolves locally: still no fetch.
        store.sync_models(&more);
        assert!(!store.needs_refresh(&more));
        // After restart (no catalog) a new id needs one fetch; known ids do not.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("c.json");
        let a = ModelStatsStore::new("k".into()).with_cache_path(path.clone());
        a.set_catalog_for_tests(catalog());
        a.sync_models(&ids);
        let b = ModelStatsStore::new("k".into()).with_cache_path(path);
        assert!(!b.needs_refresh(&ids));
        assert!(b.needs_refresh(&more));
    }

    #[test]
    fn non_chat_legacy_and_snapshot_ids() {
        let store = ModelStatsStore::new(String::new());
        let d = |id: &str| store.insights_for(id).unwrap();
        for id in [
            "text-embedding-3-small",
            "text-embedding-ada-002",
            "trajectory-compaction",
        ] {
            let i = d(id);
            assert!(
                i.description.is_none() && i.excels_at.is_empty() && !i.curated,
                "{id}"
            );
        }
        for id in [
            "gpt-3.5-turbo",
            "gpt-4",
            "gpt-4o",
            "gpt-4o-mini",
            "gpt-4-0613",
            "gpt-4o-2024-08-06",
            "o3-mini",
            "claude-3.5-sonnet",
        ] {
            assert!(d(id).description.is_none(), "{id}");
        }
        assert_eq!(
            d("gpt-5.4-nano").description.as_deref(),
            Some(GENERIC_FAST_DESCRIPTION)
        );
        assert_eq!(
            d("gpt-5.6-sol-fast").description.as_deref(),
            Some(GENERIC_FAST_DESCRIPTION)
        );
        // Dated snapshot inherits a sourced base row only.
        let snap = d("claude-opus-5.5-2026-09-22");
        assert!(snap.curated && snap.description.is_some());
        assert!(d("gpt-5.4-nano-2026-01-02").description.is_some());
        assert!(!d("gpt-5.4-nano-2026-01-02").curated);
    }

    #[test]
    fn legacy_families_get_no_generic_description() {
        let store = ModelStatsStore::new(String::new());
        for id in [
            "gemini-1.5-pro",
            "gemini-2.0-flash",
            "claude-instant-1.2",
            "grok-3",
            "claude-2.1",
        ] {
            assert!(
                store.insights_for(id).unwrap().description.is_none(),
                "{id}"
            );
        }
        assert!(
            store
                .insights_for("gemini-3.9-flash")
                .unwrap()
                .description
                .is_some()
        );
        assert!(
            store
                .insights_for("claude-opus-4.9")
                .unwrap()
                .description
                .is_some()
        );
    }

    #[test]
    fn resolved_without_entry_is_refetched() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("c.json");
        std::fs::write(
            &path,
            format!(r#"{{"version":{CACHE_VERSION},"resolved":["grok-4.7"],"entries":{{}}}}"#),
        )
        .unwrap();
        let store = ModelStatsStore::new("k".into()).with_cache_path(path);
        assert!(store.needs_refresh(&["grok-4.7".to_string()]));
    }

    #[tokio::test]
    async fn persist_inside_runtime_writes_off_thread() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("c.json");
        let store = ModelStatsStore::new("k".into()).with_cache_path(path.clone());
        store.sync_models(&["grok-4.7".to_string()]);
        for _ in 0..100 {
            if path.exists() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        panic!("cache file never written");
    }

    #[test]
    fn old_cache_with_stale_description_is_ignored() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("c.json");
        std::fs::write(
            &path,
            r#"{"version":1,"resolved":["gpt-4"],"entries":{"gpt-4":{"description":"Frontier model for complex work","curated":false}}}"#,
        )
        .unwrap();
        let store = ModelStatsStore::new("k".into()).with_cache_path(path);
        assert!(store.insights_for("gpt-4").unwrap().description.is_none());
        // Same-version file with a stray description field is ignored too.
        let path2 = dir.path().join("d.json");
        std::fs::write(
            &path2,
            format!(r#"{{"version":{CACHE_VERSION},"resolved":["gpt-4"],"entries":{{"gpt-4":{{"description":"Frontier model for complex work"}}}}}}"#),
        )
        .unwrap();
        let store = ModelStatsStore::new("k".into()).with_cache_path(path2);
        assert!(store.insights_for("gpt-4").unwrap().description.is_none());
    }

    #[test]
    fn legacy_ids_keep_stats_but_get_no_tags_and_names_do_not_overlap() {
        let store = ModelStatsStore::new("k".into());
        store.set_catalog_for_tests(catalog());
        let legacy = store.insights_for("gpt-4o").unwrap();
        assert!(legacy.excels_at.is_empty());
        let cur = store.insights_for("claude-opus-5.5").unwrap();
        let mut seen = BTreeSet::new();
        assert!(cur.excels_at.iter().all(|t| seen.insert(t.clone())));
        assert!(
            !cur.excels_at
                .iter()
                .any(|t| t == "agentic" || t == "coding")
        );
        assert!(cur.excels_at.contains(&"high-intelligence".to_string()));
    }

    #[test]
    fn high_intelligence_uses_documented_cut() {
        let mk = |v: f64| AaStats {
            aa_slug: String::new(),
            intelligence_index: Some(v),
            coding_index: None,
            agentic_index: None,
            input_price_per_1m_tokens: None,
            output_price_per_1m_tokens: None,
            median_output_tokens_per_second: None,
            creator: None,
            release_date: None,
        };
        assert!(derived_tags(&mk(46.4)).contains(&"high-intelligence"));
        assert!(!derived_tags(&mk(44.9)).contains(&"high-intelligence"));
    }

    #[test]
    fn coding_tag_fires_for_strong_coding_ratio() {
        let mut m = AaStats {
            aa_slug: String::new(),
            intelligence_index: Some(36.0),
            coding_index: Some(75.0),
            agentic_index: None,
            input_price_per_1m_tokens: None,
            output_price_per_1m_tokens: None,
            median_output_tokens_per_second: None,
            creator: None,
            release_date: None,
        };
        assert!(derived_tags(&m).contains(&"strong-coding-index"));
        m.coding_index = Some(50.0);
        assert!(!derived_tags(&m).contains(&"strong-coding-index"));
    }

    #[test]
    fn zero_price_counts_as_low_price_but_missing_does_not() {
        let mut m = AaStats {
            aa_slug: String::new(),
            intelligence_index: None,
            coding_index: None,
            agentic_index: None,
            input_price_per_1m_tokens: None,
            output_price_per_1m_tokens: Some(0.0),
            median_output_tokens_per_second: None,
            creator: None,
            release_date: None,
        };
        assert!(derived_tags(&m).contains(&"low-price"));
        m.output_price_per_1m_tokens = None;
        assert!(!derived_tags(&m).contains(&"low-price"));
    }
}
