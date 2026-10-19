# Model router (`POST /v1/models`)

Send a task description; get back which listed model fits best. The candidate
list is exactly what `GET /v1/models` returns (enriched, exclusions applied),
rebuilt on every request. Ranking comes from the TypeSafe JEV API.

```sh
curl -s localhost:8080/v1/models -H 'content-type: application/json' \
  -d '{"input":"Refactor a large Rust codebase and fix failing tests"}'
```

Response: `recommended` (`id`, `description`, `excels_at`), `confidence` (0-1),
`ranking` (top 5 `id` + `probability`), `jev_model`.

- Body: `{"input": "<text>"}`; empty or over 16 KiB gives 400.
- Key: `TYPESAFE_API_KEY` from the environment, else from `.env.local` in the
  config dir (only that key is read). Missing key gives 503.
- `COPILOT_PROXY_RS_JEV_MODEL` sets the JEV model string (default `jev-preview`, an alias of jev-1.14.0);
  `COPILOT_PROXY_RS_JEV_URL` sets the base URL (default `https://api.typesafe.ai`).
- Upstream failure, timeout (20s), 429 or an unlisted choice gives a generic
  502. Inputs, keys and upstream bodies are never logged.
- Same inbound auth as `GET /v1/models`.

## UI and visual routing (user-specified)

UI, frontend, CSS, layout, design, visual and image tasks go to the latest
listed Opus model (highest version among ids starting `claude-opus-`, resolved
per request). This is a user instruction, not provider documentation: the latest
Opus gets the tags `ui-design` and `visual-work` and the sentence "Preferred for
UI and visual work." in its public `description`, and the router instructions
say so. Other models get no UI claim.
