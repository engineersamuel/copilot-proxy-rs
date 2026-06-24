use std::io::Read;

use flate2::read::GzDecoder;
use serde_json::{Map, Value};
use thiserror::Error;

#[derive(Debug, Error, PartialEq, Eq)]
pub enum RequestBodyError {
    #[error("unsupported content-encoding: {encoding}")]
    UnsupportedContentEncoding { encoding: String },
    #[error("invalid {encoding} request body: {message}")]
    InvalidCompressedBody { encoding: String, message: String },
    #[error("decoded request body exceeds {limit} bytes")]
    DecodedBodyTooLarge { limit: usize },
    #[error("{message}")]
    InvalidJson { message: String },
}

pub fn decode_request_body(
    raw_body: &[u8],
    content_encoding: &str,
) -> Result<Vec<u8>, RequestBodyError> {
    decode_request_body_with_limit(raw_body, content_encoding, usize::MAX)
}

pub fn decode_request_body_with_limit(
    raw_body: &[u8],
    content_encoding: &str,
    max_decoded_bytes: usize,
) -> Result<Vec<u8>, RequestBodyError> {
    let encoding = content_encoding.trim().to_ascii_lowercase();
    if encoding.contains("gzip") {
        return decompress_gzip_with_limit(raw_body, max_decoded_bytes);
    }
    if encoding.contains("zstd") {
        let mut decoder = zstd::stream::read::Decoder::new(raw_body).map_err(|source| {
            RequestBodyError::InvalidCompressedBody {
                encoding: "zstd".to_string(),
                message: source.to_string(),
            }
        })?;
        return read_to_limit(&mut decoder, max_decoded_bytes, "zstd");
    }
    if encoding.is_empty() || encoding == "identity" {
        if raw_body.len() > max_decoded_bytes {
            return Err(RequestBodyError::DecodedBodyTooLarge {
                limit: max_decoded_bytes,
            });
        }
        return Ok(raw_body.to_vec());
    }
    Err(RequestBodyError::UnsupportedContentEncoding { encoding })
}

pub fn parse_json_request_body(
    raw_body: &[u8],
    content_encoding: &str,
) -> Result<Map<String, Value>, RequestBodyError> {
    parse_json_request_body_with_limit(raw_body, content_encoding, usize::MAX)
}

pub fn parse_json_request_body_with_limit(
    raw_body: &[u8],
    content_encoding: &str,
    max_decoded_bytes: usize,
) -> Result<Map<String, Value>, RequestBodyError> {
    let decoded = decode_request_body_with_limit(raw_body, content_encoding, max_decoded_bytes)?;
    parse_json_value(decoded)
}

fn parse_json_value(decoded: Vec<u8>) -> Result<Map<String, Value>, RequestBodyError> {
    let value: Value =
        serde_json::from_slice(&decoded).map_err(|source| RequestBodyError::InvalidJson {
            message: source.to_string(),
        })?;
    match value {
        Value::Object(object) => Ok(object),
        _ => Err(RequestBodyError::InvalidJson {
            message: "Top-level JSON body must be an object".to_string(),
        }),
    }
}

fn decompress_gzip_with_limit(
    raw_body: &[u8],
    max_decoded_bytes: usize,
) -> Result<Vec<u8>, RequestBodyError> {
    let mut decoder = GzDecoder::new(raw_body);
    read_to_limit(&mut decoder, max_decoded_bytes, "gzip")
}

fn read_to_limit<R: Read>(
    reader: &mut R,
    max_decoded_bytes: usize,
    encoding: &'static str,
) -> Result<Vec<u8>, RequestBodyError> {
    let mut decoded = Vec::new();
    let limit = max_decoded_bytes.saturating_add(1) as u64;
    reader
        .take(limit)
        .read_to_end(&mut decoded)
        .map_err(|source| RequestBodyError::InvalidCompressedBody {
            encoding: encoding.to_string(),
            message: source.to_string(),
        })?;
    if decoded.len() > max_decoded_bytes {
        return Err(RequestBodyError::DecodedBodyTooLarge {
            limit: max_decoded_bytes,
        });
    }
    Ok(decoded)
}
