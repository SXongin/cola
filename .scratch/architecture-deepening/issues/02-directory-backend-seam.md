# 02 — DirectoryBackend seam

**What to build:** Introduce the instance handle. Directory-scoped backend calls
(list/reply permissions, list/reply/reject questions, session info) move onto a
`DirectoryBackend` returned by `for_directory(dir)`; the mock implements both
seams; callers that iterate known session directories use the handle instead of
threading a `?directory=` query param. The `Backend` trait stays as the test
seam (ADR-0010) — this is a reshape, not a deletion.

**Blocked by:** None — can start immediately.

**Status:** resolved

- [x] No directory-scoped backend method takes a caller-supplied directory; the
      handle owns it.
- [x] MockBackend implements both the `Backend` and `DirectoryBackend` seams;
      the suite stays green.
- [x] The silent-omission cwd footgun is gone — omitting a directory is no
      longer representable at the call sites that iterate known directories.
- [x] Full verification loop green.

## Answer

Implemented as the first wave of the deepening. `opencode/mod.rs` gains a
`DirectoryBackend` trait (list/reply permissions, list/reply/reject questions,
session_info — all without a directory argument) and a single concrete adapter
`BackendDirectory` that wraps any `Arc<dyn Backend>` and forwards the carried
directory into its directory-scoped methods. `Backend::for_directory(self:
Arc<Self>, dir)` returns the handle; `Client` and `MockBackend` both produce a
`BackendDirectory` via it, so the mock participates in both seams.

Every call site now routes through the handle:
- permission/question poll loops: `opencode.clone().for_directory(dir)`
- card actions: the card's carried `directory` routes the reply; a card with no
  directory is a hard failure ("处理失败") instead of silently hitting the
  server-cwd instance
- parent-chain walks (`resolve_card_target`, `inline_host_session`,
  `should_auto_accept`, `session_descends_from`) use the handle for
  `session_info`
- `session_subtitle` / external-flow title fetches now use the session's
  directory from the SessionStore instead of `None` (server cwd)

Three question-card-action tests were updated to carry `"directory": "/work"`
in their simulated button payloads — real cards always carry the directory, so
the old payloads were unrepresentative (previously the mock silently ignored the
missing directory).

Verification: fmt clean, clippy `-D warnings` clean, 251 tests pass, release
build succeeds.