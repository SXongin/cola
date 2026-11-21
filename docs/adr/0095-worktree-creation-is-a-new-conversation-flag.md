# Worktree creation is a flag on the new-conversation flow

cola **follows** a Session that has already moved into a linked worktree (issue
#433: the mapping follows the server's location and the **Turn Footer** marks
the checkout, `📁 zh-user-guide 🌲 · docs/zh-user-guide`). Starting work in one
is this ADR: a `--worktree` flag on the new-conversation flow creates the
checkout with git and starts the Session in it. ADR-0088 first gave cola a
`/worktree` inventory card plus `/worktree new`; that shape was never released —
no tag contains `4fd148fa` — and #890 identified what was wrong with it:
creation is a decision about a **new conversation**, yet it ran ahead of one;
the base defaulted to the source's `HEAD`, so a worktree made from a feature
branch silently inherited its commits (the 22-commit offset discovered only when
the PR opened); the card duplicated `/dir` and `/switch`; and an abandoned
conversation left a checkout behind. This ADR replaces the shape. The parts of
ADR-0088 that still hold — cola runs git, the root is cola's own setting, cola
creates and never removes, the collision and dirty rules — stand and are
restated here; ADR-0088 is a pointer.

## Context

- **The server API exists, but it only detaches.** OpenCode 2.1.0 exposes
  `GET /api/worktree` (list), `POST /api/worktree` (create) and
  `DELETE /api/worktree` (remove), and `GET /api/location` resolves the
  `projectID`. But every create runs
  `git worktree add --detach <path> <ref>` (`packages/core/src/git.ts`): the
  `branch` field is a *start point*, never a branch to create. Measured
  2026-10-21 on 2.1.0 — a create without `branch` lands on a detached HEAD, and
  `branch: "wt/x"` for a branch that does not exist is
  `fatal: invalid reference: wt/x`. The server's list is an inventory, not a
  git view: `[{directory, strategy}]`, the main checkout included, removed
  directories kept until a refresh, and neither the branch nor the dirty state —
  exactly what a user checks a worktree for, and both are git reads anyway.
- **`src/git.rs` already shells out to git** for the Turn Footer, so
  `git worktree list --porcelain` and `git worktree add` reuse it: no new
  dependency, and no `projectID` resolution cola otherwise never performs.
- **The two-step dance.** `/topic` is refused inside every topic, and
  `/worktree new` was refused in a bound topic. Starting isolated work was
  therefore `/topic` in the main conversation, `/worktree new` in the still
  pending topic, then the first message. Only the middle step was absent from
  the new-conversation commands.
- **The session title and the filesystem name are different things.** The server
  generates the title from the first message (ADR-0007), in any language, and it
  can be changed at any time. The directory must exist before the session, and
  the branch should be checked out with it; both need an ASCII slug. The old
  command used the slug as the session title, which forced an English name onto
  every worktree conversation.
- **A title cannot be moved into a directory.** OpenCode can move a session into
  a worktree, but Codex has no equivalent, and moving a running session is the
  failure #428 was about.

## Decision

1. **Creation is a flag on the new-conversation flow.** `--worktree` on `/topic`
   and `/new`; nothing is created without it. Adoption and pick (`/dir`,
   `/switch`, `/topic --adopt`) never create: an existing directory is already a
   checkout or not, so there is no "default-on" question.
2. **Creation is lazy: at materialisation, not at the command.** The Pending
   Session carries its **source** directory plus a worktree intent (base ref,
   resolved SHA, slug, planned target). The checkout is created when the first
   non-command message materialises the session (ADR-0041), so an abandoned
   pending creates nothing. Commands in the pending window (`/cat`, `/diff`,
   `/dir`, the cover) read the **source**; the target is computed by the intent.
3. **The base is the integration line, never `HEAD`.** Ladder:
   `[worktree] base` → `origin/HEAD` → `origin/main` → `origin/master` → local
   `main` → `master`; if none resolves, refuse and ask for `--base`. Resolve the
   ref to a **SHA at command time**, store it in the intent, and create from that
   SHA, so the cover's promise is what gets built; a stale ref is visible as a
   stale SHA (cola never fetches). `--base HEAD` is the explicit "branch off the
   branch I am looking at" (stacked work). A configured ref that does not
   resolve is a refusal, never a silent fall-through.
4. **Preconditions fail at command time, before a pending is written.** The
   source must be a non-bare git work tree, the base must resolve, and the root
   must be creatable; otherwise reply `不能在当前目录创建 worktree：<reason>`. A
   source that is itself a linked worktree is allowed: the root resolves to the
   primary checkout, so the new checkout is a sibling, never nested. `--base`
   without `--worktree` and `/topic --adopt --worktree` are usage errors.
5. **Adjustment is pending-only.** Bare `/worktree` reports the intent (target,
   branch, `base @ sha`, source); `/worktree --branch <branch>` and
   `/worktree --base <ref>` update it and re-render a pending topic's cover. Once
   the checkout exists — including a pending kept after a failed session create
   — the directory is fixed and the command says so instead of pretending a
   rename would move it.
6. **A creation failure at materialisation never loses the message.** Keep the
   pending, do not treat the first message as delivered, and reply that
   resending retries. The created checkout's path is recorded on the intent
   **before** the session is created, so a retry after a session-create failure
   reuses that checkout instead of making a `-2` sibling. The record is scoped to
   the claim's generation and the pending's epoch: a pending replaced while the
   checkout was being made cannot inherit its path, and `/worktree --branch` is
   refused while a materialisation owns the pending.
7. **cola runs git, and the root is cola's own setting.** `[worktree] root`,
   default `.worktrees`, resolved against the **primary checkout**
   (`git rev-parse --git-common-dir`'s parent), never the session's own
   directory — that session may itself be in a worktree, and nesting is never
   the intent. An absolute root is allowed. Discovery is the
   `/run git worktree list` terminal command; entering an existing checkout is
   `/dir <path>` (adopting a session there stays `/switch`). The server API
   stays a possible fast path later.
8. **Collisions reuse, never silently destroy.** A directory collision suffixes
   only the directory (`-2`, …); the branch keeps the exact name the user typed.
   An existing branch is attached only when its tip is the resolved SHA; a
   branch that starts elsewhere is refused (reusing the name must not build a
   different commit than the cover named), and a branch already checked out
   elsewhere is refused, naming the checkout that holds it (a path the user can
   act on, not git's raw `already checked out` line).
9. **An unignored root is a warning, not an edit.** After a successful create,
   `git check-ignore` decides whether the reply adds "`.worktrees/` is not
   ignored, the main checkout will read dirty"; cola never edits the user's
   `.gitignore`. The read-only inventory card and its per-worktree dirty
   measurement retire with `/worktree new`: discovery is the
   `/run git worktree list` terminal command.
10. **The title and the branch are separate inputs.** `--name <title>` is the
    session title, exactly like `/new <name>` and `/topic <dir> <name>`: any
    language, PATCHed at materialisation, omitted means the server auto-titles.
    `--branch <branch>` is the **full branch name**, with the repository's own
    shape allowed but never forced: `feat/fix-login` names the branch and the
    directory is its last segment (`<root>/fix-login`); a bare `fix-login`
    names both; no `wt/` prefix is added. Omitted, it is derived from the title
    with a lenient ASCII slugifier, and a title with no usable ASCII falls back
    to a deterministic `YYYYMMDD-1` token — never a refusal for a value the user
    did not type. Each component is validated by the strict slug rules (ASCII,
    lowercased, at most 40 characters, no `.lock` suffix, which git would reject
    only later inside `git worktree add`). The cover shows both before the first
    message, which is the naming veto window. `--name` takes words up to the
    next flag; `--branch` and `--base` take one word.
11. **Alignment is a creation-time property.** The branch and directory share the
    branch's last segment when created. The user chooses the delivery shape up
    front (`feat/<slug>`), so no rename is needed at push time; if a branch is
    renamed later, the directory stays and the last segment is the
    correspondence, not an invariant. Moving the directory with the branch needs
    an unsupported session move and is rejected.
12. **cola creates, never removes.** Removal stays git's (or OpenChamber's) job;
    a removed worktree degrades to #433's missing-directory behavior — no git
    facts on the footer, no new hard failure. A GC (clean + no live session +
    merged branch) is a follow-up; the guards a remove needs (clean vs dirty,
    force, mappings outliving a directory) are their own decision.
13. **No compatibility work.** The feature has never shipped.

## Why

- A branch — a thing the session can commit to and push — is the reason
  worktrees exist here, and the server API cannot produce one; running git also
  keeps V1 and V2 identical.
- A flag on the conversation it belongs to removes the two-step dance and the
  command that ran ahead of its conversation; lazy creation removes the
  abandoned-checkout class entirely.
- The integration-line base makes what the work starts from predictable instead
  of silently inheriting a feature branch; resolving it to a SHA at command time
  makes the cover's promise checkable.
- Separate title/branch inputs let the session keep its natural-language name
  while the directory and branch stay typeable across machines.

## Consequences

- `sessions.json` gains an optional `worktree` intent on a pending; an older
  binary ignores the unknown field, and an abandoned intent creates nothing.
  The pending's directory is the **source**, not the target; materialisation
  points the session entry at the created checkout before the session is
  created; the stored pending keeps its source until activation clears it.
- The command-time root check walks to the nearest existing ancestor, proves it
  writable with one transient probe directory, and does not create the root, so
  an abandoned pending leaves nothing on disk.
- The first message pays the whole-tree `git worktree add` and the per-directory
  instance boot; on a large repository that is seconds. cola sends an immediate
  acknowledgement before the Git call (and the pending cover states the cost), so
  the wait is visible rather than looking like a wedged bot.
- The cover names the target/branch/base before the first message; a colliding
  directory at creation may still get a `-2` suffix, and materialisation replies
  with the actual path when that happens. The cover's path is a prediction, not a
  reservation. A recorded checkout that disappears before the retry is reported
  as missing; the next message recreates it, and a new slug/base is allowed
  again.
- The `[worktree] base` and `[worktree] root` settings can disagree with the
  project's `opencode.json` (`worktree.directory`); cola reads only its own, and
  the documentation says they should agree. A worktree created here is unknown
  to the server's inventory until OpenCode's own discovery runs; cola does not
  care, because a Session is created **in** a directory, not **for** a worktree
  resource.
- A worktree session still behaves like any other session: the Turn Footer's 🌲
  marker, the removed-directory ending (#450), and the external-move follow
  (#433) are unchanged. A removed worktree keeps the missing-directory behavior.
- The feature's surface shrinks to a flag on two commands plus one pending
  adjuster. `/new --worktree` is the first thing to cut if the surface still
  feels large: topics are the natural isolation unit.
- Rejected: the server's create/list (detached, no branch, no dirty), eager
  creation, default-on creation, reading the project's `opencode.json`, deriving
  the slug from the first message, creating the session first and moving it,
  creating detached and naming the branch later, a `wt/` prefix, transliterating
  Chinese titles, auto-editing `.gitignore`, and removal in the same change.

Origins: #449 (the command), #890 (this shape), #433 (the follow), #428 (the move
that must not happen). ADR-0088 is a pointer here; CONTEXT.md's Worktree entry
records the flag, the lazy checkout and the base ladder.
