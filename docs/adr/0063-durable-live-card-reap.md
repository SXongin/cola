# A durable Live Card record: a restart reaps the card it orphaned

## Context

ADR-0059 decided that card state stays in memory — "a cola restart loses live
cards, as today: a waiting card goes stale, but the Wake path is
card-independent". ADR-0061 narrowed that for exactly one fact: what a card has
already announced. #428's other live defect (2026-09-29) is the complementary
half. The card that was live when the process restarted stays frozen forever:
nothing knows its Feishu message id, and Session Sync's Wake pass only renders
*new* Wakes onto continuation cards — it never settles the orphan. In the
incident the card sat frozen for ~1h43m while the server's stale active entry
kept the session "live", and stayed frozen after the session idled. The
acceptance is explicit: "a card whose owner died with a cola restart is
reaped/settled rather than staying frozen forever".

## Decision

**Persist a per-session Live Card record.** `(message id, Turn anchor)` per
session, in its own sidecar (`live_cards.json`, the wake_watermarks pattern:
atomic tmp+rename, best-effort, absent or corrupt reads as empty). It is
written when a card becomes a Session's live card, and removed when that card
reaches a terminal (Done / Error / Stopped) or is collected by a successor. The
anchor is the settle decision's scope — without it the transcript cannot be
asked what happened to the run.

**Reap in Session Sync, every pass, not just at startup.** For each record the
pass reconciles the card against the Session's own reads:

- the transcript's real ending settles it in place — ✅ / Error / Waiting on
  Background Tasks — so a run that finished while cola was down gets its true
  ending, never an invented interruption;
- an idle Session whose Turn message never landed ends Unreceived (ADR-0062),
  never ✅;
- a live Session keeps the card: the run may still be answering it;
- a continuation that took the chain over collects the old card in place, so no
  card is left looking live.

The same pass covers a Waiting card orphaned by a restart: its Session's true
end settles it (ADR-0060's quiet true end), instead of leaving it frozen.

**Missed content is not rebuilt onto the old card.** The record reaps the
card's *state*; content that landed while cola was down is rendered by the
existing paths — a Wake's continuation card is exactly that message — or simply
left in the transcript. Replaying a whole turn onto a stale card is the
re-render the Fresh path deliberately avoids.

## Considered options

- **Status quo, in-memory only.** Rejected: leaves the orphan frozen forever —
  the reported defect.
- **Persist the message id alone, no anchor.** Rejected: no anchor, no settle
  decision; the reap could only guess an ending.
- **Rebuild the card from the transcript at startup.** Rejected: a full card
  renderer on the restart path for a rare case; the continuation machinery
  already publishes missed content as a new message, which is also the
  notification.
- **Settle every orphan as "interrupted".** Rejected: it lies about runs that
  finished while cola was down; the transcript-truth rule of ADR-0062 applies
  here too.

## Consequences

- ADR-0059's "Card state stays in memory" is narrowed twice over: with
  ADR-0061, the announced-Wake fact; here, the live card's identity and anchor.
- A restart no longer leaves a frozen card: every non-terminal card either gets
  its true ending or is collected, and a still-live Session's card is settled
  by the same reconcile later.
- `live_cards.json` holds at most one record per Session and shrinks at every
  terminal or collection; no separate pruning pass is needed.
- Tests pin: a persisted live card over a restart settles in place by the
  transcript's ending; a never-promoted one ends Unreceived; a continuation
  collects the old card; a still-live Session leaves the record in place.

Related: #428, #424, ADR-0059, ADR-0060, ADR-0061, ADR-0062.
