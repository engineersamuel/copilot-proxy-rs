# Streaming performance report

This report captures streaming benchmark results against the local proxy at `http://127.0.0.1:8080`.

## Summary

| Model | Status | TTFT (s) | TPS | Avg generated tokens |
| --- | --- | ---: | ---: | ---: |
| claude-opus-4.6 | ok | 2.438 | 135.29 | 1520.00 |
| claude-opus-4.7 | ok | 5.507 | 111.28 | 1660.00 |
| claude-opus-4.8 | ok | 5.526 | 93.56 | 1537.33 |
| claude-sonnet-4.6 | ok | 8.166 | 148.31 | 1520.33 |
| gemini-3.1-pro-preview | ok | 4.921 | 168.17 | 1520.00 |
| gemini-3.5-flash | ok | 6.601 | 271.25 | 1520.00 |
| gpt-5.3-codex | ok | 3.476 | 110.81 | 1539.00 |
| gpt-5.4-mini | ok | 8.612 | 154.14 | 1444.00 |
| gpt-5.4 | ok | 1.076 | 112.01 | 1520.00 |
| gpt-5.5 | ok | 1.628 | 175.22 | 1520.00 |
| mai-code-1-flash-internal | ok | 3.973 | 144.18 | 2774.00 |

## Method
- Endpoint: `/v1/chat/completions`
- Streaming: enabled
- Prompt: a deterministic, token-heavy prompt designed to produce enough streamed content to measure
- Runs per model: 3
- Metrics reported:
  - Time to first token (TTFT), in seconds
  - Tokens per second (TPS), approximated from emitted streamed content

## Quick takeaways
- Fastest TTFT in this run: `gpt-5.4` (1.076 s)
- Highest TPS in this run: `gemini-3.5-flash` (271.25)
- Slowest TTFT in this run: `gpt-5.4-mini` (8.612 s)
