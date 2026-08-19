# Ticket 01: cola 客户端加 prompt_async 方法

Type: task
Status: resolved
Blocked by:

## 背景

补充消息要"并入当前轮"，靠 OpenCode 的 runLoop 在工具间隙拾取新入库的消息。但 cola 当前的 `prompt` 是**同步** API（`POST /session/{id}/message`，阻塞到本轮结束才返回）。补充消息若用同步 API，调用方会阻塞等到当前轮结束——WS 读循环虽在独立 task 不会卡死，但补充路径的响应和提示会被拖延。

OpenCode 提供 `prompt_async` 端点（`POST /session/{id}/prompt_async`，返回 204，`Effect.forkIn` 立即 fork run）。它同样执行 `createUserMessage`（消息即时入库），但调用方不用等待。

## 任务

在 `src/opencode/client.rs` 增加异步 prompt 方法：

```rust
pub async fn prompt_async(&self, session_id: &str, text: &str) -> crate::error::Result<()>
```

- 调用 `POST /session/{id}/prompt_async`（注意：路径无 `/api` 前缀，见 AGENTS.md 已知坑 #1）
- payload 与 `prompt` 相同：`{"parts":[{"type":"text","text":...}], ...}`
- 期望 204；非 2xx 返回错误

## 验收

- `prompt_async` 存在且正确调用 `prompt_async` 端点
- 单元测试覆盖成功路径（204）与失败路径

## Resolution

Implemented in `src/opencode/client.rs` (`prompt_async`) + `Backend` trait + MockBackend. Uses `POST /session/{id}/prompt_async` (no `/api` prefix). Committed as `5f83be5`.

## Comments

- payload mirrors synchronous `prompt` (`parts[].text` + model); OpenCode persists the message and forks a run (204).
