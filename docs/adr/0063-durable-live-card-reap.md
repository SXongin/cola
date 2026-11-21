# A durable Live Card record: a restart reaps the card it orphaned

> **Amended by ADR-0069**: the record is the `records` section of the **Chain
> Record** (`chain_records.json`, one module owning a Card Chain's durable
> facts), and the reap lives in that module. The semantics below — written when
> a card becomes the Session's live card, removed only after a confirmed ending,
> fail-open load — are unchanged.
>
> **Amended by ADR-0071**: the reap gained the projection disposition — a record
> carrying a **Rendered Cursor** whose run is unowned is projected onto a
> successor (or adopted and followed) instead of being settled in place — and
> the #443 stamp narrowed to the cursorless fallback.
>
> **Amended by ADR-0072**: the #443 stamp is a **keyed submission** the
> card-delivery queue orders by **chain generation**. The detached
> never-cancelled write task, the in-flight claim and its decision gates, the
> post-read pre-check, the post-PATCH repair collect and the `restarted_reaped` /
> `restart_stamp_rejected` marks all retired, and the record-release gate counts
> the queue's keyed ending state.

## Context

ADR-0059 decided that card state stays in memory — "a cola restart loses live
cards: a waiting card goes stale, but the Wake path is card-independent".
ADR-0061 narrowed that for exactly one fact: what a card has already announced.
#428's other live defect (2026-09-29) is the complementary half: the card that
was live when the process restarted stays frozen forever — nothing knows its
Feishu message id, and Session Sync's Wake pass only renders *new* Wakes onto
continuation cards, never settling the orphan. In the incident the card sat
frozen for ~1h43m while the server's stale active entry kept the session "live",
and stayed frozen after the session idled. The acceptance is explicit: "a card
whose owner died with a cola restart is reaped/settled rather than staying frozen
forever".

## Decision

**Persist a per-Session Live Card record.** `(message id, Turn anchor)` per
Session, now the `records` section of the **Chain Record** (ADR-0069; formerly
`live_cards.json`; atomic tmp+rename, best-effort, absent or corrupt reads as
empty). It is written when a card becomes a Session's live card, and removed
when that card reaches a terminal (Done / Error / Stopped) or is collected by a
successor. The anchor is the settle decision's scope — without it the transcript
cannot be asked what happened to the run. The record also carries the Session's
directory at track time: the reap's reads route by it, so an orphan whose
mapping is gone still asks the instance the card belongs to — never the
process's cwd, which on a per-directory read generation (V1) could be another
instance's run.

**Reap in Session Sync, every pass, not just at startup.** For each record the
pass reconciles the card against the Session's own reads:

- the transcript's real ending settles it in place — ✅ / Error / Waiting on
  Background Tasks — so a run that finished while cola was down gets its true
  ending, never an invented interruption (a cursor-carrying record follows
  ADR-0071's projection ladder instead, below: it projects a tail that is owed
  and settles in place when the delta is empty);
- an idle Session whose Turn message never landed ends Unreceived (ADR-0062),
  never ✅ — the same ending, without the 重新发起 action: the click's fixture
  died with the process, so the reaped card tells the user to re-send;
- a live Session keeps the card, unless the record carries a Rendered Cursor
  whose run this process does not own: then the run is projected onto a
  successor — `Project` for an ended-while-down chain, `ProjectLive` for a
  still-live one — the recorded card is collected as taken over, and the record
  re-points to the successor with the cursor carried (ADR-0071 owns that ladder
  and the atomicity);
- a continuation that took the chain over collects the old card in place, so no
  card is left looking live;
- every ending is stamped over the card's own view, read best-effort: the body
  the card already rendered stays (interactive elements stripped, since a
  whole-card read does not return their callback values), and a failed read
  still settles the bare ending.

The same pass covers a Waiting card orphaned by a restart: its Session's true
end settles it (ADR-0060's quiet true end), instead of leaving it frozen.

**The #443 stamp is the cursorless fallback.** A still-live orphan whose record
carries no cursor is stamped **once per process life** with the header
「⏳ 已重启，等待运行结束」: the card's own view is read back and only the header
changes — body kept, controls stripped, like every ending — except that a failed
view read leaves the card untouched rather than settling bare, because the
stamp's whole value is the body it preserves. The view read and composition run
on a small pre-submission task (the pass must not await a stuck Feishu call);
the write is a **keyed submission** to the card-delivery queue (ADR-0072) under
`(chain generation, Stamp)`, the generation of the record snapshot the decision
read. A collect submitted later at a newer generation supersedes it; a stamp
whose generation the queue's floor has passed is dropped without a Feishu call;
and re-deciding every tick costs no card read and no Feishu call once the key
settled or is being written (the apply asks the queue's `keyed_write_covered`
before composing), while an owed recoverable failure is not covered and the
next tick's re-composition replaces it.

A record carrying a cursor is never stamped — an unresolved cursor, a missing
read, no scope to arm against, a frontier the read cannot place, or no
deliverable target claims nothing and leaves the record for a later pass the
projection can seed, because a stamp would freeze the run the projection is
supposed to follow. The first confirmed card write gives a record its cursor, so
the fallback window closes by itself. A PATCH Feishu permanently refuses as card
content (`CardContentRejected`) settles its `(generation, Stamp)` key: it is
deterministic — the same preserved payload can never land — so every later
re-submission is dropped without a card read and without a Feishu call; the card
simply stays without its stamp and the record with it. Every transient failure —
transport, timeout, auth, server — keeps the retry. No timer refresh, no
recurring PATCHes, no adoption: re-following a cursorless run is #505's faithful
restore, not the stamp.

**The record lives until its ending is confirmed.** A terminal card's record is
dropped only once its ending write is **confirmed** — delivered, or permanently
refused; a final PATCH that fails (or is still pending in ADR-0067's Pending Card
Update outbox) leaves the record in place, so Session Sync's reap still finds the
card and re-settles it from transcript truth, and the reap doubles as the
record's cleaner. The release gate (`release_spent`, via
`has_pending_card_update`) counts a keyed `Settle` the queue still owes or is
writing, not only the keyless outbox entry. Without this, a failed final PATCH
plus a cola restart froze the card forever: the record was gone and nothing knew
the card existed.

**Missed content is not rebuilt onto the old card.** The record reaps the card's
*state*; content that landed while cola was down is rendered by the existing
paths — a Wake's continuation card is exactly that message — or left in the
transcript. The projection (ADR-0071) renders only the delta after the confirmed
frontier onto the successor, and the old card keeps its body (minus the running
markers the successor resolved, ADR-0068's rule) and is collected as taken over.
Replaying a whole turn onto a stale card is the re-render the Fresh path
deliberately avoids; Interaction Receipts stay in ADR-0038's request-flow domain,
re-hosted onto the successor by the existing sweeps and never rebuilt.

## Consequences

- ADR-0059's "Card state stays in memory" is narrowed twice over: with
  ADR-0061, the announced-Wake fact; here, the live card's identity and anchor.
- A restart no longer leaves a frozen card: every non-terminal card either gets
  its true ending, is projected onto a successor, or is collected, and a
  still-live Session's card is settled by the same reconcile later.
- Every reap PATCH keeps the card's already-rendered body best-effort (#434
  acceptance feedback), while a failed read still settles the bare ending and
  content the card never showed is still never rebuilt.
- The Chain Record's records section holds at most one record per Session and
  sheds it at every terminal or collection — the file itself stays as the
  migration marker; no separate pruning pass is needed.
- #505's deferral lifts: the projection is that faithful restore.

Origins: #424, #428, #434, #443, #522, #575 (spec #571); ADR-0038, ADR-0059,
ADR-0060, ADR-0061, ADR-0062, ADR-0067, ADR-0069, ADR-0071, ADR-0072.
