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
