use std::sync::Arc;
use std::time::Duration;

use reqwest::{Client, StatusCode};
use serde_json::{Map, Value};
use tokio::sync::Mutex;

use crate::auth::CopilotAuth;
use crate::config::AppConfig;
use crate::copilot::errors::{CopilotError, CopilotHttpError, TransientBackendError};
use crate::copilot::request::{
    CopilotRequestMetadata, base_copilot_request_headers, compute_initiator,
};
use crate::models::{EffortLevel, ModelRegistry};

#[derive(Debug, Clone)]
pub struct CopilotEndpoints {
    pub chat_url: String,
    pub messages_url: String,
    pub responses_url: String,
    pub responses_ws_url: String,
    pub models_url: String,
}

impl Default for CopilotEndpoints {
    fn default() -> Self {
        Self {
            chat_url: "https://api.githubcopilot.com/chat/completions".to_string(),
            messages_url: "https://api.githubcopilot.com/v1/messages".to_string(),
            responses_url: "https://api.githubcopilot.com/responses".to_string(),
            responses_ws_url: "wss://api.githubcopilot.com/responses".to_string(),
            models_url: "https://api.githubcopilot.com/models".to_string(),
        }
    }
}

#[derive(Debug)]
pub struct TokenBucket {
    rate: u32,
    inner: Mutex<TokenBucketInner>,
}

#[derive(Debug)]
struct TokenBucketInner {
    tokens: f64,
    last_refill: std::time::Instant,
}

impl TokenBucket {
    pub fn new(rate_per_minute: u32) -> Self {
        Self {
            rate: rate_per_minute.max(1),
            inner: Mutex::new(TokenBucketInner {
                tokens: rate_per_minute.max(1) as f64,
                last_refill: std::time::Instant::now(),
            }),
        }
    }

    pub async fn acquire(&self) -> f64 {
        let interval = 60.0 / self.rate as f64;
        let mut waited = 0.0;
        loop {
            let sleep_for = {
                let mut inner = self.inner.lock().await;
                let elapsed = inner.last_refill.elapsed().as_secs_f64();
                inner.tokens = (self.rate as f64).min(inner.tokens + elapsed / interval);
                inner.last_refill = std::time::Instant::now();
                if inner.tokens >= 1.0 {
                    inner.tokens -= 1.0;
                    return waited;
                }
                interval * (1.0 - inner.tokens)
            };
            tokio::time::sleep(Duration::from_secs_f64(sleep_for)).await;
            waited += sleep_for;
        }
    }
}

#[derive(Debug)]
pub struct CopilotBackend {
    config: Arc<AppConfig>,
    auth: Arc<CopilotAuth>,
    models: Arc<ModelRegistry>,
    pub(crate) endpoints: CopilotEndpoints,
    client: Client,
    rate_limiter: Option<Arc<TokenBucket>>,
}

impl CopilotBackend {
    pub fn new(config: Arc<AppConfig>, auth: Arc<CopilotAuth>, models: Arc<ModelRegistry>) -> Self {
        Self::with_endpoints_for_tests(config, auth, models, CopilotEndpoints::default())
    }

    pub fn with_endpoints_for_tests(
        config: Arc<AppConfig>,
        auth: Arc<CopilotAuth>,
        models: Arc<ModelRegistry>,
        endpoints: CopilotEndpoints,
    ) -> Self {
        let client = Client::builder()
            .timeout(Duration::from_secs(config.copilot_timeout))
            .connect_timeout(Duration::from_secs(30))
            .build()
            .expect("reqwest client should build");
        let rate_limiter = (config.copilot_max_rate > 0)
            .then(|| Arc::new(TokenBucket::new(config.copilot_max_rate)));
        Self {
            config,
            auth,
            models,
            endpoints,
            client,
            rate_limiter,
        }
    }

    pub async fn post_chat(
        &self,
        body: Map<String, Value>,
        metadata: Option<CopilotRequestMetadata>,
    ) -> Result<Value, CopilotError> {
        let result = self
            .post_json(&self.endpoints.chat_url, body.clone(), metadata.clone())
            .await;
        match result {
            Err(CopilotError::Http(ref http_err))
                if http_err.status_code == 400
                    && (http_err.detail.contains("model_not_supported")
                        || http_err.detail.contains("requested model is not supported")
                        || http_err.detail.contains("unsupported_api_for_model")) =>
            {
                if let Some(fallback) = self.chat_retry_with_downgraded_effort(&body) {
                    return self
                        .post_json(&self.endpoints.chat_url, fallback, metadata)
                        .await;
                }
                Self::warn_chat_model_fallback_suppressed(&body);
                result
            }
            other => other,
        }
    }

    pub async fn stream_chat(
        &self,
        body: Map<String, Value>,
        metadata: Option<CopilotRequestMetadata>,
    ) -> Result<reqwest::Response, CopilotError> {
        let result = self
            .stream_request(&self.endpoints.chat_url, body.clone(), metadata.clone())
            .await;
        match result {
            Err(CopilotError::Http(ref http_err))
                if http_err.status_code == 400
                    && (http_err.detail.contains("model_not_supported")
                        || http_err.detail.contains("requested model is not supported")
                        || http_err.detail.contains("unsupported_api_for_model")) =>
            {
                if let Some(fallback) = self.chat_retry_with_downgraded_effort(&body) {
                    return self
                        .stream_request(&self.endpoints.chat_url, fallback, metadata)
                        .await;
                }
                Self::warn_chat_model_fallback_suppressed(&body);
                result
            }
            other => other,
        }
    }

    fn chat_retry_with_downgraded_effort(
        &self,
        body: &Map<String, Value>,
    ) -> Option<Map<String, Value>> {
        let model = body.get("model").and_then(Value::as_str)?;
        let normalized_model = model.strip_prefix("github-copilot/").unwrap_or(model);
        if normalized_model != "gpt-5.5" {
            return None;
        }
        let original = body.get("reasoning_effort").and_then(Value::as_str)?;
        let requested = EffortLevel::parse(original)?;
        if requested <= EffortLevel::High {
            return None;
        }
        let retry = EffortLevel::High.as_str();
        let mut retry_body = body.clone();
        retry_body.insert(
            "reasoning_effort".to_string(),
            Value::String(retry.to_string()),
        );
        tracing::warn!(
            model.requested = model,
            effort.original = original,
            effort.retry = retry,
            "chat effort downgraded for retry"
        );
        Some(retry_body)
    }

    fn warn_chat_model_fallback_suppressed(body: &Map<String, Value>) {
        tracing::warn!(
            model.requested = body
                .get("model")
                .and_then(|value| value.as_str())
                .unwrap_or(""),
            "chat model fallback suppressed"
        );
    }

    pub async fn post_messages(
        &self,
        body: Map<String, Value>,
        metadata: Option<CopilotRequestMetadata>,
    ) -> Result<Value, CopilotError> {
        self.post_json(&self.endpoints.messages_url, body, metadata)
            .await
    }

    pub async fn stream_messages(
        &self,
        body: Map<String, Value>,
        metadata: Option<CopilotRequestMetadata>,
    ) -> Result<reqwest::Response, CopilotError> {
        self.stream_request(&self.endpoints.messages_url, body, metadata)
            .await
    }

    pub async fn post_responses(
        &self,
        body: Map<String, Value>,
        metadata: Option<CopilotRequestMetadata>,
    ) -> Result<Value, CopilotError> {
        self.post_json(&self.endpoints.responses_url, body, metadata)
            .await
    }

    pub async fn stream_responses(
        &self,
        body: Map<String, Value>,
        metadata: Option<CopilotRequestMetadata>,
    ) -> Result<reqwest::Response, CopilotError> {
        self.stream_request(&self.endpoints.responses_url, body, metadata)
            .await
    }

    pub async fn get_response(
        &self,
        response_id: &str,
        metadata: Option<CopilotRequestMetadata>,
    ) -> Result<Value, CopilotError> {
        self.get_json(
            &format!("{}/{}", self.endpoints.responses_url, response_id),
            metadata,
        )
        .await
    }

    pub async fn connect_responses_websocket(
        &self,
        body: &Map<String, Value>,
        metadata: Option<CopilotRequestMetadata>,
    ) -> Result<
        tokio_tungstenite::WebSocketStream<
            tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
        >,
        CopilotError,
    > {
        use tokio_tungstenite::tungstenite::client::IntoClientRequest;
        let token = self.auth.copilot_token().await?;
        let mut request = self
            .endpoints
            .responses_ws_url
            .as_str()
            .into_client_request()
            .map_err(|err| CopilotError::Transport(err.to_string()))?;
        for (key, value) in self.headers(&token, body, metadata.as_ref()) {
            if let (Ok(name), Ok(val)) = (
                http::header::HeaderName::try_from(key.as_str()),
                http::header::HeaderValue::try_from(value.as_str()),
            ) {
                request.headers_mut().insert(name, val);
            }
        }
        let (stream, _) = tokio_tungstenite::connect_async(request)
            .await
            .map_err(|err| CopilotError::Transport(err.to_string()))?;
        tracing::info!(
            api.family = "responses_ws",
            model.effective = body.get("model").and_then(|v| v.as_str()).unwrap_or(""),
            "copilot websocket connected"
        );
        Ok(stream)
    }

    pub async fn cancel_response(
        &self,
        response_id: &str,
        metadata: Option<CopilotRequestMetadata>,
    ) -> Result<Value, CopilotError> {
        self.post_json(
            &format!("{}/{}/cancel", self.endpoints.responses_url, response_id),
            Map::new(),
            metadata,
        )
        .await
    }

    pub async fn refresh_models_if_stale(&self) {
        if !self
            .models
            .refresh_needed(self.config.copilot_models_ttl)
            .await
        {
            return;
        }
        match self.get_json(&self.endpoints.models_url, None).await {
            Ok(value) => {
                if let Some(models) = value.get("data").and_then(Value::as_array) {
                    self.models.set_copilot_models(models.clone()).await;
                }
            }
            Err(error) => {
                tracing::warn!(
                    error = %error,
                    "failed to refresh Copilot models; keeping cached/static models"
                );
            }
        }
    }

    async fn post_json(
        &self,
        url: &str,
        body: Map<String, Value>,
        metadata: Option<CopilotRequestMetadata>,
    ) -> Result<Value, CopilotError> {
        let family = endpoint_family(url);
        let model = body.get("model").and_then(Value::as_str);
        let mut last_response_text = String::new();
        for attempt in 0..=self.config.copilot_retry_max {
            if let Some(rate_limiter) = &self.rate_limiter {
                let waited = rate_limiter.acquire().await;
                if waited > 0.0 {
                    tracing::info!(
                        wait.seconds = waited,
                        api.family = family,
                        "copilot request rate limited"
                    );
                }
            }
            let started = std::time::Instant::now();
            tracing::info!(
                api.family = family,
                model.effective = model.unwrap_or(""),
                attempt = attempt as u64,
                stream = false,
                "copilot request started"
            );
            let token = self.auth.copilot_token().await?;
            let mut request = self.client.post(url);
            for (key, value) in self.headers(&token, &body, metadata.as_ref()) {
                request = request.header(key, value);
            }
            let response = request
                .json(&body)
                .send()
                .await
                .map_err(map_reqwest_error)?;
            if response.status() == StatusCode::TOO_MANY_REQUESTS
                && attempt < self.config.copilot_retry_max
            {
                let delay = retry_delay(
                    response.headers(),
                    attempt,
                    self.config.copilot_retry_base_delay,
                );
                tracing::warn!(
                    api.family = family,
                    http.status_code = response.status().as_u16() as u64,
                    attempt = attempt as u64,
                    retry.delay_ms = delay.as_millis() as u64,
                    "copilot request retrying"
                );
                tokio::time::sleep(delay).await;
                continue;
            }
            if response.status().is_success() {
                let status = response.status().as_u16();
                let value = response.json().await.map_err(map_reqwest_error)?;
                let usage = crate::telemetry::summarize_usage(&value);
                tracing::info!(
                    api.family = family,
                    http.status_code = status as u64,
                    elapsed.ms = started.elapsed().as_millis() as u64,
                    attempt = attempt as u64,
                    "copilot request completed"
                );
                if usage.input_tokens.is_some()
                    || usage.output_tokens.is_some()
                    || usage.cached_tokens.is_some()
                    || usage.total_tokens.is_some()
                {
                    tracing::info!(
                        api.family = family,
                        tokens.input = usage.input_tokens.unwrap_or(0),
                        tokens.output = usage.output_tokens.unwrap_or(0),
                        tokens.cached = usage.cached_tokens.unwrap_or(0),
                        tokens.total = usage.total_tokens.unwrap_or(0),
                        "copilot usage"
                    );
                }
                return Ok(value);
            }
            let status = response.status().as_u16();
            let should_retry = retryable_status(status) && attempt < self.config.copilot_retry_max;
            let delay = if should_retry {
                Some(retry_delay(
                    response.headers(),
                    attempt,
                    self.config.copilot_retry_base_delay,
                ))
            } else {
                None
            };
            last_response_text = response.text().await.unwrap_or_default();
            let sanitized_detail = sanitize_upstream_error(status, &last_response_text);
            if let Some(delay) = delay {
                tracing::warn!(
                    api.family = family,
                    http.status_code = status as u64,
                    attempt = attempt as u64,
                    retry.delay_ms = delay.as_millis() as u64,
                    "copilot request retrying"
                );
                tokio::time::sleep(delay).await;
                continue;
            }
            tracing::warn!(
                api.family = family,
                http.status_code = status as u64,
                elapsed.ms = started.elapsed().as_millis() as u64,
                attempt = attempt as u64,
                "copilot request completed"
            );
            if retryable_status(status) {
                return Err(transient_error_for_status(status, sanitized_detail).into());
            }
            return Err(CopilotHttpError {
                status_code: status,
                detail: sanitized_detail,
            }
            .into());
        }
        Err(CopilotHttpError {
            status_code: 500,
            detail: last_response_text,
        }
        .into())
    }

    async fn get_json(
        &self,
        url: &str,
        metadata: Option<CopilotRequestMetadata>,
    ) -> Result<Value, CopilotError> {
        let family = endpoint_family(url);
        for attempt in 0..=self.config.copilot_retry_max {
            if let Some(rate_limiter) = &self.rate_limiter {
                rate_limiter.acquire().await;
            }
            let started = std::time::Instant::now();
            tracing::info!(
                api.family = family,
                model.effective = "",
                attempt = attempt as u64,
                stream = false,
                "copilot request started"
            );
            let token = self.auth.copilot_token().await?;
            let mut request = self.client.get(url);
            for (key, value) in self.headers(&token, &Map::new(), metadata.as_ref()) {
                request = request.header(key, value);
            }
            let response = request.send().await.map_err(map_reqwest_error)?;
            if response.status() == StatusCode::TOO_MANY_REQUESTS
                && attempt < self.config.copilot_retry_max
            {
                let delay = retry_delay(
                    response.headers(),
                    attempt,
                    self.config.copilot_retry_base_delay,
                );
                tracing::warn!(
                    api.family = family,
                    http.status_code = response.status().as_u16() as u64,
                    attempt = attempt as u64,
                    retry.delay_ms = delay.as_millis() as u64,
                    "copilot request retrying"
                );
                tokio::time::sleep(delay).await;
                continue;
            }
            if response.status().is_success() {
                let status = response.status().as_u16();
                let value = response.json().await.map_err(map_reqwest_error)?;
                tracing::info!(
                    api.family = family,
                    http.status_code = status as u64,
                    elapsed.ms = started.elapsed().as_millis() as u64,
                    attempt = attempt as u64,
                    "copilot request completed"
                );
                return Ok(value);
            }
            let status = response.status().as_u16();
            tracing::warn!(
                api.family = family,
                http.status_code = status as u64,
                elapsed.ms = started.elapsed().as_millis() as u64,
                attempt = attempt as u64,
                "copilot request completed"
            );
            return Err(CopilotHttpError {
                status_code: status,
                detail: sanitize_upstream_error(status, &response.text().await.unwrap_or_default()),
            }
            .into());
        }
        Err(CopilotError::Transport(
            "GET retry loop exhausted".to_string(),
        ))
    }

    async fn stream_request(
        &self,
        url: &str,
        body: Map<String, Value>,
        metadata: Option<CopilotRequestMetadata>,
    ) -> Result<reqwest::Response, CopilotError> {
        let family = endpoint_family(url);
        let model = body.get("model").and_then(Value::as_str).unwrap_or("");
        let started = std::time::Instant::now();
        tracing::info!(
            api.family = family,
            model.effective = model,
            attempt = 0_u64,
            stream = true,
            "copilot request started"
        );
        let mut last_err: Option<CopilotError> = None;
        for attempt in 0..=self.config.copilot_retry_max {
            if let Some(rate_limiter) = &self.rate_limiter {
                rate_limiter.acquire().await;
            }
            let token = self.auth.copilot_token().await?;
            let mut request = self.client.post(url);
            for (key, value) in self.headers(&token, &body, metadata.as_ref()) {
                request = request.header(key, value);
            }
            let response = request
                .json(&body)
                .send()
                .await
                .map_err(map_reqwest_error)?;
            if response.status().is_success() {
                let status = response.status().as_u16();
                tracing::info!(
                    api.family = family,
                    http.status_code = status as u64,
                    elapsed.ms = started.elapsed().as_millis() as u64,
                    stream = true,
                    "copilot request completed"
                );
                return Ok(response);
            }
            if response.status() == StatusCode::TOO_MANY_REQUESTS
                && attempt < self.config.copilot_retry_max
            {
                let status = response.status().as_u16();
                let delay = retry_delay(
                    response.headers(),
                    attempt,
                    self.config.copilot_retry_base_delay,
                );
                let msg =
                    sanitize_upstream_error(status, &response.text().await.unwrap_or_default());
                tokio::time::sleep(delay).await;
                last_err = Some(
                    TransientBackendError {
                        status_code: 429,
                        error_type: "rate_limit_error".to_string(),
                        message: msg,
                        backend: "copilot".to_string(),
                    }
                    .into(),
                );
                continue;
            }
            let status = response.status().as_u16();
            let raw_detail = response.text().await.unwrap_or_default();
            let detail = sanitize_upstream_error(status, &raw_detail);
            tracing::warn!(
                api.family = family,
                http.status_code = status as u64,
                elapsed.ms = started.elapsed().as_millis() as u64,
                stream = true,
                "copilot request completed"
            );
            return if matches!(status, 429 | 500 | 502 | 503 | 504) {
                Err(TransientBackendError {
                    status_code: status,
                    error_type: error_type_for_status(status).to_string(),
                    message: detail,
                    backend: "copilot".to_string(),
                }
                .into())
            } else {
                Err(CopilotHttpError {
                    status_code: status,
                    detail,
                }
                .into())
            };
        }
        // Unreachable: the final loop iteration (attempt == retry_max) always
        // hits the non-retry error handler above and returns before the loop
        // exits normally. Kept as a compile-time necessity.
        Err(last_err
            .unwrap_or_else(|| CopilotError::Transport("stream retry loop exhausted".to_string())))
    }

    fn headers(
        &self,
        token: &str,
        body: &Map<String, Value>,
        metadata: Option<&CopilotRequestMetadata>,
    ) -> Vec<(String, String)> {
        let mut headers = base_copilot_request_headers(token);
        headers.insert("Content-Type".to_string(), "application/json".to_string());
        let initiator = metadata
            .and_then(|m| m.initiator.clone())
            .unwrap_or_else(|| compute_initiator(body, false).to_string());
        headers.insert("X-Initiator".to_string(), initiator);
        if let Some(metadata) = metadata {
            if let Some(request_id) = &metadata.request_id {
                headers.insert("X-Request-Id".to_string(), request_id.clone());
            }
            if let Some(openai_intent) = &metadata.openai_intent {
                headers.insert("OpenAI-Intent".to_string(), openai_intent.clone());
            }
            if let Some(interaction_id) = &metadata.interaction_id {
                headers.insert("X-Interaction-Id".to_string(), interaction_id.clone());
            }
            if let Some(interaction_type) = &metadata.interaction_type {
                headers.insert("X-Interaction-Type".to_string(), interaction_type.clone());
            }
            if let Some(agent_task_id) = &metadata.agent_task_id {
                headers.insert("X-Agent-Task-Id".to_string(), agent_task_id.clone());
            }
            headers.extend(metadata.extra_headers.clone());
        }
        headers.into_iter().collect()
    }
}

fn endpoint_family(url: &str) -> &'static str {
    if url.contains("/chat/completions") {
        "chat_completions"
    } else if url.contains("/v1/messages") {
        "messages"
    } else if url.contains("/responses") {
        "responses"
    } else if url.contains("/models") {
        "models"
    } else {
        "copilot"
    }
}

fn retry_delay(headers: &reqwest::header::HeaderMap, attempt: u32, base_delay: f64) -> Duration {
    let retry_after = headers
        .get(reqwest::header::RETRY_AFTER)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<f64>().ok());
    Duration::from_secs_f64(
        retry_after
            .unwrap_or(base_delay * 2_f64.powi(attempt as i32))
            .min(30.0),
    )
}

fn retryable_status(status: u16) -> bool {
    matches!(status, 429 | 500 | 502 | 503 | 504)
}

fn transient_error_for_status(status: u16, detail: String) -> TransientBackendError {
    TransientBackendError {
        status_code: status,
        error_type: error_type_for_status(status).to_string(),
        message: detail,
        backend: "copilot".to_string(),
    }
}

fn error_type_for_status(status: u16) -> &'static str {
    match status {
        429 => "rate_limit_error",
        504 => "timeout_error",
        _ => "api_error",
    }
}

fn sanitize_upstream_error(status: u16, raw: &str) -> String {
    let safe_class = [
        "model_not_supported",
        "requested model is not supported",
        "unsupported_api_for_model",
    ]
    .into_iter()
    .find(|needle| raw.contains(needle));
    match safe_class {
        Some(class) => format!("Copilot request failed with HTTP {status}: {class}"),
        None => format!("Copilot request failed with HTTP {status}"),
    }
}

fn map_reqwest_error(error: reqwest::Error) -> CopilotError {
    if error.is_timeout() {
        TransientBackendError {
            status_code: 504,
            error_type: "timeout_error".to_string(),
            message: error.to_string(),
            backend: "copilot".to_string(),
        }
        .into()
    } else if error.is_connect() {
        TransientBackendError {
            status_code: 502,
            error_type: "connection_error".to_string(),
            message: error.to_string(),
            backend: "copilot".to_string(),
        }
        .into()
    } else {
        CopilotError::Transport(error.to_string())
    }
}
