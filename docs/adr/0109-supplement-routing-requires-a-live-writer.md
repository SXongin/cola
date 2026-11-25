# A live writer owns the Supplement, and a completion Wake resumes in place

> **Amended by ADR-0113 (2026-11-25)**: the route and the Turn's admission are
> now one handshake — the guard claim IS the admission, and a merge-only steer's
> refusal is the typed `SteerOutcome::RefusedNoLiveRun` that re-routes the same
> message instead of dropping it. See the amendment at the end.

> **Amends**: ADR-0043 (a failed Supplement's continuation). **Supersedes**
> ADR-0062 (the card-only arm is no longer a Supplement routing key) and
> ADR-0066 (the mid-resume message's proof is the Wake loop's guard lease);
> their surviving rules are below.

Two questions share one answer — *is anyone in this process actually driving the
Session?* — and this record owns both. A user message is a **Supplement** only
while a held in-flight guard proves an in-process writer; anything else starts a
Turn that owns and settles the message. And a shell/subagent completion **Wake**
resumes a yielded (⏳) card in place, so one user request keeps one card. It
merges ADR-0062 (a stale live read must never absorb a message; the Unreceived
ending), ADR-0066 (in-place Wake resumption) and this ADR's own guard-key
decision; the earlier ADRs are pointers here.

## Context

- **ADR-0059 made the Backend's liveness the routing key.** A message arriving
  while an Execution was live was a Supplement — submitted without starting a
  competing Turn — and the read was treated as a fact about the future: "live"
  meant a runner would merge the message at its next step boundary.
- **#428 showed the read can be stale.** A session moved itself into a git
  worktree mid-turn; the move interrupts the in-flight step, yet the server's
  active map kept it live for 1h43m, until the location was evicted. A message
  admitted in that window was durable (`session_inbox`, `delivery:"steer"`) but
  had no runner to promote it — and cola took the Supplement path with no card,
  so `split_card_chain` found nothing to split and the message vanished with no
  receipt. The active map is process-local and lossy (ADR-0059 said so): an interrupt, a
move, or a location eviction can all leave a run outliving its runner.
- **ADR-0062 fixed that with ownership** — the guard, or a card chain a live
  renderer owns — but **#1056 showed the card-only arm has the mirror failure.**
  A Supplement whose steer failed still split the chain; when the run that was
  supposed to continue the new card was gone, the card stayed non-terminal with
  no renderer, every later prompt routed as a Supplement, `turn/steer` returned
  `Ok` into a run nobody rendered (Codex's adapter falls back to `turn/start`
  when it observes no live turn), and the user saw only 「📨 已收到补充」 until
  the reap collected the orphan. A stale `Busy`/`Retry` read kept the same hole
  open. **Neither the card's display state nor the Backend's status read proves
  a writer. Only an in-process renderer does.**
- **And a Turn could not end honestly** in that state: a turn whose submitted
  message never reached the transcript settled as Done, stamping ✅ over a
  message nobody executed.
- **The continuation model was separate and also wrong.** ADR-0059 gave every
  Wake a continuation card below the user's message, so one request produced one
  card per completion, each opening with the same 承接 line while the folded
  completion entry stayed on the old card. Worse, a continuation with no
  reachable reply target — a lobby, a restart-adopted card — could never
  continue at all (#426).

## Decision

### A held guard is the one Supplement routing key

The guard proves an in-process renderer (a Turn, or the out-of-turn follow), so
a message merges into it and the chain splits. Every other case — a card-only
chain, a stale `Busy`/`Retry` read, an unreadable read — starts a new Turn,
whose card owns and settles the message. Its V2 prompt still carries the merge
delivery, so a genuinely live run merges it exactly as a Supplement would. The
ownership is re-read **after** that awaited read, so a renderer that leases the
guard meanwhile routes the message as the Supplement it is. A card-only chain's
recorded card is **collected before the message is classified**: no guard means
no in-process writer, so that card is exactly the #1056 orphan, and the reap
runs on this message's path instead of leaving the stale card under the new
Turn's card.

The status read stays **advisory**: it picks the new card's opening line —
「📨 已收到，将并入当前运行」 when it said live, an ordinary loading card
otherwise — and the waiting hint, but it never routes. #409's cases are
unchanged (the follow holds the guard).

**Every out-of-turn renderer holds the guard for its window — a
reference-counted lease.** The follow already did. The external snapshot render
loop and the Wake continuation's settle loop hold a per-session lease: the first
holder claims the guard when free, a replacement renderer that starts while the
previous one retires joins the count, and only the last release gives it back. A
guard a Turn or follow already held is not released by a lease's exit; when that
holder releases while a lease is live, the guard **transfers** to the lease. A
message during a live render therefore stays a Supplement; a card left behind
with no loop has no guard.

**A Supplement's steer may not start a run.** `Backend::steer` gains
`may_start_turn` and a `SteerOutcome`: the bridge passes `false`, and an adapter
whose steer can fall back to a start refuses **before** submitting when it
observes no live turn (Codex's `turn/steer`), and a generation whose plain
prompt merges-or-starts checks its own status read and refuses when no run is
live (reporting `Merged` only after a live submit). `StartedTurn` is for a future
caller that can adopt the run. The cost is the notification-race fold — a live
turn the adapter has not observed yet — where the user sees the unconfirmed
notice and can resend: the deliberate trade for never owning an unrenderable
run.

**A failed steer only continues with a writer, and never claims the message was
lost.** When `steer` fails, the merge is unconfirmed. If a guard still owns the
chain, it splits and the continuation carries
「⚠️ 补充消息未确认送达，请留意当前回复」 (`SplitKind::SupplementFailed`); a race
that lost the guard keeps the chain and reports a plain notice — a continuation
would be a live card handed to nobody. The split's admission re-checks the
card's state under the same cards lock an ending is applied with, so a Turn that
settles between the ownership read and the split either stamps the continuation
or is seen as terminal and refuses it. A merged steer whose chain ends in that
window reports 「补充消息已提交，但当前卡片无法继续」, never silently. Every path
that frees the guard settles the card first — a failed stale-mapping recreate
stamps the Error ending before releasing — so no guard loss leaves a
live-looking card behind. Both messages say **unconfirmed**, never undelivered:
an ambiguous Codex response may still have landed and keeps its marker for
reconciliation (ADR-0092), so cola must not invite a resend. The split's admission reports a permanently
refused flush as a failure — the same plain notice follows — while the loading
window's queued split (no card id yet) and a write owed to the delivery layer
stay admitted.

### An unanswered submit ends Unreceived, never Done

When a Turn's own message never appears in the transcript (`anchor_of_user`
absent) and the Session is not live, the card ends 「⚠️ 这条消息未被接收」 with a
「重新发起」 action — never ✅. While the Session reads live but cola's own
message is not yet in the transcript, the card carries a neutral
「⏳ 等待当前运行接收…」 line once the follow grace passes (ten minutes): a genuine
long tool call is indistinguishable from a stale run until the session idles, so
cola does not nag early and never claims the run is dead while it might simply
be busy.

**重新发起 resumes.** The action interrupts (a no-op when already idle) and
resumes the Session, so a steer queued in the inbox is promoted at the new run's
start; V2 gains the `POST /api/session/{id}/resume` write. The button is the
user's consent — cola never interrupts a run on its own, because a long tool call
and a dead run look identical from outside. The same ending reaped onto a card a
restart orphaned shows the copy without the action (ADR-0063): the click's
fixture died with the process, so the user re-sends instead.

### A completion Wake resumes a yielded card in place

**A shell/subagent completion Wake resumes a yielded (⏳) card in place; every
other continuation keeps the chain behavior.** One card per user request: the
completion entry, the resumed content and the eventual ✅ all live on the same
card.

- **Trigger.** The Wake step (`Turn::wake_continuation`) picks the in-place
  handoff iff the chain's card is `Waiting` and the newest placeable Wake — the
  completion whose work the continuation would render — is a shell or subagent
  Wake the card has not yet taken over. "Taken over" is the HANDOFF, never the
  announcement: a yielded card's ledger refresh may already have placed the
  Wake's completion entry while its work was still unrendered, and that entry
  must not read as a handoff. Only a 承接 line's split or an earlier in-place
  resume takes a Wake over. Restart and interrupt Wakes
  (`WakeSource::is_genuine_resumption`), the Wake-less content-diff fallback, and
  a card already at a terminal (✅/❌/⏹) keep the split; re-opening a terminal
  card would misread the ending it recorded.
- **Resumption.** Under the session's card-write lock the same accumulator takes
  one ledger read — the retiring Wake's fixed completion entry lands at its own
  moment and the remaining live list stays — then the card takes
  「🔄 后台任务完成，继续处理中…」 (a new `CardState::Resuming`: render-owned, so a
  mid-resume user message is a Supplement, and non-terminal) and the shared
  out-of-turn settle loop renders the resumed work into that same card. No 承接
  line is written: only the post-restart Fresh card carries one. The completion
  entry is the timeline marker — the record stays where the task lived.
- **Back to waiting, or the true end.** A resumed run that ends with Background
  Tasks still live yields the card back to 「⏳ 等待后台任务」; one that reaches
  the true end settles it there as ✅/❌/⏹.
- **One notice.** An in-place update never notifies. The Completion Notice fires
  at the true end under its existing rules (groups per
  `[bridge] group_completion_notice`, p2p past the long-task threshold; the clock
  is the request's original Turn start) and exactly once: the quiet true end and
  the resumed run's ending are mutually exclusive by card state. The restart
  Fresh card stays notice-free — its own send was the notification.
- **Failure keeps Retry.** The in-place card carries the request's prompt, so a
  failed resumed run ends ❌ with the ordinary 重试, which keeps the notice's
  「可点击卡片上的『重试』」 copy true. A continuation armed without a user prompt
  (the Fresh card and the remaining splits) offers none.
- **The ledger stays.** The live list no longer leaves the card for a completion
  — the resumed run renders on it — so ADR-0060's handover serves only the
  remaining splits (size, supplement, terminal-card continuation, Fresh).
- **Watermark.** The in-place PATCH that carries the completion entry is the
  announcement; it drains the staged Wake Watermark (ADR-0061) exactly as the
  承接 line's send did, so a restart cannot re-post the Wake.
- **Mid-resume user message.** The card is live again, so the Supplement rule
  applies unchanged (ADR-0043): the message steers into the run and the chain
  splits below it, the continuation carrying the receipt line. "Same card" holds
  at the chain level (the same Turn continues), not at the Feishu-message level.
  A message on a merely-waiting card still starts a new Turn and collects the
  card (ADR-0059).
- **External follows.** The external render loop ends by the shared settle
  decision, not on the first terminal step: a complete read asks `settle` —
  `Waiting` stamps ⏳ and stops, `Running` keeps observing until the Wake's
  boundary, `Complete` is ✅, `Failed` is ❌. A completion Wake resumes the
  yielded external card in place by the same handoff, and the resumed run ends by
  the same mapping. No notice fires for an external chain (its 有新消息 card was
  the notification, and the card carries no Turn clock), and a failed resumed run
  ends ❌ with no Retry — the external card carries no cola prompt to re-ask.
  Restart/interrupt Wakes and the Wake-less late tail keep the split; for an
  external chain after its true end (or on a collected card) wakes stay
  unrendered per #408.
- **The Wake-less tail.** Only a genuine resumption may open a new card. A
  Wake-less content diff — content the Backend wrote with no Wake, including a
  same-request tail after a finalize — never produces a 「🔔 已恢复执行」 receipt:
  on an open yielded card it resumes that card in place; on a card past its wait
  (terminal or restart-stamped) it lands on the one neutral residual Card
  (「📄 还有更新」), bounded to one per request by the chain's
  `residual_card_posted` marker, with a `WARN`. This is a defensive floor, and
  the **same-snapshot ending rule** makes it near-unreachable: `Done` is decided
  only on a snapshot that was both rendered *and* shows no new content, so a
  later separate status read can no longer finalize a stale snapshot.
- **The third taker (#1087).** The card that renders the Wake's resumed work
  also takes the Wake over: a shell/subagent completion that lands while its Turn
  is still running is absorbed by that run — its completion entry and the
  resumed parts land on the card the Turn already owns — and the render pass
  marks the Wake handed over. The taker is the completion class only; a restart
  or interrupt Wake keeps its 承接 split. The terminal exception narrows the same
  way: a completion Wake on a terminal card is a fresh continuation only while
  the card does not already show that Wake's work, and the Wake step re-asks the
same content diff as a floor, so a card rendered by a venue that placed no part
still answers honestly. The durable Wake Watermark
  cannot answer this — an evidence ending suppresses the Wake's own completion
  entry (#659) and with it the Watermark, while the work still renders — so the
  handoff mark is the fact.

## Why

- One question — *is an in-process renderer driving this?* — answers both
  routing and continuation: a message cannot be absorbed by a writer that does
  not exist, and a completion cannot be split off a card that is already showing
  its work.
- The in-place handoff removes the per-completion card noise and fixes the
  lobby/restart gap where a continuation had no reply target; the completion
  entry plus the true-end notice carry the visibility the split used to provide.
- The steer contract makes "never owns an unrenderable run" a type-level fact
  instead of a caller convention.
- The status read stays useful for copy without ever deciding ownership, which
  is the failure both #428 and #1056 shared.

## Consequences

- ADR-0059's "Routing key: live Execution" and its continuation default are
  narrowed; ADR-0060's freeze carve-out widens (a yielded card may receive
  resumed content); ADR-0062's card-only arm and its ownership pair are retired
  as routing keys. Those ADRs carry their banners.
- `CardOwnership::routing_label` stays as the Wake gate's live-chain probe; the
  prompt router uses `holds_guard`. A direct `Turn::run` (a `/retry`, a
  low-level seam) is refused while an external/Wake renderer holds the guard.
- A card-only chain whose renderer has just exited may start a Turn for a message
  the renderer could have split; the renderer owns no run then, so the new Turn
  is the safe owner.
- The chat sees one failure for a failed Supplement: the continuation card (with
  a writer) or a plain notice (without one), never both.
- The instant between the router's final ownership read and `Turn::start` is not
  atomic; a renderer retiring or acquiring its lease in that instant is not
  re-routed. Closing it needs a route/admission handshake, deferred to #1086.
- GLOSSARY.md's Supplement, Unreceived Message, Wake, Card, Card Chain, Waiting on
  Background Work and Background Task Ledger entries record the guard key, the
  Unreceived ending and the in-place resumption.

Origins: #1056 and #1055 (the guard key), #1083 (the steer contract), #428 and
#409 (routing at all), #434 (the Unreceived ending), #426 (in-place resumption), #568 (external follows), #602
(the residual card and the same-snapshot ending), #1087 (the third taker).

## Amendment (2026-11-25): the route's decision and the Turn's admission are one handshake (ADR-0113)

ADR-0113 closes this ADR's own deferred consequence — "the instant between the
router's final ownership read and `Turn::start` is not atomic; a renderer
retiring or acquiring its lease in that instant is not re-routed" (#1086).

The guard is still the one routing key, and its holders and leases are
unchanged. What moved is the **admission**: `Turn::try_admit` claims the guard
before the coordinator builds the Turn's context, so a busy claim re-routes the
same message as a Supplement and a renderer's lease taken after the route can no
longer make `Turn::start` refuse a message the router chose a Turn for
(`src/bridge/turn/mod.rs:422`, `src/bridge/handler.rs:2252`). The ownership
re-reads this ADR's implementation placed after the advisory status read and the
orphan collect are gone; the claim taken after them covers both.

A merge-only steer's refusal is now **typed**:
`SteerOutcome::RefusedNoLiveRun` means nothing was submitted
(`src/backend/mod.rs:204`), so the message is re-routed rather than reported.
The router waits a bounded moment for the refusing guard to clear and starts the
Turn that renders the message; when a writer took the guard meanwhile, it
retries the merge so the message joins that run instead of being reported
unsubmitted (`src/bridge/handler.rs:2623`, `src/bridge/handler.rs:2734`). This is what makes
a **restart-armed continuation** safe: it holds a lease rehydrated from durable
state and cannot render a new message, and the signal that it cannot is the
adapter's typed refusal, not the card's display class. A typed refusal therefore
never opens a `SupplementFailed` continuation; that receipt remains for
ambiguous failures and an adapter breaking the merge-only contract. An
ambiguous failure under a lease-only guard keeps its chain too — the lease is
not a writer (`WaitsHandle::owns_render_lease`, `src/bridge/handles.rs`). Every
path submits the message at most once, and the busy notice is spoken only by the
self-admitting callers (an error-card retry, the V1 resume resubmit), never by a
message that then merges.
