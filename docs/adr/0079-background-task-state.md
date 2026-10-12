# One owner for a Background Task's lifecycle: the Background Task State module

A **Background Task**'s state has no owner. It is re-derived from the
transcript on every read, then corrected by a hand-assembled pile of
memoization sets that live in three tiers across four modules: the Backend's
process-local overlay (`retired` / `unconfirmed`, the **Cleanup** dismissals),
the `StreamAccumulator`'s per-chain delivery sets (`announced_wakes`,
`handed_over_wakes`, `wake_floor`, `render_marks`), and the **Chain Record**'s
durable sections (`announcements`, `rendered`, `owed`). Each set exists to
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
    `wake_floor` (`:1552`), `render_marks` (`:1564`). The first two keep one
    completion entry per chain; `wake_floor` is the in-memory mirror of the
    durable **Wake Watermark**; `render_marks` the mirror of the durable
    `rendered` section (ADR-0061/0071, #671/#672).
  - *Durable* — the **Chain Record** (ADR-0069): `announcements` (the Wake
    Watermark), `rendered` (synthetic completion keys a delivered card showed,
    #672), `owed` (a recoverably-failed body's mirror, `src/bridge/chain/records.rs`).
  - *Planning* — `plan_wake_entries` / `plan_runtime_entries`
    (`src/bridge/turn/render.rs:481/551`) combine the floors, anchors, announce
    sets and rendered marks into the entries a render owes. *Recording* —
    `ReconcilePass::commit` / `record_overlay` (`src/bridge/runtime.rs:255/370`)
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
- **Two adjacent deepenings are deliberately *not* folded in.** Making an
  **ending** a durable fact rendered by the **Rendered Cursor** (so the
  exactly-once is the cursor's job, not a parallel `rendered` set) would change
  ADR-0065/0073's duplicate-over-loss trade and dissolve the two-key problem
  behind #659; and resolving liveness from its three sources (transcript,
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
  seam. The adapter's `BackgroundTaskOverlay` folds into the module, and the
  bridge reads through a wrapper that applies the module's view;
  `Backend::retire_background_tasks` / `set_unconfirmed_tasks` leave the trait.
- **The durable-ownership *decision* is the module's; the staging *mechanics*
  stay in the flush.** The module says which facts a write makes durable (and
  which are volatile); the flush owns staged cursors, the owed mirror and the
  capacity bounds. The two meet at one `commit(accepted keys)` seam — so the
  card-write pipeline does not leak into the task module.
- **Zero behaviour change in the first slice.** Ticket 1 is a pure physical
  move (new module, sets relocated, callers untouched); later tickets replace
  the callers one by one. Semantics — the volatile/durable split and every
  existing rule — are preserved.

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
  `apply(scope, view)` runs under the cards lock: it re-checks every gate — a
  mark a queued delivery landed during the await drops the plan — installs the
  rows, records the in-memory claims, and returns what moved. Callers stop
  knowing the anchor, the directory, the cadence, the output reads or the
  `recorded_marks` re-check.
- **Commit is two methods, preserving ADR-0067/0072's accepted-vs-delivered
  split.** `commit_accepted` records the volatile liveness (the retirements and
  the post-verdict unconfirmed set) after any accepted carrier write;
  `commit_delivered` advances the durable **Wake Watermark** and folds the
  body's synthetic keys into the durable `rendered` marks at the delivered-write
  choke point. Neither is reachable from the other, so #590 and PR #595 are
  preserved as types rather than as rules four call sites remember.
- **One claim vocabulary.** A completion entry's exactly-once claim is one type
  with two variants — a **Wake** (which may advance the Watermark and carries
  the hand-off flag) and a **Retirement** (key `runtime:<call_id>`, which never
  may). Today's `PlannedAnnounce` + the separate `synthetic_key` argument + the
  `runtime:<call_id>` string format collapse into it, so "a retirement is not a
  Wake" becomes unrepresentable to violate.
- **The overlay keeps its free application.** Its bytes move into the module,
  but it is still applied at the read seam — a module-owned handle the OpenCode
  adapter holds — so every reader (the reap's settle, the Snapshot status line)
  still sees the corrected read without calling anything. This **revises Q8's
  "the adapter stops filtering"**: the apply point stays; only the home of the
  bytes moves. Applying it inside `read` instead was rejected because a reader
  that skips `read` would see the raw launch list.
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
  the durable-ending deepening above.

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
- **Fold the durable ending (candidate 2) in now.** It is the root of #671's
  *class* and of #659, but it changes ADR-0065/0073's trade; shipping both at
  once repeats #672's mistake. Deferred to its own ADR/PR.
- **Fold the TaskLiveness resolution (candidate 3) in now.** A pure function and
  a state owner are different concerns; each earns its own interface. Deferred.
- **Absorb the card renderer.** The module would then depend on card JSON and
  could not be tested on a read model. Rejected.
- **Move the staging mechanics (staged cursors, owed mirror, caps) behind the
  module's seam.** That pulls the card-write pipeline inside the task module and
  makes `view` depend on how a card is written — the coupling this ADR exists to
  undo. Rejected.

## Consequences

- The module's read model is the new test surface: `ledger.rs` / `wake.rs`
  integration tests stay as the end-to-end net, and unit tests feed the three
  inputs and assert rows / owed entries / overlay.
- Migration is staged: ticket 1 is a pure physical move with no behavior change;
  later tickets replace callers (the bridge read wrapper, `ReconcilePass`,
  `plan_*`, the Cleanup click) and, last, retire `Backend::retire_background_tasks`
  / `set_unconfirmed_tasks`.
- **#671's class is not closed by this ADR, only its cause is named.** Until the
  durable-ending deepening lands, a Wake-less retirement is still exactly-once
  through the `rendered` marks, and the two-key problem behind #659 remains.
- `plan_wake_entries` / `plan_runtime_entries` and the overlay move behind the
  seam: callers no longer see `announced_wakes`, `wake_floor`, `render_marks`
  or the `BackgroundTaskOverlay`.
- The volatile tier stays per-life: a restart still re-derives retirement, and
  the durable marks still suppress the re-render — the module changes *where*
  the facts live, not *whether* they survive a restart.

## Domain note

A new **Background Task State** glossary entry names the module; it is distinct
from the **Background Task Ledger**, which is where its rows and entries render.
