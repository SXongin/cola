# An exit hands back what the process holds, best-effort

## Context

cola attaches to runtimes it does not own (ADR-0013, ADR-0090). Some of those
runtimes keep state on cola's behalf: a Codex connection holds a subscription per
resumed thread, and the daemon keeps an unsubscribed thread **loaded** for its
own grace window (issue #918 measured a provider switch failing for ten minutes
after a restart, caused by that window).

Issue #918 asked for `thread/unsubscribe` on shutdown. Two things changed since
it was filed. Spec #922 made the provider **creation-only**, so no operation
depends on a thread being cold — the unload is hygiene, not correctness. And the
issue's wording ("normal exit and `/restart`") assumed an exit hook that does not
exist: `cola stop` and Ctrl-C terminate the process on the default signal action,
with no user code running, and `/restart`/`/update` call `std::process::exit`
from the command handler.

There is also a transport constraint. `RpcConnection::send` only enqueues a frame
on the writer task; `std::process::exit` right after would discard it. So the
release must be awaited at least until the daemon acknowledges it — "send and
forget" is not available without a synchronous socket write, which would bypass
the connection's id bookkeeping.

## Decision

1. **The capability is the client seam's, not Codex's.**
   `ServerLifecycle::detach(budget)` is a new method with a default no-op body:
   a client whose runtime has no such concept (OpenCode) keeps the default, and
   the default is the explicit "nothing to give back", never a failed release.
   The method hands back what the client holds; it never stops, restarts or
   mutates the runtime or its sessions.
2. **Every exit this process can run code on calls it.** `/restart` (both the
   supervisor and the re-exec exit) and `/update` call it after the user reply
   is sent and before the exit; on the re-exec path, before the replacement is
   spawned, so a new connection cannot re-adopt a thread and keep it loaded
   while the old one is still letting go. A **stop request** — `SIGTERM`
   (systemd, `cola stop`) or `SIGINT` (Ctrl-C), Ctrl-C elsewhere — is caught by
   the run loop's signal future, which runs the same pass before returning. That
   is the graceful-shutdown capability issue #918 assumed: without it, the
   default signal action ends the process with no code running.
3. **Best-effort and bounded.** The contract is: return within `budget`
   (`DETACH_BUDGET`, 300 ms), log what was managed, never fail or panic. The
   caller wraps the call in a backstop of the budget plus a 50 ms margin: a
   client that ignores the contract still cannot hold an exit, while a
   slow-but-finishing pass still gets to write its own summary line.
4. **Codex releases every held thread once.** `Projections` knows the threads
   with a live release channel; the pass sends each through the thread's
   subscriber task (the only holder of its `ThreadSubscription`, so the
   "last local handle" rule still decides whether an unsubscribe goes out),
   concurrently, under one **total** deadline (not one per thread) — a wedged
   daemon costs the budget once, not once per thread. Successes are debug lines,
   a failure names its thread id, and one info line states
   requested/released/failed/unfinished.
5. **The limit is documented, not papered over.** A release only starts the
   daemon's grace window earlier; it cannot remove it, and a thread another
   client still holds is not unloaded at all. The guarantee for a cold thread
   would have been `thread/fork` (issue #919), which #922 also retired.

## Consequences

- The normal paths — prompt, turn, `/model`, switching, reads — send no new
  request; `detach` is reachable only from the two exit call sites.
- A restart costs at most the budget when the daemon is wedged, and a few
  milliseconds otherwise (a local round trip per held thread).
- OpenCode and every mock keep the default; nothing else in the bridge branches
  on a client kind to use this.
- If a future runtime needs to release something on exit (a lease, a cursor), it
  overrides `detach` and gets the same bounded, logged, non-failing treatment.
- `cola stop` and Ctrl-C are no longer immediate: they cost at most the budget
  (plus the backstop margin) before the process ends. The signal handler only
  exists while the run loop is awaited; a signal during startup still takes the
  default action, which is acceptable because nothing is held yet.
