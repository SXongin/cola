# Switch card defaults to the current directory; Feishu-side terms split from 会话

The `/switch` card and `/switch list` used to show every recent session in the
shared store, and the word 「会话」 meant both the Feishu conversation and the
OpenCode session. Users reported two problems: "why are so many sessions all
本会话?" and "I want to see the sessions of the directory I'm working in."

> **Amended by ADR-0041**: the switch card's default scope falls back to the
> Pending Session's directory when one exists (a conversation with a pending has
> no active session). The terminology rule stands: 会话 stays reserved for a real
> Session, and the pending state is never labelled with the noun.

> **Amended by the 2026-09-26 update below**: the text `/switch list` is deleted
> outright, not retired — its root-session find role passes to the card's 全部
> scope + keyword search, and `--all`'s child listing to `/sub list`. The
> `/switch list` passages below are superseded on that point; everything else
> stands.

## Context

- A lobby (Feishu chat top-level) maps several OpenCode sessions to one Feishu
  conversation. The switch card marked every mapped session as
  「_(本会话)_」, so several rows read "本会话" at once — the active row said
  "_(active)_/✅ 当前", the rest said "_(本会话)_/切换". The label overloaded
  会话: in Feishu's own UI a conversation is also a 会话, so users could not
  tell which "本会话" was which.
- The switch card lists the whole shared store (`switch_card_data` →
  `cached_session_list`), filtered only by keyword, sorted by recency, children
  and archived excluded. There is no directory dimension: the user's "current
  directory" (the active session's `directory`, ADR-0012) plays no part.
- cola's internal vocabulary already calls a single user→assistant exchange a
  "turn" (ADR-0019, `src/bridge/turn/state.rs`); the UI never labels individual
  turns.
- The Feishu side has two levels today: `ThreadKey { chat_id, thread_id }`.
  The glossary called the top-level a **Lobby** and a topic a **Thread**, but
  the user-facing copy was inconsistent — some strings said 本对话, others
  本会话/当前会话, all meaning the Feishu conversation.

## Decision

### Terminology (glossary + user-facing copy)

- **会话 is the ONLY meaning of the OpenCode Session.** The Feishu side is
  never called 会话 in cola's UI.
- The Feishu side gets two terms, matching Feishu's own UI:
  - **聊天** (Chat) — the top-level Feishu conversation (`chat_id`); contains
    many Topics and may hold several Sessions directly (the lobby).
  - **话题** (Topic) — a Feishu thread (`thread_id`); holds exactly one
    Session. `Thread`/`Lobby` remain the code names.
- A single user→assistant exchange is a **Turn** — internal English
  vocabulary only, no user-facing Chinese noun (if one is ever needed,
  轮次/本轮).
- Rename every user-facing string where 会话/对话 means the Feishu
  conversation → 本聊天 (lobby) or 本话题 (topic). Keep 会话 where it means the
  OpenCode session — e.g. Auto-Accept's 「本会话的权限请求」(per-session flag),
  重命名当前会话, 已创建会话, 已接管会话.

### Switch card default view: current directory

- The `/switch` card's default view is **scoped to the current directory** —
  the active session's `directory`. Rows show only sessions of that directory.
  - No active session (fresh conversation) → fall back to the global-recent
    view (current behavior).
  - A 「全部」 toggle expands from the current directory to the whole store,
    still hiding sub-task children (the text `/switch list --all` remains the
    only children escape hatch. **Superseded by the 2026-09-26 update**: the
    text list is deleted; children are `/sub`'s surface — `/sub list` read-only,
    `/sub attach` takeover — and the switch card stays roots-only). The toggle
    state rides in the card's routing payload so it persists across
    search/rebuilds; the card header shows the directory when scoped.
  - Keyword search stays within the current scope (directory or all).

### Marking: only the active session

- Only the **active** session is marked (✅ 当前 / `(active)`). The
  「_(本会话)_」 text marker on mapped-but-not-active rows is removed.
- Mapped-but-not-active rows keep the **切换** button (they belong to this
  conversation); unmapped foreign sessions keep **接管**.

### Text forms stay global and lean

> **Partly superseded by the 2026-09-26 update**: only the `/switch <keyword>`
> text form remains — the text list is deleted outright, so nothing carries the
> `--all` child listing any more.

- `/switch list` stays global (the reliable "find any session anywhere"
  escape hatch) and keeps only the `(active)` marker — the 「本会话」 marker is
  dropped. **Superseded by the 2026-09-26 update: the command was deleted
  outright (no tombstone, no `--all`), and `/switch list` is now an ordinary
  keyword query.**
- `/switch <keyword>` keeps its resolution order unchanged: thread-mapped
  matches first, then the global store. The directory filter is a switch-card
  concern only.

## Why

- The directory default matches how users actually work: most switches are
  between sessions of the same project, so the current-directory view surfaces
  the relevant sessions first without a keyword.
- Splitting 聊天/话题 from 会话 resolves the ambiguity at the root: 会话 now
  always names an OpenCode Session, so "why are so many sessions 本会话" cannot
  arise — only one row is ever marked, and the mark says ✅ 当前 (active), not
  ownership.
- Keeping text forms global preserves a lossless path: `/switch list` finds
  anything, the card is the scoped quick-switcher. The 全部 toggle is the card's
  own escape hatch for the same job. **Superseded by the 2026-09-26 update**:
  the text list is deleted; the card's 全部 scope + keyword search carries the
  find-anything role for root sessions, and `--all`'s child listing is replaced
  by `/sub list`.
- Marking only the active session removes the "several 本会话" surprise without
  losing information: the 切换 button still distinguishes this-conversation
  sessions from foreign ones.

## Alternatives considered

- **Rename the marker instead of dropping it** (本会话 → 本聊天/本话题): kept
  the multiple-marker confusion (a lobby still maps several sessions), so the
  marker was dropped for non-active rows. The 聊天/话题 vocabulary lives on the
  container level, not per row.
- **Group the card by directory instead of filtering**: keeps the full list on
  screen but pushes the current directory's sessions below foreign ones; the
  directory-default filter is the sharper answer.
- **Directory-filter the text forms too**: forces an escape hatch into every
  text command and breaks the "text = global, card = scoped" story. Rejected.
- **全部 also shows children**: blurs the card's child-hiding policy; the text
  `--all` already exists. **Partly superseded by the 2026-09-26 update**: the
  text list (and `--all`) is deleted; children have their own scoped surface
  (`/sub list`, `/sub attach`), so the switch card stays roots-only.

## Risks / open questions

- The directory default hides sessions of *other* directories that a lobby has
  mapped; a user who wants one must hit 全部. Mitigated by the 全部 toggle and
  the still-global text forms.
- The switch-card header needs a visible directory line when scoped, so a user
  understands *why* the list shrank. Undecided exact wording; implement with
  「本目录」+ the directory basename.
- `/switch list` keeps `(active)` but drops the ownership marker; a lobby user
  loses the "which sessions belong to this conversation" signal from the text
  list. Acceptable — the card carries it (切换 vs 接管). **Superseded by the
  2026-09-26 update: the command is deleted outright, so the trade-off no
  longer applies.**
- The card a `/switch <keyword>` opens after a no-match search is deliberately
  sent in the `All` scope, not the directory default — the text form searched
  the whole store, so a directory-scoped card would hide the candidates the
  user was just shown. The bare `/switch` and the `/topic --adopt` card open in
  `Directory` scope.

## Update (2026-09-26)

The text `/switch list` is **deleted outright, not retired** (spec #344):
`/switch list` now parses as an ordinary `/switch list` keyword query — no
tombstone reply, no `--all` flag, and nothing forwarded to the model.

- **Root sessions.** The card's 全部 scope + keyword search is now the lossless
  find-anything path for root sessions: both card scopes stay roots-only, so
  every root session anywhere remains reachable, and a row's 接管 button adopts
  it in place. That is the role the deleted text list played for roots.
- **Children.** `--all`'s child listing has exactly one replacement: `/sub
  list` — read-only, scoped to the Active Session's direct children — with
  `/sub attach` for the explicit takeover. Children of other sessions and
  nested descendants are not reachable from the switch surface at all: the card
  and `/switch <keyword>` never offer a child, and a `/switch` adoption attempt
  on a child this chat has not mapped is refused, naming `/sub attach`
  (ADR-0008's 2026-09-26 update).

The `/switch <keyword>` resolution order and the directory default stand.
