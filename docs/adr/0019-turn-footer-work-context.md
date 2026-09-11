# Turn footer: work context captured at turn start and refreshed at turn end

The card footer used to appear only on the final card as `📁 full-path · 🤖 provider/model · 📊 上下文 N%` — nothing told the user which git state the AI was about to operate on. The footer now shows a **work-context line**: project basename, git branch, and a dirty marker (⚠). It is captured **before the prompt is sent** — the state the AI started from — and renders from the very first loading card, so a user can spot "AI is about to work on the wrong branch" before the turn completes. When the turn ends, the git halves are **re-read and refreshed before the final flush**, so the completed card shows where the turn landed — a branch the AI created or switched to, and whether it left uncommitted changes. The model and context-window ratio are end-of-turn facts, so the footer renders in two stages: `📁 project · branch ⚠` on every card, appended with `· 🤖 model · 📊 上下文 N%` only on the final card.

The ⚠ thus carries two readings by card state: while the turn runs it means the tree had uncommitted work **before the prompt** (what the AI builds on); on the completed card it means the AI **left** uncommitted work (it did not commit). Neither reading is guaranteed: a read-only turn leaves the tree untouched, and an AI that commits (even pushes) ends clean.

Non-git directories degrade to just `📁 project`; a detached HEAD falls back to the short commit hash; branches are never truncated. "Dirty" means `git status --porcelain` is non-empty, including untracked files.

## Considered options

- **Real-time refresh (re-read on every card flush)**: the branch would track a mid-turn switch immediately. Rejected — the streaming poll cadence runs it every ~3 s per active session, the `status` scan scales with repository size, and each read takes git's index lock to refresh the stat cache while the AI is running its own git commands in the same worktree. Two reads per turn capture the moments that matter.
- **Capture at turn end only**: one git call alongside the existing context-ratio fetch, but the branch — the whole point of the work-context line — would only be visible after the turn completes, too late to catch a wrong-branch run. (The original rejection argued the dirty marker "would always be lit"; that premise was wrong — see the amendment below.) Rejected.
- **Footer end-only (status quo)**: minimal change, but same flaw as above. Rejected.
- **Dirty = tracked changes only**: quieter, but untracked source files are exactly the local state an AI works on top of, and ignored files never appear in porcelain output anyway. Chosen: untracked included.

## Consequences

- Two places create the turn accumulator (handler.rs and external.rs); both capture branch/dirty before the prompt is sent, and both terminal paths (handler.rs finalize, external.rs `finalize_done`) refresh them before the final flush. The refresh read runs outside the cards lock; the lock only wraps the field swap.
- The halves never regress: only a resolved branch overwrites them, so a failed or empty end read keeps the start capture rather than dropping `branch ⚠` from the footer.
- The footer builder splits into an always-visible part (directory/branch/dirty) and a final-card-only part (model/context ratio); the token-cost fallback path becomes unreachable and is removed.
- Two git subprocess pairs per turn (`rev-parse` + `status`, at start and at end), strictly best-effort: any failure silently omits — or, at turn end, keeps — the branch/dirty halves.

## Amendment (2026-09-12)

The original decision rejected end capture on the premise that the AI's own edits
always leave the tree dirty, so an end ⚠ would carry no signal. That premise was
wrong: a coding turn that commits its work (the normal end state) leaves the tree
clean, and a read-only turn does not touch it. End refresh does not replace the
start capture — the start value is the state the AI builds on, the end value is
where it left the tree — so the footer now does both. Source:
`.scratch/turn-footer-end-refresh/issues/01-end-of-turn-refresh.md`.
