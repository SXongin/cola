# Singleton lock & restart: functionally-alive takeover, supervisor-first

The singleton lock must survive a `/restart`'s spawn-and-exit handover. We decided: a lock owner counts as alive only while **Functionally Alive** — running AND with its identity (cmdline/exe) still readable. A process mid-`exit()` (the kernel has torn down its memory map but not yet marked it a zombie) is stale and reclaimable, so a restart child never bails "not a cola process" against its dying predecessor — the exit-window bug, which hit twice (the zombie state first, then mid-`exit()`).

## Decision

- **Alive = Functionally Alive.** `owner_functionally_alive(pid)` means the process is non-zombie AND its cmdline/exe are still readable. Used at every point the lock logic inspects an owner PID: `stale_lock_owner`, the `AlreadyExists` branch of `acquire_at`, and `replace_instance`'s guard. A mid-`exit()` owner is stale (it can no longer process events) even though `/proc/<pid>/status` reads non-zombie — in the kernel, `exit_mm()` tears down the memory map *before* the task becomes `TASK_DEAD`.
- **Supervisor-first restart.** When a systemd unit owns cola itself — `INVOCATION_ID` set AND `SYSTEMD_EXEC_PID` equals cola's own PID — both `/restart` and `/update` exit with `EXIT_SUPERVISOR_RESTART` (3) and let the unit's `Restart=on-failure` bring cola back from the same ExecStart; spawning a child is forbidden because the unit's default `KillMode=control-group` would kill it (ADR-0015). Without a supervisor, restart re-execs (`restart_process()` + `--replace`), and the Functionally Alive predicate makes the lock takeover deterministic rather than timing-dependent. The `SYSTEMD_EXEC_PID` half matters: `INVOCATION_ID` alone leaks into every child of a unit, so a terminal under a unit would hand the inherited var to a cola running as a plain foreground process, which then exited 3 expecting a supervisor that did not own it — nothing restarted it until a manual start.
- **The PID-reuse guard survives.** `replace_instance` still refuses to kill a genuinely alive, identity-readable process that is not cola — that is the real PID-reuse danger. Mid-`exit()` is the one state it may skip, because the process is dying regardless.

## Considered Options

- **Explicit lock handover** (the parent removes the lock file before spawning): removes the race entirely, but leaves a spawn-failure window where the parent keeps running lockless, and overlaps with the predicate fix. Rejected for this fix; noted as future cleanup.
- **Big refactor** (dedicated restart module, injected process queries): the defect was a wrong definition of "alive", not wrong structure; a refactor would move the decision without making it more correct. Deferred.

## Consequences

- A `/restart` child may reclaim a lock from a parent that is still nominally alive (mid-`exit()`) — it does not wait for the parent to finish dying.
- `replace_instance`'s "not a cola process" refusal is now only evaluated for owners that are verifiably functional, so it stays sound.
- launchd KeepAlive and no-supervisor restarts keep the spawn-and-exit behavior; the predicate makes the race resolve deterministically.

## Amendment (2026-11-19, #1081): the non-supervised successor leads its own process group

Issue #1081 showed the handover above is necessary but not sufficient: started
from a foreground terminal, a `/restart` successor reached the lock takeover
(`WARN … replacing running cola instance`) and then vanished before `App::run`,
leaving the dead PID in `cola.lock` and `restart-notify.json` unconsumed. The
successor was spawned into the predecessor's **process group**; once the
foreground job ended, whatever tears that job's group down signalled the
successor too. Nothing supervised it, so nothing brought it back.

- **The successor leaves the predecessor's process group.** The spawn shape is
  now `restart_spawn_command` (`src/bridge/command.rs`), used by both the
  Feishu `/restart` and `/update`'s in-place restart, and it sets
  `CommandExt::process_group(0)` on Unix. This is ADR-0085's detached-service
  reasoning applied to the restart handover: leaving the foreground group is
  what a terminal or job runner's teardown no longer reaches. No `setsid` is
  needed — the successor never reads the terminal, and a hangup is delivered to
  the session leader and the foreground group, not to a background group.
- **Inherited stdio stays.** Unlike the detached service, the successor keeps
  `stdin`/`stdout`/`stderr`, because that is the documented behaviour: a
  restart preserves the operator's shell redirect (`./cola > test.log`). On
  Windows this rules out `DETACHED_PROCESS` (it takes the console from a child
  that keeps the inherited handles); the spawn sets only
  `CREATE_NEW_PROCESS_GROUP`, which keeps Ctrl-C out of the successor. The
  supervisor path is unchanged: a unit-owned process still exits with the
  restart code, and `restart_process` is never called.
- **Coverage.** The spawn shape is no longer only reachable by re-execing the
  real binary: a unit test asserts the successor leads its own process group
  and carries `--replace`, and an end-to-end test re-executes the test binary
  as a group-leading predecessor, has it spawn a successor through the real
  shape, then signals the old group and asserts the successor is still alive
  and still writing to the predecessor's redirected stdout.