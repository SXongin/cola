# Singleton lock & restart: functionally-alive takeover, supervisor-first

The singleton lock must survive a `/restart`'s spawn-and-exit handover. We decided: a lock owner counts as alive only while **Functionally Alive** — running AND with its identity (cmdline/exe) still readable. A process mid-`exit()` (the kernel has torn down its memory map but not yet marked it a zombie) is stale and reclaimable, so a restart child never bails "not a cola process" against its dying predecessor — the exit-window bug, which hit twice (the zombie state first, then mid-`exit()`).

## Decision

- **Alive = Functionally Alive.** `owner_functionally_alive(pid)` means the process is non-zombie AND its cmdline/exe are still readable. Used at every point the lock logic inspects an owner PID: `stale_lock_owner`, the `AlreadyExists` branch of `acquire_at`, and `replace_instance`'s guard. A mid-`exit()` owner is stale (it can no longer process events) even though `/proc/<pid>/status` reads non-zombie — in the kernel, `exit_mm()` tears down the memory map *before* the task becomes `TASK_DEAD`.
- **Supervisor-first restart.** Under a systemd unit (`INVOCATION_ID` set), both `/restart` and `/update` exit with `EXIT_SUPERVISOR_RESTART` (3) and let the unit's `Restart=on-failure` bring cola back from the same ExecStart; spawning a child is forbidden because the unit's default `KillMode=control-group` would kill it (ADR-0015). Without a supervisor, restart re-execs (`restart_process()` + `--replace`), and the Functionally Alive predicate makes the lock takeover deterministic rather than timing-dependent.
- **The PID-reuse guard survives.** `replace_instance` still refuses to kill a genuinely alive, identity-readable process that is not cola — that is the real PID-reuse danger. Mid-`exit()` is the one state it may skip, because the process is dying regardless.

## Considered Options

- **Explicit lock handover** (the parent removes the lock file before spawning): removes the race entirely, but leaves a spawn-failure window where the parent keeps running lockless, and overlaps with the predicate fix. Rejected for this fix; noted as future cleanup.
- **Big refactor** (dedicated restart module, injected process queries): the defect was a wrong definition of "alive", not wrong structure; a refactor would move the decision without making it more correct. Deferred.

## Consequences

- A `/restart` child may reclaim a lock from a parent that is still nominally alive (mid-`exit()`) — it does not wait for the parent to finish dying.
- `replace_instance`'s "not a cola process" refusal is now only evaluated for owners that are verifiably functional, so it stays sound.
- launchd KeepAlive and no-supervisor restarts keep the spawn-and-exit behavior; the predicate makes the race resolve deterministically.