# Auto-Accept approves an unshowable target

## Context

A permission request whose target is longer than the card's display budget
(`PERMISSION_COMMAND_MAX_CHARS` = 1000 for a command, `PERMISSION_PATTERN_MAX_CHARS`
= 150 otherwise) cannot be rendered in full on the card. Since #883 the bridge
refuses such a request **before any surface**: `PermissionKind::refuse` declines
it with `reject` and posts a card that explains why. The rule exists so no
button can approve text the card clipped.

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
  approved with the rest.
- Every such approval is recorded at WARN with the request id, the target's
  length, the budget it crossed, and the target's first line (bounded). With no
  card, that log line is the audit record of what the session approved unseen.
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
  already-offered decision.
- The card's budgets themselves are untouched. The reason they are small is the
  card's wire shape — every button echoes the body, so it appears six times in
  the card's JSON — and that is a separate, interactive-surface problem. Moving
  the body out of the button callback (the click already re-reads the request
  from the server) is the recorded next lever, not part of this decision.
