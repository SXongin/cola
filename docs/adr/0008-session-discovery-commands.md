# Session discovery & adoption commands

How a user finds and takes over sessions that were not created in Feishu
(OpenChamber, CLI), and how the session-selection commands behave.

## Context

- Users cannot see or adopt sessions created outside Feishu even though they
  share the same default store: `/list` and `/switch` only read the current
  thread's `SessionStore` entries.
- The server's `GET /session` is **project-scoped**: it returns only the
  sessions of the project the server instance runs in (its cwd), NOT the whole
  shared store. Cross-project listing lives at the experimental
  `GET /experimental/session` (`Session.GlobalInfo`, sorted by
  `time_updated` desc, sub-task children and archived excluded by default).
  Both carry `title`, `directory`, `parentID`, `time`.
- Current scale: ~174 sessions, ~70 KB of `Session.Info` JSON over a localhost
  GET (<50 ms). The display is capped regardless of store size.
- The `Session.Info` of a sub-task child shows `parentID`; its title is always
  `Child session - <iso>` (never auto-generated, see ADR-0007).
- cola already resolves a chat's display name pattern for users
  (`feishu/client.rs`), and the topic anchor mechanism (ADR-0006) is the
  building block for landing cards inside a topic.
- A session is mapped to at most one thread at a time (ADR-0007). Adopting an
  already-mapped session is therefore a *steal* unless handled explicitly.

## Decision

- **`/list [keyword] [--all]`**
  - Pulls the cross-store session list (`GET /experimental/session`, falling
    back to project-scoped `GET /session` on servers without it) into an
    in-memory cache with a 30 s TTL, invalidated immediately when cola
    creates/adopts/renames a session.
  - Sorts client-side by `time.updated`; shows at most 15 entries.
  - Keyword: case-insensitive substring match on `title` / `directory` / id.
  - Children (`parentID` set) and archived sessions hidden by default; `--all`
    includes children. Archived stays hidden unless listed explicitly.
  - Marks the current thread's active session and its other mapped sessions.
  - Rejected inside a topic that already has a session (ADR-0007); in a
    never-had-a-session topic it is allowed (the outcome is that topic's single
    session).
- **`/switch <keyword>`** (the lobby — topics are single-session)
  1. Match among the current thread's mapped sessions first; a unique hit
     switches without touching the mapping.
  2. Otherwise match globally (**children excluded**); a unique hit adopts the
     session (writes the mapping, sets active).
  3. Multiple hits: list up to 8 candidates (`title · dir · id-tail`) and point
     at `/attach <full id>`.
- **`/attach <id|title> [--force]`**
  - Resolution order: exact id > unique id-prefix > unique title substring.
  - Already mapped to the current thread: idempotent switch.
  - Already mapped to another thread: reject with an actionable card — session
    title, directory, the mapped chat's name (`GET /im/v1/chats/{chat_id}`) and
    topic/lobby flag — and a pointer to `/forget` or `/attach ... --force`.
  - `--force`: steals the mapping (the other thread becomes sessionless; its next
    message auto-creates a fresh session).
  - In a never-had-a-session topic the adopted session becomes that topic's
    single session; the anchor for fallback cards is the command's own reply
    message inside the topic.
- **`/forget`**: removes the current thread's session mapping without touching
  the server session (still listed and adoptable). Covers accidental attach,
  transferring a session between threads, and stale mappings left by deleted
  Feishu topics.
- **`/name <new>`**: `PATCH /session/{id}` with the new title and refreshes the
  list cache (ADR-0007 title policy).
- **External-notification card title** (`bridge/external.rs`) switches from the
  stored `name` to the server title.
- **Cleanup**: `list_sessions` moves to the cross-project
  `GET /experimental/session` (falling back to `GET /session` for older
  servers) and starts parsing `Session.GlobalInfo[]`; dead store helpers are
  enabled or removed.

## Why

- The global list plus adoption is exactly the "find sessions not in Feishu"
  capability. The cache + client-side sort gives accurate recency without a
  server-side feature or a per-invocation fetch; the display cap keeps Feishu
  output constant regardless of store size.
- Candidate listing + exact-id attach keeps ambiguity safe instead of
  auto-adopting the wrong session.
- The rejection card fixes the "which chat was that?" failure: a raw `chat_id`
  is useless in a busy client, so the card shows the session and the mapped
  chat's human name.

## Alternatives considered

- **Server-side `search` only**: loses directory/id matching and `time_updated`
  ordering. Rejected.
- **Pagination now** (`/list --page`): premature at the current scale; revisit
  at ~10k sessions (~4 MB) when the full pull stops being free.
- **Silent steal on `/attach`** of a mapped session: breaks the one-session-one-
  thread invariant and silently orphans the other thread. Rejected in favor of
  the explicit `--force`.
- **No `/forget`**: leaves no way to undo an accidental attach or to transfer a
  session between threads (the rejection card would permanently block it).

## Risks / open questions

- An adopted session that another client (e.g. OpenChamber) is actively driving
  will be picked up by cola's render and external-message pollers once mapped —
  desirable as a "real-time view", but the two clients can interleave. Streaming
  assistant-message updates into Feishu is a separate feature, out of scope here.
- `/list` caching can serve up to 30 s-stale data after external renames or
  activity; acceptable for a listing command.
- `/list --all` shows sub-task children by id-tail/directory only (their titles
  are `Child session - ...`); adoption of a child is possible and deliberately
  left out of `/switch` auto-adoption.