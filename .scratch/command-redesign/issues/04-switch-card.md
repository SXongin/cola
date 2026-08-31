# 04 - `/switch` card: search + list + row buttons + "＋new"

Status: resolved
Type: task
Blocked by: none

## What to build

The strong-interaction half of `/switch` (ADR-0012). `/switch` with **no
arguments** pops an interactive Feishu card instead of text:

- **Search box** (`input`, live-filter via card callback re-search).
- **Session list** — one row per session (title · directory · id-tail), each
  row with a right-hand button: "切换" for a session mapped to the current
  thread, "接管" for a global one.
- **Bottom action**: a "＋新建" button — creates a fresh session in the current
  conversation's project (equivalent to `/new`), closing the card.
- Bounded output: reuse the `/list` cap (15 rows) and the session cache with
  its 30s TTL (ADR-0008); re-search on search-box submit invalidates/refreshes.
- Children and archived sessions hidden unless `--all` was implied by the
  search; sub-task children excluded from adoption.

## Scope

- New card builder in `src/feishu/card.rs` (or a sibling `switch_card.rs`) using
  `input`, `column_set` per-row layouts, and `button`; route callbacks through
  `handle_card_action` in `src/bridge/ws.rs`/`handler.rs` with an `action:
  "switch"` value carrying the target session id and thread key.
- `handle_switch` in `src/bridge/command.rs` dispatches to the card when the
  argument list is empty; the matching-hint text (issue 03 step 3) is replaced
  by the card.
- Reuse the session-list fetch + keyword filter logic from `handle_list` so
  text and card share one code path (no duplicated matching).
- Card component budget: stay under `MAX_CARD_COMPONENTS` (150) and
  `MAX_CARD_JSON_CHARS`; list rows are cheap column_sets but cap rows lower if
  needed.

## Acceptance criteria

- [ ] `/switch` (no args) sends one interactive card; typing in the search box
      and submitting re-filters the list.
- [ ] Each row's button switches (mapped) or adopts (global) that session and
      the card is patched to "已切换/已接管" state.
- [ ] "＋新建" creates a session in the active conversation's project and
      patches the card to the new session.
- [ ] Card callbacks ack correctly (pitfall #5/#8) — no "目标回调服务超时".
- [ ] `cargo test --workspace --locked` green; card JSON stays under the
      Feishu component/size limits.