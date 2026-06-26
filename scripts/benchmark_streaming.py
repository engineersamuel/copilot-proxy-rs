#!/usr/bin/env python3
import argparse
import csv
import json
import re
import sys
import time
import urllib.request
import urllib.error
from statistics import mean
from typing import List, Optional


def format_seconds(value: Optional[float]) -> str:
    if value is None:
        return "n/a"
    return f"{value / 1000.0:.3f}"

DEFAULT_BASE_URL = "http://127.0.0.1:8080"
DEFAULT_ENDPOINT = "/v1/chat/completions"
DEFAULT_RUNS = 3
DEFAULT_TIMEOUT = 180
DEFAULT_MAX_TOKENS = 800
DEFAULT_PROMPT = (
    "Repeat exactly this sentence 80 times, one sentence per line, and do not add any other text: "
    "'The quiet river carried lantern light across the sleeping city while the patient traveler listened to distant bells.'"
)


def approximate_token_count(text: str) -> int:
    return len(re.findall(r"\w+|[^\w\s]", text))


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description="Benchmark streaming model latency and throughput against a local proxy")
    parser.add_argument("--base-url", default=DEFAULT_BASE_URL, help="Proxy base URL")
    parser.add_argument("--endpoint", default=DEFAULT_ENDPOINT, help="Streaming endpoint path")
    parser.add_argument("--runs", type=int, default=DEFAULT_RUNS, help="Number of runs per model")
    parser.add_argument("--timeout", type=int, default=DEFAULT_TIMEOUT, help="Per-request timeout in seconds")
    parser.add_argument("--max-tokens", type=int, default=DEFAULT_MAX_TOKENS, help="Max output tokens to request")
    parser.add_argument("--prompt", default=DEFAULT_PROMPT, help="Prompt to send for each run")
    parser.add_argument("--models", action="append", default=[], help="Model ID to benchmark (repeatable or comma-separated)")
    parser.add_argument("--output-json", help="Optional path for JSON output")
    parser.add_argument("--output-csv", help="Optional path for CSV output")
    parser.add_argument("--output-markdown", help="Optional path for markdown report output")
    return parser.parse_args()


def normalize_models(raw_models: List[str]) -> List[str]:
    normalized: List[str] = []
    for entry in raw_models:
        for part in entry.split(","):
            item = part.strip()
            if item:
                normalized.append(item)
    return normalized


def fetch_models(base_url: str, timeout: int) -> List[str]:
    url = f"{base_url.rstrip('/')}/v1/models"
    request = urllib.request.Request(url, headers={"Accept": "application/json"})
    with urllib.request.urlopen(request, timeout=timeout) as response:
        payload = json.load(response)
    models = payload.get("data", [])
    return [model["id"] for model in models if isinstance(model, dict) and model.get("id")]


def make_request(url: str, model: str, prompt: str, max_tokens: int, timeout: int) -> dict:
    payload = {
        "model": model,
        "messages": [{"role": "user", "content": prompt}],
        "stream": True,
        "max_tokens": max_tokens,
        "temperature": 0.2,
    }
    data = json.dumps(payload).encode("utf-8")
    request = urllib.request.Request(
        url,
        data=data,
        headers={
            "Content-Type": "application/json",
            "Accept": "text/event-stream",
            "Cache-Control": "no-cache",
        },
        method="POST",
    )
    start_time = time.perf_counter()
    first_token_time: Optional[float] = None
    token_count = 0
    content_chunks = []

    try:
        with urllib.request.urlopen(request, timeout=timeout) as response:
            for raw_line in response:
                line = raw_line.decode("utf-8", errors="replace").rstrip("\n")
                if not line:
                    continue
                if line.startswith(":"):
                    continue
                if line.startswith("event:"):
                    continue
                if line.startswith("data:"):
                    payload_text = line[5:].strip()
                    if payload_text == "[DONE]":
                        break
                    if not payload_text:
                        continue
                    try:
                        event = json.loads(payload_text)
                    except json.JSONDecodeError:
                        continue
                    choices = event.get("choices", [])
                    if not choices:
                        continue
                    delta = choices[0].get("delta", {})
                    content = delta.get("content")
                    if isinstance(content, str) and content:
                        if first_token_time is None:
                            first_token_time = time.perf_counter()
                        token_count += approximate_token_count(content)
                        content_chunks.append(content)
    except urllib.error.HTTPError as exc:
        detail = exc.read().decode("utf-8", errors="replace")
        return {
            "ok": False,
            "error": f"HTTP {exc.code}: {detail}",
            "duration_seconds": time.perf_counter() - start_time,
        }
    except Exception as exc:
        return {
            "ok": False,
            "error": str(exc),
            "duration_seconds": time.perf_counter() - start_time,
        }

    end_time = time.perf_counter()
    duration = max(end_time - start_time, 1e-9)
    if first_token_time is None:
        return {
            "ok": False,
            "error": "No content emitted",
            "duration_seconds": duration,
            "content": "".join(content_chunks),
        }

    ttft_ms = (first_token_time - start_time) * 1000.0
    throughput_tps = (token_count / max((end_time - first_token_time), 1e-9)) if token_count else 0.0
    return {
        "ok": True,
        "duration_seconds": duration,
        "ttft_ms": ttft_ms,
        "ttft_seconds": ttft_ms / 1000.0,
        "throughput_tps": throughput_tps,
        "generated_tokens": token_count,
        "content": "".join(content_chunks),
    }


def benchmark_model(base_url: str, endpoint: str, model: str, prompt: str, max_tokens: int, timeout: int, runs: int) -> dict:
    url = f"{base_url.rstrip('/')}{endpoint}"
    results = []
    for run_index in range(runs):
        result = make_request(url, model, prompt, max_tokens, timeout)
        result["run"] = run_index + 1
        results.append(result)

    successful = [r for r in results if r.get("ok")]
    if not successful:
        return {
            "model": model,
            "runs": results,
            "status": "failed",
            "error": results[0].get("error", "unknown failure") if results else "no runs executed",
        }

    return {
        "model": model,
        "runs": results,
        "status": "ok",
        "ttft_ms_avg": mean(item["ttft_ms"] for item in successful),
        "ttft_seconds_avg": mean(item["ttft_seconds"] for item in successful),
        "throughput_tps_avg": mean(item["throughput_tps"] for item in successful),
        "generated_tokens_avg": mean(item["generated_tokens"] for item in successful),
        "successes": len(successful),
        "failures": len(results) - len(successful),
    }


def write_json(path: str, payload: dict) -> None:
    with open(path, "w", encoding="utf-8") as handle:
        json.dump(payload, handle, indent=2)
        handle.write("\n")


def write_csv(path: str, payload: list[dict]) -> None:
    with open(path, "w", encoding="utf-8", newline="") as handle:
        writer = csv.DictWriter(handle, fieldnames=["model", "status", "ttft_ms_avg", "throughput_tps_avg", "generated_tokens_avg", "successes", "failures"])
        writer.writeheader()
        for row in payload:
            writer.writerow(row)


def print_table(results: List[dict]) -> None:
    header = f"{'model':<30} {'status':<8} {'ttft_s_avg':>12} {'tps_avg':>12} {'tokens_avg':>12}"
    print(header)
    print("-" * len(header))
    for result in results:
        status = result.get("status", "unknown")
        ttft = f"{result.get('ttft_seconds_avg', float('nan')):.3f}" if result.get("ttft_seconds_avg") is not None else "n/a"
        tps = f"{result.get('throughput_tps_avg', float('nan')):.2f}" if result.get("throughput_tps_avg") is not None else "n/a"
        tokens = f"{result.get('generated_tokens_avg', float('nan')):.2f}" if result.get("generated_tokens_avg") is not None else "n/a"
        print(f"{result['model']:<30} {status:<8} {ttft:>12} {tps:>12} {tokens:>12}")


def render_markdown(results: List[dict], base_url: str, endpoint: str, runs: int) -> str:
    rows = []
    for result in results:
        status = result.get("status", "unknown")
        ttft = result.get("ttft_seconds_avg")
        tps = result.get("throughput_tps_avg")
        tokens = result.get("generated_tokens_avg")
        tps_text = f"{tps:.2f}" if tps is not None else "n/a"
        tokens_text = f"{tokens:.2f}" if tokens is not None else "n/a"
        rows.append(
            f"| {result['model']} | {status} | {format_seconds(ttft)} | {tps_text} | {tokens_text} |"
        )

    summary_lines = [
        "# Streaming performance report",
        "",
        f"This report captures streaming benchmark results against the local proxy at `{base_url}`.",
        "",
        "## Summary",
        "",
        "| Model | Status | TTFT (s) | TPS | Avg generated tokens |",
        "| --- | --- | ---: | ---: | ---: |",
    ]
    summary_lines.extend(rows)
    summary_lines.extend([
        "",
        "## Method",
        "",
        f"- Endpoint: `{endpoint}`",
        "- Streaming: enabled",
        "- Runs per model: {runs}",
        "- Metrics reported:",
        "  - Time to first token (TTFT), in seconds",
        "  - Tokens per second (TPS), approximated from emitted streamed content",
    ])
    return "\n".join(summary_lines) + "\n"


def main() -> int:
    args = parse_args()
    selected_models = normalize_models(args.models)

    try:
        available_models = fetch_models(args.base_url, args.timeout)
    except Exception as exc:
        print(f"Failed to fetch models from {args.base_url}: {exc}", file=sys.stderr)
        return 2

    if selected_models:
        models_to_benchmark = [model for model in selected_models if model in available_models]
        missing = [model for model in selected_models if model not in available_models]
        if missing:
            print(f"Skipping missing models: {', '.join(missing)}", file=sys.stderr)
    else:
        models_to_benchmark = available_models

    if not models_to_benchmark:
        print("No models to benchmark", file=sys.stderr)
        return 2

    results = []
    for model in models_to_benchmark:
        result = benchmark_model(args.base_url, args.endpoint, model, args.prompt, args.max_tokens, args.timeout, args.runs)
        results.append(result)

    print_table(results)

    if args.output_json:
        write_json(args.output_json, {"base_url": args.base_url, "endpoint": args.endpoint, "runs": args.runs, "results": results})
    if args.output_csv:
        csv_rows = []
        for result in results:
            csv_rows.append({
                "model": result["model"],
                "status": result.get("status", "unknown"),
                "ttft_ms_avg": result.get("ttft_ms_avg"),
                "ttft_seconds_avg": result.get("ttft_seconds_avg"),
                "throughput_tps_avg": result.get("throughput_tps_avg"),
                "generated_tokens_avg": result.get("generated_tokens_avg"),
                "successes": result.get("successes"),
                "failures": result.get("failures"),
            })
        write_csv(args.output_csv, csv_rows)
    if args.output_markdown:
        with open(args.output_markdown, "w", encoding="utf-8") as handle:
            handle.write(render_markdown(results, args.base_url, args.endpoint, args.runs))

    return 0


if __name__ == "__main__":
    raise SystemExit(main())
