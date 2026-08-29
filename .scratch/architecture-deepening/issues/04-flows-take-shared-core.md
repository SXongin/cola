# 04 — Flows take SharedCore

**What to build:** Narrow the dependency of the flows, pollers and render
pipeline to the shared state they actually use, instead of the whole bridge
coordinator, so each flow is constructible and testable with the shared state +
the two adapter mocks alone. The one cross-flow map poke (writing another flow's
user-message baseline from the prompt path) becomes a method on the owning flow.
Break the command↔coordinator module cycle by giving command dispatch the same
narrowed dependency.

**Blocked by:** 02 — DirectoryBackend seam.

**Status:** resolved

- [x] Every flow constructs with the shared state + adapter mocks alone — no
      coordinator needed.
- [x] No module pokes another flow's private state; that coupling is a method
      call on the owning flow.
- [x] The module dependency graph is acyclic (no command↔coordinator cycle).
- [x] Full verification loop green.

## Answer

All flows, pollers and the render pipeline now take `&Arc<SharedCore>` instead
of `&Arc<App>`:

- `render.rs`: `flush_card` / `render_and_flush` / `render_poll_loop` take the
  core; `session_subtitle` and `refresh_session_title` moved out of the App as
  free functions on the core.
- `pollers.rs`: `reconnect_poll_loop`, `resolve_topic_anchor`,
  `resolve_card_target`, `inline_host_session`, `mark_stale_cards` take the core.
- `permission.rs` / `question.rs` / `external.rs`: `poll_loop` /
  `handle_card_action` / `start_reply_render` take the core.
- Coordinator-only helpers moved onto `SharedCore`: `get_session_id`,
  `cached_session_list`, `invalidate_session_list_cache`,
  `approve_pending_for_session` (+ private `session_descends_from`).
- The cross-flow poke (`handler.rs` writing `external.last_user_msg_epoch`
  directly) became `ExternalFlow::record_prompt_baseline`.
- The command↔coordinator cycle is broken: `command.rs` no longer imports
  `handler::App` — `handle_command` and its helpers are free functions taking
  `&Arc<SharedCore>`. `Command::Forward` is intercepted by the coordinator
  (`handle_message`), which owns the prompt pipeline.

App keeps only coordination (message dispatch, prompt pipeline, retry) and the
flows/pollers/render are constructible with SharedCore + adapter mocks alone.

Verification: fmt clean, clippy `-D warnings` clean, 251 tests pass, release
build succeeds.