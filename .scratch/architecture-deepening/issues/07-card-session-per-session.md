# 07 — CardSession per session

**What to build:** Make "one live card per session" a single persistent module
that owns the streaming accumulator and the card identity chain, replacing the
two per-session maps kept in lockstep. The prompt path resets the card's content
per turn but keeps its identity; the mid-prompt session-recreate becomes one
remap operation instead of a three-map re-key; card updates (including
continuation cards) are app-independent. This builds on the dead-fold removal.

**Blocked by:** 01 — Delete the dead SSE event fold; 04 — Flows take SharedCore.

**Status:** resolved

- [x] "One live card per session" is one module; no parallel identity/content
      maps kept in lockstep by hand.
- [x] The session-recreate path performs a single remap operation.
- [x] State transitions (loading → streaming/reasoning → done/error, inline
      sections) unit-tested; card flushing tested against the recording platform.
- [x] Full verification loop green.

## Answer

`SharedCore`'s two lockstep maps (`accumulators` + `card_message_ids`) are gone,
replaced by one `cards: HashMap<String, CardSession>`. `CardSession`
(`streaming.rs`) owns the accumulator and the card identity chain
(`card_message_id`), updated in place by `flush_card` including continuation
cards.

- `run_prompt` builds the loading card and inserts one `CardSession` carrying
  the card id — content reset per turn, identity kept.
- The mid-prompt session-recreate re-keys a single `cards` entry (was a
  three-map re-key: card ids + accumulators + inflight).
- `flush_card`, `refresh_session_title`, `render_and_flush`, `handle_retry_action`,
  the request flows' inline sections, the external renderer, and the pollers'
  target/host resolution all read/write `CardSession` through the shared core —
  app-independent.
- Tests now assert through `app.cards[...].acc` / `.card_message_id`.

State transitions (loading → streaming/reasoning → done/error, inline sections)
are covered by the existing render-pipeline and integration tests; the
continuation-card flushing against the recording platform is exercised by
`long_answer_splits_across_cards_no_plain_text` and the streaming card-split
tests. 235 tests pass.

Verification: fmt clean, clippy `-D warnings` clean, 235 tests pass, release
build succeeds.