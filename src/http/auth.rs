use axum::extract::{Request, State};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use http::HeaderMap;
use http::StatusCode;

use crate::errors::openai_error;
use crate::state::AppState;

pub fn request_has_valid_api_key(headers: &HeaderMap, configured_api_key: &str) -> bool {
    if configured_api_key.is_empty() {
        return true;
    }

    let bearer_matches = headers
        .get(http::header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| {
            let (scheme, token) = value.split_once(' ')?;
            scheme.eq_ignore_ascii_case("Bearer").then_some(token)
        })
        .is_some_and(|token| token == configured_api_key);

    let api_key_matches = headers
        .get("x-api-key")
        .and_then(|value| value.to_str().ok())
        .is_some_and(|token| token == configured_api_key);

    bearer_matches || api_key_matches
}

pub async fn require_inbound_auth(
    State(state): State<AppState>,
    request: Request,
    next: Next,
) -> Response {
    if request_has_valid_api_key(request.headers(), &state.config.api_key) {
        return next.run(request).await;
    }

    openai_error(
        StatusCode::UNAUTHORIZED,
        "authentication_error",
        "Missing or invalid proxy API key",
    )
    .into_response()
}
