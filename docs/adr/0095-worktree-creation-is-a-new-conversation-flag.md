# Worktree creation is a flag on the new-conversation flow

ADR-0088 gave cola `/worktree` (a read-only inventory card) and
`/worktree new <name>`, which created the checkout **eagerly** and declared a
Pending Session in it. That shape was never released: no tag contains
`4fd148fa`. Before it ships, #890 identifies what is wrong with it: creation is
a decision about a **new conversation**, yet it lives in a command that runs
ahead of one; the base defaults to the source's `HEAD`, so a worktree made from
a feature branch silently inherits its commits (the 22-commit offset discovered
only when the PR opened); the card duplicates `/dir` and `/switch`; and an
abandoned conversation leaves a checkout behind. This ADR replaces the shape.
The parts of ADR-0088 that still hold — cola runs git, the root is cola's own
setting, cola creates and never removes — stand.

## Context

- **The two-step dance.** `/topic` is refused inside every topic, and
  `/worktree new` is refused in a bound topic. Starting isolated work is
  therefore `/topic` in the main conversation, `/worktree new` in the still
  pending topic, then the first message. Only the middle step is absent from
  the new-conversation commands.
- **`--base` defaults to `HEAD`.** `create_worktree` falls back to
  `base.unwrap_or("HEAD")`, so the source checkout's branch — not the project's
  integration line — chooses what the work starts from. The wrong base is
  invisible until the PR.
- **Creation is eager.** `bridge::worktree::create` runs `git worktree add`
  first and declares the pending second, so a corrected or abandoned topic
  leaves a checkout nobody uses.
- **The session title and the filesystem name are different things.** The
  server generates the title from the first message (ADR-0007), in any
  language, and it can be changed at any time. The directory must exist before
  the session, and the branch should be checked out with it; both need an ASCII
  slug. The old command used the slug as the session title, which forced an
  English name onto every worktree conversation.
- **A title cannot be moved into a directory.** OpenCode can move a session into
  a worktree, but Codex has no equivalent, and moving a running session is the
  failure #428 was about. The universal flow cannot wait for the title and then
  relocate the session.

## Decision

1. **Creation is a flag on the new-conversation flow.** `--worktree` on
   `/topic` and `/new`; nothing is created without it. Adoption and pick
   (`/dir`, `/switch`, `/topic --adopt`) never create: an existing directory is
   already a checkout or not, so there is no "default-on" question.
2. **Creation is lazy: at materialisation, not at the command.** The Pending
   Session carries its **source** directory plus a worktree intent (base ref,
   resolved SHA, slug, planned target). The checkout is created when the first
   non-command message materialises the session (ADR-0041), so an abandoned
   pending creates nothing. Commands in the pending window (`/cat`, `/diff`,
   `/dir`, the cover) read the **source**; the target is computed by the
   intent.
3. **The base is the integration line, never `HEAD`.** Ladder:
   `[worktree] base` → `origin/HEAD` → `origin/main` → `origin/master` → local
   `main` → `master`; if none resolves, refuse and ask for `--base`. Resolve the
   ref to a **SHA at command time**, store it in the intent, and create from
   that SHA, so the cover's promise is what gets built; a stale ref is visible
   as a stale SHA (cola never fetches). `--base HEAD` is the explicit "branch
   off the branch I am looking at" (stacked work). A configured ref that does
   not resolve is a refusal, never a silent fall-through. An existing branch is
   attached only when its tip is that SHA; a branch that starts elsewhere is
   refused (reusing the name must not build a different commit than the cover
   named).
4. **Preconditions fail at command time, before a pending is written.** The
   source must be a non-bare git work tree, the base must resolve, and the root
   must be creatable; otherwise reply `不能在当前目录创建 worktree：<reason>`. A
   source that is itself a linked worktree is allowed: `worktree_root` resolves
   to the primary checkout, so the new checkout is a sibling, never nested.
   `--base` without `--worktree` and `/topic --adopt --worktree` are usage
   errors.
5. **Adjustment is pending-only.** Bare `/worktree` reports the intent (target,
   branch, `base @ sha`, source); `/worktree --branch <branch>` and
   `/worktree --base <ref>` update it and re-render a pending topic's cover.
   Once the checkout exists — including a pending kept after a failed session
   create — the directory is fixed and the command says so instead of
   pretending a rename would move it.
6. **A creation failure at materialisation never loses the message.** Keep the
   pending, do not treat the first message as delivered, and reply that
   resending retries. The created checkout's path is recorded on the intent
   **before** the session is created, so a retry after a session-create failure
   reuses that checkout instead of making a `-2` sibling. The record is scoped
   to the claim's generation and the pending's epoch: a pending replaced while
   the checkout was being made cannot inherit its path, and `/worktree --branch`
   is refused while a materialisation owns the pending — both would otherwise
   route a later message into the wrong checkout.
7. **Retire `/worktree new` and the read-only card.** Discovery is
   `/run git worktree list`; entering an existing checkout is `/dir <path>`
   (adopting a session there stays `/switch`). Keep `[worktree] root`; keep
   "cola never removes worktrees" — lazy creation removes the abandoned-topic
   class, and a GC (clean + no live session + merged branch) is a follow-up.
8. **The title and the branch are separate inputs.** `--name <title>` is the
   session title, exactly like `/new <name>` and `/topic <dir> <name>`: any
   language, PATCHed at materialisation, omitted means the server auto-titles.
   `--branch <branch>` is the **full branch name**, with the repository's own
   shape allowed but never forced: `feat/fix-login` names the branch and the
   directory is its last segment (`<root>/fix-login`); a bare `fix-login` names
   both; no `wt/` prefix is added. Omitted, it is derived from the title with a
   lenient ASCII slugifier, and a title with no usable ASCII falls back to a
   deterministic `YYYYMMDD-1` token — never a refusal for a value the user did
   not type. Each component is validated by the strict slug rules (ASCII,
   lowercased, at most 40 characters, no `.lock` suffix, which git would reject
   only later inside `git worktree add`). The cover shows both before the first
   message, which is the naming veto window. `--base` without `--worktree` is a
   usage error; `/topic --adopt --worktree` is rejected; `--name` takes words
   up to the next flag, `--branch` and `--base` take one word.
9. **Alignment is a creation-time property.** The branch and directory share the
   branch's last segment when created. The user chooses the delivery shape up
   front (`feat/<slug>`), so no rename is needed at push time; if a branch is
   renamed later, the directory stays and the last segment is the
   correspondence, not an invariant. A directory collision suffixes only the
   directory; the branch keeps the exact name the user typed. The alternative —
   moving the directory with the branch — needs an unsupported session move and
   is rejected.
10. **No compatibility work.** The feature has never shipped.

## Considered options

- **Keep the card and `/worktree new`.** Rejected: the card duplicates `/dir`
  and `/switch`, and the command runs ahead of the conversation it belongs to.
- **Eager creation (`/topic --worktree` creates immediately).** Rejected: a
  corrected or abandoned topic leaves a checkout, and the pending window is the
  only free moment to change the base or the name.
- **Default creation on.** Rejected: a conversation that only reads code should
  not get a branch. The flag is the choice.
- **Derive the slug from the first message at materialisation.** Rejected: the
  first message is Chinese often enough that the fallback wins, a vague first
  message names nothing, the directory is permanent once created, and the
  server's title is generated independently anyway. Deriving at command time
  from `--name` keeps the veto.
- **An out-of-band model call before creation.** Rejected: its only input is
  the first message, which the user must already have phrased clearly; it
  duplicates or replaces the server's own title call (ADR-0007) and adds
  latency, cost, a failure path and retry nondeterminism to the first message.
- **Create the session first, then move it into the title-named checkout.**
  Rejected as the primary flow: OpenCode-only, Codex has no move, and it
  reintroduces the running-turn interruption #428 was about.
- **Create detached and name the branch later.** Rejected: the branch is the
  reason the checkout exists (ADR-0088), the title still arrives after the
  first prompt, and a first-turn commit would land detached.
- **Keep branch and directory equal for life.** Rejected: the delivery branch is
  `<type>/<slug>`, so equality would have to be broken at push time or the
  directory would have to move. The slug is the stable correspondence.
- **Transliterate Chinese titles into slugs.** Rejected: a table that is always
  partly wrong, for a name that is a handle rather than an identity. The date
  token is honest; the user can pass `--branch` or rename the branch later.

## Consequences

- `sessions.json` gains an optional `worktree` intent on a pending; an older
  binary ignores the unknown field, and an abandoned intent creates nothing.
- The pending's directory is the **source**, not the target; every
  pending-window reader keeps working without a new rule. Materialisation
  points the session entry at the created checkout before the session is
  created; the stored pending keeps its source until activation clears it.
  The command-time root check walks to the nearest existing ancestor, proves
  it writable with one transient probe directory, and does not create the
  root, so an abandoned pending leaves nothing on disk.
- The cover names the target/branch/base before the first message; a colliding
  directory at creation may still get a `-2` suffix, and materialisation
  replies with the actual path when that happens. The cover's path is a
  prediction, not a reservation. A recorded checkout that disappears before
  the retry is reported as missing; the next message recreates it, and a new
  slug/base is allowed again.
- An older binary ignores the unknown `worktree` field and would materialise in
  the source directory. The feature has never shipped (decision 10), so that
  downgrade is not supported; this is a forward-compatible format only.
- The `[worktree] base` setting can disagree with the source checkout's branch;
  that is the point, and the reason is documented on the setting.
- The feature's surface shrinks to a flag on two commands plus one pending
  adjuster. `/new --worktree` is the first thing to cut if the surface still
  feels large: topics are the natural isolation unit.
- A worktree session still behaves like any other session: the Turn Footer's 🌲
  marker, the removed-directory ending (#450), and the external-move follow
  (#433) are unchanged.
