# Reject Feishu streaming cards; honest progress header instead

cola deliberately does **not** use Feishu's streaming-card mode. The real problem was UX: during long reasoning/tool phases the card header sat frozen on "思考中/调用工具中", so users could not tell a slow turn from a dead one. Streaming mode fixes that only by accident: its typewriter effect targets the final answer text (not the goal), and its interactive-callback restriction conflicts with cola's inline permission/question buttons — the split-card workaround would bury interactions on separate cards users may miss, re-creating the very "looks stuck" case it tries to solve. Instead the card header carries honest progress signals — phase timer, seconds since last content, reasoning length, and a "等待你的授权/回答" state when a permission/question is pending — refreshed by re-rendering only when the header text changes.

## Considered options

- **Feishu streaming mode** (cardkit entities, `streaming_mode`, `card-element/content`): rejected. Needs `cardkit:card:write`, globally unique `element_id`s, and a streaming-mode lifecycle (disable before any interactive callback); while active it blocks interactive-callback card updates; typewriter only benefits the final answer, which is not the goal.
- **Split-card-on-interaction** (stop streaming, finalize current card, start a fresh card for the interaction): rejected. The interaction moves to a separate card a user may not notice (AI then waits forever = the stuck case re-appears elsewhere), one turn spawns many cards, and the finalized card's "部分完成，继续中" header is misleading (the turn is paused, not truncated).
- **Dynamic header via periodic whole-card refresh** (chosen): reuses the existing PATCH pipeline. A ticking clock passively distinguishes "cola died" (header frozen forever) from "cola alive"; seconds-since-last-content and reasoning length show real progress; a pending permission/question flips the header to "等待你的授权/回答". The timer tracks four active phases — Loading, Reasoning, Streaming, and a running Tool (its own phase so a long tool's elapsed time counts from when it started, not from the previous tool).

## Consequences

- Active turns re-flush the card up to ~1/s (when the header's displayed second changes), similar to today's peak PATCH rate; the "flush only when header text differs" rule keeps it throttled.
- cola cannot positively declare a turn "stuck" — the header stays honest (a growing silence number) instead of guessing; a future silence-threshold alarm was explicitly deferred.
- No `element_id` bookkeeping or card-entity lifecycle enters the render pipeline; the message-PATCH path remains the single card-update mechanism.