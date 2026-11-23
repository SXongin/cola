//! The run lifecycle's one owner (ADR-0111 §2).
//!
//! **Invariant.** Every run — a **Background Task** or a child-only **Agent
//! Run**, keyed by one [`RunKey`] and its generation — carries at most one
//! ending of each class (Evidence, Cleanup, Wake); a run is live exactly when no
//! ending resolves its `(key, generation)`. A client's `status` is evidence
//! toward a transition, never the definition of live.
//!
//! [`RunLifecycle`] is the fact's one writer. It is **capability-agnostic**: the
//! caller hands it the [`LiveRunSurface`] its client declares, and the
//! capability axes stay the caller's read-admission gates (ADR-0106). Its
//! durable tier is the narrow [`RunEndingStore`](super::RunEndingStore) trait
//! (implemented by `bridge::chain::ChainRecords`); its volatile overlay — this
//! life's `retired` cache and the `unconfirmed` marker set — is a cache of those
//! durable endings plus a per-read marker, never a second source of truth.
//!
//! # The interface
//!
//! - [`RunLifecycle::observe`] — apply the durable endings and the overlay to
//!   one read: synthesize the owed retirements, prune the ended (alias-aware),
//!   settle the launch panels of what left, re-apply the markers.
//! - [`RunLifecycle::retire`] — the single transition for every derivation
//!   cause (runtime verdict, child evidence, Cleanup): the row leaves its live
//!   list, its launch panel settles, and its ending is recorded durably *at
//!   derivation*.
//! - [`RunLifecycle::evidence_waits`] — the one suspect list a cycle may spend a
//!   child-evidence read on, the "a Background Task already represents this run"
//!   rule included.
//! - [`RunLifecycle::live`] — the live set's projection on an observed read.
//!
//! # Two-phase commit
//!
//! [`Pass`] keeps ADR-0080's split: an evidence or Cleanup ending is recorded at
//! derivation (a Wake's with its completion commit, in the render path), while
//! the volatile overlay is written only after the carrier write that showed the
//! read's entries was accepted (ADR-0072/0080). Dropping a pass loses nothing
//! durable — the next read synthesizes the owed entry from the record.
//!
//! # Lock order
//!
//! The store slot and the overlay are plain [`std::sync::Mutex`]es, each taken
//! and released inside one call — never awaited, never held across an await.
//! They sit last in the canonical order (`write_lock` → `cards` → task state):
//! [`RunLifecycle::observe`] holds the overlay while it asks the durable tier
//! (whose own lock is always taken and released inside one call, so it is never
//! held while waiting for the overlay), and nothing here takes the card or the
//! session locks.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex, MutexGuard};

use crate::backend::{
    AgentRun, BackgroundTask, ChildEvidence, ChildRuntime, Part, RunStatus, SessionTranscript, ShellEnd,
    ShellRuntime, TaskRetirement, TaskRetirementEnding, TaskRuntime, ToolIdentity, ToolStatus, TurnAnchor,
};

use super::{
    RunEndingStore, RunKey, TaskEnding, TaskEndingClass, TaskEndingEvidence, TaskEndingState, TaskRunRecord,
};

/// The provider-specific surface a caller's read may observe (ADR-0106): the
/// owner never reads the backend's capability declaration itself, so it stays
/// neutral. The axes are the caller's declaration, not a probe.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct LiveRunSurface {
    /// The client reports live runs (`background_runs || child_evidence`).
    /// Without it no reconcile ever records a marker, so the overlay's marker
    /// set is not consulted.
    pub(crate) reports_live_runs: bool,
    /// The client can be asked for a child session's own evidence. The
    /// reconcile gates its reads on this; the owner records what a caller
    /// derives.
    pub(crate) child_evidence: bool,
}

/// One run a reconcile cycle may spend a child-evidence read on (ADR-0111 §2):
/// the id its ending keys and the child session to read.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct EvidenceWait {
    /// The id the retirement keys: a Background Task's call id, or a child-only
    /// run's durable key (`AgentRun::ending_key`).
    pub(crate) key: String,
    /// The child session to read.
    pub(crate) child_id: String,
    /// Whether a **Background Task** represents this run (the task path) or
    /// the run stands alone (the run path).
    pub(crate) task: bool,
}

/// Why one run leaves the live list (ADR-0111 §2): the derivation the caller
/// made, never the owner's guess. A **Wake**'s ending is recorded where its
/// completion entry is committed (ADR-0080/0111 §2), not through here.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum RetireCause {
    /// The runtime's verdict: the shell's terminal end, or that the runtime no
    /// longer knows it (`end: None`).
    RuntimeVerdict {
        end: Option<ShellEnd>,
        completed_at: Option<i64>,
    },
    /// A **Background Task**'s child session evidence (the task path).
    ChildEvidence(ChildEvidence),
    /// A child-only **Agent Run**'s child session evidence (the run path — a run
    /// no Background Task represents).
    RunChildEvidence(ChildEvidence),
    /// The user's cleanup click, at `now_ms`.
    Cleanup { now_ms: i64 },
}

/// One reconcile pass's admission to commit its verdicts to the process-local
/// overlay (spec #588, review PR #595; ADR-0080/0111 §2). Returned by
/// [`RunLifecycle::observe`] and `reconcile_now` when — and only when — the read
/// moved.
///
/// **The invariant that stays deferred**: the overlay — this life's `retired`
/// cache and the unconfirmed-marker set — is written only after the flush that
/// carried the read's entries was accepted. [`Pass::commit`] is the only door,
/// so no read writes the overlay from inside itself. Dropping the pass leaves
/// the cache untouched — no loss: the ENDING was already recorded durably at
/// derivation, the task still leaves the ledger, and its entry stays owed to the
/// next card that can carry it.
#[must_use = "commit the pass after the flush that carried its entries; dropping it leaves the cache (not the durable ending) unwritten"]
pub(crate) struct Pass {
    /// Whether the pass moved the read: a new retirement, a pruned row, or a
    /// changed unconfirmed-marker set. A pass that changed nothing is still a
    /// pass — its post-verdict marker set is what a commit writes.
    changed: bool,
}

impl Pass {
    pub(crate) fn new(changed: bool) -> Self {
        Self { changed }
    }

    /// Whether the pass moved the read.
    pub(crate) fn changed(&self) -> bool {
        self.changed
    }

    /// Commit the pass's verdicts to the process-local overlay: every retirement
    /// the transcript carries — the runtime verdict's, the child evidence's and
    /// the cleanup click's — so every later read sees them gone, and the pass's
    /// post-evidence unconfirmed set, so a read the shared throttle does not
    /// spend a verdict on keeps the markers and the cleanup button they gate.
    ///
    /// The caller's contract: the flush that carried this read's entries was
    /// accepted, so every later read agrees with the card the user just saw.
    pub(crate) fn commit(self, owner: &RunLifecycle, session_id: &str, transcript: &SessionTranscript) {
        owner.record_overlay(session_id, transcript);
    }
}

/// The run lifecycle's one owner (ADR-0111 §2). Cheap to clone: the durable
/// slot and the overlay are each shared behind an `Arc`.
#[derive(Clone)]
pub(crate) struct RunLifecycle {
    /// The durable tier, late-bound like the Chain Record it belongs to: the
    /// owner is constructed before the record exists (the adapter is built
    /// first) and the record is attached once. `None` reads as the
    /// recordless-session shape.
    store: Arc<Mutex<Option<Arc<dyn RunEndingStore>>>>,
    /// The volatile overlay (ADR-0080): this life's cache of retired ids and the
    /// markers a verdict could not confirm.
    overlay: Arc<Mutex<OverlayState>>,
}

#[derive(Default)]
pub(crate) struct OverlayState {
    /// session_id → the call ids the runtime (or a cleanup) retired.
    pub(crate) retired: HashMap<String, HashSet<String>>,
    /// session_id → the live call ids the runtime could not confirm as
    /// running.
    pub(crate) unconfirmed: HashMap<String, HashSet<String>>,
}

impl Default for RunLifecycle {
    /// The owner with an unattached durable tier and nothing recorded — the
    /// shape [`Self::new`] builds, and the one the liveness tier's own unit
    /// tests construct (`src/backend/transcript.rs`).
    fn default() -> Self {
        Self::new()
    }
}

impl RunLifecycle {
    /// The owner with no durable tier attached and nothing recorded.
    pub(crate) fn new() -> Self {
        Self {
            store: Arc::new(Mutex::new(None)),
            overlay: Arc::new(Mutex::new(OverlayState::default())),
        }
    }

    /// Attach (or replace) the durable tier. Idempotent: `SharedCore::new` calls
    /// it once, right after it builds the Chain Record, on the SAME owner the
    /// adapter holds.
    pub(crate) fn attach_store(&self, store: Arc<dyn RunEndingStore>) {
        *self.store_slot() = Some(store);
    }

    /// The attached durable tier, cloned out of its shared slot. `None` until
    /// [`Self::attach_store`] runs; every durable read degrades to the
    /// recordless-session shape until then.
    fn store(&self) -> Option<Arc<dyn RunEndingStore>> {
        self.store_slot().clone()
    }

    /// Observed ONE read (ADR-0111 §2): synthesize every durable ending the
    /// session's records still owe, prune the live lists of every run an ending
    /// resolves (alias-aware), settle the launch panels of what left, and
    /// re-apply this life's markers to what is still live.
    ///
    /// The synthesis runs BEFORE the retention, over the same records, so an
    /// owed ending's entry is planned before its row leaves. Only an OWED
    /// ending is synthesized: a shown one already rendered, and a Wake ending
    /// renders from the Wake itself, never here.
    ///
    /// The returned [`Pass`] is the overlay's admission, not a durable write:
    /// nothing here is gated, and dropping it loses nothing durable.
    pub(crate) fn observe(
        &self,
        session_id: &str,
        transcript: &mut SessionTranscript,
        surface: LiveRunSurface,
    ) -> Option<Pass> {
        let mut changed = false;
        let state = self.overlay();
        let store = self.store();
        // A launch panel is the record of the request: once the client reports
        // every run it launched terminal, the panel settles with them instead of
        // reading live beside a finished run (ADR-0108 §10). This is the fold's
        // own settle, kept here because the derivation writes nothing and the
        // panel is the run lifecycle's fact.
        {
            let SessionTranscript { messages, runs, .. } = &mut *transcript;
            settle_reported_panels(messages, runs);
        }
        // One owed ending, one entry: a read that derived this run's retirement
        // itself already carries it, and the synthesis never doubles it.
        let records: Vec<TaskRunRecord> = store
            .as_ref()
            .map(|store| store.owed(session_id))
            .unwrap_or_default();
        for record in &records {
            for ending in record
                .endings
                .iter()
                .filter(|ending| ending.state == TaskEndingState::Owed)
            {
                let Some(retirement) = synthesized_retirement(record, ending) else {
                    continue;
                };
                if transcript
                    .task_retirements
                    .iter()
                    .any(|known| known.task.tool.call_id == retirement.task.tool.call_id)
                {
                    continue;
                }
                transcript.task_retirements.push(retirement);
                changed = true;
            }
        }
        // Every run with a durable ending has ended, whenever it was derived:
        // its row leaves the read, so a later reconcile cannot resurrect it and
        // the ledger cannot keep waiting on it. The match is ALIAS-AWARE
        // (ADR-0080): the record carries every id the run is known by, and the
        // row's own ids resolve the same record. RESOLVE AND FOLD IN ONE LOCK
        // (`observe_run`), so a writer holding only one of the ids resolves the
        // one record instead of minting a second one for one run. The
        // process-local `retired` cache rides along for a run whose ending write
        // a restart lost; it too matches any id the row is known by.
        //
        // ORDERING: the endings snapshot above was taken BEFORE this
        // observation, so a completion a writer records in between is pruned
        // from THIS read's live list while its ending is synthesized only on a
        // later read — the entry stays owed to the next card.
        changed |= prune_tasks(transcript, session_id, store.as_deref(), &state.retired);
        // The child-only runs obey the same rule (spec #1057's P4): a run with a
        // durable ending has ended, and the process-local cache matches it by
        // any id it is known by.
        let mut pruned_runs: Vec<AgentRun> = Vec::new();
        transcript.runs.retain(|run| {
            let identity = run_identity(run);
            // A resumed run is a new generation of the same child: an ending
            // older than the run's newest start must not suppress it.
            let ended = match store.as_deref() {
                Some(store) => {
                    store
                        .ended_at(session_id, &identity)
                        .is_some_and(|(ended_at, ending_start)| {
                            match (ended_at, run.started_at) {
                                (Some(ended_at), Some(started_at)) => ended_at >= started_at,
                                // An undated ending still names its generation by the
                                // start the derivation captured: a resumed run (a later
                                // start) survives, the ended one does not.
                                (None, Some(started_at)) => {
                                    ending_start.is_none_or(|ending_start| started_at <= ending_start)
                                }
                                _ => true,
                            }
                        })
                }
                None => false,
            };
            // The process-local set carries no clock: it may only suppress a run
            // the client already reports ended, never a live resume.
            let retired = !run.is_live()
                && state
                    .retired
                    .get(session_id)
                    .is_some_and(|ids| identity.ids().any(|id| ids.contains(id)));
            let keep = !ended && !retired;
            if !keep {
                pruned_runs.push(run.clone());
            }
            keep
        });
        if !pruned_runs.is_empty() {
            changed = true;
        }
        // A run with a durable ending leaves the read before the fold that would
        // settle its launch panel, so the panel would keep reading live beside
        // the ledger's entry. The prune settles it (ADR-0108 §10); the newest
        // report still in the messages supplies a failure word.
        for run in &pruned_runs {
            let launches: Vec<String> = run
                .launched_by
                .iter()
                .map(|launch| launch.call_id.clone())
                .collect();
            transcript.settle_run_launch_panels(
                &launches,
                run.child_id.as_deref(),
                &crate::backend::TaskRetirementEnding::ChildEnded,
            );
        }
        // A client with no live-run surface never records a marker (the
        // reconcile's own admission), so there is nothing to re-apply.
        if !surface.reports_live_runs {
            return changed.then_some(Pass::new(true));
        }
        let Some(ids) = state.unconfirmed.get(session_id) else {
            return changed.then_some(Pass::new(true));
        };
        let before = transcript.unconfirmed_tasks.clone();
        for task in &transcript.background_tasks {
            if ids.contains(task.tool.call_id.as_str()) {
                transcript.unconfirmed_tasks.insert(task.tool.call_id.clone());
            }
        }
        for run in &transcript.runs {
            // A marker minted while a call-keyed run had no child id names its
            // creating call. Once a later sighting fills the child id, the
            // durable key moves and that marker no longer names this run's
            // ending: drop it, so a live child never reads as unconfirmed.
            let stale_childless = run.child_id.is_some()
                && !ids.contains(run.ending_key())
                && ids.contains(run.run_id.as_str());
            if !stale_childless && run.ids().any(|id| ids.contains(id)) {
                transcript.unconfirmed_tasks.insert(run.ending_key().to_string());
            }
        }
        changed |= transcript.unconfirmed_tasks != before;
        changed.then_some(Pass::new(true))
    }

    /// The suspects a reconcile cycle may spend a child-evidence read on
    /// (ADR-0111 §2), in read order: the live subagent tasks the runtime could
    /// not confirm, then the runs that still owe a child read.
    ///
    /// A **Background Task**'s child is never read twice: the task path owns
    /// that row, so its run is excluded here — the one place the rule lives.
    /// The status is deliberately not a filter for the runs: a run the client
    /// declared terminal through a launch/ref status, or through a report, still
    /// has no durable ending until its child's own item is read — skipping it
    /// would drop the row with no entry and no ADR-0080 record.
    pub(crate) fn evidence_waits(
        &self,
        transcript: &SessionTranscript,
        anchor: Option<&TurnAnchor>,
    ) -> Vec<EvidenceWait> {
        let mut waits: Vec<EvidenceWait> = transcript
            .background_tasks
            .iter()
            .filter(|task| transcript.unconfirmed_tasks.contains(task.tool.call_id.as_str()))
            .filter_map(|task| {
                task.child_id.as_ref().map(|child_id| EvidenceWait {
                    key: task.tool.call_id.clone(),
                    child_id: child_id.clone(),
                    task: true,
                })
            })
            .collect();
        // Only a live run, or a terminal one belonging to the current Turn,
        // needs an evidence read. A terminal run from an earlier Turn is
        // history: every read would otherwise re-read the session's whole child
        // history and mint a completion entry for each.
        let cutoff = run_cutoff(transcript, anchor);
        waits.extend(
            transcript
                .runs
                .iter()
                .filter(|run| SessionTranscript::run_in_turn(run, cutoff))
                .filter_map(|run| {
                    let child = run.child_id.as_deref()?;
                    let task = transcript
                        .background_tasks
                        .iter()
                        .any(|task| transcript.run_is_task(run, task));
                    (!task).then(|| EvidenceWait {
                        key: run.ending_key().to_string(),
                        child_id: child.to_string(),
                        task: false,
                    })
                }),
        );
        waits
    }

    /// The live set's projection on an OBSERVED read (ADR-0111 §1): every run an
    /// ending resolves has already left `transcript.runs`, so the list is the
    /// projection. A run the client calls terminal stays listed until its own
    /// evidence records the ending (terminal-but-owed, ADR-0111 §1).
    pub(crate) fn live<'a>(&self, transcript: &'a SessionTranscript) -> impl Iterator<Item = &'a AgentRun> {
        transcript.runs.iter()
    }

    /// The single transition (ADR-0111 §2): `key` names one live run — a
    /// Background Task by its call id, or a child-only Agent Run by any id it is
    /// known by — and `cause` is the derivation that ended it. The row leaves
    /// its live list, its launch panel settles, and its ending is recorded
    /// durably AT DERIVATION. `None` when the key names no live run, or the
    /// cause is not evidence for the one it names (a previous turn's terminal
    /// item, an unfinished child): nothing is guessed.
    pub(crate) fn retire(
        &self,
        session_id: &str,
        transcript: &mut SessionTranscript,
        key: &str,
        cause: RetireCause,
    ) -> Option<TaskRetirement> {
        let retirement = match cause {
            RetireCause::RuntimeVerdict { end, completed_at } => {
                retire_runtime_verdict(transcript, key, end.as_ref(), completed_at)?
            }
            RetireCause::ChildEvidence(evidence) => retire_task_child_evidence(transcript, key, evidence)?,
            RetireCause::RunChildEvidence(evidence) => retire_run_child_evidence(transcript, key, evidence)?,
            RetireCause::Cleanup { now_ms } => retire_cleanup(transcript, key, now_ms)?,
        };
        // The durable ending goes at derivation (ADR-0080): the fact about the
        // run, not the card that will show its entry, so a refused carrier, an
        // entry-less chain and a restart all keep it.
        self.record_retirement(session_id, transcript, &retirement);
        Some(retirement)
    }

    /// The user's cleanup click (spec #588, #590): every named live Background
    /// Task or child-only run leaves its live list through [`Self::retire`], and
    /// the cleared ids leave the read's marker set. The ids come from the
    /// caller's read of this same transcript, so an id with no live row is
    /// ignored: a cleanup never invents a retirement.
    pub(crate) fn cleanup(
        &self,
        session_id: &str,
        transcript: &mut SessionTranscript,
        call_ids: &[String],
        now_ms: i64,
    ) {
        for call_id in call_ids {
            self.retire(session_id, transcript, call_id, RetireCause::Cleanup { now_ms });
            transcript.unconfirmed_tasks.remove(call_id);
        }
    }

    /// Apply one runtime reconciliation read (issue #454) to a transcript: a
    /// shell the runtime reports ended — or no longer knows at all — leaves the
    /// live list through [`Self::retire`] with the runtime's verdict; a
    /// subagent child the runtime reports inactive stays live but is marked
    /// unconfirmed; a task with no verdict is untouched. An empty read changes
    /// nothing, so V1 and a server that could not answer reconcile to the
    /// transcript exactly as read.
    ///
    /// The unconfirmed marker is resolved only on evidence: the read carries the
    /// markers the overlay re-applied (the previous reconciles' state), a
    /// `Running` verdict clears that child's marker, an `Inactive` one (re)sets
    /// it, and a verdict that does not answer for the child keeps exactly what
    /// the read carried. A task the verdict retires drops its marker with it.
    pub(crate) fn apply_task_runtime(
        &self,
        session_id: &str,
        transcript: &mut SessionTranscript,
        runtime: &TaskRuntime,
    ) {
        if runtime.shells.is_empty() && runtime.children.is_empty() {
            return;
        }
        let mut verdicts: Vec<(String, Option<ShellEnd>, Option<i64>)> = Vec::new();
        for index in 0..transcript.background_tasks.len() {
            let (call_id, shell_id, child_id) = {
                let task = &transcript.background_tasks[index];
                (
                    task.tool.call_id.clone(),
                    task.shell_id.clone(),
                    task.child_id.clone(),
                )
            };
            if let Some(shell_id) = shell_id {
                match runtime.shell(&shell_id) {
                    // No verdict: the read could not place this shell — leave
                    // the task exactly as the transcript read it.
                    None | Some(ShellRuntime::Running) => {}
                    Some(ShellRuntime::Ended { end, completed_at }) => {
                        verdicts.push((call_id, Some(end.clone()), *completed_at));
                    }
                    Some(ShellRuntime::Missing) => verdicts.push((call_id, None, None)),
                }
                continue;
            }
            let Some(child_id) = child_id else {
                // A task with neither identity cannot be reconciled.
                continue;
            };
            match runtime.child(&child_id) {
                // Positive evidence the run is live: the marker resolves.
                Some(ChildRuntime::Running) => {
                    transcript.unconfirmed_tasks.remove(&call_id);
                }
                Some(ChildRuntime::Inactive) => {
                    transcript.unconfirmed_tasks.insert(call_id);
                }
                // No answer (a read that could not place the child): keep the
                // marker exactly as the read carried it.
                None => {}
            }
        }
        for (call_id, end, completed_at) in verdicts {
            self.retire(
                session_id,
                transcript,
                &call_id,
                RetireCause::RuntimeVerdict { end, completed_at },
            );
        }
    }

    /// Whether the read carries an exit nothing else can produce: a run with no
    /// child id (the call-keyed run a report created, spec #1057) or a
    /// Background Task with neither a shell nor a child id. Neither has a
    /// transcript to read and no Wake can name it, so the runtime reconcile must
    /// run even when nothing is live — the Cleanup it marks is the only exit.
    pub(crate) fn needs_cleanup_marking(&self, transcript: &SessionTranscript) -> bool {
        self.live(transcript).any(|run| run.child_id.is_none())
            || transcript
                .background_tasks
                .iter()
                .any(|task| task.shell_id.is_none() && task.child_id.is_none())
    }

    /// Mark every childless run and identity-less task for the Cleanup exit:
    /// with no child there is no evidence read that can end it, so the ⚠️ row
    /// and its button are the only way the run/task — and the Turn waiting on
    /// it — can end.
    pub(crate) fn mark_cleanup_exits(&self, transcript: &mut SessionTranscript) {
        let keys: Vec<String> = self
            .live(transcript)
            .filter(|run| run.child_id.is_none())
            .map(|run| run.ending_key().to_string())
            .collect();
        let tasks: Vec<String> = transcript
            .background_tasks
            .iter()
            .filter(|task| task.shell_id.is_none() && task.child_id.is_none())
            .map(|task| task.tool.call_id.clone())
            .collect();
        for key in keys {
            transcript.unconfirmed_tasks.insert(key);
        }
        for call_id in tasks {
            transcript.unconfirmed_tasks.insert(call_id);
        }
    }

    /// Record the call ids the runtime retired for one session. Idempotent.
    pub(crate) fn record(&self, session_id: &str, call_ids: &[String]) {
        let mut state = self.overlay();
        let session = state.retired.entry(session_id.to_string()).or_default();
        for call_id in call_ids {
            session.insert(call_id.clone());
        }
    }

    /// Replace one session's unconfirmed-marker set with exactly `call_ids` —
    /// the reconcile's post-verdict set of live tasks it could not confirm as
    /// running. Replace semantics let a verdict clear a marker: a set without
    /// the id stops riding later reads, while a failed read never calls this and
    /// so clears nothing.
    pub(crate) fn set_unconfirmed(&self, session_id: &str, call_ids: &[String]) {
        let mut state = self.overlay();
        if call_ids.is_empty() {
            state.unconfirmed.remove(session_id);
            return;
        }
        state
            .unconfirmed
            .insert(session_id.to_string(), call_ids.iter().cloned().collect());
    }

    /// Seed the `retired` cache from the session's durable endings at a card
    /// install (ADR-0080): every run with a durable ending ended, whatever the
    /// read still lists, so a fresh install's reads carry the retirements the
    /// Chain Record already knows about without spending a reconcile verdict on
    /// them. EVERY id the run is known by is seeded — not just its stored key
    /// and call id — so the process-local fallback matches a row a read names by
    /// an alias. Idempotent — [`Self::record`] folds the same ids.
    pub(crate) fn seed_retired(&self, session_id: &str) {
        let Some(store) = self.store() else {
            return;
        };
        let records = store.owed(session_id);
        if records.is_empty() {
            return;
        }
        let mut state = self.overlay();
        let retired = state.retired.entry(session_id.to_string()).or_default();
        for record in &records {
            for id in record.run.ids() {
                retired.insert(id.to_string());
            }
        }
    }

    /// Record one run's ending durably from the retirement it derived. `input`
    /// is the launch's tool input, when the read still carries it, so the ending
    /// captures the label.
    fn record_retirement(
        &self,
        session_id: &str,
        transcript: &SessionTranscript,
        retirement: &TaskRetirement,
    ) {
        let Some(store) = self.store() else {
            return;
        };
        let input = transcript.tool_input(retirement.task.tool.call_id.as_str());
        store.record_retirement(session_id, retirement, input);
    }

    /// Commit the overlay one pass owes (the body [`Pass::commit`] wraps).
    fn record_overlay(&self, session_id: &str, transcript: &SessionTranscript) {
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
            self.record(session_id, &call_ids);
        }
        let unconfirmed: Vec<String> = transcript.unconfirmed_tasks.iter().cloned().collect();
        self.set_unconfirmed(session_id, &unconfirmed);
    }

    fn store_slot(&self) -> MutexGuard<'_, Option<Arc<dyn RunEndingStore>>> {
        self.store
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    pub(crate) fn overlay(&self) -> MutexGuard<'_, OverlayState> {
        self.overlay
            .lock()
            .expect("the run-lifecycle overlay lock is never poisoned")
    }
}

/// Settle a launch panel whose launched runs the client reports ALL terminal
/// (ADR-0108 §10): the panel is the record of the request, not the run, so it
/// must not read `Running` beside a finished child. A failure outranks a
/// cancellation, which outranks a complete; the launch call's own client
/// statement is a fact about the request and stays.
///
/// This is the read model's own projection of the client's statements, and the
/// run lifecycle's (ADR-0111 §2): the fold ([`super::derive`]) writes nothing,
/// so the panel settles here — applied by `SessionTranscript::new` (so a raw
/// projection and an observed read agree) and again by
/// [`RunLifecycle::observe`]. A run that RETIRES settles through
/// [`SessionTranscript::settle_run_launch_panels`] instead
/// ([`RunLifecycle::retire`]'s and the prune's own ending word), which is the
/// same rule seen from the ending rather than from the client's report.
pub(crate) fn settle_reported_panels(messages: &mut [crate::backend::TranscriptMessage], runs: &[AgentRun]) {
    let mut launch_ids: Vec<String> = runs
        .iter()
        .flat_map(|run| run.launched_by.iter().map(|launch| launch.call_id.clone()))
        .collect();
    launch_ids.sort();
    launch_ids.dedup();
    let mut settled: Vec<(String, ToolStatus)> = Vec::new();
    for launch_id in launch_ids {
        let group: Vec<&AgentRun> = runs
            .iter()
            .filter(|run| run.launched_by.iter().any(|launch| launch.call_id == launch_id))
            .collect();
        // One launch may name N children: it settles only when every one of
        // them is terminal.
        if group.is_empty() || !group.iter().all(|run| run.status.is_terminal()) {
            continue;
        }
        let client_status = messages
            .iter()
            .flat_map(|message| &message.parts)
            .find_map(|part| match part {
                Part::Tool(call) if call.identity.call_id == launch_id => Some(call.status.clone()),
                _ => None,
            });
        let failed = group.iter().any(|run| run.status == RunStatus::Failed)
            || matches!(client_status, Some(ToolStatus::Error));
        let cancelled = group.iter().any(|run| run.status == RunStatus::Cancelled)
            || matches!(&client_status, Some(ToolStatus::Other(reason)) if reason == "interrupted");
        let status = if failed {
            ToolStatus::Error
        } else if cancelled {
            ToolStatus::Other("interrupted".to_string())
        } else {
            ToolStatus::Completed
        };
        settled.push((launch_id, status));
    }
    for (call_id, status) in settled {
        for message in messages.iter_mut() {
            for part in &mut message.parts {
                if let Part::Tool(call) = part
                    && call.identity.call_id == call_id
                {
                    call.status = status.clone();
                }
            }
        }
    }
}

/// Prune every Background Task a durable ending resolves (ADR-0080), by the
/// writers' alias order. `observe_run` resolves and folds in ONE critical
/// section, so a writer holding only one of the row's ids resolves the record
/// the read observed instead of minting a second one.
fn prune_tasks(
    transcript: &mut SessionTranscript,
    session_id: &str,
    store: Option<&dyn RunEndingStore>,
    retired: &HashMap<String, HashSet<String>>,
) -> bool {
    let before = transcript.background_tasks.len();
    transcript.background_tasks.retain(|task| {
        let run = RunKey::of_launch(task);
        match store {
            Some(store) => !store.observe_run(session_id, &run),
            None => true,
        }
    });
    if let Some(ids) = retired.get(session_id) {
        transcript.background_tasks.retain(|task| {
            let run = RunKey::of_launch(task);
            !run.ids().any(|id| ids.contains(id))
        });
    }
    transcript.background_tasks.len() != before
}

/// The identity a read names `run` by, for the durable generation comparison.
fn run_identity(run: &AgentRun) -> RunKey {
    RunKey {
        key: run.ending_key().to_string(),
        call_id: run.task_run_id.clone(),
        shell_id: None,
        child_id: run.child_id.clone(),
        job_id: None,
        wake_id: None,
    }
}

/// The runtime verdict's transition: a shell the runtime reported ended or no
/// longer knows leaves the live list.
fn retire_runtime_verdict(
    transcript: &mut SessionTranscript,
    key: &str,
    end: Option<&ShellEnd>,
    completed_at: Option<i64>,
) -> Option<TaskRetirement> {
    let index = transcript
        .background_tasks
        .iter()
        .position(|task| task.tool.call_id == key)?;
    let (ending, finished_at) = match end {
        Some(end) => (TaskRetirementEnding::Ended(end.clone()), completed_at),
        None => (TaskRetirementEnding::Lost, None),
    };
    let task = transcript.background_tasks.remove(index);
    transcript.unconfirmed_tasks.remove(key);
    let retirement = TaskRetirement {
        task,
        ending,
        finished_at,
        label: None,
    };
    transcript.task_retirements.push(retirement.clone());
    Some(retirement)
}

/// The child evidence's transition for a **Background Task** named by its call
/// id. ADR-0080's rejections stay: a previous turn's terminal item and an
/// `Unfinished` read are no evidence.
fn retire_task_child_evidence(
    transcript: &mut SessionTranscript,
    key: &str,
    evidence: ChildEvidence,
) -> Option<TaskRetirement> {
    let index = transcript
        .background_tasks
        .iter()
        .position(|task| task.tool.call_id == key)?;
    let started_at = transcript.background_tasks[index].started_at;
    let (ending, finished_at) = match evidence {
        ChildEvidence::Terminal {
            completed_at,
            started_at: turn_started_at,
        } => {
            let turn_start = turn_started_at.unwrap_or(completed_at);
            if started_at.is_some_and(|started| turn_start.div_euclid(1000) < started.div_euclid(1000)) {
                // A previous turn's terminal item: the task stays live.
                return None;
            }
            (TaskRetirementEnding::ChildEnded, Some(completed_at))
        }
        ChildEvidence::Gone => (TaskRetirementEnding::Lost, None),
        ChildEvidence::Unfinished => return None,
    };
    let task = transcript.background_tasks.remove(index);
    // One run, one ending: the run this task represents leaves with it, so the
    // run-evidence loop cannot read the same child again and append a second
    // retirement under its own key.
    let mut launch_calls = vec![task.tool.call_id.clone()];
    if let Some(child) = task.child_id.as_deref() {
        for run in transcript
            .runs
            .iter()
            .filter(|run| run.run_id == child || run.child_id.as_deref() == Some(child))
        {
            launch_calls.extend(run.launched_by.iter().map(|launch| launch.call_id.clone()));
        }
        transcript
            .runs
            .retain(|run| run.run_id != child && run.child_id.as_deref() != Some(child));
    }
    // The launch panel is the record of the request, not the run: it settles
    // with the ending instead of reading live beside it. A shared launch waits
    // for its other children.
    transcript.settle_run_launch_panels(&launch_calls, task.child_id.as_deref(), &ending);
    transcript.unconfirmed_tasks.remove(key);
    let retirement = TaskRetirement {
        task,
        ending,
        finished_at,
        label: None,
    };
    transcript.task_retirements.push(retirement.clone());
    Some(retirement)
}

/// The child-evidence transition for a run no Background Task represents
/// (`apply_child_evidence`'s run path): a terminal child retires the run as a
/// [`TaskRetirement`] keyed by its durable identity; a gone child retires it as
/// lost. `Unfinished` is no evidence and changes nothing.
fn retire_run_child_evidence(
    transcript: &mut SessionTranscript,
    run_id: &str,
    evidence: ChildEvidence,
) -> Option<TaskRetirement> {
    let index = transcript
        .runs
        .iter()
        .position(|run| run.run_id == run_id || run.ending_key() == run_id)?;
    // A task path may already have retired this child this pass: never mint a
    // second ending for one run.
    if transcript.task_retirements.iter().any(|retirement| {
        retirement.task.child_id.is_some() && retirement.task.child_id == transcript.runs[index].child_id
    }) {
        transcript.runs.remove(index);
        return None;
    }
    let client_status = transcript.runs[index].status;
    let (ending, finished_at) = match evidence {
        ChildEvidence::Terminal {
            completed_at,
            started_at,
        } => {
            let turn_start = started_at.unwrap_or(completed_at);
            // Only a launch (or a resume, which moves `started_at`) dates the
            // generation. A report is an observation, not a new generation: a
            // terminal report between the child's turn start and its evidence
            // must not reject the child's own ending. A same-second start is
            // genuinely ambiguous (ADR-0108 records the accepted ambiguity).
            if transcript.runs[index]
                .started_at
                .is_some_and(|started| turn_start.div_euclid(1000) < started.div_euclid(1000))
            {
                tracing::debug!(
                    run = %transcript.runs[index].run_id,
                    completed_at,
                    turn_start,
                    started_at = transcript.runs[index].started_at,
                    "child evidence predates the run's start; keeping the run live"
                );
                return None;
            }
            let ending = match client_status {
                RunStatus::Failed => TaskRetirementEnding::ClientFailed,
                RunStatus::Cancelled => TaskRetirementEnding::ClientCancelled,
                _ => TaskRetirementEnding::ChildEnded,
            };
            (ending, Some(completed_at))
        }
        ChildEvidence::Gone => (TaskRetirementEnding::Lost, None),
        // No evidence: the run stays exactly as the client stated it.
        ChildEvidence::Unfinished => return None,
    };
    let run = transcript.runs.remove(index);
    // The launch panel is the record of the request, not the run: it settles
    // with the ending instead of reading live beside it. A shared launch waits
    // for its other children.
    let launch_calls: Vec<String> = run
        .launched_by
        .iter()
        .map(|launch| launch.call_id.clone())
        .collect();
    transcript.settle_run_launch_panels(&launch_calls, run.child_id.as_deref(), &ending);
    let retirement = TaskRetirement {
        task: BackgroundTask {
            // The ledger's rows stay `shell`/`subagent` (ADR-0105's
            // consequence): a child agent is a subagent run whatever producer
            // named it.
            tool: ToolIdentity {
                name: "subagent".to_string(),
                call_id: run.ending_key().to_string(),
            },
            shell_id: None,
            child_id: run.child_id.clone(),
            started_at: run.started_at,
        },
        ending,
        finished_at,
        label: Some(run.label.clone()).filter(|label| !label.trim().is_empty()),
    };
    transcript.unconfirmed_tasks.remove(run.ending_key());
    transcript.task_retirements.push(retirement.clone());
    Some(retirement)
}

/// The cleanup click's transition: a live Background Task named by the key
/// leaves with the runs it represents (one run, one ending); else a child-only
/// run any of whose ids the key names leaves on its own. A run a live task
/// represents is that task's to clear, never the run path's.
fn retire_cleanup(transcript: &mut SessionTranscript, key: &str, now_ms: i64) -> Option<TaskRetirement> {
    if let Some(index) = transcript
        .background_tasks
        .iter()
        .position(|task| task.tool.call_id == key)
    {
        let task = transcript.background_tasks.remove(index);
        let mut cleaned_ids: HashSet<String> = HashSet::new();
        cleaned_ids.insert(task.tool.call_id.clone());
        if let Some(child) = task.child_id.clone() {
            cleaned_ids.insert(child);
        }
        let mut represented: Vec<AgentRun> = Vec::new();
        transcript.runs.retain(|run| {
            let gone = run.ids().any(|id| cleaned_ids.contains(id))
                || run
                    .child_id
                    .as_ref()
                    .is_some_and(|child| cleaned_ids.contains(child));
            if gone {
                represented.push(run.clone());
            }
            !gone
        });
        for run in &represented {
            let launches: Vec<String> = run
                .launched_by
                .iter()
                .map(|launch| launch.call_id.clone())
                .collect();
            transcript.settle_run_launch_panels(
                &launches,
                run.child_id.as_deref(),
                &TaskRetirementEnding::Cleaned,
            );
        }
        let retirement = TaskRetirement {
            task,
            ending: TaskRetirementEnding::Cleaned,
            finished_at: Some(now_ms),
            label: None,
        };
        transcript.task_retirements.push(retirement.clone());
        return Some(retirement);
    }
    let index = transcript
        .runs
        .iter()
        .position(|run| run.ids().any(|id| id == key))?;
    // A run a live task represents leaves with its task, not on its own.
    if transcript
        .background_tasks
        .iter()
        .any(|task| transcript.run_is_task(&transcript.runs[index], task))
    {
        return None;
    }
    let run = transcript.runs.remove(index);
    let launches: Vec<String> = run
        .launched_by
        .iter()
        .map(|launch| launch.call_id.clone())
        .collect();
    transcript.settle_run_launch_panels(&launches, run.child_id.as_deref(), &TaskRetirementEnding::Cleaned);
    let retirement = TaskRetirement {
        task: BackgroundTask {
            tool: ToolIdentity {
                name: "subagent".to_string(),
                call_id: run.ending_key().to_string(),
            },
            shell_id: None,
            child_id: run.child_id.clone(),
            started_at: run.started_at,
        },
        ending: TaskRetirementEnding::Cleaned,
        finished_at: Some(now_ms),
        label: Some(run.label.clone()).filter(|label| !label.trim().is_empty()),
    };
    transcript.task_retirements.push(retirement.clone());
    Some(retirement)
}

/// The boundary between the current Turn and history: the Turn's own anchor when
/// the caller has one, else the newest user message (a caller with no Turn at
/// all — a Session Card read). `None` when neither exists, which leaves terminal
/// runs in scope.
fn run_cutoff(transcript: &SessionTranscript, anchor: Option<&TurnAnchor>) -> Option<i64> {
    anchor.map(|anchor| anchor.created_ms).or_else(|| {
        // The newest user message by server time — where history ends and the
        // newest Turn begins.
        transcript
            .messages
            .iter()
            .rev()
            .find(|message| message.role == crate::backend::MessageRole::User)
            .and_then(|message| message.time.as_ref().map(|time| time.created))
    })
}

/// The [`TaskRetirement`] an OWED durable ending owes the read (ADR-0080): the
/// entry the next card renders, rebuilt from the record's own identity — no
/// transcript launch needed. `None` for a class the read synthesizes from
/// somewhere else: a **Wake** ending renders from the Wake itself, and the
/// pre-#659 **legacy** class is the retirement's own dedup, never an entry. An
/// evidence ending whose evidence a record did not carry is not synthesized
/// either — a claim about the run with nothing to render.
fn synthesized_retirement(record: &TaskRunRecord, ending: &TaskEnding) -> Option<TaskRetirement> {
    let retirement_ending = match ending.class {
        TaskEndingClass::Evidence => match ending.evidence.as_ref()? {
            TaskEndingEvidence::Ended(status) => {
                TaskRetirementEnding::Ended(crate::backend::ShellEnd::from_status(status))
            }
            TaskEndingEvidence::Lost => TaskRetirementEnding::Lost,
            TaskEndingEvidence::ChildEnded => TaskRetirementEnding::ChildEnded,
            TaskEndingEvidence::Wake(_) => return None,
        },
        TaskEndingClass::Cleanup => TaskRetirementEnding::Cleaned,
        TaskEndingClass::Wake | TaskEndingClass::Legacy => return None,
    };
    Some(TaskRetirement {
        task: ending.identity.task(&record.run.key),
        ending: retirement_ending,
        finished_at: ending.finished_at,
        label: ending.identity.label.clone(),
    })
}
