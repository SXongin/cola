# 03 - Wire the snapshot into every adoption entry point

Status: ready-for-agent
Type: task
Blocked by: 01, 02

## What to build

Make each session-activation path end in exactly one Session Snapshot card
(ADR-0028 "one card per activation"), replacing today's confirmations, and apply
the suppression rule. This is the behavioural core of the feature.

## Scope

Entry points in `src/bridge/command.rs` (+ `handler.rs` where the switch card
lives — see ticket 04 for the card buttons):

- **Lobby text adopt** — `adopt_session` when `kind != Topic`
  (command.rs:1920-1927): the snapshot reply replaces the 「已接管会话…」 text.
- **In-topic `/switch`/`/attach` adopt** — `adopt_session` when `kind == Topic`
  (command.rs:1882-1896): the 「📎 已接管…」 `reply_in_thread` anchor becomes the
  snapshot card. Its returned message id is stored as `SessionEntry.topic_anchor`
  so permission/question fallback routing (`resolve_topic_anchor`,
  `src/bridge/pollers.rs`) still resolves inside the topic (ADR-0023).
- **`/topic --adopt`** — `create_topic_and_map_adopted` /
  `open_topic_for_session` (command.rs:1738-1786): the Topic Cover Card stays the
  root in the main chat; the snapshot is the first bot message inside the new
  topic and the persisted anchor.
- **Re-`/switch` to an already-mapped session** — the mapped hit branch of
  `handle_switch` (command.rs:1413-1428): apply suppression (ticket 01's
  predicate). Content to report → snapshot; nothing to report → keep today's
  one-line 「已切换/Switched to」 text, no card.
- First-time adoption ALWAYS snapshots (even an empty/idle session → card with
  header and no/empty sections). `/new` and the bare `/topic` form are untouched
  (no adoption).
- Sequence for in-topic emission: send the snapshot card, capture its message id,
  THEN persist the `SessionEntry` with that anchor, so the gather step can read
  pre-adoption state and the anchor is correct.
- Update docs/help if they quote the old adoption confirmation strings, and any
  tests asserting 「已接管会话…」 / 「Switched to…」 text that this changes.

## Acceptance criteria

- [ ] Every activation above ends in exactly one snapshot card (or, in the
      suppressed re-switch case, the one-line text ack) — never both, never none
      for a first adopt.
- [ ] In-topic adopt: the snapshot is the topic anchor; fallback cards and
      permission/question standalone delivery still route inside the topic.
- [ ] `/topic --adopt`: cover root unchanged in the main chat; snapshot is the
      new topic's first bot message and anchor.
- [ ] Suppression fires only per the predicate; re-switch with external newness /
      busy / pending shows a snapshot.
- [ ] `cargo test --workspace --locked` green, clippy/fmt clean.
