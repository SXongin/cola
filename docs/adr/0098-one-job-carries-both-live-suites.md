# One job carries both live contract suites

## Context

ADR-0057 left the CI shape of the live contract suite open: "Live V1 / Live V2,
or one generation matrix". It shipped as two jobs, `Live V1` and `Live V2`, and
both are required contexts of the `main: CI` ruleset.

Each job is almost entirely a **compile**. The suites are `#[ignore]`-gated: a
few tests that drive a real pinned `opencode` binary against a scripted
provider. Measured on a green PR (2026-11-04), `Live V1` was 2.3 minutes and
`Live V2` 2.0 — for the same crate, built twice, because the two jobs differ
only in which binary is on `PATH`.

The installs are compatible by construction: `.github/actions/install-opencode-v1`
exposes `opencode` (the pinned 1.18.31 release asset) and
`.github/actions/install-opencode-v2` exposes `opencode2` (the npm pin), so
neither shadows the other when both run in one job. On this repository every
push is a public-repo run, but compute is not free in wall-clock terms: the
duplicate compile competes with the other jobs for the same runner pool.

## Decision

One job, `Live OpenCode`, runs both suites: install V1 → `live_v1` → install V2
→ `live_v2`. It is the single required live context, replacing `Live V1` and
`Live V2`, and the ruleset change lands with the workflow.

The V2 steps carry `always()` (still behind the `heavy`/`release` gate) so a
broken V1 chain cannot hide the V2 result for the same commit; the failing suite
is named by its step in the job log.

## Consequences

- One compile per push instead of two, and the required live **contexts** go from
  two to one (the total in the ruleset depends on the other gating decisions of
  the day; `CONTRIBUTING.md` "Pull request rules" is the source).
- A V1 failure no longer stops V2 from running, but both share one check: a red
  `Live OpenCode` still names one step, and the pair is only distinguished in
  the log. That is the same information a reviewer had before, minus the second
  check name.
- The two pins stay independent (spec #364 §11): V1 retirement still deletes the
  V1 steps without touching V2's install or pin policy.
- A future third generation joins this job until its compile cost outweighs the
  second runner; the split point is a measured decision, not a rule.
