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

## Gate rehearsal

[cola] **Rehearse the CI gate before every push.** The gate reviews the whole diff read-only on each push (`.github/workflows/codex-review.yml`, `.github/codex/prompts/review.md`), so discovering a finding from CI pays a full round — the queue, the whole check set, and another whole-diff read. Run the gate's own prompt locally on the committed candidate, and push only after it PASSes (or after a recorded lane skip, which leaves the review to CI). The pair is still the review at every review point; the rehearsal adds the gate's harness on top and replaces neither half.

1. Pin the candidate. `HEAD` is the committed tree to push; `BASE=$(git merge-base origin/main HEAD)`. Never rehearse a dirty tree — commit the fix first, then rehearse.
2. Assemble the prompt outside the repo and run it — one shell, so the temp dir and its `trap` live for the run. Write the body you are about to open, verbatim, to a temp file and set `BODY_FILE` to it; once the PR exists, pass `PR=<n>` instead so the live body is used. The prompt stays verbatim (host-tree clause included), with one local-only header forbidding sub-agents:

   ```bash
   set -euo pipefail
   BASE=$(git merge-base origin/main HEAD)
   HEAD_SHA=$(git rev-parse HEAD)
   [ -z "$(git status --porcelain)" ] || { echo "rehearse a committed tree: the tree is dirty" >&2; exit 1; }
   mkdir -p /tmp/opencode
   WORK=$(mktemp -d /tmp/opencode/gate-rehearsal.XXXXXX)
   trap 'rm -rf "$WORK"' EXIT
   PROMPT="$WORK/prompt.md"; OUT="$WORK/review.md"
   TITLE="<PR title>"; BODY_FILE="<pr-body-file>"; PR="${PR:-}"
   if [ -n "$PR" ]; then
     TITLE=$(gh pr view "$PR" --json title --jq .title)
     BODY=$(gh pr view "$PR" --json body --jq .body)
   else
     BODY=$(cat "$BODY_FILE")
   fi
   [ -n "$BODY" ] || { echo "empty PR body file" >&2; exit 1; }
   # The workflow's collector (#555): body references first, then the commits'
   # trailers, deduplicated; the first eight are embedded, the rest named.
   refs=$( { printf '%s\n' "$BODY"; git log --format=%B "$BASE"..HEAD; } \
     | grep -oiE '(Closes|Fixes|Resolves|Refs|Part of):? *#[0-9]+' \
     | grep -oE '[0-9]+' | awk '!seen[$0]++' || true )
   [ -n "$refs" ] || { echo "no linked issue in the body or commits" >&2; exit 1; }
   linked=$(printf '%s\n' "$refs" | head -n 8)
   omitted=$(printf '%s\n' "$refs" | tail -n +9)
   {
     printf 'Local gate rehearsal. This is a single-session review: do not spawn or dispatch sub-agents; do not read or echo credentials or unrelated host files. Review only the scope below.\n\n'
     cat .github/codex/prompts/review.md
     printf '\n\n## PR metadata\n\nTitle: %s\n\n%s\n\n## Linked issues\n' "$TITLE" "$BODY"
     printf '%s\n' "$linked" | while IFS= read -r n; do
       [ -n "$n" ] || continue
       body=$(gh issue view "$n" --json number,title,body --jq '"### #" + (.number | tostring) + " " + .title + "\n\n" + (.body // "(empty)")')
       [ -n "$body" ] || { echo "issue #$n has no body" >&2; exit 1; }
       printf '\n%s\n' "$body"
     done
     if [ -n "$omitted" ]; then
       printf '\nAdditional references (bodies omitted): '
       printf '%s\n' "$omitted" | sed 's/^/#/' | tr '\n' ' '
       printf '\n'
     fi
     printf '\n\n## Rehearsal scope\n\nBASE=%s HEAD=%s\n\nRun `git diff %s...%s`; the tree is clean (tracked and untracked).\n' "$BASE" "$HEAD_SHA" "$BASE" "$HEAD_SHA"
   } > "$PROMPT"
   if [ -z "${GATE_MODEL:-}" ]; then
     if model=$(gh variable get GATE_MODEL 2>&1); then
       GATE_MODEL=$model
     elif [ "$model" = "variable GATE_MODEL was not found" ]; then
       GATE_MODEL=deepseek-flash
     else
       echo "cannot read GATE_MODEL: $model" >&2; exit 1
     fi
   fi
   [ -n "$GATE_MODEL" ] || { echo "GATE_MODEL is empty" >&2; exit 1; }
   if [ -z "${GATE_ENDPOINT:-}" ]; then
     if endpoint=$(gh variable get GATE_ENDPOINT 2>&1); then
       GATE_ENDPOINT=$endpoint
     elif [ "$endpoint" = "variable GATE_ENDPOINT was not found" ]; then
       GATE_ENDPOINT=""
     else
       echo "cannot read GATE_ENDPOINT: $endpoint" >&2; exit 1
     fi
   fi
   if [ -n "$GATE_ENDPOINT" ] && [ "$GATE_ENDPOINT" != "https://api.deepseek.com/responses" ]; then
     echo "the gate endpoint is $GATE_ENDPOINT; configure a matching profile or skip" >&2
     exit 1
   fi
   OWNER_TYPE=$(gh api repos/:owner/:repo --jq .owner.type)
   [ "$OWNER_TYPE" != "Organization" ] || { echo "org-scoped gate variables are not visible; configure a profile or skip" >&2; exit 1; }
   TIMEOUT=$(command -v timeout || command -v gtimeout || true)
   [ -n "$TIMEOUT" ] || { echo "GNU timeout (or gtimeout) is required" >&2; exit 1; }
   set +e
   "$TIMEOUT" --kill-after=30 2700 env -u GH_TOKEN -u GITHUB_TOKEN -u GH_ENTERPRISE_TOKEN -u GITHUB_ENTERPRISE_TOKEN \
     codex exec --sandbox read-only --ephemeral -C . \
       --disable multi_agent --disable goals \
       -c features.default_mode_request_user_input=false \
       -m "$GATE_MODEL" \
       -o "$OUT" - < "$PROMPT"
   codex_status=$?
   set -e
   [ -s "$OUT" ] || { echo "no review output (exit $codex_status)" >&2; exit 1; }
   cat "$OUT"; printf '\n'
   [ -z "$(git status --porcelain)" ] || { echo "the tree moved during the rehearsal" >&2; exit 1; }
   [ "$(git rev-parse HEAD)" = "$HEAD_SHA" ] || { echo "HEAD moved during the rehearsal" >&2; exit 1; }
   last=$(grep -v '^[[:space:]]*$' "$OUT" | tail -n 1 || true)
   verdict=$(printf '%s\n' "$last" | sed -nE 's/^[*`[:space:]]*CODEX_REVIEW_VERDICT: (PASS|FAIL)[*`[:space:]]*$/\1/p')
   [ "$verdict" = PASS ] || { echo "rehearsal did not PASS (verdict: ${verdict:-none})" >&2; exit 1; }
   ```

   The collector mirrors the workflow's: body references first, then the commits' `Refs:` trailers, deduplicated; the first eight are embedded and the rest named by number. The body is read once and fails closed, as does a failed or empty `gh issue view`, so Codex can never review a prompt that silently lost its spec. Once a PR exists, `PR=<n>` reads its live title and body; the body file is only for the pre-PR run. The dirty-tree check keeps `git diff BASE...HEAD` and the candidate the same tree; the endpoint check refuses a gate that moved off the default lane, and an org-owned repo fails closed because repository-scoped `gh variable get` cannot see org `vars.*`; the `HEAD` recheck rejects a candidate that moved during the run. A `gh` auth, network or permission failure is a failed read, not an unset variable — only the literal "was not found" takes the default. `mktemp` + `trap` + a GNU `timeout` (or `gtimeout`) with `--kill-after` bound and clean the run (the CI step's bound is 45 minutes). The output is read even when the run exits nonzero — a completed verdict after a timeout is still a verdict, the way the gate salvages one — and the verdict is parsed with the approval parser's strict form: the command exits nonzero unless the last non-blank line is exactly `CODEX_REVIEW_VERDICT: PASS` modulo emphasis, so a caller cannot push on a FAIL.
3. Read the verdict before pushing. Codex prints `model:`, `provider:` and `sandbox:` at startup — confirm they match the gate. The verdict is the **approval** parser's, not the publication parser's: the last non-blank line, with markdown emphasis stripped, must be exactly `CODEX_REVIEW_VERDICT: PASS`. Prose on that line, a missing marker, or a FAIL is a failed rehearsal. The `deepseek-flash` fallback mirrors the workflow's current default; when the workflow default moves, pass `GATE_MODEL` explicitly or update this fallback. Never commit the prompt or the output.

   The rehearsal is valid only on the gate's own lane. The command reads `GATE_MODEL` but not `GATE_ENDPOINT`; `codex exec` uses the local config's provider. If the repository's `vars.GATE_ENDPOINT`/`vars.GATE_MODEL` name another provider, select a matching profile with `codex exec -p <profile>` (the profile lives in `~/.codex/<profile>.config.toml`) or skip the rehearsal and let CI review — a PASS from a different lane is not a rehearsal. Record the skip.

   **Trusted input only.** The action keeps the key inside its Responses proxy and `GH_TOKEN` out of the Codex step; a local `codex exec` runs with the user's files and auth, and read-only blocks writes, not reads. Rehearse only the batch's own spec and tickets, never an external PR or issue body; keep the token variables unset as above, and do not read or echo `~/.codex/config.toml` or the gh config. Untrusted text belongs in CI.
4. A failed rehearsal is a review round: route each finding like the gate's (a ticket-local finding to the implementer that wrote that window; a cross-ticket one to a fresh implementer), fix **every** finding — including non-blocking — or answer it with a reason, re-run the pair and the rehearsal at the same scope, then push once.

[cola] **A passing rehearsal is not an approval.** The `main: review` approval comes only from the CI gate and is pinned to the commit it reviewed. A local PASS only says the push is worth spending a gate round on.

[cola] **One push per review round.** Batch the fixes a round raised, rehearse, then push once. Once the CI gate PASSes, freeze the branch: push again only for a real finding (yours or the user's) or a required rebase, and treat that push as a new round — rehearse it, push it, wait for its own PASS. Never merge on a stale PASS.

## Steps

1. Read the spec and its tickets; build the **task graph** from the native blocking edges and find the ready **frontier**. [cola] Reconcile the spec's open children before dispatching (`issue-tracker.md`, the gh conventions): each one is either in the batch or on a **drop list** with its reason — not `ready-for-agent`, or a blocker outside the batch — and a child in neither is a resolution bug, not a silent omission.
2. (optional) Dispatch an **exploration sub-agent** for whatever the tickets need, saving its notes **outside the repo** (a scratch directory every later sub-agent can read), so implementers focus on implementation rather than exploration.
3. Prepare the integration branch off `origin/main`: `<type>/<spec-slug>`. Do not open the PR yet — a branch with no commits ahead of `main` cannot.
4. Dispatch one `implementer` sub-agent per **frontier** ticket, each in its **own worktree under `.worktrees/`** on its own branch, based on the integration tip. Each implementer drives `/tdd` and, before reporting, **rebases onto the integration tip** (`git rebase <integration-tip>`) — never merges it in. Implementers do not review.
5. Land each completed ticket with a **`merger` sub-agent** (linear, per the [cola] landing rule). When a landing unblocks new tickets, dispatch more implementers — that is the concurrency. [cola] A tightly coupled spec whose tickets keep touching the same files runs **serially**: keep the frontier at one ticket at a time, so each landing is reviewed on a settled tip instead of trading conflicts.
6. Review with `/code-review` at each landing over that ticket's window, and again over the **whole branch** (`origin/main...HEAD`) once every ticket is in. A finding goes back to **the implementer that wrote that ticket** — resume its session: it holds the context, so the fix is one round rather than a re-brief. Only a finding the ticket cannot own (a cross-ticket interaction, or a session already gone) goes to a fresh implementer. Re-review at the scope that raised it, per `docs/agents/flow.md` — a same-scope fix round resumes the reviewer sessions that raised the finding (keep their ids from that round, so it re-reads the delta rather than the whole diff), while the whole-branch pass is a new scope and starts fresh reviewers. [cola] Once a ticket's window review is clean or its findings are fixed, post exactly one progress comment on the ticket — `gh issue comment <n> --body "implement-spec: landed <sha> on <branch> (batch spec #<spec>). review: clean | fixed N findings."`, where `<sha>` is the ticket's last commit (the window tip, which Resume matches against) — and when a later fix extends that window, update the same comment (`gh api --method PATCH repos/<owner>/<repo>/issues/comments/<id> -f body="..."`) rather than adding a second. That way an interrupted batch can be resumed from the tracker. That comment is also the batch's sanctioned `/compact` boundary: the ticket is self-contained and everything step 6 accumulated for it is now disposable. Never compact mid-ticket: a live findings thread is session-local. A ticket-window fix is local and does not push; the whole-branch rehearsal before the first push (step 8) covers every landed ticket. If an interrupted batch resumes and pushes a ticket fix, rehearse the adopted tip first.
7. `git fetch` and rebase the integration branch onto the fetched `origin/main` — resolving a conflict by intent (a genuine conflict pauses the batch) — then run the repo's full verification loop (`CONTRIBUTING.md`, "Verification loop") on the **resulting** tree: the one that ships, not the pre-rebase one.
8. [cola] Rehearse the whole branch ("Gate rehearsal"), then push — unless a lane skip was recorded. Open exactly **one** PR against `main` whose body carries the dossier and closes **every ticket and the spec** (one keyword per number), per `CONTRIBUTING.md` "Pull request rules" — [cola] the spec stays open when the drop list named a child the batch did not cover, and the body names that child instead. Then verify the branch is merge-commit-free (`git log --merges main..HEAD` empty) and rebase-mergeable (`gh api repos/<owner>/<repo>/pulls/<n> --jq .rebaseable` is `true`) — that read needs the PR's number to exist, and a conflict with `main` leaves it `false`, so rebase onto `origin/main`, re-run the checks the rebase touched, rehearse the rebased tip again, and push again.
9. [cola] Drive the PR to **green**: `gh pr checks <branch> --required` plus the Codex gate. A red check or a FAIL is a fix round (route it like step 6): fix **all** of it, re-run the rehearsal at the same scope, and push once — unless a lane skip was recorded, which leaves the review to CI. Once the required set and the gate are green, freeze the branch — push again only for a real finding (yours or the user's) or a required rebase, and wait for that round's PASS. Then clean up the implementer worktrees.
10. Stop at the green PR — the user verifies and merges.

## Guardrails

- The orchestrating session calls the reviewers (`implementer` and `merger` deny `subagent`); the merger only lands.
- One PR per spec. Never merge.
- [cola] One push per review round: batch the findings, rehearse, push once; a gate PASS covers only the commit it reviewed, and a recorded lane skip defers the review to CI.
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
