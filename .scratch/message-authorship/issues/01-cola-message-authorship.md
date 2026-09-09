# 01 — cola-authored messages self-identify via `msg_cola_`; external sync owns the Sync Watermark

**What to build:** Fix the external-message poller re-notifying cola's OWN round
into Feishu after a server dies mid-turn (observed 2026-09-09). Make authorship a
property of the message (`msg_cola_` id prefix chosen at send time, persisted by
the server), give the poller sole ownership of the Sync Watermark, and remove the
legacy `/api` prompt fallback that cannot carry authorship and double-sends.

**Blocked by:** None — root cause confirmed from `/root/.cola/cola.log`;
design confirmed by grilling; ADR-0026 written.

**Status:** resolved

## Root cause

`run_prompt` recorded the external-sync baseline at the end of every prompt,
falling back to cola's client-side `epoch_ms` when the dying server could not be
read back. opencode persists a prompt's user message at a server timestamp ≥ that
epoch, so after cola healed on the same store the poller saw cola's own message
as "newer than baseline" and re-notified + re-rendered it into the topic.

## Implementation (done, uncommitted on `feat/msg-cola-authorship`)

- `src/opencode/client.rs`: `cola_message_id()` + `is_cola_message_id()`;
  `prompt`/`prompt_async` take a `message_id` and send it as `messageID`;
  the legacy `/api/session/{id}/prompt` fallback is deleted (404 stays
  `SessionNotFound`; other failures surface as a prompt error).
- `src/opencode/mod.rs`: trait + Client impl carry `message_id`.
- `src/bridge/handler.rs`: run_prompt resolves one `cola_message_id` per logical
  message, stores it on the accumulator, and reuses it across the error-card
  retry (`handle_retry_action`) and the 404-recreate re-send; the supplement
  path (`prompt_async`) gets a fresh id each time; the prompt-side baseline
  block is removed.
- `src/bridge/streaming.rs`: `StreamAccumulator.cola_message_id`.
- `src/bridge/external.rs`: `record_prompt_baseline` deleted; poll_loop reads the
  newest user message and branches on `is_cola_message_id` (advance watermark,
  never notify) vs external (notify when above watermark). ADR-0017 semantics
  preserved.
- Docs: terms in `CONTEXT.md`; decision in `docs/adr/0026-…`.

## Verification

- Live server probes (2026-09-09): custom `messageID` echoed as `info.id` and
  persisted; same-id re-POST is idempotent (no duplicate user message); legacy
  `/api/.../prompt` is alive on a modern server and appends a message + run.
- Tests: `cola_message_id_self_identifies_and_is_unique`,
  `cola_own_message_after_heal_is_never_notified_external`,
  `newer_external_message_after_cola_own_still_notifies`,
  retry reuses the id (`error_card_retry_reuses_card_and_reruns_prompt`),
  supplement carries a `msg_cola_` id
  (`message_during_inflight_goes_to_supplement_path`).
- `cargo fmt --all -- --check`, `cargo clippy --workspace --all-targets -- -D
  warnings`, `cargo test --bin cola` (415 passed) all clean.

## Deferred

Auto-resume of interrupted turns after a heal (currently: user clicks 重试).
