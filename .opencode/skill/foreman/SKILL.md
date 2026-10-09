---
name: foreman
description: Run one spec's tickets as a serial batch — resolve them via ## Parent, order by native blocking edges, implement each with an `implementer` sub-agent, run the repo's review pair on each ticket's commit window and once more over the whole branch (`docs/agents/flow.md`), then stop for one spec-level acceptance before a single PR. Use ONLY when the user explicitly asks to run a spec's tickets (e.g. "/foreman 123"); never start a batch on your own.
---

# Foreman

Run a spec's tickets as one serial batch on one branch, ending in one PR. The spec is the unit of acceptance: the user reviews once, at the end, instead of once per ticket.

The main session is the foreman — it resolves the batch, prepares the branch, dispatches one `implementer` sub-agent per ticket, reviews each ticket's commit window, and stops before the PR. Sub-agents implement; the foreman schedules, reviews, and reports.

## 1. Resolve the batch

Input: a spec issue number, plus optional explicit ticket numbers that override the scan.

Use the gh conventions in `docs/agents/issue-tracker.md`, and paginate — `gh issue list` returns 30 rows by default, `--json` does not auto-paginate, and a result at the `--limit` ceiling is truncated: raise the limit (or fall back to `gh api --paginate`) before resolving.

- Fetch the spec's open children — every open issue whose `## Parent` references the spec issue, whatever its label. A scan that finds nothing falls back to asking for numbers.
- The batch is the `ready-for-agent` children, ordered blockers-first from the native `blocked_by` edges (`issue-tracker.md`); tracker order breaks ties. Explicit ticket numbers from the user skip the scan and the label filter — they are the batch verbatim — except that a ticket whose blocker falls outside the batch is still dropped.
- Reconcile before presenting (scan mode): every child outside the batch goes on the drop list with its reason — not `ready-for-agent`, or a blocker outside the batch. A child that appears in neither the batch nor the drop list is a resolution bug, not a silent omission.
- Derive the branch name `<type>/<spec-slug>` from the spec's title, e.g. `feat/lazy-session-creation`.

Present the batch: each ticket's number, title, and blockers; the resulting order; the branch name; and the drop list with reasons. Ask the user to confirm. Done when the user approves the list, the order, the branch, and the drop list.

## 2. Prepare the branch

From `main`:

- `git status` must be clean — stop and ask if not.
- `git fetch`, then `git switch -c <branch> origin/main`.

Done when the branch exists and the tree is clean.

## 3. Work the batch, one ticket at a time

For each ticket, in order:

1. **Mark the window** — `PRE=$(git rev-parse HEAD)`, the review's fixed point.
2. **Dispatch the implementer** (`subagent_type: implementer`) with the dispatch prompt below.
3. **Review the window** — run the repo's review pair over `PRE...HEAD` with the ticket as the spec source (`docs/agents/flow.md`). The review sub-agents dispatch from this main session; a sub-agent cannot spawn them.
4. **Fix findings** — resume the same implementer session (`task_id`) with the findings, then re-review under the binding's re-review bound (`docs/agents/flow.md`).
5. **Record progress** — post the progress comment below. It carries the review outcome and marks the batch's sanctioned `/compact` boundary; never compact mid-ticket.

Done when the ticket's commits are on the branch, its window review is clean or fixed, and the progress comment is posted. Then take the next ticket; never two at once.

## 4. Verify and build the dossier

Fetch and, if `origin/main` advanced, rebase the branch onto it — a conflict pauses the batch.

Then close the per-ticket review's fidelity gap: no per-ticket pass saw the tickets together, and two tickets on different files can still interact (a config default one changes, the code another consumes). Run the repo's review pair once each over the **whole branch** — `git diff origin/main...HEAD` (the branch was just rebased onto the fetched `origin/main`), the spec issue as the spec source — the same diff the PR's Codex gate will review. Fix findings like step 3.4 (the originating implementer for a ticket-local finding, a fresh implementer for a cross-ticket one or when the ticket's session is gone) and re-run the pass over the whole branch under the binding's re-review bound (`docs/agents/flow.md`).

Then run the repo's full verification loop (`AGENTS.md`, "Local development") on the branch: fmt check, clippy, tests, release build.

Assemble the dossier:

    # Spec #<n>: <title>
    Branch: <branch> — <commit count> commits, <diff stat> vs main

    ## Tickets
    - #<n> <title> — <commit shas> — review: clean | fixed N findings — criteria: all met | gaps

    Aggregate review (whole branch): clean | fixed N findings

    ## Verification
    <one line per command, with its result>

    ## Risks / gaps
    <what to look at first, or "none">

Done when every ticket appears, the aggregate review has a verdict or a skip, and every verification command has a result.

## 5. Stop for acceptance

Present the dossier and stop. Do not push. Continue only on the user's explicit go.

## 6. Ship

On the user's go:

- Push the branch — `git log --merges main..HEAD` must be empty first (rebase is the only merge method this repo allows).
- Open exactly one PR against `main`: a Conventional Commits title derived from the spec (`CONTRIBUTING.md`, "Pull request rules"), body with what the batch delivers, the per-ticket summary, the risks, and a close line listing **every ticket and the spec issue itself** — one keyword per issue (`Closes #a, closes #b, …, closes #<spec>`). The batch PR is the spec's delivery, so the spec closes with it; it stays open only when the drop list named a child the batch did not cover, and the body then names that child.
- Verify the PR is rebase-mergeable: `gh api repos/<owner>/<repo>/pulls/<n> --jq .rebaseable` must be `true`. A conflict with `main` can leave a merge-free branch `rebaseable: false`; if so, rebase onto `origin/main`, push, and check again.

Done when the PR exists, its branch is merge-commit-free, and `rebaseable` is `true`. Stop there — the user merges.

## Dispatch prompt

Every implementer dispatch carries:

- The ticket's number, title, and full body, fetched with `gh issue view <n>` and included verbatim.
- The batch context: branch name, this is the only ticket this session touches, and `AGENTS.md` / `CONTRIBUTING.md` hold the conventions.
- "Use `/implement`, but skip its closing review steps — the foreman runs the repo's review pair after you report (`docs/agents/flow.md`)."
- "Do not push, open PRs, close issues, or comment on the tracker."

Fix rounds resume the same `task_id` and carry the findings verbatim, plus: "Fix what is valid, say why for what isn't, rerun the affected tests, commit, and report again."

## Progress comments

After each ticket, post exactly one comment so an interrupted batch can be resumed, and carry the review outcome so the batch's bookkeeping survives a `/compact`:

    gh issue comment <ticket> --body "foreman: implemented in <sha>… on <branch> (batch spec #<spec>). review: clean | fixed N findings."

That comment is also the batch's sanctioned phase boundary: the ticket is self-contained and everything step 3 accumulated for it — both review reports and the fix exchange — is now disposable, so the main session may `/compact` here before the next dispatch, carrying the batch state forward (spec, branch, tickets done, next ticket). Never compact mid-ticket: a live findings thread is session-local.

## Pause and ask

Put the decision to the user when:

- no tickets resolve, or a blocker falls outside the batch;
- a ticket produces no commits;
- the implementer reports a blocker or a question;
- review findings survive two fix rounds;
- the rebase onto `main` conflicts, or full verification fails and the implementer cannot fix it;
- the working tree is dirty with work that is not a ticket's (Recovery covers a ticket's own uncommitted work).

## Guardrails

- Push and open the PR only after the user accepts at step 5.
- One ticket at a time, blockers first.
- Progress comments are the only writes the batch makes to the tracker; the PR closes the tickets.
- The implementer stays on the batch branch — `main` is never its checkout.
- The batch branch is merge-commit-free and rebase-mergeable: syncs and landings are rebases onto the integration tip, and no landing creates a merge commit — before the push, `git log --merges main..HEAD` must be empty, and once the PR exists `gh api repos/<owner>/<repo>/pulls/<n> --jq .rebaseable` must be `true` (rebase is the only merge method this repo allows; a merge commit or a conflict with `main` blocks the button).

## Recovery

Re-run `/foreman <spec>` after an interruption. The branch log and the progress comments are the batch's state; the session's captured `PRE` and implementer `task_id`s do not survive it, so rebuild them:

- **Branch**: if `<branch>` exists, verify it still has commits `origin/main` does not and that every one of them carries a `Refs:` trailer naming one of this spec's tickets; then switch to it. A branch failing either check stops and asks instead of being adopted.
- **Windows**: a ticket's window is the run of commits carrying its `Refs: #<n>` trailer, with `PRE` at the commit before the run. Rebuild both from the log.
- **Tickets**: one with a progress comment is done. One with commits but no comment gets its window reviewed, then its comment. One with neither is re-dispatched.
- **Interrupted fix round**: the `task_id` is gone; re-review the window (the findings resurface), then dispatch a fresh implementer with them.
- **Dirty tree**: never start a ticket on one. Inspect `git status` and the diff: work belonging to a ticket goes to a fresh implementer to finish and commit; anything else stops and asks.

## Setup

The implementer inherits this session's model and takes the user's permission rules, except where its own block sets them (`subagent` — V1's `task` — is denied). For an unattended batch, allow `git checkout`/`switch`, `git fetch`/`pull`/`rebase`, and `gh issue view`/`list`/`comment`/`api`; keep `git push` and `gh pr create` on ask, so acceptance is enforced twice.
