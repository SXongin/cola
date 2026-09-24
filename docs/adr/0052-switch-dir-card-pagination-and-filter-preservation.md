# Switch and dir cards paginate and preserve the active filter through rebuilds

The `/switch` and `/dir` cards cap their rows at six (`MAX_SWITCH_ROWS`).
Overflow was handled by search alone: `/dir` pointed at its search box,
`/switch` truncated silently (ADR-0051). Both cards now paginate six rows per
page, and the active filter — keyword, scope, page — rides every button on the
card, so any rebuild (page flip, row action, scope toggle, force-confirm round
trip, snapshot return) lands back on the same filtered view. Search stays the
finder; pagination is the browser. This reverses ADR-0051's "the keyword does
not survive a row action" and its rejection of pagination; its search form,
visibility rule and match rule stand.

## Context

- Both cards render at most `MAX_SWITCH_ROWS` (6) rows: `/switch` silently
  drops the rest; `/dir` prints 「还有 N 个…用上方搜索查找 / 请细化关键词」
  (ADR-0051).
- The cards are stateless: the filter lives in the button payload and
  round-trips through the card callback. The switch search form encodes routing
  in the submit button's `name` (`switchsearch|chat|thread|scope`) and the
  typed keyword arrives in `form_value.search`; the scope toggle carries the
  keyword; row buttons carry `scope` only; `new`, `/dir`'s `pick`/`topic`
  rebuild with an empty keyword; the force-confirm card's 返回列表/强制接管
  buttons drop the keyword entirely.
- Deferred issue #318 recorded the open question — do row actions keep the
  keyword? — noting both cards must decide together.
- ADR-0028's one-card rule patches the switch list card into the Session
  Snapshot on adopt; the list is discarded (a risk it accepted).

## Decision

- **Pagination**: both cards show 6 entries per page (the row cap becomes the
  page size). A three-column `column_set` below the rows renders
  上一页 / 「第 x/y 页 · 共 N 个」 / 下一页; the boundary button is `disabled`;
  the pager is omitted when there is at most one page. `/dir` no longer prints
  its overflow hints.
- **Stateless page**: `page` rides in the button payload like keyword and
  scope. Nothing is persisted per thread; reopening `/switch` / `/dir` starts
  from page 1 with an empty filter.
- **Reset/clamp**: a submitted search and a scope toggle rebuild at page 1; a
  page that no longer exists after the data changed clamps to the last page.
- **Filter preservation**: every control that rebuilds the list carries the
  active keyword (plus scope and page for `/switch`): row actions, `new`,
  `back`, the force-confirm buttons, and the search submit (keyword only; page
  resets). `/dir`'s `pick` (including the 已在当前目录 no-op path) and `topic`
  carry keyword + page. This resolves #318 as "preserve".
- **Adopt's list**: the Session Snapshot that replaced the switch card gains an
  optional 返回列表 button, carrying keyword/scope/page, when it was built from
  the list card — full snapshot and suppressed 已切换 state alike. Other
  snapshot call sites pass none. The one-card rule is unchanged.
- **Unchanged**: search-form visibility (`/switch` always; `/dir` when more than
  a page of entries or a keyword is active), the token-AND match rules, and the
  current directory's participation in `/dir` filtering.

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
- A disabled boundary button keeps the pager's geometry stable; hiding one end
  would shift the other.
- 返回列表 on the adopt snapshot preserves ADR-0028's one-card rule and its
  stale-controls cleanup while returning the browser to its filter; reversing
  the rule (keep the list, send a snapshot) would put two bot messages in the
  thread per activation — the noise ADR-0028 exists to prevent.

## Alternatives considered

- **Load more (accumulate pages into one card)**: the card grows without bound
  and eventually hits Feishu's 30KB / 200-element limits; pages are finite and
  predictable.
- **Prev/next plus a page-number input**: extra control, no real gain at 6 rows
  per page.
- **Persist the filter per thread**: new state with its own invalidation rules
  and the "why is the list shorter than I remember" confusion; rejected.
- **Raise the row cap instead**: longer cards without a way to navigate them —
  ADR-0051 already rejected this shape.
- **Reset to page 1 when out of range**: disorienting after an external
  deletion; clamp.
- **Always-visible `/dir` search box**: re-introduces the short-list noise
  ADR-0051 deliberately avoided.
- **Keep the list card and send a separate snapshot message**: two messages per
  activation; rejected for the one-card rule above.

## Consequences

- A filtered result longer than a page is both searchable and browsable;
  「共 N 个」 answers "how many matched" without scrolling.
- Text typed but not submitted is lost on any rebuild — a page flip filters by
  the last submitted keyword, not the live input; same accepted limitation as
  the scope toggle today.
- Page size stays 6 for both cards and is the renamed `MAX_SWITCH_ROWS`; the
  builders gain a `page` argument and handlers read it from the payload
  (missing/garbage → 1).
- ADR-0051 and ADR-0046 carry amendment banners; ADR-0028 is amended for the
  返回列表 button; CONTEXT.md's Recent Directories entry notes pagination; #318
  closes with the preserve decision.
