# Runtime reconciliation for background-task liveness

## Context

ADR-0059/0060 derive a **Background Task**'s liveness from durable transcript
facts: the completed `shell`/`subagent` tool part whose metadata still says the
run is `running`, minus every Wake that retired it. The completion record is
the Wake — OpenCode 2.0.x writes one whenever a background job settles, and its
restart sweep writes one for every persisted background job — so the happy path
is exact, and no poll of the runtime is needed while the queue is healthy.

The derivation has a failure mode (issue #454, reproduced 2026-09-30 on 2.0.18):
the process that hosted a background shell — or the layer that reported its
completion — dies without the Wake ever being written. The tool part's
`metadata.status` never flips (by design it is a launch record, not a status),
no Wake arrives, and every reader of the transcript reads the task as live
forever: the card sits on 「⏳ 等待后台任务」, a cola restart re-reads the same
transcript, and the official 2.0.x app shares the derivation, so it shows the
same phantom. In the observed failure the runtime had already forgotten the
shell entirely: `GET /api/shell/{id}` answered 404.

The server's runtime registries do know the truth, and only the *liveness* part
is missing from the transcript: `GET /api/shell` lists the location's currently
running shells (and `GET /api/shell/{id}` a retained terminal one — status,
exit and completion time), while `GET /api/session/active` reports the live
session drains a subagent child runs under. These are the same handles the
metadata's ids already name; none is a purpose-built "background task status"
endpoint (measured against the current release, 2.0.20 — no job list/get route
exists). They are process-local: a restarted server answers `Missing` for
shells it never hosted and reports nobody active, so they can corroborate the
transcript but never replace it.

## Decision

**Reconcile the ledger against the runtime on the reads that already drive it,
with positive evidence only.**

- The Session Sync pass (~8 s), on the same transcript read that refreshes a
  yielded card's ledger and decides Wakes, asks the backend for a
  `TaskRuntime` verdict over the live tasks' ids. Only when the transcript
  still lists live tasks. Shells cost one
  location-scoped list read plus one read per shell the list does not carry;
  children ride the active map. A failed read yields **no verdict** — the task
  stays exactly as the transcript read it, never guessed.
- **Ended**: the runtime reports a terminal status (`exited` / `timeout` /
  `killed`), or has no record at all (404). The task leaves the live list as a
  `TaskRetirement`, so the settle decision sees it ended, and the card records
  a runtime completion entry — 「🔔 后台任务结束」, or 「🔔 后台任务已失联」 for
  the gone record — with the runtime's own completion time when it has one,
  never an invented clock. The last retirement ends the wait per ADR-0060's
  quiet true end (✅ in place, no new card).
- **A 404 is an ending, not a maybe** (the one deliberate departure from the
  design comment on #454, which first proposed 待确认): the runtime that hosts
  the task says it has no such shell, and a shell cannot be running under a
  server that does not know it. The restart-recovery race this could lose (a
  new server whose sweep has not yet written its cancellation Wake) still ends
  the wait honestly; the late Wake renders its own continuation as before.
- **Inactive**: a child session absent from the active map stays **live** but
  its row carries the marker `⚠️ 状态待确认`, and the title counts it
  (`⏳ 后台任务（2 · 1 待确认）`). Absence from a process-local map cannot
  distinguish "it finished an instant ago" from "a different server hosted
  it", so only a Wake (or a shell's positive verdict) retires a task.
- **The entry names the ending it saw**: a Wake's own reported state
  (完成 / 已取消 / 失败), a runtime-confirmed end (结束), a lost record
  (已失联). 完成 is never stamped on work that failed, was cancelled, or was
  lost. A lost entry carries identity only — there is nothing to clock.
- V1 carries no Background Tasks: its strategy answers an empty read without a
  request, and nothing changes there.
- The **cleanup action** (dismiss / ignore a stranded task from the card) is
  deliberately deferred. This change first makes the state honest and ends the
  wait in the observed failure mode; a button needs its own dismissal
  persistence and interaction design.

## Considered options

- **Transcript only — accept the phantom.** Rejected: issue #454 is exactly
  this, and the card cannot tell "really running" from "no completion record
  will ever come". A restart never clears it.
- **Poll the runtime registries on a new, faster cadence.** Rejected: the
  registries move no faster than the completion record in the happy path, and
  ADR-0060's existing reads already give the ledger its clock. The reconcile
  rides them.
- **Trust the transcript for liveness and the runtime only for display.**
  Rejected: display is the whole problem; a row that still claims liveness
  while the runtime says otherwise is the lie #454 reports.
- **Retire on every runtime absence** (including subagents and listed-only
  shells). Rejected: the maps are process-local, so absence is also what a
  server replacement looks like; only a positive verdict may end a task.
- **Fix it upstream only** (write a tombstone at process exit, or expose a job
  list). Desirable (see Consequences) but not sufficient: the observed failure
  had no reporter left to write anything, and cola must not wait for a server
  release to stop lying on its own card.
- **Ship the cleanup button first.** Rejected for this cut: it makes the user
  do the reconciling by hand on every stale row, and the runtime read answers
  most cases without one.

## Consequences

- The ledger's copy gains three endings (结束 / 已失联 / 已取消 / 失败) and one
  marker (状态待确认); CONTEXT.md's **Background Task**,
  **Background Task Ledger** and **Waiting on Background Work** entries record
  them.
- The reconcile is one extra backend read only while a transcript lists live
  tasks, sharing the pass's request bound; a failure leaves every path exactly
  as it was, so a flaky runtime read can never end a wait.
- The runtime read is location-scoped: a session that moved directories after
  launching a shell asks the new location and may see `Missing` for a live
  task. The move flow (#433) is rare mid-task and the loss is a false 失联, not
  a silently stuck card; a future cut can carry the launch directory.
- The upstream fix remains desirable: a runtime that reconciles its own
  transcript markers (or a job list endpoint) would make this a fallback
  rather than the cure. #454 keeps that half open.
- Tests pin: the verdict decode (running list, per-shell terminal/404, active
  map, unknown statuses), the neutral reconciliation (ended/missing/running/
  inactive/no-verdict/empty), the marker and ending copy, and the yielded
  card's runtime settle — in place, one PATCH, no continuation.

Related: #454, ADR-0059, ADR-0060, ADR-0063.
