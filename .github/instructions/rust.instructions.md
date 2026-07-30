---
applyTo: "{src,tests}/**/*.rs"
description: "Rust implementation and test rules for the proxy"
---

# Rust Rules

- Keep request routing explicit: configured local model IDs must not enter Copilot auth, refresh, or fallback paths.
- Preserve public model IDs across upstream request and downstream response translation.
- Add focused tests for behavior changes and regression fixes.
- Run `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`, and `cargo test --locked`.
- Never log tokens, prompts, response bodies, or raw upstream diagnostics.
