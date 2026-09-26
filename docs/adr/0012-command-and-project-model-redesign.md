# Project follows the active session; commands get a text/card dual form

cola's commands grew incrementally (ADR-0007, ADR-0008) into an overlapping
15-command surface, and the session-selection cluster shared no consistent
project model — `/new` jumped to the configured default directory instead of the
project the user was working in. We decided two things together: (1) a
conversation's "current project" is always the directory of its active session
(derived, never stored separately — the server stays the single source of truth,
ADR-0007), so `/new` inherits it and falls back to the default directory only
when the conversation has no session; (2) every command gains a dual form —
text-direct when you know what you want (e.g. `/switch cola` switches in one
step) and a card when you don't (e.g. `/switch` with no args pops a searchable
session card). The command surface converges to 12 commands: `/switch` absorbs
`/list`, `/attach`, `/forget`; `/new` and `/dir` stay split (`/new` = new session
in the current project, `/dir` = switch project + new session); `/topic` stays
(OpenCode has no "topic" concept, so the name collides with nothing on the
backend).

> **Amended by ADR-0041**: the conversation's current project is the Pending
> Session's directory when one exists, otherwise the active session's
> (pending-first); `/new` records a Pending Session instead of creating and
> activating a session at command time; the session file's structure changed
> to `{"entries":[…],"pending":[…]}` (legacy bare arrays still load).

> **Amended by ADR-0022's 2026-09-26 update**: the `/list` text form that
> `/switch` absorbed is deleted outright (no tombstone, no `--all`); the switch
> card's 全部 scope + keyword search is the lossless find-anything path, and
> child sessions gained their own scoped `/sub` family (`/sub list` read-only,
> `/sub attach` explicit takeover) while session management stays root-only.
> The "12 commands" count in the text above is a historical snapshot.

Consequences: config simplifies (`url` optional, `username`/`password` deleted,
`work_dir` = default project), the log rotates daily (`cola-YYYY-MM-DD.log`,
keep N days) with cross-day sessions queried by `grep session_id=... cola-*.log`,
and the session file's semantics shift (ADR-0041 later adds the `pending` list
to its structure — see the amendment above). Cards
are only used where interaction is strong (`/switch`, `/model`, `/agent`,
`/autoaccept`) or where a reference is useful (`/help` — a buttonless command
manual, detail stays text via `/help <command>`, and a failed card falls back to
the plain-text help so `/help` never dies silently); one-step commands stay
text.