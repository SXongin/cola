# R4 — OpenChamber v2.0.x + OpenCode 2: integration, discovery, and whether cola can keep attaching

Ticket: #357 on wayfinder map #353. Author: research agent. Date: 2026-09-26.

**Question.** How does OpenChamber v2.0.x integrate with OpenCode 2, and can cola (a Feishu bot
that discovers the `opencode serve` process OpenChamber runs) keep attaching the way it does today?

## TL;DR / verdict

**cola's discovery mechanism survives OpenChamber v2 essentially unchanged, but cola's API surface does
not.** Two separate findings, and the distinction matters:

1. **Attach: still works.** OpenChamber v2 still *spawns a plain child process*
   `opencode serve --hostname 127.0.0.1 --port <ephemeral>` and still puts
   `OPENCODE_SERVER_PASSWORD` in that child's environment. Process cmdline, `--port` argv, and
   `/proc/<pid>/environ` are all exactly what cola's `scan_processes()` already reads
   (`src/bridge/discovery.rs:104-155`). No new discovery work is required.
2. **Protocol: breaks.** OpenCode 2.0.18 serves **only `/api/*`** — there is no unprefixed V1
   compatibility surface left. cola currently calls a mix of unprefixed V1 paths
   (`/session/{id}`, `/session/{id}/message`, `/session/{id}/prompt_async`, `/permission/{id}/reply`,
   `/question/{id}/reply`, `/session/{id}/abort` — `src/opencode/client.rs:278-756`) and one
   `/api/session/{id}/compact`. All of the unprefixed ones 404 on OpenCode 2, and the permission
   endpoints moved *and* changed body shape. This is a protocol-port ticket, not a discovery ticket,
   but it gates any real "cola works on OpenCode 2" claim.
3. **Do not route cola through OpenChamber's HTTP proxy.** It is not a transparent passthrough
   (it rewrites `GET /api/session` and `GET /api/session/:id`), it is unauthenticated only when no UI
   password is set, and it is explicitly an internal surface. See §4.3.

**One new operational hazard** (not present on V1): OpenChamber v2 **rotates the managed server's
password on every managed start and every managed restart** (`rotateManaged: true`,
`lifecycle.js:733`). The password also lives in OpenChamber's *own* process env
(`auth-state-runtime.js:42`) as well as the child's, so cola can still read it either way — but any
password cola caches goes stale on every OpenChamber-managed restart, and there is now a
"Restart OpenCode" button in the UI (v2.0.1) that makes that a user-triggered event.

---

## 1. v2 server lifecycle & discovery

### 1.1 OpenChamber spawns a separate `opencode serve` child process (not a service, not embedded)

Exact mechanism, at tag `v2.0.2`:

| Step | Code |
|---|---|
| Port: if none configured, allocate a **free ephemeral port** by binding `:0` and closing | `packages/web/server/lib/opencode/lifecycle.js:560-592` (`server.listen(0, hostname)` at `:590`) |
| Hostname defaults to loopback | `packages/web/server/lib/opencode/env-config.js:84-90` (`OPENCHAMBER_OPENCODE_HOSTNAME` else `'127.0.0.1'`) |
| Password: **rotated on every managed start** | `lifecycle.js:733` — `ensureLocalOpenCodeServerPassword({ rotateManaged: true })` |
| argv | `lifecycle.js:396` — `let args = ['serve', '--hostname', hostname, '--port', String(port)];` |
| spawn | `lifecycle.js:432-437` — `spawn(binary, args, { cwd, env: processEnv, detached: process.platform !== 'win32', windowsHide: true, stdio: ['ignore','pipe','pipe'] })` |
| child env | `lifecycle.js:770-780` — `{...shellEnv, ...process.env, ...managedOpenCodeEnv, PATH: envPath, OPENCODE_SERVER_PASSWORD: openCodePassword}` |
| readiness | `lifecycle.js:511` — parses the child's stdout line `server listening on <url>` |
| version gate | `lifecycle.js:1004` — `"The server did not identify itself as OpenCode 2.x. OpenChamber requires OpenCode 2.x"` |

So the command on this machine today (observed, running OpenChamber 1.21.0) is already the same
shape, and v2.0.2 produces the same shape:

```
/usr/sbin/opencode serve --hostname 127.0.0.1 --port 44475
# env: OPENCODE_SERVER_PASSWORD=BwI7VVr6… OPENCODE_BINARY=/usr/sbin/opencode
#      OPENCODE_CONFIG_CONTENT={"plugin":["file:///root/.config/openchamber/agent-tool/openchamber-plugin.js"]}
```

### 1.2 It is *not* the `opencode service` background daemon

OpenCode 2 does have a background-service mode, but OpenChamber deliberately does not use it:

* `opencode serve` in v2 takes `--service` (background service), `--stdio`, or neither
  (`packages/cli/src/commands/handlers/serve.ts:9-17` → `mode: "service" | "stdio" | "default"`).
* With no `--service`, `mode: "default"` and `foreground = true`; the server is started **in the same
  process** via `import("@opencode/server/process")` and then the command blocks on `Effect.never`
  (`packages/cli/src/server-process.ts:60-155`).
* The background service is a separate registered daemon with its own port/password config
  (`opencode service start|stop|restart|status|set|get|unset`,
  `packages/cli/src/commands/commands.ts:449-480`), and it generates its **own** password
  (`server-process.ts:78-82`: service mode uses `config.password || randomBytes(32)…`, ignoring the
  environment). OpenChamber never passes `--service`.

**Consequence for cola: the listening pid *is* the `opencode serve …` pid.** `dispatch()` in
`packages/server/src/process.ts:150-195` binds the HTTP server in-process. If OpenChamber had used
`--service`, cola's `args.iter().skip(1).any(|a| a == "serve")` + `--port` parse would still work but
the password would no longer be in the environment. It is in the environment. Good.

### 1.3 Host / port / password / auth, and is it cola-discoverable? — Yes, on all four counts

| Question | v2.0.2 answer | cola's requirement | Match |
|---|---|---|---|
| argv[0] contains `opencode`? | `lifecycle.js:395` resolves `binary` from `~/.opencode/bin/opencode` first (`env-runtime.js:402`), then PATH | `args.first().contains("opencode")` (`discovery.rs:118`) | ✅ |
| `serve` in argv? | yes, `lifecycle.js:396` | `args.iter().skip(1).any(\|a\| a == "serve")` | ✅ |
| `--port N` in argv? | yes, `lifecycle.js:396` | `position("--port")` + parse (`discovery.rs:121-124`) | ✅ |
| `OPENCODE_SERVER_PASSWORD` in child env? | yes, `lifecycle.js:778` | `strip_prefix("OPENCODE_SERVER_PASSWORD=")` (`discovery.rs:133-137`) | ✅ |
| `OPENCODE_SERVER_USERNAME`? | **not set** by OpenChamber; OpenCode 2 hardcodes the username to `"opencode"` (`packages/server/src/auth.ts:20`, `packages/server/src/process.ts:178`) | cola falls back to `DEFAULT_SERVER_USERNAME = "opencode"` (`discovery.rs:33`, `:70-77`) | ✅ |
| `XDG_DATA_HOME`? | never set by OpenChamber (`buildManagedChildEnv()` only ever returns `OPENCODE_CONFIG` or `OPENCODE_CONFIG_CONTENT`, `managed-config-file.js:98-110`) | unset ⇒ `uses_default_store = true` (`discovery.rs:138-146`) | ✅ |
| store really is the default one? | child inherits OpenChamber's env; OpenCode resolves `XDG_DATA_HOME \|\| ~/.local/share` (`packages/util/src/global-roots.ts:4-13`) and opens `<data>/opencode.db` (`packages/cli/src/database-path.ts:4-8`) | cola requires the default store (ADR-0013) | ✅ |

**Password env var name.** Note the ticket brief said `OPENCODE_PASSWORD`; the code actually reads
`OPENCODE_SERVER_PASSWORD` (`discovery.rs:133-137`). OpenCode 2 accepts **either**
(`packages/cli/src/env.ts:10-12`: `Config.redacted("OPENCODE_PASSWORD")` with
`Config.orElse(() => Config.redacted("OPENCODE_SERVER_PASSWORD"))`), and OpenChamber always writes
`OPENCODE_SERVER_PASSWORD`, so this is a non-issue — but worth recording that both names are live.

**New: the password rotates.** `ensureLocalOpenCodeServerPassword({ rotateManaged: true })`
(`lifecycle.js:733`) generates a fresh 32-byte password on *every* `startOpenCodeOnce()`, which also
runs on each of the 2 startup attempts and on every `restartOpenCode()`. Two escapes:
`getUserProvidedPassword()` wins and is used verbatim, never rotated
(`auth-state-runtime.js:62-65`), so `OPENCODE_SERVER_PASSWORD` exported in OpenChamber's environment
makes the password stable; otherwise it rotates.

This is the one behavioural change that touches cola's runtime. cola re-reads cmdline+environ on
every `scan_processes()` (`discovery.rs:88-92`, refresh kind `Always`), so the *reconcile loop* is
fine. The risk is a **cached** `ResolvedServer` inside cola's HTTP client: after an OpenChamber-managed
restart, every in-flight request 401s until cola re-resolves. OpenChamber v2.0.1 added a user-facing
"Restart OpenCode" button and a command-palette entry (v2.0.1 release notes, "Settings/OpenCode"), so
this is now routinely user-triggered rather than only startup-triggered.

### 1.4 OpenChamber does **not** steal an already-running server (docs are stale here)

`packages/docs/content/docs/opencode-server.mdx` at v2.0.2 claims the search order is
"reuse → external if configured → **auto-detect on the default port (4096)** → start our own".

The code says otherwise. `lifecycle.js:1159-1166`:

```
// We never auto-attach to an arbitrary pre-existing OpenCode instance.
// Attaching to an external server requires explicit opt-in via env
// (OPENCODE_HOST / OPENCODE_PORT / OPENCODE_SKIP_START), handled by the
// branches above. Without that opt-in we always start our OWN managed
// instance on a freshly-allocated port. A blind probe of the default
// port 4096 used to hijack a user's separately-running OpenCode (e.g.
// the OpenCode desktop app) …
```

`ENV_EFFECTIVE_PORT` is `null` unless `OPENCODE_HOST` / `OPENCODE_PORT` /
`OPENCHAMBER_OPENCODE_PORT` / `OPENCHAMBER_INTERNAL_PORT` is set (`env-config.js:27-70`), so a plain
`openchamber` always spawns its own. The `4096` fallback survives in exactly one place —
re-probing when OpenChamber is *already* in external mode and someone hits restart
(`lifecycle.js:877-878`). The same code was already in v1.19.0 (`lifecycle.js:905-908`), so the doc
page is stale in both generations, not a v2 regression.

**This is good news for cola.** cola's own `opencode serve` (when it self-starts) will *not* be
hijacked by OpenChamber, and OpenChamber's managed server will *not* collide with cola's on a port.
Both will exist concurrently on the default store, which is exactly today's steady state.

### 1.5 Binary install / pin / upgrade

* **Install target:** `~/.opencode/bin` — `v2-install.js:110`
  (`const directory = path.join(homeDirectory, '.opencode', 'bin')`), binary names
  `opencode` and `opencode2` on POSIX, `opencode.exe` on Windows (`v2-install.js:109`).
* **Version source:** `GET https://registry.npmjs.org/@opencode%2Fcli/latest`, schema-validated to
  `^2\.\d+\.\d+$` (`v2-install.js:12`, `:119-123`).
* **Mechanism:** downloads and runs the official `https://opencode.ai/v2/install` bash script with
  `--version <v> --no-modify-path` (`v2-install.js:64-69`, `:26-28`); on Windows it fetches the npm
  platform package (`@opencode/cli-windows-arm64`, or `@opencode/cli-windows-x64-baseline` on x64),
  verifies the published `sha512` integrity, and unpacks with the system `tar.exe`
  (`v2-install.js:22`, `:71-91`).
* **Route:** `POST /api/opencode/install-v2` (`opencode/routes.js:114`), guarded by
  `GET /api/opencode/compatibility` (`routes.js:108`) which reports `{ state, version, installation,
  minimumVersion, canInstall }` (`compatibility.js:83-88`).
* **Rollback safety:** filesystem lock `.opencode/bin/.openchamber-install`, per-binary backups
  restored on failure, 5-minute deadline, installer stdout discarded (it "may contain registry
  credentials") (`v2-install.js:104-146`; `opencode/DOCUMENTATION.md:244-262`).
* **Post-install upgrade:** `opencode upgrade` via `execFile`, no shell
  (`cli-upgrade.js:6-18`), exposed as `POST /api/opencode/upgrade` (`routes.js:133`).
* **Ownership rule:** upgrades only work for a **managed, non-bundled** binary. External servers,
  unresolved runtimes, and the desktop app's bundled binary all fail closed
  (`upgrade-capability.js:1-27`). The desktop app ships its own OpenCode; the npm/web build does not.

`~/.opencode/bin` does not exist on this machine yet, and `/usr/sbin/opencode` is 1.18.23, so nothing
has been installed by v2 code paths here.

---

## 2. V1 support and upgrade behaviour

### 2.1 OpenChamber v2 is V2-only

* `MINIMUM_OPENCODE_VERSION = '2.0.15'` (`compatibility.js:14`), and
  `isSupportedOpenCodeVersion` requires `releaseParts(version)[0] === 2` *and* `>= 2.0.15`
  (`compatibility.js:31-33`). A future major is explicitly "a contract OpenChamber has not met yet".
* v2.0.0 release notes, Misc: *"Requires OpenCode 2.0.15 or newer; the desktop app bundles it.
  Sessions from OpenCode 1.x carry over."*
* Blog post "Updating" section: *"OpenChamber 2.0.0 needs OpenCode 2.0.15 or newer. The desktop app
  bundles it. If you run your own OpenCode, the app notices a 1.x install on startup and offers to
  update it. Your sessions from OpenCode 1.x come along."*

### 2.2 What happens to an existing OpenCode 1.x server

`lifecycle.js:1144-1157` — if `OPENCODE_HOST`/`OPENCODE_PORT` points at a server that identifies as
v1 (or 2.x below the minimum), OpenChamber **attaches to it as external, not ready**, and throws
`UnsupportedOpenCodeVersionError`, rather than spawning a managed instance:

```
// The configured server is an OpenCode this OpenChamber cannot use.
// Attach to it anyway, not ready, so the compatibility check reports
// its version instead of a managed instance silently replacing it.
```

`readExternalOpenCodeVersion` (`compatibility.js:69-77`) identifies the generation by probing
`/api/info` first and falling back to v1's `/global/health`; its comment explains that v1 has no
`/api/info` because the catch-all serves the web UI. **Verified live on this machine**, where
OpenCode 1.18.23 is running on port 44475:

```
GET /api/info      (no auth) → 401
GET /api/info      (auth)   → the SPA HTML (v1 catch-all), not JSON
GET /global/health (auth)   → {"healthy":true,"version":"1.18.23"}
```

So on this box, OpenChamber v2.0.2 would classify the running server as v1 and show the update
screen (v2.0.1 release notes, Fixes: *"Startup: connecting to an OpenCode 1.x server shows a clear
'update OpenCode to 2.x' screen"*; v2.0.0 Fixes: *"Startup: with OpenCode 1.x installed, the app
shows a screen to update to OpenCode 2, with one-click install on macOS, Linux and Windows"*;
UI strings `opencodeCompatibility.outdatedTitle` = "Update OpenCode",
`settings.openchamber.about.openCode.manualUpdate` in `packages/ui/src/lib/i18n/messages/en.ts:14-17`
and `en.settings.ts:549-554`).

### 2.3 What the one-click update does to the install

It installs v2 into `~/.opencode/bin` and leaves "existing npm/Bun packages … installed"
(`opencode/DOCUMENTATION.md:255-262`). So after the upgrade there can be **two OpenCode binaries on
one machine**: a V1 one on PATH (e.g. `/usr/sbin/opencode`, npm/bun-installed) and the V2 one at
`~/.opencode/bin/opencode`.

Binary resolution order in OpenChamber v2 puts `~/.opencode/bin/opencode` ahead of PATH
(`env-runtime.js:382-443`, with the bundled-app candidates checked even earlier at
`env-runtime.js:309-325`), and the user can override it with the `opencodeBinary` setting
(`opencode/DOCUMENTATION.md:262-264`). Practical consequence for this box: after the upgrade,
OpenChamber would run v2 while a bare `opencode` in the shell still resolves to 1.18.23 — and if cola
ever self-started a server, it would also use whatever `opencode` is on cola's PATH. That asymmetry
is a real (small) footgun worth a cola-side note.

### 2.4 OpenChamber's own "settings apply live" claim is about files, not env

The v2.0.0 notes and the blog say settings apply "the moment you save them … with no OpenCode
restart". The mechanism is OpenCode 2's file watching plus an OpenChamber-managed config **file**:
`OPENCODE_CONFIG=<data-dir>/opencode.managed.json` is rewritten (temp + rename) whenever a
managed-plugin toggle changes, and OpenCode reloads within a couple of seconds
(`opencode/DOCUMENTATION.md:920-945`). But `PATH` and `OPENCODE_SERVER_PASSWORD` are explicitly
**lifecycle-owned** and cannot be replaced by injected values (`opencode/DOCUMENTATION.md:321`), and
the documented remaining restart triggers are: env vars, switching the OpenCode binary, an update, or
a crash (blog, "Everything reloads while you work"). This is why the password still rotates on a
managed restart even though config does not.

---

## 3. Store / data implications

### 3.1 The store path is unchanged

* OpenChamber's own reader hardcodes `OPENCODE_DATA_DIR = ~/.local/share/opencode`
  (`auth.js:25`) and reads `<data>/opencode.db` (`credential-db.js:23`, `:70`).
* OpenCode 2 resolves `XDG_DATA_HOME || ~/.local/share` (`packages/util/src/global-roots.ts:4-13`)
  and the db file is `opencode.db` unless `OPENCODE_DB` overrides it or the channel is non-release
  (`packages/cli/src/database-path.ts:4-8`).
* OpenCode 1.18.32 used the same `join(Global.Path.data, "opencode.db")`
  (`packages/core/src/database/database.ts:53`).

Live confirmation on this machine: `~/.local/share/opencode/opencode.db`, 8.4 GB, WAL mode
(`opencode.db-shm` / `opencode.db-wal` present), tables include `session`, `session_message`,
`message`, `part`, `event`, `event_sequence`, `credential`, `permission`, `migration`,
`data_migration`, `__drizzle_migrations`. **No `session_v2` and no `kv` table yet** — i.e. no OpenCode
2 has ever run against this store, so the V1→V2 migration has not run.

### 3.2 What the first OpenCode 2 start does to the store

`V1Migration.run()` (`packages/core/src/database/v1-migration.bun.ts:535-698`, wired in
`packages/server/src/routes.ts`) runs at startup when legacy sessions exist:

1. If there is **no** `migration.v1-v2` row in `kv`, it **deletes the entire `event` table** in one
   transaction and writes `{phase:"sessions"}` (`:547-565`).
2. Imports from `<data>/opencode-next.db` if present (`nextPath`, `:702-706`) into `session_v2` /
   `session_message` / `event_sequence` (`:611`, `:641`, `:664-671`).
3. Then walks the legacy `session` table by id cursor (`SELECT id, project_id FROM session WHERE id <
   cursor ORDER BY id DESC LIMIT 1`, `:587-590`). For each session it **deletes that session's
   `session_message` rows** (`:641`) and re-inserts the V1-transformed messages, **updates the
   `session` row** (agent/model/cost/tokens/revert/time fields, `:642-663`), and upserts
   `event_sequence` (`:664-671`).
4. Writes `{phase:"completed"}` (`:678-692`).

**Destructive rule that OpenChamber documents as a hard "never":** deleting or clearing the
`migration.v1-v2` row makes OpenCode treat the DB as pre-migration and DELETE the whole `event` table
— v2's durable event log — before starting over (`v1-migration-topup.js:16-20`).

So: **sessions and messages carry over** (release notes: "Sessions from OpenCode 1.x carry over"),
but the migration is one-way, mutates rows in place, and consumes the `event` log.

### 3.3 The "V1 sessions created after the migration finished" problem — and OpenChamber's fix

OpenCode's V1→V2 import runs **once**, then writes `{phase:"completed"}`. Anyone who kept running
OpenCode 1.x beside a v2 install afterwards created V1 sessions that v2 will never look at again —
"they are simply invisible in v2" (`v1-migration-topup.js:5-11`).

OpenChamber re-arms the import before spawning a *managed* OpenCode (`lifecycle.js:749-758` calling
`topUpV1Migration()`), by writing a resume cursor into the same `kv` row
(`v1-migration-topup.js:270-290`). Its guards, verified against v2.0.8's
`packages/core/src/database/v1-migration.bun.ts`:

* never clear the row (see above);
* never schedule a cursor that revisits a session which has seen v2 activity — every visited session
  gets its `session_message` rows deleted and replaced, so a migrated-then-continued session would
  lose that conversation (`:22-28`, `:150-163`);
* only re-import when a legacy session's `time_updated` is newer than the completion stamp, because
  "a v2 delete removes the `session_v2` row but leaves the legacy `session` row behind, so 'missing
  from v2' alone would bring deleted sessions back on every start" (`:29-37`).

If the guards fail, the top-up returns `unsafe` and logs a warning instead of running
(`:277-286`). It also only ever runs for a **managed** OpenCode, before spawn — "writing the
migration state under a running OpenCode would race its own loop, and an external OpenCode is not
OpenChamber's to steer" (`:200-205`).

### 3.4 Can V1 and V2 run concurrently on one store?

**Technically yes; practically no.**

* Technically: SQLite in WAL mode tolerates multiple processes on one file, and the store is already
  in WAL mode here. v2's migration guard is `Semaphore.makeUnsafe(1)` — an **in-process** lock
  (`v1-migration.bun.ts:214`, `:535`). There is no cross-process lock on the migration, so two v2
  servers on one store would race the cursor.
* Practically: it is a documented data-loss shape, not a supported configuration. Specifically —
  a V1 server keeps writing legacy `session`/`message`/`part` rows that v2 will not see; v2's
  migration *updates* the shared `session` rows and *deletes/reinserts* `session_message`; and v2's
  first run deletes the whole `event` table, which v1.18 also uses
  (`v1.18.32` `packages/opencode/src/control-plane/workspace.ts`, `…/httpapi/handlers/sync.ts`,
  `…/shared/fence.ts`). OpenChamber never runs two OpenCodes on one store, and its top-up module
  exists precisely because mixing the generations corrupts session visibility.

Note this is already cola's problem today, not a new one: cola deliberately attaches to a
default-store server and only self-starts when none exists (ADR-0013, `discovery.rs:156-183`), so on a
normal box there is exactly one server. The V1/V2 question only becomes live if someone leaves a V1
`opencode serve` running after the upgrade.

### 3.5 What else moved into the store

* Provider credentials: v2 imports legacy `auth.json` **once** and then owns them in
  `<data>/opencode.db` table `credential`; `auth.json` is never written again and there is no HTTP
  route that returns a key (`auth.js:1-19`, `credential-db.js:1-20`,
  `opencode/DOCUMENTATION.md:97-119`, `:897`). Anything that read `auth.json` to get a raw key must
  now read the DB.
* OpenChamber's own per-session records (goal progress, recap, notes) moved from
  `sessions-metadata.json` to `PATCH /api/session/{id}` (`opencode/DOCUMENTATION.md:899`),
  which is one of the reasons 2.0.15 is the floor.
* **cola-relevant:** OpenCode 2 reads project instructions from `AGENTS.md` and **no longer loads
  `CLAUDE.md`** (v2.0.0 release notes, Misc; blog "Updating" section). Cola already relies on
  `AGENTS.md` (`AGENTS.md` header), so this is neutral-to-good for cola's own instructions — but it
  is a user-visible behaviour change for any OpenChamber user whose project rules live in
  `CLAUDE.md`.

---

## 4. Verdict on cola's current attach strategy, with alternatives

### 4.1 Discovery: survives. Verified line by line.

cola's three discovery inputs — cmdline, `--port` argv, `OPENCODE_SERVER_PASSWORD` in the process
environment — are all still present, produced by the same code shape, by the same child process.
`uses_default_store` also still evaluates correctly because OpenChamber never sets `XDG_DATA_HOME`.

Two things to plan for, both cheap:

1. **Username.** OpenCode 2 dropped `OPENCODE_SERVER_USERNAME` entirely: `ServerAuth.Config.configLayer`
   hardcodes `username: "opencode"` (`packages/server/src/auth.ts:20`) and `authorized()` still
   compares it (`auth.ts:34`); `dispatch()` constructs it the same way
   (`packages/server/src/process.ts:178`). So cola's `username_from_env` (`discovery.rs:70-77`) is
   right by accident. Worth **hardcoding `"opencode"` for the v2 generation** rather than trusting the
   env, so a stale/inherited `OPENCODE_SERVER_USERNAME` in the *OpenChamber* process env can never be
   read off the child and sent.
2. **Password rotation.** See §1.3. Either (a) export `OPENCODE_SERVER_PASSWORD` in OpenChamber's
   environment so it is used verbatim and never rotates
   (`auth-state-runtime.js:61-66`), or (b) treat any 401 from the resolved server as a
   re-resolve trigger and re-run `scan_processes()`. (a) is strictly better; it also makes the
   password inspectable by a human without reading `/proc`.

### 4.2 The real blocker: the protocol surface moved under cola

cola's client (`src/opencode/client.rs`) uses:

| cola call | line | OpenCode 2.0.18 |
|---|---|---|
| `PATCH /session/{id}` | `:278` | ✗ not mounted; v2 metadata is `PATCH /api/session/{id}` |
| `POST /session/{id}/message` | `:339` | → `POST /api/session/:sessionID/message` (exists, `protocol/src/groups/message.ts`) |
| `POST /session/{id}/prompt_async` | `:446` | ✗ gone; → `POST /api/session/:id/prompt` (`protocol/src/groups/session.ts`) |
| `GET /session/{id}/message` | `:548` | → `GET /api/session/:id/message/:messageID` |
| `POST /permission/{id}/reply` `{reply}` | `:495` | → `POST /api/session/:sessionID/permission/:requestID/reply` with body `{decision, message}` (`protocol/src/groups/permission.ts:117-130`) |
| pending-permission list | — | `GET /api/permission/request` (`permission.ts:23`); the global unprefixed `GET /permission` from ADR-0007/0026 is gone |
| `POST /question/{id}/reply`, `/reject` | `:675`, `:732` | → `/api/session/:id/form/:formID/reply` (`protocol/src/groups/form.ts`) |
| `POST /session/{id}/abort` | `:746` | → `POST /api/session/:id/interrupt` |
| `POST /api/session/{id}/compact` | `:756` | ✅ unchanged |
| global SSE | pollers | `/global/event` → `GET /api/event` (only path in `protocol/src/groups/event.ts`); OpenChamber's proxy aliases `/api/global/event` → `/api/event` for old clients (`proxy.js:977-986`) |

I found **no** unprefixed compatibility mount in v2.0.18: `git grep '"/session'` over
`packages/server/src` and `packages/protocol/src` at v2.0.18 returns nothing, and every path in the
protocol groups is `/api/…` or `/api/experimental/…`.

This is a separate, larger ticket than R4 — but R4's answer is only useful if the orchestrator knows
this gate exists.

### 4.3 Should cola talk to OpenChamber instead of OpenCode? — No.

Measured against the criteria:

* **Not a transparent proxy.** `GET /api/session` (`proxy.js:933`) and `GET /api/session/:id`
  (`proxy.js:939`) are intercepted, "sanitized to an allowlist of `SessionInfo` fields", and have
  archive state folded in from OpenChamber's own store; unknown answers leave the upstream record
  untouched (`opencode/DOCUMENTATION.md:867`). cola's session/metadata reads would silently lose
  fields. Generic `/api/*` and `/api/event` do pass through, but with a readiness gate
  (`proxy.js:817-905`) and a worktree checkout gate that returns 503.
* **Different credentials.** OpenChamber's `/api/*` is gated by the *UI* password / pairing client
  token, not by OpenCode's server password (`lib/ui-auth/ui-auth.js:580-600`: with no UI password
  `requireAuth` is a pass-through; with one, `Client authentication required`). Verified live:
  `GET http://127.0.0.1:3000/api/opencode/health` → `200 {"healthy":true}` with no credentials — but
  that is only because this install has no UI password. A pairing token is revocable and rotates
  independently.
* **Not a stable contract.** Everything above lives under `lib/opencode/` and
  `lib/openchamber-sessions/`, which the module's own `DOCUMENTATION.md` describes as internal
  runtimes wired by `index.js`. It changed wholesale between v1.19 and v2.0 (e.g. the whole
  `session-metadata` store moved from a file to `PATCH /api/session/{id}`).
* **Couples cola to OpenChamber's release cadence** for zero benefit: cola needs nothing OpenChamber
  adds. The two genuinely OpenChamber-only surfaces a bot could use are
  `POST /api/notifications/emit` (`lib/notifications/emit-route.js:3`, rate-limited 10/10 s) and the
  `PUT /api/message-queue/sessions/:id/hold` route
  (`lib/message-queue/DOCUMENTATION.md` "Routes" table) — neither replaces the OpenCode API.

**Conclusion: keep cola on the direct OpenCode connection.** Its current design — discover the
process, read the port and password, speak OpenCode's own HTTP API — is exactly the right shape, and
it is the shape OpenChamber v2 keeps.

### 4.4 Alternatives, ranked

1. **Keep `/proc` (sysinfo) discovery, add generation detection.** Cheapest and sufficient. Probe
   `GET /api/info` (v2) and `GET /global/health` (v1) exactly the way `compatibility.js:69-77` does,
   and route on the answer. Bonus: `/api/info` returns `{ version, pid, urls, paths }`
   (`packages/server/src/process.ts:186-206`) — **`pid` is the server's own pid**, so cola can
   cross-check its `/proc` guess instead of trusting argv alone. That is a strictly better
   identity check than anything cola does today.
2. **Pin the password.** Export `OPENCODE_SERVER_PASSWORD` into OpenChamber's environment
   (`auth-state-runtime.js:61-66` uses it verbatim, never rotates). Removes the whole
   rotation-after-restart failure class, and makes the setup debuggable.
3. **Reduce reliance on the child env.** If OpenChamber ever moves to `--service`
   (`server-process.ts:78-82` generates the password out-of-band), the env read dies. A belt-and-
   braces fallback: also read OpenChamber's own `/api/opencode/version` and
   `/api/system/info` (`opencode/routes.js:225`, `:328`) for the port, or adopt the pid from
   `/api/info`.
4. **Don't.** Do not route through the OpenChamber proxy (§4.3), and do not give cola its own
   long-lived data directory — that is precisely the mistake that broke session sharing before
   (`discovery.rs:1-7`, pitfall 7 in cola's `AGENTS.md`), and it would additionally split the
   V1→V2 migration, which only ever runs for the store OpenCode actually opens.

---

## 5. Open questions

1. **Which OpenCode does cola self-start with?** After the one-click upgrade, `~/.opencode/bin/opencode`
   is v2 but `/usr/sbin/opencode` (first on PATH) is 1.18.23. cola's `self_start_command`
   (`discovery.rs:196-217`) runs bare `opencode serve`. Should cola resolve the same binary
   OpenChamber resolves (`~/.opencode/bin/opencode` first, `env-runtime.js:402`), or should the
   upgrade instructions require fixing PATH? Unresolved — this is a decision ticket, not research.
2. **Is `/api/info`'s `pid` reliable enough to be cola's primary identity?** It is `process.pid`
   (`process.ts:189`). Worth confirming on a real v2 server that it is the same pid as the
   `opencode serve` argv cola scans, including under OpenChamber's `detached: true` spawn.
3. **The `event` table deletion.** v2's first migration wipes `event` wholesale when no
   `migration.v1-v2` row exists. Does any cola data live there? cola reads sessions/messages only, so
   probably not — but the 8.4 GB store here makes a first-run migration a long, disk-heavy operation
   worth timing and backing up before recommending the upgrade to a user.
4. **Two-generation support: does cola want it at all, or a hard cutover?** OpenChamber v2 is
   V2-only, so the "auto-detect existing server" story is really "auto-detect existing server, then
   fail every call". A generation probe that produces a clear cola-side message beats a silent 404
   storm.
5. **OpenChamber ↔ cola interaction on a V1 store.** If the user runs the one-click upgrade while
   cola is mid-turn, the V1→V2 migration deletes/reinserts `session_message` and updates `session`
   rows for the very session cola is polling. Untested; the `v1-migration-topup.js` rules suggest the
   safe order is: stop cola's turns → stop the V1 server → let OpenChamber install v2 → let it
   migrate → re-attach.
6. **Does `PATCH /api/session/{id}` replace the whole object?** Yes —
   "`OpenCode replaces the whole object`, so `sessionMetadataStore.setSessionMetadata` reads the
   record, applies the JSON Merge Patch and writes the result" (`opencode/DOCUMENTATION.md:899`).
   cola's `PATCH /session/{id}` (`client.rs:278`) will need read-merge-write semantics, not a blind
   patch. Needs verification against the v2 handler, not just the docs.
7. **Unverified by me:** I did not run an OpenCode 2 server. Every v2 claim above is from source at
   tag `v2.0.18` (the newest local tag; the OpenChamber floor is 2.0.15, so v2.0.18 is a valid
   upper reference) and from OpenChamber source at tag `v2.0.2`. The live HTTP checks I ran were
   against the **V1** 1.18.23 server on this machine, used to confirm the V1 probe behaviour that
   `compatibility.js` relies on. A smoke test against a real 2.0.18 `serve` should close this.

---

## Sources

* OpenChamber, tag `v2.0.2` (read via `git show v2.0.2:<path>` in
  `/root/workspace/dev/openchamber`; the working checkout is older, ~v1.19.0):
  * `packages/web/server/lib/opencode/lifecycle.js` (spawn `:396`, `:432`, `:560-592`, `:733`,
    `:749-758`, `:770-780`, `:877-878`, `:1004`, `:1144-1157`, `:1159-1166`)
  * `packages/web/server/lib/opencode/compatibility.js` (`:14`, `:31-33`, `:69-77`, `:83-88`)
  * `packages/web/server/lib/opencode/v2-install.js` (`:22`, `:26-28`, `:64-69`, `:71-91`, `:104-146`)
  * `packages/web/server/lib/opencode/cli-upgrade.js`, `upgrade-capability.js`
  * `packages/web/server/lib/opencode/auth-state-runtime.js` (`:42`, `:54`, `:61-83`)
  * `packages/web/server/lib/opencode/auth.js` (`:25`), `credential-db.js` (`:23`, `:70`)
  * `packages/web/server/lib/opencode/v1-migration-topup.js` (`:5-11`, `:16-20`, `:22-28`, `:29-37`,
    `:150-163`, `:200-205`, `:270-290`)
  * `packages/web/server/lib/opencode/proxy.js` (`:817-905`, `:933`, `:939`, `:977-986`)
  * `packages/web/server/lib/opencode/env-config.js` (`:27-70`, `:84-90`), `env-runtime.js`
    (`:309-325`, `:382-443`, `:402`)
  * `packages/web/server/lib/opencode/managed-config-file.js` (`:98-110`), `routes.js` (`:108`,
    `:114`, `:133`, `:202`, `:225`)
  * `packages/web/server/lib/opencode/DOCUMENTATION.md` (`:244-262`, `:321`, `:867`, `:897`,
    `:899`, `:920-945`)
  * `packages/web/server/lib/ui-auth/ui-auth.js` (`:580-600`)
  * `packages/web/server/lib/message-queue/DOCUMENTATION.md`,
    `packages/web/server/lib/notifications/emit-route.js` (`:3`)
  * `packages/docs/content/docs/opencode-server.mdx` (the stale 4096 claim),
    `packages/docs/content/docs/remote-instances.mdx`, `packages/docs/content/docs/sdk.mdx`
  * `packages/ui/src/lib/i18n/messages/en.ts:14-17`, `en.settings.ts:549-554`
* OpenChamber release notes: `gh release view v2.0.0|v2.0.1|v2.0.2 --repo openchamber/openchamber`
  (2026-09-23 / later), quoted above.
* OpenChamber blog: <https://openchamber.dev/blog/opencode-v2/> — "OpenChamber 2.0: it's getting hot
  reload in here", Bohdan Triapitsyn, 2026-09-23 ("Everything reloads while you work", "Updating").
* OpenCode, tags `v2.0.18` and `v1.18.32` in `/root/workspace/dev/opencode`:
  * `packages/cli/src/server-process.ts` (`:60-155`, `:78-82`), `packages/cli/src/env.ts` (`:10-12`),
    `packages/cli/src/database-path.ts` (`:4-8`), `packages/cli/src/commands/handlers/serve.ts`
  * `packages/server/src/process.ts` (`:150-195`, `:178`, `:186-206`), `packages/server/src/auth.ts`
    (`:20`, `:34`)
  * `packages/protocol/src/groups/{session,message,permission,form,event}.ts` (path inventories)
  * `packages/core/src/database/v1-migration.bun.ts` (`:214`, `:535-698`), `packages/util/src/global-roots.ts`
  * `v1.18.32`: `packages/core/src/database/database.ts:53`
* This machine (observed 2026-09-26): running `opencode serve --hostname 127.0.0.1 --port 44475`
  (pid 1224378) under OpenChamber 1.21.0 (`/usr/lib/node_modules/@openchamber/web`, port 3000);
  `opencode` 1.18.23 at `/usr/sbin/opencode`; store `~/.local/share/opencode` (8.4 GB `opencode.db`,
  WAL, no `session_v2`/`kv`); live probes of `/api/info`, `/global/health`, `/api/session`,
  `/api/config` on 44475 with and without Basic auth; `GET /api/opencode/health` on 3000.
