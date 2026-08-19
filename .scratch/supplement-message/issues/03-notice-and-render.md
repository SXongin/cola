# Ticket 03: 补充消息的用户提示与卡片呈现

Type: task
Status: resolved
Blocked by: 02

## 背景

补充消息并入当前轮后，用户需要在飞书上有清晰的反馈，否则不知道自己的消息"排上了没有"、会不会被处理。同时要考虑卡片上是否要标记补充内容。

## 任务

1. **提示文案**：补充消息收到时回复一条即时提示，例如：
   - "📨 已收到补充，将并入当前处理。"
   - 若无法判定赶上/下一轮，文案需诚实说明（见 spec Q1）："将并入当前处理；若当前轮已结束，会作为下一条消息。"

2. **卡片标记（可选，需确认）**：是否在 accumulator / 卡片上显示"用户补充：…"标记，还是静默并入。倾向静默并入（内容自然合入当前卡，cola 无需改渲染），除非用户要求可见标记。

## 关键点

- 提示必须是**即时**的（用 `prompt_async` 返回后立即 reply_text），不能等当前轮结束才提示。
- 若提示用同步 `prompt` 会拖延，须用 `prompt_async`（依赖 ticket 01）。

## 验收

- inflight 时发补充消息 → 立即收到提示
- 提示内容说明补充将并入当前轮
- 卡片最终显示补充内容相关的回复，不丢原消息

## Resolution

Folded into Ticket 02's implementation (`09babfb`). The supplement notice replies immediately (via the fire-and-forget path, not blocked): "📨 已收到补充，将并入当前处理。若当前轮已结束，会作为下一条消息继续。" Card marking: silent merge (decision Q2) — no visible marker, content naturally lands in the current card.

## Comments

- Notice only sent for P2p / Topic kinds (lobby prompts are guidance-only).
