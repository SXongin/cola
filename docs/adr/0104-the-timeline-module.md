# The Timeline module: one owner for card placement

> **Landed** with spec #1040's batch. The landing amendment below records the
> finished mechanism, the accumulator split as built, the rules as landed and
> the tickets that landed them.
>
> **Amends**: ADR-0038 (synthetic keying, the read floor and the equal-key rule),
> ADR-0040 (`seq` assignment), ADR-0045 (a tail panel's settle rule), ADR-0050
> (the entry types' home) and ADR-0082 (a compaction row's receipt cut). The
> exact ordering rules and their invariants are #1040's acceptance criteria; this
> ADR records the module boundary and the ownership move.

Where a card entry lands has no owner. The rule is written three times, its
protocol is re-armed by hand in four walks, and its inputs cross the seam as
bare `i64`s.

## Context

- **Three bodies implemented one rule (removed by this batch; the anchors name
  `main@1268d2d0`).** `StreamAccumulator::part_order_key`
  (`src/bridge/turn/state.rs`, since deleted) decided the key a part will take;
  `insert_item` (same file, since deleted) decided it again when the part lands,
  clamp included; `insert_kind_after` (same file, since deleted) mirrored the
  clamp a third time. The code said so:
  "Apply the same live-slice clamp [`Self::insert_item`] will".
- **Four walks re-armed the protocol by hand (line numbers name
  `main@1268d2d0`).** `render_turn_parts`
  (`turn/render.rs:1631`), `render_pending_gap` (`:1847`), `render_seed_scope`
  (`:2027`) and `resolve_seeded_calls` (`:2354`) each wrote the sequence
  `set_message_floor` → `read_floor_before` → `set_read_floor` … →
  `set_message_floor(None)`, and `render.rs` called `reset_read_floor()` thirteen
  times (eleven in production, two in the test-only `render_parts`).
- **The inputs traveled as bare integers (at `main@1268d2d0`; the batch
  narrowed them).** Every push took an order key and a display clock side by
  side, and `render_part` threaded the key from `part_order_times`. Five wrapper
  layers (`push_text` → … → `push_text_at_key`) existed to default one
  parameter.
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

- **One walk interface.** `Timeline::walk(&read_order)` yields a walk carrying
  the read's message positions; `walk.message(timeline, created, id,
  pinned_tails)` yields a message scope. Every method takes the `&mut Timeline`
  for the call — `place` returns a `Placement`, `push`/`replace` consume it — so
  one walk serves a whole render pass and one scope a whole message without
  borrowing the accumulator. The walk owns the read floor and clears it on its
  own `Drop`; a scope owns the message floor, marks the walk occupied and
  clears that floor and the mark on its `Drop`, so a second scope cannot open
  over an undropped one and the half-written protocol is no longer
  representable. No accessor sets a floor from outside.
- **`Placement` is an owned, linear token, not a borrow.** It carries the entry
  key, the merge identity, the display clock, the floor it was placed under, the
  `TranscriptKind` variant its part expects, and a private scope generation. Its fields are private with no public constructor
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
  replaces `part_order_key` and the mirrored clamps, so one key computation
  feeds both `place` and `push` (the live-slice clamp itself lives in
  `slot_at`, and `push` re-partitions at that key).
- **What stays.** The Rendered Cursor and its staging, content-keyed dedup and
  the rewritten part's prefix digest, the ledger, the interactions and the card
  build. The live (unsettled) tool tail stays in the accumulator as a
  `TailPlacement` plus its panel body; a pin is settled through the Timeline but
  never through `push`. `StreamAccumulator` keeps its name.
- **Behaviour-preserving, with named tightenings.** The ordering semantics and
  the deliberate tightenings (the settle clamp, the read-position tie-break and
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

**Landed** in the Timeline batch, tickets #1041–#1046, spec #1040. The module is
`src/bridge/turn/timeline.rs`, `pub(super)` to the Turn; the Turn boundary
ADR-0050 records is unchanged.

### The module as built

- **Types.** `TimelineItem` (key, `shown_at`, `order`, `seq`, kind, source,
  lead, read position), `TimelineKind`, `PartSource`, `PartForm`, `PartFacts`,
  `TranscriptKind`, `Placement`, `PlacementError`, `ReadPosition`, `ReceiptCut`,
  `ReservationKeys`, `TailFact`, `TailPlacement`, `SyntheticEntry` and
  `part_order_times`. `TimelineKind` keeps its shape — `Tool(call_id)` with the
  panel body in the accumulator — and gains no generic parameter.
- **One walk interface.** `Timeline::walk(&read_order)` yields a `Walk`;
  `Walk::message(timeline, created, id, pinned_tails)` yields a `MessageScope`
  that owns the message floor against the walk's read floor and marks the walk
  occupied. Its `place(timeline, PartFacts)`
  produces the linear `Placement`; `push(timeline, placement, kind)` and
  `replace(timeline, placement, source, kind)` consume it and return
  `Result<(), PlacementError>` after validating generation, kind and source.
  `MessageScope::synthetic(timeline, entry)` admits a synthetic entry under the
  scope's message floor; `MessageScope::pin(timeline, facts, call_id)` produces
  the `TailPlacement` a live panel carries. The walk owns the read floor and
  clears it at its own `Drop`; a scope owns the message floor, marks the walk
  occupied, and clears the floor and the mark at its `Drop`, so a second scope
  cannot open over an undropped one and a half-written protocol cannot survive.
  One walk serves a whole render pass and one scope a whole message, so
  preparation may use the accumulator between placements.
- **One insert path.** `Timeline::seat` seats an entry at its already-resolved
  slot, allocates `seq` when the caller has none and lowers a clamped entry's
  `order`; the live-slice clamp is `slot_at`, the equal-key partition is
  `slot_for_new` / `slot_for_seq`, and the continuation/merge walk is
  `push_text_at_slot` (`source_entry`, the remaining-space probe, `slot_after`).
  `place` and the insert consume one key computation, so `part_order_key` and
  the mirrored clamps are gone.
- **What stays in the accumulator.** `StreamAccumulator` (name unchanged) keeps
  the text and reasoning buffers, content-keyed dedup and the rewritten part's
  prefix digest, the interactions and the `request_id → ReservedReceipt` map
  (with `reservation_seq`), the Rendered Cursor and its staging, the ledger,
  the header/footer, the card build and the live tool panel bodies (`tools`).
  It holds `live_tools: IndexMap<String, TailPlacement>`; the tail token's type
  is the Timeline's, but its panel body and lifecycle are the accumulator's.
  The Timeline owns the Timeline's ordered entries, the message and read floors, the receipt-cut
  staircase, `render_from`, `sealed_entries`, merge-by-part lookup, key minting
  and `seq`.

### Rules as landed

- `place` applies the message's `receipt_cut_floor(created)`, the walk's read
  floor (earlier messages' placed entries **and their pinned tails**),
  placed-part stability (`placed_source_key`: a part already on the live slice
  keeps its key) and the live-slice clamp. A source-bearing placement carries
  its read position; a source-less one clamps like the insert path always did.
  Pinned by `a_placed_part_keeps_its_key_across_reads`.
- A synthetic entry inserts directly and returns the key it took (`None` for a
  `Lead`, which may merge or chunk, and for a blank `UserMessage`). Every
  variant's `at` is an order key only: a receipt, a notice, a user lead and a
  compaction row render no clock, and a ledger entry shows its payload's
  `finished_at`. A compaction row takes its message's Interaction Receipt cut,
  keeps its start time on `shown_at` for the cursor frontier only, and updates
  in place by `PartSource`. Pinned by
  `a_wake_ledger_entry_shows_its_finished_at_not_its_keyed_clock`,
  `a_post_click_compaction_row_floors_at_the_cut_and_shows_no_clock` and
  `a_message_scope_floors_a_compaction_row`.
- `replace` drops the part's run across the boundary (`drop_source_run` keeps
  `render_from` on the same logical position) and re-floors the placement at
  `max(placement.key, placement.floor)` before inserting the new run.
- `settle` takes `tail.floor` (the floor the pin carried), the CURRENT receipt
  cut and the CURRENT read floor for the tail's message — every entry that
  precedes it in read order (earlier messages' entries and the panel's own
  preceding parts) plus every pinned tail that precedes it (an earlier
  message's, or the panel's own) — seats at `max(current, floor)`, caps at the
  minimum key among the entries that follow it in read order at or above the
  floor, and seats through the normal insert path (clamp included) with its
  pinned `seq` — never renumbered (ADR-0040). Pinned by
  `a_settled_panel_keeps_the_identity_it_was_pinned_with` and
  `a_settle_is_floored_by_the_pinned_tails_that_precede_it`.

### Deliberate tightenings

Three rules are stricter than the pre-module code; every other ordering rule
and every existing test is unchanged.

- **The settle clamp.** A tail-only panel's late `started_at` cannot drag it
  above the current receipt cut or the earlier messages' read floor, and a
  pending tool's late start never pushes it past a same-message continuation or
  a later message's placed part: the settled key is a **minimum** over the
  following entries at or above the floor, never the first one (after a tool
  those keys need not be monotonic). Its first half is the rekey clamp: a later
  sighting's `started_at` becomes `max(pin-time floor, at)`, so a late start
  cannot jump the floor the pin already carried either. Pinned by
  `a_rekey_never_drops_below_the_pins_floor`,
  `a_tail_only_panels_late_start_cannot_jump_the_current_floors`,
  `a_late_start_is_capped_at_a_later_messages_placed_part`,
  `a_late_start_is_capped_at_the_following_minimum`.
- **The equal-key read-position tie-break.** A source-bearing transcript entry
  carries (message position in the read, part index) and sorts by it at equal
  keys; a source-less or synthetic entry has no read position and sorts
  **before** positioned entries at the same key (ADR-0038 rule 4: content
  floored to a receipt's key stays below it), keeping allocation order among
  its peers. Pinned by `equal_keys_order_by_read_position_then_class`.
- **The pinned-tail read-floor extension.** An earlier message's live panel
  floors a later message's part even when the later part's server key is older,
  and the panel keeps the earlier read position at the tied key. The floor is
  the tail's **effective** key — its raw pin/rekey key raised to the floor the
  pin carried **and** to the CURRENT receipt cut for its message,
  `max(key, floor, cut)`, the lower bound at or above which the panel settles
  (a receipt recorded after the pin can push it higher; `settle` re-applies the
  same cut, so the fact and the settle agree) — never the bare key a receipt
  cut may already outrank. Pinned by
  `a_pinned_tails_effective_key_includes_its_carried_floor`,
  `a_post_pin_receipt_cut_floors_later_messages_through_the_fact`,
  `an_earlier_messages_pinned_tail_floors_a_later_message` and
  `the_accumulators_scoped_push_uses_the_pinned_tail_floor`.

### One interface, one walk per pass

The walk is one interface per render pass, not a borrow of the timeline. A
`Walk` carries only the read's message-position map; the accumulator hands the
timeline to each `place` / `push` / `replace` / `pin` / `synthetic` call, so
preparation — chunking, content dedup, the rewrite digest, the panel body —
stays free to use the accumulator between placements. Each pass opens ONE walk
and one `MessageScope` per message: `render_turn_parts`'s window,
`render_pending_gap`, `render_seed_scope` and `resolve_seeded_calls`, plus the
compaction walk (`place_compactions`, one walk per read and one scope per row).
The walk owns the read floor and clears it at its own `Drop`; the scope borrows
the walk, owns the message floor, carries the read position and generation, and
marks the walk occupied. Its `Drop` frees the walk and clears the message floor,
so a second scope cannot open over an undropped one and the transient protocol
cannot survive either. No accessor sets a floor by hand, and
the four walks contain no `set_message_floor`, `set_read_floor` or
`reset_read_floor` call. The only remaining `Turn::set_message_floor` is the
`#[cfg(test)]` scripted seam, which delegates to a real message scope.

### The rewrite keeps ADR-0038's rule, re-floored

ADR-0038 says a part already on the timeline "keeps the key it was placed at",
because a later read's floor can differ and re-keying would split a growing
part. The rewrite path keeps that rule and restores the floor `place` computed
before the run dropped: `replace` raises `placement.key` to
`max(placement.key, placement.floor)` BEFORE dropping the run
(`drop_source_run`), then pushes under it; `push` applies no floor of its own. Same part, same key — unless the
read's floor has moved past it, in which case the rewritten run takes the floor,
exactly as the pre-module replace did. Pinned by
`a_rewritten_run_keeps_the_boundary_on_its_entry`,
`a_later_chunk_merges_into_the_rewritten_run`, and the rejection tests
`a_refused_replace_keeps_the_run` and
`a_stale_or_mismatched_placement_is_refused_before_inserting`.

### The clamp and the cut stay consistent

A receipt whose reserved key the live slice no longer reaches clamps to the
slice's first key, and the returned key is the clamped one; its cut keeps the
old floor key. Content floored to that now-behind cut clamps to the same slice
key, and the class rule (source-less before positioned at equal keys) keeps it
below the receipt. Pinned by
`a_reserved_click_receipt_keeps_its_key_and_clamps_after_a_split` and
`synthetic_admission_takes_cuts_merges_and_returns_the_clamped_key`.

### Landed tickets

- **#1041** — the module and its types; the accumulator delegates; the synthetic
  admission; the ordering tests move beside the module.
- **#1042** — the walk guard and the turn walk.
- **#1043** — the gap, seed and compaction walks onto the guard.
- **#1044** — the receipt-cut staircase and reservation key minting; the rewrite
  path with the boundary-preserving replace.
- **#1045** — delete the duplicates: `part_order_key`, the mirrored clamps and
  the push wrapper layers; keep the `Turn::push_*` test seam.
- **#1046** — the guard invariants and this amendment.

### Out of scope

- **#1049** — a compaction's terminal revision arriving after a confirmed
  `running` one (the split/restart delivery gap) still needs its own durable
  section; no durable-format change lands in this batch.
- **The Rendered Cursor's staging/confirm/discard protocol** (the 2026-11-12
  architecture review's candidate 2) remains the separate deepening: the
  Timeline decides where an entry lands, the cursor decides how a delivered
  body's frontier is confirmed, and the two stay different owners.
