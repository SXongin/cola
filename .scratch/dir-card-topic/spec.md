# spec: `/dir` card 建话题 button — one-tap topic per recent directory

Design source: `docs/adr/0025-dir-card-topic-button.md`.

## What

The `/dir` Recent Directories card opens a fresh Feishu topic in any listed
directory without typing the path — the card equivalent of `/topic <dir>`.
Also closes the card-form nesting loophole shared with the `/switch` card.

## Decision summary

- Every `/dir` row becomes two buttons like `/switch` rows: left
  切换到这里 / ✅ 当前 (unchanged), right 建话题.
- 建话题 creates a **NEW** server session in that directory and maps it to a
  brand-new topic through the `/topic` pipeline (cover card at chat top level,
  in-topic anchor, `topic_anchor`/`topic_root` persisted). The current
  conversation is untouched. No session adoption, no `--force`.
- Card-only: no `/dir <path> --topic` text flag; `/topic <dir> [name]` stays.
- Success = rebuild the card + short toast (directory basename). Failure paths
  mirror existing ones (missing `open_message_id` / no `thread_id` / session
  creation error → toast).
- Nesting guard at the card-op level: the action is rejected when the card
  lives in a topic thread (`thread_id != chat_id`), toast guides back to the
  main conversation. Retrofit the same guard onto the switch card's existing
  建话题接管 op.

## Issues

| # | Title | Blocked by |
|---|-------|-----------|
| 01 | `/dir` card 建话题 button + nesting guard retrofit | — |

Triage state: `ready-for-agent` — ticket written from the ADR after grilling.
