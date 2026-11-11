# Reject Feishu streaming cards; honest progress header instead

> **Amended by the 2026-11-04 update below**: the decision stands. Two parts of
> the framing were too broad: element-level increments buy more than a
> typewriter, and the interactive-callback restriction is narrower than stated —
> it forbids a write *while a callback is outstanding* (`200810`), and for cola
> the real collision is that our ack returns the whole card. The split-card
> rejection is what a current streaming implementation arrives at for **forms**.

cola deliberately does **not** use Feishu's streaming-card mode. The real problem was UX: during long reasoning/tool phases the card header sat frozen on "思考中/调用工具中", so users could not tell a slow turn from a dead one. Streaming mode fixes that only by accident: its typewriter effect targets the final answer text (not the goal), and its interactive-callback restriction conflicts with cola's inline permission/question buttons — the split-card workaround would bury interactions on separate cards users may miss, re-creating the very "looks stuck" case it tries to solve. *(Narrowed by the 2026-11-04 update: the restriction's window is a write while a callback is outstanding, and the split-card trade is what streaming costs for **forms** specifically — see below.)* Instead the card header carries honest progress signals — phase timer, reasoning length, and a "等待你的授权/回答" state when a permission/question is pending — refreshed by re-rendering only when the header text changes.

> **Amended by ADR-0040**: collapsible panels on the message card now carry a
> stable `element_id`, for the client's own fold-state keying. No card entity and
> no streaming-mode lifecycle enter the pipeline; the message-PATCH path remains
> the single card-update mechanism.

## Considered options

- **Feishu streaming mode** (cardkit entities, `streaming_mode`, `card-element/content`): rejected. Needs `cardkit:card:write`, globally unique `element_id`s, and a streaming-mode lifecycle (disable before any interactive callback); while active it blocks interactive-callback card updates; typewriter only benefits the final answer, which is not the goal. *(Narrowed by the 2026-11-04 update: "disable before any interactive callback" means before a write that **answers** it — a toast-only ack, followed by an async write, needs no close; the same blocked window is what the third clause refers to; and element-level increments buy more than a typewriter.)*
- **Split-card-on-interaction** (stop streaming, finalize current card, start a fresh card for the interaction): rejected. The interaction moves to a separate card a user may not notice (AI then waits forever = the stuck case re-appears elsewhere), one turn spawns many cards, and the finalized card's "部分完成，继续中" header is misleading (the turn is paused, not truncated).
- **Dynamic header via periodic whole-card refresh** (chosen): reuses the existing PATCH pipeline. A ticking clock passively distinguishes "cola died" (header frozen forever) from "cola alive"; reasoning length shows real progress; a pending permission/question flips the header to "等待你的授权/回答". The timer tracks four active phases — Loading, Reasoning, Streaming, and a running Tool (its own phase so a long tool's elapsed time counts from when it started, not from the previous tool).

## Consequences

- Active turns re-flush the card up to ~1/s (when the header's displayed second changes), similar to today's peak PATCH rate; the "flush only when header text differs" rule keeps it throttled.
- cola cannot positively declare a turn "stuck" — the header stays honest (a ticking phase timer) instead of guessing; a future silence-threshold alarm was explicitly deferred. An earlier seconds-since-last-content ("静默") signal was dropped as redundant once per-step tool status and timing rendered into the card.
- No `element_id` bookkeeping or card-entity lifecycle enters the render pipeline; the message-PATCH path remains the single card-update mechanism.

## Update (2026-09-19)

The awaiting-action title now names the pending kind: a permission-only wait
shows "⏳ 等待你的授权", a question-only wait "⏳ 等待你的回答", and the combined
"⏳ 等待你的授权/回答" only when both are live at once (`AwaitingAction`). The
decision itself is unchanged — a paused turn still says what it waits on, and
the header still re-flushes only when its text changes.
## Update (2026-10-31): the signature cache belongs to every card write

The flush gate throttles by comparing the accumulator's header signature with
the signature the card last showed (`TurnState::last_header_sig`, plus the same
shape for the footer's context segment). Only the render pass advanced those
caches. Every other flush — a question surfacing, a click's ack, a receipt
landing — rendered the new header itself and left the cache stale, so the very
next render tick PATCHed a body identical to the one the card already carried,
just to "discover" a header that had changed before the flush. Each card write
now advances the cache to the body it built (`advance_sent_signatures`,
`src/bridge/turn/flush.rs`), so the tick stays a real second's change (#917).
The render pass itself no longer pre-advances it: a write that was suppressed
(frozen behind a typed answer) or refused then still owes its PATCH, and the
next pass retries instead of believing a header the card never showed.

The tick still stops while the turn waits on the Host, since the awaiting
title is a static string; a card that renders an unanswered free-text answer is
additionally never repainted for background progress at all (ADR-0038's
2026-10-31 update, ADR-0060's 2026-10-31 amendment) — the repaint would wipe the
text being typed — and the frozen read is not consumed, so the pass after the
answer lands it.

## Update (2026-11-04): which of these reasons still carry

Discussion #972 describes a one-card-per-turn bridge that does use streaming —
and it lands in the same place this ADR did on the part that matters. Its
question form goes on a **separate, non-streaming card**, because a form inside
a card whose text is being played out loses its input ("选完弹回占位符、输入框
吃不到字符"). That is the split-card cost in the options list above, paid by the
other side, and it is the same client behaviour as pitfall `card-repaints`. So
"the interaction moves to a separate card a user may not notice" is not a
cola-specific worry: #972, which streams its answer, makes that trade for forms.
What it shows can stay inline is **approval**, which is buttons only — no input,
no draft to lose — and only because its ack returns a toast; cola's ack returns
the card, so it would need the rework described in ADR-0004's 2026-11-04 update
before any of this applies.

Two corrections to the options list above:

- **"While active it blocks interactive-callback card updates"** is imprecise.
  A callback cannot update the card *while it is outstanding* (`200810`); an
  async write after the ack is fine, and #972 is built that way. cola's ack
  path does the opposite on purpose: `settle_ack` returns the whole card in the
  response so the click's result never races a PATCH (ADR-0038 rule 3). Moving
  to streaming would therefore mean giving that up for a toast + async write on
  every interactive path — real work, but bounded, and not the reason to stop.
  The always-true statements are narrower: a streaming card cannot be
  **forwarded** while it streams, and streaming auto-closes after 10 minutes
  (`200850`).
- **"Typewriter only benefits the final answer"** undersells element-level
  increments. Their real payoff is not the animation but that a write touches
  one element instead of rebuilding `body.elements`, so the reading position is
  not reset and — **asserted by #972, not verified here** — a live `form` input
  keeps what the Host typed. That is the same problem ADR-0093's freeze exists
  to dodge, and it is the reason this ADR's decision is dated rather than
  eternal: the freeze could go if an element write really leaves an input
  alone, which a live check would have to establish.

The decisive cost is unchanged: `cardkit:card:write`, the card-entity lifecycle,
and a second rendering path — none of which #972 removes. See ADR-0004's
2026-11-04 update.


## Update (2026-11-11): a running tool outranks reasoning

The four phases are a priority order, not just a set of labels (issue #1025). A
reasoning part can render while a `task`/`subagent` call is still running — a
seeded tail replayed by the renderer, or reasoning that lands before the wait is
live — and the card's state then says Reasoning while the card is genuinely
waiting on that tool. The phase and the header label therefore read the running
call first: a Reasoning state with a running tool reports the Tool phase and the
tool's own label, and no reasoning length rides along. The STATE keeps its
ordinary meaning (`mark_reasoning` is still a plain write), so when the call
settles and nothing else has streamed the header falls back to 「💭 推理中」
rather than claiming a reply nobody wrote.
