use flate2::Compression;
use flate2::write::GzEncoder;
use std::io::Write;

use copilot_proxy_rs::request_body::{
    RequestBodyError, decode_request_body, parse_json_request_body,
};

fn gzip_bytes(input: &[u8]) -> Vec<u8> {
    let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
    encoder.write_all(input).unwrap();
    encoder.finish().unwrap()
}

#[test]
fn identity_returns_raw_body() {
    let payload = br#"{"ok":true}"#;

    assert_eq!(decode_request_body(payload, "identity").unwrap(), payload);
    assert_eq!(decode_request_body(payload, "").unwrap(), payload);
}

#[test]
fn gzip_decodes_body() {
    let payload = gzip_bytes(br#"{"ok":true}"#);

    assert_eq!(
        decode_request_body(&payload, "gzip").unwrap(),
        br#"{"ok":true}"#
    );
}

#[test]
fn zstd_decodes_body() {
    let payload = zstd::bulk::compress(br#"{"ok":true}"#, 0).unwrap();

    assert_eq!(
        decode_request_body(&payload, "zstd").unwrap(),
        br#"{"ok":true}"#
    );
}

#[test]
fn unsupported_encoding_reports_encoding() {
    let err = decode_request_body(b"{}", "br").unwrap_err();

    assert!(
        matches!(err, RequestBodyError::UnsupportedContentEncoding { encoding } if encoding == "br")
    );
}

#[test]
fn invalid_gzip_reports_structured_error() {
    let err = decode_request_body(b"not-gzip", "gzip").unwrap_err();

    assert!(
        matches!(err, RequestBodyError::InvalidCompressedBody { encoding, .. } if encoding == "gzip")
    );
}

#[test]
fn invalid_zstd_reports_structured_error() {
    let err = decode_request_body(b"not-zstd", "zstd").unwrap_err();

    assert!(
        matches!(err, RequestBodyError::InvalidCompressedBody { encoding, .. } if encoding == "zstd")
    );
}

#[test]
fn parses_json_object() {
    let parsed = parse_json_request_body(br#"{"ok":true}"#, "identity").unwrap();

    assert_eq!(
        parsed.get("ok").and_then(|value| value.as_bool()),
        Some(true)
    );
}

#[test]
fn rejects_invalid_json() {
    let err = parse_json_request_body(b"{", "identity").unwrap_err();

    assert!(matches!(err, RequestBodyError::InvalidJson { .. }));
}

#[test]
fn rejects_non_object_json() {
    let err = parse_json_request_body(br#"["not","an","object"]"#, "identity").unwrap_err();

    assert!(
        matches!(err, RequestBodyError::InvalidJson { message } if message == "Top-level JSON body must be an object")
    );
}
