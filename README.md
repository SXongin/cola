# cola

A bridge bot that brings the [OpenCode](https://opencode.ai) AI coding experience into [Feishu](https://www.feishu.cn). You chat with the bot in Feishu; cola maps Feishu threads to OpenCode sessions, streams the AI's reasoning/tools/answers onto interactive cards, and surfaces permission and question requests as tap-to-answer cards.

## How it works

- **Platform** = Feishu (long connection WebSocket, interactive cards).
- **Backend** = a running `opencode serve` instance. cola discovers one already running on the **default store** (so sessions stay shared with OpenChamber / the CLI — see [Known pitfalls](AGENTS.md#known-pitfalls-learned-the-hard-way)) and attaches to it. If none is running, cola **lazily starts its own** at the moment a message needs it (a Coexistent Server — e.g. OpenChamber's — always wins over cola's own, which cola then yields; see [ADR-0013](docs/adr/0013-server-ownership-yield-and-lazy-start.md)).
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
[feishu]
app_id = "cli_xxxxxxxxxxxx"
app_secret = "your-app-secret"
```

Only the Feishu credentials are required. Optional `[opencode]` / `[bridge]`
values:

```toml
[opencode]
url = "http://localhost:4096"    # preferred/fallback port (default 4096)
start_server = "auto"            # auto|lazy (default), never (attach-only), eager (spawn at boot)
# model is optional: unset → cola uses the OpenCode server's default model
# model = "opencode/deepseek-v4-flash"

[bridge]
# session_file = "~/.cola/sessions.json"
# work_dir = "/path/to/a/project"    # default project when a conversation has no session
# log_days = 14                       # daily log retention
```

`opencode.url` is only a preferred port — a tiebreaker when several servers of
the same kind are running: cola attaches to whatever server is running on the
shared store automatically (username and password are read from the server's
environment, so nothing to configure), and a server cola did **not** start
(OpenChamber's, a manual one) wins over cola's own.

`opencode.start_server` controls when cola may start its own `opencode serve`:
`auto` (default) attaches at boot and spawns an own server only at the moment a
message needs one and none is running; `never` is attach-only (cola replies that
OpenCode is unavailable when no server exists); `eager` restores the old
start-at-boot behavior.

## Run

```bash
cargo build --release
nohup ./target/release/cola --config cola.toml >/dev/null 2>&1 &
```

cola attaches to an already-running OpenCode server automatically (see above);
with the default `auto` policy it starts its own server on demand, so no manual
`opencode serve` is needed if the binary is on `PATH`.

## Autostart

Register cola to start at boot/login with `cola autostart` (one command per
platform; the launcher runs the `cola` binary itself — Lazy Start handles the
OpenCode server):

- **Linux**: `cola autostart enable` writes a systemd **user** unit and enables
  it. Run `loginctl enable-linger $USER` (printed as a hint) so the service
  runs without a logged-in desktop session.
- **macOS**: writes and loads a LaunchAgent.
- **Windows**: writes an `HKCU\...\Run` registry value.

`cola autostart disable` removes the registration; `cola autostart status`
shows whether it is installed.

## Logs

cola always appends to a log file — by default `~/.cola/cola.log` (no ANSI codes), overridable with `--log-file`. A restart never wipes history (append, not overwrite). Logs rotate **daily**: yesterday's content moves to `cola-YYYY-MM-DD.log` and older files are swept after `[bridge] log_days` (default 14). Cross-day sessions are queried with `grep session_id=... cola-*.log`. When stdout is a real terminal, logs also mirror there; a redirected stdout gets no cola logs, keeping the redirect target clean.

## Singleton lock & restart

Only one cola may run at a time (two would double-handle Feishu events). The lock lives at `~/.cola/cola.lock`.

- Starting cola while another runs, **non-interactively**: cola refuses and prints a clear message. Take over with `cola --replace` or from Feishu send the old instance `/restart`.
- Starting interactively (a terminal): cola asks `旧实例 PID x 在运行，是否替换它并接管？[y/N]`.
- `/restart` re-execs cola itself with `--replace`, so the new process always takes over the lock.

## Self-update

`/update` (Feishu) or `cola update [--check]` checks GitHub Releases for a newer cola. If one exists it downloads the binary for your platform, verifies it against the release's `SHA256SUMS`, replaces the running binary and restarts. Platforms without a prebuilt binary (e.g. Linux aarch64) report that instead of failing. Under a systemd unit the restart hands back to `Restart=on-failure`; elsewhere cola re-execs itself.

## Commands

In Feishu: `/dir <path>`, `/switch`, `/switch <kw>`, `/switch list [kw] [--all]`, `/switch <id> [--force]`, `/switch forget`, `/new [name]`, `/topic [dir] [name]`, `/topic --adopt <kw> [--force]`, `/name <name>`, `/stop`, `/compact`, `/agent <name>`, `/model <p/m>`, `/autoaccept [on|off]`, `/restart`, `/restart-opencode`, `/update`, `/help`. Unrecognized `/...` commands are forwarded to OpenCode.

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

`.github/workflows/release.yml` builds the release binary, packages it as `cola-<version>-<target>.tar.gz` plus a `SHA256SUMS` file, and attaches both to a GitHub release with auto-generated notes. It also enforces `Cargo.toml`'s `version` matching the tag — the binary's embedded version drives self-update, so a mismatch would report "update available" forever.