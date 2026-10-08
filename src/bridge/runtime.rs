//! The shared Background Task runtime reconciliation (#589, issue #454):
//! one positive-evidence runtime read, spent on the transcript read each
//! caller already made, and one process-wide throttle all callers share.
//!
//! The reconcile used to be a private step of the external flow, reachable
//! only from the Session Sync pass — which skips an inflight Session — so a
//! Background Task that died mid-turn kept its ledger row, forced an
//! intermediate 「⏳ 等待后台任务」 yield, and only retired on a later pass.
//! It is now one step with three callers on the read each already performs:
//! Session Sync, the Turn's drain tick and the out-of-turn follow/settle loop.
//! The live render path renders the retirement entries the read carries, so
//! nothing is swallowed by the process-local retirement overlay (ADR-0065).
//!
//! Guard rails, all shared:
//!
//! - a read that lists no live Background Task spends nothing;
//! - one verdict per Session per [`RuntimeReconcile::interval_ms`], across
//!   every caller — the reconcile's cost has a bound independent of the poll
//!   cadences that observe it;
//! - a failed or timed-out read leaves the transcript exactly as read: no
//!   verdict, no retirement, no settle (a flaky runtime can never end a wait).
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

    /// The one shared reconcile step (issue #454): read the runtime registries
    /// for the transcript's still-live Background Tasks and apply the
    /// positive-evidence verdicts to it. The caller passes the transcript read
    /// it already made; a shell the runtime reports ended — or no longer knows
    /// — leaves the live list as a retirement the caller's render will record,
    /// and its call ids go into the process-local overlay so every later read
    /// agrees (the launch record never flips).
    ///
    /// The same admitted cycle also fills the read's shell output windows
    /// (spec #588, #592): one tail read per still-live shell, so the ledger's
    /// window refreshes at this shared cadence and never at the caller's poll
    /// cadence. A window read is display-only — it never prompts, retires or
    /// settles — and a failed, vanished or empty one records
    /// [`ShellOutputRead::Unavailable`], which the row renders as no window at
    /// all (never a placeholder).
    ///
    /// Returns whether the read carried evidence that changed it (a retirement
    /// or a newly unconfirmed task) — a caller that is about to skip its render
    /// reads this so a retirement is never dropped on the floor. Zero requests
    /// when the read lists no live task; the throttle consumes the attempt even
    /// when the read fails or times out, and such a read changes nothing.
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
        let shells: Vec<String> = transcript
            .background_tasks
            .iter()
            .filter_map(|task| task.shell_id.clone())
            .collect();
        let children: Vec<String> = transcript
            .background_tasks
            .iter()
            .filter_map(|task| task.child_id.clone())
            .collect();
        if shells.is_empty() && children.is_empty() {
            return false;
        }
        if !self.begin(session_id) {
            return false;
        }
        let changed = match crate::bridge::bounded_call(
            "task runtime read",
            read_timeout_ms,
            backend.task_runtime(session_id, Some(directory), &shells, &children),
        )
        .await
        {
            Some(Ok(runtime)) => {
                let retired_before = transcript.runtime_retired.len();
                let unconfirmed_before = transcript.unconfirmed_tasks.len();
                transcript.apply_task_runtime(&runtime);
                // Record the retirements so every later transcript read — the
                // live render, the drain's settle, the follow, the reap — sees
                // them gone: the launch record never flips (issue #454), so
                // without the overlay the next read would resurrect the task.
                if !transcript.runtime_retired.is_empty() {
                    let call_ids: Vec<String> = transcript
                        .runtime_retired
                        .iter()
                        .map(|retirement| retirement.task.tool.call_id.clone())
                        .collect();
                    tracing::info!(
                        "session {session_id}: runtime reconciliation retired {} background task(s)",
                        call_ids.len()
                    );
                    backend.retire_background_tasks(session_id, &call_ids);
                }
                transcript.runtime_retired.len() > retired_before
                    || transcript.unconfirmed_tasks.len() > unconfirmed_before
            }
            Some(Err(error)) => {
                tracing::debug!(
                    "session {session_id} task runtime read failed: {error}; waiting for the next read"
                );
                false
            }
            None => false,
        };
        // The windows are read after the verdicts: a shell the runtime just
        // retired leaves the live list here and spends no tail read. A failed
        // runtime read changes nothing about them — the transcript still lists
        // the shells, and the window is display-only.
        capture_shell_outputs(backend, session_id, directory, transcript, read_timeout_ms).await;
        changed
    }
}

/// Fill one read's shell output windows (spec #588, #592): one tail read per
/// still-live shell, deduped, each bounded by the caller's read timeout. A
/// shell whose read fails, vanishes or answers nothing records
/// [`ShellOutputRead::Unavailable`] — the row omits the window — while a shell
/// is never asked twice in one cycle.
async fn capture_shell_outputs(
    backend: &Arc<dyn Backend>,
    session_id: &str,
    directory: &str,
    transcript: &mut SessionTranscript,
    read_timeout_ms: u64,
) {
    let mut seen = std::collections::HashSet::new();
    let shells: Vec<String> = transcript
        .background_tasks
        .iter()
        .filter_map(|task| task.shell_id.clone())
        .filter(|shell_id| seen.insert(shell_id.clone()))
        .collect();
    for shell_id in shells {
        let output = match crate::bridge::bounded_call(
            "shell output read",
            read_timeout_ms,
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
