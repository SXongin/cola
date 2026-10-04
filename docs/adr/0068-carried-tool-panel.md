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

**A fresh Turn's takeover carries the orphaned card's running tool calls onto
the successor.** When the takeover finds the durable record naming a different
card, the Turn makes **one bounded Session Transcript read** (before its prompt
is submitted), collects every `running`/`pending` tool call by its call
identity, and seeds those calls into the new accumulator's live tail — the
same slots ADR-0045's running panels use — so the successor renders them as
ordinary live panels.

**The carried calls reconcile against the transcript, not the Turn window.**
Every render read resolves each carried call identity against the transcript
and applies its current status and output; when the tool settles, the panel
leaves the tail and joins the successor's timeline at its server start key
(ADR-0045, the late-part clamp). A carried call is display-only: it does not
extend the new Turn's anchor, its durable record, or its settle decision, and
it is never an adoption of the old run.

**The old card is collected without the running markers.** The takeover's
collect strips from the preserved body every `collapsible_panel` whose
plain-text title begins `⏳` — the running Tool Panel's marker; the panel now
lives on the successor. The removal is gated on the carry read succeeding: a
failed read carries nothing and leaves today's collected body untouched, and a
failed old-card PATCH is the same best-effort degrade as every collect.

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
  completion renders exactly once; the collected old card carries no running
  marker; a failed carry read degrades to today; and the already-working
  in-window case stays a regression test. The probe for a tool that settled
  while cola was down is dropped with a pointer to #505.

Related: #428, #434, #443, #444, #505, ADR-0045, ADR-0061, ADR-0062, ADR-0063.
