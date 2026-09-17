# Collapsible panel fold state rides a stable element_id

Every flush PATCHes the whole card JSON (`update_message`), and a collapsible
panel's open/closed state lives in the client: toggling a panel fires no
callback (折叠面板 has no 回传交互), so the server can never learn or restore the
operator's choice. Without an identity, the client's local state followed the
panel *position* — inserting a new tool panel before an expanded panel collapsed
it and opened the newcomer. This ADR names every panel with a JSON 2.0
`element_id` derived from the timeline entry it renders. Observed in a live
Turn: with the ids, the todo tail stayed expanded while 11 panels were inserted
before it. A Card Chain split still starts its continuation card folded; that is
accepted, because a new message carries no client state and the folded header
keeps the plan size and status counts readable.

## Context

- **The card is replaced, not edited.** `flush_card` PATCHes the message's full
  `content` (`src/feishu/client.rs:463-475`), so the Feishu client re-renders
  the whole card from JSON on every flush (~every 2 s while a turn runs).
  `expanded` defaults to `false` in `collapsible_panel_chunks`
  (`src/feishu/card/shell.rs:337-357`).
- **No toggle callback.** The collapsible panel is not an interactive component:
  toggling it produces no event, so cola cannot know, remember, or re-emit the
  operator's choice. Whatever state survives an update is the client's own.
- **Position is not stable; identity is.** The timeline is ordered by server part
  time, and a part rendered late is inserted at its key's position
  (`insert_kind`, `src/bridge/streaming.rs:641-661`) — an item's index can change
  after it was first rendered. Each entry does have a stable identity:
  `TimelineItem::seq`, assigned once at insertion.
- **Observed failure.** Expanding the todo tail collapsed it as soon as a new
  tool panel was inserted before it, and the new panel appeared expanded — the
  client's local override was keyed to the position, not the panel.

## Decision

**Every collapsible panel names itself with an `element_id` derived from the
timeline entry it renders.**

- Tool and reasoning panels: `tool_{seq}` / `reason_{seq}`
  (`build_card_inner`, `src/bridge/streaming.rs:957-973`).
- The todo tail: the fixed `todo` (`src/bridge/streaming.rs:1001`) — it renders
  once per card in the tail and is not a timeline entry.
- `seq` is assigned once, in `insert_kind`, and never reused or renumbered.
- One-shot cards (the Session Snapshot) pass `None`: nothing re-renders those
  cards in place.
- **A Card Chain split's continuation starts folded.** Accepted: a new card is a
  new message with no local state to inherit, and the todo panel's header still
  carries the plan size and the non-empty status counts.

## Why

- The state belongs to the client; the id is the only handle cola has without a
  callback. Deriving it from `seq` keeps it stable under insertion, merging and
  reordering — which an index-derived id would not survive.
- The alternative — forcing `expanded: true` whenever the list is live — would
  make the panel impossible to keep folded: the next flush reopens it.

## Consequences

- Panel ids are load-bearing for the panel UX: `TimelineItem::seq` must never be
  reused, and no id may be derived from a body position.
- The client's keying is undocumented and verified by observation, not contract.
  If a client version stops honouring `element_id`, the old position-jump
  returns; the folded default keeps that non-fatal.
- Fold state is per card and per message: a split (a long turn) or a new turn
  cannot inherit it, and the reader re-expands once. ADR-0038's repaint handles
  are not involved.

## Alternatives considered

- **Position-derived ids** (`tool_{index}`): rejected — a late part shifts the
  ids of everything after it, moving the state to the wrong panels.
- **Always-expanded todo panel**: rejected — the operator loses the choice to
  fold it, since every flush reopens it.
- **cardkit card entities with element-level updates**: the documented partial
  update path, but message cards are not card entities; adopting it reworks the
  send/update chain, and Feishu's component limit still forces splits. Deferred,
  not rejected.
