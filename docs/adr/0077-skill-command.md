# The `/skill` command: structured prompt skills from Feishu, with a V1 text fallback

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
  skill list **once**, scoped to the conversation's current project directory
  (the location the eventual prompt runs in), resolves each typed id to its
  canonical `{ id, name }`, keeps the ids that resolve, and submits the turn on
  the existing prompt pipeline (the `Forward` route, not the command
  dispatcher). A dispatch that resolves at least one id submits those; ids that
  do not resolve are dropped.
- **The picker is the recovery path.** A bare `/skill` (no id) and a dispatch
  whose ids **all** fail to resolve both answer with the same picker card — one
  row per registered skill, its name plus its description — built from the list
  just read; the second leads with an unknown-id error line. The list read is
  deliberately **unfiltered**: hidden (`disable-model-invocation`) and
  description-less skills are listed too, since reaching exactly the user-invoked
  skills the model never advertises is the point.
- **One generation-neutral skill-list read.** The Backend contract grows
  `list_skills(directory)` returning `{ id, name, description? }`. V2 reads
  `GET /api/skill`; V1 reads its own `GET /skill`, whose identity IS the name
  (V1's `Skill.Info` has no id, so V1's `id` is the name). A read failure
  returns an empty list, and the picker degrades to a no-skills state.
- **The prompt attachment is one neutral field.** `prompt` gains a
  `skills: &[PromptSkill]` axis (`{ id, name }`). V2 emits the body's structured
  `skills` array carrying only the id; V1, which has neither the field nor a
  skill prompt part, folds the request into the prompt text as an instruction to
  load the skill by name with the `skill` tool. An empty slice leaves the prompt
  byte-for-byte untouched on both generations.
- **The loaded-skill fold is one shared renderer.** A loaded skill renders as a
  folded collapsible panel titled `🧩 已加载技能：<name>` whose body is the
  skill's own instructions, unwrapped from the server's `<skill_content>`
  envelope with the sampled `<skill_files>` list dropped. The live Turn card,
  the Session Snapshot's 「最近对话」 tail and the External Message preview all
  call it, so a skill attached by another client is visible in Feishu too.
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
- A denied skill id still injects through `/skill` on V2; only the `skill` tool
  enforces the permission. Recorded, not silent.
- V1 retirement stays a deletion: the text fallback lives in the V1 generation
  strategy, not the Bridge.
- `/skill` is discoverable in every help surface — the `/help` card, `/help
  skill`, and the plain-text fallback — and the Feishu-specific syntax (no `@`;
  the repeated-token form) is recorded in `AGENTS.md`.

Related: #652, #651, ADR-0055, ADR-0056.
