# cola

A bridge bot that brings the [OpenCode](https://opencode.ai) AI coding experience into [Feishu](https://www.feishu.cn). You chat with the bot in Feishu; cola maps Feishu threads to OpenCode sessions, streams the AI's reasoning/tools/answers onto interactive cards, and surfaces permission and question requests as tap-to-answer cards.

## How it works

- **Platform** = Feishu (long connection WebSocket, interactive cards).
- **Backend** = a running `opencode serve` instance. cola discovers one already running on the **default store** (so sessions stay shared with OpenChamber / the CLI — see [Known pitfalls](AGENTS.md#known-pitfalls-learned-the-hard-way)) and attaches to it. If none is running, cola starts its own.
- **Bridge** = the coordinator: one Feishu thread maps to one OpenCode session; card updates stream live as parts complete.

## Prerequisites

- Rust toolchain (edition 2024).
- An `opencode` binary on `PATH` (cola can start its own server).
- A Feishu custom app (`app_id` / `app_secret`).

## Feishu app setup

1. Create a custom app at <https://open.feishu.cn/app>.
2. Give the bot the permissions cola needs (at minimum `im:message`, `im:message:send_as_bot`; add `im:chat` if you want chat reading) and **publish** the version so the bot is usable.
3. Copy `cola.toml.example` to `cola.toml` and fill in the app id / secret. The secret is committed nowhere — `cola.toml` is gitignored.

## Configuration

```toml
[opencode]
url = "http://localhost:4096"          # preferred/fallback port
# model is optional: unset → cola uses the OpenCode server's default model
# model = "opencode/deepseek-v4-flash"

[feishu]
app_id = "cli_xxxxxxxxxxxx"
app_secret = "your-app-secret"

[bridge]
# session_file = "~/.cola/sessions.json"
# work_dir = "/path/to/a/project"
```

`opencode.url`/`password` are a fallback: cola rewrites them to whatever server it discovers running on the default store.

## Run

```bash
cargo build --release
nohup ./target/release/cola --config cola.toml >/dev/null 2>&1 &
```

cola needs the OpenCode server it attaches to to be running; it discovers it automatically (see above), so no manual `opencode serve` needed if the binary is on `PATH`.

## Logs

cola always appends to a log file — by default `~/.cola/cola.log` (no ANSI codes), overridable with `--log-file`. A restart never wipes history (append, not overwrite). When stdout is a real terminal, logs also mirror there; a redirected stdout gets no cola logs, keeping the redirect target clean.

## Singleton lock & restart

Only one cola may run at a time (two would double-handle Feishu events). The lock lives at `~/.cola/cola.lock`.

- Starting cola while another runs, **non-interactively**: cola refuses and prints a clear message. Take over with `cola --replace` or from Feishu send the old instance `/restart`.
- Starting interactively (a terminal): cola asks `旧实例 PID x 在运行，是否替换它并接管？[y/N]`.
- `/restart` re-execs cola itself with `--replace`, so the new process always takes over the lock.

## Commands

In Feishu: `/dir <path>`, `/switch <name>`, `/list [keyword] [--all]`, `/attach <id|title> [--force]`, `/forget`, `/new [name]`, `/topic <dir> [name]`, `/name <name>`, `/stop`, `/compact`, `/agent <name>`, `/model <provider/model>`, `/autoaccept [on|off]`, `/restart`, `/restart-opencode`, `/help`. Unrecognized `/...` commands are forwarded to OpenCode.

`/agent` and `/model` set per-session overrides sent with the next message and persisted across restarts. `/restart-opencode` restarts only an OpenCode server **cola itself started**; a server launched by another tool is left alone.

## Development

```bash
cargo fmt --all -- --check   # formatting
cargo clippy --workspace --all-targets -- -D warnings   # lints
cargo test    # 188 unit/integration tests
cargo build --release
```

Read `AGENTS.md` before touching the OpenCode server API, Feishu card/WS integration, or the bridge protocol — the known pitfalls there are hard-won. Reference source trees live under `/root/workspace/dev/` (opencode, cc-connect, openchamber).

## CI & releases

GitHub Actions runs the four gates above on every push to `main` and every pull request (`.github/workflows/ci.yml`). clippy, test and build run on all three platforms (Linux/macOS/Windows); runtime discovery and the singleton lock are cross-platform via `sysinfo` (no `/proc` dependency). See `docs/adr/0005-cross-platform-process-discovery.md` for the design.

Tagging a version publishes a release with the compiled binary for all three platforms. Tags follow strict semver without a `v` prefix:

```bash
git tag 1.2.3 && git push origin 1.2.3
```

`.github/workflows/release.yml` builds the release binary, packages it as `cola-<version>-x86_64-unknown-linux-gnu.tar.gz` plus a `SHA256SUMS` file, and attaches both to a GitHub release with auto-generated notes.