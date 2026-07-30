# Agent Guide

## Project

`copilot-proxy-rs` is a Rust proxy for GitHub Copilot-compatible chat, messages, and responses APIs.
The crate uses Rust 2024 and supports Rust 1.85 or newer.
The HTTP server is built with Axum and Tokio.

## Architecture

`src/main.rs` is the executable entry point.
`src/http/` owns routes for chat, messages, responses, models, health, and diagnostics.
`src/copilot/` owns GitHub Copilot authentication and upstream transport.
`src/local/` owns configured OpenAI-compatible local model transport and translation.
`src/models.rs` owns model metadata and target resolution.
`src/config.rs` owns environment and file-backed configuration.
Integration and contract tests live under `tests/`.

## Checks

Run `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`, and `cargo test --locked` before submitting changes.
Keep behavior changes covered by focused tests.
Keep changes narrow and preserve public API compatibility unless explicitly required.

## Conventions

Use existing error, routing, and translation patterns before adding new abstractions.
Local model failures must not silently fall back to Copilot.
Preserve public model IDs when translating requests and responses.

## Safety

Do not log raw tokens, prompts, response bodies, or upstream diagnostics.
Treat authentication, token handling, WebSockets, bind addresses, and upstream errors as security-sensitive.
