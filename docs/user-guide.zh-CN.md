# cola 用户使用手册

[English](user-guide.md) | **简体中文**

英文版是唯一事实来源；本译本与它的结构一致性由 `cargo xtask check-doc-drift` 守护。

这是日常运行 cola 的完整操作手册。安装与第一次对话请看 [README](../README.md)。

## 运行时文件

所有运行状态默认位于 `~/.cola/`：

| 文件 | 用途 | 可覆盖方式 |
| --- | --- | --- |
| `cola.toml` | 配置 | `./cola.toml`（当前工作目录）或 `cola --config <path>` |
| `cola.log` | 日志（追加写入，按日轮转） | `cola --log-file <path>` |
| `sessions.json` | 飞书话题 ↔ 会话映射 | `[bridge] session_file` |
| `chain_records.json` | 持久链记录：实时卡片、唤醒水位、任务结束、已送达位置 | 与 `session_file` 同目录 |
| `interactive_surfaces.json` | 重启后重新接管的权限/问答卡片（ADR-0038） | 与 `session_file` 同目录 |
| `authored_turns.json` | cola 自己发起的轮次；让自行分配 id 的客户端也能区分自己的消息与外部消息（ADR-0092） | 与 `session_file` 同目录 |
| `pinned_chats.json` | 持久化的即时提醒置顶 | 与 `session_file` 同目录 |
| `terminal-runs/` | 实时/已结束 `/run` 卡片的终端运行记录 | 与 `session_file` 同目录 |
| `access.json` | 访问名单（机主；ADR-0035） | `[bridge] access_file` |
| `cola.lock` | 单实例锁（每台机器只能有一个 cola） | — |
| `restart-notify.json` | 重启通知记账 | — |

每个持久状态附属文件都带有格式 `version`（锁、日志与一次性重启记账除外）。以会话为键的记录还会标明所属的 **agent client**，因此两个客户端的会话可以共存于同一个文件。旧版本写出的文件按版本 0 读取，归入 `default` 客户端，并且只重写一次；来自**更新**版本的文件会被拒绝读取，而不是误读。因此，重命名 `[clients.<id>]` 的 id 属于**状态迁移**而非配置修改：必须把旧 id 下的记录重新键控，否则重命名后的客户端将看不到它们（ADR-0091）。

锁与配置正是 `cola` 在自动启动启动器里不带任何参数的原因：无论工作目录是什么，它都从 `~/.cola` 解析全部信息。

## 安装

从[最新的 GitHub Release](https://github.com/SXongin/cola/releases/latest) 下载对应平台的压缩包，把二进制放到 `PATH` 中一个**当前用户可写**的目录：

| 平台 | Release 资产 | 安装位置 |
| --- | --- | --- |
| Linux x86_64 | `cola-<version>-x86_64-unknown-linux-gnu.tar.gz` | `~/.local/bin` |
| macOS（Apple Silicon） | `cola-<version>-aarch64-apple-darwin.tar.gz` | `~/.local/bin` |
| Windows x86_64 | `cola-<version>-x86_64-pc-windows-msvc.zip` | `%LOCALAPPDATA%\Programs\cola` |

**校验下载（可选）。** 每个 Release 压缩包都带有 GitHub 构建来源证明（build-provenance attestation），可据此确认它由本仓库的 cola release 工作流构建：

    gh attestation verify cola-<version>-<target>.tar.gz --repo SXongin/cola

（`gh` 即 [GitHub CLI](https://cli.github.com)。）完整性另有 Release 的 `SHA256SUMS` 覆盖。

**为什么要放在用户可写目录？** cola 的自更新会就地替换自己的二进制（`/update`、`cola update`），这需要二进制所在目录可写。放在 root 拥有的位置（如 `/usr/local/bin`，或 `C:\Program Files`）cola 能正常运行，但**无法自更新**——每次发版都得手动替换。（cargo 安装不受影响：它们通过 cargo 更新。）

**macOS。** Release 构建只有 Apple Silicon（`aarch64`）：2020 年末以来售出的 Mac 全部是 ARM，且 macOS 26 Tahoe 是最后一个支持 Intel Mac 的 macOS 版本。`aarch64` 二进制无法在 Intel Mac 上运行（Rosetta 只把 x86 翻译成 ARM，不能反向）——Intel 用户可以从源码构建。下载的二进制未签名，解压后请清除 Gatekeeper 的隔离标记：

    xattr -d com.apple.quarantine ~/.local/bin/cola

**Windows。** 建议选择 `%LOCALAPPDATA%\Programs\cola` 而不是 `%USERPROFILE%\bin`：`AppData\Local` 用户可写且**不参与漫游**，不同于 `AppData\Roaming`（后者会把二进制通过域漫游配置文件同步）；而 `Programs\` 是每用户应用惯用的目录。解压 `cola.exe`，把该目录加入用户 `PATH`（设置 → 系统 → 关于 → 高级系统设置 → 环境变量），然后重开终端。

**crates.io。** 有 Rust 工具链时可以从 [crates.io](https://crates.io/crates/colark) 安装：`cargo binstall colark`（需要 [cargo-binstall](https://github.com/cargo-bins/cargo-binstall)）会下载对应平台的预编译 Release 二进制——无需编译；`cargo install colark` 则从源码构建。两种方式得到的二进制都叫 `cola`。源码构建需要 C 编译器与链接器——TLS 栈会构建 AWS-LC；在 Windows 上还需要 NASM（或设置 `AWS_LC_SYS_PREBUILT_NASM=1`）；不需要系统 OpenSSL。在精简的 Linux 镜像上，请安装 `ca-certificates`，以便飞书 WebSocket 握手能验证服务器。在没有预编译资产的平台上（Linux aarch64、Intel macOS），binstall 会回退为从源码编译；`cargo install colark` 直接走这条路。由 cargo 跟踪的二进制（binstall 安装也会写入 cargo 的安装记录）通过 cargo 更新，而不是 cola 的自更新——见[自更新](#自更新)。

**其他平台。** Linux aarch64 与 Intel macOS 没有预编译资产；通过 GitHub 渠道自更新时会报告 "no asset for this platform" 而不是失败（cargo 安装不受影响——它们通过 crates.io 更新）。从源码构建（前置条件与上面的 crates.io 安装相同）：`cargo build --release`；`cargo install --path .` 也会把二进制放进用户可写目录。

还要确保 `PATH` 中有 `opencode` 二进制——cola 会发现并启动它。OpenCode **1.18.x（V1）与 2.0.x（V2）都受支持**：cola 在每次附加/重连时探测所连接服务器的协议并记录结果（下面的 `generation` 设置是少见的覆盖手段）。自动启动启动器会在 `cola autostart enable` 时快照你的 `PATH`，所以请在启用自动启动**之前**把两者都装好。

## 飞书应用配置

cola 以企业自建应用的身份与飞书通信。配置是一次性的，在[开发者后台](https://open.feishu.cn/app)完成；[README 快速开始](../README.md#try-it) 有一份精简清单——本节是完整说明。

### 1. 开启机器人能力

应用必须具备**机器人**能力（应用能力 → 机器人）。没有它，应用无法被加入聊天、无法接收消息、也无法以机器人身份发送——同时它也是消息与资源类 API 的硬性前置条件。

### 2. 申请权限

| 权限 | 飞书权限名称 | cola 用它做什么 | 缺失后果 |
| --- | --- | --- | --- |
| `im:message` | 获取与发送单聊、群组消息 | 发送/回复消息并更新卡片；按 id 读取消息（引用上下文）；下载消息图片 | cola 无法回复或更新卡片——完全不可用 |
| `im:message:send_as_bot` | 以应用的身份发消息 | 以机器人身份发送和回复消息 | 卡片发送失败 |
| `im:message.p2p_msg:readonly` | 读取用户发给机器人的单聊消息 | 接收私聊消息 | 私聊消息永远到不了 cola |
| `im:message.group_at_msg:readonly` | 接收群聊中@机器人消息事件 | 接收 @ 机器人的群聊消息 | 群聊消息永远到不了 cola |
| `im:chat:readonly` | 获取群组信息 | 聊天显示名（`/attach` 拒绝卡片、`/switch` 卡片） | 只能显示原始 chat id 而不是名称 |
| `contact:contact.base:readonly` | 获取通讯录基本信息 | 授权通讯录 API 调用 | 无法获取用户名称 |
| `contact:user.base:readonly` | 获取用户基本信息 | 返回用户的 `name` 字段 | 完成通知失去 @名字 |
| `im:datasync.feed_card.time_sensitive:write` | 设置即时提醒 (`time_sensitive`) — 可选 | 即时提醒：在权限/提问等待期间置顶聊天或话题（`[bridge] instant_reminder`，默认关闭） | 跳过置顶（只记一行日志）；其他一切照常 |

说明：

- `im:datasync.feed_card.time_sensitive:write` 是唯一**可选**权限：
  它为会话级的即时提醒提供支持，这是一个可选功能
  （`[bridge] instant_reminder = true`，默认关闭）。在同一 `instant_reminder`
  开关下，等待中卡片的消息置顶不需要额外权限——`im:message` 已覆盖。只有你
  需要会话置顶时才授予它；没有它 cola 会记录失败调用并继续运行。
- `im:chat`（获取与更新群组信息）是 `im:chat:readonly` 的超集——cola 只
  读取群组信息，所以只读权限就是最小权限。
- 通讯录 API 需要两个权限：一个用于授权调用
  （`contact:contact.base:readonly`），一个用于返回 `name` 字段
  （`contact:user.base:readonly`）。
- 按 id 读取消息与下载消息图片仅凭 `im:message` 就能完成——
  媒体不需要额外权限。

### 3. 订阅事件

- **事件与回调 → 回调配置**：选择**使用长连接接收事件**（长连接模式）。
  cola 启动时会作为 WebSocket 客户端连接，所以**请先运行 cola 再保存该设置**——
  只有在有长连接客户端连接时，飞书才接受该模式。
- 订阅 `im.message.receive_v1` 事件（接收消息）。卡片按钮回调
  （`card.action.trigger`）会经同一条连接自动到达——无需配置 webhook URL。
- 长连接模式仅适用于企业自建应用（不适用于应用商店应用），且每个应用
  最多允许 50 条连接——cola 占用一条。

### 4. 发布

权限授予与事件订阅只有在**发布新版本**（版本管理与发布）之后才会生效。企业自建应用可以自行发布，无需应用商店审核。你不需要在每次 cola 更新时重新发布——只有改动应用的权限或事件时才需要。

## 配置

查找顺序：`cola --config <path>`（原样使用）→ `./cola.toml` → `~/.cola/cola.toml`。
只有 `[feishu]` 是必填的。

### `[feishu]` —— 必填

```toml
[feishu]
app_id = "cli_xxxxxxxxxxxx"
app_secret = "your-app-secret"
```

### `[opencode]` —— 全部可选

```toml
[opencode]
url = "http://localhost:4096"    # preferred/fallback port (default 4096)
start_server = "auto"            # auto (default) | never | eager
# generation = "auto"            # auto (default) | v1 | v2
# model = "opencode/deepseek-v4-flash"
```

- **`url`** —— 只是一个*首选*端口。cola 会自动连接到**共享存储**上已在运行的
  `opencode serve`（用户名与密码直接读自运行中的服务器本身——它的环境变量，
  或 V2 Shared Service 的注册文件——所以无需配置），因此 `url` 只是同类多个服务器
  之间的优先级选择——也是 cola 启动自己的服务器时的回退端口。在 cola 可连接的
  服务器中，优先级是 **Shared Service > Coexistent Server > Owned Server**：
  V2 的 `serve --service` 最优先，其次是其他工具启动的服务器，最后才是
  cola 自己的。
- **`start_server`** —— cola 何时可以启动自己的 `opencode serve`：
  - `auto`（默认）—— 启动时只附加；仅当消息需要服务器而当时没有任何服务器
    在运行时，才启动自己的服务器。
  - `never` —— 只附加；没有服务器时 cola 回复 OpenCode 不可用。
  - `eager` —— 旧行为：启动时没有服务器就启动自己的服务器。
- **`generation`** —— cola 使用哪一代 OpenCode 协议：`auto`（默认）或强制的
  `v1`/`v2`。在 `auto` 下，cola 在每次附加/重连时探测所连接的服务器并记录
  证据（要查找的日志行见 [FAQ](#faq--故障排查)）。OpenCode 1.18.x（V1）与
  2.0.x（V2）都受支持；强制指定某一代只用于探测无法归类的代理或特殊构建，
  而与之矛盾的探测仍会带证据记录一条警告。在 `auto` 下，无法归类的服务器
  会让 cola 保持无服务器状态，而不是去猜测。
- **`model`** —— 新会话的默认模型（`provider/model`）。不设置（多数环境下
  推荐）→ cola 不固定任何模型，由 OpenCode 服务器使用**它自己的**默认模型。
  若设置，该模型必须在 cola 连接的服务器上存在（通常是 `opencode/...`）。
  会话级覆盖用 `/model` 设置。

### `[clients.<id>]` —— 可选，用于第二个 Agent Client

```toml
# `[opencode]` 已经是隐式 Agent Client `default`——不要改名。
[clients.ada]
kind = "codex"                   # 必填：opencode | codex
# sandbox = "danger-full-access" # 可选：read-only | workspace-write | danger-full-access
# approval_policy = "on-request" # 可选：untrusted | on-request | never
# apply_on_adopt = false         # 可选：在 thread/resume 上也发送这一对
# reasoning_summary = "auto"     # 可选：auto | concise | detailed | none

[bridge]
default_client = "ada"           # 哪个 Agent Client 处于活动状态（默认 "default"）
```

- **`kind`** —— 必填，用于选择适配器。`opencode` 是 OpenCode 适配器；
  `codex`（`app-server` 适配器，ADR-0090）能解析成功、能在 `$CODEX_HOME`
  下发现守护进程并探测其接口。两种 kind 都能启动：活动的 Agent Client 就是
  cola 所服务的那个，而 Codex 客户端需要其 app-server 守护进程可连接
  （或按 `start_server` 可启动）。agent 主动发起的提问只有在 daemon 侧的
  `~/.codex/config.toml` 中启用 `[features] default_mode_request_user_input =
  true` 时才会到达 Feishu；修改后需重启 daemon 与 TUI。未启用时模型只会在
  文本中提问。MCP elicitation 请求（`mcpServer/elicitation/request`）不受该
  特性影响。
- **选项与 `kind` 写在同一个表里。** `opencode` 的 Agent Client 接受 `url`、
  `start_server`、`generation` 与 `model`，和 `[opencode]` 完全一样；
  `codex` 的 Agent Client 接受 `start_server`（`auto`/`never`/`eager`）：
  `never` 只附加，其余取值允许通过 `codex app-server daemon start` 启动共享
  守护进程——cola 从不拥有、也从不回收它。它还接受 `sandbox`、
  `approval_policy`、`apply_on_adopt` 与 `reasoning_summary`（下面逐项说明）。
  具名的 Agent Client 不会继承 `[opencode]` 的取值。
- **`sandbox` / `approval_policy` / `apply_on_adopt`（仅 codex）** —— Codex
  线程运行在什么策略下。cola 在 `thread/start` 上发送这一对，因此它创建的
  线程可以在这个仓库里开发：`sandbox = "danger-full-access"` 去掉沙箱及其
  网络限制，这正是 `git commit`、`gh` 以及修改 `.codex/` 能工作的原因
  （桌面端的“完全放行”就是这个模式）。另外两个模式是 `workspace-write` 与
  `read-only`；`workspace-write` 会保留 Codex 的受保护路径（`.git`、`.agents`、
  `.codex`、`.aws`）只读，且其命令网络受限，所以它并不能让 `git commit` 或
  `gh` 变得可用。`approval_policy` 取 `untrusted`、`on-request` 或 `never`。
  代价是真实的：`danger-full-access` 没有可逃逸的沙箱，所以即使
  `approval_policy` 是 `on-request`，**该会话也不再收到权限卡片**——它运行的
  一切拥有守护进程用户的权限（以 root 运行 cola 时，线程就能写 root 能写的
  任何位置）。`approval_policy = "never"` 同样会抑制卡片，与沙箱无关；带沙箱
  的客户端仍保留写入与网络限制。`apply_on_adopt`（默认 `false`）把这一对也带到
  `thread/resume`：被接管的线程可能与你的 TUI/VSCode 共享，所以除非你显式
  选择，cola 只接管、从不重新配置。未设置的键不发送任何字段，由守护进程自己
  的默认值生效；`cola config show` 会打印文件里写了什么。这些是**线程**设置：
  改动之前创建的线程会保留它创建时的策略，你的 TUI 创建、被 cola 接管的线程
  也一样——除非 `apply_on_adopt = true`。
- **provider 只在创建时选择。** `/new`（Pending Session）与 `/fork` 是唯一选择
  provider 的路径；它们把 `model` + `modelProvider` 成对交给 `thread/start` 或
  `thread/fork`（`turn/start` 没有这个字段）。活动线程的 provider 不会改变：
  `/model` 选到不同 provider 时**不发送任何请求**，并提示 `/new` 或 `/fork`。
  活动线程的 provider 页是只读的——只显示当前 provider 与这条说明，没有可点的
  rows；Pending Session 仍提供创建时会带上的成对行。provider 不能离开模型单独
  选择：把旧模型名留在新 provider 上会 404。
- **`reasoning_summary`（仅 codex，默认 `auto`）** —— 模型的思考过程有多大程度
  被总结成文字。cola 把它作为 `thread/start` 的
  `config.model_reasoning_summary` 发送，卡片会把结果渲染成可折叠的 💭 推理
  过程面板；取值有 `auto`、`concise`、`detailed` 与 `none`。Codex 在固定版本
  上的默认值是 `none`，这会让每个 reasoning item 都是空的，卡片上完全看不到
  思考——cola 的默认值因此改为 `auto`。`none` 恢复静默（也省掉总结产生的
  token）。和沙箱那一对一样，它是**线程**设置：只作用于新线程；被接管的线程
  保留自己的设置，除非 `apply_on_adopt = true`。只有文件里显式写了它，
  `cola config show` 才会打印。
- **给其他客户端一个全放行默认值是 Codex 自己的配置，不是 cola 的事。** 如果
  你自己的 TUI/VSCode 线程也应默认全放行，请在守护进程所用 `$CODEX_HOME` 的
  `~/.codex/config.toml` 中设置 `default_permissions = ":danger-full-access"`，
  或者每次运行加 `-s danger-full-access`；然后重启守护进程并新建线程——已有
  线程会重新应用它们创建时的设置。cola 从不写这个文件：它带有你的 provider
  token，并与守护进程共享。两点后果值得注意。该键只影响共享同一个
  `$CODEX_HOME` 的客户端；而在 Codex 的 profile 下（`:workspace`），
  `[sandbox_workspace_write] network_access = true` 会被忽略——profile 的受限
  网络优先，这正是那里 `gh` 连不上 `api.github.com` 的原因。只需要偶尔全放行
  时，优先用每次运行的 `-s`。
- **只接受该 kind 自己的键。** 其他任何键——例如拼错的 `model_typo`，或嵌套的
  `[clients.ada.opencode]` 表——都是配置错误，并会指出
  `clients.<id>.<key>`；写在该 kind 会忽略的键上（例如 `kind = "codex"` 配
  `model`）同样是配置错误。绝不会被静默忽略。
- **`default_client`** —— 活动 Agent Client 的 id。不设置时是 `default`
  （即 `[opencode]`）。每个 cola 进程只有一个活动的 Agent Client（ADR-0090）；
  切换 Agent Client 就是改这个键并重启。
- **`[opencode]` 永远有效。** cola 把它当作隐式的 `clients.default` /
  `kind = "opencode"` 读取，并且从不改写你的文件——不会生成任何
  `[clients.*]` 表，也不会重命名任何键。显式的 `[clients.default]` 会作为
  该隐式 Agent Client 的重复被拒绝：请给表换一个 id，或把它的选项移进
  `[opencode]`。
- **Agent Client id 是持久状态。** cola 会把活动 id 记录在每个会话旁，因此
  重命名 id 是一次状态迁移，而不是一次配置修改。

### `[bridge]` —— 全部可选

```toml
[bridge]
# session_file = "~/.cola/sessions.json"
# access_file = "~/.cola/access.json"
# work_dir = "/path/to/a/project"
# default_auto_accept = true
# group_completion_notice = true
# long_task_notice = true
# instant_reminder = true
# log_days = 14
# default_client = "default"
```

- **`session_file`** —— 话题↔会话映射的持久化位置。
- **`access_file`** —— 访问名单（机主，ADR-0035）的持久化位置。文件缺失表示
  机器人尚未被认领，它会拒绝每一条消息，直到 `/claim`。
- **`work_dir`** —— 对话没有活动会话时（全新聊天，或 `/switch forget` 之后），
  新会话的默认目录。默认取进程的当前工作目录。`/new` 继承活动会话的目录
  （或待建状态的目录）；`/dir` 指定显式目录。两者都只是声明会话：会话由该
  会话的下一条消息创建。
- **`default_auto_accept`** —— **默认关闭**：cola 创建的会话（全新聊天的首条
  消息、`/new`、`/dir`、`/topic`、404 重建）以自动授权启动，其工具权限请求会被
  自动应答而不是弹出卡片。接管已有会话不会开启它——会话保留自身状态；
  `/autoaccept off` 仍可按会话关闭。
- **`group_completion_notice`** —— 群聊中，向请求者回复一条简短的完成通知
  （流式卡片是就地更新的，不会推送新通知）。`false` 关闭它。私聊不需要。
- **`instant_reminder`** —— 可选，**默认关闭**：在权限请求或提问等待期间，
  用飞书的即时提醒把聊天或话题置顶到请求者消息列表的顶部。等待一解除，置顶
  立即清除；它是一个**状态**，会一直保留到你处理为止，因此不会像消息那样
  被错过。一个聊天或话题只有一个置顶：最新的等待拥有它；该等待解除时，
  下一个待处理的等待立即接管。置顶意味着「这个聊天或话题还需要你」——
  预览中的卡片标题会说明原因（等待你的授权 · 等待你的回答 · 两者合并为
  等待你的授权/回答）。需要 `im:datasync.feed_card.time_sensitive:write`
  权限；没有它时置顶会被跳过（只记一行日志），其他一切照常。在线的置顶
  会与会话映射一起持久化；若崩溃留下了孤儿置顶，启动时会清除（清除失败
  会在下次启动重试）。如果 cola 永久消失，置顶会保留——那是应用控制的
  平台状态，飞书客户端没有取消入口；把会话标记为完成可以移除它，但新活动
  会再次置顶。在同一开关下，等待中的卡片也会被消息置顶，因此打开聊天就能
  看到它。
- **`long_task_notice`** —— **默认关闭**：在私聊中，当一个轮次运行了至少
  5 分钟时，回复请求者的消息，让长任务的结束以新消息形式送达。流式卡片
  是就地更新的，既不推送通知也不把会话顶起，所以没有它时，长任务的结束很容易
  被忽略；短轮次保持安静。群聊已通过 `group_completion_notice` 在每个轮次
  结束时通知。
- **`log_days`** —— 轮转后的每日日志保留多少天（默认 14）。
- **`default_client`** —— 活动 Agent Client 的 id，见上面的 `[clients.<id>]`。
  不设置时是 `default`（`[opencode]`）。

### `[review]` —— 全部可选

```toml
[review]
# base = "origin/main"
```

- **`base`** —— `/diff` 未显式指定时用于对比工作树的基准 ref。未设置时依次尝试
  `origin/main`、`main`，两者都不存在则报错。`/diff --base <ref>` 可覆盖单次调用。

### `[worktree]` —— 全部可选

```toml
[worktree]
# root = ".worktrees"
```

- **`root`** —— `/worktree new` 创建检出所用的父目录。相对路径相对项目的**主检出**
  解析（主 worktree，而不是会话自己的目录——该会话本身可能就在某个链式 worktree
  里）；绝对路径按原样使用。默认 `.worktrees`。OpenCode 自己读的是项目
  `opencode.json` 里的 `worktree.directory`，那是桌面端/TUI 用的另一个设置；cola
  只读这里这一个，需要两方一致时请把两处设成同一个值。

## 运行

```bash
nohup cola >/dev/null 2>&1 &
```

cola 会自动连接到已在运行的 OpenCode 服务器；在默认的 `auto` 策略下，它按需
启动自己的服务器，因此无需手动运行 `opencode serve`。如果配置文件还不存在，
cola 会打印一条提示，告诉你该把它放在哪里，然后退出。

## 多客户端同步

cola 不为任何客户端做适配：同步发生在**服务器 + 共享存储 + 会话**这一层。
只要所有客户端都连同一个 default-store 服务器、并打开同一个会话，任一客户端
发出的提示词都会自动到达其他客户端。

**V2 的 canonical 服务器。** OpenCode V2 的 `opencode serve --service` 是整台机器
的会合点：TUI、桌面应用和 Web UI 都能零参数解析到它——不带参数的 `opencode`
就是那个进程。没有服务器在运行时，cola 会启动它（`start_server = "auto"` 或
`"eager"`）；否则附加到已在运行的那个。V1 没有 service，继续使用
`serve --port`。

**自动启动下的重启。** 当 cola 由 systemd **用户**单元（自动启动）拉起时，该单元默认的
`KillMode=control-group` 会在 `systemctl --user restart cola` 时、以及在 `/restart`
由该单元的进程处理时，连同 cola 一起停掉 cola 启动的 Shared Service。这是单元的 cgroup
边界，不是 cola 的泄漏：cola 把守护进程分离到自己的进程组，但单元仍然拥有它启动的每个进程。
要让 service 独立于 cola，请在 cola 之前自己运行 `opencode service start`（或在单元之外
运行 cola），也可以给单元设置 `KillMode=process`，这样重启只会停掉 cola。以其他方式启动
时，service 会比 cola 活得更久。

**OpenChamber** 默认起自己的 managed 服务器（随机端口），而不读注册文件，
因此在把它指向 service 之前不会同步：

```bash
OPENCODE_HOST=$(jq -r .url "${XDG_STATE_HOME:-$HOME/.local/state}/opencode/service.json") \
OPENCODE_SERVER_PASSWORD=$(jq -r .password "${XDG_STATE_HOME:-$HOME/.local/state}/opencode/service.json") \
OPENCODE_SKIP_START=true openchamber
```

注册文件位于 `$XDG_STATE_HOME`（默认 `~/.local/state`）——即守护进程注册时
解析到的 state home，也就是 cola 读取的那一个。`OPENCODE_HOST` 必须带显式端口、
不能带 path。`OPENCODE_SERVER_PASSWORD` 是
OpenChamber 读取服务器密码所用的名字（OpenCode 服务器也接受
`OPENCODE_PASSWORD`，但 OpenChamber 不读它）。`OPENCODE_SKIP_START=true`
让 OpenChamber 附加而不是自起服务器；去掉它就会重新启动那个随机端口的
managed 服务器。

**同一个会话是唯一的手动杠杆。** 只有同一个服务器还不够——两边必须打开同一个
会话：

| 客户端 | 加入共享会话的方式 |
| --- | --- |
| TUI | `opencode -s <会话 id>` |
| 飞书 | `/switch <id\|短哈希\|标题>` |

`/new` 会把 Active Session 切到新会话；在 TUI 里用 `-s` 打开那个新会话，两边
就会继续同步。非活动会话不会被跟随：那里的外部消息或回复不会推送到飞书
（ADR-0017）；权限/提问卡片仍会弹出，因为它们是按目录轮询的，而不是按会话。

**两个服务器。** 当有多个 default-store 服务器在运行时，cola 只确定性地选一个
——Shared Service 优先于 Coexistent Server，后者优先于 cola 自有的 Owned
Server。cola 从不聚合它们，两个服务器也不共享会话，所以不要指望两边都同步。
日志会说明选中了谁、忽略了谁，[`cola status`](#检查status--doctor--config)
会列出被忽略的服务器。

**配置语义。** `[opencode] url` 只是同类服务器之间的 tiebreaker——从来不是
pin，也无法越过 service 类。`start_server` 保持三态：`auto` 仅在消息需要服务器
时启动，`eager` 在启动时启动，`never` 只附加。`opencode service set disabled
true`（OpenCode 自己的设置）会让 cola 跳过启动 service；此时它会附加一个已在
运行的服务器，或回退到自有的 `serve --port`。

## 认领（默认私有）

全新或升级后的 cola 以**未认领**状态启动：它拒绝每一条消息。启动日志会打印
一次性的**认领码**（8 个字符，不含易混淆字形）；在**与机器人的私聊**中发送它
来指定机主：

    /claim <code>

- 在日志里找 `认领码`（`~/.cola/cola.log`，或前台运行时的终端）。认领码每次
  重启都会轮换，且从不写入磁盘。
- 群里的 `/claim` 会被拒绝——认领是私密行为。
- 认领成功后只有机主可以使用机器人；其他人会收到一句简短的拒绝。机主的
  `open_id` 保存在访问名单（`[bridge] access_file`）中，因此重启和升级
  都不会要求重新认领。
- 重建飞书应用会改变每个用户的 `open_id`：删除访问名单文件，然后用新的
  认领码重新认领。

## 自动启动

把 cola 注册为开机/登录时启动。启动的进程就是 `cola` 二进制本身（OpenCode 服务器由按需启动机制处理），因此不需要任何参数：

- **Linux**：`cola autostart enable` 写入一个 systemd **用户**单元
  （`~/.config/systemd/user/cola.service`）并启用它。运行
  `loginctl enable-linger $USER`（会作为提示打印）以便服务在没有登录桌面
  会话时也能运行。
- **macOS**：写入并加载 LaunchAgent
  （`~/Library/LaunchAgents/com.cola.bot.plist`）。
- **Windows**：写入一个 `HKCU\...\Run` 注册表值。

`cola autostart disable` 停止正在运行的实例并移除注册（见[停止](#停止)）；
`cola autostart status` 显示是否已安装。

> **重要**：`enable` 会把**当前二进制路径**和你的**当前 PATH** 快照进自动启动
> 注册。请在安装之后（移动二进制之前）运行它；如果你移动了 cola 或
> `opencode`，请重新运行。

## 停止

`cola stop` 只停止正在运行的 cola 实例，不碰其他任何东西：

- 已安装自动启动注册时，它先经平台的**监督程序**（systemd `stop` / launchd
  `bootout`）——否则带 `KeepAlive` 的 LaunchAgent 会把直接杀掉的进程重新
  拉起——然后回退为按 PID 终止单实例锁的持有者。
- 在交互式终端上它会先询问（正在运行的轮次的卡片会被截断）；`--yes` 跳过
  询问，非交互运行从不询问，因此脚本不受影响。
- 没有实例运行时打印 `cola 未运行` 并以 0 退出。cola 启动的 OpenCode 服务器
  会保持运行：它是共享存储的端点，其他客户端可能仍在使用。

`cola autostart disable` 两件事都做——先停止实例，再移除注册（停止失败也会
注销，并以非零码退出）。它还会停止仍在启动（或崩溃重启循环）中、尚未取得
单实例锁的被监督实例：决定 `disable` 停止什么的是注册，而不是锁。
`cola stop` 不动注册，所以下次开机 cola 会再次启动。

## 日志

cola 总是追加写入日志文件——默认 `~/.cola/cola.log`（不含 ANSI 转义码），
可用 `--log-file` 覆盖。重启不会清空历史（追加，而非覆盖）。日志按**天**
轮转：昨天之前的内容移到 `cola-YYYY-MM-DD.log`，更早的文件在超过
`[bridge] log_days`（默认 14）天后清除。跨天会话可用
`grep session_id=... cola-*.log` 查询。当 stdout 是真实终端时，日志也会
镜像到那里；被重定向的 stdout 不会收到 cola 日志，从而保持重定向目标干净。

`cola logs` 打印日志文件末尾（`-n N` 行，默认 50 行）；`--date YYYY-MM-DD`
读取某天轮转出的文件，`-f` 持续跟随新行，跨午夜轮转时会重新打开新文件。

## 检查（status / doctor / config）

`cola status` 打印一次性快照：运行状态、自启动、认领、解析到的配置与日志
路径、当前 Agent 客户端（id、kind、端点、连接状态与所公布的能力），以及所
连接的 OpenCode 服务器及其世代。`--json` 以相同事实输出，供脚本使用；新增的
`client` 对象承载客户端相关行，既有键保持不变。

`cola doctor` 运行支持排查所需的检查——配置解析、飞书凭证、客户端二进制与
运行时、单实例锁、日志路径、自启动二进制路径——每项带 ✅/⚠️/❌ 标记并给出
汇总，任一检查失败时以非零码退出。OpenCode 客户端仍探测共享服务器；Codex
客户端则报告 app-server daemon、你自己的客户端是否连接到它，以及运行时是否
公布了预期的能力。连接与否由 Linux 上客户端进程打开的套接字描述符（`/proc`、
`ss`）证明，而不是只看 argv：无法证明的 argv 匹配会给出警告，而带
`--no-daemon` 或指向其他端点的客户端会被明确判为失败。缺少 daemon 同样判为
失败——`start_server = "never"` 只是禁止 cola 启动 daemon；请自行运行
`codex app-server daemon start` 并让客户端指向它。完全没有本地客户端时
（纯 Feishu 部署）该检查通过，因为没有需要连接的对象。

`cola doctor` 会验证配置的 provider 能否完成一次真实回合：它在**共享**
daemon（并非 cola 所有）上创建一个临时线程并发起最小回合并等待
`turn/completed`，遇到可重试或终止性错误、或回合超出 60 秒预算未完成时判定
失败。这会写入共享 daemon，并**可能产生 provider 费用**：doctor 会创建以探针
命名的临时线程，在回合仍在运行时中断它，然后删除该线程（清理失败会报告为
警告）。`--no-turn-check` 跳过该检查；`--check-turn` 仍作为旧脚本的无效别名
被接受。所有检查都不读取 ChatGPT 账号状态——使用 provider key 的
`[model_providers.*]` 配置可以全绿。

`cola config path|show|validate` 分别打印解析到的配置路径、脱敏后的生效配置、
或做一次解析校验。`cola version` 等价于 `--version`。`cola completions <shell>`
打印补全脚本；加上 `--install` 则把脚本写入该 shell 的用户补全目录（遵循
`XDG_DATA_HOME`/`XDG_CONFIG_HOME`，`--dry-run` 只预览，省略 shell 时从 `$SHELL`
推断），且绝不覆盖不是 cola 生成的文件。自动安装仅支持 Unix；符号链接或硬链接的
补全文件、以及符号链接的父目录组件，都会被拒绝而不是跟随。PowerShell 不支持自动
安装——请重定向脚本并在 `$PROFILE` 中 source；zsh 会打印需加入 `fpath` 的目录。

## 单实例锁与重启

同一时间只能运行一个 cola（两个实例会重复处理飞书事件）。锁位于
`~/.cola/cola.lock`。

- **非交互地**在另一个实例运行时启动 cola：cola 拒绝启动并打印清晰提示。
  用 `cola --replace` 接管，或在飞书里给旧实例发送 `/restart`。
- 交互式启动（终端）时，cola 会询问
  `⚠️ 另一个 cola 实例（PID x）正在运行。是否替换它并接管？[y/N]`。
- `/restart` 会以 `--replace` 重新执行 cola 自身（保留它的启动参数与日志
  重定向），因此新进程总能接管锁。接管对处于 `exit()` 中途的旧实例是健壮的：
  不再**功能存活**的持有者（已死、僵尸进程、或正在拆除）会被回收，而不是
  阻塞重启。
- 在 systemd 单元下，`/restart`（与 `/update` 一样）不会派生新子进程——
  它以重启码退出，由单元的 `Restart=on-failure` 用同一个 ExecStart 把
  cola 拉起来。

`cola restart [-y]` 无需手动先停后启即可重启运行中的实例：当平台监督者正是
该实例的启动者时走监督者，否则读取锁持有者自身的命令行与工作目录，并以脱离
终端的方式重新执行（附加 `--replace`）。两条路径都会确认新实例已接管锁，再
报告成功。

## 自更新

`/update`（飞书）或 `cola update [--check]` 按 cola 的安装渠道更新。

- **GitHub Releases 安装**：检查 GitHub Releases，下载对应平台的二进制，
  用 Release 的 `SHA256SUMS` 校验，替换正在运行的二进制并重启。二进制所在
  目录必须**当前用户可写**（见[安装](#安装)）。在 systemd 单元下，重启交回
  `Restart=on-failure`；其他情况下 cola 重新执行自身。
- **cargo 安装**（`cargo install colark` / `cargo binstall colark`）：cola
  检测到 cargo 的安装记录，不会替换任何文件。它检查 crates.io 上的版本，
  并回复要执行的命令：`cargo install colark`——从源码编译——或
  `cargo binstall colark`，下载预编译的 Release 资产；如果 cargo 提示
  "already installed"，加上 `--force`。通过 cargo 更新能让它的安装记账
  （`cargo install --list`、cargo-update）保持真实。
- 没有预编译二进制的平台（如 Linux aarch64）会报告这一点，而不是失败。

## 飞书命令

无法识别的 `/...` 命令会转发给 OpenCode。`/help <command>` 显示其中任何一条的详细帮助。

| 命令 | 作用 |
| --- | --- |
| `/dir <path> [name]` | 声明一个以 `<path>` 为根的新会话——由下一条消息创建 |
| `/claim <code>` | 把本 cola 认领为机主（仅私聊；认领码来自启动日志） |
| `/cat <path>` | 在 File Viewer 卡片中打开机主机器上的文件：文本使用带扩展名语言提示的围栏代码块（markdown 文件保留源码，另给「📖 渲染预览」切换），`.csv`/`.tsv` 使用 markdown 表格，图片在飞书限制允许时内嵌。其他二进制文件、超过内嵌限制的图片以及超过 8 MB 预览上限的文本，仅在飞书 30 MB 上传上限内通过文件消息通道发送；超过上限则不发送，卡片会写明「未发送」。卡片标明解析后的路径、大小与类型。`/cat <directory>` 在该目录打开 Directory Browser；裸 `/cat` 回复用法（`/help cat` 提供详情）。`~` 会展开，相对路径按会话目录解析（会话待建时按当前项目目录），绝对路径按绝对路径解析，包括会话目录之外的路径 |
| `/diff [--base <ref>]` | 把当前分支的**仓库 review** 作为单个自包含 HTML 文件消息发送：相对分叉点的已提交改动、未提交的 tracked 改动，以及未跟踪文件。base 依次取 `--base`、`[review] base`、`origin/main`、`main`；cola 不会 fetch，也不会暂存任何条目（不使用 `git add -N`），也不会修改 tracked 内容；git 可能刷新 index 的 stat 数据。命令在对话当前项目目录中执行，并回复到命令消息（其聊天或话题）。没有改动时回复 `分支 <branch> 相对 <base> 没有改动`，空仓库还没有提交时回复 `仓库还没有提交，没有可对比的改动。`；非 git 目录和找不到 base 各有可读错误。同一聊天 10 秒内只允许一次 `/diff`（`稍后再试`） |
| `/dir` | 最近目录卡片：选一个文件夹并在那里声明会话，或将其作为新话题打开（每行「建话题」= `/topic <dir>` 的免打字版）；其 📁 浏览目录 按钮打开文件系统浏览器——逐级进入子目录、输入路径（前往 浏览它、选择 直接声明）、显示/隐藏点目录、过滤子目录名 |
| `/worktree` | worktree 卡片：本项目已有的 git 检出——包含主检出并标为「主检出」——每行显示分支、⚠ 脏标记、会话数与 ✅ 当前标记。只读：要把会话切到某个检出，用 `/dir <路径>`；接管其中已有的会话用 `/switch <名字>`。脏状态逐检出读取，超过 5 秒上限的行显示 `?` |
| `/worktree new <name> [--base <ref>]` | 在 `<root>/<name>` 创建 worktree，分支为 `wt/<name>`，起点是当前会话的 HEAD（或 `<ref>`），并在那里声明会话——不移动任何东西，因此正在运行的轮次不受影响。`<name>` 会被规范化为小写 ASCII slug（`[a-z0-9.-]`，最多 40 字符）；含中文的名字会被拒绝并给出示例。目录已存在时加 `-2` 后缀，分支已存在且没有检出占用时直接接上；cola 从不删除 worktree |
| `/switch` | 会话卡片：浏览 / 搜索 / 接管 / 新建 |
| `/switch <kw>` | 按名称/目录/id 切换会话（会接管他人会话；无匹配时打开预过滤的卡片） |
| `/switch <id> [--force]` | 按 id/标题接管会话 |
| `/switch forget` | 解除本聊天的会话映射（服务器上的会话保留） |
| `/sub` | 子会话卡片：当前会话的直接子会话，只读（每行 运行中/空闲） |
| `/sub list [kw]` | 同上卡片，按关键词过滤（标题/目录/id）；过滤与分页像 `/switch` 一样往返保留 |
| `/sub attach <id\|标题> [--force]` | 把当前会话的一个直接子会话接管到本聊天（会话快照回执；`--force` 抢走其他聊天持有的映射） |
| `/new [name]` | 在当前项目中声明新会话——由下一条消息创建（无会话 → 默认目录） |
| `/topic [dir] [name]` | 在 `<dir>` 中打开新的飞书话题；其第一条消息创建会话（不带参数的 `/topic` 使用当前项目） |
| `/topic --adopt <kw> [--force]` | 围绕已有会话打开话题 |
| `/name <name>` | 重命名当前会话（服务器侧，所有客户端可见；对待建状态则设置创建标题） |
| `/stop` | 中断执行 |
| `/compact` | 压缩上下文 |
| `/card` | 在轮次中间的命令回复把实时卡片埋到下面之后，把它拉回最新位置（没有正在运行的轮次时回复一条提示） |
| `/agent <name>` | 切换 agent（下一条消息生效；持久化；`--reset` 清除为服务器默认） |
| `/model <p/m>` | 切换模型（下一条消息生效；持久化） |
| `/think [level]` | 设置/清除思考等级，按模型生效（下一条消息生效） |
| `#<id> [#<id> …] [text]` | 把 OpenCode 技能加载进本条消息的提示词（如 `#implement-spec 644`）；整段消息原样提交，匹配不到技能的 `#<id>` 原样保留为正文（`#644`、`C#caveman`、`##foreman` 都不是技能记号）。裸 `/skill` 打开技能选择卡片——可按 id/名称/描述搜索、每页 20 个翻页——`/skill <id>` 是别名，用于 `#` 形式打不出的 id（`2fa-helper`、`web_scrape`） |
| `/autoaccept [on\|off]` | 查看/切换本会话是否自动放行工具权限请求。卡片无法完整显示的权限请求（例如过长的命令）仍会被拒绝，绝不批准你看不到的内容 |
| `/restart` | 重启 cola（保留启动参数与日志重定向） |
| `/restart-opencode` | 重启 OpenCode 服务器（cola 自有的服务器或 V2 Shared Service） |
| `/update` | 按安装渠道更新 cola（GitHub Releases 自更新，或 `cargo install`/`cargo binstall` 安装的 cargo 命令） |
| `/version` | 显示 cola 版本与构建来源（release / crates.io / dev 构建） |
| `/help [command]` | 列出命令，或显示某一条的详细帮助 |

说明：

- `/cat` 在 cola 运行的机主机器上读取文件字节——不经过模型，且无论 OpenCode
  是哪一代，行为都一样。查看器标明解析后的路径、大小与类型；文本显示为带语言
  提示的围栏代码块（不支持的语言提示会退化为纯文本，较大的文本预览按行分页），
  `.csv`/`.tsv` 显示为 markdown 表格，图片在飞书限制允许时内嵌。其他二进制文件、
  超过内嵌限制的图片以及超过 8 MB 预览上限的文本，仅在飞书 30 MB 上传上限内通过
  文件消息通道发送；超过上限则不发送，卡片会写明「未发送」。放不进卡片的预览可能
  使用富文本消息通道，具体渲染受飞书限制；超过该通道 30 KB 预算的内容会改用文件
  消息通道（仍受相同的上传上限限制）。卡片会说明发送结果。
  「发为文件」会把原始字节作为一条文件消息发送。文本、内嵌图片和富文本预览
  都保留此按钮；原始字节已通过文件消息发送后则不再显示。若文件消息发送超时，
  卡片会说明结果未知，并且不会重试，以免重复发送。
  对 markdown 文件（`.md`/`.markdown`/`.mark`），卡片保留源码预览，并把文档**渲染**后
  的文本放在首按钮「📖 渲染预览」后面（「查看源码」切回）。渲染体积超出卡片时不提供该
  按钮；文件消息仍是完整、无上限的渲染。
  `.html`/`.htm` **文本**文件的传输按钮会**排在首位**并显示为「🌐 预览网页」：飞书会在
  打开该文件消息时把上传的 `.html` 渲染成页面（内联脚本也会执行），因此文件消息
  才是这类文件真正的查看器——卡片组件无法渲染 HTML。
  「🔄 重新读取」会重新读取文件并就地更新查看器。「📂 所在目录」打开文件所在目录的 Directory Browser；
  从浏览卡文件行打开的查看器可用「← 返回目录」回到原列表。写入/编辑工具面板折叠时，
  标题会显示它操作的文件名；「📂 打开」按钮位于折叠卡内，展开面板后即可在查看器中
  打开对应文件。`write` 面板还会预览它写入的内容（按文件语言加代码块围栏，截断到与
  `read` 相同的 3000 字符预览，并附上同样的截断提示）。对 HTML 页面（`.html`/`.htm`）该按钮显示为「🌐 预览网页」，点击直接
  把文件作为一条文件消息发送，无需先打开查看器：面板已经展示了源码，剩下的只是它
  渲染出的页面。文件消息是供读者下载或交给客户端处理
  的文件：cola **不承诺**平台会内联预览。飞书自身的预览会渲染
  `.md` 与 `.html`（已实测），但那是客户端行为，cola 无法控制。
- **路径作用域**（`~` 展开、相对路径按会话目录解析、绝对路径按绝对路径解析）是**隐私提示，
  不是安全控制或边界**：机主进程能读取的路径都可达；不存在、无法读取和空文件都会
  带解析后的路径分别说明。示例：`/cat src/main.rs`、`/cat ~/notes.md`、
  `/cat /etc/hosts`。卡片内容对聊天里的每个人可读，因此群聊成员可以读取机主打开的
  任何内容。Access List（`/claim`）控制谁能在 cola 中**操作**（发送命令或驱动会话），
  **不控制谁能阅读**：卡片内容对聊天里的每个人可见。
- `/diff` 回复一个自包含的 `.html` **文件消息**（没有卡片）：打开后阅读分支
  review，逐文件折叠，hunk 有红绿底色。产物每次调用都重新生成，并发送到命令所在的
  聊天或话题；没有改动时不发文件，发送结果未知时不会重试（重试可能重复消息）。
  同一聊天 10 秒冷却用于阻止连点重复上传同一份 review；改完代码后想立刻重跑，
  需要稍等片刻。
- `/new`、`/dir`、`/topic` 与 `/worktree new` 只**声明**会话——会话由该会话
  下一条非命令消息在所选目录中创建并映射到这里，所以在下一条消息之前，误发的
  `/new`、`/dir` 或 `/topic` 可以用另一个 `/new`、`/dir`、`/switch` 或
  `/topic` 纠正，而不会在共享存储里留下会话。在仍在等待第一条消息的话题里，
  `/switch <id>` 则会改把该话题指向一个已有会话。`/dir` 卡片的点选与建话题
  遵循相同时机（建话题的封面卡片在此之前显示「下一条消息创建」）。
  这也是 `/worktree new` 可以在轮次运行中安全使用的原因：它只写一个待建会话，
  不移动任何东西，进行中的步骤绝不会被打断（#428 中途移动走不通的根源）。
- `/sub` 是当前会话**子会话**的只读视图——即它的 `task` 工具调用创建的那些
  会话。每一行带有子会话的标题、id、agent、最后活动时间与实时状态
  （Busy/Retry 显示 运行中，Idle 显示 空闲），每渲染一行读取一次状态；
  其他会话的子会话不会出现，且只列出**直接**子会话（没有嵌套后代，没有
  转录）。`list <kw>` 按标题/目录/id 过滤，每页六行，关键词与页码像
  `/switch` 卡片一样在每次重建后保留。没有活动会话（或只有待建状态）的
  对话打开纯空状态。`/sub list` 视图仅供观察：它从不向子会话发消息。
  `/sub attach <id|id-prefix|title> [--force]` 把当前会话的**直接**子会话
  之一接管到本聊天——与 `/switch` 相同的接管（带待处理权限/提问的会话快照
  回执；除非 `--force`，否则以点名另一聊天的方式拒绝，`--force` 会抢走该
  映射）。查询接受与 `/switch` 相同的形式（精确 id、唯一 id 前缀、标题
  子串），但限定在这些子会话内：未知与歧义查询会被报告，不是直接子会话的
  会话会被拒绝。接管运行中的子会话是允许的，快照会显示其实时状态。父会话
  保持映射，所以 `/switch` 可以切回去，对已处于活动状态的子会话重复
  `/sub attach` 不会改变任何东西。这是 cola 唯一获准接管子会话的途径；
  任何消息都不会被注入正在运行的子会话（ADR-0054）。
- `/dir` 卡片的行是一个**并集**（ADR-0046）：共享存储的会话目录、cola 已
  映射的目录——它们在其他客户端删除其会话后仍然存在——以及对话的当前目录，
  因此 当前 总能被标记，只有待建状态的目录也绝不会缺失。所以「最近」指的是
  cola 所知道的，而不是服务器上仍然存在的：会话已被清理的目录可以再次出现。
  丢掉已映射目录的是 `/switch forget` 与移除话题。
- 并集只装 cola 已经知道的目录；其「📁 浏览目录」按钮打开**目录浏览器**
  （ADR-0078），可到达机器上的任意目录：可点击的面包屑、逐级下钻的目录行
  （仓库优先并标 📦），随后是显示文件名与大小的文件行，标题为「子目录与文件」。
  点击文件行会在原位用 File Viewer 替换浏览卡；「← 返回目录」会恢复原列表，
  包括页码、过滤条件与隐藏项设置。卡片还有「显示隐藏项」开关、子项名称过滤与
  路径表单（前往 浏览输入的路径，选择 直接在那里声明会话——等同于 `/dir <路径>`
  而不必离开卡片）。列表为空时显示「没有子目录或文件」；路径已不存在时回退到
  最近的已存在祖先，只有已存在的目录才能被选择。**路径作用域不是安全边界**：
  浏览器可到达机主进程能读取的任意目录。群聊中的每个人都能阅读卡片，而 Access
  List 控制谁能在 cola 中操作，不控制谁能阅读。
- `/agent`、`/model`、`/think`、`/autoaccept` 都是**会话级**的，从下一条消息
  起生效，并在重启后保留。在 OpenCode 2 上，前三个是会话的持久状态——所选值
  写入会话本身（`POST /api/session/{id}/model|agent`，思考等级在模型引用
  内部），因此无需重发即可持久，共享存储上的其他客户端也能看到。对仍处于
  待建状态的会话（`/new`、`/dir` 或 `/topic` 在其第一条消息之前——包括它们的
  卡片形式），覆盖值记录在待建状态上，并应用于第一条消息创建的会话。
  `/model` 的值必须在 cola 连接的服务器上存在。不带参数的 `/model` 打开
  provider → model 选择卡片，其说明会显示下一条消息实际将运行的模型（会话
  自身的选择，否则 `[opencode] model`，否则服务器记录的值）。`/compact` 与
  `/stop` 需要真实会话，对待建状态保持它们的「还没有会话」回复。Codex 的
  Agent Client 读取 `/model` 的方式不同——见下文「Codex 客户端上的 `/model`」。
- `/restart-opencode` 会重启 Owned Server 或 V2 Shared Service（`opencode service
  restart`）；由其他工具启动的服务器不会被动（`NotOwned`）。
- 话题规则：在已绑定会话的话题里，`/switch`、`/new`、`/dir` 与 `/sub attach`
  会被拒绝——请回到主聊天操作。从未绑定过会话的话题（包括 `/topic` 打开、
  仍在等待第一条消息的话题）可以用它们来绑定它唯一的会话。

### Codex 客户端上的 `/model`

Codex 的模型轴不是 provider → model 树：provider 是**线程级**事实，模型是
**每轮**事实，内置目录是**全局**的。因此不带参数的 `/model` 只有一行当前
模型与**一个模型页**；底部的 `切换 provider…` 一行打开 provider 页，不会出现
「选择 provider（共 1 个）」这种伪层级：

- **当前模型来自线程，cola 自己的选择排在它前面。** 卡片的 当前模型 行按
  顺序取：会话记录的所选值（你做过选择时），再用 `thread/read` 读到的线程
  自身 `model` 与 `modelProvider`——所以其他客户端改过这条线程后，再打开
  卡片就能看到那次变化，footer 与选择卡片不会各说一套。
- **「模型（每轮可换）」** 列出只与模型有关的来源：`model/list` 的内置目录
  （分组为「内置目录（全局，未校验）」——这些 id 不属于任何 provider，可以经
  任意一个运行）、配置顶层 `model`（配置默认），以及**当前 provider** 的
  `/models` 返回的 id。点选后，下一条消息的 `turn/start.model` 就是该 id，
  线程后续轮次也继续使用它。
- **任何清单之外的 id 用文本输入。** `/model codex/<id>` 记录一个 id，
  `/model <provider>/<model>` 记录一对。Codex 对两者都不校验：只有内置目录
  的条目带显示名、上下文窗口与思考等级，所以拼错会在下一条消息上以 provider
  自身的报错出现。
- **`[profiles.*]` 不会被读取。** Codex 0.134+ 已不再读取它（`--profile` 会
  分层 `$CODEX_HOME/<name>.config.toml`），所以 cola 忽略该表：profile 既不
  增加模型行，也不提供 provider 组合。

**`/models` 探测是尽力而为，它被拒绝也不算错误。** `GET <base_url>/models`
不属于 Responses API——`wire_api = "responses"` 并不保证它存在。cola 用各
`[model_providers.<id>]` 自己的 `experimental_bearer_token` 去问，每次探测
2 秒，并缓存结果：确定性的答案（403、404、401、`env_key`、或解析出的模型）
缓存三分钟，**暂时性**失败（超时、网络错误）只缓存约 20 秒，下次打开就会重试。
provider 页把原因作为行尾的短标签（`403`、`超时`…），不是一段话，也不会出现在
模型页上。内置目录与配置默认始终在，而卡片回调只从缓存作答，所以
第一次冷点击可能只显示 provider、没有探测到的模型，下一次打开时补齐。token 只出现在请求的 `Authorization`
头里：卡片、日志行与错误信息中都不会出现。

**provider 只在创建时选择。** provider 页（`切换 provider…` 那一行）列出配置好的
成对行，标签为 `provider/model`。provider 只在线程创建或 fork 时确定：`/new` 把这一对
交给 `thread/start`，`/fork` 交给 `thread/fork`（fork 是单独的功能）。`turn/start`
没有 provider 字段，而且 provider 永远不能离开模型单独选——把旧模型名留在新 provider
上会 404。因此：

- **新线程直接带走这一对。** 在待建会话（第一条消息之前）上点 provider 行，
  这一对随 `thread/start` 发送。
- **活动线程的 provider 不可变。** 活动线程的 provider 页是只读的：显示当前
  provider，并说明只能在新会话或 fork 中更换。`/model` 选到不同 provider 时会被
  拒绝且**不发送任何请求**——没有 `turn/start`，也没有 `thread/resume`——卡片与
  文本回复都指向 `/new` / `/fork`。
- **同一 provider 内的点选只是一次模型变更。** `codex/<id>` 目录/默认引用，或
  线程当前 provider 自己 `/models` 返回的行，都只记录模型并随下一条消息生效
  ——不 resume、不重新加载。
- **`codex` 是保留名。** 内置目录的引用是 `codex/<id>`——一个共享的中性命名
  空间，永远不是一个渠道——所以 `[model_providers.codex]` 不能被选中：Provider
  一栏把它列成「`codex` 是内置目录的保留名，不能在这里选择」，不提供任何组合
  （组合值会和目录自身的引用撞车）。分类是纯字面的，因此目录点选从不读取你的
  配置。

`/model <provider>/<model>` 遵循同一规则：活动线程上的不同 provider 会被拒绝
（绝不静默改动线程），而 Pending Session 在创建时接受这一对。

## FAQ / 故障排查

**升级后机器人拒绝一切。** 全新或升级后的 cola 以未认领状态启动（ADR-0035）。在启动日志里找到认领码（`认领码`），从私聊发送 `/claim <code>`——一次即可。见[认领（默认私有）](#认领默认私有)。

**机器人看不到我的群消息。** 有两道门槛。第一，飞书只推送 @ 机器人的群消息——这是服务端的应用设置，cola 无法修复。第二，应用需要 `im:message.group_at_msg:readonly` 权限、订阅 `im.message.receive_v1`、一个**已发布**的版本，并且机器人必须是群成员（见[飞书应用配置](#飞书应用配置)）。机器人始终可以私聊（`im:message.p2p_msg:readonly`）。

**私聊能到达机器人，群消息却总是不能。** 应用缺少 `im:message.group_at_msg:readonly`，或群消息没有 @ 机器人。添加权限并重新发布。

**图片在提示词里显示为 `[图片]`。** cola 用 `im:message`（已在权限集中）下载图片。当机器人不是图片所在聊天的成员，或消息被标记为保密时，下载会失败——cola 会降级为占位符，而不是报错。

**`另一个 cola 实例（PID x）正在运行`** —— 另一个实例持有锁。用 `cola --replace` 接管，或在飞书里给旧实例发送 `/restart`。

**`/update` 失败并报权限错误。** 二进制位于 root 拥有的目录（如 `/usr/local/bin`）。重新安装到 `~/.local/bin`（见[安装](#安装)），或手动替换该二进制。

**`/model` 报告模型不存在。** 模型必须在 cola 连接的 OpenCode 服务器上可用（通常是共享服务器上的 `opencode/...` provider）。拿不准时，从配置里删掉 `[opencode] model`，让服务器使用自己的默认模型。

**cola 支持 OpenCode 2 吗？** 支持——OpenCode 1.18.x（V1）与 2.0.x（V2）都受支持，cola 会自动选择协议世代：无需任何配置，原地升级 OpenCode 也不需要改动 cola。每次附加/重连都会记录选择及其探测证据：

    Attached to OpenCode server at http://localhost:4096 — generation=v2 (probe: GET http://localhost:4096/api/info -> 200 application/json (V2 info envelope, version 2.0.18))

如果探测无法归类某台服务器（代理或特殊构建），用 `[opencode] generation = "v1"` 或 `"v2"` 强制指定。存储是共享的，V2 首次启动时会原地迁移它，因此升级会影响使用该存储的每个客户端；会话 id 保持不变，所以聊天↔会话映射会存活下来。

**我移动了 cola 二进制，自动启动坏了。** 重新运行 `cola autostart enable`——启动器在启用时会快照二进制路径。
<!-- ruleset verification commit -->
