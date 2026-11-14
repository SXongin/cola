# The read model carries tool semantics and agent runs

> **Amends**: ADR-0042 (built-ins are tailored by tool id, payloads are
> interpreted by the Platform) and ADR-0053's rejected option ("per-built-in-tool
> typed payloads inside the read model"). The opaque fallback those ADRs chose
> survives as `ToolKind::Other` / `ToolDetail::Opaque`.
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
  68 occurrences of six ids in `tool_render.rs` alone.
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
`ToolCall` (`src/backend/transcript.rs:1574`) gains four adapter-supplied fields:

```rust
pub struct ToolCall {
    pub identity: ToolIdentity,        // vendor name + call id — unchanged
    pub kind: ToolKind,                // the coarse category
    pub title: Option<String>,         // the client's human title (ACP `title`)
    pub locations: Vec<ToolLocation>,  // files the call touches
    pub status: ToolStatus,
    pub started_at: Option<i64>,
    pub input: Option<Value>,          // raw, unchanged
    pub metadata: Option<Value>,       // raw, unchanged
    pub output: ToolOutput,            // unchanged
    pub detail: ToolDetail,            // typed only for the shared shapes
}
```

**2. `ToolKind` is ACP's ten, and the adapter declares it.**

```rust
pub enum ToolKind {
    Read, Edit, Delete, Move, Search, Execute, Think, Fetch, SwitchMode, Other,
}
```

`Other` is the default. A tool the adapter does not recognise is `Other` +
`Opaque`; it renders through today's raw path, exactly as ADR-0042 requires.
`kind` is never inferred from a third-party payload — the adapter, which owns
its client's contract, is the only writer.

**3. `ToolDetail` is typed only where the semantics are shared.**

```rust
pub enum ToolDetail {
    None,
    Command { command: String, cwd: Option<String>, exit: Option<i64>,
              timeout: bool },
    Program { source: String, language: Option<String>,
              calls: Vec<ToolCallRef> },   // Code Mode: a program that calls tools
    FileEdits(Vec<FileEdit>),   // path, create/modify/delete/rename, unified diff
    Search { query: Option<String>, matches: Option<u64>, scope: SearchScope },
    Read { path: String, preview: Option<String>, directory: bool },
    Delegation { runs: Vec<AgentRunRef>, effect: DelegationEffect },
    Opaque,
}
```

The rule for adding a variant is the ADR-0090 rule for DTOs: **a second
adapter must need it.** `Command`, `Program`, `FileEdits` and `Delegation` have
two producers today (OpenCode's built-ins and Codex's item kinds); `Search` and
`Read` have two and are cheap; nothing else grows until a third client forces
it. The existing `ToolCall::background_launch()` marker is unchanged — it is a
durable launch fact, not a detail of the command.

`Program` is the **Code Mode** shape: the model writes a program that calls
tools from inside a sandbox, and only its output returns. It is not an
OpenCode-specific idea — Cloudflare named the pattern (Code Mode, 2025-09),
Anthropic calls it programmatic tool calling (2025-11), OpenAI's Responses API
and Pi both ship it, and the ecosystem already spells the tool four ways
(`codemode`, `exec`, `execute_tools`, `code_execution`). It gets a detail now
because its shape differs from `Command` (source + nested calls, not one command
line) and because the current `execute` panel already has a bespoke renderer:
mapping it to `Opaque` would keep a name arm, which is the cost this ADR
removes. The renderer counts as the second consumer the rule asks for.

**4. `AgentRun` is a neutral transcript fact.**

```rust
pub struct AgentRun {
    pub run_id: String,            // child session/thread id when it has one
    pub kind: AgentRunKind,        // Subagent | Collaboration | Task
    pub label: String,
    pub agent: Option<String>,     // subagent type / agent path / model
    pub started_at: Option<i64>,
    pub status: RunStatus,         // Running | Completed | Failed | Cancelled | Lost
    pub activity: Option<RunActivity>, // newest live tool or phase (ADR-0054's fact)
    pub launched_by: Option<ToolCallRef>,
    pub addressed_by: Vec<ToolCallRef>,
}
```

A call relates to a run in one of three ways: it **launches** one
(`task`/`subagent`, `spawnAgent`), **addresses** an existing one (`sendInput`,
`wait`, `closeAgent`), or **reports** on one (`subAgentActivity`). A report
item updates its run; it is not a panel. A call that addresses a run is a
receipt on that run, not a timeline panel (the surface consequence is ADR-0106's;
the timeline rule is recorded there).

The run is a read-model fact. The bridge's background overlay
(`src/bridge/tasks/*`) stays cola-side: it answers *what ended it and has the
ending been shown*, which is cola's own durable bookkeeping, not the client's
statement.

**5. The card derives what it shows from `(kind, detail)`.** `identity.name` is
diagnostic and a fallback title, never a renderer key. A known shape renders
through one renderer per shape; `Opaque` renders raw.

**6. Nothing else moves.** Raw `input`/`metadata`/`output` stay; the `Part` enum
stays; the transcript's message/turn projections stay; durable formats do not
change. This is an additive read-model change, so P1 can land with a
byte-identical card.

### Migration (each step shippable, in order)

- **P1 — fields only.** Add `kind`/`title`/`locations`/`detail`; every adapter
  fills them; the card ignores them. The invariant is that every existing card
  is byte-identical.
- **P2 — Execute.** One Execute family: `Command` (the three shell ids, with the
  Host Terminal Card sharing the output block) and `Program` (Code Mode's
  source plus its nested calls). `command_input`'s vendor split and the
  `execute` name arm go.
- **P3 — FileEdits.** One diff renderer; `edit`/`patch`/`apply_patch`/
  `codex/file_change` collapse onto it.
- **P4 — AgentRun.** Codex activity reports fold into runs; the liveness line,
  the ledger and the completion entry read runs; `TASK_TOOLS` and
  `child_session_id`'s name check go.

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
  budget and the fold, and a client should not ship Feishu JSON. Accepted for
  the factual `title` and `locations`, which are the client's knowledge.
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
