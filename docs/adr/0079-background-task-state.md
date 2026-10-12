# One owner for a Background Task's lifecycle: the Background Task State module

A **Background Task**'s state has no owner. It is re-derived from the
transcript on every read, then corrected by a hand-assembled pile of
memoization sets that live in three tiers across three modules: the Backend's
process-local overlay (`retired` / `unconfirmed`, the **Cleanup** dismissals),
the `StreamAccumulator`'s per-chain delivery sets (`announced_wakes`,
`handed_over_wakes`, `wake_floor`, `render_marks`), and the **Chain Record**'s
durable sections (`announcements`, `rendered`; the owed mirror is in-memory).
Plus the render's own planning step, which combines them. Each set exists to
police one pairwise interleaving, and none of them owns the task. This ADR
names the deepening: one per-Session **Background Task State** module owns the
lifecycle, and its interface is a read model — the rows the **Background Task
Ledger** renders, plus the completion entries a read owes. It is a pure
consolidation: the volatile/durable split, and every existing semantic, is
unchanged.

## Context

- **The facts, and where they live today.**
  - *Liveness memoization* — `BackgroundTaskOverlay` (`src/backend/transcript.rs:1187`):
    `retired` call ids (a **Runtime Reconciliation** verdict or a **Cleanup**)
    and `unconfirmed` markers. Held by `OpenCodeBackend` and applied by
    `transcript()` to *every* read (`src/opencode/client.rs:344`). Process-local
    by design (ADR-0065/0073): a restart drops it and re-derives.
  - *Per-chain delivery exactly-once* — `StreamAccumulator::announced_wakes`
    (`src/bridge/turn/state.rs:1454`), `handed_over_wakes` (`:1468`),
    `wake_floor` (`:1552`), `render_marks` (`:1564`). `announced_wakes` is the
    completion-entry gate (keyed by a Wake id, or by the retirement's
    `ended:<call_id>` / `cleanup:<call_id>` — issue #659);
    `handed_over_wakes` instead tracks which Wakes this chain handed to a card —
    the in-place-vs-continuation decision (ADR-0066), **not** an entry gate;
    `wake_floor` is the in-memory mirror of the durable **Wake Watermark**;
    `render_marks` the mirror of the durable `rendered` section (ADR-0061/0071,
    #671/#672).
  - *Durable* — the **Chain Record** (ADR-0069), at `src/bridge/chain/records.rs`:
    a per-chain `record`, plus **session-scoped sections that outlive it** —
    `announcements` (the Wake Watermark) and `rendered` (the synthetic
    completion keys a delivered card showed, #672); `owed` (a recoverably-failed
    body's mirror) is in-memory. `ChainRecords::release` drops only the chain's
    `record`, so a fresh, cursorless chain still reads the marks rather than
    starting blind (ADR-0073's 2026-10-11 amendment).
  - *Planning* — `plan_wake_entries` / `plan_runtime_entries`
    (`src/bridge/turn/render.rs:488/553`) combine the floors, anchors, announce
    sets and rendered marks into the entries a render owes. *Recording* —
    `ReconcilePass::commit` / `record_overlay` (`src/bridge/runtime.rs:255/371`)
    write the overlay after an accepted card write.
- **The cost of the missing owner.** Issue #671 — a Wake-less Background Task
  (#454's lost-Wake mode) re-posted its `🔔 … 结束` entry on every restart, seven
  copies in one day — took a 2354-line change (#672, PR #676) whose whole job was
  to thread one more durable fact through the card-write pipeline; 585 of those
  lines landed in `flush.rs`. The behavior was one line; the *place the fact had
  to be recorded* was nine. A read of the area shows why: it is a sustained hot
  spot (244 commits in 90 days), and every symptom so far (#454, #464, #463,
  #465, #588, #590, #659, #671) added a set or a gate rather than concentrating
  the state.
- **The precedent is in-repo.** ADR-0069 did exactly this move for a **Card
  Chain**: it collapsed one durable fact per symptom (`wake_watermarks.json`,
  `live_cards.json`, four `serde(skip)` marks, two in-memory overlays) into one
  **Chain Record** module with one lifecycle. The task lifecycle is the same
  shape one level down.
- **#671's asked decision is already resolved, with ADR-0073's two named
  residuals left standing — this ADR does not reopen it.** #671 asked whether
  the "one entry on the newest chain" sanction still holds and, if it should be
  tightened, proposed a durable mark separate from the Wake Watermark.
  #672/#676 landed exactly that mark, and ADR-0073's 2026-10-11 amendment
  records the tightened semantics: once a delivered card has shown a synthetic
  retirement's entry, a later chain — restart included — does not re-render it,
  because the marks are session-scoped and survive the observing chain's record
  release. A restart may still create a card; its *entry* no longer replays.
  **Surfacing the conflict rather than overriding it**: that guarantee is
  bounded, exactly as ADR-0073 states — the marks are bounded
  (`MAX_RENDERED_KEYS`, oldest evicted first), so a very old retirement can
  render once more after a restart; and two chains that PLAN the same retirement
  concurrently, before either body's write is accepted, can each render it once.
  #671's symptom — the same entry replaying on every restart — is what the
  landed fix stops; the two residuals above are its remaining, admitted
  exceptions. What this ADR leaves open is a different, structural question —
  see the next bullet — not the sanction's validity.
- **Two adjacent deepenings are deliberately *not* folded in.** Making an
  **ending** a durable fact rendered by the **Rendered Cursor** (so exactly-once
  is the cursor's job, not a parallel `rendered` set) would dissolve the
  two-key-space problem behind #659 and let the module own one ending identity;
  it is a structural change to ADR-0061/0065/0073's design, and it is #683's own
  ADR, not this one. And resolving liveness from its three sources (transcript,
  runtime registries, child transcript) behind one **TaskLiveness** interface is
  a pure function, a different concern from owning state. Both are their own
  ADR/PR.

## Decision

- **A per-Session `Background Task State` module owns the lifecycle.** It owns
  all three tiers — the volatile liveness memoization, the per-chain delivery
  exactly-once sets, and the durable facts — and, internally, keeps the
  volatile/durable split exactly as it is today. The **Chain Record** remains
  its durable adapter through a narrow task-scoped interface; no second sidecar
  is created.
- **Its interface is a read model.** Given a resolved read, it yields the live
  **Background Task Ledger** rows and the completion entries the read owes —
  both card-agnostic (no `PlannedEntry` / `PlannedAnnounce` card internals leak
  into the interface). Callers ask it one question instead of orchestrating four
  sets.
- **It never owns the Turn's ending, the liveness resolution, or the render.**
  `settle` / `TurnSettle` stay in the Turn layer (the ending vocabulary is
  **Disposition**, ADR/GLOSSARY); `apply_task_runtime` / `apply_child_evidence`
  stay where they are until the TaskLiveness deepening; the card stays in
  `src/feishu/card/ledger.rs`.
- **The seam is internal.** A new module inside `src/bridge/`, not a new trait:
  there is exactly one adapter behind it, so a port would be a hypothetical
  seam. The adapter's `BackgroundTaskOverlay` bytes fold into the module, but
  the OpenCode adapter keeps applying it to every read through a module-owned
  handle (so every reader still sees a corrected read);
  `Backend::retire_background_tasks` / `set_unconfirmed_tasks` leave the trait.
  **Surfaced ADR conflict**: ADR-0069 left that overlay with its Backend
  adapter, "read at the seam as a predicate, never moved into the record". This
  ADR relocates the bytes' *home*; ADR-0069's actual invariant — in-memory,
  never in the durable record, applied at the read seam — is preserved and the
  apply point does not move.
- **The durable-ownership *decision* is the module's; the staging *mechanics*
  stay in the flush.** The module says which facts a write makes durable (and
  which are volatile); the flush owns staged cursors, the owed mirror and the
  capacity bounds. The two meet at the module's one commit seam — so the
  card-write pipeline does not leak into the task module.
- **Zero behaviour change in the first slice.** Ticket #680 is a pure physical
  move — the new module and the read-model type definitions only; it relocates
  no state and changes no caller. The state moves next (#681 the per-chain
  sets, #682 the overlay), and the callers are replaced after that. Semantics —
  the volatile/durable split and every existing rule — are preserved.

## The interface (design round, 2026-10-12)

Three interfaces were designed in parallel — one minimising the entry-point
count, one maximising extension, one optimising the common caller. The chosen
shape is the common-caller-first one carrying the minimal design's claim
vocabulary, and rejecting the maximal design's generality:

- **The common path is two calls.** `read(scope, &mut transcript) -> TaskView`
  runs off the cards lock and awaits: it applies the module's remembered
  liveness to the read, derives the live rows, plans the completion entries
  through every gate (the anchor, the durable Wake floor, the announce set, the
  durable rendered marks), and spends each entry's shell-output tail.
  `apply(scope, view)` runs under the cards lock: it re-checks every gate
  against the durable marks and the delivered owed keys the flush resolves
  today (`Turn::recorded_marks`) — a mark a queued delivery landed during the
  await drops the plan — installs the rows, records the in-memory claims, and
  returns what moved. Callers stop knowing the anchor, the directory, the
  cadence, the output reads or the `recorded_marks` re-check. **Lock order is
  one-way: `write_lock` → `cards` → task state.** `apply` takes the module's
  per-Session lock *inside* the cards lock, and that lock is a plain
  **synchronous** mutex — never awaited and never held across an await — so
  acquiring it inside the cards lock cannot suspend while holding that lock, and
  no task-state guard spans `read`'s awaits. No path may hold the task-state
  lock and then reach for the cards lock or `write_lock`. (The slice #681
  records the order beside `CardsHandle`'s existing lock doc.)
- **Commit is two methods, preserving ADR-0067/0072's accepted-vs-delivered
  split.** `commit_accepted` records the volatile liveness (the retirements and
  the post-verdict unconfirmed set) after a carrier write the delivery layer
  *accepted* — delivered, or recoverably failed and owed for retry
  (`FlushOutcome::Accepted`, ADR-0073/0067), **not** mere keyed-queue admission
  (ADR-0072); `commit_delivered` advances the durable **Wake Watermark** and
  folds the body's synthetic keys into the durable `rendered` marks at the
  delivered-write choke point. Neither is reachable from the other, so #590 and
  PR #595 are preserved as types rather than as rules four call sites remember.
- **One claim vocabulary.** A completion entry's exactly-once claim is one type
  with two variants — a **Wake** (which may advance the Watermark and carries
  the hand-off flag) and a **Retirement** (key `ended:<call_id>` for an evidence
  ending, `cleanup:<call_id>` for the user's own — one spelling per claim class,
  issue #659; the pre-#659 `runtime:<call_id>` is read, never written — which
  never may). Today's `PlannedAnnounce` + the separate `synthetic_key` argument
  + the retirement's string format collapse into it, so "a retirement is not a
  Wake" becomes unrepresentable to violate.
- **The overlay keeps its free application.** Its bytes move into the module,
  but it is still applied at the read seam — a module-owned handle the OpenCode
  adapter holds — so every reader (the reap's settle, the Snapshot status line)
  still sees the corrected read without calling anything. **The apply point
  stays where it is; only the home of the bytes moves.** Applying it inside
  `read` instead was rejected because a reader that skips `read` would see the
  raw launch list.
- **Restart re-derivation is one call.** `restore(record, scope)` seeds the
  durable tier (the Wake floor and the rendered marks) at chain install, so the
  seed-then-install race the four `seed_durable_announcements` sites police
  today cannot occur.
- **Rejected from the other two designs.** The maximal-flexibility design's
  public rules table (`FactKey` / `FactRule` over `fn` pointers) is premature
  for today's three ending sources; its "one identity, one key" idea survives
  as an internal representation. The minimal design's two semantic deltas —
  committing the overlay on *delivery* rather than *acceptance*, and a
  session-scoped announce set — are **not** taken in this slice; they belong to
  the durable-ending deepening (#683).

## Why

- **Locality: bugs concentrate in one module.** Today a new ending source, a new
  restart window, or a new exactly-once key touches five files. The 244 commits
  in 90 days are the symptom; one owner is the cure.
- **Leverage: five callers, one interface.** Session Sync, the Turn's drain, the
  follow/settle loop, the **Cleanup** click and the renderer all hold the same
  question; today each assembles its own answer from the sets.
- **The interface becomes the test surface, and it is a *read model*.** Feed it
  one read and assert the rows and owed entries; a new ending source is one more
  input to a unit test, not another branch to thread through the card pipeline.
- **Risk isolation explains sequencing.** The reason #672 was slow was mixing a
  large structural question with a semantic one. Naming the owner first — with
  every semantic fixed — is what makes the durable-ending and TaskLiveness
  deepenings small.

## Alternatives considered

- **Leave it as is.** Each symptom adds a set; #671 is the measured cost of that
  path (a 2354-line change for a one-line behavior). Rejected.
- **One module per tier** (a liveness store, an announce store, a durable
  store): keeps the pairwise interleavings as cross-module contracts, so the
  missing owner survives with more files. Rejected.
- **A new port (trait) for the state, with an in-memory adapter for tests.** One
  real adapter is a hypothetical seam; the testability win comes from the read
  model, not from a trait. Rejected.
- **Fold the durable ending (#683) in now.** It dissolves the two announce key
  spaces behind #659, but it is a structural change to ADR-0061/0065/0073's
  design; shipping both at once repeats #672's mistake. Deferred to its own
  ADR/PR.
- **Fold the TaskLiveness resolution (#684) in now.** A pure function and
  a state owner are different concerns; each earns its own interface. Deferred.
- **Absorb the card renderer.** The module would then depend on card JSON and
  could not be tested on a read model. Rejected.
- **Move the staging mechanics (staged cursors, owed mirror, caps) behind the
  module's seam.** That pulls the card-write pipeline inside the task module and
  makes `view` depend on how a card is written — the coupling this ADR exists to
  undo. Rejected.

## Consequences

- The module's read model (its `read` / `apply` interface) becomes the new test
  surface once the callers are routed through it; until then the `ledger.rs` /
  `wake.rs` / `reap.rs` integration tests are the end-to-end net, and the pure
  move lands with them unchanged.
- Migration is staged: ticket 1 is a pure physical move with no behavior change;
  later tickets replace callers (the bridge read wrapper, `ReconcilePass`,
  `plan_*`, the Cleanup click) and, last, retire `Backend::retire_background_tasks`
  / `set_unconfirmed_tasks`.
- **#671's replay is already fixed; what stays deferred is its *structure*.**
  The session-scoped `rendered` marks (ADR-0073's 2026-10-11 amendment) stop the
  unbounded every-restart replay, within that ADR's bounded-mark and
  concurrent-plan residuals (see Context), so this ADR reopens no sanction. It
  defers only #683: making the ending itself durable so exactly-once is the
  **Rendered Cursor**'s job and the two announce key spaces behind #659
  dissolve.
- `plan_wake_entries` / `plan_runtime_entries` and the overlay move behind the
  seam: callers no longer see `announced_wakes`, `wake_floor`, `render_marks`
  or the `BackgroundTaskOverlay`.
- The volatile tier stays per-life: a restart still re-derives retirement, and
  the durable marks still suppress the re-render — the module changes *where*
  the facts live, not *whether* they survive a restart.

## Domain note

A new **Background Task State** glossary entry names the module; it is distinct
from the **Background Task Ledger**, which is where its rows and entries render.

## Related

- Implementation slices (pure moves, zero behaviour change):
  - **#680** — introduce `src/bridge/tasks/` and move the read-model type
    definitions (`TaskKind`, `TaskEnding`, `TaskOutput`, `TaskLedgerRow`,
    `TaskCompletionEntry`, `LedgerRowClock`, `LedgerChange`, `LedgerCadence`)
    into it; `src/feishu/card/ledger.rs` keeps only formatting and re-exports
    them.
  - **#681** — move the per-chain delivery sets (`announced_wakes`,
    `handed_over_wakes`, `wake_floor`, `render_marks`) out of
    `StreamAccumulator` into a per-Session store keyed by chain generation, with
    thin forwarding methods so no caller changes.
  - **#682** — move the liveness overlay's bytes into the module while the
    OpenCode adapter keeps applying it through a module-owned handle; remove
    `Backend::retire_background_tasks` / `set_unconfirmed_tasks`.
- Deferred deepenings (each needs its own ADR, not this one):
  - **#683** — make the ending a durable fact rendered by the Rendered Cursor,
    so exactly-once is the cursor's job and the two announce key spaces
    dissolve.
  - **#684** — one TaskLiveness resolution over transcript, runtime and child
    evidence.
  - **#685** — one confirmed-write seam that commits every durable fact a card
    write carried.
- Depended-on ADRs: ADR-0061 (the Wake Watermark), ADR-0065 (Runtime
  Reconciliation), ADR-0067 (the Pending Card Update outbox and the
  accepted-vs-delivered delivery contract), ADR-0069 (the Chain Record),
  ADR-0071 (the Rendered Cursor), ADR-0072 (card delivery order), ADR-0073
  (background-task exits).
