use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use reqwest::Client;
use serde::{Deserialize, Serialize};
use tokio::sync::RwLock;

use crate::config::{AppConfig, EnvSource};
use crate::copilot::errors::AuthError;

pub const COPILOT_CLIENT_ID: &str = "Iv1.b507a08c87ecfe98";
pub const COPILOT_SCOPE: &str = "copilot";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TokenSource {
    Env,
    File,
    DeviceFlow,
    None,
}

#[derive(Debug, Clone)]
pub struct AuthEndpoints {
    pub device_code_url: String,
    pub oauth_token_url: String,
    pub copilot_token_url: String,
}

impl Default for AuthEndpoints {
    fn default() -> Self {
        Self {
            device_code_url: "https://github.com/login/device/code".to_string(),
            oauth_token_url: "https://github.com/login/oauth/access_token".to_string(),
            copilot_token_url: "https://api.github.com/copilot_internal/v2/token".to_string(),
        }
    }
}

impl AuthEndpoints {
    pub fn localhost_for_tests() -> Self {
        Self {
            device_code_url: "http://127.0.0.1/device/code".to_string(),
            oauth_token_url: "http://127.0.0.1/oauth/token".to_string(),
            copilot_token_url: "http://127.0.0.1/copilot/token".to_string(),
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct DeviceCodeResponse {
    pub device_code: String,
    pub user_code: String,
    pub verification_uri: String,
    #[serde(default = "default_poll_interval")]
    pub interval: u64,
    #[serde(default = "default_device_expires_in")]
    pub expires_in: u64,
}

fn default_poll_interval() -> u64 {
    5
}
fn default_device_expires_in() -> u64 {
    900
}

#[derive(Debug, Clone)]
struct CachedCopilotToken {
    token: String,
    expires_at: u64,
}

#[derive(Debug)]
pub struct CopilotAuth {
    #[allow(dead_code)]
    config: Arc<AppConfig>,
    env: EnvSource,
    endpoints: AuthEndpoints,
    client: Client,
    interactive: bool,
    github_token: RwLock<Option<(String, TokenSource)>>,
    copilot_token: RwLock<Option<CachedCopilotToken>>,
    /// Guards the token-exchange HTTP call so that concurrent callers perform at
    /// most one refresh (double-check pattern: re-read the cache after acquiring).
    copilot_token_refresh_lock: tokio::sync::Mutex<()>,
}

impl CopilotAuth {
    pub fn new(config: Arc<AppConfig>) -> Self {
        use std::io::IsTerminal;
        Self::with_env_for_tests(
            config,
            EnvSource::current(),
            AuthEndpoints::default(),
            std::io::stdin().is_terminal(),
        )
    }

    pub fn with_env_for_tests(
        config: Arc<AppConfig>,
        env: EnvSource,
        endpoints: AuthEndpoints,
        interactive: bool,
    ) -> Self {
        Self {
            config,
            env,
            endpoints,
            client: Client::new(),
            interactive,
            github_token: RwLock::new(None),
            copilot_token: RwLock::new(None),
            copilot_token_refresh_lock: tokio::sync::Mutex::new(()),
        }
    }

    pub async fn token_source(&self) -> TokenSource {
        self.github_token
            .read()
            .await
            .as_ref()
            .map(|(_, source)| source.clone())
            .unwrap_or(TokenSource::None)
    }

    pub async fn github_token(&self) -> Result<String, AuthError> {
        if let Some((token, _)) = self.github_token.read().await.as_ref() {
            return Ok(token.clone());
        }
        let (token, source) = self.load_or_request_github_token().await?;
        *self.github_token.write().await = Some((token.clone(), source));
        Ok(token)
    }

    async fn load_or_request_github_token(&self) -> Result<(String, TokenSource), AuthError> {
        if let Some(token) = self
            .env
            .get("GITHUB_TOKEN")
            .map(str::trim)
            .filter(|value| !value.is_empty())
        {
            return Ok((token.to_string(), TokenSource::Env));
        }
        let path = self.token_file();
        if let Ok(raw) = tokio::fs::read_to_string(&path).await {
            let token = raw.trim();
            if !token.is_empty() {
                return Ok((token.to_string(), TokenSource::File));
            }
        }
        if !self.interactive {
            return Err(AuthError::NonInteractiveDeviceFlow);
        }
        let token = self.run_device_flow().await?;
        self.persist_token(&token).await?;
        Ok((token, TokenSource::DeviceFlow))
    }

    fn token_file(&self) -> PathBuf {
        let home = self
            .env
            .get("HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("."));
        let config_dir = self
            .env
            .get("COPILOT_PROXY_RS_CONFIG_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| home.join(".config").join("copilot-proxy-rs"));
        config_dir.join("github_token")
    }

    async fn persist_token(&self, token: &str) -> Result<(), AuthError> {
        let path = self.token_file();
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        tokio::fs::write(&path, token).await?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            tokio::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).await?;
        }
        Ok(())
    }

    fn now_epoch_seconds() -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or(Duration::ZERO)
            .as_secs()
    }

    pub async fn copilot_token(&self) -> Result<String, AuthError> {
        // Fast path: valid cached token without holding the refresh lock.
        if let Some(cached) = self.copilot_token.read().await.as_ref() {
            if Self::now_epoch_seconds() < cached.expires_at.saturating_sub(120) {
                return Ok(cached.token.clone());
            }
        }
        // Slow path: acquire the single-flight lock and double-check.
        let _guard = self.copilot_token_refresh_lock.lock().await;
        if let Some(cached) = self.copilot_token.read().await.as_ref() {
            if Self::now_epoch_seconds() < cached.expires_at.saturating_sub(120) {
                return Ok(cached.token.clone());
            }
        }
        let github_token = self.github_token().await?;
        let refreshed = self.refresh_copilot_token(&github_token).await;
        match refreshed {
            Ok(token) => Ok(token),
            Err(err) => {
                if let Some(cached) = self.copilot_token.read().await.as_ref() {
                    if Self::now_epoch_seconds() < cached.expires_at {
                        tracing::warn!(
                            error = %err,
                            "Copilot token refresh failed; using cached token"
                        );
                        return Ok(cached.token.clone());
                    }
                }
                Err(err)
            }
        }
    }

    async fn refresh_copilot_token(&self, github_token: &str) -> Result<String, AuthError> {
        let response = self
            .client
            .get(&self.endpoints.copilot_token_url)
            .header("Editor-Version", "vscode/1.100.0")
            .header("Editor-Plugin-Version", "copilot-chat/0.27.2025040201")
            .header("User-Agent", "GithubCopilot/1.155.0")
            .header("Accept", "application/json")
            .header("X-GitHub-Api-Version", "2025-04-01")
            .header("Authorization", format!("token {github_token}"))
            .send()
            .await?;
        if response.status() == reqwest::StatusCode::UNAUTHORIZED {
            *self.github_token.write().await = None;
            return Err(AuthError::CopilotToken(
                "GitHub token is invalid or expired".to_string(),
            ));
        }
        if response.status() == reqwest::StatusCode::FORBIDDEN {
            return Err(AuthError::CopilotToken(
                "Copilot token request denied".to_string(),
            ));
        }
        if !response.status().is_success() {
            return Err(AuthError::CopilotToken(format!(
                "HTTP {}",
                response.status().as_u16()
            )));
        }
        let value: serde_json::Value = response.json().await?;
        let token = value
            .get("token")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| AuthError::CopilotToken("missing token field".to_string()))?
            .to_string();
        let expires_at = value
            .get("expires_at")
            .and_then(serde_json::Value::as_u64)
            .unwrap_or_else(|| Self::now_epoch_seconds() + 1800);
        *self.copilot_token.write().await = Some(CachedCopilotToken {
            token: token.clone(),
            expires_at,
        });
        Ok(token)
    }

    async fn run_device_flow(&self) -> Result<String, AuthError> {
        let device = self.request_device_code().await?;
        eprintln!();
        eprintln!("To authenticate with GitHub Copilot:");
        eprintln!("  1. Open {}", device.verification_uri);
        eprintln!("  2. Enter code: {}", device.user_code);
        eprintln!();
        eprintln!("Waiting for authorization...");
        self.poll_device_flow(device).await
    }

    async fn request_device_code(&self) -> Result<DeviceCodeResponse, AuthError> {
        Ok(self
            .client
            .post(&self.endpoints.device_code_url)
            .form(&[("client_id", COPILOT_CLIENT_ID), ("scope", COPILOT_SCOPE)])
            .header("Editor-Version", "vscode/1.100.0")
            .header("Editor-Plugin-Version", "copilot-chat/0.27.2025040201")
            .header("User-Agent", "GithubCopilot/1.155.0")
            .header("Accept", "application/json")
            .header("X-GitHub-Api-Version", "2025-10-01")
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?)
    }

    async fn poll_device_flow(&self, device: DeviceCodeResponse) -> Result<String, AuthError> {
        let mut interval = device.interval;
        let deadline = tokio::time::Instant::now() + Duration::from_secs(device.expires_in);
        loop {
            if tokio::time::Instant::now() >= deadline {
                return Err(AuthError::DeviceFlowExpired);
            }
            tokio::time::sleep(Duration::from_secs(interval)).await;
            let response = self
                .client
                .post(&self.endpoints.oauth_token_url)
                .form(&[
                    ("client_id", COPILOT_CLIENT_ID),
                    ("device_code", &device.device_code),
                    ("grant_type", "urn:ietf:params:oauth:grant-type:device_code"),
                ])
                .header("Accept", "application/json")
                .send()
                .await?;
            let body: serde_json::Value = response.json().await?;
            if let Some(token) = body.get("access_token").and_then(serde_json::Value::as_str) {
                return Ok(token.to_string());
            }
            match body
                .get("error")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("")
            {
                "authorization_pending" => {}
                "slow_down" => interval += 5,
                "expired_token" => return Err(AuthError::DeviceFlowExpired),
                "access_denied" => return Err(AuthError::DeviceFlowDenied),
                other => return Err(AuthError::OAuth(other.to_string())),
            }
        }
    }

    #[cfg(test)]
    pub fn _config(&self) -> &Arc<AppConfig> {
        &self.config
    }
}
