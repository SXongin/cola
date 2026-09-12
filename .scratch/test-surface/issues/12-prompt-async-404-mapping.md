# 12: prompt_async 的 404 是否对齐 prompt 的 SessionNotFound

**What to build:** 决策并落地：`prompt_async` 遇到 404 时，是像 `prompt` 一样返回 `BridgeError::SessionNotFound`（可触发现有重建路径），还是把现状（任意非 2xx → `OpenCode` 文案）显式固化。

**Source:** ticket 07 合并后 `/code-review` 的 Spec 轴 finding（2026-09-12，标为 "Wrong"）。

**Blocked by:** None (can start immediately).

**Status:** needs-triage

现状（2026-09-12）：

- `prompt`（`src/opencode/client.rs`）：404 → `BridgeError::SessionNotFound`，bridge 据此重建 session。
- `prompt_async`：所有非 2xx 一律 → `BridgeError::OpenCode("prompt_async {id} failed: ...")`。
- 唯一调用者是 supplement path（`src/bridge/handler.rs:499`）：任何错误都只 log + 回「⚠️ 补充消息发送失败，请稍后重试。」，不区分错误类型；因此对齐全无行为差异，但映射不对称。
- ticket 07 的 wire tests 已覆盖 prompt_async 的 500 错误映射，未覆盖 404。

- [ ] 决策：对齐（404 → `SessionNotFound`）或明确不对齐并说明理由。
- [ ] 落地：改映射 + 补 wire test；或只加 404 pin 测试 + 代码注释。
- [ ] 验证循环全绿。

## Comments

- 2026-09-12：由 ticket 07 会话整理成 ticket（用户确认）。
