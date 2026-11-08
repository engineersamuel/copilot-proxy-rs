//! Pass-through routes for Copilot image generation and editing.

use axum::Json;
use axum::body::Bytes;
use axum::extract::State;
use axum::extract::rejection::BytesRejection;
use axum::response::{IntoResponse, Response};
use http::{HeaderMap, StatusCode, header};
use serde_json::{Map, Value};

use crate::errors::{OpenAiErrorResponse, openai_error};
use crate::http::errors::{
    openai_copilot_error, request_body_error_details, request_body_rejection_details,
};
use crate::request_body::parse_json_request_body_with_limit;
use crate::state::AppState;

#[derive(Debug, Clone, Copy)]
enum ImageRoute {
    Generations,
    Edits,
}

impl ImageRoute {
    fn path(self) -> &'static str {
        match self {
            Self::Generations => "/v1/images/generations",
            Self::Edits => "/v1/images/edits",
        }
    }
}

type ImageError = (StatusCode, Json<OpenAiErrorResponse>);

fn bad_request(message: impl Into<String>) -> ImageError {
    openai_error(StatusCode::BAD_REQUEST, "invalid_request_error", message)
}

fn header_text(headers: &HeaderMap, name: header::HeaderName) -> Option<&str> {
    headers.get(name).and_then(|value| value.to_str().ok())
}

fn content_encoding(headers: &HeaderMap) -> &str {
    header_text(headers, header::CONTENT_ENCODING).unwrap_or("identity")
}

fn reject_local_model(state: &AppState, model: &str, route: ImageRoute) -> Result<(), ImageError> {
    match state.models.configured_local_target(model) {
        Some(_) => Err(bad_request(format!(
            "Local models do not support {}",
            route.path()
        ))),
        None => Ok(()),
    }
}

async fn copilot_model_for(state: &AppState, model: &str) -> String {
    state.copilot.refresh_models_if_stale().await;
    state.models.get_copilot_openai_model(model).await
}

fn read_body(
    state: &AppState,
    headers: &HeaderMap,
    body: Result<Bytes, BytesRejection>,
) -> Result<Bytes, ImageError> {
    body.map_err(|rejection| {
        let (status, message) = request_body_rejection_details(
            rejection,
            headers,
            state.config.max_decoded_body_bytes,
            "images",
        );
        openai_error(status, "invalid_request_error", message)
    })
}

fn upstream_response(result: Result<Value, crate::copilot::errors::CopilotError>) -> Response {
    match result {
        Ok(value) => Json(value).into_response(),
        Err(error) => openai_copilot_error(error).into_response(),
    }
}

pub(crate) async fn images_generations(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Result<Bytes, BytesRejection>,
) -> Response {
    let body = match read_body(&state, &headers, body) {
        Ok(body) => body,
        Err(error) => return error.into_response(),
    };
    json_image_request(&state, &headers, &body, ImageRoute::Generations)
        .await
        .unwrap_or_else(IntoResponse::into_response)
}

pub(crate) async fn images_edits(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Result<Bytes, BytesRejection>,
) -> Response {
    let body = match read_body(&state, &headers, body) {
        Ok(body) => body,
        Err(error) => return error.into_response(),
    };
    let content_type = header_text(&headers, header::CONTENT_TYPE).unwrap_or_default();
    let media_type = content_type
        .split(';')
        .next()
        .unwrap_or_default()
        .trim()
        .to_ascii_lowercase();
    let result = match media_type.as_str() {
        "application/json" => json_image_request(&state, &headers, &body, ImageRoute::Edits).await,
        "multipart/form-data" => multipart_edit_request(&state, &headers, content_type, body).await,
        _ => Err(openai_error(
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            "invalid_request_error",
            "Content-Type must be application/json or multipart/form-data",
        )),
    };
    result.unwrap_or_else(IntoResponse::into_response)
}

async fn json_image_request(
    state: &AppState,
    headers: &HeaderMap,
    body: &[u8],
    route: ImageRoute,
) -> Result<Response, ImageError> {
    let mut body: Map<String, Value> = parse_json_request_body_with_limit(
        body,
        content_encoding(headers),
        state.config.max_decoded_body_bytes as usize,
    )
    .map_err(|err| {
        let (status, message) = request_body_error_details(&err);
        openai_error(status, "invalid_request_error", message)
    })?;
    let model = body
        .get("model")
        .and_then(Value::as_str)
        .ok_or_else(|| bad_request("model is required"))?
        .to_string();
    reject_local_model(state, &model, route)?;
    if body.get("stream").and_then(Value::as_bool) == Some(true) {
        return Err(bad_request("streaming image generation is not supported"));
    }
    let copilot_model = copilot_model_for(state, &model).await;
    body.insert("model".to_string(), Value::String(copilot_model));
    let result = match route {
        ImageRoute::Generations => state.copilot.post_images_generations(body).await,
        ImageRoute::Edits => state.copilot.post_images_edits(body).await,
    };
    Ok(upstream_response(result))
}

/// Forwards a multipart edit unmodified; only the `model` part is read, so
/// model aliases are not rewritten in the forwarded bytes.
async fn multipart_edit_request(
    state: &AppState,
    headers: &HeaderMap,
    content_type: &str,
    body: Bytes,
) -> Result<Response, ImageError> {
    let boundary = multipart_boundary(content_type)
        .ok_or_else(|| bad_request("multipart/form-data requires a boundary"))?;
    if !content_encoding(headers).eq_ignore_ascii_case("identity") {
        return Err(bad_request(
            "Content-Encoding is not supported for multipart image edits",
        ));
    }
    let model = multipart_text_field(&body, boundary, "model")
        .ok_or_else(|| bad_request("model is required"))?;
    reject_local_model(state, &model, ImageRoute::Edits)?;
    let copilot_model = copilot_model_for(state, &model).await;
    let result = state
        .copilot
        .post_images_edits_raw(content_type.to_string(), body, Some(copilot_model))
        .await;
    Ok(upstream_response(result))
}

fn multipart_boundary(content_type: &str) -> Option<&str> {
    content_type
        .split(';')
        .filter_map(|param| param.split_once('='))
        .find(|(key, _)| key.trim().eq_ignore_ascii_case("boundary"))
        .map(|(_, value)| value.trim().trim_matches('"'))
        .filter(|value| !value.is_empty())
}

fn find(haystack: &[u8], needle: &[u8], from: usize) -> Option<usize> {
    haystack
        .get(from..)?
        .windows(needle.len())
        .position(|window| window == needle)
        .map(|index| index + from)
}

/// Reads one text part from a multipart body without decoding file parts.
fn multipart_text_field(bytes: &[u8], boundary: &str, name: &str) -> Option<String> {
    let delimiter = format!("--{boundary}\r\n");
    let wanted = format!("name=\"{name}\"");
    let mut cursor = 0;
    while let Some(start) = find(bytes, delimiter.as_bytes(), cursor) {
        let headers_start = start + delimiter.len();
        let headers_end = find(bytes, b"\r\n\r\n", headers_start)?;
        let value_start = headers_end + 4;
        let value_end = find(bytes, format!("\r\n--{boundary}").as_bytes(), value_start)?;
        let headers = String::from_utf8_lossy(&bytes[headers_start..headers_end]);
        let is_wanted = headers.lines().any(|line| {
            line.to_ascii_lowercase()
                .starts_with("content-disposition:")
                && line.split(';').any(|param| param.trim() == wanted)
        });
        if is_wanted {
            return String::from_utf8(bytes[value_start..value_end].to_vec())
                .ok()
                .filter(|value| !value.is_empty());
        }
        cursor = value_end;
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    const BODY: &[u8] = b"--b1\r\nContent-Disposition: form-data; name=\"image\"; filename=\"model\"\r\n\r\n\xff\x00\r\n--b1\r\ncontent-disposition: form-data; name=\"model\"\r\n\r\ngpt-image\r\n--b1--\r\n";

    #[test]
    fn multipart_text_field_matches_name_not_filename() {
        assert_eq!(
            multipart_text_field(BODY, "b1", "model").as_deref(),
            Some("gpt-image")
        );
        assert_eq!(multipart_text_field(BODY, "b1", "prompt"), None);
        assert_eq!(multipart_text_field(BODY, "other", "model"), None);
        assert_eq!(
            multipart_text_field(BODY, "b1", "image").as_deref(),
            None,
            "file bytes are not UTF-8 text"
        );
        let first =
            b"--b1\r\nContent-Disposition: form-data; name=\"model\"\r\n\r\nm\r\n--b1--\r\n";
        assert_eq!(
            multipart_text_field(first, "b1", "model").as_deref(),
            Some("m")
        );
    }

    #[test]
    fn multipart_boundary_reads_quoted_parameter() {
        assert_eq!(
            multipart_boundary("multipart/form-data; Boundary=\"x y\""),
            Some("x y")
        );
        assert_eq!(multipart_boundary("multipart/form-data"), None);
        assert_eq!(multipart_boundary("multipart/form-data; boundary="), None);
    }
}
