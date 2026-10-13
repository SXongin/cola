# A Background Task's ending is one durable fact, shown once by the Rendered Cursor's confirmation

> **Ratified (2026-10-14, spec #699)**: three points where the landed
> implementation diverges from this ADR's literal wording — the **Wake** path
> records its ending with its completion entry's commit, not at derivation; the
> plan's durable read suppresses an entry, it never creates one; and the
> retirement's entry gate is one entry per run, whatever class. The
> "recorded at derivation / an entry-less chain included" promise is scoped to
> the evidence and **Cleanup** endings. See the amendment below.

A **Background Task**'s end is re-derived from the transcript on every read,
corrected by a process-local overlay, marked single by a per-chain announce set,
and kept single across a restart by a parallel `rendered` key set — while one
run's end has **two** announce identities (`wake.id` for its Wake, the synthetic
`runtime:<call_id>` / `ended:<call_id>` / `cleanup:<call_id>` for its evidence
retirement), which is the root of #659. This ADR records each ending **once** —
where the **Runtime Reconciliation**, the user's **Cleanup**, or a **Wake**
derives it — as an ordinary durable fact on the **Chain Record**, keyed by
**Task Run Id** and carrying its own "shown" fact, so exactly-once is the job of
the confirmed card write the
**Rendered Cursor** already rides, and both the parallel marks and the second
key space dissolve. A **Wake** completion is the exception to "where it derives
it": its ending is recorded with its completion entry's commit, not at
derivation — see the 2026-10-14 amendment below.

## Context

- **The facts, and where they live today.** A run's *liveness* is derived from
  the transcript's launch record (which never flips) and corrected by a
  process-local overlay — `OverlayState::retired` / `unconfirmed` in
  `src/bridge/tasks/handle.rs`. An **evidence ending** (a runtime verdict, a
  shell the runtime no longer knows, a subagent's own terminal transcript) and
  the user's **Cleanup** are produced by a read, hold no durable record, and are
  re-derived on every later read; the overlay is deliberately in-memory, so a
  restart drops it and the next read derives the same retirements again
  (ADR-0065/0073). Exactly-once for a retirement's completion entry is therefore
  patched on top with a **second** durable fact — the `rendered` section of the
  Chain Record (ADR-0073's 2026-10-11 amendment, #671/#672) — whose keys must
  ride the card-write pipeline, which is why #672 cost 2354 lines (585 of them
  in `flush.rs`).
- **Two identities for one end.** A Wake's completion entry is keyed by
  `wake.id`; an evidence retirement's by `runtime:<call_id>`, respelled
  `ended:<call_id>` / `cleanup:<call_id>` by ADR-0073's 2026-10-13 amendment.
  The two spaces assert exactly-once independently, so one run's end could be
  recorded twice (#659); the amendment's fix is a **cross-key gate** — the
  Wake's plan reads the retirement's key. The duplicate is gone, but the second
  key space it had to reach into is the structure #659 is really about.
- **The call id is not always available.** A retirement is derived from the
  read's live list, so it always carries the launch and its call id; a Wake may
  arrive for a run whose launch the read no longer carries (compaction, the V2
  page cap), and then it names the run by the shell/child id it reports and
  nothing more (`wake_completion_entry`'s "never a made-up one" fallback).
- **A monotone clock cannot carry the coverage.** The retirement's `finished_at`
  is the runtime's own completion time, so two runs can deliver out of clock
  order: a run the runtime reports late may have ended *before* a run whose
  entry a card already showed, and a "newest shown clock" floor would swallow
  it. The coverage has to name the runs it covers.
- **The ADR conflict, surfaced.** ADR-0065/0073 deliberately keep a retirement
  process-local and accept a duplicate over a loss ("a retirement is observed
  only while a waiting card can receive it"). Making the ending durable changes
  what a restart knows and when a wait ends; this ADR reopens that trade and
  restates it (below) rather than overriding it silently. It also makes the
  **Cleanup**'s dismissal durable, which ADR-0073 deliberately deferred
  ("in-memory first, re-derived at restart").
- **Predecessor.** ADR-0079 named this deepening and deferred it to its own ADR;
  its slices (#680–#682, spec #687) have landed the **Background Task State**
  module that will own the new facts, so this ADR changes semantics on top of an
  owned state, not a move.

## Decision

**One run, one record; one shown fact per ending.**

- **The identity is the run, and the run's ids are aliases of one record.** A
  new `TaskRunId`: the launch's call id when the read carries the launch, else
  the shell/child id a Wake itself reports (never an invented one). One run's
  endings — evidence, the user's Cleanup, a Wake completion — live under that one
  id, and the record carries every id the run is known by (the launch's call id,
  its shell/child id, its tool call id, and the Wake id whose completion it
  recorded). **"Once" is once per ending, not once per run**: a run holds at most
  one ending of each class, because the classes are different claims about it —
  an evidence ending says the run ended, so it suppresses a late Wake's
  completion entry (#659), while a **Cleanup** says nothing about how the run
  ended, so a cleaned run's late Wake is its own ending with its own entry
  (#590). The ENTRY gate is separate and is one entry per run, whatever class
  (the 2026-10-14 amendment below). "Recorded once" fixes HOW each ending is
  recorded — at derivation, except that a **Wake** completion's ending is
  recorded when its completion entry is committed to a card's render (per the
  2026-10-14 amendment below) — never re-derived, never a second copy. Without
  an alias step two writers naming
  one run differently — a
  Wake arriving after the launch left the read (compaction, the V2 page cap), or
  a reconciliation read that still carries the launch but whose call id the
  record no longer keys on — would each mint a record, and the duplicate #659 is
  about would come straight back. So **every writer resolves before it mints**,
  and it resolves by every id it holds, strongest identity first:
  1. an exact **`TaskRunId`** — the launch's call id; a retirement always holds
     it, so evidence and cleanup endings are exact;
  2. an exact **Wake id** already on a record — a Wake's own id is unique, so a
     re-encounter is exact;
  3. else the **newest** record naming any other id the writer holds (the
     shell/child id, the tool call id) — the same newest-match rule the entry
     join already uses (#552: a resumed child's later launch supersedes the
     earlier one), so a reused child id cannot attach a Wake to an older run and
     a Wake delayed past a resume reports the newer launch, the residual #552
     already accepts.
  The first match wins and a writer mints only when nothing matched, so the order
  in which the two writers arrive cannot matter: a launchless Wake resolves the
  record a reconciliation read keyed by call id (its shell/child id is on that
  record), and a reconciliation read resolves the record a launchless Wake keyed
  by shell id (the read's shell/child id names it) — one record per run, however
  the run is named. A record already carrying an **evidence** ending is never
  merged into: the resolution stops and the Wake's completion is suppressed
  (#659), it is not attached to some other run.
- **A `Task Ending` is durable and outlives the record.** A new `endings`
  section of the Chain Record, **session-scoped** like `announcements` and the
  `rendered` section it replaces — `release` drops only the chain's record — so
  the ending is a fact about the run, and a restart after a terminal release
  still knows both that the run ended and whether its entry was shown. Each
  run's record
  carries the endings it has, each with its class, its clock, its evidence, and
  the identity a `BackgroundTask` needs to be re-synthesized (tool name, call
  id, shell/child id, start clock, label). The label and the start clock are
  captured when the ending is derived — that read necessarily carries the launch
  — so an **evidence** or **Cleanup** ending's entry is immune to compaction and
  to a truncated read, because `apply` re-synthesizes it from the durable
  record. A **Wake** completion's ending is the exception: it is recorded with
  its entry's commit, and while the read still carries that Wake a later card
  can still place an owed entry (the durable identity fills the label when the
  launch has left the read); once the Wake itself leaves the read, nothing
  recovers it, because `apply` never synthesizes a Wake ending — the 2026-10-14
  amendment below.
  The shell **output tail** is deliberately NOT captured: it is a network read
  of a record that may only become readable once the run has settled, so it
  stays where it is, spent by the render that places the entry.
- **The ending is recorded when it is derived.** The reconcile writes the
  evidence ending at the moment it derives it; the Cleanup click writes its own;
  the Wake path writes the run's Wake ending when the run has no evidence
  ending. The Wake path's write is scoped by the 2026-10-14 amendment below: it
  rides the commit of the run's completion entry, not the derivation. The
  process-local overlay becomes this process life's **cache** of the
  durable endings, seeded at install, so a restart re-derives nothing and no
  later read can resurrect a retired task — the `retired` set's job is now an
  optimization, not the fact.
- **The entry renders from the durable ending, through the read.** The
  adapter's existing `apply` seam — the one place every reader sees the
  corrected read for free — synthesizes the read's `TaskRetirement` from a run's
  durable ending a card still owes an entry for. `plan_wake_entries` /
  `plan_runtime_entries`, the entry's body, the settle decision and the ledger
  copy stay as they are: one path, one trust source. A restart whose read
  derives nothing still renders the owed entry — an **evidence** or **Cleanup**
  retirement, which `apply` re-synthesizes from the durable record; a **Wake**
  completion's ending has no such path and is recoverable only while the read
  still carries that Wake (the 2026-10-14 amendment below).
- **Exactly-once is the ending's own shown fact, advanced by the confirmed
  write.** A built body stages the endings its timeline rendered with the
  existing **Rendered Cursor** stage; the confirmation — a PATCH `Ok`, a create
  `Ok`, or a queued payload the drain delivers — flips their shown bits in the
  SAME Chain Record write as the cursor, the gap coverage and the Wake Watermark
  it already carries. The `rendered` key set dissolves; the parallel `owed`
  mirror stays, because a recoverably-failed body's delivery may land after its
  accumulator is gone — it is the *same* staging mechanism, now holding
  `TaskRunId`s rather than the three old spellings.
- **The Wake Watermark stays, and so does its floor.** The watermark answers
  "was this Wake's arrival announced?", which is a Wake fact; the ending's shown
  bit answers "was this run's ending shown?", which is an ending fact. They
  legitimately differ — a Wake that opened a 承接 line announced without
  rendering an entry — so the floor remains an entry gate. A Wake writes an
  ending only when the run has no **evidence** ending: a 🧹 **Cleanup** claims
  nothing about how the run ended, so a cleaned run's late Wake still writes its
  own ending and shows its own completion (#590), and both endings live on the
  one run's record, each with its own shown state.
- **The legacy spellings are translated once.** On load, `ended:<call_id>` and
  `cleanup:<call_id>` become evidence/cleanup endings and the pre-#659
  `runtime:<call_id>` becomes a **legacy class** that only the retirement's own
  dedup reads — never the Wake gate, exactly as today (#590) — so one release's
  records cannot be miscounted, and only the new shape is ever written. Past the
  bound the old marks age out.
- **The ending is recorded when it is derived, unconditionally; the bound never
  evicts an owed entry, and one saturation case is an explicitly ratified loss.**
  The durable write happens wherever the **Runtime Reconciliation** (or the
  **Cleanup**, or the Wake path) derives an ending — an entry-less chain
  included, because the fact is about the run, not about the card that will show
  it; an ending derived while no card can place it still renders on the first
  card that can. The 2026-10-14 amendment below scopes this promise to the
  evidence and **Cleanup** endings; the Wake path's ending is the stated
  exception. An ending is **owed** until a confirmed write shows it
  (**shown**), or until the saturation cap below surrenders it
  (**surrendered**). Only an owed ending is ever rendered, so a surrendered one
  is never re-offered, and a record with no owed ending is ordinary
  bound-eviction material. A run carries at most one ending per class (evidence,
  **Cleanup**, **Wake**), so the section grows with runs, never with endings, and
  it is bounded: owed records up to the saturation cap plus everything else up to
  the bound. The saturation cap is the family's one loss side, taken for the
  single shape that cannot otherwise bound the sidecar — a carrier the platform
  permanently refuses while a long run of runs keeps ending. Past the cap the
  OLDEST owed ending becomes **surrendered** with a `WARN`: the fact is retained
  (the run stays ended, no card resurrects it) and only its entry is given up,
  because the alternative — an unbounded backlog — trades a bounded, logged loss
  for an unbounded one. The bound's own residual stays: past it, a very old
  **shown** ending may render once more after a restart.
- **Out of scope, deliberately.** The unconfirmed marker (`⚠️ 状态待确认`) stays
  process-local and re-derived at restart: it is "no positive evidence yet", an
  endingless state whose "an unasked read must not drop it" rule (ADR-0073) is
  about a read's scope, not a session fact. `handed_over_wakes` stays a
  per-chain volatile set: it answers ADR-0066's in-place-vs-continuation
  question, which is not about an ending. Both are recorded as accepted
  residuals, not silently absorbed.

### Where the exactly-once gate lives — a deliberate deviation from #683's wording

#683 asks that the ending be rendered by the **Rendered Cursor**'s frontier, "so
exactly-once is the cursor's job". This ADR keeps the cursor's **job** — the
confirmed card write is the one machinery that already distinguishes delivered
from owed from refused, and it advances the ending's shown bit in the same Chain
Record write as the cursor itself — but not the cursor's **frontier** as the
gate's home. The design round (2026-10-13) settled why the literal reading
cannot carry it, and this ADR records the deviation rather than pretending it
away. **The originating issue records the same supersession in its decision
comment** ([#683 comment](https://github.com/SXongin/cola/issues/683#issuecomment-6144150664)),
and the implementation spec ([#699](https://github.com/SXongin/cola/issues/699))
restates it, so the ask, the record and the plan agree:

- **Lifetime.** The Rendered Cursor is a field of the **records** section and
  dies with `release`; the shown fact must outlive the record — a restart after a
  terminal release must not re-show an entry. A frontier-carried gate would need
  a session-scoped twin of the cursor.
- **Ordering.** The frontier names a *transcript position*; a retiring ending's
  clock is the runtime's own completion time, and endings deliver out of clock
  order, so a "newest shown" scalar would swallow a legitimately late entry. The
  gate has to name the runs it covers — a set, not a frontier.
- **What the cursor still owns.** Exactly one durable write path: a built body
  stages the endings its timeline rendered, and the confirmation flips their
  shown bits in the SAME Chain Record write as the cursor, the gap coverage and
  the Wake Watermark. The parallel `rendered` key set and the deep link into the
  Wake's key space dissolve, which is the structural outcome #683 is after.

## The reopened trade (ADR-0065/0073)

ADR-0073 (PR #595) settled that every caller commits a reconcile pass to the
overlay **only after the flush that carried the read's entries was accepted**, so
a permanently refused carrier records nothing and a task is never hidden with no
card having rendered the change. Its premise was "nothing will ever be written
for that payload". A durable ending falsifies the premise: the entry is **owed**,
and the next card that can place an entry renders it (the frontier did not move
past it). The trade therefore becomes:

- **No-loss, kept and strengthened.** A run the runtime has confirmed ended is
  ended, and its ending survives a restart, a refused carrier and a replaced
  card; the entry renders on the next card that can carry it, exactly once, by
  the ending's shown fact. The card no longer has to be *in hand* for the
  observation to count — ADR-0065's "a retirement is observed only while a
  waiting card can receive it" is retired with the process-local overlay that
  made it necessary.
- **Duplicate, accepted, unchanged in direction.** Two chains that PLAN the same
  ending concurrently — before either body's write is confirmed — can each show
  it once, because the shown bit is claimed at confirmation. Past the shared
  bound, a very old ending can be shown once more after a restart. Both stay the
  duplicate-over-loss side this family always trades toward.
- **The refusal side flips.** A carrier a card permanently refuses no longer
  keeps the task live: the ending is durable, so the task leaves the ledger and
  the turn can settle ✅ while the entry is still owed. This is the visible
  behavior change, and it is honest — the runtime said the run ended; the entry
  is history, not the ending's condition.
- **The Cleanup's dismissal becomes durable.** A user's 🧹 click now outlives a
  restart; ADR-0073's deferral of durable dismissal persistence is superseded.

## Considered options

- **Keep the process-local overlay and the `rendered` marks** (the status quo,
  #672). Rejected: the measured cost of one durable fact riding the card-write
  pipeline was 585 lines in `flush.rs`, and the two announce key spaces remain
  the structure #659 is about.
- **A monotone "newest ending shown" clock** instead of a per-run shown fact.
  Rejected: retirement clocks are the runtime's own completion times and deliver
  out of order, so a floor would swallow a legitimately late entry.
- **Fold the coverage into the Rendered Cursor's field and drop the section
  entirely.** Rejected: the cursor is record-scoped and dies with `release`, while
  the shown fact must outlive the record (a restart after a terminal release must
  not re-render) — the cursor would need a session-scoped twin, which is the same
  fact with more moving parts.
- **Have the render plan read the durable endings directly** (a second input
  beside `transcript.task_retirements`). Rejected: two sources would need
  de-duplicating in the plan; routing the durable ending through `apply` keeps one
  path and changes no downstream code. The 2026-10-14 amendment below records
  the landed shape: the plan's durable read can never **create** an entry (it
  may fill an entry's identity, label and start clock when the read no longer
  carries the launch), so the one entry source is unchanged.
- **Make the Wake's entry ride the ending's shown bit alone, retiring the Wake
  floor.** Rejected: a Wake that opened a 承接 line announced without an entry, so
  a single ending gate would re-render it on a later card.
- **Persist the unconfirmed marker too.** Rejected for this ADR: it is not an
  ending, and its rule is about a read's own scope.
- **Capture the shell output tail into the durable ending.** Rejected: it is a
  network read whose record may not be readable until the run settles; deriving
  must not spend it.
- **Hard-cut the legacy marks.** Rejected: one release's `rendered` data would
  replay entries; translation is cheap and keeps #590's asymmetry explicit.
- **Leave the unshown backlog unbounded** (no saturation cap: one small record per
  run that ends while a carrier is dead). Rejected: it trades a bounded,
  `WARN`-logged loss of a single owed entry for unbounded storage and unbounded
  record writes — the worse failure of the two, and the one a sidecar cannot
  afford.

## Consequences

- `ChainRecords` gains the `endings` section (session-scoped, bounded, with the
  shown bits); the `rendered` section and the pre-#659 spelling retire into its
  legacy read; `MAX_RENDERED_KEYS` becomes the bound over a session's run
  records — a record with no owed ending is the eviction candidate, oldest
  first — plus the saturation cap over owed records, past which the oldest owed
  ending becomes **surrendered** with a `WARN` (its fact retained).
- `StreamAccumulator`/`StagedCursor` no longer carry a separate `rendered` key
  list — the staged body carries the endings it rendered, and the confirmation
  flips their shown bits beside the cursor; `Turn::recorded_marks` and
  `retirement_recorded` read one fact.
- The overlay's `retired` set is seeded from the durable endings and stops being
  the source of truth; a restart's first read carries the retirements the
  durable record knows, so the reconcile re-derives nothing for them.
- The wake step's evidence gate reads the run's ending class instead of probing
  two key spellings; `ended_announce_key` / `cleanup_announce_key` /
  `legacy_retirement_announce_key` retire into the ending's class.
- Cost is unchanged per read; the new writes are one synchronous record write at
  derivation (a reconcile verdict, a cleanup click, a Wake) and the shown bits
  riding the confirmation that already writes the cursor. The Wake's write is
  scoped by the 2026-10-14 amendment below: it rides its completion entry's
  commit, not the derivation.
- **A failed ending write degrades to re-derivation after a restart.** Every
  Chain Record write is best-effort and atomic (the existing sidecar contract): a
  lost write leaves this process life correct — the task stays retired, its entry
  stays shown, the in-memory record is the truth for this life — so only a
  restart can observe the loss. A restart then sees the pre-ADR-0080 shape for
  that run: an **evidence** ending is re-derived by the next **Runtime
  Reconciliation** verdict (the shared throttle bounds it to one per Session per
  30 s) and a **Wake** ending is re-derived from the transcript, so the worst
  case is one entry shown twice — the duplicate side this family trades toward.
  The one ending with no re-derivation is the **Cleanup**'s (a user action leaves
  no transcript trace): a lost write for it costs ADR-0073's old behavior after a
  restart — the wait returns and the user can click again — and no retry
  machinery is added for that case.
- Vocabulary: **Task Ending** and **Task Run Id** join `GLOSSARY.md`; **Chain
  Record**, **Background Task State**, **Runtime Reconciliation**, **Cleanup**
  and **Wake** record the durable ending.
- Tests: the bridge integration seam (`tests/ledger.rs`, `wake.rs`, `reap.rs`
  and the #672 restart suite) is the acceptance net — a restart renders an owed
  entry the read no longer derives, an entry shown once is never re-shown, a
  cleanup survives a restart, a cleaned run's late Wake still shows its own
  completion (#590), an evidence ending suppresses a late Wake across a restart,
  a Wake naming only a reused shell/child id resolves the NEWEST matching run
  (and an evidence-ended run never absorbs it), the alias order holds in both
  arrival orders so two writers never make two records for one run, an owed
  ending derived while no card could place it renders on the first card that
  can, a surrendered ending is never re-offered and its record is evictable, and
  a legacy mark is read once then ages out — plus `ChainRecords`
  unit tests for the new section's round-trip, migration, the three ending
  states, the alias order and bound eviction. Unlike ADR-0079's slices these
  tests change: the restart semantics change with them.

## Amendment (2026-10-14): three ratifications of the landed implementation

The implementation (spec #699, tickets #700–#704) landed three behaviours that
this ADR's literal wording over-states or leaves implicit. Each is ratified
here; the sentences each qualifies are scoped in place at the bullets they name,
so the ask, this record and the plan agree.

- **"Recorded when derived" does not cover the Wake path.** The **evidence**
  ending — the reconcile's verdict, `Lost`, the child-evidence ending, written
  by `record_new_endings` (`src/bridge/runtime.rs`, through
  `TasksHandle::record_evidence_ending`) — and the **Cleanup** ending, written
  by the click, are recorded at derivation, independent of any card; an
  entry-less chain included, and `TasksHandle::apply`'s `synthesized_retirement`
  (`src/bridge/tasks/handle.rs`) turns such an owed ending into the read's
  `TaskRetirement`, so it renders on the first card that can place it. A **Wake**
  completion's ending is **not** written at derivation: `Endings::record` inside
  `commit_planned_entries` (`src/bridge/turn/render.rs`) records it as an `Owed`
  `TaskEnding` only where the run's completion entry is committed to a card's
  render, and that write's confirmation flips it to `Shown` (`mark_shown`,
  `src/bridge/chain/records.rs`). An `Owed` Wake ending is not dead: while the
  read still carries that Wake, a later card can still place its entry —
  `plan_wake_entries` plans it again under that card's own anchor, the commit
  re-reads the durable facts, and `facts.identity(run)` fills the label when the
  launch has left the read. What has **no** path is recovery once the **Wake
  itself** leaves the read (compaction, or a newer Turn's anchor passing it):
  `synthesized_retirement` (`src/bridge/tasks/handle.rs`) returns `None` for a
  Wake ending, so — unlike the evidence and **Cleanup** endings, which `apply`
  re-synthesizes from the durable record however late — `apply` never
  synthesizes a Wake. **Ratified**: a Wake completion rides its own Wake's turn
  and its own resumed-work handoff (ADR-0066), so the pre-ADR behaviour is
  preserved, not widened. The accepted residual is that narrow: a Wake ending is
  recoverable only while its Wake is still read, so an entry that no card ever
  commits or shows is lost the moment the Wake leaves the read — where an
  evidence or **Cleanup** ending would still render. Giving the Wake ending a
  card-independent source would instead be the second-source shape this ADR's
  *Considered options* rejects. The **Decision**
  bullet *"The ending is recorded when it is derived"* and the unconditional
  promise in *"The ending is recorded when it is derived, unconditionally ...
  an entry-less chain included ... renders on the first card that can"* are
  scoped by this: both describe the evidence and **Cleanup** endings, and the
  Wake path is the stated exception.
- **The plan-time durable read is a gate, not a second entry source.** This
  ADR's *Considered options* rejects *"Have the render plan read the durable
  endings directly (a second input beside `transcript.task_retirements`)"*
  because *"two sources would need de-duplicating in the plan"*. That is **not**
  the shape that landed: the durable snapshot (`Endings::facts`) can never
  **create** an entry. `plan_wake_entries` / `plan_runtime_entries`
  (`src/bridge/turn/render.rs`) plan every entry solely from the read's
  transcript (`transcript.wakes`, `transcript.task_retirements`), with the owed
  retirements synthesized by the adapter's `apply` seam (`TasksHandle::apply` →
  `synthesized_retirement`, `src/bridge/tasks/handle.rs`) — the same one entry
  source the pre-ADR code had. Beside the suppress-only gates
  (`facts.recorded(...)`, `facts.shown_any(...)`, `wake_ending_spoken_for`), the
  snapshot may also fill in an entry's **content**: `facts.identity(run)` feeds
  the Wake completion plan (`src/bridge/turn/render.rs:697-702`) and the runtime
  entries fall back to the ending record's label and start clock
  (`:831-847`) — the identity, label and duration #702's acceptance criteria
  require when the read no longer carries the launch. It never adds an entry, so
  there is nothing to de-duplicate in the plan and the rejected option's reason
  does not apply to the shipped shape. The **Decision** bullet *"The entry
  renders from the durable ending, through the read"* describes that `apply`
  synthesis and content fallback, not a second plan input.
- **The retirement's entry gate is one entry per run, whatever class.**
  `plan_runtime_entries`'s gate is deliberately class-agnostic — the
  `acc.ending_announced_any(&run) || facts.shown_any(&run)` check
  (`src/bridge/turn/render.rs`; `EndingFacts::shown_any`,
  `src/bridge/chain/records.rs`; `ending_announced_any`,
  `src/bridge/tasks/state.rs`) — so a **shown Cleanup** ending suppresses a
  later **Evidence** retirement entry for the same run: that run renders ONE
  retirement entry, not two. This preserves the pre-ADR "exactly once per call
  id" rule and is deliberate. It is distinct from a **Wake** completion's entry,
  which is per its own ending (`facts.shown(run, TaskEndingClass::Wake)`,
  `wake_ending_spoken_for`): a cleaned run's late Wake still shows its own
  completion (#590), and both endings live on the one run's record. The
  **Decision** sentence "Once is once per ending, not once per run" answers how
  many endings a run may hold; the ENTRY gate is separate and is one per run,
  whatever class.

One blanket reading closes the class: wherever this ADR — or `GLOSSARY.md` —
states the general "recorded when the ending is derived" or "a restart renders
the owed entry" rule without naming the **Wake** exception, it is read subject
to that exception, never as a second, contradictory rule.

## Related

- Implementation: the spec issue and its slices (model + migration; the read
  path; the render path; the shrinkage; cleanup + restart regression).
- Reopened: ADR-0065 (the process-local overlay and "observed only while a
  waiting card can receive it"), ADR-0073 (the accepted-write gate, the
  `rendered` marks, the deferred durable dismissal).
- Depended on: ADR-0061 (the Wake Watermark, kept), ADR-0066 (the in-place
  handoff, unchanged), ADR-0067/0072 (accepted vs delivered, and the write
  order), ADR-0069 (the Chain Record), ADR-0071 (the Rendered Cursor, whose
  stage carries the confirmation), ADR-0079 (the Background Task State module).
- Sibling deepenings: #684 (one TaskLiveness resolution — orthogonal) and #685
  (one confirmed-write seam — the shrinkage here is what makes it smaller).
- Issues: #683 (this ADR), #671, #672, #659, #590, #580, #454.
