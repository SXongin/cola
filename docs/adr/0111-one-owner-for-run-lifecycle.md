# One owner for the run lifecycle: the Run Lifecycle module

> **Amends ADR-0080 and ADR-0105.** Their ending facts (at most one ending per
> class per run generation; an evidence or Cleanup ending recorded at
> derivation, a Wake's with its completion commit; shown once on a confirmed
> card write) and read-model shapes (a Background Task, an Agent Run) stay as
> recorded. What changes is who applies them: one owner, not four apply paths.
> ADR-0079 is already a five-line pointer and carries no new banner.
>
> It also supersedes ADR-0080's "the seam is an internal module, not a trait —
> one adapter is not a seam": this store trait is the backend→bridge dependency
> boundary (the owner is neutral, `ChainRecords` the one implementation), not an
> abstraction over hypothetical adapters.

## Context

The ticket counted four homes — `backend/runs.rs` (2,079),
`backend/lifecycle.rs` (473), `codex/lifecycle.rs` (3,463) and `bridge/tasks/`
(1,751) — and called the ~7.8k lines one missing seam. Two facts say otherwise.

**Two of the four homes are a different lifecycle.** `backend/lifecycle.rs` is
the `ServerLifecycle` trait (`ensure_attached`, `client_report`, `probe_turn`,
`restart_server`, `detach`) and `codex/lifecycle.rs` implements it; most of that
file is attach state, probe threads, cleanup and `cola doctor`. Its invariant is
server attachment, not a run's phase or ending. **None** of the ten fixes the
candidate refresh sampled (`cb66cb36`, `1f58ec3e`, `3d611fc5`, `ce56cd21`,
`9d1e0d19`, `91b90ee2`, `ea9e157e`, `f2296b54`, `268ec8e3`, `2cbdc717`) touches
either file; they land in `backend/transcript.rs` (5,082) and
`bridge/runtime.rs` (1,620), which the count omitted, plus `turn/render.rs` and
`turn/state.rs`.

What those fixes have in common is one fact applied in four places. A run's
**ending** is already one durable fact (ADR-0080). Yet four read paths each
implement "retire it, settle its launch panel, push its retirement" —
`apply_task_runtime`, `apply_cleanup`, `apply_child_evidence` and
`apply_run_child_evidence` in `backend/transcript.rs` — while the live set is
re-derived by `TasksHandle::apply`, `run_evidence_suspects` /
`unconfirmed_children`, and `derive_runs`'s launch-panel side effect. #659 (two
key spaces for one end) and `268ec8e3` (a terminal run's row must survive while
it owes an ending) are one missing seam seen from two sides.

## Decision

### 1. The invariant is the ending

For every run — a Background Task or a child-only Agent Run, keyed by one
**Task Run Id** and its generation — **at most one ending of each class** is
recorded (evidence, Cleanup, Wake). The live set is its complement: a run is
live when no ending resolves its `(key, generation)`. A client's `status` is
**evidence toward a transition, never the definition of live**: a run the
client declared terminal stays in the read until its child transcript's evidence
records the ending, or it would leave the ledger with no completion entry
(`268ec8e3`).

The predicate is `is_live(run) = !store.resolves(run.key, run.generation)`; the
row, the liveness line, the entry and `Waiting` are projections of it.

### 2. One owner, in the neutral read model

The owner is `RunLifecycle`, under `src/backend/runs/` (the module becomes a
directory: pure derivation in `derive.rs`, the owner and its volatile overlay in
`lifecycle.rs`, the key and ending types beside them). It is
**capability-agnostic**: the caller hands it the surface it may observe
(`LiveRunSurface`), and the capability axes `reports_live_runs` /
`child_evidence` stay render/read-admission gates (ADR-0106).

Its durable tier is a narrow `RunEndingStore` trait — `ended_at`, `record`,
`owed` — implemented by `bridge::chain::ChainRecords`. The volatile overlay
(the `retired` cache and the `unconfirmed` marker set) lives with the owner; it
is a cache of durable endings plus a per-read marker, never a second source of
truth.

The interface is one transition and one projection:

- `observe(session, &mut transcript, surface) -> Option<Pass>` — apply the
  endings and overlay, prune the ended, synthesize an owed retirement;
- `evidence_waits(transcript, anchor) -> Vec<EvidenceWait>` — the suspects a
  cycle may read, with the task-owned rule in one place;
- `retire(session, &mut transcript, key, cause) -> Option<TaskRetirement>` — the
  single transition for all four causes (Wake, runtime verdict, child evidence,
  Cleanup); the store dedupes per `(key, generation, class)`, so an evidence
  ending suppresses a late Wake (ADR-0080) while a Cleanup does not (it says
  nothing about how the run ended).
- `live(transcript) -> impl Iterator<Item = &AgentRun>` — the predicate's projection.

`Pass` keeps today's two-phase commit: evidence/Cleanup recorded at derivation
(a Wake's at its completion commit), the overlay only after the carrier write is
accepted (ADR-0072/0080). Dropping a pass loses nothing durable.

### 3. What the owner does not own

- **Shown / owed.** The ending's exactly-once entry stays with the Chain Record
  and the Rendered Cursor; the owner does not touch `turn/flush.rs`. Its
  invariant is "one ending", not "one entry".
- **The capability axes.** They stay where ADR-0106 put them.
- **The card-delivery cell.** `TaskCell` and its `install` / `rekey` / `cell` /
  `durable_seed` are card-chain state, not run state; they stay in
  `bridge/tasks/`. `TasksHandle` splits: its run half becomes the owner, its
  cell half stays.
- **The read surface.** `SessionTranscript`'s run fields stay public. C12
  guarantees one **writer**, not one reader; privatising ~199 read sites into a
  projection type is a later ticket, because a reader today already sees the
  post-overlay snapshot.

### 4. Scope: the run lifecycle, not the server attachment

`backend/lifecycle.rs` and `codex/lifecycle.rs` are out: a run's life and a
client's attachment are different invariants with different failure modes, and
merging them adds a 3.9k-line attach/diagnostics move with no fix evidence. The
attach seam gets its own research candidate; C8 (`ServerIdentity`) stays HOLD.

## Consequences

- **One writer.** The four `apply_*` paths and `TasksHandle::apply`'s
  prune/synthesis dissolve into `observe` + `retire`; the two suspect lists
  become `evidence_waits`; and `derive_runs` keeps only the pure delegation
  fold, with the launch-panel settle joining `retire`.
- **#684 is absorbed.** Its four-state verdict (Live / Ended(evidence) /
  Unconfirmed / Lost) is this interface's output.
- **The regression suite gains a home.** One test per sampled invariant
  (retire-once per class, terminal-but-owed, turn-start evidence,
  `child_evidence` gating, launch settle, resumed generation, reported
  liveness), plus an absence test for each deleted guard that fails if it
  returns.

## Migration (three landings, each behavior-preserving)

1. **Types and store.** Move the ending and identity types into
   `src/backend/runs/`, fold `TaskRunId` into `RunKey`, define
   `RunEndingStore`, implement it for `ChainRecords`. No behaviour change.
2. **The owner.** Create `RunLifecycle`, move the overlay and the prune /
   synthesis / four retire paths / panel settle behind it, split `TasksHandle`
   into the run half and the card-delivery half. Still no behaviour change.
3. **Delete the duplicates.** Remove the cross-home guards, lower
   `check-structure` on `--bless`, land the seven regression tests and the
   absence tests. The guards go **last**, after the owner is the only writer,
   so each landing is independently reversible.

## Not in scope

- The server-attachment seam (its own research candidate).
- The shown bit, the delivery pipeline (#685) and the Rendered Cursor.
- The capability axes and the card surface (C5, ADR-0106).
- Read-surface privatisation / a projection type (deferred ticket).

## References

- #1140 (this decision), #1141 (the spec), #684 (absorbed), #685 (untouched).
- ADR-0079 (pointer), ADR-0080, ADR-0105, ADR-0106, ADR-0110.

## Amendment (2026-11-23): two methods beside the durable seam's three

§2's `RunEndingStore` pinned three methods (`ended_at`, `record`, `owed`); the
landing (#1169) keeps all three with their names, and adds two, because both
facts are the sidecar's and not the neutral owner's. `observe_run` is the
read's alias-aware resolve-and-fold in one critical section, so a read's
observation cannot race a writer minting a second record for the same run —
the one-lock rule §2's prune depends on. `record_retirement` carries the
retirement→ending identity and label mapping, which belongs beside the bytes it
writes: the label rule is the ledger's, and the owner must stay neutral and
never name a card type.

The same landing gives `retire` a read-less form: §2's sketch takes the observed
transcript, but a **Wake**'s ending is its completion commit's own fact
(ADR-0080), so the transition takes either that read or none (`RetireRead`) and
the Wake cause records the ending without pruning a row — the read prunes it
later through `observe`.
