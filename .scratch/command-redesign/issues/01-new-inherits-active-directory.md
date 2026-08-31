# 01 - `/new` inherits the active session's directory

Status: resolved
Type: task
Blocked by: none

## What to build

Make the "current project" follow the active session (ADR-0012). `/new` must
create the fresh session in the **active session's directory**, falling back to
the default directory (`[bridge] work_dir`, else process cwd) **only** when the
conversation has no active session.

Today `Command::New` (src/bridge/command.rs:397) calls
`core.default_session_directory()` unconditionally — this is the bug that
yanks the user back to the config default folder mid-project.

## Scope

- `handle_new` (src/bridge/command.rs): when the thread has an active session
  (`store.get_active(thread_key)`), create the fresh session in that session's
  `directory`; otherwise fall back to `default_session_directory()`.
- `handle_dir` keeps its own semantics (new session in an explicit path) —
  no change needed beyond the shared create path.
- `get_or_create_session` (src/bridge/handler.rs:673) already uses
  `default_session_directory()` for the first-message auto-create — that is the
  correct "no session yet" case, keep it.

## Acceptance criteria

- [ ] `/new` in a thread whose active session lives in `/proj/a` creates the
      new session in `/proj/a`, not in `work_dir`/cwd.
- [ ] `/new` in a conversation with no session still uses `work_dir` (or cwd).
- [ ] `cargo fmt --all -- --check` and `cargo clippy --workspace --all-targets -- -D warnings` pass.