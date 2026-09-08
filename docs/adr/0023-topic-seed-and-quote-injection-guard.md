# Topic seed card & quote-injection guard

Creating a topic with `/topic` or `/topic --adopt` leaves two messages that are
thin by nature: the **thread root** — the user's `/topic ...` command message the
Feishu thread is built around (it stays in the main Chat) — and the **seed
card** — cola's one-line `📌 已创建会话...` confirmation that is the first
message *inside* the topic. Because Feishu reports a topic reply's `parent_id`
pointing at the thread root, cola's Quoted Context (ADR-0009) fetched and
prepended that thin command text to **every** prompt in the topic — noise the
model could not tell apart from a genuine quote of a real message.

## Decision

- **Persist the thread root as `topic_root`** on `SessionEntry` for cola-created
  topics (`/topic`, `/topic --adopt`): the `message_id` of the command message
  the topic was anchored on, alongside the existing `topic_anchor` (the seed
  card inside the topic).
- **Injection guard**: in the Quoted Context path (`handle_prompt`), skip
  fetching/injecting the parent when `parent_id` equals the active session's
  `topic_anchor` **or** `topic_root`. Manually-created Feishu topics leave both
  `None`, so the guard is silent there and the user's own subject message still
  injects — that is valuable context, not noise.
- **Enrich the seed card**: the `/topic` / `/topic --adopt` confirmation card
  becomes a session brief — title, directory, agent, model, session-id tail, git
  branch — instead of one thin line. Purely human-facing: because the guard
  suppresses both `topic_root` and `topic_anchor`, the enriched card's text is
  never injected, so richness costs no tokens. Feishu card limits (30 KB, 200
  elements) leave ample headroom.

## Why

- A plain topic reply is indistinguishable from an explicit quote at the event
  level (both may carry `parent_id`); cola's only reliable signal is *what* the
  parent is. `topic_root` and `topic_anchor` are cola's own topic-creation
  boilerplate, so suppressing exactly those two drops no user content.
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
- **The brief renders the model** as `providerID/modelID@variant` when the
  adopted session carries one (fresh `/topic` sessions have none and omit it).