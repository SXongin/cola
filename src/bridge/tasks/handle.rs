//! The Background Task State store (ADR-0079, tickets #681, #682).
//!
//! [`TasksHandle`] is the module's entry point and its per-Session store: it
//! maps a `(session, chain generation)` key to the [`TaskCell`] that
//! generation's live card owns — the generation **selects** the entry — and
//! reads the durable **Chain Record** a fresh cell is seeded from. It also owns
//! the module's liveness tier (#682): the process-local **cache** of the durable
//! **Task Ending**s (ADR-0080) — the tasks a **Runtime Reconciliation**
//! confirmed ended or the user cleared, and the live tasks it could not confirm
//! as running. The endings themselves are durable, recorded at derivation;
//! [`Self::apply`] reads them directly and the `retired` cache is seeded from
//! them at every install, so the restart re-derives nothing and no later read
//! can resurrect a retired task.
//!
//! **Binding lifetime.** A binding is born at a card install ([`Self::install`],
//! through [`Self::restore`]) and dies with the session's next install (which
//! prunes it) or when the card moves to a fresh session (`Turn::recreate`,
//! [`Self::rekey`]). Because a Chain Record re-point *follows* the install that
//! preceded it, the generation a binding was stamped with can lag the chain's
//! current generation; the store therefore never resolves a binding against a
//! stale key — [`Self::cell`] re-stamps it at the generation the chain is on
//! when the read happens (see that method).
//!
//! **One instance, two holders.** The handle is created by `src/main.rs` and
//! threaded into `OpenCodeBackend` **at construction**
//! ([`crate::opencode::client::OpenCodeBackend::with_tasks`], ticket #682): the
//! adapter holds it and applies the liveness tier to **every** transcript read
//! at the pinned apply seam, while the Bridge reaches the SAME handle through
//! [`crate::backend::Backend::tasks`] and records through
//! [`Self::record_evidence_ending`] / [`Self::record_cleanup_ending`] (the
//! durable endings) and [`Self::record`] / [`Self::set_unconfirmed`] (the
//! cache). There is therefore exactly one owner of the
//! tier: a retirement the Bridge records is what every later read sees, and a
//! later caller holding the handle as the lifecycle owner sees the same
//! retirements the adapter holds, because the two are one handle. The module
//! owns the tier's bytes and its instance alike — the adapter no longer holds
//! a separate overlay.
//!
//! **Late-bound durable tier.** The handle is created BEFORE the Chain Record
//! (the adapter needs one at construction; `SharedCore::new` builds the Chain
//! Record), so the durable tier is attached afterwards with
//! [`Self::attach_chains`] — idempotent and replaceable. Until it is attached
//! the durable seed is empty and a binding's generation is `0`, exactly the
//! recordless-session shape; every production path attaches before it binds.
//!
//! The lock order is recorded once, canonically, beside
//! `crate::bridge::handles::CardsHandle`'s lock doc. The store, chain and
//! liveness mutexes here are plain [`std::sync::Mutex`]es, taken and released
//! inside each call — never awaited, never held across an await; the Chain
//! Record's own lock is taken before the store lock, never after.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex, MutexGuard};

use crate::backend::{BackgroundTask, SessionTranscript, TaskRetirement, TaskRetirementEnding};
use crate::bridge::chain::{
    ChainRecords, TaskEnding, TaskEndingClass, TaskEndingEvidence, TaskEndingState, TaskRunRecord,
    resolves_run,
};

use super::TaskCell;
use super::model::{TaskRunId, identity_of_task, identity_task};

/// The durable facts an earlier card of this Session already announced, read
/// from the Chain Record — the durable half of ADR-0079's `restore(record,
/// scope)`. Only the Wake floor is a cell fact: a retirement's exactly-once is
/// the ending's own durable shown bit (ADR-0080), read from the Chain Record
/// where it gates, and a retirement must never move the floor a late Wake is
/// judged against (#590).
pub(crate) struct DurableSeed {
    /// The Wake Watermark as a floor (spec #561, review #569; ADR-0061): every
    /// Wake at or below this `created_ms` was announced by an earlier card —
    /// the mark outlives the record it was announced on — so the fresh cell's
    /// render neither re-inserts its completion entry nor stages a watermark
    /// advance for it. A strictly newer Wake still renders and announces.
    /// `None` when the session announced no Wake.
    pub(crate) watermark_ms: Option<i64>,
}

/// The module's per-Session store, its handle to the durable tier and its
/// ownership of the volatile liveness tier. Cheap to clone (the map, the Chain
/// Record and the liveness map are each behind an `Arc`) and held beside the
/// Chain Record it reads.
///
/// The store is keyed by `(session_id, chain generation)`: a lookup names a
/// session AND a generation and gets that chain's cell, so the generation
/// genuinely selects the entry. [`Self::install`] keeps exactly one live cell
/// per session by pruning the session's other generations, so a re-install
/// under a changed generation cannot leave the previous chain's cell behind.
/// [`Self::cell`] is the store's read side; [`Self::rekey`] is the binding's
/// session-id transition (`Turn::recreate`).
#[derive(Clone)]
pub(crate) struct TasksHandle {
    /// The durable Chain Record (ADR-0069): the Wake Watermark a fresh cell is
    /// seeded from, and the durable Task Endings this tier's reads consult
    /// (ADR-0080). Late-bound ([`Self::attach_chains`]) because the handle is
    /// created before the record; `None` until then, which reads as the
    /// recordless-session shape.
    chains: Arc<Mutex<Option<Arc<ChainRecords>>>>,
    /// session_id → chain generation → the cell that generation's live card
    /// owns. Nested rather than a flat `(String, u64)` key so the per-session
    /// pruning in [`Self::install`] is one `retain` on the session's own map,
    /// and the session → generation hierarchy reads as it is.
    store: Arc<Mutex<HashMap<String, HashMap<u64, TaskCell>>>>,
    /// The process-local **cache** of the durable **Task Ending**s (ADR-0080):
    /// the tasks the runtime confirmed ended or the user cleared, and the live
    /// tasks the runtime could not confirm as running (the `⚠️ 状态待确认`
    /// markers). The facts themselves are now durable — the reconcile records
    /// an evidence ending the moment it derives it, and the cleanup click its
    /// own ([`Self::record_evidence_ending`] / [`Self::record_cleanup_ending`]),
    /// so the `retired` set is an optimization, not the source of truth: it is
    /// seeded from the session's durable endings at every install
    /// ([`Self::restore`]) and [`Self::apply`] consults the endings directly, so
    /// a restart re-derives nothing and no later read can resurrect a retired
    /// task. The transcript's launch record never flips, so without this tier
    /// every re-read would resurrect a task whose ending a failed write could
    /// not record — a live turn's ledger would show it again, and the next Turn
    /// would yield waiting on it, forever.
    ///
    /// The unconfirmed marker is NOT durable and keeps ADR-0073's accepted-write
    /// gate unchanged: it is derived per read
    /// ([`SessionTranscript::apply_task_runtime`]) from a verdict the shared
    /// throttle rations, so a read the runtime was not asked about — a Session
    /// Sync refresh, a drain tick, a follow — must keep the marker (and the
    /// cleanup button it gates) rather than drop it with no evidence the child
    /// is running (review, spec #588). The Bridge replaces it
    /// ([`Self::set_unconfirmed`]) only after the flush that carried a
    /// reconciliation read's entries was accepted, so a refused carrier records
    /// nothing.
    ///
    /// [`Self::apply`] applies the tier to every transcript the adapter returns,
    /// so no read path can disagree. A marker is re-applied only to a task still
    /// live in the read, so a retired task's marker never surfaces as live state.
    liveness: Arc<Mutex<OverlayState>>,
}

#[derive(Default)]
struct OverlayState {
    /// session_id → the call ids the runtime (or a cleanup) retired.
    retired: HashMap<String, HashSet<String>>,
    /// session_id → the live call ids the runtime could not confirm as
    /// running.
    unconfirmed: HashMap<String, HashSet<String>>,
}

impl Default for TasksHandle {
    /// The handle with an unattached durable tier and nothing recorded — the
    /// shape [`Self::new`] builds, and the one the liveness tier's own unit
    /// tests construct (`src/backend/transcript.rs`).
    fn default() -> Self {
        Self::new()
    }
}

impl TasksHandle {
    /// Build the handle. The durable tier is attached later
    /// ([`Self::attach_chains`]) — the handle exists before the Chain Record,
    /// and only the durable reads need it.
    pub(crate) fn new() -> Self {
        Self {
            chains: Arc::new(Mutex::new(None)),
            store: Arc::new(Mutex::new(HashMap::new())),
            liveness: Arc::new(Mutex::new(OverlayState::default())),
        }
    }

    /// Attach the durable Chain Record the handle's seed, generation lookups
    /// and bindings read. Idempotent and replaceable: called once by
    /// `SharedCore::new`, right after it builds the record, on the SAME handle
    /// the adapter was constructed with — every clone of the handle sees the
    /// attachment, since the slot is shared behind an `Arc`.
    pub(crate) fn attach_chains(&self, chains: Arc<ChainRecords>) {
        let mut slot = self.chains_slot();
        *slot = Some(chains);
    }

    /// The attached Chain Record, cloned out of its shared slot. `None` until
    /// [`Self::attach_chains`] runs; the durable reads degrade to the
    /// recordless-session shape until then.
    fn chains(&self) -> Option<Arc<ChainRecords>> {
        self.chains_slot().clone()
    }

    /// The chain's current generation, or `0` — the recordless-session shape —
    /// before the durable tier is attached.
    fn generation(&self, session_id: &str) -> u64 {
        self.chains().map_or(0, |chains| chains.generation(session_id))
    }

    /// Read the durable tier a fresh cell is born from: the Wake floor. The
    /// Chain Record's own lock is a plain [`std::sync::Mutex`], taken and
    /// released inside this call.
    pub(crate) fn durable_seed(&self, session_id: &str) -> DurableSeed {
        match self.chains() {
            Some(chains) => DurableSeed {
                watermark_ms: chains.announced(session_id).map(|mark| mark.created_ms),
            },
            None => DurableSeed { watermark_ms: None },
        }
    }

    /// Bind `session_id`'s cell: the reset/rebinding point (ADR-0079, ticket
    /// #681), reached when a `CardSession` replaces the session's live card. The
    /// store keeps a shallow handle to the SAME cell, so the registered state is
    /// the live card's state — while the accumulator keeps its own handle, so a
    /// later generation bump can never orphan it.
    ///
    /// The generation is resolved HERE, from the Chain Record, so a caller can
    /// never bind against a generation it read earlier. The new binding is keyed
    /// by `(session_id, generation)` and the session's OTHER generations are then
    /// pruned, so exactly one live cell per session remains. Write only: the
    /// store's read side is [`Self::cell`].
    pub(crate) fn install(&self, session_id: &str, cell: &TaskCell) {
        let generation = self.generation(session_id);
        let mut store = self.store();
        let generations = store.entry(session_id.to_string()).or_default();
        generations.insert(generation, cell.share());
        // Reset: the session's live card is THIS binding alone — drop whatever
        // earlier chains left behind, so the session never holds two cells.
        generations.retain(|bound, _| *bound == generation);
    }

    /// The session's registered cell: the store's read side, and the resolution
    /// a later slice's `apply` uses. `install` keeps a session at exactly ONE
    /// entry, but that entry is stamped with the generation the chain was on at
    /// BIND time while a Chain Record re-point bumps the chain's generation
    /// afterwards (the re-point follows the install) with no following install.
    /// So the read HEALS the binding: the session's single entry is re-stamped
    /// at the chain's CURRENT generation before it resolves, and a binding can
    /// never be read against a stale key. `None` when the session has no
    /// binding.
    pub(crate) fn cell(&self, session_id: &str) -> Option<TaskCell> {
        let current = self.generation(session_id);
        let mut store = self.store();
        let generations = store.get_mut(session_id)?;
        let held = generations.keys().next().copied();
        if let Some(cell) = held
            .filter(|held| *held != current)
            .and_then(|held| generations.remove(&held))
        {
            generations.insert(current, cell);
        }
        generations.get(&current).map(TaskCell::share)
    }

    /// Rebind `session_id`'s cell and hand back the durable facts it is seeded
    /// with — the one call each of the four card-install sites makes. The bind
    /// ([`Self::install`]) keys the cell at the chain's current generation, so
    /// the facts come from [`Self::durable_seed`] read AFTER the binding; the
    /// caller seeds it through the accumulator's forwarding setter
    /// (`set_wake_floor`), which reaches the cell the store owns for that
    /// binding, not a side door. The liveness tier's `retired`
    /// cache is seeded here too (ADR-0080), from the same durable endings.
    pub(crate) fn restore(&self, session_id: &str, cell: &TaskCell) -> DurableSeed {
        self.install(session_id, cell);
        let seed = self.durable_seed(session_id);
        self.seed_retired(session_id);
        seed
    }

    /// Seed the liveness tier's `retired` cache from the session's durable
    /// endings (ADR-0080): every run with a durable ending ended, whatever the
    /// read still lists, so a fresh install's reads carry the retirements the
    /// Chain Record already knows about without spending a reconcile verdict on
    /// them. EVERY id the run is known by is seeded — not just its stored key
    /// and call id — so the process-local fallback matches a row a read names by
    /// an alias (a shell/child id where the read's call id differs). Idempotent —
    /// [`Self::record`] folds the same ids.
    fn seed_retired(&self, session_id: &str) {
        let records = self.session_run_records(session_id);
        if records.is_empty() {
            return;
        }
        let mut state = self.liveness();
        let retired = state.retired.entry(session_id.to_string()).or_default();
        for record in &records {
            for id in record.run.ids() {
                retired.insert(id.to_string());
            }
        }
    }

    /// The session's durable run records, oldest first (ADR-0080) — the one
    /// read every ending consumer here goes through. Empty before the durable
    /// tier is attached (the recordless-session shape) and for a session that
    /// recorded no ending.
    fn session_run_records(&self, session_id: &str) -> Vec<TaskRunRecord> {
        self.chains()
            .map(|chains| chains.endings(session_id))
            .unwrap_or_default()
    }

    /// The binding's **session-id transition**: the live card moved from
    /// `from_session_id` to `to_session_id` (a `Turn::recreate` remap, whose
    /// Chain Record was renamed in the same step). READ the `from` session's
    /// binding out of the store — [`Self::cell`], the store's read side — and
    /// re-bind that cell under `to_session_id` at the target's current chain
    /// generation, pruning exactly as [`Self::install`] does; the `from`
    /// session's binding is then dropped, since the card went with it.
    pub(crate) fn rekey(&self, from_session_id: &str, to_session_id: &str) {
        let Some(cell) = self.cell(from_session_id) else {
            return;
        };
        self.install(to_session_id, &cell);
        let _ = self.store().remove(from_session_id);
    }

    /// Record the call ids the runtime retired for one session. Idempotent.
    pub(crate) fn record(&self, session_id: &str, call_ids: &[String]) {
        let mut state = self.liveness();
        let session = state.retired.entry(session_id.to_string()).or_default();
        for call_id in call_ids {
            session.insert(call_id.clone());
        }
    }

    /// Record the **Evidence** ending a reconcile pass just derived (ADR-0080):
    /// one synchronous Chain Record write at derivation, the fact about the run
    /// rather than the card that will show it — so a refused carrier, an
    /// entry-less chain and a restart all keep it. `input` is the launch's tool
    /// input, when the read still carries it, so the ending captures the label.
    /// A failed or timed-out read never reaches here; a run that already carries
    /// an evidence ending is not re-recorded.
    pub(crate) fn record_evidence_ending(
        &self,
        session_id: &str,
        retirement: &TaskRetirement,
        input: Option<&serde_json::Value>,
    ) -> bool {
        let evidence = match &retirement.ending {
            TaskRetirementEnding::Ended(end) => TaskEndingEvidence::Ended(end.as_str().to_string()),
            TaskRetirementEnding::Lost => TaskEndingEvidence::Lost,
            TaskRetirementEnding::ChildEnded => TaskEndingEvidence::ChildEnded,
            // Never derived by the reconcile; the cleanup click records its own.
            TaskRetirementEnding::Cleaned => return false,
        };
        self.record_ending(
            session_id,
            &TaskRunId::of_launch(&retirement.task),
            TaskEnding {
                class: TaskEndingClass::Evidence,
                state: TaskEndingState::Owed,
                finished_at: retirement.finished_at,
                evidence: Some(evidence),
                identity: identity_of_task(&retirement.task, input),
            },
        )
    }

    /// Record the **Cleanup** ending the user's click just derived for one
    /// cleared run (ADR-0080): durable the moment the click derives it — outside
    /// the refresh's accepted-write gate, because the fact is about the run, not
    /// the card that will show its 🧹 entry. A cleaned run's later Wake is its
    /// own ending (#590), so this never claims the run ended.
    pub(crate) fn record_cleanup_ending(
        &self,
        session_id: &str,
        task: &BackgroundTask,
        finished_at: i64,
        input: Option<&serde_json::Value>,
    ) -> bool {
        self.record_ending(
            session_id,
            &TaskRunId::of_launch(task),
            TaskEnding {
                class: TaskEndingClass::Cleanup,
                state: TaskEndingState::Owed,
                finished_at: Some(finished_at),
                evidence: None,
                identity: identity_of_task(task, input),
            },
        )
    }

    /// Record one **Task Ending** durably (ADR-0080). `false` when the durable
    /// tier is not attached (the recordless-session shape) or the run already
    /// carries this class.
    fn record_ending(&self, session_id: &str, run: &TaskRunId, ending: TaskEnding) -> bool {
        match self.chains() {
            Some(chains) => chains.record_ending(session_id, run, ending),
            None => false,
        }
    }

    /// Replace one session's unconfirmed-marker set with exactly `call_ids` —
    /// the reconcile's post-verdict set of live tasks it could not confirm as
    /// running. Replace semantics let a verdict clear a marker: a set without
    /// the id stops riding later reads, while a failed read never calls this
    /// and so clears nothing.
    pub(crate) fn set_unconfirmed(&self, session_id: &str, call_ids: &[String]) {
        let mut state = self.liveness();
        if call_ids.is_empty() {
            state.unconfirmed.remove(session_id);
            return;
        }
        state
            .unconfirmed
            .insert(session_id.to_string(), call_ids.iter().cloned().collect());
    }

    /// Every id one session's liveness tier has recorded as retired, sorted —
    /// the test seam for "a refused refresh records nothing" (spec #588, review
    /// PR #595). The set holds EVERY id a run is known by (its call id, its
    /// shell/child id, its Wake id), not just call ids, so it matches a row the
    /// read names by any of them. Production reads the tier only through
    /// [`Self::apply`].
    #[cfg(test)]
    pub(crate) fn retired_ids(&self, session_id: &str) -> Vec<String> {
        self.sorted_ids(|state| state.retired.get(session_id))
    }

    /// The call ids one session's liveness tier carries as unconfirmed, sorted
    /// — [`Self::retired_ids`]'s marker-side twin.
    #[cfg(test)]
    pub(crate) fn unconfirmed_call_ids(&self, session_id: &str) -> Vec<String> {
        self.sorted_ids(|state| state.unconfirmed.get(session_id))
    }

    /// A session's recorded id set, cloned and sorted for an order-free test
    /// assertion.
    #[cfg(test)]
    fn sorted_ids(&self, pick: impl Fn(&OverlayState) -> Option<&HashSet<String>>) -> Vec<String> {
        let state = self.liveness();
        let mut ids: Vec<String> = pick(&state)
            .map(|ids| ids.iter().cloned().collect())
            .unwrap_or_default();
        ids.sort();
        ids
    }

    /// Apply the liveness tier to one read (ADR-0080): every ending the
    /// session's Chain Record carries is a **Task Retirement** the read owes an
    /// entry — synthesized for the read even when it derives nothing (the
    /// restart case, a carrier that never took the entry) — every run with a
    /// durable ending leaves `transcript`'s live list whatever the read still
    /// lists, and every recorded unconfirmed marker is re-inserted for a task
    /// still live in it. A session with nothing recorded is untouched (the
    /// common case does no set lookup per task).
    ///
    /// The synthesis runs BEFORE the retention, over the same records, so an
    /// owed ending's entry is planned before its row leaves. Only an OWED
    /// ending is synthesized: a shown one already rendered, and a Wake ending
    /// renders from the Wake itself, never here.
    pub(crate) fn apply(&self, session_id: &str, transcript: &mut SessionTranscript) {
        let state = self.liveness();
        let records = self.session_run_records(session_id);
        for record in &records {
            for ending in record
                .endings
                .iter()
                .filter(|ending| ending.state == TaskEndingState::Owed)
            {
                let Some(retirement) = synthesized_retirement(record, ending) else {
                    continue;
                };
                // A read that derived this run's retirement itself already
                // carries it; the synthesis never doubles it.
                if transcript
                    .task_retirements
                    .iter()
                    .any(|known| known.task.tool.call_id == retirement.task.tool.call_id)
                {
                    continue;
                }
                transcript.task_retirements.push(retirement);
            }
        }
        // Every run with a durable ending has ended, whenever it was derived:
        // its row leaves the read, so a later reconcile cannot resurrect it and
        // the ledger cannot keep waiting on it. The match is ALIAS-AWARE
        // (ADR-0080): the record carries every id the run is known by, and the
        // row's own ids (its call id, its shell/child id) resolve the same
        // record — so a launchless Wake that keyed the record by a shell id
        // still prunes a row the read names by a different call id. RESOLVE,
        // THEN FOLD: the prune also hands the record the ids the row was named
        // by (one write, only when an id was learned), so a later writer holding
        // only that id resolves the one record instead of minting a second one
        // for one run. The process-local `retired` cache rides along for a run
        // whose ending write a restart lost; it too matches any id the row is
        // known by.
        let chains = self.chains();
        transcript.background_tasks.retain(|task| {
            let run = TaskRunId::of_launch(task);
            if !resolves_run(&records, &run) {
                return true;
            }
            if let Some(chains) = &chains {
                chains.fold_run_ids(session_id, &run);
            }
            false
        });
        if let Some(ids) = state.retired.get(session_id) {
            transcript.background_tasks.retain(|task| {
                let run = TaskRunId::of_launch(task);
                !run.ids().any(|id| ids.contains(id))
            });
        }
        let Some(ids) = state.unconfirmed.get(session_id) else {
            return;
        };
        for task in &transcript.background_tasks {
            if ids.contains(task.tool.call_id.as_str()) {
                transcript.unconfirmed_tasks.insert(task.tool.call_id.clone());
            }
        }
    }

    /// The Chain Record's shared slot, locked for the caller's one step.
    fn chains_slot(&self) -> MutexGuard<'_, Option<Arc<ChainRecords>>> {
        self.chains
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn store(&self) -> MutexGuard<'_, HashMap<String, HashMap<u64, TaskCell>>> {
        self.store
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn liveness(&self) -> MutexGuard<'_, OverlayState> {
        self.liveness
            .lock()
            .expect("the task-overlay lock is never poisoned")
    }
}

/// The [`TaskRetirement`] an OWED durable ending owes the read (ADR-0080): the
/// entry the next card renders, rebuilt from the record's own identity — no
/// transcript launch needed. `None` for a class the read synthesizes from
/// somewhere else: a **Wake** ending renders from the Wake itself, and the
/// pre-#659 **legacy** class is the retirement's own dedup, never an entry.
/// An evidence ending whose evidence a record did not carry is not synthesized
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
        task: identity_task(&ending.identity, &record.run.key),
        ending: retirement_ending,
        finished_at: ending.finished_at,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::{MessageId, Wake, WakeSource};
    use crate::bridge::chain::TaskRunIdentity;

    /// A Wake naming only the shell id it reports — the launchless shape a read
    /// whose launch left it yields (ADR-0080).
    fn launchless_wake(shell_id: &str) -> Wake {
        Wake {
            id: MessageId::new("msg_wake_1"),
            created_ms: Some(2_500),
            source: WakeSource::Shell,
            shell_id: Some(shell_id.to_string()),
            job_id: Some(shell_id.to_string()),
            child_id: None,
            state: Some("completed".into()),
            label: Some("gh run watch".into()),
        }
    }

    /// ADR-0080 (CI gate): the retired cache a restart seeds holds EVERY id the
    /// run is known by — not just its stored key and call id — so the
    /// process-local fallback still matches a row a later read names by an alias
    /// (the gate's row-pruning case, whose durable half is pinned by
    /// `bridge::tests::ledger::a_durable_ending_prunes_a_row_named_by_a_different_call_id`).
    #[test]
    fn the_retired_cache_is_seeded_with_every_id_the_run_is_known_by() {
        let dir = tempfile::tempdir().unwrap();
        let chains = ChainRecords::load(dir.path().join("chain_records.json"));
        let run = TaskRunId::of_wake(&launchless_wake("sh_call_bg")).expect("the Wake names the run");
        chains.record_ending(
            "ses_a",
            &run,
            TaskEnding::owed(TaskEndingClass::Wake, TaskRunIdentity::default()),
        );

        let handle = TasksHandle::new();
        handle.attach_chains(std::sync::Arc::new(chains));
        // The install a restart runs: it seeds the retired cache from the durable
        // endings.
        handle.restore("ses_a", &TaskCell::new());

        let ids = handle.retired_ids("ses_a");
        assert!(
            ids.contains(&"sh_call_bg".to_string()),
            "the shell id (the record's key) is seeded: {ids:?}"
        );
        assert!(
            ids.contains(&"msg_wake_1".to_string()),
            "every id the run is known by is seeded, not just the key: {ids:?}"
        );
    }
}
