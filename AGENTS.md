# cola

A bridge bot that brings the OpenCode AI coding experience into Feishu.

## Agent skills

### Skill flow

Which skill runs when: the main flow (idea → ship), the bug/issue on-ramps, and the local bindings. See `docs/agents/flow.md`.

### Issue tracker

Issues live as GitHub issues on `SXongin/cola` (use `gh`); `.scratch/` is a frozen pre-GitHub archive. See `docs/agents/issue-tracker.md`.

### Triage labels

Five canonical GitHub labels: `needs-triage`, `needs-info`, `ready-for-agent`, `ready-for-human`, `wontfix`. See `docs/agents/triage-labels.md`.

### Domain docs

Single-context: one `CONTEXT.md` + `docs/adr/` at the repo root. See `docs/agents/domain.md`.

## Reference materials

For any work touching OpenCode server API, Feishu card/WS integration, or the bridge protocol, consult these first. They are the primary sources; the code in `src/` follows them.

### Local source trees (sibling repos, read-only references)

- **OpenCode source** (HTTP Server API, SSE, permissions, SDK): `/root/workspace/dev/opencode`
  - HTTP API groups/handlers: `packages/opencode/src/server/routes/instance/httpapi/groups/`, `.../handlers/`
  - Session prompt/processor: `packages/opencode/src/session/prompt.ts`, `processor.ts`, `message-v2.ts`
  - Core event projector (message/part/session_message tables): `packages/core/src/session/projector.ts`, `message-updater.ts`
  - Schema (event types, camelCase field names): `packages/schema/src/session-event.ts`, `packages/client/src/generated/types.ts`
- **cc-connect source** (Go, Feishu bot integration reference): `/root/workspace/dev/cc-connect`
  - Feishu platform + card building: `platform/feishu/`, `core/card.go`, `core/engine.go`
- **OpenChamber source** (web client that works end-to-end): `/root/workspace/dev/openchamber`
  - OpenCode proxy: `packages/web/server/lib/opencode/proxy.js`
  - Prompt dispatch: `packages/web/server/lib/openchamber-sessions/routes.js`
  - Event reducer (what UI renders): `packages/ui/src/sync/event-reducer.ts`
- **Lark Go SDK** (Feishu WS pbbp2 binary protocol reference): `/root/go/pkg/mod/github.com/larksuite/oapi-sdk-go/v3@v3.5.3/ws/`

### Feishu / Lark official docs

- Feishu card callback: https://open.feishu.cn/document/feishu-cards/card-callback-communication
- Feishu message card overview: https://open.feishu.cn/document/server-docs/im-v1/message-card/overview
- Feishu embed web app in workbench: https://open.feishu.cn/document/embed-web-app-into-feishu-workbench/introduction

### OpenCode official docs

- OpenCode HTTP server API (canonical endpoints, paths, payloads): https://opencode.ai/docs/zh-cn/server/
- OpenCode SDK: https://opencode.ai/docs/sdk/

## Local development

Git hooks are managed by **lefthook** (https://lefthook.dev). They install
automatically on your first `cargo build`/`check`/`test` (root `build.rs` runs
`lefthook install`; skipped in CI and non-git checkouts, and re-synced whenever
`lefthook.yml` changes). If lefthook is missing, the build prints a warning. To
install or re-sync manually at any time:

    lefthook install

`lefthook.yml` wires up:

- **pre-commit**: `cargo fmt --all -- --check` + `cargo clippy --workspace --all-targets -- -D warnings`. Both only fire when staged files touch `*.rs`, so docs/`.scratch`-only commits stay fast.
- **commit-msg**: Conventional Commits validation (`cargo xtask check-commit-msg`).
- **pre-push**: `cargo xtask audit` — dependency audit via cargo-deny (`deny.toml`). Install cargo-deny with `cargo install cargo-deny`.

Standard local verification loop before pushing (CI's Format job is `cargo fmt --all -- --check` — clippy and rustc do NOT check formatting, so a clean clippy does not mean a clean fmt):

    cargo fmt --all -- --check
    cargo clippy --workspace --all-targets -- -D warnings
    cargo xtask check-generation
    cargo test --workspace --locked
    cargo build --release --locked

`cargo xtask check-generation` is the V1 coupling guard (spec #364): it rejects V1 route literals and wire field names outside `src/opencode/v1/` (plus the tool-render allow-list), so V1 retirement stays a deletion.

The live contract suite (ADR-0057) is opt-in: every test is `#[ignore]`-gated and runs one generation per test, always in a fresh isolated XDG tree (never the default store). The V1 chains (`live_v1_scripted_capability_chain`, `live_v1_scripted_failure_and_retry_chain`) need the pinned OpenCode **1.18.31** binary (the one `.github/actions/install-opencode-v1` installs — never V2); CI installs it on `PATH` and runs `cargo test --locked -- --ignored live_v1`. The V2 chains (`live_v2_scripted_transcript_read`, `live_v2_scripted_write_chain`, `live_v2_scripted_permission_chain`, `live_v2_scripted_form_chain`, `live_v2_scripted_selection_chain`, `live_v2_scripted_retry_id_chain`) need a 2.x binary and spawn it directly with relocated XDG trees; point `COLA_LIVE_OPENCODE_V2_BIN` at one (`opencode2` on `PATH` is the fallback). The `Live V2` job installs the exact-version + sha256 pin via `.github/actions/install-opencode-v2` (npm-based, version-keyed cache) and runs `cargo test --locked -- --ignored live_v2`; `Live V1` deliberately does not run it. Locally:

    COLA_LIVE_OPENCODE_BIN=/path/to/opencode cargo test --locked -- --ignored live_v1
    COLA_LIVE_OPENCODE_V2_BIN=/path/to/opencode2 cargo test --locked -- --ignored live_v2

Set `COLA_LIVE_CAPTURE_DIR=<dir>` on either run to re-record the raw transcript responses into the fixture format; sanitize them by hand before committing under `src/opencode/wire/fixtures/`.

## Contribution guidelines

- **Branch workflow (TBD, rebase-linear)**: `main` is the only long-lived branch. Every task runs the loop — start on `main`, branch off it, work, rebase on the latest `main`, open a PR with base `main`, get CI and review green — and then **stops: agents never merge a feature/task PR**. The user verifies the change themselves and gives the explicit go-ahead (or merges it themselves); only after the merge does the next task return to `main` (`CONTRIBUTING.md` "Branch workflow"). The one exception is the release cut: `cargo xtask release` merges its own CI-gated PR after the user confirms the smoke test (ADR-0033). The base is always `main`. Check `git branch --show-current` before editing.
- **Check gates**: only `Format`, `Check`, `Test (macos-latest)`, `Test (windows-latest)`, `Live V1`, `Live V2` and `Dependency audit` are required by the `main: CI` ruleset — judge merge-readiness with `gh pr checks <branch> --required`, never by waiting for every check. CodeQL is advisory on normal PRs: do not wait for it, and do not re-triage its recurring test-only/loopback findings — dismiss them per the recorded policy (`CONTRIBUTING.md`, "Security scanning"; issue #383). The release cut is the exception (`cargo xtask release` watches every check, CodeQL included). `release/*` and docs-only PRs skip the code-dependent work — a skipped job or step reports success and still satisfies the ruleset; the Test matrix and the two live jobs still dispatch (their steps gated) so their required checks report; `Dependency audit` always runs.
- **PR rules**: `CONTRIBUTING.md` — commit conventions, the pre-PR verification loop, and the PR description checklist. Follow it when creating commits or PRs.
- **Coding standards**: `CODING_STANDARDS.md` — the source the `/code-review` skill's Standards axis reads (together with `CONTRIBUTING.md`).
- **Releases**: cut with `cargo xtask release <version>` (ADR-0033) — it bumps the manifest, merges through the CI-gated PR, and tags the merged commit. Never hand-edit the version or tag a branch commit; only pass `--yes` after a human confirmed the smoke test.
- **Commit attribution (agents)**: every commit an agent creates ends with one `Co-authored-by:` trailer naming the agent and the model — `Co-authored-by: opencode (<Model Display Name>) <noreply@opencode.ai>`, e.g. `Co-authored-by: opencode (DeepSeek V4.1 Flash) <noreply@opencode.ai>`. Resolve the model name from the session; never hardcode it. See `CONTRIBUTING.md` ("Agent attribution").

## Handoff

A handoff document for continuing this project lives at `/tmp/opencode/cola-handoff.md` (may be stale — regenerate with the handoff skill if it's missing or outdated). Read it first when starting work that continues past conversations.

## Known pitfalls (learned the hard way)

1. **Two API generations are mounted: unprefixed = V1 compatibility, `/api` = current (announced as V2)**. Upstream redefined the prefix: when ADR-0001/0007/0026 were written, `/api/*` was the older generation (its prompt appended a fresh message and emitted only v2 events, invisible to the OpenChamber-readable message tables) and the unprefixed paths were canonical. Today the current protocol is `/api/...` (`@opencode-ai/protocol`, `sdk-next`); the unprefixed paths remain as the V1 compatibility surface. Check a route's generation and payload shape before using it.
2. **OpenCode event JSON fields are camelCase** (`callID`, `sessionID`, `assistantMessageID`, `textID`, `reasoningID`). Serde structs need `#[serde(rename = "...")]` — missing renames silently null out fields (e.g. tool panels never rendered).
3. **Permissions live at global endpoints**: `GET /permission` (list all pending) and `POST /permission/{requestID}/reply` with `{reply}`. The old `/api/session/{id}/permission` returns empty — permission cards never appear, AI hangs on tool permission.
4. **Model is optional — unset means "use the server default"**: cola pins a model only when `[opencode] model` is set in cola.toml; it then must exist on the server cola attaches to (usually `opencode/...`, matching the shared server's providers, NOT OpenChamber-only providers like `opencode-go/...`). When unset, cola sends no model and the OpenCode server falls back to its own default (`provider.defaultModel()`) — the safest option on a new machine. `/model <provider/model>` sets a runtime per-session override: on V1 the server has NO model-switch endpoint, so cola stores it and sends it per-prompt; on V2 `/model`, `/think` and `/agent` are durable session switches (`POST /api/session/{id}/model|agent`, the variant inside the model ref) and cola mirrors them locally for display.
5. **Feishu WS frames are binary protobuf (pbbp2)**: manually parsed in `src/feishu/pbbp2.rs`. Card button callbacks require sending an ack frame (`{"code":200,"headers":null,"data":"<base64>"}`) or Feishu shows "目标回调服务超时".
6. **Feishu resends events at-least-once**: dedupe by `header.event_id` and filter events older than 5 minutes.
7. **Sessions are shared through the default store — one store, many clients**: cola attaches to whatever `opencode serve` runs on the default store (`~/.local/share/opencode`; `XDG_DATA_HOME` unset), whoever started it (OpenChamber's managed server, a manual one). It reads port + password live from `/proc`. If none is running, cola self-starts one on the default store (`bridge/discovery.rs`). Never use a private data dir — that's what silently broke sharing before. If a mapped session 404s (leftover from another store), cola recreates it automatically. Note: on OpenChamber's server the global SSE connection is ended every few seconds — cosmetic, cola renders by polling.
8. **Ack EVERY WS event, not just card callbacks**: the Lark SDK replies `{"code":200,"headers":null,"data":null}` to each `MessageTypeEvent`; cola previously only acked card actions. An unacked event is re-delivered forever, and a client that never acks is eventually treated as dead and stops receiving new events entirely (symptom: bot silently unresponsive, TCP still ESTAB). Also answer pbbp2 "ping" frames with "pong", and keep a read timeout + proactive ping so a half-dead connection triggers a reconnect (`handle_connection` in `src/feishu/ws.rs`).
9. **OpenCode part payloads have NO `id`**: the part's `id` is a database column that is not serialised into the part JSON (`{"type":"text","text":"...","time":{...}}`). Any dedup keyed on `part.get("id")` silently never dedupes — card text doubles (81→162 chars) because the poll loop and final render both append. Dedupe text/reasoning on **content**, not id (see `render_new_turn_parts` in `src/bridge/turn/render.rs`).
10. **Feishu @mentions arrive as opaque `@_user_N` tokens**: the message payload carries `message.mentions: [{key:"@_user_1", id:{open_id}, name}]`. Without parsing them, prompts leak `@_user_1` and the AI can't see who was referenced. Strip the bot's own mention (needs `bot_open_id` from `GET /open-apis/bot/v3/info`) and replace others with `@名字` (`strip_mentions` in `src/feishu/message.rs`). Note: Feishu only pushes group messages that @ the bot — that's a server-side permission, not fixable in cola.
11. **Sub-task (task tool) permissions carry the CHILD session id**: the `task` tool creates a separate child session for each subagent, so its tool permissions/list_questions come back with `sessionID = child`, which is NOT in cola's SessionStore → no chat mapping → permission card silently dropped ("No reply target or chat"). Resolve the delivery target by walking the parent chain via `GET /session/{id}` (`resolve_card_target` in `src/bridge/pollers.rs`), and carry the owning `directory` on the permission card so `handle_card_action` can route the reply without the SessionStore map.
12. **Server Basic auth needs BOTH username and password**: the server's effective username is `OPENCODE_SERVER_USERNAME` (default `"opencode"`), and `authorized()` in opencode's `auth.ts` checks username equality too — a wrong/missing username 401s even with a correct password. cola's client only sends an Authorization header when BOTH are set, so discovery must supply the username (`src/bridge/discovery.rs`, `username_from_env`), not just the password. cola's own self-started server strips an inherited `OPENCODE_SERVER_USERNAME` so it always uses the default `opencode`.
13. **Feishu does NOT truncate long markdown elements at 3000 chars**: `MAX_ELEMENT_TEXT_CHARS = 3000` (`src/feishu/card/mod.rs`) is cola's own card-size budget, not a platform per-element cap. Measured 2026-09-18: one 8000-char ASCII element and one 3060-char CJK element (~9.2KB) render fully through both create and PATCH; the documented card limits are 30KB total and 200 elements/components (11310). Don't "fix" a tool panel (`tool_panel_element`, max ~3.4k chars) for exceeding 3000 — splitting there buys nothing.
14. **V1 answers `GET /api/info` with `200 text/html`, not 404**: V1's web-UI catch-all serves `index.html` for every unknown path, so the HTTP status alone cannot tell V1 from V2 (measured 2026-09-27 on the running 1.18.23 and the pinned 1.18.31). Attach detection (`src/opencode/generation.rs`) follows one rule: **200 + V2's JSON info envelope (a `version` string and a `urls` array) = V2; 200 `text/html` = V1 (the catch-all); 404 = V1; a 200 without the envelope / 401 / 503 / a transport error = inconclusive** — cola stays serverless (never guesses, never spawns a second server against the same store). The `[opencode] generation` override is the escape hatch for proxies/odd builds.
