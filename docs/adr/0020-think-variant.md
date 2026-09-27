# `/think`: per-session thinking level as the model's own declared variants

Users could pick a model via `/model` but had no way to control how hard the model thinks. Reasoning-effort settings are **not standardized across models** — OpenAI/xAI expose `reasoning_effort` with their own value sets, Gemini uses `thinkingConfig.thinkingLevel` (e.g. `minimal`), Modal declares `none/low/medium/high/xhigh/max`, and most models declare none at all — so OpenCode models each carry their own `variants: [{id, body}]`, applied server-side per prompt via `PromptInput.variant`. On the wire (`GET /provider`) the same set serializes as a **Record keyed by variant id** — `model.variants` → `{"high": {"reasoningEffort":"high"}, ...}` — so a client reads the variant names as the Record's keys (OpenChamber does `Object.keys`, cola's `declared_variants` does the same; older servers sent the array form). cola now exposes that directly: a new `/think` command (card + text forms) lists the current session's model-declared variants plus a "default" option, and stores the pick per session in a dedicated SessionStore field sent as `variant` on every prompt. There is deliberately **no universal low/medium/high scale** — inventing one would mislead users about models whose variants don't match it. "Unset" means the server's default for that model.

## Considered options

- **Normalized scale (low/medium/high)**: intuitive but impossible to map correctly across providers; requires cola to maintain a translation table that would silently diverge. Rejected.
- **Variant folded into the model string (`provider/model/variant`)**: `parse_model` already read three segments, but model IDs can contain slashes (`openrouter/openai/o3`), making a flat string ambiguous, and `inject_model` silently dropped the parsed variant anyway. Rejected — variant is a separate `Option<String>` field, the model string stays `provider/model`, and `parse_model` narrows to a two-part split.
- **Third level in the `/model` picker**: adds complexity to an already-chunked 30KB-card picker and mixes model-switch (which may clear the variant) with variant-pick. Rejected — a standalone `/think` command mirrors OpenChamber's separate "thinking" dropdown.
- **Dispatch-time validation of every prompt**: costs an extra `GET /provider` per turn for a case the set-time checks already cover. Rejected — validation happens only at set time (`/think` rejects undeclared values; `/model` clears a variant the new model doesn't declare); the server's `VariantUnavailableError` remains the last-resort surface.

## Consequences

- SessionStore entries gain a `variant: Option<String>`; `/think --reset` (or the card's dedicated `think_clear` button) clears it, and it is persisted across restarts like `/model`.
- **Clearing is a mechanism, never a value word.** The "默认（清除）" card button carries its OWN `think_clear` action tag, and the text form reserves the `--reset` flag — so the *value* namespace holds only real variant names. A variant literally named `default`/`off`/`reset` stays selectable, and cola never depends on the server's internal `default` variant sentinel (`Session.Info.model.variant === "default"`) to mean "unset".
- `list_models` (`GET /provider`) must carry each model's declared variants, used by both the `/think` card and the `/model` auto-clear check.
- `inject_model` must now write `variant` (currently drops it); the turn footer renders `provider/model@variant` from the session store.
- The `/think` card resolves the current model as session override → configured default → server-recorded session model (`GET /session/{id}`); with none of those it tells the user to `/model` first. A model with no declared variants gets a text prompt, not a card.
- Out of scope: agent-config-pinned default variants (an explicit `/think` override wins) and sub-task child sessions.

## Amendment (2026-09-27): the variant's storage and transport are per generation

The Context and Decision above describe the variant as a per-session SessionStore
field "applied server-side per prompt via `PromptInput.variant`". That is V1's
mechanism. On V2 the variant lives inside the session's durable model ref
(`Model.Ref.variant`) and `/think` rewrites that ref through
`POST /api/session/{id}/model` (ADR-0055, spec #364 slice S7); V2 has no
per-prompt `variant` field at all. Everything else decided here stands: there
is still no universal scale, the value namespace still holds only real variant
names, and **clearing is still a mechanism, never a value word**. The
clear-on-model-switch rule is unchanged in intent and now applies against the
session's own selection — a model that does not declare the current variant
clears it from the durable ref, and a failed selection read is treated as
unknown (the variant is dropped rather than revived from cola's mirror).
"Unset" still means the server's default for that model.
