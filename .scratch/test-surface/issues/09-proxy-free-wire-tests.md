# 09: wire 测试对开发机 env proxy 免疫

**What to build:** 让 `feishu::Client` / `opencode::Client` 的 wire 测试在设置了 `http_proxy` / `all_proxy` 的 shell 里也稳定通过：本地 loopback 的 fake server 不应经过代理。目标是与无代理的 CI 行为一致，且不改生产 client 的代理语义（生产仍沿用 env proxy）。

**Source:** ticket 08 验证期间的本地复现（2026-09-12）；已在 `main`（`0d35ec6`）上复现，属既有环境问题。

**Blocked by:** None (can start immediately).

**Status:** ready-for-agent

现状（2026-09-12）：本机 shell 导出 `http_proxy` / `https_proxy` / `all_proxy`。reqwest 默认读取 env proxy，因此对 `127.0.0.1:<fake-port>` 的请求会被送进代理；代理间歇性返回 `502 Bad Gateway`（空 body），表现为随机某条 wire 测试失败。证据：

- 带代理：`cargo test feishu::client` 4 次里 1 次失败，耗时 ~4–5.5s；失败形如 `token HTTP 502 Bad Gateway:`。
- 去掉代理：`env -u http_proxy -u https_proxy -u all_proxy cargo test ...` 全绿，~0.4s。
- 保留代理变量、加 `NO_PROXY=127.0.0.1,localhost`：连续 5 次全绿。

- [ ] wire 测试构造的 HTTP client 绕过 env proxy（推荐 reqwest `ClientBuilder::no_proxy()`）；生产路径（`Client::new` / `with_base_url` 在非 test 构建下）行为不变。
- [ ] feishu 与 opencode 两个 client 都覆盖；seam 尽量复用 `src/test_http.rs`。
- [ ] 不破坏 ADR-0031 的原则：`with_base_url` 仍是生产可用的普通构造器、不是 cfg(test) 后门；若需要测试专用注入点，在 PR 里说明选择理由。
- [ ] 在带代理变量的 shell 里 `cargo test --workspace` 连跑 ≥5 次全绿，耗时回到亚秒级。
- [ ] 验证循环全绿（fmt / clippy / test / release build）。

## Comments

- 2026-09-12：由 ticket 08 会话整理成 ticket（用户确认）。
