# 02: 本地 fake HTTP server + base_url + token wire 测试

**What to build:** 让真实 `feishu::Client` 可以被指向本地 fake server，并用第一条 wire 测试把 token 路径完整跑通：成功获取、缓存命中、过期重取、业务错误码。这是 C2 的第一刀，同时立起后续所有 feishu（以及 opencode）wire 测试共用的基础设施。

**Blocked by:** None (can start immediately).

**Status:** ready-for-agent

- [ ] `Client` 增加私有 `base_url` 字段，`new` 默认 `https://open.feishu.cn`；新增生产可用的 `pub fn with_base_url(cfg, url)` 构造器（不是 cfg(test) 后门）。
- [ ] 全部现有 HTTP 调用（含 WS endpoint 获取）改为基于 `base_url` 拼接，不再散落硬编码 URL。
- [ ] 新增共享 `cfg(test)` 模块 `src/test_http.rs`：绑定 `127.0.0.1` 随机端口；路由表（method + path 前缀 → 状态码 + body）；请求日志（method / path / query / headers / body）。不新增 dev-dependency。
- [ ] `get_access_token` wire 测试：成功；缓存命中（token 端点只被请求一次）；过期后重取；业务错误码（`code != 0`）映射为 Feishu 错误。
- [ ] 每条 wire 测试同时断言请求日志（发出去什么）与返回值（拿回什么），不断言 client 私有状态。
- [ ] 现有测试全绿，验证循环全绿。
