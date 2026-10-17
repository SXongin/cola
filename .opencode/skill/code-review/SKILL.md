---
name: code-review
description: "Review the changes since a fixed point (commit, branch, tag, or merge-base) — the repo's *paired* pass: the two-axis review (Standards + Spec) AND the adversarial pass (correctness, test adequacy, robustness), reported side by side under `## Standards`, `## Spec`, `## Adversarial`, never merged. Use when the user wants to review a branch, a PR, work-in-progress changes, or asks to \"review since X\"."
metadata:
  upstream: "mattpocock/skills (skills/engineering/code-review/SKILL.md)"
  upstream-sha256: "47f4e52c21694def9c7c11cbfbf891ca35eac7a93e395797515be3c8a409ae50"
---

Cola's entry point for review. It runs **both** halves of the repo's paired pass in one skill, so a review point can never run one axis alone.

- **The two-axis review** — Standards + Spec, run as **parallel sub-agents** and reported separately.
- **The adversarial pass** — the Codex gate's correctness / test-adequacy / robustness pass, run as its own sub-agent.

The binding — *when* a review point runs, *who* calls it, how findings route, and the re-review bound — is `docs/agents/flow.md`. This skill owns only the composition and the two passes' mechanics.

## The two-axis half

The process is upstream's, unchanged. Read the upstream definition at `~/.agents/skills/code-review/SKILL.md` and follow it for this half: pin the fixed point and fail fast on a bad ref or an empty diff; identify the spec source; identify the standards sources and carry the smell baseline; spawn the **Standards** and **Spec** sub-agents in parallel; report under `## Standards` and `## Spec` without merging or reranking. Do **not** invoke the `code-review` skill by id — this file shadows it, so a by-id invocation resolves here; read that path instead.

If that file is absent, take the two axes from the repo: **Standards** from `CODING_STANDARDS.md` and `CONTRIBUTING.md` ("Reviewing"), **Spec** from the originating issue/spec (`docs/agents/issue-tracker.md`) — same parallel-sub-agent shape.

## The adversarial half

Run the `adversarial-review` skill over the **same base** and, when one was given, the **same pathspec**. Do not invent the rubric: that skill — or `.github/codex/prompts/review.md`'s `## The adversarial pass` and `## Rules` — is the source, and its final `CODEX_REVIEW_VERDICT` line is kept.

## Aggregate

Present three sections — `## Standards`, `## Spec`, `## Adversarial` — verbatim or lightly cleaned. **Never merge or rerank across them**: the axes are deliberately separate, and a single cross-axis "worst issue" is the reranking the separation exists to prevent. Close with a one-line per-axis finding count (and each axis's worst issue, if any) — but keep the adversarial section's `CODEX_REVIEW_VERDICT` line **last, with nothing after it**, so a gate that parses it is not broken by the summary.

This skill reports; it never edits.
