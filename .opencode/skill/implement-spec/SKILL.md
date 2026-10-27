---
name: implement-spec
description: "Implement the result of /to-spec and /to-tickets in code — one integration branch, every ticket landed linearly (rebase, never a merge commit) by the merger sub-agent, reviewed by the repo's paired pass, ending in a single rebase-mergeable PR."
disable-model-invocation: true
metadata:
  upstream: "mattpocock/skills (skills/engineering/implement-spec/SKILL.md)"
---

# Implement Spec (cola)

Cola's adaptation of the upstream `implement-spec` skill (`mattpocock/skills`, `skills/engineering/implement-spec/SKILL.md`). The step structure follows upstream so the two stay diffable; every **[cola]** divergence from it is marked.

You have been given a spec with tickets. The goal is the whole spec implemented on one **integration branch**, with every ticket **landed linearly**, reviewed by the repo's **paired** pass, ending in a single PR.

Communicate with sub-agents through sparse **context pointers** — the spec, tickets, commits, and notes on disk — not restated prose.

[cola] **Landing is linear.** This repo merges a PR by **rebase only**, so the integration branch must never gain a merge commit. Every landing is a rebase of the ticket branch onto the integration tip followed by `git merge --ff-only`, never `git merge` of the ticket branch. The branch stays merge-commit-free (`git log --merges main..HEAD` empty) and, once the PR exists, rebase-mergeable (`gh api repos/<owner>/<repo>/pulls/<n> --jq .rebaseable` is `true`) — step 8 sequences those two checks. The `merger` sub-agent (`.opencode/agent/merger.md`) owns landings: it lands, it never judges.

[cola] **Review is the pair, run at every landing.** `/code-review` carries both the two axes and the adversarial pass; *when* it runs, *who* calls it, and how findings route are the binding in `docs/agents/flow.md`.

[cola] **Run to a green PR, then stop.** This is `CONTRIBUTING.md`'s loop: push, open the PR, and drive it to a green required-check set and a passing Codex gate. Stop there — the user verifies and merges; agents never merge.

## Steps

1. Read the spec and its tickets; build the **task graph** from the native blocking edges and find the ready **frontier**. [cola] Reconcile the spec's open children before dispatching (`issue-tracker.md`, the gh conventions): each one is either in the batch or on a **drop list** with its reason — not `ready-for-agent`, or a blocker outside the batch — and a child in neither is a resolution bug, not a silent omission.
2. (optional) Dispatch an **exploration sub-agent** for whatever the tickets need, saving its notes **outside the repo** (a scratch directory every later sub-agent can read), so implementers focus on implementation rather than exploration.
3. Prepare the integration branch off `origin/main`: `<type>/<spec-slug>`. Do not open the PR yet — a branch with no commits ahead of `main` cannot.
4. Dispatch one `implementer` sub-agent per **frontier** ticket, each in its **own worktree under `.worktrees/`** on its own branch, based on the integration tip. Each implementer drives `/tdd` and, before reporting, **rebases onto the integration tip** (`git rebase <integration-tip>`) — never merges it in. Implementers do not review.
5. Land each completed ticket with a **`merger` sub-agent** (linear, per the [cola] landing rule). When a landing unblocks new tickets, dispatch more implementers — that is the concurrency. [cola] A tightly coupled spec whose tickets keep touching the same files runs **serially**: keep the frontier at one ticket at a time, so each landing is reviewed on a settled tip instead of trading conflicts.
6. Review with `/code-review` at each landing over that ticket's window, and again over the **whole branch** (`origin/main...HEAD`) once every ticket is in. A finding goes back to **the implementer that wrote that ticket** — resume its session: it holds the context, so the fix is one round rather than a re-brief. Only a finding the ticket cannot own (a cross-ticket interaction, or a session already gone) goes to a fresh implementer. Re-review at the scope that raised it, per `docs/agents/flow.md` — a same-scope fix round resumes the reviewer sessions that raised the finding (keep their ids from that round, so it re-reads the delta rather than the whole diff), while the whole-branch pass is a new scope and starts fresh reviewers. [cola] Once a ticket's window review is clean or its findings are fixed, post exactly one progress comment on the ticket — `gh issue comment <n> --body "implement-spec: landed <sha> on <branch> (batch spec #<spec>). review: clean | fixed N findings."`, where `<sha>` is the ticket's last commit (the window tip, which Resume matches against) — and when a later fix extends that window, update the same comment (`gh api --method PATCH repos/<owner>/<repo>/issues/comments/<id> -f body="..."`) rather than adding a second. That way an interrupted batch can be resumed from the tracker. That comment is also the batch's sanctioned `/compact` boundary: the ticket is self-contained and everything step 6 accumulated for it is now disposable. Never compact mid-ticket: a live findings thread is session-local.
7. `git fetch` and rebase the integration branch onto the fetched `origin/main` — resolving a conflict by intent (a genuine conflict pauses the batch) — then run the repo's full verification loop (`CONTRIBUTING.md`, "Verification loop") on the **resulting** tree: the one that ships, not the pre-rebase one.
8. Push. Open exactly **one** PR against `main` whose body carries the dossier and closes **every ticket and the spec** (one keyword per number), per `CONTRIBUTING.md` "Pull request rules" — [cola] the spec stays open when the drop list named a child the batch did not cover, and the body names that child instead. Then verify the branch is merge-commit-free (`git log --merges main..HEAD` empty) and rebase-mergeable (`gh api repos/<owner>/<repo>/pulls/<n> --jq .rebaseable` is `true`) — that read needs the PR's number to exist, and a conflict with `main` leaves it `false`, so rebase onto `origin/main`, re-run the checks the rebase touched, and push again.
9. Drive the PR to **green**: `gh pr checks <branch> --required` plus the Codex gate. A red check or a FAIL is a fix round (route it like step 6) and a re-push. Then clean up the implementer worktrees.
10. Stop at the green PR — the user verifies and merges.

## Guardrails

- The orchestrating session calls the reviewers (`implementer` and `merger` deny `subagent`); the merger only lands.
- One PR per spec. Never merge.
- [cola] The progress comments are the only writes the batch makes to the tracker; the PR closes the tickets.
- The integration branch stays merge-commit-free and rebase-mergeable.

## Resume

[cola] Re-run `/implement-spec <spec>` after an interruption. The integration branch's log and the tickets' progress comments are the batch's state; the session's captured ticket sessions do not survive it:

- **Adopt the branch**: resume only a branch whose every commit `origin/main` lacks carries a `Refs: #<n>` trailer naming one of this spec's tickets — a commit without one is not the batch's, and adopting the branch would ride it into the PR. A branch failing that check stops and asks instead of being resumed.
- **Windows**: a ticket's window is the commits carrying its `Refs: #<n>` trailer, with the base at the commit before the first of them — normally one contiguous run, because landings are linear.
- **Tickets**: a ticket is done only when its progress comment names this branch, names a commit still on the integration tip (`git merge-base --is-ancestor <sha> HEAD`), and that commit is the **last** one carrying the ticket's trailer — only then does the comment's review cover the whole window. A trailer-bearing commit *after* the comment is unreviewed work: review the extended window and replace the ticket's comment (never a second one), never re-dispatch over landed commits. A ticket with trailer commits but no comment covering them is landed but unreviewed: review its window and post the comment, not a new implementer. A ticket with neither is dispatched.
- **Interleaved**: when another ticket's commits sit between this ticket's (a fix that landed after a later ticket), no single run defines its diff — leave that ticket to the whole-branch pass rather than reconstructing a partial window, and name it in the dossier.
- **Interrupted fix round**: the sessions are gone — re-review the window with fresh reviewers (the findings resurface), then dispatch a fresh implementer with them.
- **Dirty tree or worktrees**: never start a ticket on a dirty tree, and never reuse a stale worktree — inspect and clean up before dispatching.
