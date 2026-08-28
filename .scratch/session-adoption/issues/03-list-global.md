# 03 - `/list` global

Status: needs-triage
Type: task
Blocked by: 01, 02

## What to build

Turn `/list` into a global, cached, recently-active list of every session in the
shared store, so sessions created outside Feishu become visible.

## Scope

- `/list` pulls `list_sessions` into an in-memory cache with a **30 s TTL**,
  invalidated immediately whenever cola creates/adopts/renames a session.
- Sort client-side by `time.updated` (descending); show at most **15** entries.
- Optional keyword: case-insensitive substring match on `title` / `directory` /
  id.
- `--all`: include sub-task children (`parentID` set) and archived sessions;
  otherwise hide them. Child titles are `Child session - ...` — rely on
  directory/id to tell them apart.
- Output per line: `title · dir · id-tail · relative time`. Mark the current
  thread's active session and its other mapped sessions.
- Inside a topic that already has a session: reject with a pointer to the lobby
  (see issue 07 for the shared gate).

## Acceptance criteria

- [ ] `/list` shows sessions cola did not create (verify against a session
      created outside Feishu on the shared store).
- [ ] Repeated `/list` within 30 s does not re-hit the server; an external
      rename is visible after the TTL.
- [ ] Keyword filters by title, directory, and id; `--all` reveals sub-task
      children.
- [ ] Ordering is by last activity, capped at 15, current-thread markers shown.

## Blocked by

- 01 - Session identity from server (drop `name`)
- 02 - Canonical session list client