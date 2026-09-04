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

1. Create a custom app at <https://open.feishu.cn/app>.
2. Grant these scopes and **publish** the version so the bot is usable:
   - `im:message`, `im:message:read`, `im:message:send_as_bot`
   - `im:chat`
   - `contact:user.base:readonly`
3. Add the bot to a chat (or DM it) so it can receive messages.

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