# /topic command: create a real Feishu topic for a new session

How a user opens a new, UI-separated conversation in a p2p chat without
manually creating a topic.

## Context

cola's session model (ADR-0003) isolates sessions **by topic**: a Feishu topic
(`thread_id`, `omt_...`) maps one-to-one to an OpenCode session, and cola
already routes messages correctly when a user creates a topic manually (in p2p
and group chats alike — `event.rs` parses `thread_id` on both).

The gap is **creation**. Today the only way to start a topic is for the user to
do it by hand in the Feishu client (hover a message → 创建话题). `cola`'s own
commands don't:

- `/new [name]` opens a fresh session **in the current thread key** — for a p2p
  chat that is the lobby `(chat_id, chat_id)`, so the new session's messages
  **interleave** with the existing conversation in the Feishu UI. The user must
  remember to `/switch` back and forth, and the two conversations' cards are
  visually mixed. This is the concrete complaint that motivated this ADR.
- `/dir <path>` also lives in the current thread key (it re-roots the active
  session), not a new topic.

What the user wants: a command that opens a **new Feishu topic**, with a
**specifiable directory**, that does **not** disturb the current conversation,
so they can switch between topic-separated sessions in the Feishu UI.

## Feasibility (researched)

The blocker that made "create a topic by command" look impossible is that
topics are seeded **from a message** — there is no "create empty topic" API.
But Feishu's reply API provides exactly the needed primitive:

- **`POST /im/v1/messages/{message_id}/reply` with `"reply_in_thread": true`**
  replies to a message *in topic form*, i.e. **creates a topic** anchored on
  that message, and the **response carries the new `thread_id`** (topic overview
  docs, "方式二" / Method 2).
- Topic replies are supported **in single chats too** — Feishu's official help:
  "话题回复是以话题形式回复普通单条消息，既可以在单聊中使用，也可在群组中使用"
  (requires Feishu ≥ V5.19). Real-world bots (openclaw, deer-flow) use
  `reply_in_thread` for p2p topics.

So the flow is viable:

1. User sends `/topic <dir> [name]` (a top-level/lobby message in p2p).
2. cola creates an OpenCode session rooted at `<dir>`.
3. cola replies to that command message with `reply_in_thread: true`, reads the
   returned `thread_id`, and maps `ThreadKey { chat_id, thread_id }` → the new
   session in the SessionStore (the same map ADR-0003 uses).
4. cola tells the user "created topic; reply inside it". When they do, the
   message carries that `thread_id` and routes to the session.

## Decision

Add a `/topic` command:

- `Command::Topic { directory, name }`
- Parse: `/topic <dir>` or `/topic <dir> <name>`; `/topic` with no arg shows
  help. `<dir>` is required (the whole point is specifying where the session
  works); `<name>` is optional (defaults to the dir basename).
- Handler: create session at `<dir>` → reply `reply_in_thread: true` →
  persist `ThreadKey{chat_id, thread_id} -> session` → confirm to the user with
  guidance to reply inside the topic.
- `/topic` only makes sense on a **non-topic** message (opening a topic from
  inside another topic would nest confusingly); if invoked inside a topic, reply
  with a short note instead.

### Thread-aware replies (verified — normal replies stay in topic)

Live verification (Aug 2026) confirmed that replying to a topic message's
`message_id` **stays inside the topic in p2p chats** — the reply API defaults to
topic form when the target is already a topic message. So the ordinary reply
pipeline (streaming cards, notices) needs no change. The earlier openclaw
concern does not apply to cola's reply flow.

The one gap was **cards that must be *sent*** (permission/question cards and
external-message notifications when no in-flight streaming card exists — e.g.
after a restart). Attempting `send_card("thread_id", ...)` fails: the create API
**rejects `thread_id` as `receive_id_type`** (documented enum is
open_id/union_id/user_id/email/chat_id; observed as `error decoding response
body`). The working mechanism is the anchor:

- `reply_in_thread` also returns the created confirmation message's `message_id`
  — a message that lives *inside* the topic.
- That `message_id` is persisted as `SessionEntry.topic_anchor`.
- Fallback cards (permission / question / external notification) **reply to the
  anchor** instead of sending, so they land inside the topic. Without an anchor
  (non-topic sessions, or topic sessions that predate this field) they fall back
  to `send_card("chat_id", ...)` as before.

## Why

- Directly fixes the interleaving complaint: each `/topic` yields a genuinely
  separate, UI-separated conversation, switchable in the Feishu client without
  `/switch`.
- Reuses the ADR-0003 topic→session model and the existing SessionStore — no new
  mapping concept.
- Directory is specifiable at creation (the second complaint).

## Risks / open questions

- **Feishu response `thread_id` availability**: the reply-in-thread response
  carries `thread_id` (verified live). If it is absent, cola surfaces an error
  and does not persist a broken mapping.
- **Topic anchoring**: the created topic is anchored on the user's `/topic`
  command message. The user must reply *inside* that topic; a stray lobby reply
  goes to the lobby session. The confirmation message says so explicitly.
- **`reply_in_thread` scope/permission**: needs `im:message` /
  `im:message:send_as_bot` (already held) — verified working live.
- **`topic_anchor` staleness**: the anchor is the confirmation message from
  `/topic`; if the user later deletes it (or the session predates this field),
  fallback cards degrade to the chat top level rather than dropping — acceptable.

## Alternatives considered

- **Extend `/new` with a directory** (`/new <dir>`): simpler, but still lives in
  the current thread key → messages still interleave. Rejected for the primary
  complaint.
- **`/dir` in a topic**: works today for re-rooting a session, but doesn't
  create separation. Supplementary, not a replacement.
- **Manual topic + first-message `/dir`**: user creates the topic by hand, then
  the first message carries `/dir`. No new API surface, but adds a manual step
  and still can't specify the dir in the same gesture. Kept as a fallback UX if
  live verification of `reply_in_thread` fails.
