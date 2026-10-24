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
//! The reconcile records the evidence ending it derives **at derivation**
//! (ADR-0080): one synchronous Chain Record write, the fact about the run
//! rather than the card that will show its entry, an entry-less chain included.
//! What remains deferred is the process-local tier (spec #588, review PR #595):
//! a reconcile pass hands back a [`ReconcilePass`] and writes NOTHING to the
//! overlay itself. The caller commits it — one [`ReconcilePass::commit`] call —
//! only AFTER the card write that carried the read's entries was accepted
//! (Session Sync's wake/ledger refresh, the drain tick's and the settle loop's
//! completed render, the cleanup click's landed refresh). A caller whose
//! carrier was refused drops the pass: the durable ending still retired the
//! task (the next read synthesizes its entry from the record), and the
//! unconfirmed marker stays exactly as the refused read left it.
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
    /// The step is deliberately gate-free on the READ side: each caller decides
    /// whether it can render what it observes (Session Sync's waiting-card
    /// admission, the live loops' own anchor) before calling. The WRITE side is
    /// deferred: the returned [`ReconcilePass`] is the caller's to commit after
    /// its card write landed, and a pass that never commits records nothing.
    pub(crate) async fn observe(
        &self,
        backend: &Arc<dyn Backend>,
        session_id: &str,
        directory: &str,
        transcript: &mut SessionTranscript,
        read_timeout_ms: u64,
    ) -> Option<ReconcilePass> {
        if !backend.background_tasks_supported() {
            // A client with no Background Task surface has nothing to
            // reconcile: no verdict, no child evidence, no shell window. The
            // absence is the client's declaration, not an empty read.
            return None;
        }
        let (shells, children) = live_task_ids(transcript);
        if shells.is_empty() && children.is_empty() {
            return None;
        }
        if !self.begin(session_id) {
            return None;
        }
        let budget = CycleBudget::within(read_timeout_ms);
        let pass = reconcile_now(backend, session_id, Some(directory), transcript, budget).await;
        // The windows are read after the verdicts: a shell the runtime just
        // retired leaves the live list here and spends no tail read. A failed
        // runtime read changes nothing about them — the transcript still lists
        // the shells, and the window is display-only.
        capture_shell_outputs(backend, session_id, directory, transcript, budget).await;
        pass
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
        // A remainder at or below the timer wheel's resolution counts as spent:
        // a read granted it would be abandoned on the same tick (and a tokio
        // timer may fire up to ~1 ms early), so issuing it would spend a
        // request with no budget. The granted timeout rounds UP, so a hung
        // read cannot leave a sub-millisecond remainder behind for the next
        // read to be issued against (spec #588, review PR #595).
        if remaining <= std::time::Duration::from_millis(1) {
            tracing::debug!("{what} skipped: the reconcile cycle's read budget is spent");
            return None;
        }
        crate::bridge::bounded_call(what, remaining.as_millis() as u64 + 1, fut).await
    }
}

/// One reconcile pass's admission to commit its verdicts to the process-local
/// Background Task **cache** (spec #588, review PR #595; ADR-0080). Returned by
/// [`RuntimeReconcile::observe`] and [`reconcile_now`] when — and only when — a
/// verdict was applied to the read.
///
/// **The invariant that stays deferred**: the process-local overlay — this
/// life's `retired` cache and the unconfirmed-marker set — is written only
/// after the flush that carried the read's entries was accepted. This type is
/// the only door to [`record_overlay`], so no reconcile writes the overlay from
/// inside itself; each caller commits after the card write that carried the
/// read's entries landed. Dropping the pass leaves the cache untouched — which
/// is no loss, because the ENDING itself was already recorded durably at
/// derivation ([`reconcile_now`]): the task still leaves the ledger and its
/// entry stays owed to the next card that can carry it, and only the marker
/// set waits on the accepted write.
#[must_use = "commit the pass after the flush that carried its entries; dropping it leaves the cache (not the durable ending) unwritten"]
pub(crate) struct ReconcilePass {
    /// Whether the pass moved the read: a new retirement, or a changed
    /// unconfirmed-marker set. A pass that changed nothing is still a pass —
    /// its post-verdict marker set is what a commit writes.
    changed: bool,
}

impl ReconcilePass {
    /// Whether the pass moved the read: a retirement joined, or the
    /// unconfirmed-marker set changed.
    pub(crate) fn changed(&self) -> bool {
        self.changed
    }

    /// Commit the pass's verdicts to the process-local overlay: every
    /// retirement the transcript carries — the runtime verdict's, the child
    /// evidence's, and the cleanup click's own synthetic Cleaned entries when
    /// it calls this — so every later read (the live render, the drain's
    /// settle, the follow, the reap) sees them gone; the launch record never
    /// flips, so without the overlay the next read would resurrect the task.
    /// And the pass's post-evidence unconfirmed set — carried markers
    /// included, resolved ones absent — so a read the shared throttle does not
    /// spend a verdict on keeps the markers and the cleanup button they gate
    /// (review, spec #588).
    ///
    /// The caller's contract: the flush that carried this read's entries was
    /// accepted, so every later read agrees with the card the user just saw.
    pub(crate) fn commit(self, backend: &Arc<dyn Backend>, session_id: &str, transcript: &SessionTranscript) {
        record_overlay(backend, session_id, transcript);
    }
}

/// One NON-throttled reconcile pass over a read's still-live Background Tasks
/// (issue #454): the shell and child ids the read names, one bounded runtime
/// read, the positive-evidence verdicts applied to the transcript, and at most
/// one child-evidence read (#591) per subagent the verdict left unconfirmed.
/// Zero requests when the read lists no live task; a failed or timed-out read
/// leaves the transcript exactly as read — no verdict, no retirement, no
/// settle — so a flaky runtime can never end a wait.
///
/// NOTHING is written to the overlay here (review, PR #595): the returned
/// [`ReconcilePass`] is the caller's to commit once the card write that carried
/// the read's entries was accepted. [`RuntimeReconcile::observe`] composes this
/// under its attempt-stamped 30 s throttle, which is how the poll paths
/// (Session Sync, the drain, the follow/settle loop) share one verdict; the
/// cleanup click calls it directly (spec #588, #590), because a click must
/// never no-op behind a recent poll's verdict — it spends its own runtime read
/// on the transcript it just read and commits the overlay itself only once its
/// card refresh landed. The recording stays this one path either way, so every
/// later read of the Session agrees on what retired.
///
/// The caller hands in the cycle's [`CycleBudget`] (spec #588, review PR
/// #595): `observe` starts one for the whole admitted cycle (this pass plus
/// the shell-window reads), the cleanup click starts its own (`now + its read
/// timeout`). Every read here is bounded by what remains of it, and a spent
/// budget skips the read — no request — so no caller can stack one timeout
/// per suspect.
///
/// `None` when no verdict was applied at all: the read lists no live task
/// (nothing was asked), or the runtime read failed or timed out (the transcript
/// stays exactly as read). The cleanup click keeps `None` and a
/// changed-nothing pass apart, because only a re-derived set may clear rows
/// (ADR-0073).
pub(crate) async fn reconcile_now(
    backend: &Arc<dyn Backend>,
    session_id: &str,
    directory: Option<&str>,
    transcript: &mut SessionTranscript,
    budget: CycleBudget,
) -> Option<ReconcilePass> {
    if !backend.background_tasks_supported() {
        return None;
    }
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
            // Record the RUNTIME verdict's endings AT DERIVATION (ADR-0080) —
            // one synchronous Chain Record write per run, the fact about the run
            // rather than the card that will show its entry, an entry-less chain
            // included — BEFORE the child-evidence reads below. A slow or
            // timed-out child read (and a pass the caller abandons while it
            // waits) can then neither delay nor lose a parent's ending. A failed
            // or timed-out runtime READ never reaches this arm, so it records
            // nothing: no verdict, no ending. This is the reopened trade
            // (ADR-0073/PR #595): the ending is durable, so a permanently
            // refused carrier no longer keeps the task live — the task leaves
            // the ledger and the entry stays owed. Only the process-local
            // overlay below stays gated on the caller's accepted write; the
            // unconfirmed marker keeps that gate unchanged.
            let mut recorded_retirements = retired_before;
            record_new_endings(backend, session_id, transcript, &mut recorded_retirements);
            // The child-evidence step (#591, issue #464): a subagent the
            // runtime no longer reports active can still be retirable on its
            // own transcript's evidence — and the very failure mode this
            // family exists for is the missing Wake. At most ONE light read
            // per suspect per pass: a terminal step finish with its completion
            // stamp retires the task, a gone child session retires it as lost,
            // and a read that cannot conclude — or fails, or times out, or is
            // skipped by the spent cycle budget — keeps the row exactly as the
            // runtime marked it, never guessed. A read that DOES conclude
            // records its ending at the derivation too.
            for (call_id, child_id) in transcript.unconfirmed_children() {
                match budget
                    .read("child evidence read", backend.child_evidence(&child_id))
                    .await
                {
                    Some(Ok(evidence)) => {
                        transcript.apply_child_evidence(&call_id, evidence);
                        record_new_endings(backend, session_id, transcript, &mut recorded_retirements);
                    }
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
            // NOTHING is committed here (review, PR #595): the caller commits
            // the returned pass only after the flush that carried this read's
            // entries was accepted.
            Some(ReconcilePass {
                changed: transcript.task_retirements.len() > retired_before
                    || transcript.unconfirmed_tasks != unconfirmed_before,
            })
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

/// Record every evidence ending the read derived after
/// `recorded_retirements` (ADR-0080) — one synchronous Chain Record write per
/// newly retired run, at the moment it was derived.
/// `recorded_retirements` is advanced past what it wrote, so a caller can
/// record again after a later child-evidence read without writing the same
/// retirement twice. A read that FAILED or TIMED OUT never produced a
/// retirement, so nothing is recorded for it.
fn record_new_endings(
    backend: &Arc<dyn Backend>,
    session_id: &str,
    transcript: &SessionTranscript,
    recorded_retirements: &mut usize,
) {
    for retirement in &transcript.task_retirements[*recorded_retirements..] {
        let input = transcript.tool_input(retirement.task.tool.call_id.as_str());
        backend
            .tasks()
            .record_evidence_ending(session_id, retirement, input);
    }
    *recorded_retirements = transcript.task_retirements.len();
}

/// Commit one reconcile pass's verdicts to the process-local cache (issue
/// #454, review PR #595) — the body [`ReconcilePass::commit`] wraps, private so
/// the pass is the only door: every retirement the transcript carries (the
/// **durable ending** was already written at derivation, so this only fills
/// this life's cache), and the pass's post-evidence unconfirmed set.
/// [`reconcile_now`] never calls it itself; the caller commits only after the
/// flush that carried the read's entries was accepted.
///
/// The DURABLE Task Ending fact (ADR-0080) is deliberately not written here:
/// this gate counts a recoverably failed write still owed by the outbox as
/// accepted, and a shown bit must ride a write that really DELIVERED the entry.
/// The bit therefore travels with the body's staged cursor, whose confirmation
/// is the project's delivered-write choke point.
fn record_overlay(backend: &Arc<dyn Backend>, session_id: &str, transcript: &SessionTranscript) {
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
        backend.tasks().record(session_id, &call_ids);
    }
    let unconfirmed: Vec<String> = transcript.unconfirmed_tasks.iter().cloned().collect();
    backend.tasks().set_unconfirmed(session_id, &unconfirmed);
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::{BackgroundTask, ChildRuntime, ShellEnd, ShellRuntime, ToolIdentity};
    use crate::bridge::chain::ChainRecords;
    use crate::bridge::test_support::MockBackend;
    use std::sync::Arc;

    /// A launch as a read still carries it.
    fn task(call_id: &str, shell_id: Option<&str>, child_id: Option<&str>) -> BackgroundTask {
        BackgroundTask {
            tool: ToolIdentity {
                name: "shell".into(),
                call_id: call_id.into(),
            },
            shell_id: shell_id.map(str::to_string),
            child_id: child_id.map(str::to_string),
            started_at: Some(1_000),
        }
    }

    /// ADR-0080: "The ending is recorded when it is derived." A parent's
    /// runtime verdict must be durable the moment it is derived — BEFORE the
    /// pass awaits a child-evidence read — so a slow or timed-out child read
    /// can neither delay it nor, if the pass is abandoned while it waits, lose
    /// it. The runtime READ here succeeded, so the ticket's rule ("a failed or
    /// timed-out runtime read records nothing") does not apply.
    #[tokio::test]
    async fn a_parent_verdict_records_before_a_timed_out_child_read() {
        let dir = tempfile::tempdir().unwrap();
        let chains = Arc::new(ChainRecords::load(dir.path().join("chain_records.json")));
        let backend = Arc::new(MockBackend::new(Vec::new()));
        backend.overlay.attach_chains(Arc::clone(&chains));
        // The shell is retired by the runtime verdict; the child stays a
        // suspect, so the pass spends a child-evidence read — which hangs.
        backend.task_runtime.lock().unwrap().shells = vec![(
            "sh_call_bg".into(),
            ShellRuntime::Ended {
                end: ShellEnd::Killed,
                completed_at: Some(2_000),
            },
        )];
        backend.task_runtime.lock().unwrap().children = vec![("ses_call_sub".into(), ChildRuntime::Inactive)];
        backend.hang_child_evidence_reads(1);

        let mut transcript = SessionTranscript::new(vec![]).with_background_tasks(vec![
            task("call_bg", Some("sh_call_bg"), None),
            task("call_sub", None, Some("ses_call_sub")),
        ]);

        let dyn_backend: Arc<dyn Backend> = backend.clone();
        let handle = tokio::spawn(async move {
            reconcile_now(
                &dyn_backend,
                "ses_test",
                None,
                &mut transcript,
                CycleBudget::within(200),
            )
            .await
        });

        // Wait until the (hanging) child-evidence read has STARTED.
        for _ in 0..2_000 {
            if !backend.child_evidence_calls.lock().await.is_empty() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(1)).await;
        }
        assert!(
            !backend.child_evidence_calls.lock().await.is_empty(),
            "the child-evidence read must have started"
        );
        // The parent's ending is already durable while that read is still in
        // flight: it was recorded AT DERIVATION, not after the loop.
        assert!(
            chains
                .endings("ses_test")
                .iter()
                .any(|record| record.run.names("call_bg")),
            "the parent verdict's ending is recorded before the child read returns"
        );
        assert!(
            !chains
                .endings("ses_test")
                .iter()
                .any(|record| record.run.names("call_sub")),
            "the child's ending is not derived: its read gave no verdict"
        );

        let _ = handle.await;
    }

    /// Spec #860, ticket #880: a client that declares no Background Task
    /// surface is not asked for a verdict at all. The empty ledger is a
    /// declaration, not an empty read — so the reconcile spends no request and
    /// leaves the scripted transcript untouched.
    #[tokio::test]
    async fn a_client_without_a_task_surface_spends_no_runtime_read() {
        let mut mock = MockBackend::new(Vec::new());
        mock.without_task_surface();
        let mock = Arc::new(mock);
        let calls = Arc::clone(&mock.task_runtime_calls);
        let backend: Arc<dyn Backend> = mock;
        let mut transcript = SessionTranscript::new(vec![]).with_background_tasks(vec![task(
            "call_bg",
            Some("sh_call_bg"),
            None,
        )]);

        let reconcile = RuntimeReconcile::new();
        reconcile
            .interval_ms
            .store(0, std::sync::atomic::Ordering::Relaxed);
        assert!(
            reconcile
                .observe(&backend, "ses_test", "/work", &mut transcript, 50)
                .await
                .is_none(),
            "a client with no task surface has nothing to reconcile"
        );
        assert!(
            reconcile_now(
                &backend,
                "ses_test",
                Some("/work"),
                &mut transcript,
                CycleBudget::within(50)
            )
            .await
            .is_none()
        );
        assert!(calls.lock().await.is_empty(), "no runtime verdict was asked for");
        assert!(
            transcript.background_tasks.len() == 1,
            "the pass changes nothing, exactly as a failed read would"
        );
    }
}
