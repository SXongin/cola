# Supplement-triggered Card Chain splits and Instant Reminder pinning

Feishu cannot move a message: a card update (PATCH) leaves the card where it
was sent, and the conversation-list preview always shows the newest message.
A user message sent while a Turn is in flight (a **Supplement**) therefore
lands *below* the live card, and the old text acknowledgement
(「📨 已收到补充…」) only added a second message below it — the reader's
viewport drifts away from the streaming card with nothing they can do. This
ADR reuses the existing **Card Chain** for position as well as size (a
Supplement forces a split, so the continuation card is the newest message),
keeps the render loop alive across a supplement that starts a new Turn (today
that reply is rendered nowhere), and adds Feishu's **Instant Reminder** to pin
the conversation while a Turn needs attention.

## Context

- **Updates don't move, sends do.** The only way to put the live card back at
  the bottom is to send it again as a new message. The Card Chain already does
  exactly that when a card overflows: the filled card is finalized with a
  「部分完成，继续中」 header and a continuation card takes over.
- **A Supplement is not a Turn.** `handler.rs` intercepts a message whose
  session has a Turn in flight and forwards it via `prompt_async`; the Backend
  merges it into the running Turn if that loop is still alive, or starts a new
  Turn if it has already exited. Cola cannot tell which at send time, and the
  answer does not change what the user should do.
- **The new-Turn case renders nowhere today.** The turn's render loop has
  exited by then, and external-message sync skips Cola-Authored Messages
  (ADR-0026) — so a supplement that starts a new Turn silently loses its
  reply. Observed by the Host before this design.
- **Commands land below the card too.** Commands are dispatched before the
  busy check (`handler.rs:328`) and answer with their own message/card. They
  are the interaction the user is performing right now; displacing them with a
  re-sent live card would fight the user.
- **Feishu's list layer offers two things.** An app feed card
  (`im:app_feed_card:write`) is a *separate* list entry whose only link is a
  plain https URL; the chat-open AppLink takes `openChatId` only — there is no
  message parameter, and no OpenAPI yields a message-specific link. Instant
  Reminder (`time_sensitive`) instead pins an existing conversation (group
  `feed_cards/:chat_id`, bot p2p `feed_cards/bot_time_sentive`) but carries no
  reason text.

## Decision

- **A Supplement splits the Card Chain.** The continuation card is the reply
  to the supplement message, carries the accumulated card, and becomes the
  live card; the previous card is finalized with the standard split header.
  No new concept, no new card type, no recall.
- **No text acknowledgement.** The continuation card records the supplement
  with a receipt line (「📨 已收到补充」). The wording is neutral: merge vs.
  new Turn is only observable after the fact, and both outcomes end with the
  reply rendered on the continuation card, so the receipt is always true.
  Every supplement splits — no coalescing window.
- **Commands never split the chain.** Their replies are what the user is
  looking at; inserting the live card below them would push them away.
- **Keep rendering after the prompt call returns.** The render loop stays
  alive while the session is still running or an unanswered Cola-Authored
  supplement is newer than the turn anchor, and renders into the live
  (continuation) card. This closes the new-Turn gap for both merged and
  split cases.
- **Pin the conversation while attention is needed.** Instant Reminder is
  enabled while a Permission/Question is pending, and for a Turn running past
  a threshold (60 s default); it is cleared when the wait resolves. A
  completed long Turn stays pinned for a short TTL (2 min default) so someone
  waiting elsewhere still catches the end. Pinning is best-effort:
  failures log only, a missing scope disables nothing else, and p2p/group
  share the logic.
- **One config switch:** `[bridge] pin = true | false` (default true) turns
  Instant Reminder off. The split behavior itself is not configurable for now.

## Why

- Reusing the Card Chain keeps one mental model for both split causes: "the
  live card is always the newest card of the chain", whether it grew too big
  or was overtaken by a user message.
- The receipt-on-the-card replaces a second message with zero extra sends and
  keeps the acknowledgement where the reader is already looking.
- Keeping the render loop alive costs nothing when nothing is pending (the
  loop exits on idle) and rescues the one path where a supplement currently
  disappears into the store.

## Alternatives considered

- **One card per Turn, no splits** — rejected: progress becomes invisible
  exactly when the user is talking to cola.
- **Recall + resend the old card** — rejected: recall failure modes, a
  recall hint in history, and the old card's content vanishes.
- **App feed card as a status bar** — rejected: its click cannot reach the
  live message (AppLink has no message parameter), it adds a second list
  entry, and it needs another scope.
- **Split on every message, commands included** — rejected: pushes the user's
  current interaction away.
- **Unpin the moment a Turn completes** — rejected as the TTL default: the
  completion signal would disappear with it for someone waiting elsewhere.
  A read-event-driven unpin was deferred (p2p-only signal, new event
  subscription) in favor of the fixed TTL.
- **Configurable thresholds/TTL now** — deferred: one on/off switch first;
  revisit if the defaults annoy.

## Consequences

- A supplement produces one extra message (the continuation card) plus a
  finalized prior card; the chain's earlier cards stay as
  「部分完成，继续中」 history.
- The list preview is the status surface. While the live card is newest it
  shows the card title (等待你的授权/回答 · 运行中 · ✅ 完成); when the newest
  message is a command reply, it shows that reply — accepted, it is what the
  user just did. The pin carries no reason; the preview does.
- Pin disappears meaning "no longer needs you", not "just completed".
- Group chats can only split on messages cola receives (@-mentions) until the
  sensitive `im:message.group_msg` scope is ever granted; group behavior is
  untested in the current private model.

## Domain note

**Card Chain** is broadened to cover position-driven splits. **Supplement** and
**Instant Reminder** enter the glossary.
