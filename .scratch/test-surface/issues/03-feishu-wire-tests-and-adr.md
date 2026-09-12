# 03: 其余 feishu 方法与错误路径 wire 测试 + ADR-0031

**What to build:** 把 `feishu::Client` 剩下的全部 public 方法纳入 wire 覆盖：卡片收发更新、消息查询、图片下载、用户/群/机器人信息，以及非 JSON 错误页、HTTP 5xx、业务错误码等失败路径。用 ADR-0031 记录「wire 测试用本地 fake server + base_url 注入，不 mock reqwest」的决策。

**Blocked by:** 02.

**Status:** ready-for-agent

- [ ] `reply_card` / `send_card` / `update_message` / `reply_text` / `reply_in_thread` / `reply_card_in_thread` / `reply_completion_notice`：断言请求 URL、query、Bearer 头、body JSON 与解析结果。
- [ ] `list_messages`：断言 query 参数（`container_id_type` / `container_id` / `sort_type` / `page_size`）与 items 解析；生产实现没有分页，不写不存在的分页测试。
- [ ] `get_message`：断言 `card_msg_content_type=raw_card_content` 与 content / mentions 解析。
- [ ] `download_image`：断言字节与 content-type 透传，以及非 2xx 的错误映射。
- [ ] `user_name` / `chat_name` / `bot_open_id` / `get_ws_endpoint`：happy path 与失败降级。
- [ ] 错误路径：非 JSON（HTML）错误页、HTTP 5xx、业务错误码、token 解析失败，均映射为可诊断的错误信息。
- [ ] ADR-0031 落盘：wire 测试用本地 fake server + `base_url` 注入；不引入可 mock 的 reqwest 层；`Platform` / `Backend` trait seam 保持不变（与 ADR-0010 一致）。
- [ ] 验证循环全绿。
