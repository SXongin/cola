# cola

[![CI](https://github.com/SXongin/cola/actions/workflows/ci.yml/badge.svg)](https://github.com/SXongin/cola/actions/workflows/ci.yml)
[![Release](https://img.shields.io/github/v/release/SXongin/cola.svg)](https://github.com/SXongin/cola/releases/latest)
[![crates.io](https://img.shields.io/crates/v/colark.svg)](https://crates.io/crates/colark)
[![Codecov](https://codecov.io/gh/SXongin/cola/graph/badge.svg?branch=main)](https://codecov.io/gh/SXongin/cola)
[![OpenSSF Scorecard](https://api.securityscorecards.dev/projects/github.com/SXongin/cola/badge)](https://securityscorecards.dev/viewer/?uri=github.com/SXongin/cola)
[![Platforms](https://img.shields.io/badge/platform-Linux%20%7C%20macOS%20%7C%20Windows-lightgrey)](#1-install)
[![License](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)

**Your coding agent, in Feishu.** Send the bot a message from your phone — the AI
edits code on your machine, and you watch, steer and approve every step without
opening a terminal.

cola bridges [Feishu](https://www.feishu.cn) and the coding agent on your machine
— [OpenCode](https://opencode.ai) by default, or a
[Codex Agent Client](docs/user-guide.md#connect-a-codex-client). Each DM is a
session on that runtime, each turn streams onto one live card, and every
permission request becomes a button.

It is a bot for one person: you create the app, you claim it, and only you can
drive it.

*The name: **Co**de with **La**rk — Lark's China brand is Feishu.*

<!--
Hero media — drop the recorded GIF in and uncomment (see docs/images/README.md
for the shot list and naming). Keep the width so the card is legible on desktop
without dominating the page.
<p align="center">
  <img src="docs/images/turn-card.gif" width="340"
       alt="A cola turn in Feishu: reasoning and tool calls stream into one card, then a permission card is approved with one tap">
</p>
-->

**中文文档**: [用户使用手册（简体中文）](docs/user-guide.zh-CN.md)。

## What it feels like

> **You** · 把 pollers 里那张卡的 chat 映射修一下
>
> **cola** · ⏳ 思考中 · `read pollers.rs` · `edit pollers.rs` · 14s
> ⚠️ 要运行 `cargo test` 吗？　**［允许］［总是允许］［拒绝］**
>
> **You** · （点「允许」）
>
> **cola** · ✅ 改完了 · 2 个文件 · **+18 −6**　**［🔍 查看完整改动］**

One card per turn, buttons for decisions, and `/cat` / `/diff` / `/run` when you
want to read the code, the diff or the shell's output yourself. That is the
whole surface.

## Try it

About **5 minutes**, and the Feishu side is free: you create a **self-built app**
(企业自建应用) for yourself (model usage bills to whatever provider your agent is
configured with). You need:

- **either** an `opencode` binary on `PATH` (see <https://opencode.ai>) — OpenCode
  **1.18.x (V1) and 2.0.x (V2) both work**; cola attaches to the shared server,
  starting one when none is running, and detects which protocol it speaks (the
  log records the choice) — **or** the Codex CLI (`codex-cli 0.166.0`) on
  `PATH`, signed in with `codex login`, plus a `[clients.<id>]` table with
  `kind = "codex"` (step 3). See [Connect a Codex
  client](docs/user-guide.md#connect-a-codex-client) for the app-server daemon
  and the platform notes.
- a Feishu account where you can create an app

### 1. Install

Download the archive for your platform from the [latest GitHub
release](https://github.com/SXongin/cola/releases/latest) and put the binary on
your `PATH` — **in a user-writable directory**. cola's self-update replaces its
own binary in place, which a root-owned path like `/usr/local/bin` would block:

| Platform | Release asset | Install to |
| --- | --- | --- |
| Linux x86_64 | `cola-<version>-x86_64-unknown-linux-gnu.tar.gz` | `~/.local/bin` |
| macOS (Apple Silicon) | `cola-<version>-aarch64-apple-darwin.tar.gz` | `~/.local/bin` |
| Windows x86_64 | `cola-<version>-x86_64-pc-windows-msvc.zip` | `%LOCALAPPDATA%\Programs\cola` |

- **macOS**: the binary is unsigned — clear the quarantine flag after
  extracting (`xattr -d com.apple.quarantine ~/.local/bin/cola`). There is no
  Intel build; Intel users must build from source.
- **Windows**: extract `cola.exe`, add the install directory to your user
  `PATH` (via the Environment Variables dialog), then reopen the terminal.
  `AppData\Local` is user-writable and not roamed, unlike `AppData\Roaming`.

Or install with Rust tooling: `cargo binstall colark` fetches the prebuilt
release asset (no compile), while `cargo install colark` (edition 2024) builds
from source — source builds need a C compiler and linker, plus NASM on Windows.
Updates follow the install channel: a cargo-tracked install is updated with
cargo (`/update` prints the command), an archive install uses cola's own
updater. Building from source is `cargo build --release`. The [user
guide](docs/user-guide.md#install) explains each choice in detail.

### 2. Create the Feishu app

One-time setup at <https://open.feishu.cn/app> (create a custom app). Do **not**
publish it yet — the event subscription in step 4 needs cola running first.

1. Enable the **bot capability** (应用能力 → 机器人) — required before the app can
   join chats or send anything.
2. Grant these three scopes (权限管理) and you are done:
   - `im:message` — send, reply to and update cards; read quotes and images
   - `im:message:send_as_bot` — send as the bot
   - `im:message.p2p_msg:readonly` — receive DMs

   Add `im:message.group_at_msg:readonly` only if the bot should answer in
   group chats — Feishu pushes a group message only when it @-mentions the bot.
   Real names and the optional Instant Reminder each need one more scope — see
   below.

<details>
<summary>Optional extras: group chats, names and Instant Reminder</summary>

| Scope | What cola uses it for | If missing |
| --- | --- | --- |
| `im:message` | send/reply to and update cards; read a message by id (quote context); download message images | cola cannot reply or update cards — unusable |
| `im:message:send_as_bot` | send and reply to messages as the bot | card sending fails |
| `im:message.p2p_msg:readonly` | receive DMs | DMs never reach cola |
| `im:message.group_at_msg:readonly` | receive group messages that @ the bot | group messages never reach cola |
| `im:chat:readonly` | chat display names (`/attach` rejection card, `/switch` cards) | raw chat ids instead of names |
| `contact:contact.base:readonly` | authorizes the contact API call | user names unavailable |
| `contact:user.base:readonly` | returns the user's `name` field | completion notices lose the @name |
| `im:datasync.feed_card.time_sensitive:write` | *(optional)* Feishu Instant Reminder — pin a chat while a permission or question waits (`[bridge] instant_reminder`, off by default) | pinning is skipped, everything else works |

Missing scopes degrade features instead of crashing cola. The [user
guide](docs/user-guide.md#feishu-app-setup) maps every scope to what it unlocks.

</details>

### 3. Configure

Write `~/.cola/cola.toml` (or `./cola.toml` in the directory you run from, or
point explicitly with `cola --config <path>`):

```toml
[feishu]
app_id = "cli_xxxxxxxxxxxx"
app_secret = "your-app-secret"
```

That is the minimum. Everything else is optional — for example
`[bridge] instant_reminder = true` pins a chat while a permission waits, and
`[bridge] long_task_notice = true` announces a p2p turn that ran 5+ minutes with
a reply message (the card patch itself pushes no notification). A second Agent
Client is a `[clients.<id>]` table plus `[bridge] default_client`, never a second
config file — the guide's [Connect a Codex
client](docs/user-guide.md#connect-a-codex-client) has the exact shape. See the
[user guide](docs/user-guide.md#configuration) for every key.

### 4. Start cola, then finish the app

```bash
cola
```

Leave it running (Ctrl-C stops it). Now, back in the developer console:

1. **事件与回调**: choose **使用长连接接收事件** (long-connection mode) and
   subscribe to `im.message.receive_v1`. This is why cola is running already —
   Feishu only accepts long-connection mode while a client is connected. Card
   buttons (`card.action.trigger`) arrive over the same connection; there is no
   webhook URL to configure.
2. **Publish** the version (版本管理与发布) so the scopes and events take effect.
3. DM the bot — that is the whole setup.

### 5. Claim the bot

cola is **private by default**: a fresh (or upgraded) instance starts
*unclaimed* and refuses everyone. On startup it prints a one-time **claim code**
(also in `~/.cola/cola.log`); DM the bot `/claim <code>` from your own Feishu
account to become its Host. After that only you can use it.

The code rotates on every restart, is never written to disk, and the claim
itself is one-time — restarts and upgrades never ask again. DM the bot `/help`
to see every command.

Inspect and control it from the shell: `cola status` (run state, autostart,
claim, the active Agent Client — its endpoint, whether it is attached and what
it advertises — the attached OpenCode server and its generation; `--json` for
scripts), `cola doctor` (the environment checks a support request needs —
including a real turn on a Codex app-server daemon, which may cost provider
tokens and is skipped with `--no-turn-check`), `cola logs [-f]`, `cola restart`,
`cola config path|show|validate`, and `cola completions <shell>` (add `--install`
to write the script into your shell's completion directory).

### 6. Keep it running

Stop the foreground instance (Ctrl-C) and register cola to start at boot:

```bash
cola autostart enable
cola autostart status   # confirm it is registered
```

Two instances cannot share the bot — cola takes a singleton lock — so stop this
one first. macOS starts the supervised instance immediately; on Linux the unit
is only enabled, so `systemctl --user start cola` hands over now (otherwise it
comes up at your next login), and a headless machine also needs
`loginctl enable-linger $USER`. **Windows** uses an `HKCU\...\Run` value, which
takes effect at the next sign-in.

`cola autostart disable` stops the instance and removes the registration;
`cola stop` stops it but keeps it. Enable it **after** installing both cola and
its agent (`opencode`, or `codex` for a Codex Agent Client) — the registration
snapshots the binary path and your `PATH`, so re-run `enable` if you move either.
To detach from the terminal instead:
`nohup cola >/dev/null 2>&1 &` on Linux/macOS, or `start /b cola` on Windows.

## What you get

- **OpenCode or Codex, one bridge.** The default Agent Client is OpenCode; add a
  `[clients.<id>]` table with `kind = "codex"` to drive the Codex app-server
  instead. The same cards and Feishu commands carry over, but what each agent
  surfaces differs: a Codex thread under `approval_policy = "never"` (or full
  access) raises no permission card, its questions need the daemon's
  `default_mode_request_user_input` flag, and `/restart-opencode` only restarts
  an OpenCode server.
- **One card per turn, updated live.** Reasoning, tool calls and the answer
  stream onto a single card with collapsible panels, a phase timer and a live
  todo panel, so a slow turn never looks like a dead one; images in a message or
  a quoted reply ride along for vision-capable models.
- **Decisions are two taps.** Permission requests render as
  Allow / Deny / Always buttons on the card, and questions become a
  multi-select card that accepts custom answers.
- **See what changed, and read the code.** A finished turn with edits carries
  `N files · +A −D` and delivers the whole diff as one self-contained red/green
  HTML file — open it anywhere, nothing to install (OpenCode 2; V1 has no
  session diff). `/diff [--base <ref>]` does it for a branch, and `/cat <path>`
  previews any file on the host: paged, markdown/raw toggle, images and
  directory browsing, sendable as a file.
- **The host's terminal, too.** `/run <cmd>` — or any message starting with `!`
  — runs one shell command on the host and replies with a terminal card: the
  cwd, the merged output, the exit code and the elapsed time. `/run` on its own
  pulls that card back to the newest position.
- **As many sessions as you have tasks.** `/new`, `/dir <path>`, `/switch` to
  adopt a session started elsewhere (one line confirming the takeover and its
  status), `/sub`
  for the subagents a turn spawned, and `/status` for the current model, the
  next-message pick and a summary. With OpenCode the Shared Store is shared with
  the CLI and OpenChamber; with Codex, a TUI attached to the same app-server
  daemon shares its sessions. Either way a session begun elsewhere continues
  here — and messages sent from another client come back into Feishu.
- **A checkout per task, from your phone.** `/topic --worktree` or
  `/new --worktree` starts the next conversation in its own `git worktree`:
  name a branch and base if you want specific ones (both default), and the first
  message creates the checkout and the session inside it. `/worktree` shows or
  adjusts the pending intent, so an abandoned conversation leaves nothing
  behind.
- **Set it and forget it.** Private by default, autostart at boot, a singleton
  lock, restart and self-update — on Linux, macOS and Windows.

## How it works

```mermaid
flowchart LR
    you(["You · Feishu<br/>phone or desktop"])
    cola["cola"]
    opencode["OpenCode server"]
    codex["Codex app-server"]
    ocstore[("Shared Store")]
    cxstore[("$CODEX_HOME<br/>thread store")]
    other["OpenCode CLI<br/>OpenChamber"]
    tui["Codex CLI / TUI"]

    you <-->|"messages · cards · buttons"| cola
    cola <-->|"HTTP + event stream"| opencode
    cola <-->|"JSON-RPC"| codex
    opencode --- ocstore
    codex --- cxstore
    other <--> opencode
    tui <--> codex
```

cola drives one Agent Client at a time. With OpenCode (the default) it attaches
to a running `opencode serve` on the Shared Store and starts one when none is
running; because that store is shared, the same session can be driven from
Feishu, the OpenCode CLI or OpenChamber. With a Codex Agent Client it attaches
to the app-server daemon and its thread store under `$CODEX_HOME`, which a Codex
CLI or TUI pointed at that same daemon shares — the OpenCode store is not
involved, and OpenChamber never sees those sessions.

## Docs

- **User guide** — [docs/user-guide.md](docs/user-guide.md): where things live,
  full configuration reference, autostart, logs, self-update, singleton &
  restart, every Feishu command, troubleshooting.
- **用户使用手册（简体中文）** — [docs/user-guide.zh-CN.md](docs/user-guide.zh-CN.md):
  用户手册的简体中文译本（英文版为事实来源；结构漂移由
  `cargo xtask check-doc-drift` 守护）。
- **Contributing** — [CONTRIBUTING.md](CONTRIBUTING.md): commit conventions,
  the verification loop, PR rules.
- **Architecture** — [docs/adr/](docs/adr/), [GLOSSARY.md](GLOSSARY.md),
  [AGENTS.md](AGENTS.md) (includes the hard-won known pitfalls for the OpenCode
  server API and Feishu integration — read before contributing).
- **Code of conduct** — [CODE_OF_CONDUCT.md](CODE_OF_CONDUCT.md).

MIT licensed — see [LICENSE](LICENSE).
