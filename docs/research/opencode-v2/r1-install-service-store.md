# R1 — OpenCode V2: install, service model, and data store

**Ticket:** #354 on wayfinder map #353 · **Date:** 2026-09-26
**Question:** How is OpenCode V2 ("OpenCode 2") officially installed, started, and where does it
keep its data — and can it coexist with the V1 install on this machine?

**Short answer:** V2 is real and officially published, but as a **different npm package**
(`@opencode/cli`, not `opencode-ai`) with **no GitHub Releases and no user-facing docs**. It ships
**two bin names for the same binary** (`opencode` and `opencode2`). It replaces V1's
`opencode serve` with a **managed background daemon discovered through a state-dir registration
file** on port **49374** with HTTP Basic auth. And it defaults to the **exact same data, state and
config directories as V1**, including the same `opencode.db` — starting it applies **10 schema
migrations in place** to V1's database. It can coexist, but only with deliberate XDG isolation.

---

## 1. Distribution channels — official vs third-party

### 1.1 Official V2 = npm `@opencode/cli`

The project's own update service names the package explicitly:

```
$ curl -fsSL 'https://opencode.ai/update/api/latest/cli/npm?current=0.0.0'
{"channel":"latest","name":"cli","distribution":"npm","version":"2.0.18",
 "metadata":{"package":"@opencode/cli",
             "github":{"sha":"041885d838c7eea81cffbe446e0cb0ac6f197728",
                       "ref":"refs/heads/v2"}},
 "active":true,"minimum":false,...}
```

So `v2.0.18` **is** the npm `latest` of `@opencode/cli`, published from the `v2` branch.

```
$ npm view @opencode/cli dist-tags
{ beta: '0.0.0-beta-19507', dev: '0.0.0-dev-20199', latest: '2.0.18', reserved: '0.0.0-reserved' }

$ npm view @opencode/cli@2.0.18 bin optionalDependencies --json
bin: { "opencode": "bin/opencode.exe", "opencode2": "bin/opencode.exe" }
optionalDependencies:
  @opencode/cli-linux-x64@2.0.18           (dist.unpackedSize 203,146,956)
  @opencode/cli-linux-x64-musl@2.0.18
  @opencode/cli-linux-x64-baseline@2.0.18
  @opencode/cli-linux-x64-baseline-musl@2.0.18
  @opencode/cli-linux-arm64@2.0.18  (+ -musl)
  @opencode/cli-darwin-x64@2.0.18  (+ -baseline)
  @opencode/cli-darwin-arm64@2.0.18
  @opencode/cli-windows-x64@2.0.18  (+ -baseline)
  @opencode/cli-windows-arm64@2.0.18
scripts: { postinstall: "node ./postinstall.mjs" }
```

Matches `packages/cli/package.json` @v2.0.18 (`"name": "@opencode/cli"`, `"version": "2.0.18"`,
`bin: {opencode, opencode2}`) and `packages/cli/script/publish.ts` @v2.0.18
(`publishDistribution({ name: pkg.name, command: "opencode", legacyCommand: "opencode2",
binary: "opencode", packagePrefix: "@opencode/cli-", artifact: "cli" })`).

`@opencode/cli` is a **metapackage**: its published `bin/opencode.exe` is a placeholder that errors
if `postinstall` never ran; `packages/cli/script/postinstall.mjs` @v2.0.18 then hardlinks (or
copies) the real native binary from `node_modules/@opencode/cli-<platform>-<arch>/bin/opencode`
into place and runs `binary --version` to verify.

### 1.2 Official V2 = curl installer at `https://opencode.ai/v2/install`

The endpoint is live and is **byte-identical** to the `install` blob committed at tag `v2.0.18`:

```
$ curl -fsSL -o v2install.sh https://opencode.ai/v2/install     # HTTP 200
$ sha256sum v2install.sh
bc70dd317fd12ef09350cdb1667e55e19f9e7b3d2897d3a1b750b165ae579d87  v2install.sh
$ git -C /root/workspace/dev/opencode show v2.0.18:install | sha256sum
bc70dd317fd12ef09350cdb1667e55e19f9e7b3d2897d3a1b750b165ae579d87  -
```

What it does (`install` @v2.0.18, lines ~168-215): detects OS/arch/musl/AVX2 → builds
`target=linux-x64[-baseline][-musl]`; resolves the version by fetching
`https://opencode.ai/update/api/latest/cli/npm` and reading `"version"` + `"package"`; sets
`package_scope="${package%/cli}"` → **`@opencode`**; downloads
`https://registry.npmjs.org/@opencode/cli-$target/-/cli-$target-$version.tgz`; extracts the binary
to **`$HOME/.opencode/bin/opencode`** (`INSTALL_DIR=$HOME/.opencode/bin`).
It falls back to the legacy `@opencode-ai/cli-$target` scope for older pinned versions.

So the "V2 installer" is a thin npm-tarball fetcher — the binary always comes from npm.

### 1.3 V2 has **no GitHub Releases**

```
$ gh release list --repo anomalyco/opencode --limit 15
v1.18.32  Latest  v1.18.32  2026-09-21T22:51:20Z
v1.18.31           v1.18.31  2026-09-14T17:47:30Z
... (v1.18.18 … v1.18.31)
$ gh release view v2.0.18 --repo anomalyco/opencode
release not found
```

Every release row is V1. V2.0.x exists only as git tags + npm + the update service. Note the V1
series kept shipping through 2026-09-21, i.e. **V1 is still the released product**; V2 ships
alongside it.

### 1.4 Other official V2 channels (from `packages/cli/script/publish.ts` + `.github/workflows/publish.yml` @v2.0.18)

- AUR (`script/publish-aur.ts`) and Homebrew tap (`script/publish-homebrew.ts`), when
  `channel ∈ {beta, latest}` and a release is cut.
- Docker `ghcr.io/anomalyco/opencode:<version>` when `channel === "latest" && release`.
- `@opencode/cli-node` on **non-latest** channels, bin `opencode2-node` (a Node-hosted build).
- The `publish` workflow triggers on branches `ci, dev, beta, v2, fix/npm-native-binary-install,
  snapshot-*`, and `version.ts` only sets `release=true` when `Script.preview || channel === "beta"`
  (and the legacy V1 CLI is only built when `ref_name != 'v2' && != 'beta'`).

### 1.5 Official V1 = npm `opencode-ai` + `https://opencode.ai/install`

```
$ npm view opencode-ai version bin dist-tags
version = '1.18.32'
bin = { opencode: 'bin/opencode.exe' }
dist-tags = { …, dev: '0.0.0-dev-202609252310', latest: '1.18.32', next: '0.0.0-next-202606270058', … }
```

Note `opencode-ai`'s `dev` dist-tag was updated **2026-09-25T23:34Z** — V1 is actively developed.
V1's `packages/opencode/package.json` @v1.18.32 is `"private": true`; the published name is
`opencode-ai` (that is also what `publish.yml` installs in CI: `bun i -g opencode-ai`).

**V2 is not documented for users.** `https://opencode.ai/docs/` (fetched 2026-09-26) still says:

```
curl -fsSL https://opencode.ai/install | bash
npm install -g opencode-ai   /   bun install -g opencode-ai   /   pnpm …   /   yarn …
```

and the `README.md` at tag `v2.0.18` is likewise still the V1 install section
(`npm i -g opencode-ai@latest`, `brew install anomalyco/tap/opencode`, …). The V2 install path is
discoverable only from the source tree (`install`, `packages/cli/script/publish.ts`) or from the
update service's JSON.

### 1.6 The third-party `opencode2` npm package — **not official**

```
$ npm view opencode2
opencode2@2.0.16 | MIT | deps: none | versions: 1
OpenCode v2 CLI with prompt modes, code-review-graph auto-indexing, and the browser skill
https://github.com/game-libgdx-unity/opencode2#readme
bin: opencode2
dist.unpackedSize: 4.6 kB
maintainers: - thanhvinh1 <mrthanhvinh168@gmail.com>
published 15 hours ago by thanhvinh1 <mrthanhvinh168@gmail.com>

$ gh repo view game-libgdx-unity/opencode2
GraphQL: Could not resolve to a Repository with the name 'game-libgdx-unity/opencode2'. (repository)
```

The tarball is 4 files: a 656-byte `package.json`, a `README.md`, `bin/opencode2.cjs`, and
`bin/opencode2` — the last only via the platform package:

```
$ npm view opencode2-linux-x64
opencode2-linux-x64@2.0.16 | versions: 1 | dist.unpackedSize: 203.0 MB
$ tar tzvf opencode2-linux-x64-2.0.16.tgz
-rwxr-xr-x 0/0  202986976  package/bin/opencode2
$ file package/bin/opencode2
ELF 64-bit LSB executable, x86-64, dynamically linked, … not stripped
$ ./package/bin/opencode2 --version
opencode v2.0.16
```

**Assessment.** It is a **derivative repackage**, not a squatter-with-no-binary: it ships a real
~203 MB OpenCode **v2.0.16** build (2.0.16 is a genuine upstream v2 tag) under a different package
name, wrapped in a hand-written launcher that is a near-copy of the official `bin/opencode.cjs`
logic. Its `bin/opencode2.cjs` resolves `opencode2-<platform>-<arch>` instead of
`@opencode/cli-<platform>-<arch>`, so the wrapper is the only thing that differs mechanically.

Reasons to treat it as **unofficial and untrusted**:
- Sole maintainer `thanhvinh1`, one version, published 15 h before this query.
- Its declared repository `game-libgdx-unity/opencode2` **does not exist on GitHub** (404), so
  there is no source, no tag, and nothing to audit or diff against upstream.
- Only `linux-x64` is published ("Other platforms are not published yet; build from source in the
  repository" — a repository that 404s).
- Its README advertises features that are **not in upstream v2.0.16**: "prompt modes
  (`answer`/`implement`/`test`/`deploy`)", "code-review-graph" with a 30-tool MCP server, and a
  "browser skill" materialized with a `bsk` helper binary. Upstream's `package.json` @v2.0.18 has
  no `code-review-graph` and no such skill set.

**Would a user "upgrading to OpenCode 2" ever get it? No.** Every official path names
`@opencode/cli`: the v2 curl installer derives the scope from the update service, and the CLI's own
`opencode upgrade` reads `metadata.package` from `https://opencode.ai/update/api/<channel>/<artifact>/<dist>`
(`packages/cli/src/services/updater.ts` @v2.0.18, `release()`) and runs
`npm install --global [--force] @opencode/cli@<version>`. The only way to install the squatter is to
type `npm i -g opencode2` by hand.

**Why the name is a genuine trap:** the *official* `@opencode/cli` metapackage also declares a bin
literally named `opencode2`. A user googling "opencode2" finds the third-party package first, and a
user who sees `opencode2` in the official bin map may reasonably assume `npm i -g opencode2` is the
supported install. It is not.

### 1.7 `opencode` and `opencode2` are the **same binary** (official)

`packages/cli/bin/opencode2.cjs` @v2.0.18 is three lines:

```js
#!/usr/bin/env node
require("./opencode.cjs")
```

and `bin/opencode.cjs` @v2.0.18 always sets `sourceCommand = "opencode"`, so both shims resolve and
exec the identical `node_modules/@opencode/cli-<platform>-<arch>/bin/opencode`. The CLI self-name is
a compile-time define — `packages/cli/script/build.ts` @v2.0.18 sets
`OPENCODE_CLI_NAME: "'opencode'"` for **every** target — and
`packages/cli/src/commands/commands.ts` @v2.0.18 uses it for the root command name.

**`opencode2` is a binary-name alias, not a second product, not a "v2 mode", and not a separate
install target.** `opencode2 --version` and `opencode --version` print the same thing from the same
file.

---

## 2. Install recipe (exact commands)

### 2.1 Recommended for this machine — non-global, pinned

```bash
sudo mkdir -p /opt/opencode-v2 && cd /opt/opencode-v2
sudo npm init -y
sudo npm install @opencode/cli@2.0.18

/opt/opencode-v2/node_modules/.bin/opencode  --version   # -> opencode v2.0.18
/opt/opencode-v2/node_modules/.bin/opencode2 --version   # same binary
```

`postinstall` resolves `@opencode/cli-linux-x64@2.0.18`, hardlinks its 203 MB binary into
`/opt/opencode-v2/node_modules/@opencode/cli/bin/opencode.exe`, and verifies it with
`binary --version`. Pinning `@2.0.18` avoids surprise upgrades.

### 2.2 Global install — **will take over `opencode` on this machine**

```bash
npm install -g @opencode/cli@2.0.18
# or
curl -fsSL https://opencode.ai/v2/install | bash     # -> ~/.opencode/bin/opencode
```

On this box `npm prefix -g` is `/usr`, so npm writes `/usr/bin/opencode`, which is a **hardlink
(shared inode 654919, 177,427,656 bytes, dated Aug 25) shared with `/usr/sbin/opencode`,
`/sbin/opencode` and `/bin/opencode`** — the running V1 1.18.23 install. `npm i -g @opencode/cli`
replaces it. `packages/cli/src/services/updater.ts` @v2.0.18 even adds `--force` for exactly this
case ("Keep the old package: uninstalling it can unlink the replacement command"), so the v2 CLI's
own `opencode upgrade` will hijack `/usr/bin/opencode` on a global install.

Also note: **both** the V1 and V2 curl installers target `$HOME/.opencode/bin/opencode`
(`INSTALL_DIR=$HOME/.opencode/bin` in both the live V1 script and the `install` blob @v2.0.18).

---

## 3. Startup / service model

### 3.1 Command surface (`packages/cli/src/commands/commands.ts` @v2.0.18)

| Command | Purpose |
| --- | --- |
| `opencode [directory] [prompt]` | TUI. **Default = connect to / auto-start the managed background service.** |
| `--standalone` | "Run with a private server instead of the background service" |
| `--server <url>` | "Connect to a server URL instead of the background service" |
| `opencode serve [--hostname H] [--port P] [--cors O]… [--service] [--stdio]` | "Start the v2 API and web server" |
| `opencode service start\|restart\|status\|stop\|get\|set\|unset` | "Manage the background server" |
| `opencode api <operation \| METHOD path> [-d body] [-H name:value] [--param k=v]` | request the running server |
| `opencode run <message…>` | headless, `--format json`, `--auto`/`--yolo`/`--dangerously-skip-permissions` |
| `opencode session list\|delete\|export\|import` | session management (`export --sanitize`, `import <file|url>`) |
| `opencode auth list\|login\|logout\|switch` | provider credentials |
| `opencode mcp …`, `opencode plugin …`, `opencode pair` | MCP / plugins / one-time browser+app links |
| `opencode models`, `opencode stats`, `opencode reload` | |
| `opencode debug paths [db\|home\|data\|config\|cache\|state\|tmp\|bin\|log\|repos]` | print global paths |
| `opencode debug config`, `opencode debug agents` | |
| `opencode upgrade [version] [--method curl\|npm\|pnpm\|bun\|yarn\|vp\|brew]` | self-upgrade |
| `opencode uninstall [--keep-config] [--keep-data] [--dry-run] [--force]` | |
| `opencode acp` | Agent Client Protocol server |

`--standalone` and `--server` are mutually exclusive. The root `package.json` @v2.0.18 exercises
exactly this in its dev loop:

```json
"dev:live": "sh -c 'OPENCODE_TUI_CHANNEL=dev OPENCODE_PASSWORD=\"$(opencode service get password)\" exec bun run dev \"$@\" --server \"$(opencode service status)\"' --"
```

### 3.2 Three `serve` modes (`packages/cli/src/server-process.ts` @v2.0.18)

**`default`** — `opencode serve` in the foreground.
- hostname: `--hostname` ?? service-config `hostname` ?? **`127.0.0.1`**
- port: `--port` ?? service-config `port` ?? **undefined → OS-assigned ephemeral**
- prints `server listening on <url>` and, if no `OPENCODE_PASSWORD` in the environment,
  `server password <password>`; then never returns.

**`service`** — `opencode serve --service`, the managed background daemon.
- `process.chdir(global.home)` at startup.
- port: `--port` ?? service-config `port` ?? `ServiceConfig.defaultPort()`, which is
  (`packages/cli/src/services/service-config.ts` @v2.0.18):
  - `0xc0de` = **49374** for channels `latest | dev | beta | next`
  - `0xc0df` = **49375** for channel `local`
  - otherwise `10000 + (hash(channel) % 50000)`
- password: persisted service-config password, else a fresh `randomBytes(32).toString("base64url")`
- on listen: persist the password if absent, then write the registration file.

**`stdio`** — `opencode serve --stdio`, the child-server mode behind `--standalone`.
- `Standalone.start()` (`packages/cli/src/services/standalone.ts` @v2.0.18) spawns
  `[...selfCommand(), "serve", "--stdio", "--port", "0"]` with a lease password injected as
  `OPENCODE_PASSWORD` (explicitly overriding any inherited value), `stdin: "pipe"`, `SIGTERM`,
  `forceKillAfter: "3 seconds"`. The server prints `{"url":…}` on stdout and **shuts down on stdin
  EOF** — the OS closing the pipe is what ends the lease. In `stdio` mode the server also
  **deletes `OPENCODE_PASSWORD`/`OPENCODE_SERVER_PASSWORD` from its own environment** so tools
  spawned by the agent never inherit the lease credential.

### 3.3 Discovery contract — the registration file

`packages/client/src/effect/service.ts` @v2.0.18 states the design in its own header comment:
*"The service daemon advertises itself through a registration file in the user's state directory:
url, pid, version, and the private password, with 0600 permissions. That file is the complete
discovery contract — reading it is all a client needs to connect."*

- **Path**: `$XDG_STATE_HOME/opencode/service.json` — i.e. `~/.local/state/opencode/service.json`
  by default. `ServiceConfig.filename()` uses `service.json` for channels
  `latest|dev|beta|next` and `service-<channel>.json` otherwise; legacy `service-<hash>.json`
  files are migrated on read. `Service.fallback()` computes the same path from
  `XDG_STATE_HOME ?? ~/.local/state`.
- **Contents** (`ServiceRegistration.register`, `packages/cli/src/services/service-registration.ts`):
  `{ id, version, url, pid, password }`, written to a temp file then `rename`, **mode 0600**.
- **Health probe**: `GET {url}/api/info` with HTTP **Basic** `opencode:<password>`. The username is
  **hardcoded `"opencode"`** in `probeResult` — there is no `OPENCODE_SERVER_USERNAME` equivalent
  in v2 (unlike v1).
- **State machine**: `ready` (200) / `waiting` (non-200, non-500) / `failed` (500). A **404 on
  `/api/info`** is explicitly handled as "The previous V2 service exposes `/api/status` instead" →
  `compatible: false` → the client stops and replaces it.
- **Version gate**: `matchesVersion` — mismatch ⇒ the client stops the old daemon and spawns a new
  one (`ensure()`; `mismatch: "replace"` is what the TUI uses).
- **Self-healing**: the daemon re-reads its own registration every **5 s**; if another process
  replaced it (different `id`/`version`/`url`/`pid`/`password`) it logs
  `"managed service registration replaced; shutting down"` and exits.
- **Liveness recovery**: 3 consecutive probe timeouts ⇒ `PtyHandoff.clear`, then SIGTERM, wait,
  SIGKILL, unlink the registration file, respawn.

`opencode service status` prints the discovered URL or `stopped`
(`handlers/service/status.ts`); `opencode service start` prints the URL after
`Service.ensure(...)` succeeds.

### 3.4 Service config file

`~/.config/opencode/service.json` (mode 0600, dir overridable with `OPENCODE_CONFIG_DIR`), shape
`{ hostname?, port?, password?, cors?, env? }` (`ServiceConfig.Info`). `service set`/`unset` **stop
the daemon first** and then rewrite the file (temp + rename). `service get password` prints the
password, **generating and persisting a 32-byte base64url one on first use**; `service get` with no
key prints the config with `password` redacted. The comment in `ServiceConfig.password` states the
intent: *"Keep one private credential across server restarts so discovered clients can reconnect
without exposing a password flag or environment variable."*

### 3.5 Auth model summary

- Managed service: HTTP Basic, username `opencode`, password = 32 random bytes base64url in
  `~/.config/opencode/service.json` (0600) and echoed in the 0600 registration file.
- Foreground `serve`: `OPENCODE_PASSWORD` (or legacy `OPENCODE_SERVER_PASSWORD`) is adopted; if
  absent a random password is generated and printed to stdout. `Env.password`
  (`packages/cli/src/env.ts` @v2.0.18) redacts both names, and `Env.session()` strips them from the
  environment handed to the TUI.
- `--server <url>`: credentials come from `OPENCODE_PASSWORD` / `OPENCODE_SERVER_PASSWORD`; the
  client probes `client.server.info` with a 5 s timeout and errors with
  *"Server at … requires a password; set OPENCODE_PASSWORD"* on 401.
- **V1, for contrast** (`packages/opencode/src/cli/cmd/serve.ts` @v1.18.32): warns
  `"OPENCODE_SERVER_PASSWORD is not set; server is unsecured."` and otherwise runs with **no auth
  at all**. V2 always has auth.

### 3.6 How the client spawns the daemon

`ServiceConfig.options()` @v2.0.18 returns
`command: [...selfCommand(), "serve", "--service"]`, i.e. **the currently running binary re-execs
itself**. `selfCommand()` (`packages/cli/src/util/process.ts` @v2.0.18) is `[process.execPath]` for
a native binary, or `[bun|node, …flags, entrypoint]` for a source checkout.
`spawnServiceContender` launches it **detached** with `env: { ...process.env, ...env }` — so
`XDG_DATA_HOME`, `XDG_STATE_HOME`, `OPENCODE_CONFIG_DIR` and `OPENCODE_DB` set on the client
**propagate to the daemon**. This is what makes the isolation recipe in §5 work.

### 3.7 Port comparison

| | default port |
| --- | --- |
| V1 `opencode serve` | **0** (ephemeral, OS-assigned) — `packages/opencode/src/cli/network.ts` @v1.18.32. The instance running on this box is on **44475**. |
| V2 `opencode serve` (foreground) | **0** (ephemeral) |
| V2 managed service (`serve --service`) | **49374** |

No port collision by default. (`opencode attach` in V1 documents `http://localhost:4096` as an
example only, not a default.)

---

## 4. Data store layout and the V1↔V2 data relationship

### 4.1 V2 global paths

`packages/util/src/global-roots.ts` + `packages/util/src/global.ts` @v2.0.18. The app name is
hardcoded `"opencode"`; every root is a plain XDG lookup:

```ts
const data   = process.env.XDG_DATA_HOME   || (home ? path.join(home, ".local", "share") : undefined)
const cache  = process.env.XDG_CACHE_HOME  || (home ? path.join(home, ".cache") : undefined)
const config = process.env.XDG_CONFIG_HOME || (home ? path.join(home, ".config") : undefined)
const state  = process.env.XDG_STATE_HOME  || (home ? path.join(home, ".local", "state") : undefined)
```

| Path | Default | Override |
| --- | --- | --- |
| data | `~/.local/share/opencode` | `XDG_DATA_HOME` |
| config | `~/.config/opencode` | `XDG_CONFIG_HOME`, or `OPENCODE_CONFIG_DIR` (used by `Global.node.replace(...)` in `server-process.ts` and `global.ts`) |
| state | `~/.local/state/opencode` | `XDG_STATE_HOME` |
| cache | `~/.cache/opencode` | `XDG_CACHE_HOME` |
| tmp | `$TMPDIR/opencode` | — |
| log | `<data>/log` | — |
| bin | `<cache>/bin` | — |
| repos | `<data>/repos` | — |
| home | `$OPENCODE_TEST_HOME ?? os.homedir()` | — |

### 4.2 The database file

`packages/cli/src/database-path.ts` @v2.0.18, applied as `database.path = databasePath(global.data)`:

```ts
const filename =
  process.env.OPENCODE_DB ??
  (["latest","dev","beta","next","prod"].includes(OPENCODE_CHANNEL) ||
   OPENCODE_DISABLE_CHANNEL_DB === "1" || "true")
    ? "opencode.db"
    : `opencode-${sanitize(OPENCODE_CHANNEL)}.db`
return filename === ":memory:" ? filename : path.resolve(data, filename)
```

So on a stock `latest` build: **`~/.local/share/opencode/opencode.db`**. `OPENCODE_DB` can override
the name (or force `:memory:`). `OPENCODE_CHANNEL` itself is a **compile-time define**
(`Script.channel` baked by `script/build.ts`), not an env var — so the only runtime lever is
`OPENCODE_DB`.

### 4.3 V1 uses the **identical** layout

`packages/core/src/global.ts` @v1.18.32 — same `app = "opencode"`, same XDG roots via
`xdg-basedir`, same `log`/`bin`/`repos` derivations, same `Flock.setGlobal({ state })`.

Verified on this machine (V1 1.18.23, `/usr/bin/opencode`):

```
~/.local/share/opencode/
  opencode.db            8,471,150,592 bytes (8.4 GB)
  opencode.db-shm        opencode.db-wal (31 MB)
  storage/  snapshot/  tool-output/  repos/  log/
  auth.json  account.json
~/.local/state/opencode/
  locks/  model.json  prompt-history.jsonl
~/.config/opencode/
  opencode.json  agents/  node_modules/  package.json
```

### 4.4 The V1↔V2 relationship: **same store, forward-only, destructive**

This is the most important finding for cola.

1. **V2.0.18's migration set is a strict superset of V1.18.32's.** V1.18.32's newest migration is
   `packages/core/src/database/migration/20260622202450_simplify_session_input.ts`. V2.0.18
   contains **all** of those *plus* ten more:

   ```
   20260804233008_loose_psylocke
   20260805200742_import_legacy_credentials
   20260808023530_workspace_domain
   20260811161259_execution_claim_attempts
   20260812181746_session_inbox
   20260812213948_worktree
   20260819222447_session_viewed_state
   20260823191254_nullable_workspace_binding
   20260910120000_clear_v1_session_permission
   20260923013825_project_time_active
   ```

2. **Migrations run unconditionally at server startup.**
   `packages/core/src/database/database.ts` @v2.0.18:
   ```ts
   const semaphore = yield* lock
   yield* semaphore.withPermit(DatabaseMigration.apply(db))
   ```
   The only guard is a per-file `Semaphore` (`lockFor(filename)`) that serialises *bootstrap*.

3. ⇒ **Starting a V2 server against the default data dir applies 10 schema migrations in place to
   V1's `opencode.db`.** Drizzle's migration ledger is append-only and there is no down-migration,
   so V1 1.18.23 keeps running against a database whose ledger is ahead of it. Whether it tolerates
   that was not tested (see §6).

4. **Two of those migrations mutate existing V1-era data:**
   - `20260805200742_import_legacy_credentials` reads `~/.local/share/opencode/auth.json` and
     imports OAuth / API-key / well-known credentials into the new credential tables (destructive
     read of the live auth file).
   - `20260910120000_clear_v1_session_permission` is literally
     `UPDATE session_v2 SET permission = NULL` — it **wipes pending permission state on V1-era
     sessions**. For cola this means in-flight permission cards would silently stop resolving.

5. **There is no V1→V2 session or message migration, and no V1 store reader.**
   The only migration code reachable from the CLI is `packages/cli/src/config/migrate.ts` @v2.0.18,
   and it migrates **TUI config only**: `~/.config/opencode/tui.json` +
   `~/.local/state/opencode/kv.json` → `~/.config/opencode/opencode.json` (theme, keybinds,
   plugins, scroll, diffs, session, …). It never touches sessions. All other
   `migrations/` directories in the v2.0.18 tree are `packages/console/core/migrations/*.sql` —
   Postgres migrations for the hosted console, irrelevant to the local store.

6. **The V1 HTTP bridge is a client-side concern and is unreachable from the CLI.**
   `packages/cli/src/run/v1.ts` @v2.0.18 exports `runV1Bridge(...)`, which resolves either an
   explicit `--server` endpoint or a `Standalone.start()` and then calls
   `runNonInteractiveWithOptions(..., { compatibility: "v1" })`. `grep` over `packages/cli/src`
   shows it is re-exported from `run/index.ts` and consumed **nowhere** — no command in
   `commands.ts` reaches it at v2.0.18. It speaks V1 HTTP, not the V1 store.

7. **Both generations use the same tables.** V1.18.32 already ships
   `packages/core/src/v2-schema.ts` and the migration
   `20260622170816_reset_v2_session_state`, so V1 sessions live in `session_v2` — the same table
   V2's `clear_v1_session_permission` migration targets. The two generations genuinely contend for
   one dataset.

---

## 5. Coexistence recipe and collisions

### 5.1 Collision table (this machine)

| Resource | V1 (1.18.23) | V2 (2.0.18) default | Collision? |
| --- | --- | --- | --- |
| bin `opencode` | `/usr/{bin,sbin}/opencode`, `/bin/opencode` — one inode (654919), 177 MB | `@opencode/cli` declares bin `opencode`; `npm prefix -g` = `/usr` | **YES** — global install clobbers V1 |
| bin `opencode2` | — | same v2 binary (alias) | no clash, misleading name |
| data dir | `~/.local/share/opencode` | same | **YES** |
| state dir | `~/.local/state/opencode` | same | **YES** — V2 adds `service.json` + `service.json` config there |
| config dir | `~/.config/opencode` | same (`opencode.json` shared, `service.json` added) | **YES** |
| DB file | `opencode.db` (8.4 GB) | `opencode.db` | **YES** — 10 migrations applied in place |
| service registration | none (V1 has no daemon and no discovery file) | `~/.local/state/opencode/service.json` (0600) | new file; V1 unaware |
| default port | `serve --port` default **0**; the running one is 44475 | managed service **49374**; foreground 0 | no (by luck) |
| auth | `OPENCODE_SERVER_PASSWORD`, **unsecured** if unset | Basic `opencode:<random 32 B>`, always on | different mechanisms |
| self-upgrade | `opencode upgrade` on `opencode-ai` | `npm install --global --force @opencode/cli@<ver>` | **YES** — hijacks `/usr/bin/opencode` |

### 5.2 Recommended side-by-side layout

Keep V1 exactly where it is; put V2 in `/opt` with a wrapper that redirects every global path.

```bash
# 1. Install V2 outside the global bin namespace (never `npm i -g`)
sudo mkdir -p /opt/opencode-v2 && cd /opt/opencode-v2
sudo npm init -y
sudo npm install @opencode/cli@2.0.18

# 2. Give V2 its own data / state / config / cache trees
sudo mkdir -p /var/lib/opencode-v2/{share,state,config,cache}

# 3. Wrapper: V1 stays the default `opencode`; V2 is `opencode-v2`
sudo tee /usr/local/bin/opencode-v2 >/dev/null <<'EOF'
#!/usr/bin/env bash
export XDG_DATA_HOME=/var/lib/opencode-v2/share
export XDG_STATE_HOME=/var/lib/opencode-v2/state
export XDG_CONFIG_HOME=/var/lib/opencode-v2/config
export XDG_CACHE_HOME=/var/lib/opencode-v2/cache
export OPENCODE_CONFIG_DIR="$XDG_CONFIG_HOME/opencode"
exec /opt/opencode-v2/node_modules/.bin/opencode "$@"
EOF
sudo chmod +x /usr/local/bin/opencode-v2

# 4. Optional: make the V1 binary explicit and stop `upgrade` confusion
sudo ln -sfn /usr/bin/opencode /usr/local/bin/opencode-v1
```

`/usr/local/bin` precedes `/usr/bin` in the default `PATH`, so `opencode` keeps resolving to V1
while `opencode-v2` (and `opencode-v1` if you prefer the explicit spelling) go where you point
them.

Why the XDG variables are sufficient: they are read at module load in `global-roots.ts`
(`packages/util` @v2.0.18) inside the client process, and the managed daemon is spawned with
`env: { ...process.env, ...env }` (`packages/client/src/service-contender.ts` @v2.0.18), so the
daemon inherits them. The registration file follows `XDG_STATE_HOME` (both `ServiceConfig.paths`
and `Service.fallback()`), the service config follows `OPENCODE_CONFIG_DIR`, and the DB follows
`XDG_DATA_HOME`.

Verify the isolation before trusting it:

```bash
opencode-v2 debug paths                 # db home data config cache state tmp bin log repos
opencode-v2 debug paths db              # -> /var/lib/opencode-v2/share/opencode/opencode.db
opencode-v2 debug paths state           # -> /var/lib/opencode-v2/state/opencode
opencode-v2 service status              # -> stopped
opencode-v2 service start               # -> http://127.0.0.1:49374
opencode-v2 service get password        # lease password (auto-generated, 0600)
opencode-v2 service stop
ls -l /var/lib/opencode-v2/state/opencode/service.json   # 0600
```

Confirm V1 is untouched: `ls -l ~/.local/share/opencode/opencode.db` (mtime unchanged),
`stat -c %i /usr/bin/opencode` (still 654919), `opencode --version` (still 1.18.23).

### 5.3 Usage notes for the isolated V2

- **TUI**: `opencode-v2` — auto-starts / reuses the isolated daemon on 49374.
- **Headless**: `opencode-v2 run --format json "…"`, or `opencode-v2 api /session` to hit the server.
- **Point a client at the existing V1 server** instead (read-only, no store contention):
  `OPENCODE_PASSWORD=… opencode-v2 --server http://127.0.0.1:44475`
  (the version-mismatch warning is non-fatal: *"Server at … has version …; this client is …
  Continuing anyway."*).
- **Don't rely on `--standalone` for isolation.** It only changes port and password *lifetime*;
  the server still opens `databasePath(global.data)`. The XDG wrapper is what isolates the store.
- **Isolate dev builds too.** A dev checkout run with `bun run dev` will *replace* the registered
  daemon when versions differ (`mismatch: "replace"`, and the daemon self-evicts after 5 s when its
  registration is overwritten). The documented workflow
  (`AGENTS.md` @v2.0.18, "Live V2 TUI Testing") uses
  `opencode service status` + `opencode service get password` + an explicit `--server` +
  `OPENCODE_TUI_CHANNEL=dev` to *avoid* replacing the live server; add the same XDG wrapper if you
  don't want a dev run touching the installed daemon's state dir at all.
- **cola's own constraint** (from this repo's `AGENTS.md`): sessions are shared through the
  **default store** `~/.local/share/opencode`, and cola reads port+password live from `/proc`. A V2
  server must not be started on that store while cola/V1 1.18.23 is live — §4.4 explains why.

### 5.4 What *not* to do

- `npm install -g @opencode/cli` — replaces `/usr/bin/opencode` (all four hardlinked paths).
- `curl -fsSL https://opencode.ai/v2/install | bash` — overwrites `~/.opencode/bin/opencode`.
- `npm i -g opencode2` — installs the unofficial derivative (§1.6).
- Running `opencode-v2 upgrade` / `opencode upgrade` on a global install — the updater's npm path
  hardcodes `--global --force` against `metadata.package`.
- Starting any V2 server without `XDG_DATA_HOME` pointed elsewhere, while V1 is running.

---

## 6. Open questions / what could not be verified

1. **Provenance of the `opencode2` npm binary.** It self-reports `opencode v2.0.16` and is
   202,986,976 bytes vs upstream `@opencode/cli-linux-x64@2.0.16`'s comparable size, so it is very
   likely a genuine upstream build — but there is **no repository, no tag, and no published hash to
   diff it against**, and its README advertises features absent from upstream. Whether the binary
   is byte-identical to upstream, or a patched build, could not be established. Nothing in upstream
   v2.0.18 mentions `code-review-graph`, prompt modes, or a `bsk` helper.
2. **Does V1 1.18.23 tolerate a DB whose migration ledger is 10 entries ahead?** Not tested. The
   ledger is append-only and drizzle does not down-migrate, so recovery would mean restoring a copy
   of the 8.4 GB `opencode.db`. **Recommend taking a filesystem-level copy before any V2
   experiment.**
3. **Concurrent V1 + V2 servers on one `opencode.db`.** The only coordination found is
   `Flock.setGlobal({ state })` plus the per-path bootstrap `Semaphore` in
   `packages/core/src/database/database.ts` — that serialises *bootstrap*, not concurrent operation.
   Two servers writing one SQLite file is untested by anything in the tree.
4. **No official user-facing V2 documentation exists.** `opencode.ai/docs` and the `README.md` at
   tag `v2.0.18` both still document V1. Anything about V2 install/service semantics had to be read
   out of the source and the update-service JSON. It is unknown whether a V2 docs page is planned
   or whether the V1 docs will be replaced.
5. **`runV1Bridge` (`packages/cli/src/run/v1.ts` @v2.0.18) is exported but unreachable** from any
   command in `commands.ts`. Its intended consumer (Desktop app? in-flight work?) is unconfirmed.
   If it is wired up later, V2's TUI would be able to drive a V1 server — a coexistence path worth
   re-checking.
6. **`OPENCODE_CHANNEL` is compile-time only.** There is no env var to run an installed v2 binary
   under a different channel, so the only runtime control over the DB filename is `OPENCODE_DB`
   (or the XDG data dir). The per-channel DB/port/registration-file split
   (`service-<channel>.json`, hash-derived ports) is therefore not reachable from a stock install.
7. **Not installed or executed on this machine** (the ticket's read-only constraint), so
   install-time behaviour — the `postinstall` hardlink, `verifyBinary()`'s `--version` spawn, the
   `@opencode/cli-linux-x64` optional-dependency resolution — is read from
   `packages/cli/script/postinstall.mjs` @v2.0.18 rather than observed. The one thing actually
   executed is the third-party `opencode2-linux-x64@2.0.16` binary (`--version` → `opencode v2.0.16`).
8. **V2's HTTP API surface was not audited** beyond `/api/info` and the auth/discovery handshake.
   Whether V2's `/api/*` routes are wire-compatible with what cola currently calls on a V1 server
   is a separate question (this repo's `AGENTS.md` pitfall #1 already flags that the prefix
   generation was redefined mid-project).
9. **Whether the update service will ever move V2 onto the `opencode-ai` package name** (which
   would remove the bin conflict) or the reverse — no signal either way in the tree. Today they are
   two permanently separate npm lineages.

---

## Source index

| Claim | Source |
| --- | --- |
| Official V2 package + version + channel | `curl https://opencode.ai/update/api/latest/cli/npm?current=0.0.0`; `npm view @opencode/cli dist-tags` |
| V2 npm package shape (bins, platform deps) | `packages/cli/package.json`, `packages/cli/script/publish.ts`, `npm view @opencode/cli@2.0.18`, @v2.0.18 |
| V2 postinstall mechanics | `packages/cli/script/postinstall.mjs` @v2.0.18 |
| V2 curl installer | `install` @v2.0.18; sha256 match with live `https://opencode.ai/v2/install` |
| No V2 GitHub Releases | `gh release list --repo anomalyco/opencode`; `gh release view v2.0.18` → "release not found" |
| V1 package = `opencode-ai` | `npm view opencode-ai version bin dist-tags`; `packages/opencode/package.json` @v1.18.32 (`private: true`) |
| Release workflow / channels | `.github/workflows/publish.yml`, `script/version.ts`, @v2.0.18 |
| `opencode2` npm is unofficial | `npm view opencode2`; `npm view opencode2-linux-x64`; `gh repo view game-libgdx-unity/opencode2` (404); unpacked tarballs; `./package/bin/opencode2 --version` |
| `opencode`/`opencode2` are the same binary | `packages/cli/bin/opencode2.cjs`, `packages/cli/bin/opencode.cjs`, `packages/cli/script/build.ts` (`OPENCODE_CLI_NAME: "'opencode'"`) @v2.0.18 |
| Command surface | `packages/cli/src/commands/commands.ts` @v2.0.18 |
| Three serve modes, host/port/password | `packages/cli/src/server-process.ts` @v2.0.18 |
| Default port 49374 / 49375 | `packages/cli/src/services/service-config.ts` (`defaultPort`) @v2.0.18 |
| Registration file, discovery, auth, self-heal | `packages/client/src/effect/service.ts`, `packages/client/src/service.ts`, `packages/client/src/service-contender.ts`, `packages/cli/src/services/service-registration.ts` @v2.0.18 |
| `--stdio` lease / standalone | `packages/cli/src/services/standalone.ts` @v2.0.18 |
| `OPENCODE_PASSWORD` env names | `packages/cli/src/env.ts` @v2.0.18 |
| Global paths (V2) | `packages/util/src/global-roots.ts`, `packages/util/src/global.ts` @v2.0.18 |
| Global paths (V1, identical) | `packages/core/src/global.ts` @v1.18.32 |
| DB filename resolution | `packages/cli/src/database-path.ts` @v2.0.18 |
| Migrations run at startup | `packages/core/src/database/database.ts` @v2.0.18 |
| V2 = V1 migrations + 10 | `git ls-tree -r v2.0.18 packages/core/src/database/migration` vs `v1.18.32` |
| Destructive V2 migrations | `.../20260805200742_import_legacy_credentials.ts`, `.../20260910120000_clear_v1_session_permission.ts` @v2.0.18 |
| Config-only migration (no session migration) | `packages/cli/src/config/migrate.ts` @v2.0.18 |
| V1 HTTP bridge, unreferenced | `packages/cli/src/run/v1.ts`, `packages/cli/src/run/index.ts`, `packages/cli/src/run/noninteractive.ts` @v2.0.18 |
| V1 serve: port 0, optional password | `packages/opencode/src/cli/network.ts`, `packages/opencode/src/cli/cmd/serve.ts` @v1.18.32 |
| V1 still developed; V2 branch is default | `AGENTS.md` @v2.0.18 ("The default branch in this repo is `v2`"); `npm view opencode-ai time` |
| Local machine state | `opencode --version`, `stat -c %i /usr/{,s,}bin/opencode`, `ls ~/.local/share/opencode`, `ps aux \| grep opencode`, `npm prefix -g` |
| Docs still describe V1 | `curl https://opencode.ai/docs/`; `README.md` @v2.0.18 |
