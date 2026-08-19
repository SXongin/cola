# Spec: 补充消息并入当前轮

Status: drafting

## Problem

用户在飞书里给 cola 发消息时，如果该会话（session）正在处理一轮（turn），cola 目前会直接拒绝：

```
⏳ 上一条消息还在处理中，请稍等它完成后重发。
```

（`src/bridge/handler.rs` 的 `run_prompt` inflight 检查，第 583-592 行。）

这对用户不友好：着急补充一句话纠正方向时，必须先 `/stop` 打断，再重发。用户希望**补充消息尽可能并入当前正在进行的这一轮**——只要赶得上（当前轮的 OpenCode runLoop 还没退出），就让模型基于完整上下文（含补充）继续；赶不上才作为下一轮。

## 已确认的 OpenCode 原生能力（基于最新 `origin/dev` 源码）

1. **消息即时入库**：`prompt()` 里 `createUserMessage(input)`（`prompt.ts:1057`）在任何状态（包括 busy）都会立即把消息写入数据库，然后才进入 `loop()`。
2. **runLoop 每轮重新加载全部消息**：`let msgs = MessageV2.filterCompactedEffect(sessionID)`（`prompt.ts:1092`），`lastUser = latest(msgs).user`（`prompt.ts:1096`，取时间上最新的 user）。
3. **新消息并入当前轮**：若新消息在 runLoop 还活着时（工具间隙）写入，下一次迭代 `lastUser` 变为新消息，退出条件 `lastAssistant.parentID === lastUser.id`（`prompt.ts:1115`）不满足 → 不 break → 同一轮继续处理新消息。
4. **原消息不丢失**：模型每次迭代接收 `toModelMessagesEffect(msgs)`（`prompt.ts:1262`），`msgs` 是完整会话历史（`message-v2.ts:131` 遍历全部 input，不裁剪旧消息）。`lastUser` 只决定"本轮回复哪条"，模型始终看到完整上下文。
5. **并发语义**：`ensureRunning`（`runner.ts:115-138`）在 `Running` 状态时等待当前 run 完成、不启动新 run——不影响消息已入库。

结论：**OpenCode 原生支持"补充消息并入当前轮"，原用户消息保留**。cola 只需要把消息发出去（让它入库），runLoop 会自然拾取。

## cola 改动方向

当前 cola 用同步 `prompt` + 应用层 inflight 锁，第二条消息被拦在门外（根本没调用 `prompt`，消息没入库）。

### 补充路径（新入口）

收到消息时，若该 session 正在 inflight：

1. **不拒绝**，改为调用 `prompt` 把消息发给 OpenCode → 它自己 `createUserMessage` 入库，runLoop 拾取并入当前轮。
2. **不新建 / 不覆盖 accumulator**（当前轮已有的 render loop 在 poll 时自然看到新内容）——避免 `accs.insert` 覆盖导致卡片错乱（`handler.rs:636`）。
3. 给用户提示："已收到补充，将并入当前处理"。

### 关键交互点

- **提示文案**要区分"赶上当前轮" vs "已作为下一轮"（cola 无法直接知道 runLoop 是否还活着，需设计判定或说明）。
- 补充消息的 `prompt` 用同步 API 会 `ensureRunning` 等待当前轮结束才返回；cola 的补充路径应在独立 task 里后台发，不阻塞 WS 读循环（现架构每条消息已是 `tokio::spawn`，`ws.rs:679`）。

## 范围

- 只做"并入当前轮"（方案 A），不做 OpenChamber 式客户端队列（方案 B）。
- 不动卡片渲染逻辑（并入后内容自然合入当前卡）。
- 需要评估 accumulator 竞争、提示反馈、同步 prompt 阻塞三个风险。

## Open Questions

- Q1: cola 如何判定"赶上当前轮" vs "已作为下一轮"？可靠的信号是什么？
- Q2: 补充消息是否需要在 accumulator / 卡片上可见标记（如"用户补充：…"），还是保持静默并入？
- Q3: 补充消息的 `prompt` 同步阻塞（等当前轮结束）是否可接受，还是必须用 `prompt_async`？
