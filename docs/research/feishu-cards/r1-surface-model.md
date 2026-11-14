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
> copy stays Chinese until an i18n catalog exists. The state/model/migration
> sections were removed: they now live in the ADRs and spec #1057, and keeping a
> second copy is what drifted during review. §7/§8 remain the pre-decision
> record.

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
appear **78 times** in it (68 quoted match positions), tests included
**[measured]**. Every one of those
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
| OpenCode V1 `task` | panel: `🔀 <description>` + `🤖 <subagent_type>`, plus a **liveness line** while running (today `bash 28s` / `推理中 12s` / `等待你的授权`; the target wording is `shell 28s` / `reasoning 12s`, ADR-0107) | yes | no (V1 has no background fact) | yes (`parentID`) | ADR-0054, `tool_render.rs:491`, `:554` |
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
| `codex/command_execution` | agent tool call | transcript part | same arms, shape-gated `:3322` |
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
(`tool_render.rs:228`, `:1902`, `:2600`, `bridge/turn/state.rs:316`, `:1881`, `:4248`)
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

## 3. State model

Superseded: the state levels, the capability axes and the event→surface matrix
now live in **ADR-0105/0106** and **spec #1057** (`Surface gates and selection`).
This note keeps only the evidence for why they were needed.

## 4. The proposed model

Superseded: the shapes are **spec #1057's `Canonical shapes`**, and the rules are
implied by **ADR-0105/0106/0107**. This note does not restate them — that
duplication is what drifted three times during review.

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

## 6. Migration

Superseded: the phases and their invariants are **spec #1057's ticket cut**
(P1 fields → P2 Execute → P3 FileEdits → P4 AgentRun → P5 capabilities →
vocabulary). The note does not keep a second copy.

## 7. Decisions this note asks for

Each is phrased as an ADR candidate.

- **D1 — Tool semantics in the read model.** Amend ADR-0042/0053: `ToolKind`
  plus typed `ToolDetail` for shared shapes, `Opaque` for everything else, with
  `kind` adapter-declared and never inferred. *(One-way door: it fixes the
  vocabulary every later adapter answers.)*
- **D2 — Agent Run as a neutral fact.** One run model for OpenCode
  `task`/`subagent` and Codex `collabAgentToolCall`/`subAgentActivity`, with
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
