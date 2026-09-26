# R6 — Provisioning OpenCode V2 alongside V1: verified coexistence recipe

**Ticket:** [#361](https://github.com/SXongin/cola/issues/361) on wayfinder map [#353](https://github.com/SXongin/cola/issues/353) · **Date:** 2026-09-26
**Question:** Provision OpenCode V2 on this machine alongside the existing V1 setup **without disturbing
the daily V1 workflow**, and capture the exact working recipe.

**Verdict:** Done and verified. V2 (`@opencode/cli@2.0.18`) is installed at `/opt/opencode-v2`, runs
through an XDG-isolating wrapper `/usr/local/bin/opencode-v2`, and starts/stops on demand on
`127.0.0.1:49374`. The default store, the V1 binary, the V1 config, and the running V1 server were
proven untouched before/after. OpenChamber 1.21.0 still spawns its V1 child exactly as before.

**One correction to R1's picture:** on a *fresh* data dir, V2 **skips all migration bodies** (it
creates the full schema and marks every migration applied). So an isolated store does **not** import
`auth.json` — credentials must be added explicitly. Details in §5.2.

---

## 1. What is now provisioned on this machine

| Thing | Value |
| --- | --- |
| V2 install | `/opt/opencode-v2/node_modules/@opencode/cli@2.0.18` (metapackage) |
| V2 platform binary | `@opencode/cli-linux-x64@2.0.18` → 203,146,720 B, inode 1055802 |
| V2 entry points | `/opt/opencode-v2/node_modules/.bin/{opencode,opencode2}` → same `bin/opencode.exe` (one binary; `opencode2` is an alias, R1 §1.7 confirmed) |
| Version check | `opencode-v2 --version` → `opencode v2.0.18` |
| Wrapper | `/usr/local/bin/opencode-v2` (see §3) |
| Isolated trees | `/var/lib/opencode-v2/{share,state,config,cache}` |
| V2 service port | `127.0.0.1:49374` (`serve --service`) |
| Registration file | `/var/lib/opencode-v2/state/opencode/service.json` (0600, removed by `service stop`) |
| Service config/credentials | `/var/lib/opencode-v2/config/opencode/service.json` (0600) |
| Isolated DB | `/var/lib/opencode-v2/share/opencode/opencode.db` |
| Shared config link | `/var/lib/opencode-v2/config/opencode/opencode.json` → `/root/.config/opencode/opencode.json` |
| V2 service state | **stopped** (leave it stopped; start with `opencode-v2 service start`) |

Nothing under `/usr`, `~/.local/share/opencode`, `~/.local/state/opencode`, or `~/.config/opencode`
was modified. No `~/.opencode/bin/opencode` was created (both the V1 and V2 curl installers would
clobber that path).

## 2. Install — exact commands (and the npm 12 gotcha)

```bash
mkdir -p /opt/opencode-v2 && cd /opt/opencode-v2
npm init -y
npm install --no-audit --no-fund @opencode/cli@2.0.18
```

**Measured on this box with npm 12.0.2:** npm now *blocks install scripts by default*, so the
package's `postinstall.mjs` did **not** run:

```
npm warn install-scripts 1 package had install scripts blocked because they are not covered by allowScripts:
npm warn install-scripts   @opencode/cli@2.0.18 (postinstall: node ./postinstall.mjs)
```

Without it, `node_modules/@opencode/cli/bin/opencode.exe` stays a 229-byte placeholder (the real
203 MB binary sits unused in `@opencode/cli-linux-x64`). Fix — either run the script directly (what
was done here) or approve it:

```bash
# Option A (used here): run it in place
cd /opt/opencode-v2/node_modules/@opencode/cli && node ./postinstall.mjs

# Option B: use npm's new approval flow, then rebuild
npm install-scripts approve @opencode/cli && npm rebuild @opencode/cli
```

Verify (both must print `opencode v2.0.18` and share one inode):

```bash
stat -c '%i %s %n' /opt/opencode-v2/node_modules/@opencode/cli/bin/opencode.exe \
  /opt/opencode-v2/node_modules/@opencode/cli-linux-x64/bin/opencode
/opt/opencode-v2/node_modules/.bin/opencode --version
/opt/opencode-v2/node_modules/.bin/opencode2 --version
```

A machine with npm ≤ 11 (scripts allowed) needs no extra step. The registry here is
`registry.npmmirror.com` and `@opencode/cli@2.0.18` + its `linux-x64` platform package resolved fine.

## 3. Isolation wrapper

`/usr/local/bin/opencode-v2` (mode 0755, owned by root):

```bash
#!/usr/bin/env bash
export XDG_DATA_HOME=/var/lib/opencode-v2/share
export XDG_STATE_HOME=/var/lib/opencode-v2/state
export XDG_CONFIG_HOME=/var/lib/opencode-v2/config
export XDG_CACHE_HOME=/var/lib/opencode-v2/cache
export OPENCODE_CONFIG_DIR="$XDG_CONFIG_HOME/opencode"
exec /opt/opencode-v2/node_modules/.bin/opencode "$@"
```

`/usr/local/bin` precedes `/usr/bin` in `PATH`, so `opencode` still resolves to V1 (1.18.23) and
`opencode-v2` is the isolated V2. The XDG variables are read in-process and are inherited by the
spawned daemon (`spawnServiceContender` passes `env: { ...process.env, ...env }`, R1 §3.6) — verified
by reading `/proc/<v2-pid>/environ` (see §4.2).

Caveat: the wrapper does not sanitise the inherited environment. If the calling shell has
`OPENCODE_SERVER_PASSWORD` set (our agent shells do: `cola-secret`), the daemon inherits it too. In
**service mode it is ignored** (the service uses its own persisted random password — verified), but
a foreground `opencode-v2 serve` would adopt it. Normal user shells don't set it.

## 4. Verification evidence

### 4.1 V2 paths and service

```
$ opencode-v2 debug paths
home       /root
data       /var/lib/opencode-v2/share/opencode
cache      /var/lib/opencode-v2/cache/opencode
config     /var/lib/opencode-v2/config/opencode
state      /var/lib/opencode-v2/state/opencode
tmp        /tmp/opencode
db         /var/lib/opencode-v2/share/opencode/opencode.db

$ opencode-v2 service start
http://127.0.0.1:49374

$ ls -l /var/lib/opencode-v2/state/opencode/service.json
-rw------- 1 root root 163 ... service.json
$ cat ...                        # {id, version:"2.0.18", url:"http://127.0.0.1:49374", pid, password}

$ curl -u opencode:$PW http://127.0.0.1:49374/api/info
{"version":"2.0.18","pid":7494,"urls":["http://127.0.0.1:49374"],"paths":{"tmp":"/tmp/opencode"}}
$ curl -o /dev/null -w '%{http_code}' http://127.0.0.1:49374/api/info   # 401 without auth
```

*The daemon process:* `/opt/opencode-v2/node_modules/@opencode/cli/bin/opencode.exe serve --service`,
listening on 49374; `/proc/<pid>/environ` contains `XDG_DATA_HOME/XDG_STATE_HOME/XDG_CONFIG_HOME/
XDG_CACHE_HOME` = the isolated trees and `OPENCODE_CONFIG_DIR` = the isolated config dir.
`service stop` → `stopped`, registration file gone, 49374 closed.

### 4.2 V1 was not disturbed (before vs after)

| Check | Before | After |
| --- | --- | --- |
| `opencode --version` | 1.18.23 | 1.18.23 |
| inode `/usr/{,s}bin/opencode`, `/bin`, `/sbin` | 654919 (one file) | 654919, unchanged |
| running V1 server (`:4096`, `OPENCODE_SERVER_PASSWORD=cola-secret`) | `{"healthy":true,"version":"1.18.23"}` | same, still running |
| default DB `__drizzle_migrations` | `20 | 1778457851000` | `20 | 1778457851000` |
| `~/.config/opencode/opencode.json` sha256 | `fc54126d…` | `fc54126d…` |
| `~/.config/opencode/AGENTS.md`, `package.json`, `agents/vision.md` | hashes recorded | identical |
| `~/.local/state/opencode/` | locks, model.json, prompt-history.jsonl | unchanged |

The isolated V2 DB is its own file (`/var/lib/opencode-v2/share/opencode/opencode.db`) created fresh
by the bootstrap log line `database schema bootstrap started migrations=48` / `completed`. For
comparison: the V1 DB's `migration` table has 38 rows → **V2 adds 10 migrations to the default store
when pointed at it**, matching R1 §4.4.

### 4.3 Ports and collision table after provisioning

| Resource | V1 | V2 (isolated) | Collision |
| --- | --- | --- | --- |
| binary `opencode` | `/usr/bin/opencode` (inode 654919) | `/opt/opencode-v2/…` | none |
| server | `:4096` (manual), OpenChamber child got `:38313`/`:44475` (ephemeral) | `:49374` | none |
| data/state/config | `~/.local/share|state|config/opencode` | `/var/lib/opencode-v2/*` | none |
| DB file | `~/.local/share/opencode/opencode.db` | `/var/lib/opencode-v2/share/opencode/opencode.db` | none |
| auth | `OPENCODE_SERVER_PASSWORD=cola-secret` (server-side) | HTTP Basic `opencode:<random 43-char>` | different |

### 4.4 OpenChamber 1.21.0 (daily flow) smoke test

```
$ openchamber serve          # daemon, :3000
$ curl -o /dev/null -w '%{http_code}' http://127.0.0.1:3000/   # 200
$ ps …                        # /usr/sbin/opencode serve --hostname 127.0.0.1 --port 38313
$ curl -u opencode:$PW http://127.0.0.1:38313/global/health
{"healthy":true,"version":"1.18.23"}
$ openchamber stop           # child + managed registration cleaned up
```

Its managed-server record still resolves `binary: "/usr/sbin/opencode"`; nothing about the V2 install
changed that. (Fresh ephemeral port each start, as before.)

### 4.5 Config sharing (`~/.config/opencode`) — verified behaviour

The isolated config dir contains a **symlink** to the real V1 config:
`/var/lib/opencode-v2/config/opencode/opencode.json -> /root/.config/opencode/opencode.json`.

With that link in place, V2 loaded and normalised the V1-shaped config entirely in memory:

* `opencode-v2 debug agents` gained `code-reviewer` with `model:{id:"space-bunny-free",providerID:"opencode"}`,
  `request.body.temperature: 0.1`, `steps: 15` (all migrated from the V1 keys);
* `title` got `model:{id:"mimo-v2.5",providerID:"opencode-go"}` (from `small_model`);
* the server log shows `mcp connected server=codegraph tools=1` and `mcp connected server=context7 tools=2`
  (both MCP servers from the config, remote context7 included, no OAuth prompt);
* the source file's sha256 was **identical before and after** — V2 never rewrites `opencode.json`
  (matches R3 §1.5/§4).

`OPENCODE_CONFIG=<file>` is **not** honoured by the CLI debug commands (they listed only directory
sources and never showed the file), so the symlink/global-config route is the working one for testing.
V2's own files (`cli.json`, `service.json`) still land in the isolated config dir because the wrapper
sets `OPENCODE_CONFIG_DIR`.

Sharing semantics for the spec: keep the source file V1-parseable (never hand-convert it), and V2
reads it fine; V1 must never be pointed at a V2-native file (R3 §4). A fully separate config dir is
one `rm` of the symlink away.

## 5. Credentials

### 5.1 Where V2 keeps them

V2 credentials live in the SQLite **`credential` table** inside the data dir; `auth.json` is
referenced *only* by the one-time import migration (`packages/core/src/database/migration/20260805200742_import_legacy_credentials.ts`).
`opencode-v2 auth list` lists credential **connections** (it prints "No authenticated integrations"
when the table is empty).

### 5.2 The fresh-store surprise (important)

`packages/core/src/database/migration.ts` → `apply()`:

* **empty data dir** → `schema.up(tx)` then `INSERT` every migration id into `migration`, **without
  executing any migration body**;
* **existing DB with a session table** → `applyOnly()`, which does run the missing bodies (this is the
  V1→V2 upgrade path R1 described).

Observed consequence: we copied `auth.json` into the isolated data dir *before* the first start, and
after boot the `credential` table was still empty while `20260805200742_import_legacy_credentials`
was marked applied (copying `auth.json` in after the first start of course changes nothing). **A fresh
isolated store gets no credentials from `auth.json`.** The import only happens when V2 is started
against an existing V1 database — i.e. on the default store (destructive, R1 §4.4) or a full copy of it
(8.4 GB here; not exercised).

### 5.3 How to give the isolated V2 credentials (verified)

1. **Interactive login** (simplest for a human):
   `opencode-v2 auth login opencode-go` (or `opencode`, `deepseek`, …). Persists into the isolated DB.
2. **API** (scriptable; verified end-to-end with a throwaway key):
   ```bash
   PW=$(opencode-v2 service get password)
   curl -u "opencode:$PW" -X POST \
     'http://127.0.0.1:49374/api/integration/deepseek/connect/key?location[directory]=/root' \
     -H 'content-type: application/json' -d '{"key":"<api-key>"}'
   # → 204; opencode-v2 auth list shows "DeepSeek  DeepSeek  stored"
   opencode-v2 auth logout deepseek <credential-id>   # remove
   ```
3. **Environment variables** (no persistence): the integration list (`GET /api/integration`) declares
   the env name per provider — `OPENCODE_API_KEY` for both `opencode` (`OpenCode Console`) and
   `opencode-go` (`OpenCode Go`); `DEEPSEEK_API_KEY` for `deepseek`. Export before launching.

## 6. Runbook — start/stop V2 for testing

```bash
# start (daemon on 49374; auto-starts the data dir if absent)
opencode-v2 service start
opencode-v2 service status              # prints the URL, or "stopped"

# use it
opencode-v2                             # TUI against the isolated daemon
opencode-v2 run --format json "…"       # headless
opencode-v2 api GET /api/session        # raw HTTP via the CLI
PW=$(opencode-v2 service get password)
curl -u "opencode:$PW" http://127.0.0.1:49374/api/info

# inspect
opencode-v2 debug paths [db|config|state|data]
opencode-v2 debug agents                # shows the migrated V1 config (agents/permissions)
opencode-v2 auth list

# stop (removes the registration file; DB/state survive)
opencode-v2 service stop
```

Do **not**:
* `npm install -g @opencode/cli` (clobbers `/usr/bin/opencode` = V1),
* run `curl -fsSL https://opencode.ai/v2/install | bash` (overwrites `~/.opencode/bin/opencode`),
* `npm i -g opencode2` — that is the **unofficial third-party** package (one maintainer, declared repo
  404s, advertises features absent from upstream; R1 §1.6). The official `@opencode/cli` also ships a
  bin named `opencode2`, but it is the *same* V2 binary — install only from `@opencode/cli`,
* start any V2 server without the wrapper while V1 is live, or point V2 at the default store without a
  fresh DB backup.

## 7. What was intentionally not done

* No V2 run against the default store or a full copy of it (8.4 GB) — the migration-on-upgrade path and
  "does V1 tolerate a ledger 10 ahead" remain untested (R1 §6 q.2/q.3). User opted out of a DB backup
  for this session; the recipe does not touch the store, so none was needed.
* No V2 TUI/`run` session driven end-to-end (no credentials were seeded; mechanism verified with a
  dummy via the API).
* No edits to `cola.toml`/cola code — that is the architecture ticket (#359) and the spec (#362).
* V2 service left **stopped**, OpenChamber left stopped but verified working.

## 8. Source index

| Claim | Evidence |
| --- | --- |
| Install, bins, version | `/opt/opencode-v2` inspection; `--version`; `npm view @opencode/cli@2.0.18`; R1 |
| npm 12 blocks postinstall | npm warn output captured; `node ./postinstall.mjs` fix; placeholder vs 203 MB hardlink |
| Wrapper + isolated paths | wrapper file; `opencode-v2 debug paths`; `/proc/<daemon>/environ` |
| Service, port, registration, auth | `service start/status/get password`; `ls -l` (0600); `curl /api/info` 200/401; daemon cmdline |
| V1 untouched | before/after inode, `--version`, health, migration ledger, config hashes |
| V2 adds 10 migrations | V1 `migration` count 38 vs V2 fresh-bootstrap 48; R1 §4.4 |
| OpenChamber 1.x still works | `openchamber serve` → child `/usr/sbin/opencode serve … :38313` → `/global/health` 1.18.23 → `openchamber stop` |
| Config read/migrate, never rewritten | `debug agents` output (code-reviewer/title models, temperature), MCP-connected log lines, unchanged sha256; R3 §1.5/§4 |
| `OPENCODE_CONFIG` ignored by debug commands | `debug config` listed only directory sources; source `server-process.ts:107-115` |
| Fresh store skips migration bodies | `packages/core/src/database/migration.ts` `apply()` empty-DB branch + observed empty `credential` table with `auth.json` present |
| Credential store + API + env names | `credential` table schema/rows; `GET /api/integration`; `POST …/connect/key` → 204; `auth list`; `auth logout` |
| Unofficial `opencode2` npm package | R1 §1.6 (re-verified package name/`@opencode/cli` channel) |
