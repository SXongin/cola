# Permission targets beyond the card

## Context

A permission request whose target cannot be shown in full on a card used to be
refused **before any surface** (`PermissionKind::refuse`): `reject` settles it
server-side, so no client can approve it afterwards. #1009 asked for a way to
approve the text anyway; #1010 is the parent spec, and #1012 is the interactive
half. #892 / ADR-0101 already removed the refusal for Auto-Accept sessions.

The budget is the card's wire cost, not a readability preference: every button
used to echo the body. #1011 moved the body out of the callback and made the
completeness proof a single predicate, `PermissionOverflow` — the summed
`PERMISSION_TARGET_MAX_CHARS` display budget or the rendered-body
`PERMISSION_BODY_MAX_CHARS` render invariant. This decision supersedes
ADR-0101's remaining clause that an interactive surface still declines an
over-budget target; its Auto-Accept decision is unchanged.

The parent spec directs this ticket to reuse **ADR-0083 §5**'s existing
Rich Text Message → File Message ladder as the one overflow mechanism. That
lane's own charter says "by what the content *is*, not how big it is"
(`GLOSSARY.md` calls the Rich Text Message "deliberately not the overflow lane
for size alone"). Using it here is a deliberate bend, not a new lane: the
mechanism, its 30 KB body cap and its File Message fallback are unchanged; only
the TRIGGER differs (a permission target past the card budget, which #1010
explicitly assigns to this ladder). The GLOSSARY and user-guide wording are
#1014's work, not this ADR's.

Surface persistence is ADR-0038's: a restart re-adopts the card a previous
process surfaced, and the persisted state is client-scoped (ADR-0091).

## Decision

The completeness proof's authority is the **surface**: an over-budget approval
is allowed only when the full content was delivered for that request.

- **No refusal on the way in.** `PermissionKind::refuse` no longer declines an
  over-budget fresh request. The sweep surfaces it with a summary. The refusal
  path remains for the shapes that must not be offered: a companion that could
  not be delivered, and a recovered card with no companion record.
- **One proof, two renderings.** The summary card, the inline block and the
  Interaction Receipt share one summary form: the action, the summed target
  length, the first 120 characters of what the proof measures, and where the
  full content lives. The full description stays for in-budget requests.
- **ADR-0083 §5's ladder.** The full content goes out as one Rich Text Message
  (`post` with an `md` tag); a body past the 30 KB `post` cap, or a send that
  definitely did not land, falls to one File Message (an uploaded `.txt` whose
  name and preceding caption name the request). An ambiguous send is terminal:
  it is never retried into a duplicate, but it is **not** a delivery. It leaves
  a non-proof marker and the request follows the refusal path; the copy names
  the unknown outcome instead of claiming either delivery or a definite failure.
- **Delivered before interactive.** The companion is sent from the sweep's
  surfacing path before the inline block or standalone card is created, so there
  is no window in which a button approves text the user never saw. It is sent
  once per request, whichever surface hosts it. Auto-Accept returns before this
  step: it renders no card and sends no companion (ADR-0101 unchanged).
- **Delivery is persisted; uncertainty is only a marker.** A client-scoped
  `request_id → CompanionDelivery { message_id, lane, target digest }` record
  rides beside the existing surface records (ADR-0038). A **confirmed** send
  writes the record; an ambiguous send writes the same shape with an empty
  message id, which only prevents a second send. The record is removed when the
  request resolves and hydrated at startup. Readoption trusts a confirmed record
  whose digest matches and never re-sends; a marker never proves anything.
- **The click guard checks it.** An `once`/`always` click re-reads the pending
  request and accepts an over-budget approval only when a **confirmed** companion
  record exists whose digest matches that request. A stale record is replaced at
  surfacing time, never trusted; an uncertain marker is refused like a missing
  record, with copy that names the unknown outcome. This covers **any**
  `PermissionOverflow` — the summed target budget and the rendered-body ceiling
  alike.
- **A recovered card without proof is withdrawn, never decided in place.**
  Readoption does not trust a persisted over-budget card that has no matching
  confirmed companion record (including one whose only record is an uncertain
  marker): the card is withdrawn neutrally and the recovered entry is forgotten.
  The request then continues through the **normal fresh path in the same sweep**
  — `prepare` may approve it unseen on an Auto-Accept session (with its usual
  INFO line), the companion lane may deliver and surface the summary, or a
  failed companion follows the refusal path. The old decision is never silently
  taken over (ADR-0101), and a card from an older build cannot approve text a
  newer build never delivered.
- **Fallback is out of scope here.** If neither lane delivers, this ticket keeps
  the existing refusal path. A fallback card is a later ticket and this ADR does
  not pre-decide its controls or confirmation steps.

## Consequences

- A huge command is readable in the chat, stays inside the card's JSON budget,
  and can be approved where it was seen. The transcript keeps the full content.
- The proof depends on persisted state, which is why the record is client-scoped
  and versioned with the other surfaces (ADR-0091). A lost or corrupt record
  fails closed: the click is refused, never approved unseen.
- The digest is over the exact text the companion carries — for a target
  overflow, the raw targets; for a rendered-body overflow, the full rendered
  description including its non-diff chrome. A request whose text changed while
  its record stayed behind cannot borrow the old delivery.
- An ambiguous send is recorded only as a no-duplicate marker (empty message
  id) and never retried. It is not a completeness proof: no interactive
  approval is offered, the request follows the refusal path, and the copy names
  the unknown outcome.
- The refusal copy, the reason card and the retry behaviour stay in use for a
  failed companion delivery, an uncertain one, and a legacy card without a
  record.
- A target with only a chat reply target (no message to reply under) cannot take
  either companion lane, so it follows the refusal path. This is safer than
  offering an approval for an undelivered target.
