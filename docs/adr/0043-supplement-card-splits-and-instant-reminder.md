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
  **Superseded by the 2026-09-19 amendment below: the continuation carries
  only the delta, not the accumulated card.**
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
  waiting elsewhere still catches the end — unless a new Turn starts first: a
  new Turn means the user is active again, so its `begin_turn` releases the
  previous turn's TTL hold immediately (a Pending hold survives; the user
  still owes an answer). Pinning is best-effort: failures log only, a missing
  scope disables nothing else, and p2p/group share the logic.
- **Pins are generation-scoped and in-memory.** Every pin carries the turn
  generation that owns it, and a clear from an older generation is a no-op
  against a newer turn's pin — so a stale clear (the long-turn TTL timer) can
  never unpin the turn that superseded it. The threshold timer and the turn's
  completion are decided under the pin lock: whichever runs first wins, so a
  completion racing the threshold either makes the timer a no-op (a short turn
  never pins) or keeps the live pin for the TTL — never both, never a leaked
  pin. The release a new Turn issues passes the live pin's own generation, so
  the guard protects against stale timers, not against the newer turn itself.
  The state is not reconciled at startup: a pin orphaned by a crash or
  restart is cleared on the Chat or Topic's next turn (self-healing), never a
  permanent pin.
- **One config switch, off by default:** `[bridge] instant_reminder = true`
  turns Instant Reminder on. Absent or `false` means off — an upgrade never
  changes notification behavior without consent. The split behavior itself is
  not configurable for now.

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
- The long-turn threshold (60 s), the completion TTL (2 min) and the render
  poll cadence are internal constants; the bridge tests inject tiny values
  through the pin state's / external flow's atomics, so the whole lifecycle
  runs in milliseconds without a real Feishu tenant.
- Group chats can only split on messages cola receives (@-mentions) until the
  sensitive `im:message.group_msg` scope is ever granted; group behavior is
  untested in the current private model.

## Domain note

**Card Chain** is broadened to cover position-driven splits. **Supplement** and
**Instant Reminder** enter the glossary.

## Amendment (2026-09-19): the continuation carries only the delta

The Decision above says the continuation "carries the accumulated card" — it
re-renders everything the previous card already shows. The acceptance smoke
test showed that duplicating the whole Turn on every Supplement is worse than
a non-self-contained newest card: the chain already reads contiguously
top-down, and the reader's viewport is at the bottom where the delta lands.

- The split is now a **delta handoff**, exactly like a size split: the
  previous card is finalized with everything before the split, and the
  continuation — the reply to the supplement message — carries its receipt
  line plus only the content that arrives after it.
- Consequence: the newest card is not self-contained. A reader who jumps
  straight to the bottom sees the newest content and the receipts, not the
  whole Turn; the earlier cards above carry the rest of the chain. Accepted,
  because the cards are adjacent messages and reading them in order is the
  normal path.

Source: product-owner smoke test of the accumulated-card behavior and
approval of the delta revision.

## Amendment (2026-09-19): waiting cards are message-pinned

The Decision above pins only the conversation, and the pinned entry previews
the chat's newest message. Once a chat holds several topics (several sessions
share one `chat_id`), a wait in an older topic has no in-list surface: tapping
the reminder lands on the newest message, which may be anything — a resolved
card, another topic's completion, the user's own message. Feishu's **Pin
message** API (`POST /im/v1/pins`; satisfied by the already-held `im:message`
scope, or `im:message.pins:write_only`) pins a specific message into the
chat's pinned-message list, so entering the chat shows exactly what waits.

- A pending Permission/Question's **host card** — the streaming card carrying
  its Interaction Block, the standalone card it was sent as, or the Session
  Snapshot card hosting a claimed block (ADR-0028) — is pinned while the
  request waits and unpinned once it leaves the pending list. A card rendering
  both kinds is pinned once (reference-counted by message); the last wait off
  it unpins it. A card replaced by a re-host, a split, or a snapshot claim
  moves its pin.
- Only waiting requests pin a message. A long Turn pins none: a running card
  is not an item to answer, and its content changes; the conversation-level
  reminder keeps the long-turn lifecycle above unchanged.
- Cola pins each (request → card) transition once: an unchanged wait is never
  re-asserted, so a card the user unpins by hand is not fought. A failed pin
  retries while the wait lasts; a failed unpin stays tracked; a failed pin
  never fabricates a later unpin. A directory whose list call failed is
  unknown, never resolved (#130), exactly as in the reminder's sweep.
- No read-back reconciliation: the pin list's consistency lags writes
  (measured 2026-09-19: a `DELETE` still listed around 20 s later), so cola
  tracks its own transitions in memory. A pin orphaned by a crash stays until
  the user removes it — its message id is unknowable after a restart. Accepted
  for now, like the reminder's startup self-heal exception.
- Naming: the pre-existing `bridge/pin.rs` only ever implemented
  `time_sensitive`, so it is renamed `bridge/reminder.rs`
  (`ReminderState`/`ReminderReason`/`ReminderTarget`), and the new registry is
  `bridge/message_pins.rs` (`MessagePins`) — "reminder" means the
  conversation-level `time_sensitive` state, "message pin" the platform's
  pinned-message list.

Source: #247 grilling of multi-session precedence and the live API probe that
confirmed message pinning works on the existing scope.

## Amendment (2026-09-19): the long-turn threshold measures user silence

The Decision above says a Turn running past 60 s pins the conversation. Live
testing showed a one-shot timer from turn start firing even when the user had
just acted — clicked a permission, answered a question, or sent a supplement —
which is the opposite of the reminder's intent: the pin exists to say
something needs the user, and a user who is already acting does not need
telling.

- The threshold is now measured from the Chat/Topic's last **inbound user
  activity**: any Feishu message (a Supplement, a recognized command such as
  `/new`, an unknown slash command) or card action (permission reply, question
  answer/handoff), plus turn start. External/non-Feishu activity never counts.
- Any interaction also releases a live `LongTurn` hold and restarts the clock;
  a new Turn expresses its release through the same path. A `Pending` hold
  survives — the user still owes an answer.
- Firing is idempotent but not once-only: while the Turn keeps running,
  renewed silence pins again, still generation-scoped and never after
  completion.
- The one-shot timer is replaced by a per-turn checker that re-evaluates the
  silence under the pin lock every injected tick, and exits once the turn
  completed or a newer generation armed the Chat/Topic. The tick, threshold
  and TTL remain injectable atomics, so the tests run the lifecycle in
  milliseconds.

Source: product-owner live-testing observation of a pin firing right after a
permission click, and approval of the silence semantics.

## Update (2026-09-19)

The preview example in Consequences tracks the kind split above: a
permission-only wait previews 等待你的授权, a question-only wait 等待你的回答,
and only both pending at once keep 等待你的授权/回答 (ADR-0014, 2026-09-19
update).
