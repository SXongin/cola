# Tool Panel rendering: tailor built-ins, treat third-party payloads as opaque

> **Amended by ADR-0105**: a Tool Panel is now chosen by the call's **Tool
> Kind** and **Tool Detail**, declared by the adapter, rather than by the
> vendor's tool id. The id survives as identity and the opaque fallback; this
> ADR's rule that a third-party payload is never interpreted still stands.

A **Tool Panel** renders each tool call on the card, and today only `read`,
`edit` and `todowrite` have tailored renderers: an audit of 100 sessions /
5,685 tool parts (issue #202) found `task`/`skill` XML wrappers leaking onto the
card, `apply_patch` diffs dropped, and WebSearch's JSON rendered as a wall. The
tempting fix is one renderer per tool — including the MCP servers and injected
plugins that show up in the Shared Store. This ADR fixes the **boundary**
instead: cola tailors Tool Panel rendering only for **OpenCode built-in tools**
(and tools cola itself injects); every other tool's input/output is opaque and
rendered raw. A built-in's id (`bash`, `read`, `task`, …) is a stable registry
contract cola can follow; a third-party tool's payload shape is another
project's implementation detail with no compatibility promise, and matching on
it would couple cola's card code to servers it does not own.

## Context

- **Built-ins are the stable set.** OpenCode's registry defines them by id
  (`src/tool/*.ts`); observed in the Shared Store: `bash`, `read`, `edit`,
  `write`, `glob`, `grep`, `webfetch`, `task`, `todowrite`, `skill`,
  `websearch`, `question`, `invalid`, plus the source-confirmed `apply_patch`
  (gpt-* models) and the flag-gated `lsp`/`plan_exit`/`execute`.
- **Third-party shapes are arbitrary.** `codegraph_*` and `context7_*` (MCP)
  return markdown; OpenChamber's `openchamber*` plugin tools return
  `JSON.stringify({schemaVersion, ok, action, data|error})` on one line. Any new
  MCP server or plugin can appear with any shape at any time.
- **Rendering is name-keyed today.** `format_tool_input` / `format_tool_output`
  match on the tool name (`src/feishu/card/tool_render.rs`); unmatched tools
  already fall through: output verbatim (plus the markdown-safety fence from
  `needs_code_block`), input through the generic key-value fallback.
- **`todowrite` is not a Tool Panel.** It is the live tail status section (**Todo
  Panel**), replaced by each call; its special-casing is not the pattern for
  other tools.

## Decision

- **Tailor by exact tool id** for OpenCode built-ins — the set above as they are
  observed — and for tools cola itself registers (none today). Each gets a
  renderer only where the shape is known and worth showing better.
- **Everything else is opaque.** For an unrecognized tool, output passes through
  unchanged except `needs_code_block` fencing and `state.error` extraction, and
  input keeps the generic key-value fallback (scalars one per line; nested
  values compacted and clipped).
- **No shape sniffing.** A generic "any valid JSON → pretty-print" fallback is
  explicitly not adopted: it would still treat third-party payloads as a
  presentation contract, and it would change under us when a server changes its
  return format.

## Why

- A built-in id is a contract: when OpenCode changes `read`'s XML wrapper, cola
  has one renderer to update and a reason to notice. A third-party id carries no
  such promise, so every adaptation is a dependency on someone else's release
  schedule.
- The audit shows the real pain is built-in shapes (raw XML, dropped diffs,
  JSON walls on built-ins) — none of it third-party.
- Fixing raw third-party output at the source (the server returning readable
  text) helps every client, not just cola.

## Alternatives considered

- **Generic JSON pretty-printer for all unrecognized outputs**: rejected — it
  applies to whatever payloads happen to exist (today's OpenChamber envelope,
  tomorrow's unknown), which is exactly the shape dependency this decision
  refuses. A built-in that returns JSON gets its own renderer (`websearch`
  today); any other built-in — `lsp` included — stays raw until someone adds
  one.
- **Per-tool renderers for the MCP servers in use** (`codegraph_*`,
  `context7_*`): rejected — they return markdown already, and pinning cola to
  their tool names would break the moment a Host configures different servers.
- **Hide third-party outputs entirely**: rejected — the operator may need to see
  what a tool returned; raw beats invisible.

## Consequences

- A third-party tool's raw JSON wall stays on the card. If that is ugly, the fix
  belongs in that tool (return readable text), not in cola.
- A new built-in renders raw until someone adds an arm — accepted; the set is
  small, and the fallback is safe.
- The name match is against the part's `tool` field: a renamed or replaced
  built-in silently falls back to raw, never to a wrong renderer.
- **A third-party tool that claims a built-in id can hit a built-in arm.** The
  part payload carries no provenance, so the id is all cola has; each renderer
  additionally gates on shape (a `<path>` block, a `<task …>` envelope, a
  `results` array, a parseable diff), so a same-named tool with a different
  shape falls through to raw. A same-named tool with the exact same shape gets
  the built-in rendering — accepted: the rendering is then correct anyway.
- This bounds the renderer to OpenCode's registry; a future "cola tool" set must
  be named explicitly when cola starts injecting tools.

## Domain note

The concepts enter the glossary as **Tool Panel** (工具面板) and **Built-in
Tool**.

## Update (2026-09-19)

This ADR is about *how* a Tool Panel renders (tailored vs. opaque) and assumed
it is a timeline row. ADR-0045 moves a panel whose tool is still running out of
the timeline and into the card tail, so a Card Chain split can never strand it;
the rendering boundary here is unchanged.

## Amendment (2026-10-01): the OpenCode 2 built-in set

The observed built-in set recorded above is V1's. The current generation
(OpenCode 2.0.x, the `/api` surface) ships a different set, and the Tool Panel
arms now match it by the same rule — exact built-in id, shape-gated:

- `shell` replaces V1's `bash`. The id arm is unchanged; V2 additionally
  records the run's outcome in `metadata.exit`/`metadata.timeout`, which the
  panel reads for its failure icon (a settled call can report a failure while
  its protocol status reads `completed`).
- `subagent` replaces V1's `task`, and records the child session as
  `metadata.sessionID` (V1: `sessionId`). Both ids keep ADR-0054's child
  liveness; the output envelope is `<subagent sessionID state>` rather than
  `<task …>` with `<summary>`/`<task_result>` inner tags.
- `patch` replaces V1's `apply_patch`; its diff is the same
  `metadata.files[].patch` shape `edit` records, and its input is the same
  `patchText` framing.
- `todowrite` does not exist in OpenCode 2 (the V1→V2 migration removes it);
  the Todo Panel is a V1-only surface until V1 retirement.
- `execute` is new: the Code Mode entry. Its `metadata.toolCalls` records the
  nested calls the program made (name, status, input) and updates while it
  runs; the panel renders them as display-only status rows. cola never treats
  another tool's payload as a protocol trigger.
- Namespaced tools (MCP servers, the `opencode_*` session/model tools) are
  flattened to `<namespace>_<tool>` ids and stay opaque, per this ADR's
  third-party rule.

Nothing above changes the boundary: only ids OpenCode ships get arms, each arm
stays shape-gated, and every other payload stays raw.

## Amendment (2026-10-10): a `file` content block is protocol, not tool payload

ADR-0076 renders the `file` variant of `Tool.Content` (OpenCode's protocol
content union, `text` | `file`) as a File Content — for `read`, an MCP tool, or
any other tool alike. This does not breach the boundary above: that boundary
keeps a tool's `input`/`output` *payload* opaque because its shape is the tool's
own contract, whereas a `file` content block is a protocol-level content variant
the server defines for every tool, decoded neutrally before any tool id is
consulted. Rendering it is reading the protocol, not sniffing a third-party
shape, and the renderer never inspects the tool id to do so.

## Amendment (2026-10-27): the Codex adapter's item kinds

The Codex app-server adapter (ADR-0090) projects its `ThreadItem` kinds onto the
neutral transcript as `codex/command_execution`, `codex/file_change`,
`codex/web_search` and `codex/image`. Those ids are **cola's own vocabulary for
its own adapter**, not a third party's payload names: cola writes them in
`src/codex/read_model.rs` for every Codex client, and each wire shape is the
adapter's typed decode of the pinned app-server schema. The `codex/` namespace
is the same device an MCP call uses (`server/tool`): a third-party tool with a
raw item-kind name (`image`, `command_execution`, …) is a different id and stays
opaque.

They therefore join the tailored set under this ADR's rule — exact id,
shape-gated:

- `codex/command_execution` — `{command, cwd}`, the adapter's `commandExecution`.
- `codex/file_change` — `{changes: [{path, kind}]}`, the adapter's `fileChange`.
- `codex/web_search` — `{query, action}`, the adapter's `webSearch`.
- `codex/image` — `{path}`, the adapter's `imageView`.

Each arm additionally requires the **whole** input shape the adapter writes (not
one key), so even a tool that squats the namespaced id with a partial payload
falls through to raw. `fileChange`'s shape includes the empty `changes` list the
adapter emits at `item/started` and fills through `patchUpdated`. The boundary is otherwise unchanged: a payload whose shape
cola's own adapter did not write stays raw, and no third-party id is renamed.

## Amendment (2026-11-01): the Codex arms' output side

The 2026-10-27 amendment named these ids' input shapes. #902 adds the output
side for the two whose text blocks carry nothing the reader can use, under the
same boundary and the same id-plus-shape gate:

- `codex/file_change` renders **the adapter's recorded change**, not its text:
  `metadata.changes[].diff` is the bare hunk body per file, so
  `codex_file_change_diff` takes the path from the change entry (one change) and
  joins several behind `Index:` lines, then reuses the existing `EditDiff`
  parser, diff budget, clip note and count header. The file count is the
  input's `changes` count, so an entry that records no diff is still counted
  (and named in the body whenever any entry yields a hunk). A
  single change names its file and carries the 「📂 打开」 button — a rename
  names and opens its destination everywhere (Input line, folded title,
  button), a delete still names the removed file but carries no button;
  several name the input's file count and, except on a failed call whose
  change is unknown, the parsed hunk totals in the folded title (zero totals
  when no hunk parses) and carry no button.
- `codex/web_search` reads the adapter's `metadata.results`, which the pinned
  app-server schema calls **opaque JSON** ("new result fields and result types
  can pass through without a Codex release"). Only one subset is a presentation
  contract: an array whose every entry carries a non-empty `url` (an optional
  `title` names it) renders as `🔎 N 条结果` and the shared numbered list, with
  excerpts dropped. Every other value is shown inside a `json` fence — a JSON
  string's own text, otherwise its pretty-printed JSON — so an unrecognized
  shape is never dropped and never guessed into a result list. A search with no
  results yet — or a blank message — renders no Output section rather than an
  empty one beside the query. This is not the generic "any valid JSON → pretty-print" fallback
  rejected above: it applies to exactly one adapter-owned id and that item's own
  payload, and no other tool's rendering changes.

`codex/image` stays unchanged, and a third-party tool that squats either id with
a partial input shape still falls through to the opaque path.

## Amendment (2026-11-04): the Codex collaboration items

Issue #962 adds the two V2 collaboration item kinds to the adapter's vocabulary,
under the same rule — cola's own `codex/` id, shape-gated on the **whole**
payload `src/codex/read_model.rs` writes:

- `codex/collab_agent` — the adapter's `collabAgentToolCall`:
  `{tool, status, senderThreadId, receiverThreadIds[], prompt, model,
  reasoningEffort, agents: [{threadId, label, status, message}]}`. The optional
  scalars are always present (written as `null` when the value is unknown),
  the same device `webSearch`'s `action` uses. `agents` is written in the
  wire's receiver order, then any state the receiver list did not name, sorted
  by thread id — deterministic, so an unchanged call compares equal across
  polls instead of repainting the card.
- `codex/sub_agent_activity` — the adapter's `subAgentActivity`:
  `{kind, agentThreadId, agentPath, model, reasoningEffort}`.

Each arm renders one Tool Panel, one item per call (the wire gives every call
its own id and lifecycle, and a `wait`/`close` targets threads that several
`spawn` calls produced — folding them into one card would invent an N:M join
and retro-patch an older card). The collaboration call's folded title names the
call (`派生子 Agent`, `等待子 Agent`, …) and its first target thread; its
Input line carries the prompt and the model/reasoning effort, and the target
rows ride the **Output body** — one row per target's own state — because a
`wait` may name many targets, more than the Input line's shared
400-character budget could hold. That body is assembled from the adapter's
payload like `codex/web_search`'s results, so it renders under the output
budget and the card-size estimate charges it exactly. A call with more targets
than that budget holds ends on an explicit `… 另有 N 个目标` line — every shown
row is whole, and the folded title still counts them all. A call whose item
carries no prompt, model or target row at all renders its own state plus
`未上报目标状态` rather than an empty body: V2's `wait` always writes empty
`receiverThreadIds`/`agentsStates`, and its result text is a tool-output
response item the item stream does not carry, so the card states what it knows
instead of reading as a rendering gap. An unknown `tool` or
`status` keeps its raw string on the card rather than falling back to `Other` —
the wire enums are upstream's to extend (an unknown status rides the Input line,
since the icon alone could not name it). The activity is a one-line panel
naming the kind, the child thread and its `agentPath` — clipped to the folded
header's own short-line budget by keeping the path's tail, the same rule a long
file name takes.

The target label prefers the child's own `agentPath`, which the same child
thread's `subAgentActivity` reports in the same read; without one it falls back
to the thread id's first eight characters. The label is folded to one line and
clipped to the folded header's budget, like a file name. No extra read is made
for a nickname (`thread.agentNickname` is a `thread/read` fact, and a render
pass performs no reads), so the card only shows a name the item stream already
carries. Sub-agent threads
stay out of the session list (`thread/list` filters the `subAgent*` source kinds,
pinned by a regression test in `src/codex/threads.rs`): this amendment changes
rendering only, creates no chat mapping, and opens no write path into a child
thread.

The boundary is otherwise unchanged: a third-party tool that squats either id
with a partial payload stays raw, and no Codex-only vocabulary is renamed.

## Amendment (2026-11-04): Codex `update_plan` feeds the Todo Panel

The 2026-10-01 amendment called the Todo Panel a V1-only OpenCode surface
because OpenCode 2 removed `todowrite`. Codex now supplies the same card-tail
status section from its `update_plan` tool's `turn/plan/updated` notification
(#961). That notification is not a `ThreadItem` and does not become a Tool
Panel.

The Codex adapter decodes its shape-gated
`{threadId, turnId, explanation?, plan: [{step, status}]}` payload into the
neutral whole-list `TodoList`; the Bridge and Platform consume that list through
`set_todo_list`. No synthetic `ToolCall`, `todowrite` id, or new timeline id is
created. OpenCode's real `todowrite` call continues to use the same neutral seam
when its input parses as a list; a malformed call keeps its existing raw Tool
Panel fallback.

The Codex panel is display-only: it never drives the header's running-tool
view, and it keeps the established `📋 todowrite` title so the two sources read
as one card-tail section. Because the notification is not part of thread
history, a freshly built projection shows no plan until the next
`turn/plan/updated` arrives; a resync keeps the plan it already holds.

The rest of this ADR is unchanged: Tool Panel rendering remains limited to the
exact built-in or adapter-owned ids named above, together with their recorded
shape gates.
