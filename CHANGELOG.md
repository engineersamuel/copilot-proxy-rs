# Changelog

## Unreleased

- Raise the default `copilot_max_rate` from 15 to 120 requests per minute;
  environment/file overrides and `0` to disable rate limiting are unchanged.
- Decision models answer prompts that name options (`Option A: ...`, numbered
  lists) with the favored option, normalized per-option probabilities and a
  reason line; a `gpt-6-luna-decisions` refusal now returns 400 with guidance
  instead of a 502 "malformed" error.
- Decision options tied at two decimals are reported as a tie that defaults to
  the first listed option; an answer scoring every option 0 returns the same
  400 as a refusal instead of an equal split.
- Prepare the repository for safe alpha publication.
- Add public license, security, contribution, Docker, Compose, and CI guidance.
- Fail closed for non-loopback binds unless explicitly opted in.
- Sanitize raw upstream Copilot error bodies before returning client errors.
- Honor the configured request-body limit at Axum's buffering layer and add
  actionable 413 responses plus content-safe request-size diagnostics.
- Normalize Chat Completions tool-call indices during Responses streaming so
  tools after text remain valid while unknown-index deltas still fail closed.
- Normalize Responses output indices to dense tool-local indices when streaming
  Chat Completions; preserve parallel calls and reject unknown argument indices.
