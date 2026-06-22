use std::sync::Arc;

use tokio::sync::RwLock;

use crate::auth::CopilotAuth;
use crate::config::AppConfig;
use crate::copilot::client::CopilotBackend;
use crate::models::ModelRegistry;
use crate::responses::state::ResponsesStateStore;

#[derive(Debug, Clone)]
pub struct AppState {
    pub config: Arc<AppConfig>,
    pub backend: Arc<BackendState>,
    pub models: Arc<ModelRegistry>,
    pub copilot: Arc<CopilotBackend>,
    pub responses: Arc<ResponsesStateStore>,
}

impl AppState {
    pub fn new(config: AppConfig) -> Self {
        let config = Arc::new(config);
        let primary = if let Some(kind) = BackendKind::parse(&config.backend) {
            kind
        } else {
            if !config.backend.trim().is_empty() {
                tracing::warn!(
                    value = %config.backend,
                    "unrecognized primary backend; falling back to copilot"
                );
            }
            BackendKind::Copilot
        };
        let fallback = if config.fallback_backend.trim().is_empty() {
            None
        } else if let Some(kind) = BackendKind::parse(&config.fallback_backend) {
            Some(kind)
        } else {
            tracing::warn!(
                value = %config.fallback_backend,
                "unrecognized fallback_backend; ignoring"
            );
            None
        };
        let models = Arc::new(ModelRegistry::new());
        let auth = Arc::new(CopilotAuth::new(config.clone()));
        let copilot = Arc::new(CopilotBackend::new(config.clone(), auth, models.clone()));
        Self {
            backend: Arc::new(BackendState::new(primary, fallback)),
            config,
            models,
            copilot,
            responses: Arc::new(ResponsesStateStore::default()),
        }
    }

    pub fn with_parts_for_tests(
        config: Arc<AppConfig>,
        models: Arc<ModelRegistry>,
        copilot: Arc<CopilotBackend>,
    ) -> Self {
        let primary = BackendKind::parse(&config.backend).unwrap_or(BackendKind::Copilot);
        let fallback = BackendKind::parse_optional(&config.fallback_backend);
        Self {
            backend: Arc::new(BackendState::new(primary, fallback)),
            config,
            models,
            copilot,
            responses: Arc::new(ResponsesStateStore::default()),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BackendKind {
    Copilot,
    Bedrock,
}

impl BackendKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Copilot => "copilot",
            Self::Bedrock => "bedrock",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "copilot" => Some(Self::Copilot),
            "bedrock" => Some(Self::Bedrock),
            _ => None,
        }
    }

    pub fn parse_optional(value: &str) -> Option<Self> {
        let value = value.trim();
        if value.is_empty() {
            None
        } else {
            Self::parse(value)
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BackendSnapshot {
    pub primary: BackendKind,
    pub fallback: Option<BackendKind>,
}

#[derive(Debug)]
pub struct BackendState {
    inner: RwLock<BackendSnapshot>,
}

impl BackendState {
    pub fn new(primary: BackendKind, fallback: Option<BackendKind>) -> Self {
        Self {
            inner: RwLock::new(BackendSnapshot { primary, fallback }),
        }
    }

    pub async fn snapshot(&self) -> BackendSnapshot {
        *self.inner.read().await
    }

    pub async fn set(&self, primary: BackendKind, fallback: Option<BackendKind>) {
        *self.inner.write().await = BackendSnapshot { primary, fallback };
    }
}
