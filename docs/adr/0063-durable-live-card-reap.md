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
asked what happened to the run. The record also carries the Session's directory
at track time: the reap's reads route by it, so an orphan whose mapping is gone
still asks the instance the card belongs to — never the process's cwd, which on
a generation with per-directory reads (V1) could be another instance's run.

**Reap in Session Sync, every pass, not just at startup.** For each record the
pass reconciles the card against the Session's own reads:

- the transcript's real ending settles it in place — ✅ / Error / Waiting on
  Background Tasks — so a run that finished while cola was down gets its true
  ending, never an invented interruption;
- an idle Session whose Turn message never landed ends Unreceived (ADR-0062),
  never ✅ — the same ending, without the 重新发起 action: the click's fixture
  died with the process, so the reaped card tells the user to re-send (the
  amendment in ADR-0062);
- a live Session keeps the card: the run may still be answering it;
- a continuation that took the chain over collects the old card in place, so no
  card is left looking live;
- every ending is stamped over the card's own view, read best-effort: the body
  the card already rendered stays (interactive elements stripped, since a
  whole-card read does not return their callback values), and a failed read
  still settles the bare ending.

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
- Every reap PATCH keeps the card's already-rendered body best-effort (#434
  acceptance feedback): the card's own view is read and the ending restamped
  over it with its controls stripped, while a failed read still settles the
  bare ending and content the card never showed is still never rebuilt.
- `live_cards.json` holds at most one record per Session and shrinks at every
  terminal or collection; no separate pruning pass is needed.
- Tests pin: a persisted live card over a restart settles in place by the
  transcript's ending; a never-promoted one ends Unreceived; a continuation
  collects the old card; a still-live Session leaves the record in place.

Related: #428, #424, ADR-0059, ADR-0060, ADR-0061, ADR-0062.

## Amendment (2026-10-02): a still-live restart orphan is stamped once (#443)

Between a cola restart and the run's true end — or a successor's takeover —
nothing updated the orphaned card: the reap keeps its record while the
Session reads live, and no in-process accumulator owns the card, so it sat
frozen at its pre-restart state (observed during #434's acceptance round; the
batch promised "not frozen forever", not "keeps ticking"). The reap now
stamps such a card **once per process life** with the header
「⏳ 已重启，等待运行结束」: the card's own view is read back and only the
header changes — body kept, controls stripped, like every ending, except
that a failed view read leaves the card untouched rather than settling bare,
because the stamp's whole value is the body it preserves. The in-memory mark
is set only when the PATCH lands, so a failed attempt retries on the next
pass; the transcript-truth settle (or a successor's collect) supersedes the
stamp as today. No timer refresh, no recurring PATCHes, no adoption:
re-following the run is deferred until a faithful no-duplicate/no-omission
restore exists (#505).

The stamp's attempt is handed to a **detached task** and its write is
**never cancelled**. The pass must not await the write — a stuck Feishu call
would otherwise freeze Session Sync behind one orphan — while the write,
once issued, must run to its own result: the card-delivery lock is held
until it resolves, and that lock is what orders a successor's later collect
after the stamp, so the ordering no longer rests on timing. An in-memory
in-flight claim holds every other reap decision for the record until the
attempt resolves — a second attempt, or a settle decided meanwhile, could be
overtaken by the write still owed. The attempt marks the record when the
PATCH lands (a failed read or PATCH claims nothing and releases the claim
for the next pass), and a takeover admitted while it was in flight is
repaired by the post-PATCH re-collect. A hung write therefore holds the old
card's delivery lock until it resolves — exactly like every normal card
write (the flush, a settle, a collect); the pass and the pending-update
drain are never blocked by it. The view read stays bounded: nothing has been
sent yet, so a timed-out read is safe to abandon.

## Amendment (2026-10-02): the record is removed after delivery, not before (ADR-0067)

The removal rule above ("removed when that card reaches a terminal") is
narrowed: a terminal card's record is dropped only once its ending write is
**confirmed** — delivered, or permanently refused; a final PATCH that fails
(or is still pending in ADR-0067's Pending Card Update outbox) leaves the
record in place, so Session Sync's reap still finds the card and re-settles
it from transcript truth, and the reap doubles as the record's cleaner once
the write is confirmed. Without this, a failed final PATCH plus a cola
restart froze the card forever: the record was already gone and nothing knew
the card existed.

## Amendment (2026-10-05): the takeover collect drops carried running markers (ADR-0068)

The body-preservation rule above ("every ending is stamped over the card's own
view... the body the card already rendered stays") covers the takeover collect
too. ADR-0068 narrows it for one collect: the takeover's preserved body drops
the **Background Task Ledger** element always — the successor's own reads
rebuild the live list, so ADR-0060's one-card handover rule applies to the
restart collect exactly as it does to an in-process supersede — and drops the
orphaned Turn's **running tool panels** (the `⏳` foldables) when the takeover
carried them onto the successor; they are live there now, and keeping them
would leave the collected card looking busy. Every other preserved body — the
reap's settle / Unreceived / Waiting endings and the #443 restart stamp — is
unchanged, and a takeover that carried no running call keeps those panels (the
ledger still goes).
