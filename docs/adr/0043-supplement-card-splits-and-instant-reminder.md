# Supplement-triggered Card Chain splits and Instant Reminder pinning

> **Amended by ADR-0059, ADR-0074 and ADR-0109**: the inflight guard covers the
> Turn's whole post-prompt path — a message arriving during the now-merged
> follow is a Supplement, and the Turn's own task owns the unbounded path — and
> a failed Supplement splits only while a held guard still owns the chain. The
> earlier hand-off that passed the card to an out-of-turn follow at a bound is
> gone (ADR-0074): the no-total-budget conclusion stands, and the Turn owns the
> whole path.
>
> **Amended by ADR-0100 (2026-11-06)**: `/card` always answers — with no renderer
> owning the card it reads the session once and posts a read-only Session Card —
> and a pending request's Message Pin loses its claimed-snapshot host arm.

## Context

Feishu cannot move a message: a card update (PATCH) leaves the card where it was
sent, and the conversation-list preview always shows the newest message. A user
message sent while a Turn is in flight (a **Supplement**) therefore lands *below*
the live card, and the old text acknowledgement (「📨 已收到补充…」) only added a
second message below it — the reader's viewport drifts away from the streaming
card with nothing they can do. This ADR reuses the existing **Card Chain** for
position as well as size (a Supplement forces a split, so the continuation card
is the newest message), keeps the render loop alive across a supplement that
starts a new Turn, and adds Feishu's **Instant Reminder** and **Message Pin** to
keep the conversation and the waiting card in view.

- **A Supplement is not a Turn.** `handler.rs` intercepts a message whose
  session has a Turn in flight and forwards it via `prompt_async`; the Backend
  merges it into the running Turn if that loop is still alive, or starts a new
  Turn if it has already exited. Cola cannot tell which at send time, and the
  answer does not change what the user should do.
- **The new-Turn case rendered nowhere before this ADR.** The turn's render loop
  had exited, and external-message sync skips Cola-Authored Messages
  (ADR-0026), so a supplement that started a new Turn silently lost its reply.
- **Commands land below the card too.** They are dispatched before the busy
  check and answer with their own message/card; they are the interaction the
  user is performing right now, so displacing them with a re-sent live card
  would fight the user.
- **Feishu's list layer offers two things.** An app feed card
  (`im:app_feed_card:write`) is a *separate* list entry whose only link is a
  plain https URL (the chat-open AppLink takes `openChatId` only, so no
  message-specific link exists), and Instant Reminder (`time_sensitive`) pins an
  existing conversation but carries no reason text.

## Decision

### A Supplement splits the chain

- **A Supplement splits the Card Chain.** The continuation card is the reply to
  the supplement message, carries the delta, and becomes the live card; the
  previous card is finalized with the standard split header. No
  new concept, no new card type, no recall. The split is a **delta handoff**,
  exactly like a size split: the previous card is finalized with everything
  before the split, and the continuation carries its receipt line plus only the
  content that arrives after it. The newest card is therefore not
  self-contained — accepted, because the cards are adjacent messages and reading
  them in order is the normal path.
- **No text acknowledgement.** The continuation records the supplement with a
  receipt line (「📨 已收到补充」). The wording is neutral: merge vs. new Turn is
  only observable after the fact, and both outcomes end with the reply rendered
  on the continuation card, so the receipt is always true.
- **A merged supplement always splits** — no coalescing window. A failed steer
  is the exception (ADR-0109): a merge attempt that fails
  is unconfirmed, so if a held guard still owns the chain it splits and the
  continuation carries 「⚠️ 补充消息未确认送达，请留意当前回复」
  (`SplitKind::SupplementFailed`); a race that lost the guard keeps the chain and
  reports a plain notice — a continuation would be a live card handed to nobody.
  The split's admission re-checks the card's state under the same lock an ending
  is applied with, so a Turn that settles between the ownership read and the
  split either stamps the continuation or is seen as terminal and refuses it. A
  merged steer whose chain ends in that window reports 「补充消息已提交，但当前卡片
  无法继续」, never silently. Both messages say **unconfirmed**, never
  undelivered: an ambiguous Codex response may still have landed and keeps its
  marker for reconciliation (ADR-0092), so cola must not invite a resend.
- **Commands never split the chain.** Their replies are what the user is looking
  at; inserting the live card below them would push them away.
- **Keep rendering after the prompt call returns.** The render loop stays alive
  while the session is still running or an unanswered Cola-Authored supplement
  is newer than the turn anchor, and renders into the live (continuation) card.
  This closes the new-Turn gap for both merged and split cases. The post-prompt
  path is one unbounded loop owned by the Turn's own task (ADR-0074): there is no
  hand-off and no total budget, the inflight guard is held for the whole path,
  and the one grace bounds only the states where nobody can act — lost contact,
  an unreconcilable live panel on an idle Session, and the unreceived waiting
  line. A readable, merely-long run is never clocked: a bound reached with the
  session still running is not completion. A wait is unbounded by construction —
  the accumulator's awaiting state keeps naming it and the follow never consults
  the pending records — and `/stop` rejects the requests it leaves behind
  (ADR-0038 rule 7). The follow remains only for the paths
  with no Turn task of their own (the recovery re-attach and the Wake
  continuation); it lives in the Turn (`turn/follow.rs`), not the
  external-renderer arming path, because the card, its chain and its accumulator
  are the Turn's own state (ADR-0050) and a fresh accumulator would drop the
  continuation card. A new Turn (or an external arming) replaces the accumulator
  and the follow exits on its next tick, the same replacement guard the external
  render loop uses. The render loop keeps the **topic cover** current, syncing
  it whenever the server title changes.

### The stop terminal

`CardState::Stopped` (header 「⏹ 已停止」, grey) is the deliberate stop's own
terminal, neither 完成 nor 出错.

- **It is terminal everywhere.** It is terminal for every running/probe check
  (`is_running`, the `/card` pull, the header phase timer), so a stopped card is
  never treated as live; it is excluded from the awaiting override (a
  still-pending block cannot restyle it) and renders no retry button.
- **A terminal header wins over the awaiting override.** Error / Done / Stopped
  take precedence over the header's 「等待你的授权/回答」, so a dead card never
  reads as waiting for anyone.
- **The abort is not a failure.** The server records the interrupt as an error on
  the turn's transcript; a turn whose session carries the stopped marker
  (`waits.stopped_sessions`) is classified `Stopped` and that error text is never
  written to the card. A stopped turn rejects its leftover pending
  Permission/Question requests (ADR-0038 rule 7) even when the stop recorded no
  error text, because the abort strands whatever was pending.
- **Both finalization paths classify by the marker**: `finish` for the in-turn
  path — which also never hands a stopped card to the follow — and the
  out-of-turn follow's stop branch, which closes the card promptly after one last
  render of the abort's settled tool states. Each re-reads the marker at its last
  await boundary, so a stop that lands while a tick or a finalization is in
  flight still wins. The stop command records the marker BEFORE the interrupt
  round-trip (rolling back only a marker that call itself inserted, when the
  interrupt fails), so the abort's own server-side settling cannot beat the
  marker.
- **The completion notice follows the card's real terminal**: 「⏹ 已停止。」,
  never 完成 or 出错. Both fallback endings take the Error notice, and a run
  still going server-side is never announced as complete.

### Reminders and pins

- **Pin the conversation while attention is needed.** Instant Reminder
  (`time_sensitive`) is enabled while a Permission/Question is pending and
  cleared when the wait resolves. Pin surfaces are for **waiting**, not for
  running turns: a blocked request must stay pinned until a human acts — that is
  a state and cannot be missed the way a message can. A long Turn needs no
  attention while it runs, and its end is an **event**, delivered as a new
  message: the **Completion Notice**, a reply to the prompt (p2p: only when the
  turn ran at least 5 minutes, `LONG_TASK_NOTICE_MS`, opt-in
  `[bridge] long_task_notice`, no @ mention; groups: every turn, `[bridge]
  group_completion_notice`, @-mentioning the requester), sent under ADR-0074's
  delivery gate and repaint delay. The long-turn class — the silence threshold
  and clock (`note_interaction`), the completion TTL, `CompletionPin`, the
  card's 「取消置顶」 button and the `ReminderReason` set — is removed with it: a
  live pin now always means a pending wait.
- **Pinning is best-effort**: failures log only, a missing scope disables nothing
  else, and p2p/group share the logic.
- **Pins are generation-scoped** (in memory): every pin carries the turn
  generation that owns it, and a clear from an older generation is a no-op
  against a newer pin, so a stale clear can never unpin the wait that superseded
  it.
- **Pins are persisted.** Each live pin is mirrored to a small file beside
  `sessions.json` as `{chat_id, is_group, user_ids}` — `user_ids` because
  clearing an app reminder needs the target user (p2p clears
  `feed_cards/bot_time_sentive`), not just the chat — written atomically when a
  pin lands and removed when the clear is confirmed. On startup cola best-effort
  clears every recorded pin, then drops only the entries whose clear succeeded; a
  failed clear stays for the next startup, and the next-turn self-heal remains
  the second net. A pin cola never sees again (app uninstalled, tenant gone)
  stays — that is app-controlled platform state, and the user guide documents
  the client-side 完成 workaround.
- **One deterministic owner per chat.** The newest pending wins; when the owning
  pending resolves, the next sweep hands the pin to the next pending immediately
  (no pause), and with no pending left the sweep clears it. A new pending may
  retarget a live pin away from an older pending — the newest wait is the one
  needing the user now. Rejected: FIFO (an older unanswered wait would hide a
  newer one), delayed handover, and map insertion order (nondeterministic across
  sweeps).
- **Waiting cards are message-pinned.** Feishu's Pin message API
  (`POST /im/v1/pins`) pins a specific message into the chat's pinned-message
  list, so a wait in an older topic has an in-list surface (the
  conversation-level reminder previews the chat's newest message, which may be
  anything). A pending Permission/Question's **host card** — the live card
  carrying its Interaction Block, or the standalone card it was sent as (ADR-0028's
  claim registry is deleted, ADR-0100) — is pinned while the request waits and
  unpinned once it leaves the pending list. A card rendering both kinds is
  pinned once (reference-counted by message); the last wait off it unpins it. A
  card replaced by a re-host or a split moves its pin. Only waiting requests pin
  a message: a long Turn pins none. Cola pins each (request → card) transition
  once: an unchanged wait is never re-asserted, so a card the user unpins by hand
  is not fought. A failed pin retries while the wait lasts; a failed unpin stays
  tracked; a failed pin never fabricates a later unpin. A directory whose list
  call failed is unknown, never resolved (#130). No read-back reconciliation: the
  pin list's consistency lags writes (measured 2026-09-19: a `DELETE` still
  listed ~20 s later), so cola tracks its own transitions in memory; a pin
  orphaned by a crash stays until the user removes it — its message id is
  unknowable after a restart.
- **Naming**: `bridge/pin.rs` only ever implemented `time_sensitive`, so it is
  renamed `bridge/reminder.rs` (`ReminderState`/`ReminderReason`/`ReminderTarget`),
  and the message registry is `bridge/message_pins.rs` (`MessagePins`) —
  "reminder" means the conversation-level state, "message pin" the platform's
  pinned-message list.

### `/card` pulls the live card, or reads the session

- **With a renderer owning the card**, `/card` splits the Card Chain at the
  command message: the previous card is finalized with the standard split
  header, and a continuation card — replied to the command message — becomes the
  tracked live card and the newest message, carrying
  「⏬ 实时卡片已移到底部」 as its receipt. It reuses
  `split_card_chain` / `PendingSplit` exactly as a Supplement does, and the
  continuation stays a delta handoff. A pending Permission/Question hosted on the
  pulled card migrates to the continuation and its **Message Pin** follows on
  the next sweep — the Supplement split's existing path, no special case. Each
  pull costs one finalization plus one continuation message.
- **With no renderer owning the card**, `/card` reads the session once and posts
  a read-only **Session Card** in `CardState::Idle`, which joins no Card Chain
  (ADR-0100; the 「当前没有正在运行的实时卡片」 notice is retired — `/card`
  always answers). A second `/card` with nothing in between stamps the previous
  idle card `TakenOver` while it is still the session's current card; a Turn that
  ran in between has superseded it and is left alone. When the read reports busy
  or a Waiting turn, `/card` posts the same card and hands it to the existing
  external renderer instead of freezing it, so the continuation streams into the
  card the reader just asked for. A **cola-authored** newest message is the
  exception: that run is cola's own, so the card stays static and the hand-off
  stays with the Chain Record and Session Sync (ADR-0063, ADR-0071).
- Rejected: a 「回到实时卡片」 button on every command reply card (the text
  replies cannot carry a button, so the scenario that buries the card most often
  would lose it, and every reply path would need it) and an automatic split after
  every command (the reply must stay where the user just acted).

### Configuration

- **One config switch, off by default:** `[bridge] instant_reminder = true`
  turns Instant Reminder on. Absent or `false` means off — an upgrade never
  changes notification behavior without consent. The split behavior itself is not
  configurable.
- **Not adopted:** message-scoped **urgent** (`PATCH …/urgent_app`; it needs the
  `im:message.urgent` app permission, has no cancel API and per-recipient quotas,
  and its behavior on an interactive card is unverified), the feed-card quick
  actions (their callback path is unverified, and one chat hosting several
  topics makes a list-level action ambiguous), and a "no-progress" (stuck)
  trigger for the reminder — a silent long-running tool and a truly stuck turn
  are indistinguishable from the card's parts, so it would false-positive on
  builds and test suites.

## Why

- Reusing the Card Chain keeps one mental model for both split causes: "the live
  card is always the newest card of the chain", whether it grew too big or was
  overtaken by a user message.
- The receipt-on-the-card replaces a second message with zero extra sends and
  keeps the acknowledgement where the reader is already looking.
- A pin is state, a completion is an event: the pin cannot be missed, and the
  event needs a new message because a card PATCH pushes nothing and does not
  reset unread (Feishu gives bots no read-receipt signal to cancel a pin on).
- Keeping the render loop alive costs nothing when nothing is pending (the loop
  exits on idle) and rescues the one path where a supplement would disappear
  into the store.

## Consequences

- A supplement produces one extra message (the continuation card) plus a
  finalized prior card; the chain's earlier cards stay as 「部分完成，继续中」
  history.
- The list preview is the status surface: while the live card is newest it shows
  the card title (等待你的授权/回答 · 运行中 · ✅ 完成); when the newest message
  is a command reply, it shows that reply — accepted, it is what the user just
  did. A permission-only wait previews 等待你的授权, a question-only wait
  等待你的回答, and only both pending at once keep 等待你的授权/回答 (ADR-0014).
  The pin carries no reason; the preview does.
- Pin disappears meaning "no longer needs you", not "just completed".
- The grace, the read timeout and the notice delay are internal constants; the
  bridge tests inject tiny values through the pin state's / external flow's
  atomics, so the whole lifecycle runs in milliseconds without a real Feishu
  tenant.
- Group chats can only split on messages cola receives (@-mentions) until the
  sensitive `im:message.group_msg` scope is ever granted; group behavior is
  untested in the current private model.
- GLOSSARY: **Card Chain** is broadened to cover position-driven splits, and
  **Supplement**, **Instant Reminder** and **Message Pin** are its vocabulary.

Origins: #244, #246, #247, #249, #284, #386, #391 (spec), #394, #602, #945, #999,
#1056; ADR-0014, ADR-0026, ADR-0038, ADR-0050, ADR-0059, ADR-0063, ADR-0071,
ADR-0074, ADR-0092, ADR-0100, ADR-0109.
