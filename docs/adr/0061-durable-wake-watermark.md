# A durable Wake watermark: a restart never re-announces an announced Wake

> **Amended by ADR-0069**: the watermark is the `announcements` section of the
> **Chain Record** (`chain_records.json`, one module owning a Card Chain's
> durable facts). Its semantics — advance only after a card write lands,
> monotonic per Session — are unchanged.

> **Amended by ADR-0071**: the render-frontier watermark rejected below is no
> longer rejected — it is the **Rendered Cursor** on the Chain Record,
> advanced only by a confirmed card write. The Wake Watermark's own decision
> stands; only a chain with no durable record uses its announcement rule now.
> See the amendment at the end.

## Context

ADR-0059 decided that card state stays in memory and the Wake path is
card-independent: with no chain (a cola restart) the continuation is armed
"fresh" from the durable transcript alone — the newest placeable Wake, not
stale (no newer user message), and the content probe. The live defect of
2026-09-29 (#424) showed the missing fact: a subagent Wake whose completion the
previous cola life had already rendered (its entry rode the live card) read as
"the run finished while cola was down" after a `/restart`, so Session Sync
posted a 承接-only card ~0.4 s before the user's next message was admitted; had
the continuation won that race instead, the same decision would have replayed
work the user had already seen. What distinguishes the two cases — "this Wake
was announced" — lived only in the in-memory `announced_wakes` set, which a
restart drops. No transcript rule recovers it: the rule that an Execution idled
before this process started cannot tell an already-announced run from one that
finished while cola was down — both idled before this process started, and a
shutdown writes no boundary cola could count.

The second-order race is a read-model fact, not a timing accident: a Feishu
message is invisible to the Session Transcript until cola submits it as a
Cola-Authored Message, so a Fresh post's transcript re-read cannot see a
just-received message; and `Turn::start` claims the session with no interlock
(its `inflight` guard is inserted before any card work, no write lock is held,
and it replaces the `CardSession` unconditionally — a Waiting card is the only
one collected).

## Decision

**Persist a per-session Wake Watermark.** The newest Wake whose completion a
card has announced, as `(wake id, created_ms)`, keyed by session id — its own
sidecar file at the time (`~/.cola/wake_watermarks.json`); since ADR-0069 the
`announcements` section of `chain_records.json`, atomic tmp+rename, the
`interactive_surfaces.json` / `pinned_chats.json` pattern. It is advanced only
after the card write that carries the announcement lands — a split's
continuation send, the PATCH carrying a merged completion entry, the Fresh
card's own send — so it tracks a user-visible fact, never an intention. It
survives a session being unmapped (a re-mapped session does not re-announce),
and a user message never moves it (that is the Sync Watermark's scope).

**A restart posts only a strictly newer Wake.** The Fresh path continues only
when the newest placeable Wake is newer than the watermark; the content probe
stays as a second gate, so a 承接 line never opens a card alone. A missing
watermark — the first restart after upgrading — reads as "nothing announced":
the pre-#424 behavior stands for at most one announcement per session, then the
watermark exists and the bug is gone. Seeding the watermark from the newest
existing Wake was rejected: it silently drops a genuine "finished while cola
was down" Wake.

**The race guard is the inbound claim, not a transcript re-read.** A message
routing to the session sets an in-process claim (earlier than `inflight`),
checked before the Fresh send; a bounded pre-send re-read additionally requires
the newest user anchor to be unchanged, covering messages another shared-store
client wrote. The residual window between the last check and the card being
sent is accepted and logged — no destructive recall is added for it.

The watermark mirrors `announce_wake` on every path (the merged entry, the 承接
line, the Fresh arm), so #426's in-place announcement advances it through the
same choke point.

## Considered options

- **An announced-Wake id set.** Exact but unbounded and needs pruning, for no
  gain over the watermark: announcements advance in transcript order.
- **A render-frontier content watermark.** Would also cover non-Wake content a
  restart might replay, but must be written on every card PATCH — the hot path
  — for a case the Wake-scoped mark already covers. (Reopened by ADR-0071,
  which persists exactly this frontier as the **Rendered Cursor**; see the
  amendment at the end.)
- **An Execution-idled-before-process-start rule.** Cannot carry the decision
  (both cases look identical); useful only as a supporting signal.
- **Re-read the transcript before sending instead of an inbound claim.**
  Cannot see the just-received message at all (cola writes the prompt only at
  admission), so it alone does not close #424's observed race.
- **Recall a raced Fresh card after the send.** Rejected: destructive platform
  surface and a new failure mode for a sub-second window.

## Consequences

- ADR-0059's "card state stays in memory" is narrowed for one fact: what has
  already been announced is durable. Live cards themselves still die with the
  process (a waiting card still goes stale), and the Fresh continuation still
  scopes itself at the newest Wake so the lost card's content is never
  replayed.
- CONTEXT.md gains **Wake Watermark**, distinct from the **Sync Watermark**
  (user-message-scoped, in-memory, never moved by a Wake).
- Tests pin: an already-announced Wake posts nothing after a restart; a Wake
  whose run finished while cola was down still continues; the missing-watermark
  migration read; a racing inbound message (claim set, or a changed anchor in
  the corroborating read) posts nothing; the mark advances after a successful
  send/PATCH and not before.
- Adjacent, out of scope: after a completion entry lands on a waiting card the
  header still reads 「⏳ 等待后台任务」 until the resumed run renders something
  (reasoning is not exposed live) — a #426 in-place question, not this mark's.

Related: #424, #426, #405, #403, ADR-0059, ADR-0060, ADR-0026, ADR-0043,
ADR-0053.

## Amendment (2026-10-06): the render-frontier watermark is reopened (ADR-0071)

The option in *Considered options* was rejected as "must be written on every
card PATCH — the hot path". ADR-0071 reopens it because the cost that
rejection weighed is no longer there. The **Rendered Cursor** rides the
**existing delivery choke point**: it is staged with the body about to be
written and drained by the same confirmation — a PATCH `Ok`, a create `Ok`,
or a Pending Card Update the drain later delivers — that already orders every
card write, so no new write path and no extra per-tick write is added. The
payload is a frontier position plus a set of call ids, **small** and
content-free, and the local atomic write is negligible beside the network
PATCH it follows, whose cadence is content-driven and bounded. The decisive
change is the **mechanism count**: one cursor retires the #443 restart stamp
for cursor-carrying records, ADR-0068's carry, and the Fresh gate's no-replay
for recorded chains, collapsing the pairwise restart rules into one
projection.

The Wake Watermark's own decision is unchanged — including its rule that the
mark advances only after the card write carrying the announcement lands, which
the cursor's advance rule mirrors — and the Fresh path keeps its
**recordless scope**: the watermark's announcement gate still decides a
post-restart continuation for a chain with no durable record. What the
reopening changes is which chains reach it: a **recorded** chain's Wake
belongs to the projection (or, cursorless, to the reap's fallback), so the
watermark's exactly-once meaning stands exactly as decided above, where it
applies (ADR-0071's retirement of the recorded-chain Fresh arm).
