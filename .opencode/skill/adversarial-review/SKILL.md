---
name: adversarial-review
description: Repo-local companion to the two-axis `/code-review`. Run the Codex gate's adversarial pass — correctness, test adequacy, robustness — on a branch, a ticket window, or a PR diff before the PR is opened, so the CI gate is far less likely to raise a new blocking finding.
---

# Adversarial review

The binding — its pairing with `/code-review`, which review points, who runs it, how findings route — is `docs/agents/flow.md`; this skill adds only the pass below.

CI deliberately kept this pass out of `/code-review` (`ADR-0034`, 2026-10-04 amendment), which left the author-side review blind to exactly the class the gate blocks on: a negative path the new test never walks, a test that still passes with the change reverted, an unhandled error/cleanup/ordering path, a lost tail under a size split. Running it locally is the deliberate trade recorded in `ADR-0034`'s 2026-10-09 amendment — fewer review round-trips.

**This is not the gate rehearsal.** The pass here is one half of the pair. `implement-spec`'s pre-push rehearsal runs the **whole** gate prompt — both axes plus this pass — with the spec and ticket bodies, in the gate's harness; a clean adversarial verdict here does not stand in for it.

## The rubric is not written here

Do not invent the checks. The sub-agent lifts its `## What blocks the merge`, `## The adversarial pass (the part an author-side review tends to miss)` and `## Rules` sections **verbatim** from `.github/codex/prompts/review.md`, so the local pass and the gate stay one rubric. The rubric is a file in this repo, so hand over its path rather than a copy — no network is involved. Two clauses are conditional here: the reference trees named in `docs/agents/references.md` exist on this host, so the host-tree exemption is dropped — say so in the prompt; and when the pass runs before the PR exists there is no PR metadata to read, so the metadata part of the completeness rule does not apply — when the PR exists, the dispatch supplies its body and linked issues and that part applies. Everything else stands, the final `CODEX_REVIEW_VERDICT` line included.

## Process

### 1. Pin the base

Whatever the caller names for the **base**: a branch, a SHA, or a **ticket window** — the run of commits carrying one ticket's `Refs: #<n>` trailer, with the base at the commit before the run (the `implement-spec` window definition). Default the base to `main` when none is given. A caller may also scope the review to a **pathspec** (a subset of the touched files) — carry it through to both commands.

Confirm the base resolves (`git rev-parse <base>`). Review with the gate's merge-base semantics: `BASE=$(git merge-base <base> HEAD)`, then `git diff "$BASE"` (add `-- <paths>` when scoped) — the scope of CI's `git diff <base>...HEAD`, extended to the working tree so committed, staged, and unstaged work are all in scope. That diff still omits **untracked** files, so list them with `git ls-files --others --exclude-standard` (add `-- <paths>` when scoped) and hand the reviewer the **paths** — it reads each one itself and reports any it could not. The commit list is `git log <base>..HEAD --oneline` (add `-- <paths>` when scoped). Fail here when the diff and the untracked list are both empty, never inside the sub-agent.

### 2. Spawn one reviewer sub-agent

Dispatch a single `code-reviewer` sub-agent **from the orchestrating session** — a sub-agent cannot spawn it. The prompt carries:

- the merge-base SHA `BASE` and the pathspec, so the reviewer runs `git diff "$BASE"` itself (plus `-- <paths>`) — hand over the **ref, never pasted diff text**; it holds `git diff`, `git log` and `git show`. Give the commit list as the command too (`git log <base>..HEAD --oneline`, plus `-- <paths>` when scoped);
- the untracked **paths** from `git ls-files --others --exclude-standard` (they are in no diff), with the brief requiring every one to be read;
- the path `.github/codex/prompts/review.md` and the three sections to lift from it (`## What blocks the merge`, `## The adversarial pass`, `## Rules`) — the rubric is a local file, so a pointer replaces a copy;
- the PR's body and linked issues when the PR already exists, so the completeness rule's metadata part applies;
- the originating issue or spec body when the caller names one, with whether the PR closes it (`Closes`/`Fixes`/`Resolves`) or only references it (`Refs`): the blocking definition's acceptance-criterion clause applies only to a closed issue, and without one that clause does not apply — never FAIL for a criterion the dispatch did not supply;
- this brief:
  > You are reproducing the CI Codex gate's adversarial pass on this diff. Take `## What blocks the merge`, `## The adversarial pass` and `## Rules` verbatim from `.github/codex/prompts/review.md`. The reference trees named in `docs/agents/references.md` exist on this host, so the host-tree exemption in the Rules does not apply. When this dispatch is pre-PR (no PR exists), the metadata part of the completeness rule does not apply; otherwise the PR body and linked issues this dispatch supplies are what it reads. Read every untracked path this prompt lists, and report any you could not read. Apply the acceptance-criterion clause only against a linked issue this dispatch marks as closed by the PR, not a `Refs`-only one. Report blocking findings first, then the non-blocking backlog, and finish with the `CODEX_REVIEW_VERDICT` line the Rules prescribe.

### 3. Re-review round (same scope)

`docs/agents/flow.md` binds when a re-review runs. Resume the **same** reviewer session with:

- the findings **verbatim**;
- the ref pair `<round-head>..HEAD` for the fix's delta, plus the unchanged full-diff command;
- the **refreshed** untracked **paths** (`git ls-files --others --exclude-standard` run again, `-- <paths>` when the review is scoped — a fix that adds a file is in neither diff, so the first round's list is stale), read in full by the reviewer; a path it silently skips is a coverage hole;
- the brief: for every finding, resolved / not resolved / partially, with the reason; what the delta could break; read every refreshed untracked path; and re-derive `CODEX_REVIEW_VERDICT` from the **whole** `git diff "$BASE"`, not the delta alone.

Start a fresh reviewer at a new scope, on a design-changing fix, and for a cross-ticket finding — a resumed reviewer has already accepted the rest of the diff.

### 4. Aggregate

Present the findings under `## Adversarial`, verbatim or lightly cleaned, keeping the reviewer's final `CODEX_REVIEW_VERDICT` line. Do **not** merge or rerank them against `/code-review`'s two-axis report — the axes stay separate, exactly as CI reports them.

This skill reports; it never edits.
