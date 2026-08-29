# 01 — Delete the dead SSE event fold

**What to build:** Remove the unreachable "event → card state" fold — the
`OpenCodeEvent` protocol enum and the accumulator's `apply` fold, plus their
serde fixture tests — so the polled-parts render pipeline is the only voice that
turns backend parts into card state. Recorded in ADR-0011; the SSE surface is
heartbeat-only on this store and the fold has already drifted (shell panels).

**Blocked by:** None — can start immediately.

**Status:** ready-for-agent

- [ ] No `OpenCodeEvent` type or `apply` fold remains anywhere in the codebase.
- [ ] Tests that exercised the fold are removed; any coverage they uniquely
      provided on the live path is replaced by equivalent render-pipeline tests.
- [ ] Full verification loop green: fmt check, clippy `-D warnings`, test suite,
      release build.