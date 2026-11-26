# A proved-unsent split prefix stays owed

> **Amends ADR-0115** decision 3: the flush no longer claims *only* the ending
> slice. A split's finalized prefix renders no tail either, so the same typed
> frequency limit claims it — the ending's terminal card state is not the
> relevant predicate; "this body carries no live block" is.

## Context

ADR-0115 fixed the **terminal** write: a final PATCH refused with Feishu's
in-body frequency limit (`230429` on a 200, `230020` on a 400) is proved
unsent, so the ending's writer claims it as the card's keyless Pending Card
Update and the drain retries it. The claim was gated on the card state being
terminal, because the drain's keyless retry is freeze-blind: a body that renders
a live answer form must never be repainted by it (#917).

A **live** split has the same shape but no terminal state. When a running card
outgrows one card, the flush builds a finalized prefix, PATCHes it, and
continues on a new card. The build advanced `render_from` past the prefix
before the send. If that PATCH is refused with the frequency limit, the live
branch read `is_recoverable_card_write() == false` as a permanent refusal: the
prefix was never re-rendered (the render boundary had moved) and never claimed
(the gate said "not terminal"). The old card kept its last delivered body while
the continuation carried the tail — content the user already generated was
missing from the chain forever (#1208).

The ADR-0115 gate named the wrong fact. A finalized prefix is built with
`include_tail = false` (`build_finalized_handoff`, and the size split's
`build_card_with_info` slice), so it renders **no live block** — the exact
property the freeze-blind drain needs. The terminal state only happened to
imply it.

## Decision

**The claim's predicate is "this body renders no live block", not "this card is
terminal".**

1. `claim_rate_limited_unsent` takes `claimable`, named for that property
   rather than the card state. A rate limit on a claimable body still goes
   through `DeliveryObserver::owe_rate_limited_write` (ADR-0115): only the
   card's newest recorded attempt, only this exact payload, newest-wins against
   a keyed admission.
2. **Both claim sites name the property explicitly.** The plain live slice
   passes `terminal` — a non-full live build renders the tail, so only a
   settled ending is claimable there. The finalized prefix passes `true`
   unconditionally: its build renders no tail whether the split is a size split
   or a Supplement, and whether the card was live or already ended.
3. **The live-block arrival path is unchanged.** A rate-limited body that
   renders a question or permission block is still the render pass's own retry
   (ADR-0114's arrival window), never the queue's. A plain live slice is still
   rebuilt and retried by the next pass, as before.
4. **The claimed prefix rides the existing chain.** Its sequence ties the
   Rendered Cursor and its endings (`note_cursor_write_failure`), and the
   continuation create still re-points the Chain Record to the new card. The
   claim is on the OLD card's message id, so the drain retries exactly that
   card while the chain continues.

**Invariant.** A proved-unsent body that renders no live block is either
delivered or queued; a body that renders a live block is only ever retried by
the render pass, never by the freeze-blind drain.

## Consequences

- A live split whose prefix PATCH is refused with the frequency limit keeps the
  prefix owed on the original card and converges when the window clears. The
  continuation's own send is unaffected; the old card no longer freezes on an
  older slice.
- This also covers a Supplement split (a live split with a forced handoff):
  its prefix is built the same tail-less way.
- The terminal split path is a special case of the same predicate and behaves
  exactly as before.
- A platform with no delivery observer still answers `None`, so the flush's
  refusal stays final — the pre-existing behaviour for every claimable body.
- Tests pin: a live size split whose prefix PATCH is rate-limited stays owed on
  the original card, and the drain delivers the prefix after the window clears;
  the existing terminal split and live-block arrival tests are unchanged.

## Alternatives considered

- **Restore the render boundary on the refused prefix.** The chain has already
  split onto the continuation, so the next build would re-render the prefix
  content *on the new card*, duplicating it while the old card stays stale.
  The content's home is the card that was building it.
- **Keep the terminal gate and let the render pass retry the prefix.** The
  build advanced `render_from` before the send; without a restore there is
  nothing for the render pass to rebuild, which is the bug.
- **Flip `is_recoverable_card_write()` for the limit.** Rejected in ADR-0115
  for the arrival path; nothing here changes that.

Related: #1208, #1201, #1206, ADR-0114, ADR-0115, ADR-0067, #917.
