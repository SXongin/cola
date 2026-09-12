# 06: 最小断言层 + 文案常量 + seed_session

**What to build:** 在拆分后的集成测试上逐步收窄断言面与 setup 面：新测试默认通过结构化查询 helper 断言，卡片状态文案成为生产常量并被测试引用，迁移到手的模块用 `seed_session` 造状态。目标是不丢覆盖地把「改一句 UI 文案红一片」降下来。

**Blocked by:** 05.

**Status:** ready-for-agent

- [ ] `RecordingPlatform` 增加最小查询 helper：`replied_cards()` / `sent_cards()` / `updated_cards()` / `texts()` / `button_values()`；不做 fluent DSL。
- [ ] 卡片状态文案（等待授权 / 运行中 / 需要重试 / 空闲等）提为 `card.rs` 的 `pub(crate) const`，测试断同一常量；教程类长文案在测试中最多保留一条断言。
- [ ] 迁移到手的模块把 `app.sessions.lock()` / `app.core` 直戳 setup 换成 `seed_session`（或等价 helper）。
- [ ] 迁移按模块小步进行；每一步维持验证循环全绿，断言覆盖不减少。
