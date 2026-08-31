# 05 - Dual-form cards for `/model` `/agent` `/autoaccept` + `/help` navigation

Status: resolved
Type: task
Blocked by: none

## What to build

Extend the dual-form (text-direct / no-arg card) pattern from `/switch`
(ADR-0012, issue 04) to the other strong-interaction commands, plus the `/help`
navigation card.

- `/model` — with an argument keeps today's text-direct set. No argument pops a
  card listing available models (from `GET /provider`, client.rs:422) as
  buttons; clicking one sets the per-session override (same handler as text).
- `/agent` — no-arg card lists agents as buttons; arg keeps text-direct.
- `/autoaccept` — no-arg card shows the current on/off state as two toggle
  buttons; `on|off` keeps text-direct.
- `/help` — navigation card grouped by 会话 / 操作 / 运维: every command listed
  with a "试试" button that triggers it directly; card-able commands get a
  "看卡" button that pops their interactive card. Text `/help` remains for
  non-card clients.

## Scope

- One generic "option list card" builder (title, markdown intro, button rows,
  shared `action` callback routing with a per-command `action` value) reused by
  `/model` `/agent` — avoid three bespoke card builders.
- Toggle card builder for `/autoaccept`.
- `/help` navigation card builder; wire each command's "试试"/"看卡" callback to
  the corresponding handler (reuse issue 03/04 dispatch).
- `parse_command` routes: empty-arg `/model` `/agent` `/autoaccept` → card.

## Acceptance criteria

- [ ] `/model` no-arg pops an option card; clicking a model sets the override
      and persists to `SessionEntry.model` (same path as text `/model`).
- [ ] `/agent` no-arg lists agents; click sets the override.
- [ ] `/autoaccept` no-arg shows the current state as buttons; toggling works.
- [ ] `/help` card groups all 12 commands; every "试试" fires the command; every
      "看卡" pops the matching interactive card.
- [ ] Text forms are unchanged for all four commands.
- [ ] `cargo test --workspace --locked` green.