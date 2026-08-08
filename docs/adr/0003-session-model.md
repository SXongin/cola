# Topic-based session model

Feishu topics give cola a multi-conversation carrier within one chat. We decided how a message maps to a session.

## Context

- Feishu's `thread_id` (`omt_...`) is the authoritative topic identifier. A message is a topic message IFF it carries `thread_id`; topic messages also carry `root_id` (the topic seed message's `message_id`).
- Topics are created **from** an existing message — there is no empty topic. The seed message is itself a chat-top-level message before the topic exists.
- Topics work in groups **and** p2p chats; both carry `thread_id`/`root_id`.
- The previous mapping keyed sessions by `root_id` (falling back to `chat_id` for non-topic messages). `root_id` is a *message* id, not a conversation id, and is a worse topic identity than `thread_id`.

## Decision

- **Key on `thread_id` when present**: `ThreadKey = (chat_id, thread_id)` for topic messages (group or p2p). Non-topic messages (no `thread_id`) key as the chat's top-level "lobby": `ThreadKey = (chat_id, chat_id)`. `ThreadKey.thread_id` replaced the old `root_id` field (serde alias keeps old `sessions.json` loadable).
- **Pure classification** (`ConversationKind::classify(chat_type, thread_id)`): `Topic` if `thread_id` present; `GroupLobby` for a top-level group message; `P2p` for a top-level p2p message. The kind also derives the key (`ConversationKind::thread_key`).
- **Lobby policy**: a top-level message in a group auto-creates the group's **lobby session**, and cola replies once with guidance (each topic = a separate session; `/new`, `/dir`, `/help`). Guidance fires only on session creation, so it is one-time per lobby. p2p top-level messages get no guidance — that's the bot's normal single conversation.

## Why

- `thread_id` is the actual topic identity; `root_id` was a heuristic proxy. Topic isolation now survives seed-message churn and is correct in p2p topics too.
- Group-root messages are an unavoidable side effect of topics (the seed lands in the lobby before the topic exists), so a lobby session is legitimate — but it must be explicit and explain itself, rather than silently creating a session a user didn't intend.

## Alternatives considered

- **Redirect group-roots** (no auto-create; reply "use a topic or `/new`"): avoids accidental lobby sessions but makes plain group chat unusable without extra steps, and the seed-message case still has no clean target.
- **Config toggle `group_root_session`**: more config surface for a behavior that can be explained once in guidance instead.
- **Key on `root_id`** (status quo): topic identity is a message id; fragile and wrong for p2p topics once a topic's root changes.
