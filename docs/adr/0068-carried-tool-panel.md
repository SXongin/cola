# Carried Tool Panels: a restart takeover hands the orphan's running tools to the successor card

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
