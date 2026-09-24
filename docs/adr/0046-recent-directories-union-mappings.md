# Recent Directories union cola's mappings and the current directory

> **Amended by ADR-0051**: the card now carries a keyword search over
> directory paths when the list outgrows its row budget (or a keyword is
> active). Overflow no longer degrades to `/switch <path>` + `/new` — it is
> searchable, and the overflow hint points at the search box. The union, its
> order, and the current-directory insertion stand; the insertion applies to
> the unfiltered list, and a keyword can drop the current directory's row.

The `/dir` Recent Directories picker derived its rows exclusively from the
Shared Store's session list: a directory left the card the moment its last
session was deleted or archived (OpenChamber's retention, `opencode session
delete`), and a directory a Pending Session had declared (ADR-0041) never
appeared at all — the card could show the empty-state hint while the
conversation's current directory pointed at a valid folder. The picker now
unions the session-derived directories with the directories cola has mapped
and the conversation's current directory.

## Context

- **The session list is a live view, not a history.** OpenChamber's
  `useSessionAutoCleanup` keeps the recent 5 sessions and deletes or archives
  the rest; `opencode session delete` removes sessions directly. Either can
  drop the last session of a directory the user still works in.
- **cola already persists its own mappings.** `SessionStore` (`sessions.json`)
  keeps an entry per mapped session, independent of the server's store and
  untouched by another client's cleanup; `directories()` exposes them
  most-recently-mapped first. That is not a folder history: an entry leaves
  the store on `/switch forget` and on thread removal, same as today.
- **A Pending Session is invisible to both sources.** `/dir <path>`, the
  card's `pick`, `/new` and `/topic` record a pending that materialises only
  when the next non-command message arrives (ADR-0041): no server session and
  no mapping entry exists for its directory yet, but `current_directory`
  already reports it. `build_dir_card` marks `当前` only on a directory that
  is in the row list, so the pending's directory has to be in it to be marked.
- The card is a convenience picker: a missing row costs one typed path
  (`/dir <path>`), so the union only needs to be plausible, not canonical.

## Decision

- `dir_card_data` renders the union, deduplicated, in this order:
  1. session-derived directories, sorted by last activity (unchanged);
  2. cola-mapped directories the session list no longer carries, most
     recently mapped first (the store's own order);
  3. the conversation's current directory, inserted at the FRONT only when
     neither source above already carries it (the pending-only case).
- The rendered card caps the list at `MAX_SWITCH_ROWS` as before; overflow
  still degrades to `/switch <path>` + `/new`.
- The union is read-only: nothing new is persisted, and no directory is
  recorded that cola did not already know through a mapping, a server session
  or the current pending.

## Why

- The picker stops losing a directory exactly when it is most useful — after
  a cleanup deleted the sessions in it.
- The current directory is always on its own card, so `当前` can always be
  marked; the pending-only case no longer shows the empty-state hint over a
  declared directory.
- Reusing `SessionStore::directories()` needs no new state, no migration and
  no third source of truth; `/switch forget` and thread removal remain the
  user's explicit way to make a directory leave the card.

## Alternatives considered

- **Persist a dedicated Recent Directories list (folder history).** Rejected
  for now: it adds a third piece of state with its own retention and staleness
  rules, and duplicates what the mapping store already records. Revisit only
  if the card needs directories that have neither a live session nor a
  mapping.
- **Validate every directory against the filesystem before rendering.**
  Rejected: existence checks add I/O to a card build, and a missing directory
  already fails loudly when the next session is created; `/dir <path>` keeps
  its own existence check.
- **Restore deleted sessions' directories from the server only (do nothing).**
  Rejected: that is the loss this ADR exists to fix.

## Consequences

- A directory whose only session the user deliberately deleted reappears on
  the card for as long as a mapping points at it — intended recovery, but
  worth knowing when reading "recent": the union is "what cola knows", not
  "what still exists on the server".
- The card's row order is older at the tail: resurrected directories sit
  after live ones regardless of their real age, and a pending-only directory
  is pinned to the top.
- `SessionStore::directories()` is now part of the card's contract; its
  meaning ("unique mapped directories, most recently mapped first") must not
  change without revisiting this ADR.

## Domain note

The **Recent Directories** glossary entry (CONTEXT.md) now states the union
and points here.
