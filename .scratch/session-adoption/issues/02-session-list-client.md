# 02 - Canonical session list client

Status: resolved
Type: task
Blocked by: none

## What to build

Give the OpenCode client (src/opencode/client.rs) the canonical, working
server calls the discovery commands need, replacing the legacy dead code.

## Scope

- **`list_sessions`**: hit the canonical `GET /session` (no `/api` prefix —
  the old path is dead code and returns nothing usable). Parse `Session.Info[]`
  (camelCase: `id`, `title`, `directory`, `parentID`, `time.created/updated`,
  `agent`, `model`). No `directory` query param → whole shared store.
- **`update_session_title(session_id, title)`**: `PATCH /session/{id}` with
  `{"title": ...}`.
- **`chat_name(chat_id)`**: `GET /im/v1/chats/{chat_id}` → `data.name`, for the
  attach-rejection card (Feishu client, following the existing user-name lookup
  pattern in src/feishu/client.rs).
- Delete or wire up the `#[allow(dead_code)]` store helpers left behind
  (`remove`, `thread_for_session` in src/config.rs) — decide per helper whether
  the new commands use it.

## Acceptance criteria

- [ ] `list_sessions` returns sessions from the live server (unit-testable
      against a fixture; no `/api` in the URL).
- [ ] `update_session_title` PATCHes and the title round-trips via
      `session_info`.
- [ ] `chat_name` resolves a chat id to a name.
- [ ] No dead-code warnings introduced; `list_sessions` is referenced by a real
      call site.

## Blocked by

None - can start immediately
## Comments

Implemented in one pass (2026-08-28). All tickets done together in dependency order; see commit message. Verified by `cargo test` (222 passed) and `cargo clippy --all-targets` (clean).
