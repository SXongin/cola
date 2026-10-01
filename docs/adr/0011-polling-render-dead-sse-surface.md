# cola renders by polling only; the SSE event surface is dead code

> **Amended 2026-10-01 (#470)**: cola now consumes V2's `GET /api/event`
> stream — for one purpose only: overlaying ephemeral `session.tool.progress`
> metadata onto RUNNING tool parts at transcript decode, so a live `subagent`
> carries its child session id (ADR-0054) and Code Mode's nested rows update
> while they run. This follows the rule below rather than contradicting it:
> the overlay starts from the polled-parts representation, the polled message
> remains the source of truth (a lost stream degrades to no live line, never a
> wrong one), and V1 stays polling-only. The dead V1 SSE fold stays deleted.
> See ADR-0059's "Amended 2026-10-01 (#470)" banner for the decision.

ADR-0001 already decided the global SSE is heartbeat-only (the server ends it
every few seconds on the shared store) and that cola renders by polling
`GET /session/{id}/message`. This ADR records the consequence: the
`OpenCodeEvent` enum in `opencode/client.rs` and `StreamAccumulator::apply` in
`streaming.rs` (the accumulator now lives in `src/bridge/turn/state.rs`) — a
second, parallel "event → card state" fold built for the v1
SSE events — are unreachable in production, referenced only by their own tests.
They had already drifted (shell-start/end panels exist only there).

They are to be deleted, not maintained. Any future effort to consume SSE events
live must start from the polled-parts representation in
`src/bridge/turn/render.rs`, not by
reviving the dead fold. The serde fixtures that document the SSE protocol shape
move to the OpenCode source tree as reference (see AGENTS.md) if needed.

## Considered Options

- **Wire cola to real SSE consumption.** Rejected — ADR-0001 records the shared
  server's SSE as unreliable on this store; the poller already works.
- **Keep the type as protocol documentation.** Rejected — dead code that drifts
  is worse documentation than the live source tree it mirrors.