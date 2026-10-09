---
name: adversarial-review
description: Repo-local companion to the two-axis `/code-review`. Run the Codex gate's adversarial pass — correctness, test adequacy, robustness — on a branch, a ticket window, or a PR diff before the PR is opened, so the CI gate is far less likely to raise a new blocking finding.
---

# Adversarial review

The binding — its pairing with `/code-review`, which review points, who runs it, how findings route, the re-review bound — is `docs/agents/flow.md`; this skill adds only the pass below.

CI deliberately kept this pass out of `/code-review` (`ADR-0034`, 2026-10-04 amendment), which left the author-side review blind to exactly the class the gate blocks on: a negative path the new test never walks, a test that still passes with the change reverted, an unhandled error/cleanup/ordering path, a lost tail under a size split. Running it locally is the deliberate trade recorded in `ADR-0034`'s 2026-10-09 amendment — fewer review round-trips.

## The rubric is not written here

Do not invent the checks. Read `.github/codex/prompts/review.md` and lift its `## The adversarial pass (the part an author-side review tends to miss)` and `## Rules` sections **verbatim** into the sub-agent prompt, so the local pass and the gate stay one rubric. One clause does not carry to this host: the reference trees named in `AGENTS.md` exist here, so the host-tree exemption is dropped — say so in the prompt. Everything else stands, the final `CODEX_REVIEW_VERDICT` line included.

## Process

### 1. Pin the base

Whatever the caller names for the **base**: a branch, a SHA, or a **ticket window** — the run of commits carrying one ticket's `Refs: #<n>` trailer, with the base at the commit before the run (the `foreman` window definition). Default the base to `main` when none is given. A caller may also scope the review to a **pathspec** (a subset of the touched files) — carry it through to both commands.

Confirm the base resolves (`git rev-parse <base>`). Review with the gate's merge-base semantics: `BASE=$(git merge-base <base> HEAD)`, then `git diff "$BASE"` (add `-- <paths>` when scoped) — the scope of CI's `git diff <base>...HEAD`, extended to the working tree so committed, staged, and unstaged work are all in scope. That diff still omits **untracked** files, so list them with `git ls-files --others --exclude-standard` (add `-- <paths>` when scoped) and hand them to the reviewer as content. The commit list is `git log <base>..HEAD --oneline` (add `-- <paths>` when scoped). Fail here when the diff and the untracked list are both empty, never inside the sub-agent.

### 2. Spawn one reviewer sub-agent

Dispatch a single `code-reviewer` sub-agent **from the orchestrating session** — a sub-agent cannot spawn it. The prompt carries:

- the diff `git diff "$BASE"` (with `BASE` the merge-base, plus `-- <paths>` when scoped), the untracked files from `git ls-files --others --exclude-standard` (not in the diff), and the commit list (`git log <base>..HEAD --oneline`, plus `-- <paths>` when scoped);
- the `## The adversarial pass` and `## Rules` sections pasted in full (the sub-agent has no network; paste the rubric rather than pointing at it), minus the host-tree clause, with that omission stated in the prompt;
- this brief:
  > You are reproducing the CI Codex gate's adversarial pass on this diff. The reference trees named in `AGENTS.md` exist on this host, so the host-tree exemption in the Rules does not apply. Report findings first and finish with the `CODEX_REVIEW_VERDICT` line the Rules prescribe.

### 3. Aggregate

Present the findings under `## Adversarial`, verbatim or lightly cleaned, keeping the reviewer's final `CODEX_REVIEW_VERDICT` line. Do **not** merge or rerank them against `/code-review`'s two-axis report — the axes stay separate, exactly as CI reports them.

This skill reports; it never edits.
