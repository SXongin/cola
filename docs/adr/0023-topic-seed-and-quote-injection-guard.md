# Topic cover card & quote-injection guard

Creating a topic with `/topic` or `/topic --adopt` leaves the topic's identity
thin in two places: the **thread root** — the message the Feishu thread is built
around, which is what the chat-list topic entry shows as its title — and the
**seed card** — cola's confirmation that is the first message *inside* the
topic. Because Feishu reports a topic reply's `parent_id` pointing at the thread
root, cola's Quoted Context (ADR-0009) fetched and prepended that text to
**every** prompt in the topic — noise the model could not tell apart from a
genuine quote of a real message.

## Decision

- **The thread root is the Topic Cover Card**: `/topic` and `/topic --adopt`
  first send the session brief to the main Chat as an interactive card, then
  `reply_in_thread` on **that card** — so the card is the thread root and the
  chat-list topic entry shows it permanently. The old root (the user's `/topic`
  command message) was the least informative message in the topic and is not
  editable; a bot card is. If the cover send fails, the thread falls back to
  anchoring on the command message (the pre-change behavior) and no cover title
  is recorded, so nothing is ever patched onto a user message.
- **Persist the thread root as `topic_root`** on `SessionEntry` for cola-created
  topics (`/topic`, `/topic --adopt`): the cover card's `message_id` (or the
  command message's on fallback), alongside the existing `topic_anchor` (the
  seed card inside the topic).
- **Injection guard**: in the Quoted Context path (`handle_prompt`), skip
  fetching/injecting the parent when `parent_id` equals the active session's
  `topic_anchor` **or** `topic_root` — i.e. the messages cola itself placed in
  the topic: the seed card, the `/switch` / `/attach` adopt card inside an
  existing topic (`adopt_session`), and the cover card (or command message on
  fallback) that is the thread root. A manually-created topic that never
  received a cola card keeps both `None`, so the guard is silent there and the
  user's own subject message still injects — that is valuable context, not
  noise.
- **Enrich the seed card**: the seed card (first message inside the topic) is a
  short hint — "请在本话题内回复" — because the full session brief lives on the
  cover card at the top of the thread. Purely human-facing: the guard suppresses
  both `topic_root` and `topic_anchor`, so neither card's text is ever injected,
  and richness costs no tokens. Feishu card limits (30 KB, 200 elements) leave
  ample headroom.
- **Title sync after the first turn**: OpenCode auto-generates a session title
  after the first exchange (and `/name` changes it later). After each completed
  turn cola compares the server title (`GET /session/{id}`) against the one
  recorded in memory (`core.cover_titles`) and, when it changed, patches the
  cover card in place via `update_message` — the chat-list topic entry follows.
  The recorded title is in-memory only; after a restart the next completed turn
  re-syncs once (same content, harmless).

## Why

- A plain topic reply is indistinguishable from an explicit quote at the event
  level (both may carry `parent_id`); cola's only reliable signal is *what* the
  parent is. `topic_root` and `topic_anchor` are cola's own topic-creation
  boilerplate, so suppressing exactly those two drops no user content.
- The chat-list topic entry shows the thread root as its title, so making the
  root a bot card is the only lever that survives replies: the latest-message
  preview changes every turn, but the title stays the cover card's content —
  and an interactive card can be patched in place later.
- The `parent_id == root_id` discriminator was rejected: it also fires in
  manually-created topics, where the root is the user's subject message and
  injecting it is desirable.
- Blindly skipping all topic injections (cc-connect's `threadIsolation`) was
  rejected: it would also drop genuine quotes of real messages inside topics.
- Sender-based filtering (skip bot-sent parents) was rejected: it needs an extra
  message fetch per prompt and would wrongly skip explicit quotes of a bot card.

## Risks / open questions

- **Feishu's exact parent target is not fully settled by docs**: the "always the
  thread root" wording vs. observed null `parent_id` on plain topic replies. The
  dual guard is correct under both interpretations, but a real event payload
  would confirm which message the guard actually matches (see the
  `WS event payload:` log at src/feishu/ws.rs).
- **Legacy `SessionEntry`s**: entries persisted before this change default
  `topic_root` to `None` (serde default), so pre-existing cola-created topics
  fall back to the `topic_anchor`-only guard until the topic is recreated.
  `topic_root` could be recovered by listing the thread and taking its earliest
  message if the noise shows up in practice.
- **Session 404-recreate keeps the topic identity**: when a mapped session no
  longer exists and cola recreates it (`create_fresh_session`), the per-session
  overrides reset but `topic_root`/`topic_anchor` survive — they are Feishu
  message ids, not session state, so the guard keeps working after the recreate.
  The recorded cover title moves to the recreated session id, so the title-sync
  hook keeps patching the cover card too.
- **The brief renders the model** as `providerID/modelID@variant` when the
  adopted session carries one (fresh `/topic` sessions have none and omit it).