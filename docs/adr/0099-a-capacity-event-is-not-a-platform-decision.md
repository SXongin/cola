# A capacity event is not a platform decision

## Context

On 2026-11-04, hosted macOS arm64 runners starved: jobs queued with an empty
`runner_name`, ran zero steps, and were cancelled after ~15 minutes with "The
job was not acquired by Runner of type hosted even after multiple attempts".
ADR-0097 responded by taking `Test (macos-latest)` out of the required set and
moving the macOS leg to `main` pushes only.

The measurement that decision lacked came afterwards, and it changes the
picture. Counting every macOS job the repository dispatched:

| window (UTC) | macOS jobs | never acquired a runner |
| --- | --- | --- |
| 11-01 – 11-03 | 83 | 3 (sporadic) |
| 11-04 00:00–14:59 | 90 | **0** |
| 11-04 15:00–15:59 | 17 | 15 |
| 11-04 16:00–16:59 | 7 | 7 |
| 11-04 17:00–17:59 | 13 | 9 |
| 11-04 18:00 onwards | 5 | **0** |

So it was a **three-hour capacity event**, bracketed by zero failures before and
after, on a pool with a ~3–4 % baseline of un-acquired jobs that a re-run clears.
ADR-0097 was calibrated on the worst three hours of a resource whose ordinary
behaviour is fine.

The rule also does not generalise. "If a runner pool starves, take that platform
out of the required set" applies equally to Windows and to Linux, and applying
it as written removes the gate one outage at a time. A required check that
depends on a shared pool is a real cost, but it is the cost of having a gate;
the answer is to make runs cheaper, not to stop gating the platform that
happened to be scarce this week.

## Decision

Every platform leg of `Test` is required, on every pull request:

- `Test (macos-latest)` is back in `main: CI`'s required set — seven contexts
  with `Test (windows-latest)`, `Format`, `Check`, `Live OpenCode`,
  `Dependency audit` and `Docs drift`;
- the `test` job's matrix is the static `[macos-latest, windows-latest]` again,
  not event-dependent;
- a starvation event is answered with `gh run rerun --failed`, and the run's
  job is to be cheap enough that the wait is short. ADR-0097 is superseded.

The cost work is deliberately separate from the gating policy: fewer jobs and
less duplicated compile per job are the levers (each live/test job compiles the
crate before running a handful of tests), and they do not depend on which pool
was scarce.

## Consequences

- A PR can still stall ~15 minutes and then need a re-run during a capacity
  event. That is accepted as the price of a uniform gate, and it is identical
  on every platform.
- Retention of the previous split (macOS only on `main`) is rejected: it made
  the gate silently asymmetric in a way nothing in the workflow would ever
  surface again.
- The macOS leg remains a macOS-only compile-break detector *before* merge,
  which `release.yml` alone could only catch at release time.
- If a pool starves for long enough that re-runs stop working, the recorded
  remedy is a maintainer decision on the ruleset, made once, in the open — not
  a workflow change that quietly weakens one platform.
