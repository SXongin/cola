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

[cola] **Landing is linear.** This repo merges a PR by **rebase only**, so the integration branch must never gain a merge commit. Every landing is a rebase of the ticket branch onto the integration tip followed by `git merge --ff-only`, never `git merge` of the ticket branch. The branch stays merge-commit-free (`git log --merges main..HEAD` empty) and, once the PR exists, rebase-mergeable (`gh api repos/<owner>/<repo>/pulls/<n> --jq .rebaseable` is `true`) — step 8 sequences those two checks. The `merger` sub-agent (`AGENTS.md`) owns landings: it lands, it never judges.

[cola] **Review is the pair, run at every landing.** `/code-review` carries both the two axes and the adversarial pass; *when* it runs, *who* calls it, how findings route, and the re-review bound are the binding in `docs/agents/flow.md`.

[cola] **Run to a green PR, then stop.** This is `CONTRIBUTING.md`'s loop: push, open the PR, and drive it to a green required-check set and a passing Codex gate. Stop there — the user verifies and merges; agents never merge.

## Steps

1. Read the spec and its tickets; build the **task graph** from the native blocking edges and find the ready **frontier**.
2. (optional) Dispatch an **exploration sub-agent** for whatever the tickets need, saving its notes **outside the repo** (a scratch directory every later sub-agent can read), so implementers focus on implementation rather than exploration.
3. Prepare the integration branch off `origin/main`: `<type>/<spec-slug>`. Do not open the PR yet — a branch with no commits ahead of `main` cannot.
4. Dispatch one `implementer` sub-agent per **frontier** ticket, each in its **own worktree on its own branch**, based on the integration tip. Each implementer drives `/tdd` and, before reporting, **rebases onto the integration tip** (`git rebase <integration-tip>`) — never merges it in. Implementers do not review.
5. Land each completed ticket with a **`merger` sub-agent** (linear, per the [cola] landing rule). When a landing unblocks new tickets, dispatch more implementers — that is the concurrency.
6. Review with `/code-review` at each landing over that ticket's window, and again over the **whole branch** (`origin/main...HEAD`) once every ticket is in. A finding goes back to **the implementer that wrote that ticket** — resume its session: it holds the context, so the fix is one round rather than a re-brief. Only a finding the ticket cannot own (a cross-ticket interaction, or a session already gone) goes to a fresh implementer. Re-review at the scope that raised it, per `docs/agents/flow.md`.
7. Run the repo's full verification loop (`AGENTS.md`, "Local development") on the branch.
8. `git fetch`, rebase the integration branch onto the fetched `origin/main`, and push. Open exactly **one** PR against `main` whose body carries the dossier and closes **every ticket and the spec** (one keyword per number), per `CONTRIBUTING.md` "Pull request rules". Then verify the branch is merge-commit-free (`git log --merges main..HEAD` empty) and rebase-mergeable (`gh api repos/<owner>/<repo>/pulls/<n> --jq .rebaseable` is `true`) — that read needs the PR's number to exist, and a conflict with `main` leaves it `false`, so rebase onto `origin/main` and push again.
9. Drive the PR to **green**: `gh pr checks <branch> --required` plus the Codex gate. A red check or a FAIL is a fix round (route it like step 6) and a re-push, under the binding's bound. Then clean up the implementer worktrees.
10. Stop at the green PR — the user verifies and merges.

## Guardrails

- The orchestrating session calls the reviewers (`implementer` and `merger` deny `subagent`); the merger only lands.
- One PR per spec. Never merge.
- The integration branch stays merge-commit-free and rebase-mergeable.
