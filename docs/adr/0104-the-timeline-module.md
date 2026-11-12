# The Timeline module: one owner for card placement

> **Names the deepening before it lands** (the ADR-0079 pattern); spec #1040. The
> landing amendment records the finished mechanism and the tickets that landed
> it.
>
> **Amends, when it lands**: ADR-0038 (the read floor and the equal-key rule),
> ADR-0040 (`seq` assignment), ADR-0045 (a tail panel's settle rule), ADR-0050
> (the entry types' home) and ADR-0082 (a compaction row's receipt cut). The
> exact ordering rules and their invariants are #1040's acceptance criteria; this
> ADR records the module boundary and the ownership move.

Where a card entry lands has no owner. The rule is written three times, its
protocol is re-armed by hand in four walks, and its inputs cross the seam as
bare `i64`s.

## Context

- **Three bodies implement one rule.** `StreamAccumulator::part_order_key`
  (`src/bridge/turn/state.rs:3192`) decides the key a part will take;
  `insert_item` (`:3929`) decides it again when the part lands, clamp included;
  `insert_kind_after` (`:4066`) mirrors the clamp a third time. The code says so:
  "Apply the same live-slice clamp [`Self::insert_item`] will".
- **Four walks re-arm the protocol by hand.** `render_turn_parts`
  (`turn/render.rs:1631`), `render_pending_gap` (`:1847`), `render_seed_scope`
  (`:2027`) and `resolve_seeded_calls` (`:2354`) each write the sequence
  `set_message_floor` → `read_floor_before` → `set_read_floor` … →
  `set_message_floor(None)`, and `render.rs` calls `reset_read_floor()` thirteen
  times (eleven in production, two in the test-only `render_parts`).
- **The inputs travel as bare integers.** Every push takes an order key and a
  display clock side by side, and `render_part` threads the key from
  `part_order_times`. Five wrapper layers (`push_text` → … → `push_text_at_key`)
  exist to default one parameter.
- **The cost is measurable.** 111 of the 2,268 subjects on `main` in the three
  months to 2026-11-12 match `order|floor|receipt|place|clamp|seq`
  (`git log --oneline 1268d2d0 --since=2026-08-12 | grep -ciE …`), and a run of
  consecutive `fix(bridge)` commits is nothing but placement repairs.
- **The precedent is in-repo and of the same card.** ADR-0069 collapsed one
  durable fact per symptom into the **Chain Record**; ADR-0070 made one card
  ownership verdict; ADR-0072 made card delivery own the *write* order. This is
  the *placement* half of the same card, one level below the write order.
- **The architecture review (2026-11-12, candidate 1) named the deepening.**

## Decision

**A Timeline module (`src/bridge/turn/timeline.rs`, private to the Turn module
per ADR-0050) owns the ordered entries of a card body and the rule that places
content into them.**

- **One walk interface.** `timeline.walk(&read_order)` yields a walk;
  `walk.message(created, id, pinned_tails)` yields a message scope; its `place`
  returns a `Placement` and `push`/`replace` consume it. The scope's `Drop`
  clears the message floor and the walk's `Drop` clears the read floor, so the
  half-written protocol is no longer representable and no accessor sets a floor
  from outside.
- **`Placement` is an owned, linear token, not a borrow.** It carries the entry
  key, the merge identity, the display clock, the floor it was placed under, and
  a private scope generation. Its fields are private with no public constructor
  and it is neither `Clone` nor `Copy`; `push`/`replace` return
  `Result<(), PlacementError>` and validate completely before mutating, so a
  stale or mismatched token is refused without touching the timeline.
- **Synthetic entries have their own admission.** A content-bearing
  `SyntheticEntry` inserts directly, returns the key the entry took (or nothing
  when it may merge/chunk), and never feeds `push`; each variant maps onto the
  stored `TimelineKind` its call site uses today. **#1040 owns the variant list,
  the clock rules and the receipt cut.**
- **What moves.** The entry types (`TimelineItem`, `TimelineKind`, `PartSource`,
  `Placement`), the message and read floors, the Interaction Receipt cut
  staircase, the live-slice boundary, the sealed-entry set, merge-by-part
  lookup, key minting and entry identity (`seq`). One private insert path
  replaces `part_order_key` and the mirrored clamps, so the live-slice clamp is
  computed once.
- **What stays.** The Rendered Cursor and its staging, content-keyed dedup and
  the rewritten part's prefix digest, the ledger, the interactions and the card
  build. The live (unsettled) tool tail stays in the accumulator as a
  `TailPlacement` plus its panel body; a pin is settled through the Timeline but
  never through `push`. `StreamAccumulator` keeps its name.
- **Behaviour-preserving, with named tightenings.** The ordering semantics and
  the deliberate tightenings (the settle floor, the read-position tie-break and
  the pinned-tail read floor) are #1040's acceptance criteria, and the batch's
  tests pin them. No wire or durable-format change lands here; the compaction
  split/restart delivery gap is #1049.
- **The integration suite's seam stays.** `Turn::push_text` / `push_tool` /
  `push_reasoning`, their `_at` forms, and `Turn::set_message_floor` keep
  working through a test-only scripted message scope; no production caller gains
  a new path.

## Consequences

- **Locality**: the placement rule has one body; a repair edits one module
  instead of three and a walk.
- **Leverage**: four walks share one cursor, and a new walk cannot forget it.
- **Test surface**: the guard is the seam — a test arranges a message scope, not
  a floor field, and the rule is verified through the interface callers use.
- **Residual**: the accumulator remains a large struct; its delivery half (the
  Rendered Cursor's staging) is a separate deepening.

## Landing amendment

To be written with the batch: the finished module, the accumulator split as
built, the exact rules as landed, and the tickets that landed them.
