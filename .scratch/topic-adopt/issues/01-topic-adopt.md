# 01 - `/topic --adopt`: open a new topic around an existing session

Status: ready-for-agent
Type: task
Blocked by: none

## What to build

Extend `/topic` with an adopt mode (ADR-0016): create a real Feishu topic whose
backing session is an **existing** server session, in one gesture.

## Scope

- **Text form** `/topic --adopt <keyword> [--force]` (src/bridge/command.rs):
  - Parse: `--adopt` as the first token; the whole remaining arg (minus a
    standalone `--force` token) is the keyword; no name-in-gesture.
  - Resolution: exact id → unique id-prefix → unique title substring (extract
    the `/attach` resolution from `handle_attach` into a shared helper and reuse
    it). Multiple hits list candidates and point at the full id.
  - Child sessions (`parentID` set) rejected with a hint, always.
  - Occupied elsewhere: reject with the chat-name card unless `--force` (reuse
    `adopt_session`'s owner check + steal path); `--force` steals the mapping.
  - Then create the topic: `reply_in_thread` on the command message → new
    `thread_id` + anchor; write a `SessionEntry` mapping the **new** topic's
    `ThreadKey` to the adopted session (directory/agent copied from the server
    `SessionListInfo`, `topic_anchor` = the in-topic confirmation, `auto_accept`
    = false).
  - Refuse inside another topic (no nesting), same as `/topic` today.
- **Card form** `/topic --adopt` (no keyword): reuse the `/switch` session card
  with a per-row "建话题接管" button.
  - `build_switch_card` (src/feishu/card.rs): add the button per row (right
    column becomes an `action` container with the existing switch/adopt button
    plus the new topic button).
  - `extract_card_action_value` (src/feishu/ws.rs): thread
    `event.action.open_message_id` through into the value.
  - `handle_switch_card_action` (src/bridge/handler.rs): handle the new op by
    `reply_in_thread` off the card's `open_message_id`, then map to the new
    topic key. No `--force` from the card — occupied sessions Toast and point at
    the text form.
- Stealing an actively-driven session with `--force` needs no extra check.
- Update `/help` topic entry, the README Commands list, and the card footer if
  it advertises the topic form.

## Acceptance criteria

- [ ] `/topic --adopt <kw>` opens a topic and maps the existing session there;
      replying inside it drives that session.
- [ ] `/topic --adopt <kw> --force` steals a session mapped to another thread
      (that thread becomes sessionless, its next message auto-creates fresh).
- [ ] Child sessions are rejected with a hint.
- [ ] Ambiguous keywords list candidates; no auto-adopt of the wrong session.
- [ ] `/topic --adopt` (no arg) pops the picker card; its "建话题接管" button
      creates the topic and maps the chosen session.
- [ ] Occupied sessions reject with the owner-chat card and a pointer to the
      text `--force`.
- [ ] `cargo test --workspace --locked` green, `cargo clippy --all-targets`
      clean, `cargo fmt --all -- --check` clean.