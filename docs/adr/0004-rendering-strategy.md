# Card rendering strategy: polling + PATCH over native streaming

Feishu offers native card streaming (`config.streaming_mode` + cardkit component APIs) that AI bots can use for a "typewriter" effect. We evaluated it and deferred it.

## Decision

cola renders progress by **polling the session's messages every 1.5s and PATCHing the whole card** (`PATCH /im/v1/messages/{id}`) through the existing `render_poll_loop` → `flush_card` pipeline. Native streaming mode (`config.streaming_mode`, cardkit `cardkit:card:write` permission, `element_id`-keyed component updates) is **not** used. Recorded here so the exploration isn't repeated.

## Why

- **Callback conflict**: in streaming mode, a card callback (permission / question / retry buttons) **cannot update the card immediately** — the app must first turn streaming mode off, then process the callback. cola's permission and question cards depend on instant ack-updated result cards, so streaming would complicate the interaction flow for little gain.
- **Extra permission**: streaming needs `cardkit:card:write` and a switch from message JSON cards to card-entity creation. The user's constraint is to avoid new Feishu permissions unless necessary.
- **Global SSE is unreliable here**: the shared OpenCode server's global SSE connection is ended every few seconds, so cola already renders by polling; streaming would add a second, parallel rendering path.

## Notes for a future attempt

- Requires card entity (`POST /cardkit/v1/card`), `config.streaming_mode: true` + `streaming_config`, and per-component updates via cardkit APIs.
- Must disable streaming mode before handling any interactive callback.
- Streaming cards can't be forwarded while streaming.
