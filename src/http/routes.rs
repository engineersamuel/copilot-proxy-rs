use axum::Router;
use axum::extract::{DefaultBodyLimit, MatchedPath, Request};
use axum::handler::Handler as _;
use axum::middleware;
use axum::response::Response;
use axum::routing::{get, post};

use crate::http::chat::chat_completions;
use crate::http::health::{count_tokens, debug_copilot_models, health, list_models, version};
use crate::http::messages::messages;
use crate::http::responses::{
    responses, responses_cancel, responses_compact, responses_retrieve, responses_ws,
};
use crate::request_body::encoded_body_limit;
use crate::state::AppState;

pub fn router(state: AppState) -> Router {
    let max_request_body_bytes = encoded_body_limit(state.config.max_decoded_body_bytes);

    // Keep the Copilot-backed routes behind inbound auth middleware.
    let protected_routes = Router::new()
        .route("/debug/copilot/models", get(debug_copilot_models))
        .route(
            "/v1/models",
            get(list_models).post(crate::http::model_router::route_model.layer(
                DefaultBodyLimit::max(crate::http::model_router::MAX_BODY_BYTES),
            )),
        )
        .route("/v1/messages/count_tokens", post(count_tokens))
        .route("/v1/messages", post(messages))
        .route("/v1/chat/completions", post(chat_completions))
        .route("/v1/responses", post(responses).get(responses_ws))
        .route("/v1/responses/compact", post(responses_compact))
        .route("/v1/responses/{response_id}", get(responses_retrieve))
        .route("/v1/responses/{response_id}/cancel", post(responses_cancel))
        .route_layer(middleware::from_fn_with_state(
            state.clone(),
            crate::http::auth::require_inbound_auth,
        ))
        .layer(DefaultBodyLimit::max(max_request_body_bytes));

    Router::new()
        .route("/health", get(health))
        .route("/version", get(version))
        .merge(protected_routes)
        .layer(middleware::from_fn(log_failed_request))
        .with_state(state)
}

async fn log_failed_request(request: Request, next: middleware::Next) -> Response {
    let route = request
        .extensions()
        .get::<MatchedPath>()
        .map(|path| path.as_str().to_string())
        .unwrap_or_else(|| "unmatched".to_string());
    let response = next.run(request).await;
    if response.status().is_client_error() || response.status().is_server_error() {
        tracing::info!(
            http.route = route,
            http.status_code = response.status().as_u16(),
            "http request failed"
        );
    }
    response
}
