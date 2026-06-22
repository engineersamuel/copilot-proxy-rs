use thiserror::Error;

#[derive(Debug, Error)]
pub enum AuthError {
    #[error("GitHub token is required; set GITHUB_TOKEN or complete Copilot OAuth device flow")]
    MissingToken,
    #[error("GitHub OAuth device flow is not available in this non-interactive process")]
    NonInteractiveDeviceFlow,
    #[error("GitHub OAuth device flow expired")]
    DeviceFlowExpired,
    #[error("GitHub OAuth device flow denied by the user")]
    DeviceFlowDenied,
    #[error("GitHub OAuth error: {0}")]
    OAuth(String),
    #[error("Copilot token request failed: {0}")]
    CopilotToken(String),
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("HTTP error: {0}")]
    Http(#[from] reqwest::Error),
}

#[derive(Debug, Error, Clone)]
#[error("Copilot HTTP {status_code}: {detail}")]
pub struct CopilotHttpError {
    pub status_code: u16,
    pub detail: String,
}

#[derive(Debug, Error, Clone)]
#[error("{backend} transient error {status_code}: {message}")]
pub struct TransientBackendError {
    pub status_code: u16,
    pub error_type: String,
    pub message: String,
    pub backend: String,
}

#[derive(Debug, Error)]
pub enum CopilotError {
    #[error(transparent)]
    Auth(#[from] AuthError),
    #[error(transparent)]
    Http(#[from] CopilotHttpError),
    #[error(transparent)]
    Transient(#[from] TransientBackendError),
    #[error("invalid request: {0}")]
    InvalidRequest(String),
    #[error("transport error: {0}")]
    Transport(String),
}
