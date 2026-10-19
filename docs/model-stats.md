# Model stats

Stats come from the Artificial Analysis free API
(`GET /api/v2/language/models/free`, about 100 requests/day). Data credit:
[Artificial Analysis](https://artificialanalysis.ai). `/v1/models` responses
carry `stats_attribution` when stats are present.

## Matching

Order: AA slug, normalized name (parentheticals removed), alias map in
`src/model_stats.rs`, then effort-variant slugs (`<id>-low|medium|high|xhigh|max|reasoning`).
For variants, the one with the highest intelligence index wins. No looser fuzzy
match is used, to avoid wrong stats.

## Descriptions and `excels_at`

Copilot-routed chat models of known current families get a `description`.
Legacy, non-chat (embedding etc.) and unknown-family ids get none (field omitted).

- **Curated** (`curated: true`): text and tags in `CURATED` (`src/model_stats.rs`)
  are taken from the GitHub Copilot model comparison page, "Task area" and
  "Excels at" columns:
  https://docs.github.com/en/copilot/reference/ai-models/model-comparison
  Model list: https://docs.github.com/en/copilot/reference/ai-models/supported-models
  (read 2026-10-19). Covered ids: gpt-5-mini, gpt-5.3-codex, gpt-5.4,
  gpt-5.4-mini, gpt-5.5, gpt-5.6-{luna,sol,terra}, gpt-6-{astra,luna,sol},
  gpt-6.1-sol, claude-fable-5, claude-fable-5.1, claude-haiku-4.5/5.5,
  claude-opus-4.8/5/5.5, claude-sonnet-4.6/5/5.5, gemini-3.7/3.8-flash,
  gemini-4-argon, mai-code-1.1-flash, grok-4.5/4.6/4.7, kimi-k3.
- **Generic** (`curated: false`): only current-generation ids of known families
  (gpt-5/6, kimi, mai-, gemini >= 3, grok >= 4, claude opus/sonnet/haiku/fable
  >= 4) without a curated row get "Frontier model for complex work", or
  "Frontier model for quick and fast work" when the id contains haiku, flash,
  mini, nano, luna, lite or fast. All other ids get no description.
- The earlier "GPT-6.1 Sol strict spec adherence" and "Opus UI design" claims
  were dropped: the Copilot page does not support them.
- Derived tags are added only for curated or current-generation ids (legacy and
  non-chat ids keep stats at most, never tags). Names: `strong-coding-index`,
  `strong-agentic-index`, `high-intelligence` (fixed cut: intelligence >= 45),
  `high-throughput` (>= 150 tok/s), `low-price` (output <= $2/1M).
  Floors for the rest:
  use floors (intelligence >= 25; coding >= 70 and >= 1.9x intelligence;
  agentic >= 45 and >= 1.1x) tuned on the free catalog so `coding` marks about
  the top 20% of models.

## Key and cache

`AA_API_KEY` is read from the environment, else from `AA_API_KEY=` in
`<config dir>/.env.local` (default `~/.config/copilot-proxy-rs/`); no other
line or file is read. Enriched data is cached in
`<config dir>/model-stats-cache.json` (atomic write; a corrupt file is
ignored). Per-model stats are persisted, so a restart does not refetch.

Refetch rule: AA is fetched only when a listed id has never been resolved
against an AA catalog (not in the cache's `resolved` set). There is no
time-based refetch. Removed ids are dropped. When a fetch happens (AA only
offers a full catalog call), only the unresolved ids are recomputed; cached
entries stay as they were. Failures keep the last good data and back off
(`Retry-After`, else 15 min). Ids AA does not know are marked resolved, so
they do not trigger repeat fetches. Stats and curated text are skipped for local-backend or
overridden ids.

## Curated row check (2026-10-19)

Re-read the comparison page: Haiku 4.5, Haiku 5.5, Gemini 3.7/3.8 Flash,
Grok 4.5/4.6, GPT-5.4/5.5 each have their own row, and the table's task/excels
text matches our rows. Identical text across versions is the page's own text.

## Excluding models from the listing

Ids in `config/model-excludes.txt` (embedded at build time) are omitted from
`/v1/models` `data` and `models`. Requests to them still work.

- Add or remove an id: edit the file (one id per line, `#` comments, blank
  lines ok) and rebuild.
- Runtime additions without a rebuild: set `COPILOT_PROXY_RS_MODEL_EXCLUDES`
  to a comma list; it adds to the file and cannot remove file entries.

Patterns: an entry ending in `*` is a prefix match (`gpt-4*` hides gpt-4,
gpt-4o, gpt-4.1, ...). Anything else is an exact id (`gpt-5.5` does not hide
`gpt-5.5-xyz`). The same matcher applies to the file and to the env var.
Excluded ids are skipped before stats (no AA matching or fetch, no cache entry;
old entries are purged on the next listing). Routing is unchanged.
