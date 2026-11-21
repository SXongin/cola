# Switch and dir cards: union, search, pagination, and a filter that survives
# every rebuild

The `/switch` and `/dir` cards cap their rows at six (`MAX_SWITCH_ROWS`). Both
now paginate six rows per page, and the active filter — keyword, scope, page —
rides every button on the card, so any rebuild (page flip, row action, scope
toggle, force-confirm round trip, snapshot return) lands back on the same
filtered view. Search stays the finder; pagination is the browser. `/dir`'s
list is a union of the session-derived directories, cola's own mappings and the
conversation's current directory, and it carries a keyword search over those
paths. This merges ADR-0046 (the union), ADR-0051 (the search) and this ADR's
pagination and filter preservation into one record; the two earlier ADRs are
pointers here.

## Context

- **The session list is a live view, not a history.** OpenChamber's
  `useSessionAutoCleanup` keeps the recent 5 sessions and deletes or archives
  the rest; `opencode session delete` removes sessions directly. Either can drop
  the last session of a directory the user still works in.
- **cola already persists its own mappings.** `SessionStore` (`sessions.json`)
  keeps an entry per mapped session, independent of the server's store and
  untouched by another client's cleanup; `directories()` exposes them
  most-recently-mapped first. That is not a folder history: an entry leaves the
  store on `/switch forget` and on thread removal.
- **A Pending Session is invisible to both sources.** `/dir <path>`, the card's
  `pick`, `/new` and `/topic` record a pending that materialises only when the
  next non-command message arrives (ADR-0041): no server session and no mapping
  entry exists for its directory yet, but `current_directory` already reports it.
- **The cards are stateless.** The filter lives in the button payload and
  round-trips through the card callback; nothing is persisted per thread.
- **Deferred issue #318** recorded the open question — do row actions keep the
  keyword? — noting both cards must decide together. This ADR resolves it as
  "preserve".

## Decision

### The `/dir` list is a union, read-only and ordered

`dir_card_data` renders the union, deduplicated, in this order (#273):

1. session-derived directories, sorted by last activity;
2. cola-mapped directories the session list no longer carries, most recently
   mapped first (the store's own order);
3. the conversation's current directory, inserted at the FRONT only when neither
   source above already carries it (the pending-only case).

The union is read-only: nothing new is persisted, and no directory is recorded
that cola did not already know through a mapping, a server session or the
current pending. `SessionStore::directories()`'s meaning — "unique mapped
directories, most recently mapped first" — is part of the card's contract and
must not change without revisiting this ADR. A directory whose only session was
deliberately deleted reappears for as long as a mapping points at it: the union
is "what cola knows", not "what still exists on the server", so resurrected
directories sit after live ones regardless of real age.

### Search

- **Conditional visibility** (#319): the form renders when the list has more
  than `MAX_SWITCH_ROWS` entries OR the keyword is non-empty. `/switch`'s form
  is always visible. A short `/dir` list stays exactly as it is; an active
  keyword keeps the form even after the filtered result shrinks to ≤6, so it can
  be refined or cleared.
- **Match rule**: the keyword matches the directory **path** only — lowercased,
  split on whitespace, every token a substring of the path (the `/switch` search
  box's token-AND rule). Matching session titles or ids is explicitly not this
  card: rows don't display titles, so the match would be invisible.
- **The current directory participates in filtering**: the "always on the card"
  guarantee holds for the unfiltered list; a keyword that does not match the
  current directory drops its row, exactly as `/switch` drops its active row.
- **Routing**: the `/switch` form's submit button is named
  `switchsearch|chat|thread|scope`; the `/dir` form's is
  `dirsearch|chat|thread` (the WS extractor synthesizes `action:"dir"`,
  `op:"search"` from it). The keyword arrives in `form_value.search` and
  `default_value` echoes it, so a re-render never blanks the box. Form submits
  do not reliably deliver the button `value`, which is why routing rides `name`.
  An empty keyword rebuilds the full list.
- **The form is last on the card**: an input sits at the card's end (ADR-0100),
  so the search form renders after the rows and the pager.

### Pagination

- Both cards show 6 entries per page — the row cap becomes the page size. A
  three-column `column_set` below the rows renders 上一页 / 「第 x/y 页 · 共 N 个」
  / 下一页; the boundary button is `disabled`; the pager is omitted when there is
  at most one page. `/dir` no longer prints its overflow hints.
- **Stateless page**: `page` rides in the button payload like keyword and scope.
  Reopening `/switch` / `/dir` starts from page 1 with an empty filter.
- **Reset/clamp**: a submitted search and a scope toggle rebuild at page 1; a
  page that no longer exists after the data changed clamps to the last page.

### The filter rides every rebuild

Every control that rebuilds the list carries the active keyword (plus scope and
page for `/switch`): row actions, `new`, `back`, the force-confirm buttons, and
the search submit (keyword only; page resets). `/dir`'s `pick` (including the
已在当前目录 no-op path) and `topic` carry keyword + page. This resolves #318 as
"preserve": process several results of one filter in a row without retyping.

### Adopt's list

The compact 已切换 state card that replaces an adopted list card gains an
optional 返回列表 button carrying keyword/scope/page (ADR-0052's 2026-11-06
amendment, after ADR-0100 deleted the Session Snapshot card). The switch-list op
patches the clicked list card **in place**, button included, and sends no new
message; a text adoption has no list card to return to and carries no button.
The one-card rule is unchanged.

## Why

- Six rows with silent truncation made the rest of the store unreachable without
  inventing a keyword; paging makes the full filtered set reachable while search
  remains the fast path for a known target.
- Carrying the filter in the payload is the cards' existing contract (keyword
  and scope already do it): zero new state, no expiry or cleanup rules.
- Restoring the same filter after a row action serves the workflow #318 named —
  process several results of one filter in a row (adopt sessions, declare
  directories) without retyping.
- Clamping a stale page keeps the user near where they were; resetting would
  teleport them to page 1 after another client's cleanup.
- The union stops the picker losing a directory exactly when it is most useful —
  after a cleanup deleted the sessions in it — and reusing
  `SessionStore::directories()` needs no new state, migration or third source.
- Path-only matching keeps the mental model singular: a directory row shows a
  path, so the keyword searches that path.

## Consequences

- A filtered result longer than a page is both searchable and browsable;
  「共 N 个」 answers "how many matched" without scrolling.
- Text typed but not submitted is lost on any rebuild — a page flip filters by
  the last submitted keyword, not the live input; same accepted limitation as
  the scope toggle.
- Page size stays 6 for both cards and is the renamed `MAX_SWITCH_ROWS`; the
  builders gain a `page` argument and handlers read it from the payload
  (missing/garbage → 1).
- The WS card-action extractor has a `dirsearch` branch beside `switchsearch`;
  both share the form-submit fallback shape.
- `/dir`'s old overflow workaround (`/switch <path>` + `/new`) is no longer
  advertised from the card, though the commands remain.
- Rejected: a persisted Recent Directories list or a persisted filter (new state
  with its own retention), raising the row cap or "load more" (unbounded growth
  against Feishu's limits), basename-only matching, a page-number input, and
  always showing `/dir`'s search box (short-list noise). Raising the cap was
  rejected here and in ADR-0051; validating paths against the filesystem stays
  out (the card build does no I/O).

Origins: the union #273; the search #319; pagination and filter preservation
#326, closing #318. CONTEXT.md's Recent Directories entry records the union,
search and pagination.
