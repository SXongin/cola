# The card speaks in tokens and copy

> **Evidence**: `docs/research/feishu-cards/r1-surface-model.md` §2.1, §2.2.
> **Relates to**: ADR-0105 (the semantic fields these words name) and ADR-0106
> (which surfaces appear at all).

## Context

The card's vocabulary is mixed, and the mix is not a style choice — it is the
symptom of a missing classification:

- **Tool names follow the adapter, not the concept.** OpenCode's ids pass through
  as English (`bash`, `read`, `grep`); Codex's item kinds are translated in the
  card (`命令`, `文件修改`, `网页搜索`, `子 Agent`). One card header can read
  `⏳ bash` beside `等待你的授权`; another reads `⏳ 命令` for the same concept.
- **Two phase words are synonyms.** `思考中` (a request with no streamed part
  yet) and `推理中` (a reasoning stream) are different states whose Chinese names
  a reader cannot tell apart. The English pair has the same problem.
- **A German-style structural label sits in Chinese prose.** Tool panels carry
  `**Input**` / `**Output**` between Chinese copy.
- **The same word is written in several places.** `思考中`/`推理中`/`回复中` live
  in both `shell.rs` and `tool_render.rs` (`ChildActivity`); `shell`/`subagent`
  are written in the ledger, the permission description and the tool panel. A
  rename today misses a copy-paste.
- **i18n is explicitly deferred.** cola serves Chinese users for now. The
  classification must therefore be made without building locales, and must not
  need revisiting when locales arrive.

The distinction that actually matters is not Chinese versus English. It is
**domain vocabulary versus interface text**.

## Decision

**1. Every user-visible string is a Token or Copy. The test is one question:
would a translator touch it?**

- **No → Token.** Domain vocabulary: tool nouns, live phase words, protocol
  names, identifiers. Tokens are invariant: they stay English when a locale
  arrives, and they never enter a translation catalog.
- **Yes → Copy.** Interface text: card titles, sentences, buttons, empty states,
  hints, units, section labels. Copy is authored in Chinese today and becomes a
  `zh-CN` catalog when i18n lands.

**2. The tool tokens are one per concept, and a token is never a vendor id.**

| Token | Absorbs | Note |
|---|---|---|
| `shell` | `bash`, `shell`, `command_execution` | command + cwd + exit + background |
| `read` | `read`, Codex `imageView` (`codex/image`) | a file or image viewed |
| `write` | `write` | a new file (every file in the edits is an add) |
| `edit` | `edit`, `patch`, `apply_patch`, Codex `file_change` | a diff |
| `glob` | `glob` | file-name search |
| `grep` | `grep` | content search |
| `fetch` | `webfetch` | one URL fetched |
| `websearch` | `websearch`, Codex `web_search` | a web search |
| `subagent` | `task`, `subagent`, Codex `collab_agent` (launch) | a delegated run |
| `script` | Code Mode's `execute` (OpenCode V2; `codemode` / `exec` / `execute_tools` / `code_execution` in other harnesses) | a program that calls tools |
| `plan` | `todowrite` | the plan surface's tool form |
| `skill` | `skill` | a loaded skill |
| `ask` | `question` | a question raised as a tool |

Anything not in this table is **not a token**: it keeps its own vendor name
(`codegraph_explore`, `mcp_server/tool`), exactly ADR-0042's opaque rule. The
table is closed: a new token arrives with the `ToolDetail` variant that carries
it (ADR-0105), and an unknown id keeps its name. The three name-only tokens
(`plan`, `skill`, `ask`) are the recorded exception: they have no detail because
OpenCode is their only producer, and the closure rule admits them by the closed
name table rather than by a variant.

`script` names the one **pattern** rather than a built-in id. Code Mode is
general — Cloudflare named it (2025-09), Anthropic calls it programmatic tool
calling (2025-11), OpenAI's Responses API and Pi both ship it — and the
ecosystem spells the **tool** four ways (`codemode`, `exec`, `execute_tools`,
`code_execution`). Pi has both a `codemode` tool and a `/codemode` mode: the
tool is what this token absorbs, while the mode is a session setting and is not
a token. The token names the **executed unit** (a program that calls tools),
not the artifact (`code`, which is what `edit` shows). It parallels `shell` the
way `subagent` parallels a single call: one command versus one program. Its
`ToolDetail::Program` carries the source and the nested calls.

**Token derivation is a table, not a guess.** `tool_token(identity, kind,
detail)` reads the detail first, then the kind, and falls back to the vendor
name. The name is consulted in exactly one place — the `Other` + `None` arm,
where a closed three-entry table maps `todowrite`/`skill`/`question` — and is
otherwise only the opaque fallback. It never chooses a renderer (ADR-0105 §5);
it names the three tools that have no shared shape because OpenCode is their
only producer:

| `(kind, detail)` | Token |
|---|---|
| `Execute` + `Command` | `shell` |
| `Execute` + `Program` | `script` |
| `Read` + `Read` | `read` |
| `Edit` + `FileEdits` with a **non-empty** list, every edit `Add` | `write` |
| `Edit` + `FileEdits` otherwise (empty, mixed, or any non-add) | `edit` |
| `Search` + `scope: FileName` | `glob` |
| `Search` + `scope: Content` | `grep` |
| `Search` + `scope: Web` | `websearch` |
| `Fetch` + `None` | `fetch` |
| `Other` + `Delegation { effect: Launch, .. }` | `subagent` |
| `Other` + `None`, vendor `todowrite` | `plan` |
| `Other` + `None`, vendor `skill` | `skill` |
| `Other` + `None`, vendor `question` | `ask` |
| anything else | the vendor name (opaque) |

Three consequences worth stating because they are the ambiguous cases: a mixed
`FileEdits` list reads `edit` (the umbrella; `write` means "a non-empty list
whose every entry adds"), an **empty** `FileEdits` also reads `edit` (Codex's
`fileChange` starts empty and gains entries, so `write` would flip back), and a
`Delegation` whose effect is `Address` or `Report` has **no** token at all — it
renders as a receipt on the run, not as a panel. `AgentRun`'s types, `FileAction`
and `SearchScope` live in spec #1057.

**3. The live phase slot speaks three tokens.**

| Token | State | Replaces |
|---|---|---|
| `waiting` | the request is sent; nothing has streamed yet | `⏳ 思考中` |
| `reasoning` | a reasoning part is streaming | `💭 推理中` |
| `responding` | a text part is streaming | `✍️ 回复中` |

`waiting` / `reasoning` / `responding` are three distinct words on purpose:
`thinking` versus `reasoning` has the same synonym problem in English that
`思考` versus `推理` has in Chinese. The same three words serve the child-agent
activity line (`ChildActivity`), so a run's liveness and a card's header cannot
use different words for the same phase.

**4. A terminal header is copy, not a token.** `✅ 完成` / `❌ 出错` /
`⏹ 已停止` close the card as a sentence; they are not a machine slot. The ledger
has two layers: `RunStatus` (`Running` / `Completed` / `Failed` / `Cancelled` /
`Unknown`) is **the client's statement**, and cola's `TaskEnding` (Wake /
RuntimeEnded / Lost / Cleaned) is how cola learned it ended — wider than
`RunStatus`, because `结束` (a runtime-confirmed end with no Wake) and `已清理`
(the user's dismissal) have no client-side status, and a runtime-confirmed end
does **not** become a client `Completed`. The rendered verb — `完成` / `失败` /
`已取消` / `结束` / `已失联` / `已清理` — is copy derived from `TaskEnding`; the
enum spellings are invariant because adapters and logs compare them. *(If a
later review prefers `🔔 shell completed：…` verbatim, only the copy table
changes — the model values are already there.)*

**5. Section labels are copy.** `**Input**` / `**Output**` become `**输入**` /
`**输出**`. Units (`个文件`, `行`, `处匹配`) stay copy. Card titles (`📄 会话`,
`🧩 子会话`) stay copy and stay Chinese.

**6. The split is enforced by one module: `src/feishu/card/vocab.rs`.**

- It owns every token constant and the functions that derive a token from the
  read model (`tool_token`, `phase_token`). Their signatures are spec #1057's.
- `imageGeneration` is not a `read`: it creates an image and has no file the card
  viewed, so it stays `Other`/`Opaque` until a second adapter gives it a shape.
- It owns the copy constants a second module would otherwise duplicate; copy that
  is purely local to one card may stay beside that card.
- No renderer writes a token as a string literal. A rename edits `vocab.rs` and
  its tests; the compiler finds the callers.
- `vocab.rs` is the future `zh-CN` catalog's seed: copy functions become lookups,
  token functions do not change.

**7. No i18n machinery now.** No locale plumbing, no lookup layer, no resource
files. The classification is the groundwork; building the catalog before a
second locale exists would be a guess.

**8. What is never touched.** Model output, user input, command lines, paths and
ids are neither tokens nor copy — they render verbatim (subject to the existing
escaping and budget rules).

## Why

- **It resolves the user's own observation.** "`推理中` and `shell` are in the
  same position" is exactly the question "is this a token or copy?" — and once
  asked, the answer follows: both are slots, so both are tokens.
- **It fixes the synonym defect.** Two near-synonyms became three distinct
  words, in both languages.
- **It makes i18n a catalog, not a hunt.** Tokens are already stable; copy is
  already enumerated by module.
- **It ends the per-adapter vocabulary.** `命令` and `bash` cannot both be the
  word for an execute call once the token is `shell` and the vendor id is
  diagnostic (ADR-0105).

## Considered options

- **All English.** Rejected: card titles and sentences are copy, and making them
  English does not follow from the classification. It would also make a
  Chinese-facing bot read like a log file.
- **All Chinese.** Rejected: tool nouns are domain vocabulary shared with the
  CLI and with other clients; translating them would detach the card from the
  thing it describes, and would need re-translation per locale.
- **Keep the mix, document the exceptions.** Rejected: the mix is the defect,
  and the Codex arms show that an undocumented exception becomes a second
  vocabulary.
- **Build i18n now.** Rejected: one locale, no second consumer, and the
  classification is the expensive part; the lookup layer is cheap later.

## Consequences

- **Visible changes:** the three live phase words become `waiting` /
  `reasoning` / `responding`; `bash` → `shell`; `task`/`subagent` →
  `subagent`; Codex's translated nouns disappear in favour of the same tokens;
  `**Input**`/`**Output**` become `**输入**`/`**输出**`.
- **Tests change with the words.** Header tests, ledger tests and tool-panel
  tests each pin a copy string; those diffs are expected and are the review's
  evidence that the rename reached every surface.
- **A locale later adds copy only.** A `zh-CN` catalog replaces copy functions;
  the token table and `tool_token`/`phase_token` stay as they are.
- **A vendor tool keeps its name.** MCP and plugin tools are unaffected; they
  were never in the token table.
- **The token table and ADR-0105's `ToolKind` are different axes.** Several
  tokens may share one kind (`glob`/`grep` → `Search`, `write`/`edit` → `Edit`),
  and the token refines the kind through `ToolDetail`. A client that can only
  fill the coarse kind still renders correctly.

## Open

- **`plan_exit`, `lsp`, `invalid`.** Built-ins outside the shared kinds; they
  stay opaque (vendor name) until a second adapter makes one shared.
- **Where the copy catalog lives** if a second locale ever lands (`vocab.rs`
  functions versus a resource file) — deferred with the locale itself.
