# Carried Tool Panels: a restart takeover hands the orphan's running tools to the successor card

> **Amended 2026-10-05 (#527), by ADR-0071 and ADR-0072**: the carry's bounded
> newest-first read and its page cap retired into the **Rendered Cursor**'s
> seed, and the cursorless fallback now resolves the orphaned Turn's still-live
> calls from the render's own transcript read, scoped by the membership rule
> itself. The predecessor keep rule the takeover collect once recorded on the
> successor's record retired with the keyed ordering; the collect's strip rule
> is its own payload composition. The panel semantics — display-only, settles
> exactly once, the collect's strip — stand.

## Context

ADR-0045 made an unfinished **Tool Panel** card-tail live content: it rides
every split onto the newest card of the **Card Chain** and joins the timeline
when the tool settles. A cola restart kills the accumulator, so the tail has no
carrier: the card that was live stays frozen (ADR-0063's reap settles its
*state*; the #443 stamp names the restart while the Session still reads live),
and a user message arriving afterwards starts a new Turn (ADR-0062) whose card
collects the old one — but the in-flight tool panel moves to neither card
(#434, #444).

A running call inside the membership rule's in-flight window (ten minutes)
already renders onto the successor through the normal transcript render.
Outside it — a long-running command, the issue's own example — the call was
dropped for the whole new Turn, and the takeover's collect preserved the old
card's body best-effort, so its `⏳` marker stayed behind. #505 owns the faithful
no-duplicate/no-omission restore of *missed* content; this decision covers only
the running tail at the takeover moment.

## Decision

**A fresh Turn's takeover carries the orphaned Turn's running tool calls onto
the successor.** The successor stays the fresh reply card the new Turn already
sends — never a split-style continuation of the old chain, whose accumulator is
gone and whose continuation semantics (pending split, render boundary) have no
object. The takeover seeds the successor from the record's **Rendered Cursor**
(ADR-0071): the call ids whose newest delivered state was `running`, resolved by
call identity — never the panel's sequence-numbered `element_id`, which a later
transcript render could not dedupe against — against the whole Session
Transcript on every render read: display-only while the call still runs, joining
the successor's timeline exactly once at settle, at the server start key the
call was born with. Those are the same slots ADR-0045's running panels use. A
record with no cursor keeps the carry's fallback semantics: the orphaned Turn's
still-live calls, nothing replayed.

**The seed is scoped to the orphan Turn's projection.** Membership is the
`belongs_to_turn` rule anchored on the record's anchor (its `time.is_some()`
guard plus `TurnAnchor::may_still_belong`):

- a message created at/after the anchor's `created_ms` always may belong;
- a **completed** message may belong iff its completion is at/after the anchor;
- an **unfinished** message may belong while its newest activity is at/after the
  anchor's in-flight boundary;
- a message with **no server time** cannot be placed, so it never belongs to the
  projection — but it is no evidence about older messages either, so the
  membership rule answers "may still belong" for it.

A record with no anchor carries nothing. This is what keeps an older card's
stale running part from being resurrected as live on an unrelated successor: a
killed run leaves its `running` part behind (#378 verified), so a carry scoped
to "every running call in the transcript" would misreport it.

**The cursorless fallback resolves the orphaned Turn's still-live calls from
the same transcript read the render polls.** The carry's own newest-first
keyhole read, its descending scan and its page cap retired into ADR-0071's seed,
so no second read happens: `CursorSeed::live_calls_only` reads the standard
transcript and scopes `turn_running_tools` by the membership rule above,
excluding the plan carrier (the successor's own reads rebuild the plan from its
neutral fact). A read truncated at the V2 page cap still carries the live calls
it sees — nothing about a cursor is resolved — while a failed or timed-out read
carries nothing, and the common case is one standard read rather than a history
walk of its own.

**The carried calls reconcile against the transcript, not the Turn window.**
Every render read resolves each carried call identity against the transcript and
applies its current status and output; when the tool settles, the panel leaves
the tail and joins the successor's timeline at its server start key (ADR-0045,
the late-part clamp). A carried call is display-only: it does not extend the new
Turn's anchor, its durable record, or its settle decision, and it is never an
adoption of the old run.

**The old card is collected without the live tail the successor takes over.**
The takeover's collect drops two things from the preserved body. The
**Background Task Ledger** (its stable `task_ledger` element) goes always: the
successor's own reads rebuild the live list, so ADR-0060's handover rule —
exactly one card carries it — applies to the restart collect exactly as it does
to an in-process supersede. The **running tool panels** (`collapsible_panel`s
whose plain-text title begins `⏳`) go only for the calls the seed actually
resolved — carried still-running, or settled while cola was down and joined once
— so no frozen marker remains on a collected card, while a call the read did not
carry keeps its panel as a frozen witness. A failed or timed-out read resolves
nothing and strips no tool panel, leaving today's collected body for them. This
is ADR-0063's body-preservation rule narrowed for this one collect, and a failed
old-card PATCH is the same best-effort degrade as every collect.

**Only the fresh-Turn takeover carries.** A Wake continuation keeps ADR-0061's
no-replay scope at the newest Wake, and the reap's endings keep preserving the
body as today (a #443 stamp targets a Session that may genuinely still be
running, so dropping its panel would lie; a terminal ending's frozen panel is
#505's faithful-restore question).

**No new durable state.** The call identity is the identity, the transcript is
the content, and the durable fact is the Rendered Cursor on the Chain Record
(ADR-0071), not state about a carry; nothing about a carry survives the process.

## Consequences

- A restarted, taken-over run keeps one live card: the successor shows the
  running panel, its completion appears there exactly once, and the old card is
  collected as taken over without a stale marker.
- #505's open problem "live tool panels crossing the watermark" is answered for
  the takeover case; its watermark remains the answer for missed content.
- A generation whose transcript read cannot surface running calls carries
  whatever it reports — possibly nothing — and keeps today's behavior; no
  generation-specific branch is added.
- Boundaries: an older card's stale running part is never carried; a call
  outside a truncated read keeps its panel as a frozen witness; a failed carry
  read degrades to today, the ledger still dropped; the already-working in-window
  case stays a regression test, and the settled-while-down probe is #505's.

Origins: #428, #434, #443, #444, #505, #527; ADR-0045, ADR-0060, ADR-0061,
ADR-0062, ADR-0063, ADR-0071, ADR-0072.
