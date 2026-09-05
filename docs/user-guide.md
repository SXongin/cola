# cola User Guide

The full operation manual for running cola day to day. For installation and the
first chat, see the [README](../README.md).

## Where things live

All runtime state defaults to `~/.cola/`:

| File | Purpose | Overridable |
| --- | --- | --- |
| `cola.toml` | configuration | `./cola.toml` (cwd) or `cola --config <path>` |
| `cola.log` | logs (append, daily rotation) | `cola --log-file <path>` |
| `sessions.json` | Feishu thread ↔ OpenCode session mapping | `[bridge] session_file` |
| `cola.lock` | singleton lock (one cola per machine) | — |
| `restart-notify.json` | restart announcement bookkeeping | — |

The lock and config are why `cola` has no flags in the autostart launcher: it
resolves everything from `~/.cola` regardless of the working directory.

## Install

Download the prebuilt binary from the [latest GitHub
release](https://github.com/SXongin/cola/releases/latest) and put it on your
`PATH`.

**Install into a user-writable directory — `~/.local/bin` is the sweet spot.**
cola's self-update replaces its own binary in place (`/update`, `cola update`),
which requires write permission on the binary's directory. In a root-owned
location like `/usr/local/bin` cola runs fine but **cannot update itself** (you
would have to replace it manually each release).

If you build from source (`cargo build --release`), `cargo install --path .`
also places it in a user-writable directory.

Also make sure an `opencode` binary is on `PATH` — cola discovers and spawns it.
The autostart launcher snapshots your `PATH` at `cola autostart enable` time, so
install both **before** enabling autostart.

## Feishu app setup

cola talks to Feishu as a custom app (企业自建应用). Setup is one-time, in the
[developer console](https://open.feishu.cn/app); the [README Quick
start](../README.md#quick-start) has the condensed checklist — this section is the
full map.

### 1. Enable the bot capability

The app must have the **机器人** capability (应用能力 → 机器人). Without it the app
cannot be added to chats, receive messages, or send as a bot — and it is a hard
precondition for the message and resource APIs.

### 2. Grant scopes

| Scope | Feishu permission | What cola uses it for | If missing |
| --- | --- | --- | --- |
| `im:message` | 获取与发送单聊、群组消息 | send/reply to and update cards; read a message by id (quote context); download message images | cola cannot reply or update cards — unusable |
| `im:message:send_as_bot` | 以应用的身份发消息 | send and reply to messages as the bot | card sending fails |
| `im:message.p2p_msg:readonly` | 读取用户发给机器人的单聊消息 | receive DMs | DMs never reach cola |
| `im:message.group_at_msg:readonly` | 接收群聊中@机器人消息事件 | receive group messages that @ the bot | group messages never reach cola |
| `im:chat:readonly` | 获取群组信息 | chat display names (`/attach` rejection card, `/switch` cards) | raw chat ids instead of names |
| `contact:contact.base:readonly` | 获取通讯录基本信息 | authorizes the contact API call | user names unavailable |
| `contact:user.base:readonly` | 获取用户基本信息 | returns the user's `name` field | completion notices lose the @name |

Notes:

- `im:chat` (获取与更新群组信息) is a superset of `im:chat:readonly` — cola only
  reads chat info, so the read-only scope is the minimum.
- The contact API needs two scopes: one to authorize the call
  (`contact:contact.base:readonly`) and one to reveal the `name` field
  (`contact:user.base:readonly`).
- Reading a message by id and downloading its images both work with `im:message`
  alone — no extra scope is needed for media.

### 3. Event subscription

- **事件与回调 → 回调配置**: choose **使用长连接接收事件** (long-connection mode).
  cola connects as a WebSocket client when it starts, so **run cola before saving
  this setting** — Feishu only accepts the mode while a long-connection client is
  connected.
- Subscribe to the `im.message.receive_v1` event (接收消息). Card-button callbacks
  (`card.action.trigger`) arrive over the same connection automatically — there is
  no webhook URL to configure.
- Long-connection mode only works for 企业自建应用 (not store apps), and each app
  allows up to 50 connections — cola holds one.

### 4. Publish

Scope grants and event subscriptions only take effect after you **publish a new
version** (版本管理与发布). Self-built apps can publish for themselves without a
store review. You do not need to re-publish on every cola update — only when you
change the app's scopes or events.

## Configuration

Lookup order: `cola --config <path>` (used verbatim) → `./cola.toml` → `~/.cola/cola.toml`.
Only `[feishu]` is required.

### `[feishu]` — required

```toml
[feishu]
app_id = "cli_xxxxxxxxxxxx"
app_secret = "your-app-secret"
```

### `[opencode]` — all optional

```toml
[opencode]
url = "http://localhost:4096"    # preferred/fallback port (default 4096)
start_server = "auto"            # auto (default) | never | eager
# model = "opencode/deepseek-v4-flash"
```

- **`url`** — only a *preferred* port. cola attaches to whatever `opencode
  serve` is already running on the **shared store** automatically (username and
  password are read from the server's environment, so nothing to configure), so
  `url` is a tiebreaker among several servers of the same kind — and the fallback
  port when cola starts its own server. A server cola did **not** start
  (OpenChamber's, a manual one) always wins over cola's own.
- **`start_server`** — when cola may start its own `opencode serve`:
  - `auto` (default) — attaches at boot and spawns an own server only at the
    moment a message needs one and none is running.
  - `never` — attach-only; cola replies that OpenCode is unavailable when no
    server exists.
  - `eager` — the old behavior: spawn an own server at boot when none is running.
- **`model`** — the default model for new sessions (`provider/model`). Unset
  (recommended on most setups) → cola sends no model and the OpenCode server
  uses **its own** default. If set, the model must exist on the server cola
  attaches to (usually `opencode/...`). Per-session overrides are set with
  `/model`.

### `[bridge]` — all optional

```toml
[bridge]
# session_file = "~/.cola/sessions.json"
# work_dir = "/path/to/a/project"
# group_completion_notice = true
# log_days = 14
```

- **`session_file`** — where the thread↔session mapping is persisted.
- **`work_dir`** — default directory for new sessions when a conversation has no
  active session (fresh chat, or after `/switch forget`). Defaults to the process
  cwd. `/new` inherits the active session's directory; `/dir` overrides per
  session.
- **`group_completion_notice`** — in group chats, reply to the requester with a
  short completion notice (the streaming card is patched in place, so it does not
  push a new notification). `false` disables it. p2p chats don't need it.
- **`log_days`** — how many days of rotated daily logs to keep (default 14).

## Run

```bash
nohup cola >/dev/null 2>&1 &
```

cola attaches to an already-running OpenCode server automatically; with the
default `auto` policy it starts its own server on demand, so no manual
`opencode serve` is needed. If no config file exists yet, cola prints a hint
telling you where to put one and exits.

## Autostart

Register cola to start at boot/login. The launcher runs the `cola` binary itself
(Lazy Start handles the OpenCode server), so no flags are needed:

- **Linux**: `cola autostart enable` writes a systemd **user** unit
  (`~/.config/systemd/user/cola.service`) and enables it. Run
  `loginctl enable-linger $USER` (printed as a hint) so the service runs without
  a logged-in desktop session.
- **macOS**: writes and loads a LaunchAgent (`~/Library/LaunchAgents/com.cola.bot.plist`).
- **Windows**: writes an `HKCU\...\Run` registry value.

`cola autostart disable` removes the registration; `cola autostart status` shows
whether it is installed.

> **Important**: `enable` snapshots the **current binary path** into the launcher
> and your **current PATH**. Run it after installing (and before moving the
> binary), and re-run it if you ever relocate cola or `opencode`.

## Logs

cola always appends to a log file — by default `~/.cola/cola.log` (no ANSI
codes), overridable with `--log-file`. A restart never wipes history (append, not
overwrite). Logs rotate **daily**: yesterday's content moves to
`cola-YYYY-MM-DD.log` and older files are swept after `[bridge] log_days`
(default 14). Cross-day sessions are queried with
`grep session_id=... cola-*.log`. When stdout is a real terminal, logs also
mirror there; a redirected stdout gets no cola logs, keeping the redirect target
clean.

## Singleton lock & restart

Only one cola may run at a time (two would double-handle Feishu events). The
lock lives at `~/.cola/cola.lock`.

- Starting cola while another runs, **non-interactively**: cola refuses and
  prints a clear message. Take over with `cola --replace`, or from Feishu send
  the old instance `/restart`.
- Starting interactively (a terminal): cola asks
  `旧实例 PID x 在运行，是否替换它并接管？[y/N]`.
- `/restart` re-execs cola itself with `--replace` (keeping its startup args and
  log redirect), so the new process always takes over the lock. The takeover is
  robust to the old instance mid-`exit()`: an owner that is no longer
  functionally alive (dead, a zombie, or tearing down) is reclaimed instead of
  blocking the restart.
- Under a systemd unit, `/restart` (like `/update`) does NOT spawn a child — it
  exits with a restart code and lets the unit's `Restart=on-failure` bring cola
  back up from the same ExecStart.

## Self-update

`/update` (Feishu) or `cola update [--check]` checks GitHub Releases for a newer
cola. If one exists, it downloads the binary for your platform, verifies it
against the release's `SHA256SUMS`, replaces the running binary and restarts.

- The binary's directory must be **user-writable** (see [Install](#install)).
- Under a systemd unit the restart hands back to `Restart=on-failure`; elsewhere
  cola re-execs itself.
- Platforms without a prebuilt binary (e.g. Linux aarch64) report that instead
  of failing.

## Feishu commands

Unrecognized `/...` commands are forwarded to OpenCode. `/help <command>` shows
detailed help for any of these.

| Command | What it does |
| --- | --- |
| `/dir <path> [name]` | Switch to a project: open a NEW session rooted at `<path>` |
| `/dir` | Recent Directories card: pick a recently-used folder and switch there |
| `/switch` | Session card: browse / search / adopt / new |
| `/switch <kw>` | Switch to a session by name/dir/id (adopts foreign ones) |
| `/switch list [kw] [--all]` | List recent sessions across the shared store (up to 15) |
| `/switch <id> [--force]` | Take over a session by id/title |
| `/switch forget` | Un-map this chat's session (the server session stays) |
| `/new [name]` | New session in the current project (no session → default dir) |
| `/topic [dir] [name]` | Create a new Feishu topic + session in `<dir>` (bare `/topic` uses the current project) |
| `/topic --adopt <kw> [--force]` | Open a topic around an existing session |
| `/name <name>` | Rename current session (server-side, visible to all clients) |
| `/stop` | Interrupt execution |
| `/compact` | Compact context |
| `/agent <name>` | Switch agent (takes effect next message; persisted) |
| `/model <p/m>` | Switch model (takes effect next message; persisted) |
| `/think [level]` | Set/clear thinking level, per model (takes effect next message) |
| `/autoaccept [on\|off]` | Show/switch auto-allowing tool-permission requests for this session |
| `/restart` | Restart cola (keeps startup args + log redirect) |
| `/restart-opencode` | Restart the OpenCode server (only one cola itself started) |
| `/update` | Check for and apply a cola self-update from GitHub Releases |
| `/help [command]` | List commands, or show detailed help for one |

Notes:

- `/agent`, `/model`, `/think`, `/autoaccept` are **per-session** overrides sent
  with the next message and persisted across restarts. `/model`'s value must
  exist on the server cola attaches to.
- `/restart-opencode` leaves a server launched by another tool alone — it only
  restarts a server cola started itself.
- Topic rule: inside a topic already bound to a session, `/switch`, `/new` and
  `/dir` are rejected — go back to the main conversation. A topic that has never
  bound a session can use them to bind its single session.

## FAQ / troubleshooting

**The bot never sees my group messages.** Two things gate this. First, Feishu only
pushes group messages that @-mention the bot — a server-side app setting, not
fixable in cola. Second, the app needs the `im:message.group_at_msg:readonly`
scope, a subscription to `im.message.receive_v1`, a **published** version, and the
bot must be a member of the group (see [Feishu app setup](#feishu-app-setup)). The
bot can always be DM'd (`im:message.p2p_msg:readonly`).

**DMs reach the bot but group messages never do.** The app is missing
`im:message.group_at_msg:readonly`, or the group message did not @ the bot. Add the
scope and re-publish.

**Images show as `[图片]` in the prompt.** cola downloads images with `im:message`
(already in the scope set). Downloading fails when the bot is not a member of the
chat holding the image, or the message is marked confidential — cola degrades to a
placeholder rather than erroring.

**`另一个 cola 实例（PID x）正在运行`** — another instance holds the lock. Take
over with `cola --replace`, or send the old instance `/restart` from Feishu.

**`/update` fails with a permission error.** The binary is in a root-owned
directory (e.g. `/usr/local/bin`). Reinstall into `~/.local/bin` (see
[Install](#install)) or replace the binary manually.

**`/model` reports the model doesn't exist.** The model must be available on the
OpenCode server cola attaches to (usually the `opencode/...` provider on the
shared server). When in doubt, remove `[opencode] model` from the config and let
the server use its own default.

**I moved the cola binary and now autostart is broken.** Re-run
`cola autostart enable` — the launcher snapshots the binary path at enable time.
<!-- ruleset verification commit -->
