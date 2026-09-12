# 11: opencode::Session 的 projectID 反序列化 + list_messages 过期 allow

**What to build:** 两个 client 小清理：(a) `opencode::Session.project_id` 补 `#[serde(rename = "projectID")]`（服务端发 camelCase，当前静默为 `None`）；(b) 删掉 `feishu::Client::list_messages` 上过期的 `#[allow(dead_code)]`（它已被生产路径使用）。

**Source:** ticket 07 / 08 工作期间的顺带发现（2026-09-12，用户确认立项）。

**Blocked by:** None (can start immediately).

**Status:** ready-for-agent

现状（2026-09-12）：

- `src/opencode/client.rs` 的 `Session` 结构里 `project_id` 没有 `#[serde(rename)]`。OpenCode v2 `SessionsCreateOutput` 发的是 `projectID`，所以该字段永远是 `None`。当前没有消费者（`rg project_id src` 只命中 test_support 的构造），但属 AGENTS.md pitfall #2 的同类问题。
- `src/feishu/client.rs` 的 `list_messages` 带 `#[allow(dead_code)]`（live harness 时代的残留），但它被 `src/bridge/pollers.rs` 的 `resolve_topic_anchor` 生产使用。

- [ ] 补 `#[serde(rename = "projectID")]`，并把 ticket 07 的 create_session wire fixture 从 `projectID` 断言 `project_id` 解析成功。
- [ ] 删除 `list_messages` 的过期 allow；确认 fmt / clippy 仍绿。
- [ ] 验证循环全绿。

## Comments

- 2026-09-12：由 ticket 07 / 08 会话整理成 ticket（用户确认）。
