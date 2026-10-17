# Reference materials

For any work touching the OpenCode server API, Feishu card/WS integration, or
the bridge protocol, consult these first. They are the primary sources; the code
in `src/` follows them. `AGENTS.md` points here.

## Local source trees (sibling repos, read-only references)

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

## Feishu / Lark official docs

- Feishu card callback: https://open.feishu.cn/document/feishu-cards/card-callback-communication
- Feishu message card overview: https://open.feishu.cn/document/server-docs/im-v1/message-card/overview
- Feishu card JSON 2.0 component catalog: https://open.feishu.cn/document/feishu-cards/card-json-v2-components/component-json-v2-overview (what a card can hold — no file picker, no tree, no cascading/dynamic options)
- Feishu card JSON 2.0 structure: https://open.feishu.cn/document/feishu-cards/card-json-v2-structure
- Feishu embed web app in workbench: https://open.feishu.cn/document/embed-web-app-into-feishu-workbench/introduction

## OpenCode official docs

- OpenCode HTTP server API (canonical endpoints, paths, payloads): https://opencode.ai/docs/zh-cn/server/
- OpenCode SDK: https://opencode.ai/docs/sdk/
