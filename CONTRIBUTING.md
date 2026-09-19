# Contributing to cola

Welcome! This file is the contract for how changes land in this repo. Agents and
humans alike are expected to follow it — the git hooks, CI, and code review all
assume it.

## Before you start

- Read `AGENTS.md` — especially **Known pitfalls** — before touching the
  OpenCode server API, Feishu card/WS integration, or the bridge protocol. Those
  pitfalls are hard-won and cheap to re-introduce.
- Read `CONTEXT.md` and `docs/adr/` before designing anything: use the glossary's
  vocabulary and don't silently contradict an existing ADR (surface it instead).
- Keep PRs small and focused on one thing. A reviewable diff is more valuable
  than a big one.

## Branch workflow

This is a **trunk-based (TBD)** repo with **rebase-linear** history. `main` is
the only long-lived branch; it receives merges only. Every piece of work rides
a short-lived branch pulled off `main` and merged back to `main` via PR.

The per-task loop is always:

1. **Start on `main`** — `git switch main && git pull` (check
   `git branch --show-current` first; never start from a stale checkout).
2. **Branch off `main`** with a short descriptive name (`<type>/<slug>`, see
   *Branch naming* below).
3. **Do the work** on that branch, in small Conventional Commits.
4. **Rebase-linear before merging**: `git switch main && git pull`, then
   `git switch <branch> && git rebase main` so the branch sits on the latest
   `main` (a rebase, not a merge — history stays linear).
5. **Open a PR** with base `main`, pass the verification loop, and let `main`
   receive a rebase-merge (the repo's PR merge setting: rebase merges keep
   the branch's commits verbatim on `main`; squash is disabled in settings).
   The branch is deleted once merged.
6. **Return to `main`** for the next task — a fresh branch, never reusing one.

Keep one PR to one thing (see *Before you start*) and the branch short-lived.

This rule applies to agents too: **agents never merge a feature/task PR.**
They run the loop up to the green PR — branch, work, commit, rebase, open the
PR, get CI and review green — and then stop. The user verifies the change
themselves and gives the explicit go-ahead (or merges it themselves). The
release cut is the one exception: `cargo xtask release` performs its own
CI-gated merge after the user confirms the smoke test (ADR-0033). Do not stop
at "branch off `main`" or ask which base to use; the base is always `main`.

### Branch naming

A branch name is `<type>/<slug>`:

- **`type`** is the Conventional Commits type, matching the change's nature:
  `feat/`, `fix/`, `docs/`, `refactor/`, `test/`, `chore/`, `ci/`, `build/`,
  `perf/`, `revert/`. A branch that only touches docs (including `.scratch/`)
  is `docs/`; a release cut uses the branch-only `release/` type
  (`release/<version>`), the one type that is not a commit type.
- **`slug`** is short, `kebab-case`, and says what the branch changes — the
  noun of the change, not the whole sentence. One feature, one fix, one branch.

Examples:

```
feat/bridge-dir-recent-card
fix/switch-card-action-tag
docs/feishu-permissions
release/0.6.2
```

## Commit conventions

Enforced locally by `cargo xtask check-commit-msg` (via lefthook `commit-msg`) and
checked by anyone reviewing:

- Subjects follow Conventional Commits: `<type>(<scope>)?: <subject>`
- Allowed types: `feat`, `fix`, `docs`, `style`, `refactor`, `test`, `chore`,
  `ci`, `build`, `perf`, `revert`
- `scope` is optional and lowercase (`fix(bridge): ...`)
- Subject line ≤ 72 characters
- `Merge` / `Revert` prefixes are tolerated by the hook

Examples:

```
feat(bridge): permission card gains an auto-accept toggle
fix(feishu): cap question form name under Feishu's 100-char limit
docs(adr): turn footer shows work context
```

## Verification loop (must pass before pushing)

Git hooks (lefthook) enforce the cheap ones automatically — `pre-commit` runs
`fmt` + `clippy` when Rust files are staged, `commit-msg` checks Conventional
Commits, `pre-push` runs the dependency audit. The full loop CI runs:

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace --locked
cargo build --release --locked
```

Note: CI's Format job is `cargo fmt --all -- --check` — clippy and rustc do
**not** check formatting, so a clean clippy does not mean a clean fmt.

`Dependency audit` runs `cargo deny check` on every push and PR; a scheduled
`Advisory audit` workflow re-runs `cargo deny check advisories` weekly, so
newly published RUSTSEC advisories surface even when nothing is pushed.

## Pull request rules

1. **Title** is a Conventional Commits subject (kept verbatim by the rebase
   merge — it lands on `main` as the commit message's subject, so make it
   accurate and complete).
2. **Description** answers: what changed, why, and how it was verified.
3. Reference the originating issue/spec when one exists (a GitHub issue
   number; archive specs live under `.scratch/<feature>/`).
4. Record architectural decisions as ADRs in `docs/adr/` when the change is
   hard to reverse.
5. Do not merge until CI (fmt, clippy, test, release build, dependency audit) is
   green. Agent-authored PRs additionally wait for the user's own verification
   and explicit go-ahead before merging (see *Branch workflow*; the release cut
   is the ADR-0033 exception).

The `main: CI` ruleset requires exactly those five checks — judge merge-readiness
with `gh pr checks <branch> --required`, not by eyeballing every check. CodeQL
also runs on PRs, but it is advisory: it is not a required check, so a slow or
red CodeQL result never blocks a normal merge. The release cut is the one flow
that makes it required (its watch covers every check).

Release PRs (`release/*`) and docs-only PRs skip the code-dependent jobs
(Format, Check, Coverage, Test): a skipped job reports success and still
satisfies the `main: CI` ruleset, so the PR stays mergeable without spending
runner time on code that did not change. `Dependency audit` always runs, and
the release cut keeps its CodeQL gate.

Description template:

```markdown
## What
<what changed, one paragraph>

## Why
<the problem this solves / the issue it closes>

## How tested
<commands run; e.g. the verification loop above, specific cargo test filters>
```

## Reviewing

Reviews check two axes separately:

- **Spec**: does the diff implement what the originating issue/spec asked for?
- **Standards**: does it follow `CODING_STANDARDS.md` and the repo conventions?

Flag any place where the diff contradicts an existing ADR explicitly rather than
silently overriding it.

Every non-draft PR gets an OpenCode review
(`.github/workflows/opencode-review.yml`, ADR-0034). It runs on `opened`,
`synchronize`, `reopened` and `ready_for_review` with the OpenCode Go
subscription, reviews the diff on the two axes above, and — when it finds
nothing blocking — `github-actions[bot]` approves. That approval is what
satisfies the `main: review` ruleset's required review for solo work. A
blocking review is a comment with a `FAIL` verdict and no approval, not a
`request-changes`: Actions bots cannot dismiss their own review and would lock
the PR. Fork PRs are skipped (no secrets) and Dependabot PRs are skipped
(upstream's permission check rejects bot actors). Release PRs (`release/*`) are
skipped too: their diff is a version bump of code already reviewed on `main`,
and the cut merges with the admin bypass. The admin bypass remains for
emergencies; the bot can never lock the maintainer out.

## Releasing

A release is cut with one command from a clean, up-to-date `main` (ADR-0033):

```bash
cargo xtask release 1.2.3
```

The command is the whole process — do not hand-edit the version or tag by hand:

1. Prints the [release smoke test](#release-smoke-test) and waits for
   confirmation. `--yes` skips the prompt; pass it only after a human confirmed
   the smoke test (agents: ask in chat first).
2. Bumps `Cargo.toml`/`Cargo.lock`, commits `chore(release): bump version to
   1.2.3` on a `release/1.2.3` branch and pushes it.
3. Opens the PR and watches every check — the release cut is the one flow where
   advisory CodeQL gates too. The PR skips the code-dependent jobs and the
   automated review (the cut merges with the admin bypass), so what it waits on
   is Dependency audit and CodeQL.
4. Rebase-merges with the admin bypass once every check is green (the
   `main: review` ruleset requires a PR and one approval — the admin role
   bypasses both; the `main: CI` ruleset has no bypass, so the checks are
   always enforced).
5. Pulls `main` and tags the **merged** commit `1.2.3`, then pushes the tag.

Tags are strict semver **without** a `v` prefix and must match `Cargo.toml`'s
`version` (the embedded version drives self-update — a mismatch reports "update
available" forever, ADR-0015). The tag must sit on the commit `main` actually
contains: a rebase merge can rewrite the branch commit, so a tag created
before the merge can be left behind (the `0.7.0` failure ADR-0033 cites). The
command enforces both invariants.

Re-running the command after a failure resumes from the state it finds (fresh
cut, release branch before the merge, or merged cut with the tag missing); it
never force-pushes and never moves a tag.

`.github/workflows/release.yml` then builds all three platforms and attaches the
binaries + `SHA256SUMS` to a GitHub release, then publishes the crate to
crates.io as `colark`. The GitHub release runs first on purpose (ADR-0030): its
assets are the self-update channel and must appear atomically, and a crates.io
release cannot be revoked. Two operational rules follow:

- If the crates-io job fails after a release, rerun it (or cut a patch
  version) — a GitHub release with no crates.io version means cargo installs
  cannot reach that version.
- If a version must be yanked on crates.io, remove the matching GitHub Release
  too, or GitHub-channel installs keep receiving it.

Release notes are generated from merged PR titles (`generate_release_notes`).
A release that ships a user-visible compatibility change must add the note
itself (`gh release edit <version>`) — e.g. ADR-0041's `sessions.json` format
change, which older binaries read as an empty mapping.

### Release smoke test

`cargo xtask release` prints this checklist and waits for confirmation
(`--yes` skips it). It is the only check that Feishu itself accepts the cards
cola builds, so run it in a real Feishu chat — no second bot or test group is
needed:

1. **Send a message** — the Done card renders and updates in place as the turn
   progresses (reasoning collapses, tool panels appear in call order, text
   streams) without duplicated text.
2. **Tool permission** — with `/autoaccept` off, make the agent call a gated
   tool (e.g. ask it to run a shell command). The permission card's
   允许一次 / 始终允许 / 拒绝 buttons round-trip and the answer reaches the
   tool.
3. **Question tool** — ask the agent to use the `question` tool. The question
   card's buttons round-trip and the turn continues with the chosen answer.
4. **`/topic` in a group** — @ the bot with `/topic`. The topic is created, it
   shows up in the group's topic list, and its cover card is the topic root.
5. **`/model`** — the model picker card appears, and a picked model shows in
   the next message's footer.
6. **`/dir <non-git directory>`** — the session starts there without error.
