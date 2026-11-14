# R1 — Feishu card surfaces: what is shared today, and the neutral model to build on

Research date: **2026-11-14**. Scope: the **Platform** (Feishu) card layer —
which surfaces exist, how much of them is already agent-neutral, where the
second **Agent Client** (Codex) forced a second implementation, and what a
third adapter (ACP, or anything else) would have to answer. This is a research
note, not a decision: the decisions it recommends are listed at the end, each
phrased so it can become an ADR.

Evidence tags: **[code]** = read from `src/` at the cited line, **[adr]** = an
existing ADR, **[acp]** = the Agent Client Protocol v1 documentation,
**[measured]** = counted in this tree on the research date.

> **Decided (2026-11-14).** This note's recommendations became **ADR-0105**
> (tool semantics and agent runs in the read model), **ADR-0106** (capabilities
> gate the surfaces) and **ADR-0107** (the card speaks in tokens and copy).
> The naming question the note left open is settled there: fixed tokens are
> English (`shell`, `script`, `waiting`, `reasoning`, `responding`), interface
> copy stays Chinese until an i18n catalog exists. §4.2–§4.4 are kept in step
> with ADR-0105/0106; §7 and §8 below are the pre-decision record, and the ADRs
> supersede them where they differ.

---

## Verdict

The **read side** of the backend seam is already neutral (ADR-0053), and two
surfaces have already been generalised the right way — the model picker
(ADR-0092: the client declares a `ModelSurface`, the card renders it) and the
plan/todo list (`SessionTranscript.todo_lists`, fed by OpenCode's `todowrite`
and Codex's `turn/plan/updated` **[code]** `src/backend/transcript.rs:94`).
Those are the precedent, and they work.

The **tool surface** is where neutrality stops. The read model carries a tool
call as a vendor **name** plus raw JSON (`ToolCall { identity, status, input,
metadata, output }`, `src/backend/transcript.rs:1574`), so every consumer
re-derives what the call *means* from its name and payload shape. There are
three parallel name→meaning tables:

| Consumer | Where | What it decides |
|---|---|---|
| Tool Panel | `src/feishu/card/tool_render.rs` | icon, folded title, input line, output body, file target, fold shape |
| Permission card | `src/bridge/request/kind.rs:2999` (`describe_permission`), table at `:2959` | icon + 「执行 Shell 命令」-style sentence |
| Background ledger | `src/bridge/tasks/model.rs:358` (`task_kind`), `:371` (`task_label`) | `shell` vs `subagent`, and the row label |

`tool_render.rs` alone is **9,725 lines** of a **33,645-line** card layer
**[measured]**, and the six adapter-namespaced ids (`command_execution`,
`file_change`, `web_search`, `image`, `collab_agent`, `sub_agent_activity`)
appear **68 times** in it, tests included **[measured]**. Every one of those
arms is a *presentation consequence of a semantics gap*: the card cannot ask
"is this an edit?", only "is this `patch`, `apply_patch`, `edit`, or
`codex/file_change`?".

That is the finding: the card layer is not missing reuse, it is missing a
**semantic vocabulary** to reuse over. The fix is a read-model field, not a card
refactor — and it has a working precedent in this repo.

---

## 1. What exists today

### 1.1 Layers and owners

```
wire  ──►  neutral read model  ──►  Turn / Task model  ──►  card builders  ──►  Feishu JSON
(src/opencode/*,   (src/backend/*)      (src/bridge/turn,      (src/feishu/card/*)
 src/codex/*)                            src/bridge/tasks)
```

- **Wire → neutral.** Each generation's decoder owns field names and
  tolerates `Other`/`Unknown` (ADR-0053; `src/backend/mod.rs:808`).
- **Neutral read model.** `SessionTranscript` → `TranscriptMessage` → `Part`
  (`Text`, `Reasoning`, `Tool`, `StepStart`, `StepFinish`, `Patch`, `File`,
  `Compaction`, `Other`) at `src/backend/transcript.rs:1417`; `ToolCall`
  at `:1574`; `ToolStatus` at `:1612`; `BackgroundLaunch` at `:1593`.
- **Turn/Task model.** The accumulator, timeline, card chain, background run
  overlay and settlement live in `src/bridge/turn/*` and `src/bridge/tasks/*`.
- **Card builders.** One module per card family (`session.rs`, `question.rs`,
  `picker.rs`, `terminal.rs`, `viewer.rs`, `ledger.rs`, `compaction.rs`,
  `tool_render.rs`, …), plus `shell.rs` as the one JSON-2.0 shell
  (header/body/state) every non-streaming card goes through
  (`card_shell`, `src/feishu/card/shell.rs:111`).

### 1.2 The card families

| Family | Builder | Joins a Card Chain? | Agent-specific? |
|---|---|---|---|
| Streaming **Turn card** | `bridge/turn/*` → `CardBuilder` | yes | no |
| **Session Card** (idle read, `/card`) | `session.rs:1665` | no | no |
| **Permission** card | `question.rs:122` | inline block or standalone | **partly** (name table) |
| **Question** / Location Form card | `question.rs:1409`, `:1506` | inline block or standalone | no |
| **Pickers** (`/model`, `/think`, `/agent`, `/autoaccept`) | `picker.rs` | no | **partly** (ADR-0092 already fixed `/model`) |
| `/switch`, `/dir`, `/sub`, browse | `session.rs:377`, `:792`, `:617`, `:965` | no | no |
| **File Viewer** | `viewer.rs` | no | no |
| **Terminal Card** (Host `/run`, `!cmd`) | `terminal.rs` | no | no (Host job, ADR-0086) |
| **Compaction** row | `compaction.rs` | rides the Turn card | no |
| **Ledger** (background tasks) | `ledger.rs` | rides the newest card | **partly** (`TaskKind`) |
| `/mcp`, `/help`, `/skill`, notify cards | `mcp.rs`, `help.rs`, `skill.rs`, `notify.rs` | no | no |

**Conclusion for the "is there a lot of reuse missing?" question:** the *card
families* are already well factored — one shell, one markdown sanitizer, one
budget/splitter, one picker grammar. The duplication is concentrated in **one
module (`tool_render.rs`) and three small semantic tables**, and it exists
because tool *meaning* was never lifted into the read model. That is a much
smaller surface than it looks.

---

## 2. The evidence, by the three examples you named

### 2.1 Sub-agents: four display paths for one concept

| Path | What the card shows | Live? | Row in ledger? | `/sub`? | Source |
|---|---|---|---|---|---|
| OpenCode V1 `task` | panel: `🔀 <description>` + `🤖 <subagent_type>`, plus a **liveness line** while running (`bash 28s` / `推理中 12s` / `等待你的授权`) | yes | yes (`TaskKind::Subagent`) | yes (`parentID`) | ADR-0054, `tool_render.rs:3433`, `:491` |
| OpenCode V2 `subagent` | same, `metadata.sessionID`, `<subagent …>` envelope | yes | yes | yes | `read_model` V2, `tool_render.rs:505` |
| Codex `collab_agent` | **one panel per call**, title from the collaboration verb (`派生子 Agent`, `发送输入`, `等待子 Agent`, …), body is the target rows / prompt / model | **no** | **no** | *(threads have `parent_thread_id` → `parent_id`, so `/sub` works)* `src/codex/threads.rs:79` | `src/codex/read_model.rs:1589`, `tool_render.rs:1029`, `:3109` |
| Codex `sub_agent_activity` | **a tool panel of its own** (`子 Agent`, target = `agentPath`) — a lifecycle *event* rendered as a *call* | n/a | **no** | n/a | `read_model.rs:1633`, `tool_render.rs:1034` |

So: OpenCode has a **run** (child session, liveness, ledger membership);
Codex has a **call** (`spawnAgent`/`wait`/`closeAgent`) and a **separate event
stream** (`subAgentActivity`). The card cannot express "these two are the same
thing", because the read model does not have the thing.

The unifying fact is not a tool kind — it is an **Agent Run**: a child
agent/thread with an identity, a label, a start time, a status, and an
activity. A tool call can *launch* one (`spawnAgent`, `task`), *address* one
(`sendInput`, `wait`, `closeAgent`), or *report on* one (`subAgentActivity`).
That distinction is exactly what the four paths above collapse into.

### 2.2 Shell / command execution: three ids and a look-alike

| Surface | Owner | Lifecycle | Renderer |
|---|---|---|---|
| `bash` (OpenCode V1) | agent tool call | transcript part | `format_tool_input` `:3321`, `command_input` `:3470` |
| `shell` (OpenCode V2) | agent tool call | transcript part + `BackgroundLaunch::Shell` | same arms; `metadata.exit/timeout` → failure icon `:437` |
| `codex/command_execution` | agent tool call | transcript part | same arms, shape-gated `:3326` |
| **Terminal Card** (`/run`, `!cmd`) | **Host** job (ADR-0086) | cola process group, no transcript | `terminal.rs:246` |

**Two of these are one concept** (an agent's execute call: command + cwd +
output + exit + optionally backgrounded) and **the fourth is a different
concept** (a Host job cola owns). They happen to look alike — command line in,
output out, exit code — which is why the Terminal Card *should* share a render
block with an execute panel, and *must not* share a lifecycle, a Chain, or a
record.

This is the answer to "一边交命令一边交 shell, 我觉得其实没必要": at the card
layer, yes — the `bash` / `shell` / `codex/command_execution` split is pure
vendor naming and should collapse to one **Execute** shape. At the model layer,
the Host Terminal and an agent shell are two owners of one presentation, and
merging them would be the mistake: one is a transcript fact, the other is a
durable Host job with restart/adoption semantics.

### 2.3 Plan / todo: the positive precedent

Already unified, and worth copying:

- Neutral fact: `SessionTranscript.todo_lists: HashMap<MessageId, TodoList>`
  (`src/backend/transcript.rs:94`), surfaced on the turn (`:499`).
- OpenCode supplies it from the `todowrite` tool; Codex supplies it from
  `turn/plan/updated` (`src/codex/read_model.rs:727`, `:981`).
- The card renders **one** plan section (`todo_panel_element`), with no
  vendor arms.

But the *tool-panel* side still name-checks `todowrite` in five places
(`tool_render.rs:228`, `:1902`, `bridge/turn/state.rs:316`, `:1881`, `:4248`)
— that is a leftover, not a design.

### 2.4 Model picker: the second positive precedent

ADR-0092's amendment moved the *surface description* into the **Agent Client**
(`Backend::model_surface` → `ModelSurface { current, scopes: Vec<ModelScope> }`,
`src/backend/types.rs:237`) and left the card as a pure renderer. The reasoning
is the template for this note:

> "The surface now belongs to the **Agent Client**, the card only renders it …
> a third **Agent Client** (ACP) only has to answer this description."
> — ADR-0092, amendment (2026-10-29)

---

## 3. The state model: what is neutral, what is mixed

Today's card states are a 14-variant enum (`CardState`,
`src/feishu/card/mod.rs:39`) that mixes **three different questions**:

1. **What is the run doing?** `Loading`, `Reasoning`, `Streaming`, `Done`,
   `Error`, `Stopped`, `Waiting`.
2. **Why did this card stop being live?** `Superseded`, `SwitchedAway`,
   `TakenOver`, `Restarted`, `Continued`, `Retried`.
3. **Was the message even received?** `Unreceived`.

`src/bridge/turn/disposition.rs` already separates the third group as
`Disposition` ("the one ending vocabulary"), and `CardState` is the *render*
union. That split is right; it just is not named as the model, and the card
header template (`header_title_and_template`, `shell.rs:495`) is the only place
the mapping exists.

**Proposed neutral levels** (each owned by a different layer, none agent-specific):

| Level | States | Owner | Drives |
|---|---|---|---|
| **Session** | `idle` / `working` / `waiting-user` / `waiting-background` / `unknown` | backend read (`session_status`, `snapshot`) | Session Card status chip, `/switch` row |
| **Turn** | `thinking` / `streaming` / `tool-running` / `settling` / `ended(disposition)` | bridge accumulator | header phase + timer, footer |
| **Card** | `live` / `waiting` / `terminal(disposition)` | card chain (ADR-0071/0072) | header colour, controls, recovery |
| **Request** | `permission(scope)` / `question(single · multi · custom)` / `location-form` | backend request DTOs | inline block or standalone card |
| **Run** | `running` / `ended(completed · failed · cancelled · lost · cleaned)` | bridge task overlay (ADR-0065/0080) | ledger row, completion entry |

Two concrete gaps this exposes:

- **Capability gaps are invisible.** `background_tasks_supported()` returns
  `false` for both Codex clients and for OpenCode V1
  (`src/codex/backend.rs:3191`, `src/opencode/strategy.rs:260` **[code]**), and
  the ledger simply never appears. That is fine — but it is expressed as a
  method per subsystem, not as one declaration the card can consult, so every
  new surface re-invents "does this client support X?".
- **The capability differs *within* one client.** Codex reports child-agent
  **activity** but no background runs. Today that means "no ledger" — and the
  activity items pile up as panels. A capability declaration with a
  `ChildAgentMode` would let the card render the liveness line without
  inventing a ledger.

### 3.1 Which surface for which event

This table already exists implicitly across ADR-0038, 0043, 0063, 0074, 0092,
0100; naming it is the "when does which card show" half of the model.

| Event | Surface | Owner decision |
|---|---|---|
| A prompt starts a Turn | streaming **Turn card**, as a reply to the message | one card per request (ADR-0074) |
| The Turn's content fills the card | **continuation card** in the same Card Chain | splitter (ADR-0043) |
| A message arrives mid-Turn | **Supplement card** (steer), never a second live card | ADR-0043 |
| The Turn ends with live background runs | same card yields → `Waiting` | ADR-0059 |
| A run's completion resumes the work | same card → `Resuming` | ADR-0066 |
| A Permission / Question on a live mapped Turn | **inline block** on the Turn card | ADR-0038 |
| … on a session with no live card | **standalone request card**, re-adopted after restart | ADR-0038, `surfaces.rs` |
| … from a child session | re-homed to the **parent's** card | pitfall `child-session-permissions` |
| … already resolved in another client | `build_resolved_elsewhere_card` | `notify.rs:31` |
| A read of a Session nobody renders (`/card`) | **Session Card**, one-shot, no controls | ADR-0100 |
| Another client posted a user message | **notify card** | `notify.rs:13` |
| A Host command (`/run`, `!cmd`) | **Terminal Card**, outside every Chain | ADR-0086 |
| A Host file (`/cat`) or dir (`/dir`) | **Viewer / browse card**, outside every Chain | ADR-0083, 0078 |
| A compaction | **row on the Turn card** at its server position | ADR-0082 |
| A restart finds a live card | `Restarted` stamp, **not** a new card | ADR-0063 |

The gaps are all of one kind: the table is not derived from a declaration, so
a client that cannot raise a Question or cannot background a run simply leaves
its row unreachable instead of the surface degrading explicitly.

---

## 4. The proposed model

### 4.1 One sentence

> **The read model says what happened in neutral terms; the Agent Client
> declares what it can produce; the card decides what to draw and owns every
> word of it.**

That is ADR-0053 (read model) + ADR-0092 (capability/surface) applied to the
tool surface.

### 4.2 The read model addition: tool *semantics*, not tool *payloads*

ADR-0053 deliberately kept tool payloads raw:

> "Per-built-in-tool typed payloads inside the read model. Rejected: tool
> payload interpretation is the Platform's deliberate specialization
> (ADR-0042)." — ADR-0053, "Considered Options"

That was correct while one adapter existed and the built-ins were one
vendor's. It is now the thing forcing every card arm: the *shape gates*
(`command_execution_shape`, `file_change_shape`, …) exist because the card
cannot trust a name, and there is no neutral type to trust instead.

**Proposal: four additive fields, all adapter-supplied, with `Other` staying
opaque.**

```rust
// src/backend/transcript.rs
pub struct ToolCall {
    pub identity: ToolIdentity,     // vendor name + call id — unchanged
    pub kind: ToolKind,             // NEW: coarse, adapter-declared
    pub title: Option<String>,      // NEW: the adapter's human title (ACP `title`)
    pub locations: Vec<ToolLocation>, // NEW: files this call touches (ACP `locations`)
    pub status: ToolStatus,
    pub started_at: Option<i64>,
    pub input: Option<Value>,       // unchanged, stays raw
    pub metadata: Option<Value>,    // unchanged
    pub output: ToolOutput,
    pub detail: ToolDetail,         // NEW: typed only for the shared shapes
}

/// ACP's ToolKind, which is exactly the coarse taxonomy the card needs.
pub enum ToolKind {
    Read, Edit, Delete, Move, Search, Execute, Think, Fetch, SwitchMode, Other,
}

pub enum ToolDetail {
    None,
    Command { command, cwd, exit, timeout },
    Program { source, language, calls }, // Code Mode: a program that calls tools
    FileEdits(Vec<FileEdit>),       // path, FileAction, rename_to, diff
    Search { query: Option<String>, matches: Option<u64>, scope: SearchScope },
    Read { path: String, preview: Option<String>, directory: bool },
    Delegation { runs: Vec<AgentRunRef>, effect: DelegationEffect },
    Opaque,                         // everything else — ADR-0042's rule stands
}
```

`FileAction`, `SearchScope`, `DelegationEffect` and the `AgentRunRef`/`ToolCallRef`
shapes are ADR-0105's; §4.3/§4.4 below follow it.

Rules that keep ADR-0042 intact:

1. **`kind` is declared by the adapter, never sniffed from a third-party
   payload.** A tool the adapter does not recognise is `ToolKind::Other` and
   `ToolDetail::Opaque`, exactly today's raw path.
2. **A vendor name remains.** `identity.name` still says `codex/file_change`
   or `bash`; it becomes diagnostic and the fallback title, not the dispatch
   key.
3. **`detail` is filled only where the semantics are shared.** ACP's
   `diff`/`terminal`/`content` content blocks and OpenCode's `edit`/`patch`
   metadata both produce `FileEdits`/`Command`; a `websearch` produces
   `Search`; nothing else grows a variant until a second adapter needs it.
4. **Presentation copy stays in the card.** The adapter supplies *facts and
   titles*; 「文件修改」/「命令」/「子 Agent」 and the icon remain the card's.

### 4.3 The Agent Run: the sub-agent fact

```rust
pub struct AgentRun {
    pub run_id: String,             // child session/thread id when it has one
    pub label: String,              // description / prompt / agent name
    pub agent: Option<String>,      // subagent_type / agentPath / model
    pub started_at: Option<i64>,
    pub status: RunStatus,          // Running | Completed | Failed | Cancelled | Unknown
    pub activity: Option<RunActivity>, // newest live tool or phase (ADR-0054's fact)
    pub launched_by: Option<ToolCallRef>,
    pub addressed_by: Vec<ToolCallRef>, // sendInput/wait/closeAgent
    pub reported_by: Vec<ToolCallRef>,  // subAgentActivity
}
```

Read-model presence, not card presence:

- `SessionTranscript.runs` (derived by each adapter) plus the existing
  background overlay (`src/bridge/tasks/*`) become one run view.
- OpenCode: `task`/`subagent` calls with `metadata.sessionID` → one run.
- Codex: `collabAgentToolCall` → a run; `subAgentActivity` → an **update to
  that run**, not a panel; `wait`/`sendInput` → `addressed_by`.
- The card uses the run for: the launching panel's liveness line, the ledger
  row, the completion entry, and `/sub`'s identity. **One fact, four surfaces,
  no vendor arms.**

### 4.4 Capabilities: one declaration per Agent Client

```rust
pub struct AgentCapabilities {
    pub background_runs: bool,        // a call can outlive its Turn
    pub child_agents: ChildAgentMode, // None | ChildSessions | ActivityReports
    pub plan: PlanMode,               // None | TodoTool | TurnPlan
    pub compaction: bool,
    pub usage: UsageMode,             // None | PerTurn | Cumulative
    pub permissions: PermissionMode,  // option kinds the client can express
    pub questions: QuestionMode,
    pub modes: bool,                  // agent/mode axis
    pub diffs: bool,                  // session/turn diff reads
}
```

Surfaces gate on it instead of on per-subsystem methods:

| Surface | Requires | When absent |
|---|---|---|
| Ledger section | `reports_live_runs()` (`background_runs \|\| child_agents != None`) | no section |
| Panel liveness line | `child_agents != None` + a run with activity | plain panel |
| Plan section | `plan != None` | no section |
| Compaction row | `compaction` | part renders as `Other`/omitted |
| Footer usage | `usage != None` | no footer figure |
| Question surface | `questions != None` | no question card/block |
| 「始终允许」 | `permissions` advertises a session scope | one-time buttons only |
| Agent picker | `modes` | `/agent` says unsupported |

### 4.5 Rendering: one renderer per shape, no per-name arms

Do **not** start by inventing a general block tree. The staged version:

1. **One renderer per `ToolDetail` shape**, not per name. `Execute` has one
   builder reused by the tool panel *and* the Terminal Card's output section.
   `FileEdits` has one diff builder. `Delegation` has one run block.
2. **Key the panel title on `kind` + `detail`**, with `title` as the
   adapter's refinement: `Execute` → `shell`, `Edit` → `edit`, `Read` →
   `read`, `Delegation` → `subagent` (the token table is ADR-0107's).
3. **A `ToolDetail::Opaque` path identical to today's** — raw input as a
   key-value list, raw output with the markdown-safety fence.
4. Only after those three exist, decide whether the card needs a block model.
   The likely answer is a small one: the *tail* already is a list of typed
   sections (todo, ledger, live tools, compaction, footer) in `CardBuilder`;
   naming that list is the block model, and it costs one enum.

The card-size estimate mirror (`panel_estimate`, `bridge/turn/state.rs:591`)
is the early warning for this: it is a second implementation of the renderer's
byte cost, kept in lockstep by hand. A per-shape renderer that returns its own
estimate keeps the pair together; the current per-name arithmetic cannot.

---

## 5. ACP as a reference (and where it does not reach)

ACP v1's `session/update` variants map onto Cola's neutral model almost one to
one **[acp]**:

| ACP | Cola today | Gap |
|---|---|---|
| `agent_message_chunk` / `user_message_chunk` | `Part::Text` (+ `messageId`) | — |
| `agent_thought_chunk` | `Part::Reasoning` | — |
| `tool_call` / `tool_call_update` | `Part::Tool(ToolCall)` | **`kind`, `title`, `locations`** |
| `plan` (entries: content, priority, status) | `TodoList` | already neutral; priority unused |
| `usage_update` (used, size, cost) | `TokenUsage` | cost not modelled |
| `compaction_update` / `compaction_summary_chunk` | `Compaction` | already neutral (ADR-0082) |
| `current_mode_update` | agent/mode picker | mode not a neutral session fact |
| `available_commands_update` | `/help` is static | no neutral command catalog |
| `session_info_update` | session title read | — |
| `session/request_permission` + `PermissionOptionKind` | `PermissionRequest.always` + `always_supported` | cola's model is **weaker**: no per-option kind |
| tool content: `content` / `diff` / `terminal` | `ToolOutput`, `EditDiff`, Host Terminal | terminal is Host-only today |

What ACP does **not** cover, and cola must keep its own model for:

- **Session identity and the peer attachment** — ACP is stdio with the agent as
  a subprocess, one writer at a time (ADR-0090). Cola's Session/peer model is
  richer and stays.
- **Background runs.** ACP has no "a tool call outlived its turn"; a long
  process is a tool call plus `terminal/*`. Cola's background-task ledger
  (ADR-0060) has no ACP equivalent.
- **Child-agent sessions.** ACP v1 has no child session concept; a subagent is
  just a tool call from the client's view. Cola's `AgentRun` is richer — which
  is why it must be a cola fact, with ACP mapping `spawn` to a run and
  `activity` to nothing.

So ACP is the right **vocabulary** (kind, status, locations, content blocks,
plan entries, permission option kinds) and not a sufficient **architecture**
(session ownership, background runs, agent trees). Adopting its names costs
little and buys the third adapter a translation table instead of a card
refactor.

---

## 6. Migration plan (each step independently shippable)

Ordered by (evidence strength × risk⁻¹). Every step keeps a single invariant
and its tests.

| Phase | Change | Invariant | Tests |
|---|---|---|---|
| **P0** | This note → GLOSSARY terms + one ADR for the model | docs only | — |
| **P1** | `ToolKind`, `title`, `locations` on `ToolCall`; adapters fill; card ignores them | every existing card byte-identical | adapter conformance + snapshot |
| **P2** | `ToolDetail::Command`; one Execute renderer; Terminal Card reuses its output block; delete `bash`/`shell`/`codex/command_execution` arms and `command_input`'s vendor split | the three ids render the same as today | `shell.rs` header tests + `terminal.rs` |
| **P3** | `ToolDetail::FileEdits`; collapse `edit`/`patch`/`apply_patch`/`codex/file_change` | same diffs, same open-button rule | `tool_render` diff tests |
| **P4** | `AgentRun` + `ToolDetail::Delegation`; Codex activity items fold into runs; ledger + liveness read runs | OpenCode output unchanged; Codex gains liveness/ledger | `codex/cards.rs`, `bridge/tests/ledger.rs` |
| **P5** | `AgentCapabilities`; surfaces gate on it | every absence is explicit and tested | per-client card tests |
| **P6** *(optional)* | Name the tail-section list as a block model; `/model`-style surfaces stay as-is | — | — |

Deliberately **not** in scope:

- Merging the Terminal Card's lifecycle with an agent shell (different owners).
- Moving card copy into adapters — the card owns the words.
- Sniffing third-party payloads. `Other` stays opaque (ADR-0042).
- Per-chat multi-client routing (ADR-0090 defers it; it needs its own ADR).

---

## 7. Decisions this note asks for

Each is phrased as an ADR candidate.

- **D1 — Tool semantics in the read model.** Amend ADR-0042/0053: `ToolKind`
  plus typed `ToolDetail` for shared shapes, `Opaque` for everything else, with
  `kind` adapter-declared and never inferred. *(One-way door: it fixes the
  vocabulary every later adapter answers.)*
- **D2 — Agent Run as a neutral fact.** One run model for OpenCode
  `task`/`subagent` and Codex `collabAgent`/`subAgentActivity`, with
  launch/address/report relations. *(One-way door for the task surface.)*
- **D3 — Capabilities as one declaration.** Backends answer
  `AgentCapabilities`; surfaces gate on it. *(Two-way.)*
- **D4 — Presentation ownership.** The adapter supplies facts and titles; the
  card supplies copy, icons, budgets and layout. Refines ADR-0092's line for
  tool panels. *(Two-way.)*
- **D5 — Terminal Card / execute panel share a render block, never a
  lifecycle.** *(Two-way.)*

## 8. Open questions

1. **`ToolKind` spelling.** *(Settled by ADR-0105/0107.)* `ToolKind` is
   ACP's ten, because that is the coarsest category every adapter can fill; the
   card's display word is a separate **token** refined through `ToolDetail`
   (`glob`/`grep` → `Search`, `write`/`edit` → `Edit`), so the two axes cannot
   collapse into one enum.
2. **Codex `sendInput`/`wait`/`closeAgent`.** *(Settled by ADR-0106.)* Receipts
   on the run, with the raw call kept in the transcript.
3. **Codex ledger.** Codex has child threads and no background launches. Do we
   want a ledger for *live child agents* even when no "background" concept
   exists? *Recommendation: yes, but call it what it is — the ledger is "live
   runs", not "background tasks".*
4. **Terminal Card for Codex.** `/run` is Host-side and client-independent
   today. Keep it that way. *(No action, recorded so nobody "unifies" it.)*
5. **Cost.** ACP's `usage_update` carries a currency amount; `TokenUsage` does
   not. Worth a field only when a client reports it.

## 9. Risks

- **Losing vendor nuance.** A coarse kind can flatten a distinction users rely
  on (a `patch` reads differently from an `edit`). Mitigation: `title` from the
  adapter, and `ToolDetail` variants preserve the shape, not the name.
- **Rewriting a working renderer.** P2–P4 touch the most tested module in the
  repo. Mitigation: byte-identical snapshots first (P1 asserts no change), then
  one shape per PR.
- **Treating ACP as the target.** ADR-0090 is explicit that ACP is a later,
  cola-owned *Owned Runtime* mode. The model above is agent-neutral; ACP is a
  consumer, not the design.
- **The size-estimate mirror.** Any new renderer must return its own estimate;
  the current hand-maintained arithmetic (`panel_estimate`,
  `bridge/turn/state.rs:591`) is the most likely place a refactor silently
  breaks a card budget.
