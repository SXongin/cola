# Turn footer: work context captured at turn start, rendered in two stages

The card footer used to appear only on the final card as `📁 full-path · 🤖 provider/model · 📊 上下文 N%` — nothing told the user which git state the AI was about to operate on. The footer now shows a **work-context line**: project basename, git branch, and a dirty marker (⚠). It is captured **before the prompt is sent** — the state the AI started from, not the working tree it leaves behind — and renders from the very first loading card, so a user can spot "AI is about to work on the wrong branch" before the turn completes. The model and context-window ratio are end-of-turn facts, so the footer renders in two stages: `📁 project · branch ⚠` on every card, appended with `· 🤖 model · 📊 上下文 N%` only on the final card.

Non-git directories degrade to just `📁 project`; a detached HEAD falls back to the short commit hash; branches are never truncated. "Dirty" means `git status --porcelain` is non-empty, including untracked files.

## Considered options

- **Capture at turn end**: one git call alongside the existing context-ratio fetch, but the dirty marker would always be lit — the AI's own edits make the tree dirty, so ⚠ carries no signal. Rejected.
- **Footer end-only (status quo)**: minimal change, but the branch — the whole point of the work-context line — would only be visible after the turn completes, too late to catch a wrong-branch run. Rejected.
- **Dirty = tracked changes only**: quieter, but untracked source files are exactly the local state an AI works on top of, and ignored files never appear in porcelain output anyway. Chosen: untracked included.

## Consequences

- Two places create the turn accumulator (handler.rs and external.rs); both must capture branch/dirty at the same point — before the prompt is sent.
- The footer builder splits into an always-visible part (directory/branch/dirty) and a final-card-only part (model/context ratio); the token-cost fallback path becomes unreachable and is removed.
- One git subprocess pair per turn (`rev-parse` + `status`), strictly best-effort: any failure silently omits the branch/dirty halves.