# `/think`: per-session thinking level as the model's own declared variants

Users could pick a model via `/model` but had no way to control how hard the model thinks. Reasoning-effort settings are **not standardized across models** — OpenAI/xAI expose `reasoning_effort` with their own value sets, Gemini uses `thinkingConfig.thinkingLevel` (e.g. `minimal`), Modal declares `none/low/medium/high/xhigh/max`, and most models declare none at all — so OpenCode models each carry their own `variants: [{id, body}]`, applied server-side per prompt via `PromptInput.variant`. cola now exposes that directly: a new `/think` command (card + text forms) lists the current session's model-declared variants plus a "default" option, and stores the pick per session in a dedicated SessionStore field sent as `variant` on every prompt. There is deliberately **no universal low/medium/high scale** — inventing one would mislead users about models whose variants don't match it. "Unset" means the server's default for that model.

## Considered options

- **Normalized scale (low/medium/high)**: intuitive but impossible to map correctly across providers; requires cola to maintain a translation table that would silently diverge. Rejected.
- **Variant folded into the model string (`provider/model/variant`)**: `parse_model` already read three segments, but model IDs can contain slashes (`openrouter/openai/o3`), making a flat string ambiguous, and `inject_model` silently dropped the parsed variant anyway. Rejected — variant is a separate `Option<String>` field, the model string stays `provider/model`, and `parse_model` narrows to a two-part split.
- **Third level in the `/model` picker**: adds complexity to an already-chunked 30KB-card picker and mixes model-switch (which may clear the variant) with variant-pick. Rejected — a standalone `/think` command mirrors OpenChamber's separate "thinking" dropdown.
- **Dispatch-time validation of every prompt**: costs an extra `GET /provider` per turn for a case the set-time checks already cover. Rejected — validation happens only at set time (`/think` rejects undeclared values; `/model` clears a variant the new model doesn't declare); the server's `VariantUnavailableError` remains the last-resort surface.

## Consequences

- SessionStore entries gain a `variant: Option<String>`; `/think default|off|reset` clears it, and it is persisted across restarts like `/model`.
- `list_models` (`GET /provider`) must carry each model's declared variants, used by both the `/think` card and the `/model` auto-clear check.
- `inject_model` must now write `variant` (currently drops it); the turn footer renders `provider/model@variant` from the session store.
- The `/think` card resolves the current model as session override → configured default → server-recorded session model (`GET /session/{id}`); with none of those it tells the user to `/model` first. A model with no declared variants gets a text prompt, not a card.
- Out of scope: agent-config-pinned default variants (an explicit `/think` override wins) and sub-task child sessions.