# Rust bridge architecture

colca is a Rust binary. Feishu + OpenCode adapters connect to a shared bridge core. Single global SSE connection filters events by sessionID. Sessions map to Feishu threads via a JSON file.

## Decision

- **Rust**: native performance for SSE parsing + Feishu card building. Opens door to all platform API patterns.
- **One global SSE connection** to OpenCode's `GET /api/event` — filters events by `sessionID`. Per-session SSE is unnecessary because the global stream carries every event we need.
- **Thread = session boundary**: Feishu thread root message ID maps to one OpenCode sessionID. Stored in `~/.cola/sessions.json`.
- **Project = session property**: `/dir` changes the working directory for a session.
- **Feishu WebSocket** for event delivery — outbound WSS connection, no public endpoint required.
- **Permission events live on the global SSE only** — not on per-session SSE. We watch the one global connection.
- **Card state machine**: loading → reasoning → streaming → done. Error is inline in done state. Tool updates are collapsible panels.
- **Streaming throttle**: 200ms debounce for text accumulation, immediate flush on tool lifecycle boundaries.
- **JSON file for session mapping**: single `HashMap<ThreadKey, SessionID>` serialized. Small data, single-writer, no migrations.
