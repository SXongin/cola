# The companion message is the completeness proof

## Context

A permission request whose card cannot show every target in full — its targets
summed past `PERMISSION_TARGET_MAX_CHARS`, or its rendered body past
`PERMISSION_BODY_MAX_CHARS` — used to be refused **before any surface**
(`PermissionKind::refuse`). The refusal was final: `reject` settles the request
server-side, so it leaves the pending list and no client can approve it
afterwards. #892 fixed the Auto-Accept half (ADR-0101); an interactive session
still had no way to say yes to a heredoc, a `python3 -c` script or a generated
one-liner it could not see in full.

#1010's requirement is not to raise the card budget — the budget is the card's
wire cost, and every button used to echo the body. The requirement is to keep
the decision surface readable while the **full target still reaches the user**,
and to make the click guard's proof a fact about what was delivered rather than
a constant alone.

## Decision

The completeness proof's authority is the **surface**: an over-budget approval is
allowed only when the full target was delivered for that request.

- **No refusal on the way in.** `PermissionKind::refuse` no longer declines an
  over-budget fresh request. The sweep surfaces it with a summary. The refusal
  path remains for the two shapes that must not be offered: a companion that
  could not be delivered, and a recovered card with no companion record.
- **One budget, two renderings.** The summary card, the inline block and the
  Interaction Receipt share `describe_permission`'s summary form: the action,
  the summed target length, the first 120 characters, and where the full target
  lives. The full description stays for in-budget requests.
- **ADR-0083 §5's ladder, reused.** The full target goes out as one Rich Text
  Message (`post` with an `md` tag and a fenced block); a body past the 30 KB
  post cap, or a failed Rich Text send, falls to one File Message (an uploaded
  `.txt` whose name names the request). No new overflow mechanism is invented.
- **Delivered before interactive.** The companion is sent from the sweep's
  surfacing path before the inline block or standalone card is created, so there
  is no window in which a button approves text the user never saw. It is sent
  once per request, whichever surface hosts it. Auto-Accept returns before this
  step: it renders no card and sends no companion (ADR-0101 unchanged).
- **The delivery is persisted.** A client-scoped
  `request_id → CompanionDelivery { message_id, lane, target digest }` record
  rides beside the existing surface records. It is written only after a
  successful send, removed when the request resolves, and hydrated at startup.
  Readoption trusts it and never re-sends.
- **The click guard checks it.** An `once`/`always` click re-reads the pending
  request and accepts an over-budget approval only when a companion record
  exists whose target digest matches that request. Without a match the click is
  refused and the result card names the missing delivery. This is final: the
  match is the proof, for **any** `PermissionOverflow` — the summed target
  budget and the rendered-body ceiling alike.
- **A recovered card without proof is withdrawn.** Readoption does not trust a
  persisted over-budget card that has no matching companion record: it follows
  the refusal path and the card becomes non-interactive. A card from an older
  build therefore cannot approve text a newer build never delivered.
- **The fallback is a deliberate, narrow exception (next ticket).** When neither
  lane delivers, the next ticket renders a fallback card instead of refusing:
  the summary, the risk copy, and a two-step confirmation for either allow
  button, with deny and Auto-Accept one click. That risk-confirmed click is the
  only approval that may exist without a delivery record. It is recorded here
  so the companion proof is not re-litigated when it lands.

## Consequences

- A huge command is readable in the chat, stays inside the card's JSON budget,
  and can be approved where it was seen. The transcript keeps the full text.
- The proof now depends on persisted state, which is why the record is
  client-scoped and versioned with the other surfaces (ADR-0091). A lost or
  corrupt record fails closed: the click is refused, never approved unseen.
- The digest is over the exact target text, so a request whose target changed
  while its record stayed behind cannot borrow the old delivery.
- The refusal copy, the reason card and the retry behaviour stay in use, but
  only for a failed companion delivery and a legacy card without a record.
- A target with only a chat reply target (no message to reply under) cannot take
  either companion lane, so it follows the refusal path. This is safer than
  offering an approval for an undelivered target and is the shape the fallback
  ticket will make actionable.
