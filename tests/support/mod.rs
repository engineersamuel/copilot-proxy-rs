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
use copilot_proxy_rs::config::{AppConfig, EnvSource};
use copilot_proxy_rs::copilot::client::{CopilotBackend, CopilotEndpoints};
use copilot_proxy_rs::models::ModelRegistry;
use copilot_proxy_rs::state::AppState;

type HeaderPairs = Vec<(&'static str, &'static str)>;
type SeqEntry = (u16, Value, HeaderPairs);
type RequestKey = (String, String);
type CapturedHeaders = HashMap<String, String>;
type SeqMap = Arc<Mutex<HashMap<RequestKey, VecDeque<SeqEntry>>>>;
type HeaderCaptureMap = Arc<Mutex<HashMap<RequestKey, CapturedHeaders>>>;
type BodyCaptureMap = Arc<Mutex<HashMap<RequestKey, Vec<u8>>>>;

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
    hits: Arc<Mutex<HashMap<(String, String), usize>>>,
    last_headers: HeaderCaptureMap,
    last_request_body: BodyCaptureMap,
}

impl Default for MockState {
    fn default() -> Self {
        Self {
            routes: Arc::new(Mutex::new(HashMap::new())),
            sequences: Arc::new(Mutex::new(HashMap::new())),
            hits: Arc::new(Mutex::new(HashMap::new())),
            last_headers: Arc::new(Mutex::new(HashMap::new())),
            last_request_body: Arc::new(Mutex::new(HashMap::new())),
        }
    }
}

#[derive(Debug, Clone)]
pub struct MockServer {
    base_url: String,
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
        }
    }
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
