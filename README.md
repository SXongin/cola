# cola

[![CI](https://github.com/SXongin/cola/actions/workflows/ci.yml/badge.svg)](https://github.com/SXongin/cola/actions/workflows/ci.yml)

A bridge bot that brings the [OpenCode](https://opencode.ai) AI coding experience
into [Feishu](https://www.feishu.cn). You chat with the bot in Feishu; cola maps
Feishu threads to OpenCode sessions, streams the AI's reasoning/tools/answers
onto interactive cards, and surfaces permission and question requests as
tap-to-answer cards.

## Quick start

### 1. Install

Download the prebuilt binary from the [latest GitHub
release](https://github.com/SXongin/cola/releases/latest) and put it on your
`PATH` — **in a user-writable directory** (`~/.local/bin` is ideal). cola's
self-update replaces its own binary in place, which a root-owned path like
`/usr/local/bin` would block. Or build from source (Rust toolchain, edition
2024): `cargo build --release`.

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

That's the minimum. Every other setting is optional — see the [user
guide](docs/user-guide.md#configuration).

### 4. Run

```bash
nohup cola >/dev/null 2>&1 &
```

cola attaches to an already-running `opencode serve` on the shared store
(so sessions stay shared with OpenChamber / the CLI), and lazily starts its own
when none is running. DM the bot `/help` to get started.

## Docs

- **User guide** — [docs/user-guide.md](docs/user-guide.md): where things live,
  full configuration reference, autostart, logs, self-update, singleton &
  restart, every Feishu command, troubleshooting.
- **Contributing** — [CONTRIBUTING.md](CONTRIBUTING.md): commit conventions,
  the verification loop, PR rules.
- **Architecture** — [docs/adr/](docs/adr/), [CONTEXT.md](CONTEXT.md),
  [AGENTS.md](AGENTS.md) (includes the hard-won known pitfalls for the OpenCode
  server API and Feishu integration — read before contributing).