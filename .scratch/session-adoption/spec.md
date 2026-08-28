# spec: Session adoption & global discovery

Design source: `docs/adr/0007-session-ownership.md` and `docs/adr/0008-session-discovery-commands.md`.

## What

Let a user find and take over sessions that were not created in Feishu
(OpenChamber, CLI), and make the server the single source of truth for session
identity.

- **Session identity**: server `title` is the truth; cola's `SessionEntry` stops
  storing a name. One session maps to at most one thread. Topics hold exactly one
  session; the lobby may hold several.
- **Discovery**: `/list` becomes a global, cached, recently-active list of every
  session in the shared store, filterable by keyword.
- **Adoption**: `/switch` and `/attach` can adopt any server session into the
  current thread; `/forget` unmaps; `--force` steals from another thread.
- **Renaming**: `/name` (and `/new <name>`) PATCH the server title; `/dir` leaves
  the title to the server's auto-generation.
- **Topic gate**: session-selection/creation commands are rejected inside a topic
  that already has a session; a never-had-a-session topic may adopt one.

## Issues

| # | Title | Blocked by |
|---|-------|-----------|
| 01 | Session identity from server (drop `name`) | — |
| 02 | Canonical session list client | — |
| 03 | `/list` global | 01, 02 |
| 04 | `/attach` + `/forget` | 01, 02 |
| 05 | `/switch` auto-adoption | 03 |
| 06 | `/name` PATCH + creation title policy | 01, 02 |
| 07 | Topic single-session gate | 01, 03, 04 |

Triage state for each issue: `needs-triage`.