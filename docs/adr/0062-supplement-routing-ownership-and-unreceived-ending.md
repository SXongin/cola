# Supplement routing: cola's own live ownership, and the Unreceived ending

## Context

ADR-0059 made the Backend's own liveness the routing key: a user message
arriving while an Execution is live is a **Supplement** — submitted without
starting a competing Turn, the chain splitting below it — and the read that
answers "is an Execution live" is `session_has_live_execution`: cola's in-flight
guard first, then the Backend's status read (`/api/session/active` on V2, a
process-local map). That read was treated as a fact about the future: "live"
meant a runner would merge the message at its next step boundary.

The live incident of 2026-09-29 (#428) showed the read can be stale in a way
that strands the message. A session moved itself into a git worktree mid-turn
(`session_move`; the user had asked for the worktree). The move interrupts the
in-flight step — the runner at the old location fails its own location check at
the next attempt — yet the server's active map kept the session live for 1h43m,
until the location was evicted. A message admitted in that window was durable
(`session_inbox`, `delivery:"steer"`) but had no runner left to promote it:
steers are promoted only at the start of a run attempt. cola read "live", took
the Supplement path although it owned no card — the card that had been
rendering had died with a `/restart` — and `split_card_chain`, whose whole job
is to split a live card, found nothing to split. No card, no receipt, nothing
on Feishu: the message was silently absorbed. The frozen card half of the
incident is ADR-0063's.

The stale read is not a freak accident: the active map is process-local and
lossy (ADR-0059's own considered options said so), and a run can outlive its
runner through an interrupt, a move, or a location eviction. Second, even a
correct Turn could not end honestly in that state: a Turn whose submitted
message never reached the transcript settled through `settle_or_confirm`'s idle
confirmation window — the pre-#436 rule, since retired — as Done, stamping ✅
over a message nobody executed.

## Decision

**Ownership is the routing key.** A message is a Supplement only when cola
itself owns the Session's live Execution: the in-flight guard its Turn holds,
or the follow that inherited it for its whole window — or a card chain a live
renderer owns (the busy-adopt snapshot follow's external render, a Wake
continuation), which owns its card without holding the guard: the same
`inflight || card_is_owned` pair the Wake step reads. The Backend's status read
stays — as an advisory fact for the card's opening line and the waiting hint —
but it never routes. With no owned card chain, a message starts a new Turn,
whose V2 prompt still carries `delivery:"steer"`: a genuinely live run merges
it exactly as a Supplement would (no competing run, no second accumulator),
while a stale read gets a card that owns the message and settles it. #409's
cases are unchanged — the follow holds the guard.

**An unanswered submit ends Unreceived, never Done.** When a Turn's own message
never appears in the transcript (`anchor_of_user` absent) and the Session is
not live, the card ends 「⚠️ 这条消息未被接收」 with a 「重新发起」 action —
never ✅. While the Session reads live but cola's own message is not yet in the
transcript, the card carries a neutral 「⏳ 等待当前运行接收…」 line once a long
grace passes (the follow grace, ten minutes): a genuine long tool call is
indistinguishable from a stale run until the session idles, so cola does not
nag early and never claims the run is dead while it might simply be busy.

**重新发起 resumes.** The action interrupts (a no-op when already idle) and
resumes the Session, so a steer queued in the inbox is promoted at the new
run's start; V2 gains the `POST /api/session/{id}/resume` write. The button is
the user's consent — cola never interrupts a run on its own, because a long
tool call and a dead run look identical from outside.

The new-Turn card opens with 「📨 已收到，将并入当前运行」 when the advisory
read said live, and an ordinary loading card otherwise.

## Considered options

- **Keep server-read routing, add a visible receipt and an observer.**
  Rejected: the receipt still needs an owner that settles it, and the Turn
  machinery already is that owner; the steer delivery makes a Turn safe against
  a genuinely live run.
- **Auto-nudge (interrupt → resume) while live-but-unpromoted.** Rejected:
  indistinguishable from a genuine long tool call; interrupting another
  client's run needs consent.
- **Auto-resume on idle-without-promotion.** Rejected for now: silently running
  an hours-old message; the button keeps the user in control of when it runs.
- **Settle Done at idle regardless.** Rejected: a ✅ over an unexecuted message
  is a lie.

## Consequences

- ADR-0059's "Routing key: live Execution" is superseded; its considered-option
  note that `/api/session/active` is process-local and lossy is now a
  load-bearing fact rather than a footnote.
- CONTEXT.md's **Supplement** is redefined on ownership, and gains
  **Unreceived Message**.
- The advisory read cannot regress #409: every #409 case holds the guard. The
  external snapshot follow (ADR-0028) holds no guard, so it is covered as a
  render-owned chain instead — a message during it still splits that chain.
- Tests pin: a stale live read with no owned chain starts a Turn, not a
  Supplement; a never-promoted submit at idle ends Unreceived, never Done; the
  waiting hint appears only after the grace; 重新发起 resumes.

Related: #428, #409, #405, ADR-0043, ADR-0056, ADR-0059, ADR-0063.
