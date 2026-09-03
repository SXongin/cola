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

## Commit conventions

Enforced locally by `scripts/check-commit-msg.sh` (via lefthook `commit-msg`) and
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

## Pull request rules

1. **Title** is a Conventional Commits subject (used as the squash-merge title).
2. **Description** answers: what changed, why, and how it was verified.
3. Reference the originating issue/spec when one exists (`.scratch/<feature>/`
   files or a GitHub issue number).
4. Record architectural decisions as ADRs in `docs/adr/` when the change is
   hard to reverse.
5. Do not merge until CI (fmt, clippy, test, release build, dependency audit) is
   green.

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

## Releasing

Tags are strict semver **without** a `v` prefix and must match `Cargo.toml`'s
`version` (the embedded version drives self-update — a mismatch reports "update
available" forever):

```bash
git tag 1.2.3 && git push origin 1.2.3
```

`.github/workflows/release.yml` builds all three platforms and attaches the
binaries + `SHA256SUMS` to a GitHub release.