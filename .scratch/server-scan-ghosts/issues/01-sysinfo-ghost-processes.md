# 01 — `scan_processes` 被 sysinfo 的"幽灵进程"污染，触发 Yield 误杀 Owned Server

**What to build:** 修复 `bridge/discovery.rs::scan_processes` 通过 sysinfo 0.39
扫描 `opencode serve` 进程时把已死/不存在的进程（下文称"幽灵进程"）当成
server 候选，导致 `reconcile` 把 cola 自己拉起的 Owned Server 误判为
"Coexistent Server 出现"而触发 Yield 杀掉它，造成对话中断、反复拉起又被杀、
以及僵尸进程泄漏。

**Blocked by:** None — 根因已通过实测确认（见 Answer），可以立即开始修复。

**Status:** needs-triage

## 症状（2026-09-06 实测）

- cola 用 Lazy Start 拉起自己的 opencode（`--port 4096`）后，每轮对话结束
  `reconnect_poll_loop` 的 `reconcile` 都会打
  `yielding: terminating own OpenCode server (pid X)`，杀掉自己的 server。
- 用户未启动 OpenChamber 时（无真实 Coexistent Server 可让位），Yield 照样
  触发 → 下一轮对话又要重新拉一个 Owned Server → 再被杀 → 6 个 zombie 挂在
  cola 下。期间权限/问答 poller 持续 502。
- 附带问题：`spawn_self_server` 丢弃 `Child` 从不 `.wait()`，被杀死的 Owned
  Server 变 zombie 永不回收（可拆成独立 issue，或在本 issue 一并修）。

## 根因（已实测锁定）

用与 cola 相同的 sysinfo 0.39 逻辑（`refresh_processes_specifics(
ProcessesToUpdate::All, true, refresh)`）独立复现：

- `/proc` 中真实存在的 `opencode serve` 进程只有 **1 个**（pid 71849）。
- sysinfo 却报告 **29 个**候选，全部 `port=45491`。
- 多出的 28 个候选的 `Tgid` 全部 = 71849 —— 它们是**同一个进程的 28 个
  线程**（bun/opencode 的 `mi-scavenger`、`HeapHelper`、`Bun Pool N` 等）。
- 线程与主进程共享 cmdline（`/usr/sbin/opencode serve --port 45491`），所以
  全部通过 `scan_processes` 的 `is_server` 过滤。
- 线程 pid 频繁创建/回收 → 每次 scan 的候选数在 22~29 之间浮动；已退出的线程
  pid 因 `remove_dead` 未及时清理，看起来像"幽灵 pid 持续增长"。

结论：**sysinfo 0.39 的 `ProcessRefreshKind::nothing()` 默认 `tasks: true`
（"Process by default includes all tasks"），导致 `refresh_processes_specifics`
把 `/proc/<pid>/task/*` 里的线程全部作为独立 `Process` 返回**。cola 的
`scan_processes` 没有过滤线程，把这些线程当成 default-store server 候选。

## 触发路径

1. `scan_processes()` 返回 28 个线程候选 + 1 个真 server，全部 `port=45491`。
2. `reconcile` 里 `pick_server(&candidates, pref, self_pid)` 的 `coexist` 集合
   非空（线程候选 pid ≠ self_pid）→ 返回其中一个线程候选。
3. `pick_is_owned = (self_pid == Some(thread.pid))` = false → 进入
   `reap_owned_server`（Yield 分支）→ 杀掉 cola 自己的 Owned Server。
4. 即使没有 OpenChamber，线程候选依然让 `coexist` 非空 → Yield 无条件误杀。

## 修复方案（已实测评估）

- **首选：`ProcessRefreshKind::nothing().without_tasks()`**。`tasks: true` 是
  默认值，`without_tasks()` 显式关闭后，sysinfo 不再把线程当作独立进程。
  实测：29 个候选 → 1 个（只剩真 server 71849）。一行改动，最小侵入，不依赖
  平台细节（macOS/Windows 的 `thread_kind()` 返回 None，但 `without_tasks()`
  在各平台语义一致）。改动点：`discovery.rs::scan_processes` 第 86 行。
- 排除 `ProcessesToUpdate::Some` 的调用点（`process_cmd`、`process_alive`、
  `main.rs::process_identity`）：它们按具体 pid 查询，不受线程遍历影响，无需改。
- **已否决的备选**：
  - `/proc` 存在性复核：线程在 `/proc/<tid>/` 也存在，过滤不掉；且跨平台要写
    平台分支。
  - 端口监听探测：能区分真 server，但要对每个候选做 connect/lsof，重且慢。
  - 升级 sysinfo：`tasks: true` 是设计行为非 bug，升级无效。
- 回归测试：给 `scan_processes` 补一条"同 cmdline 的线程不产生候选"的断言
  （现有 `pick_server` 测试可复用），或验证 `without_tasks()` 后候选只含
  Tgid。

## Answer

**已实现。** `src/bridge/discovery.rs`：
- 新增 `server_scan_refresh()` 函数（`ProcessRefreshKind::nothing()
  .without_tasks()`），`scan_processes` 使用它刷新进程列表。线程不再作为
  server 候选，Yield 不再误杀 Owned Server。
- 回归测试 `server_scan_refresh_excludes_tasks`。

**验证**：实际环境（OpenChamber 的 opencode 服务）用与 `scan_processes` 相同
逻辑实测：`without_tasks()` 前 29 个候选（28 个是线程）→ 后 1 个（只剩真
server）。fmt / clippy `-D warnings` / 358 tests / release 构建全绿。修复后的
cola 已部署运行。

**测试取舍说明（code review 记录）**：spec 第 66-68 行要求行为级断言（调用
`scan_processes` 验证候选只含 Tgid）。实现采用配置级断言
（`!server_scan_refresh().tasks()`），理由：行为级测试需在测试环境构造一个
真实的多线程 `opencode serve` 进程并运行真实 scan（依赖外部二进制 + 端口 +
进程生命周期），成本高且脆弱；而 `without_tasks()` 是 sysinfo 的确定性开关，
配置级断言已锁定"不枚举线程"这个根因，行为正确性已由上面的实际环境实测
（29→1）证明。行为级测试成本超出收益，故未补，此处记录取舍。

## Comments

- 2026-09-06: 现场定位。复现脚本 `/tmp/oc-scan/`（独立 Cargo 项目，sysinfo
  0.39 + 与 `scan_processes` 相同逻辑），对照 `/proc` 实况确认幽灵进程。
  临时在 `pollers.rs::reconcile` 加 debug 日志（已还原）输出
  `candidates=[(pid,port)...]` 直接观察到 28 个幽灵。
- 2026-09-06（第二次深挖）：发现幽灵并非"死进程缓存"——前 4 个 pid 的
  `Tgid` 全部 = 71849（真 server），是 **bun/opencode 的内部线程**；后续
  "消失"的 pid（256048/259187/306140）是已回收的线程。读 sysinfo 0.39.6
  源码确认：`ProcessRefreshKind::nothing()` 默认 `tasks: true`（"Process by
  default includes all tasks"），`refresh_procs` 遍历 `/proc/<pid>/task/*`
  把线程当独立 Process，key 为 TID。验证脚本 `/tmp/oc-scan2/` 实测：
`without_tasks()` 后候选从 29 降到 1（只剩真 server）。修复方案定为
   `without_tasks()`。
- 2026-09-07: 实现完成并部署。配套的进行中对话自愈（`pollers.rs` 的
  `heal_when_busy`）是独立设计（grill Q1-Q10），不在本 issue 范围内——它属于
  `.scratch/server-scan-ghosts/` 的后续或单独 issue。code review：Standards 无
  硬性违规；Spec 指出回归测试为配置级而非行为级（取舍见 Answer），
  `heal_when_busy` 为 scope creep（已知，另立）。