# 01 - `/dir` card 建话题 button + nesting guard retrofit

Status: ready-for-agent
Type: task
Blocked by: none

## What to build

Add a per-row 建话题 button to the `/dir` Recent Directories card (ADR-0025):
one tap creates a fresh session in that directory inside a brand-new Feishu
topic — the card equivalent of `/topic <dir>`. Guard card-form topic creation
against nested topics (chat top level only), and retrofit the same guard onto
the `/switch` card's existing 建话题接管 op.

## Scope

- **Card layout** (`src/feishu/card.rs`):
  - `dir_card_row`: one button per row today (op `pick`, single auto-width
    column). Restructure to the `switch_card_row` shape: a `column_set` with
    `flex_mode: "bisect"` and two columns — left 切换到这里 / ✅ 当前
    (current `pick` behavior, op unchanged), right 建话题 (new op `topic`).
    Both `type: "default"` unless the row is current (keep ✅ 当前 styling).
  - The 建话题 button carries the routing payload: `action: "dir"`,
    `op: "topic"`, chat/thread ids, `directory`, like `pick` does today.
- **Card action** (`src/bridge/handler.rs`, `handle_dir_card_action`):
  - Dispatch on the new `op: "topic"`.
  - **Nesting guard**: if the card lives in a topic thread
    (`thread_id != chat_id` from the value — lobby keys use `chat_id` as their
    own `thread_id`), toast a rejection and do nothing. Suggested copy:
    `话题里不能开话题，请回主对话操作。`
  - Require `open_message_id` (already threaded into values by
    `extract_card_action_value`, src/feishu/ws.rs); missing → toast
    `无法创建话题（缺少卡片消息引用）` (same as the switch card's op).
  - Reject nothing else: rows always create a NEW session; no ownership check
    is needed (no adoption).
- **Shared topic pipeline** (`src/bridge/command.rs`):
  - Extract the new-session-topic creation body currently inside
    `Command::Topic`'s handler (create session → `open_cover_topic` with
    `topic_cover_text` → persist the topic `SessionEntry` with
    `topic_anchor`/`topic_root` → `record_cover_title` → invalidate cache) into
    a `pub(crate)` helper taking the already-created session + display name
    (display name = directory basename for the card; `/topic <dir> <name>`
    keeps its name path). The text handler and the card op must share it so
    cover/anchor/mapping cannot drift (mirrors `create_topic_and_map_adopted`
    for the adopt path).
  - Card op flow: `create_session(Some(dir))` → shared helper → on `None`
    thread_id toast `当前会话不支持创建话题，请改用 /dir <目录> 或在飞书里手动创建话题。`-ish;
    on session error toast `创建会话失败：{e}`.
  - Success feedback: rebuild the `/dir` card in place
    (`build_dir_card_for`) + toast `已建话题并新建会话（<basename>）`.
- **Guard retrofit**: in `handle_switch_card_action`'s `topic_adopt` arm
  (src/bridge/handler.rs, ~line 1044), add the same
  `thread_id != chat_id` rejection before creating the topic. This closes the
  existing inconsistency where the card form was clickable in a never-bound
  topic while `/topic --adopt` text forms refuse.
- **Copy + docs**:
  - `/help dir` text in src/bridge/command.rs (~line 269): document the 建话题
    row button (new session + new topic, equivalent to `/topic <dir>`).
  - `docs/user-guide.md`: `/dir` table row + a line under the topic-rule note.
  - README if it lists commands/flow (verify — grep first).

## Acceptance criteria

- [ ] `/dir` (no args) card in a lobby/p2p shows two buttons per row; 建话题
      creates a fresh session + topic in that directory, the cover card lands
      at chat top level, replying inside the topic drives the new session, and
      the lobby's active session is untouched.
- [ ] The 当前 row's 建话题 works (equals bare `/topic` in the current
      project).
- [ ] Clicking 建话题 from a card opened inside a topic (never-bound topic)
      toasts the rejection and creates nothing — including after the topic
      bound itself via the row's left button on the same card.
- [ ] The switch card's 建话题接管 op now rejects inside a topic thread too
      (previously clickable).
- [ ] Existing card-action tests updated: `dir_card_row` shape, `pick` still
      works, topic op missing `open_message_id` → hint toast, no-`thread_id`
      path degrades with a toast, topic-thread rejection for both cards.
      (`src/bridge/test_support.rs` switch/dir card tests around lines
      3746-4016 are the pattern to mirror.)
