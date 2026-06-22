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
    #[error("{message}")]
    InvalidJson { message: String },
}

pub fn decode_request_body(
    raw_body: &[u8],
    content_encoding: &str,
) -> Result<Vec<u8>, RequestBodyError> {
    let encoding = content_encoding.trim().to_ascii_lowercase();
    if encoding.contains("gzip") {
        return decompress_gzip(raw_body);
    }
    if encoding.contains("zstd") {
        let mut decoder = zstd::stream::read::Decoder::new(raw_body).map_err(|source| {
            RequestBodyError::InvalidCompressedBody {
                encoding: "zstd".to_string(),
                message: source.to_string(),
            }
        })?;
        let mut decoded = Vec::new();
        decoder.read_to_end(&mut decoded).map_err(|source| {
            RequestBodyError::InvalidCompressedBody {
                encoding: "zstd".to_string(),
                message: source.to_string(),
            }
        })?;
        return Ok(decoded);
    }
    if encoding.is_empty() || encoding == "identity" {
        return Ok(raw_body.to_vec());
    }
    Err(RequestBodyError::UnsupportedContentEncoding { encoding })
}

pub fn parse_json_request_body(
    raw_body: &[u8],
    content_encoding: &str,
) -> Result<Map<String, Value>, RequestBodyError> {
    let decoded = decode_request_body(raw_body, content_encoding)?;
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

fn decompress_gzip(raw_body: &[u8]) -> Result<Vec<u8>, RequestBodyError> {
    let mut decoder = GzDecoder::new(raw_body);
    let mut decoded = Vec::new();
    decoder.read_to_end(&mut decoded).map_err(|source| {
        RequestBodyError::InvalidCompressedBody {
            encoding: "gzip".to_string(),
            message: source.to_string(),
        }
    })?;
    Ok(decoded)
}
