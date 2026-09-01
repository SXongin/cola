# spec: `/topic --adopt` — open a new topic around an existing session

Design source: `docs/adr/0016-topic-adopt-command.md`.

## What

Let a user open a real Feishu topic whose backing session is an existing server
session, in one gesture — instead of creating a fresh session with `/topic` and
later being blocked from switching it by the topic single-session gate.

## Decision summary

- `/topic --adopt <keyword> [--force]`: resolve (exact id → prefix → title,
  whole remaining arg as keyword), create the topic via `reply_in_thread`, map
  the adopted session to the new topic key.
- `/topic --adopt` (no arg): session-picker card with a per-row "建话题接管"
  button (reuses the `/switch` card; requires `open_message_id` passthrough).
- `--force` symmetric with `/switch` (text form only); learn-on-rejection.
- Child sessions always rejected.
- Stealing an actively-driven session is allowed, no extra check.

## Issues

| # | Title | Blocked by |
|---|-------|-----------|
| 01 | `/topic --adopt` text + card forms | — |

Triage state: `ready-for-agent` — ticket written from the ADR after grilling.