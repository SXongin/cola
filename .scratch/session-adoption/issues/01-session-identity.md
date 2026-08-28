# 01 - Session identity from server (drop `name`)

Status: resolved
Type: task
Blocked by: none

## What to build

Make the OpenCode server the single source of truth for a session's identity.
`SessionEntry` (src/config.rs) stops storing a `name`; every place that read or
wrote `entry.name` reads the server's `title` instead (fetched on demand, never
cached locally).

End-to-end behavior: card subtitles, external-message notification titles, and
command output all show the server `title`; legacy `~/.cola/sessions.json`
files (which contain `name`) still load and run unchanged.

## Scope

- Remove the `name` field from `SessionEntry` (serde ignores unknown fields, so
  old JSON still loads).
- `session_subtitle` (src/bridge/handler.rs) drops the stored-name fallback;
  keep preferring the server `title`, fall back to id-tail while the server
  title is still the default.
- `build_external_message_card` call site (src/bridge/external.rs) passes the
  server `title` instead of `entry.name` (one `session_info` fetch at notify
  time).
- `/new`, `/dir` (src/bridge/command.rs) and `create_fresh_session`
  (src/bridge/handler.rs) stop writing a name into the entry.
- Update `CONTEXT.md` glossary: the lobby may hold several sessions; "A Thread
  contains exactly one Session" holds for topics only.

## Acceptance criteria

- [ ] `cargo build` and the session-store unit tests pass with `name` gone.
- [ ] A legacy `sessions.json` containing `name` fields loads without loss of
      mapping or directory.
- [ ] Card subtitle shows the server title (AI-generated) once present, id-tail
      before that; never a cola-side name.
- [ ] External-message notification card title comes from the server title.
- [ ] `CONTEXT.md` glossary corrected.

## Blocked by

None - can start immediately
## Comments

Implemented in one pass (2026-08-28). All tickets done together in dependency order; see commit message. Verified by `cargo test` (222 passed) and `cargo clippy --all-targets` (clean).
