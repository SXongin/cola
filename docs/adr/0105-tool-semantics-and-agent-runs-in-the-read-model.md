# The read model carries tool semantics and agent runs

> **Amends**: ADR-0042 (built-ins are tailored by tool id, payloads are
> interpreted by the Platform), ADR-0053's rejected option ("per-built-in-tool
> typed payloads inside the read model") and ADR-0090's second-adapter rule (the
> `Program` exception is recorded in the decision below). The opaque fallback
> those ADRs chose survives as `ToolKind::Other` / `ToolDetail::Opaque`.
>
> Evidence, counts and the current-surface inventory:
> `docs/research/feishu-cards/r1-surface-model.md`.

## Context

ADR-0053 moved the conversation's **shape** into a neutral read model and left
tool **meaning** to the Platform, on purpose:

> "Per-built-in-tool typed payloads inside the read model. Rejected: tool payload
> interpretation is the Platform's deliberate specialization (ADR-0042)."

That trade was correct for one adapter. The built-in ids were one vendor's, and
their payload shapes were a contract cola could follow. The second adapter
inverts the economics:

- **Codex's tool ids are protocol names.** `commandExecution`, `fileChange`,
  `webSearch`, `image`, `collabAgentToolCall` and `subAgentActivity` are item
  kinds, not display words. The adapter namespaces them `codex/*`
  (`src/codex/read_model.rs:2563`), and the card grows a shape-gated arm per id —
  78 mentions of six ids in `tool_render.rs` alone (68 of them quoted match
  positions).
- **Three consumers interpret the same call, and they disagree.** The Tool Panel,
  the Permission card (`describe_permission`, `src/bridge/request/kind.rs:2999`)
  and the Background Task Ledger (`task_kind`/`task_label`,
  `src/bridge/tasks/model.rs:358`) each keep their own name→meaning table. The
  same Codex command is 「命令」 on its panel, 「执行 Shell 命令」 on a permission
  card, and a `shell` row in the ledger.
- **Sub-agents are four display paths for one concept.** OpenCode's
  `task`/`subagent` carries a child session, a liveness line and a ledger row;
  Codex's `collabAgentToolCall` is a panel per call with no liveness and no
  ledger row; `subAgentActivity` is a lifecycle *event* rendered as a *panel*.
  The read model has no fact that says "these are the same thing".
- **The missing vocabulary already has a published shape.** ACP's `tool_call`
  carries `kind`, `title`, `locations`, `status` and typed content, and
  `tool_call_update` patches optional fields. Its `ToolKind` is exactly the
  coarse category the card needs, and a third adapter would have to answer it
  anyway.
- **The repo already proves the pattern twice.** The plan/todo surface is one
  neutral `TodoList` fed by `todowrite` and by `turn/plan/updated`
  (`src/backend/transcript.rs:94`); `/model` is one client-declared
  `ModelSurface` the card only renders (ADR-0092). Neither has vendor arms.

## Decision

**1. A tool call carries neutral semantics beside its vendor identity.**
`ToolCall` gains four adapter-supplied facts — a `ToolKind`, a human `title`, the
`locations` it touches and a typed `detail` — while its identity, status, raw
input/metadata/output and output blocks are unchanged. **This ADR fixes what
those facts mean; their layout, the detail variants and every per-adapter
mapping live in spec #1057.** The separation the layout must keep is the
decision: identity belongs to the vendor, semantics is the adapter's
declaration.

**2. `ToolKind` is ACP's ten, declared by the adapter.** `Read`, `Edit`,
`Delete`, `Move`, `Search`, `Execute`, `Think`, `Fetch`, `SwitchMode`, `Other`.
`Other` is the default and the fallback; a tool the adapter does not recognise is
`Other` + opaque detail and renders raw (ADR-0042). A kind is never inferred from
a third-party payload — the adapter owns its Agent Client's contract and is the only
writer.

**3. `ToolDetail` is typed only where the semantics are shared.** The variants
are `Command`, `Program`, `FileEdits`, `Search`, `Read`, `Delegation`, `None` and
`Opaque`. The rule is ADR-0090's: a second adapter must need a variant.
`Command`, `FileEdits` and `Delegation` have two producers today; `Search` and
`Read` have two. `Program` is the argued exception (ADR-0090 is amended for it):
one producer today, but the Code Mode pattern is general, its shape differs from
`Command`, and leaving it opaque would keep the very `execute` name arm this
change removes. No variant carries a vendor id, and a vendor tool with no shared
shape is `Opaque`.

**4. `AgentRun` is a neutral transcript fact — one run per child agent.** The
run's shape is #1057's; these are the rules it must satisfy:

- **One key space with the Task Run Id.** `run_id` **is** ADR-0080's Task Run
  Id — there is no alias and no second string. It is the child id when the fact
  that creates the run names one, otherwise the creating call's id (with `#<i>`
  for an id-less receiver among several). A run whose creating fact named no
  child keeps that id and fills `child_id` when it learns one. **A launch that
  names no receiver creates no run**: a completion naming N children creates N
  runs, each keyed by its child id. Once created, a run keeps its key forever.
- **A child may be launched more than once.** A resumed `task` passes the prior
  child id, so the launch relation is a set, not one call.
- **Three relations, compactly enumerated.** A call **launches** a run
  (`task`/`subagent`, `spawnAgent`), **addresses** an existing one (`sendInput`,
  `sendMessage`, `followupTask`, `resumeAgent`, `wait`, `closeAgent`,
  `interruptAgent`, `listAgents`) or **reports** on one (`subAgentActivity`).
  `resumeAgent` resumes a child and is an address, not a launch; `wait` and
  `listAgents` may name no target and then address the Turn's live runs as a
  set. A report is an *observation*: it is applied when its state is newer, and
  identical observations collapse — never deduped by the call id alone, because
  the wire reuses the item id between an activity's start and its completion.
- **Addressing may name no target.** Codex's `wait` writes empty receiver lists;
  such a call addresses the Turn's live runs as a set and renders a turn-level
  receipt, never a guess at one run.
- **No vendor `kind`.** `task`/`subagent`/`spawnAgent`/a collaboration call all
  mean "a child agent this Turn set running"; the relations, `agent` and `label`
  carry the provenance.
- **Liveness carries neutral semantics.** The activity line's newest call carries
  its `ToolKind`, `ToolDetail` and identity (so it can render a tool token) plus
  the child's pending wait; a bare vendor name is not enough.
- **`RunStatus` is the client's statement, not cola's ending.** `Running`,
  `Completed`, `Failed`, `Cancelled`, `Unknown`. Cola's durable `TaskEnding`
  (Wake / RuntimeEnded / Lost / Cleaned) answers how **cola** learned it ended and
  owns the exactly-once rules (ADR-0065/0073/0080). They are never folded: a
  runtime-confirmed end renders `结束` while the client still says `Running`, and
  a **Cleanup** renders `已清理` with no `RunStatus`. The ledger reads
  `TaskEnding` first and `RunStatus` only for an end the client itself reported.

The run is a read-model fact. The bridge's background overlay
(`src/bridge/tasks/*`) stays cola-side: it answers *what ended it and has the
ending been shown*, which is cola's own durable bookkeeping, not the client's
statement.

**5. The card derives what it shows from `(kind, detail)`.** `identity.name` is
diagnostic and a fallback title, never a renderer key. The one exception is a
display token, not a renderer: ADR-0107's `tool_token` may read the name in the
`Other` + `None` arm for the three name-only tokens (`plan`/`skill`/`ask`), and
otherwise returns it only as the opaque fallback. A known shape renders through
one renderer per shape; `Opaque` renders raw.

**6. Nothing else moves.** Raw `input`/`metadata`/`output` stay; the `Part` enum
stays; the transcript's message/turn projections stay; durable formats do not
change. This is an additive read-model change, so P1 can land with a
byte-identical card.

### Migration

P1 fields only (every existing card byte-identical) → P2 Execute → P3 FileEdits
→ P4 AgentRun. **The per-step field lists, adapter mappings and acceptance
criteria are spec #1057's**, not this ADR's; the order and the P1 invariant are
the decision recorded here.

## Why

- **One vocabulary instead of N adapters × M consumers.** A third adapter
  answers a mapping table; it edits no card.
- **The three tables become one fact.** A call's kind is written once, where the
  client's contract is understood.
- **Sub-agents stop being a special case per client.** One run fact serves four
  surfaces.
- **It is the ADR-0090 decision-7 generalisation, now earned.** The read model
  was generalised after the second generation existed (ADR-0053); the tool
  surface is generalised after the second adapter exists — the same trigger, one
  layer up.

## Considered options

- **Keep the shape gates and add a third set for the next adapter.** Rejected:
  that is the current cost, paid again per adapter. The gates exist because the
  card cannot trust a name; the fix is something it *can* trust.
- **Put the whole presentation in the adapter** (a `tool_view` like
  `model_surface`). Rejected for layout and copy: the card owns the words, the
  budget and the fold, and an Agent Client should not ship Feishu JSON. Accepted
  for the factual `title` and `locations`, which are the client's knowledge.
- **A typed payload per built-in id.** Rejected: a vendor id is not a
  cross-adapter contract, and it would grow a variant per client.
- **A generic "valid JSON → pretty-print" fallback.** Rejected again, for
  ADR-0042's reason: it makes a third-party shape a presentation contract.
- **Make `kind` a field of `ToolOutput` or a card-side enum.** Rejected: it is
  a property of the call, read by the bridge's ledger and permission flows too,
  not only by the card.

## Consequences

- **A lying adapter gets a wrong renderer.** The same residual risk ADR-0042
  recorded for a same-named tool applies to a wrong `kind`; the mitigation is
  the same — conformance tests per adapter, and shape-gated refinements inside a
  detail.
- **The vendor name survives, and must.** `identity.name` still says
  `codex/file_change`; logs, debug output and a fallback title use it.
- **Per-shape renderers replace per-name arms.** Each migration step deletes
  arms and adds one renderer; the card-size estimate moves beside the renderer
  that produces it (`panel_estimate`'s hand-maintained arithmetic,
  `src/bridge/turn/state.rs:591`, is the coupling this removes).
- **ADR-0042's core rule stands.** Third-party tools stay opaque; a built-in's
  tailoring now keys on a semantic category rather than an id, which is a
  narrower dependency, not a wider one.
- **The card's words are not decided here.** The token set, the phase names and
  the Input/Output labels are ADR-0107's.

## Not decided here

- Whether the *card* needs a general block model. The five detail shapes and one
  opaque path are sufficient; a block tree remains optional (research note §4.5).
- How Codex's addressed calls render on the timeline. ADR-0106 records the
  receipt rule; the exact row is P4's UI work.
- Per-chat multi-client routing. ADR-0090 defers it; the read model above is a
  prerequisite, not a decision about it.
