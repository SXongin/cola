# The Chain Record module: one home for a Card Chain's durable facts

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
  a card becomes the Session's live card and removed when that card reaches a
  terminal or is collected (ADR-0063's rule, unchanged); an announcement is
  monotonic per Session and never removed (ADR-0061's rule, unchanged). The
  load stays fail-open (a missing or corrupt file reads as empty) and the
  write stays best-effort and atomic; the file is kept even when both
  sections are empty, because its presence is the one-time migration marker.
- **One-time migration.** When `chain_records.json` is absent, the module
  reads the pre-ADR-0069 sidecars (`live_cards.json`, `wake_watermarks.json`)
  into the two sections, and never writes them again. The legacy files stay
  on disk for one release so a rollback still finds them; a later release
  deletes them. Because the merged file is never removed, a deliberately
  emptied record cannot be resurrected from the stale legacy file on the
  next restart.
- **One facade.** Callers hold one `ChainRecords`; `CardsHandle::chains`
  replaces the separate `live_cards` and `wake_watermarks` handles. The store
  methods keep today's names and semantics — the refactor moves the seam, it
  does not change behavior.
- **The reconcile lives with the record.** Session Sync's reap (`reap.rs`)
  moves into the module. The follow-up cuts, recorded here as the module's
  intended shape rather than this step's delivery: the pass is split into a
  pure decision (`ChainDisposition`, mirroring today's observable outcomes:
  Keep / NoDecision / DiscardRecord / CollectThenRepoint /
  CollectThenRelease / StampRestart / Settle(TurnSettle)) plus a thin apply
  that owns reads and card writes, and the Fresh path's Wake-watermark gate
  becomes the module's second decision entry sharing the same store. The
  restart-stamp attempt's retry policy (#522) then becomes one typed outcome.
  The waiting-card ledger/wake refresh stays outside: its evidence is the
  in-memory accumulator, not the durable record.

This step of the refactor is mechanically behavior-preserving: sidecar
merge, module move, call-site rename.

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
