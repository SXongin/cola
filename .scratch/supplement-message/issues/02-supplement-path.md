# Ticket 02: 补充路径——inflight 时并入当前轮而非拒绝

Type: task
Status: resolved
Blocked by: 01

## 背景

当前 `run_prompt` 的 inflight 检查（`src/bridge/handler.rs:583-592`）在 session 正在处理时**直接拒绝**并提示"上一条消息还在处理中"。要支持补充消息并入当前轮，需要改为：inflight 时走**补充路径**，不再拒绝。

## 任务

在 `handle_prompt` / `run_prompt` 的入口处改造：

1. 检测到 session inflight 时，**不拒绝**，改为：
   - 调用 `prompt_async` 把消息发给 OpenCode（让它 `createUserMessage` 入库，runLoop 在工具间隙拾取并入当前轮）
   - **不新建 / 不覆盖当前 accumulator**（避免 `accs.insert` 覆盖导致卡片错乱，`handler.rs:636`）
   - 给用户提示："已收到补充，将并入当前处理"
2. 需要新入口（如 `supplement_prompt`）与 `run_prompt` 分开，避免复用会触发 accumulator 重建的完整流程。

## 关键点

- **accumulator 竞争**：补充路径绝不能碰当前轮的 accumulator，否则渲染错乱。当前轮已有的 render loop 会在 poll 时自然看到新入库内容。
- **判定"赶上 vs 下一轮"**：OpenCode 的 runLoop 是否还活着 cola 无法直接知道。设计上应接受"让 OpenCode 自己决定"——补充消息入库后，runLoop 活着就并入，已 break 就留着等下轮。提示文案需说明这个不确定性（见 Q1）。

## 验收

- inflight 时发消息不再收到"请稍等"拒绝
- 补充消息被发送到 OpenCode（入库）
- 当前轮 accumulator 不被覆盖，卡片不因补充而错乱
- 集成测试：inflight 时发第二条消息 → 走补充路径 → 不拒绝、不覆盖 accumulator

## Resolution

Implemented in `handle_prompt` (`src/bridge/handler.rs`): when the session is in-flight, a new message calls `prompt_async` (fire-and-forget) + replies a notice; no `run_prompt`, no accumulator overwrite, in-flight marker preserved. Committed as `09babfb`. Test: `message_during_inflight_goes_to_supplement_path`.

## Comments

- Supplement content merges silently into the current card via OpenCode's runLoop (decision Q2).
- "赶不上/已结束" is left to OpenCode: if the turn already ended, the message stays as the next user message (decision Q1).
