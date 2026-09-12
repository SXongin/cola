# 01: 退役活体 E2E harness

**What to build:** 从套件中移除已经腐坏的「第二个飞书机器人 + 测试群」自动 E2E，代之以 CONTRIBUTING 里一份 6 项真实聊天人工 smoke 清单。套件不再宣称自己不拥有的覆盖，发布验收仍完整但不再需要第二套机器人凭据。

**Blocked by:** None (can start immediately).

**Status:** ready-for-agent

- [ ] `LiveHarness`、`live_setup`、`send_and_process`、`wait_for_card` 与两个 `#[ignore]` live E2E 测试全部删除。
- [ ] `Client::send_text`（仅测试用的 `#[cfg(test)]` 方法）删除；`list_messages` / `get_message` 保留，且 `list_messages` 注释改为描述真实生产用途（`resolve_topic_anchor`），不再写 "live end-to-end harness"。
- [ ] `cola-test.toml.example` 删除，`.gitignore` 中 `cola-test.toml` 条目删除；`SURVEY.md` / `test.log` 条目保留。本机 `cola-test.toml` 一并删除。
- [ ] CONTRIBUTING「Releasing」小节含 6 项 smoke 清单：① 真实聊天发消息看到 Done 卡片且原位更新；② 权限按钮往返；③ 问题按钮往返；④ `/topic` 建话题且群列表可见；⑤ `/model` 选择卡；⑥ 非 git 目录不报错。不记录 `/update`、`/restart`。
- [ ] 验证循环全绿（fmt / clippy / test / release build）；`cargo test -- --list` 的测试数量恰好比之前少 2。
