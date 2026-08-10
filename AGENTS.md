# cola

A bridge bot that brings the OpenCode AI coding experience into Feishu.

## Agent skills

### Issue tracker

Issues live as markdown files under `.scratch/`. See `docs/agents/issue-tracker.md`.

### Triage labels

Five canonical labels mapped to `Status:` values in issue files. See `docs/agents/triage-labels.md`.

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

## Handoff

A handoff document for continuing this project lives at `/tmp/opencode/cola-handoff.md` (may be stale — regenerate with the handoff skill if it's missing or outdated). Read it first when starting work that continues past conversations.

## Known pitfalls (learned the hard way)

1. **Canonical API paths have no `/api` prefix**: `POST /session/{id}/message` (not `/api/session/{id}/prompt`). Old path only emits v2 events; messages never appear in OpenChamber-readable tables.
2. **OpenCode event JSON fields are camelCase** (`callID`, `sessionID`, `assistantMessageID`, `textID`, `reasoningID`). Serde structs need `#[serde(rename = "...")]` — missing renames silently null out fields (e.g. tool panels never rendered).
3. **Permissions live at global endpoints**: `GET /permission` (list all pending) and `POST /permission/{requestID}/reply` with `{reply}`. The old `/api/session/{id}/permission` returns empty — permission cards never appear, AI hangs on tool permission.
4. **Model must exist on the server cola attaches to**: cola attaches to the running shared OpenCode server (usually OpenChamber's managed one) and uses its providers. cola.toml `model` must match what that server loads (`opencode/...`), not OpenChamber-only providers (`opencode-go/...`).
5. **Feishu WS frames are binary protobuf (pbbp2)**: manually parsed in `src/feishu/ws.rs`. Card button callbacks require sending an ack frame (`{"code":200,"headers":null,"data":"<base64>"}`) or Feishu shows "目标回调服务超时".
6. **Feishu resends events at-least-once**: dedupe by `header.event_id` and filter events older than 5 minutes.
7. **Sessions are shared through the default store — one store, many clients**: cola attaches to whatever `opencode serve` runs on the default store (`~/.local/share/opencode`; `XDG_DATA_HOME` unset), whoever started it (OpenChamber's managed server, a manual one). It reads port + password live from `/proc`. If none is running, cola self-starts one on the default store (`bridge/discovery.rs`). Never use a private data dir — that's what silently broke sharing before. If a mapped session 404s (leftover from another store), cola recreates it automatically. Note: on OpenChamber's server the global SSE connection is ended every few seconds — cosmetic, cola renders by polling.
8. **Ack EVERY WS event, not just card callbacks**: the Lark SDK replies `{"code":200,"headers":null,"data":null}` to each `MessageTypeEvent`; cola previously only acked card actions. An unacked event is re-delivered forever, and a client that never acks is eventually treated as dead and stops receiving new events entirely (symptom: bot silently unresponsive, TCP still ESTAB). Also answer pbbp2 "ping" frames with "pong", and keep a read timeout + proactive ping so a half-dead connection triggers a reconnect (`handle_connection` in `src/feishu/ws.rs`).
9. **OpenCode part payloads have NO `id`**: the part's `id` is a database column that is not serialised into the part JSON (`{"type":"text","text":"...","time":{...}}`). Any dedup keyed on `part.get("id")` silently never dedupes — card text doubles (81→162 chars) because the poll loop and final render both append. Dedupe text/reasoning on **content**, not id (see `render_new_turn_parts` in `src/bridge/render.rs`).
10. **Feishu @mentions arrive as opaque `@_user_N` tokens**: the message payload carries `message.mentions: [{key:"@_user_1", id:{open_id}, name}]`. Without parsing them, prompts leak `@_user_1` and the AI can't see who was referenced. Strip the bot's own mention (needs `bot_open_id` from `GET /open-apis/bot/v3/info`) and replace others with `@名字` (`strip_mentions` in `src/feishu/ws.rs`). Note: Feishu only pushes group messages that @ the bot — that's a server-side permission, not fixable in cola.
