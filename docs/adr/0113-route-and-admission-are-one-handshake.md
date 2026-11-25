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

**3. A typed refusal re-routes the same message.** `submit_supplement` waits a
bounded moment for the guard to clear — the holders that answer this way retire
within their settle pass — and claims it the instant it does, starting the Turn
that renders the message (`submit_supplement`,
`src/bridge/handler.rs:2607`; `admit_after_guard_release`,
`src/bridge/handler.rs:2799`). This is the path for both a retiring renderer
(terminal card, lease still held) and the restart-armed continuation. A typed
refusal therefore never opens a `SupplementFailed` continuation — that receipt
survives only for genuinely ambiguous failures and the broken
`StartedTurn` contract.

**4. Every path submits at most once.** A Turn route submits one prompt; a
Supplement route submits one steer (merged); a typed refusal submitted nothing
and submits one fallback prompt; a bound that expires submits nothing and says
so. No path can resubmit a message the ambiguity rule forbids re-sending.

**5. The user sees no spurious busy notice.** The busy notice is spoken only by
the self-admitting callers that cannot re-route. A message that merges through
the handshake never shows it, and a typed refusal whose guard never clears
inside the bound gets the honest copy 「这条消息未被提交，请重发」 — not ADR-0092's
"unconfirmed" wording, which is reserved for responses that may have landed.

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
  the orphan collect (`src/bridge/handler.rs:2252`): both await, and the
  admission taken afterwards covers them. The routing decision is one read; the
  admission is one claim.
- The handshake bound is three seconds, polled every 25 ms
  (`SUPPLEMENT_HANDSHAKE_BOUND`, `SUPPLEMENT_HANDSHAKE_POLL`,
  `src/bridge/handler.rs`). Past it the message is reported as not submitted;
  it is never silently dropped and never claimed as delivered.
- A renderer that leases the guard between routing and admission now results in
  a Supplement regardless of whether the router had chosen a Turn. The
  message's *route* follows the writer, which is the property ADR-0109 wanted.
- ADR-0109's consequence "the instant between the router's final ownership read
  and `Turn::start` is not atomic … deferred to #1086" is closed by decision 1.
- A merge-only steer can now be answered three ways (`Merged`,
  `StartedTurn`, `RefusedNoLiveRun`) instead of two, and an adapter that folds a
  start into its merge keeps reporting `Merged` as before.
- The out-of-turn renderer lease is unchanged: acquiring while a Turn holds the
  guard still joins it without owning it, and only the last release gives it
  back.

Origins: #1086 (the handshake and the live message loss), #1056 and ADR-0109
(the guard key and the merge-only contract), #1082 (the failed-supplement
admission), #1089 (the restart-armed Wake family), ADR-0092 (the ambiguity
rule).
