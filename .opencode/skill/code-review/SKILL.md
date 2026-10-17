---
name: code-review
description: "Review the changes since a fixed point (commit, branch, tag, or merge-base) — the repo's *paired* pass: the two-axis review (Standards + Spec) AND the adversarial pass (correctness, test adequacy, robustness), reported side by side under `## Standards`, `## Spec`, `## Adversarial`, never merged. Use when the user wants to review a branch, a PR, work-in-progress changes, or asks to \"review since X\"."
metadata:
  upstream: "mattpocock/skills (skills/engineering/code-review/SKILL.md)"
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

## Re-review round (same scope)

*When* a re-review runs, who routes the fix, and the two-round bound are `docs/agents/flow.md`; this is the mechanics.

A fix round at the **same scope** resumes the sessions of the reviewers that raised the findings — all three — instead of dispatching fresh ones. Keep each reviewer's session id from the first round; `foreman` and `implement-spec` hold it for the lifetime of a ticket.

The resumed prompt carries:

- the findings **verbatim**, never a summary;
- the fix's delta as a **ref pair**, not pasted text: the HEAD the first round reviewed and the current HEAD, so the reviewer runs `git diff <from>..<to>` itself — plus the original full-diff command for the whole picture;
- the brief: for every finding, resolved / not resolved / partially, with the reason; and what the delta could break that the first round did not see.

The adversarial half follows its own skill's re-review step, which re-derives its verdict from the whole diff rather than the delta alone. Resuming keeps the reviewer independent of the author and leaves the bound and the reported verdict untouched.

Start fresh reviewers the moment the scope is no longer the one that raised the findings — the whole-branch pass after a ticket window included. A resumed reviewer judges the delta it was briefed on and has already accepted the rest; a new scope has no such "rest".

## Aggregate

Present three sections — `## Standards`, `## Spec`, `## Adversarial` — verbatim or lightly cleaned. **Never merge or rerank across them**: the axes are deliberately separate, and a single cross-axis "worst issue" is the reranking the separation exists to prevent. Close with a one-line per-axis finding count (and each axis's worst issue, if any) — but keep the adversarial section's `CODEX_REVIEW_VERDICT` line **last, with nothing after it**, so a gate that parses it is not broken by the summary.

This skill reports; it never edits.
