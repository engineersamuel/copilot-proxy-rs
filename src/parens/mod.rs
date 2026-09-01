//! Matching-parenthesis scanning over raw bytes.
// Temporary: the public entry point and its callers land in a follow-up task in
// this feature. Remove this attribute when the public dispatcher is wired in.
#![allow(dead_code)]

pub(crate) mod scalar;
