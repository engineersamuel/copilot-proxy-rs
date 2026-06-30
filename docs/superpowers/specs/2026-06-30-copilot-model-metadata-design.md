# Copilot Model Metadata Exposure Design

## Objective

Expose enough Copilot model metadata for local clients to understand actual model
capabilities, especially context-window and reasoning-mode options, while keeping
the existing OpenAI-compatible `/v1/models` response stable.

## Requirements

- WHEN a generic OpenAI-compatible client calls `/v1/models`, THE SYSTEM SHALL
  continue returning `object: "list"` and `data[]` model entries.
- WHEN a richer client calls `/v1/models`, THE SYSTEM SHALL include a `models[]`
  catalog with Codex-oriented capability metadata.
- WHEN the backend is Copilot, THE SYSTEM SHALL preserve sanitized upstream
  model metadata so operators can see what GitHub Copilot advertised.
- IF the upstream Copilot model refresh fails, THEN THE SYSTEM SHALL keep
  `/v1/models` usable with static fallback metadata.
- IF debug metadata is exposed, THEN THE SYSTEM SHALL not return credentials,
  authorization headers, tokens, or request-specific private content.

## Architecture

The model registry remains the central in-memory source for model metadata. It
continues storing the upstream Copilot `/models` entries, then adds helper logic
to build two read views:

1. OpenAI compatibility view: existing `data[]` entries with `id`, `object`,
   `created`, and `owned_by`.
2. Rich catalog view: Codex-style `models[]` entries with model slug, display
   name, reasoning support, context-window limits, long-context modes, endpoint
   support, and static fallback values.

A protected debug route, `/debug/copilot/models`, returns the sanitized cached
upstream model snapshot plus derived metadata and refresh status. The route uses
the existing app state and inbound auth middleware route protection conventions.

## Data flow

1. `CopilotClient::refresh_models_if_stale` calls GitHub Copilot `/models`.
2. `ModelRegistry::set_copilot_models` stores the returned `data[]` objects and
   refresh timestamp.
3. `/v1/models` asks the registry for an enriched response. The registry uses
   dynamic upstream fields when present and static overrides when the upstream
   data does not advertise known client-facing details.
4. `/debug/copilot/models` refreshes when stale, then returns a sanitized
   snapshot of cached upstream data and the derived catalog.

## Context and reasoning metadata

Static overrides cover known Copilot extended-capability behavior from official
GitHub docs and observed model picker UI. For GPT-5.5, the initial derived
metadata should distinguish:

- default context mode: conservative client packing limit of 272K unless a
  dynamic upstream value or explicit override says otherwise
- max or long-context mode: 1M-class context support
- reasoning levels: include `none`, `low`, `medium`, `high`, and `xhigh` when
  dynamic upstream metadata confirms them; otherwise preserve existing known
  supported levels and avoid inventing unsupported values

Pricing thresholds are separate from context-window capacity. The 272K threshold
from GitHub pricing is treated as a billing tier boundary, not proof that the
model cannot accept larger context.

## Error handling

- `/v1/models` never fails only because Copilot metadata refresh failed; it falls
  back to static metadata.
- `/debug/copilot/models` returns cached stale data with a warning when refresh
  fails after a prior successful fetch.
- `/debug/copilot/models` returns a clear upstream error when no cached data is
  available and refresh fails.
- Sanitization is fail-closed: unknown top-level request or auth fields are not
  added to the debug response.

## Testing

Tests will cover:

- `/v1/models` still contains the OpenAI-compatible `object` and `data[]` shape.
- `/v1/models` includes `models[]` rich metadata for known static models.
- dynamic upstream model metadata is preserved and used for supported endpoints
  and reasoning support.
- GPT-5.5 receives static extended-capability metadata when upstream data is
  minimal.
- `/debug/copilot/models` omits sensitive fields and reports refresh status.

## Scope

This design does not change request routing, authentication, Copilot token
acquisition, or Codex configuration. It only exposes and derives model metadata.
