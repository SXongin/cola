# cola User Guide

A **Chinese translation** lives at [user-guide.zh-CN.md](user-guide.zh-CN.md) —
English is the source of truth, and `cargo xtask check-doc-drift` keeps the
translation's structure in sync with this file.

The full operation manual for running cola day to day. For installation and the
first chat, see the [README](../README.md).

## Where things live

All runtime state defaults to `~/.cola/`:

| File | Purpose | Overridable |
| --- | --- | --- |
| `cola.toml` | configuration | `./cola.toml` (cwd) or `cola --config <path>` |
| `cola.log` | logs (append, daily rotation) | `cola --log-file <path>` |
| `sessions.json` | Feishu thread ↔ Session mapping | `[bridge] session_file` |
| `chain_records.json` | durable Chain Record: live cards, wake watermarks, Task Endings, delivered positions | beside `session_file` |
| `interactive_surfaces.json` | Permission/Question cards re-adopted after a restart (ADR-0038) | beside `session_file` |
| `authored_turns.json` | turns cola started, so a client that assigns its own ids still tells its own messages from external ones (ADR-0092) | beside `session_file` |
| `pinned_chats.json` | persisted Instant Reminder pins | beside `session_file` |
| `terminal-runs/` | Terminal Run Records for live/settled `/run` cards | beside `session_file` |
| `access.json` | Access List (the Host; ADR-0035) | `[bridge] access_file` |
| `cola.lock` | singleton lock (one cola per machine) | — |
| `restart-notify.json` | restart announcement bookkeeping | — |

Every durable state sidecar carries a format `version` (the lock, log and
one-shot restart bookkeeping do not). Records keyed by a Session also
name the **agent client** that owns them, so two clients' sessions can coexist
in one file. A file written by an older release reads as version 0 under the
`default` client and is rewritten once; a file from a **newer** release is
refused rather than misread. Renaming a `[clients.<id>]` id is therefore a
**state migration**, not a config edit: the old id's records must be re-keyed,
or they stay invisible to the renamed client (ADR-0091).

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
OpenCode **1.18.x (V1) and 2.0.x (V2) are both supported**: cola probes the
attached server's protocol at every attach/reconnect and logs the result (the
`generation` setting below is the rare override). The autostart launcher
snapshots your `PATH` at `cola autostart enable` time, so install both **before**
enabling autostart.

## Feishu app setup

cola talks to Feishu as a custom app (企业自建应用). Setup is one-time, in the
[developer console](https://open.feishu.cn/app); the
[README](../README.md#try-it) has the condensed checklist — this section is the
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
| `im:datasync.feed_card.time_sensitive:write` | 设置即时提醒 (`time_sensitive`) — optional | Instant Reminder: pin the Chat or Topic while a permission/question waits (`[bridge] instant_reminder`, off by default) | pinning is skipped (one log line); everything else keeps working |

Notes:

- `im:datasync.feed_card.time_sensitive:write` is the only **optional** scope:
  it powers the conversation-level Instant Reminder, an opt-in feature
  (`[bridge] instant_reminder = true`, off by default). The waiting-card
  message pins under the same opt-in need no extra scope — `im:message`
  covers them. Grant this one only if you want the conversation pin;
  without it cola logs the failed call and continues.
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
# generation = "auto"            # auto (default) | v1 | v2
# model = "opencode/deepseek-v4-flash"
```

- **`url`** — only a *preferred* port. cola attaches to whatever `opencode
  serve` is already running on the **shared store** automatically (username and
  password are read from the running server itself — its environment, or a V2
  Shared Service's registration file — so nothing to configure), so `url` is a
  tiebreaker among several servers of the same kind — and the fallback
  port when cola starts its own server. Among the servers cola can attach to,
  the priority is **Shared Service > Coexistent Server > Owned Server**: the V2
  `serve --service` wins, then a server another tool started, and only then
  cola's own.
- **`start_server`** — when cola may start its own `opencode serve`:
  - `auto` (default) — attaches at boot and spawns an own server only at the
    moment a message needs one and none is running.
  - `never` — attach-only; cola replies that OpenCode is unavailable when no
    server exists.
  - `eager` — the old behavior: spawn an own server at boot when none is running.
- **`generation`** — which OpenCode protocol cola speaks: `auto` (the default) or
  the forced `v1` / `v2`. On `auto`, cola probes the attached server at every
  attach/reconnect and logs the evidence (see
  [FAQ](#faq--troubleshooting) for the line to look for). Both OpenCode 1.18.x
  (V1) and 2.0.x (V2) are supported; forcing a generation exists only for a
  proxy or an unusual build the probe cannot classify, and a contradicting probe
  still logs a warning with its evidence. On `auto`, a server that cannot be
  classified leaves cola serverless rather than guessing.
- **`model`** — the default model for new sessions (`provider/model`). Unset
  (recommended on most setups) → cola pins no model and the OpenCode server
  uses **its own** default. If set, the model must exist on the server cola
  attaches to (usually `opencode/...`). Per-session overrides are set with
  `/model`.

### `[clients.<id>]` — optional, for a second Agent Client

```toml
# `[opencode]` is already the implicit Agent Client `default` — leave its name alone.
[clients.ada]
kind = "codex"                   # required: opencode | codex
# sandbox = "danger-full-access" # optional: read-only | workspace-write | danger-full-access
# approval_policy = "on-request" # optional: untrusted | on-request | never
# apply_on_adopt = false         # optional: also send the pair on thread/resume
# reasoning_summary = "auto"     # optional: auto | concise | detailed | none

[bridge]
default_client = "ada"           # which Agent Client is active (default: "default")
```

- **`kind`** — required, and it selects the adapter. `opencode` is the
  OpenCode adapter; `codex` (the `app-server` adapter, ADR-0090) resolves,
  discovers the daemon under `$CODEX_HOME` and probes its surface. Either kind
  boots: the active Agent Client is the one cola serves, and a Codex client
  needs its app-server daemon reachable (or startable, per `start_server`).
  Agent-initiated questions reach Feishu only when the daemon-side
  `[features] default_mode_request_user_input = true` is on in
  `~/.codex/config.toml`; restart the daemon and TUI after changing it.
  Without the feature the model asks in plain text. MCP elicitation requests
  (`mcpServer/elicitation/request`) work regardless of that feature.
- **Options live in the same table as `kind`.** An `opencode` Agent Client
  accepts `url`, `start_server`, `generation` and `model`, exactly as
  `[opencode]` does. A `codex` Agent Client accepts `start_server`
  (`auto`/`never`/`eager`): `never` is attach-only, while the others may start
  the shared daemon through `codex app-server daemon start` — cola never owns
  or reaps it. It also accepts `sandbox`, `approval_policy`, `apply_on_adopt`,
  and `reasoning_summary` (each described below). A
  named Agent Client does not inherit `[opencode]`'s values.
- **`sandbox` / `approval_policy` / `apply_on_adopt` (codex only)** — what a
  Codex thread runs under. cola sends the pair on `thread/start`, so a thread it
  creates can develop in this repo: `sandbox = "danger-full-access"` removes the
  sandbox and its network restriction, which is what makes `git commit`, `gh`
  and edits to `.codex/` work (the desktop app's “full access” is exactly this
  mode). The other modes are `workspace-write` and `read-only`; `workspace-write`
  keeps Codex's protected paths (`.git`, `.agents`, `.codex`, `.aws`) read-only
  and its command network restricted, so it does not unblock a commit or `gh`.
  `approval_policy` takes `untrusted`, `on-request` or `never`. The trade-off is
  real: `danger-full-access` raises no sandbox approval — there is no sandbox to
  escape — so even with `on-request` **permission cards stop arriving for that
  session**, and everything it runs has the daemon user's access (running cola as
  root means the thread can write wherever root can). `approval_policy = "never"`
  suppresses the cards as well, whatever the sandbox; a sandboxed client keeps
  its write and network restrictions. `apply_on_adopt`
  (default `false`) extends the pair to `thread/resume`: a resumed thread may be
  shared with your TUI/VSCode, so cola only adopts, never reconfigures, unless
  you opt in. Unset keys send nothing and the daemon's own default applies;
  `cola config show` prints what the file spelled. These are **thread**
  settings: a thread created before the change keeps the policy it was created
  with, and a thread your TUI created and cola adopted keeps it too unless
  `apply_on_adopt = true`.
- **A provider is creation-only.** `/new` (a Pending Session) is the only path
  that chooses a provider today: it sends the `model` + `modelProvider` pair to
  `thread/start` (`turn/start` has no such field). `/fork` is an independent
  capability planned for a later version; it will send the pair to
  `thread/fork`, and it is not a command today. A live thread's provider never
  changes: `/model` with a different provider is refused without any request and
  points at `/new`, naming `/fork` only as that later option. The provider page
  on a live thread is read-only — the current provider plus that note, with no
  clickable rows — while a Pending Session still offers the pairs its creation
  will carry. A provider is never selectable without a model: the old model name
  on a new provider is a 404.
- **`reasoning_summary` (codex only, default `auto`)** — how much of the model's
  reasoning is summarised into a Codex thread. cola sends it as
  `thread/start`'s `config.model_reasoning_summary`, and the card renders the
  result as a folded 💭 推理过程 panel; the values are `auto`, `concise`,
  `detailed` and `none`. Codex's own default on the pinned build is `none`, which
  leaves every reasoning item empty, so the card shows no thinking at all —
  `auto` is why cola's default differs. `none` restores the silent behaviour (and
  saves the summary's tokens). Like the sandbox pair it is a **thread** setting:
  new threads only, and an adopted thread keeps its own unless
  `apply_on_adopt = true`. `cola config show` prints it only when the file spells
  it.
- **A full-access default for your other clients is Codex's config, not cola's.**
  If your own TUI/VSCode threads should also default to full access, set
  `default_permissions = ":danger-full-access"` in the `~/.codex/config.toml` of
  the daemon's `$CODEX_HOME` (or pass `-s danger-full-access` per run), restart
  the daemon, and create a new thread — existing threads re-apply the settings
  they were created with. cola never writes that file: it carries your provider
  tokens and is shared with the daemon. Two consequences matter. The key only
  affects clients sharing that `$CODEX_HOME`, and under a Codex profile
  (`:workspace`) a `[sandbox_workspace_write] network_access = true` is ignored —
  the profile's restricted network wins, which is why `gh` cannot reach
  `api.github.com` there. Prefer per-run `-s` if only occasional full access is
  needed.
- **Only a kind's own keys are accepted.** Any other key — a typo such as
  `model_typo`, or a nested `[clients.ada.opencode]` table — is a config error
  naming `clients.<id>.<key>`, and so is an option on a kind that ignores it
  (for example `kind = "codex"` plus `model`), which the running client would
  never read. Never a silently ignored setting.
- **`default_client`** — the id of the active Agent Client. Unset means
  `default` (`[opencode]`). Exactly one Agent Client is active per cola process
  (ADR-0090); switching Agent Clients is this key plus a restart.
- **`[opencode]` stays valid forever.** cola reads it as the implicit
  `clients.default` / `kind = "opencode"` and never rewrites your file — no
  `[clients.*]` table is generated and no key is renamed. An explicit
  `[clients.default]` is rejected as a duplicate of that implicit Agent Client:
  give the table another id, or move its options into `[opencode]`.
- **An Agent Client id is durable state.** cola records the active id beside
  each Session, so renaming an id is a state migration, not a config edit.

### `[bridge]` — all optional

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

- **`session_file`** — where the thread↔session mapping is persisted.
- **`access_file`** — where the Access List (the Host, ADR-0035) is persisted.
  A missing file means the bot is unclaimed and refuses every message until
  `/claim`.
- **`work_dir`** — default directory for new sessions when a conversation has no
  active session (fresh chat, or after `/switch forget`). Defaults to the process
  cwd. `/new` inherits the active session's directory (or the pending's);
  `/dir` names an explicit one. Both only declare the session: it is created by
  the conversation's next message.
- **`default_auto_accept`** — **off by default**: sessions cola creates (a fresh
  chat's first message, `/new`, `/dir`, `/topic`, and a 404 recreate) start with
  Auto-Accept on, so their tool-permission requests are answered automatically
  instead of surfacing a card. Adopting an existing session never enables it —
  the session keeps its own state — and `/autoaccept off` still turns it off per
  session.
- **`group_completion_notice`** — in group chats, reply to the requester with a
  short completion notice (the streaming card is patched in place, so it does not
  push a new notification). `false` disables it. p2p chats don't need it.
- **`instant_reminder`** — opt-in, **off by default**: use
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
- **`default_client`** — the id of the active Agent Client; see
  `[clients.<id>]` above. Unset means `default` (`[opencode]`).

### `[review]` — all optional

```toml
[review]
# base = "origin/main"
```

- **`base`** — the base ref `/diff` compares the working tree against when the
  command names none. Unset resolves `origin/main`, then `main`, and errors if
  neither exists. `/diff --base <ref>` overrides it for that call.

### `[worktree]` — all optional

```toml
[worktree]
# root = ".worktrees"
```

- **`root`** — the parent directory `/worktree new` creates checkouts in. A
  relative value resolves against the project's **primary checkout** (the main
  worktree, not the session's own directory — that session may itself be in a
  linked worktree); an absolute path is used as given. Defaults to `.worktrees`.
  OpenCode's own `worktree.directory` in a project's `opencode.json` is a
  separate setting read by the desktop/TUI; cola reads only this one, so set both
  to the same value when the two clients should agree.

## Run

```bash
nohup cola >/dev/null 2>&1 &
```

cola attaches to an already-running OpenCode server automatically; with the
default `auto` policy it starts its own server on demand, so no manual
`opencode serve` is needed. If no config file exists yet, cola prints a hint
telling you where to put one and exits.

## Multi-client sync

cola adds no per-client adapter: sync happens at the **server + shared store +
session** layer. If every client talks to one default-store server and opens the
same session, a prompt from any of them reaches the others automatically.

**V2 canonical server.** OpenCode V2's `opencode serve --service` is the
machine-wide rendezvous the TUI, the desktop app and the Web UI resolve from
zero arguments — a bare `opencode` *is* that process. cola starts it when no
server is running (`start_server = "auto"` or `"eager"`) and otherwise attaches
to the running one. V1 has no service and keeps using `serve --port`.

**Restarting under autostart.** When cola runs from its systemd **user** unit
(autostart), the unit's default `KillMode=control-group` stops the Shared
Service cola started along with cola — on `systemctl --user restart cola` and on
a `/restart` the unit's process handles. That is the unit's cgroup boundary, not
a cola leak: cola detaches the daemon into its own process group, but the unit
still owns every process it started. To keep the service independent of cola,
start it yourself with `opencode service start` before cola (or run cola outside
the unit), or set `KillMode=process` on the unit so a restart stops only cola.
Started any other way, the service outlives cola.

**OpenChamber** raises its own managed server on a random port instead of
reading the registration file, so it does not sync until you point it at the
service:

```bash
OPENCODE_HOST=$(jq -r .url "${XDG_STATE_HOME:-$HOME/.local/state}/opencode/service.json") \
OPENCODE_SERVER_PASSWORD=$(jq -r .password "${XDG_STATE_HOME:-$HOME/.local/state}/opencode/service.json") \
OPENCODE_SKIP_START=true openchamber
```

The registration lives under `$XDG_STATE_HOME` (default `~/.local/state`) —
the state home the daemon resolved when it registered itself, which is the same
one cola reads. `OPENCODE_HOST` must carry an explicit port and no path.
`OPENCODE_SERVER_PASSWORD` is the name OpenChamber reads for the server's
password (OpenCode's server also accepts `OPENCODE_PASSWORD`, but OpenChamber
does not). `OPENCODE_SKIP_START=true` makes OpenChamber attach instead of
raising its own server; without it, it launches that random-port managed server
again.

**The same session is the only lever.** One server is not enough — both sides
must open the same session:

| Client | Join the shared session with |
| --- | --- |
| TUI | `opencode -s <session id>` |
| Feishu | `/switch <id\|short hash\|title>` |

`/new` switches the Active Session; open that new session in the TUI with `-s`
and the two keep syncing. Non-active sessions are not followed: an external
message or reply there is not pushed to Feishu (ADR-0017), while
permission/question cards are still raised, because those are polled per
directory rather than per session.

**Two servers.** With more than one default-store server running, cola picks
exactly one, deterministically — a Shared Service beats a Coexistent Server
beats cola's Owned Server. It never aggregates them, and two servers do not
share sessions, so do not expect both to sync. The log warns which server was
chosen and which were ignored, and
[`cola status`](#inspect-status--doctor--config) lists the ignored ones.

**Config semantics.** `[opencode] url` is a tiebreaker among servers of the same
class — never a pin, and it cannot outrank the service class. `start_server`
keeps its three states: `auto` starts a server only when a message needs one,
`eager` starts it at boot, and `never` attaches only. `opencode service set
disabled true` (OpenCode's own setting) makes cola skip starting the service; it
then attaches a running server, or falls back to its Owned `serve --port`.

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

`cola logs` prints the tail of the log file (`-n N` lines, default 50);
`--date YYYY-MM-DD` reads a rotated day's file, and `-f` follows new lines,
reopening the fresh file across a midnight rotation.

## Inspect (status / doctor / config)

`cola status` prints a one-shot snapshot: run state, autostart, claim, the
resolved config and log paths, the active agent client (its id, kind, endpoint,
attachment and advertised capabilities), and the attached OpenCode server with
its generation. `--json` emits the same facts for scripts; the new `client`
object carries the client-scoped rows and the existing keys are unchanged.

`cola doctor` runs the checks a support request needs — config parse, Feishu
credentials, the client binary and runtime, the singleton lock, the log path,
and the autostart binary path — each with a ✅/⚠️/❌ mark and a summary, and exits
non-zero when a check fails. For an OpenCode client it probes the shared server
as before; for a Codex client it reports the app-server daemon, whether your own
client is attached to it, and whether the runtime advertises the expected
surface. Attachment is proven from the client process's open socket descriptors
on Linux (`/proc`, `ss`), not from its argv: an argv match that cannot be proven
is a warning, while a `--no-daemon` client or one pointed at another endpoint is
a named failure. A missing daemon is a failure too — `start_server = "never"`
only stops cola from starting one; run `codex app-server daemon start` yourself
and point your client at it. With no local client at all (a Feishu-only
deployment) the check passes, because there is nothing to attach.

`cola doctor` proves the configured provider can complete a real turn: it starts
a minimal turn on a scratch thread on the **shared** daemon (which cola does not
own) and observes `turn/completed`, failing on a retryable or terminal error or
when the turn does not finish within its 60-second budget. That call writes to
the shared daemon and **may incur provider spend**: doctor starts the named
scratch thread, then interrupts the turn while it is live and deletes the thread
(a cleanup failure is reported as a warning). `--no-turn-check` skips the probe;
`--check-turn` is accepted as a no-op alias for scripts written against the
previous release. No check reads a ChatGPT account state: a provider-keyed
`[model_providers.*]` setup is fully green.

`cola config path|show|validate` prints the resolved config path, the effective
config with the app secret masked, or a parse check. `cola version` mirrors
`--version`. `cola completions <shell>` prints a completion script; with
`--install` it writes that script into the shell's user completion directory
(honoring `XDG_DATA_HOME`/`XDG_CONFIG_HOME`, `--dry-run` to preview, the shell
inferred from `$SHELL` when omitted), never overwriting a file cola did not
generate. Automatic install is Unix-only, and a symlinked completion, a
hard-linked one, or a symlinked directory component is refused rather than
followed. PowerShell has
no automatic install — redirect the script and source it from `$PROFILE` — and
zsh prints the directory to add to `fpath`.

## Singleton lock & restart

Only one cola may run at a time (two would double-handle Feishu events). The
lock lives at `~/.cola/cola.lock`.

- Starting cola while another runs, **non-interactively**: cola refuses and
  prints a clear message. Take over with `cola --replace`, or from Feishu send
  the old instance `/restart`.
- Starting interactively (a terminal): cola asks
  `⚠️ 另一个 cola 实例（PID x）正在运行。是否替换它并接管？[y/N]`.
- `/restart` re-execs cola itself with `--replace` (keeping its startup args and
  log redirect), so the new process always takes over the lock. The takeover is
  robust to the old instance mid-`exit()`: an owner that is no longer
  functionally alive (dead, a zombie, or tearing down) is reclaimed instead of
  blocking the restart.
- Under a systemd unit, `/restart` (like `/update`) does NOT spawn a child — it
  exits with a restart code and lets the unit's `Restart=on-failure` bring cola
  back up from the same ExecStart.

`cola restart [-y]` restarts the running instance without a manual
stop-then-start: through the platform supervisor when it owns the running
instance, otherwise by re-execing the lock holder's own command line and
working directory detached (plus `--replace`). Both paths verify a new instance
took the lock before reporting success.

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
| `/cat <path>` | Open a Host file in a File Viewer card: text uses a fenced code block with an extension language hint (a markdown file keeps its source and offers a 「📖 渲染预览」 toggle), `.csv`/`.tsv` uses a markdown table, and images are embedded when Feishu's limits allow. Other binary files, images beyond those limits and text beyond the 8 MB preview cap use the File Message lane only up to Feishu's 30 MB upload cap; above it, nothing is sent and the card says 未发送. The card names the resolved path, size and type. `/cat <directory>` opens the Directory Browser there; bare `/cat` answers with usage (`/help cat` gives details). `~` expands, relative paths resolve against the session directory (the current project while a session is pending), and absolute paths resolve as absolute paths, including outside the session directory |
| `/diff [--base <ref>]` | Send this branch's **repository review** as one self-contained HTML File Message: committed changes since the fork point, uncommitted tracked changes, and untracked files. The base is `--base`, else `[review] base`, else `origin/main`, else `main`; cola never fetches, never stages entries (no `git add -N`) and never changes tracked content; git may refresh index stat data. Runs in the conversation's current project directory and replies to the command message (its chat or topic). An empty review answers `分支 <branch> 相对 <base> 没有改动`, while an empty repository with no commit answers `仓库还没有提交，没有可对比的改动。`; non-git directories and missing bases answer their own readable error. One `/diff` per chat per 10 seconds (`稍后再试`) |
| `/dir` | Recent Directories card: pick a folder and declare a session there, or open it as a fresh topic (每行「建话题」= `/topic <dir>` 的免打字版); its 📁 浏览目录 button opens a filesystem browser — drill into subdirectories, type a path (前往 browses it, 选择 declares it), toggle dot-directories, filter child names |
| `/worktree` | Worktree card: this project's git checkouts — the main checkout included and labelled 「主检出」 — each with its branch, ⚠ dirty marker, session count and ✅ current marker. Read-only: to move the conversation into one, use `/dir <path>`; to adopt an existing session there, `/switch <name>`. Dirty is read per checkout, so a row that misses the 5 s bound shows `?` |
| `/worktree new <name> [--base <ref>]` | Create a worktree at `<root>/<name>` on branch `wt/<name>` from the current session's HEAD (or `<ref>`) and declare the session there — nothing is moved, so a running turn is untouched. `<name>` is normalised to a lowercase ASCII slug (`[a-z0-9.-]`, at most 40 characters); a name with Chinese is refused with an example. An existing directory gets a `-2` suffix, an existing branch that no checkout holds is attached, and cola never removes worktrees |
| `/switch` | Session card: browse / search / adopt / new |
| `/switch <kw>` | Switch to a session by name/dir/id (adopts foreign ones; no match opens the card pre-filtered) |
| `/switch <id> [--force]` | Take over a session by id/title |
| `/switch forget` | Un-map this chat's session (the server session stays) |
| `/sub` | Child-session card: the current session's direct child sessions, read-only (运行中/空闲 per row) |
| `/sub list [kw]` | Same card, filtered by keyword (title/dir/id); the filter and page round-trip like `/switch` |
| `/sub attach <id\|标题> [--force]` | Take over one of the current session's direct child sessions into this chat (Session Snapshot receipt; `--force` steals a mapping owned by another chat) |
| `/new [name]` | Declare a new session in the current project — created by the next message (no session → default dir) |
| `/topic [dir] [name]` | Open a new Feishu topic in `<dir>`; its first message creates the session (bare `/topic` uses the current project) |
| `/topic --adopt <kw> [--force]` | Open a topic around an existing session |
| `/name <name>` | Rename current session (server-side, visible to all clients; on a pending, set the creation title) |
| `/stop` | Interrupt execution |
| `/compact` | Compact context |
| `/card` | Pull the live card down to the newest position after mid-turn command replies bury it (replies a notice when no turn is running) |
| `/agent <name>` | Switch agent (takes effect next message; persisted; `--reset` clears to the server default) |
| `/model <p/m>` | Switch model (OpenCode: takes effect next message, persisted; Codex: records a one-shot pick for the next new message, dropped on restart) |
| `/think [level]` | Set/clear thinking level, per model (OpenCode: next message, persisted; Codex: one-shot for the next new message, dropped on restart) |
| `#<id> [#<id> …] [text]` | Load OpenCode skills into this message's prompt (e.g. `#implement-spec 644`); the whole message is submitted verbatim, and a `#<id>` that matches no skill stays plain text (`#644`, `C#caveman` and `##foreman` are never skill tokens). A bare `/skill` opens the skill picker — searchable by id/name/description and paged 20 rows at a time — and `/skill <id>` is an alias for ids the `#` form cannot type (`2fa-helper`, `web_scrape`) |
| `/autoaccept [on\|off]` | Show/switch auto-allowing tool-permission requests for this session. A permission whose target the card cannot show in full (an over-long command) is still declined, never approved unseen |
| `/restart` | Restart cola (keeps startup args + log redirect) |
| `/restart-opencode` | Restart the OpenCode server (cola's Owned Server or the V2 Shared Service) |
| `/update` | Update cola via its install channel (GitHub Releases self-update, or the cargo command for `cargo install`/`cargo binstall` installs) |
| `/version` | Show cola version and build provenance (release / crates.io / dev build) |
| `/help [command]` | List commands, or show detailed help for one |

Notes:

- `/cat` reads bytes on the Host where cola runs — no model is involved, and the
  behavior is the same across OpenCode generations. The viewer names the resolved
  path, size and type; text appears in a language-hinted fenced block (an
  unsupported language hint falls back to plain text; large text previews page
  by lines), `.csv`/`.tsv` as a markdown table, and images are embedded when
  Feishu's limits allow. Other binary files, images beyond those limits and text
  beyond the 8 MB preview cap use the File Message lane only up to Feishu's 30 MB
  upload cap; past it, nothing is sent and the card says 未发送. A preview that
  cannot fit in the card may use the Rich Text Message lane, subject to Feishu's
  rendering; content beyond that lane's 30 KB budget uses the File Message lane
  (subject to the same upload cap). The card reports the delivery outcome.
  「发为文件」 sends the original bytes as one File Message. It remains available
  for text, embedded-image and Rich Text previews; it is suppressed after a File
  Message has already carried the bytes. If a File Message send times out, the
  card says the result is unknown and does not retry to avoid a duplicate.
  For a markdown file (`.md`/`.markdown`/`.mark`) the card keeps its source
  preview and offers the document's **rendered** text behind its first button
  「📖 渲染预览」 (「查看源码」 switches back). A render too large for the card is
  not offered at all; the File Message stays the complete, uncapped render.
  For a **text** `.html`/`.htm` file the transfer **leads** the row as
  「🌐 预览网页」: Feishu renders an uploaded `.html` as a page when the file
  message is opened (inline scripts included), so the File Message is that
  file's real viewer — no card component renders HTML.
  「🔄 重新读取」 re-reads and updates the viewer in
  place. 「📂 所在目录」 opens the file's parent in the Directory Browser;
  「← 返回目录」 returns from a browse-row viewer to its listing. A write/edit
  Tool Panel's folded header names the file it touched, and its 「📂 打开」 button
  rides inside the fold — expand the panel, then open the file in the viewer. A
  `write` panel also previews the content it wrote (fenced with the file's
  language, clipped to the same 3000-character preview a `read` shows, with the
  same truncation note). For
  an HTML page (`.html`/`.htm`) that button instead reads 「🌐 预览网页」 and sends
  the file as one File Message directly, without opening the viewer first: the
  panel already showed the markup, so the page its client renders is the only
  thing left to reach. A File Message
  is a file for the reader to download or hand to the client; cola does **not**
  promise the client will inline-preview it. Feishu's own preview renders
  `.md` and `.html` (measured), but that is the client's behaviour, not
  something cola controls.
- The **Path Scope** (`~` expanded, relative paths resolved from the session
  directory, absolute paths resolved as absolute paths) is a **privacy hint, not a security
  control or boundary**: any path the Host can read is reachable. Missing,
  unreadable and empty paths are reported distinctly with the resolved path.
  Examples: `/cat src/main.rs`, `/cat ~/notes.md`, `/cat /etc/hosts`. A card's
  content is readable by everyone in its chat, so a group chat can read whatever
  a Host opens. The Access List (`/claim`) gates who may **act** in cola (send
  commands or drive a session), not who may **read**: card content is visible to
  everyone in the chat.
- `/diff` answers with a single self-contained `.html` **File Message** (no
  card): open it to read the branch review, with per-file folds and red/green
  hunk shading. The artifact is generated fresh on every call and sent into the
  command's own chat or topic; an empty review sends no file, and a send whose
  outcome is unknown is not retried (a retry could duplicate the message). The
  10-second per-chat cooldown exists to stop a double click from uploading the
  same review twice, so a deliberate re-run after an edit needs a short wait.
- `/new`, `/dir`, `/topic` and `/worktree new` only **declare** the session —
  the conversation's next non-command message creates it in the chosen directory
  and maps it here, so a mistaken `/new`, `/dir`, `/worktree new` or `/topic` can
  be corrected with another `/new`, `/dir`, `/switch` or `/topic` before then
  without leaving a session in the shared store. Inside a topic that is still
  waiting for its first message, `/switch <id>` re-points the topic at an
  existing session instead. The `/dir` card's pick and 建话题
  follow the same timing (建话题's cover card shows 「下一条消息创建」
  until then). This is why `/worktree new` is safe while a turn is running: it
  writes a Pending Session and moves nothing, so the in-flight step is never
  interrupted (the reason #428's mid-turn move was a dead end).
- `/sub` is the read-only view of the current session's **child sessions** —
  the ones its `task` tool calls created. Each row carries the child's title,
  id, agent, last activity and live state (运行中 when Busy/Retry, 空闲 when
  Idle), read one status call per rendered row; children of other sessions
  never appear, and only **direct** children are listed (no nested descendants,
  no transcripts). `list <kw>` narrows by title/dir/id, six rows per page, and
  the keyword/page survive every rebuild like the `/switch` card. A
  conversation with no Active Session (or a Pending Session) opens the plain
  empty state. The `/sub list` view is observation only: it never messages a
  child. `/sub attach <id|id-prefix|title> [--force]` takes one of the current
  session's **direct** children over into this chat — the same adoption as
  `/switch` (Session Snapshot receipt with its pending permissions/questions;
  an owner refusal naming the other chat unless `--force`, which steals that
  mapping). The query accepts the same forms as `/switch` (exact id, unique
  id-prefix, title substring), scoped to those children: unknown and ambiguous
  queries are reported, and a session that is not a direct child is refused.
  Taking over a running child is allowed and the snapshot shows its live state.
  The parent stays mapped, so `/switch` switches back, and re-running `/sub
  attach` for the already-active child changes nothing. This is cola's only
  sanctioned way to take over a child; no message is ever injected into a
  running one (ADR-0054).
- The `/dir` card's rows are a **union** (ADR-0046): the shared store's session
  directories, the directories cola has mapped — they survive another client
  deleting their sessions — and the conversation's current directory, so 当前
  is always markable and a pending-only directory is never missing. "Recent"
  therefore means what cola knows, not what still exists on the server: a
  directory whose sessions were cleaned up can reappear. `/switch forget` and
  removing the thread are what drop a mapped directory.
- The union only holds directories cola already knows; its 「📁 浏览目录」 button
  opens the **Directory Browser** (ADR-0078), which reaches anything on the
  machine: a clickable breadcrumb, drill-down directory rows (repos first,
  📦-marked), then file rows showing each file's name and size under 「子目录与
  文件」. Clicking a file row replaces the browse card with its File Viewer in
  place; 「← 返回目录」 restores that listing, including its page, filter and
  hidden-item setting. The card also has a 「显示隐藏项」 toggle, child-name
  filter, and path form (前往 browses a typed path, 选择 declares a session
  there — like `/dir <path>` without leaving the card). 「没有子目录或文件」
  is shown when the listing is empty; a path that no longer exists falls back to
  its nearest existing ancestor, and only existing directories can be selected.
  **Path Scope is not a security boundary**: the browser can reach any directory
  the Host process can read. In a group chat everyone can read the card, while
  the Access List gates who may act in cola, not who may read.
- `/agent`, `/model`, `/think`, `/autoaccept` are **per-session** and apply from
  the next message; on OpenCode they persist across restarts. On OpenCode 2 the first three are
  durable session state — the pick is written to the session itself (`POST
  /api/session/{id}/model|agent`, the thinking level inside the model ref), so it
  persists with nothing re-sent and other clients on the shared store see it. On
  a session that is still pending (`/new`, `/dir` or `/topic` before its first
  message — including its card forms), the override is recorded on the pending
  and applies to the session the first message creates. A Codex client is the
  exception: its `/model` and `/think` are a one-shot next-new-message pending,
  dropped by a restart — see 「`/model` on a Codex client」 below.
  `/model`'s value must exist on the server cola attaches to. Bare `/model` opens
  the provider → model picker, whose intro shows the model the next message will
  actually run (the session's own selection, else `[opencode] model`, else what
  the server recorded). `/compact` and `/stop` need a real session and keep their
  "还没有会话" replies on a pending.
- `/restart-opencode` restarts an Owned Server or the V2 Shared Service
  (`opencode service restart`); a server launched by another tool is left alone
  (`NotOwned`).
- Topic rule: inside a topic already bound to a session, `/switch`, `/new`,
  `/dir` and `/sub attach` are rejected — go back to the main conversation. A
  topic that has never bound a session (including a `/topic`-opened topic still
  waiting for its first message) can use them to bind its single session.

### `/model` on a Codex client

Codex's model axis is not a provider → model tree: a provider is a **creation
fact**, a model is **per turn**, and the built-in catalog is **global**. Bare
`/model` therefore shows one current line and **one model page**; a
`切换 provider…` row at the bottom opens the provider page. There is no
「选择 provider（共 1 个）」 pseudo-level:

- **A pick is recorded, not switched.** `/model` records the choice as a
  one-shot pending pair (model plus its `/think` level, in memory) and answers
  「已记录：将在下一条新消息生效」. The card shows two facts: **当前** — the
  thread's own `provider / model` with its reasoning level, as the adapter holds
  it (the newest `thread/read` or `thread/settings/updated`) — and **待应用** —
  the recorded pair with 「下一条新消息生效」.
  A pick never rewrites the current line before the server has run it, so a
  picker opened after another client changed the thread shows that change, and
  the footer and the picker cannot disagree.
- **The next new message applies it — once.** That turn's `turn/start` carries
  `model` + `effort`; when the turn is accepted the pick is **consumed**, and
  every later message carries no model and no effort: the thread's own default
  is the fact, so a peer client's later change is respected and the read-back
  only refreshes 当前. Only a genuinely new turn counts — a steer, a turn folded
  into the running one, and a command such as `/compact` do not consume it. A
  definite refusal (a JSON-RPC error) keeps it for a retry; an ambiguous submit
  (a timeout, a lost response) drops it, because the server may already have
  accepted it; a cola restart drops it too, so re-pick with `/model`.
- **`/think` is the same one-shot window.** On a live Codex thread a level is
  recorded as the next new message's `effort` (keeping any pending model), not
  a per-prompt override; once it applies, later messages carry no effort.
  `/think --reset` clears the pending level, and a fresh pick on a model that
  does not declare it clears it as usual.
- **A Pending Session's pick is a creation parameter.** Before the first message
  there is no thread, so the pair is `thread/start`'s `model` + `modelProvider`
  and the reply says 「将在会话创建时生效」 — never 待应用. `/think` there is the
  same creation intent: it rides the first new turn once the thread exists.
- **「模型（每轮可换）」** lists the model-only sources: `model/list`'s built-in
  catalog (grouped as 「内置目录（全局，未校验）」 — those ids name no provider and
  may run through any of them), the config's top-level `model` (配置默认), and
  **the current provider's** `/models` ids. A pick is recorded, not sent on every
  prompt: it reaches the next new message's `turn/start.model` once.
- **An id outside every list is typed.** `/model codex/<id>` records it, and
  `/model <provider>/<model>` records a pair. Codex validates neither one: only
  the catalog's entries carry a display name, a context window and reasoning
  levels, so a typo shows up as the provider's own error on the next message.
- **`[profiles.*]` is not read.** Codex 0.134+ no longer reads it (a
  `--profile` layers `$CODEX_HOME/<name>.config.toml`), so cola ignores the
  table: a profile neither adds a model row nor supplies a provider pair.

**The `/models` probe is best effort, and its refusal is not an error.**
`GET <base_url>/models` is not part of the Responses API — `wire_api =
"responses"` does not promise it. cola asks each `[model_providers.<id>]` with
that provider's own `experimental_bearer_token`, with 2 s per probe, and caches
the answer: a definitive one (403, 404, 401, `env_key`, or the decoded models)
for three minutes, a **transient** one (a timeout, a network error) for about
20 seconds so the next open retries. The provider page shows the reason as a
short tag on the provider row (`403`, `超时`, …) — never a paragraph, and never
on the model page. The catalog and the config default are always
there, and a card callback answers from the cache, so a first cold click may
show the providers without their probed models until the next open fills them. The token is sent only in the
request's `Authorization` header: it never appears on a card, in a log line or in
an error message.

**A provider is creation-only.** The provider page (the `切换 provider…` row)
holds the configured pairs, labelled `provider/model`. A provider is chosen only
when a thread is created: `/new` sends the pair to `thread/start`. `/fork` is an
independent capability planned for a later version (it will use `thread/fork`);
cola has no `/fork` command today, so typing it is submitted as an ordinary
prompt. `turn/start` has no provider field, and a provider is never selectable
without a model — the old model name on a new provider is a 404. So:

- **A new thread takes the pair directly.** Pick a provider row on a Pending
  Session (before its first message) and the pair rides `thread/start`.
- **A live thread's provider cannot change.** The provider page on a live thread
  is read-only: it shows the current provider and says the provider can only be
  changed in a new session (with `/fork` as the later, independent option).
  `/model` with a different provider is refused without any request — no
  `turn/start`, no `thread/resume` — and points at `/new`, naming `/fork` only
  as that later capability.
- **A same-provider pick is just a model change.** A `codex/<id>` catalog/default
  ref, or one of the thread's own provider's `/models` rows, is recorded and
  rides the next new message — no resume, no reload, no thread change.
- **`codex` is a reserved name.** The built-in catalog's ref is `codex/<id>` —
  a shared neutral namespace, never a channel — so a `[model_providers.codex]`
  cannot be selected: the provider section lists it as 「`codex` 是内置目录的保留名，
  不能在这里选择」 and offers no pair (the pair value would collide with the
  catalog's own). The classification is lexical, so a catalog pick never reads
  your config.

`/model <provider>/<model>` follows the same rule: a different provider on a
live thread is refused (never a silent thread change), while a Pending Session
takes the pair at creation.

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

**Does cola work with OpenCode 2?** Yes — OpenCode 1.18.x (V1) and 2.0.x (V2) are
both supported, and cola picks the generation automatically: nothing needs
configuring, and upgrading OpenCode in place needs no cola change. Every
attach/reconnect logs the choice and its probe evidence:

    Attached to OpenCode server at http://localhost:4096 — generation=v2 (probe: GET http://localhost:4096/api/info -> 200 application/json (V2 info envelope, version 2.0.18))

If the probe cannot classify a server (a proxy or an unusual build), force the
generation with `[opencode] generation = "v1"` or `"v2"`. The store is shared,
and V2 migrates it in place on first boot, so the upgrade affects every client
using that store; session ids are preserved, so the chat↔session mappings
survive.

**I moved the cola binary and now autostart is broken.** Re-run
`cola autostart enable` — the launcher snapshots the binary path at enable time.
<!-- ruleset verification commit -->
