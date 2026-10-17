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

Each local skill records its upstream source and the upstream `SKILL.md`'s sha256 in frontmatter `metadata`:

    metadata:
      upstream: "mattpocock/skills (skills/engineering/implement-spec/SKILL.md)"
      upstream-sha256: "<64 hex>"

Recompute and compare (one command per skill):

    sha256sum ~/.agents/skills/<id>/SKILL.md

Equal → unchanged since the last fold; report and move on. Different → go to step 4. If the file is missing, upstream was renamed or removed — report it rather than inventing a diff. (Only `SKILL.md` is pinned; if an upstream skill ships support files your copy references, hash and diff those too.)

## 4. Show the delta

    diff -u .opencode/skill/<id>/SKILL.md ~/.agents/skills/<id>/SKILL.md

Read it as "what upstream now has that cola's copy does not". For a **fork**, the lines carrying cola's intent are the `[cola]`-marked ones; everything else in the diff is upstream's own change. For a **compose**, re-check that the sections the pointer names still exist upstream — a rename or a move invalidates the pointer silently.

## 5. Fold

Fold only what still applies, by hand:

- **fork** — apply upstream's changed instructions onto the local text, keeping every `[cola]` divergence. An upstream change to a line a `[cola]` note already overrides is folded by **updating the note**, never by dropping it.
- **compose** — update the pointer only if upstream renamed a section or moved the file.

Never replace the local file with upstream's version — that discards cola's divergence, which is the whole reason the shadow exists.

## 6. Record and verify

- Set `metadata.upstream-sha256` to the sha256 just folded from, so the next run reads "unchanged" until upstream moves again.
- Verify the local skill still **loads**: its frontmatter parses and the skill list (or the skill tool) serves cola's file. This matters more than it looks — broken frontmatter makes OpenCode **skip** the local file, and the upstream skill silently takes over.
- Report, per skill: the upstream source, whether it had moved, what was folded, and what was deliberately left.

This skill folds by hand and reports; it never runs `skills update` for you and never overwrites a local skill.
