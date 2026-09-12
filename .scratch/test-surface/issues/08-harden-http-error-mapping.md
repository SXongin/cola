# 08: HTTP 错误处理统一为可诊断的 Feishu 错误

**What to build:** 把 `feishu::Client` 全部 public 方法的 HTTP 失败路径收进一条可诊断的错误通道：非 2xx（含 5xx、HTML 错误页）与不可解析的 body 一律变成带状态码 / body 片段的 `BridgeError::Feishu`，且 5xx 永不作为成功返回。

**Source:** test-surface ticket 02/03 的合并后 code-review（2026-09-12，Spec 轴 finding 1-3）。属 `.scratch/test-surface/spec.md` C2 的 follow-up；该 spec 冻结为「行为零变化」，故本 ticket 独立处理。

**Blocked by:** None (can start immediately).

**Status:** ready-for-agent

现状（2026-09-12）：`reply_card` / `get_access_token` 走 `read_body_with_diag`，但 `send_card` / `reply_text` / `reply_card_in_thread` / `reply_completion_notice` / `update_message` / `list_messages` 直接 `.json().await?`（HTML 或 5xx 会裸露 reqwest decode 错误）；`bot_open_id` / `get_ws_endpoint` / `user_name` / `chat_name` 读完 body 不检查 HTTP status（5xx + `{"code":0}` 会被当成功）。

- [ ] 每个 public 方法在非 2xx 时返回 `BridgeError::Feishu`，消息含 HTTP 状态与截断的 body 片段（沿用 `read_body_with_diag` 的格式或等价 helper）。
- [ ] 2xx 但 body 非 JSON 时同样返回 `BridgeError::Feishu`（含 body 片段），不裸露 reqwest decode 错误。
- [ ] 非 2xx 响应即使 body 是合法 `{"code":0,...}` 也不得作为成功返回。
- [ ] 业务错误码（`code != 0`）在 body 可解析时保持现有 `"<op> error <code>: <msg>"` 文案，不因重构退化。
- [ ] wire 测试补齐受影响方法的 5xx / HTML / 非法 JSON 失败路径，并同时断言请求日志与返回错误（复用 `src/test_http.rs`）。
- [ ] 补齐 ticket 03 遗留的 happy-path 请求断言：`get_message` / `download_image` 补 method + Authorization，`user_name` / `chat_name` 补 method + query + Authorization。
- [ ] 验证循环全绿（fmt / clippy / test / release build）。

## Comments

- 2026-09-12：由 `/code-review` 对 02+03 终态的重跑发现，评审范围 `git diff 20ea1df...5b057b9`。当日维护者确认范围并提为 `ready-for-agent`（新增断言补齐项纳入本 ticket）。
