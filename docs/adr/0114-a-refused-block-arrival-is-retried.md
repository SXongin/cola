# A refused block arrival is the render pass's one exception

> **Amended by ADR-0115**: decision 1's account of the **terminal** write is
> superseded — the limit is still not `is_recoverable_card_write()`, but the
> writer of a terminal body may claim the proved-unsent payload as a keyless
> Pending Card Update, so the ending converges instead of being lost. The
> arrival window and its allow-list below are unchanged.

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

**2. A block's arrival has a wall-clock retry window, and the freeze reads it.** A
question block's insert opens a window of `BLOCK_ARRIVAL_WINDOW` (45 s,
`owe_block_arrival`, `src/bridge/turn/state.rs`) — a duration, not a tick count,
because the refusals it must outlast are Feishu's per-message update window (the
incident ran ~30 s) and a tick count would make the guarantee depend on the
render cadence. `awaits_typed_answer()` is false while the window is open and
unexpired: the freeze's own premise is what the carve-out reads, so a block the
card does not carry does not guard text it does not show, and the next pass
writes it.

**The window closes around every write that can carry the block**
(`close_arrival_window`, before the live PATCH and the continuation create, and
only when that body rendered a live question block). If the write lands — or a
cancellation swallows its outcome — the freeze is already engaged over the box
the card may now show, so no later pass can repaint it. A body that renders no
tail (a finalized prefix, an intermediate continuation slice) neither closes nor
re-opens the window: its blocks live on the next card.

Re-opening is an **allow-list**, because only an answer Feishu itself returned
proves the body was not written: a typed content rejection, a typed frequency
limit, a business refusal, or a 4xx other than 408. Everything else — transport,
timeout, 5xx, a 408, and a 2xx whose body failed to parse *after* the PATCH was
applied (typed `FeishuParse`, never a business refusal) — is read as "may have
landed" and leaves the freeze closed. When the window expires with the block
still owed, the card keeps its previous body and the predicate that closes the
freeze reports the expiry once, naming the request the window was armed for (no
write runs after that point).

**3. Typed text wins over a sibling block.** An insertion that lands on a card
already guarding another question's box arms nothing: the flow's own out-of-band
flush still writes the new block (allowed while a box is open), but the retry —
which could repaint the box being typed into — does not arm.

**4. The one legitimate exception is when nothing is on the card.** The carve-out
applies only while the arrival is owed, which is exactly the state in which the
card holds no unsubmitted text.

## Consequences

- A rate-limited block arrival now keeps retrying, on the existing ticks (no
  timer, no task, no lock held while waiting), for as long as Feishu's own update
  window can plausibly last, instead of never. The 14:18 storm's shape — repeated
  refusals on one card for ~30 s — stays covered by the 45 s window.
- A card Feishu refuses for good (a content rejection) retries until the window
  expires and then behaves exactly as before; the window is what keeps this from
  becoming a repaint loop.
- The window is time, not attempts: its length was chosen from the observed
  storm, and every refused write inside it costs one PATCH on the tick that was
  going to run anyway.
- The freeze keeps its one hard rule: a card that shows a box is never written by
  the render pass, so a draft cannot be eaten. The carve-out is only ever open
  when the card provably does not show the box: a definite refusal spends, an
  ambiguous failure or any delivered carrier closes it, and settlement rides the
  successful write's own arm. Safety over liveness — a stranded block is visible
  (and the WARN names it), an erased draft is not.

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
