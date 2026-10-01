# Skill Flow

The route work travels through the skills in this repo. `/ask-matt` is the full map; this file is cola's binding of it — the order, the branch decisions, and where each path lands locally.

## Main flow: idea → ship

Keep steps 1–3 in one unbroken context window, so the grilling, spec, and tickets build on the same thinking. A multi-session build then runs under `/foreman`: implementation stays out of the orchestrating window (one implementer sub-agent per ticket), while the per-ticket review reports and fix rounds do accumulate there — so each ticket's progress comment is a sanctioned `/compact` boundary, and `/clear` between tickets is needed only when implementing them by hand. If the window approaches the smart zone before tickets exist, `/compact` at the nearest phase boundary rather than pushing on.

1. **`/grill-with-docs`** sharpens the idea by interview and writes what it learns into `GLOSSARY.md` and `docs/adr/` as it goes (`domain.md`). Done when every question is settled, or routed to step 2.
2. **Does a question need a runnable answer?** — a state model, business logic, a UI you have to see. Take the prototype detour:
   - `/handoff` out, open a fresh session on the file,
   - `/prototype` answers the question in throwaway code,
   - `/handoff` back; reference the answer from the idea thread.

   Done when the one design question is answered.
3. **Is this a multi-session build?**
   - **Yes** → `/to-spec` turns the thread into a spec, then `/to-tickets` cuts it into tracer-bullet GitHub issues with native blocking edges (`issue-tracker.md`). Pass `/to-tickets` the spec's issue number so every ticket's `## Parent` points back at it. Then **`/foreman <spec-number>`** works the batch: one `implementer` sub-agent per ticket, blockers first, on one branch, reviewed per ticket, and stopped for one spec-level acceptance before a single PR. Implementing the issues by hand with `/implement` stays the fallback.
   - **No** → `/implement` right here, in this context window.
4. **`/implement` drives `/tdd`** — one red-green slice at a time — and closes with **`/code-review`**, the two-axis Standards + Spec review, before committing. Done when both axes are clean or their findings are fixed.

All of it runs under the `CONTRIBUTING.md` branch loop: start on `main`, branch, work, rebase, PR (`AGENTS.md` holds the verification loop).

## PR body

`/pr` is model-invoked whenever a PR body is written. It layers on the repo's own template (`.github/pull_request_template.md`), never replaces it: keep `## What` / `## Why` / `## How tested` and the `CONTRIBUTING.md` rules (a Conventional Commits title, one `Closes #N` per issue, an ADR for a one-way door), and take the skill's rendering:

- `## What` opens with the smallest visual — a diff sketch, tree, sequence diagram, or pseudocode — then the paragraph.
- `## How tested` reads as **Before:** → **After:** — failing → passing test, old log → new log, card screenshots for visual changes — ahead of the verification-loop commands.
- Add `## Merge Danger` only when the change is non-trivial: **Door** (two-way, or one-way naming what reversal would cost) and **Blast Radius**. A one-way door is where an ADR is due.

## On-ramps

A starting situation that generates work, then merges onto the main flow.

- **Raw issues piling up** → **`/triage`**. For issues you didn't create — bug reports and incoming requests, never the tickets `/to-tickets` already produced. It applies the triage labels that make an issue agent-ready (`triage-labels.md`), and `/implement` picks it up from there.
- **Something's broken** → **`/diagnosing-bugs`**. For the bug that resists a first glance: it refuses to theorise until one command already goes red on *this* bug, then fixes with a regression test. A post-mortem that finds no seam to lock the bug down hands off to `/improve-codebase-architecture`.
- **A huge, foggy effort** → **`/wayfinder`**. When the way from here to the destination isn't visible, it charts a shared map of decision tickets and resolves them until the fog lifts, then hands off to `/to-spec`, which collapses the map into a buildable plan before `/to-tickets`. Save it for work too big for one session; a well-scoped feature goes straight to step 3.

## Upkeep

- **`/improve-codebase-architecture`** — when there's a spare moment, it surfaces deepening opportunities; the one you pick becomes an idea for `/grill-with-docs`.

Standalone skills live in `/ask-matt`, along with the five phase-boundary options: **Continue**, `/clear`, `/handoff`, **subagent**, `/compact`.
