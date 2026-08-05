# Rust bridge architecture

cola is a Rust binary. Feishu + OpenCode adapters connect to a shared bridge core. Sessions map to Feishu threads via a JSON file.

## Decision

- **Rust**: native performance for SSE parsing + Feishu card building. Opens door to all platform API patterns.
- **Canonical API paths, no `/api` prefix**: `POST /session/{id}/message`, `GET /permission`, `POST /permission/{id}/reply`. The legacy `/api/*` paths emit only v2 events and are not used.
- **Synchronous prompt + incremental render poll**: cola submits the prompt via the blocking canonical endpoint and renders the final card from the persisted parts. While the prompt runs, a poller (`GET /session/{id}/message`, 1.5s) renders reasoning / tool / text parts as they are written, so the card updates step by step.
- **Global SSE carries only v2 durable events** (`message.updated`, `message.part.updated`, `session.updated`). The v1 `session.next.*` streaming events are *not* delivered — they carry no `location`, so the server's `event.location?.directory === instance.directory` filter drops them. The SSE connection is kept for heartbeats only; live rendering is driven by the poller.
- **Permissions are polled, not streamed**: `permission.*` events are typed-PubSub only and never reach any SSE. cola polls `GET /permission?directory=<dir>` every 3s per known session directory (permissions are per-instance, so the directory must scope the request), and replies via `POST /permission/{id}/reply?directory=<dir>`.
- **Thread = session boundary**: Feishu thread root message ID maps to one OpenCode sessionID. Stored in `~/.cola/sessions.json`.
- **Project = session property**: `/dir` changes the working directory for a session.
- **Feishu WebSocket** for event delivery — outbound WSS connection, no public endpoint required.
- **Card state machine**: loading → reasoning → streaming → done → error. Error is a distinct terminal state (ADR 0002). Tool updates are collapsible panels.
- **Per-session prompt serialization**: one in-flight prompt per session; a second message is answered with a busy notice instead of racing on the same card.
- **JSON file for session mapping**: single `HashMap<ThreadKey, SessionID>` serialized. Small data, single-writer, no migrations.
