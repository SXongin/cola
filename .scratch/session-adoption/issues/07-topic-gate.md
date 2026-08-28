# 07 - Topic single-session gate

Status: needs-triage
Type: task
Blocked by: 01, 03, 04

## What to build

Enforce "a topic holds exactly one session" across the session-selection and
creation commands, with the fresh-topic exception, and update `/help`.

## Scope

- Inside a topic (`thread_id` present) that **already has** a session, reject:
  `/list`, `/switch`, `/attach`, `/new`, `/dir` (and keep `/topic` blocked) —
  reply "回主对话操作" (this is the single gate all commands share).
- Inside a topic that **never had** a session, the same commands are allowed and
  their outcome becomes that topic's single session. For `/attach`/`/switch`
  adoptions, the anchor for fallback cards is the command's own reply message
  inside the topic (reuse the ADR-0006 `reply_in_thread` flow).
- Always allowed in topics: `/stop`, `/compact`, `/agent`, `/model`,
  `/autoaccept`, `/name`, `/restart`, `/restart-opencode`, `/help`; unknown
  slash commands still forward as prompt text.
- `/forget` in a topic returns it to the "never had a session" state.
- Update `/help` text for the new/changed commands and the topic rule.

## Acceptance criteria

- [ ] Each blocked command inside a topic with a session replies with the
      "回主对话" guidance and does not touch the mapping.
- [ ] In a fresh topic, `/attach` adopts a session whose fallback permission
      cards land inside the topic (anchor is the command's own reply).
- [ ] Operation commands and unknown `/commands` still work inside topics.
- [ ] `/help` documents `/list`, `/attach`, `/forget`, `/name`, `/switch` and
      the topic rule.

## Blocked by

- 01 - Session identity from server (drop `name`)
- 03 - `/list` global
- 04 - `/attach` + `/forget`