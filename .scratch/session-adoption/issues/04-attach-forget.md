# 04 - `/attach` + `/forget`

Status: resolved
Type: task
Blocked by: 01, 02

## What to build

Let a user take over an arbitrary server session into the current thread
(`/attach`), and un-map it again (`/forget`), with safe handling of the
one-session-one-thread invariant.

## Scope

- `/attach <id|title> [--force]` resolution order: exact id → unique id-prefix →
  unique title substring. Multiple hits: list candidates and ask for the full id.
- Already mapped to the **current** thread: idempotent switch (no-op aside from
  making it active).
- Already mapped to **another** thread: reject unless `--force`, with an
  actionable card showing session title, directory, the mapped chat's **name**
  (`chat_name`), topic/lobby flag, and a pointer to `/forget` or `--force`.
- `--force`: steal the mapping (the other thread becomes sessionless; its next
  message auto-creates a fresh session).
- Adopting copies `directory` and `agent` from the server `Session.Info` into the
  entry; `auto_accept` resets to false.
- `/forget`: remove the current thread's session mapping only; the server
  session stays untouched, still listed and adoptable. Works in topics too.
- The adopted session's title is the server's (issue 01); nothing to set here.

## Acceptance criteria

- [ ] `/attach <id>` adopts a foreign session; the next message routes to it and
      runs in the session's own directory.
- [ ] Rejecting an already-mapped session shows the card (title, directory,
      mapped chat name, topic/lobby flag); `/attach ... --force` then steals and
      the old thread recovers with a fresh auto-created session.
- [ ] `/attach <title>` resolves uniquely or lists candidates; ambiguity never
      adopts silently.
- [ ] `/forget` unmaps without deleting the server session; the thread is
      sessionless until its next message.

## Blocked by

- 01 - Session identity from server (drop `name`)
- 02 - Canonical session list client
## Comments

Implemented in one pass (2026-08-28). All tickets done together in dependency order; see commit message. Verified by `cargo test` (222 passed) and `cargo clippy --all-targets` (clean).
