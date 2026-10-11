# The `/skill` command: structured prompt skills from Feishu, with a V1 text fallback

> **Amended 2026-10-11 (#662)**: the invocation sigil is `#<id>` — one `#` per
> skill (`#implement-spec #foreman 644`) — measured to pass through Feishu
> byte-for-byte. `/skill` stays as the picker (the list entry) and `/skill <id>`
> as an alias. Every `/skill <id>` spelling below describes the earlier form and
> is superseded on the sigil alone; the mechanism is unchanged. See the
> Amendment at the end.
>
> **Amended 2026-10-11 (#664)**: the picker is now ONE paged, searchable card —
> a keyword search over the skill's id/name/description plus a pager, the shape
> ADR-0051/ADR-0052 gave `/switch`, `/dir` and `/sub`. It no longer splits a long
> catalog across several card messages.

Feishu gets its own way to load an OpenCode **skill**: the slash command
`/skill <id>`, repeated once per skill, with the whole message text kept as the
prompt. It cannot be `@` — that sigil belongs to Feishu's person mentions — so
cola mirrors the composer's `@skill-id` with a command instead. The ids ride the
existing prompt axis as one generation-neutral `{ id, name }` attachment: V2
emits the prompt body's structured `skills: [{ id }]` array, and V1, which has
no such field, folds a "load the skill by name with your `skill` tool"
instruction into the text. The V2 prompt route does **not** enforce the `skill`
permission; `/skill` is an explicit human action and we accept that gap rather
than adding a cola-side allow-list.

## Context

- **The composer's path is client-side.** The TUI/OpenChamber composer resolves
  `@skill-id` into a structured `skills: [{ id }]` field on the prompt, and the
  server resolves and injects each skill's body deterministically. cola forwards
  only flat text, so `@implement-spec 644` reaches the model as a literal
  string — the model does not know the skill exists and goes looking for it.
- **`@` cannot be the sigil in Feishu.** Typing `@` opens the person-mention
  picker, and mentions arrive as opaque `@_user_N` tokens
  (`AGENTS.md` pitfall #10); there is no client-side `@skill-id` resolution in
  a message.
- **The workflow skills are user-invoked.** `implement-spec`, `grill-with-docs`,
  `to-spec`, … set `disable-model-invocation`, so the model never sees them in
  `<available_skills>` and cannot reach them on its own. The human is the only
  index, and from Feishu the human had no way to be it.
- **The prompt axis already exists.** ADR-0055/0056 put generation differences
  behind the per-generation strategy; a skill attachment is one more neutral
  field on the existing prompt call, not a parallel submit path.

## Decision

- **A new Bridge command.** `Skill(Vec<String>)` is parsed from a message that
  begins with the `/skill` token: each `/skill <id>` token contributes the token
  that follows it, collected in order, and the whole original message text —
  tokens included — is the prompt text. This mirrors the composer, which keeps
  the `@skill-id` reference in the text, so no separate "arguments" concept is
  invented.
- **Dispatch resolves before submitting.** On dispatch the Bridge reads the
  skill list **once**, scoped to the Chat/Topic's current project directory
  (the location the eventual prompt runs in), resolves each typed id to its
  canonical `{ id, name }`, keeps the ids that resolve, and submits the turn on
  the existing prompt pipeline (the `Forward` route, not the command
  dispatcher). A dispatch that resolves at least one id submits those; ids that
  do not resolve are dropped.
- **The picker is the recovery path.** A bare `/skill` (no id) and a dispatch
  whose ids **all** fail to resolve both answer with the same picker card — one
  row per registered skill, its name, plus its description when the skill
  declares one (a description-less skill shows its name alone) — built from the
  list just read; the second leads with an unknown-id error line. The list read
  is deliberately **unfiltered**: hidden (`disable-model-invocation`) and
  description-less skills are listed too, since reaching exactly the
  user-invoked skills the model never advertises is the point.
- **One generation-neutral skill-list read.** The Backend contract grows
  `list_skills(directory)` returning `{ id, name, description?, content? }`. V2
  reads `GET /api/skill`; V1 reads its own `GET /skill`, whose identity IS the
  name (V1's `Skill.Info` has no id, so V1's `id` is the name). `content` is the
  skill's own markdown — the loaded-skill card's fold body. A read failure
  returns an empty list, and the picker degrades to a no-skills state.
- **The prompt attachment is one neutral field.** `prompt` gains a
  `skills: &[PromptSkill]` axis (`{ id, name }`). Ids are **deduped by id**
  (first-occurrence order preserved) before the attachment is built: the server
  already dedupes injection within one prompt, but cola dedupes too. V2 emits
  the body's structured `skills` array carrying only the id; V1, which has
  neither the field nor a skill prompt part, folds the request into the prompt
  text as an instruction to load the skill by name with the `skill` tool. An
  empty slice leaves the prompt byte-for-byte untouched on both generations.
- **The loaded-skill fold is one renderer on a dedicated card** (acceptance
  reversal, 2026-10-10). A resolved `/skill <id>` — typed, or a picker-row tap —
  replies a small card under the user's message carrying one folded collapsible
  panel titled `🧩 已加载技能：<name>` per **distinct** loaded skill, its body the
  skill's own markdown from the skill-list read's `content` (raw; any server
  `<skill_content>` envelope is unwrapped harmlessly, so the renderer is shared
  with the `skill` tool panel's shape). Each fold's body is capped at the `skill`
  tool panel's 3,000 characters, and the card's folds share one aggregate budget
  — 24 KB of structure, titles and bodies, charged by each body's ACTUAL
  sanitized byte length, not a `chars × 3` estimate (ASCII bodies would
  over-block; a `<` expands to `&#60;` (5 bytes) and would under-block) — sized
  so an ordinary 1–3 skill dispatch shows every body in FULL while the whole card
  stays comfortably under Feishu's 30 KB limit. Only a genuinely large
  dispatch degrades: past `SKILL_FOLD_MAX` folds the overflow collapses into one
  `🧩 已加载技能（等 N 个）` summary fold, and a fold past the aggregate keeps its
  title with an omission body (AGENTS.md #13). A **mixed dispatch** — some ids
  resolve, some do not — still submits the resolved ones and leads the card with
  a `⚠️ 未找到技能：…` line naming the rest, so an unknown id is never dropped
  silently (Q19); a dispatch whose ids ALL fail to resolve keeps the
  error-plus-picker card and submits no prompt. A rejected card degrades to a
  plain-text message that consumes the SAME budget walk and shows the SAME body
  extent as the folds. The live Turn card, the Session Snapshot's 「最近对话」
  tail and the External Message preview do **not** render skill folds: those
  sites stay text-only, and non-text user-message structures from other clients
  are deferred (#660).
- **Accepted permission gap.** The V2 prompt route does not enforce the
  `skill` permission: the server injects any existing skill id, even one the
  agent's config `deny`s, and only the server-side `skill` tool runs the
  permission check. cola accepts the gap rather than adding a cola-side
  allow-list or routing through the tool (which would make injection depend on
  the model deciding to call it). Revisit if a denied skill leaking through
  `/skill` becomes a real problem.

## Considered options

- **An `@skill-id` sigil, like the composer.** Rejected: Feishu owns `@` for
  person mentions, and its mention tokens are opaque — there is no client-side
  hook to resolve a skill id from them.
- **Route the load through the server's `skill` tool.** Rejected: it would make
  injection depend on the model deciding to call the tool, replacing
  deterministic injection with a model decision, and it is not how the composer
  path works either.
- **A cola-side skill allow-list to close the permission gap.** Rejected: it
  duplicates the server's own permission rules and drifts from them. The gap is
  recorded here instead.
- **Structured skill attachment on V1.** Rejected as impossible: V1 has neither
  the prompt field nor a skill part, so it gets the text fallback only.
- **Strip the `/skill` tokens from the prompt text.** Rejected: it would make
  cola's transcript diverge from the composer's, and the skill's argument text
  (`/skill implement-spec 644`) must reach the model exactly as typed.

## Consequences

- Loading a skill is **strictly explicit**: no skill is ever auto-loaded, and
  the user-invoked workflow skills become reachable from Feishu.
- The loaded-skill feedback is a **dedicated small card** replying under the
  user's `/skill` message; the live Turn card, the Session Snapshot's
  「最近对话」 tail and the External Message preview render **no** skill fold
  (acceptance reversal, 2026-10-10). Non-text user-message structures from other
  clients or adopted snapshots (skill / file / …) are deferred to #660.
- A denied skill id still injects through `/skill` on V2; only the `skill` tool
  enforces the permission. Recorded, not silent.
- V1 retirement stays a deletion: the text fallback lives in the V1 generation
  strategy, not the Bridge.
- `/skill` is discoverable in every help surface — the `/help` card, `/help
  skill`, and the plain-text fallback — and the Feishu-specific syntax (no `@`;
  the repeated-token form) is recorded in `AGENTS.md`.

Related: #652, #651, ADR-0055, ADR-0056.

## Amendment (2026-10-11, #662): the skill sigil is `#<id>`

The invocation sigil moved from `/skill <id>` to `#<id>`, one `#` per skill
(`#implement-spec #foreman 644`). Feishu was **measured** to pass `#` through
byte-for-byte (2026-10-11: `#probe-test 井号 #644 忽略本条` reached cola's log
intact — no entity-ization, no rewrite, no zero-width characters), so `#` is the
sibling of the composer's `@` that `@` itself cannot be (Context above). `/skill`
stays as the picker — the list-entry command — and `/skill <id>` as an alias.

- **`#<id>` is resolve-gated and silent on failure.** A `#<id>` token attaches a
  skill only when it resolves against the skill list; one that resolves nothing
  is ordinary prose (`#644`, a heading) and stays in the prompt text verbatim —
  the `#` form never opens the picker. That is the only behavioural difference
  from `/skill <id>`, whose dispatch shows the error-plus-picker card when
  nothing resolves: the source rides `SkillInvocation::picker_on_empty`.
- **Only `#` + an id run starting with a letter enters the skill path.**
  `parse_command` takes, after a `#`, the run of `[A-Za-z0-9-]` that starts with
  an ASCII letter (ids are kebab-case). A message carrying `#644` or `# 标题` is a
  plain prompt and costs **no** skill-list read — the ubiquitous issue reference
  stays off the hot path. The run ends at any other character, so trailing
  punctuation and CJK text run straight into the token (`#implement-spec,`,
  `用#caveman试试`) still name the skill. A `#word`-shaped token does trigger the
  read (and Lazy Start), exactly like `/skill`.
- **A picker-row tap re-enters as `#<id>`.** The synthesized message text (and so
  the prompt the model sees) is now `#<id>`, not `/skill <id>`.

Unchanged: the structured V2 `skills: [{ id }]` injection and V1's text fallback,
the single generation-neutral list read, resolution to the canonical `{id, name}`,
dedupe, the dedicated loaded-skill card, the accepted permission gap, and the
whole-message-verbatim rule (ids ride the attachment; the `#` tokens stay in the
text). Everything above describes the earlier `/skill <id>` spelling.

Related: #651, #652.

## Amendment (2026-10-11, #664): the picker paginates and searches

The picker was a single list chunked across several card *messages* past the
button/size budget (`（第 x / y 页）`), with no way to filter. It is now ONE card
that windows the list into pages and carries a keyword search box — the shape
ADR-0051 gave `/dir`'s search and ADR-0052 gave every list card's pager.

- **Pages of 20, byte-bounded.** A page holds up to `SKILL_PAGE_ROWS` (20) rows —
  skill rows are one short line, so more fit per page than the session cards'
  six — while their estimated serialized bytes stay under `MAX_CARD_JSON_CHARS`.
  A single oversized id (spec #652/#655) still takes its own page rather than
  building an over-limit card, and a stale page clamps to the last (ADR-0052).
- **Search over id / name / description.** The box renders once the list
  outgrows a page, or a keyword is active (ADR-0051's conditional visibility);
  the keyword matches the skill's id, name or description, case-insensitively,
  whitespace-token AND — the list cards' rule. The submit is named
  `skillsearch|<chat>|<thread>` and the typed keyword arrives as
  `form_value.search`.
- **Stateless filter, server re-read.** `keyword` and `page` ride every button;
  a rebuild (a search, a flip) re-reads the server's skill list — unlike `/dir`,
  which reads the local store — and replaces the card in place. The read is
  best-effort (a failure degrades to the no-skills state), and a rebuild never
  runs Lazy Start: it submits no prompt and the ack must stay fast.
- **Row taps are unchanged.** A row still carries the exact id, `chat_type` and
  `reply_message_id`; the search and pager buttons carry the latter two too, so a
  rebuild re-stamps the fresh rows and a tap after a search still replies the
  loaded-skill card under the ORIGINAL user message (spec #655). The form's
  name-only submit fallback recovers chat/thread; the reply target then degrades
  to the picker card, exactly as every other list card's fallback does.
- **The text fallback matches the card.** The initial picker send still degrades
  a rejected card to `skill_list_text` over the same list the card renders (an
  empty keyword ⇒ the whole list). A rebuild replaces the card in place through
  the ack — exactly like `/dir`'s and `/sub`'s rebuilds — so it has no
  reply-with-fallback path.

Unchanged: the invocation forms (`#<id>`, `/skill`, `/skill <id>`), the
structured V2 injection and V1 text fallback, the loaded-skill card, and the
picker's unfiltered default (hidden and description-less skills are still
listed).

Related: #664, ADR-0051, ADR-0052.

## Amendment (2026-10-11, #662): the `#<id>` grammar hardens, and its coverage is bounded

The `#<id>` form shipped with a boundary-free lexer and exact-string
resolution. Reviewing it against the sigil's whole purpose — a *text* channel for
`/skill <id>`, because Feishu has no client-side `@skill-id` resolution — settled
three refinements and recorded one boundary.

- **A `#` opens a token only at a left boundary.** The character before it must
  be absent or outside `[A-Za-z0-9-#]`. So `C#caveman`, `abc#foreman` and the
  second `#` of `##foreman` are prose: they never enter the skill path and cost
  **no** skill-list read. The **right** side stays free-ending — the id run stops
  at the first non-id character — so `#implement-spec,` and `用#caveman试试`
  still name their skill. That is deliberate: skill ids are ASCII, so CJK text and
  trailing punctuation cannot be part of one, and demanding a trailing delimiter
  would have broken the common no-space CJK spelling.
- **Resolution is case-insensitive.** The typed token is matched against the
  list's canonical id with `eq_ignore_ascii_case`, so `#Implement-Spec` loads
  instead of silently landing as prose. The **matched list entry's** id — never
  the typed token — is what rides the attachment, so the server's own
  exact-match lookup (a miss is a `Skill not found` error) still sees the
  canonical id; casing never crosses the wire.
- **The coverage is bounded, and the channels are layered.** Skill ids carry no
  format guarantee: the server's id is the skill's directory name (or its `.md`
  filename) with no slugification, and its `Skill.ID` is an unconstrained string
  — an id may start with a digit, contain `_` or `.`, or hold whitespace/CJK.
  The `#<id>` grammar covers the conventional `[a-z][a-z0-9-]*` form only;
  `/skill <id>` (a whole whitespace-delimited token) covers `2fa-helper` and
  `web_scrape`; a picker row (a structured callback value) is the only channel
  for an id with whitespace — a V1 name, or a V2 spaced directory. This mirrors
  the composer, where the picker plus structured attachment is the general path
  and no prose sigil exists at all: `#<id>` is cola's convenience for the
  conventional form, not a promise that every id is typable.
- **No escape hatch.** There is no way to *mention* a resolvable id without
  loading it; writing the name without a `#` is the documented way to refer to a
  skill in prose.

Considered and rejected: **loosening the character class** to any non-whitespace
run (it breaks `用#caveman试试` and puts `#644` on the hot path); **list-driven
longest-prefix matching** (it makes the parse server-dependent — the command
decision would need the skill list before `parse_command` can answer —
re-introduces the `#644` list read, and is ambiguous over prefix-sharing ids such
as `grill-me` / `grilling` / `grill-with-docs`); **a `\#` or code-span escape**
(grammar for a case the current corpus does not have — every skill installed
alongside this decision is `[a-z][a-z0-9-]*`); **retitling this ADR** (amendments
do not rewrite history; the banner above carries the current sigil).

Unchanged: the whole-message-verbatim rule, silent prose when a `#` dispatch
resolves nothing, the `⚠️ 未找到技能：…` line when SOME ids resolve, the picker,
the loaded-skill card, and the accepted permission gap.

Related: #662, #652.
