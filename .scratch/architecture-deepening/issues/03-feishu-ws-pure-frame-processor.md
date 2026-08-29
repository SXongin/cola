# 03 — Feishu WS: pure frame processor + shared parse helpers

**What to build:** Extract per-frame processing — dedupe by event id, the
5-minute age filter, type dispatch, a single typed parse, and the
what-to-ack / what-to-dispatch decision — into a pure function; keep only socket
I/O in the connection handler. Cap or time-expire the dedupe set. Share the
message text/image/mention parse helpers between the live receive path and the
quoted-context fetch (no unified message model — that stays out of scope).

**Blocked by:** None — can start immediately.

**Status:** resolved

- [x] Dedupe replay, age-filtering and ack/outcome decisions covered by pure
      unit tests with no socket involved.
- [x] The dedupe set is bounded (cap or expiry) — no unbounded growth.
- [x] The live path and the quoted-context fetch call one shared parse helper
      set for text/image/mentions.
- [x] Full verification loop green.

## Answer

`process_frame(frame, &mut DedupeSet) -> FrameAction` is now a pure function in
`ws.rs` — no socket involved. It owns dedupe by event id, the 5-minute age
filter, type dispatch (ping / event / card), a single typed parse
(`MessageReceiveEvent`, `extract_card_action_value`), and the
what-to-ack / what-to-dispatch decision. `FrameAction` (Pong / Ack / Message /
CardAction / None) preserves the ack semantics exactly: every event is acked
even when deduped, stale, or unparseable (Feishu re-delivers unacked events
forever); only control/card/unknown frame types get no ack. The connection
handler keeps only socket I/O — matching each outcome to the ack/pong/response
writes and the spawned image-download dispatch.

`WsState`'s unbounded `HashSet<String>` is now a `DedupeSet` capped at 10,000
ids; on overflow it clears (the age filter makes stale ids worthless). The
dedupe decision and the ack remain in the pure function.

The text-extraction logic is unified into `message_text(message_type, content)`
used by both the live `parse_message_content` and `quoted_context`; the
image/mention helpers (`extract_image_keys`, `strip_mentions`) were already
shared by both paths.

Seven new pure unit tests cover dedupe replay (acked, not re-dispatched), the
stale-age filter, ping/unknown/card routing, unparseable-event acking, card
action routing, and dedupe-set eviction on cap overflow. 242 tests pass.

Verification: fmt clean, clippy `-D warnings` clean, 242 tests pass, release
build succeeds.