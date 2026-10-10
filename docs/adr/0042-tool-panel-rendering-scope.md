# Tool Panel rendering: tailor built-ins, treat third-party payloads as opaque

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
