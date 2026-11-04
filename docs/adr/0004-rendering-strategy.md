# Card rendering strategy: polling + PATCH over native streaming

> **Amended by the 2026-11-04 update below**: the decision stands, but the
> first reason under "Why" (callback conflict) was written more absolutely than
> the platform rule. What the rule forbids is a write while a callback is
> outstanding; cola's ack returns the card in that window, which is the rework
> streaming would cost. The reason that survives is the form/streaming conflict
> plus the card-entity cost.

Feishu offers native card streaming (`config.streaming_mode` + cardkit component APIs) that AI bots can use for a "typewriter" effect. We evaluated it and deferred it.

## Decision

cola renders progress by **polling the session's messages every 1.5s and PATCHing the whole card** (`PATCH /im/v1/messages/{id}`) through the existing `render_poll_loop` → `flush_card` pipeline. Native streaming mode (`config.streaming_mode`, cardkit `cardkit:card:write` permission, `element_id`-keyed component updates) is **not** used. Recorded here so the exploration isn't repeated.

## Why

- **Callback conflict**: in streaming mode, a card callback (permission / question / retry buttons) **cannot update the card immediately** — the app must first turn streaming mode off, then process the callback. cola's permission and question cards depend on instant ack-updated result cards, so streaming would complicate the interaction flow for little gain. *(Narrowed by the 2026-11-04 update: the forbidden window is the one while the callback is outstanding, and the collision is with cola's ack carrying the whole card — a rework, not a wall.)*
- **Extra permission**: streaming needs `cardkit:card:write` and a switch from message JSON cards to card-entity creation. The user's constraint is to avoid new Feishu permissions unless necessary.
- **Global SSE is unreliable here**: the shared OpenCode server's global SSE connection is ended every few seconds, so cola already renders by polling; streaming would add a second, parallel rendering path.

## Notes for a future attempt

- Requires card entity (`POST /cardkit/v1/card`), `config.streaming_mode: true` + `streaming_config`, and per-component updates via cardkit APIs.
- Must close streaming mode before the update that **answers the callback**
  (`200810`, "the card is in an ongoing interaction and cannot be updated").
  cola puts its update in that very response, so this is the path a migration
  would have to change: answer with a toast only and write asynchronously,
  which needs no close. Returning the card in the ack is **not** a
  streaming-compatible shape. See the 2026-11-04 update.
- Streaming cards can't be forwarded while streaming.

## Update (2026-11-04): the reason that survives is the form, not the callback

Discussion #972 (a sequel to #940) revisits this decision with a Bridge that
runs one card per turn — streaming answer, inline approval slot, separate
question card. It does not overturn the decision, but the **first** bullet under
"Why" — the one a reader takes as decisive — was written more absolutely than
the rule it names.

The streaming guide still says a card in streaming mode cannot take a
callback-driven update, and its error table pins the window: `200810`, "the card
is in an ongoing interaction and cannot be updated". What that forbids is a
write **while the callback is outstanding**, not an async write after it has
been answered. #972 answers its callbacks with a toast and patches the card
afterwards, so streaming leaves it unblocked. cola's ack path is not that shape:
`settle_ack` deliberately returns the **whole card** in the response
(`{card:{type:raw,data:…}}`, ADR-0038 rule 3) because that avoids a PATCH race,
and `ack_inline_card` only falls back to a flush after the fact. So streaming
would force that path to become "toast now, async write after" and give up what
rule 3 bought. That is a bounded rework, not the hard blocker this ADR's wording
implies — which is why the correction below narrows the reason rather than
replacing it.

What #972 confirms from the other side is the reason that does survive: a card
whose text is being played out re-renders under the client, and a `form` input
on it loses whatever the Host typed. Their answer is to put the question form on
its own non-streaming card — which is exactly the split-card-on-interaction
option ADR-0014 rejected. cola keeps the form inline and pays for that with the
render freeze (pitfall `card-repaints`). Element-level writes **would** let a
card keep an inline form without the freeze, because they do not rebuild
`body.elements` — #972 asserts that; nothing here has verified it against a
live client, so treat it as the assumption this deferral waits on, not as a
measured fact. It is the one genuine structural gain on offer, and the reason
this decision is a deferral rather than a permanent no.

The rest of the cost is unchanged and is what #972's own pitfall list is mostly
made of: `cardkit:card:write`; a card-entity lifecycle (14-day validity, single
send, 10 ops/s per card, a `sequence` high-water mark no reader can query, `uuid`
idempotency); a brief `card_id` invisibility window on the IM side (`230099`);
streaming mode auto-closing after 10 minutes (`200850`); and no forwarding while
it streams.

