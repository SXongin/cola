# A Supplement needs a held guard: the renderer's lease is the routing proof

> **Amends**: ADR-0062 (the card-only arm is no longer a Supplement routing
> key), ADR-0043 (a failed Supplement's continuation), ADR-0066 (the
> mid-resume message's proof is the Wake loop's guard lease).

## Context

ADR-0062 made cola's **ownership** the Supplement routing key: the in-flight
guard a Turn holds (or the follow that inherited it), or a card chain a live
renderer owns — the external snapshot render, a Wake continuation. The Backend's
status read was left advisory, because a stale *live* read (the #428 incident)
must never absorb a message; a new Turn is the safe shape, its prompt carrying
the merge delivery.

Issue #1056 showed the card-only arm has the mirror failure. A Supplement whose
steer failed still split the chain, because "the split is requested even when
the send failed". When the run that was supposed to continue the new card was
already gone, the card stayed non-terminal with no renderer — every later prompt
then routed as a Supplement, `turn/steer` returned `Ok` into a run nobody
rendered (Codex's adapter falls back to `turn/start` when it observes no live
turn), and the user saw only 「📨 已收到补充」 until the durable reap collected the
orphan minutes later. A stale `Busy`/`Retry` read keeps the same hole open.

The shape is ADR-0062's #428 lesson, one level deeper: **neither the card's
display state nor the Backend's status read proves a writer.** Only an
in-process renderer does.

## Decision

**1. A held guard is the ONE Supplement routing key.** The guard proves an
in-process renderer (a Turn, or the out-of-turn follow), so a message merges
into it and the chain splits. Every other case — a card-only chain, a stale
`Busy`/`Retry` read, an unreadable read — starts a new Turn, whose card owns and
settles the message. Its V2 prompt still carries the merge delivery, so a
genuinely live run merges it exactly as a Supplement would. The status read is
advisory again: it only picks the new card's opening line. The ownership is
re-read AFTER that awaited read, so a renderer that leases the guard meanwhile
routes the message as the Supplement it is instead of losing it to a refused
Turn. A card-only chain's recorded card is **collected before the message is
classified**: no guard means no in-process renderer, so that card is exactly
the orphan #1056 describes, and the reap runs on this message's path instead of
leaving the stale card under the new Turn's card.

**2. Every out-of-turn renderer holds the guard for its window — a
reference-counted lease.** The follow already did (`inherit_guard`). The
external snapshot render loop and the Wake continuation's settle loop now hold a
per-session lease: the first holder claims the guard when free, a replacement
renderer that starts while the previous one retires joins the count, and only
the last release gives the guard back. A guard a Turn or follow already held is
not released by a lease's exit; instead, when that holder releases while a
lease is live, the guard **transfers** to the lease (the renderer is still
rendering). A message during a *live* render therefore stays a
Supplement (ADR-0062, ADR-0066 keep their behaviour), while a card left behind
with no loop has no guard — the exact stale shape #1056 reports.

**3. A failed steer only continues with a writer, and never claims the message
was lost.** When `steer` fails, the merge is unconfirmed. If a guard still owns
the chain, it splits and the continuation carries
「⚠️ 补充消息未确认送达，请留意当前回复」 (`SplitKind::SupplementFailed`); a race
that lost the guard keeps the chain and reports a plain notice — a continuation
would be a live card handed to nobody. The split's admission re-checks the
card's state under the same cards lock an ending is applied with, so a Turn that
settles between the ownership read and the split either stamps the continuation
(same chain) or is seen as terminal and refuses it; the caller then sends the
notice. A *merged* steer whose chain ends in that window is reported the same
way (「补充消息已提交，但当前卡片无法继续」), never silently. The split's
admission reports a permanently refused flush as a failure: the same plain
notice follows, while the loading window's queued split (no card id yet) and a
write owed to the delivery layer stay admitted. And every path that
frees the guard settles the card first: a failed stale-mapping recreate stamps
the Error ending before releasing, so no guard loss can leave a live-looking
card behind. Both messages say **unconfirmed**, never undelivered: an ambiguous
Codex response may still have landed and keeps its marker for reconciliation
(ADR-0092), so cola must not invite a resend.

**4. A Supplement's steer may not start a run.** The `Backend::steer` contract
gains `may_start_turn` and a `SteerOutcome`: the bridge passes `false`, and an
adapter whose steer can fall back to a start refuses **before** submitting when
it observes no live turn (Codex's `turn/steer`), and a generation whose plain
prompt merges-or-starts checks its own status read and refuses when no run is
live (reporting `Merged` only after a live submit, because a genuinely live run
is the one case it can merge). `StartedTurn` is for a future caller that can
adopt the run. Without this, a steer reaching a settled run starts a fresh run
nobody renders; with it, the message is never submitted and the visible failure
path runs. The cost is the notification-race fold (a live turn
the adapter has not observed yet): the user sees the unconfirmed notice and can
resend, which is the deliberate trade for never owning an unrenderable run.

## Considered options

- **Back the card-only arm with the status read, demoting on a settled read.**
  Rejected after review: a stale `Busy`/`Retry` still routes a Supplement into a
  run with no runner, and the issue's criterion demands a prompt never be
  classified as a Supplement when no turn is active.
- **Remove the card-only arm without the lease.** Rejected: a live external or
  resumed run would start a competing Turn card, silently reversing ADR-0062
  and ADR-0066 for a run that is genuinely being rendered.
- **Track a separate renderer token beside the guard.** Rejected as duplicate
  state: the guard already means "this process is driving the session", and the
  follow has held it for exactly this purpose since ADR-0059.
- **Keep splitting after a failed steer without a writer.** Rejected: a failed
  Supplement has nothing to continue, and the new card is an orphan.

## Consequences

- `CardOwnership::routing_label` stays as the Wake gate's live-chain probe; the
  prompt router uses `holds_guard` instead (ADR-0062's "the two can never
  disagree" narrows to "the Wake gate never double-renders a chain a Turn or a
  lease owns").
- A direct `Turn::run` (a `/retry`, a low-level seam) is refused while an
  external/Wake renderer holds the guard — the same busy answer a concurrent
  Turn gets, and what a routed message avoids by supplementing.
- A card-only chain whose renderer has just exited may start a Turn for a
  message the renderer could have split; the renderer owns no run at that
  point, so the new Turn is the safe owner.
- The chat sees one failure for a failed Supplement: the continuation card (with
  a writer) or a plain notice (without one), never both.
- The instant between the router's final ownership read and `Turn::start` is not
  atomic. A renderer that is retiring its lease with a terminal card (the
  Supplement cannot merge) or acquiring its lease in that instant (the Turn is
  refused as busy) is not re-routed; closing it needs a route/admission
  handshake, deferred to #1086 (#1056 defers that sub-case). The reported
  failure path is unaffected.

Related: #1056, #1055, #1083, #1086, #428, ADR-0043, ADR-0062, ADR-0063, ADR-0066,
ADR-0070, ADR-0092.
