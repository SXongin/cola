# Retire CodeQL

## Context

CodeQL (advanced setup; `.github/workflows/codeql.yml`, wired into the
Scorecard posture by ADR-0034) ran for the repository's first four weeks: 640
runs in 29 days — 403 on `pull_request`, 233 on `main` pushes, 4 scheduled —
about 6.2 minutes each on the most recent 100.

Its output over that window: 52 alerts, **zero true positives**. The classes
were all heuristic credential findings (test-only fixture literals, the one
deliberate loopback bootstrap password from ADR-0013), an empty-string false
positive, and two line-drift re-reports; every alert was dismissed under the
policy this repository recorded for it (`CONTRIBUTING.md`, issue #383). No code
change ever came out of it, and the test-tree `paths-ignore` filter (#452) made
it quiet rather than useful — quiet because the noise was excused, not because
the scanner had nothing to say.

The class it kept flagging is exactly what secret scanning with push protection
covers, with a purpose-built tool that sees all history and blocks a push
rather than reporting after the fact — and it is enabled on this repository.
The classes only CodeQL covers in Rust (path injection, command injection,
cleartext transmission, …) never fired. Rust query coverage is young and may
mature; the evidence so far is a six-minute analysis on every atomic PR push
that had never once changed a line of code.

## Decision

Retire CodeQL: delete `.github/workflows/codeql.yml` and its configuration
(`.github/codeql/codeql-config.yml`, the test-tree `paths-ignore` list), and the
README badge. `CONTRIBUTING.md` ("Security scanning") and the `AGENTS.md` check
gates record the posture; ADR-0033 and ADR-0034 carry amendments because their
texts assumed CodeQL stays.

Nothing else moves: `cargo xtask release`'s `watch_checks` is a generic
`gh pr checks --watch` and needs no change; the `main: CI` ruleset never
required CodeQL; secret scanning, `cargo-deny` (`Dependency audit`), the weekly
pbbp2 fuzz job, and review remain.

## Considered options

- **Keep it for Scorecard's `SAST` check.** Rejected: ADR-0034's own posture is
  "real supply-chain risk, with the score as a byproduct". Keeping a scanner
  that has never found anything, to hold a check, is the theater that ADR
  rejects. The check becomes a recorded deliberate no instead.
- **Downgrade to `main` pushes + the weekly schedule** (no `pull_request`
  trigger): would remove 403 of the 640 runs and keep a post-merge net, but the
  speculative value is unchanged and PR time — the only moment a finding could
  still change the merge — is when it would have been useful. If Rust coverage
  matures, this is the shape to re-add first.
- **Keep it as-is after the test-tree filter** (0 open alerts). Rejected: quiet
  by dismissal, not by evidence; the per-push cost stays.
- **Exclude the credential query classes via `query-filters`.** Rejected: that
  hides a class in production code to silence test-only noise, and the noise
  was already structurally solved by the path filter — the problem was value.

## Consequences

- The Security tab loses first-party code scanning; Scorecard's supply-chain
  SARIF uploads remain, and the dismissed CodeQL alerts stay as history.
- Scorecard's `SAST` check returns to 0; the ADR-0034 amendment records it as a
  deliberate no, superseding the "heals on its own" expectation.
- The release cut no longer has a CodeQL check to gate on (ADR-0033
  amendment); it still waits on every check that reports.
- Re-adding a scanner is one workflow file away. The bar it must clear next
  time is this Context, not a Scorecard point: evidence that the queries catch
  this codebase's own classes (path handling, process spawning, cleartext).

Related: #383, #452, ADR-0033, ADR-0034.
