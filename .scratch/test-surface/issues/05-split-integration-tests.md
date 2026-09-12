# 05: 集成测试按特性机械拆分

**What to build:** `test_support.rs` 里的 176 个集成测试按特性拆进 `src/bridge/tests/` 的 12 个 `cfg(test)` 模块，让「找某个特性的测试」不再需要滚过上万行。这一步是纯搬家：断言一字不改，测试名与数量一一对应。

**Blocked by:** 01, 04.

**Status:** ready-for-agent

- [ ] `src/bridge/tests/` 下建立 12 个特性模块：prompt-render / permission / external / subtask / topic / dir / session-routing / question / switch / snapshot-follow / config-commands / misc；harness（MockBackend / RecordingPlatform / build_app / incoming / seed_session 等）留在 `test_support.rs`。
- [ ] 嵌套的 `integration_tests` 模块删除；模块间共享的 helper 通过既有 `test_support` 的 `pub(crate)` 接口引用，不暴露新的 public API。
- [ ] 机械搬迁是独立 commit，断言零改动；搬迁后 `cargo test -- --list` 的测试名集合与搬迁前一致（扣除 01 删除项）。
- [ ] 验证循环全绿。
