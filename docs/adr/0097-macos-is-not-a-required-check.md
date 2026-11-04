# macOS is not a required check

## Context

`Test (macos-latest)` was one of the eight required contexts of the `main: CI`
ruleset. On 2026-11-04 it stopped being obtainable: three macOS jobs on run
41851644811 sat queued with an empty `runner_name`, executed **zero** steps, and
were cancelled after 15m03s; the rerun's `Test (macos-latest)` repeated it at
15m07s, and one `Live Codex tier-2 (macos)` job reached 30m22s before failing.
Across Nov 1–4 the repository dispatched 205 macOS job instances, 19 of which
never acquired a runner, for 787 macOS job-minutes.

This is **not** a spending limit. The repository is public, so GitHub Actions
usage on standard hosted runners is not billed; the constraint is hosted macOS
arm64 **capacity**. GitHub reports it as

```
The job was not acquired by Runner of type hosted even after multiple attempts
Due to capacity constraints, jobs targeting macOS arm64 runners may experience longer queue times.
```

and gives up after about 15 minutes. A required check that cannot be acquired is
worse than a missing one: the PR is not merely unverified, it is blocked, and
each push pays 15 minutes of queue to learn that.

Three macOS jobs ran per push before #979 merged the two Codex live tiers: the
Test leg (~7 min), and tier-1 and tier-2 (~5 min each).

## Decision

The `test` job's platform follows the event:

```yaml
matrix:
  os: ${{ fromJSON(github.event_name == 'pull_request' && '["windows-latest"]' || '["macos-latest"]') }}
```

- a **pull request** runs `Test (windows-latest)`;
- a **push to `main`** runs `Test (macos-latest)`;
- Linux stays the `Coverage` job on both events.

`main: CI` therefore requires **seven** checks — `Format`, `Check`,
`Test (windows-latest)`, `Live V1`, `Live V2`, `Dependency audit`,
`Docs drift` — and amends ADR-0033's amendment of 2026-09-23, which described
both `Test (…)` contexts as required.

The advisory Codex live jobs keep their macOS leg: a starved runner there leaves
a yellow check instead of blocking a merge.

## Considered options

- **Keep macOS required and retry on the "not acquired" annotation.** Rejected:
  every retry competes for the same scarce pool, and the PR still stalls 15+
  minutes per attempt before it can fail over.
- **Compile-only macOS on pull requests** (`cargo test --no-run`). Rejected: it
  needs a macOS runner to start, so it starves exactly like the test run does,
  and it is the *run* that the platform signal is worth having for.
- **Drop macOS from CI entirely.** Rejected: the platform signal would then only
  appear at release time. A merge to `main` is the first place a macOS-only
  regression can be seen, and it is early enough to revert a merge.

## Consequences

- PR wall time no longer depends on macOS acquisition; the critical path becomes
  `Test (windows-latest)`, ~7 minutes.
- A macOS-only regression is discovered **after** the merge, on the `main` push,
  instead of on the PR. The window is one merge, and the remedy is a revert —
  acceptable because macOS-specific code paths are platform-gated and
  `release.yml` builds all three platforms before a release.
- A fork or a new ruleset must repeat the seven-check list; `CONTRIBUTING.md`
  "Pull request rules" is the single source for it.
- Nothing here reduces *what* macOS runs, only *when* it gates: the merge still
  gets the full workspace suite on macOS.
- Because macOS gating moved behind the merge, the run that covers a merge must
  not be cancellable by the next one: a `main` push gets a concurrency group of
  its own (workflow + ref + the pushed sha), so two merges in flight run both
  macOS legs instead of the later one cancelling the earlier. Pull requests keep
  the superseded-run cancellation, which is what that policy is for.
