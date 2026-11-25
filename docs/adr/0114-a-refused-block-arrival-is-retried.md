# A refused block arrival is the render pass's one exception

> **Amends ADR-0038** (its 2026-10-31 freeze, "a live form freezes the render
> pass") and **ADR-0100** (the same rule restated there): the freeze stands, but
> it no longer guards a box the card does not show. Everything else those ADRs
> record — one form, one open question, the draft rules, the signature cache —
> is unchanged.

## Context

ADR-0038's freeze exists because Feishu rebuilds `body.elements` on every card
update and a form input's unsubmitted text lives only in the client. It says a
card rendering an unanswered typed answer is not written by the render pass at
all.

That rule has one premise — the card **renders** the box — and one gap. A live
interaction block is written exactly once: the flow inserts it and flushes
immediately (`Turn::add_question`, `src/bridge/turn/interaction.rs`). Every
later pass stands down because the block renders a box. So if that single write
is refused, nothing ever writes the block again: the card keeps its previous
body while its header already says 「等待你的回答」, and the user sees the
question's prose with no button, no input and no confirm.

Live (issue #1103, 2026-11-20, `cola-2026-11-20.log`): Feishu's per-message
update window was saturated (fifteen `230020` refusals in thirty seconds), the
question block's arrival PATCH came back `230429` (24018 bytes, 82 components),
and the block never landed. The identical card renders correctly — the payload
and the renderer were never at fault; the refusal was.

Two facts made it worse. The frequency limit is a **business code inside the
response body** (`230020` on a 400, `230429` on a 200), so it reached the
bridge as a bare `Feishu(String)` and was logged — and accounted — as a
"permanent refusal". And the user could not recover it themselves: the card
offered no control to click.

## Decision

**1. The frequency limit is its own, retryable refusal.**
`BridgeError::FeishuRateLimited { code, detail }` is typed at the client
boundary, on both the 400-body and the 200-body shapes; `is_rate_limited()` names
it. `is_recoverable_card_write()` still answers **false** for it, deliberately:
that predicate means "the delivery layer still owes this write", and a direct
card PATCH is never queued there. Reading the limit as owed would claim a retry
nobody holds. The flush's rejection log distinguishes all three refusals
(content refused / rate limited / permanent) from one helper.

**2. A block's arrival carries a bounded retry, and the freeze reads it.** A
question block's insert arms `BLOCK_ARRIVAL_RETRIES` (4) attempts
(`owe_block_arrival`, `src/bridge/turn/state.rs`) — unless an arrival is already
owed, which joins the remaining budget instead of re-arming it, so a run of
questions cannot hold the freeze open. `awaits_typed_answer()` is
false while an arrival is owed — the freeze's own premise is what the carve-out
reads, so a block the card does not carry does not guard text it does not show,
and the next pass writes it. Every **refused card write** spends one retry
(`spend_block_arrival`, from `note_cursor_write_failure` — the one seam all
three write sites share). A delivered body that **renders** the block settles the
obligation in the write's own `Ok` arm, synchronously, through the budget's
shared handle (`block_arrival_handle`) — before the awaits that follow, because a
cancellation between the PATCH and its bookkeeping must never leave the freeze
lifted over a box the card already shows. A finalized prefix renders no tail, so
its PATCH settles nothing: the blocks stay owed until the continuation carries
them. Spending the budget returns the card to the pre-fix behaviour: the freeze
holds, and a WARN names the request the budget was armed for — the block that
never reached the card, not whichever question happens to sort first.

**3. Typed text wins over a sibling block.** An insertion that lands on a card
already guarding another question's box arms nothing: the flow's own out-of-band
flush still writes the new block (allowed while a box is open), but the retry —
which could repaint the box being typed into — does not arm.

**4. The one legitimate exception is when nothing is on the card.** The carve-out
applies only while the arrival is owed, which is exactly the state in which the
card holds no unsubmitted text.

## Consequences

- A rate-limited block arrival now lands after at most four further ticks (one
  PATCH per tick, no timer, no task, no lock held while waiting) instead of
  never. The 14:18 storm's shape — repeated refusals on one card — still costs
  one attempt per tick, but it no longer leaves a wait card without controls.
- A card Feishu refuses for good (a content rejection) spends the same budget
  and then behaves exactly as before; the bound is what keeps this from becoming
  a repaint loop.
- The budget bounds the card's **refusals**, not the block's own attempts: any
  refused write spends one, because none of them carried the block to the card.
  A turn whose writes are all refused (the incident's shape) therefore spends in
  step with the retry, and a mixed turn can spend a retry or two early — the
  bound is the guarantee, not a per-arrival count.
- The freeze keeps its one hard rule: a card that shows a box is never written by
  the render pass, so a draft cannot be eaten. The carve-out's bookkeeping is
  cancellation-safe in both directions: the budget is spent on a refusal (where
  nothing was written) and cleared by the successful write itself, so the window
  where the card shows the box but the freeze is still lifted does not exist.

## Alternatives considered

- **Make the frequency limit `is_recoverable_card_write() == true`.** Changes
  what `FlushOutcome::Accepted` claims — "the payload is accepted and the card
  converges to it" — for a direct PATCH that nobody retries, and it would not
  release the freeze anyway: the misclassification was the log's story, the
  freeze was the defect.
- **Sleep-and-retry inside the arrival flush.** The flow holds the session's
  card write lock across the flush; sleeping there delays a click's ack. Rides
  the existing render cadence instead.
- **Re-send the block as a standalone card on refusal.** Two live cards for one
  request, and the inline surface's element range would still be stale — the
  retry answers the same question in place.

Source: #1103.
