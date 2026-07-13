# Changelog

## Unreleased

- Prepare the repository for safe alpha publication.
- Add public license, security, contribution, Docker, Compose, and CI guidance.
- Fail closed for non-loopback binds unless explicitly opted in.
- Sanitize raw upstream Copilot error bodies before returning client errors.
- Honor the configured request-body limit at Axum's buffering layer and add
  actionable 413 responses plus content-safe request-size diagnostics.
