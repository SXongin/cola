# A terminal write proved unsent stays owed

> **Amends ADR-0067** (the Pending Card Update set also admits a typed frequency
> limit the ending's writer claims), **ADR-0112** (one more required
> `DeliveryObserver` method: `owe_rate_limited_write`), and **ADR-0114**
> decision 1 (a direct card PATCH is still never queued on its own — but the
> writer of a terminal body may queue that body when the limit proves nothing
> was written).

## Context

Feishu reports its per-message update window as a business code inside the
response body (`230020` on a 400, `230429` on a 200, and the app-level
`99991400`). ADR-0114 typed it
(`BridgeError::FeishuRateLimited`) and gave a **live interaction block's
arrival** a wall-clock retry window, but deliberately left
`is_recoverable_card_write()` false: that predicate means "the delivery layer
still owes this write", and a direct PATCH is not queued by the delivery
decorator on its own.

That left the terminal write broken, which is issue #1201. The final render's
PATCH is refused with `230429`; `observe` records a settled tombstone; the flush
reads `refused`, `ending_write_for_patch` answers `EndingWrite::Failed`, the
completion notice is suppressed, and the card keeps an older slice forever. The
reporter's log carries 159 such refusals in one day, one of them the turn's final
render.

## Decision

**The predicate stays honest; the ending's writer queues the body.**

1. **`is_recoverable_card_write()` still answers false for a rate limit.** The
   limit proves nothing was written, but the queue does not owe the write until
   something puts it there. Flipping the predicate would also queue every
   rate-limited **live-block arrival**, and the drain's keyless retry does not
   read the typed-answer freeze — it would repaint a form the Host is typing
   into (ADR-0038/0100, #917). That is the wrong direction: the freeze is
   safety over liveness, and it must not be widened by a retry mechanism.
2. **`DeliveryObserver::owe_rate_limited_write(message_id, card)` claims one
   proved-unsent attempt as the card's keyless Pending Card Update.** It answers
   `Some(seq)` only when the card's **newest** recorded attempt was that
   payload's typed frequency limit and nothing is owed; a later keyless write —
   or an accepted keyed state write, which already tombstones the keyless
   payload — clears the claim (ADR-0067's newest-wins), so an older body can
   never be resurrected over what the card now holds. That includes a keyed
   admission **while the PATCH is in flight**: the observation snapshots the
   keyed-state counter before the request and does not recreate a claim the
   admission just cleared. Any other outcome — no queue, a superseded attempt,
   a different payload — answers `None`, and the caller keeps the permanent
   refusal it had.
3. **The flush claims only the ENDING slice.** The final render applies the
   terminal card state before flushing; a rate-limited PATCH of a body built
   under that state is claimed. A live slice's rate limit changes nothing: its
   next render pass rebuilds and retries it, as before.
4. **The claimed write rides the existing keyless chain.** Its sequence is the
   refused attempt's own sequence, so everything already keyed on it works
   unchanged: `note_cursor_write_failure` ties the staged Rendered Cursor and its
   endings, `ending_write_for_patch` reads `EndingWrite::Owed(seq)`,
   `defer_notice_until_delivered` arms the Completion Notice against that exact
   sequence, and `release_spent` keeps the Chain Record while the payload is
   owed. A rate limit on the **drain's own retry** keeps the payload owed — the
   queue owns it now — instead of settling it as a permanent refusal and
   dropping the armed notice.
5. **The refused attempt stays in the card's failure memory.** A forced drain (a
   WS reconnect) can deliver the claimed body before the flush reads its ending;
   the ending must then still answer for **that payload's own sequence**. The
   limit's attempt is recorded exactly like a recoverable failure, so
   `failed_card_write` + `card_write_delivered` confirm the staged cursor and
   the notice gate resolves to `Delivered` instead of reading `Failed` over a
   body that did land.

**Invariant.** A terminal payload is either delivered or queued; a refusal that
proves nothing was written never becomes a permanent failure. The Completion
Notice is announced only over a delivered terminal body, a queued-but-never
delivered one stays suppressed, and a claim cannot loop into a second
announcement.

## Consequences

- A terminal PATCH refused with `230429`/`230020` is retried by the delivery
  drain after the window, and the Completion Notice fires when that retry
  delivers. The turn's last body is no longer lost to a platform window.
- A limit that never clears keeps the payload owed and the notice suppressed.
  The drain backs off (5 s doubling, capped); there is no spin and no second
  notice.
- The live-block arrival path is untouched. A rate-limited arrival is still the
  render pass's own retry (ADR-0114), not the queue's.
- A platform without an observer answers `None`, so the flush's refusal stays
  final exactly as before.
- Tests pin: the terminal PATCH refused once then delivered → the card ends on
  the terminal body **and** the notice fires; a limit that never clears → the
  notice stays suppressed and the payload stays owed; the queue unit tests cover
  the claim, a second limited retry, and a newer write clearing the claim.

## Alternatives considered

- **Flip `is_recoverable_card_write()` for the limit.** Queues the live-block
  arrival too, whose keyless drain retry is freeze-blind; and it makes
  `FlushOutcome::Accepted` claim convergence for a payload nobody has queued
  yet.
- **Re-submit the terminal body as a keyed `Settle`.** The keyed queue has no
  notice gate and no staged-cursor tie; the notice and cursor contracts are
  keyless-sequence based. Reusing them is the point of claiming the same
  sequence.
- **Sleep and retry inside the flush.** The flush holds the session's card-write
  lock; waiting out a platform window there blocks clicks and the next turn.
  The drain already has a schedule and a backoff.

Related: #1201, #1103, #1204, ADR-0067, ADR-0072, ADR-0112, ADR-0114, #917.
