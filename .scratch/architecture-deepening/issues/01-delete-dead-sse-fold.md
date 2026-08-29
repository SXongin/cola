# 01 — Delete the dead SSE event fold

**What to build:** Remove the unreachable "event → card state" fold — the
`OpenCodeEvent` protocol enum and the accumulator's `apply` fold, plus their
serde fixture tests — so the polled-parts render pipeline is the only voice that
turns backend parts into card state. Recorded in ADR-0011; the SSE surface is
heartbeat-only on this store and the fold has already drifted (shell panels).

**Blocked by:** None — can start immediately.

**Status:** resolved

- [x] No `OpenCodeEvent` type or `apply` fold remains anywhere in the codebase.
- [x] Tests that exercised the fold are removed; any coverage they uniquely
      provided on the live path is replaced by equivalent render-pipeline tests.
- [x] Full verification loop green: fmt check, clippy `-D warnings`, test suite,
      release build.

## Answer

Deleted the second, parallel "event → card state" implementation (ADR-0011):

- `opencode/client.rs`: the `OpenCodeEvent` enum + every SSE `*Data` struct
  (`PromptedData` … `QuestionAskedData`, including the drifted `ShellStarted` /
  `ShellEnded` shell panels) and the ~300 lines of serde fixture tests that
  documented the SSE protocol.
- `bridge/streaming.rs`: `StreamAccumulator::apply` and its ~9 flow tests plus
  the `make_*` event builders.

Coverage on the live path is already provided by the polled-parts render
pipeline: `render.rs` tests cover reasoning/text/tool parts, content dedup,
tool running→completed re-render, and interleaving; `streaming.rs` still has
`card_interleaves_text_and_tools_in_timeline_order` and
`tool_state_update_does_not_duplicate_timeline_marker`. Nothing remains that
reaches the SSE shape. 235 tests pass (21 fold tests removed).

Verification: fmt clean, clippy `-D warnings` clean, 235 tests pass, release
build succeeds.