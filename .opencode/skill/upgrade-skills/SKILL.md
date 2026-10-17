---
name: upgrade-skills
description: Upgrade the repo's local (shadowing) skills against their upstreams — detect which upstreams moved since the local copy was folded, show the delta, and fold the still-applicable changes in by hand. Use after `skills update`, or before trusting a local skill that may have drifted.
disable-model-invocation: true
---

# Upgrade skills

The local skills under `.opencode/skill/` **shadow** their upstreams, so an upstream update is invisible to the running agent: `skills update` rewrites `~/.agents/skills/`, which the shadow hides (`docs/agents/skills.md`). This skill is the deliberate diff-and-fold that closes that gap — a **review, not a command**. Nothing here is applied automatically, and nothing replaces a local copy wholesale.

`skills update` only *fetches* into `~/.agents/skills/`; the fold below is what changes cola. Never let the update be the thing that changes behaviour.

Input: optional skill ids. Default: every local skill, `.opencode/skill/*/SKILL.md`.

## 1. Enumerate the shadows

List `.opencode/skill/<id>/SKILL.md`. Each one's upstream is `~/.agents/skills/<id>/SKILL.md`. A local skill with **no** upstream is cola's own (`foreman`, `adversarial-review`, `upgrade-skills`) — skip it, there is nothing to fold.

## 2. Classify: compose or fork

Read the local `SKILL.md` (mirrors `docs/agents/skills.md` "Compose or fork"):

- **compose** — it reads the upstream **file path** at runtime (`code-review` does). Upstream's body flows through live, so there is no text to fold — only the pointer can go stale.
- **fork** — it owns a copy of upstream's steps and marks its divergences with `[cola]` (`implement-spec` does).

## 3. Detect whether upstream moved

The pin is `docs/agents/upstream/<id>.md` — a verbatim copy of the upstream `SKILL.md` as of the last fold (the local skill's frontmatter `metadata.upstream` names its source). Compare it to the live upstream, one command per skill:

    git diff --no-index --quiet docs/agents/upstream/<id>.md ~/.agents/skills/<id>/SKILL.md

Exit **0** → unchanged since the last fold; report and move on. Exit **1** → upstream moved; go to step 4. A missing upstream file means it was renamed or removed — report that rather than inventing a diff. A local skill with no snapshot cannot be compared: treat it as "unknown", fold it by hand against upstream, then write the snapshot.

## 4. Show the delta

The snapshot makes "what did upstream change since the last fold" exact, for both kinds:

    git diff --no-index docs/agents/upstream/<id>.md ~/.agents/skills/<id>/SKILL.md

Read it as upstream's own change, then judge every hunk against the local skill:

- **fork** — the local copy owns upstream's steps with `[cola]`-marked divergences, so a hunk touching a `[cola]` line is folded by updating the note, and the rest is folded as new upstream text.
- **compose** — the local file is only a pointer, so most hunks are irrelevant; what matters is whether a section the pointer **names** was renamed, moved or removed. A pointer to a section that no longer exists is the one way a compose skill goes stale silently.

## 5. Fold

Fold only what still applies, by hand:

- **fork** — apply upstream's changed instructions onto the local text, keeping every `[cola]` divergence. An upstream change to a line a `[cola]` note already overrides is folded by **updating the note**, never by dropping it.
- **compose** — update the pointer only if upstream renamed a section or moved the file.

Never replace the local file with upstream's version — that discards cola's divergence, which is the whole reason the shadow exists.

## 6. Record and verify

- Refresh the snapshot (`docs/agents/upstream/<id>.md`) **only to a revision you folded in full**. If you deliberately left an upstream change out, leave the snapshot alone (and say so in the report) — refreshing it would make the next run read "unchanged" and drop the deferral out of the review path for good.
- Verify the local skill still **loads**: its frontmatter parses and the skill list (or the skill tool) serves cola's file. This matters more than it looks — broken frontmatter makes OpenCode **skip** the local file, and the upstream skill silently takes over.
- Report, per skill: the upstream source, whether it had moved, what was folded, and what was deliberately left.

This skill folds by hand and reports; it never runs `skills update` for you and never overwrites a local skill.
