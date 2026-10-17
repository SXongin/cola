# Local skills

Where cola's agent workflow diverges from an upstream skill, the **mechanism** — *how* a step runs — lives in a **repo-local skill** under `.opencode/skill/`, not restated in `AGENTS.md` prose or a sub-agent prompt. The **policy** — *when* a step runs and *who* calls it — stays in `docs/agents/flow.md`. This file is the convention; the skills are the mechanism. (ADR-0034's 2026-10-17 amendment supersedes that ADR's earlier "never forked" clause and records the decision.)

## Why a skill, not prose

Policy prose (`AGENTS.md`, `flow.md`) is *ambient*: it sits in every context window and competes for attention, so a rule that must fire at one step is diluted there. A skill is *invoked*: it loads exactly when the flow reaches it. So *how* a step runs belongs in a skill; *whether and when* it runs stays in `flow.md`.

## Where a local skill lives

Both `.opencode/skill/<id>/SKILL.md` and `.agents/skills/<id>/SKILL.md` are live sources, and `.opencode` wins a name collision. The rule is **what the skill drives**:

- **`.opencode/skill/`** — the skill drives OpenCode machinery: it dispatches sub-agents (`implementer`, `merger`, `code-reviewer`) or leans on OpenCode config (`subagent` denials, `disable-model-invocation`, the skill tool). Every skill here — `foreman`, `adversarial-review`, `code-review`, `implement-spec`, `upgrade-skills` — is this case: the sub-agent types they dispatch exist only in `.opencode/agent/`.
- **`.agents/skills/`** — the skill is instructions any agent could follow, with no OpenCode coupling; put it there so another tool (Claude Code, Codex) can share it. Note the two traps in that path: it reads **only `skills/`** (plural — `.agents/skill/` silently loads nothing), and `.opencode` outranks it, so a same-id `.opencode/skill/` silently shadows it. Sub-agents are OpenCode-only: `.agents/agent/` is not read at all.

## Shadowing

OpenCode loads `~/.agents/skills/<id>` (global, via the compatibility plugin) and every `.opencode/skill/<id>` (project) into one map keyed by **id**; the later write wins, and project sources load **after** global ones. So a repo-local `.opencode/skill/<id>/SKILL.md` **shadows** the global skill with the same id — `/code-review` and `/implement-spec` are cola's. (Verified live 2026-10-17: a repo-local `.opencode/skill/caveman/` replaced the global `caveman`, and a repo-local `.agents/skills/caveman/` did too; with both present `.opencode` won; removing them restored the global one.) The **id is the folder name**, so the folder must match the skill's `name`.

A shadowed skill must never point at itself: its `SKILL.md` reads the upstream **file path** (`~/.agents/skills/<id>/SKILL.md`), never invokes the id — a by-id invocation resolves to the shadow.

## Compose or fork

- **Pure addition → compose.** When the delta only *adds* a pass or an entry point and leaves upstream's internals untouched, shadow with a thin skill that reads the upstream file and adds the delta. Upstream improvements flow through; there is nothing to merge. `code-review` is this case: it composes upstream's two axes with `adversarial-review`.
- **Real change → fork.** When the delta changes upstream's own steps, own the text. `implement-spec` is this case (landing is linear, not merging), and it names its upstream source in its header so the diff stays findable.

## Upgrading

A shadow replaces the global skill, so the local copy does **not** inherit upstream updates — and a stale skill is worse than stale prose, because the agent follows it confidently. So upgrading is a **review, not a command**: fetch upstream, diff it against the local copy, and fold in only what still applies. Never let a CLI update be the thing that changes behaviour — `skills update` only rewrites `~/.agents/skills/`, which the shadow hides.

The procedure is the **`/upgrade-skills`** skill. Its pin is a **snapshot**: `docs/agents/upstream/<id>.md` is a verbatim copy of the upstream `SKILL.md` as of the last fold, so a `diff` against `~/.agents/skills/<id>/SKILL.md` is exactly what upstream changed since (the directory vendors third-party skill text for that diff alone; the local skill's `metadata.upstream` names its source). A **compose** skill reads upstream live and has no text to fold — only its pointer can go stale; a **fork** owns upstream's steps and marks its divergences. Either way, a fold refreshes the snapshot.
