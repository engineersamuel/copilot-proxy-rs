use std::time::Duration;

use reqwest::{Client, Response};
use serde_json::{Map, Value};

use crate::models::LocalModelTarget;

const MAX_ERROR_CHARS: usize = 4096;
const REQUEST_TIMEOUT: Duration = Duration::from_secs(300);

#[derive(Debug, thiserror::Error)]
pub enum LocalModelError {
    #[error("local model request timed out")]
    Timeout,
    #[error("local model connection failed")]
    Transport,
    #[error("local model returned HTTP {status}: {detail}")]
    Http { status: u16, detail: String },
    #[error("local model returned invalid JSON")]
    InvalidJson,
}

#[derive(Debug, Clone)]
pub struct LocalBackend {
    client: Client,
}

impl LocalBackend {
    pub fn new() -> Self {
        Self {
            client: Client::new(),
        }
    }

    pub async fn post_chat(
        &self,
        target: &LocalModelTarget,
        body: Map<String, Value>,
    ) -> Result<Value, LocalModelError> {
        self.send(target, body)
            .await?
            .json()
            .await
            .map_err(|_| LocalModelError::InvalidJson)
    }

    pub async fn stream_chat(
        &self,
        target: &LocalModelTarget,
        body: Map<String, Value>,
    ) -> Result<Response, LocalModelError> {
        self.send(target, body).await
    }

    async fn send(
        &self,
        target: &LocalModelTarget,
        mut body: Map<String, Value>,
    ) -> Result<Response, LocalModelError> {
        body.insert(
            "model".to_string(),
            Value::String(target.upstream_model.clone()),
        );
        let response = self
            .client
            .post(target.chat_completions_url())
            .json(&body)
            .timeout(REQUEST_TIMEOUT)
            .send()
            .await
            .map_err(map_transport_error)?;

        checked_response(response).await
    }
}

impl Default for LocalBackend {
    fn default() -> Self {
        Self::new()
    }
}

fn map_transport_error(error: reqwest::Error) -> LocalModelError {
    if error.is_timeout() {
        LocalModelError::Timeout
    } else {
        LocalModelError::Transport
    }
}

async fn checked_response(response: Response) -> Result<Response, LocalModelError> {
    let status = response.status();
    if status.is_success() {
        return Ok(response);
    }

    let detail = response.text().await.unwrap_or_default();
    let detail = if detail.is_empty() {
        status
            .canonical_reason()
            .unwrap_or("upstream error")
            .to_string()
    } else {
        detail.chars().take(MAX_ERROR_CHARS).collect()
    };
    Err(LocalModelError::Http {
        status: status.as_u16(),
        detail,
    })
}
