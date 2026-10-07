# Carried Tool Panels: a restart takeover hands the orphan's running tools to the successor card

> **Amended 2026-10-05 (#527)**: the bounded tail read's stop rule is now the
> membership rule itself, exactly — the scan stops only at a page whose oldest
> message cannot belong to the orphan Turn under either arm. The Decision's
> stop-rule prose below is superseded on that point (and "reaching the
> boundary" now means reaching that page); see the amendment at the end.

> **Amended by ADR-0071**: the bounded tail read and the carry mechanism
> retire into the **Rendered Cursor**'s seed — a takeover resolves the
> orphaned Turn's calls from the chain's cursor (or, cursorless, from the
> read's still-live calls), and the message-first race seeds its text tail
> too. The panel semantics below (display-only, settles exactly once, the
> collect's strip) stand. See the amendment at the end.

> **Amended by ADR-0072**: the predecessor keep rule the takeover collect
> recorded on the successor's record — for the #443 stamp's post-PATCH repair
> to reproduce this collect's strip — retired with the repair. The collect's
> strip rule is unchanged: its own payload composition. See the amendment at
> the end.

## Context

ADR-0045 made an unfinished **Tool Panel** card-tail live content: it rides
every split onto the newest card of the **Card Chain** and joins the timeline
when the tool settles. A cola restart kills the accumulator, so the tail has no
carrier: the card that was live stays frozen (ADR-0063's reap settles its
*state*; #443's stamp names the restart while the Session still reads live), and
a user message arriving afterwards starts a new Turn (ADR-0062) whose card
collects the old one — but the in-flight tool panel moves to neither card. The
live signal is split across the two (observed in #434's acceptance round, PR
#442; issue #444).

The probe round (2026-10-04, #444) pinned what today's reads already do. A
running call inside `belongs_to_turn`'s in-flight window (ten minutes) already
renders onto the successor through the normal transcript render — no transfer
needed. Outside the window (a long-running command, the issue's own example)
the call is dropped for the whole new Turn; and the takeover's collect
preserves the old card's body best-effort, so its `⏳` marker stays behind.
#505 (deferred) owns the faithful no-duplicate/no-omission restore of *missed*
content; this decision covers only the running tail at the takeover moment.

## Decision

**A fresh Turn's takeover carries the orphaned Turn's running tool calls onto
the successor.** The successor stays the fresh reply card the new Turn already
sends — never a split-style continuation of the old chain, whose accumulator is
gone and whose continuation semantics (pending split, render boundary) have no
object. When the takeover finds the durable record naming a different card, the
Turn reads the newest end of the Session Transcript (**a bounded tail read**,
below), collects the `running`/`pending` tool calls **belonging to the orphan
Turn's projection** — the membership rule (`belongs_to_turn`, anchored on the
record's anchor) the orphaned card itself rendered under, so an older card's
stale running part is never resurrected as live on an unrelated successor; a
record with no anchor carries nothing — and seeds those calls by call identity
into the new accumulator's live tail, the same slots ADR-0045's running panels
use.

**The read is bounded and newest-first, and happens before the prompt is
submitted.** It pages messages descending (`desc` order, following
`cursor.next` for older pages) and stops only when the page's oldest message's
newest activity predates the orphan anchor minus the in-flight window — past
that point no part can belong to the orphan Turn. A page with no unfinished
message does **not** end the scan: another client can queue messages above the
orphan's unfinished call, so the window boundary — not the page's shape — is
the stop. A small hard page cap and the existing read timeout bound the worst
case; the common case is one request. A read stopped by the cap before reaching
the boundary carries nothing rather than guessing. V1's
session read has no wire pagination — one request decoding the whole message
array — but that is already the read class V1's render polls perform every tick
(1.5 s in production), so the carry adds one standard, timeout-bounded read
rather than a history walk of its own; the wire-level page bound is a V2
commitment, and on both generations a timed-out read carries nothing.

**The carried calls reconcile against the transcript, not the Turn window.**
Every render read resolves each carried call identity against the transcript
and applies its current status and output; when the tool settles, the panel
leaves the tail and joins the successor's timeline at its server start key
(ADR-0045, the late-part clamp). A carried call is display-only: it does not
extend the new Turn's anchor, its durable record, or its settle decision, and
it is never an adoption of the old run.

**The old card is collected without the live tail the successor takes over.**
The takeover's collect drops two things from the preserved body. The
**Background Task Ledger** (its stable `task_ledger` element) goes always: the
successor's own reads rebuild the live list, so ADR-0060's handover rule —
exactly one card carries it — applies to the restart collect exactly as it does
to an in-process supersede. The **running tool panels**
(`collapsible_panel`s whose plain-text title begins `⏳`) go only when the carry
actually handed at least one call over: a failed, timed-out, or cap-stopped
read carries nothing and strips no tool panel, leaving today's collected body
for them. This is
ADR-0063's body-preservation rule narrowed for this one collect (recorded as an
amendment there), and a failed old-card PATCH is the same best-effort degrade
as every collect.

**Only the fresh-Turn takeover carries.** A Wake continuation's arm keeps
ADR-0061's scope at the newest Wake — it never replays the lost chain's
content — and the reap's endings keep preserving the body as today (a #443
stamp targets a Session that may genuinely still be running, so dropping its
panel would lie; a terminal ending's frozen panel is #505's faithful-restore
question).

**No new durable state.** The call identity is the identity, the transcript is
the content, and the live-card record is unchanged; nothing about the carry
survives the process.

## Considered options

- **Carry the read-back view's rendered elements.** The issue's first sketch;
  rejected: a card view returns rendered elements, and a running panel's
  `element_id` is sequence-numbered, not a call identity — a later transcript
  render could not be deduped against it, so a settled result would render
  twice.
- **Widen the in-flight window.** Rejected: it admits the whole stale message,
  replaying its reasoning and text onto the successor, and reopens the
  orphan-replay hole #310/#378 closed.
- **Hand the read + carry to a detached task** (the #443 stamp pattern).
  Rejected: that pattern exists so Session Sync's shared pass is never blocked;
  this is one user event whose prompt submit is the only thing waiting, and a
  detached carry needs an ownership/liveness guard against a Turn that finishes
  first. A bounded synchronous read is simpler and race-free.
- **Read the whole Session Transcript** (the render path's own read).
  Rejected: the V2 read pages through the entire history (up to its page cap on
  a long session), and the takeover's read sits before the user's prompt
  submit — a one-message action must not become a history walk. The
  newest-first tail read keeps the common case to one request.
- **Carry every `running`/`pending` call in the transcript.** Rejected: a killed
  run leaves its `running` part behind (verified in #378), so an unrelated old
  card's stale part could be resurrected as live on a successor. The carry is
  scoped to the orphan Turn's projection.
- **Carry the whole live tail (todo, ledger, interaction blocks).** Rejected:
  the successor's own reads rebuild the todo list and the ledger, and
  interaction blocks are stripped from every preserved body already.
- **Also restore what settled while cola was down.** Rejected for this
  decision: that is #505's faithful restore (the rendered watermark), not a
  tail carry.

## Consequences

- A restarted, taken-over run keeps one live card: the successor shows the
  running panel, its completion appears there exactly once, and the old card
  is collected as taken over without a stale marker.
- #505's open problem "live tool panels crossing the watermark" is answered
  for the takeover case; its watermark remains the answer for missed content.
- A generation whose transcript read cannot surface running calls carries
  whatever it reports — possibly nothing — and keeps today's behavior; no
  generation-specific branch is added.
- Tests pin: a stale in-flight running call appears on the successor; its
  completion renders exactly once; an older card's stale running part is never
  carried; the carry read scans to the anchor-minus-window boundary (a long
  transcript stays a bounded newest-first scan, a queued message above the
  unfinished call does not hide it, and a cap stop carries nothing and strips
  no panel); the takeover collect drops the ledger on every collect — the
  successor rebuilds the live list — and drops the running markers only when a
  call was carried; a failed carry read degrades to today, the ledger still
  dropped; and the already-working in-window case stays a regression test. The
  probe for a tool that settled while cola was down is dropped with a pointer
  to #505.

Related: #428, #434, #443, #444, #505, ADR-0045, ADR-0061, ADR-0062, ADR-0063.

## Amendment (2026-10-05): the stop rule IS the membership rule

The Decision above says the scan "stops only when the page's oldest message's
newest activity predates the orphan anchor minus the in-flight window — past
that point no part can belong to the orphan Turn". That stated invariant was
looser than the membership rule it claimed to mirror, as #527's gate review
found: `belongs_to_turn` ALSO admits a **completed** message whose completion
is at/after the anchor even when every part it carries started before that
window, so a completed message carrying a live `running`/`pending` tool could
be cut off before the scan reached the page below it. The rule is narrowed to
the membership rule itself.

The V2 scan stops at the first page whose OLDEST message cannot belong to the
orphan Turn under either arm — `TurnAnchor::may_still_belong`, the
single-sourced test `belongs_to_turn` now reads:

- a message with `time.created >= anchor.created_ms` always may belong;
- a **completed** message (`time.completed = Some(c)`) may belong iff
  `c >= anchor.created_ms`; the scan stops only when `c < anchor.created_ms`;
- an **unfinished** message may belong while
  `newest_activity_ms(created) >= anchor.in_flight_boundary_ms()`;
- a message with **no server time at all** cannot be placed, so the scan keeps
  going rather than stopping on an unknown (it still never belongs to the
  projection — the membership rule's own `time.is_some()` guard).

The rest of the Decision stands unchanged: the crossing page is kept whole
(older messages are filtered by the projection's membership rule, not by the
scan), a page with no unfinished message never ends the scan by its shape, and
a scan the page cap stops still reports `complete: false` and carries nothing.
"Reaching the boundary" in the Decision above now means reaching that first
page that cannot belong; a cap stop reaches no such page, so it is incomplete
exactly as stated.

Tests pin the exact rule: a page whose oldest message completed at/after the
anchor does not end the scan and the older page's live call is found, while a
page whose oldest message completed before the anchor ends it (complete, one
request, page kept whole); the descending-scan, unknown-time, empty-end-page
and cap-stop tests were re-fixtured to real anchors, and the mock backend
records the anchor itself rather than a derived boundary.

## Amendment (2026-10-06): the carry is generalized into the projection seed (ADR-0071)

The Carried Tool Panel's semantics are unchanged, but its mechanism is: the
bounded newest-first tail read, its page cap and the carry path retired with
ADR-0071, replaced by the **Rendered Cursor**'s live set. A takeover seeds the
successor from the record's cursor — the tool call ids whose newest delivered
state was `running`, resolved by call identity against the whole Session
Transcript on every render read: display-only while the call still runs, and
joining the successor's timeline exactly once at settle, at the server start
key the call was born with. A cursorless record keeps the carry's fallback
semantics — the orphaned Turn's still-live calls, nothing replayed — and the
message-first race additionally seeds the orphaned Turn's text tail (only the
undelivered suffix after the confirmed frontier), so a message that wins
against the adoption shows the run's ending once instead of losing it.

The read the carry needed for its keyhole view — newest-first and cap-bounded
only to stay off the full history — left with it. The seed resolves against
the transcript read the render polls already perform, and the durable cursor
supplies the boundary the read alone could not carry.

The takeover collect's rule is now stated on the seed's own result
(`collect_orphan_after_takeover`): it drops every running `⏳` panel the
successor resolved, whether the call was carried still-running or settled
while cola was down and joined once — no frozen marker remains on a collected
card.

The Decision's "Only the fresh-Turn takeover carries" stands: a **Wake**
continuation keeps ADR-0061's no-replay scope, and the reap's live adoption is
ADR-0071's projection rule, not a Wake arm. "No new durable state" is
narrowed the same way ADR-0061 and ADR-0063 are: the durable fact is the
Rendered Cursor on the Chain Record, not state about a carry.

## Amendment (2026-10-07): the recorded predecessor keep rule retires (ADR-0072)

The takeover collect's strip rule is exactly what the collect composes: the
`keep` rule `collect_orphan_after_takeover` applies to its payload — the
Background Task Ledger always, the running `⏳` panels each call the seed
actually resolved. That is now the rule's only home. Between the #527 fix and
this batch the same rule was ALSO recorded on the successor's Chain Record
(scoped to the predecessor card) before the collect's PATCH, so the #443
stamp's post-PATCH repair could re-collect the orphan under the takeover's rule
when the stamp had landed over it. That recording, its lookup and the repair it
fed retired with the keyed ordering (ADR-0072): the collect and the stamp are
keyed submissions to one per-card queue, so a stamp can no longer land after
the collect that outranks it, and there is nothing left to repair — the rule
lived one object away from the collect that owned it.

The Decision and its 2026-10-06 amendment stand where they describe the
collect's own composition, and "a failed old-card PATCH is the same
best-effort degrade as every collect" still holds. The 2026-10-05 amendment's
observation that the reap's endings — including the #443 stamp — keep
preserving the body also remains true of the stamp: only its ordering
mechanism changed (ADR-0063's amendment).
