# Message authorship & the Sync Watermark

External-message sync tells Feishu when another shared-store client (OpenChamber,
the CLI) posts to a cola-mapped session. It must never re-notify cola's OWN
rounds. The old discriminator — a per-session "baseline" timestamp recorded at
the end of cola's own prompt — broke when the attached Coexistent Server died
mid-turn: the baseline degraded to cola's client-side submit epoch, which
predates the server-side creation time of the very message cola had just
persisted, so after cola healed (Owned Server on the same store) the poller
re-notified and re-rendered cola's own round into the same topic (observed
2026-09-09, `ses_f7c193fc…` 「cola开发节省token工具调研」). A timestamp baseline can
never be authoritative after an ungraceful death.

## Decision (ADR-0026, confirmed via grilling 2026-09-09)

1. **Every user message cola submits gets a self-identifying id `msg_cola_<uuid>`
   sent as `PromptInput.messageID`** — server persists it. Main prompt,
   supplements (`prompt_async`), and error-card retries (which REUSE the id).
   No cola-side ledger; the prefix IS the authorship record and survives a
   server crash/replacement and a cola restart.
2. **The external poller alone owns the Sync Watermark** (`record_prompt_baseline`
   deleted). Rule (active sessions, inflight skipped): the NEWEST user message —
   `msg_cola_` prefix → cola-authored → advance watermark, never notify;
   otherwise newer than the watermark → External Message → notify + advance.
   First observation and deactivation semantics unchanged (ADR-0017).
3. **Legacy `/api/session/{id}/prompt` fallback removed from the client.** It
   cannot carry `messageID`, appends a fresh user message, and re-runs the model
   (verified live); a canonical 5xx now surfaces the error card and the retry
   reuses the same id (idempotent server-side).

## Key terms (CONTEXT.md)

Cola-Authored Message · External Message · Sync Watermark (replacing "baseline").

## Scope

In: authorship id at all send sites, poller-owned watermark, fallback removal.
Out (deferred): auto-resume of interrupted turns after a server heal — still a
user-clicked 重试 today.
