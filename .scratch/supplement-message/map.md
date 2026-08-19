# Map: 补充消息并入当前轮

## Notes / context

- 用户在飞书给 cola 发消息，session 处理中时 cola 直接拒绝（"上一条消息还在处理中"）。用户希望补充消息**并入当前轮**，不打断工具调用，也不需要 `/stop`。
- 已确认 OpenCode（最新 `origin/dev`）原生支持并入：消息即时入库（`prompt.ts:1057`）→ runLoop 每轮重载 `msgs` 取最新 user（`prompt.ts:1092-1096`）→ 工具间隙写入的新消息成为新 `lastUser`，同一轮继续处理（退出条件 `parentID===lastUser.id` 不满足，`prompt.ts:1115`）→ 原消息不丢（模型每次迭代收完整 `msgs`，`message-v2.ts:131`）。
- OpenChamber 用的是客户端队列 + idle 后逐条补发（独立 turn），**不是**并入当前轮。用户需求更贴 OpenCode 原生并入，故不照搬 OpenChamber。

## Decisions so far

- 方案 A：依赖 OpenCode 原生并入。补充消息用 `prompt_async` 入库，不新建/不覆盖 accumulator，runLoop 自然拾取。
- 不做方案 B（OpenChamber 式客户端队列，独立 turn）。
- 补充消息用 `prompt_async`（返回 204，不阻塞），避免同步 `prompt` 拖延。

## Fog

- 如何判定"赶上当前轮" vs "已作为下一轮"？（cola 无法直接知道 runLoop 是否还活着；倾向让 OpenCode 自己决定 + 提示说明不确定性）
- 补充内容是否要在卡片上可见标记？（倾向静默并入）
- 补充路径与 `run_prompt` 的重构边界如何切分，才能避免 accumulator 竞争？
