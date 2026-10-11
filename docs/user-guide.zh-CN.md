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
| `sessions.json` | 飞书话题 ↔ OpenCode 会话映射 | `[bridge] session_file` |
| `access.json` | 访问名单（机主；ADR-0035） | `[bridge] access_file` |
| `cola.lock` | 单实例锁（每台机器只能有一个 cola） | — |
| `restart-notify.json` | 重启通知记账 | — |

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

cola 以企业自建应用的身份与飞书通信。配置是一次性的，在[开发者后台](https://open.feishu.cn/app)完成；[README 快速开始](../README.md#quick-start) 有一份精简清单——本节是完整说明。

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
  或 V2 托管服务的注册文件——所以无需配置），因此 `url` 只是同类多个服务器
  之间的优先级选择——也是 cola 启动自己的服务器时的回退端口。不是 cola
  启动的服务器（OpenChamber 的、手动启动的）总是优先于 cola 自己的。
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

## 运行

```bash
nohup cola >/dev/null 2>&1 &
```

cola 会自动连接到已在运行的 OpenCode 服务器；在默认的 `auto` 策略下，它按需
启动自己的服务器，因此无需手动运行 `opencode serve`。如果配置文件还不存在，
cola 会打印一条提示，告诉你该把它放在哪里，然后退出。

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
| `/dir` | 最近目录卡片：选一个文件夹并在那里声明会话，或将其作为新话题打开（每行「建话题」= `/topic <dir>` 的免打字版） |
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
| `#<id> [#<id> …] [text]` | 把 OpenCode 技能加载进本条消息的提示词（如 `#implement-spec 644`）；整段消息原样提交，匹配不到技能的 id 会原样保留为正文。裸 `/skill` 打开技能选择卡片——可按 id/名称/描述搜索、每页 20 个翻页——`/skill <id>` 为别名 |
| `/autoaccept [on\|off]` | 查看/切换本会话是否自动放行工具权限请求 |
| `/restart` | 重启 cola（保留启动参数与日志重定向） |
| `/restart-opencode` | 重启 OpenCode 服务器（仅限 cola 自己启动的那个） |
| `/update` | 按安装渠道更新 cola（GitHub Releases 自更新，或 `cargo install`/`cargo binstall` 安装的 cargo 命令） |
| `/version` | 显示 cola 版本与构建来源（release / crates.io / dev 构建） |
| `/help [command]` | 列出命令，或显示某一条的详细帮助 |

说明：

- `/new`、`/dir` 与 `/topic` 只**声明**会话——会话由该会话下一条非命令消息
  在所选目录中创建并映射到这里，所以在下一条消息之前，误发的 `/new`、
  `/dir` 或 `/topic` 可以用另一个 `/new`、`/dir`、`/switch` 或 `/topic`
  纠正，而不会在共享存储里留下会话。在仍在等待第一条消息的话题里，
  `/switch <id>` 则会改把该话题指向一个已有会话。`/dir` 卡片的点选与建话题
  遵循相同时机（建话题的封面卡片在此之前显示「下一条消息创建」）。
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
- `/agent`、`/model`、`/think`、`/autoaccept` 都是**会话级**的，从下一条消息
  起生效，并在重启后保留。在 OpenCode 2 上，前三个是会话的持久状态——所选值
  写入会话本身（`POST /api/session/{id}/model|agent`，思考等级在模型引用
  内部），因此无需重发即可持久，共享存储上的其他客户端也能看到。对仍处于
  待建状态的会话（`/new`、`/dir` 或 `/topic` 在其第一条消息之前——包括它们的
  卡片形式），覆盖值记录在待建状态上，并应用于第一条消息创建的会话。
  `/model` 的值必须在 cola 连接的服务器上存在。不带参数的 `/model` 打开
  provider → model 选择卡片，其说明会显示下一条消息实际将运行的模型（会话
  自身的选择，否则 `[opencode] model`，否则服务器记录的值）。`/compact` 与
  `/stop` 需要真实会话，对待建状态保持它们的「还没有会话」回复。
- `/restart-opencode` 不会动由其他工具启动的服务器——它只重启 cola 自己启动
  的服务器。
- 话题规则：在已绑定会话的话题里，`/switch`、`/new`、`/dir` 与 `/sub attach`
  会被拒绝——请回到主聊天操作。从未绑定过会话的话题（包括 `/topic` 打开、
  仍在等待第一条消息的话题）可以用它们来绑定它唯一的会话。

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
