use axum::Router;
use axum::middleware;
use axum::routing::{get, post};

use crate::http::chat::chat_completions;
use crate::http::health::{count_tokens, debug_copilot_models, health, list_models, version};
use crate::http::messages::messages;
use crate::http::responses::{
    responses, responses_cancel, responses_compact, responses_retrieve, responses_ws,
};
use crate::state::AppState;

pub fn router(state: AppState) -> Router {
    // Keep the Copilot-backed routes behind inbound auth middleware.
    let protected_routes = Router::new()
        .route("/debug/copilot/models", get(debug_copilot_models))
        .route("/v1/models", get(list_models))
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
        ));

    Router::new()
        .route("/health", get(health))
        .route("/version", get(version))
        .merge(protected_routes)
        .with_state(state)
}
