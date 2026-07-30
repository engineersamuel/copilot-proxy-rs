---
description: Run the repository's required Rust verification checks
---

Run these checks from the repository root, stopping at the first failure:

1. `cargo fmt --check`
2. `cargo clippy --all-targets -- -D warnings`
3. `cargo test --locked`

Report the failing command and its actionable error. Do not weaken checks or tests.
