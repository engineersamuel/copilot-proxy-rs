# Contributing

Thanks for helping improve `copilot-proxy-rs`.

## Development workflow

1. Fork or branch from `main`.
2. Keep changes focused and include tests for behavior changes.
3. Run:

   ```bash
   cargo fmt --check
   cargo clippy --all-targets -- -D warnings
   cargo test
   ```

4. Open a pull request with a clear description, test evidence, and any security
   implications.

## Security-sensitive changes

Changes touching authentication, token handling, logging, WebSocket behavior,
bind addresses, or upstream error handling need extra care. Avoid logging raw
tokens, prompts, response bodies, or upstream diagnostics.
