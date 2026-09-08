# Recent Directories rows open a new topic (建话题)

The `/dir` Recent Directories card gains a per-row 建话题 button, mirroring the
`/switch` card's two-button rows: one tap creates a fresh session in that
directory wrapped in a brand-new Feishu topic — the card equivalent of
`/topic <dir>` — instead of typing the directory by hand. Topic creation from a
card is only valid at chat top level; card ops get a runtime nesting guard, and
the `/switch` card's existing 建话题接管 op is retrofitted with the same guard.

## Context

- `/topic <dir>` already creates a new session in `<dir>` inside a new topic,
  but the user must type the directory path every time; bare `/topic` only
  reopens the current project. The Recent Directories card (`/dir`, no args)
  is the no-typing picker, but its rows only re-root into the current
  conversation (`op: "pick"` → new session in the lobby).
- The `/switch` card rows already carry two buttons (adopt + 建话题接管,
  ADR-0016) — the established shape for "do the main thing, or do it inside a
  new topic". The `/dir` card is the odd one out with a single button per row.
- Topic creation must not nest: `/topic`, `/topic --adopt` and
  `/topic --adopt`'s text forms all refuse to run inside a topic. The switch
  card's card-form 建话题接管 op never re-checked this at button level — from a
  never-bound topic (where `/dir`/`/switch` cards may legally open, ADR-0007)
  it could be clicked, risking a nested topic or a confused mapping.
- Card actions receive the card's own `message_id` via `open_message_id`
  (`extract_card_action_value`, ws.rs), which is what an in-topic anchor needs.

## Decision

- Each `/dir` row gets a second button 建话题 next to 切换到这里/✅ 当前 —
  two equal columns exactly like `switch_card_row` (including on the row
  marked 当前, where it equals bare `/topic` in the current project).
- 建话题 semantics: create a **NEW** server session in that directory and map
  it to a brand-new topic via the `/topic` pipeline (`open_cover_topic`: cover
  card at chat top level, in-topic anchor, `topic_anchor`/`topic_root`
  persisted). The current conversation is untouched. It is NOT an adopt —
  no existing session is reused or moved; reuse of an existing session stays
  the job of the `/switch` card's 建话题接管.
- Card-only affordance: no `/dir <path> --topic` text flag. Keyboard users who
  want to type keep `/topic <dir> [name]` (which also supports naming).
- Feedback: rebuild the `/dir` card in place + short toast naming the topic's
  directory basename. Failures mirror existing paths: missing
  `open_message_id` → toast; no `thread_id` back from Feishu (chat doesn't
  support topics) → toast guiding to `/dir <path>` or a manual topic; session
  creation error → toast.
- **Nesting guard**: the card op is rejected at runtime when the card lives in
  a topic thread (`thread_id != chat_id`), with a toast telling the user to go
  back to the main conversation. The same guard is retrofitted onto the switch
  card's 建话题接管 op, closing the latent inconsistency with the text forms.

## Why

- Kills the typing round trip: any recent directory becomes a topic in one
  tap, and the current directory can be isolated into its own topic from the
  same card — the two gestures the user combines by hand today
  (`/dir` then `/topic <dir>`, typed).
- Mirroring the `/switch` card keeps one mental model: the second button on a
  row always means "open this in a new topic".
- A runtime guard (rather than hiding the button in topic context) keeps the
  card builder context-free and also covers stale cards: a never-bound topic
  can bind itself via the row's left button and then click 建话题 on the same
  card — the guard rejects that too.

## Alternatives considered

- **Adopt the directory's most-recent session into the topic** (mirroring
  建话题接管's semantics per directory): conflicts with session ownership —
  recent directories often belong to other chats' topics, and the card has no
  `--force`. Always-new matches both `/dir` and `/topic`'s default behavior.
- **Text flag `/dir <path> --topic`**: redundant with `/topic <path>`; cards
  are the no-typing surface and the only place recents are listed.
- **Hide 建话题 when the card renders inside a topic**: needs the builder to
  know the conversation context; the runtime guard covers the same cases plus
  staleness for less code.

## Consequences

- Sessions accumulate per click, exactly as repeated `/dir` picks and repeated
  `/topic` calls already do — no dedup.
- `/topic`'s new-session creation body and the card op must share one helper so
  cover/anchor/mapping behavior cannot drift (mirroring
  `create_topic_and_map_adopted` for the adopt path).
- `/help dir`, the user guide and the card row docs need the new affordance.
