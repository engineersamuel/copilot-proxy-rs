use std::collections::BTreeMap;
use std::fs;

use copilot_proxy_rs::config::{AppConfig, EnvSource};

fn repo_tempdir(prefix: &str) -> tempfile::TempDir {
    tempfile::Builder::new()
        .prefix(prefix)
        .tempdir_in(env!("CARGO_MANIFEST_DIR"))
        .unwrap()
}

#[test]
fn defaults_match_copilot_proxy_rs() {
    let config = AppConfig::default();

    assert_eq!(config.backend, "copilot");
    assert_eq!(config.fallback_backend, "");
    assert_eq!(config.host, "127.0.0.1");
    assert_eq!(config.port, 8080);
    assert_eq!(config.copilot_timeout, 300);
    assert_eq!(config.copilot_models_ttl, 300);
    assert_eq!(config.copilot_retry_max, 3);
    assert_eq!(config.copilot_retry_base_delay, 1.0);
    assert_eq!(config.copilot_max_rate, 15);
    assert_eq!(config.web_search_model, "gpt-5.6-sol");
    assert_eq!(config.context_guard_threshold, 0.90);
    assert_eq!(config.bedrock_region_prefix, "us");
    assert_eq!(config.aws_region, "us-west-2");
    assert_eq!(config.bedrock_read_timeout, 300);
    assert!(config.update_check);
    assert!(config.auto_restart);
    assert!(!config.auto_update);
    assert_eq!(config.log_level, "INFO");
    assert_eq!(config.cowork_host, "198.18.1.1");
    assert_eq!(config.cowork_port, 8443);
    assert!(!config.allow_non_loopback_bind);
    assert!(!config.hide_getting_started);
    assert_eq!(config.search_provider_order, vec!["tavily", "exa"]);
    assert!(config.model_overrides.copilot.is_empty());
    assert!(config.model_overrides.bedrock.is_empty());
}

#[test]
fn loopback_bind_is_publication_safe_by_default() {
    let config = AppConfig {
        host: "127.0.0.1".to_string(),
        ..AppConfig::default()
    };

    config.validate_bind_safety().unwrap();
}

#[test]
fn wildcard_bind_requires_explicit_public_exposure_opt_in() {
    let config = AppConfig {
        host: "0.0.0.0".to_string(),
        ..AppConfig::default()
    };

    let err = config.validate_bind_safety().unwrap_err().to_string();

    assert!(err.contains("non-loopback bind"));
    assert!(err.contains("COPILOT_PROXY_RS_ALLOW_NON_LOOPBACK=true"));
}

#[test]
fn explicit_public_exposure_opt_in_allows_wildcard_bind() {
    let config = AppConfig {
        host: "0.0.0.0".to_string(),
        allow_non_loopback_bind: true,
        ..AppConfig::default()
    };

    config.validate_bind_safety().unwrap();
}

#[test]
fn container_loopback_mode_allows_wildcard_container_bind_without_public_opt_in() {
    let config = AppConfig {
        host: "0.0.0.0".to_string(),
        container_loopback_only: true,
        ..AppConfig::default()
    };

    config.validate_bind_safety().unwrap();
}

#[test]
fn loads_existing_json_config_from_copilot_proxy_rs_config_dir() {
    let temp = repo_tempdir("config-contract-");
    let config_file = temp.path().join("config.json");
    fs::write(
        &config_file,
        r#"{
          "backend": "copilot",
          "fallback_backend": "bedrock",
          "port": 9090,
          "copilot_timeout": "600",
          "web_search_model": "gpt-5.6-terra",
          "context_guard_threshold": "0.75",
          "model_overrides": {
            "copilot": {"claude-sonnet-4-6": "claude-sonnet-4.6"},
            "bedrock": {"claude-sonnet-4-6": "us.anthropic.claude-sonnet-4-6"}
          }
        }"#,
    )
    .unwrap();

    let env =
        EnvSource::from_pairs([("COPILOT_PROXY_RS_CONFIG_DIR", temp.path().to_str().unwrap())]);
    let config = AppConfig::load_from_env(&env).unwrap();

    assert_eq!(config.backend, "copilot");
    assert_eq!(config.fallback_backend, "bedrock");
    assert_eq!(config.port, 9090);
    assert_eq!(config.copilot_timeout, 600);
    assert_eq!(config.web_search_model, "gpt-5.6-terra");
    assert_eq!(config.context_guard_threshold, 0.75);
    assert!(config.update_check);
    assert!(config.auto_restart);
    assert!(!config.auto_update);
    assert_eq!(
        config.model_overrides.copilot.get("claude-sonnet-4-6"),
        Some(&"claude-sonnet-4.6".to_string())
    );
    assert_eq!(
        config.model_overrides.bedrock.get("claude-sonnet-4-6"),
        Some(&"us.anthropic.claude-sonnet-4-6".to_string())
    );
}

#[test]
fn loads_default_json_config_from_copilot_proxy_rs_home_dir() {
    let temp = repo_tempdir("config-home-");
    let config_dir = temp.path().join(".config").join("copilot-proxy-rs");
    fs::create_dir_all(&config_dir).unwrap();
    fs::write(config_dir.join("config.json"), r#"{"port": 9191}"#).unwrap();

    let env = EnvSource::from_pairs([("HOME", temp.path().to_str().unwrap())]);
    let config = AppConfig::load_from_env(&env).unwrap();

    assert_eq!(config.port, 9191);
}

#[test]
fn environment_values_override_file_values() {
    let temp = repo_tempdir("config-contract-");
    fs::write(
        temp.path().join("config.json"),
        r#"{"backend":"copilot","fallback_backend":"","port":8080,"copilot_timeout":600}"#,
    )
    .unwrap();

    let env = EnvSource::from_pairs([
        ("COPILOT_PROXY_RS_CONFIG_DIR", temp.path().to_str().unwrap()),
        ("COPILOT_PROXY_RS_BACKEND", "copilot,bedrock"),
        ("COPILOT_PROXY_RS_PORT", "9444"),
        ("COPILOT_TIMEOUT", "900"),
        ("COPILOT_PROXY_RS_WEB_SEARCH_MODEL", "gpt-5.6-luna"),
    ]);
    let config = AppConfig::load_from_env(&env).unwrap();

    assert_eq!(config.backend, "copilot");
    assert_eq!(config.fallback_backend, "bedrock");
    assert_eq!(config.port, 9444);
    assert_eq!(config.copilot_timeout, 900);
    assert_eq!(config.web_search_model, "gpt-5.6-luna");
}

#[test]
fn invalid_file_values_fall_back_to_defaults() {
    let temp = repo_tempdir("config-contract-");
    fs::write(
        temp.path().join("config.json"),
        r#"{"port":"not-a-port","copilot_timeout":"not-a-number","model_overrides":{"copilot":{},"bedrock":{}}}"#,
    )
    .unwrap();

    let env =
        EnvSource::from_pairs([("COPILOT_PROXY_RS_CONFIG_DIR", temp.path().to_str().unwrap())]);
    let config = AppConfig::load_from_env(&env).unwrap();

    assert_eq!(config.port, 8080);
    assert_eq!(config.copilot_timeout, 300);
}

#[test]
fn invalid_bool_file_values_fall_back_to_true_defaults() {
    let temp = repo_tempdir("config-contract-");
    fs::write(
        temp.path().join("config.json"),
        r#"{"update_check": "garbage", "auto_restart": "garbage", "auto_update": "garbage"}"#,
    )
    .unwrap();

    let env =
        EnvSource::from_pairs([("COPILOT_PROXY_RS_CONFIG_DIR", temp.path().to_str().unwrap())]);
    let config = AppConfig::load_from_env(&env).unwrap();

    // True-default bools must survive an invalid file value
    assert!(
        config.update_check,
        "invalid update_check should fall back to default true"
    );
    assert!(
        config.auto_restart,
        "invalid auto_restart should fall back to default true"
    );
    // False-default bool should also not be clobbered by an invalid value
    assert!(
        !config.auto_update,
        "invalid auto_update should fall back to default false"
    );
}

/// Explicit `false` values in config.json must override `true` defaults for
/// `update_check` and `auto_restart`.  This is the contract that the sanitize
/// layer and unconditional merge preserve: valid bool values — including `false`
/// — are always honoured.
#[test]
fn explicit_false_overrides_true_defaults() {
    let temp = repo_tempdir("config-contract-");
    fs::write(
        temp.path().join("config.json"),
        r#"{"update_check": false, "auto_restart": false}"#,
    )
    .unwrap();

    let env =
        EnvSource::from_pairs([("COPILOT_PROXY_RS_CONFIG_DIR", temp.path().to_str().unwrap())]);
    let config = AppConfig::load_from_env(&env).unwrap();

    assert!(
        !config.update_check,
        "explicit false in config.json must override true default for update_check"
    );
    assert!(
        !config.auto_restart,
        "explicit false in config.json must override true default for auto_restart"
    );
    // auto_update default is already false; confirm it stays false
    assert!(
        !config.auto_update,
        "auto_update explicit false should remain false"
    );
}

/// Direct `serde_json::from_str::<AppConfig>(...)` must reject non-bool/non-string
/// values (numeric, null, array) for bool fields rather than silently coercing to false.
#[test]
fn direct_deserialization_rejects_invalid_bool_shapes() {
    // numeric 1 for update_check must be a hard error
    let result = serde_json::from_str::<AppConfig>(r#"{"update_check": 1}"#);
    assert!(
        result.is_err(),
        "numeric 1 for update_check must fail deserialization, got: {:?}",
        result.ok()
    );

    // null for update_check must be a hard error
    let result = serde_json::from_str::<AppConfig>(r#"{"update_check": null}"#);
    assert!(
        result.is_err(),
        "null for update_check must fail deserialization, got: {:?}",
        result.ok()
    );

    // array for update_check must be a hard error
    let result = serde_json::from_str::<AppConfig>(r#"{"update_check": []}"#);
    assert!(
        result.is_err(),
        "array for update_check must fail deserialization, got: {:?}",
        result.ok()
    );
}

/// Explicit zero numeric values in config.json must survive `load_from_env`.
/// Python's merge is key-presence-based, so a user who sets `copilot_max_rate: 0`
/// to disable rate limiting must get 0, not the non-zero default.
#[test]
fn explicit_zero_numeric_file_values_are_preserved() {
    let temp = repo_tempdir("config-contract-");
    fs::write(
        temp.path().join("config.json"),
        r#"{
          "copilot_max_rate": 0,
          "copilot_retry_max": 0,
          "copilot_retry_base_delay": 0.0,
          "context_guard_threshold": 0.0
        }"#,
    )
    .unwrap();

    let env =
        EnvSource::from_pairs([("COPILOT_PROXY_RS_CONFIG_DIR", temp.path().to_str().unwrap())]);
    let config = AppConfig::load_from_env(&env).unwrap();

    assert_eq!(
        config.copilot_max_rate, 0,
        "explicit zero copilot_max_rate must be preserved, not replaced by default"
    );
    assert_eq!(
        config.copilot_retry_max, 0,
        "explicit zero copilot_retry_max must be preserved, not replaced by default"
    );
    assert_eq!(
        config.copilot_retry_base_delay, 0.0,
        "explicit zero copilot_retry_base_delay must be preserved, not replaced by default"
    );
    assert_eq!(
        config.context_guard_threshold, 0.0,
        "explicit zero context_guard_threshold must be preserved, not replaced by default"
    );
}

#[test]
fn env_source_can_be_constructed_for_tests() {
    let source = EnvSource::from_pairs([("A", "1"), ("B", "two")]);
    let all: BTreeMap<String, String> = source.into_inner();

    assert_eq!(all.get("A"), Some(&"1".to_string()));
    assert_eq!(all.get("B"), Some(&"two".to_string()));
}

#[test]
fn default_inbound_auth_config_is_disabled() {
    let temp = repo_tempdir("config-default-auth-");
    let env = EnvSource::from_pairs([
        ("HOME", temp.path().to_str().unwrap()),
        ("COPILOT_PROXY_RS_CONFIG_DIR", temp.path().to_str().unwrap()),
    ]);

    let config = AppConfig::load_from_env(&env).unwrap();

    assert_eq!(config.api_key, "");
    assert!(config.allowed_origins.is_empty());
    assert_eq!(config.max_decoded_body_bytes, 16 * 1024 * 1024);
}

#[test]
fn env_overrides_inbound_auth_config() {
    let temp = repo_tempdir("config-env-auth-");
    let env = EnvSource::from_pairs([
        ("HOME", temp.path().to_str().unwrap()),
        ("COPILOT_PROXY_RS_CONFIG_DIR", temp.path().to_str().unwrap()),
        ("COPILOT_PROXY_RS_API_KEY", "local-secret"),
        (
            "COPILOT_PROXY_RS_ALLOWED_ORIGINS",
            "http://localhost:3000,https://example.test",
        ),
        ("COPILOT_PROXY_RS_MAX_DECODED_BODY_BYTES", "4096"),
    ]);

    let config = AppConfig::load_from_env(&env).unwrap();

    assert_eq!(config.api_key, "local-secret");
    assert_eq!(
        config.allowed_origins,
        vec![
            "http://localhost:3000".to_string(),
            "https://example.test".to_string(),
        ]
    );
    assert_eq!(config.max_decoded_body_bytes, 4096);
}
