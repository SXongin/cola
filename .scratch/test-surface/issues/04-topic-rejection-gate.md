# 04: topic 限制规则收拢为一个 gate

**What to build:** ADR-0007 的「话题内命令限制」规则只有一个归属点：`Command::topic_rejection(has_session)`；`handle_command` 顶部唯一调用。四个手写守卫四个测试缩成一份规则和一个表驱动测试，新增命令自动继承 gate。

**Blocked by:** None (can start immediately).

**Status:** ready-for-agent

- [ ] `Command::topic_rejection(&self, has_session: bool) -> Option<&'static str>`：`Topic` / `TopicAdopt` / `TopicAdoptCard` 在任意话题返回原因（与 `has_session` 无关）；`Dir` / `DirCard` / `Switch` / `New` 仅在 `has_session == true` 时返回原因；其余命令返回 `None`。
- [ ] `handle_command` 顶部唯一调用点，命中即回复原因并返回；`command.rs` 中其余三处手写守卫删除。
- [ ] `/topic --adopt` 的两条逐字重复文案合并为一条；选择类与 `/topic` 各自文案保留。
- [ ] 删除 `test_support.rs` 中 4 个专用拒绝测试，新增 1 个表驱动集成测试：遍历被禁命令集合（绑定/未绑定两种话题状态）与允许命令集合，断言每个被禁命令恰好得到一条拒绝回复、允许命令不被拦截。
- [ ] 行为零变化（除重复文案合并外，用户可见输出不变）；验证循环全绿。
