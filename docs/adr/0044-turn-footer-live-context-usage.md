# Turn Footer: live context usage on streaming cards, standalone interaction cards excluded

ADR-0019 made the 📊 context-window segment final-card-only, and its
2026-09-12 amendment repeated the premise that usage "is genuinely computed
only at turn end". That premise is wrong: OpenCode writes token usage at every
step boundary, and cola's render poll already reads it. The segment now
advances with the turn (each completed step), on every card of a
streaming-card chain, formatted `📊 上下文 used/window (percent)` — degrading to
the used tokens alone when the window size is unknown. Standalone
Permission/Question cards and session snapshot cards never carry the Turn
Footer.

## Context

- **Usage lands at step-finish.** OpenCode sets `info.tokens` when each model
  call completes (`Session.getUsage` in `processor.ts`; v2 projector
  `session.next.step.ended`), not continuously during a step. The render poll
  already reads every assistant message's tokens into the accumulator
  (`render_new_turn_parts`), so the numerator costs nothing extra.
- **An interaction arrives mid-step.** A Permission/Question is raised by a
  tool call before that step's usage is written, so the freshest value at the
  decision moment is the last completed step's context — under any refresh
  policy. The choice is only whether the number is visible the rest of the
  time.
- **The denominator is a lookup.** `model.limit.context` comes from
  `GET /provider`; it can be missing (custom provider, unknown model, failed
  request).
- **The user's need.** Decide how to answer a question or whether to grant a
  permission, and judge how much context space remains before sending more
  work.

## Decision

- The context segment renders on every card of a streaming-card chain — live,
  interaction, and finalized 「部分完成」 slices — once a value exists, exactly
  like the model segment; `include_tail` stops gating it.
- It refreshes whenever the accumulator's token usage changes (each completed
  step the poll observes) and at turn end. The window size is memoized per
  provider/model for the turn, so polling adds no repeated `GET /provider`.
- Format: `📊 上下文 84k/200k (42%)`. When the window is unknown, the used
  tokens alone: `📊 上下文 84k`.
- Standalone Permission/Question cards (no live accumulator: after a restart,
  or an unfollowed external turn) and session snapshot cards never carry the
  footer. This exclusion is permanent, not deferred.

## Why

- The refresh is free: an active turn's card is already PATCHed about once per
  second by the header phase timer, and interactions force their own flush; the
  only new work is the memoized window lookup.
- Always-live subsumes refresh-at-interaction: the number shown at a permission
  is the same last-completed-step value either way, and always-live also shows
  compaction drops and lets the user plan the next prompt.
- One computation site (the shared render path) instead of a hook at every
  interaction source (permission poller, question poller, external flow,
  re-host).

## Alternatives considered

- **Interaction-only refresh**: the same number at the decision moment,
  invisible otherwise, and it needs the computation wired into several
  interaction call sites. Rejected.
- **Final-card-only (status quo)**: the user answers permissions without the
  information they need; the premise that the value doesn't exist earlier was
  wrong. Rejected.
- **Percent only**: less actionable for "can I paste this file". Rejected in
  favor of `used/window (percent)`.
- **Standalone/snapshot coverage**: a rare path needing its own messages fetch
  and data plumbing; excluded permanently by product decision.
- **Warning-threshold display (show only at ≥N%)**: hiding the value until it
  is nearly full withholds planning information that is cheap to show
  continuously. Rejected.

## Consequences

- The ratio can tick up between cards; after compaction it drops — an honest
  signal that the context shrank.
- A split 「部分完成」 card also carries the ratio, consistent with its model
  line; a card that knows neither tokens nor window shows no segment.
- The turn-end computation in `turn.rs` stays the authoritative final value;
  mid-turn refreshes share the same memoized window.
- Tests: the assertion that the ratio must not appear mid-turn flips, and the
  footer format assertions update.
- ADR-0019's amendment sentence "The 📊 context-window ratio stays
  final-card-only" is superseded.

## Domain note

The **Turn Footer** glossary entry is updated; no new term enters.
