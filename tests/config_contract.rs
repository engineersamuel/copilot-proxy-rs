use std::fs;

use copilot_proxy_rs::config::{AppConfig, ConfigError, EnvSource};

fn repo_tempdir(prefix: &str) -> tempfile::TempDir {
    tempfile::Builder::new()
        .prefix(prefix)
        .tempdir_in(env!("CARGO_MANIFEST_DIR"))
        .unwrap()
}

#[test]
fn local_models_load_from_json() {
    let temp = repo_tempdir("config-local-model-");
    fs::write(
        temp.path().join("config.json"),
        r#"{
          "local_models": {
            "qwen3-coder-30b-local": {
              "base_url": "http://100.98.223.125:8080/v1",
              "upstream_model": "models\\Qwen3-Coder-30B-A3B-Instruct-IQ4_XS.gguf"
            }
          }
        }"#,
    )
    .unwrap();
    let env =
        EnvSource::from_pairs([("COPILOT_PROXY_RS_CONFIG_DIR", temp.path().to_str().unwrap())]);

    let config = AppConfig::load_from_env(&env).unwrap();
    let local = config.local_models.get("qwen3-coder-30b-local").unwrap();

    assert_eq!(local.base_url, "http://100.98.223.125:8080/v1");
    assert_eq!(
        local.upstream_model,
        r"models\Qwen3-Coder-30B-A3B-Instruct-IQ4_XS.gguf"
    );
}

#[test]
fn invalid_local_model_names_the_id_and_field() {
    let temp = repo_tempdir("config-invalid-local-model-");
    fs::write(
        temp.path().join("config.json"),
        r#"{"local_models":{"qwen-local":{"base_url":"ftp://host/v1","upstream_model":"model.gguf"}}}"#,
    )
    .unwrap();
    let env =
        EnvSource::from_pairs([("COPILOT_PROXY_RS_CONFIG_DIR", temp.path().to_str().unwrap())]);

    let error = AppConfig::load_from_env(&env).unwrap_err().to_string();

    assert!(error.contains("qwen-local"));
    assert!(error.contains("base_url"));
    assert!(error.contains("http or https"));
}

#[test]
fn local_model_base_url_rejects_surrounding_whitespace() {
    let temp = repo_tempdir("config-spaced-local-base-url-");
    fs::write(
        temp.path().join("config.json"),
        r#"{"local_models":{"qwen-spaced":{"base_url":" http://example.com/v1 ","upstream_model":"model.gguf"}}}"#,
    )
    .unwrap();
    let env =
        EnvSource::from_pairs([("COPILOT_PROXY_RS_CONFIG_DIR", temp.path().to_str().unwrap())]);

    let error = AppConfig::load_from_env(&env).unwrap_err();

    match error {
        ConfigError::InvalidLocalModel {
            model_id,
            field,
            message,
        } => assert_eq!(
            (model_id.as_str(), field, message.as_str()),
            (
                "qwen-spaced",
                "base_url",
                "must contain no surrounding whitespace"
            )
        ),
        other => panic!("expected InvalidLocalModel, got {other}"),
    }
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
        ("COPILOT_CONNECT_TIMEOUT", "120"),
        ("COPILOT_PROXY_RS_WEB_SEARCH_MODEL", "gpt-5.6-luna"),
    ]);
    let config = AppConfig::load_from_env(&env).unwrap();

    assert_eq!(config.backend, "copilot");
    assert_eq!(config.fallback_backend, "bedrock");
    assert_eq!(config.port, 9444);
    assert_eq!(config.copilot_timeout, 900);
    assert_eq!(config.copilot_connect_timeout, 120);
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
    assert_eq!(config.copilot_connect_timeout, 60);
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
        ("COPILOT_PROXY_RS_LOG_FAILED_REQUEST_BODIES", "false"),
        (
            "COPILOT_PROXY_RS_STRIP_REJECTED_AGENT_MESSAGE_CIPHERTEXT",
            "false",
        ),
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
    assert!(!config.log_failed_request_bodies);
    assert!(!config.strip_rejected_agent_message_ciphertext);
}

#[test]
fn aa_key_prefers_env_then_env_local_file() {
    let temp = tempfile::tempdir().unwrap();
    let dir = temp.path().to_str().unwrap();
    std::fs::write(
        temp.path().join(".env.local"),
        "OTHER=x\nAA_API_KEY=\"from-file\"\n",
    )
    .unwrap();
    let from_file = AppConfig::load_from_env(&EnvSource::from_pairs([(
        "COPILOT_PROXY_RS_CONFIG_DIR",
        dir,
    )]))
    .unwrap();
    assert_eq!(from_file.aa_api_key, "from-file");
    let from_env = AppConfig::load_from_env(&EnvSource::from_pairs([
        ("COPILOT_PROXY_RS_CONFIG_DIR", dir),
        ("AA_API_KEY", "from-env"),
    ]))
    .unwrap();
    assert_eq!(from_env.aa_api_key, "from-env");
}

#[test]
fn aa_key_file_strips_inline_comments() {
    for (line, want) in [
        ("AA_API_KEY=\"abc\" # c", "abc"),
        ("AA_API_KEY='abc' # c", "abc"),
        ("AA_API_KEY=abc # c", "abc"),
        ("export AA_API_KEY=abc", "abc"),
        ("AA_API_KEY=abc\t# c", "abc"),
        ("AA_API_KEY=a#bc", "a#bc"),
    ] {
        let temp = tempfile::tempdir().unwrap();
        std::fs::write(temp.path().join(".env.local"), format!("{line}\n")).unwrap();
        let env =
            EnvSource::from_pairs([("COPILOT_PROXY_RS_CONFIG_DIR", temp.path().to_str().unwrap())]);
        assert_eq!(
            AppConfig::load_from_env(&env).unwrap().aa_api_key,
            want,
            "{line}"
        );
    }
}

#[test]
fn config_debug_never_prints_aa_key() {
    let env = EnvSource::from_pairs([
        ("COPILOT_PROXY_RS_CONFIG_DIR", "/nonexistent-dir-for-test"),
        ("AA_API_KEY", "super-secret-aa"),
    ]);
    let config = AppConfig::load_from_env(&env).unwrap();
    assert_eq!(config.aa_api_key, "super-secret-aa");
    assert!(!format!("{config:?}").contains("super-secret-aa"));
}

#[test]
fn model_excludes_parse_and_env_adds() {
    use copilot_proxy_rs::models::{model_exclude_set, parse_model_excludes};
    assert_eq!(
        parse_model_excludes("# c\n\n  a-1  \nb-2 # tail\n   \n"),
        vec!["a-1", "b-2"]
    );
    let env = EnvSource::from_pairs([
        ("COPILOT_PROXY_RS_CONFIG_DIR", "/nonexistent-dir-for-test"),
        ("COPILOT_PROXY_RS_MODEL_EXCLUDES", " x-1, ,y-2 "),
    ]);
    let config = AppConfig::load_from_env(&env).unwrap();
    assert_eq!(config.model_excludes, vec!["x-1", "y-2"]);
    let set = model_exclude_set(&config.model_excludes);
    assert!(set.contains("gpt-3.5-turbo") && set.contains("x-1"));
}

#[test]
fn model_excludes_support_trailing_wildcard_without_accidental_matches() {
    use copilot_proxy_rs::models::model_exclude_set;
    let set = model_exclude_set(&["foo-*".to_string(), "bar".to_string()]);
    for id in [
        "gpt-4o",
        "gpt-4.1",
        "gpt-4",
        "gpt-3.5-turbo",
        "foo-x",
        "bar",
    ] {
        assert!(set.contains(id), "{id}");
    }
    // Exact entries stay exact.
    for id in ["gpt-5.5-xyz", "gpt-5.7", "bar-2", "foo", "gpt-5"] {
        assert!(!set.contains(id), "{id}");
    }
    assert!(set.contains("gpt-5.5"));
}

#[test]
fn real_excludes_file_keeps_exactly_the_expected_models() {
    use copilot_proxy_rs::models::model_exclude_set;
    const KEPT: &[&str] = &[
        "claude-haiku-5.5",
        "claude-opus-5.5",
        "claude-sonnet-5.5",
        "gemini-3.8-flash",
        "gemini-4-argon",
        "gpt-5.6-sol-fast",
        "gpt-6-astra",
        "gpt-6-luna",
        "gpt-6.1-sol",
        "grok-4.7",
        "mai-code-1.1-flash",
    ];
    const ALL: &str = "claude-haiku-4.5 claude-haiku-5.5 claude-opus-5.5 claude-sonnet-5 claude-sonnet-5.5 gemini-3.7-flash gemini-3.8-flash gemini-4-argon gpt-3.5-turbo gpt-3.5-turbo-0613 gpt-4 gpt-4-0125-preview gpt-4-0613 gpt-4-o-preview gpt-4.1 gpt-4.1-2025-04-14 gpt-41-copilot gpt-4o gpt-4o-2024-05-13 gpt-4o-2024-08-06 gpt-4o-2024-11-20 gpt-4o-mini gpt-4o-mini-2024-07-18 gpt-5-mini gpt-5.3-codex gpt-5.4 gpt-5.4-mini gpt-5.4-nano gpt-5.5 gpt-5.6-luna gpt-5.6-sol gpt-5.6-sol-fast gpt-5.6-terra gpt-6-astra gpt-6-luna gpt-6-sol gpt-6.1-sol grok-4.5 grok-4.6 grok-4.7 mai-code-1.1-flash text-embedding-3-small text-embedding-3-small-inference text-embedding-ada-002 trajectory-compaction";
    let set = model_exclude_set(&[]);
    let visible: Vec<&str> = ALL.split(' ').filter(|id| !set.contains(id)).collect();
    assert_eq!(visible, KEPT);
}

#[test]
fn typesafe_key_and_jev_model_config() {
    let temp = tempfile::tempdir().unwrap();
    let dir = temp.path().to_str().unwrap();
    std::fs::write(
        temp.path().join(".env.local"),
        "AA_API_KEY=aa\nTYPESAFE_API_KEY=\"ts-file\" # c\n",
    )
    .unwrap();
    let c = AppConfig::load_from_env(&EnvSource::from_pairs([(
        "COPILOT_PROXY_RS_CONFIG_DIR",
        dir,
    )]))
    .unwrap();
    assert_eq!(c.typesafe_api_key, "ts-file");
    assert_eq!(c.aa_api_key, "aa");
    assert_eq!(c.jev_model, "jev-preview");
    let c = AppConfig::load_from_env(&EnvSource::from_pairs([
        ("COPILOT_PROXY_RS_CONFIG_DIR", dir),
        ("TYPESAFE_API_KEY", "ts-env"),
        ("COPILOT_PROXY_RS_JEV_MODEL", "jev-x"),
    ]))
    .unwrap();
    assert_eq!(c.typesafe_api_key, "ts-env");
    assert_eq!(c.jev_model, "jev-x");
    assert!(!format!("{c:?}").contains("ts-env"));
}

#[test]
fn typesafe_key_comment_only_is_missing_and_star_exclude_is_flagged() {
    let temp = tempfile::tempdir().unwrap();
    let dir = temp.path().to_str().unwrap();
    std::fs::write(
        temp.path().join(".env.local"),
        "TYPESAFE_API_KEY=# nothing\n",
    )
    .unwrap();
    let c = AppConfig::load_from_env(&EnvSource::from_pairs([(
        "COPILOT_PROXY_RS_CONFIG_DIR",
        dir,
    )]))
    .unwrap();
    assert_eq!(c.typesafe_api_key, "");
    use copilot_proxy_rs::models::{invalid_model_excludes, model_exclude_set};
    let extra = vec!["*".to_string(), "x-1".to_string()];
    assert_eq!(invalid_model_excludes(&extra), vec!["*"]);
    let set = model_exclude_set(&extra);
    assert!(!set.contains("claude-opus-5.5") && set.contains("x-1"));
}
