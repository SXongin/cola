# Worktree: cola creates the checkout with git and re-roots by Pending Session

Issue #433 taught cola to **follow** a Session that has already moved into a linked
worktree: the directory mapping follows the server's location and the **Turn
Footer** marks the checkout (`📁 zh-user-guide 🌲 · docs/zh-user-guide`). What it
could not do was **start** work in one — the worktree had to exist first, and
asking the agent to create-and-enter one mid-turn is #428, where the move
interrupts the in-flight step and no follow-up can undo it. #449's `/worktree`
closes that half: cola creates the checkout with git, then declares a **Pending
Session** there, so the conversation's next message *starts* in the worktree and
nothing is ever moved.

## Context

- **The server API exists, but it only detaches.** OpenCode 2.1.0 exposes
  `GET /api/worktree?projectID=…` (list), `POST /api/worktree` (create, taking
  `{projectID, from?, branch?, directory?, name?}`) and `DELETE /api/worktree`
  (remove), and `GET /api/location?location[directory]=…` resolves the
  `projectID` cola would need. But every create runs
  `git worktree add --detach <path> <ref>` (`packages/core/src/git.ts`): the
  `branch` field is a *start point*, never a branch to create. Measured
  2026-10-21 on 2.1.0 — a create without `branch` lands on a detached HEAD, and
  `branch: "wt/x"` for a branch that does not exist is
  `fatal: invalid reference: wt/x`. #449's 2026-09-30 measurement (2.0.18,
  `/api/experimental/worktree` → 404) recorded a **path move**, not a missing
  capability.
- **The server's list is an inventory, not a git view.** It returns
  `[{directory, strategy}]` — the main checkout included, directories that no
  longer exist kept until `POST /api/worktree/refresh` — and carries neither the
  branch nor the dirty state. Both are exactly what a user picks a worktree to
  see, and both are git reads either way.
- **The root already has two conventions.** This repository sets
  `worktree.directory = ".worktrees"` in `opencode.json`; the server's default is
  outside the repository (`<server data dir>/worktree/<first six of the project
  id>`). Reading the project's OpenCode configuration from cola would mean
  re-implementing a layered resolver (global + project + environment) — a
  half-implementation is worse than a second switch the documentation names.
- **No new dependency.** `src/git.rs` already shells out to git for the Turn
  Footer, so `git worktree list --porcelain` and `git worktree add` reuse it.
- **#433's degradation stands.** A worktree removed later is a missing
  directory; the follow loop and the footer already handle that without a new
  failure mode, so creation adds no lifecycle cola must own.

## Decision

- **cola runs git, not `/api/worktree`.** `/worktree` lists
  `git worktree list --porcelain`; `/worktree new` runs
  `git worktree add -b`. A branch — a thing the session can commit to and push —
  is the reason worktrees exist here, and the server API cannot produce one. This
  also keeps V1 and V2 identical and avoids a `projectID` resolution cola
  otherwise never performs. The server API stays a possible fast path later.
- **The root is cola's own setting.** `[worktree] root`, default `.worktrees`,
  resolved against the **primary checkout** (`git rev-parse --git-common-dir`'s
  parent), never the session's own directory — that session may itself be in a
  worktree, and nesting a worktree inside one is never the intent. An absolute
  root is allowed.
- **The name is required and becomes the slug.** `/worktree new` without one
  answers with usage; there is no generated name. Normalisation is mechanical:
  trim, lowercase, whitespace/underscore → `-`, collapse repeats, trim `-`; then
  the result must match `[a-z0-9.-]` and be at most 40 characters (the name
  rides the Turn Footer's `📁` segment). Anything else — Chinese included — is
  refused with a `user-guide` example: transliteration is a table that is always
  partly wrong, and a slug must stay typeable across machines. Directory is
  `<root>/<slug>`, branch is `wt/<slug>`: the prefix separates throwaway worktree
  branches from real delivery branches, and a session that becomes a PR renames
  the branch first.
- **The base is where the conversation stands.** The current session directory's
  `HEAD`, overridable with `--base <ref>`. The default branch is never guessed
  (`origin/HEAD` may be unset or point at a stale name).
- **Collisions reuse, never silently destroy.** An existing directory gets a
  `-2`… suffix (the branch follows it). An existing branch that no worktree has
  checked out is attached to the new checkout — "start the new directory on that
  branch" is the natural intent; a branch already checked out elsewhere is
  refused, naming the checkout that holds it (the card's toast is the place for
  a path the user can act on, not git's raw `already checked out` line).
- **Creation declares, it never moves.** `new` writes a **Pending Session**
  (ADR-0041) titled with the worktree's name, so the next message creates the
  Session in the worktree. Nothing in the server moves, which is precisely why
  #428's interruption cannot occur. A bound **Topic** is refused like `/dir`
  (`TOPIC_SELECTION_REJECTION`); a busy session is allowed; an existing Pending
  Session is replaced, as repeating `/dir` is.
- **The card lists the project's checkouts, and pick means a new Session.** Rows
  carry the directory's name, branch, dirty marker, session count and current
  marker; the main checkout is a row labelled 「主检出」. Picking declares a
  Pending Session — the same one meaning `/dir`'s card has — and never adopts:
  `/switch` is the adoption surface, and one card with two meanings is the
  harder thing to explain. The current directory falls back to `bridge.work_dir`
  when the conversation has neither a session nor a pending, matching `/new`.
- **Dirty is measured, bounded.** One `git status --porcelain` per worktree,
  concurrency 4 under a 5 s overall bound; a row that misses the bound renders
  `?` rather than pretending to be clean. The list is not cached.
- **An unignored root is a warning, not an edit.** After a successful create,
  `git check-ignore` decides whether the reply adds "`.worktrees/` is not
  ignored, the main checkout will read dirty". cola never edits the user's
  `.gitignore`.
- **cola creates, never removes.** Removal stays git's (or OpenChamber's) job;
  `/worktree` reports what exists. The guards a remove needs — clean vs dirty,
  force, and mappings that outlive a directory — are their own decision, and a
  removed worktree already degrades to #433's missing-directory behavior.

## Considered options

- **Create through `POST /api/worktree`.** Rejected: `--detach` is hard-wired,
  so a Session started there works on no branch; `branch` only picks a start
  point. It would also tie the command to V2 and to a `projectID` lookup.
- **List through the server's inventory.** Rejected: no branch, no dirty state,
  stale entries until `refresh`, and the main checkout indistinguishably mixed
  in — while the branch/dirty reads need `git` anyway.
- **A fixed `.worktrees`, no configuration.** Rejected: a user project need not
  ignore that path, and repository layouts differ; the setting is one line.
- **Read the project's `opencode.json` for the root.** Rejected: that is a
  layered config resolver cola does not otherwise implement, and the failure mode
  (two clients disagreeing about where worktrees live) is silent.
- **Auto-append `.worktrees/` to `.gitignore`.** Rejected: it rewrites a user's
  repository and can land in a commit nobody reviewed.
- **Accept Chinese names, or transliterate them.** Rejected: a slug that cannot
  be typed on another machine, or a transliteration table that is always partly
  wrong, for a value the user can choose in English.
- **Make pick adopt an existing Session when the directory has one.** Rejected:
  the same click would mean "create" or "adopt" depending on invisible history;
  the row's session count carries that information without changing the action.
- **Removal in the same change.** Rejected: clean/dirty and force guards plus
  mappings that outlive a directory are a separate decision with their own
  failure modes.

## Consequences

- The root can be stated twice — `[worktree] root` and the project's
  `opencode.json worktree.directory` — and the documentation must say they should
  agree. cola reads only its own.
- A worktree created here is unknown to the server's inventory until OpenCode's
  own discovery runs; cola does not care, because a Session is created **in** a
  directory, not **for** a worktree resource.
- Listing costs one `git worktree list` plus N status reads, bounded at 5 s on
  the command path; a hung filesystem shows `?` instead of blocking the card.
  A pick is a card callback on a 2 s ack deadline, so it validates with one
  short list read and gives the refresh only what is left — a read past that
  bound leaves the row `?` rather than holding the ack hostage.
- ADR-0041 gains a creation surface (`/worktree new`); nothing else in the
  session model changes — a worktree Session is an ordinary Session whose
  directory happens to be a linked checkout.
- A removed worktree keeps #433's behavior: no directory, no git facts on the
  footer, no new hard failure.
