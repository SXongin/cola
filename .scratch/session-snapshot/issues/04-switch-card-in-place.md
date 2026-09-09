# 04 - Switch-card 接管/切换 patches in place to the snapshot

Status: ready-for-agent
Type: task
Blocked by: 03

## What to build

The switch card's row buttons currently end adoption by patching the interactive
list card to a 「已接管」 state (ADR-0012, issue 04). Under the one-card rule
(ADR-0028) that same message becomes the Session Snapshot: the list is replaced
**in place** by snapshot content — no second message.

## Scope

- `handle_switch_card_action` (src/bridge/handler.rs:920-…) and the card value
  routing: for the row **接管** (global adopt) and **切换** (mapped) ops, build
  the snapshot (ticket 03's shared emit/build path) and return it as the
  `CardActionResult.card` so the ack patches the list card to it.
- In-topic routing: the patched card must remain a valid in-thread reply target
  for later fallback cards (anchor semantics) — reuse the same anchor logic as
  ticket 03, since the card's `open_message_id` stays the same message.
- **切换 on a fully-visible mapped session** (suppression): patch to a compact
  「已切换 `title`」 state instead of a full snapshot, mirroring ticket 03's
  text-ack rule for the text form.
- New-session (「＋新建」) and scope/search ops are untouched.

## Acceptance criteria

- [ ] 接管/切换 on a row ends with the SAME message showing snapshot content (no
      follow-up card, list no longer visible).
- [ ] Suppressed 切换 patches to a compact confirmation, not a full snapshot.
- [ ] In a topic, the patched card still resolves as the fallback/anchor target.
- [ ] `cargo test --workspace --locked` green, clippy/fmt clean.
