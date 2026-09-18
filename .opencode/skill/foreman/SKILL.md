---
name: foreman
description: Run one spec's tickets as a serial batch — resolve them via ## Parent, order by native blocking edges, implement each with an `implementer` sub-agent, code-review each ticket's commit window, then stop for one spec-level acceptance before a single PR. Use ONLY when the user explicitly asks to run a spec's tickets (e.g. "/foreman 123"); never start a batch on your own.
---

# Foreman

Run a spec's tickets as one serial batch on one branch, ending in one PR. The spec is the unit of acceptance: the user reviews once, at the end, instead of once per ticket.

The main session is the foreman — it resolves the batch, prepares the branch, dispatches one `implementer` sub-agent per ticket, reviews each ticket's commit window, and stops before the PR. Sub-agents implement; the foreman schedules, reviews, and reports.

## 1. Resolve the batch

Input: a spec issue number, plus optional explicit ticket numbers that override the scan.

Use the gh conventions in `docs/agents/issue-tracker.md`:

- List open `ready-for-agent` issues, and keep the bodies whose `## Parent` references the spec issue (`issue-tracker.md`, "Specs and tickets"). Explicit numbers from the user are used verbatim; a scan that finds nothing falls back to asking for numbers.
- Order the set blockers-first from the native `blocked_by` edges (`issue-tracker.md`); tracker order breaks ties.
- A ticket whose blockers fall outside the batch is not runnable — leave it out and report it.
- Derive the branch name `<type>/<spec-slug>` from the spec's title, e.g. `feat/lazy-session-creation`.

Present the batch: each ticket's number, title, and blockers; the resulting order; the branch name; anything dropped. Ask the user to confirm. Done when the user approves the list, the order, and the branch.

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
5. **Record progress** — post the progress comment below.

Done when the ticket's commits are on the branch, its window review is clean or fixed, and the progress comment is posted. Then take the next ticket; never two at once.

## 4. Verify and build the dossier

Fetch and, if `origin/main` advanced, rebase the branch onto it — a conflict pauses the batch. Then run the repo's full verification loop (`AGENTS.md`, "Local development") on the branch: fmt check, clippy, tests, release build.

Assemble the dossier:

    # Spec #<n>: <title>
    Branch: <branch> — <commit count> commits, <diff stat> vs main

    ## Tickets
    - #<n> <title> — <commit shas> — review: clean | fixed N findings — criteria: all met | gaps

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

After each ticket, post exactly one comment so an interrupted batch can be resumed:

    gh issue comment <ticket> --body "foreman: implemented in <sha>… on <branch> (batch spec #<spec>)."

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

Re-run `/foreman <spec>` after an interruption: if the branch exists, switch to it instead of recreating it. A ticket with a progress comment is done; a ticket with `Refs: #<n>` commits but no comment gets its window reviewed, then its comment. Everything else comes from the branch log and the comments — no hidden state.

## Setup

The implementer inherits this session's model and takes the user's permission rules, except where its own block sets them (`task` and `todowrite` are denied). For an unattended batch, allow `git checkout`/`switch`, `git fetch`/`pull`/`rebase`, and `gh issue view`/`list`/`comment`/`api`; keep `git push` and `gh pr create` on ask, so acceptance is enforced twice.
