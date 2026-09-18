---
name: foreman
description: Run one spec's tickets as a serial batch — resolve them via ## Parent, order by native blocking edges, implement each with an `implementer` sub-agent, code-review each ticket's commit window, then stop for one spec-level acceptance before a single PR. Use ONLY when the user explicitly asks to run a spec's tickets (e.g. "/foreman 123"); never start a batch on your own.
---

# Foreman

Run a spec's tickets as one serial batch on one branch, ending in one PR. The spec is the unit of acceptance: the user reviews once, at the end, instead of once per ticket.

The main session is the foreman — it resolves the batch, prepares the branch, dispatches one `implementer` sub-agent per ticket, reviews each ticket's commit window, and stops before the PR. Sub-agents implement; the foreman schedules, reviews, and reports.

## 1. Resolve the batch

Input: a spec issue number, plus optional explicit ticket numbers that override the scan.

Use the gh conventions in `docs/agents/issue-tracker.md`, and paginate — `gh issue list` returns 30 rows by default and `--json` does not auto-paginate:

- Fetch the spec's open children — every open issue whose `## Parent` references the spec issue, whatever its label. Explicit numbers from the user are used verbatim; a scan that finds nothing falls back to asking for numbers.
- The batch is the `ready-for-agent` children, ordered blockers-first from the native `blocked_by` edges (`issue-tracker.md`); tracker order breaks ties.
- Reconcile before presenting: every child outside the batch goes on the drop list with its reason — not `ready-for-agent`, or a blocker outside the batch. A child that appears in neither the batch nor the drop list is a resolution bug, not a silent omission.
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
3. **Review the window** — load the `/code-review` skill and run both axes over `PRE...HEAD` with the ticket as the spec source. The review sub-agents dispatch from this main session; a sub-agent cannot spawn them.
4. **Fix findings** — resume the same implementer session (`task_id`) with the findings, then re-review. Stop and ask after two fix rounds.
5. **Record progress** — post the progress comment below. It carries the review outcome and marks the batch's sanctioned `/compact` boundary; never compact mid-ticket.

Done when the ticket's commits are on the branch, its window review is clean or fixed, and the progress comment is posted. Then take the next ticket; never two at once.

## 4. Verify and build the dossier

Fetch and, if `origin/main` advanced, rebase the branch onto it — a conflict pauses the batch.

Then close the per-ticket review's fidelity gap. A later ticket can edit a file an earlier ticket's window already passed, so the earlier ticket's reviewed state is not its shipped state, and no per-ticket pass examined the two together. Collect the files touched by two or more tickets (group `main..HEAD` commits by their `Refs: #<n>` trailer and intersect the per-ticket file sets) and, when any exist, run `/code-review` once with `main` as the fixed point, the diff scoped to that shared set (`git diff main...HEAD -- <paths>`), and the spec issue as the spec source. A file touched by a single ticket was reviewed exactly as it ships — skip it; no shared files at all — skip the pass. Fix findings like step 3.4 and re-run the pass on the changed shared files until clean or two rounds have passed.

Then run the repo's full verification loop (`AGENTS.md`, "Local development") on the branch: fmt check, clippy, tests, release build.

Assemble the dossier:

    # Spec #<n>: <title>
    Branch: <branch> — <commit count> commits, <diff stat> vs main

    ## Tickets
    - #<n> <title> — <commit shas> — review: clean | fixed N findings — criteria: all met | gaps

    Aggregate review (shared files): clean | fixed N findings | skipped, no shared files

    ## Verification
    <one line per command, with its result>

    ## Risks / gaps
    <what to look at first, or "none">

Done when every ticket appears and every verification command has a result.

## 5. Stop for acceptance

Present the dossier and stop. Do not push. Continue only on the user's explicit go.

## 6. Ship

On the user's go:

- Push the branch.
- Open exactly one PR against `main`: a Conventional Commits title derived from the spec (`CONTRIBUTING.md`, "Pull request rules"), body with what the batch delivers, the per-ticket summary, the risks, and a `Closes #a, #b, …` line listing every ticket.

Done when the PR exists. Stop there — the user merges.

## Dispatch prompt

Every implementer dispatch carries:

- The ticket's number, title, and full body, fetched with `gh issue view <n>` and included verbatim.
- The batch context: branch name, this is the only ticket this session touches, and `AGENTS.md` / `CONTRIBUTING.md` hold the conventions.
- "Use `/implement`, but skip its closing `/code-review` step — the foreman reviews after you report."
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
- the working tree is dirty.

## Guardrails

- Push and open the PR only after the user accepts at step 5.
- One ticket at a time, blockers first.
- Progress comments are the only writes the batch makes to the tracker; the PR closes the tickets.
- The implementer stays on the batch branch — `main` is never its checkout.

## Recovery

Re-run `/foreman <spec>` after an interruption. The branch log and the progress comments are the batch's state; the session's captured `PRE` and implementer `task_id`s do not survive it, so rebuild them:

- **Branch**: if `<branch>` exists, verify it is this spec's — its commits carry `Refs:` trailers for the spec's tickets — and that it still holds commits `main` does not; then switch to it. A branch that fails either check stops and asks instead of being adopted.
- **Windows**: a ticket's window is the run of commits carrying its `Refs: #<n>` trailer, with `PRE` at the commit before the run. Rebuild both from the log.
- **Tickets**: one with a progress comment is done. One with commits but no comment gets its window reviewed, then its comment. One with neither is re-dispatched.
- **Interrupted fix round**: the `task_id` is gone; re-review the window (the findings resurface), then dispatch a fresh implementer with them.
- **Dirty tree**: never start a ticket on one. Inspect `git status` and the diff: work belonging to a ticket goes to a fresh implementer to finish and commit; anything else stops and asks.

## Setup

The implementer inherits this session's model and takes the user's permission rules, except where its own block sets them (`task` and `todowrite` are denied). For an unattended batch, allow `git checkout`/`switch`, `git fetch`/`pull`/`rebase`, and `gh issue view`/`list`/`comment`/`api`; keep `git push` and `gh pr create` on ask, so acceptance is enforced twice.
