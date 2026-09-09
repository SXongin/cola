# Message authorship via a `msg_cola_` id prefix; the external-message sync owns the Sync Watermark alone

cola shares the Shared Store with OpenChamber, so the external-message sync has to
decide whether a user message was written by cola (Feishu) or by another client.
That decision used a per-session "baseline" recorded at the end of cola's own
prompt; when the attached Coexistent Server died mid-turn the baseline degraded
to cola's client-side submit epoch, which predates the server-side creation time
of the very message cola had just persisted — so after cola healed by starting an
Owned Server on the same store, the poller read cola's own message as
newer-than-baseline and re-notified + re-rendered it into Feishu, duplicating the
round (observed 2026-09-09). A timestamp baseline can never be authoritative:
after an ungraceful server death a cola write and an OpenChamber write are
indistinguishable rows. We decided to make authorship a property of the message
itself and give the poller sole ownership of the sync state.

## Decision

1. **cola assigns every user message it submits a self-identifying id
   `msg_cola_<unique>`, sent as `PromptInput.messageID`.** The server persists
   that id as the message's identity (`info.id = input.messageID ?? MessageID.ascending()`).
   Covers the main prompt, supplements (`prompt_async`), and error-card retries
   (a retry reuses the same id, held on the in-memory card session). No cola-side
   ledger is kept — the prefix *is* the authorship record, and it survives a
   server crash/replacement and even a cola restart.
2. **The external-message poller is the single owner of the Sync Watermark.**
   `record_prompt_baseline` is deleted. The poller (active sessions only, skipped
   while a turn is in flight) reads the newest user message: if its id starts
   with `msg_cola_` it is a Cola-Authored Message — advance the watermark, no
   notification; otherwise, if newer than the watermark, it is an External
   Message — notify and advance. First observation still establishes silently;
   deactivation still clears the watermark so a later `/switch` re-baselines
   silently (ADR-0017 semantics unchanged).
3. **The legacy `/api/session/{id}/prompt` fallback is removed from the OpenCode
   client.** It cannot carry `messageID`, and it appends a fresh user message and
   re-runs the model (verified live against a current server), so falling back on
   a 5xx is a duplicate-message + double-run hazard. A canonical `404` still maps
   to `SessionNotFound` (session recreate); any other failure surfaces the error
   card, and the user's retry reuses the same `messageID`, which the server
   deduplicates by id.

## Considered options

- **Stored ledger of chosen ids vs prefix self-identification.** A ledger adds
  persistence + pruning for no gain; the `msg_cola_` namespace is cola's to
  reserve on the shared store. Rejected.
- **Prompt-side baseline with a `now()` fallback vs poller ownership.** Fixing
  the fallback still leaves authorship split across two modules and re-guesses
  after crashes; the poller owning the watermark removes the cross-module poke
  (architecture-deepening #04) and needs no epoch heuristics.
- **Feature-detect-gated legacy fallback vs removal.** The legacy path cannot
  express authorship and is live on current servers anyway; keeping it gated
  adds probe+branch complexity to defend an "old server" that may not exist.
  Rejected.

## Verification (2026-09-09, live server)

- `POST /session/{id}/message` with a custom `messageID` echoes it as
  `info.id` and persists it; ids need only start with `msg`.
- Re-posting the same `messageID` does not duplicate the user message (single
  row kept) — retries are idempotent server-side.
- `POST /api/session/{id}/prompt` is alive on a current server, appends a new
  user message, and starts a run.

## Consequences

- `msg_cola_` is reserved on the Shared Store; any client that fabricated that
  prefix would be misread as cola (accepted — only cola uses it).
- Retries can no longer duplicate user-message rows. Residual accepted gap: a
  cola restart between a committed send and a manual retry loses the in-memory
  id, so one manual retry after such a restart can add a duplicate user message.
- Interrupted turns are still recovered by the user clicking 重试 on the error
  card; automatic resume after healing is a separate, deferred decision.
