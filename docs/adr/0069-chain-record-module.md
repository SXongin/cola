# The Chain Record module: one home for a Card Chain's durable facts

> **Amended by ADR-0071**: the `records` section gains the **Rendered
> Cursor** — the chain's confirmed render frontier and live-call set — on the
> same atomic best-effort write and with the same lifetime as the record:
> `track` carries it across a re-point, `release` drops it, and a record
> without it reads cursorless. See the amendment at the end.

> **Amended by ADR-0072**: #522's permanently refused carve-out is no longer a
> mark on the record — it is the keyed queue's permanent-refusal outcome — and
> the mark's `Keep` mapping retired with the restart stamp's guard cluster.
> See the amendment at the end.

## Context

A cola restart has, over ADR-0058–0068, grown one durable fact per symptom:
ADR-0061's **Wake Watermark** (`wake_watermarks.json`) stops a restart
re-announcing a Wake; ADR-0063's **Live Card record** (`live_cards.json`)
lets Session Sync reap the card the previous process orphaned; ADR-0067's
Pending Card Update outbox is deliberately in memory; ADR-0065's runtime
retirement overlay is deliberately in memory. Each decision was right on its
own, but the facts about ONE Card Chain now live in two sidecars, one record
struct that mixes durable fields with four `serde(skip)` reconciliation marks,
and two in-memory overlays — and the one record's lifecycle ("when is it
written, read, removed?") is implemented across `live_cards.rs`,
`turn/mod.rs`, `flush.rs`, `render.rs`, `reap.rs` and `external.rs`, with
ADR-0063 amended twice (ADR-0067, ADR-0068).

The architecture review (2026-10-05) named the deepening: one module owning a
Chain's durable facts and its restart reconciliation, so the pairwise
interactions that every recent fix had to police (stamp vs collect, carry vs
collect, collect vs drain) live inside one interface instead of between
flows. The design round settled the shape; this ADR records it.

## Decision

**A Chain Record module (`src/bridge/chain/`) owns the durable per-Session
facts about its Card Chain, in one sidecar.**

- **One sidecar, two sections with distinct lifetimes.**
  `chain_records.json` holds `records` — the live card's identity, its Turn
  anchor, the Session's directory and the per-process-life reconciliation
  marks — and `announcements` — the Wake Watermark. A record is written when
  a card becomes the Session's live card and released when its card's terminal
  ending write is confirmed or a successor collect cannot carry it
  (ADR-0063's rule, as amended by ADR-0067); an announcement is
  monotonic per Session and never removed (ADR-0061's rule, unchanged). The
  load stays fail-open (a missing or corrupt file reads as empty) and the
  write stays best-effort and atomic; the file is kept even when both
  sections are empty, because its presence is the one-time migration marker.
- **One-time migration.** When `chain_records.json` is absent, the module
  reads the pre-ADR-0069 sidecars (`live_cards.json`, `wake_watermarks.json`)
  into the two sections and materializes the merged file immediately, so the
  marker exists from the first load; the legacy files are never written
  again. They stay on disk for one release so a rollback still finds them; a
  later release deletes them. Because the merged file is never removed, a
  deliberately emptied record cannot be resurrected from the stale legacy
  file on the next restart.
- **One facade.** Callers hold one `ChainRecords`; `CardsHandle::chains`
  replaces the separate `live_cards` and `wake_watermarks` handles. The store
  methods keep today's names and semantics — the refactor moves the seam, it
  does not change behavior.
- **The reconcile lives with the record.** Session Sync's reap (`reap.rs`)
  moves into the module, and the record's lifecycle with it. The pass is a
  pure decision (`ChainDisposition`, one variant per observable outcome:
  Keep / NoDecision / DiscardRecord / CollectThenRepoint /
  CollectThenRelease / StampRestart / Settle(TurnSettle)) plus a thin apply
  that owns reads and card writes; the Fresh path's Wake-watermark gate is the
  module's second decision entry (`fresh`) sharing the same store; and the
  restart-stamp attempt's permanently refused carve-out (#522) is a mark on
  the record that the ladder maps to `Keep`. The record's lifecycle is three
  entries on the module: `ChainRecords::track` creates or re-points a record
  from the facts, returning the predecessor the takeover paths act on;
  `ChainRecords::release` is the only removal; and `chain::release_spent`
  owns the spent rule: a record leaves when its card reaches a terminal
  ending whose write is confirmed (ADR-0063's amendment, ADR-0067), never
  while the delivery outbox still owes one. The flush path calls it after
  every ending PATCH, as does the reap's `DiscardRecord` arm. The reap's
  ladder calls `release` directly only where it established the cause itself:
  a successor collect that cannot carry the record (settled, or no armed
  anchor), or a terminal ending whose PATCH just landed. The in-memory
  overlays stay with their owners and are read at the seam as predicates,
  never moved into the record: the Pending Card Update outbox (the delivery
  adapter's, ADR-0067) answers whether an ending write is still owed, and the
  runtime-retirement overlay (the Backend adapter's, ADR-0065) corroborates a
  background task's liveness. The waiting-card ledger/wake refresh likewise
  reads the accumulator's announced and handed-over Wake sets, its evidence
  the in-memory accumulator, not the durable record.

This step of the refactor is mechanically behavior-preserving — sidecar
merge, module move, call-site rename — with two alignments the merge makes
possible: the fold materializes the marker file at its first load, and a
re-keyed record (the 404 recreate) carries its announcement, because the
recreated id names the same logical Session.

## Considered options

- **Keep two sidecars, unify only the module.** Rejected: the point of the
  deepening is one home; the migration is a fold-on-load and a one-release
  compatibility window, which is cheap.
- **One record with a Live → Spent lifecycle** (announcements living on a
  spent record, one retention policy). Rejected for now: it narrows
  ADR-0063's "removed once the ending write is confirmed" contract into a
  new state and needs a retention rule that does not exist today; the two
  explicit sections express the two lifetimes without inventing one.
- **Fold the Pending Card Update outbox and the runtime-retirement overlay
  in too.** Rejected: the outbox is the delivery adapter's in-memory
  newest-wins state (ADR-0067) and the overlay is the Backend adapter's
  per-process corroboration (ADR-0065); both have different lifetimes and
  owners. The Chain Record module is not a place to re-collect every
  in-memory fact.
- **Remove the merged file when empty, as every other sidecar does.**
  Rejected: absence would fall back to the legacy files and resurrect state
  that was deliberately removed.

## Consequences

- GLOSSARY gains **Chain Record**; ADR-0061 and ADR-0063 each carry a
  one-line amendment pointing here. Both decisions' semantics (advance only
  after a landed write; remove only after a confirmed ending) are unchanged.
- A single module now answers "what does this Chain remember, and what does
  a restart owe it?" — the open restart issues (#505's rendered cursor,
  #528's Wake-arm carry, #529's auto-adopt, #510's cross-restart retry) land
  on this interface instead of growing new sidecars.
- `chain_records.json` is the one file whose absence means "migrate"; a
  rollback to the pre-ADR-0069 binary still finds the legacy files it wrote,
  at the cost that anything the new binary wrote after the fold is invisible
  to it (the accepted one-release window).
- Tests: the merged store's round-trip, the legacy fold, the "new file wins"
  and the "cleared record stays cleared" rules are unit-tested; the reap's
  restart matrix keeps its end-to-end tests, seeding one store instead of
  two.

Related: #522, #505, #528, #529, #510, ADR-0061, ADR-0063, ADR-0065,
ADR-0067, ADR-0068.

## Amendment (2026-10-06): the record gains the Rendered Cursor (ADR-0071)

The `records` entry — until now the Live Card record (ADR-0063) — also carries
the chain's **Rendered Cursor** (ADR-0071): the frontier of the newest
text/reasoning content the chain confirmed delivered (message identity, part
position, kind, server start and the delivered character extent) plus the set
of tool call ids whose newest delivered state was `running`. It is one more
durable fact on the same atomic best-effort write, never a new sidecar — the
records section now answers both "which card is live?" and "how far has this
chain rendered?".

The store owns the fact's lifecycle exactly as it owns the record's: `track`
carries the cursor across a re-point within the chain (a split continuation, a
new Turn on the same chain, a takeover), while a genuinely new chain starts
cursorless; `release` drops it with the record; `advance_cursor` writes only
the record naming the card a confirmed write landed on, so a stale flush
cannot touch a successor; and `recorded` is the Fresh gate's one read — a
durable record exists, so its Wake belongs to the projection (or, cursorless,
to the reap's fallback), never to a Fresh post. A record written by an older
release has no cursor field and reads cursorless, which keeps the pre-#561
fallback behavior; the fallback window closes on the record's first confirmed
write.

Nothing else about the module moves: the sections' lifetimes, the fail-open
load, the atomic write, the one-time legacy fold and the interface's claim on
the chain's reconciliation are as decided above.

## Amendment (2026-10-07): #522's carve-out leaves the record (ADR-0072)

The Decision's "the restart-stamp attempt's permanently refused carve-out
(#522) is a mark on the record that the ladder maps to `Keep`" retired with the
restart stamp's guard cluster (spec #571, ticket #575): the refusal is now
remembered by the card-delivery queue as the settled `(chain generation,
Stamp)` key, and the reap's per-tick re-decision is a read-only queue query
(`keyed_write_covered`) instead of a record mark. The record carries no #522
state: a still-live cursorless orphan's ladder returns its ordinary
`StampRestart`, and the apply's coverage query is what suppresses the
submission once the key is refused — or delivered, or already being written.
The mark's unit test went with it; the queue's own tests pin the refusal.
