# A Background Task's ending is one durable fact, shown once by the Rendered Cursor's confirmation

A **Background Task**'s end is re-derived from the transcript on every read,
corrected by a process-local overlay, marked single by a per-chain announce set,
and kept single across a restart by a parallel `rendered` key set — while one
run's end has **two** announce identities (`wake.id` for its Wake, the synthetic
`runtime:<call_id>` / `ended:<call_id>` / `cleanup:<call_id>` for its evidence
retirement), which is the root of #659. This ADR records the ending **once**,
where the **Runtime Reconciliation** derives it, as an ordinary durable fact on
the **Chain Record** — a **Task Ending** per **Task Run Id**, carrying its own
"shown" fact — so exactly-once is the job of the confirmed card write the
**Rendered Cursor** already rides, and both the parallel marks and the second
key space dissolve.

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
  recorded). Without an alias step a Wake arriving after the launch left the read
  (compaction, the V2 page cap) would mint a second record for a run that
  already has one, and the duplicate #659 is about would come straight back. The
  resolution order is fixed, strongest identity first:
  1. an exact **`TaskRunId`** — the launch's call id; a retirement always holds
     it, so evidence and cleanup endings are exact;
  2. an exact **Wake id** already on a record — a Wake's own id is unique, so a
     re-encounter is exact;
  3. else the **newest** record whose captured ids contain the shell/child id the
     Wake reports — the same newest-match rule the entry join already uses
     (#552: a resumed child's later launch supersedes the earlier one), so a
     reused child id cannot attach a Wake to an older run, and a Wake delayed
     past a resume reports the newer launch, the residual #552 already accepts.
  A record already carrying an **evidence** ending is never merged into: the
  resolution stops and the Wake's completion is suppressed (#659), it is not
  attached to some other run. Only when no record matches does the writer mint
  one under the id it holds. "One record per run", the ledger's own rule, is a
  property of the record rather than a spelling convention.
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
  — so a later card's entry is immune to compaction and to a truncated read.
  The shell **output tail** is deliberately NOT captured: it is a network read
  of a record that may only become readable once the run has settled, so it
  stays where it is, spent by the render that places the entry.
- **The ending is recorded when it is derived.** The reconcile writes the
  evidence ending at the moment it derives it; the Cleanup click writes its own;
  the Wake path writes the run's Wake ending when the run has no evidence
  ending. The process-local overlay becomes this process life's **cache** of the
  durable endings, seeded at install, so a restart re-derives nothing and no
  later read can resurrect a retired task — the `retired` set's job is now an
  optimization, not the fact.
- **The entry renders from the durable ending, through the read.** The
  adapter's existing `apply` seam — the one place every reader sees the
  corrected read for free — synthesizes the read's `TaskRetirement` from a run's
  durable ending a card still owes an entry for. `plan_wake_entries` /
  `plan_runtime_entries`, the entry's body, the settle decision and the ledger
  copy stay as they are: one path, one trust source. A restart whose read
  derives nothing still renders the owed entry.
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
  own ending and shows its own completion (#590), and the two records coexist on
  the one run.
- **The legacy spellings are translated once.** On load, `ended:<call_id>` and
  `cleanup:<call_id>` become evidence/cleanup endings and the pre-#659
  `runtime:<call_id>` becomes a **legacy class** that only the retirement's own
  dedup reads — never the Wake gate, exactly as today (#590) — so one release's
  records cannot be miscounted, and only the new shape is ever written. Past the
  bound the old marks age out.
- **The bound never drops an owed ending; a prolonged carrier failure has one
  ratified exception.** The `endings` section is bounded like the `rendered`
  marks it replaces, and an eviction candidate is always a record whose endings
  have all been **shown**: an unshown ending is what the next card owes, so
  evicting it would lose an entry no card displayed — the loss side this family
  never takes (every mark the old section held had already been shown, so its
  bound could only ever cost a duplicate). A run carries at most one ending per
  class (evidence, **Cleanup**, **Wake**), so the section grows with runs, never
  with endings, and two safeguards keep that growth from running away:
  - **Recording needs a placeable render path.** The durable write is made where
    an entry can be planned — a chain with a resolved **Turn** anchor and a live
    card, the same precondition planning already has. A chain that cannot place
    an entry keeps today's process-local correction (its read is still corrected;
    nothing durable is owed), so an entry-less chain cannot accumulate at all.
  - **Saturation is capped and ratified.** A card the platform permanently
    refuses can strand owed entries while a long run of new runs ends. The
    unshown backlog is capped; past the cap the OLDEST owed entry is dropped with
    a `WARN`, while its ending's fact is retained — the run stays ended and no
    card resurrects it, and the loss is the entry alone, under a prolonged
    failure. This is the family's one deliberate loss side, admitted explicitly
    because the alternative — an unbounded sidecar — trades a bounded, logged
    loss for an unbounded one.
  The bound keeps the marks' own residual: past it, a very old **shown** ending
  may render once more after a restart.
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
  path and changes no downstream code.
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
  records — a shown record is the eviction candidate, oldest first, never an
  unshown one — with the two safeguards above (recording needs a placeable render
  path; the unshown backlog is capped, the oldest owed entry dropped with a
  `WARN` while its fact is retained).
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
  riding the confirmation that already writes the cursor.
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
  (and an evidence-ended run never absorbs it), an owed entry survives a chain
  that cannot place entries and renders once that chain gains an anchor, and a
  legacy mark is read once then ages out — plus `ChainRecords`
  unit tests for the new section's round-trip, migration, monotonic shown bits,
  the alias order and bound eviction. Unlike ADR-0079's slices these tests
  change: the restart semantics change with them.

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
