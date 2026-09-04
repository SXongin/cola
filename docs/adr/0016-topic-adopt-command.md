# `/topic --adopt`: open a new Feishu topic around an existing session

How a user opens a topic whose backing session already exists, instead of
creating a fresh session and only later switching it.

## Context

`/topic <dir> [name]` (ADR-0006) always creates a **new** OpenCode session. The
topic single-session gate (ADR-0007, issue 07) then forbids `/switch`/`/attach`
**inside a topic that already has a session**, so there is no one-gesture way to
get an existing session into a UI-separated topic. The only workaround is to
hand-create a Feishu topic and `/switch <kw>` inside its never-had-a-session
state — two manual steps that bypass `/topic` entirely.

The pieces already exist: `adopt_session` (src/bridge/command.rs) is already
topic-aware (it anchors fallback cards via `reply_in_thread` when the target
conversation is a topic), and `/topic` already creates a topic via
`reply_in_thread` (command.rs). The missing operation is composing the two in
one gesture: resolve the target session, then create the topic around it.

## Decision

Extend `/topic` with an adopt mode:

- **`/topic --adopt <keyword> [--force]`** — resolve an existing session by the
  `/attach` resolution order (exact id → unique id-prefix → unique title
  substring; whole remaining arg is the keyword, so multi-word titles match),
  create a real Feishu topic anchored on the command message, and map the
  adopted session to the new topic's `ThreadKey`. No name-in-gesture: renaming
  stays a `/name` concern (matches `/attach`, keeps one match semantic).
- **`/topic --adopt` (no keyword)** — pop the session-picker card (reuses the
  `/switch` card) with a per-row "建话题接管" button.
- **`--force`** — a standalone token, symmetric with `/switch <id> --force`.
  The text form honors it (steals a session mapped to another thread, the other
  thread becomes sessionless); the card form does not (occupied sessions are
  rejected with a Toast pointing at the text form). Users learn about `--force`
  on first rejection, exactly as with `/switch` today.
- **Child sessions** are always rejected (`parentID` set), matching OpenChamber
  which does not allow driving sub-task sessions. The server technically allows
  `POST /session/{id}/message` on a child, but it is a task-derived temporary
  context; cola refuses with a hint.
- **Stealing an actively-driven session** (one currently streaming in another
  thread) is allowed with `--force` and gets no extra check — the already-
  accepted ADR-0008 risk note covers the interleaving.
- The new topic is created fresh, so the ADR-0007 "already has a session" gate
  does not apply; the gate still blocks `/topic` itself from inside another
  topic (no nesting).

## Why

- One gesture, one semantic: "open a topic whose session is X" — the session is
  a parameter of topic creation, not a separate later step.
- Reuses both existing primitives (`reply_in_thread` for topic creation,
  `adopt_session`'s topic-aware adoption) plus the `/attach` resolution order —
  no new mechanisms, only one composition.
- `--force` symmetry and "learn on rejection" keep the command family
  predictable against `/switch`.

## Risks / open questions

- **Card path needs `open_message_id`**: `extract_card_action_value`
  (src/feishu/ws.rs) reads the card's own message id from the schema 2.0
  callback's `event.context.open_message_id` (with a fallback to
  `event.action.open_message_id` for older shapes) and threads it into the value
  it hands the handler. The card "建话题接管" button relies on that id so the
  handler can `reply_in_thread` off the card message itself.
  ✅ Resolved by `fix(feishu): read card open_message_id from event.context`.
- **Resolution ambiguity** (multiple hits) lists candidates and points at the
  full id, matching `/attach`; no silent auto-adopt of the wrong session.