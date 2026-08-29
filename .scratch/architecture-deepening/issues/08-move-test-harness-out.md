# 08 — Move the test harness out of the coordinator

**What to build:** Move the test harness (mock backend, recording platform,
app-builder, and the ~4,500 test lines living in the coordinator module) into a
`cfg(test)` support module, so the coordinator reads as a coordinator and its
production size returns to normal scale. The harness stays importable by sibling
modules so flow-level tests keep driving it.

**Blocked by:** 05 — Fuse Permission and Question into one RequestFlow.

**Status:** resolved

- [x] Coordinator module contains no test harness and no wholesale test bodies;
      production-only scale.
- [x] Harness is a `cfg(test)` support module importable by sibling modules.
- [x] Full suite (moved tests included) green.
- [x] Full verification loop green.

## Answer

The whole test stack moved out of `handler.rs` into a new sibling module
`src/bridge/test_support.rs` (`#![cfg(test)]`, declared `#[cfg(test)]
pub(crate) mod test_support` in `bridge/mod.rs`):

- the harness (MockBackend, RecordingPlatform, PlatformCall, test_config,
  realistic_parts, long_answer_parts) at top level,
- the ~4,500 test lines in a nested `pub(crate) mod integration_tests` (with
  the shared helpers `build_app`, `incoming`, `test_work_dir`, the LiveHarness
  and its `wait_for_card`/`send_and_process`/`live_setup`).

`App.core` became `pub(crate)` (the tests build/reach the shared core through
it). `handler.rs` shrank from 5409 lines to 792 — production-only coordinator
code. Sibling modules can drive the harness via
`crate::bridge::test_support::{MockBackend, RecordingPlatform, ...}`.

Verification: fmt clean, clippy `-D warnings` clean, 251 tests pass, release
build succeeds.