# Background jobs are a second source of Background Tasks, discovered on the transcript read

> **Amends ADR-0060, ADR-0080 and ADR-0111**: the ledger's rows gain
> job-sourced Background Tasks (the job's kind as the type word, the job's own
> label, `↳` for a child job); and a job-sourced task's liveness is the V2
> job-list read, so its list-ended retirement is a read verdict rather than a
> durable evidence ending and owes no entry; ADR-0060's freeze carve-out also
> admits a job notice — an `Other`-source synthetic rendered in place as late
> content, with no 承接 line. The rest of each decision is unchanged.

## Context

OpenCode 2.4 unified background work as **jobs**: `shell`, `subagent`,
`codemode`, `image` and `video`, listed by
`GET /api/experimental/job?sessionID=…&status=running&descendants=true` and
announced by `experimental.job.updated`. cola derived its **Background Task**
ledger only from an assistant tool part whose call completed while its
`metadata.status` still said `running`, and only for `shell` / `subagent`
(`ToolCall::background_launch`, `src/opencode/v2/wire.rs`). So a Code Mode
script (`execute` with `background: true`), image generation and video
generation were invisible while they ran: the card showed no live row until the
synthetic `<background-result>` notice arrived (issue #1216). The same gap
covered a shell or subagent moved to the background by
`POST /api/session/:id/background` when its tool part carried no
`metadata.status = "running"` marker.

The obvious alternative — teaching the shared **Runtime Reconciliation**
(`src/bridge/runtime.rs`) to list jobs — does not fit: the reconcile spends
nothing unless the read already lists a live task, and job-only work is exactly
what the read does not list; its 30 s throttle would also let a freshly started
job stay invisible for half a minute. cola polls at `render_poll_ms` (1.5 s), so
the transcript read is the cadence a live row must follow. The job list is a
small read that returns durable rows (a backgrounded job persists until its
notice is consumed), so a successful read is also a verdict: "not reported
running" means the job is over.

## Decision

**A running job is a Background Task, discovered on the transcript read.**

- `V2Strategy::transcript` reads the running job list after decoding the
  messages and applying the live-progress overlay, and merges each running job
  into `SessionTranscript::background_tasks` unless a tool part already names
  that run. One request per transcript read; no separate throttle or cache of
  the whole list.
- Dedupe is on identity, not order: a job is skipped when its `id` matches a
  task's call id or job id, its `shellID` a task's shell id, or its
  `childSessionID` a task's child id. The tool-part derivation wins (it carries
  the launch's own call and label), and the job fills the rows the tool part
  cannot name. All five kinds are merged, so the `POST /background` case above
  is covered too.
- A job-sourced task carries the job id as its call identity, plus its own
  neutral `label` — a `codemode` task's first non-empty script line, falling
  back to the job's `title` and then its id; a `shell` job's `command`; a
  `subagent` job's `description`; an `image` or `video` job's `path` — and, for
  a **Child Job**, the owning subagent's label. `TaskKind` gains `CodeMode`,
  `Image` and `Video`; the ledger's type word is the job's own kind
  (`codemode` / `image` / `video`) and its row shows the start clock and total
  elapsed. Child jobs render `↳ <subagent label>` (or `↳ 子会话`) so they are
  never mistaken for the parent's own work; they stay flat because the ledger
  has no per-subagent nesting. An image job's `model` stays out of the row: the
  completion notice names it, and the row keeps the path it saved to.
- The transcript-derived rows keep their existing order; job rows follow them,
  sorted by `time.started` then id so they never trade places between reads.
- Lifecycle: a job-sourced row is retired by its completion **Wake** (the job
  id already matches `Wake::retires` through the call id) or by the first
  success read whose job list no longer reports it running. Only the Wake
  produces a completion entry; the list-ended case retires silently, because a
  normal completion always writes the synthetic notice first, and that notice
  is the record. That list-ended retirement is a **read verdict, not a durable
  Task Ending** (the ADR-0080 / ADR-0111 amendment above): a job-sourced row's
  liveness is the list itself, so there is no ending to record and no entry to
  owe; its Wake, when one arrives, still records the ending exactly as any
  other. Job rows count toward the Turn wait like any Background Task,
  and are excluded from the childless-task Cleanup marking
  (`needs_cleanup_marking` / `mark_cleanup_exits`): they have a job identity,
  not an unresolvable one.
- Failure contract (`src/opencode/v2/mod.rs`, one debug line): a 404/405 means
  the server has no job surface, so the read carries no jobs and blocks
  nothing. A transport error, timeout or 5xx keeps the last successfully read
  job rows, unchanged and unmarked, so the card neither flickers nor settles a
  live job; a later successful read corroborates or retires them. A failure
  with no previous rows invents nothing and blocks nothing. The transcript read
  itself never fails on the job read (AC4).
- A job completion generates a `🔔 codemode/image/video 完成` entry by joining
  the Wake to the launching tool part's `metadata.jobID` for the label and
  start. Its notice is `WakeSource::Other`, so it is **not** a genuine
  resumption: an open yielded card renders it in place as ordinary late
  content, with no 承接 line, exactly as today.

## Considered options

- **Runtime Reconciliation owns the job read.** Rejected: the "no live task,
  no request" gate is load-bearing (an idle Session spends nothing) and
  job-only work never satisfies it; widening the gate would poll every idle
  Session, and the 30 s throttle is the wrong latency for a live row.
- **Subscribe to `experimental.job.updated`.** Rejected for now: cola has no
  experimental event channel, the render already polls, and a durable list read
  is the same source of truth. It can be added later without changing the read
  model.
- **A separate "jobs" ledger section.** Rejected: users already read the
  `⏳ 后台任务` panel as "what is still running"; a second section would split
  the same question and the same Turn wait.
- **Retire a list-ended job with an `结束` entry.** Deferred: it needs a new
  retire cause and must suppress the Wake entry when the Wake lands later
  (#659). The synthetic notice already covers every normal path, and the
  issue's AC3 explicitly allows a silent retirement.
- **Reuse `⚠️ 状态待确认` for a failed job read.** Rejected: the marker drives
  the `清理待确认任务` button, so a transient list failure would invite the user
  to end a job that is still running (the hazard #1178 tracks). A failed read
  keeps the rows without the marker instead.

## Consequences

- One extra small GET per transcript read on V2 (the same 1.5 s cadence the
  ledger already runs at); a failure costs a debug line, never a card.
- The adapter keeps a process-local, per-session copy of the last successful
  job rows so a failed read can re-attach them. A session that fails forever
  keeps its last rows until a successful read or the process exits; the cache
  is per attach and never durable.
- A job row can outlive the subagent row that started it (the child's job is
  still running after the subagent retired); it falls back to `↳ 子会话`.
- Older V2 servers without `experimental/job` degrade silently to the tool-part
  derivation; no capability axis changes.
- The job notice for image/video/codemode still has no `<background-result>`
  label parsing (out of scope in #1216); the new completion entry's label comes
  from the launching tool part, not the notice.
