pub mod log_capture;

use std::collections::HashMap;
use std::collections::VecDeque;
use std::net::SocketAddr;
use std::sync::Arc;

use axum::Router;
use axum::extract::{Request, State};
use axum::routing::any;
use bytes::Bytes;
use http::{Method, StatusCode};
use http_body_util::BodyExt as _;
use serde_json::Value;
use tempfile::TempDir;
use tokio::net::TcpListener;
use tokio::sync::Mutex;

use copilot_proxy_rs::auth::{AuthEndpoints, CopilotAuth};
use copilot_proxy_rs::config::{AppConfig, EnvSource, LocalModelConfig};
use copilot_proxy_rs::copilot::client::{CopilotBackend, CopilotEndpoints};
use copilot_proxy_rs::models::ModelRegistry;
use copilot_proxy_rs::state::AppState;

type HeaderPairs = Vec<(&'static str, &'static str)>;
type SeqEntry = (u16, Value, HeaderPairs);
type TextSeqEntry = (u16, &'static str, String);
type RequestKey = (String, String);
type CapturedHeaders = HashMap<String, String>;
type SeqMap = Arc<Mutex<HashMap<RequestKey, VecDeque<SeqEntry>>>>;
type TextSeqMap = Arc<Mutex<HashMap<RequestKey, VecDeque<TextSeqEntry>>>>;
type HeaderCaptureMap = Arc<Mutex<HashMap<RequestKey, CapturedHeaders>>>;
type BodyCaptureMap = Arc<Mutex<HashMap<RequestKey, Vec<u8>>>>;
type BodyLogMap = Arc<Mutex<HashMap<(String, String), Vec<Vec<u8>>>>>;

fn repo_tempdir(prefix: &str) -> TempDir {
    tempfile::Builder::new()
        .prefix(prefix)
        .tempdir_in(env!("CARGO_MANIFEST_DIR"))
        .unwrap()
}

#[allow(dead_code)]
#[derive(Debug, Clone)]
enum MockRouteResponse {
    Json(u16, Value),
    Text(u16, &'static str, String),
    /// Streams the body as separate HTTP chunks with a 1 ms pause between them,
    /// reproducing the split-chunk scenario that exposes the SSE line-buffering bug.
    SplitChunks(u16, String, Vec<Bytes>),
    /// Waits `delay_ms` before sending the JSON response, creating the window
    /// needed to demonstrate a concurrent-refresh race in tests.
    DelayedJson(u64, u16, Value),
}

#[derive(Debug, Clone)]
struct MockState {
    routes: Arc<Mutex<HashMap<(String, String), MockRouteResponse>>>,
    sequences: SeqMap,
    text_sequences: TextSeqMap,
    hits: Arc<Mutex<HashMap<(String, String), usize>>>,
    last_headers: HeaderCaptureMap,
    last_request_body: BodyCaptureMap,
    request_bodies: BodyLogMap,
}

impl Default for MockState {
    fn default() -> Self {
        Self {
            routes: Arc::new(Mutex::new(HashMap::new())),
            sequences: Arc::new(Mutex::new(HashMap::new())),
            text_sequences: Arc::new(Mutex::new(HashMap::new())),
            hits: Arc::new(Mutex::new(HashMap::new())),
            last_headers: Arc::new(Mutex::new(HashMap::new())),
            last_request_body: Arc::new(Mutex::new(HashMap::new())),
            request_bodies: Arc::new(Mutex::new(HashMap::new())),
        }
    }
}

#[derive(Debug, Clone)]
pub struct MockServer {
    pub base_url: String,
    state: MockState,
}

impl MockServer {
    pub async fn start() -> Self {
        let state = MockState::default();
        let app_state = state.clone();
        let app = Router::new()
            .route("/{*path}", any(handle_mock))
            .route("/", any(handle_mock))
            .with_state(app_state);
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr: SocketAddr = listener.local_addr().unwrap();
        let base_url = format!("http://{addr}");
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        Self { base_url, state }
    }

    pub async fn respond_json(&self, method: &str, path: &str, status: u16, body: Value) {
        self.state.routes.lock().await.insert(
            (method.to_string(), path.to_string()),
            MockRouteResponse::Json(status, body),
        );
    }

    #[allow(dead_code)]
    pub async fn respond_text(
        &self,
        method: &str,
        path: &str,
        status: u16,
        content_type: &'static str,
        body: String,
    ) {
        self.state.routes.lock().await.insert(
            (method.to_string(), path.to_string()),
            MockRouteResponse::Text(status, content_type, body),
        );
    }

    #[allow(dead_code)]
    pub async fn respond_sse_split_chunks(
        &self,
        method: &str,
        path: &str,
        status: u16,
        chunks: Vec<&[u8]>,
    ) {
        let bytes: Vec<Bytes> = chunks.into_iter().map(Bytes::copy_from_slice).collect();
        self.state.routes.lock().await.insert(
            (method.to_string(), path.to_string()),
            MockRouteResponse::SplitChunks(status, "text/event-stream".to_string(), bytes),
        );
    }

    #[allow(dead_code)]
    pub async fn respond_json_delayed(
        &self,
        method: &str,
        path: &str,
        delay_ms: u64,
        status: u16,
        body: Value,
    ) {
        self.state.routes.lock().await.insert(
            (method.to_string(), path.to_string()),
            MockRouteResponse::DelayedJson(delay_ms, status, body),
        );
    }

    #[allow(dead_code)]
    pub async fn respond_sse(
        &self,
        method: &str,
        path: &str,
        status: u16,
        lines: Vec<&'static str>,
    ) {
        let body = lines.join("\n\n") + "\n\n";
        self.state.routes.lock().await.insert(
            (method.to_string(), path.to_string()),
            MockRouteResponse::Text(status, "text/event-stream", body),
        );
    }

    #[allow(dead_code)]
    pub async fn respond_sequence_json(&self, method: &str, path: &str, responses: Vec<SeqEntry>) {
        let queue: VecDeque<_> = responses.into_iter().collect();
        self.state
            .sequences
            .lock()
            .await
            .insert((method.to_string(), path.to_string()), queue);
    }

    #[allow(dead_code)]
    pub async fn respond_sequence_text(
        &self,
        method: &str,
        path: &str,
        responses: Vec<TextSeqEntry>,
    ) {
        let queue: VecDeque<_> = responses.into_iter().collect();
        self.state
            .text_sequences
            .lock()
            .await
            .insert((method.to_string(), path.to_string()), queue);
    }

    #[allow(dead_code)]
    pub async fn hits(&self, method: &str, path: &str) -> usize {
        *self
            .state
            .hits
            .lock()
            .await
            .get(&(method.to_string(), path.to_string()))
            .unwrap_or(&0)
    }

    #[allow(dead_code)]
    pub async fn last_request_header(
        &self,
        method: &str,
        path: &str,
        header: &str,
    ) -> Option<String> {
        self.state
            .last_headers
            .lock()
            .await
            .get(&(method.to_string(), path.to_string()))
            .and_then(|headers| headers.get(header).cloned())
    }

    #[allow(dead_code)]
    pub async fn last_request_body_json(&self, method: &str, path: &str) -> Option<Value> {
        self.state
            .last_request_body
            .lock()
            .await
            .get(&(method.to_string(), path.to_string()))
            .and_then(|bytes| serde_json::from_slice(bytes).ok())
    }

    /// Every request body recorded for a route, in the order they arrived.
    #[allow(dead_code)]
    pub async fn request_bodies_json(&self, method: &str, path: &str) -> Vec<Value> {
        self.state
            .request_bodies
            .lock()
            .await
            .get(&(method.to_string(), path.to_string()))
            .map(|bodies| {
                bodies
                    .iter()
                    .filter_map(|bytes| serde_json::from_slice(bytes).ok())
                    .collect()
            })
            .unwrap_or_default()
    }

    pub fn auth_endpoints(&self) -> AuthEndpoints {
        AuthEndpoints {
            device_code_url: format!("{}/device/code", self.base_url),
            oauth_token_url: format!("{}/oauth/token", self.base_url),
            copilot_token_url: format!("{}/copilot/token", self.base_url),
        }
    }

    pub fn copilot_endpoints(&self) -> CopilotEndpoints {
        CopilotEndpoints {
            chat_url: format!("{}/chat/completions", self.base_url),
            messages_url: format!("{}/v1/messages", self.base_url),
            responses_url: format!("{}/responses", self.base_url),
            responses_ws_url: format!(
                "ws://{}/responses",
                self.base_url.trim_start_matches("http://")
            ),
            models_url: format!("{}/models", self.base_url),
            decisions_url: format!("{}/v1/decisions", self.base_url),
            systemone_url: format!("{}/v1/systemone", self.base_url),
            images_generations_url: format!("{}/v1/images/generations", self.base_url),
            images_edits_url: format!("{}/v1/images/edits", self.base_url),
        }
    }

    #[allow(dead_code)]
    pub async fn last_request_body_bytes(&self, method: &str, path: &str) -> Option<Vec<u8>> {
        self.state
            .last_request_body
            .lock()
            .await
            .get(&(method.to_string(), path.to_string()))
            .cloned()
    }

    /// Asserts no chat/messages/responses generation endpoint was called.
    #[allow(dead_code)]
    pub async fn assert_no_generation_calls(&self) {
        for path in ["/chat/completions", "/responses", "/v1/messages"] {
            assert_eq!(self.hits("POST", path).await, 0, "unexpected POST {path}");
        }
    }
}

/// Loads `tests/fixtures/<name>` (recorded upstream cases).
#[allow(dead_code)]
pub fn fixture_json(name: &str) -> Value {
    let path = format!("{}/tests/fixtures/{name}", env!("CARGO_MANIFEST_DIR"));
    serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
}

/// Sends one request through the proxy router; returns status, content type and body text.
#[allow(dead_code)]
pub async fn send(
    state: &AppState,
    uri: &str,
    content_type: &str,
    body: impl Into<axum::body::Body>,
) -> (StatusCode, String, String) {
    use tower::ServiceExt as _;
    let response = copilot_proxy_rs::http::router(state.clone())
        .oneshot(
            http::Request::builder()
                .method("POST")
                .uri(uri)
                .header("content-type", content_type)
                .body(body.into())
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    let content_type = response
        .headers()
        .get("content-type")
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default()
        .to_string();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    (
        status,
        content_type,
        String::from_utf8(bytes.to_vec()).unwrap(),
    )
}

#[allow(dead_code)]
pub async fn send_json(state: &AppState, uri: &str, body: Value) -> (StatusCode, String, String) {
    send(state, uri, "application/json", body.to_string()).await
}

/// `data:` payloads of an SSE body, parsed as JSON (skips `[DONE]`).
#[allow(dead_code)]
pub fn sse_data(body: &str) -> Vec<Value> {
    body.lines()
        .filter_map(|line| line.strip_prefix("data: "))
        .filter(|payload| *payload != "[DONE]")
        .map(|payload| serde_json::from_str(payload).unwrap())
        .collect()
}

async fn handle_mock(
    State(state): State<MockState>,
    method: Method,
    request: Request,
) -> axum::response::Response {
    let path = request.uri().path().to_string();
    let key = (method.as_str().to_string(), path.clone());

    *state.hits.lock().await.entry(key.clone()).or_default() += 1;

    let (parts, body) = request.into_parts();
    {
        let mut last_headers = state.last_headers.lock().await;
        let captured: HashMap<String, String> = parts
            .headers
            .iter()
            .filter_map(|(key, value)| {
                value
                    .to_str()
                    .ok()
                    .map(|value| (key.as_str().to_string(), value.to_string()))
            })
            .collect();
        last_headers.insert(key.clone(), captured);
    }
    let body_bytes = body
        .collect()
        .await
        .map(|collected| collected.to_bytes())
        .unwrap_or_default();
    {
        state
            .last_request_body
            .lock()
            .await
            .insert(key.clone(), body_bytes.to_vec());
    }
    {
        state
            .request_bodies
            .lock()
            .await
            .entry(key.clone())
            .or_default()
            .push(body_bytes.to_vec());
    }

    if let Some(entry) = state.text_sequences.lock().await.get_mut(&key) {
        if let Some((status, content_type, body)) = entry.pop_front() {
            return axum::response::Response::builder()
                .status(StatusCode::from_u16(status).unwrap_or(StatusCode::OK))
                .header("content-type", content_type)
                .body(axum::body::Body::from(body))
                .unwrap();
        }
    }

    if let Some(entry) = state.sequences.lock().await.get_mut(&key) {
        if let Some((status, body, extra_headers)) = entry.pop_front() {
            let mut builder = axum::response::Response::builder()
                .status(StatusCode::from_u16(status).unwrap_or(StatusCode::OK))
                .header("content-type", "application/json");
            for (name, value) in &extra_headers {
                builder = builder.header(*name, *value);
            }
            let json_bytes = serde_json::to_vec(&body).unwrap();
            return builder.body(axum::body::Body::from(json_bytes)).unwrap();
        }
    }

    // Clone the route response so the lock is dropped before any potential sleep.
    let route_response = state.routes.lock().await.get(&key).cloned();
    match route_response {
        Some(MockRouteResponse::Json(status, body)) => {
            let json_bytes = serde_json::to_vec(&body).unwrap();
            axum::response::Response::builder()
                .status(StatusCode::from_u16(status).unwrap_or(StatusCode::OK))
                .header("content-type", "application/json")
                .body(axum::body::Body::from(json_bytes))
                .unwrap()
        }
        Some(MockRouteResponse::Text(status, content_type, body)) => {
            axum::response::Response::builder()
                .status(StatusCode::from_u16(status).unwrap_or(StatusCode::OK))
                .header("content-type", content_type)
                .body(axum::body::Body::from(Bytes::copy_from_slice(
                    body.as_bytes(),
                )))
                .unwrap()
        }
        Some(MockRouteResponse::SplitChunks(status, content_type, chunks)) => {
            // Emit each chunk as a separate HTTP chunk with a 1 ms gap so that
            // reqwest's bytes_stream() sees them as distinct Bytes items.
            let stream = async_stream::stream! {
                for chunk in chunks {
                    tokio::time::sleep(tokio::time::Duration::from_millis(1)).await;
                    yield Ok::<Bytes, std::io::Error>(chunk);
                }
            };
            axum::response::Response::builder()
                .status(StatusCode::from_u16(status).unwrap_or(StatusCode::OK))
                .header("content-type", content_type)
                .body(axum::body::Body::from_stream(stream))
                .unwrap()
        }
        Some(MockRouteResponse::DelayedJson(delay_ms, status, body)) => {
            tokio::time::sleep(tokio::time::Duration::from_millis(delay_ms)).await;
            let json_bytes = serde_json::to_vec(&body).unwrap();
            axum::response::Response::builder()
                .status(StatusCode::from_u16(status).unwrap_or(StatusCode::OK))
                .header("content-type", "application/json")
                .body(axum::body::Body::from(json_bytes))
                .unwrap()
        }
        None => {
            let body = serde_json::json!({"error": "not found", "path": path});
            let json_bytes = serde_json::to_vec(&body).unwrap();
            axum::response::Response::builder()
                .status(StatusCode::NOT_FOUND)
                .header("content-type", "application/json")
                .body(axum::body::Body::from(json_bytes))
                .unwrap()
        }
    }
}

#[allow(dead_code)]
pub struct AppFixture {
    pub state: AppState,
    pub mock: MockServer,
    _config_dir: TempDir,
}

#[allow(dead_code)]
impl AppFixture {
    pub async fn with_mock_local() -> Self {
        let mock = MockServer::start().await;
        let base_url = format!("{}/v1", mock.base_url);
        Self::with_mock_and_local_base_url(mock, base_url)
    }

    pub async fn with_local_base_url(base_url: &str) -> Self {
        let mock = MockServer::start().await;
        Self::with_mock_and_local_base_url(mock, base_url.to_string())
    }

    fn with_mock_and_local_base_url(mock: MockServer, base_url: String) -> Self {
        let temp = repo_tempdir("app-fixture-local-");
        let temp_path = temp.path().to_path_buf();
        std::fs::write(temp_path.join("github_token"), "github-token").unwrap();
        let env =
            EnvSource::from_pairs([("COPILOT_PROXY_RS_CONFIG_DIR", temp_path.to_str().unwrap())]);
        let mut config = AppConfig::default();
        config.local_models.insert(
            "qwen3-coder-30b-local".to_string(),
            LocalModelConfig {
                base_url,
                upstream_model: r"models\Qwen3-Coder-30B-A3B-Instruct-IQ4_XS.gguf".to_string(),
            },
        );
        let mut state = AppState::new(config);
        let auth = Arc::new(CopilotAuth::with_env_for_tests(
            state.config.clone(),
            env,
            mock.auth_endpoints(),
            false,
        ));
        state.copilot = Arc::new(CopilotBackend::with_endpoints_for_tests(
            state.config.clone(),
            auth,
            state.models.clone(),
            mock.copilot_endpoints(),
        ));
        Self {
            state,
            mock,
            _config_dir: temp,
        }
    }

    pub async fn with_mock_copilot() -> Self {
        let mock = MockServer::start().await;
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
        let temp = repo_tempdir("app-fixture-");
        let temp_path = temp.path().to_path_buf();
        std::fs::write(temp_path.join("github_token"), "github-token").unwrap();
        let env =
            EnvSource::from_pairs([("COPILOT_PROXY_RS_CONFIG_DIR", temp_path.to_str().unwrap())]);
        let config = Arc::new(AppConfig::load_from_env(&env).unwrap());
        let auth = Arc::new(CopilotAuth::with_env_for_tests(
            config.clone(),
            env,
            mock.auth_endpoints(),
            false,
        ));
        let models = Arc::new(ModelRegistry::new());
        let copilot = Arc::new(CopilotBackend::with_endpoints_for_tests(
            config.clone(),
            auth,
            models.clone(),
            mock.copilot_endpoints(),
        ));
        let state = AppState::with_parts_for_tests(config, models, copilot);
        Self {
            state,
            mock,
            _config_dir: temp,
        }
    }

    /// Copilot fixture whose `/models` catalog is the recorded non-chat sample.
    pub async fn with_nonchat_models() -> Self {
        let fixture = Self::with_mock_copilot().await;
        fixture
            .mock
            .respond_json(
                "GET",
                "/models",
                200,
                fixture_json("copilot_models_nonchat.json"),
            )
            .await;
        fixture
    }

    /// Non-chat catalog plus recorded `/v1/decisions` and `/v1/systemone` answers.
    pub async fn with_decision_models() -> Self {
        let fixture = Self::with_nonchat_models().await;
        let decisions = fixture_json("copilot_decisions_predicate.json");
        let systemone = fixture_json("copilot_systemone_noul.json");
        fixture
            .mock
            .respond_json(
                "POST",
                "/v1/decisions",
                200,
                decisions["predicate_messages_input"]["response"].clone(),
            )
            .await;
        fixture
            .mock
            .respond_json(
                "POST",
                "/v1/systemone",
                200,
                systemone["noul_messages_state"]["response"].clone(),
            )
            .await;
        fixture
    }

    /// Non-chat catalog plus recorded per-option `/v1/decisions` and `/v1/systemone` answers.
    pub async fn with_decision_option_answers() -> Self {
        let fixture = Self::with_nonchat_models().await;
        for model in ["gpt-6-luna-decisions", "microsoft-decision-1"] {
            let (path, recorded) = recorded_options_exchange(model);
            fixture
                .mock
                .respond_json("POST", path, 200, recorded["response"].clone())
                .await;
        }
        fixture
    }
}

#[allow(dead_code)]
pub const OPTIONS_QUESTION: &str = "Pick a lunch spot. Option A: tacos by the park. Option B: noodles downtown. Which option should we pick?";

/// Answer text expected for [`OPTIONS_QUESTION`] given the recorded option answers.
#[allow(dead_code)]
pub fn expected_options_answer(model: &str) -> &'static str {
    match model {
        "gpt-6-luna-decisions" => {
            "Option B (0.75); Option A 0.25\nReason: Option B is favored over Option A (0.75 vs 0.25)."
        }
        _ => {
            "Option A (0.75); Option B 0.25\nReason: Option A is favored over Option B (0.75 vs 0.25)."
        }
    }
}

/// Recorded per-option exchange (request and response) for a decision model.
#[allow(dead_code)]
pub fn recorded_options_exchange(model: &str) -> (&'static str, Value) {
    match model {
        "gpt-6-luna-decisions" => (
            "/v1/decisions",
            fixture_json("copilot_decisions_options.json")["options_messages_input"].clone(),
        ),
        _ => (
            "/v1/systemone",
            fixture_json("copilot_systemone_options.json")["options_messages_state"].clone(),
        ),
    }
}

/// Asserts the per-option question went upstream once, as recorded.
#[allow(dead_code)]
pub async fn assert_decision_options_upstream(mock: &MockServer, model: &str) {
    let (path, recorded) = recorded_options_exchange(model);
    assert_upstream_request(mock, path, &recorded["request"], 1).await;
}

/// Recorded upstream request for a decision model's synthesized sky question.
#[allow(dead_code)]
pub fn recorded_decision_request(model: &str) -> (&'static str, Value) {
    match model {
        "gpt-6-luna-decisions" => (
            "/v1/decisions",
            fixture_json("copilot_decisions_predicate.json")["predicate_messages_input"]["request"]
                .clone(),
        ),
        _ => (
            "/v1/systemone",
            fixture_json("copilot_systemone_noul.json")["noul_messages_state"]["request"].clone(),
        ),
    }
}

#[allow(dead_code)]
pub const SKY_QUESTION: &str = "Is the sky blue on a clear day?";

/// Asserts the decision model hit only its own endpoint, with the recorded request body.
#[allow(dead_code)]
pub async fn assert_decision_upstream(mock: &MockServer, model: &str, calls: usize) {
    let (path, expected) = recorded_decision_request(model);
    assert_upstream_request(mock, path, &expected, calls).await;
}

async fn assert_upstream_request(mock: &MockServer, path: &str, expected: &Value, calls: usize) {
    let other = if path == "/v1/decisions" {
        "/v1/systemone"
    } else {
        "/v1/decisions"
    };
    assert_eq!(mock.hits("POST", path).await, calls);
    assert_eq!(mock.hits("POST", other).await, 0);
    assert_eq!(
        &mock.last_request_body_json("POST", path).await.unwrap(),
        expected
    );
    mock.assert_no_generation_calls().await;
}

/// Asserts an image-model rejection made no upstream POST at all.
#[allow(dead_code)]
pub async fn assert_no_upstream_posts(mock: &MockServer) {
    for path in [
        "/v1/decisions",
        "/v1/systemone",
        "/v1/images/generations",
        "/v1/images/edits",
    ] {
        assert_eq!(mock.hits("POST", path).await, 0, "unexpected POST {path}");
    }
    mock.assert_no_generation_calls().await;
}

#[allow(dead_code)]
pub fn image_model_message(route: &str) -> String {
    format!(
        "model 'gpt-image-2.5-flare' is an image model and does not support {route}; \
         use POST /v1/images/generations or POST /v1/images/edits"
    )
}

#[allow(dead_code)]
pub struct BackendFixture {
    pub backend: Arc<CopilotBackend>,
    pub mock: MockServer,
    _config_dir: TempDir,
}

#[allow(dead_code)]
pub async fn backend_fixture(mock: MockServer) -> BackendFixture {
    let temp = repo_tempdir("backend-fixture-");
    let temp_path = temp.path().to_path_buf();
    std::fs::write(temp_path.join("github_token"), "github-token").unwrap();
    let env = EnvSource::from_pairs([("COPILOT_PROXY_RS_CONFIG_DIR", temp_path.to_str().unwrap())]);
    let config = Arc::new(AppConfig::load_from_env(&env).unwrap());
    let auth = Arc::new(CopilotAuth::with_env_for_tests(
        config.clone(),
        env,
        mock.auth_endpoints(),
        false,
    ));
    let models = Arc::new(ModelRegistry::new());
    let backend = Arc::new(CopilotBackend::with_endpoints_for_tests(
        config,
        auth,
        models,
        mock.copilot_endpoints(),
    ));
    BackendFixture {
        backend,
        mock,
        _config_dir: temp,
    }
}
