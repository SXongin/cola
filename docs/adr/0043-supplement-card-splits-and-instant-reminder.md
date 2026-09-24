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

> **Superseded** by the 2026-09-21 amendment "the long-turn reminder is
> removed": the whole long-turn class is gone, so this silence semantics no
> longer applies. Kept as the record of what was tried.

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

## Amendment (2026-09-21): pins are persisted, and one pending owns the chat

> **Partially superseded** by the 2026-09-21 amendment "the long-turn reminder
> is removed": the persistence and the pending-only owner rule stand; every
> mention of the long-turn class below no longer applies.

The Decision above says the pin state is in-memory and "not reconciled at
startup". Live use showed the hole (#249): the Feishu client gives the user
**no way to cancel an app's Instant Reminder**, so a pin orphaned by a crash
stays until the chat's next Turn — and if cola never runs again in that chat,
forever. The multi-pending sweep was also nondeterministic (#247): one slot
per chat, last insertion wins, and a long-turn `ensure` could retarget away a
waiting `Pending`.

- **Pins are persisted.** Each live pin is mirrored to a small file beside
  `sessions.json` as `{chat_id, is_group, user_ids}` — `user_ids` because
  clearing an app reminder needs the target user (p2p clears
  `feed_cards/bot_time_sentive`), not just the chat. The file is written
  atomically when a pin lands, and the entry is removed when the clear is
  confirmed.
- **Startup clears the orphan set.** On startup cola best-effort clears every
  recorded pin, then drops only the entries whose clear succeeded; a failed
  clear stays for the next startup. The existing next-turn self-heal remains
  the second net. A pin cola never sees again (app uninstalled, tenant gone)
  stays — that is app-controlled platform state, and the user guide documents
  the client-side 完成 workaround.
- **One deterministic owner per chat.** `Pending` outranks `LongTurn`; within a
  class the newest turn wins. When the owning pending resolves, the next sweep
  hands the pin to the next pending in the chat immediately (no pause); with
  no pending left the sweep clears it. A new pending may retarget a live pin
  away from an older pending — the newest wait is the one needing the user
  now. A `LongTurn` `ensure` never retargets or releases a live `Pending`
  hold. Presence stays chat-wide: any interaction resets the silence clock
  for the whole chat, whichever topic it happened in.
- Rejected: FIFO (an older unanswered wait would hide a newer one), delayed
  handover (the second wait still needs the user now), and leaving map
  insertion order as the de-facto rule (nondeterministic across sweeps).

Source: #247 grilling of multi-pending precedence and #249's platform finding.

## Amendment (2026-09-21): the completed card offers 「取消置顶」

> **Superseded** by the 2026-09-21 amendment "the long-turn reminder is
> removed": with no completion TTL there is no pin to release, so the button
> (and #246) is dropped.

The Decision above leaves a completed long Turn pinned for a TTL. A user who
already came back and read the card had no way to release it (#246) — typing
anything already releases the hold, so only the silent reader was stuck.

- The final card of a completed long Turn carries a small 「取消置顶」 button
  **while its completion TTL hold is live**. It is not rendered while the turn
  is running and not rendered while a `Pending` hold exists: there the
  conversation still needs the user, and the button would imply a wait is
  dismissible.
- The click releases the `LongTurn` hold through the same generation-guarded
  path any interaction uses. If the hold is already gone (activity, a new
  turn, TTL expiry) the click is a no-op, and the next repaint no longer shows
  the button.

## Amendment (2026-09-21): message-scoped urgent and feed-card buttons deferred

Feishu's message **urgent** (`PATCH /im/v1/messages/{message_id}/urgent_app`)
is the only platform capability found that alerts a *specific* message: it
buzzes named users about a message the app itself sent. It is not adopted now;
if ever, it is a separate, explicit opt-in switch — a buzz is a far stronger
interruption than a quiet pin, and `im:message.urgent` is an app permission
release, not just a config flag.

- Probe result (live, 2026-09-21): the app has no `im:message.urgent` scope
  (`99991672`); the probe stopped there. Still unverified, and to be answered
  before any adoption: urgent on an `interactive` card, where the popup click
  lands, and p2p-specific constraints. Quotas to respect: 200 unread urgents
  per recipient (`230023`), no cancel API, and group chats require "all
  members may urgent" or the bot is an admin.
- Neither feed-card mode can address a message. The chat-entry mode's
  quick-action buttons (`im/v2/chat_button`) are deferred to an issue: their
  callback path over cola's WS connection is unverified, and one chat hosting
  several topics makes a list-level action ambiguous. The separate
  app-entry mode stays rejected (this ADR's original alternative).
- Persistent state keeps its job: urgent is a transient event;
  `time_sensitive` and **Message Pin** are the state.

## Amendment (2026-09-21): the long-turn reminder is removed; completion is a notice

The long-turn reminder — the 2026-09-19 silence threshold, its completion TTL,
and the completed card's 「取消置顶」 button — is removed. The pin surfaces are
for **waiting**, not for running turns. A Permission/Question blocks the AI
until a human acts, so the conversation must stay pinned until then: that is a
**state**, and it cannot be missed the way a message can. A long Turn is
different: it needs no attention while it runs, and its end is an **event**.
Feishu's card PATCH pushes no notification and does not bump the conversation
(only a new message does), so the event is delivered as a new message: the
**Completion Notice**, a reply to the prompt.

- p2p: notify only when the turn ran at least 5 minutes
  (`LONG_TASK_NOTICE_MS`), opt-in `[bridge] long_task_notice`, no @ mention —
  the reply itself is the notification. A short turn stays silent.
- groups: unchanged — every turn notifies (`[bridge]
  group_completion_notice`) and @-mentions the requester.
- Removed with the long-turn class: the silence clock (`note_interaction`),
  the threshold checker, the completion TTL, `CompletionPin`, the card's
  「取消置顶」 button, and the `ReminderReason` set — a live pin now always
  means a pending wait. The persistence and owner rules of the 2026-09-21
  amendments stay, scoped to pending waits.

Why not a "no-progress" (stuck) trigger: a silent long-running tool and a
truly stuck turn are indistinguishable from the card's parts, so the trigger
would false-positive on builds and test suites. The reliable "needs a human"
signals are the pending Permission/Question — and a message-scoped urgent
(the deferred capability above) would be the right event channel for a stuck
turn, not a pin.

Rejected alternatives: keeping the pin as a pure state and adding a notice
(duplicates the surfaces for no gain), and a "read receipt cancels the pin"
rule (Feishu gives bots no read receipts, and a card PATCH does not reset
unread).

## Amendment (2026-09-22): `/card` pulls the live card back to the newest position

A user who runs several commands during a long Turn ends up with the live card
buried above their replies, and the removal of the long-turn reminder
(2026-09-21 amendment) left a running Turn with no attention surface at all
(#244). The command replies themselves must still not split the chain — they
are the interaction the user is performing — so the fix is an explicit
request.

- `/card` splits the Card Chain at the command message: the previous card is
  finalized with the standard split header, and a continuation card — replied
  to the command message — becomes the tracked live card and the newest
  message. It reuses `split_card_chain` / `PendingSplit` exactly as a
  Supplement does.
- `PendingSplit` gains a kind: a Supplement's continuation carries its
  「📨 已收到补充」 receipt, a pull's carries 「⏬ 实时卡片已移到底部」. The
  continuation stays a delta handoff (2026-09-19 amendment): the status line
  plus only the content that arrives after the split.
- No separate acknowledgement message. When the conversation has no live card
  (the Turn ended, or no Session exists), cola replies one text line
  「当前没有正在运行的实时卡片」 and creates nothing.
- A pending Permission/Question hosted on the pulled card migrates to the
  continuation and its **Message Pin** follows on the next sweep — the
  Supplement split's existing path, no special case.
- Each pull costs one finalization plus one continuation message: Feishu
  cannot move a message, so re-sending the card is the only way to put it at
  the bottom.

Rejected: a 「回到实时卡片」 button on every command reply card (the text
replies — `/dir`, `/new`, `/model <name>` — cannot carry a button, so the
scenario that buries the card most often would lose it, and every reply path
would need the button), and an automatic split after every command (the
command reply must stay where the user just acted — the rule this ADR already
settled).

Source: #244 cost/benefit evaluation with the product owner; approval of the
minimal explicit-command form.

## Amendment (2026-09-24): the drain bound hands the card to an out-of-turn follow

The Decision above says the render loop stays alive while the session is still
running, and the drain's fixed bound (10 min, `turn_drain_timeout_ms`) was read
as completion: at the bound `finish` finalized the card Done from a snapshot
that could still show running tools, freezing every `⏳` panel under a `✅`
header and losing all work after the bound (#284; observed live on a Supplement
whose `task` subagents outlived the budget by 1.6 s).

- **A bound reached with the session still running is not completion.** The
  turn ends exactly as before — the inflight guard is released, so the next
  message is a normal new Turn — but the card is not finalized. It is handed to
  an out-of-turn follow that keeps the SAME accumulator and Card Chain, renders
  on the same injected poll cadence, and finalizes Done only when the session
  reports a non-busy status.
- **The follow is bounded by its own ceiling** (`turn_follow_timeout_ms`,
  default 10 min, injectable). A session still running — or a Backend still
  unreadable — at that ceiling finalizes Error: never Done under a running
  panel, never an eternal spinner. `/stop` ends the follow promptly through the
  same sticky stopped-session marker the drain observes, with the same final
  reconcile before the Done card.
- **The completion notice belongs to the follow's end** when it owns the card,
  so 「✅ 已完成」 is only ever sent for a run that actually ended. The topic
  cover is synced once at the hand-off (starting its title-retry window there)
  and kept current by the follow's render ticks, which already sync it whenever
  the server title changes. A new Turn (or an external arming) replaces the
  accumulator and the follow exits on its next tick — the replacement guard the
  external render loop already uses.
- **Done waits for the panels, not just the status.** A non-busy session whose
  accumulator still carries a `running`/`pending` panel (a crash-orphaned tool)
  keeps the follow watching instead of stamping `✅ 完成` over a `⏳`; the
  ceiling then ends it Error. `/stop` is the deliberate exception: it closes the
  card promptly, after one last render of the abort's settled tool states.
- The external-renderer arming path is NOT reused: it builds a fresh
  accumulator, which would drop the Supplement's continuation card. The follow
  lives in the Turn (`turn/follow.rs`) because the card, its chain and its
  accumulator are the Turn's own state (ADR-0050).

Rejected: raising the bound (moves the threshold, same failure mode one budget
later); not releasing the guard while busy (the bound exists precisely so a
merely-busy session cannot hold the guard).

Source: #284.
