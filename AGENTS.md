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
4. **Model must exist on the cola-managed server**: cola.toml `model` must match providers the 4096 server actually loads (`opencode/...`), not OpenChamber-only providers (`opencode-go/...`).
5. **Feishu WS frames are binary protobuf (pbbp2)**: manually parsed in `src/feishu/ws.rs`. Card button callbacks require sending an ack frame (`{"code":200,"headers":null,"data":"<base64>"}`) or Feishu shows "目标回调服务超时".
6. **Feishu resends events at-least-once**: dedupe by `header.event_id` and filter events older than 5 minutes.
7. **OpenChamber's server (40897) hogs the single-threaded OpenCode runner + DB write lock**: cola uses its own server (port 4096) with an independent data dir (`XDG_DATA_HOME=/root/.local/share/opencode-cola`) but shared config (`XDG_CONFIG_HOME=/root/.config`). Shared-DB is possible when OpenChamber is idle (user preference — see handoff).
