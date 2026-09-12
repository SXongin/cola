# 07: opencode::Client wire 测试（后续独立 PR）

**What to build:** 复用 02 建起的本地 fake HTTP server 基础设施，给 `opencode::Client` 补 wire 层覆盖：prompt 发送 payload（模型/变体/agent）、Basic auth 头、目录作用域请求与错误路径。让「真实 HTTP adapter」的测试面从 feishu 扩展到后端。

**Blocked by:** 02.

**Status:** ready-for-agent

- [ ] prompt / prompt_async 的请求路径、Basic auth（username + password 同时存在才发送）、payload（模型、变体、agent、目录）与错误映射。
- [ ] session 生命周期方法（create / list / update title / session info / status）的请求构造与解析。
- [ ] 目录作用域方法（list/reply permissions、list/reply/reject questions）的 `?directory=` 不再靠调用者记忆，wire 层验证其真实发出。
- [ ] 断言方式与 02/03 一致：请求日志 + 返回解析双断言，不断言私有状态。
- [ ] 验证循环全绿。
