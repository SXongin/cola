# Route and admission are one handshake

> **Amends ADR-0109**: its Supplement routing key and its "a merge-only steer is
> refused" trade. The refusal becomes a **typed** answer that re-routes the same
> message, the Turn's guard is claimed *before* its context is consumed, and an
> out-of-turn renderer that cannot steer is asked to **yield**. ADR-0109's guard
> key, its `SteerOutcome` contract, its lease rules and its in-place Wake
> resumption stand.

## Context

ADR-0109 made a held in-flight guard the one Supplement routing key and let a
merge-only steer refuse rather than start a run cola cannot render. It left the
router and the Turn reading the guard at different instants, with nothing to
retry the message on the other route:

- a renderer **acquires** its lease after the router's last read: the router
  chose a Turn, `Turn::start` sees the guard and refuses it busy;
- a renderer **retires** with its card already terminal: the steer fails and the
  terminal card refuses the split, leaving only a notice;
- a **restart-armed Wake continuation** holds a rehydrated lease with no run
  behind it (live 2026-11-24): the steer went into nothing, the card split as
  `SupplementFailed`, and the user's question was silently dropped.

Re-reading ownership after each awaited step only moved the window. And the
guard can be held for an unbounded time: the Wake settle loop deliberately waits
for an Execution boundary with no time bound (ADR-0059/#568), so a message behind
it could wait forever — unusable.

## Decision

**1. The guard claim is the admission.** `Turn::try_admit` takes the Session's
in-flight guard and returns an opaque `Admission`, or `None` when another writer
holds it (`src/bridge/turn/mod.rs:422`). The coordinator takes it **before** the
Turn's context is consumed (`admit_turn`, `src/bridge/handler.rs`), so a refused
claim can still submit the same message on the other route. `Turn::start` skips
its busy check when handed an admission (`Turn::claim_start_guard`,
`src/bridge/turn/mod.rs:490`); the self-admitting callers (an error-card retry,
the V1 resume resubmit) keep ADR-0109's refusal and its busy notice.

**2. A merge-only refusal is typed.** `SteerOutcome::RefusedNoLiveRun` means the
adapter saw no run to steer into and refused **before submitting**
(`src/backend/mod.rs:204`): nothing reached the Backend and nothing may still
land, so the message may be routed elsewhere. `Err` stays ambiguous (ADR-0092)
and is never re-routed. Both the generation-neutral default
(`src/backend/mod.rs:409`) and Codex's `turn/steer` (`src/codex/backend.rs:3231`)
answer this way.

**3. A typed refusal re-routes the same message.** `settle_refused_supplement`
(`src/bridge/handler.rs:2749`) resolves it by what actually holds the guard:

- **free**: claim it and run the Turn;
- **a writer** (another message's Turn, or a renderer that began its run): retry
  the merge on a coarse cadence so the message joins that run as soon as the
  adapter can see it. Every try is merge-only, every earlier one refused before
  submitting, and the retry reuses the message's `cola_msg_id`, so this is one
  logical message's first submission;
- **an out-of-turn renderer's lease**: ask it to **yield** (§3b).

The wait has no user-facing deadline — the message's task is the same one a Turn
lives in for minutes, and reporting it unsubmitted while a writer exists is the
loss this record fixes. `SUPPLEMENT_WAIT_WARN_AFTER` (3 s) is a diagnostic: past
it a WARN records an unexpectedly long wait and the poll slows
(`SUPPLEMENT_HANDSHAKE_POLL_COARSE`). A typed refusal never opens a
`SupplementFailed` continuation; that receipt survives for ambiguous failures and
the broken `StartedTurn` contract.

**3a. A renderer's lease is not proof of a writer.** `SupplementFailed` requires
a **Turn's own** guard: a lease can own the guard and have no run behind it, which
is exactly the restart-armed continuation. When only a lease owns the guard, an
ambiguous steer failure keeps the chain and reports the unconfirmed notice
(`finish_supplement`, `src/bridge/handler.rs:2835`); a renderer that joined a
Turn's guard records `owns_guard = false` and still gets its continuation. The
ownership read and the lease read are two acquisitions, so a lease releasing
between them reads as a writer for one instant — strictly narrower than the
pre-handshake race, and the split's own admission re-checks the card under the
cards lock.

**3b. A guard a waiting message cannot use is yielded, not waited out.** Waiting
cannot end an unbounded Wake lease and taking it by force would race the
renderer's own pass. The message asks instead: `retire_render_lease` marks the
lease currently held (`src/bridge/handles.rs`); both out-of-turn loops check it
before every write on every path (`render_or_yield`/`stalled_poll` in the
external renderer, `render_pass` in the shared settle loop) and stop, releasing
their own lease. The request targets the **current window**: the first acquire of
a replacement window clears it, so a stale request never kills a successor
renderer.

Safety comes from the guard's release ordering, not from timing: the lease is
released only after the holder's in-flight pass returns, and the successor's
install needs the guard, so a pass never overlaps a takeover. In addition every
out-of-turn pass is chain-scoped (`render_and_flush_in_chain`,
`src/bridge/turn/render.rs`): it takes the Session's card only while it is still
the pass's chain (`Turn::card_in_chain`), so a pass that starts after the chain
moved writes nothing. The takeover seeds the orphan's undelivered delta and
collects the old card, so a run still producing renders on the successor rather
than being lost.

**4. Every path submits exactly once.** A Turn route submits one prompt; a
Supplement route submits one steer (merged); a typed refusal submits nothing and
then submits the message as either a merged retry or the Turn the yield admits.
No path submits twice. The single exception is the ambiguity rule's own: an `Err`
may have landed and is never resubmitted (ADR-0092).

**5. No spurious busy notice, and no unsubmitted notice.** The busy notice is
spoken only by the self-admitting callers that cannot re-route. A message that
merges or waits through the handshake shows nothing until it is submitted;
ADR-0092's "unconfirmed" wording stays reserved for responses that may have
landed.

## Why

- Route and admission were two reads of one fact; making the claim *be* the
  admission removes the fact's ability to change between them.
- The typed refusal is the adapter's own knowledge ("I did not submit") reaching
  the router; without it, "no live run" and "the response was lost" are
  indistinguishable.
- A lease proves a renderer is in its window, not that a run exists. The yield
  turns that into a cooperative transfer, so an unbounded Wake wait cannot wedge
  a user's session.

## Consequences

- `route_prompt` no longer re-reads ownership after the advisory status read or
  the orphan collect (`src/bridge/handler.rs:2284`): the admission taken after
  both covers them.
- `SUPPLEMENT_WAIT_WARN_AFTER` (3 s) and `SUPPLEMENT_MERGE_RETRY` (500 ms) are
  constants, not injectable knobs; the WARN path is diagnostic.
- Every out-of-turn render pass is chain-scoped. A renderer that loses the chain
  stops; its old card is collected by the successor's takeover. An **external
  message arming a new card mid-pass** remains the pre-existing exposure (its
  install needs no guard), unchanged by this record.
- The `SupplementFailed` continuation narrows to a Turn's own guard. An
  out-of-turn renderer whose steer failed ambiguously keeps its chain and shows
  the unconfirmed notice; a run that did merge the message still renders in that
  renderer's own card.
- A message waiting on a lease holds its inbound claim
  (`INBOUND_CLAIM_TTL` degrades it to absent after 120 s).
- A render loop that panics without releasing its lease parks the message's task
  (WARN after 3 s) where the pre-ADR code reported a notice; the leak path is
  unchanged code and the trade is recorded here.

Origins: #1086 (the handshake, the yield, and the live message loss), #1056 and
ADR-0109 (the guard key and the merge-only contract), #1082 (the failed-supplement
admission), #1089 (the restart-armed Wake family), ADR-0092 (the ambiguity rule).
