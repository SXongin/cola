# The `/dir` browse card reaches any directory by drill-down or typed path

> **Amended (2026-10-16)**: the card lists **files** as well as directories, and a file row
> opens a **File Viewer** — see **ADR-0083**. The "directories only" scope below is the first
> version's; ADR-0083 governs the listing now.

The `/dir` Recent Directories card could only pick from the directories cola
already knew — the ADR-0046 union of the shared store's session directories,
cola's mappings and the conversation's current directory. A directory the user
had never worked in, or had just created, was unreachable from the card: the
only way to it was to type `/dir <path>` as a text command. A new **Directory
Browser** card, opened from that card's 「📁 浏览目录」 button, now reaches the
whole filesystem: drill into child directories, hop back up a clickable
breadcrumb, or type a path (前往 to browse it, 选择 to declare it) — with the
same 切换到这里 / 建话题 node actions the Recent Directories rows carry.

## Context

- **Feishu card 2.0 has no file-picker, no tree, and no server-loaded dynamic
  options.** The component catalog is exhaustive (containers, display,
  interactive) and contains no directory component; cascading/dynamic dropdown
  options are explicitly unsupported, and the documented pattern for anything
  richer is "click/submit → callback → the server sends a new card version".
  Type-ahead directory search is therefore not buildable in-card. The browser is
  click-driven re-rendering, and each round trip is one card callback (3 s,
  no retry) plus one in-place card update.
- **`table` cannot host the rows.** Its cells are text/options, not buttons, so
  a clickable directory row must be a `button`. The card uses a
  `column_set`/`flex_mode: "flow"` grid for the breadcrumb and a full-width
  (`width: "fill"`) button per child; it avoids the loop container (builder-tool
  only, no JSON) and `collapsible_panel` (a `form` cannot nest inside it).
- **A `form` must be a root-level element and cannot wrap the drill-down
  controls** — inside a form, only the submit button fires its callback, so the
  clickable rows must sit outside it. The browse card's one form (path input +
  optional filter input + three submit buttons) is a sibling of the rows.
- **The cards are stateless** (ADR-0052): the browsed directory, the hidden
  toggle, the child filter and the page all round-trip in the button payload,
  and nothing is persisted per thread.
- **The card's audience is the whole chat.** The Access List (ADR-0035) governs
  who may *click*, not who may *read* a card, so unrestricted browsing publishes
  directory names to everyone in the chat. The Recent Directories card already
  leaks a user's own project paths; a full browse would leak `$HOME`, `/etc`,
  and every unrelated project — which is why a bound was attempted here and
  deferred to #673 (see Decision).
- **Repo detection must stay cheap.** A real git read spawns `git`
  (`src/git.rs`, once per turn for the footer); per row that would be a process
  per child. The presence of a `.git` entry is a single `stat` per child and is
  a sufficient "is a repo" signal.

## Decision

- **Entry point.** The Recent Directories card gains a 「📁 浏览目录」 button
  (op `browse`). It **replaces that card in place** (one message, no new send)
  with the browse card; the browse card's 「← 最近目录」 button (op `recent`)
  swaps back to a fresh Recent Directories card.
- **The browse card** (`build_dir_browse_card`) is flat root-level schema-2.0
  elements: an optional `ⓘ` notice line, a clickable breadcrumb (🏠 + ⬆ 上一级
  + the path's last four segments, prefixed by `…` when truncated), the current
  path as a monospace markdown line, the node action row (切换到这里 / 建话题,
  `✅ 当前` and a no-op toast when this is already the conversation's directory),
  one `form`, the page of child directories, the shared pager, and ← 最近目录.
- **Navigation is the classic file-browser model**: a child row *descends*
  (`op: "go"`); selection acts on the directory you are standing in. One verb
  (选择/切换到这里) at one place keeps the row layout to a single button and the
  mental model singular; reaching a deep directory is enter-then-select.
- **The form** carries a path `input` with 前往 (navigate) and 选择 (declare a
  Pending Session there) and, when the directory holds more children than a page
  or a filter is active, a filter `input` with 过滤. The submit routing rides the
  button `name` (`browsego` / `browsepick` / `browsefilter`, the ADR-0051
  form-submit convention) with the typed value read from `form_value`; the
  browsed dir/hidden/page ride the button `value` and degrade on a name-only
  submit (the handler falls back to the current project).
- **Child listing**: directories only in this version — **files were added by ADR-0083**, which
  also gives a file row the File Viewer it opens (a symlink to a directory counts, via
  `is_dir()`); dot-directories hidden unless the `显示隐藏` toggle is on; filtered
  by the child filter (case-insensitive token-AND on the name); **repos first**
  (a child containing a `.git` entry, 📦-marked), then case-insensitive
  alphabetical; six per page via the shared pager. A render scans at most
  `BROWSE_SCAN_CAP` (2000) entries — a larger directory is truncated with a
  notice, so no listing can outrun the card callback's budget.
- **Failed paths fall back, they do not error out of the card**: an expanded
  (`~`, relative) target that is not a directory clamps to its nearest existing
  ancestor with a notice; an unreadable directory renders an error notice with
  the empty state; a directory entry that errors mid-iteration yields the
  entries read so far plus a partial-read notice; 选择 on a missing target (or a
  regular file) toasts and declares nothing. The browser **resolves a target
  once** (`resolve_browse_path`: the longest existing prefix canonicalized, any
  remaining components applied to that symlink-free base) and renders and reads
  that same path, so a display spelling never disagrees with the read.
- **No path boundary in this slice — deliberately deferred (#673).** The browser
  may reach any directory the cola process can read. An opt-in
  `[bridge] browse_roots` allow-list was designed, implemented and **withdrawn**
  here: six review rounds each found another escape, all the same class — a
  *path-based* check judged against a *live, mutable* filesystem (a `..` after a
  missing component; a symlink whose check spelling and read resolution differ;
  an ancestor fallback climbing out; an empty entry matching every path; a
  symlinked child probed outside). Making it sound needs **fd-relative
  traversal** (`openat`/`O_NOFOLLOW`), which `std` does not expose, so the
  boundary moves to #673 to be designed around directory handles rather than
  path strings. Until then the browser is unbounded — correct for the
  personal-machine model, but a Host who puts cola in a group chat should know
  the card renders directory names to everyone who can read the chat.
- **The whole read is local and synchronous** — `stat`/`read_dir` only, no git
  spawn, no network, and bounded to `BROWSE_SCAN_CAP` entries — which keeps a
  callback inside Feishu's 3 s, no-retry budget even for a pathological
  directory.
- **The two existing ops are shared, not duplicated**: 建话题 on both cards is
  the one `open_topic_from_card` (nested-topic guard, `open_message_id` anchor,
  `NoThreadId` guidance), and 选择 is the existing `declare_pending`
  (ADR-0041). The browse card multiplexes through the `dir` action tag,
  distinguished by `view: "browse"`, so the dispatch has one new arm.

## Why

- **The gap was real and the workaround was the keyboard.** The only card-free
  path was typing an absolute path the user could not see; the browser makes the
  same declaration one or two taps.
- **Click-driven re-rendering is the only shape Feishu allows.** Designing
  around a type-ahead that does not exist would have been wasted effort; the
  breadcrumb + rows + form is the honest ceiling of card 2.0.
- **Stateless beats a session-paced browse cursor.** Carrying the browsed dir in
  the payload costs nothing and needs no expiry or cleanup, matching every other
  list card.
- **A boundary needs directory handles, not path strings.** Path arithmetic
  against a live filesystem kept leaking (see the deferral bullet in Decision);
  bounding the browser properly is #673's job, not a patch on top of one.
- **Repo-first via `.git` is nearly free and visibly useful** when browsing a
  workspace full of checkouts; a real git read per row is not.

## Alternatives considered

- **Extend the Recent Directories card in place** (a browser region on the same
  card): that card already carries a search form, six rows and a pager; adding a
  breadcrumb, a path form and a child grid crowds it, and the two "select"
  verbs on one card confuse. A separate card keeps one card to one job.
- **A new `/browse` text command as the entry**: rejected — the card already
  exists to be used; a command is a keyboard path the browser is meant to
  replace, and it splits the entry point.
- **Free-text path only** (no browsing): the cheap half, but it answers only the
  user who already knows the path — the exact user the Recent Directories card
  already served.
- **Server-side path autocomplete** (a `select_static` re-rendered per keystroke):
  not possible — components inside a `form` only submit via the form button, and
  there is no focus/keystroke callback in card 2.0.
- **`mkdir` for a not-yet-existing path**: deferred. It adds a mutating action
  behind a card click with its own error surface; the first version only reaches
  directories that exist (`/dir <path>`'s existing rule).
- **Per-row 进入 + 选择 buttons**: two buttons per row crowd the label on mobile
  (the ADR-0025 lesson) and put the select verb in N places.
- **Persist the browse position per thread**: new state with its own staleness
  rules; the payload already carries everything the next render needs.

## Consequences

- The `dir` action tag now multiplexes two cards (`view: "browse"` selects the
  browse handler); a browse-card control that forgets `view` would be handled as
  a Recent Directories op — the builders set it from one helper (`browse_value`),
  and the pager carries it as a string callback field.
- The `/dir` browse card is a second renderer of the conversation's current
  directory and the Pending Session state, so a change to `current_directory`
  or the pending accessor touches it.
- The card's read is O(children) `stat`s per render (one per `.git` check), but
  capped at `BROWSE_SCAN_CAP` entries, so its cost is bounded rather than
  proportional to the directory: a directory with more than the cap is listed
  truncated, with the notice saying so.
- **An unbounded browser is a disclosure if cola is in a group chat.** The card
  renders directory names to everyone who can read the chat, and no path-based
  boundary ships in this slice (#673). A Host who needs a bound today can run
  cola in p2p, or wait for #673.
- `/help dir`, both user guides and the Recent Directories glossary entry point
  at the browser.

## Domain note

A new **Directory Browser** glossary entry describes the card; the **Recent
Directories** entry gains the entry-point sentence.
