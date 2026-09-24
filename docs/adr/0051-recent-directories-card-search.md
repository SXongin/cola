# Recent Directories card gains a keyword search over directory paths

The `/dir` Recent Directories picker now carries a search box — rendered only
when the directory list outgrows the card's six-row budget, or a keyword is
already active. The keyword filters the same union the card already renders,
matching directory paths only (case-insensitive, whitespace-token AND — the
`/switch` search box's rule), including the conversation's current directory.
This amends ADR-0046's overflow story: an overflowing card is now searchable
instead of degrading to `/switch <path>` + `/new`.

## Context

- The card caps its rows at `MAX_SWITCH_ROWS` (6) and, on overflow, tells the
  user 「还有 N 个最近目录未显示。用 `/switch <路径>` 接管已有会话，再 `/new`
  新建。」 — a two-command workaround that requires the user to type a path
  they cannot see.
- The `/switch` card has carried a search form since ADR-0022: an input + 搜索
  button, the routing encoded in the button `name` (`switchsearch|…`, because
  form submits do not reliably deliver the button `value`), the typed keyword
  in `action.form_value.search`, echoed back via `default_value`.
- Recent Directories is a union (ADR-0046) — session-derived directories,
  cola's mappings, and the conversation's current directory — so a search must
  filter the union, not just the session list.

## Decision

- **Conditional visibility**: the search form renders when the unioned list has
  more than `MAX_SWITCH_ROWS` directories OR the keyword is non-empty. Short
  lists stay exactly as they are; an active keyword keeps the form even after
  the filtered result shrinks to ≤6, so the keyword can be refined or cleared.
- **Match rule**: the keyword matches the directory path only — lowercased,
  split on whitespace, every token a substring of the path (the `/switch`
  search box's token-AND rule).
- **The current directory participates in filtering.** ADR-0046's guarantee
  ("the current directory is always on the card") holds for the unfiltered
  list; a keyword that does not match the current directory drops its row,
  exactly as the `/switch` card drops its active row.
- **Row cap unchanged**: at most 6 rows, most-recent-first. The overflow copy
  depends on the keyword — no keyword: 「还有 N 个最近目录未显示。用上方搜索
  查找。」; keyword: 「还有 N 个匹配未显示。请细化关键词。」
- **The keyword does not survive a row action.** `pick` / 建话题 rebuild the
  card with an empty keyword — the full list, with the new current directory
  marked (the row buttons carry no keyword, matching the `/switch` card). A
  deferred issue (#318) tracks whether both cards should keep the keyword
  instead.
- **Routing**: the form's submit button is named `dirsearch|<chat>|<thread>`
  (the WS extractor synthesizes `action:"dir"`, `op:"search"` from it), the
  typed keyword arrives in `form_value.search`, and `default_value` echoes it
  so a re-render never blanks the box. An empty keyword rebuilds the full
  list.

## Why

- The overflow workaround asked users to retype a path they could not see;
  search is the same gesture the session card already teaches.
- Path-only matching keeps the mental model singular: a directory row shows a
  path, so the keyword searches that path. Which sessions live in a directory
  is `/switch`'s question, not this card's.
- Conditional rendering answers the actual complaint (too many directories); a
  permanently visible box on a two-directory card is noise.

## Alternatives considered

- **Always show the search form** (full parity with `/switch`): simpler
  builder, but a search box over two rows is visual noise, and the ask was
  specifically "when there are many".
- **Match the basename only**: shorter queries, but a path prefix (`/work/`)
  is a legitimate way to find a group of directories, and basename matching is
  a subset of path matching.
- **Also match session titles/ids of sessions in the directory**: turns the
  card into "where did I work on X" search; rows don't display titles, so the
  match would be invisible, and it duplicates `/switch`.
- **Raise the row cap or paginate instead of searching**: more rows do not
  help find one directory among many, and Feishu cards have size limits.
- **A text escape hatch (`/dir list <keyword>`)**: `/dir <path>` already means
  "declare this path", so a keyword sub-form would collide with real paths.
- **Silent truncation** (the `/switch` card's behavior): loses the "there are
  more" signal the card already gives; the overflow hint now points at the
  search box.

## Consequences

- The overflow hint no longer points at `/switch <path>` + `/new`; the text
  forms remain available but are no longer advertised from the card.
- The WS card-action extractor grows a `dirsearch` branch next to
  `switchsearch`; both share the form-submit fallback shape.
- `/help dir`, the card tests and the WS extraction tests cover the new form.
- The Recent Directories glossary entry (CONTEXT.md) notes the search.
