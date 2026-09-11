# cola Lazy Start（自建 Owned Server）路径下静默挂死

Status: ready-for-agent
Type: task

## 一句话

没有共享 OpenCode server 时，cola 收到 Feishu 消息 → 自己 spawn 一个 `opencode serve` → 之后**完全静默**：没有 prompt 写进服务端 store，没有错误日志，没有 Feishu 卡片，进程既不退出也不再响应后续消息。attach 到外部已有的 server 则一切正常（历史 300+ 回合全部如此）。

## 运行环境（注意二进制与 HEAD 不一致）

- 仓库 `/root/workspace/dev/cola`，branch `main`，HEAD `a2e532c`（今天 10:45 提交：crate 改名 colark + crates.io 发布 CI）
- **实际运行的二进制不是 HEAD 的 build**：`target/release/cola`，mtime 2026-09-11 01:09，启动横幅 `cola 0.8.0-dev chore/deps-tungstenite@39900c1 ⚠`（即 build 于 commit 39900c1，`chore(deps): bump reqwest from 0.12.28 to 0.13.5`，分支 chore/deps-tungstenite）
- 日志 `/root/.cola/cola.log`（**时间戳是 UTC**，本地 CST = +8h）、`/root/.cola/cola.stderr.log`、lock `/root/.cola/cola.lock`、映射 `/root/.cola/sessions.json`
- OpenCode server 自建时的命令行/env：`opencode serve --port 4096 --hostname 127.0.0.1`，`OPENCODE_SERVER_PASSWORD=cola-secret`，**是 cola 的子进程**（ppid = cola 的 pid）

## 复现步骤（确定性）

1. 杀掉所有 cola 和 opencode（`pkill -x cola; pkill -x opencode`），确认 4096 无监听
2. 只启动 cola：`cd /root/workspace/dev/cola && ./target/release/cola`
   → 启动日志出现 `No shared OpenCode server running; lazy start — attach to a server or spawn an Owned Server at the first prompt`
3. 在 Feishu 给 bot 发任意一条消息
4. 现象：`lazily started own OpenCode server at http://localhost:4096` 之后**再无任何输出**，进程仍在，消息无响应

两次复现（01:52 UTC 与 04:52 UTC）特征完全一致。

## 证据（第二次复现，04:5x UTC）

### 1. cola 日志原文（最后几行即静默点）

```
2026-09-11T04:48:46.053606Z  INFO cola: Acquired singleton lock at /root/.cola/cola.lock
2026-09-11T04:48:46.058530Z  INFO cola: No shared OpenCode server running; lazy start — attach to a server or spawn an Owned Server at the first prompt
2026-09-11T04:48:46.413065Z  INFO cola::feishu::ws: Connected to Feishu WebSocket
2026-09-11T04:52:42.722480Z  INFO cola::feishu::ws: Message: chat=oc_0249606bcb5f0f5af5113612f5c78672 type=p2p thread=- text=现在应该是自己起的 opencode server 了吧，能响应吗？
2026-09-11T04:52:43.954385Z  INFO cola::opencode::client: reconnected opencode client to http://localhost:4096
2026-09-11T04:52:43.954435Z  INFO cola::bridge::pollers: lazily started own OpenCode server at http://localhost:4096
（之后无输出；04:53:51 采样时已静默 70s，更晚仍然无输出）
```

`cola.stderr.log` 只有自建 server 的 stdout：`opencode server listening on http://127.0.0.1:4096`（无 panic、无 bail error）。

### 2. prompt 从未到达服务端 store（最硬的证据）

直接查 SQLite（服务端存储）：`/root/.local/share/opencode/opencode.db`

```sql
-- 消息到达时刻 2026-09-11T04:52:42.722Z = 1789102362722
select count(*) from message        where time_created > 1789102362722;  -- 0
select count(*) from part           where time_created > 1789102362722;  -- 0
select count(*) from session_message where time_created > 1789102362722; -- 0
-- 最新一条 message 仍是上一轮成功回合：
-- msg_08eb0d73a0016oXkYfH5ywJwX0  ses_f717c779effevGQjgxGk5WkS45  time_created=1789100349242 (= 04:19:09Z)
```

服务端自己的日志 `/root/.local/share/opencode/log/opencode.log` 在 04:52:46 之后只有 instance bootstrap，没有任何 session/prompt 活动：

```
04:52:43.821  loading path=/root/.config/opencode/config.json      （server 启动）
04:52:46.112  "creating instance" directory=/root/workspace/dev/cola
04:52:46.254  "creating instance" directory=/root
04:53:46.384  cleanup prune=7.days                                  （60s 周期清理）
```

**含义**：至少有两个 HTTP 请求确实到达了自建 server（触发了两个 instance 的 bootstrap），但**没有任何 prompt 被创建**。所以不是"请求根本发不出去"，而是"prompt 那一步永远没执行到"。

### 3. 进程状态：await 永不完成（不是死循环、不是 panic）

```
cola pid 40518，9 线程：
  8 × state=S syscall=202 wchan=futex_wait_queue
  1 × state=S syscall=232 wchan=do_epoll_wait
CPU：3 秒 4 ticks（≈0）
/proc/40518/io: rchar 13,456,964  wchar 66,098  read_bytes 0  write_bytes 16,384
```

第一次复现（01:52）同形：9 线程全 futex/epoll，CPU 几乎为 0。

### 4. 鉴权事实（自建 server 与 attach 的 server 差在这里）

自建 server 带 Basic auth；对当前自建 server 实测：

```
curl http://127.0.0.1:4096/session                       -> 401
curl -u opencode:cola-secret http://127.0.0.1:4096/session -> 200
curl -u foo:cola-secret      http://127.0.0.1:4096/session -> 401
```

即**用户名必须是 `opencode`**（与 `AGENTS.md` 已知坑 #12 一致：`authorized()` 同时校验 username 和 password，client 只在两者都设置时才发 Authorization 头）。外部 attach 的那个 server 是无密码的，所以 attach 路径不受影响。

⚠ 注意：`creating instance` 是请求驱动产生的，无法据此断定那次请求返回的是 200 还是 401（auth 失败是否仍会 bootstrap instance 需要确认）。这也是一个明确的待验证点。

### 5. 未解异常：socket 计数器（存疑，不要当结论）

同一时刻 cola 到自建 server 的连接（`ss -tnpi`）：

```
127.0.0.1:59442 -> :4096  bytes_sent 38,302   bytes_received 67,537,456
127.0.0.1:59448 -> :4096  bytes_sent 23,359   bytes_received 33,772,808
127.0.0.1:54538 -> :4096  bytes_sent    217   bytes_received        218   （一次请求后闲置）
```

即 70 秒内约 100MB 被"接收"。但同时：

- cola 自身 `/proc/40518/io` rchar 只有 13.4MB（进程生命周期总量），根本读不了 100MB
- 自建 server `/proc/40751/io` wchar 只有 7,077,908（7MB）
- 服务端日志在 04:52:46 之后没有任何请求活动

三者互相矛盾，怀疑 WSL2 loopback 下 `ss` 的 per-socket 计数不可靠。**如果你能用 strace/tcpdump 定位这条连接在传什么，可能直接破案；不能定位的话，请以 2、3 两条为准。**

另注：两次复现里都有"一条 socket 发 217 字节、收 218 字节后彻底闲置"的同一签名（第一次是 fd 11，这次是 fd 10），这个 217/218 值得单独看一眼（像是一次很小的请求 + 一次性小响应后就再没复用过该连接）。

## 代码入口（按执行顺序）

- `src/bridge/handler.rs:331` `handle_prompt`；`:352` 调 `ensure_server`；`:418` `get_or_create_session`；`:426` `session_subtitle`；`:437` inflight 判定；`:483`/`:502` `run_prompt`
- `src/bridge/pollers.rs:190` `ensure_server`（返回 `Ok(true)` 的分支）；`:105` `reconcile`（自建分支 `:112-:131`：`discovery::spawn_own_server` → `core.opencode.reconnect(&spawned.url, &spawned.password)` → 打日志 → `return Ok(true)`）；`:175` `reconnect_poll_loop`（每 `RECONNECT_POLL_INTERVAL_SECS` 拿一次 `core.server_lock`，而 `reconcile` 内部还会调 `busy(core)`）
- `src/bridge/render.rs:13` `session_subtitle`（第一个业务 HTTP 请求：`session_info`）
- `src/opencode/client.rs:456` `messages()`（`GET /session/{id}/message`）
- `src/bridge/discovery.rs` `spawn_own_server` / `username_from_env`（自建路径下 username/password 的装配）
- 参考 `AGENTS.md` 已知坑 #12（Basic auth 必须两个字段都对）

## 待验证假设（按可疑度排序，均未证实）

1. **自建路径的 Basic auth 装配**：`reconnect(&spawned.url, &spawned.password)` 只传了 password（不含 username），若 client 因此不发 Authorization 头 → 全 401 → 静默失败。请核对 `reconnect` 的签名与 `username_from_env` 在自建路径下的取值，并确认 401 后的错误路径为什么没有日志。
2. **`ensure_server` 返回之后到第一个业务请求之间的 await 永久 pending**：重点查锁序——`reconcile` 在持有 `core.server_lock` 期间调 `busy(core)`，`reconnect_poll_loop` 也拿 `server_lock`；handler 路径还涉及 `inflight` 与 `sessions` 两把锁，检查是否存在锁序反转导致互等。
3. **reqwest 0.13 大版本升级**（二进制正好 build 在 bump reqwest 的那个 commit）：用 `git log` + 二进制/进程时间线确认上一版成功回合用的是哪个 build，排除连接池/超时语义变化。
4. 第 5 节的 socket 计数异常（可能只是计数不可靠，也可能与 2 相关）。

## 要求

- 先复现、拿到根因（哪一行 await 不返回 / 哪个请求返回了什么），再改
- 修复后：无外部 server 时，首条消息在 Lazy Start 下能正常走完一轮
- 补回归测试；本地跑 `cargo fmt --all -- --check`、`cargo clippy --workspace --all-targets -- -D warnings`、`cargo test --workspace --locked`、`cargo build --release --locked`
- 不改动无关接口语义；遵守 `CONTRIBUTING.md` / `CODING_STANDARDS.md`
- 建议把日志级别打到 debug 再复现一次（当前静默处没有任何 WARN/ERROR）

## 对照：attach 到外部 server 一切正常

预先启动 `cd /root/workspace/dev/cola && opencode serve --port 4096 --hostname 127.0.0.1`，再启动 cola：

```
INFO cola: Attached to OpenCode server at http://localhost:4096
INFO cola::feishu::ws: Connected to Feishu WebSocket
```

同一条消息（04:52 那条，重发于 02:01:41 那次）收到后 8 秒即出现 `render poll: 1 new parts`，回合正常完成。历史上 300+ 次成功回合全部是这一模式。

## Comments

### 2026-09-11 修复完成（PR: fix/lazy-start-silent-hang）

**根因**：`opencode serve`（本机 opencode 构建）在 TCP listen 后约 1 秒内存在启动窗口——落入窗口的 HTTP 请求被读取但永不分发（连接永久无响应）。cola 的 Lazy Start 路径里 `wait_for_port` 在 TCP accept 瞬间返回，`session_subtitle` 的 `session_info`（t+0 毫秒级）必中窗口 → 消息任务永久 await（无超时）→ 静默挂死。独立实验确认：同一服务器 t+0s 请求 WEDGED、t+1s+ 全部 200（最快 4ms）。attach 模式因服务器早已启动完成而永不触发。

**修复**（两层防御）：
1. `pollers.rs`：`wait_for_server_ready` —— Lazy Start spawn 后轮询 `list_sessions`（每次 1.5s 有界、20s 总限）直到服务器真正分发请求；失败则回收自建 server + 回 serverless（避免下一条消息重新附着到坏 server），并向用户报可见错误
2. `render.rs`：`session_subtitle`/`refresh_session_title` 的 `session_info` 加 3s 超时，降级为 id-tail

**回归测试**：`subtitle_degrades_when_session_info_hangs`、`readiness_wait_recovers_after_wedged_attempts`、`readiness_wait_fails_when_server_never_serves`

**验证**：fmt / clippy / 487 tests / release build 全绿；真机验证——无外部 server 时首条消息在 Lazy Start 下正常走完回合（07:44 实测）。
