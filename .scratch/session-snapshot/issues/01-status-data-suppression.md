# 01 - Session-status client + snapshot data gathering & suppression predicate

Status: ready-for-agent
Type: task
Blocked by: none

## What to build

The read side of the Session Snapshot (ADR-0028): teach cola to (a) ask the
server whether an arbitrary session is running, and (b) gather everything the
snapshot card needs from server reads alone. No card, no wiring, no writes.

## Scope

- **Session-status client** (`src/opencode/client.rs` + the `Backend`/
  `DirectoryBackend` seams in `src/opencode/mod.rs` and their test mock in
  `src/bridge/test_support.rs`):
  - `GET /session/status` returns `Record<sessionID, {type: "idle"|"busy"|"retry", …}>`
    (server status service; route under `httpapi/groups/session.ts`). Add a typed
    method + a small `SessionStatus { Idle, Busy, Retry }` (unknown/error → omit,
    never guess). Check the real payload shape in the opencode source tree before
    coding — cola has never called this endpoint.
  - Expose it over the backend seam so tests can mock a busy/retry session.
- **Snapshot data gathering**: for an adopted `(session_id, directory)` build a
  snapshot payload from reads only:
  - server status for the session;
  - pending Permissions/Questions whose `sessionID` == the adopted session, via
    `for_directory(&dir).list_permissions()/list_questions()` (reuse
    `src/bridge/request.rs` kinds or the client list calls directly; filter to
    the adopted session id — never another session's in the same directory);
  - transcript tail: `messages(&session_id)`, take the last 4 text-bearing
    user/assistant messages, verbatim `text` parts only (reasoning/tool parts
    are not conversation); keep message created-times and roles for display;
  - authorship of the newest user message (`msg_cola_` prefix,
    `src/opencode/client.rs:26-28`) — the suppression input.
- **Suppression predicate** (pure, unit-tested): `should_emit_snapshot(
  already_mapped_to_this_thread, status, has_pending, newest_user_is_cola_authored)`
  → show unless (mapped AND idle AND no pending AND newest user is
  cola-authored). First-time adoption always shows.

## Acceptance criteria

- [ ] `session_status()` returns idle/busy/retry typed; a failed/unknown read
      yields `None`/unknown, never a guessed answer.
- [ ] Gathering filters pending items to the adopted session id only.
- [ ] Tail = at most the last 4 text-bearing user/assistant messages, text parts
      only, newest last.
- [ ] Suppression predicate unit tests cover the full matrix (first adopt /
      re-switch × idle/busy × pending × external/cola newest user).
- [ ] `cargo test --workspace --locked` green, clippy/fmt clean.
