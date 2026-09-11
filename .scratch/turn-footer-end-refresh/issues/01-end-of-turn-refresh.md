# 01 - Refresh the Turn Footer's work context at turn end

Status: ready-for-agent
Type: task
Blocked by: none

## What to build

The Turn Footer's work-context half (`📁 project · branch ⚠`, ADR-0019) is
currently captured once, before the prompt is sent. Re-read it when the turn
ends so the final card shows where the turn landed: a branch the AI created or
switched to, and whether it left uncommitted changes.

This is start + end, not real-time. Re-reading on every card refresh would run
`git rev-parse` + `git status --porcelain` on each ~3 s poll, per session; the
`status` scan scales with repo size, and concurrent reads would contend with the
AI's own git commands (they take the index lock to refresh the stat cache) in
the same worktree. Two extra git calls per turn is the cheap approximation, and
it is when the value is actually meaningful: the footer says "this is the state
the AI starts from" while running and "this is where it left the tree" once
done.

## Scope

- `StreamAccumulator`: one git-state apply path shared by the start capture and
  the end refresh. The halves stay best-effort: only a resolved branch
  overwrites them, so a failed/empty read at turn end keeps the start capture
  instead of dropping `branch ⚠` from the footer.
- A core-level refresh helper that re-reads the git state for the session's
  directory **outside the cards lock** (it shells out to git) and swaps the
  accumulator fields under the lock. Called from both terminal paths, before
  the final flush:
  - `handler.rs` finalize (streaming turn, Done or Error);
  - `external.rs::finalize_done` (external reply / snapshot follow).
- Docs: amend ADR-0019 and update the Turn Footer / Dirty glossary entries.

## Acceptance criteria

- [x] The final card of a streaming turn shows the branch the AI landed on
      (e.g. a `feat/` branch it created mid-turn), not the start branch.
- [x] The final card shows `⚠` when the AI left uncommitted changes, and no
      marker when it committed them.
- [x] A failed/empty git read at turn end keeps the start capture (the footer
      never degrades below what the start card showed).
- [x] The git read does not run under the cards lock.
- [x] `cargo fmt --all -- --check`, `cargo clippy --workspace --all-targets --
      -D warnings`, `cargo test --workspace --locked`,
      `cargo build --release --locked` green.

## Comments

### 2026-09-12 implemented (branch `feat/turn-footer-end-refresh`)

- `StreamAccumulator::apply_git_state` — the one write path both the start
  capture and the end refresh use; only a resolved branch overwrites the
  halves, so a failed end read keeps the start capture. `attach_work_context`
  now delegates to it.
- `streaming::refresh_work_context(core, session_id)` — turn-end refresh: read
  the git state outside the cards lock, swap the fields under it.
- Wired into both terminal paths before the final flush: `handler.rs` finalize
  (Done and Error) and `external.rs::finalize_done` (external reply / snapshot
  follow).
- Docs: ADR-0019 retitled to "captured at turn start and refreshed at turn
  end" with an amendment correcting the old "end is always dirty" premise;
  CONTEXT.md Turn Footer / Dirty entries updated.
- Regression tests: `apply_git_state_refreshes_halves_and_never_regresses`
  (unit), `turn_footer_shows_the_branch_the_ai_landed_on` and
  `turn_footer_shows_dirty_left_by_the_ai` (handler integration, via a new
  `MockBackend::on_prompt` hook), and the external-reply test now switches
  branches mid-turn and asserts the final card shows `feat/ext`.
- Verified: `cargo fmt --all -- --check`, `cargo clippy --workspace
  --all-targets -- -D warnings`, `cargo test --workspace --locked` (490
  passed), `cargo build --release --locked`, `cargo xtask audit` all green.

