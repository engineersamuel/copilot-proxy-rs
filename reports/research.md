# Research: decisions / image models in copilot-proxy-rs

Captured 2026-11-08 (UTC) against live `https://api.githubcopilot.com` using the proxy's
own auth flow. No tokens are recorded here; `Authorization` is always redacted, base64 is
truncated. Fixtures: `reports/fixtures/`.

## TL;DR

* **The two "decisions" models are classifiers, not text generators.** They take
  `input`/`state` **plus a mandatory list of `questions`** and return **probabilities**, never
  text. A plain chat body (`messages` only) is rejected with an opaque `400 text/plain "Bad Request"`.
  To serve chat/messages/responses we must *synthesize* a question (recommended: one
  predicate whose instructions are the last user message) and *render* the answers as text.
* `gpt-6-luna-decisions` -> `POST /v1/decisions` (OpenAI "Decisions" format: `input` +
  `questions: [ {type,name,instructions,...} ]`, answers is an **array**).
* `microsoft-decision-1` -> `POST /v1/systemone` (TypeSafe "SystemOne" format: `state` +
  `questions: { name: {type,instructions,...} }`, answers is an **object keyed by name**).
  The two endpoints are **not interchangeable** (wrong model -> `400 no_available_model_endpoints`).
* Neither decisions endpoint streams: `stream:true` is silently ignored (200 JSON). Both return
  `usage`.
* `gpt-image-2.5-flare` works on `POST /v1/images/generations` (JSON) and
  `POST /v1/images/edits` (**both** JSON `images:[{image_url}]` **and** multipart `image=@file`).
  Always returns `data[].b64_json` (~1 MB per 1024x1024 PNG); `response_format:"url"` is ignored;
  `stream:true` -> `400 streaming image generation is not supported`. Returns `usage` and a
  Copilot-specific `copilot_usage` block.
* Required headers are exactly the proxy's existing `base_copilot_request_headers` + `X-Initiator`;
  same base `https://api.githubcopilot.com` (paths include the `/v1` prefix; without it -> 404).
* Model type is already in the cached upstream metadata (`capabilities.type`,
  `supported_endpoints`) held by `ModelRegistry`; only `supported_endpoints` is parsed today;
  `capabilities.type` is not parsed anywhere.

---

## 1. Code map

### Request flow

| Concern | Location |
|---|---|
| Router | `src/http/routes.rs::router` - protected routes behind `crate::http::auth::require_inbound_auth`, `DefaultBodyLimit::max(encoded_body_limit(max_decoded_body_bytes))` (default 16 MiB decoded). |
| Chat | `src/http/chat.rs::chat_completions` -> `chat_completions_inner` (validate -> local target? -> `refresh_models_if_stale` -> `get_copilot_openai_model` -> choose `/responses` vs `/chat/completions` from `supported_endpoints`). |
| Messages | `src/http/messages.rs::messages` -> `messages_inner` (web-search -> `/v1/messages` -> `/responses` -> chat fallback `handle_copilot_chat_messages`). |
| Responses | `src/http/responses.rs::responses` (`prepare_responses_request` -> chat fallback `handle_copilot_chat_responses` if no `/responses` but has `/chat/completions` -> stream/non-stream `/responses`). Also WS: `responses_ws` / `handle_responses_ws`. |
| Models list | `src/http/health.rs::list_models` -> `build_models_value` -> `ModelRegistry::list_for_snapshot` -> `exclude_models` -> `merge_model_insights` -> `mark_latest_opus_for_ui`. Same value feeds `POST /v1/models` (`src/http/model_router.rs`). |
| Debug | `src/http/health.rs::debug_copilot_models` -> `ModelRegistry::debug_snapshot` (sanitized raw `upstream_models` + rich `models`). |
| Upstream transport | `src/copilot/client.rs::CopilotBackend` - `post_json` / `stream_request` / `get_json` (rate limiter, 429/5xx retry, sanitized errors). Public wrappers: `post_chat`, `stream_chat`, `post_messages`, `stream_messages`, `post_responses`, `stream_responses`, `get_response`, `cancel_response`, `refresh_models_if_stale[_result]`. |
| Upstream URLs | `CopilotEndpoints { chat_url, messages_url, responses_url, responses_ws_url, models_url }` - **full URLs**, `Default` = `https://api.githubcopilot.com/...`. Tests build it as a struct literal in `tests/support/mod.rs::MockServer::copilot_endpoints`, `tests/copilot_client_contract.rs` (x3), `tests/responses_ws_contract.rs`. |
| Headers | `src/copilot/request.rs::base_copilot_request_headers(token)` (Authorization Bearer, `Copilot-Integration-Id: vscode-chat`, `Editor-Version: vscode/1.100.0`, `Editor-Plugin-Version: copilot-chat/0.27.2025040201`, `User-Agent: GithubCopilot/1.155.0`, `Accept: application/json`, `X-GitHub-Api-Version: 2026-06-01`) + `CopilotBackend::headers` adds `Content-Type: application/json`, `X-Initiator`, optional metadata headers. |
| Auth | `src/auth.rs::CopilotAuth` - GitHub token from `GITHUB_TOKEN` env, else `<COPILOT_PROXY_RS_CONFIG_DIR or ~/.config/copilot-proxy-rs>/github_token`, else device flow (interactive only). Exchanged at `https://api.github.com/copilot_internal/v2/token` (`Authorization: token <gh>`), cached until `expires_at-120s`, single-flight refresh. |
| Errors | `src/http/errors.rs::openai_copilot_error` / `anthropic_copilot_error` map `CopilotError::Http` to `(status, "server_error", detail)`. `client.rs::sanitize_upstream_error` **drops upstream bodies** except allow-listed classes (`model_not_supported`, ...) -> `"Copilot request failed with HTTP 400: model_not_supported"`. `endpoint_family(url)` labels logs (`/v1/decisions`, `/v1/images/...` currently fall to `"copilot"`). |
| SSE utils | `src/http/sse.rs` (`map_sse_lines`, `map_sse_lines_many[_checked]`, `inspect_sse_lines`). |
| Config | `src/config.rs::AppConfig` (`COPILOT_PROXY_RS_PORT/HOST`, `max_decoded_body_bytes`, `copilot_models_ttl`, retry/timeouts, `model_excludes`, `local_models`, overrides). |

### Model metadata cache (`src/models.rs`)

* `CopilotBackend::refresh_models_if_stale_result` GETs `models_url` and stores raw `data[]` via
  `ModelRegistry::set_copilot_models` into `ModelRegistryInner.models: Vec<serde_json::Value>`
  (TTL `copilot_models_ttl`). **Raw JSON is kept**, so `capabilities.type` is available without
  re-fetching.
* Parsed today: `supported_endpoints` (`dynamic_supported_endpoints`, private
  `ModelRegistry::supported_endpoints` with static fallback `STATIC_COPILOT_MODELS`),
  `capabilities.supports.reasoning_effort`, context windows. **`capabilities.type` is not parsed
  anywhere.**
* Predicates: `model_supports_responses_api` (`"/responses"`), `model_supports_chat_completions_api`
  (`"/chat/completions"`), `model_supports_responses_ws`, `model_supports_messages_api`
  (`"/v1/messages"`, Claude default when empty). Lookups use `canonical_model_id` (strips
  `github-copilot/`, applies `COPILOT_MODEL_ALIASES`).
* `GET /v1/models` `data[]` entries are `ModelEntry { id, object, created, owned_by }`
  (`copilot_models`, `dynamic_model_entry`, `static_model_entry`, `local_model_entry`); the
  Codex-style `models[]` (`CodexModelEntry`) **already** carries `supported_endpoints`
  (`["/v1/decisions"]`, etc. — verified live) but no `type`. `merge_model_insights` adds optional
  `description`, `excels_at`, `curated`, ... to `data[]` with `entry(key).or_insert`.

### Existing translation patterns to reuse (canonical pivot = Responses JSON/SSE)

| From -> To | Function |
|---|---|
| chat req -> Responses req | `translate::responses_formats::openai_chat_to_responses_request` |
| Responses resp -> chat resp | `responses_formats::responses_to_openai_chat_response(&resp, public_model)` |
| Responses SSE -> chat SSE | `responses_formats::ResponsesToChatStream::new(model).map_line(line)` |
| Anthropic req -> Responses req | `responses_formats::anthropic_messages_to_responses_request` |
| Responses resp -> Anthropic resp | `responses_formats::responses_to_anthropic_message_response(&resp, model)` |
| Responses SSE -> Anthropic SSE | `responses_formats::responses_sse_to_anthropic_sse_line` (maps `response.created` -> `message_start`, deltas, completed -> `message_delta`/`message_stop`) |
| completed Responses obj -> Responses SSE events | `http::responses::completed_response_events(&response)` (**private**; emits `output_item.added`, `content_part.added`, `output_text.delta`, `.done`s, `response.completed`). Opening events come from `local::ChatToResponsesStream::begin_events()` (see `streaming_web_search_response`). |
| Responses req -> chat req | `local::responses_to_chat` |
| chat resp -> Responses resp | `local::responses::chat_to_responses_with_tool_names` |

Note: `src/http/model_router.rs` already talks to a SystemOne-style API (TypeSafe JEV:
`answers.pick.choice`, `criteria`) — useful prior art for serde types.

### Tests

* `tests/support/mod.rs::MockServer` (axum catch-all; `respond_json`, `respond_text`,
  `respond_sse`, `hits`, `last_request_body_json`, `last_request_header`), `AppFixture::with_mock_copilot()`
  (token at `GET /copilot/token`, temp config dir with `github_token`), `backend_fixture`.
* Models are injected with `mock.respond_json("GET", "/models", 200, json!({"data":[...]}))` or
  `state.models.set_copilot_models(...)`.
* Contract files: `tests/chat_completions_contract.rs`, `tests/messages_contract.rs`,
  `tests/responses_contract.rs`, `tests/http_contract.rs` (models listing, e.g. lines ~600-700,
  ~1140-1170), `tests/copilot_client_contract.rs`. Fixtures dir: `tests/fixtures/`.

### Current behaviour (baseline, proxy on :18080)

All three models via chat / messages / responses -> `400`
`{"error":{"message":"Copilot request failed with HTTP 400: model_not_supported","type":"server_error",...}}`
(Anthropic: `{"type":"error","error":{"type":"server_error",...}}`). `POST /v1/images/generations`
-> `404`. `GET /v1/models` lists `gpt-6-luna-decisions`, `microsoft-decision-1`
(`owned_by:"other"`), `gpt-image-2.5-flare`.

Upstream `capabilities.type` values seen: `chat`, `completion` (`gpt-41-copilot`), `decisions`,
`embeddings` (`text-embedding-*`), `image`. Many legacy chat models have **no**
`supported_endpoints` (e.g. `gpt-4o`).

---

## 2. Upstream endpoint shapes (live)

Common: base `https://api.githubcopilot.com`, headers = proxy's `base_copilot_request_headers`
+ `Content-Type: application/json` + `X-Initiator: user`. The token-exchange response advertises
`endpoints.api = https://api.enterprise.githubcopilot.com`; it behaves identically, the proxy's
fixed base works. Unknown paths -> `404 "404 page not found"`; `GET /v1/decisions` -> `405`.
Note upstream sometimes sends JSON error bodies with `Content-Type: text/plain`.

### 2.1 `POST /v1/decisions` (`gpt-6-luna-decisions`) — fixture `copilot_decisions.json`

Request:
```json
{"model":"gpt-6-luna-decisions",
 "input":[{"role":"system","content":"You are helpful"},
          {"role":"user","content":"Is the sky blue on a clear day?"}],
 "questions":[{"type":"predicate","name":"answer","instructions":"Is the sky blue on a clear day?"}]}
```
Response `200 application/json` (~0.4 s):
```json
{"model":"gpt-6-luna",
 "answers":[{"type":"predicate","name":"answer","probability":0.99}],
 "usage":{"input_tokens":173,"input_tokens_details":{"cached_tokens":0,"cache_write_tokens":0},
          "output_tokens":0,"output_tokens_details":{"reasoning_tokens":0},"total_tokens":173}}
```
* `model` in the response is the **internal family** (`gpt-6-luna`), not the requested id -> must
  be rewritten to the public id.
* `input`: string, or array of `{role, content}` where content is a string or
  `[{"type":"input_text","text":...}]`; roles `system`/`user`/`assistant` accepted. `input_image`
  parts are accepted but appear ignored (probability 0.01 for "is there an image?").
* `questions[]` (required, non-empty, each needs `name` + `instructions`):
  * `predicate` -> `{"type":"predicate","name","probability"}`
  * `choice` + `choices:[{"value":..., "description"?}]` -> `{"type":"choice","name","choice":"refund","probabilities":[{"value","probability"}],"confidence"}`
  * `score` + `levels:[{"label", "description"?}]` -> `{"type":"score","name","score":1.49,"probabilities":[{"value":0,"label","probability"}],"confidence"}`
* Unknown fields (`temperature`, `max_tokens`, `stream`) are ignored. **`stream:true` returns normal JSON.**
* Errors:
  * no/empty `questions`, chat-shape body, missing `name`, `{}` -> `400 text/plain "Bad Request"` (no detail)
  * invalid JSON -> `400 {"error":{"message":"The request body is not valid JSON.","code":"invalid_request_body"}}`
  * missing input -> `400 {"error":{"message":"Missing required parameter: 'input'.","type":"invalid_request_error","param":"input","code":"missing_required_parameter"}}`
  * missing instructions -> same shape, `param:"questions[0].instructions"`
  * bad type -> `{"error":{"message":"Invalid value: 'text'. Supported values are: 'predicate', 'choice', and 'score'.","code":"invalid_value",...}}`
  * `choices:["a"]` -> `invalid_type` (`expected an object`); score without levels -> `missing_required_parameter questions[0].levels`; level without label -> `missing ... levels[0].label`
  * `microsoft-decision-1` here -> `400 {"error":{"message":"no model endpoints available given user constraints","code":"no_available_model_endpoints"}}`
  * chat model here -> `400 model_not_supported`

### 2.2 `POST /v1/systemone` (`microsoft-decision-1`) — fixture `copilot_systemone.json`

Request:
```json
{"model":"microsoft-decision-1",
 "state":[{"role":"system","content":"You are helpful"},{"role":"user","content":"Is the sky blue on a clear day?"}],
 "questions":{"answer":{"type":"noul","instructions":"Is the sky blue on a clear day?"}}}
```
Response `200 application/json`:
```json
{"model":"microsoft-decision-1",
 "answers":{"answer":{"noul":0.9940889323800837,"type":"noul"}},
 "usage":{"input_tokens":41,"output_tokens":1}}
```
* `state`: string or `[{role, content:string}]` (system/assistant accepted). **`state` is optional** (omitted -> still 200).
* `questions` is an **object keyed by name** (array -> `400 invalid_request_body "not valid JSON"`):
  * `noul` -> `{"noul":p,"type":"noul"}`
  * `choice` + `criteria:{"refund":"desc","other":null}` -> `{"choice":"refund","confidence":0.94,"probabilities":{"other":0.03,"refund":0.97},"type":"choice"}`
  * `score` + `criteria:[{"label":"negative"},"neutral","positive"]` -> `{"score":1.03,"confidence":0.90,"legend":{"0":{"label":"negative"},"1":"neutral","2":"positive"},"probabilities":{"0":0.02,"1":0.93,"2":0.05},"type":"score"}`
* `stream:true` ignored (JSON). Unknown fields ignored.
* Errors: no/empty questions or chat shape -> `400 text/plain "Bad Request"`; `type:"predicate"` ->
  `400 {"error":{"code":"unsupported_request_argument","message":"Model does not support request argument supplied: invalid question x","details":"..."}}`;
  decisions model here -> `no_available_model_endpoints`; chat model -> `model_not_supported`.
* Billing metadata restricts this model to business/enterprise plans (works for this account).

### 2.3 `POST /v1/images/generations` (`gpt-image-2.5-flare`) — fixture `copilot_images_generations.json`

Request: `{"model":"gpt-image-2.5-flare","prompt":"a red circle on white","n":1,"size":"1024x1024"}`

Response `200 application/json`, ~6 s, chunked, body ~956 KB:
```json
{"created":1794109009,"background":"opaque",
 "data":[{"b64_json":"iVBORw0KGgoAAAANSUhEUgAABAAAAAQACAIAAADw...<truncated 956464 chars>"}],
 "output_format":"png","quality":"low","size":"1024x1024",
 "usage":{"input_tokens":11,"input_tokens_details":{"image_tokens":0,"text_tokens":11},
          "output_tokens":196,"output_tokens_details":{"image_tokens":196,"text_tokens":0},"total_tokens":207},
 "copilot_usage":{"token_details":[{"batch_size":1000000,"cost_per_batch":500000000000,"model":"gpt-image-2.5-flare","token_count":11,"token_type":"input"}, "..."],"total_nano_aiu":593500000}}
```
* `response_format:"url"` ignored (still `b64_json`). No `revised_prompt`.
* Errors: missing prompt -> `400 text/plain {"error":{"message":"prompt is required","code":""}}`;
  missing model -> `model is required`; `size:"13x13"` -> `400 application/json {"error":{"message":"Invalid size '13x13'. Width and height must both be divisible by 16.","type":"image_generation_user_error","param":"size","code":"invalid_value"}}`;
  `stream:true` -> `400 {"error":{"message":"streaming image generation is not supported","code":""}}`;
  chat model -> `400 model_not_supported`.

### 2.4 `POST /v1/images/edits` — fixture `copilot_images_edits.json`

Both accepted, both `200` with the same response shape as generations (`usage.input_tokens_details.image_tokens:1024`):
* JSON: `{"model":"gpt-image-2.5-flare","prompt":"make the circle blue","size":"1024x1024","images":[{"image_url":"data:image/png;base64,..."}]}`
* multipart/form-data: fields `model`, `prompt`, `size`, file part `image` (`image/png`).
* Errors: no `images` (or singular `image` string in JSON) -> `400 {"error":{"message":"images must contain between 1 and 16 images","code":""}}`; bad data URL -> `400 {"error":{"message":"Invalid image data.","type":"image_generation_user_error","param":"images[0].image_url","code":"invalid_image_file"}}`.

### Usage summary

| Endpoint | usage keys |
|---|---|
| decisions | `input_tokens`, `input_tokens_details.{cached_tokens,cache_write_tokens}`, `output_tokens` (0), `output_tokens_details.reasoning_tokens`, `total_tokens` |
| systemone | `input_tokens`, `output_tokens` (1) — no total |
| images | Responses-style `usage` + `copilot_usage` (billing detail) |

`telemetry::summarize_usage` already reads `input_tokens`/`output_tokens`/`total_tokens`, so
`post_json`'s usage log works unchanged.

---

## 3. Recommended implementation design

### 3.1 Model kind detection (`src/models.rs`)

* Add `pub enum CopilotModelKind { Chat, Decisions, SystemOne, Image, Other(String) }` (or a
  small struct `{ kind: &str, endpoints: Vec<String> }`) and
  `ModelRegistry::copilot_model_kind(&self, model) -> Option<...>` that reads
  `capabilities.type` from the cached raw JSON (via `canonical_model_id`), and the endpoint
  from the existing private `supported_endpoints`:
  * `type=="decisions"` && endpoints contains `/v1/systemone` -> SystemOne
  * `type=="decisions"` && endpoints contains `/v1/decisions` -> Decisions
    (prefer endpoint over model-name heuristics; agentgateway uses names, we have metadata)
  * `type=="image"` -> Image (endpoints for the error message)
* Unknown/not-yet-fetched -> `None` -> existing behaviour (no regression).
* Call it **after** `refresh_models_if_stale()` and model resolution in each handler, using the
  resolved `copilot_model`.

### 3.2 Transport (`src/copilot/client.rs`)

* Extend `CopilotEndpoints` with `decisions_url`, `systemone_url`, `images_generations_url`,
  `images_edits_url` (defaults `https://api.githubcopilot.com/v1/...`). Adding pub fields breaks
  the struct literals in `tests/support/mod.rs`, `tests/copilot_client_contract.rs` (3x),
  `tests/responses_ws_contract.rs` — update them (or add `..CopilotEndpoints::default()` /
  a `with_base_url(base)` helper; `MockServer::copilot_endpoints()` must point the new URLs at the mock).
* New methods: `post_decisions(body)`, `post_systemone(body)` -> `post_json` (non-stream only);
  `post_images_generations(body)` -> `post_json`; `post_images_edits_raw(content_type, bytes)` for
  multipart (new small variant of `post_json` that sends a raw body with the caller's
  `Content-Type` instead of `.json()`; JSON edits can use `post_json`).
* `endpoint_family`: add `"decisions"`, `"systemone"`, `"images"` before the fallthrough.
* `sanitize_upstream_error`: consider allow-listing image/decision validation classes
  (`invalid_value`, `missing_required_parameter`, `image_generation_user_error`,
  `no_available_model_endpoints`, "streaming image generation is not supported"). Keep sanitizing
  by default (AGENTS.md: don't leak upstream diagnostics).
* Body size: image responses ~1 MB per image (b64); `reqwest` `.json()` is unbounded, fine. Edit
  requests with data URLs can approach the 16 MiB inbound limit — document
  `COPILOT_PROXY_RS_MAX_DECODED_BODY_BYTES`.

### 3.3 (a) Decisions via chat / messages / responses — new `src/translate/decisions.rs`

Pure functions (unit-testable, no I/O):

1. **Build request** from a normalized conversation `Vec<(role, text)>`:
   * Extract from chat `messages` (string or `[{type:text}]` content), Anthropic `system`+`messages`
     (text blocks), Responses `instructions`+`input` (string or message items with
     `input_text`/`output_text`). Drop tool calls/images.
   * `question_text` = last user message text (400 `invalid_request_error` if none/empty).
   * Decisions body: `{"model": <copilot id>, "input":[{role,content}...], "questions":[{"type":"predicate","name":"answer","instructions": question_text}]}`
   * SystemOne body: `{"model":..., "state":[{role,content}...], "questions":{"answer":{"type":"noul","instructions": question_text}}}`
   * Do **not** forward `stream`, `temperature`, tools, etc.
   * Optional extension (recommended, cheap): if the client body has a top-level `questions`
     field, forward it as-is (Decisions array for `/v1/decisions`; object for `/v1/systemone`) so
     power users get choice/score. Converting between the two formats (agentgateway's mapping:
     predicate<->noul, `choices[].value/description` <-> `criteria{value:desc}`, `levels[]` <->
     `criteria[]`) is optional.
2. **Render answer text** (deterministic): for the synthesized predicate,
   `p = answers[0].probability` (Decisions) or `answers.answer.noul` (SystemOne) ->
   `"Yes"` if p >= 0.5 else `"No"`, plus ` (probability 0.99)`; for explicit questions emit compact
   JSON of the normalized answers. (Decide wording in implementation; keep it stable for tests.)
3. **Build a canonical Responses object** (the pivot):
   ```json
   {"id":"resp_<uuid>","object":"response","created_at":<now>,"status":"completed","model":"<public id>",
    "output":[{"type":"message","id":"msg_<uuid>","status":"completed","role":"assistant",
               "content":[{"type":"output_text","text":"Yes (probability 0.99)","annotations":[]}]}],
    "usage":{"input_tokens":N,"output_tokens":M,"total_tokens":N+M}}
   ```
   Map usage from either format (systemone lacks `total_tokens`).

Handler hooks (each right after model resolution, before existing routing):

* `chat.rs::chat_completions_inner` -> new `handle_copilot_decisions_chat`: non-stream
  `responses_to_openai_chat_response(&resp, &requested_model)`; stream: generate the Responses SSE
  lines (`response.created` + `completed_response_events`) and feed each `data:` line through
  `ResponsesToChatStream::new(requested_model).map_line`, then `data: [DONE]` — or hand-craft
  three chunks (role+content, `finish_reason:"stop"` + `usage`, `[DONE]`).
* `messages.rs::messages_inner` -> non-stream `responses_to_anthropic_message_response`; stream:
  same Responses SSE lines through `responses_sse_to_anthropic_sse_line` (yields
  `message_start`, `content_block_*`, `message_delta`, `message_stop`).
* `responses.rs::responses` -> non-stream return the object (optionally cache via
  `state.responses.cache_response_state` for `previous_response_id`); stream: opening events
  (`response.created`, `response.in_progress`) + `completed_response_events(&resp)` (make it
  `pub(crate)` or move to `translate::responses_formats`) + `data: [DONE]` if consistent with
  existing streams. Also guard `handle_responses_ws` (decisions over WS -> clear error or same path).
* Errors: map upstream failures with existing `openai_copilot_error` / `anthropic_copilot_error`.
  Local-model routing is untouched (local check happens first).

### 3.4 (b) Image pass-through — new `src/http/images.rs`

* Routes in `routes.rs` inside `protected_routes`: `.route("/v1/images/generations", post(images_generations))`,
  `.route("/v1/images/edits", post(images_edits))`.
* Generations: parse JSON with `parse_json_request_body_with_limit`, require `model`, resolve with
  `get_copilot_openai_model` (preserve public id; the response has no `model` field), reject
  configured local models with 400 (no silent fallback), reject `stream:true` early or let upstream
  400 through. Forward body; return upstream JSON unchanged (consider stripping `copilot_usage`? —
  recommend keep, it's harmless billing info; decide explicitly).
* Edits: branch on `Content-Type`: `application/json` -> same as generations; `multipart/form-data`
  -> forward raw bytes + original `Content-Type` (boundary) via the raw-body method. Model
  resolution for multipart would require parsing; simplest is pass-through without rewriting
  (document), or parse with `axum::extract::Multipart` only to read `model`.
* Optional: if the model is a known non-image model, return 400 naming the supported endpoints.

### 3.5 (c) Clear 400 for chat requests to image models

In the same hook as (a): if kind == Image, return
`400 invalid_request_error`: `"model 'gpt-image-2.5-flare' does not support /v1/chat/completions; use POST /v1/images/generations or /v1/images/edits"`
(build the list from `supported_endpoints`). Anthropic shape for `/v1/messages`. Do **not** call
upstream. Same treatment is reasonable for `embeddings`/`completion` types but out of scope.

### 3.6 (d) `type` and `supported_endpoints` on `GET /v1/models` `data[]`

* Add to `ModelEntry`:
  ```rust
  #[serde(rename = "type", skip_serializing_if = "Option::is_none")] pub model_type: Option<String>,
  #[serde(skip_serializing_if = "Option::is_none")] pub supported_endpoints: Option<Vec<String>>,
  ```
  (`skip_serializing_if` keeps existing JSON stable where unknown; or always emit — pick one and test.)
* `dynamic_model_entry`: `capabilities.type` + `supported_endpoints` (fallback to static list).
  `static_model_entry`: `"chat"` + static endpoints. `local_model_entry`: `"chat"` +
  `LOCAL_MODEL_SUPPORTED_ENDPOINTS`.
* `ModelEntry` is a pub struct constructed only inside `src/models.rs` (no test literals), but
  adding fields is still a public-API change for external constructors — acceptable/required here.
  Existing ids/fields unchanged; `merge_model_insights` uses `or_insert`, so no collision (it never
  sets `type`/`supported_endpoints`).
* Consider (follow-up) filtering non-`chat` types from the `POST /v1/models` router candidates in
  `model_router.rs`.

### 3.7 Tests to add (using fixtures)

* `tests/fixtures/copilot_decisions_*.json`, `copilot_systemone_*.json`, `copilot_images_*.json`
  copied from `reports/fixtures/` (strip to one case each).
* Contract: mock `/models` with `reports/fixtures/copilot_models_nonchat.json` data; mock
  `POST /v1/decisions` / `/v1/systemone` responses; assert for chat/messages/responses
  (stream + non-stream) the text, public model id, usage, and the upstream request body shape
  (`last_request_body_json`) — and that `/chat/completions` / `/responses` were **not** hit.
* Image model on chat -> 400 message naming `/v1/images/generations`, zero upstream hits.
* `/v1/images/generations` + `/v1/images/edits` (JSON and multipart) pass-through.
* `/v1/models` `data[]` has `type` + `supported_endpoints` for the three models and existing fields intact.

---

## 4. Fixtures (`reports/fixtures/`)

| File | Contents |
|---|---|
| `copilot_decisions.json` | 7 success cases (predicate/choice/score/multi/string input/content parts/stream ignored) + 9 error cases |
| `copilot_systemone.json` | 5 success cases (noul/choice/score/string state/stream ignored) + 5 error cases |
| `copilot_images_generations.json` | success (b64 truncated) + 5 error cases |
| `copilot_images_edits.json` | JSON + multipart success (b64 truncated) + 3 error cases |
| `copilot_models_nonchat.json` | upstream `/models` entries for the 3 models plus representative chat/legacy/embeddings/completion entries |

Each case: `{name, request, status, content_type, response}`; files include the redacted
required headers.

## 5. Method notes

* Proxy built from this branch (`cargo build --release`) and run on `127.0.0.1:18080` with an
  isolated `COPILOT_PROXY_RS_CONFIG_DIR` (symlink to the existing `github_token`) so shared caches
  weren't touched; port 8080 instance untouched. Stopped after research.
* Direct probes used a throwaway `/tmp` Python script mirroring `src/auth.rs` + `request.rs`
  headers; tokens never printed or written to the repo.
* Decision schemas were discovered via public agentgateway docs/golden tests
  (`agentgateway/agentgateway crates/llm/src/conversion/systemone.rs`, tests under
  `crates/llm/src/tests/requests/{decisions,systemone}`), then confirmed live.
