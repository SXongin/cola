# Spec: command, config, log & session-file redesign

Status: ready-for-agent

## Problem Statement

The 15 slash commands were stacked incrementally (ADR-0007, ADR-0008) without an
overall design. The session-selection cluster (`/dir` `/new` `/switch` `/list`
`/attach` `/forget` `/topic`) overlaps heavily: `/new` and `/dir` both create;
`/switch` and `/attach` both adopt; `/list` is just `/switch`'s helper. The
deepest wart: the "current project" is not sticky — `/new` ignores the active
session's directory and jumps to the configured default folder, so working in a
project and typing `/new` yanks the user back to `work_dir` (or the process cwd).
The config file carries pseudo-required fields (`url` is mandatory but rewritten
by discovery), the log never rotates and grows without bound, and the session
file's shape is unchanged by any of this.

This is a redesign: behavior changes, command surface shrinks, config/log/session
settle around one consistent model.

## Solution

### Core concept: Project follows the active session (ADR-0012)

A conversation's "current project" is the directory of its active session —
derived, never stored separately (the server stays the single source of truth,
ADR-0007). `/new` inherits the active session's directory and falls back to the
default directory (`[bridge] work_dir`, else process cwd) **only** when the
conversation has no session. This is what fixes the `/new` jump.

### Command surface (15 → 12)

| Group | Command | Form |
|---|---|---|
| Selection | `/new [name]` | text (current project) |
| Selection | `/dir <path> [name]` | text (switch project + new session) |
| Selection | `/switch` | card: search + list + row buttons + "＋new" |
| Selection | `/switch <kw>` | text-direct (switch / adopt / candidates) |
| Selection | `/switch list|forget|<id>` | text subcommands (absorbed `/list` `/forget` `/attach`) |
| Selection | `/topic <path> [name]` | text (new Feishu topic + session) |
| Operation | `/stop` `/compact` `/name` | text |
| Operation | `/model` `/agent` `/autoaccept` | card on no-arg, text-direct on arg |
| Ops | `/help` | card (navigation + command buttons) |
| Ops | `/restart` `/restart-opencode` | text |

`/switch` matching: conversation's mapped sessions first (unique → switch, many
→ candidates), then global excluding children (unique → adopt, many →
candidates), none → hint + card.

### Config (config.rs)

- `[opencode] url` becomes `Option<String>` (default `localhost:4096`); discovery
  still rewrites it when a shared server is found.
- Delete `[opencode] username` and `[opencode] password` (discovery supplies the
  username; cola's own server strips inherited `OPENCODE_SERVER_USERNAME`).
- `[opencode] model` stays optional (three-tier priority: `/model` session
  override > `[opencode] model` > server default).
- `[bridge] work_dir` stays, semantics = "default project when no active
  session". `[bridge] session_file` stays. `[bridge] group_completion_notice` stays.

### Log

Daily rotation: `cola-YYYY-MM-DD.log` with a rolling `cola.log`; keep N days
(default 14, configurable via `--log-days` or `[bridge] log_days`). Cross-day
sessions are queried by `grep session_id=... cola-*.log` — the log layer never
knows about sessions as a business object.

### Session file

Structure unchanged (`SessionEntry` list in `sessions.json`); semantics shift
only (project derived from active session).

## User Stories

1. As a user working in project A, I type `/new` and get a fresh session in
   project A — not the configured default folder.
2. As a user who knows what they want, I type `/switch cola` and it switches
   in one step; I only see a card when there's ambiguity.
3. As a user exploring, I type `/switch` and get a searchable session card with
   per-row switch/adopt buttons.
4. As a user on a fresh machine, I run cola with a minimal config (feishu id +
   secret only) and it works.
5. As an operator, I can query a session's full history across days with one
   grep, and the log directory stays bounded.

## Out of scope

- Streaming assistant-message updates into Feishu (ADR-0008 risk note).
- Making `/new`/`/dir` cards (one-step commands stay text).
- Feishu card capabilities beyond what cola already uses (button, overflow,
  form+input, collapsible_panel, column_set; adding select_static/checker if
  the search card needs them).