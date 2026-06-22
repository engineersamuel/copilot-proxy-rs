mod support;

use std::fs;
use std::sync::Arc;

use copilot_proxy_rs::auth::{AuthEndpoints, CopilotAuth, TokenSource};
use copilot_proxy_rs::config::{AppConfig, EnvSource};

fn repo_tempdir(prefix: &str) -> tempfile::TempDir {
    tempfile::Builder::new()
        .prefix(prefix)
        .tempdir_in(env!("CARGO_MANIFEST_DIR"))
        .unwrap()
}

#[tokio::test]
async fn github_token_prefers_env_over_persisted_file() {
    let temp = repo_tempdir("auth-contract-");
    fs::write(temp.path().join("github_token"), "file-token\n").unwrap();
    let env = EnvSource::from_pairs([
        ("COPILOT_PROXY_RS_CONFIG_DIR", temp.path().to_str().unwrap()),
        ("GITHUB_TOKEN", "env-token"),
    ]);
    let config = Arc::new(AppConfig::load_from_env(&env).unwrap());
    let auth =
        CopilotAuth::with_env_for_tests(config, env, AuthEndpoints::localhost_for_tests(), false);

    assert_eq!(auth.github_token().await.unwrap(), "env-token");
    assert_eq!(auth.token_source().await, TokenSource::Env);
}

#[tokio::test]
async fn github_token_loads_persisted_file_when_env_missing() {
    let temp = repo_tempdir("auth-contract-");
    fs::write(temp.path().join("github_token"), "file-token\n").unwrap();
    let env =
        EnvSource::from_pairs([("COPILOT_PROXY_RS_CONFIG_DIR", temp.path().to_str().unwrap())]);
    let config = Arc::new(AppConfig::load_from_env(&env).unwrap());
    let auth =
        CopilotAuth::with_env_for_tests(config, env, AuthEndpoints::localhost_for_tests(), false);

    assert_eq!(auth.github_token().await.unwrap(), "file-token");
    assert_eq!(auth.token_source().await, TokenSource::File);
}

#[tokio::test]
async fn github_token_loads_persisted_file_from_default_copilot_proxy_rs_home_dir() {
    let temp = repo_tempdir("auth-home-");
    let config_dir = temp.path().join(".config").join("copilot-proxy-rs");
    fs::create_dir_all(&config_dir).unwrap();
    fs::write(config_dir.join("github_token"), "home-token\n").unwrap();

    let env = EnvSource::from_pairs([("HOME", temp.path().to_str().unwrap())]);
    let config = Arc::new(AppConfig::load_from_env(&env).unwrap());
    let auth =
        CopilotAuth::with_env_for_tests(config, env, AuthEndpoints::localhost_for_tests(), false);

    assert_eq!(auth.github_token().await.unwrap(), "home-token");
    assert_eq!(auth.token_source().await, TokenSource::File);
}

#[tokio::test]
async fn missing_token_in_non_interactive_mode_returns_actionable_error() {
    let temp = repo_tempdir("auth-contract-");
    let env =
        EnvSource::from_pairs([("COPILOT_PROXY_RS_CONFIG_DIR", temp.path().to_str().unwrap())]);
    let config = Arc::new(AppConfig::load_from_env(&env).unwrap());
    let auth =
        CopilotAuth::with_env_for_tests(config, env, AuthEndpoints::localhost_for_tests(), false);

    let err = auth.github_token().await.unwrap_err().to_string();
    assert!(
        err.contains("device flow"),
        "expected 'device flow' in error: {err}"
    );
    assert!(
        err.contains("non-interactive"),
        "expected 'non-interactive' in error: {err}"
    );
}

#[tokio::test]
async fn copilot_token_is_exchanged_and_cached() {
    let mock = support::MockServer::start().await;
    mock.respond_json(
        "GET",
        "/copilot/token",
        200,
        serde_json::json!({
            "token": "copilot-token",
            "expires_at": 4_102_444_800u64
        }),
    )
    .await;

    let temp = repo_tempdir("auth-contract-");
    fs::write(temp.path().join("github_token"), "github-token\n").unwrap();
    let env =
        EnvSource::from_pairs([("COPILOT_PROXY_RS_CONFIG_DIR", temp.path().to_str().unwrap())]);
    let config = Arc::new(AppConfig::load_from_env(&env).unwrap());
    let auth = CopilotAuth::with_env_for_tests(config, env, mock.auth_endpoints(), false);

    assert_eq!(auth.copilot_token().await.unwrap(), "copilot-token");
    assert_eq!(auth.copilot_token().await.unwrap(), "copilot-token");
    assert_eq!(mock.hits("GET", "/copilot/token").await, 1);
}

#[tokio::test]
async fn copilot_token_refresh_sanitizes_raw_upstream_error_body() {
    let mock = support::MockServer::start().await;
    mock.respond_json(
        "GET",
        "/copilot/token",
        500,
        serde_json::json!({
            "error": "secret token endpoint diagnostic"
        }),
    )
    .await;

    let temp = repo_tempdir("auth-contract-");
    fs::write(temp.path().join("github_token"), "github-token\n").unwrap();
    let env =
        EnvSource::from_pairs([("COPILOT_PROXY_RS_CONFIG_DIR", temp.path().to_str().unwrap())]);
    let config = Arc::new(AppConfig::load_from_env(&env).unwrap());
    let auth = CopilotAuth::with_env_for_tests(config, env, mock.auth_endpoints(), false);

    let err = auth.copilot_token().await.unwrap_err().to_string();

    assert!(err.contains("HTTP 500"));
    assert!(!err.contains("secret token endpoint diagnostic"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn copilot_token_concurrent_calls_hit_exchange_endpoint_exactly_once() {
    // The mock introduces a 30 ms delay so that all 8 concurrent tasks have time
    // to pass the "empty cache" check before any one of them writes the result.
    // Without the single-flight mutex the endpoint would be hit 8 times.
    let mock = support::MockServer::start().await;
    mock.respond_json_delayed(
        "GET",
        "/copilot/token",
        30,
        200,
        serde_json::json!({
            "token": "copilot-token",
            "expires_at": 4_102_444_800u64
        }),
    )
    .await;

    let temp = repo_tempdir("auth-single-flight-");
    fs::write(temp.path().join("github_token"), "github-token\n").unwrap();
    let env =
        EnvSource::from_pairs([("COPILOT_PROXY_RS_CONFIG_DIR", temp.path().to_str().unwrap())]);
    let config = Arc::new(AppConfig::load_from_env(&env).unwrap());
    let auth = Arc::new(CopilotAuth::with_env_for_tests(
        config,
        env,
        mock.auth_endpoints(),
        false,
    ));

    let barrier = Arc::new(tokio::sync::Barrier::new(8));
    let mut handles = vec![];
    for _ in 0..8 {
        let auth = auth.clone();
        let b = barrier.clone();
        handles.push(tokio::spawn(async move {
            b.wait().await;
            auth.copilot_token().await
        }));
    }
    for h in handles {
        assert_eq!(h.await.unwrap().unwrap(), "copilot-token");
    }

    assert_eq!(
        mock.hits("GET", "/copilot/token").await,
        1,
        "single-flight lock should ensure the exchange endpoint is called exactly once"
    );
}
