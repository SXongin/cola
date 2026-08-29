# cola renders by polling only; the SSE event surface is dead code

ADR-0001 already decided the global SSE is heartbeat-only (the server ends it
every few seconds on the shared store) and that cola renders by polling
`GET /session/{id}/message`. This ADR records the consequence: the
`OpenCodeEvent` enum in `opencode/client.rs` and `StreamAccumulator::apply` in
`streaming.rs` — a second, parallel "event → card state" fold built for the v1
SSE events — are unreachable in production, referenced only by their own tests.
They had already drifted (shell-start/end panels exist only there).

They are to be deleted, not maintained. Any future effort to consume SSE events
live must start from the polled-parts representation in `render.rs`, not by
reviving the dead fold. The serde fixtures that document the SSE protocol shape
move to the OpenCode source tree as reference (see AGENTS.md) if needed.

## Considered Options

- **Wire cola to real SSE consumption.** Rejected — ADR-0001 records the shared
  server's SSE as unreliable on this store; the poller already works.
- **Keep the type as protocol documentation.** Rejected — dead code that drifts
  is worse documentation than the live source tree it mirrors.