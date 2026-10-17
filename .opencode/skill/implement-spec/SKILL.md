---
name: implement-spec
description: "Implement the result of /to-spec and /to-tickets in code — one integration branch, every ticket landed linearly (rebase, never a merge commit) by the merger sub-agent, reviewed by the repo's paired pass, ending in a single rebase-mergeable PR."
disable-model-invocation: true
---

# Implement Spec (cola)

Cola's adaptation of the upstream `implement-spec` skill (`mattpocock/skills`, `skills/engineering/implement-spec/SKILL.md`). The step structure follows upstream so the two stay diffable; every **[cola]** divergence from it is marked.

You have been given a spec with tickets. The goal is the whole spec implemented on one **integration branch**, with every ticket **landed linearly**, reviewed by the repo's **paired** pass, ending in a single PR.

Communicate with sub-agents through sparse **context pointers** — the spec, tickets, commits, and notes on disk — not restated prose.

[cola] **Landing is linear.** This repo merges a PR by **rebase only**, so the integration branch must never gain a merge commit. Every landing is a rebase of the ticket branch onto the integration tip followed by `git merge --ff-only`, never `git merge` of the ticket branch. The branch stays merge-commit-free (`git log --merges main..HEAD` empty) and, once the PR exists, rebase-mergeable (`gh api repos/<owner>/<repo>/pulls/<n> --jq .rebaseable` is `true`) — step 8 sequences those two checks. The `merger` sub-agent (`AGENTS.md`) owns landings: it lands, it never judges.

[cola] **Review is the pair, run at every landing.** `/code-review` carries both the two axes and the adversarial pass; *when* it runs, *who* calls it, how findings route, and the re-review bound are the binding in `docs/agents/flow.md`.

[cola] **Stop for acceptance before the PR.** The acceptance gate is `docs/agents/flow.md` (it is stricter than `CONTRIBUTING.md`'s generic per-task loop, which pushes and opens the PR first): do **not** push until the user's explicit go. Agents never merge.

## Steps

1. Read the spec and its tickets; build the **task graph** from the native blocking edges and find the ready **frontier**.
2. (optional) Dispatch an **exploration sub-agent** for whatever the tickets need, saving its notes **outside the repo** (a scratch directory every later sub-agent can read), so implementers focus on implementation rather than exploration.
3. Prepare the integration branch off `origin/main`: `<type>/<spec-slug>`. Do **not** open a PR yet — a branch with no commits ahead of `main` cannot, and [cola] pushes only on the user's go.
4. Dispatch one `implementer` sub-agent per **frontier** ticket, each in its **own worktree on its own branch**, based on the integration tip. Each implementer drives `/tdd` and, before reporting, **rebases onto the integration tip** (`git rebase <integration-tip>`) — never merges it in. Implementers do not review.
5. Land each completed ticket with a **`merger` sub-agent** (linear, per the [cola] landing rule). When a landing unblocks new tickets, dispatch more implementers — that is the concurrency.
6. Review with `/code-review` at each landing over that ticket's window, and again over the **whole branch** (`origin/main...HEAD`) once every ticket is in. Findings route and re-review per `docs/agents/flow.md` — ticket-local to the originating implementer, cross-ticket to a fresh one.
7. Run the repo's full verification loop (`AGENTS.md`, "Local development") on the branch.
8. Present the dossier and **stop for acceptance**. On the user's go: `git fetch`, rebase the integration branch onto the fetched `origin/main`, and push. Then open exactly **one** PR whose body closes **every ticket and the spec** (one keyword per number) and follows `CONTRIBUTING.md` "Pull request rules" — and only then verify `git log --merges main..HEAD` is empty and `gh api repos/<owner>/<repo>/pulls/<n> --jq .rebaseable` is `true`; the `--jq` needs the PR's number to exist, and a conflict with `main` leaves it `false`, so rebase onto `origin/main` and push again. Stop — the user merges.
9. Clean up the implementer worktrees.

## Guardrails

- The orchestrating session calls the reviewers (`implementer` and `merger` deny `subagent`); the merger only lands.
- One PR per spec. Never push or merge before the user's go.
- The integration branch stays merge-commit-free and rebase-mergeable.
