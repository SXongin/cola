# cola

[![CI](https://github.com/SXongin/cola/actions/workflows/ci.yml/badge.svg)](https://github.com/SXongin/cola/actions/workflows/ci.yml)
[![CodeQL](https://github.com/SXongin/cola/actions/workflows/codeql.yml/badge.svg)](https://github.com/SXongin/cola/actions/workflows/codeql.yml)
[![Codecov](https://codecov.io/gh/SXongin/cola/graph/badge.svg?branch=main)](https://codecov.io/gh/SXongin/cola)
[![OpenSSF Scorecard](https://api.securityscorecards.dev/projects/github.com/SXongin/cola/badge)](https://securityscorecards.dev/viewer/?uri=github.com/SXongin/cola)
[![crates.io](https://img.shields.io/crates/v/colark.svg)](https://crates.io/crates/colark)
[![Downloads](https://img.shields.io/crates/d/colark.svg)](https://crates.io/crates/colark)
[![Release](https://img.shields.io/github/v/release/SXongin/cola.svg)](https://github.com/SXongin/cola/releases/latest)
[![Platforms](https://img.shields.io/badge/platform-Linux%20%7C%20macOS%20%7C%20Windows-lightgrey)](#1-install)
[![Last commit](https://img.shields.io/github/last-commit/SXongin/cola)](https://github.com/SXongin/cola/commits/main)
[![License](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)

A bridge bot that brings the [OpenCode](https://opencode.ai) AI coding experience
into [Feishu](https://www.feishu.cn). You chat with the bot in Feishu; cola maps
Feishu threads to OpenCode sessions, streams the AI's reasoning/tools/answers
onto interactive cards, and surfaces permission and question requests as
tap-to-answer cards.

<!-- TODO(screenshots): add Feishu screenshots here when ready. Suggested
     set — a turn card mid-stream, a permission card, a question card, the
     todo panel. Save them under docs/images/ and reference by relative path. -->

## Why cola

- **A coding agent in your chat app.** Drive OpenCode from Feishu — phone or
  desktop — instead of a terminal; each Feishu thread maps to a session.
- **One card per turn, updated live.** Reasoning, tool calls and the answer
  stream onto a single interactive card with collapsible panels, a phase timer
  and a live todo panel, so a slow turn never looks like a dead one.
- **Decisions are two taps.** Permission requests render as
  Allow / Deny / Always buttons right on the card, and questions become a
  multi-select card that accepts custom answers.
- **Send a screenshot.** Images in a message or a quoted reply are attached to
  the prompt for vision-capable models.
- **Your sessions are shared.** cola attaches to the same OpenCode server and
  store as OpenChamber and the CLI, so sessions started anywhere can be adopted
  (`/switch`, with a snapshot of what's pending) and messages sent from another
  client are surfaced back into Feishu.
- **A topic per task.** `/topic <dir>` opens a Feishu topic whose first message
  creates the session in that project, with a cover card that keeps the title,
  project, branch and model visible and up to date.
- **Steer without restarting.** `/model`, `/agent`, `/think` and `/autoaccept`
  are per-session overrides applied to the next message.
- **Private by default.** A fresh cola refuses everyone until you claim it with
  a one-time code; only the Host can drive it.
- **Set it and forget it.** Autostart at boot, a singleton lock, restart and
  self-update — on Linux, macOS and Windows.

## Quick start

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

Or install from [crates.io](https://crates.io/crates/colark) with Rust tooling:
`cargo binstall colark` fetches the prebuilt release asset for your platform
(no compile), while `cargo install colark` (edition 2024) builds from source —
source builds need a C compiler and linker (the TLS stack builds AWS-LC); on
Windows, NASM as well (or set `AWS_LC_SYS_PREBUILT_NASM=1`). No system OpenSSL
is required. The binary is named `cola` either way. Updates follow the install
channel: a cargo-tracked install (`cargo install` or `cargo binstall`) is
updated with cargo — `/update` reports the command — while an archive install
uses cola's own updater (`/update`, `cola update`).

Or build from source (same prerequisites as `cargo install`):
`cargo build --release`. The [user guide](docs/user-guide.md#install) explains
each choice in detail.

You also need an `opencode` binary on `PATH` (see <https://opencode.ai>); cola
starts and manages its own server, but the binary must be runnable.

### 2. Create a Feishu app

One-time setup at <https://open.feishu.cn/app> (create a custom app):

1. Enable the **bot capability** (应用能力 → 机器人) — required before the app can
   join chats or send anything.
2. Grant these scopes (权限管理):
   - `im:message`, `im:message:send_as_bot` — send, reply to and update cards,
     read message content and download message images
   - `im:message.p2p_msg:readonly` — receive DMs
   - `im:message.group_at_msg:readonly` — receive group messages that @ the bot
   - `im:chat:readonly` — read chat names
   - `contact:contact.base:readonly`, `contact:user.base:readonly` — read user names
   - `im:datasync.feed_card.time_sensitive:write` — *(optional, experimental)*
     Feishu's Instant Reminder, used by `[bridge] instant_reminder` (off by
     default); without it pinning is skipped and everything else keeps working
3. Configure event subscription (事件与回调): use **long-connection mode** and
   subscribe to the `im.message.receive_v1` event. Card-button callbacks
   (`card.action.trigger`) arrive over the same connection automatically — no
   webhook URL needed.
4. **Publish** the version so the configuration takes effect and the bot is usable.
5. Add the bot to a chat (or DM it) so it can receive messages.

Missing scopes degrade features instead of crashing cola. The [user
guide](docs/user-guide.md#feishu-app-setup) maps every scope to what it unlocks
and what breaks without it.

### 3. Configure

Write `~/.cola/cola.toml` (or `./cola.toml` in the directory you run from, or
point explicitly with `cola --config <path>`):

```toml
[feishu]
app_id = "cli_xxxxxxxxxxxx"
app_secret = "your-app-secret"
```

That's the minimum. Every other setting is optional — e.g. Feishu's Instant
Reminder is an **experimental** opt-in via `[bridge] instant_reminder = true`
(off by default; needs the optional `im:datasync.feed_card.time_sensitive:write`
scope above). The pin lifecycle is still being designed and may change between
releases. See the [user guide](docs/user-guide.md#configuration).

### 4. Run

Register cola to start at boot/login — the recommended way:

```bash
cola autostart enable
cola autostart status   # confirm it is registered
```

- **Linux**: a systemd **user** unit. On a headless machine (no desktop login),
  also run `loginctl enable-linger $USER` (printed as a hint) so it starts at
  boot.
- **macOS**: a LaunchAgent.
- **Windows**: an `HKCU\...\Run` registry value.

`cola autostart disable` stops the instance (even a supervised one still
starting up) and removes the registration; `cola stop` stops it but keeps the
registration. Enable it **after** installing both cola and `opencode` — the
registration snapshots the binary path and your `PATH`, so re-run `enable` if
you ever move either.

To run in the foreground instead, just `cola` (Ctrl-C stops it). To detach it
from the terminal: `nohup cola >/dev/null 2>&1 &` on Linux/macOS, or
`start /b cola` on Windows.

cola attaches to an already-running `opencode serve` on the shared store
(so sessions stay shared with OpenChamber / the CLI), and lazily starts its own
when none is running.

### 5. Claim the bot

cola is **private by default**: a fresh (or upgraded) instance starts
*unclaimed* and refuses everyone. On startup it prints a one-time **claim
code** to the log (`~/.cola/cola.log`); DM the bot `/claim <code>` from your
own Feishu account to become its Host. After that only you can use it.

The code rotates on every restart, is never written to disk, and the claim
itself is one-time — restarts and upgrades never ask again. DM the bot `/help`
to get started after claiming.

## Docs

- **User guide** — [docs/user-guide.md](docs/user-guide.md): where things live,
  full configuration reference, autostart, logs, self-update, singleton &
  restart, every Feishu command, troubleshooting.
- **Contributing** — [CONTRIBUTING.md](CONTRIBUTING.md): commit conventions,
  the verification loop, PR rules.
- **Architecture** — [docs/adr/](docs/adr/), [CONTEXT.md](CONTEXT.md),
  [AGENTS.md](AGENTS.md) (includes the hard-won known pitfalls for the OpenCode
  server API and Feishu integration — read before contributing).