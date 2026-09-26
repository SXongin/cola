# R3 — OpenCode V2: config, plugins, agents/skills/MCP, permissions

**Ticket:** #356 on wayfinder map #353 (cola) · **Date:** 2026-09-26 · **Author:** research pass

## Sources & method

| What | Where |
|---|---|
| V2 source | `/root/workspace/dev/opencode` @ `v2.0.18` (commit `cd9a14a6`), read with `git show v2.0.18:<path>` — read-only, nothing checked out |
| V1 baseline | same repo, current checkout (`16c56fe5ec`), `packages/opencode/src/config/`, `packages/plugin/src/` |
| V2 official docs | `https://opencode.ai/v2/docs/{config,permissions,plugins,build/plugins,mcp-servers,migrate-v1}` (fetched 2026-09-26) |
| OpenChamber | `/root/workspace/dev/openchamber` @ `v2.0.2`, read-only |
| This machine | `~/.config/opencode/` (`opencode.json`, `AGENTS.md`, `agents/vision.md`), `/root/workspace/dev/cola/.opencode/` |

**Authority order used throughout:** V2 source > V2 docs > the in-product `opencode` skill
(`packages/core/src/plugin/skill/opencode.md` @ v2.0.18). Two traps are worth stating up front:

1. The docs **shipped inside the v2.0.18 tree** (`packages/web/src/content/docs/*.mdx`) are still the
   **V1** docs — e.g. `permissions.mdx` documents the singular `permission` map and `tools`. Do not
   use them for V2. The live V2 docs are under `/v2/docs/`.
2. `https://opencode.ai/config.json` — the `$schema` URL your file already carries — "may describe V1
   even though V2 configuration files include that URL for editor integration. Never use it to infer V2
   field names or shapes." (`packages/core/src/plugin/skill/opencode.md:26-28`). It will validate and
   autocomplete your current V1 file perfectly while telling you nothing about V2.

---

## 1. V2 config model

### 1.1 File names

V2 accepts exactly two config file names: **`opencode.json` and `opencode.jsonc`**
(`packages/core/src/config/discovery.ts:11` @ v2.0.18).

**Removed:** V1 also read `config.json` (`packages/opencode/src/config/config.ts:141,272` @ HEAD) and
auto-converted a legacy TOML `~/.config/opencode/config` into it (`config.ts:276-289`).
V2 reads neither. This machine has neither file, so **no impact** — but it is a real break for anyone
who relied on it. `~/.config/opencode/tui.json` / project `tui.json` are likewise gone; terminal
settings now live in one global `~/.config/opencode/cli.json` (`packages/cli/src/config/config.ts:28`,
`packages/cli/src/config/schema.ts:4` — `SchemaURL = "https://opencode.ai/v2/cli.json"`), and that
file is **auto-migrated on first V2 terminal-client start** from `tui.json` + `state/kv.json`
(`packages/cli/src/config/migrate.ts:96-121`). V1 files are left on disk untouched.

### 1.2 Locations and merge order

Discovery (`packages/core/src/config/discovery.ts:22-83`), all roots resolved through symlinks:

| Source | Selector |
|---|---|
| wellknown / remote (`WellKnown` service) | credential-gated integration manifests (`config.ts:139-175`) |
| **global** | `global.config` = `$XDG_CONFIG_HOME/opencode` else `~/.config/opencode`; `OPENCODE_CONFIG_DIR` overrides (`packages/util/src/global.ts:19,92`; `global-roots.ts:6-16`) |
| **explicit file** | `OPENCODE_CONFIG` → `options.file` (`packages/cli/src/server-process.ts:113`) |
| **direct files** | `opencode.json(c)` at every ancestor from cwd to `/` (`discovery.ts:30-47`) |
| **`.opencode/` dirs** | every `.opencode` directory on that same walk (`discovery.ts:63-70`) |
| **inline content** | `OPENCODE_CONFIG_CONTENT` (`server-process.ts:126`; `config.ts:181-195`) |
| compatibility roots | `~/.claude`, `~/.agents`, and project `.claude/`, `.agents/` (`discovery.ts:27-29,71-82`) |

Assembled order, lowest → highest precedence (`packages/core/src/config.ts:196-224`):

```
wellknown → global(opencode.json, opencode.jsonc, +Directory entry)
          → explicit file → direct files (farthest ancestor first)
          → .opencode/ dirs (farthest first) → OPENCODE_CONFIG_CONTENT
```

`Config.latest()` takes `entries.findLast(...)`, so **the last document wins per key** and
non-conflicting keys from every layer survive (`config.ts:18-20`). Direct files are reversed relative
to the walk, and `.opencode` entries come after all direct files — so **every discovered `.opencode`
config overrides every discovered direct config** (matches `https://opencode.ai/v2/docs/config`,
"Locations").

Two behaviours that changed vs V1:

- **The project walk no longer stops at the project/git root.** V1 passed `stop: worktree`
  (`packages/opencode/src/config/paths.ts:10-21` @ HEAD); V2 calls `fs.up({ targets: ["."],
  start: location.directory })` with no stop (`discovery.ts:30-32`). For `/root/workspace/dev/cola`
  that means every `opencode.json(c)` between `/root/workspace/dev/cola` and `/` is now in the merge.
  Currently only the global file exists, so this is latent, not active.
- **`~/.claude` and `~/.agents` are skills-only compatibility roots.** `Config.compatibility()` returns
  exactly `{claude, agents}` paths (`config.ts:30-32,264-268`) and the sole consumer is
  `ConfigCompatibilityPlugin`, which scans only `<root>/skills` for `{*.md,**/SKILL.md}`
  (`packages/core/src/config/plugin/compatibility.ts:36-58`). `~/.claude/agents/*.md` is **not** read.

### 1.3 Schema changes (V2 native shape)

`packages/schema/src/config.ts:24-113` is the canonical V2 `Config.Info`. Against V1
(`packages/core/src/v1/config/config.ts:31-191`, which is V2's own copy of the V1 schema):

| V1 key | V2 key | Note |
|---|---|---|
| `model` (string) | `model` | short `"provider/model[#variant]"` **or** `{providerID, model, variant?}` (`schema/config/model.ts:10-30`) |
| `small_model` | *gone* | becomes `agents.title.model` |
| `default_agent` | `default_agent` | unchanged (snake_case kept) |
| `agent`, `mode` | `agents` | `mode` entries forced to `mode: "primary"` |
| `permission`, `tools` | `permissions` | ordered `{action, resource, effect}[]` |
| `provider` | `providers` | `npm`→`package` (prefixed `aisdk:`), `api`→`settings.baseURL`, `options` split into `settings`/`headers`/`body` |
| `plugin` | `plugins` | `["pkg", [path, opts]]` → `["pkg", {package, options}]` |
| `mcp` (flat map) | `mcp.servers` + `mcp.timeout` | `enabled`→`disabled`; one `timeout` int → `{startup, catalog, execution}` |
| `compaction` | `compaction` | `preserve_recent_tokens`→`keep.tokens`, `reserved`→`buffer`; **`prune`/`tail_turns` removed** |
| `command` | `commands` | `subtask`→`subagent` (old name still accepted, `schema/config/command.ts:17-19`) |
| `reference` | `references` | |
| `snapshot` | `snapshots` | |
| `attachment` | `media` | nested image keys unchanged |
| `autoupdate` | `update` | `false`→`"disable"`, `"notify"`→`"notify"`, `true`→`"auto"` |
| `autoshare` | `share` | `true`→`"auto"` |
| `skills: {paths, urls}` | `skills: string[]` | one ordered array |
| — | **new** | `shell`, `update`, `share`, `enterprise`, `username`, `snapshots`, `watcher`, `formatter`, `lsp`, `media`, `tool_output`, `websearch`, `worktree`, `warming`, `experimental` (incl. `experimental.policies`, `portable_shell_scanner`, `subagent_depth`) |
| — | **removed** | `logLevel`, `server`, top-level `subagent_depth`, `layout` (`config/normalize.ts:42`) |

Accepted-but-ignored, each emitting a warning: `logLevel`, `server`, `subagent_depth`, `layout`,
`compaction.tail_turns`, `compaction.prune`, agent `name`, enabled-only MCP entries with no `type`,
`experimental.{batch_tool,openTelemetry,primary_tools,continue_loop_on_deny,disable_paste_summary}`,
provider `{id,whitelist,blacklist}`, provider-model `{release_date,attachment,reasoning,temperature,experimental,status≠deprecated,interleaved}`
(`config/normalize.ts:42-49,585-590,300-330`; migration guide "Accepted but unsupported fields").

**`lsp` and `instructions` are accepted but not executed** in V2 — "V2 accepts and preserves `lsp`
configuration, but it does not run language servers…"; "OpenCode accepts this field but does not load
its entries" for `instructions` (`https://opencode.ai/v2/docs/config`). Both are still *normalised and
kept* by `normalizeLsp` / the `instructions` list (`config/normalize.ts:559-570,215-217`), so nothing
breaks — the capability is just gone.

### 1.4 Permissions

- Ruleset: `Permission.Ruleset = Array<{action: string, resource: string, effect: "allow"|"deny"|"ask"}>`
  (`packages/schema/src/permission.ts:52-58`). Action is a **free string** so plugins can add actions.
- Built-in actions: `read`, `edit`, `glob`, `grep`, `shell`, `subagent`, `skill`, `question`,
  `webfetch`, `websearch`, `external_directory`, `<server>_<tool>` for MCP, `execute` for Code Mode.
  `doom_loop` and `lsp` are explicitly **not** V2 actions (`https://opencode.ai/v2/docs/permissions`).
- **Last match wins.** Order: base policy → lower-priority config → global `permissions` → agent
  `permissions` → session-scoped rules (`ctx.permission.rules`, plugin-only) → hard-deny policies.
  No match ⇒ `ask`.
- Base policy for every agent: `[{*,*,allow},{external_directory,*,ask},{read,*.env,ask},{read,*.env.*,ask},{read,*.env.example,allow}]`,
  plus the shipped `plan`/`explore`/`general`/`title`/`summary` addenda.
- V1→V2 action renames: `bash`→`shell`, `task`→`subagent`, `write`/`patch`→`edit`
  (`packages/core/src/v1/config/migrate.ts:118-123`).
- Multi-resource ops: any `deny` denies, else any `ask` asks, else allow.
- Saved approvals are durable project-scoped `allow` rules, listable/removable via
  `GET|DELETE /api/permission/saved[/:id]`.

**Can an external client still auto-answer? Yes — three ways.**

1. `POST /api/session/{sessionID}/permission` **creates/evaluates** a request and returns
   `{id, effect}` synchronously (`packages/protocol/src/groups/permission.ts:70-86`;
   handler `packages/server/src/handlers/permission.ts:31-47` → `Permission.ask`,
   `packages/core/src/permission.ts:105,130`). If the resolved effect is `allow`/`deny` there is no
   human round-trip at all. This endpoint is new in V2 and did not exist in V1.
2. `POST /api/session/{sessionID}/permission/{requestID}/reply` with
   `{decision: "once"|"always"|"reject", message?}` (`groups/permission.ts:104-118`). `always` also
   persists the tool's proposed patterns. The handler enforces ownership:
   `request.sessionID !== sessionID` ⇒ 404 (`handlers/permission.ts:18-26`).
3. Enumerate with `GET /api/permission/request` (location-scoped) or
   `GET /api/session/{sessionID}/permission` — the latter **works in V2** and returns
   `{data: Permission.Request[]}` (`groups/permission.ts:88-102`). (In V1 this route existed but
   returned empty — that was cola pitfall #3; in V2 it is implemented.)

Docs are explicit that this is the client's job: "Non-interactive clients must decide how to handle
approval requests; configured `deny` rules always remain enforced"
(`https://opencode.ai/v2/docs/permissions`, "Approvals"). `reject` rejects *every* other pending
request in that session.

> **Cola-relevant shape change:** the V1 global `POST /permission/{id}/reply` is replaced by a
> **session-scoped** reply. The parent-chain walk in `resolve_card_target`
> (`src/bridge/pollers.rs`) is still required, because the `task` tool's child session owns the
> request. Nothing about the auto-answer capability is lost.

### 1.5 Live reload

Config, agents, commands, skills, MCP and plugins are all watched and hot-applied. `ConfigWatch.plan`
watches every config root directory plus a parent-entry watch per discovered file so
deletion/recreation is observable (`packages/core/src/config/watch.ts:8-35`); updates debounce 100 ms,
trigger `reload()`, and publish the ephemeral `config.updated` event only when content actually
changed (`config.ts:255-283`). Each domain plugin (`config/plugin/{agent,command,skill,mcp,policy,…}.ts`)
re-scans and calls its `ctx.<domain>.reload()`. Nothing in this path restarts a process.

---

## 2. Migration impact on *this machine's* exact config

`~/.config/opencode/opencode.json` as it stands today. **Nothing needs to change to run V2.** V2
in-memory normalisation (`packages/core/src/config/normalize.ts:51-215` + `v1/config/migrate.ts`)
accepts every key used here, and the source file is **never rewritten**
(`https://opencode.ai/v2/docs/migrate-v1`: "It normalizes supported V1 and native V2 fields in memory
without rewriting the source file").

| Your key | V2 status | What V2 does with it | Action |
|---|---|---|---|
| `$schema: "https://opencode.ai/config.json"` | unchanged | passes through; **still describes V1** — editor validation will not teach you V2 fields | none (optionally keep; do not trust it) |
| `model: "opencode-go/deepseek-v4.1-flash"` | **unchanged shape** | → `model: {providerID:"opencode-go", model:"deepseek-v4.1-flash"}`. `opencode-go` is still a first-class provider in V2's models.dev snapshot, and `deepseek-v4.1-flash` is present in it | none |
| `small_model: "opencode-go/mimo-v2.5"` | **auto-migrated, deprecated** | → `agents.title.model = {providerID:"opencode-go", model:"mimo-v2.5"}` (`normalize.ts:112-124` → `migrate.ts:133-145`). Silent, no warning | optional: move to `agents.title.model` |
| `mcp.context7` (`remote`, `url`, `enabled:true`) | **auto-migrated** | → `mcp.servers.context7 = {type:"remote", url, disabled:false}` (`normalize.ts:246-283`, `migrate.ts:169-192`). `disabled:false` is written explicitly. **But:** remote servers with `oauth !== false` get an OAuth integration registered (`packages/core/src/mcp/index.ts:163-205`); the comment there says "Servers that connect anonymously simply never use the method", so context7 should still connect — **verify `opencode mcp list` shows `connected` and not `needs_auth`** | verify after upgrade; if it demands auth, either sign in via `/mcps` or set `"oauth": false` |
| `mcp.codegraph` (`local`, `command`, `enabled:true`) | **auto-migrated** | → `mcp.servers.codegraph = {type:"local", command:["codegraph","serve","--mcp"], disabled:false}` | none — **but see the Code Mode note below** |
| `permission.bash` (40-entry map) | **auto-migrated** | → `permissions: [{action:"shell",resource:"*",effect:"ask"},{action:"shell",resource:"mkdir*",effect:"allow"}, …]` — entry order preserved, so V1's last-match-wins semantics carry over exactly (`normalize.ts:465-497`, `migrate.ts:100-116`) | none |
| `compaction.auto: true` | **unchanged** | kept | none |
| `compaction.prune: true` | **⚠ dropped** | `unsupportedIfPresent(input.compaction, "prune", …)` → warning "omitted unsupported legacy setting" and the key is gone (`normalize.ts:315`). V2 replaces pruning with checkpoint-based compaction + `compaction.keep.tokens` | **needs a decision** (see below) |
| `compaction.reserved: 10000` | **auto-migrated** | → `compaction.buffer: 10000` | none |
| `provider["opencode-go"].models["deepseek-v4.1-flash"].options.reasoningEffort:"max"` | **auto-migrated** | → `providers["opencode-go"].models["deepseek-v4.1-flash"].settings.reasoningEffort = "max"` (options pass through as `settings`; `ConfigProvider.Model.modelID` is optional so the absent `id` is fine — `schema/config/provider.ts:55-79`) | none |
| `provider.opencode.options.{chunkTimeout,headerTimeout}` | **auto-migrated** | → `providers.opencode.settings.{chunkTimeout,headerTimeout}` (both are typed `Settings` fields — `schema/provider.ts:37-45`) | none |
| `agent.plan/general/explore/summary/compaction` | **auto-migrated** | → `agents.<id>` with `model` split into `{providerID, model}`; `permission` → `permissions` | none |
| `agent.code-reviewer.description` | **unchanged** | same key | none |
| `agent.code-reviewer.mode: "subagent"` | **unchanged** | same key | none |
| `agent.code-reviewer.model: "opencode/space-bunny-free"` | **auto-migrated** | → `{providerID:"opencode", model:"space-bunny-free"}` (present in V2's snapshot) | none |
| `agent.code-reviewer.temperature: 0.1` | **auto-migrated** | → `request.body.temperature = 0.1` (`migrate.ts:148-165`) | none |
| `agent.code-reviewer.steps: 15` | **unchanged** | same key | none |
| `agent.code-reviewer.permission.task:"deny"` | **auto-migrated** | → `{action:"subagent", resource:"*", effect:"deny"}` | none |
| `agent.code-reviewer.permission.todowrite:"deny"` | **⚠ dead rule** | `todowrite` is not renamed by the migration and is **not a V2 action** — the tool was removed (`REMOVED_TOOLS = ["todowrite"]`, `packages/core/src/database/v1-migration.bun.ts:899-901`). The rule survives as a never-matching entry | **clean up** (replace with `question`/`websearch` denies you actually want) |
| `agent.code-reviewer.permission.{edit,webfetch,websearch,question,skill}` | **auto-migrated** | all five are still V2 actions; `edit`/`bash` renames applied | none |
| `agent.code-reviewer.permission.bash` (8-entry map) | **auto-migrated** | → `shell` actions, order preserved | none |
| `plugin` key | **absent** | nothing to migrate. If added later: `plugins`, `["-id"]` to disable | — |
| `~/.config/opencode/AGENTS.md` | **unchanged** | still the global instruction file (`config/plugin/instruction.ts:37`). V2 **dropped V1's `~/.claude/CLAUDE.md` fallback** — migration guide tells you to move such guidance into `AGENTS.md` and file a compatibility issue. This machine has no `CLAUDE.md` dependency | none |
| `~/.config/opencode/agents/vision.md` | **unchanged, works as-is** | V2 scans `{agent,agents}/**/*.md` and `{mode,modes}/*.md` under *every* config `Directory` entry — which includes the global config dir (`config/plugin/agent.ts:20-23,140-150`; `config.ts:171-179`). V1 scanned the same two patterns (`packages/opencode/src/config/agent.ts:13` @ HEAD). Its frontmatter (`description`, `mode`, `model`) is already all-native V2, so it decodes natively, not through the legacy path (`config/plugin/agent.ts:33,157-176`); the Markdown body becomes `system` | none |
| `~/.config/opencode/node_modules`, `package.json` | ignored | V2 reads no `package.json` for config; it only matters if you add `plugins/` packages | none |

**cola repo (`.opencode/`)** — no `opencode.json`; the directory is definitions-only:

| Path | V2 status |
|---|---|
| `.opencode/agent/implementer.md` | discovered (`agent/` still scanned). Frontmatter is **legacy** (`permission:`), so the whole file routes through the V1 decoder and is translated — including `task:"deny"` → `subagent:"deny"` (`config/plugin/agent.ts:157-176`). It carries a dead `todowrite:"deny"` rule. Optional rewrite to `permissions:` + drop `todowrite` |
| `.opencode/skill/foreman/SKILL.md` | discovered — V2 scans both `skill/` and `skills/` for `{*.md,**/SKILL.md}` (`config/plugin/skill.ts:84-88`) |
| `.opencode/package.json` pinning `@opencode-ai/plugin@1.18.23` | inert (there are no plugin files in the repo). V1-era pin; delete or bump when convenient |

### Two behaviour changes with no config key to control them

1. **MCP Code Mode is now the default.** Every local/remote server has
   `codemode` defaulting to `true` (`packages/schema/src/mcp.ts:27-28,44-45`,
   `packages/core/src/plugin/mcp-codemode-defaults.ts`): tools are grouped under a namespaced Code Mode
   surface (`tools.codegraph.<tool>(…)`) instead of sitting on the provider's native tool list. Set
   `"codemode": false` per server to keep them direct. This changes the tool names and shapes cola
   renders, and it is the single most likely source of "my MCP tools vanished" reports after the bump.
2. **Project config is discovered above the project root** (§1.2) — latent here, but it is a
   precedence change, not a rename.

### Recommended sequence

Do nothing to the config. Install V2 alongside (the V2 curl installer replaces the V1 binary, so keep
the V1 binary recoverable), start it, then verify: `opencode mcp list` (both servers `connected`),
the default model resolves, `plan`/`general`/`explore` still use `space-bunny-free`, the `bash` allowlist
still short-circuits (run `cargo fmt` unapproved and confirm no prompt), and `code-reviewer` still
refuses `edit`. Decide on `compaction.prune` (V2's checkpoint compaction is a different mechanism —
keep the warning in mind, don't try to express it) and delete the two dead `todowrite` rules.

---

## 3. Plugins, agents, skills, MCP — format changes

### 3.1 Plugins

**The V1 plugin API is not accepted.** This is one of only two intentional breaking changes
(`https://opencode.ai/v2/docs/migrate-v1`, "Breaking changes"; corroborated by
`packages/core/src/plugin/skill/opencode.md:154-160`).

| | V1 | V2 |
|---|---|---|
| Module shape | `export const server = async (input) => Hooks` (`packages/plugin/src/index.ts:74-79`) | `export default Plugin.define({id, setup(ctx)})` or `{id, effect}` (`packages/plugin/src/promise/plugin.ts:59-66`; `packages/core/src/plugin/module.ts:74-88`) |
| Enrolment | returns a `Hooks` object of named hooks (`event`, `tool.execute.before`, `chat.params`, `permission.ask`, …) — `packages/plugin/src/index.ts:222-330` | `ctx.<domain>.transform(editor => …)` registries + `ctx.<domain>.hook(name, fn)`; domains: `agent, aisdk, command, event, integration, mcp, model, generate, permission, plugin, provider, reference, rpc, session, shell, skill, storage, tool, vcs, websearch, worktree` (`packages/plugin/src/promise/plugin.ts:19-55`) |
| Config key | `plugin` | `plugins`; `["-id"]` disables, `"*"`/`".*"` wildcards; two built-ins ignore removal (`opencode.config.policy`, `opencode.provider.opencode`) |
| Discovery | — | `{plugin,plugins}/` under every config dir, `.ts`/`.js` files and package dirs (`packages/core/src/plugin/source-directory.ts:7-31`); npm names, `@scope/pkg`, `pkg@version`, git specs, `./`, `../`, absolute, `file://` |
| Entrypoint resolution | `server` export | directory → `server` / `tui` / `rpc` subpath exports (`packages/plugin/src/host.ts:16-38`) |

**A V1 plugin that is merely moved or renamed will not load.** `Module` in
`packages/core/src/plugin/module.ts:74-88` decodes `default` as
`{id: string, effect: fn} | {id: string, setup: fn}`; a V1 default export of hook functions fails with
*"Plugin must export a default definition with an id and an effect or setup function."* and a missing
entrypoint fails with *"Plugin entrypoint not found: `<target>`"*
(`module.ts:92-101`). The official guide is explicit: "V1 plugin implementations do not run in V2.
Moving a file or renaming its config entry is not enough."

**Good news for an already-ported plugin:** the V1 line *already ships* this exact API under the
`v2` subpath — `packages/plugin/src/v2/promise/plugin.ts:3-7` is byte-for-byte the V2
`{id, setup}` shape, and `packages/plugin/src/v2/` is the same domain set. In v2.0.18 the `v2`
directory is gone and `promise/`+`effect/` sit at the package root. Porting a `/v2`-subpath plugin is
therefore an import-path + export-shape change, not a rewrite.

**Error reporting** is structured and queryable:

- `GET /api/plugin` → `Plugin.Info[]` with `state: {status:"active"}` or
  `{status:"failed", error, ref?}`, plus `source` (`builtin|package|local|sdk`, with `version`,
  `outdated`, `updating`) and `features {server, tui, rpc}`
  (`packages/protocol/src/groups/plugin.ts:10-22`; `packages/schema/src/plugin.ts:11-42`).
- A plugin that *loads* but whose `transform` throws is **disabled at that registration**, with
  `error: "Plugin disabled after <state>.transform failed. Check server logs for details."` and
  `ref: "err_<8 hex>"`; the log line `disabled plugin after transform failure` carries the cause
  (`packages/core/src/plugin.ts`, `State.group` + `slotInfo`).
- On a *reload* failure the previous revision is reloaded as a fallback, and a failed revision is not
  retried until it changes (`packages/core/src/plugin.ts`, `activate`).
- Surface: `opencode plugin list [--builtin]`, `opencode plugin check`, `POST /api/plugin/check`,
  `POST /api/plugin/update`; details in `~/.local/share/opencode/log/opencode.log`.

### 3.2 Agents

- Config: `agents: Record<string, {model, request, system, description, mode, hidden, color, steps,
  disabled, permissions}>` (`packages/schema/src/config/agent.ts:12-25`); `default_agent` picks the
  primary (`packages/schema/src/config.ts:33-36`).
- Files: all four V1 directories are still scanned — `{agent,agents}/**/*.md` (subagent) and
  `{mode,modes}/*.md` (forced `mode: "primary"`); V2 additionally allows **nesting** under
  `agent(s)/` (path-derived id keeps the `team/reviewer` shape)
  (`packages/core/src/config/plugin/agent.ts:20-23,142-160`; migration guide "Agent files").
  Preferred: `.opencode/agents/<name>.md`. Globals: the same under the global config dir.
- Frontmatter: the body is the system prompt. **A file may not mix native and legacy keys** — one
  unknown key routes the whole file through the V1 decoder, which would silently drop a `permissions`
  array (`config/plugin/agent.ts:33,157-176`; OpenChamber hit this and works around it in
  `config-v2.js`). Rename `prompt`→`system`, `disable`→`disabled`, `permission`→`permissions`, join
  `model`+`variant` as `provider/model#variant`, move `temperature`/`top_p`/`options` under
  `request.body`.
- `GET /api/agent`, `GET /api/agent/{agentID}` (`packages/protocol/src/groups/agent.ts:10,24`).
- `~`/`$HOME` expansion is applied at load time for the `external_directory`, `read`, `edit` actions
  only — **not** for `shell` resources, which stay raw command text
  (`config/plugin/agent.ts:118-135`; permissions doc "Directories").

### 3.3 Skills

- Config: `skills: string[]` — one ordered array of directories or `https:` URLs
  (`packages/core/src/config/normalize.ts:217-234`; `config/plugin/skill.ts:80-105`); `~/` expands
  against home, relative paths against the location directory.
- Files: `{skill,skills}/` under every config dir; `{*.md,**/SKILL.md}`; URL sources need an
  `index.json` index and are cached under `<cache>/skills/<hash>` with version pinning
  (`packages/core/src/skill/discovery.ts:52-101,107-205`). Preferred: `skills/<id>/SKILL.md`.
- Frontmatter: `name`, `description`, `metadata`; `metadata["opencode/autoinvoke"]` boolean → `autoinvoke`
  (`config/plugin/skill-file.ts:9-57`). ID = directory name, or the file basename for a top-level
  `*.md`.
- **New in V2:** `~/.claude/skills` and `~/.agents/skills` are read (both global and project-scoped) —
  `config/plugin/compatibility.ts:36-58`. `GET /api/skill` lists them.
- Built-in skills ship as plugins: `packages/core/src/plugin/skill/{opencode,report}.md` — that
  `opencode.md` is the single most useful primary source in the whole repo for V2 semantics.

### 3.4 Commands

- Config: `commands: Record<string, {template, description, agent, model, subagent, subtask?}>`
  (`packages/schema/src/config/command.ts:12-21`). `subtask` still accepted as a deprecated alias.
- Files: `{command,commands}/**/*.md`, body = template; frontmatter `description`, `agent`, `model`
  (`provider/model#variant`), `subagent` (`packages/core/src/config/plugin/command.ts:27-35,80-…`).
- Behaviour change: "delegated commands now run automatically in the background and report their
  results to the parent session" (migration guide "Commands"). `GET /api/command`.

### 3.5 MCP

- Native: `mcp: {timeout?: {startup, catalog, execution}, servers?: Record<string, Server>}` where
  `Server` is `{type:"local", command[], cwd?, environment?, disabled?, codemode?, timeout?, protocol?}`
  or `{type:"remote", url, headers?, oauth?: OAuth|false, disabled?, codemode?, timeout?, protocol?}`
  (`packages/schema/src/config/mcp.ts:7-22`; `packages/schema/src/mcp.ts:6-71`).
- OAuth fields are **snake_case** in V2: `client_id`, `client_secret`, `scope`, `callback_port`,
  `redirect_uri`, plus new `auth_server_metadata_url` (`mcp.ts:49-58`). V1's `clientId`/`callbackPort`
  are translated (`migrate.ts:196-210`).
- OAuth is **on by default** for remote servers; `"oauth": false` opts out
  (`mcp.ts:60`; `packages/core/src/mcp/index.ts:165-166`). Credentials live outside project config, in
  the global credential store keyed by `sha1(name\0url)` (`mcp/index.ts:168-176`).
- **Protocol versions** (`mcp.ts:19-26`): `legacy` (default — classic `initialize`, revisions up to
  **2025-11-25**), `auto` (probe `server/discover` for **2026-07-28**, fall back to legacy), and
  `2026-07-28` (require it, fail otherwise). Probing a local `legacy` server spawns a second short-lived
  process and can burn the full startup timeout, so scope `auto` narrowly.
- V1 `enabled` → V2 `disabled` (inverted); V1 scalar `timeout` → `{catalog, execution}`;
  `experimental.mcp_timeout` seeds both (`normalize.ts:236-262,268-283`).
- Tool naming: `<server>_<tool>` with every char outside `[A-Za-z0-9_-]` → `_`; MCP prompts become
  commands `<server>:<prompt>`. Permission action is the same normalised `<server>_<tool>` name.
- `CallToolRequest.params._meta.sessionID` carries the OpenCode session id for session-scoped calls
  (request metadata, not a model-visible argument).
- **Layer semantics changed:** "A higher-precedence project config replaces the entire server object
  with the same name" (mcp-servers doc, "Config"). A partial project override no longer merges.
- Management: `opencode mcp add [--global] [--url … | -- cmd…]`, `opencode mcp list`,
  `opencode mcp auth|logout`, `/mcps`; HTTP `GET /api/mcp`,
  `PUT|DELETE /api/experimental/mcp/:server`, `POST …/connect|disconnect`, `GET /api/mcp/resource`.
  The CLI is preferred because it preserves unrelated config.

---

## 4. Can V1 and V2 share one config directory?

**Yes for the directory; no for a converted file.** The migration guide's rule is precise: "V1 and V2
use the same configuration locations, so do not point V1 at files after converting them to native
V2-only shapes." V1 and V2 are not installed side by side by default anyway (the V2 curl installer
replaces the V1 binary), so this is a rollback-safety question, not a concurrency one.

Sharing works as long as the file stays in V1-parseable form, because:

- **V2 reads V1 keys natively** (§1.3, and `config/normalize.ts` covers `permission`, `plugin`, `agent`,
  `mode`, `small_model`, `provider`, `command`, `reference`, `autoupdate`, `autoshare`, `snapshot`,
  `tools`, `attachment`, `skills:{paths,urls}`, flat `mcp`, boolean `formatter`/`lsp`).
- **V1 ignores V2-only keys** rather than failing: it decodes with
  `onExcessProperty: "ignore"` (`packages/opencode/src/config/parse.ts:40-44` @ HEAD).

The hazard is that "ignore" is *silent and total per key*, with two distinct failure modes once you
convert:

1. `permission` → `permissions`: V1 sees no `permission` and no `tools`, so **every** permission falls
   back to V1 defaults. Your carefully-built 40-entry `bash` allowlist silently evaporates.
2. `mcp` → `mcp.servers`: V1's `mcp` value union is `ConfigMCPV1.Info | {enabled: boolean}`, and every
   `ConfigMCPV1` variant requires a literal `type` (`packages/core/src/v1/config/mcp.ts:6-15,43-50`).
   `mcp.servers.context7` has neither `type` nor `enabled`, so the decode throws `InvalidError` — and
   `loadGlobal` catches it with `Effect.logError("failed to load global config, using defaults")` and
   `orElseSucceed(() => ({}))` (`packages/opencode/src/config/config.ts:293-300` @ HEAD). The **entire
   global config is discarded**: no model, no permissions, no agents. That is a loud-in-the-log,
   silent-to-the-user failure.

**Auto-migration / rewrite on first V2 run: none for `opencode.json(c)`.** Concretely:

- `ConfigNormalize.normalize` is in-memory only. The only file write in the config service is
  `Config.update`, whose payload schema is `{shell: string | null}` and which `jsonc-parser`-patches
  exactly that one key in the highest-precedence global `opencode.json(c)`
  (`packages/core/src/config.ts:25-27,285-305`; `PATCH /api/experimental/config`,
  `packages/protocol/src/groups/config.ts:36-49`).
- The one thing V2 *does* auto-migrate is terminal settings: `~/.config/opencode/cli.json`, built from
  legacy `tui.json` + `state/kv.json` on first V2 terminal-client start, atomically
  (`packages/cli/src/config/migrate.ts:17-121`). It leaves the V1 files alone.
- Separately, V2 **does** migrate data: the V1 session-history DB migration (resumable, with
  `GET /api/experimental/migration/v1` reporting `required|running|completed|error`, plus legacy
  credential import — `packages/core/src/database/v1-migration.bun.ts:534-…`,
  `packages/protocol/src/groups/migration.ts`). Neither touches config.

So: the safe pattern is to keep a V1-shaped `opencode.json` for as long as a V1 rollback matters, and
to convert only when you no longer need one. If you do convert, take a copy first — nothing will warn
you that V1 just went blank.

**Other shared-directory facts:** both versions read the same global dir, the same `.opencode/`
definition directories, and the same data dir (`$XDG_DATA_HOME`/`~/.local/share/opencode`,
`global-roots.ts:6-16`) — so the V1→V2 session-history migration is exactly what you'd expect.
Basic auth is unchanged: the effective username is still `"opencode"`
(`packages/server/src/auth.ts:19-22`) and `authorized()` still compares username *and* password
(`auth.ts:33-38`), so cola's existing "send `Authorization` only when both are known" behaviour stays
correct. What is new is a **service registration file** —
`$XDG_STATE_HOME/opencode/service.json` (default `~/.local/state/opencode/service.json`) holding
`{id?, version?, url, pid, password?}` at mode `0600`
(`packages/cli/src/services/service-registration.ts:12-49`,
`packages/cli/src/services/service-config.ts:29-38`,
`packages/client/src/effect/service.ts:167-175`). That is a far better discovery surface than
`/proc` scanning, and the default port is channel-derived rather than 4096
(`service-config.ts:34-38`) — flagged here because it is adjacent to cola's
`src/bridge/discovery.rs`, not because it is a config question.

### OpenChamber v2.0.2, on live-applying settings

OpenChamber cut over to OpenCode 2.x in one change (PR #3837, 2026-09) and its own skill
(`.agents/skills/opencode-v2/SKILL.md`) says "1.x code paths are gone". Relevant to this ticket:

- **Everything is live except three things.** "Plugins OpenChamber generates for OpenCode … are
  declared through the watched `<dataDir>/opencode.managed.json` (`OPENCODE_CONFIG`), so a settings
  change applies without a restart. Only the binary, port and external toggle restart."
  `persistSettings` rewrites that file (temp + rename) and "OpenCode reloads within a couple of
  seconds; no restart is involved"
  (`packages/web/server/lib/opencode/DOCUMENTATION.md`, "Managed OpenCode config layer").
- `POST /api/config/reload` still exists but is now a *manual recovery* / restart request, not
  something config edits need: "Config edits no longer need it; it stays for the changes that cannot
  be hot-applied (OpenCode binary, port, managed/external switch)" (same file, reload section).
- **It rewrites your config in place, to V2 shape.** "Writers emit v2 only. **Files are never moved.**
  Updating an entity that lives in a v1 file rewrites it at its own path in v2 shape, and a v1 JSON
  entry moves to the v2 section key inside the same file — unrelated v1 siblings are left alone. Every
  mutation response reports the `path` that changed." (same file, "v1 read fallback policy"). So if
  you ever let OpenChamber's Settings UI save an agent, provider, MCP server, plugin or permission that
  lives in `~/.config/opencode/opencode.json`, that section becomes V2 shape **in your V1 file** — and
  per §4 a `mcp`→`mcp.servers` rewrite is precisely the case that makes V1 discard the whole file.
  Practical rule: treat "OpenChamber edited my opencode.json" as a one-way door for V1.
- It reads the v1 spellings as a fallback (`agents`/`agent`, `commands`/`command`,
  `providers`/`provider`, `mcp.servers`/`mcp.<name>`, `plugins`/`plugin`, `permissions`/`permission`),
  matching V2's own precedence, and it writes only to the V2 locations
  (`.opencode/agents/`, `commands/`, `skills/`, `plugins/`).
- Its two v1-shaped sharp edges, both already handled in its code, are worth copying as a checklist:
  an agent markdown file may not mix native and legacy frontmatter keys; and a v1 provider entry
  rewrite drops fields V2 accepts but ignores (model `reasoning`, `attachment`, non-`deprecated`
  `status`, unknown custom keys), mapping `interleaved` → `compatibility.reasoningField` and a
  theme-name `color` → `#aaaaaa`.

---

## 5. Open questions

1. **Does the Code Mode default break cola's tool rendering, and where is the boundary?** V2 groups MCP
   tools under `tools.<server>.<tool>(…)` by default (`mcp.ts:27-28`), so `codegraph`'s tools arrive
   with different names and shapes than V1's flat `codegraph_*`. Unverified against a running V2
   server; needs a live probe of the tool-call payload cola renders. This is the highest-value
   follow-up and probably belongs on a sibling ticket.
2. **Will `context7` demand OAuth in V2?** `mcp/index.ts:163-205` registers an OAuth integration for
   every remote server unless `oauth: false`, and the inline comment only *asserts* that anonymous
   servers never use the method. Confirm against the live server (`opencode mcp list` → `connected`
   vs `needs_auth`).
3. **Exact merge order inside `fs.up`.** The claim "direct files farthest-first, then `.opencode`
   dirs, `.opencode` always above direct" is documented
   (`https://opencode.ai/v2/docs/config`) and consistent with `discovery.ts`'s `.toReversed()` calls,
   but `FSUtil.up`'s own ordering is not asserted in the file. Worth a two-directory fixture test
   before relying on it for precedence-sensitive config.
4. **Which top-level keys are silently inert in V2?** `lsp` and `instructions` are parsed and kept but
   never executed, and `share`/`username` are accepted but not honoured (docs). If cola surfaces any
   of these, they become dead UI. Not verified exhaustively against `packages/core/src/**` for further
   accepted-but-unused fields.
5. **Diagnostics delivery.** Config normalisation diagnostics are `Effect.logWarning` only
   (`config.ts:96-121`) — no event, no API surface. A client that wants to show "this key was dropped"
   must scrape the log (`role=server`) or diff `GET /api/config` (which returns the *normalised*
   `Config.Entry[]`, `packages/protocol/src/groups/config.ts:7-19`) against the file on disk. Whether
   cola should do the latter is undecided.
6. **Is `Config.compatibility()` really skills-only?** `config.ts:30-32` and
   `config/plugin/compatibility.ts` say yes, and I found no other consumer — but I did not exhaustively
   trace whether `~/.claude/agents/*.md` or `CLAUDE.md` is read through some other path in v2.0.18.
7. **Stale in-tree docs.** `packages/web/src/content/docs/*.mdx` at v2.0.18 still document V1
   (`permission`, `tools`, `tui.json`, `/etc/opencode` managed config, `.mobileconfig`). The live
   `/v2/docs/` are correct. If the v2 line keeps shipping those files, they are an active trap for
   anyone — including an agent — told to "read the docs in the repo".

---

## Appendix — reproduce

```bash
OC=/root/workspace/dev/opencode   # read-only; never checkout
git -C $OC show v2.0.18:packages/schema/src/config.ts                 # V2 Config.Info
git -C $OC show v2.0.18:packages/core/src/config/normalize.ts          # V1→V2 in-memory normaliser
git -C $OC show v2.0.18:packages/core/src/v1/config/migrate.ts          # key-by-key conversion
git -C $OC show v2.0.18:packages/core/src/config/discovery.ts          # paths + walk
git -C $OC show v2.0.18:packages/core/src/config.ts | sed -n '190,310p'  # merge order + the only writer
git -C $OC show v2.0.18:packages/protocol/src/groups/permission.ts     # permission HTTP surface
git -C $OC show v2.0.18:packages/core/src/plugin/module.ts              # plugin module contract + errors
git -C $OC show v2.0.18:packages/core/src/plugin/skill/opencode.md       # the in-product V2 guide
git -C $OC show v2.0.18:packages/schema/src/mcp.ts                      # MCP config + protocol revisions

# V1 baseline
sed -n '130,160p;250,300p' $OC/packages/opencode/src/config/config.ts
sed -n '35,65p'          $OC/packages/opencode/src/config/parse.ts     # onExcessProperty: "ignore"
```
