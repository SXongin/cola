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
//! The write is the run lifecycle owner's, not this module's (ADR-0111 §2):
//! every derivation goes through [`RunLifecycle::retire`], which records the
//! ending and hands back the retirement the read renders — so the runtime
//! reconcile keeps no ending-writing code of its own.
//!
//! What remains deferred is the process-local tier (spec #588, review PR #595):
//! a reconcile pass hands back a [`Pass`](crate::backend::runs::Pass) and writes
//! NOTHING to the overlay itself. The caller commits it — one
//! [`Pass::commit`](crate::backend::runs::Pass::commit) call — only AFTER the
//! card write that carried the read's entries was accepted
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

use crate::backend::runs::{Pass, RetireCause, RetireRead};
use crate::backend::{Backend, SessionTranscript, ShellOutputRead, TurnAnchor};

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
    /// deferred: the returned [`Pass`](crate::backend::runs::Pass) is the caller's
    /// to commit after
    /// its card write landed, and a pass that never commits records nothing.
    pub(crate) async fn observe(
        &self,
        backend: &Arc<dyn Backend>,
        session_id: &str,
        directory: &str,
        transcript: &mut SessionTranscript,
        read_timeout_ms: u64,
        anchor: Option<&TurnAnchor>,
    ) -> Option<Pass> {
        if !backend.reports_live_runs() {
            // A client with no live-run surface has nothing to reconcile: no
            // verdict, no child evidence, no shell window. The absence is the
            // client's declaration, not an empty read.
            return None;
        }
        let (shells, children) = live_task_ids(transcript);
        // A terminal run (a report, or a launch whose refs all declare a
        // terminal status) has no live child id (`run_children` lists live
        // runs only), but it still owes a child-evidence read: the client says
        // it ended, and the child read supplies the durable `结束` ending. The
        // work gate must see it, or the row would disappear with no entry. A
        // childless run owes no read, but the pass is the only producer of its
        // Cleanup marker: the Waiting card's button would otherwise never
        // exist, and the Turn would wait on a run nothing can end.
        if shells.is_empty()
            && children.is_empty()
            && !backend
                .runs()
                .evidence_waits(transcript, anchor)
                .iter()
                .any(|wait| !wait.task)
            && !backend.runs().needs_cleanup_marking(transcript)
        {
            return None;
        }
        if !self.begin(session_id) {
            return None;
        }
        let budget = CycleBudget::within(read_timeout_ms);
        let pass = reconcile_now(backend, session_id, Some(directory), transcript, budget, anchor).await;
        // The windows are read after the verdicts: a shell the runtime just
        // retired leaves the live list here and spends no tail read. A failed
        // runtime read changes nothing about them — the transcript still lists
        // the shells, and the window is display-only.
        capture_shell_outputs(backend, session_id, directory, transcript, budget).await;
        pass
    }
}

/// Release one child session's backend subscription, best-effort: a client that
/// keeps nothing (the trait default) is a no-op, and a failed or hung release
/// must not fail or stall the pass that retired the run. The ending is recorded
/// before this call; the bound only protects the pass from a daemon that never
/// answers `thread/unsubscribe`.
pub(crate) async fn release_child_session(backend: &Arc<dyn Backend>, child_id: &str) {
    match tokio::time::timeout(
        std::time::Duration::from_millis(250),
        backend.release_session(child_id),
    )
    .await
    {
        Ok(Ok(())) => {}
        Ok(Err(error)) => tracing::debug!("release child session {child_id} failed: {error}"),
        Err(_) => tracing::debug!("release child session {child_id} timed out; detached"),
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

/// One NON-throttled reconcile pass over a read's still-live Background Tasks
/// (issue #454): the shell and child ids the read names, one bounded runtime
/// read, the positive-evidence verdicts applied to the transcript, and at most
/// one child-evidence read (#591) per subagent the verdict left unconfirmed.
/// Zero requests when the read lists no live task; a failed or timed-out read
/// leaves the transcript exactly as read — no verdict, no retirement, no
/// settle — so a flaky runtime can never end a wait.
///
/// NOTHING is written to the overlay here (review, PR #595): the returned
/// [`Pass`](crate::backend::runs::Pass) is the caller's to commit once the card
/// write that carried the read's entries was accepted. [`RuntimeReconcile::observe`] composes this
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
    anchor: Option<&TurnAnchor>,
) -> Option<Pass> {
    if !backend.reports_live_runs() {
        return None;
    }
    let (shells, children) = live_task_ids(transcript);
    // Same gate as [`RuntimeReconcile::observe`]: a terminal run (a report or a
    // launch/ref status) has no live child id but still owes the evidence read.
    // A childless run owes no read at all — its only exit is the Cleanup the
    // pass below marks it for — so it alone still justifies a pass.
    let runs = backend.runs();
    if shells.is_empty()
        && children.is_empty()
        && !runs
            .evidence_waits(transcript, anchor)
            .iter()
            .any(|wait| !wait.task)
        && !runs.needs_cleanup_marking(transcript)
    {
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
            runs.apply_task_runtime(session_id, transcript, &runtime);
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
            read_child_evidence(backend, runs, session_id, transcript, budget, anchor).await;
            // A childless run has no evidence read to conclude it: its only
            // exit is the user's Cleanup. Mark it so the ⚠️ row rides the
            // ledger and the Waiting card offers the button.
            runs.mark_cleanup_exits(transcript);
            // NOTHING is committed here (review, PR #595): the caller commits
            // the returned pass only after the flush that carried this read's
            // entries was accepted.
            Some(Pass::new(
                transcript.task_retirements.len() > retired_before
                    || transcript.unconfirmed_tasks != unconfirmed_before,
            ))
        }
        Some(Err(error)) => {
            tracing::debug!(
                "session {session_id} task runtime read failed: {error}; waiting for the next read"
            );
            // No runtime verdict, but the Cleanup exits do not depend on it:
            // a childless run or identity-less task must still get its button,
            // or a permanently failing runtime read wedges the Turn.
            cleanup_mark_pass(backend, transcript)
        }
        None => cleanup_mark_pass(backend, transcript),
    }
}

/// The child-evidence step (#591, issue #464): one light read per suspect, the
/// task path first and then the run path. A subagent the runtime no longer
/// reports active can still be retirable on its own transcript's evidence — and
/// the very failure mode this family exists for is the missing Wake. At most ONE
/// read per suspect per pass: a terminal step finish with its completion stamp
/// retires the task, a gone child session retires it as lost, and a read that
/// cannot conclude — or fails, or times out, or is skipped by the spent cycle
/// budget — keeps the row exactly as the runtime marked it, never guessed. A
/// read that DOES conclude records its ending at the derivation too (the owner's
/// `retire`, ADR-0080/0111).
///
/// The step is a `child_evidence`-only surface: a client that does not declare
/// the read (V1's shape, or a client with background runs but no child evidence)
/// must not be asked for it. Its tasks/runs keep whatever the runtime verdict
/// left.
async fn read_child_evidence(
    backend: &Arc<dyn Backend>,
    runs: &crate::backend::runs::RunLifecycle,
    session_id: &str,
    transcript: &mut SessionTranscript,
    budget: CycleBudget,
    anchor: Option<&TurnAnchor>,
) {
    if !backend.capabilities().child_evidence {
        return;
    }
    // The Background Tasks first: the task path owns its run, and retiring one
    // frees that run to be a suspect in THIS pass, exactly as it always has.
    for wait in runs
        .evidence_waits(transcript, anchor)
        .into_iter()
        .filter(|wait| wait.task)
    {
        let call_id = wait.key.clone();
        let child_id = wait.child_id.clone();
        match budget
            .read("child evidence read", backend.child_evidence(&child_id))
            .await
        {
            Some(Ok(evidence)) => {
                let had_task = transcript
                    .background_tasks
                    .iter()
                    .any(|task| task.tool.call_id == call_id);
                runs.retire(
                    session_id,
                    RetireRead::Observed(transcript),
                    &call_id,
                    RetireCause::ChildEvidence(evidence),
                );
                let retired = had_task
                    && !transcript
                        .background_tasks
                        .iter()
                        .any(|task| task.tool.call_id == call_id);
                if retired {
                    release_child_session(backend, &child_id).await;
                }
            }
            Some(Err(error)) => {
                tracing::debug!(
                    "session {session_id} child {child_id} evidence read failed: {error}; keeping its row"
                );
            }
            // A read that timed out — or that the spent budget skipped — yields
            // no verdict, like the runtime read.
            None => {}
        }
    }
    // The runs (spec #1057's P4): a child thread's own terminal item is the
    // evidence, so a child that ends with no Wake retires its row and settles
    // the yielded Turn.
    let now_ms = chrono::Utc::now().timestamp_millis();
    for wait in runs
        .evidence_waits(transcript, anchor)
        .into_iter()
        .filter(|wait| !wait.task)
    {
        let run_id = wait.key.clone();
        let child_id = wait.child_id.clone();
        let was_live = transcript
            .runs
            .iter()
            .any(|run| run.ids().any(|id| id == run_id) && run.is_live());
        match budget
            .read("run child evidence read", backend.child_evidence(&child_id))
            .await
        {
            Some(Ok(evidence)) => {
                let retired = runs
                    .retire(
                        session_id,
                        RetireRead::Observed(transcript),
                        &run_id,
                        RetireCause::RunChildEvidence(evidence),
                    )
                    .is_some();
                if !retired
                    && (!was_live
                        || (matches!(evidence, crate::backend::ChildEvidence::Unfinished)
                            && transcript.live_run_stale(&run_id, now_ms)))
                {
                    // A client-terminal run whose read did not conclude stays
                    // visible as unconfirmed so a Cleanup can end it: once a
                    // live run outlives the in-flight window and the read still
                    // concludes nothing, no machine can end it — the ⚠️ row and
                    // its button are its exit.
                    transcript.unconfirmed_tasks.insert(run_id.clone());
                }
                let still_live = transcript
                    .runs
                    .iter()
                    .any(|run| run.ids().any(|id| id == run_id) && run.is_live());
                if !still_live {
                    // The run retired, or is terminal and no longer needs its
                    // child subscription.
                    release_child_session(backend, &child_id).await;
                }
            }
            Some(Err(error)) => {
                // A failed read is no verdict. A young live child may still be
                // starting, so it keeps its live line and the next read retries;
                // a stale one can only end by the Cleanup.
                if !was_live || transcript.live_run_stale(&run_id, now_ms) {
                    transcript.unconfirmed_tasks.insert(run_id.clone());
                }
                tracing::debug!(
                    "session {session_id} run {run_id} child {child_id} evidence read failed: {error}; keeping its row"
                );
            }
            None => {
                // Inconclusive (timed out or budget-skipped): the same rule as a
                // failed read — a young live child retries, a stale one needs
                // the manual exit.
                if !was_live || transcript.live_run_stale(&run_id, now_ms) {
                    transcript.unconfirmed_tasks.insert(run_id.clone());
                }
            }
        }
    }
}

/// Mark the exit paths that need no runtime verdict (a childless run, an
/// identity-less task) on a read that spent no verdict: the marker is the only
/// way the waiting Turn can end, so a failed or skipped read must not withhold
/// it. Returns a pass only when the set moved.
fn cleanup_mark_pass(backend: &Arc<dyn Backend>, transcript: &mut SessionTranscript) -> Option<Pass> {
    let before = transcript.unconfirmed_tasks.clone();
    backend.runs().mark_cleanup_exits(transcript);
    (transcript.unconfirmed_tasks != before).then_some(Pass::new(true))
}

/// The shell and child session ids a read's still-live Background Tasks name,
/// in the read's own order — the reconcile's ask list. Empty when the read
/// lists no live task, which is what makes the reconcile spend nothing (and,
/// under [`RuntimeReconcile::observe`], not even consume a throttle attempt).
///
/// A **job-sourced** row carries its shell/child ids too, but its liveness is
/// the job list, never a runtime verdict (ADR-0118): asking for it would admit
/// a pass every throttle cycle only for [`RunLifecycle::apply_task_runtime`] to
/// refuse the answer. Job rows are therefore excluded here — but NOT from
/// [`capture_shell_outputs`], which is a different read: a shell job's output
/// window still renders whenever a pass runs for other work.
fn live_task_ids(transcript: &SessionTranscript) -> (Vec<String>, Vec<String>) {
    let shells = transcript
        .background_tasks
        .iter()
        .filter(|task| !task.is_job_sourced())
        .filter_map(|task| task.shell_id.clone())
        .collect();
    let mut children: Vec<String> = transcript
        .background_tasks
        .iter()
        .filter(|task| !task.is_job_sourced())
        .filter_map(|task| task.child_id.clone())
        .collect();
    // A child-only run has no Background Task to name it (Codex), so its child
    // id is a live-run id too. Deduped: a run and its Background Task usually
    // name the same child.
    for (_, child) in transcript.run_children() {
        if !children.contains(&child) {
            children.push(child);
        }
    }
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

    /// The child-only runs the owner's one suspect list names, as the tests
    /// assert them.
    fn run_suspects(read: &SessionTranscript, anchor: Option<&TurnAnchor>) -> Vec<(String, String)> {
        crate::backend::runs::RunLifecycle::new()
            .evidence_waits(read, anchor)
            .into_iter()
            .filter(|wait| !wait.task)
            .map(|wait| (wait.key, wait.child_id))
            .collect()
    }

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
            ..Default::default()
        }
    }

    /// A job-sourced Background Task (ADR-0118): the job id is its call
    /// identity and `job_id` is set, so the runtime ask list must leave it out.
    fn job_row(job_id: &str, kind: &str, shell_id: Option<&str>, child_id: Option<&str>) -> BackgroundTask {
        BackgroundTask {
            tool: ToolIdentity {
                name: kind.into(),
                call_id: job_id.into(),
            },
            shell_id: shell_id.map(str::to_string),
            child_id: child_id.map(str::to_string),
            started_at: Some(1_000),
            job_id: Some(job_id.into()),
            ..Default::default()
        }
    }

    /// A tool-part task the job list corroborates carries the job id but is
    /// not job-sourced, so the runtime ask list still includes it (ADR-0118).
    #[test]
    fn an_associated_tool_task_still_joins_the_runtime_ask_list() {
        let associated = BackgroundTask {
            tool: ToolIdentity {
                name: "shell".into(),
                call_id: "call_shell".into(),
            },
            shell_id: Some("sh1".into()),
            job_id: Some("job1".into()),
            ..Default::default()
        };
        assert!(!associated.is_job_sourced());
        let transcript = SessionTranscript::new(vec![]).with_background_tasks(vec![associated]);
        assert_eq!(
            live_task_ids(&transcript),
            (vec!["sh1".to_string()], Vec::new()),
            "the associated task is not job-sourced: the runtime still asks for it"
        );
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
                None,
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

    /// Review finding 5: the run-evidence step gates on the ADR-0106 live-runs
    /// predicate (already checked by the pass), not on the narrower Background
    /// Task surface — a client that reports background runs AND has no runtime
    /// child verdicts still reads its runs' children.
    #[tokio::test]
    async fn a_background_client_still_reads_its_run_children() {
        let mock = MockBackend::new(Vec::new());
        // Deliberately the default: `capabilities().background_runs` is true.
        mock.with_child_evidence(
            "thread_child",
            crate::backend::ChildEvidence::Terminal {
                completed_at: 2_000,
                started_at: None,
            },
        )
        .await;
        let mock = Arc::new(mock);
        let reads = Arc::clone(&mock.child_evidence_calls);
        let backend: Arc<dyn Backend> = mock;

        let launch = crate::backend::ToolCall {
            identity: ToolIdentity {
                name: "task".into(),
                call_id: "call_task".into(),
            },
            status: crate::backend::ToolStatus::Running,
            started_at: Some(1_000),
            detail: crate::backend::ToolDetail::Delegation {
                runs: vec![crate::backend::AgentRunRef {
                    run_id: "thread_child".into(),
                    status: None,
                }],
                effect: crate::backend::DelegationEffect::Launch,
            },
            ..crate::backend::ToolCall::default()
        };
        let mut transcript = SessionTranscript::new(vec![crate::backend::TranscriptMessage {
            id: crate::backend::MessageId::new("msg_1"),
            role: crate::backend::MessageRole::Assistant,
            time: None,
            model: None,
            tokens: None,
            error: None,
            parts: vec![crate::backend::Part::Tool(launch)],
        }]);
        let pass = reconcile_now(
            &backend,
            "ses_test",
            Some("/work"),
            &mut transcript,
            CycleBudget::within(200),
            None,
        )
        .await;
        assert!(pass.is_some(), "the pass applied a verdict");
        assert_eq!(
            reads.lock().await.as_slice(),
            ["thread_child".to_string()],
            "the run's child is read even though the runtime registry exists"
        );
        assert!(transcript.runs.is_empty());
        assert_eq!(
            transcript.task_retirements[0].ending,
            crate::backend::TaskRetirementEnding::ChildEnded
        );
    }

    /// A client that declares no child-evidence read is never asked for one:
    /// `reports_live_runs` (background runs) can be true while
    /// `child_evidence` is false, and both the task and run loops must stay
    /// silent.
    #[tokio::test]
    async fn a_client_without_child_evidence_is_not_asked() {
        let mut mock = MockBackend::new(Vec::new());
        mock.capabilities = crate::backend::AgentCapabilities {
            background_runs: true,
            child_evidence: false,
            ..crate::backend::AgentCapabilities::full()
        };
        let mock = Arc::new(mock);
        let reads = Arc::clone(&mock.child_evidence_calls);
        let backend: Arc<dyn Backend> = mock;

        let launch = crate::backend::ToolCall {
            identity: ToolIdentity {
                name: "task".into(),
                call_id: "call_run".into(),
            },
            status: crate::backend::ToolStatus::Running,
            started_at: Some(1_000),
            detail: crate::backend::ToolDetail::Delegation {
                runs: vec![crate::backend::AgentRunRef {
                    run_id: "thread_run_child".into(),
                    status: None,
                }],
                effect: crate::backend::DelegationEffect::Launch,
            },
            ..crate::backend::ToolCall::default()
        };
        let mut transcript = SessionTranscript::new(vec![crate::backend::TranscriptMessage {
            id: crate::backend::MessageId::new("msg_launch"),
            role: crate::backend::MessageRole::Assistant,
            time: None,
            model: None,
            tokens: None,
            error: None,
            parts: vec![crate::backend::Part::Tool(launch)],
        }])
        .with_background_tasks(vec![crate::backend::BackgroundTask {
            tool: ToolIdentity {
                name: "subagent".into(),
                call_id: "call_task".into(),
            },
            shell_id: None,
            child_id: Some("ses_task_child".into()),
            started_at: Some(1_000),
            ..Default::default()
        }]);
        transcript.unconfirmed_tasks.insert("call_task".into());

        let pass = reconcile_now(
            &backend,
            "ses_test",
            Some("/work"),
            &mut transcript,
            CycleBudget::within(200),
            None,
        )
        .await;
        assert!(pass.is_some(), "the runtime read still runs");
        assert!(
            reads.lock().await.is_empty(),
            "a client without child_evidence is never asked for a child read"
        );
        assert_eq!(transcript.runs.len(), 1, "the run keeps its row");
        assert_eq!(transcript.background_tasks.len(), 1, "the task keeps its row");
    }
    /// A V2 background subagent is both a Background Task and a derived run.
    /// Retiring the task on its child evidence must remove the matching run in
    /// the same step, so the run loop does not read the child again and append a
    /// second retirement for one child.
    #[tokio::test]
    async fn a_task_retirement_removes_its_matching_run_once() {
        let mock = MockBackend::new(Vec::new());
        mock.with_child_evidence(
            "ses_child",
            crate::backend::ChildEvidence::Terminal {
                completed_at: 2_000,
                started_at: None,
            },
        )
        .await;
        let mock = Arc::new(mock);
        let reads = Arc::clone(&mock.child_evidence_calls);
        let backend: Arc<dyn Backend> = mock;

        let launch = crate::backend::ToolCall {
            identity: ToolIdentity {
                name: "subagent".into(),
                call_id: "call_spawn".into(),
            },
            status: crate::backend::ToolStatus::Running,
            started_at: Some(1_000),
            detail: crate::backend::ToolDetail::Delegation {
                runs: vec![crate::backend::AgentRunRef {
                    run_id: "ses_child".into(),
                    status: None,
                }],
                effect: crate::backend::DelegationEffect::Launch,
            },
            ..crate::backend::ToolCall::default()
        };
        let mut transcript = SessionTranscript::new(vec![crate::backend::TranscriptMessage {
            id: crate::backend::MessageId::new("msg_launch"),
            role: crate::backend::MessageRole::Assistant,
            time: None,
            model: None,
            tokens: None,
            error: None,
            parts: vec![crate::backend::Part::Tool(launch)],
        }])
        .with_background_tasks(vec![crate::backend::BackgroundTask {
            tool: ToolIdentity {
                name: "subagent".into(),
                call_id: "call_spawn".into(),
            },
            shell_id: None,
            child_id: Some("ses_child".into()),
            started_at: Some(1_000),
            ..Default::default()
        }]);
        transcript.unconfirmed_tasks.insert("call_spawn".into());

        let pass = reconcile_now(
            &backend,
            "ses_test",
            Some("/work"),
            &mut transcript,
            CycleBudget::within(200),
            None,
        )
        .await;
        assert!(pass.is_some_and(|pass| pass.changed()));
        assert_eq!(
            reads.lock().await.as_slice(),
            ["ses_child".to_string()],
            "one child, one evidence read"
        );
        assert_eq!(transcript.task_retirements.len(), 1, "one run, one retirement");
        assert!(transcript.background_tasks.is_empty());
        assert!(
            transcript.runs.is_empty(),
            "the matching run leaves with its task, without a second ending"
        );
    }

    /// Review finding 1: a freshly spawned child answers "no rollout found"
    /// until it has run a turn. That is `Unfinished`, so the reconcile leaves
    /// the run live with no Lost ending — never a permanent 「已失联」 for a
    /// healthy first-turn child.
    #[tokio::test]
    async fn a_fresh_childs_no_history_keeps_its_run_live() {
        let mock = MockBackend::new(Vec::new());
        // The scripted default for an unscripted child is `Unfinished`: the
        // same verdict the Codex adapter derives from the recorded no-rollout
        // answer.
        let mock = Arc::new(mock);
        let reads = Arc::clone(&mock.child_evidence_calls);
        let releases = Arc::clone(&mock.release_calls);
        let backend: Arc<dyn Backend> = mock;

        let launch = crate::backend::ToolCall {
            identity: ToolIdentity {
                name: "task".into(),
                call_id: "call_task".into(),
            },
            status: crate::backend::ToolStatus::Running,
            started_at: Some(1_000),
            detail: crate::backend::ToolDetail::Delegation {
                runs: vec![crate::backend::AgentRunRef {
                    run_id: "thread_fresh".into(),
                    status: None,
                }],
                effect: crate::backend::DelegationEffect::Launch,
            },
            ..crate::backend::ToolCall::default()
        };
        let mut transcript = SessionTranscript::new(vec![crate::backend::TranscriptMessage {
            id: crate::backend::MessageId::new("msg_launch"),
            role: crate::backend::MessageRole::Assistant,
            time: None,
            model: None,
            tokens: None,
            error: None,
            parts: vec![crate::backend::Part::Tool(launch)],
        }]);

        let _ = reconcile_now(
            &backend,
            "ses_test",
            Some("/work"),
            &mut transcript,
            CycleBudget::within(200),
            None,
        )
        .await;
        assert_eq!(reads.lock().await.as_slice(), ["thread_fresh".to_string()]);
        assert!(
            releases.lock().await.is_empty(),
            "a still-live run keeps its child subscription"
        );
        assert_eq!(transcript.runs.len(), 1, "the run stays live");
        assert!(transcript.runs[0].is_live());
        assert!(
            transcript.task_retirements.is_empty(),
            "no Lost ending for a child with no history yet"
        );
        assert_eq!(transcript.settle(None), crate::backend::TurnSettle::Unreceived);
    }

    /// The ending is durable before the release attempt: a daemon that never
    /// answers `thread/unsubscribe` is bounded, never allowed to stall the
    /// reconcile or lose the fact.
    #[tokio::test]
    async fn a_hung_child_release_does_not_lose_the_ending() {
        let mock = MockBackend::new(Vec::new());
        mock.with_child_evidence(
            "thread_done",
            crate::backend::ChildEvidence::Terminal {
                completed_at: 2_000,
                started_at: None,
            },
        )
        .await;
        mock.hang_release.store(1, std::sync::atomic::Ordering::SeqCst);
        let mock = Arc::new(mock);
        let releases = Arc::clone(&mock.release_calls);
        let backend: Arc<dyn Backend> = mock;

        let launch = crate::backend::ToolCall {
            identity: ToolIdentity {
                name: "spawnAgent".into(),
                call_id: "call_spawn".into(),
            },
            status: crate::backend::ToolStatus::Running,
            started_at: Some(1_000),
            detail: crate::backend::ToolDetail::Delegation {
                runs: vec![crate::backend::AgentRunRef {
                    run_id: "thread_done".into(),
                    status: Some(crate::backend::RunStatus::Completed),
                }],
                effect: crate::backend::DelegationEffect::Launch,
            },
            ..crate::backend::ToolCall::default()
        };
        let mut transcript = SessionTranscript::new(vec![crate::backend::TranscriptMessage {
            id: crate::backend::MessageId::new("msg_spawn"),
            role: crate::backend::MessageRole::Assistant,
            time: None,
            model: None,
            tokens: None,
            error: None,
            parts: vec![crate::backend::Part::Tool(launch)],
        }]);
        let started = tokio::time::Instant::now();
        let pass = reconcile_now(
            &backend,
            "ses_test",
            Some("/work"),
            &mut transcript,
            CycleBudget::within(200),
            None,
        )
        .await;
        assert!(
            started.elapsed() < std::time::Duration::from_secs(2),
            "a hung release must not stall the reconcile: {:?}",
            started.elapsed()
        );
        assert!(pass.is_some_and(|pass| pass.changed()), "the ending was derived");
        assert_eq!(
            transcript.task_retirements[0].ending,
            crate::backend::TaskRetirementEnding::ChildEnded
        );
        assert_eq!(
            releases.lock().await.as_slice(),
            ["thread_done".to_string()],
            "the release was still attempted"
        );
    }

    /// Review finding 2: a report-terminal run is the ONLY work, so no live
    /// child id exists and the old gate skipped the pass entirely. The run's
    /// child read must still run through the reconcile to produce the entry
    /// and the durable ending.
    #[tokio::test]
    async fn a_report_terminal_run_alone_still_gets_its_ending() {
        let mock = MockBackend::new(Vec::new());
        mock.with_child_evidence(
            "thread_child",
            crate::backend::ChildEvidence::Terminal {
                completed_at: 2_000,
                started_at: None,
            },
        )
        .await;
        let mock = Arc::new(mock);
        let reads = Arc::clone(&mock.child_evidence_calls);
        let backend: Arc<dyn Backend> = mock;

        let report = crate::backend::ToolCall {
            identity: ToolIdentity {
                name: "codex/sub_agent_activity".into(),
                call_id: "call_activity".into(),
            },
            status: crate::backend::ToolStatus::Completed,
            started_at: Some(1_000),
            input: Some(serde_json::json!({
                "kind": "completed",
                "agentThreadId": "thread_child"
            })),
            detail: crate::backend::ToolDetail::Delegation {
                runs: vec![crate::backend::AgentRunRef {
                    run_id: "thread_child".into(),
                    status: None,
                }],
                effect: crate::backend::DelegationEffect::Report,
            },
            ..crate::backend::ToolCall::default()
        };
        let mut transcript = SessionTranscript::new(vec![crate::backend::TranscriptMessage {
            id: crate::backend::MessageId::new("msg_activity"),
            role: crate::backend::MessageRole::Assistant,
            time: None,
            model: None,
            tokens: None,
            error: None,
            parts: vec![crate::backend::Part::Tool(report)],
        }]);
        assert!(!transcript.runs[0].is_live(), "the client ended it");
        assert!(transcript.run_children().is_empty(), "no live child id");

        let pass = reconcile_now(
            &backend,
            "ses_test",
            Some("/work"),
            &mut transcript,
            CycleBudget::within(200),
            None,
        )
        .await;
        assert!(
            pass.is_some_and(|pass| pass.changed()),
            "the pass applied an ending"
        );
        assert_eq!(reads.lock().await.as_slice(), ["thread_child".to_string()]);
        assert_eq!(
            transcript.task_retirements[0].ending,
            crate::backend::TaskRetirementEnding::ChildEnded
        );
        assert!(transcript.runs.is_empty());
    }

    /// The transcript a Codex `subAgentActivity` with no `agentThreadId` reads
    /// as: one call-keyed run, no child id, no background task, no suspect.
    fn childless_activity() -> SessionTranscript {
        let report = crate::backend::ToolCall {
            identity: ToolIdentity {
                name: "codex/sub_agent_activity".into(),
                call_id: "call_activity".into(),
            },
            status: crate::backend::ToolStatus::Running,
            started_at: Some(1_000),
            input: Some(serde_json::json!({"kind": "started"})),
            detail: crate::backend::ToolDetail::Delegation {
                runs: Vec::new(),
                effect: crate::backend::DelegationEffect::Report,
            },
            ..crate::backend::ToolCall::default()
        };
        SessionTranscript::new(vec![crate::backend::TranscriptMessage {
            id: crate::backend::MessageId::new("msg_activity"),
            role: crate::backend::MessageRole::Assistant,
            time: Some(crate::backend::MessageTime {
                created: 1_000,
                completed: None,
            }),
            model: None,
            tokens: None,
            error: None,
            parts: vec![crate::backend::Part::Tool(report)],
        }])
    }

    /// A childless run has no child-evidence read at all. The reconcile must
    /// still run — nothing is live and no suspect exists — and mark it so the
    /// Waiting card offers the Cleanup exit, the only ending it can have.
    #[tokio::test]
    async fn a_childless_run_is_marked_for_cleanup() {
        let backend: Arc<dyn Backend> = Arc::new(MockBackend::new(Vec::new()));
        let mut transcript = childless_activity();
        assert!(
            backend.runs().needs_cleanup_marking(&transcript),
            "the report minted a call-keyed run"
        );

        let pass = reconcile_now(
            &backend,
            "ses_test",
            Some("/work"),
            &mut transcript,
            CycleBudget::within(200),
            None,
        )
        .await;
        assert!(
            pass.is_some_and(|pass| pass.changed()),
            "the marker alone still makes a pass"
        );
        assert!(
            transcript.unconfirmed_tasks.contains("call_activity"),
            "the run rides the ledger as ⚠️: {:?}",
            transcript.unconfirmed_tasks
        );
    }

    /// The throttled poll path is the one every read uses. It must admit the
    /// childless run too, or the marker (and with it the cleanup button) is
    /// never produced and the Turn waits on a run nothing can end.
    #[tokio::test]
    async fn the_throttled_pass_marks_a_childless_run() {
        let backend: Arc<dyn Backend> = Arc::new(MockBackend::new(Vec::new()));
        let mut transcript = childless_activity();
        let reconcile = RuntimeReconcile::new();
        reconcile
            .interval_ms
            .store(0, std::sync::atomic::Ordering::Relaxed);

        let pass = reconcile
            .observe(&backend, "ses_test", "/work", &mut transcript, 50, None)
            .await;
        assert!(
            pass.is_some_and(|pass| pass.changed()),
            "the throttled pass must not skip the only work there is"
        );
        assert!(
            transcript.unconfirmed_tasks.contains("call_activity"),
            "the ⚠️ marker reaches the card that will wait: {:?}",
            transcript.unconfirmed_tasks
        );
    }

    /// A failed runtime verdict read is no reason to withhold the Cleanup
    /// exit: a childless run has no other way out, so the marker is still
    /// produced on the error path.
    #[tokio::test]
    async fn a_failed_runtime_read_still_marks_the_cleanup_exit() {
        let mock = MockBackend::new(Vec::new());
        mock.fail_task_runtime_reads(usize::MAX);
        let backend: Arc<dyn Backend> = Arc::new(mock);
        let mut transcript = childless_activity();
        assert!(backend.runs().needs_cleanup_marking(&transcript));

        let pass = reconcile_now(
            &backend,
            "ses_test",
            Some("/work"),
            &mut transcript,
            CycleBudget::within(200),
            None,
        )
        .await;
        assert!(
            pass.is_some_and(|pass| pass.changed()),
            "the marker alone still makes a pass"
        );
        assert!(
            transcript.unconfirmed_tasks.contains("call_activity"),
            "the failed verdict read does not withhold the exit: {:?}",
            transcript.unconfirmed_tasks
        );
    }

    /// A Background Task with neither a shell nor a child id is the same shape
    /// as a childless run: no runtime read can end it and no Wake names it, so
    /// the pass must mark it for the Cleanup click.
    #[tokio::test]
    async fn an_identityless_task_is_marked_for_cleanup() {
        let backend: Arc<dyn Backend> = Arc::new(MockBackend::new(Vec::new()));
        let mut transcript =
            SessionTranscript::new(vec![]).with_background_tasks(vec![crate::backend::BackgroundTask {
                tool: ToolIdentity {
                    name: "subagent".into(),
                    call_id: "call_sub".into(),
                },
                shell_id: None,
                child_id: None,
                started_at: Some(1_000),
                ..Default::default()
            }]);
        assert!(backend.runs().needs_cleanup_marking(&transcript));

        let pass = reconcile_now(
            &backend,
            "ses_test",
            Some("/work"),
            &mut transcript,
            CycleBudget::within(200),
            None,
        )
        .await;
        assert!(
            pass.is_some_and(|pass| pass.changed()),
            "the marker alone still makes a pass"
        );
        assert!(
            transcript.unconfirmed_tasks.contains("call_sub"),
            "the task rides the ledger as ⚠️: {:?}",
            transcript.unconfirmed_tasks
        );
    }

    /// A job-sourced row is not a childless task either (spec #1220, ticket
    /// #1223): its liveness is the V2 job list, so the runtime reconcile spends
    /// nothing on job-only work — the "no live task, no request" gate holds for
    /// all five job shapes, a shell or subagent job's ids included — and marks
    /// nothing, on the success or the failure path.
    #[tokio::test]
    async fn job_only_work_spends_no_runtime_read_and_marks_nothing() {
        for row in [
            job_row("job_cc", "codemode", None, None),
            job_row("job_img", "image", None, None),
            job_row("job_vid", "video", None, None),
            // A shell job names a shell id ...
            job_row("job_sh", "shell", Some("sh_job"), None),
            // ... and a subagent job a child id: neither makes it the runtime's.
            job_row("job_sub", "subagent", None, Some("ses_job_child")),
        ] {
            let mock = MockBackend::new(Vec::new());
            // A failed verdict read is the strongest absence: if the gate ever
            // admitted the pass, the failure path would mark the Cleanup exit.
            mock.fail_task_runtime_reads(usize::MAX);
            let mock = Arc::new(mock);
            let calls = Arc::clone(&mock.task_runtime_calls);
            let outputs = Arc::clone(&mock.shell_output_calls);
            let backend: Arc<dyn Backend> = mock;
            let mut transcript = SessionTranscript::new(vec![]).with_background_tasks(vec![row.clone()]);
            assert!(
                !backend.runs().needs_cleanup_marking(&transcript),
                "a job row has an identity: no Cleanup exit to mark ({})",
                row.tool.call_id
            );

            let reconcile = RuntimeReconcile::new();
            reconcile
                .interval_ms
                .store(0, std::sync::atomic::Ordering::Relaxed);
            assert!(
                reconcile
                    .observe(&backend, "ses_test", "/work", &mut transcript, 50, None)
                    .await
                    .is_none(),
                "job-only work is the read's, never the reconcile's ({})",
                row.tool.call_id
            );
            assert!(
                calls.lock().await.is_empty(),
                "the gate is restored: no runtime verdict was asked for ({})",
                row.tool.call_id
            );
            assert!(
                transcript.unconfirmed_tasks.is_empty(),
                "and no ⚠️ marker rides a live job ({}): {:?}",
                row.tool.call_id,
                transcript.unconfirmed_tasks
            );
            assert_eq!(
                transcript.background_tasks,
                vec![row.clone()],
                "the job row is untouched ({})",
                row.tool.call_id
            );
            // The job-only gate returns before the capture step, so a shell
            // job's window is owed to the first pass that runs for other work
            // (pinned by the sibling test below) — never to a job-only cycle.
            assert!(
                outputs.lock().await.is_empty(),
                "a job-only cycle spends no request at all ({})",
                row.tool.call_id
            );
        }
    }

    /// The shell-window half (ticket #1223 review): excluding job rows from the
    /// runtime ask list must not exclude them from the output capture. When a
    /// pass is admitted for another live task, the job's own shell still gets
    /// its window read — the capture iterates every Background Task.
    #[tokio::test]
    async fn a_shell_jobs_output_window_is_still_captured() {
        let mock = MockBackend::new(Vec::new());
        let mock = Arc::new(mock);
        let calls = Arc::clone(&mock.task_runtime_calls);
        let outputs = Arc::clone(&mock.shell_output_calls);
        let backend: Arc<dyn Backend> = mock;
        let mut transcript = SessionTranscript::new(vec![]).with_background_tasks(vec![
            // Another live shell is what admits the pass ...
            task("call_bg", Some("sh_bg"), None),
            // ... and the job's shell still rides the capture.
            job_row("job_sh", "shell", Some("sh_job"), None),
        ]);

        let reconcile = RuntimeReconcile::new();
        reconcile
            .interval_ms
            .store(0, std::sync::atomic::Ordering::Relaxed);
        assert!(
            reconcile
                .observe(&backend, "ses_test", "/work", &mut transcript, 50, None)
                .await
                .is_some(),
            "the non-job shell admits the pass"
        );
        assert_eq!(
            calls.lock().await.as_slice(),
            [("ses_test".to_string(), vec!["sh_bg".to_string()], Vec::new())],
            "only the non-job shell is asked for a verdict"
        );
        assert_eq!(
            outputs.lock().await.as_slice(),
            ["sh_bg".to_string(), "sh_job".to_string()],
            "the job's shell window is still captured"
        );
        assert!(
            transcript.shell_outputs.contains_key("sh_job"),
            "and stored on the read the ledger renders"
        );
    }

    /// A launch whose refs are ALL terminal creates a terminal run with no
    /// `reported_by`. It still owes the child read: otherwise the row leaves
    /// `runs` with no durable ending and no entry. The reconcile reads it and
    /// records the ending.
    #[tokio::test]
    async fn an_all_terminal_launch_still_reads_its_child() {
        let mock = MockBackend::new(Vec::new());
        mock.with_child_evidence(
            "thread_done",
            crate::backend::ChildEvidence::Terminal {
                completed_at: 2_000,
                started_at: None,
            },
        )
        .await;
        let mock = Arc::new(mock);
        let reads = Arc::clone(&mock.child_evidence_calls);
        let releases = Arc::clone(&mock.release_calls);
        let backend: Arc<dyn Backend> = mock;

        let launch = crate::backend::ToolCall {
            identity: ToolIdentity {
                name: "spawnAgent".into(),
                call_id: "call_spawn".into(),
            },
            status: crate::backend::ToolStatus::Running,
            started_at: Some(1_000),
            detail: crate::backend::ToolDetail::Delegation {
                runs: vec![crate::backend::AgentRunRef {
                    run_id: "thread_done".into(),
                    status: Some(crate::backend::RunStatus::Completed),
                }],
                effect: crate::backend::DelegationEffect::Launch,
            },
            ..crate::backend::ToolCall::default()
        };
        let mut transcript = SessionTranscript::new(vec![crate::backend::TranscriptMessage {
            id: crate::backend::MessageId::new("msg_spawn"),
            role: crate::backend::MessageRole::Assistant,
            time: None,
            model: None,
            tokens: None,
            error: None,
            parts: vec![crate::backend::Part::Tool(launch)],
        }]);
        assert!(!transcript.runs[0].is_live(), "the ref states it completed");
        assert!(transcript.runs[0].reported_by.is_empty());
        assert_eq!(
            run_suspects(&transcript, None),
            vec![("thread_done".to_string(), "thread_done".to_string())],
            "a terminal ref still owes its child's own evidence"
        );

        let pass = reconcile_now(
            &backend,
            "ses_test",
            Some("/work"),
            &mut transcript,
            CycleBudget::within(200),
            None,
        )
        .await;
        assert!(
            pass.is_some_and(|pass| pass.changed()),
            "the pass applied the child's ending"
        );
        assert_eq!(reads.lock().await.as_slice(), ["thread_done".to_string()]);
        assert_eq!(
            releases.lock().await.as_slice(),
            ["thread_done".to_string()],
            "the retired run's child subscription is released"
        );
        assert_eq!(
            transcript.task_retirements[0].ending,
            crate::backend::TaskRetirementEnding::ChildEnded
        );
        assert!(transcript.runs.is_empty());
    }

    /// A live child whose read never concludes (a child that died before its
    /// first turn answers `Unfinished` forever) has no machine-made exit. Once
    /// it outlives the in-flight window the pass marks it for the Cleanup
    /// click; a young child keeps its live line.
    #[tokio::test]
    async fn a_stale_live_run_is_marked_for_cleanup() {
        let mock = MockBackend::new(Vec::new());
        mock.with_child_evidence("thread_child", crate::backend::ChildEvidence::Unfinished)
            .await;
        let backend: Arc<dyn Backend> = Arc::new(mock);
        let now = chrono::Utc::now().timestamp_millis();
        let transcript_at = |at: i64| {
            let launch = crate::backend::ToolCall {
                identity: ToolIdentity {
                    name: "spawnAgent".into(),
                    call_id: "call_spawn".into(),
                },
                status: crate::backend::ToolStatus::Running,
                started_at: Some(at),
                detail: crate::backend::ToolDetail::Delegation {
                    runs: vec![crate::backend::AgentRunRef {
                        run_id: "thread_child".into(),
                        status: None,
                    }],
                    effect: crate::backend::DelegationEffect::Launch,
                },
                ..crate::backend::ToolCall::default()
            };
            SessionTranscript::new(vec![crate::backend::TranscriptMessage {
                id: crate::backend::MessageId::new("msg_1"),
                role: crate::backend::MessageRole::Assistant,
                time: Some(crate::backend::MessageTime {
                    created: at,
                    completed: None,
                }),
                model: None,
                tokens: None,
                error: None,
                parts: vec![crate::backend::Part::Tool(launch)],
            }])
        };

        let mut young = transcript_at(now - 60_000);
        let pass = reconcile_now(
            &backend,
            "ses_test",
            Some("/work"),
            &mut young,
            CycleBudget::within(200),
            None,
        )
        .await;
        assert!(!pass.is_some_and(|pass| pass.changed()));
        assert!(
            !young.unconfirmed_tasks.contains("thread_child"),
            "a young child keeps its live line: {:?}",
            young.unconfirmed_tasks
        );

        let mut stale = transcript_at(now - 11 * 60 * 1000);
        let pass = reconcile_now(
            &backend,
            "ses_test",
            Some("/work"),
            &mut stale,
            CycleBudget::within(200),
            None,
        )
        .await;
        assert!(
            pass.is_some_and(|pass| pass.changed()),
            "the marker alone makes a pass"
        );
        assert!(
            stale.unconfirmed_tasks.contains("thread_child"),
            "the stale live run is clearable: {:?}",
            stale.unconfirmed_tasks
        );

        // A transient failure on a YOUNG live run is no verdict: the line stays
        // and the next read retries.
        let failing = MockBackend::new(Vec::new());
        failing
            .fail_child_evidence_reads
            .store(1, std::sync::atomic::Ordering::SeqCst);
        let failing: Arc<dyn Backend> = Arc::new(failing);
        let mut young_failure = transcript_at(now - 60_000);
        for _ in 0..2 {
            reconcile_now(
                &failing,
                "ses_test",
                Some("/work"),
                &mut young_failure,
                CycleBudget::within(200),
                None,
            )
            .await;
        }
        assert!(
            !young_failure.unconfirmed_tasks.contains("thread_child"),
            "a transient failure (then recovery) never marks a young child: {:?}",
            young_failure.unconfirmed_tasks
        );

        // A run first seen by an address has no launch clock; its observation
        // still ages it past the window.
        let at = now - 11 * 60 * 1000;
        let address = crate::backend::ToolCall {
            identity: ToolIdentity {
                name: "spawnAgent".into(),
                call_id: "call_wait".into(),
            },
            status: crate::backend::ToolStatus::Running,
            started_at: Some(at),
            detail: crate::backend::ToolDetail::Delegation {
                runs: vec![crate::backend::AgentRunRef {
                    run_id: "thread_child".into(),
                    status: None,
                }],
                effect: crate::backend::DelegationEffect::Address,
            },
            ..crate::backend::ToolCall::default()
        };
        let mut addressed = SessionTranscript::new(vec![crate::backend::TranscriptMessage {
            id: crate::backend::MessageId::new("msg_1"),
            role: crate::backend::MessageRole::Assistant,
            time: Some(crate::backend::MessageTime {
                created: at,
                completed: None,
            }),
            model: None,
            tokens: None,
            error: None,
            parts: vec![crate::backend::Part::Tool(address)],
        }]);
        assert_eq!(
            addressed.runs[0].started_at, None,
            "an address does not date the child"
        );
        reconcile_now(
            &backend,
            "ses_test",
            Some("/work"),
            &mut addressed,
            CycleBudget::within(200),
            None,
        )
        .await;
        assert!(
            addressed.unconfirmed_tasks.contains("thread_child"),
            "a no-start live run still ages: {:?}",
            addressed.unconfirmed_tasks
        );
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
                .observe(&backend, "ses_test", "/work", &mut transcript, 50, None)
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
                CycleBudget::within(50),
                None,
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
