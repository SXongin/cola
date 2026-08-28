# 05 - `/switch` auto-adoption

Status: resolved
Type: task
Blocked by: 03

## What to build

Extend `/switch` so that when it cannot match a session inside the current
thread, it searches the global shared store and adopts the unique hit — one
command to both switch and take over foreign sessions.

## Scope

- Match on title / directory / id (case-insensitive substring), reusing the
  `/list` cache (issue 03) rather than a fresh fetch.
- Resolution order:
  1. Current thread's mapped sessions first; a unique hit switches without
     changing the mapping.
  2. Global search (**sub-task children excluded**); a unique hit adopts into the
     current thread and sets it active.
  3. Multiple hits: list up to 8 candidates (`title · dir · id-tail`) and point
     at `/attach <full id>`.
- Inside a topic with an existing session, `/switch` is rejected (issue 07).

## Acceptance criteria

- [ ] In the lobby, `/switch <name>` still switches between the thread's own
      sessions.
- [ ] `/switch <title-of-foreign-session>` adopts it in one step and routes the
      next message to it.
- [ ] A keyword matching several sessions lists candidates instead of adopting.
- [ ] Sub-task children are never adopted by `/switch`.

## Blocked by

- 03 - `/list` global
## Comments

Implemented in one pass (2026-08-28). All tickets done together in dependency order; see commit message. Verified by `cargo test` (222 passed) and `cargo clippy --all-targets` (clean).
