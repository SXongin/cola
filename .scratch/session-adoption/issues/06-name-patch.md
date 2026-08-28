# 06 - `/name` PATCH + creation title policy

Status: needs-triage
Type: task
Blocked by: 01, 02

## What to build

Make session renames write the server `title` (visible to OpenChamber/CLI too)
and apply the title policy when creating sessions.

## Scope

- `/name <new>`: `PATCH /session/{id}` with the new title; the change is visible
  to every client sharing the store. Invalidates the `/list` cache (issue 03's
  invalidation hook).
- Creation policy:
  - `/new <name>` PATCHes the given name immediately.
  - `/dir <path>` and auto-created sessions (first message) leave the default
    title so the server's small model generates one after the first turn.
  - `PATCH`ed titles are never overwritten by auto-generation (verified).
- `/name` works inside topics too (operates on the fixed session, per issue 07's
  allowed-command set).

## Acceptance criteria

- [ ] `/name` PATCHes and the new title is served by `session_info` and visible
      in `/list`.
- [ ] `/new <name>` creates a session titled `<name>` before any message.
- [ ] `/dir` / auto-created sessions get an AI-generated title after the first
      message, not a basename.

## Blocked by

- 01 - Session identity from server (drop `name`)
- 02 - Canonical session list client