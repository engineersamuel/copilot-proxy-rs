use std::collections::BTreeMap;
use std::env;
use std::fs;
use std::path::PathBuf;

use serde::Deserialize;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("failed to read config file {path}: {source}")]
    Read {
        path: String,
        #[source]
        source: std::io::Error,
    },
    #[error("failed to parse config file {path}: {source}")]
    Parse {
        path: String,
        #[source]
        source: serde_json::Error,
    },
    #[error(
        "refusing non-loopback bind address {host:?}; set COPILOT_PROXY_RS_ALLOW_NON_LOOPBACK=true only behind trusted network controls or inbound auth"
    )]
    UnsafeBind { host: String },
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(default)]
pub struct ModelOverrides {
    pub copilot: BTreeMap<String, String>,
    pub bedrock: BTreeMap<String, String>,
}

fn is_loopback_bind_host(host: &str) -> bool {
    let host = host.trim();
    if matches!(host, "localhost" | "127.0.0.1" | "::1" | "[::1]") {
        return true;
    }
    host.parse::<std::net::IpAddr>()
        .is_ok_and(|addr| addr.is_loopback())
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(default)]
pub struct AppConfig {
    #[serde(deserialize_with = "deserialize_string")]
    pub backend: String,
    #[serde(deserialize_with = "deserialize_string")]
    pub fallback_backend: String,
    #[serde(deserialize_with = "deserialize_string")]
    pub host: String,
    #[serde(deserialize_with = "deserialize_u16")]
    pub port: u16,
    #[serde(deserialize_with = "deserialize_u64")]
    pub copilot_timeout: u64,
    #[serde(deserialize_with = "deserialize_u64")]
    pub copilot_models_ttl: u64,
    #[serde(deserialize_with = "deserialize_u32")]
    pub copilot_retry_max: u32,
    #[serde(deserialize_with = "deserialize_f64")]
    pub copilot_retry_base_delay: f64,
    #[serde(deserialize_with = "deserialize_u32")]
    pub copilot_max_rate: u32,
    #[serde(deserialize_with = "deserialize_f64")]
    pub context_guard_threshold: f64,
    #[serde(deserialize_with = "deserialize_string")]
    pub bedrock_region_prefix: String,
    #[serde(deserialize_with = "deserialize_string")]
    pub aws_region: String,
    #[serde(deserialize_with = "deserialize_u64")]
    pub bedrock_read_timeout: u64,
    #[serde(default = "default_true", deserialize_with = "deserialize_bool")]
    pub update_check: bool,
    #[serde(default = "default_true", deserialize_with = "deserialize_bool")]
    pub auto_restart: bool,
    #[serde(deserialize_with = "deserialize_bool")]
    pub auto_update: bool,
    #[serde(deserialize_with = "deserialize_bool")]
    pub allow_non_loopback_bind: bool,
    #[serde(deserialize_with = "deserialize_bool")]
    pub container_loopback_only: bool,
    #[serde(deserialize_with = "deserialize_string")]
    pub log_level: String,
    #[serde(deserialize_with = "deserialize_string")]
    pub cowork_host: String,
    #[serde(deserialize_with = "deserialize_u16")]
    pub cowork_port: u16,
    #[serde(deserialize_with = "deserialize_bool")]
    pub hide_getting_started: bool,
    pub enabled_mcp_servers: BTreeMap<String, bool>,
    #[serde(deserialize_with = "deserialize_string_vec")]
    pub search_provider_order: Vec<String>,
    pub model_overrides: ModelOverrides,
}

#[derive(Debug, Clone, Default)]
pub struct EnvSource {
    values: BTreeMap<String, String>,
}

impl EnvSource {
    pub fn current() -> Self {
        Self {
            values: env::vars().collect(),
        }
    }

    pub fn from_pairs<const N: usize>(pairs: [(&str, &str); N]) -> Self {
        Self {
            values: pairs
                .into_iter()
                .map(|(key, value)| (key.to_string(), value.to_string()))
                .collect(),
        }
    }

    pub fn get(&self, key: &str) -> Option<&str> {
        self.values.get(key).map(String::as_str)
    }

    pub fn into_inner(self) -> BTreeMap<String, String> {
        self.values
    }
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            backend: "copilot".to_string(),
            fallback_backend: String::new(),
            host: "127.0.0.1".to_string(),
            port: 8080,
            copilot_timeout: 300,
            copilot_models_ttl: 300,
            copilot_retry_max: 3,
            copilot_retry_base_delay: 1.0,
            copilot_max_rate: 15,
            context_guard_threshold: 0.90,
            bedrock_region_prefix: "us".to_string(),
            aws_region: "us-west-2".to_string(),
            bedrock_read_timeout: 300,
            update_check: true,
            auto_restart: true,
            auto_update: false,
            allow_non_loopback_bind: false,
            container_loopback_only: false,
            log_level: "INFO".to_string(),
            cowork_host: "198.18.1.1".to_string(),
            cowork_port: 8443,
            hide_getting_started: false,
            enabled_mcp_servers: BTreeMap::new(),
            search_provider_order: vec!["tavily".to_string(), "exa".to_string()],
            model_overrides: ModelOverrides::default(),
        }
    }
}

/// Internal partial-config type used only for JSON file deserialization.
///
/// Every scalar field is `Option<T>` so that absent keys and invalid values
/// both map to `None` — allowing the caller to distinguish "key not present"
/// from "key present with value zero/false/empty" and apply a true
/// presence-based merge onto the defaults.  Maps are kept as concrete types
/// because an empty map is a legitimate explicit value.
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct FileConfig {
    #[serde(deserialize_with = "deserialize_opt_string")]
    backend: Option<String>,
    #[serde(deserialize_with = "deserialize_opt_string")]
    fallback_backend: Option<String>,
    #[serde(deserialize_with = "deserialize_opt_string")]
    host: Option<String>,
    #[serde(deserialize_with = "deserialize_opt_u16")]
    port: Option<u16>,
    #[serde(deserialize_with = "deserialize_opt_u64")]
    copilot_timeout: Option<u64>,
    #[serde(deserialize_with = "deserialize_opt_u64")]
    copilot_models_ttl: Option<u64>,
    #[serde(deserialize_with = "deserialize_opt_u32")]
    copilot_retry_max: Option<u32>,
    #[serde(deserialize_with = "deserialize_opt_f64")]
    copilot_retry_base_delay: Option<f64>,
    #[serde(deserialize_with = "deserialize_opt_u32")]
    copilot_max_rate: Option<u32>,
    #[serde(deserialize_with = "deserialize_opt_f64")]
    context_guard_threshold: Option<f64>,
    #[serde(deserialize_with = "deserialize_opt_string")]
    bedrock_region_prefix: Option<String>,
    #[serde(deserialize_with = "deserialize_opt_string")]
    aws_region: Option<String>,
    #[serde(deserialize_with = "deserialize_opt_u64")]
    bedrock_read_timeout: Option<u64>,
    #[serde(deserialize_with = "deserialize_opt_bool")]
    update_check: Option<bool>,
    #[serde(deserialize_with = "deserialize_opt_bool")]
    auto_restart: Option<bool>,
    #[serde(deserialize_with = "deserialize_opt_bool")]
    auto_update: Option<bool>,
    #[serde(deserialize_with = "deserialize_opt_bool")]
    allow_non_loopback_bind: Option<bool>,
    #[serde(deserialize_with = "deserialize_opt_bool")]
    container_loopback_only: Option<bool>,
    #[serde(deserialize_with = "deserialize_opt_string")]
    log_level: Option<String>,
    #[serde(deserialize_with = "deserialize_opt_string")]
    cowork_host: Option<String>,
    #[serde(deserialize_with = "deserialize_opt_u16")]
    cowork_port: Option<u16>,
    #[serde(deserialize_with = "deserialize_opt_bool")]
    hide_getting_started: Option<bool>,
    enabled_mcp_servers: BTreeMap<String, bool>,
    #[serde(deserialize_with = "deserialize_opt_string_vec")]
    search_provider_order: Option<Vec<String>>,
    model_overrides: ModelOverrides,
}

impl AppConfig {
    pub fn load() -> Result<Self, ConfigError> {
        Self::load_from_env(&EnvSource::current())
    }

    pub fn load_from_env(env: &EnvSource) -> Result<Self, ConfigError> {
        let mut config = Self::default();
        let config_path = config_path(env);

        if config_path.exists() {
            let raw = fs::read_to_string(&config_path).map_err(|source| ConfigError::Read {
                path: config_path.display().to_string(),
                source,
            })?;
            let mut raw_value: serde_json::Value =
                serde_json::from_str(&raw).map_err(|source| ConfigError::Parse {
                    path: config_path.display().to_string(),
                    source,
                })?;
            sanitize_file_bools(&mut raw_value);
            let file_config: FileConfig =
                serde_json::from_value(raw_value).map_err(|source| ConfigError::Parse {
                    path: config_path.display().to_string(),
                    source,
                })?;
            config.merge_file_values(file_config);
        }

        apply_env_overrides(&mut config, env);
        Ok(config)
    }

    pub fn validate_bind_safety(&self) -> Result<(), ConfigError> {
        if self.allow_non_loopback_bind
            || self.container_loopback_only
            || is_loopback_bind_host(&self.host)
        {
            return Ok(());
        }
        Err(ConfigError::UnsafeBind {
            host: self.host.clone(),
        })
    }

    /// Merge values from a parsed file config onto `self`.
    ///
    /// Each scalar field uses `Option<T>` in `FileConfig` so that:
    /// - `None`      → key was absent or invalid  → keep default
    /// - `Some(v)`   → key was present and valid   → apply `v` (even if zero)
    ///
    /// String fields additionally skip empty values because an empty string
    /// is never a meaningful override for host/backend/region identifiers.
    /// Maps use the existing non-empty sentinel because map *presence* is
    /// encoded by the map itself (empty vs non-empty).
    fn merge_file_values(&mut self, file: FileConfig) {
        if let Some(v) = file.backend {
            if !v.is_empty() {
                self.backend = v;
            }
        }
        if let Some(v) = file.fallback_backend {
            self.fallback_backend = v;
        }
        if let Some(v) = file.host {
            if !v.is_empty() {
                self.host = v;
            }
        }
        if let Some(v) = file.port {
            self.port = v;
        }
        if let Some(v) = file.copilot_timeout {
            self.copilot_timeout = v;
        }
        if let Some(v) = file.copilot_models_ttl {
            self.copilot_models_ttl = v;
        }
        if let Some(v) = file.copilot_retry_max {
            self.copilot_retry_max = v;
        }
        if let Some(v) = file.copilot_retry_base_delay {
            self.copilot_retry_base_delay = v;
        }
        if let Some(v) = file.copilot_max_rate {
            self.copilot_max_rate = v;
        }
        if let Some(v) = file.context_guard_threshold {
            self.context_guard_threshold = v;
        }
        if let Some(v) = file.bedrock_region_prefix {
            if !v.is_empty() {
                self.bedrock_region_prefix = v;
            }
        }
        if let Some(v) = file.aws_region {
            if !v.is_empty() {
                self.aws_region = v;
            }
        }
        if let Some(v) = file.bedrock_read_timeout {
            self.bedrock_read_timeout = v;
        }
        if let Some(v) = file.update_check {
            self.update_check = v;
        }
        if let Some(v) = file.auto_restart {
            self.auto_restart = v;
        }
        if let Some(v) = file.auto_update {
            self.auto_update = v;
        }
        if let Some(v) = file.allow_non_loopback_bind {
            self.allow_non_loopback_bind = v;
        }
        if let Some(v) = file.container_loopback_only {
            self.container_loopback_only = v;
        }
        if let Some(v) = file.log_level {
            if !v.is_empty() {
                self.log_level = v;
            }
        }
        if let Some(v) = file.cowork_host {
            if !v.is_empty() {
                self.cowork_host = v;
            }
        }
        if let Some(v) = file.cowork_port {
            self.cowork_port = v;
        }
        if let Some(v) = file.hide_getting_started {
            self.hide_getting_started = v;
        }
        if !file.enabled_mcp_servers.is_empty() {
            self.enabled_mcp_servers = file.enabled_mcp_servers;
        }
        if let Some(v) = file.search_provider_order {
            if !v.is_empty() {
                self.search_provider_order = v;
            }
        }
        if !file.model_overrides.copilot.is_empty() {
            self.model_overrides.copilot = file.model_overrides.copilot;
        }
        if !file.model_overrides.bedrock.is_empty() {
            self.model_overrides.bedrock = file.model_overrides.bedrock;
        }
    }
}

fn config_path(env: &EnvSource) -> PathBuf {
    if let Some(dir) = env.get("COPILOT_PROXY_RS_CONFIG_DIR") {
        return PathBuf::from(dir).join("config.json");
    }
    let home = env
        .get("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));
    home.join(".config")
        .join("copilot-proxy-rs")
        .join("config.json")
}

fn apply_env_overrides(config: &mut AppConfig, env: &EnvSource) {
    if let Some(value) = env.get("COPILOT_PROXY_RS_BACKEND") {
        let mut parts = value
            .split(',')
            .map(str::trim)
            .filter(|part| !part.is_empty());
        if let Some(primary) = parts.next() {
            config.backend = primary.to_ascii_lowercase();
            config.fallback_backend = parts.next().unwrap_or("").to_ascii_lowercase();
        }
    }
    apply_string(env, "COPILOT_PROXY_RS_HOST", &mut config.host);
    apply_parse(env, "COPILOT_PROXY_RS_PORT", &mut config.port);
    apply_parse(env, "COPILOT_TIMEOUT", &mut config.copilot_timeout);
    apply_parse(env, "COPILOT_MODELS_TTL", &mut config.copilot_models_ttl);
    apply_parse(env, "COPILOT_RETRY_MAX", &mut config.copilot_retry_max);
    apply_parse(
        env,
        "COPILOT_RETRY_BASE_DELAY",
        &mut config.copilot_retry_base_delay,
    );
    apply_parse(env, "COPILOT_MAX_RATE", &mut config.copilot_max_rate);
    apply_parse(
        env,
        "CONTEXT_GUARD_THRESHOLD",
        &mut config.context_guard_threshold,
    );
    apply_string(
        env,
        "BEDROCK_REGION_PREFIX",
        &mut config.bedrock_region_prefix,
    );
    apply_string(env, "AWS_REGION", &mut config.aws_region);
    apply_parse(
        env,
        "BEDROCK_READ_TIMEOUT",
        &mut config.bedrock_read_timeout,
    );
    apply_bool(
        env,
        "COPILOT_PROXY_RS_UPDATE_CHECK",
        &mut config.update_check,
    );
    apply_bool(
        env,
        "COPILOT_PROXY_RS_AUTO_RESTART",
        &mut config.auto_restart,
    );
    apply_bool(env, "COPILOT_PROXY_RS_AUTO_UPDATE", &mut config.auto_update);
    apply_bool(
        env,
        "COPILOT_PROXY_RS_ALLOW_NON_LOOPBACK",
        &mut config.allow_non_loopback_bind,
    );
    apply_bool(
        env,
        "COPILOT_PROXY_RS_CONTAINER_LOOPBACK_ONLY",
        &mut config.container_loopback_only,
    );
    apply_string(env, "COPILOT_PROXY_RS_LOG_LEVEL", &mut config.log_level);
    config.log_level = config.log_level.to_ascii_uppercase();
    apply_string(env, "COPILOT_PROXY_RS_COWORK_HOST", &mut config.cowork_host);
    apply_parse(env, "COPILOT_PROXY_RS_COWORK_PORT", &mut config.cowork_port);
}

fn apply_string(env: &EnvSource, key: &str, target: &mut String) {
    if let Some(value) = env.get(key) {
        *target = value.to_string();
    }
}

fn apply_parse<T>(env: &EnvSource, key: &str, target: &mut T)
where
    T: std::str::FromStr,
{
    if let Some(value) = env.get(key).and_then(|value| value.parse::<T>().ok()) {
        *target = value;
    }
}

fn apply_bool(env: &EnvSource, key: &str, target: &mut bool) {
    if let Some(value) = env.get(key).and_then(parse_bool) {
        *target = value;
    }
}

fn parse_bool(value: &str) -> Option<bool> {
    match value.trim().to_ascii_lowercase().as_str() {
        "true" | "1" | "yes" | "on" => Some(true),
        "false" | "0" | "no" | "off" => Some(false),
        _ => None,
    }
}

fn parse_json_bool(v: &serde_json::Value) -> Option<bool> {
    match v {
        serde_json::Value::Bool(b) => Some(*b),
        serde_json::Value::String(s) => parse_bool(s),
        _ => None,
    }
}

/// Remove bool keys whose values cannot be parsed as a valid boolean.
/// This ensures that invalid file values (e.g. `"garbage"`) do not clobber
/// true defaults: serde will fall back to the field's `#[serde(default)]` when
/// the key is absent rather than silently coercing to `false`.
fn sanitize_file_bools(raw: &mut serde_json::Value) {
    const BOOL_KEYS: &[&str] = &[
        "update_check",
        "auto_restart",
        "auto_update",
        "hide_getting_started",
    ];
    if let Some(obj) = raw.as_object_mut() {
        for &key in BOOL_KEYS {
            if obj.get(key).is_some_and(|v| parse_json_bool(v).is_none()) {
                obj.remove(key);
            }
        }
    }
}

fn default_true() -> bool {
    true
}

fn deserialize_string<'de, D>(deserializer: D) -> Result<String, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = Option::<serde_json::Value>::deserialize(deserializer)?;
    Ok(match value {
        Some(serde_json::Value::String(s)) => s,
        Some(other) => other.to_string(),
        None => String::new(),
    })
}

fn deserialize_bool<'de, D>(deserializer: D) -> Result<bool, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = Option::<serde_json::Value>::deserialize(deserializer)?;
    match value {
        Some(serde_json::Value::Bool(b)) => Ok(b),
        Some(serde_json::Value::String(s)) => parse_bool(&s)
            .ok_or_else(|| serde::de::Error::custom(format!("invalid bool string: {s:?}"))),
        Some(other) => Err(serde::de::Error::custom(format!(
            "invalid type for bool field: {other}"
        ))),
        None => Err(serde::de::Error::custom(
            "invalid type for bool field: null",
        )),
    }
}

fn deserialize_u16<'de, D>(deserializer: D) -> Result<u16, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = Option::<serde_json::Value>::deserialize(deserializer)?;
    Ok(value
        .and_then(parse_u64_value)
        .and_then(|n| u16::try_from(n).ok())
        .unwrap_or_default())
}

fn deserialize_u32<'de, D>(deserializer: D) -> Result<u32, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = Option::<serde_json::Value>::deserialize(deserializer)?;
    Ok(value
        .and_then(parse_u64_value)
        .and_then(|n| u32::try_from(n).ok())
        .unwrap_or_default())
}

fn deserialize_u64<'de, D>(deserializer: D) -> Result<u64, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = Option::<serde_json::Value>::deserialize(deserializer)?;
    Ok(value.and_then(parse_u64_value).unwrap_or_default())
}

fn deserialize_f64<'de, D>(deserializer: D) -> Result<f64, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = Option::<serde_json::Value>::deserialize(deserializer)?;
    Ok(match value {
        Some(serde_json::Value::Number(n)) => n.as_f64().unwrap_or_default(),
        Some(serde_json::Value::String(s)) => s.parse().unwrap_or_default(),
        _ => 0.0,
    })
}

fn deserialize_string_vec<'de, D>(deserializer: D) -> Result<Vec<String>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = Option::<Vec<String>>::deserialize(deserializer)?;
    Ok(value.unwrap_or_default())
}

fn parse_u64_value(value: serde_json::Value) -> Option<u64> {
    match value {
        serde_json::Value::Number(n) => n.as_u64(),
        serde_json::Value::String(s) => s.parse().ok(),
        _ => None,
    }
}

// --- Option-based deserializers for FileConfig ---
// These return None for absent or invalid values so that the merge step can
// distinguish "key not present / invalid" from "key explicitly set to zero".

fn deserialize_opt_string<'de, D>(deserializer: D) -> Result<Option<String>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = Option::<serde_json::Value>::deserialize(deserializer)?;
    Ok(match value {
        Some(serde_json::Value::String(s)) => Some(s),
        Some(other) => Some(other.to_string()),
        None => None,
    })
}

fn deserialize_opt_bool<'de, D>(deserializer: D) -> Result<Option<bool>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = Option::<serde_json::Value>::deserialize(deserializer)?;
    Ok(value.and_then(|v| parse_json_bool(&v)))
}

fn deserialize_opt_u16<'de, D>(deserializer: D) -> Result<Option<u16>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = Option::<serde_json::Value>::deserialize(deserializer)?;
    Ok(value
        .and_then(parse_u64_value)
        .and_then(|n| u16::try_from(n).ok()))
}

fn deserialize_opt_u32<'de, D>(deserializer: D) -> Result<Option<u32>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = Option::<serde_json::Value>::deserialize(deserializer)?;
    Ok(value
        .and_then(parse_u64_value)
        .and_then(|n| u32::try_from(n).ok()))
}

fn deserialize_opt_u64<'de, D>(deserializer: D) -> Result<Option<u64>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = Option::<serde_json::Value>::deserialize(deserializer)?;
    Ok(value.and_then(parse_u64_value))
}

fn deserialize_opt_f64<'de, D>(deserializer: D) -> Result<Option<f64>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = Option::<serde_json::Value>::deserialize(deserializer)?;
    Ok(match value {
        Some(serde_json::Value::Number(n)) => n.as_f64(),
        Some(serde_json::Value::String(s)) => s.parse().ok(),
        _ => None,
    })
}

fn deserialize_opt_string_vec<'de, D>(deserializer: D) -> Result<Option<Vec<String>>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    Option::<Vec<String>>::deserialize(deserializer)
}
