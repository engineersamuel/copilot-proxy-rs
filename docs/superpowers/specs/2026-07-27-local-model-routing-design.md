# Configured Local Model Routing Design

## Objective

Allow operators to add an OpenAI-compatible model hosted outside the proxy to
the existing model catalog and select it by a stable public ID. The first
configured model is `qwen3-coder-30b-local`, served by llama.cpp over Tailscale.
It must support chat completions and Responses HTTP requests without changing
existing Copilot behavior.

## Requirements

- WHEN an operator defines a local model in `config.json`, THE SYSTEM SHALL add
  its public ID to both views returned by `/v1/models`.
- WHEN a client requests `qwen3-coder-30b-local`, THE SYSTEM SHALL send the
  request to `http://100.98.223.125:8080/v1` with the exact upstream model ID
  `models\Qwen3-Coder-30B-A3B-Instruct-IQ4_XS.gguf`.
- WHEN the local model is used through `/v1/chat/completions`, THE SYSTEM SHALL
  support non-streaming and SSE streaming requests.
- WHEN the local model is used through `POST /v1/responses`, THE SYSTEM SHALL
  translate non-streaming and SSE streaming requests, tool calls, tool results,
  responses, and usage data to and from chat completions.
- WHEN a response is returned to the client, THE SYSTEM SHALL expose the public
  model ID rather than the upstream llama.cpp model path.
- IF the local endpoint is unavailable, THE SYSTEM SHALL return a clear proxy
  error and SHALL NOT retry the request through Copilot.
- Existing Copilot discovery, aliases, authentication, metadata refresh, and
  request routing SHALL remain unchanged for non-local model IDs.

## Configuration

Local models are keyed by their public client-facing IDs:

```json
{
  "local_models": {
    "qwen3-coder-30b-local": {
      "base_url": "http://100.98.223.125:8080/v1",
      "upstream_model": "models\\Qwen3-Coder-30B-A3B-Instruct-IQ4_XS.gguf"
    }
  }
}
```

`local_models` defaults to an empty map. Each public ID, base URL, and upstream
model ID must be non-empty. The base URL must use `http` or `https`; HTTP is
allowed because Tailscale supplies the private transport in this deployment.
The proxy appends `/chat/completions` after trimming a trailing slash. The
initial version adds no outbound authorization header because this llama.cpp
server does not require one.

Configured models are config-authoritative. They appear in `/v1/models`
without a startup health probe, so proxy readiness does not depend on the
remote computer. A configured local public ID takes precedence over a Copilot
model with the same ID.

## Architecture

### Configuration and model catalog

`AppConfig` gains a map of validated local model definitions. `ModelRegistry`
receives those definitions at startup and merges local entries with static and
live Copilot entries when building `/v1/models`.

Local rich catalog entries have `source: "local"`, advertise
`/chat/completions` and `/responses`, and avoid inventing context-window or
reasoning metadata. Catalog merging remains deterministic and de-duplicates by
public ID, with configured local entries winning collisions.

### Target resolution

Model resolution returns an explicit target rather than only a rewritten model
string:

```text
ModelTarget
├── Copilot { model_id }
└── Local { public_id, base_url, upstream_model }
```

Request handlers check configured local IDs before refreshing Copilot metadata.
This prevents a local request from waiting on GitHub. Non-local requests follow
the current refresh, alias, capability, and Copilot routing path.

### Local backend

A focused local backend owns outbound HTTP to configured OpenAI-compatible
servers. It replaces the public model ID with the upstream model ID, forwards
chat requests, and returns buffered JSON or a byte stream. It does not use
Copilot tokens, Copilot headers, capability fallback, or Copilot retry logic.

The endpoint is selected only from validated operator configuration. Request
input cannot override the destination URL. Logs retain the current safe
metadata policy and do not record prompts, tokens, or full endpoint URLs.

### Responses adapter

A separate, pure translation module converts between the Responses and chat
completions wire formats. It is independent of HTTP transport so mappings can
be exhaustively unit tested.

Request translation covers:

- `instructions` and text or structured `input` items;
- system, user, assistant, and tool-result messages;
- function tool definitions and tool choice;
- function calls and function-call outputs across turns;
- output-token, temperature, top-p, and parallel-tool-call controls; and
- the upstream model substitution.

Responses-only controls with no chat-completions equivalent are removed only
when they are advisory metadata. Unsupported hosted tools or input item types
fail with an OpenAI-compatible `400` error rather than being silently dropped.
The existing previous-response transcript expansion runs before translation.

Non-streaming response translation produces a normal Responses object with a
proxy-generated response ID, `output_text` message content or function-call
items, finish status, and mapped usage counts. The public model ID is restored
in the final object.

Streaming translation is a stateful adapter over llama.cpp chat SSE chunks. It
emits the standard Responses lifecycle, output-item, text-delta or
function-argument-delta, completion, and terminal events in order. It preserves
usage when llama.cpp supplies the final usage chunk and terminates with
`[DONE]`.

## Request flow

### Chat completions

1. Parse and validate the inbound request using the existing body limits.
2. Resolve the requested public model ID.
3. For a local target, replace `model` with `upstream_model` and send the body
   to `{base_url}/chat/completions`.
4. Buffer JSON or relay SSE while applying only local response normalization.
5. Restore `qwen3-coder-30b-local` in client-visible response metadata.

### Responses

1. Parse the request and expand `previous_response_id` with the existing local
   transcript state.
2. Resolve the public model ID before any Copilot refresh.
3. Translate the Responses request to chat completions and call the local
   backend.
4. Translate the JSON response or SSE stream back to Responses format.
5. Cache completed response state needed by later transcript expansion.

Provider-side retrieval, cancellation, and Responses WebSocket transport are
not added for local models. They must return an explicit unsupported response
for a local response instead of forwarding it to Copilot. Existing Copilot
behavior for those routes remains unchanged.

## Error handling

- Invalid local model configuration fails during configuration loading with the
  public model ID and invalid field named in the error.
- DNS, connection, or transport failures return `502 Bad Gateway` in the
  route's OpenAI-compatible error shape.
- Timeouts return `504 Gateway Timeout`.
- OpenAI-compatible HTTP errors returned by llama.cpp preserve their status and
  error details; non-JSON error bodies are converted to a bounded safe message.
- Unsupported Responses constructs return `400 Bad Request` before contacting
  llama.cpp.
- A local error never triggers Copilot fallback or changes the effective model.

## Testing

Implementation follows red-green-refactor. Tests cover:

- empty defaults and JSON parsing for the `local_models` configuration;
- validation of IDs, base URLs, and upstream model IDs;
- deterministic `/v1/models` merging, local metadata, and collision precedence;
- exact public-to-upstream model resolution without a Copilot refresh;
- non-streaming and streaming chat routing with public ID restoration;
- Responses request translation for text, prior items, tools, calls, and tool
  results;
- Responses JSON and SSE translation, event ordering, function arguments,
  completion status, and usage;
- unsupported input handling and upstream connection, timeout, and HTTP errors;
- proof that local failures never issue a Copilot request; and
- regression coverage for existing Copilot chat, Responses, model discovery,
  and aliases.

After automated verification, an isolated live proxy instance will query
`/v1/models`, send a non-streaming hello-world request through both supported
routes, exercise streaming, and run a tool-call round trip against the supplied
Tailscale llama.cpp endpoint.

## Alternatives considered

### Backend-wide local mode

Making the entire proxy use one local backend would require clients to choose
between Copilot and local models at process startup. It does not satisfy the
requirement to expose both in one available-model catalog.

### General provider plugin framework

A provider trait and arbitrary endpoint plugins would support broader future
use cases, but they add configuration and dispatch abstractions not required by
this change. The explicit `ModelTarget` boundary leaves room for that later
without building it now.

## Scope boundaries

This change does not add automatic model discovery from local servers, outbound
API-key configuration, load balancing, failover, model health polling,
Responses WebSocket translation, or local implementations of Copilot-only
hosted tools. It does not modify the semantics of any existing Copilot model.
