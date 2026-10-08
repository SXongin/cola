//! The shared Background Task runtime reconciliation (#589, issue #454):
//! one positive-evidence runtime read per cycle, spent on the transcript read
//! each caller already made, and one process-wide throttle all callers share.
//! A subagent the runtime leaves unconfirmed gets the second, lighter read
//! (#591, issue #464): the child session's own newest assistant message.
//!
//! The reconcile used to be a private step of the external flow, reachable
//! only from the Session Sync pass — which skips an inflight Session — so a
//! Background Task that died mid-turn kept its ledger row, forced an
//! intermediate 「⏳ 等待后台任务」 yield, and only retired on a later pass.
//! It is now one step with three callers on the read each already performs:
//! Session Sync, the Turn's drain tick and the out-of-turn follow/settle loop
//! ([`RuntimeReconcile::observe`], throttled), plus the cleanup click, which
//! calls the same step's non-throttled form ([`reconcile_now`]) directly: a
//! click must never no-op behind a recent poll's verdict. The live render path
//! renders the retirement entries the read carries, so nothing is swallowed by
//! the process-local retirement overlay (ADR-0065).
//!
//! Guard rails, all shared:
//!
//! - a read that lists no live Background Task spends nothing;
//! - one verdict per Session per [`RuntimeReconcile::interval_ms`], across
//!   every throttled caller — the reconcile's cost has a bound independent of
//!   the poll cadences that observe it;
//! - a failed or timed-out read leaves the transcript exactly as read: no
//!   verdict, no retirement, no settle (a flaky runtime can never end a wait);
//! - the child-evidence step spends at most one light read per suspect per
//!   cycle: a terminal step finish with its completion stamp retires the task
//!   as ended, a child session the server no longer knows (404) retires it as
//!   lost, and an unreadable or non-terminal child keeps its row unconfirmed —
//!   nothing is guessed;
//! - the whole admitted cycle — the runtime verdict read, the child-evidence
//!   reads and the shell-window reads — is bounded by ONE read budget (spec
//!   #588, review PR #595): each later read gets only the remainder and is
//!   skipped when the budget is spent (no request — the next cycle retries),
//!   so a stalled runtime or capture can never stack past one read timeout.
//!
//! The same admitted cycle carries the live shells' output windows (spec #588,
//! ticket #592): one tail read per live shell, display-only, so the ledger's
//! window refreshes at the shared cadence and a failed or empty read simply
//! omits it.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use crate::backend::{Backend, SessionTranscript, ShellOutputRead};

/// The default spacing between two runtime verdicts of one Session (ms): the
/// shared cadence Session Sync and the live loops observe together.
const RUNTIME_RECONCILE_INTERVAL_MS: u64 = 30_000;

/// One runtime verdict per Session per interval, process-wide (#589): the one
/// gate Session Sync, the Turn's drain and the follow/settle loop share, so
/// the reconcile's request cost cannot scale with how many loops happen to be
/// polling the same Session.
pub struct RuntimeReconcile {
    /// The minimum spacing between two runtime reads of one Session (ms).
    /// Injectable beside the other timing atomics (tests store a tiny value so
    /// every tick reconciles, or a huge one to pin the bound).
    pub interval_ms: AtomicU64,
    /// session_id → the last attempt's instant. Every attempt counts, success
    /// or not: a runtime that keeps failing must not be hammered at the poll
    /// cadence, and the next tick after the interval retries it.
    last_attempt: std::sync::Mutex<HashMap<String, std::time::Instant>>,
}

impl Default for RuntimeReconcile {
    fn default() -> Self {
        Self::new()
    }
}

impl RuntimeReconcile {
    pub(crate) fn new() -> Self {
        Self {
            interval_ms: AtomicU64::new(RUNTIME_RECONCILE_INTERVAL_MS),
            last_attempt: std::sync::Mutex::new(HashMap::new()),
        }
    }

    /// Whether this Session may spend a runtime read now. Admits the attempt
    /// and records it; the throttle is process-wide and per Session, so two
    /// loops observing the same Session together share one verdict.
    fn begin(&self, session_id: &str) -> bool {
        let interval = std::time::Duration::from_millis(self.interval_ms.load(Ordering::Relaxed));
        let now = std::time::Instant::now();
        let mut last = self
            .last_attempt
            .lock()
            .expect("the runtime-reconcile lock is never poisoned");
        match last.get(session_id) {
            Some(previous) if now.duration_since(*previous) < interval => false,
            _ => {
                last.insert(session_id.to_string(), now);
                true
            }
        }
    }

    /// The shared, throttled reconcile step (issue #454): admit at most one
    /// runtime verdict per Session per [`Self::interval_ms`] across every
    /// caller, compose [`reconcile_now`] on the caller's own transcript read,
    /// then fill the same admitted cycle's shell output windows (spec #588,
    /// #592) — one tail read per still-live shell, so the ledger's window
    /// refreshes at this shared cadence and never at the caller's poll
    /// cadence. A window read is display-only — it never prompts, retires or
    /// settles — and a failed or vanished record records
    /// [`ShellOutputRead::Unavailable`], which the row renders as no window at
    /// all (never a placeholder); a successful empty capture is a
    /// readable-empty window the row omits the same way (spec #588, review).
    ///
    /// Zero requests when the read lists no live task: the empty check runs
    /// BEFORE the throttle, so an idle Session never consumes an attempt. The
    /// throttle consumes the attempt even when the read fails or times out,
    /// and such a read changes nothing (the composed pass leaves the
    /// transcript exactly as read).
    ///
    /// The admitted cycle spends ONE read budget (spec #588, review PR #595):
    /// the budget starts here, after the throttle admitted the attempt, and
    /// the verdict read, the child-evidence reads and the window reads all
    /// draw from it. Each later read gets only the remainder; a read the
    /// spent budget cannot fund is skipped — no request — and the next cycle
    /// retries it.
    ///
    /// The step is deliberately gate-free: each caller decides whether it can
    /// render what it observes (Session Sync's waiting-card admission, the live
    /// loops' own anchor) before calling.
    pub(crate) async fn observe(
        &self,
        backend: &Arc<dyn Backend>,
        session_id: &str,
        directory: &str,
        transcript: &mut SessionTranscript,
        read_timeout_ms: u64,
    ) -> bool {
        let (shells, children) = live_task_ids(transcript);
        if shells.is_empty() && children.is_empty() {
            return false;
        }
        if !self.begin(session_id) {
            return false;
        }
        let budget = CycleBudget::within(read_timeout_ms);
        let changed = reconcile_now(backend, session_id, Some(directory), transcript, budget)
            .await
            .unwrap_or(false);
        // The windows are read after the verdicts: a shell the runtime just
        // retired leaves the live list here and spends no tail read. A failed
        // runtime read changes nothing about them — the transcript still lists
        // the shells, and the window is display-only.
        capture_shell_outputs(backend, session_id, directory, transcript, budget).await;
        changed
    }
}

/// One admitted reconcile cycle's read budget (spec #588, review PR #595): the
/// runtime verdict read, the child-evidence reads and the shell-window reads
/// all spend ONE `read_timeout_ms` window. Each read is bounded by what remains
/// of it; a read the spent budget cannot fund is skipped entirely — no request
/// — so a stalled runtime or capture can never stack past one read timeout.
/// Skipped and timed-out reads are the same to their callers: nothing is
/// decided, and the next cycle retries.
#[derive(Clone, Copy)]
pub(crate) struct CycleBudget {
    deadline: tokio::time::Instant,
}

impl CycleBudget {
    /// Start the budget now: `now + read_timeout_ms`, the same per-read bound
    /// the cycle's own transcript read was given. [`RuntimeReconcile::observe`]
    /// starts one for the whole admitted cycle; the cleanup click starts its
    /// own for its direct [`reconcile_now`], so neither can stack per-read
    /// timeouts.
    pub(crate) fn within(read_timeout_ms: u64) -> Self {
        Self {
            deadline: tokio::time::Instant::now() + std::time::Duration::from_millis(read_timeout_ms),
        }
    }

    /// Bound one cycle read by what remains of the budget. `None` when the
    /// budget is spent — the read is never issued — or when the read timed
    /// out; either way the caller changes nothing and the next cycle retries.
    async fn read<T>(
        &self,
        what: &str,
        fut: impl std::future::Future<Output = crate::error::Result<T>>,
    ) -> Option<crate::error::Result<T>> {
        let remaining = self
            .deadline
            .saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            tracing::debug!("{what} skipped: the reconcile cycle's read budget is spent");
            return None;
        }
        crate::bridge::bounded_call(what, remaining.as_millis().max(1) as u64, fut).await
    }
}

/// One NON-throttled reconcile pass over a read's still-live Background Tasks
/// (issue #454): the shell and child ids the read names, one bounded runtime
/// read, the positive-evidence verdicts applied to the transcript, at most one
/// child-evidence read (#591) per subagent the verdict left unconfirmed, and
/// the process-local overlay recording of what was retired. Zero requests when
/// the read lists no live task; a failed or timed-out read leaves the
/// transcript exactly as read — no verdict, no retirement, no settle — so a
/// flaky runtime can never end a wait.
///
/// [`RuntimeReconcile::observe`] composes this under its attempt-stamped 30 s
/// throttle, which is how the poll paths (Session Sync, the drain, the
/// follow/settle loop) share one verdict; the cleanup click calls it directly
/// (spec #588, #590), because a click must never no-op behind a recent poll's
/// verdict — it spends its own runtime read on the transcript it just read.
/// Either way the overlay recording stays this one path, so every later read
/// of the Session agrees on what retired.
///
/// The caller hands in the cycle's [`CycleBudget`] (spec #588, review PR
/// #595): `observe` starts one for the whole admitted cycle (this pass plus
/// the shell-window reads), the cleanup click starts its own (`now + its read
/// timeout`). Every read here is bounded by what remains of it, and a spent
/// budget skips the read — no request — so no caller can stack one timeout
/// per suspect.
///
/// Returns whether an applied verdict changed the read — `None` when no
/// verdict was applied at all: the read lists no live task (nothing was asked),
/// or the runtime read failed or timed out (the transcript stays exactly as
/// read). A caller about to skip its render flattens with `unwrap_or(false)`,
/// so a retirement is never dropped on the floor; the cleanup click keeps the
/// two apart, because only a re-derived set may clear rows (ADR-0073).
pub(crate) async fn reconcile_now(
    backend: &Arc<dyn Backend>,
    session_id: &str,
    directory: Option<&str>,
    transcript: &mut SessionTranscript,
    budget: CycleBudget,
) -> Option<bool> {
    let (shells, children) = live_task_ids(transcript);
    if shells.is_empty() && children.is_empty() {
        return None;
    }
    match budget
        .read(
            "task runtime read",
            backend.task_runtime(session_id, directory, &shells, &children),
        )
        .await
    {
        Some(Ok(runtime)) => {
            let retired_before = transcript.task_retirements.len();
            // The read carries the previous reconciles' markers (the adapter's
            // overlay re-applied them), so "changed" is measured against them:
            // a marker the verdict resolves is a change too.
            let unconfirmed_before = transcript.unconfirmed_tasks.clone();
            transcript.apply_task_runtime(&runtime);
            // The child-evidence step (#591, issue #464): a subagent the
            // runtime no longer reports active can still be retirable on its
            // own transcript's evidence — and the very failure mode this
            // family exists for is the missing Wake. At most ONE light read
            // per suspect per pass: a terminal step finish with its completion
            // stamp retires the task, a gone child session retires it as lost,
            // and a read that cannot conclude — or fails, or times out, or is
            // skipped by the spent cycle budget — keeps the row exactly as the
            // runtime marked it, never guessed.
            for (call_id, child_id) in transcript.unconfirmed_children() {
                match budget
                    .read("child evidence read", backend.child_evidence(&child_id))
                    .await
                {
                    Some(Ok(evidence)) => transcript.apply_child_evidence(&call_id, evidence),
                    Some(Err(error)) => {
                        tracing::debug!(
                            "session {session_id} child {child_id} evidence read failed: {error}; keeping its row"
                        );
                    }
                    // A read that timed out — or that the spent budget skipped
                    // — yields no verdict, like the runtime read above.
                    None => {}
                }
            }
            // Record the retirements so every later transcript read — the
            // live render, the drain's settle, the follow, the reap — sees
            // them gone: the launch record never flips (issue #454), so
            // without the overlay the next read would resurrect the task.
            if !transcript.task_retirements.is_empty() {
                let call_ids: Vec<String> = transcript
                    .task_retirements
                    .iter()
                    .map(|retirement| retirement.task.tool.call_id.clone())
                    .collect();
                tracing::info!(
                    "session {session_id}: runtime reconciliation retired {} background task(s)",
                    call_ids.len()
                );
                backend.retire_background_tasks(session_id, &call_ids);
            }
            // The unconfirmed markers are process-local state too (review, spec
            // #588): the read is fresh, and the ledger derives 状态待确认 from
            // it, so a read the shared throttle does not spend a verdict on
            // would drop the marker (and the cleanup button it gates) without
            // any evidence the child is running. Record this verdict's
            // post-evidence set — carried markers included, resolved ones
            // absent — so the adapter re-applies it to every later read until
            // the next verdict. The one writer; a failed or timed-out read
            // never reaches here, so it clears nothing.
            let unconfirmed: Vec<String> = transcript.unconfirmed_tasks.iter().cloned().collect();
            backend.set_unconfirmed_tasks(session_id, &unconfirmed);
            Some(
                transcript.task_retirements.len() > retired_before
                    || transcript.unconfirmed_tasks != unconfirmed_before,
            )
        }
        Some(Err(error)) => {
            tracing::debug!(
                "session {session_id} task runtime read failed: {error}; waiting for the next read"
            );
            None
        }
        None => None,
    }
}

/// The shell and child session ids a read's still-live Background Tasks name,
/// in the read's own order — the reconcile's ask list. Empty when the read
/// lists no live task, which is what makes the reconcile spend nothing (and,
/// under [`RuntimeReconcile::observe`], not even consume a throttle attempt).
fn live_task_ids(transcript: &SessionTranscript) -> (Vec<String>, Vec<String>) {
    let shells = transcript
        .background_tasks
        .iter()
        .filter_map(|task| task.shell_id.clone())
        .collect();
    let children = transcript
        .background_tasks
        .iter()
        .filter_map(|task| task.child_id.clone())
        .collect();
    (shells, children)
}

/// Fill one read's shell output windows (spec #588, #592): one tail read per
/// still-live shell, deduped, each bounded by the cycle's remaining
/// [`CycleBudget`] (spec #588, review PR #595). A shell whose read fails,
/// vanishes, or is skipped by the spent budget records
/// [`ShellOutputRead::Unavailable`] — the row omits the window — and a
/// successful empty capture records an empty window, which the row omits the
/// same way; only the completion entry's own read keeps 「输出已不可用」 to the
/// unavailable case (spec #588, review). A shell is never asked twice in one
/// cycle.
async fn capture_shell_outputs(
    backend: &Arc<dyn Backend>,
    session_id: &str,
    directory: &str,
    transcript: &mut SessionTranscript,
    budget: CycleBudget,
) {
    let mut seen = std::collections::HashSet::new();
    let shells: Vec<String> = transcript
        .background_tasks
        .iter()
        .filter_map(|task| task.shell_id.clone())
        .filter(|shell_id| seen.insert(shell_id.clone()))
        .collect();
    for shell_id in shells {
        let output = match budget
            .read(
                "shell output read",
                backend.shell_output(&shell_id, Some(directory)),
            )
            .await
        {
            Some(Ok(Some(window))) => ShellOutputRead::Window(window),
            Some(Ok(None)) => ShellOutputRead::Unavailable,
            Some(Err(error)) => {
                tracing::debug!(
                    "session {session_id} shell output read for {shell_id} failed: {error}; omitting its window"
                );
                ShellOutputRead::Unavailable
            }
            None => ShellOutputRead::Unavailable,
        };
        transcript.shell_outputs.insert(shell_id, output);
    }
}
