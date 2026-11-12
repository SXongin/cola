# The Timeline module: one owner for card placement

> **Names the deepening before it lands** (the ADR-0079 pattern). The landing
> amendment records the finished module and the tickets that landed it.
>
> **Amends ADR-0050** when this lands: the Timeline's entry state and types move
> to `turn/timeline.rs`. The Turn boundary this ADR records is unchanged — the
> accumulator, the flush and the render poll stay internal to the Turn module.
>
> **Amends ADR-0038's keying sentence**: a synthetic entry may be keyed at its
> own server time or at a key the caller already holds — a click's reserved
> receipt key, the Wake 承接 line's server time, a message's created time — not
> only at `next_order()`; the call sites already do this.
>
> **Amends ADR-0045's re-key rule**: a tail-only panel's allocated key is still
> replaced by a later real start time, but clamped to the floor the placement
> already carried.

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
  `render_compactions` (`:2114`) writes a third of it. A missed reset is
  invisible until a live card mis-sorts.
- **The inputs travel as bare integers.** Every push takes an order key and a
  display clock side by side:
  `push_text_at_key(order_at: Option<i64>, at_ms: Option<i64>, …)`, and
  `render_part(acc, source, part, order_at: Option<i64>)` threads the key from
  `part_order_times`. Five wrapper layers (`push_text` → `push_text_at` →
  `push_text_from` → `push_text_lead` → `push_text_at_key`) exist to default one
  of those parameters.
- **The cost is measurable.** 111 of the 2,268 subjects on `main` in the three
  months to 2026-11-12 match `order|floor|receipt|place|clamp|seq`
  (`git log --oneline 1268d2d0 --since=2026-08-12 | grep -ciE …`; pinned to
  `main@1268d2d0` so the figure stays reproducible), and a run of consecutive
  `fix(bridge)` commits is nothing but placement repairs ("keep read order
  across messages in one walk", "clamp the order floor and pin live panel
  keys", "keep a clamped entry's identity seq intact"). ADR-0038 documents the
  receipt-cut semantics in prose; no module owns the protocol.
- **The precedent is in-repo and of the same card.** ADR-0069 collapsed one
  durable fact per symptom into the **Chain Record**; ADR-0070 made one card
  ownership verdict; ADR-0072 made card delivery own the *write* order. This is
  the *placement* half of the same card, one level below the write order: which
  entry goes where on the body, not which body lands last.
- **The architecture review (2026-11-12, candidate 1) named the deepening.**

## Decision

**A Timeline module (`src/bridge/turn/timeline.rs`, private to the Turn module
per ADR-0050) owns the ordered entries of a card body and the rule that places
content into them.**

- **The interface is a walk.** `timeline.walk(&read_order)` yields a walk;
  `walk.message(created, id)` yields a message scope; the scope's `place(part
  facts) -> Placement` and `push(placement, kind: TimelineKind)` are the only
  way *transcript* content enters. The kind carries the content: a text or
  reasoning chunk, or `Tool(call_id)` — the panel body itself stays in the
  accumulator's tool map, which file delivery and liveness keep updating, and
  the card build reads it from there as today. `place` applies the same floors a
  transcript part gets: the message's `receipt_cut_floor(created)`, the read
  floor carried by the walk, and the live-slice clamp. The message scope's `Drop`
  clears the message floor and the walk's `Drop` clears the read floor, so the
  half-written protocol that produced the fixes above is no longer
  representable.
- **Synthetic entries have their own admission.** An Interaction Receipt, a
  compaction row, a ledger entry and the External Message's lead are not
  read-order content, so they never take the read floor and never join a walk.
  They enter through `Timeline::synthetic(kind, at: Option<i64>, row)`, which
  inserts the entry directly and returns its key — it yields no pushable
  `Placement`, so a synthetic row can never travel the transcript path. `at` is
  the entry's order key when the caller holds one — a resolved click's
  **reserved receipt key** (ADR-0038), a Wake's or a message's server time — else
  `next_order()`. The display clock is `at` only for the kinds that show one
  (compaction row, ledger entry, External Message lead) and only when `at` is
  `Some`; an Interaction Receipt always shows none, and a minted fallback is
  never displayed. The **compaction row** is the one that takes the Interaction
  Receipt cut for its message time — the floor `render_compactions` sets around it
  today (ADR-0038, issue #1023); the receipt, the ledger entry and the lead are
  placed outside any floor window today and keep that. `row` is the entry's merge
  identity: a compaction row carries its `PartSource` so a terminal fact updates
  the running row in place instead of appending a second one; the other three
  pass none and append.
- **The receipt seam.** The Timeline exposes `reserve_key` (minting a click's
  receipt and floor keys from its order sequence), `retune_cut(floor_key, at)`
  and `forget_cut(floor_key)`, so a mode change resolving several blocks can
  retune or drop cuts. `reservation_seq`, `reserved_receipts` and every
  `request_id` stay in the accumulator, which owns the reservation's lifecycle.
- **A `Placement` is the only way a push carries a position.** `place` returns a
  `Placement<'_>` borrowing its message scope; it carries the entry key, the
  merge identity (`PartSource`), the display clock and the floor it was placed
  under, with private fields and no public constructor. The borrow means it
  cannot outlive the scope or be pushed into a later one. Bare order keys stop
  crossing the seam; the five-layer push wrappers collapse.
- **The rewrite path is the Timeline's.** `Timeline::replace(placement, source,
  kind)` serves a part the server re-sent with different content: the `kind`
  carries the new text, the Timeline prefers the key the part's existing entry
  already holds (else the placement's key), drops every entry of that
  `PartSource` across the boundary while keeping `render_from` on the same
  logical position, and inserts the new run.
- **What the module owns.** The Timeline's entries (`TimelineItem`), the floors,
  the Interaction Receipt cut staircase, the live-slice boundary (`render_from`),
  the sealed-entry set, merge-by-part lookup, key minting (`next_order`) and
  entry identity (`seq`, including the seq a live tool panel pins when it
  settles, ADR-0045).
- **What does not move.** The live (unsettled) tool tail stays in the
  accumulator as a `TailPlacement` — key, display clock, merge identity, the floor
  it was placed under, and the pinned `seq` — plus its panel body in the tool map.
  `Timeline::rekey(tail, started_at)` applies ADR-0045's replacement, clamped to
  that floor, so a tail-only panel cannot jump the receipt cut or the read floor
  when its start time arrives; `Timeline::settle(tail, panel)` seats it with the
  stored `seq`. A `TailPlacement` never enters `push`. This clamp is the ADR's
  one deliberate tightening, and a test pins it; every other placement rule and
  every current test is unchanged. Content-keyed dedup and the rewritten part's
  prefix digest stay in the accumulator; the Timeline owns the entry-run replace
  defined above. The **Rendered Cursor** and its staging, the ledger, the
  interactions and the card build stay where they are. `StreamAccumulator` keeps
  its name.
- **The integration suite's seam stays.** `Turn::push_text` / `Turn::push_tool`
  keep working through a test-only Timeline entry; no production caller gains a
  new path.
- **Behaviour-preserving.** ADR-0038's receipt-skew window, ADR-0071's
  cursorless fallback and every current ordering test are unchanged, apart from
  the tail-only re-key clamp above. The timeline tests move beside the module;
  the guards gain invariant tests (a message scope always clears its floor, a
  walk always resets, a rewritten run keeps the boundary, a placed part keeps
  its key, a pending tool that later reports its start time settles at that time
  but not above its floor, a resolved click's receipt keeps its reserved key, a
  post-click compaction row still floors at the receipt, and a repeated
  compaction fact updates the row it already has).

## Consequences

- **Locality**: the placement rule has one body; a repair edits one module
  instead of three and a walk.
- **Leverage**: four walks share one cursor, and a new walk cannot forget it.
- **Test surface**: the guard is the seam — a test arranges a message scope, not
  a floor field, and the rule is verified through the same interface callers
  use.
- **Residual**: the accumulator remains a large struct; its delivery half (the
  Rendered Cursor's staging) is a separate deepening and is not reopened here.
