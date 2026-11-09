# Research: proxy `POST /v1/systemone` (TypeSafe SystemOne API) over Copilot decision models

Captured 2026-11-09 (UTC) against live `https://api.githubcopilot.com` with a throwaway harness
that reuses `CopilotAuth` + `base_copilot_request_headers` (no tokens recorded). ~200 upstream
probes. Raw sanitized cases: `reports/fixtures/copilot_decisions_multi.json` (97 cases),
`reports/fixtures/copilot_systemone_multi.json` (80 cases). Prior single-question fixtures:
`copilot_decisions.json`, `copilot_systemone.json`.

## TL;DR

* **One upstream call handles many questions on both upstreams.** `/v1/decisions` accepts up to
  **400** questions; `/v1/systemone` up to **255**. 5/40/100-question requests all returned
  200 with one answer per question.
* **Both upstreams natively support `noul`/`predicate`, `choice` and `score`.** You don't need to
  emulate a choice as one predicate per criterion. Neither upstream supports TypeSafe `box`.
* **`microsoft-decision-1` already speaks the TypeSafe wire format.** Requests and responses are
  byte-compatible with `@typesafe-ai/sdk` `SystemOneRequest`/`SystemOneResult`, so the proxy can
  pass them through and only needs to validate the body and rewrite `model`.
* **`gpt-6-luna-decisions` needs a translation layer.** The proxy converts a keyed object to an
  array, `noul` to `predicate`, `criteria` to `choices`/`levels`, and `state` to `input`, then
  reshapes the answers back. Both upstreams use the same `confidence` formulas as TypeSafe
  (checked numerically).
* **New edge: decisions may return a per-question `{"type":"refusal","name":…}` (HTTP 200)**
  while still answering the other questions. TypeSafe has no refusal type, so the proxy needs a
  policy for it.
* **Limits differ.** decisions allows 400 questions, a 5 MiB body and no token cap was hit
  (617k input tokens accepted). systemone allows 255 questions, a 1 MiB body (413) and
  64,000 tokens (422).
* **The proxy sanitizes upstream error bodies down to a class string.** Most systemone errors
  reach clients as `Copilot request failed with HTTP 4xx`, so the new route should validate
  locally.
* **The SDK zstd-compresses bodies of 1 KiB or more by default.** The proxy already decodes
  zstd/gzip in `src/request_body.rs`.
* **Each upstream attempt costs one rate-limiter token.** The existing chat decision path makes
  exactly one upstream POST per request, confirmed live in logs on port 18080.

## 1. Upstream shapes with many questions

### `/v1/decisions` (`gpt-6-luna-decisions`)

Request (`questions` is an **array**; each item has a `name`):
```json
{"model":"gpt-6-luna-decisions",
 "input":"Customer: I was charged twice ... I want a refund now",
 "questions":[
  {"type":"predicate","name":"is_billing","instructions":"Is this about billing?"},
  {"type":"choice","name":"intent","instructions":"What does the customer want?",
   "choices":[{"value":"refund","description":"money back"},{"value":"replacement"},{"value":"info"}]},
  {"type":"score","name":"severity","instructions":"How severe?",
   "levels":[{"label":"low"},{"label":"medium"},{"label":"high"}]}]}
```
Response. `answers` is an array in **request order**, probabilities are rounded to **2 decimals**,
and `model` is the **base** name `gpt-6-luna`:
```json
{"model":"gpt-6-luna",
 "answers":[
  {"type":"predicate","name":"is_billing","probability":1.0},
  {"type":"choice","name":"intent","choice":"refund","confidence":1.0,
   "probabilities":[{"value":"refund","probability":1.0},{"value":"replacement","probability":0.0},{"value":"info","probability":0.0}]},
  {"type":"score","name":"severity","score":1.33,"confidence":0.47,
   "probabilities":[{"value":0,"label":"low","probability":0.01},{"value":1,"label":"medium","probability":0.65},{"value":2,"label":"high","probability":0.34}]},
  {"type":"refusal","name":"pick"}],
 "usage":{"input_tokens":695,"input_tokens_details":{"cached_tokens":0,"cache_write_tokens":0},
          "output_tokens":0,"output_tokens_details":{"reasoning_tokens":0},"total_tokens":695}}
```
* `input` accepts a string or an array of `{role, content}` messages. Roles `system`,
  `developer`, `user` and `assistant` are accepted, and `content` can be a string or parts. A JSON
  object, an array of strings, or an item without `role` returns 400.
* Usage grows by about 150 input tokens per question: 40 questions used 6,196, 100 used 15,442
  and 400 used 61,672. `output_tokens` is always 0.
* A refusal is per question (case `refusal_mixed`): the other answers are still returned with
  HTTP 200. It appeared for vague instructions (`"Pick"` with options `opt0..opt9`) and for empty
  `input`.

### `/v1/systemone` (`microsoft-decision-1`)

Request (`questions` is an **object keyed by name**):
```json
{"model":"microsoft-decision-1","state":"Customer: I was charged twice ...",
 "questions":{
  "is_billing":{"type":"noul","instructions":"Is this about billing?"},
  "intent":{"type":"choice","instructions":"What does the customer want?",
            "criteria":{"refund":"money back","replacement":null,"info":null}},
  "severity":{"type":"score","instructions":"How severe?","criteria":["low","medium","high"]}}}
```
Response. Values are full-precision floats, `answers` is keyed by name, and `model` echoes the
request:
```json
{"model":"microsoft-decision-1",
 "answers":{
  "is_billing":{"type":"noul","noul":0.9399137519606044},
  "intent":{"type":"choice","choice":"refund","confidence":0.9981902957612758,
            "probabilities":{"refund":0.9987935305075172,"replacement":0.0009107817946347388,"info":0.00029568769784808167}},
  "severity":{"type":"score","score":1.452773941868312,"confidence":0.28812506152039075,
              "legend":{"0":"low","1":"medium","2":"high"},
              "probabilities":{"0":0.010904675225713744,"1":0.5254167076802605,"2":0.4636786170940257}}},
 "usage":{"input_tokens":139,"output_tokens":5}}
```
* Usage costs about 25 input tokens per question plus **1 output token per question**: 100
  questions used 2,535 in and 100 out.
* `state` can be any JSON value: a string, an object, an array of strings, or a messages array.
  A missing, `null` or `""` state was still accepted (HTTP 200), although the SDK requires it.
* Unknown extra top-level fields are ignored by both upstreams.

Answer fields:

| Type | decisions answer fields | systemone answer fields |
|---|---|---|
| yes/no | `type:"predicate"`, `name`, `probability` | `type:"noul"`, `noul` |
| choice | `type`, `name`, `choice`, `confidence`, `probabilities:[{value,probability}]` | `type`, `choice`, `confidence`, `probabilities:{label:p}` |
| score | `type`, `name`, `score` (expected value), `confidence`, `probabilities:[{value:int,label,probability}]` | `type`, `score`, `confidence`, `legend:{"i":criterion}`, `probabilities:{"i":p}` |
| refusal | `type:"refusal"`, `name` (no probability) | not observed |
| usage | `input_tokens`, `input_tokens_details{cached_tokens,cache_write_tokens}`, `output_tokens`, `output_tokens_details{reasoning_tokens}`, `total_tokens` | `input_tokens`, `output_tokens` |

`confidence` on both upstreams matches the TypeSafe formulas:
* Choice: `(pmax − 1/n)/(1 − 1/n)`. For example, decisions with 100 options gave
  `pmax` 0.33, which yields 0.32.
* Score: `max(0, 1 − Σpᵢ|i−m| / MAD_unif)`. For example, (0.01, 0.65, 0.34) yields 0.475.

## 2. Question-type support

| Question type and shape | `/v1/decisions` | `/v1/systemone` |
|---|---|---|
| `predicate` (name, instructions) | ✅ | ❌ 400 `unsupported_request_argument` "invalid question x" |
| `noul` (instructions) | ❌ 400 `invalid_value` "Supported values are: 'predicate', 'choice', and 'score'" | ✅ |
| noul with `criteria:{true,false}` | ❌ (`criteria` is an unknown parameter) | ✅, including criteria with no instructions |
| noul without instructions or criteria | ❌ instructions required | ✅ 200, returns a prior of about 0.41 (the SDK rejects this client-side) |
| instructions as a JSON object | ❌ `invalid_type` (string or array of objects only) | ✅ |
| `choice` | ✅ `choices:[{value, description?:string}]` | ✅ `criteria:{label: description\|null\|JSON}` (a map) |
| choice criteria as a list | n/a | ❌ "requires 2-255 options" |
| choice without instructions | ❌ instructions required | ✅ |
| choice description `null` or an object | ❌ `invalid_type` (string only; omit the key instead) | ✅ |
| choice with 1 option, or 256 or more | ❌ `array_below_min_length` / `array_above_max_length` (2–255) | ❌ "requires 2-255 options" |
| duplicate choice values | ❌ "Choice values must be unique" | n/a (map keys) |
| `score` | ✅ `levels:[{label:string, description?:string}]` | ✅ `criteria:[EntryType…]` (string, object or `null`) |
| score with string levels | ❌ `invalid_type` (objects required) | ✅ |
| score with a level missing `label` | ❌ `missing_required_parameter` (`""` is accepted) | n/a |
| score with 1 level, or 11 or more | ❌ (2–10) | ❌ "requires 2-10 levels" |
| `box` (text state) | ❌ `invalid_value` | ❌ "invalid question x" |

## 3. Limits and error shapes

| Limit | `/v1/decisions` | `/v1/systemone` |
|---|---|---|
| Max questions | **400** (401 → 400 `array_above_max_length`) | **255** (256 → **422** `{"detail":"invalid TypeSafe request: request has 256 questions; the limit is 255"}`) |
| Question name | Non-empty and unique; any characters (space, `.`, `/`, `-`, unicode, leading digit); ≤ 1,048,576 chars (`string_above_max_length`); costs no tokens | Non-empty object key; any characters; 1,024 chars accepted (counts toward tokens) |
| Empty, duplicate or missing name | `400 text/plain "Bad Request\n"` | `""` → 400 "invalid question " |
| Instructions | Required string (`""` is accepted); 1 MiB worked | Optional; `""` → 422 "value must not be empty" |
| Token cap | None observed (4 MiB input = 616,959 tokens → 200) | **64,000** for state plus all questions → 422 `{"detail":"…about 65547 tokens; the limit for the state plus all questions is 64000"}`. Docs also list a 32k per-question limit. |
| Body size | **5,242,880 bytes** exactly (+1 → 400 `invalid_request_body` "not valid JSON", served as `text/plain`) | **1,048,576 bytes** → **413** `{"detail":"request body too large: limit is 1048576 bytes"}` |
| Choice options / score levels | 2–255 / 2–10 | 2–255 / 2–10 |

Error shapes:

| Case | `/v1/decisions` | `/v1/systemone` |
|---|---|---|
| Empty `questions` (`[]`/`{}`) or missing | `400 text/plain "Bad Request\n"` | `400 text/plain "Bad Request\n"` |
| Unknown type | 400 `{"error":{"code":"invalid_value","param":"questions[0].type",…}}` | 400 `{"error":{"code":"unsupported_request_argument","message":"…invalid question x","details":…}}` |
| Missing `type` | `"Bad Request\n"` | 400 `unsupported_request_argument` |
| Missing instructions | 400 `missing_required_parameter` `questions[0].instructions` | 200 (accepted) |
| Wrong decision model for the endpoint | 400 `{"error":{"code":"no_available_model_endpoints",…}}` | same |
| Chat model (`gpt-4o`) | 400 `model_not_supported` "Model is not supported for this request." | same |
| Unknown model | 400 `model_not_supported` "The requested model is not supported." | same |
| Missing `model` | n/a | `400 "Bad Request\n"` |
| Invalid JSON | 400 `{"error":{"code":"invalid_request_body",…}}` | same |

There are three systemone error families: gateway `text/plain`, OpenAI-style `{"error":…}`
(`unsupported_request_argument`) and FastAPI-style `{"detail":…}` (413/422). The proxy's
`sanitize_upstream_error` (`src/copilot/client.rs`) only keeps an allow-listed set of classes:
`model_not_supported`, `no_available_model_endpoints`, `missing_required_parameter`,
`invalid_value` and a few others. **`unsupported_request_argument`, `array_above_max_length`,
`invalid_type` and every `{"detail"}` message are dropped**, so clients only see
`Copilot request failed with HTTP 422`.

## 4. TypeSafe SystemOne public API (`@typesafe-ai/sdk@0.7.0`)

Sources:
* `npm view @typesafe-ai/sdk` and the tarball `dist/index.d.mts` and `dist/index.mjs`:
  https://registry.npmjs.org/@typesafe-ai/sdk/-/sdk-0.7.0.tgz (MIT, published by GitHub Actions)
* Docs: https://docs.typesafe.ai/sdk/javascript.md, https://docs.typesafe.ai/primitives.md,
  https://docs.typesafe.ai/primitives/choice.md, https://docs.typesafe.ai/primitives/score.md,
  https://docs.typesafe.ai/primitives/noul.md, https://docs.typesafe.ai/confidence.md and
  https://docs.typesafe.ai/models.md

Request and response types (from `index.d.mts`):
```ts
type EntryType = string | Asset | TextWithAssets | {[k: string]: ContentValue} | ContentValue[] | null;
interface NoulQuestion   { type: "noul";   instructions?: EntryType; criteria?: { true?: EntryType; false?: EntryType } | null; }
type ChoiceCriteria = { [label: string]: EntryType /* description or null */ };          // map, ≥1 label (SDK); API needs 2–255
interface ChoiceQuestion { type: "choice"; instructions?: EntryType; criteria: ChoiceCriteria; }
type ScoreCriteria = readonly [EntryType, EntryType, ...EntryType[]];                      // list indexed from 0, ≥2 (docs: ≤10)
interface ScoreQuestion  { type: "score";  instructions?: EntryType; criteria: ScoreCriteria; }
interface BoxQuestion    { type: "box";    instructions?: EntryType; }                     // jev-1.14 + image only
interface SystemOneRequest { state: NonNullable<EntryType>; questions: { [name: string]: Question }; model?: string; assets?: Record<string, Asset>; }

interface NoulResponse   { type: "noul";   noul: number; }                                 // P(yes); no confidence
interface ChoiceResponse { type: "choice"; choice: string; confidence: number; probabilities: { [label: string]: number }; }
interface ScoreResponse  { type: "score";  score: number; confidence: number; legend: { [i: string]: EntryType }; probabilities: { [i: string]: number }; }
interface BoxResponse    { type: "box";    box: { top: number; bottom: number; left: number; right: number }; confidence: number | null; }
interface Usage          { input_tokens: number | null; output_tokens: number | null; }
interface SystemOneResult { model: string; answers: { [name: string]: ResultFor<Q> }; usage: Usage; }
```

Wire conventions:
* The SDK sends `POST {baseURL}/v1/systemone`. `baseURL` comes from the `TYPESAFE_BASE_URL`
  environment variable, defaulting to `https://api.typesafe.ai`. `GET /v1/models` returns
  `{models:[{name,description,release_date}]}`.
* Auth is `Authorization: Bearer $TYPESAFE_API_KEY`. The SDK also sends
  `User-Agent: typesafe-sdk/0.7.0`, `X-TypeSafe-SDK`, `X-TypeSafe-Runtime`, and
  `X-TypeSafe-Retry-Count` on retries. It reads the `x-typesafe-request-id` response header.
* The default model is `jev-latest` (`TYPESAFE_DEFAULT_MODEL`). The SDK always sets `model`, so a
  proxy client must pass `model: "microsoft-decision-1"` or `"gpt-6-luna-decisions"`.
* **`Content-Encoding: zstd` is the default for bodies of 1 KiB or more** on Node 22.15+
  (`compression: false` disables it).
* When `assets` (images) are present, the body is **`multipart/form-data`**: a `request` JSON part
  plus `asset-N` parts. Images are experimental and need `TypeSafe-Danger-Zone-Images: true`.
  Long context needs `TypeSafe-Danger-Zone-Long-Context: true`.
* The SDK validates client-side: a non-null `state`, at least one question, a non-empty choice
  map, at least 2 score criteria given as a list, and noul instructions or a described outcome.
  It retries 408, 429 and 5xx (honoring `Retry-After`), uses a 10 s default timeout, and raises
  `APIError(status, body)`.
* Per the docs, there is no limit on question count (only context limits). A choice can have up
  to 255 options and a score up to 10 levels.

## 5. TypeSafe → upstream mapping

| TypeSafe field | `microsoft-decision-1` (`/v1/systemone`) | `gpt-6-luna-decisions` (`/v1/decisions`) |
|---|---|---|
| `model` | Resolve the public ID, then the Copilot ID (identical) | Same |
| `state` (string) | Pass through | `input` = string |
| `state` (messages `[{role,content}]`) | Pass through | `input` = array (roles system/developer/user/assistant) |
| `state` (object, array of strings, number) | Pass through | `input` = JSON-serialized string (object and string arrays are rejected) |
| `questions{name:q}` | Pass through | Array `[{name, …}]`. Order is free: serde_json sorts keys (no `preserve_order`), and answers are re-keyed by name. |
| noul `instructions` (string) | Pass through | `predicate.instructions` |
| noul `instructions` (JSON) | Pass through | JSON-serialized string |
| noul `criteria{true,false}` | Pass through | **Fold into the instructions text** (e.g. `… Yes means: <true>. No means: <false>.`). If instructions are missing, build them from the criteria. If both are missing, reject. |
| choice `criteria{label:desc}` | Pass through | `choices:[{value:label, description?}]`. Omit `description` when it is `null`, and JSON-stringify non-string descriptions. |
| choice without instructions | Pass through | Supply a default such as `"Which option best fits?"` |
| score `criteria[i]` | Pass through | `levels[i] = {label}`: use a string as-is, JSON-stringify an object, and use `"{i}"` (or `""`) for `null` |
| `box` | Reject (400) | Reject (400) |
| answer noul | Pass through | `{type:"noul", noul: probability}` |
| answer choice | Pass through | `probabilities` array → `{value: p}` map. `choice` and `confidence` copy over. |
| answer score | Pass through | `probabilities[{value:i,…}]` → `{"i": p}`. **`legend` is built by the proxy** from the request criteria (`"i": criteria[i]`). `score` and `confidence` copy over. |
| answer refusal | n/a | No TypeSafe equivalent; see the recommendations |
| `usage` | Pass through `{input_tokens, output_tokens}` | `{input_tokens, output_tokens}` taken from the decisions usage |
| response `model` | Public model ID | Public model ID (upstream returns `gpt-6-luna`) |

**Emulation:** neither model needs the predicate-per-criterion emulation. Both upstreams answer
`choice` and `score` natively in the same single call, with matching confidence formulas. The
only content emulation is folding noul `criteria` into decisions instructions. The existing chat
path's predicate-per-option plus `normalize` approach stays chat-only. On decisions the native
`choice` gives true probabilities instead. Decisions answers are rounded to 2 decimal places,
which is a fidelity loss to document.

## 6. Rate limiting and upstream call count

* `TokenBucket` (`src/copilot/client.rs`, `copilot_max_rate`, default 120/min, disabled when
  0) calls `acquire()` **at the top of every attempt** in the `post_body` loop. `post_json`
  (used by `post_decisions` and `post_systemone`) costs 1 token, and **each retry** after a
  transport error, 429 or 5xx (`retryable_status`: 429/500/502/503/504, up to
  `copilot_retry_max`) costs another. `get_json` (the `/models` refresh) and `stream_request`
  also take one token each.
* The chat path (`handle_copilot_decisions` → `build_upstream_request` → `post_decision`) makes
  **exactly 1 upstream POST per client request**, even for an `Option A/B/C` question, because
  all options go as N predicates or nouls in one body. This was verified on a proxy at
  `127.0.0.1:18080` with `RUST_LOG=info`: one `copilot request started api.family="decisions"`
  and one `…="systemone"` for two chat requests, plus a single `models` refresh for the
  model-registry cache. The new route should likewise cost 1 token per request (plus retries).

## Recommendations for the implementation

1. Add `POST /v1/systemone` in `src/http/routes.rs`. Parse the body with
   `parse_json_request_body_with_limit` so the SDK's default `Content-Encoding: zstd` (and gzip)
   works. Reject `multipart/form-data` (assets) with a 400 for now.
2. Resolve `model` through `ModelRegistry`, and accept only the two decision models. A chat or
   unknown model gets 400 `model_not_supported`, and a local model gets 400 with no fallback.
   Echo the **public** model ID.
3. **Validate locally before calling upstream**, using the per-model limits from section 3. On
   systemone that means at most 255 questions, choice 2–255, score 2–10, non-empty instructions
   when present, and a 1 MiB serialized body. On decisions it means at most 400 questions and a
   5 MiB body. Unknown types and `box` are rejected. The reasons: upstream detail is sanitized
   away, and decisions returns opaque `"Bad Request"` responses. Use the TypeSafe-style 400 body
   with a precise message. Optionally add `unsupported_request_argument` and
   `array_above_max_length` to the `sanitize_upstream_error` allow-list. Do not forward the raw
   `detail` text.
4. For `microsoft-decision-1`, forward the validated body nearly unchanged and pass the response
   through after replacing `model`.
5. For `gpt-6-luna-decisions`, translate as in section 5, sending a single `post_decisions` call.
   Re-key the answers by name, build `legend` from the request, and convert the probability
   arrays to maps.
6. **Refusal policy.** Decisions can refuse a single question with HTTP 200. Don't fabricate a
   0.5 or uniform answer. Fail the request with a clear error (for example 422 listing the
   refused question names), matching the "no silent fallback" convention. If partial results are
   wanted later, add an explicit opt-in.
7. Keep usage logging as it is, through `telemetry::summarize_usage`, which already handles both
   shapes. Never log `state` or instructions.
8. Add focused tests using the new fixtures: a mixed 5-question round trip per model, refusal,
   limit validation (256/401 questions, 11 levels, 256 options), zstd body, and box rejection.
