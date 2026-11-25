# Route and admission are one handshake

> **Amends ADR-0109**: its Supplement routing key and its "a merge-only steer is
> refused" trade. The refusal becomes a **typed** answer that re-routes the same
> message, and the Turn's guard is claimed *before* its context is consumed, so
> the router's decision and the admission cannot disagree. ADR-0109's guard key,
> its `SteerOutcome` contract, its lease rules and its in-place Wake resumption
> all stand.

## Context

ADR-0109 made a held in-flight guard the one Supplement routing key and gave a
merge-only steer the power to refuse rather than start a run cola cannot render.
That closed #1056, but it left the router and the Turn reading the guard at
**different instants**, with nothing to retry the message on the other route:

- **Retiring renderer.** An out-of-turn renderer has stamped its card terminal
  but still holds its lease. The router sees the guard and chooses Supplement;
  the steer fails (no live run) and the terminal card refuses the split, so the
  prompt gets only a notice instead of the Turn that would render it.
- **Acquiring renderer.** A renderer leases the guard after the router's last
  ownership read but before `Turn::start`. The router chose a Turn; admission
  sees the guard and refuses it busy, so the message is never submitted.
- **The restart-armed continuation** (seen live 2026-11-24, `#1086`). A Wake
  continuation rehydrated from durable state is a **card-state claim, not a
  writer**: it holds the guard until its settle loop finalizes it, while no run
  exists behind the card. The router saw the guard, steered into nothing, and
  the typed-but-unread refusal was treated as an ambiguous failure — the card
  split as `SupplementFailed`, the user saw
  「⚠️ 补充消息未确认送达」, and their question was silently dropped. This
  variant was message **loss**, not a spurious notice.

Re-reading ownership after each awaited step (the ADR-0109 implementation did
it twice) only moves the window: an await remains between the last read and
`Turn::start`, and a lease taken there still loses the message.

## Decision

**1. The guard claim is the admission.** `Turn::try_admit` takes the Session's
in-flight guard and returns an opaque `Admission` — or `None` when another
writer holds it (`src/bridge/turn/mod.rs:422`). The coordinator takes it
**before** it builds the Turn's context, so the message facts are still owned
and can be submitted on the other route. `Turn::start` skips its busy check when
handed an admission, because the same lock acquisition that minted it proved the
session free (`Turn::claim_start_guard`, `src/bridge/turn/mod.rs:490`). The
self-admitting callers (an error-card retry, the V1 resume resubmit, low-level
seams) keep ADR-0109's refusal and its busy notice.

**2. A merge-only refusal is typed.** `SteerOutcome::RefusedNoLiveRun` means the
adapter observed no run to steer into and refused **before submitting**
(`src/backend/mod.rs:204`): nothing reached the Backend and nothing may still
land, so the caller may route the same message elsewhere. An `Err` stays
ambiguous (`ADR-0092`): a lost response may have landed, and it is never
re-routed. Both the generation-neutral default (`src/backend/mod.rs:409`) and
Codex's `turn/steer` adapter (`src/codex/backend.rs:3231`) answer this way.

**3. A typed refusal re-routes the same message.** Nothing was submitted, so
the message becomes the Turn it could not merge into. `settle_refused_supplement`
waits for the guard and claims it the instant it clears, starting that Turn
(`src/bridge/handler.rs:2757`). Every guard holder is finite — a Turn ends, and
an out-of-turn renderer's lease is scoped to its own loop (`idle_timeout_ms` in
`external_render_loop_inner`, and the Wake settle loop's finalize) — so the wait
ends with the message **admitted and submitted exactly once**. This is the path
for a retiring renderer (terminal card, lease still held), the restart-armed
continuation, and the loser of two simultaneous refusals.

While a **writer** holds the guard — another message's Turn whose run has not
registered yet, or a renderer that begins its run — the merge is retried on a
coarse cadence, so the message joins that run as soon as the adapter can see it
(`src/bridge/handler.rs:2795`). Every attempt is merge-only and every earlier
one refused before submitting, so the retry is the message's first submission,
and it reuses the message's `cola_msg_id` so it stays one logical message.

The wait has **no user-facing deadline**: the message's own task is the same one
a Turn lives in for minutes, and reporting it unsubmitted while a writer exists
is the loss this record fixes. `SUPPLEMENT_WAIT_WARN_AFTER` is a diagnostic:
past it a WARN records the long wait so a holder that never clears is visible;
the message keeps waiting. A typed refusal therefore never opens a
`SupplementFailed` continuation either — that receipt survives only for
genuinely ambiguous failures and the broken `StartedTurn` contract.

**3a. A renderer's lease is not proof of a writer either.** The
`SupplementFailed` continuation requires a **Turn's own** guard: a lease can own
the guard (`RenderLease::owns_guard`) and still have no run behind it, which is
exactly the restart-armed continuation. When only a lease owns the guard, an
ambiguous steer failure keeps the chain and reports the unconfirmed notice
(`finish_supplement`, `src/bridge/handler.rs:2835`), so the continuation is
never handed to a renderer that cannot continue it. A renderer that joined a
Turn's guard records `owns_guard = false` and is unaffected.

The ownership read and the lease read are two acquisitions, so a lease that
releases between them reads as a writer for one instant. That window is
strictly narrower than the pre-handshake one (where the release could happen
across the split's awaited lock), and the split's own admission still re-checks
the card under the cards lock; the chain is never left pointing at a card that
was replaced.

**4. Every path submits exactly once.** A Turn route submits one prompt; a
Supplement route submits one steer (merged); a typed refusal submitted nothing
and submits the message as either a merged retry or the Turn it waits for. No
path can submit twice, and no path reports a message unsubmitted while any
writer exists. The single exception is the ambiguity rule's own: an `Err` may
have landed, and it is never resubmitted (ADR-0092).

**5. The user sees no spurious busy notice, and no unsubmitted notice either.**
The busy notice is spoken only by the self-admitting callers that cannot
re-route. A message that merges through the handshake never shows it, and a
message waiting out a typed refusal shows nothing until it is submitted: it is
not reported unsubmitted while any writer exists. ADR-0092's "unconfirmed"
wording stays reserved for responses that may have landed.

## Why

- Route and admission were two reads of one fact; making the claim *be* the
  admission removes the fact's ability to change between them, instead of
  narrowing the interval between two reads.
- The typed refusal is the adapter's own knowledge ("I did not submit") reaching
  the router. Without it, "no live run" and "the response was lost" are
  indistinguishable, and the ambiguity rule must treat both as possibly-landed.
- The restart-armed continuation proves that `holds_guard() && RenderOwned` is
  still only a claim about the *card*: a rehydrated card can hold a lease and
  cannot render a new message. The handshake makes the message's fate depend on
  a writer that can actually take it, not on the claim's display state.

## Consequences

- `route_prompt` no longer re-reads ownership after the advisory status read or
  the orphan collect (`src/bridge/handler.rs:2284`): both await, and the
  admission taken afterwards covers them. The routing decision is one read; the
  admission is one claim.
- The handshake polls the guard every 25 ms and retries a merge against a
  writer every 500 ms (`SUPPLEMENT_HANDSHAKE_POLL`, `SUPPLEMENT_MERGE_RETRY`,
  `src/bridge/handler.rs`). `SUPPLEMENT_WAIT_WARN_AFTER` (3 s) is a **WARN**
  threshold, not a deadline; once it fires the poll slows to
  `SUPPLEMENT_HANDSHAKE_POLL_COARSE` (250 ms) so a long wait does not spin.
- The handshake serves the **retiring / restart-armed** holder, whose lease
  clears in under a second, and a Turn that owns the guard, which ends. A
  guard that never clears would park the message's task indefinitely; every
  holder in the code has a bound (a Turn's own lifecycle, a render loop's idle
  bound), and the WARN makes a violation visible. The parked task costs the same
  as the Turn it waits to start, and the message is never lost.
- A message waiting on a lease holds its inbound claim for that window
  (`INBOUND_CLAIM_TTL` degrades it to absent after 120 s, so Session Sync is
  never wedged by a long wait).
- A renderer that leases the guard between routing and admission now results in
  a Supplement regardless of whether the router had chosen a Turn. The
  message's *route* follows the writer, which is the property ADR-0109 wanted.
- ADR-0109's consequence "the instant between the router's final ownership read
  and `Turn::start` is not atomic … deferred to #1086" is closed by decision 1.
- A merge-only steer can now be answered three ways (`Merged`,
  `StartedTurn`, `RefusedNoLiveRun`) instead of two, and an adapter that folds a
  start into its merge keeps reporting `Merged` as before.
- The failed-steer continuation (`SupplementFailed`) narrows to a **Turn's own**
  guard. An out-of-turn renderer whose steer failed ambiguously keeps its chain
  and shows the unconfirmed notice instead of receiving a continuation; a run
  that did merge the message still renders it in that renderer's own card, so no
  content is lost.
- The out-of-turn renderer lease is unchanged: acquiring while a Turn holds the
  guard still joins it without owning it, and only the last release gives it
  back.

Origins: #1086 (the handshake and the live message loss), #1056 and ADR-0109
(the guard key and the merge-only contract), #1082 (the failed-supplement
admission), #1089 (the restart-armed Wake family), ADR-0092 (the ambiguity
rule).
