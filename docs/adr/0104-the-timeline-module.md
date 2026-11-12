# The Timeline module: one owner for card placement

> **Names the deepening before it lands** (the ADR-0079 pattern). The landing
> amendment records the finished module and the tickets that landed it. Spec:
> #1040.
>
> **Amends ADR-0040 when this lands**: `seq` is assigned once by the Timeline
> (the successor of the accumulator's `insert_kind`) with the same never-reuse,
> never-renumber invariant; a tail panel keeps the `seq` it was pinned with.
>
> **Amends ADR-0050** when this lands: the Timeline's entry state and types move
> to `turn/timeline.rs`. The Turn boundary this ADR records is unchanged — the
> accumulator, the flush and the render poll stay internal to the Turn module.
>
> **Amends ADR-0038's keying and equal-key rules**: a synthetic entry may be
> keyed at its own server time, at a key the caller already holds (a click's
> reserved receipt key), or at a derived key just before the work it introduces
> (a Wake line and the External Message lead use `created_ms - 1`) — not only at
> `next_order()`; and at equal keys entries order by **read position** (message
> position, then part index) for source-bearing parts, so a tail panel whose
> settle floor raises its key lands after earlier-message entries and before its
> own message's later parts, without renumbering anything.
>
> **Amends ADR-0045's re-key rule**: a tail-only panel's allocated key is still
> replaced by a later real start time, but settle first floors it with the
> current receipt cut and the current read floor for its message.

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

- **The interface is a walk.** `timeline.walk(&read_order, pinned_tail_keys)` yields
  a walk; the accumulator passes its live tails' keys, each mapped to its message
  (the walk is built once per pass, so this is a snapshot);
  `walk.message(created, id)` yields a message scope; the scope's `place(part
  facts) -> Placement` and `push(placement, kind: TranscriptKind)` are the only
  way transcript content enters. Transcript content is a separate type at the
  seam (`TranscriptKind::{Text { chunk, lead }, Reasoning { chunk, lead },
  Tool(call_id)}`; synthetic entries have their own admission below), so a
  synthetic entry cannot be pushed down the transcript path. The `lead` is the
  seeded cut's fence opener or table header: it renders before the chunk but is
  never counted as the part's delivered characters, exactly as today. The tool's
  panel body stays in the accumulator's tool map, which file delivery and
  liveness keep updating, and the card build reads it from there as today.
  `place` applies the same floors a transcript part gets:
  the message's `receipt_cut_floor(created)`, the read floor carried by the
  walk, the live-slice clamp, and placed-part stability (a part already on the
  **live slice** keeps the key it was placed at; a finalized entry is behind
  `render_from`, so a grown part starts a continuation entry as today). The
  transcript-order policy `part_order_times` applies today moves in too: before
  a message's first tool call, an untimed part takes the previous timed part's
  key (or the message's `time.created` when there is none) and that key floors
  the prefix; after a tool call an untimed part keeps the render-order fallback
  (`next_order()`), because moving it onto the transcript position could carry it
  above the Interaction Receipt the answer wrote. The
  message scope's `Drop` clears the message floor and the walk's `Drop` clears
  the read floor, so the half-written protocol that produced the fixes above is
  no longer representable.
- **Synthetic entries have their own admission.** An Interaction Receipt, a
  compaction row, a ledger entry, the External Message's lead and a bot-authored
  notice (the Wake 承接 line, a busy-follow static lead) are not read-order
  content, so they never take the read floor and never join a walk. They enter
  through `Timeline::synthetic(entry: SyntheticEntry)` — one content-bearing
  variant per kind: `Receipt { text, at }` (a receipt line — a click's receipt, a
  merge opening, a receive hint, the residual marker, a skipped-content note or a
  split line; `TimelineKind::Receipt`), `Compaction { fact, at, message_time, row }`,
  `Ledger { entry, ending, at }`, `UserMessage { text, at }`, and `Lead { text,
  at }` (the fresh-card Wake 承接 opener and the busy-follow static lead —
  `TimelineKind::Text`, where both still merge into a text run; the queued Wake
  continuation's split line is a `Receipt`, its call site being
  `push_receipt_at`) — which inserts
  directly and returns `Option<i64>`: `Some(key)` for a variant that owns one
  entry, `None` for a `Lead`, which may merge into an existing text run or chunk
  into several entries. Each variant maps onto the stored
  `TimelineKind` its call site uses today, so a `Lead` still merges as text and a
  `Receipt` covers every receipt line. It yields no pushable `Placement`, so a synthetic row can never travel
  the transcript path. Every synthetic kind's `at` is an **order key only** —
  a resolved click's **reserved receipt key** (ADR-0038), a Wake's or a message's
  server time, or a derived key just before the work it introduces (the Wake line
  and the External Message lead use `created_ms - 1`) — and never a display
  clock: a receipt, a lead and a user message show no clock (text blocks), a
  compaction row shows none, and a ledger entry shows its own payload's
  `finished_at`, not `at`. A compaction's `at` is its
  **pre-floor seed** (`message_time.unwrap_or_else(next_order)`), while
  `message_time` is the server time the **Interaction Receipt cut** compares
  against — the floor `render_compactions` sets around it today (ADR-0038, issue
  #1023); the admission applies that floor and returns the final key, which may
  be the receipt's. A timeless compaction (`message_time: None`) takes no cut,
  exactly as today. The other
  synthetic kinds are placed outside any floor window today and keep that. `row`
  is the compaction's merge identity, and its update-in-place lookup is scoped to
  the **live slice**. A terminal revision that arrives after a split or a restart
  is a separate, pre-existing delivery gap (the positional bound can skip it, and
  relaxing the bound would repost the fact); it is tracked in #1049 and
  deliberately **not** part of this batch — no durable-format change lands here.
- **The receipt seam.** `reserve_key(asked_at)` mints a click's receipt and
  floor keys from the order sequence **and installs the initial
  `{asked_at, floor_key}` cut**, so content written during the reply is floored
  before the receipt lands. `retune_cut(floor_key, at)` and
  `forget_cut(floor_key)` adjust or drop it, so a mode change resolving several
  blocks can retune or drop cuts. `record_cut(asked_at, key)` records the cut of a
  receipt written **without** a reservation — a remote client's answer, or a
  click whose reservation was skipped — so later messages still floor at it.
  `reservation_seq`, `reserved_receipts` and every
  `request_id` stay in the accumulator, which owns the reservation's lifecycle.
- **A `Placement` is an owned, linear token, not a borrow.** `place` returns it
  and `push` / `replace` consume it, returning `Result<(), PlacementError>`:
  a **stale** token (one whose scope generation differs from the live scope) or
  a rewrite whose source is missing or names another part is refused without
  inserting anything, never a panic in production. Reuse is already a compile
  error (the token is move-only and not `Clone`); it carries the entry key, an optional merge
  identity (`Option<PartSource>`; a source-less placement takes both the read
  floor and the message-scoped receipt cut, exactly as `part_order_key` plus
  `insert_item` apply them today), the
  display clock, the floor it was placed under, and a private scope generation. Its fields are private with no public constructor and it is
  neither `Clone` nor `Copy`, so a second push of one token is a compile error; a
  token from another scope fails the generation check, so a placement cannot be
  replayed into a later message. Bare order keys stop crossing the seam; the
  five-layer push wrappers collapse.
- **The walk's read floor includes pinned tails.** Each message's `place` floors
  the message at the highest key of earlier messages' placed entries **and their
  pinned tails** (from `walk`'s `pinned_tail_keys`), so a later message's part can
  never be placed above a panel still riding the tail under clock skew. This is
  the deliberate extension to ADR-0038's high-water mark. Within the panel's own
  message, `settle`'s `pinned_keys` covers only tails that precede it in part
  order, never a later one.
- **Equal keys order by class, then read position.** Every source-bearing
  transcript entry carries its part's read position (the message's position in
  the read, then the part index); at equal keys those entries compare by it,
  which makes the settle rule above expressible and keeps a later-visible earlier
  message ahead of a later message's part. A source-less placement and a
  synthetic entry have no read position; they sort **before** positioned entries
  at the same key — so content floored to a receipt's key stays below that
  receipt (ADR-0038 Rule 4) — and keep allocation order among themselves. `seq`
  remains identity; `order` breaks ties only within a class.
- **The rewrite path is the Timeline's.** `Timeline::replace(placement, source,
  kind)` serves a part the server re-sent with different content: the `kind`
  carries the new text, and `source` must name the same transcript part the token
  does (`placement.source.as_ref().is_some_and(|s| s.same_part(&source))`,
  checked by the Timeline; a source-less token is rejected for a rewrite) so a
  token cannot be paired with another part's run. The Timeline prefers the key
  the part's existing entry already holds (else the placement's key), drops every
  entry of that `PartSource` across the boundary while keeping `render_from` on
  the same logical position, and inserts the new run.
- **What the module owns.** The Timeline's entries (`TimelineItem`), the floors,
  the Interaction Receipt cut staircase, the live-slice boundary (`render_from`),
  the sealed-entry set, merge-by-part lookup, key minting (`next_order`) and
  entry identity (`seq`, including the seq a live tool panel keeps from first
  sighting when it settles, ADR-0045). `part_order_key` retires and `insert_item` /
  `insert_kind_after` become one private insert path, so the live-slice clamp is
  computed once. `TimelineKind` keeps its current shape (`Tool(call_id)` among
  the variants) and gains no generic parameter — there is no second entry
  adapter.
- **What does not move.** The live (unsettled) tool tail stays in the
  accumulator as a `TailPlacement` — key, display clock, merge identity, the
  floor it was placed under, the pinned message's identity and **created time**
  (the receipt cut's input), the call id, and the pinned `seq` — plus its panel
  body in the tool map. `MessageScope::pin(facts, call_id)` produces it; it
  cannot be pushed, only settled. `Timeline::rekey(tail, started_at)` applies ADR-0045's
  replacement, clamped to the floor the pin carried.
  `Timeline::settle(tail, panel, read_order, pinned_keys)` re-applies the
  **current** receipt cut for the tail's message created time (a receipt recorded
  after the pin still floors it) and the current read floor for that message,
  recomputed against the settle read. That floor is ADR-0038's earlier-messages
  high-water mark **plus the keys of pinned tails**: an earlier message's pinned
  tail counts as placed content for every later message's `place` (deliberate
  extension — otherwise a later message's part can sort above a pinned panel), and
  `pinned_keys` covers the tails in the panel's own message that precede it in
  part order. The panel seats at `max(current, floor)`, where
  `current` is its key after any `rekey` (the original pin before one), through
  the normal insert path (so the live-slice late-part clamp still applies), and
  at that key it sorts by its own part's read position (see below) — after
  earlier-message entries at the key and before its own message's later parts. A synthetic entry at that key sorts
  before both. Placed content never moves, no key or `order` is
  renumbered, and `seq` stays identity. When the panel's own following entry
  sits at a *lower* key (placed before that floor became visible), the floor
  wins and the panel lands after it. So a tail-only panel cannot jump a receipt cut or
  content read after it was pinned, before or after its start time arrives.
  The deliberate tightenings are the settle clamp, the read-position tie-break
  at equal keys, and the compaction row's live-slice lookup (a terminal fact
  after a split lands on the live card instead of updating a finalized row);
  tests pin them, and every other placement rule and every current test is
  unchanged. Content-keyed dedup and the rewritten
  part's prefix digest stay in the accumulator; the Timeline owns the entry-run
  replace defined above. The **Rendered Cursor** and its staging, the ledger,
  the interactions and the card build stay where they are. `StreamAccumulator`
  keeps its name.
- **The integration suite's seam stays.** `Turn::push_text` / `Turn::push_tool` /
  `Turn::push_reasoning`, their `_at` forms, and `Turn::set_message_floor` keep
  working through a test-only Timeline admission (a scripted, source-less message
  scope); no production caller gains a new path.
- **Behaviour-preserving.** ADR-0038's receipt-skew window, ADR-0071's
  cursorless fallback and every current ordering test are unchanged, apart from
  the tail-only settle clamp, the read-position tie-break and the compaction
  live-slice lookup above. The timeline tests
  move beside the module;
  the guards gain invariant tests (a message scope always clears its floor — a
  later transcript part with a key older than the dropped scope's floor is not
  floored; a walk always resets — the next message's first part carries a key
  below the stale floor; a rewritten run keeps the boundary and its later chunk
  merges into the new run; a rewrite whose source names another part (or none)
  is rejected; a token from another scope is rejected; a placed part keeps its
  key; an untimed part before a message's first tool borrows the previous timed
  part's key (or the message's created time) while one after a tool keeps the
  render-order fallback; a pending tool's late start time settles at that time
  but never below its floor, and lands by read position after an equal-key entry
  of an earlier message; a receipt recorded after the pin still floors it at
  settle; a resolved click's receipt keeps its reserved key while the slice
  reaches it and returns the clamped key after a split past it; a seeded cut
  inside a fence keeps its opener; a **source-bearing** post-click part floored
  to a receipt's key still sorts below that receipt; a post-click compaction row
  still floors at the receipt; a repeated compaction fact updates the row it already has; a
  compaction row finalized by a split takes a terminal-only live row when its
  terminal fact arrives, the bound bypass keys on the carried revision, and a
  second split does not recreate an already-terminal row; a Wake ledger entry
  shows its `finished_at` across a minute boundary rather than its keyed clock;
  and a compaction row shows no clock).

## Consequences

- **Locality**: the placement rule has one body; a repair edits one module
  instead of three and a walk.
- **Leverage**: four walks share one cursor, and a new walk cannot forget it.
- **Test surface**: the guard is the seam — a test arranges a message scope, not
  a floor field, and the rule is verified through the same interface callers
  use.
- **Residual**: the accumulator remains a large struct; its delivery half (the
  Rendered Cursor's staging) is a separate deepening and is not reopened here.
