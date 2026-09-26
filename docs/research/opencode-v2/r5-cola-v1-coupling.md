# cola's V1 coupling inventory (ticket #358)

**Date:** 2026-09-26 · **Repo:** `/root/workspace/dev/cola` @ `a1b9aab` (main) · **Map:** #353
**Question:** enumerate every place cola depends on OpenCode's V1 shape, and classify each dependency.

## Method and sources

Two generations of the OpenCode HTTP API are mounted side by side (ADR-0053,
"Naming across generations"):

- **V1 / "legacy"** = the **unprefixed** routes (`/session`, `/permission`,
  `/question`, `/provider`, `/agent`, `/experimental/session`). Server-side these
  are the `*V1` groups under
  `packages/opencode/src/server/routes/instance/httpapi/groups/` (verified:
  `groups/session.ts:29` `const root = "/session"`, `groups/permission.ts:11`
  `const root = "/permission"`, `groups/question.ts:11` `const root = "/question"`).
  The public OpenAPI builder classifies them explicitly:
  `httpapi/public.ts:146` `if (!isV2Api) { … }` with
  `public.ts:180-181` `function isV2ApiPath(path) { return path === "/api" || path.startsWith("/api/") }`.
- **V2 / "current"** = the **`/api/*`** surface, generated from
  `packages/protocol/src/groups/*.ts` (`HttpApiEndpoint.get("session.messages", "/api/session/:sessionID/message", …)`,
  `groups/session.ts:109` `"/api/session"`, etc.).

Primary sources used, all read on 2026-09-26:

| Source | Used for |
|---|---|
| `/root/workspace/dev/opencode` @ `16c56fe5ec` | route tables, auth, CLI, data dir, schemas |
| `cola` `src/`, `.github/workflows/`, `docs/adr/`, `CONTEXT.md` | cola's side |
| `AGENTS.md` "Known pitfalls" #1–#13 | cola's own record of protocol couplings |

**Classification**

- **(a)** generic HTTP/SSE plumbing likely stable across generations
- **(b)** V1-specific payload/endpoint shape
- **(c)** V1-specific lifecycle/process behaviour

Counts: **~86 recorded dependencies — 12 (a), 55 (b), 19 (c).**

---

## 1. Inventory

### 1.1 Server discovery & lifecycle

| # | file:line | What cola assumes | Class | Notes |
|---|---|---|---|---|
| 1 | `src/bridge/discovery.rs:115-116` | A running server is a process whose `argv[0]` **contains** `"opencode"` and whose args include the exact subcommand `"serve"` | **c** | Matches `cli/cmd/serve.ts:7` `command: "serve"`. A renamed binary breaks discovery silently. |
| 2 | `src/bridge/discovery.rs:120-124` | The port is passed as `--port <n>` on the **command line** (parsed positionally) | **c** | `cli/network.ts` owns the flags; a flag rename or `--port=4096` (equals form) yields **no candidate at all** (the `?` on `parse()` drops it). |
| 3 | `src/bridge/discovery.rs:130-134` | The password arrives as `OPENCODE_SERVER_PASSWORD=` in the process env | **c** | `server/auth.ts` `EffectConfig.string("OPENCODE_SERVER_PASSWORD")`. Empty → `password: ""` → **no Authorization header** → 401 (verified: `authorized()` returns false when `password` is `None`/empty). |
| 4 | `src/bridge/discovery.rs:69-75`, `:32` | The effective basic-auth **username** is `OPENCODE_SERVER_USERNAME` else the literal `"opencode"` | **c** | Verified primary source: `server/auth.ts` `username: EffectConfig.string("OPENCODE_SERVER_USERNAME").pipe(EffectConfig.withDefault("opencode"))`. Pitfall #12. |
| 5 | `src/bridge/discovery.rs:136-148`, `:55-64` | The "Shared Store" is `$XDG_DATA_HOME` else `~/.local/share`, **on every platform** (macOS/Windows included) | **c** | Mirrors `packages/core/src/global.ts` `import { xdgData … } from "xdg-basedir"` and `packages/desktop/src/main/logging.ts` `process.env.XDG_DATA_HOME \|\| join(homedir(), ".local", "share")`. ADR-0005 + opencode #8235. A store-path change silently makes every server ineligible → cola spawns a duplicate server on the same store. |
| 6 | `src/bridge/discovery.rs:87-92` | `sysinfo` must scan with `without_tasks()`, or each thread of a server looks like a separate server | **c** | Platform-general, but exists only because the server is a multi-threaded Bun process. |
| 7 | `src/bridge/discovery.rs:170-189` | "Owned vs Coexistent" is decided by **pid identity** recorded in `~/.cola/self-opencode.pid` | **c** | ADR-0013. Depends on cola having spawned the process itself. |
| 8 | `src/bridge/discovery.rs:207-228` | Self-start is exactly `opencode serve --port <n> --hostname 127.0.0.1` with `OPENCODE_SERVER_PASSWORD` set, and `XDG_DATA_HOME` + `OPENCODE_SERVER_USERNAME` **removed** from the child env | **c** | The `env_remove` is load-bearing: an inherited `OPENCODE_SERVER_USERNAME` would make cola's client 401 against its own server. |
| 9 | `src/bridge/discovery.rs:385` | The `opencode` binary is on `PATH` and is spawned via `Command::new("opencode")` | **c** | README/`docs/user-guide.md:86` state this as a hard prerequisite. |
| 10 | `src/bridge/discovery.rs:415-432` | Default fallback port is `4096`, default self-start password is the literal `"cola-secret"` | **c** | Both are cola's own choices; only `4096` collides with OpenChamber's default. |
| 11 | `src/bridge/discovery.rs:344-378`, `:451-493` | SIGTERM (unix) / `taskkill /F` (Windows) terminates a server and its Drop handlers release the singleton lock | **c** | ADR-0021 singleton lock. |
| 12 | `src/bridge/pollers.rs:103-121` | **`opencode serve` accepts TCP before its HTTP dispatch is wired** — requests in that ~1 s window are swallowed forever | **c** | The "Lazy Start silent-hang incident". Purely a V1-server startup-order fact, encoded as a workaround. |
| 13 | `src/bridge/pollers.rs:14`, `:202` | Base URL is `http://localhost:<port>` (localhost, not 127.0.0.1) | **a** | Built by string format. |
| 14 | `src/bridge/pollers.rs:14`, `:244-258` | The attached server can be replaced/renamed at runtime by another tool, so cola re-scans every **5 s** | **a** | Cadence is a cola constant. |

### 1.2 HTTP client (transport, auth, prefixes, timeouts)

| # | file:line | What cola assumes | Class | Notes |
|---|---|---|---|---|
| 15 | `src/opencode/client.rs:21-34`, `:184-186` | `base_url` + relative path concatenation; the whole `HttpHandle` (client + URL) is swappable via `RwLock` | **a** | Generation-agnostic. |
| 16 | `src/opencode/client.rs:54-60` | Only `connect_timeout(10s)` is set — **no total request timeout**, because a prompt POST legitimately runs for minutes | **a** | A generation with a different latency profile (e.g. fully async prompts) would make this unnecessary; harmless if kept. |
| 17 | `src/opencode/client.rs:66-73` | Basic auth is attached **only when both username and password are present**; a half-credential must never reach the wire | **a** | Verified: `server/auth.ts` `authorized()` compares `credentials.username === config.username` **and** the password. This is generation-agnostic but a real upstream quirk. |
| 18 | `src/opencode/client.rs:123-137` | A serverless client has an **empty** base URL and does no requests until `reconnect` | **a** | Lazy Start (ADR-0013). |
| 19 | `src/opencode/client.rs:168-177` | `reconnect(url, password)` swaps the endpoint; the **username is preserved** from the first `with_base_url` | **a** | Deliberate: discovery found the username once. |
| 20 | `src/opencode/client.rs:83` | Permission/question **replies** answer in milliseconds, so a 1.5 s cap is enough (Feishu's ack budget is 3 s) | **a** | Applies to any generation. |
| 21 | `src/opencode/client.rs:88`, `:229-263` | The session list is paginated with an **`x-next-cursor` response header**, capped at 100 pages | **b** | Verified: the V1-only handler sets it — `httpapi/handlers/experimental.ts:154` `? { "x-next-cursor": String(list[list.length - 1].time.updated) }`. V2 `GET /api/session` returns `{ data, cursor }` in the **body** instead (see `packages/app/e2e/regression/*.spec.ts` stubs). No V2 equivalent header. |
| 22 | `src/opencode/client.rs:238-249` | A **404** on `GET /experimental/session` means "server too old" and cola falls back to project-scoped `GET /session` | **b** | The fallback is itself V1 (`/session` is project-scoped to the server's cwd). Under V2 the "old server" 404 probe would be answering a different question. |
| 23 | `src/opencode/client.rs:97-105` | A **404** on a reply endpoint means "already resolved elsewhere" → `BridgeError::NotFound` → neutral "already handled" card | **b** | Shared by the permission and question replies. V2 nests these under a session, so a gone-session 404 would be misread as "another client answered". |

### 1.3 Endpoint inventory (the core of the coupling)

Every path cola calls, with its V1/V2 status. V2 column verified from
`packages/protocol/src/groups/*.ts`.

| cola call | file:line | Generation | V2 counterpart (if any) |
|---|---|---|---|
| `POST /api/session` | `client.rs:290` | **V2** | same (ADR-0053 amendment) |
| `POST /api/session/{id}/compact` | `client.rs:756` | **V2** | same — `protocol/src/groups/session.ts:226` |
| `GET /experimental/session` (+`x-next-cursor`) | `client.rs:233` | **V1** | `GET /api/session` (body `cursor`) |
| `GET /session` (fallback) | `client.rs:244` | **V1** | `GET /api/session` |
| `PATCH /session/{id}` `{"title"}` | `client.rs:278` | **V1** | `PATCH /api/session/{id}` (not in the protocol group list I read) |
| `POST /session/{id}/message` | `client.rs:339` | **V1** | `POST /api/session/{id}/prompt` |
| `POST /session/{id}/prompt_async` | `client.rs:446` | **V1** | **no V2 equivalent found** |
| `GET /session/{id}/message` | `client.rs:548` | **V1** | `GET /api/session/{id}/message` |
| `GET /session/{id}` | `client.rs:477` | **V1** | `GET /api/session/{id}` |
| `GET /session/status` | `client.rs:573` | **V1** | **none found** |
| `POST /session/{id}/abort` | `client.rs:746` | **V1** | `POST /api/session/{id}/interrupt` |
| `GET /permission` | `client.rs:518` | **V1** (global) | `GET /api/permission/request` + `GET /api/session/{id}/permission` |
| `POST /permission/{id}/reply` | `client.rs:495` | **V1** (global) | `POST /api/session/{id}/permission/{requestID}/reply` |
| `GET /question` | `client.rs:610` | **V1** (global) | `GET /api/question/request` |
| `POST /question/{id}/reply` | `client.rs:675` | **V1** (global) | `POST /api/session/{id}/question/{requestID}/reply` |
| `POST /question/{id}/reject` | `client.rs:732` | **V1** (global) | `POST /api/session/{id}/question/{requestID}/reject` |
| `GET /agent` | `client.rs:694` | **V1** | `GET /api/agent` |
| `GET /provider` | `client.rs:638`, `:714` | **V1** | `GET /api/provider` |
| `?directory=` on every instance-routed call | `client.rs:479`, `:497`, `:520`, `:575`, `:612`, `:677`, `:734` | **a** (routing) | `?location` + `GET /api/location` (V2 has a `location` group) |
| **no** model-switch endpoint (model sent per prompt) | `client.rs:335`, `parsing.rs:142-157` | **b** | V2 *has* `POST /api/session/{id}/model` (`protocol/src/groups/session.ts:189`) — see pitfall #4 |

So of **19 route families cola touches, 15 are V1, 2 are V2, 2 are
generation-neutral routing.**

### 1.4 Session / message / part read model

| # | file:line | What cola assumes | Class | Notes |
|---|---|---|---|---|
| 24 | `src/opencode/wire/legacy.rs:29-34` | A V1 message is an envelope **`{info, parts}`** | **b** | `WireSessionMessage`. V2 `GET /api/session/{id}/message` returns a different envelope. |
| 25 | `src/opencode/wire/legacy.rs:36-50` | `info` carries `id`, `role`, `time`, **`modelID`**, **`providerID`**, `tokens` (all camelCase) | **b** | Pitfall #2. `#[serde(rename)]` omissions silently null fields. |
| 26 | `src/opencode/wire/legacy.rs:52-71` | Usage lives at `info.tokens.{input,output,total,cache.{read,write}}` | **b** | Powers the card footer's 📊. |
| 27 | `src/opencode/wire/legacy.rs:73-80` | `time.{created, completed}` in **epoch millis**; `completed` absent while in flight | **b** | The absence is load-bearing for turn membership (below). |
| 28 | `src/opencode/wire/legacy.rs:144-177` | Part `type` vocabulary is exactly `text, reasoning, tool, step-start, step-finish, patch` (+ anything else → `Other`) | **b** | Verified upstream `packages/schema/src/v1/session.ts:360-366` `Part = Union([TextPart, SubtaskPart, ReasoningPart, FilePart, ToolPart, StepStartPart, StepFinishPart, SnapshotPart, PatchPart, AgentPart, RetryPart, CompactionPart])` — 12 kinds, of which cola models 6. The 6 unmodelled kinds include **`SubtaskPart`**, which V2 has a first-class session type for. |
| 29 | `src/opencode/wire/legacy.rs:179-193` | A tool part is `{type:"tool", tool, callID, state}`; correlation is **`callID`**, falling back to the tool name | **b** | Verified: `schema/src/v1/session.ts:317-323` `ToolPart = Struct({ …partBase, type: "tool", callID: Schema.String, tool: Schema.String, state: ToolState, metadata: … })`. |
| 30 | `src/opencode/wire/legacy.rs:184` | Tool lifecycle is `state.status ∈ {pending, running, completed, error}` | **b** | Verified: `schema/src/v1/session.ts:305-313` `ToolState = Union([ToolStatePending, ToolStateRunning, ToolStateCompleted, ToolStateError], { discriminator: "status" })`. Drives `⏳/✅/❌` (`feishu/card/tool_render.rs:55-69`). |
| 31 | `src/opencode/wire/legacy.rs:206-244` | Tool output precedence: `state.output` (string) → `state.content[].text` + `state.result` → `state.metadata.output` | **b** | Four V1 field names in one precedence ladder; `error_suppresses_fallback` (`wire/mod.rs:167-169`) suppresses the last rung for `status:error`. |
| 32 | `src/opencode/wire/legacy.rs:162-163` | `step-finish.reason ∈ {tool-calls, stop, length, content-filter, error}`; terminal-ness drives turn completion | **b** | `wire/mod.rs:82-92`; `backend/transcript.rs:51-68` `turn_for_user` sets `complete` only on a terminal reason. |
| 33 | `src/opencode/wire/legacy.rs:85-86`, `wire/mod.rs:46-48` | **Part payloads carry NO `id`** — dedup must key on content | **b** | Pitfall #9. Verified upstream: `core/src/session/sql.ts:20` `type V1PartData = Omit<SessionV1.Part, "id" \| "sessionID" \| "messageID">` and `projector.ts:85` `const { id: _, messageID: __, sessionID: ___, ...rest } = part` — `id` is a DB column, not part data. |
| 34 | `src/bridge/turn/render.rs:126-202` | Text/reasoning dedup by **exact content**; a tool re-renders when its typed panel revision changes; `todowrite` is special-cased by name and clock | **b** | The direct consequence of #33. `:135`, `:145-149`, `:165-172`. |
| 35 | `src/backend/transcript.rs:51-68`, `:105-110` | Turn membership: an **in-flight** message (no `completed`) always belongs; a completed one belongs iff `created >= anchor` or `completed >= anchor` | **b** | #190/#310 semantics. Depends on the `completed` field existing at all. |
| 36 | `src/backend/transcript.rs:129-137` + `turn/render.rs:218-243` | A turn's **anchor** is the cola-chosen `msg_cola_…` message id **plus its server `time.created`** | **b** | Two V1 facts at once: server-controlled identity + server time. |
| 37 | `src/opencode/parsing.rs:12-28`, `:171-175` | cola picks the user message id as `msg_cola_<uuid32>` and sends it as **`messageID`**; the server persists it and re-posting is idempotent | **b** | ADR-0026, verified live 2026-09-09. **Note:** ADR-0026 records the server validating a `msg` prefix; today's V2 schema is stricter — `packages/schema/src/session-message.ts:12` `Schema.String.check(Schema.isStartsWith("msg_"))`. `msg_cola_` satisfies both, so cola is currently safe, but the prefix is now load-bearing against a *tighter* rule. |
| 38 | `src/bridge/external.rs:161-196` | Authorship is the `msg_cola_` prefix; the Sync Watermark is `info.time.created` of the newest user message | **b** | ADR-0026/0017. |
| 39 | `src/opencode/client.rs:545-554` | The transcript read sends **no `limit`/`before`**, so the server returns the **whole** history array each poll | **b** | Verified: `httpapi/handlers/session.ts:119-121` `if (ctx.query.limit === undefined \|\| ctx.query.limit === 0) { return … session.messages({ sessionID }) }`. At a 1.5 s poll this is O(history) per tick. |
| 40 | `src/opencode/parsing.rs:34-69` | `GET /provider` returns `{all:[{id, models:{…}}], connected:[ids]}`; only connected providers surface | **b** | `connected` is the field that makes the picker usable; its absence keeps everything (older-server arm). |
| 41 | `src/opencode/parsing.rs:76-91` | `model.variants` is a **Record keyed by variant id**; very old servers sent an array of `{id}` | **b** | Both shapes accepted. |
| 42 | `src/opencode/client.rs:633-665` | Context window lives at `all[].models[<id>].limit.context` | **b** | Footer ratio. |
| 43 | `src/opencode/client.rs:568-603` + `parsing.rs:111-118` | `GET /session/status` returns `Record<sessionID, {type: "idle"\|"busy"\|"retry"}>`; **an absent session means Idle** | **b** | Load-bearing: drain end, follow liveness, snapshot status line, heal. A V2 shape change here silently turns "idle" into "unknown" → `Ok(None)` → the drain never ends. |
| 44 | `src/opencode/types.rs:21-36`, `:109-129` | `parentID` marks a `task`-tool child; `Session.Info.model = {id, providerID}` is rung 3 of the `/think` ladder | **b** | `handles.rs:274-302` `effective_model`. |
| 45 | `src/opencode/types.rs:38-57` | `time.archived` marks an archived session; children are excluded from `/switch` | **b** | ADR-0008. |
| 46 | `src/opencode/types.rs:13-16` | `POST /api/session` replies **`{data: Session}`** (V2 envelope) | **b** | The only V2 envelope cola parses. |
| 47 | `src/opencode/types.rs:146-170` | Session creation takes `location.directory` (not `directory`) — a **V2-only** field | **b** | Verified: `wire_tests` assert `body["location"]["directory"]` (`client.rs:1272`). V1 create takes `directory` at the top level. |
| 48 | `src/opencode/types.rs:226-250` | `PermissionRequest = {id, sessionID, permission, patterns, metadata, always}`; `QuestionRequest = {id, sessionID, questions[{question, header, options, multiple, custom}]}` | **b** | Drives the permission/question card bodies. |
| 49 | `src/opencode/parsing.rs:97-107` | Prompt parts are `[{type:"text",text}, {type:"file",mime,url:"data:…"}]` | **b** | `FilePartInput` with a data URL. |
| 50 | `src/opencode/parsing.rs:142-157`, `:162-166` | Prompt body carries `model{providerID,modelID}`, `variant`, `agent` as **top-level** keys | **b** | Verified: `groups/session.ts:70` `PromptPayload = Schema.Struct(Struct.omit(SessionPrompt.PromptInput.fields, ["sessionID"]))` — flat. |
| 51 | `src/opencode/client.rs:354-379` | Prompt response is `{info:{id,parentID,error}, parts}`; the error message is at `info.error.data.message` **or** `info.error.message` | **b** | V2's error union is a *discriminated* `name` union (`schema/src/v1/session.ts:381-391` `AssistantErrorSchema = Union([AuthError, UnknownError, OutputLengthError, AbortedError, StructuredOutputError, ContextOverflowError, ContentFilterError, APIError], {discriminator:"name"})`). |
| 52 | `src/opencode/client.rs:401-403`, `:454-456` | **404** on prompt/prompt_async ⇒ the session is gone ⇒ `SessionNotFound` ⇒ recreate | **b** | Pitfall #7. A 404 with a different meaning would trigger spurious session recreation. |
| 53 | `src/feishu/card/tool_render.rs:123-132` | A `task` tool's child session id is `state.metadata.**sessionId**` | **b** | camelCase inside a V1 tool `metadata` record; the exact field that drives the whole sub-task liveness feature (ADR-0054). |
| 54 | `src/feishu/card/tool_render.rs:101`, `:649-651`, `:675-708` | Tool names `edit`, `apply_patch`, `task`, `skill`, `todowrite`, `websearch`, `read` and their **output text grammars** (`<task …>`, `<skill_content …>`, `<path>`, `JSON.stringify(todos)`, websearch result envelope) | **b** | Presentation-only, but every one is a V1 built-in tool contract. |
| 55 | `src/feishu/card/tool_render.rs:544-590` | `todowrite` payload is `{todos:[{content, status}]}` with status ∈ `in_progress/pending/completed/cancelled` | **b** | `TODO_STATUSES`. |
| 56 | `src/feishu/card/tool_render.rs:916-1050` | Per-tool input field names: `command`, `workdir`, `description`, `filePath`, `pattern`, `include`, `offset`, `limit`, `url`, `subagent_type`, `questions[].question` | **b** | The largest single block of V1 tool-contract knowledge outside the adapter (see the ADR-0053 out-of-scope exception). |
| 57 | `src/bridge/request/kind.rs:286`, `:311`, `:656` + `src/feishu/card/question.rs:41-43` | Permission replies are the literals **`"once"` / `"always"` / `"reject"`** | **b** | Verified: `packages/schema/src/v1/permission.ts:38` `Reply = Schema.Literals(["once", "always", "reject"])`. An unmatched literal is a 4xx. |

### 1.5 Prompt dispatch

| # | file:line | What cola assumes | Class | Notes |
|---|---|---|---|---|
| 58 | `src/opencode/client.rs:321-418` | `POST /session/{id}/message` is **synchronous**: it returns only when the turn is done, with the full assistant message inline | **b** | This is the shape the whole Turn lifecycle is built around (block, then drain by polling). A V2 `/api/session/{id}/prompt` that returns early would invert cola's phases. |
| 59 | `src/opencode/client.rs:406-417` | There is deliberately **no fallback** to the older prompt route: a retry reuses the same `messageID` and the server dedupes | **b** | ADR-0026. Correctness depends on server-side id dedupe, which is a V1-store behaviour. |
| 60 | `src/opencode/client.rs:428-467` | `POST /session/{id}/prompt_async` **persists the user message and forks a run**, returning 204 | **b** | Verified: `handlers/session.ts:311-329` `promptSvc.prompt(…).pipe(Effect.forkIn(scope, { startImmediately: true })); return HttpApiSchema.NoContent.make()`. The supplement path depends on the message landing *before* the next tool boundary. |
| 61 | `src/bridge/core.rs:47`, `src/bridge/turn/mod.rs:30`, `src/bridge/external.rs:44` | Per-call timeouts (3 s session-info, 30 s drain/external read) are sized against a **localhost V1 server** that answers in ms | **a** | Tuning, not shape. |
| 62 | `src/bridge/core.rs:227`, `src/bridge/turn/follow.rs:84`, `src/opencode/client.rs:233` | Rendering is **polling only** at 1.5 s; the render cadence is the primary latency/cost knob | **a** | ADR-0011: the SSE fold is dead code. |

### 1.6 Events & polling

| # | file:line | What cola assumes | Class | Notes |
|---|---|---|---|---|
| 63 | `docs/adr/0011-…md:1-23` | The global SSE is **heartbeat-only** on this store; the server ends it every few seconds. `OpenCodeEvent` + the parallel event→card fold were deleted | **b** | The consequence, not the cause: **cola has no event subscription at all**. There is nothing to migrate; the cost is that every live fact is a poll. |
| 64 | `docs/adr/0001-…md` (SSE bullet) | V1 `session.next.*` streaming events are dropped by the server's `event.location?.directory === instance.directory` filter, and `permission.*` is PubSub-only | **b** | The stated reason cola polls permissions instead of streaming them. |
| 65 | `src/bridge/pollers.rs:313-339` | A `task` child session can be resolved by walking `parentID` up to **8 hops**, with a stop-on-self-parent rule, and **the walk is impossible without a directory** (`None` ⇒ start session only) | **b** | `walk_parent_chain`; ADR-0010 makes the directory mandatory for the hop. |
| 66 | `src/bridge/pollers.rs:390-428`, `:436-453` | A permission/question card for a child session is delivered to the chat mapped to the nearest **mapped ancestor**, carrying `directory` so the reply routes correctly | **b** | Pitfall #11. |

### 1.7 Permissions & questions

| # | file:line | What cola assumes | Class | Notes |
|---|---|---|---|---|
| 67 | `src/opencode/client.rs:510-537` | Permissions are listed **globally** (`GET /permission`) and must be scoped with `?directory=` or the server checks only its cwd instance | **b** | Pitfall #3. V2 makes them **session-scoped** (`/api/session/{id}/permission`) — a structural change: cola's poller is instance-driven, V2's is session-driven. |
| 68 | `src/opencode/client.rs:485-508` | The reply is `POST /permission/{id}/reply?directory=` with body `{"reply": …}` | **b** | Verified `schema/src/v1/permission.ts:41-44` `ReplyBody = Struct({ reply: Reply, message: optional(String) })`. |
| 69 | `src/opencode/client.rs:605-629`, `:667-688`, `:726-739` | Questions mirror permissions: global `GET /question`, `POST /question/{id}/reply` `{answers}`, `POST /question/{id}/reject` | **b** | Same structural change under V2. |
| 70 | `src/bridge/discovery.rs:390-392` | cola's self-started server must keep the **question tool enabled** (it does not pass any flag to disable it) | **c** | Explicitly reasoned in the source: disabling it would make the question cards unreachable. |
| 71 | `src/bridge/request/kind.rs:289-323`, `:623-677` | Auto-Accept answers with `"once"`; `always` is a *backend* per-type rule cola does not implement | **b** | CONTEXT.md "Auto-Accept" glossary. |

### 1.8 Session store & model handling (cola-side, but the ladder reads the server)

| # | file:line | What cola assumes | Class | Notes |
|---|---|---|---|---|
| 72 | `src/config.rs:256-298` | The persisted `SessionEntry` mirrors server facts: `directory`, `agent`, `model` ("provider/model"), `variant`, plus cola-only `auto_accept`/`topic_anchor`/`topic_root` | **b** | `directory` is copied from `Session.Info`; `model`/`variant`/`agent` are re-sent per prompt because the V1 server has no setter. |
| 73 | `src/bridge/handles.rs:274-302` | Effective model ladder: override → `[opencode] model` → **`Session.Info.model`** from `GET /session/{id}` | **b** | A V2 read that drops `model` from session info degrades the `/think` card to "pick a model first". |
| 74 | `src/bridge/handles.rs:323-334` | A `variant` not declared by the model makes the server fail with `VariantUnavailableError` | **b** | Verified: `core/src/session/runner/model.ts:41-42` `VariantUnavailableError`. |
| 75 | `src/opencode/parsing.rs:125-135` | `provider/model` splits on the **first** `/` (model ids may contain slashes) | **b** | V1 model-id grammar. |
| 76 | `src/bridge/session.rs` (whole file) | The SessionStore is pure cola state (no wire types) | **a** | Good news: this is the one big store with **no** V1 coupling. |

---

## 2. The riskiest assumptions, ranked

Ranked by (blast radius when the assumption breaks) × (likelihood the assumption is wrong in a V2 world).

**R1 — The transcript read shape: `GET /session/{id}/message` → `{info, parts}` → `type: text|reasoning|tool|step-start|step-finish|patch` with `callID`/`state.status`.**
`src/opencode/wire/legacy.rs:29-244` feeds *everything user-visible*: the streaming card, the final render, the external-message sync, the session snapshot, sub-task liveness, token usage, the context footer. ADR-0053 confines the field names to this one module — but the **semantics** leak upward: `turn_for_user` (which messages belong to a turn), the in-flight/completed distinction, and the content-based dedup that exists *only* because parts have no `id`. A generation switch here is not an adapter edit; it is a re-derivation of turn membership and dedup. This is the single highest-risk coupling, and ADR-0053's 2026-09-26 amendment is explicit that the message stores do not project into each other, so it cannot be switched alone.

**R2 — Permissions and questions are global-and-instance-scoped, not session-scoped.**
`client.rs:518`, `:610` vs `request/kind.rs:267-275` and the whole poller architecture. V2 nests both under a session (`/api/session/{id}/permission`, `/api/session/{id}/question/…`). That inverts the poller's driver from "per known directory" to "per known session" — a different iteration domain, a different 404 semantics, and it invalidates the `?directory=` instance-routing story that ADR-0010 is built on. Pitfall #3 records that the *old* per-session endpoint returned empty, so this has already burned once.

**R3 — Permissions are polled, never streamed, because `permission.*` never reaches an SSE and the global SSE is heartbeat-only.**
`docs/adr/0001` SSE/permission bullets, `docs/adr/0011`. If a generation delivers permissions as events, the poller becomes redundant — but until then cola cannot tell a *live* permission from a 3 s-stale one, and the card latency is bounded by the poll.

**R4 — `GET /session/status` absent ⇒ Idle, and `type ∈ {idle,busy,retry}`.**
`client.rs:568-603`, `parsing.rs:111-118`. Four separate behaviours hang on it: the post-prompt drain ending, the out-of-turn follow liveness, the session snapshot's status line, and the "server died mid-turn" heal. A silently-unrecognised `type` returns `Ok(None)` (never guessed) and every one of those degrades to a stuck/incorrect state rather than an error. Small code, enormous fan-out.

**R5 — cola's self-identifying `msg_cola_` message id is accepted, persisted, and idempotent.**
`parsing.rs:12-28`, `:171-175`, `client.rs:334`. Three separate guarantees: (i) the id is *valid* — V1 needs a `msg` prefix, today's V2 schema needs `msg_` (`packages/schema/src/session-message.ts:12`); (ii) the server *persists* it as `info.id`; (iii) re-posting the same id does not duplicate the user message. Break (i) → every prompt 4xx. Break (ii) → the turn anchor never captures → **the card renders nothing at all** (`turn/render.rs:263-267` returns `false` with no anchor). Break (iii) → a retry duplicates the message and re-runs the model. ADR-0026 verified (i)–(iii) live on 2026-09-09, but the *rule* is newer and tighter than cola's note records.

**R6 — Discovery keys on the `opencode serve` cmdline, `OPENCODE_SERVER_PASSWORD` in the env, and an XDG data home.**
`discovery.rs:115-148`, `:207-228`. A cmdline-flag change, an equals-form `--port=`, a password delivered by config file instead of env, or a store-path change each fail **silently** — the scan returns nothing, so cola concludes "no shared server" and **spawns a second server against the same store**, which is precisely the two-servers-one-store condition ADR-0013 was written to prevent. There is no positive assertion that anything was found.

**R7 — Basic auth needs the username as well as the password, and the username default is `"opencode"`.**
`client.rs:66-73`, `discovery.rs:32`, `:69-75`. Pitfall #12, verified against `server/auth.ts`. Classified (a) because it is generation-agnostic, but it is the difference between a working bridge and a silent 401 on every call, and the failure mode (no header at all when either half is missing) is a *silent* degradation.

**R8 — `GET /experimental/session` + `x-next-cursor` pagination, with a 404-triggered fallback to project-scoped `GET /session`.**
`client.rs:229-263`. The `x-next-cursor` header exists only on the V1 experimental handler; V2 paginates in the body. And the fallback (`/session`) is itself V1 and project-scoped — so on a V2 server the 404 probe would return empty rather than 404, and cola would silently show a stale, wrong-project list (the exact bug the primary route was added for, issue #325).

**R9 — `POST /session/{id}/message` is synchronous and `POST /session/{id}/prompt_async` returns 204 after forking.**
`client.rs:321-467`, `:420-426`. The Turn's `start → attempt → finish` phases, the 10-minute drain budget, and the supplement-merge path all assume these semantics. A V2 prompt that returns early (`/api/session/{id}/prompt`) would need `wait`/`events` to finish a turn — cola has no such mechanism (see R1/ADR-0011).

**R10 — Tool identity/correlation is `callID`, status is `state.status`, output lives in a four-deep precedence ladder.**
`wire/legacy.rs:179-244`, plus the built-in-tool contract surface in `feishu/card/tool_render.rs:101-1050`. The ADR-0053 "out-of-scope exception" (`backend/mod.rs:1-11`, `wire/mod.rs:19-23`) means the *public* `opencode::types` DTOs still spell wire fields, and the biggest remaining block of raw V1 field access is in the **Platform**, not the adapter. This is the one place the ADR-0053 boundary is knowingly porous.

---

## 3. Couplings outside the server API

### 3.1 CLI pins in CI

| Where | Pin | Class | Notes |
|---|---|---|---|
| `.github/workflows/release.yml:214-224` | Downloads `opencode-linux-x64-baseline.tar.gz` from `v1.18.31` + hardcoded `SHA256: b283e8db…` | **c** | Deliberate: the comment says `anomalyco/opencode/github` installs from `releases/latest` at run time, so pinning the action would not pin the code. Version, URL and digest must move together. |
| `.github/workflows/release.yml:226-247` | `OPENCODE_PERMISSION` is a JSON object of tool→`"deny"`/`"allow"` | **b** | A **CLI env-var contract**, not the HTTP API. A rename of any of `edit, bash, read, grep, glob, list, webfetch, task, todowrite` silently stops denying. |
| `.github/workflows/release.yml:290-297` | `opencode run --model opencode-go/deepseek-v4.1-flash --format json "$PROMPT" --file=…` | **c** | Subcommand + flag grammar. |
| `.github/workflows/release.yml:302-345` | The embedded Python parses stdout as **JSONL events** and keys on `event.type == "text"`, `part.time.end`, and dedupes by `part.text` **content** | **b** | The workflow's own comment restates pitfall #9 — "the API's part payloads carry no serialized id". Two independent implementations of the same V1 assumption (Rust + Python). |
| `.github/workflows/opencode-review.yml:54-66` | Same 1.18.31 pin + digest | **c** | |
| `.github/workflows/opencode-review.yml:82-109` | `OPENCODE_PERMISSION` with a **nested** `bash: {"*":"deny", "git diff*":"allow", …}` allowlist, plus `external_directory` and `read` entries | **b** | The nested-glob shape is a V1 CLI permission-schema contract. A schema change here fails **open** (asks hang to the 20-min timeout) or fails closed (review never runs) — both visible in CI. |
| `.github/workflows/opencode-review.yml:136` | `opencode github run` (subcommand) with `USE_GITHUB_TOKEN`, `MODEL`, `PROMPT` env | **c** | The subcommand asserts the triggering actor is a collaborator — the workflow skips Dependabot because of it (`opencode-review.yml:27-29`). |
| `.github/workflows/opencode-review.yml:158-160` | The verdict is read back out of a **bot comment body** by regex | **b** | Couples to whatever the CLI's GitHub-integration output format is. |

### 3.2 Runtime CLI

| Where | Coupling | Class |
|---|---|---|
| `src/bridge/discovery.rs:385` | `Command::new("opencode")` + `serve --port --hostname` (see rows 1, 2, 8, 9) | **c** |
| `docs/user-guide.md:86`, `README.md:86` | Documented hard prerequisite: an `opencode` binary on `PATH` | **a** |

### 3.3 Explicitly **not** coupled

- `src/update.rs` — cola's self-update reads only `https://github.com/SXongin/cola/releases/latest` (`update.rs:170`, `:216-249`). No OpenCode version check anywhere. Verified by grep.
- `src/version.rs`, `build.rs` — cola's own identity, no OpenCode version embedded.
- `fuzz/` — the only fuzz target is `pbbp2` (Feishu WS). There is **no** fuzz target over the OpenCode wire decoders, so `wire/legacy.rs` is protected only by unit tests.
- `xtask/` — `cargo`/`git`/`gh`/`cargo deny` only.

---

## 4. Test surface encoding V1 shapes

| Surface | file:line | What it pins | Class |
|---|---|---|---|
| Fake HTTP server | `src/test_http.rs:1-8`, `:19-35`, `:62-84`, `:99-132` | Route table keyed by **method + path prefix**, records method/path/query/headers/body; `x-next-cursor` supported via `MockResponse::header` (`test_http.rs:47-53`). ADR-0031. | **a** (the mechanism); the route strings it is fed are (b) |
| Create-session wire test | `client.rs:1227-1272` | `POST /api/session` and `body["location"]["directory"]` | **b** (V2) |
| Session-list wire tests | `client.rs:1280-1404` | `/experimental/session` with `x-next-cursor` paging, plus the 404 → `/session` fallback order | **b** |
| Title / status / permission / question wire tests | `client.rs:1410-1417`, `:1465-1516`, `:1528-1591`, `:1610-1717` | `PATCH /session/{id}`; `/session/status`; `GET /permission`; `POST /permission/{id}/reply`; `GET /question`; `POST /question/{id}/reply`; `…/reject` — each asserting both the recorded path and `?directory=` | **b** |
| Prompt wire tests | `client.rs:935-1040`, `:1158-1191` | `POST /session/{id}/message` and `/prompt_async`; asserts the **absence** of `model` when unset (`client.rs:1069`) and of `id` in the create body (`:1268`) | **b** |
| Transcript wire test | `client.rs:1785-1795` | `GET /session/{id}/message` | **b** |
| Legacy decoder fixtures | `wire/legacy.rs:251-594` | The richest V1 fixtures in the repo: `{info:{id,role,time,modelID,providerID,tokens}, parts:[…]}`, tool `state` with `status`/`output`/`content`/`result`/`metadata.output`, `callID` | **b** |
| Session/permission/question DTO fixtures | `opencode/types.rs:252-318` | camelCase `parentID`, `time.*`, `projectID` | **b** |
| `GET /provider` fixtures | `parsing.rs:221-306` | `{all, connected}`, Record **and** array `variants` | **b** |
| Test bridge (`MockBackend`) | `bridge/test_support.rs:1034-1043`, `:2536-2594` | Scripts the **neutral** `SessionTranscript` — deliberately *no* V1 shapes (ADR-0053 / spec #332) | **a** |

**Net test-surface finding:** the bridge-level tests are generation-clean (a real
achievement of ADR-0053). The V1 encoding is now concentrated in exactly two
places — `src/opencode/wire/` and `src/opencode/client.rs`'s wire tests — which
is the right shape for a future migration. `feishu/card/tool_render.rs` is the one
documented exception (ADR-0042/0053).

---

## 5. Documented V1 assumptions in the ADRs and CONTEXT.md

| Doc | Recorded assumption | Still true? |
|---|---|---|
| ADR-0001 (banner: *Amended by ADR-0053*) | "Canonical API paths, no `/api` prefix" — **superseded wording**; today the unprefixed routes *are* the V1 compatibility surface | Body is history; the banner is correct |
| ADR-0001 | Global SSE is heartbeat-only; v1 `session.next.*` filtered out for lacking `location`; permissions polled every 3 s with `?directory=` | Unverified in this pass; consistent with ADR-0011 |
| ADR-0005 | Discovery mirrors `xdg-basedir` on every platform (opencode #8235) | **Confirmed** — `core/src/global.ts` |
| ADR-0007 (banner) | Server is the single source of truth for session identity; `GET /session`, `GET /session/{id}`, `PATCH /session/{id}` are "normalized, canonical APIs" | Path list is V1 |
| ADR-0010 | `?directory=` is the leak; instance routing moved into a `DirectoryBackend` handle | Still the shape; V2 has a `location` concept instead |
| ADR-0011 | SSE fold is dead code, to be deleted not maintained; a future SSE effort must start from `render.rs` | Still true — nothing has revived it |
| ADR-0013 | Selection priority, yield, Lazy Start, `auto`/`never`/`eager` | Holds; the yield's busy-detection gap is documented there |
| ADR-0017 | External sync follows the Active Session; watermark cleared on deactivation | Holds (cola's own state, not V1) |
| ADR-0022 | Switch-card directory scoping; children/archived predicates (`parentID`, `time.archived`) | Predicates are V1 fields |
| ADR-0026 (banner) | `msg_cola_` prefix; server persists it; `POST /api/session/{id}/prompt` fallback **removed** because it "appends a fresh user message and re-runs the model"; ids "need only start with `msg`" | All confirmed; the id-prefix rule is now `msg_` in V2 |
| ADR-0050 | Turn is a deep module; render poll is internal | No V1 content |
| ADR-0053 (incl. 2026-09-26 amendment) | Neutral read model; the `/api` transcript decoder was **added then deleted**; "the message stores do not project into each other", so switching "would have to be a coordinated migration with the other client" | The load-bearing constraint for R1 |
| CONTEXT.md "Event" | "A typed protocol message from the Backend via SSE. Drives Card state transitions." | **Stale** — contradicts ADR-0011; nothing consumes SSE |
| CONTEXT.md "Cola-Authored Message" / "Sync Watermark" | `msg_cola_` id prefix; server-side `time.created` | Matches code |
| CONTEXT.md "Shared Store" / "Owned Server" / "Coexistent Server" / "Yield" | `~/.local/share/opencode`, pid file, one-server invariant | Matches code |

---

## 6. Open questions

1. **Is `/compact` an intentional V2 dependency or an accident?**
   `client.rs:754-761` is the *only* V2 endpoint outside session creation, and it
   is the only command in the set with no V1 path used anywhere. V1's equivalent is
   `POST /session/{id}/summarize` (`groups/session.ts:303`, requiring
   `providerID`+`modelID` in the body — `handlers/session.ts:282-290`). Since
   `POST /api/session` was adopted deliberately (V2 is where `location.directory`
   lives), `/compact` is probably deliberate too — but nothing records the
   decision. **Needs a ticket either way.**

2. **What is the V2 replacement for `prompt_async`?** I found no V2 counterpart in
   `packages/protocol/src/groups/`. The candidates are `POST /api/session/:id/wait`
   (a *wait*, not a fire-and-forget) and `GET /api/session/:id/event`
   (a per-session event stream — which, if reliable, would also let cola retire the
   whole 1.5 s poll, R3). The Supplement path (`turn/mod.rs:462`,
   `client.rs:420-426`) has no answer without one.

3. **Is there a V2 `GET /session/status` equivalent, and does "absent ⇒ idle" hold?**
   V2 has `GET /api/session/:id/context` and `…/history`, neither obviously a run
   state. If a status concept moved to `POST /api/session/:id/model` +
   `switchAgent` semantics plus events, the drain and follow liveness (R4) need a
   new source. **This is the question I would put to upstream first.**

4. **Do the V1 and V2 message stores really not project into each other?**
   ADR-0053's amendment asserts it and deleted the decoder on that basis, but the
   evidence is a design observation, not a test. `packages/core/src/session/sql.ts`
   has a single `session`/`message`/`part` table set shared by both, and
   `packages/core/src/v1/` is a schema-compat layer inside the same DB. If the
   stores *do* project, R1 collapses to a decoder swap and the whole migration is
   an order of magnitude cheaper. **Worth one focused experiment before any
   migration ticket.**

5. **Would upstream add per-session permissions/questions to the V1 surface, or
   only ship them in V2?** R2 is structural; if V1's global endpoints are frozen
   "for compatibility", cola can stay on V1 indefinitely and the migration becomes
   a *choice* rather than a *deadline*. That framing changes the risk ranking
   materially — worth asking.

6. **`msg_` vs `msg` — when did V2 tighten it, and is the V1 rule still looser?**
   ADR-0026 records `msg`; today's V2 schema is `msg_`. If a future V2 tightens
   further (e.g. `msg_` + a length or charset rule), `msg_cola_` + a 32-hex uuid is
   probably safe, but cola has no test asserting the id is *accepted* — only that
   it is *sent* (`client.rs:1268` asserts `id` is absent from the *create* body).
   **A cheap wire test could pin this.**

7. **Who owns the `feishu/card/tool_render.rs` raw-payload access?**
   ADR-0042 makes it a deliberate Platform specialization; ADR-0053 names it an
   out-of-scope exception. But it is ~100 lines of V1 built-in-tool field names
   (`command`, `filePath`, `subagent_type`, `todos[].status`, `<task …>` grammar…)
   living in the Platform, which is exactly the "adapter change, not a
   bridge-and-card change" guarantee ADR-0053 promises. **Is that a known debt or
   an oversight?**

8. **Is there a version floor for cola?** Nothing in `README.md`,
   `docs/user-guide.md`, `cola.toml.example` or the workflows states a minimum
   OpenCode version. `discovery.rs` tolerates *older* servers only in one place
   (`client.rs:238-249`, the `/experimental/session` 404). A stated floor would
   make several (b) rows into documented, intentional constraints.

9. **Should `/sub`, permission cards and liveness survive a generation switch at
   all?** The sub-task feature depends on four separate V1 facts: `parentID`
   (`types.rs:28-29`), `metadata.sessionId` on the `task` tool
   (`tool_render.rs:123-132`), the parent-chain walk needing `directory`
   (`pollers.rs:328`), and `task` creating a session absent from cola's store
   (pitfall #11). V2 has a first-class `SubtaskPart` and a `parent_id` column
   (`sql.ts:36`). If V2 models children explicitly, the whole workaround chain
   (`walk_parent_chain` → `resolve_card_target` → `inline_host_session`) could
   collapse — but that is a *feature* migration, not a compat fix, and belongs on
   the map as its own decision ticket.

---

## Appendix: how to reproduce the generation split

```bash
cd /root/workspace/dev/opencode
# V1 groups (unprefixed roots)
grep -n 'const root' packages/opencode/src/server/routes/instance/httpapi/groups/{session,permission,question}.ts
# V1 vs V2 classification in the public OpenAPI builder
sed -n '146p;180,181p' packages/opencode/src/server/routes/instance/httpapi/public.ts
# V2 surface
grep -rhn 'HttpApiEndpoint\.' packages/protocol/src/groups/*.ts | grep '"/api'
# auth: username default + username equality check
grep -n 'OPENCODE_SERVER_USERNAME\|credentials.username' packages/opencode/src/server/auth.ts
# message id validation (V2)
sed -n '12p' packages/schema/src/session-message.ts
# permission reply literals (V1)
sed -n '38p' packages/schema/src/v1/permission.ts
```
