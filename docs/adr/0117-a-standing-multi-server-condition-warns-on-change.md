# A standing multi-server condition logs on change, not per pass

> **Amends ADR-0085**: the "each reconcile pass WARNs" clause is replaced by
> WARN once per distinct canonical + ignored set, DEBUG on an identical repeat,
> INFO when it clears (issue #1209).

## Context

ADR-0085 decided that when several default-store servers exist, each reconcile
pass names the canonical server and every ignored one, so cola's choice is
visible instead of silently flapping. That decision predates the cadence it now
runs at: the reconnect loop reconciles every 5 s
(`RECONNECT_POLL_INTERVAL_SECS`, `src/bridge/pollers.rs`), and an adopted
Coexistent server never leaves the set — ADR-0085 forbids reaping it. So a
**standing** condition was re-WARNed verbatim, forever: 236,816 lines over 13
retained daily files, 94% of the log on a host running OpenChamber beside a
`serve --service` daemon (issue #1209). It also sidestepped the level policy
the loop had already declared for itself: `PollLoop` latches only `Err`
returns, and this line is emitted inside the pass (ADR-0048).

Nothing is failing when the line fires — cola's choice is correct — and the
fact is already queryable (`other_servers` in `cola status`, its `--json`, and
`cola doctor`). Repetition carried no information: only the timestamp changed.

## Decision

**The ignored set is a condition, not a per-pass event.** Keyed on the
canonical server and the ignored set (URL, pid and service/not-a-service
class, in discovery's deterministic order):

- **WARN** on the first observation and whenever the set changes (a different
  canonical, an added or departed ignored server, a replace with a new pid);
- **DEBUG** on an identical repeat, so a `cola=debug` trace still shows the
  condition is alive every pass;
- **INFO** once when it clears back to a single default-store server;
- a pass that finds **no** server at all leaves the latch untouched — its own
  serverless path reports that transition, and a genuinely new set carries new
  pids.

The latch lives on the `OpenCodeBackend` (behind an `Arc`, shared by every
clone) so it outlives a pass; a per-clone latch would warn once per clone for
the same condition. The line keeps its historical `multiple default-store
OpenCode servers` prefix so existing log filters and triage greps still match,
and still names the canonical pid and each ignored server's URL, pid and class.

No periodic heartbeat is added. A state-change line plus `other_servers` is
the visibility ADR-0085 asked for; the issue's "one line per day" was optional
hardening, and a timer would add a path whose only output repeats a fact the
status surfaces already answer on demand.

## Considered options

- **Keep the per-pass WARN.** Rejected: measured 17k lines / 3.7 MB per day,
  pushing actionable lines (including real Feishu `230429` frequency limits)
  out of the 13-day rotation.
- **Return the fact from `ensure_attached` and let `PollLoop`'s latch report
  it.** Rejected: it would put an OpenCode-only notion (several default-store
  servers) into `AttachOutcome`, the client-neutral contract the Codex adapter
  also implements — a variant and ~69 match sites for a log line.
- **A daily reminder while the condition stands.** Rejected: an extra timer
  path to test for no information the status command cannot answer on demand.
