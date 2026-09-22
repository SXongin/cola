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

Download the archive for your platform from the [latest GitHub
release](https://github.com/SXongin/cola/releases/latest) and put the binary on
your `PATH`, in a user-writable directory:

| Platform | Release asset | Install to |
| --- | --- | --- |
| Linux x86_64 | `cola-<version>-x86_64-unknown-linux-gnu.tar.gz` | `~/.local/bin` |
| macOS (Apple Silicon) | `cola-<version>-aarch64-apple-darwin.tar.gz` | `~/.local/bin` |
| Windows x86_64 | `cola-<version>-x86_64-pc-windows-msvc.zip` | `%LOCALAPPDATA%\Programs\cola` |

**Verifying a download (optional).** Each release archive carries a GitHub
build-provenance attestation, so you can confirm it was built by cola's release
workflow from this repository:

    gh attestation verify cola-<version>-<target>.tar.gz --repo SXongin/cola

(`gh` is the [GitHub CLI](https://cli.github.com).) Integrity is additionally
covered by the release's `SHA256SUMS`.

**Why a user-writable directory?** cola's self-update replaces its own binary
in place (`/update`, `cola update`), which requires write permission on the
binary's directory. In a root-owned location like `/usr/local/bin` (or
`C:\Program Files`) cola runs fine but **cannot update itself** — you would
have to replace it manually each release. (cargo installs are unaffected: they
are updated through cargo.)

**macOS.** The release build is Apple Silicon (`aarch64`) only: every Mac sold
since late 2020 is ARM, and macOS 26 Tahoe is the last macOS release supporting
Intel Macs. An `aarch64` binary cannot run on an Intel Mac (Rosetta translates
x86→ARM, not the reverse) — Intel users can build from source. The downloaded
binary is unsigned, so clear Gatekeeper's quarantine flag after extracting:

    xattr -d com.apple.quarantine ~/.local/bin/cola

**Windows.** Prefer `%LOCALAPPDATA%\Programs\cola` over `%USERPROFILE%\bin`:
`AppData\Local` is user-writable and **not roamed**, unlike `AppData\Roaming`
(which would sync a binary through a domain roaming profile), and `Programs\`
is the convention per-user apps use. Extract `cola.exe`, add the directory to
your user `PATH` (Settings → System → About → Advanced system settings →
Environment Variables), and reopen the terminal.

**crates.io.** With Rust tooling you can install from
[crates.io](https://crates.io/crates/colark): `cargo binstall colark` (needs
[cargo-binstall](https://github.com/cargo-bins/cargo-binstall)) downloads the
prebuilt release binary for your platform — no compile — while
`cargo install colark` builds from source. The binary is named `cola` either
way. Source builds need a C compiler and linker — the TLS stack builds AWS-LC —
and on Windows also NASM (or set `AWS_LC_SYS_PREBUILT_NASM=1`); no system
OpenSSL is required. On a minimal Linux image, install `ca-certificates` so the
Feishu WebSocket handshake can verify the server. On a platform with no
prebuilt asset (Linux aarch64, Intel macOS) binstall falls back to compiling
from source; `cargo install colark` gets there directly. A cargo-tracked binary
(a binstall install writes cargo's receipt too) is updated with cargo, not by
cola's self-update — see [Self-update](#self-update).

**Other platforms.** There is no prebuilt asset for Linux aarch64 or Intel
macOS; a GitHub-channel self-update reports "no asset for this platform"
instead of failing (cargo installs are unaffected — they update through
crates.io). Build from source (same prerequisites as the crates.io install
above): `cargo build --release`; `cargo install --path .` also places the
binary in a user-writable directory.

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
| `im:datasync.feed_card.time_sensitive:write` | 设置即时提醒 (`time_sensitive`) — optional, experimental | Instant Reminder: pin the Chat or Topic while a permission/question waits (`[bridge] instant_reminder`, off by default) | pinning is skipped (one log line); everything else keeps working |

Notes:

- `im:datasync.feed_card.time_sensitive:write` is the only **optional** scope:
  it powers the conversation-level Instant Reminder, an **experimental**
  opt-in feature (`[bridge] instant_reminder = true`). The waiting-card
  message pins under the same opt-in need no extra scope — `im:message`
  covers them. Grant this one only if you want to trial the conversation
  pin; without it cola logs the failed call and continues.
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
# access_file = "~/.cola/access.json"
# work_dir = "/path/to/a/project"
# group_completion_notice = true
# long_task_notice = true
# instant_reminder = true
# log_days = 14
```

- **`session_file`** — where the thread↔session mapping is persisted.
- **`access_file`** — where the Access List (the Host, ADR-0035) is persisted.
  A missing file means the bot is unclaimed and refuses every message until
  `/claim`.
- **`work_dir`** — default directory for new sessions when a conversation has no
  active session (fresh chat, or after `/switch forget`). Defaults to the process
  cwd. `/new` inherits the active session's directory (or the pending's);
  `/dir` names an explicit one. Both only declare the session: it is created by
  the conversation's next message.
- **`group_completion_notice`** — in group chats, reply to the requester with a
  short completion notice (the streaming card is patched in place, so it does not
  push a new notification). `false` disables it. p2p chats don't need it.
- **`instant_reminder`** — **experimental**, opt-in, **off by default**: use
  Feishu's Instant Reminder to pin the Chat or Topic at the top of the
  requester's message list while a permission or question is pending. The pin
  clears the moment the wait is resolved; it is a **state** that stays until
  you act, so it cannot be missed the way a message can. A Chat or Topic has
  one pin: the newest wait owns it, and when that wait resolves the next
  pending wait takes it immediately. The pin means "this Chat or Topic still
  needs you" — the card title in the preview says why (等待你的授权 ·
  等待你的回答 · both merged into 等待你的授权/回答). Requires the
  `im:datasync.feed_card.time_sensitive:write` scope; without it pinning is
  skipped (a log line only) and everything else keeps working. Live pins are
  persisted beside the session mapping and cleared at startup if a crash
  orphaned them (retrying a failed clear on the next start). If cola is
  permanently gone the pin stays — that is app-controlled platform state, and
  the Feishu client offers no cancel; marking the conversation 完成 dismisses
  it, but new activity pins it again. The waiting card
  is also message-pinned under the same opt-in, so opening the chat leads to
  it.
- **`long_task_notice`** — **off by default**: in p2p, reply to the requester's
  message when a turn ran at least 5 minutes, so a long task's end is announced
  by a new message. The streaming card is patched in place, which neither
  pushes a notification nor bumps the conversation, so without this a long
  task's end is easy to miss; a short turn stays silent. Group chats already
  notify on every turn via `group_completion_notice`.
- **`log_days`** — how many days of rotated daily logs to keep (default 14).

## Run

```bash
nohup cola >/dev/null 2>&1 &
```

cola attaches to an already-running OpenCode server automatically; with the
default `auto` policy it starts its own server on demand, so no manual
`opencode serve` is needed. If no config file exists yet, cola prints a hint
telling you where to put one and exits.

## Claim (private by default)

A fresh or upgraded cola starts **unclaimed**: it refuses every message. The
startup log prints a one-time **claim code** (8 characters, no ambiguous
glyphs); name the Host（机主）by sending it from a **private chat** with the bot:

    /claim <code>

- Look for `认领码` in the log (`~/.cola/cola.log`, or the terminal when it is
  attached). The code rotates on every restart and is never written to disk.
- A group `/claim` is refused — claiming is a private act.
- After a successful claim only the Host may use the bot; everyone else gets a
  short refusal. The Host's `open_id` is stored in the Access List
  (`[bridge] access_file`), so restarts and upgrades never re-claim.
- Rebuilding the Feishu app changes every user's `open_id`: delete the Access
  List file and claim again with the new code.

## Autostart

Register cola to start at boot/login. The started process is the `cola` binary
itself (Lazy Start handles the OpenCode server), so no flags are needed:

- **Linux**: `cola autostart enable` writes a systemd **user** unit
  (`~/.config/systemd/user/cola.service`) and enables it. Run
  `loginctl enable-linger $USER` (printed as a hint) so the service runs without
  a logged-in desktop session.
- **macOS**: writes and loads a LaunchAgent (`~/Library/LaunchAgents/com.cola.bot.plist`).
- **Windows**: writes an `HKCU\...\Run` registry value.

`cola autostart disable` stops the running instance and removes the registration
(see [Stop](#stop)); `cola autostart status` shows whether it is installed.

> **Important**: `enable` snapshots the **current binary path** into the
> Autostart registration and your **current PATH**. Run it after installing (and
> before moving the binary), and re-run it if you ever relocate cola or
> `opencode`.

## Stop

`cola stop` stops the running cola instance and nothing else:

- It goes through the platform **supervisor** first (systemd `stop` / launchd
  `bootout`) when an autostart registration is installed — a LaunchAgent with
  `KeepAlive` would otherwise respawn a directly-killed process — then falls
  back to terminating the singleton lock's holder by PID.
- On an interactive terminal it asks first (a running turn's card is
  truncated); `--yes` skips the prompt, and a non-interactive run never
  prompts, so scripts are unaffected.
- Nothing running prints `cola 未运行` and exits 0. The OpenCode server cola
  started is left running: it is a shared-store endpoint other clients may
  still use.

`cola autostart disable` does both — it stops the instance, then removes the
registration (a failed stop still unregisters, with a non-zero exit). It also
stops a supervised instance that is still starting up (or crash-looping) and
has not taken the singleton lock yet: the registration, not the lock, tells
`disable` what to stop. `cola stop` leaves the registration alone, so the next
boot starts cola again.

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

`/update` (Feishu) or `cola update [--check]` updates whichever channel cola
was installed through.

- **GitHub Releases installs**: checks GitHub Releases, downloads the binary for
  your platform, verifies it against the release's `SHA256SUMS`, replaces the
  running binary and restarts. The binary's directory must be
  **user-writable** (see [Install](#install)). Under a systemd unit the restart
  hands back to `Restart=on-failure`; elsewhere cola re-execs itself.
- **cargo installs** (`cargo install colark` / `cargo binstall colark`): cola
  detects cargo's install receipt and replaces nothing. It checks the crates.io
  version and replies with the command to run: `cargo install colark` — which
  compiles from source — or `cargo binstall colark`, which downloads the
  prebuilt release asset; add `--force` if cargo answers "already installed".
  Updating through cargo keeps its install bookkeeping (`cargo install --list`,
  cargo-update) truthful.
- Platforms without a prebuilt binary (e.g. Linux aarch64) report that instead
  of failing.

## Feishu commands

Unrecognized `/...` commands are forwarded to OpenCode. `/help <command>` shows
detailed help for any of these.

| Command | What it does |
| --- | --- |
| `/dir <path> [name]` | Declare a new session rooted at `<path>` — created by the next message |
| `/claim <code>` | Claim this cola as Host (private chat only; code from the startup log) |
| `/dir` | Recent Directories card: pick a folder and declare a session there, or open it as a fresh topic (每行「建话题」= `/topic <dir>` 的免打字版) |
| `/switch` | Session card: browse / search / adopt / new |
| `/switch <kw>` | Switch to a session by name/dir/id (adopts foreign ones) |
| `/switch list [kw] [--all]` | List recent sessions across the shared store (up to 15) |
| `/switch <id> [--force]` | Take over a session by id/title |
| `/switch forget` | Un-map this chat's session (the server session stays) |
| `/new [name]` | Declare a new session in the current project — created by the next message (no session → default dir) |
| `/topic [dir] [name]` | Open a new Feishu topic in `<dir>`; its first message creates the session (bare `/topic` uses the current project) |
| `/topic --adopt <kw> [--force]` | Open a topic around an existing session |
| `/name <name>` | Rename current session (server-side, visible to all clients; on a pending, set the creation title) |
| `/stop` | Interrupt execution |
| `/compact` | Compact context |
| `/card` | Pull the live card down to the newest position after mid-turn command replies bury it (replies a notice when no turn is running) |
| `/agent <name>` | Switch agent (takes effect next message; persisted; `--reset` clears to the server default) |
| `/model <p/m>` | Switch model (takes effect next message; persisted) |
| `/think [level]` | Set/clear thinking level, per model (takes effect next message) |
| `/autoaccept [on\|off]` | Show/switch auto-allowing tool-permission requests for this session |
| `/restart` | Restart cola (keeps startup args + log redirect) |
| `/restart-opencode` | Restart the OpenCode server (only one cola itself started) |
| `/update` | Update cola via its install channel (GitHub Releases self-update, or the cargo command for `cargo install`/`cargo binstall` installs) |
| `/version` | Show cola version and build provenance (release / crates.io / dev build) |
| `/help [command]` | List commands, or show detailed help for one |

Notes:

- `/new`, `/dir` and `/topic` only **declare** the session — the conversation's
  next non-command message creates it in the chosen directory and maps it here,
  so a mistaken `/new`, `/dir` or `/topic` can be corrected with another `/new`,
  `/dir`, `/switch` or `/topic` before then without leaving a session in the
  shared store. Inside a topic that is still waiting for its first message,
  `/switch <id>` re-points the topic at an existing session instead. The `/dir`
  card's pick and 建话题 follow the same timing (建话题's cover card shows
  「下一条消息创建」 until then).
- `/agent`, `/model`, `/think`, `/autoaccept` are **per-session** overrides sent
  with the next message and persisted across restarts. On a session that is still
  pending (`/new`, `/dir` or `/topic` before its first message — including its
  card forms), the override is recorded on the pending and applies to the session
  the first message creates. `/model`'s value must exist on the server cola
  attaches to. Bare `/model` opens the provider → model picker, whose intro shows
  the model the next message will actually run (session override, else
  `[opencode] model`, else what the server recorded). `/compact` and `/stop` need
  a real session and keep their "还没有会话" replies on a pending.
- `/restart-opencode` leaves a server launched by another tool alone — it only
  restarts a server cola started itself.
- Topic rule: inside a topic already bound to a session, `/switch`, `/new` and
  `/dir` are rejected — go back to the main conversation. A topic that has never
  bound a session (including a `/topic`-opened topic still waiting for its first
  message) can use them to bind its single session.

## FAQ / troubleshooting

**After upgrading, the bot refuses everything.** A fresh or upgraded cola
starts unclaimed (ADR-0035). Find the claim code in the startup log (`认领码`)
and send `/claim <code>` from your private chat — once. See
[Claim](#claim-private-by-default).

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
