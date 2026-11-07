# Auto-Accept approves an unshowable target

## Context

A permission request whose card cannot be rendered in full — its targets summed
past the display budget, or the rendered body past the measured single-element
ceiling — cannot be approved safely. (When this decision was recorded the two
budgets were per-command and per-pattern constants; #1011 replaced them with
`PERMISSION_TARGET_MAX_CHARS` / `PERMISSION_BODY_MAX_CHARS`, and moved the body
out of the button callback.) Since #883 the bridge refuses such a request
**before any surface**: `PermissionKind::refuse` declines it with `reject` and
posts a card that explains why. The rule exists so no button can approve text
the card clipped.

The rule was extended to Auto-Accept as well: `approve_pending_for_session` and
the sweep's `refuse` declined over-budget targets even for a session that had
opted into "answer every permission without asking me".

That extension was wrong, for a reason the card rule does not have:

- Auto-Accept renders **no card**. The completeness proof has nothing to guard —
  the session explicitly delegated the decision.
- The decline is **final**. `reject` settles the request server-side, so it
  leaves the pending list: no other client can approve it afterwards, and a
  model that re-emits the same long command (a heredoc, a `python3 -c` script,
  a generated one-liner — all routinely past 1,000 characters) loops on denials.
- The user who hit it had already opted out of reviewing each request; the
  refusal bought no review and only removed their ability to say yes.

Live report in #892: an auto-accept session lost a 5,397-character command and
had no path to approve it from any client. The explanation card's advice ("请在
原始客户端中确认后重试") was not actionable, because the request no longer
existed there either.

## Decision

The completeness proof is a property of the **card surface**, not of permission
decisions. It stands aside wherever no card is rendered:

- `PermissionKind::refuse` checks for an Auto-Accept session only *after* the
  target proves unshowable, and returns `NotApplicable` instead of declining.
  The ordinary path pays no extra session lookup.
- `approve_pending_for_session` no longer declines over-budget requests; they are
  approved with the rest. Every enable path — the permission-card toggle,
  `/autoaccept on`, and the Auto-Accept toggle card — persists the flag
  **before** that approval pass, so a sweep landing inside it reads the session
  as auto-accept instead of declining the request the call is about to approve.
  A persist that did not take effect approves nothing: the permission-card
  toggle releases its click claim and reports the failure rather than claiming
  the mode; the other two refuse to run the approval pass. The store rolls a
  failed blocking write back before returning its error, so memory never keeps a
  flag the user was told did not persist.
- A flag read the parent chain cannot finish (a failed `session_info` hop) is
  **unknown**, not "off": the request stays pending and unmarked, and the next
  sweep retries. Reading it as off would decline the request for good, which is
  the irreversible outcome this ADR exists to remove. The 8-hop cap is different
  — a structural bound, not a failed hop — so it falls back to the interactive
  rule: a showable target surfaces, an unshowable one declines (retrying a
  permanent cap would hang the turn forever).
- Every such approval is recorded at INFO with the request id, the summed target
  length, the rendered content length, the budget it crossed, and the target's
  first line (bounded). With no
  card, that log line is the audit record of what the session approved unseen.
  INFO is what ADR-0048 assigns an auto-accepted request; the "unseen" wording
  distinguishes this case. The approval pass takes its own request span, so the
  line carries `session=` even under `/autoaccept on` (whose ambient span is the
  inbound message's).
- A failed auto-approval reply keeps the request pending and unmarked
  (`Prepared::Retry`), so the next sweep retries it. Returning "handled" on the
  error would retire a still-pending request with no card at all. The retry
  failure follows ADR-0048's warn-once cadence (WARN, then DEBUG for identical
  repeats, INFO on recovery).
- An interactive surface is unchanged: `refuse` still declines an over-budget
  target, the click-time guard still refuses to approve one, and the refusal
  copy now names the two paths that exist — ask the model to split the
  operation, or enable Auto-Accept and retry.
- `/autoaccept`'s help, the user guides, the toggle's result card and the
  Auto-Accept receipt no longer claim over-budget targets are declined.

## Consequences

- Auto-Accept now really means what its name says: every permission is answered
  unseen, and an over-budget target is no exception. The trade is visible in the
  log rather than prevented by a refusal.
- A restart still re-adopts a card a previous process surfaced before the
  toggle was on (ADR-0038): Auto-Accept does not silently take over an
  already-offered decision. An over-budget card an older build could have
  persisted therefore stays unapprovable — its Allow click is refused by the
  click-time guard — which is the legacy state the guard exists for, not a
  path this build can create.
- `refuse` and `prepare` each resolve the flag independently. A toggle flipped
  between them can let `refuse` stand aside and `prepare` decline, so an
  over-budget card is built for one pass; the click-time guard still refuses its
  Allow, so no unseen target is approved. The window is a permission request's
  two backend reads wide.
- Persisting before approving narrows but does not close the read/reject race: a
  sweep that read the flag just before the persist can still call
  `decline_unshowable` after it, and the approval then finds the request gone
  (`NotFound`) — the long command is lost. The window is one list-to-reply round
  trip. Closing it needs a serialization between an enable path and the sweep,
  which this ADR does not take on.
- The card's budgets were later changed by #1011, the lever this decision named:
  every button no longer echoes the body (the click re-reads the request from
  the server), the per-command and per-pattern budgets collapsed into one summed
  `PERMISSION_TARGET_MAX_CHARS` (3,000), and the completeness proof now also
  charges the rendered body against `PERMISSION_BODY_MAX_CHARS` (the measured
  single-element ceiling). This ADR's decision — the proof guards only surfaces
  that render a card — is unchanged; only the budget the proof applies moved.
  Because the renderer's chrome consumes the remaining room, a real near-budget
  bash request (3,000-char command plus `metadata.command`) is refused before any
  surface again until #1012's companion lane surfaces it with the full target in
  a companion message; that handoff is the reason the refused class is accepted
  here rather than expanded.

## Amendment (2026-11-08, #1012): the interactive half is superseded

ADR-0102 removes the interactive refusal this ADR left in place: an over-budget
permission is no longer declined before a card. The sweep surfaces a summary and
delivers the full target in a companion message, and the click guard trusts that
delivery. Auto-Accept's decision here is unchanged — it still renders no card,
sends no companion, and approves unseen with the same INFO line.
