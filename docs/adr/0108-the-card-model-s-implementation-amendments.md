# The card model's implementation amendments

> **Amends**: ADR-0080 (a run's durable key), ADR-0105 (the detail variants and
> additive fields), ADR-0106 (the `mcp` axis and `background_tasks_supported`),
> ADR-0107 (the name-only token table), implementing spec #1057.

## Context

Spec #1057's implementation met several places where the decision text and the
wire disagreed once both adapters and a resumed child were in front of the
code. Each is recorded here rather than edited into the earlier ADRs.

## Decision

**1. An `AgentRun`'s durable key is its child id whenever one is known.** A
report that names no child creates a call-keyed run under its own call id; when
a later fact names the child, the key moves to the child id once, before any
ending is derived (a terminal childless run is never adopted). This keeps one
run's ending key independent of where the read window began — the property
ADR-0080's amendment exists for. A Background Task's `TaskRunId` selection is
unchanged.

**2. `ToolDetail` gains `Plan`, `Skill` and `Ask`.** These are carrier facts,
not vendor ids: the plan/skill/question surfaces must render from a neutral
declaration, so the closed name-only token table is retired and an adapter maps
its tool to the detail. This is the same argument ADR-0105 recorded for
`Program`, now earned by the vocabulary pass.

**3. The `mcp` capability axis is the `/mcp` pull surface — statuses plus the
directory-owned Location Form.** ADR-0103 forbids a status-only `/mcp` card, so
a client publishing only statuses declares the axis absent. The placeholder
`background_tasks_supported` helper is removed; `reports_live_runs()` remains
the derived ledger gate.

**4. Additive read-model fields carry facts the card would otherwise re-parse:**
`Command.description` (V1 `bash`'s line), `Program.error` (Code Mode's own
failure flag), `Read.image` (the image-view presentation), `AgentRunRef.status`
(a multi-child launch's per-receiver state) and `AgentRun.observed_at` (the
newest report clock, so a replayed older observation cannot revive a run).
They are additive and do not change a durable format.

**5. A same-second child turn start is accepted as this run's.** Codex
serializes a turn's start in whole seconds, so a resumed run and the previous
turn can share a second; the guard compares the turn start only and refuses a
strictly earlier second. It does not compare the completion clock (also
whole-second, so a same-second completion floors below a millisecond start).
Rejecting same-second evidence would reject a fresh child's first turn, which
starts with its spawn.

**6. A terminal run's evidence wait is scoped to its own Turn.** The evidence
read and the settle decision share one cutoff: the newest user message's server
time. A terminal child-bearing run dated before it is history — no read fetches
its evidence any more, so it must not keep a later Turn waiting on an ending
that can never arrive. A run with no start cannot be placed in any Turn, so it
is never history: it keeps owing its read (otherwise its ending would be
dropped). A live run blocks regardless: its exit is still owed.

**7. A childless run's only exit is a Cleanup.** The call-keyed run a report
creates has no child transcript, so no evidence read can end it and no Turn
cutoff may drop it: the reconcile marks it ⚠️ 状态待确认, the ledger keeps that
row on whichever Turn's card is waiting (history included), and the user's
cleanup click mints the 🧹 `已清理` ending. That holds whether the client
reports the run live or terminal — its own status is not a durable ending.

**8. A Background Task's ledger kind reads its identity first, its neutral noun
last.** A child session means a subagent, whatever the task is called, so a
third adapter's own launch noun never picks the renderer (ADR-0105 §5). The one
identity-less shape is a V2 `subagent` launch whose metadata omitted its
session id; there the neutral `subagent` noun — never a vendor id — is the fact
left, and every other identity-less task stays the tolerant shell default.

**9. `/sub` and `/sub attach` name a run by its child id, never its creating
call.** Ticket #1061 says "`/sub`'s identity is the run's `run_id`", but a
call-keyed run's `run_id` is the creating call, which is not a session and
cannot be taken over: adopting it stores an id no read can resolve. The `/sub`
row and attach therefore use the child id — the only session identity — and a
run that has not learned one (an id-less report the ledger already renders as
⚠️) shows no `/sub` row. `run_id` remains the run's stable identity in the read
model, the ledger and the completion entry.

## Consequences

- The durable chain-record format is unchanged: a client-reported Failed or
  Cancelled word stays process-local, and an entry that renders after a restart
  reads the evidence class's word (`结束`).
- A third adapter answers the detail variants and capability struct; it reads
  no card code.
- The spec's canonical-shapes text is the layout, with these fields as the
  recorded extension.
