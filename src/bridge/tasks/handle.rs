//! The Background Task State store's card-delivery half (ADR-0079, tickets
//! #681, #682).
//!
//! [`TasksHandle`] is the module's per-Session store of **Task Cells**: it maps
//! a `(session, chain generation)` key to the [`TaskCell`] that generation's
//! live card owns — the generation **selects** the entry — and reads the
//! durable **Chain Record** a fresh cell is seeded from.
//!
//! **The run half is not here.** The live/ended fact moved to
//! [`crate::backend::runs::RunLifecycle`] (ADR-0111 #1169): the process-local
//! `retired` cache, the unconfirmed-marker set, the durable-ending prune and the
//! owed synthesis all live with that owner. This handle keeps only card-chain
//! state — [`Self::install`], [`Self::cell`], [`Self::rekey`],
//! [`Self::durable_seed`] — and hands the run owner out through
//! [`Self::runs`], so the adapter and the Bridge still share ONE instance of
//! each.
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
//! threaded into the OpenCode adapter **at construction**
//! (`OpenCodeBackend::with_tasks`, ticket #682): the adapter holds it, applies
//! the run owner's [`crate::backend::runs::RunLifecycle::observe`] to **every**
//! transcript read at the pinned apply seam, while the Bridge reaches the SAME
//! handles through [`crate::backend::Backend::tasks`] and
//! [`crate::backend::Backend::runs`]. There is therefore exactly one owner of
//! the run lifecycle: a retirement the Bridge records is what every later read
//! sees.
//!
//! **Late-bound durable tier.** The handle is created BEFORE the Chain Record
//! (the adapter needs one at construction; `SharedCore::new` builds the Chain
//! Record), so the durable tier is attached afterwards with
//! [`Self::attach_chains`] — idempotent and replaceable, and it attaches to the
//! run owner in the same call. Until it is attached the durable seed is empty
//! and a binding's generation is `0`, exactly the recordless-session shape;
//! every production path attaches before it binds.
//!
//! The lock order is recorded once, canonically, beside
//! `crate::bridge::handles::CardsHandle`'s lock doc. The store, chain and
//! run-lifecycle mutexes here are plain [`std::sync::Mutex`]es, taken and
//! released inside each call — never awaited, never held across an await; the
//! Chain Record's own lock is taken before the store lock, never after.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard};

use crate::backend::runs::{RunEndingStore, RunLifecycle};
use crate::bridge::chain::ChainRecords;

use super::TaskCell;

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

/// The module's per-Session store of card-delivery cells, its handle to the
/// durable tier, and the run lifecycle owner created beside it. Cheap to clone
/// (the map, the Chain Record, the owner and its overlay are each behind an
/// `Arc`) and held beside the Chain Record it reads.
///
/// The store is keyed by `(session_id, chain generation)`: a lookup names a
/// session AND a generation and gets that chain's cell, so the generation
/// genuinely selects the entry. [`Self::install`] keeps exactly one live cell
/// per session by pruning the session's other generations, so a re-install
/// under a changed generation cannot leave the previous chain's cell behind.
/// [`Self::cell`] is the store's read side; [`Self::rekey`] is the binding's
/// session-id transition (`Turn::recreate`).
///
/// [`Self::runs`] is the ONE [`RunLifecycle`] (ADR-0111): the overlay, the
/// durable-ending prune and the retirement transitions live there, and the
/// adapter and the Bridge share this instance through
/// [`crate::backend::Backend::runs`].
#[derive(Clone)]
pub(crate) struct TasksHandle {
    /// The durable Chain Record (ADR-0069): the Wake Watermark a fresh cell is
    /// seeded from and the generation lookups a binding resolves against.
    /// Late-bound ([`Self::attach_chains`]) because the handle is created
    /// before the record; `None` until then, which reads as the
    /// recordless-session shape.
    chains: Arc<Mutex<Option<Arc<ChainRecords>>>>,
    /// session_id → chain generation → the cell that generation's live card
    /// owns. Nested rather than a flat `(String, u64)` key so the per-session
    /// pruning in [`Self::install`] is one `retain` on the session's own map,
    /// and the session → generation hierarchy reads as it is.
    store: Arc<Mutex<HashMap<String, HashMap<u64, TaskCell>>>>,
    /// The run lifecycle's one owner (ADR-0111). Created here because
    /// `src/main.rs` builds this handle before the Chain Record exists and
    /// threads it into the adapter; the two halves are separately reachable
    /// through the Backend, and share this instance.
    runs: RunLifecycle,
}

impl Default for TasksHandle {
    /// The handle with an unattached durable tier and nothing recorded — the
    /// shape [`Self::new`] builds, and the one the cell store's own unit tests
    /// construct.
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
            runs: RunLifecycle::new(),
        }
    }

    /// The run lifecycle's one owner (ADR-0111): the live/ended fact's writer.
    pub(crate) fn runs(&self) -> &RunLifecycle {
        &self.runs
    }

    /// Attach the durable Chain Record the handle's seed, generation lookups and
    /// bindings read — and, in the same call, the run owner's durable tier, so
    /// one attachment keeps both halves on one record. Idempotent and
    /// replaceable: called once by `SharedCore::new`, right after it builds the
    /// record, on the SAME handle the adapter was constructed with — every clone
    /// of the handle sees the attachment, since the slots are shared behind an
    /// `Arc`.
    pub(crate) fn attach_chains(&self, chains: Arc<ChainRecords>) {
        let mut slot = self.chains_slot();
        *slot = Some(Arc::clone(&chains));
        self.runs.attach_store(chains as Arc<dyn RunEndingStore>);
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

    /// The session's registered cell: the store's read side. `install` keeps a
    /// session at exactly ONE entry, but that entry is stamped with the
    /// generation the chain was on at BIND time while a Chain Record re-point
    /// bumps the chain's generation afterwards (the re-point follows the
    /// install) with no following install. So the read HEALS the binding: the
    /// session's single entry is re-stamped at the chain's CURRENT generation
    /// before it resolves, and a binding can never be read against a stale key.
    /// `None` when the session has no binding.
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
    /// binding, not a side door. The run owner's `retired` cache is seeded here
    /// too (ADR-0080/0111), from the same durable endings.
    pub(crate) fn restore(&self, session_id: &str, cell: &TaskCell) -> DurableSeed {
        self.install(session_id, cell);
        let seed = self.durable_seed(session_id);
        self.runs.seed_retired(session_id);
        seed
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
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::runs::{LiveRunSurface, RunKey, TaskEnding, TaskEndingClass, TaskRunIdentity};
    use crate::backend::{
        AgentRun, MessageId, MessageTime, Part, RunStatus, ToolCall, ToolCallRef, ToolIdentity,
        TranscriptMessage, Wake, WakeSource,
    };
    use crate::backend::{BackgroundTask, SessionTranscript};

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
        let run = RunKey::of_wake(&launchless_wake("sh_call_bg")).expect("the Wake names the run");
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

        let ids = handle.runs().retired_ids("ses_a");
        assert!(
            ids.contains(&"sh_call_bg".to_string()),
            "the shell id (the record's key) is seeded: {ids:?}"
        );
        assert!(
            ids.contains(&"msg_wake_1".to_string()),
            "every id the run is known by is seeded, not just the key: {ids:?}"
        );
    }

    /// A live `shell` row: the launch's call id, and (when the read named one)
    /// the shell it runs in.
    fn shell_task(call_id: &str, shell_id: Option<&str>) -> BackgroundTask {
        BackgroundTask {
            tool: crate::backend::ToolIdentity {
                name: "shell".into(),
                call_id: call_id.into(),
            },
            shell_id: shell_id.map(str::to_string),
            child_id: None,
            started_at: Some(2_000),
        }
    }

    /// The run a writer holds when it knows ONLY the launch's call id.
    fn only_call_id_run(call_id: &str) -> RunKey {
        RunKey {
            key: call_id.into(),
            call_id: Some(call_id.into()),
            shell_id: None,
            child_id: None,
            job_id: None,
            wake_id: None,
        }
    }

    /// The reviewer's interleaving (ADR-0080, CI gate): the read observes a row
    /// whose run's record is keyed by the SHELL id alone; a writer holding ONLY
    /// the row's later call id must resolve that record, never mint a second one.
    /// The observation and the fold are ONE critical section ([`ChainRecords::observe_run`]),
    /// so the record carries every id the read saw BEFORE any writer can act —
    /// and a SECOND row naming the run by that later id alone resolves the same
    /// record too, instead of surviving on a stale snapshot.
    #[test]
    fn a_read_observes_every_row_before_a_writer_can_mint() {
        let dir = tempfile::tempdir().unwrap();
        let chains = std::sync::Arc::new(ChainRecords::load(dir.path().join("chain_records.json")));
        // The launchless Wake's record: keyed by the shell id alone.
        let run = RunKey::of_wake(&launchless_wake("sh_call_bg")).expect("the Wake names the run");
        chains.record_ending(
            "ses_a",
            &run,
            TaskEnding::owed(TaskEndingClass::Wake, TaskRunIdentity::default()),
        );

        let handle = TasksHandle::new();
        handle.attach_chains(chains.clone());

        // The read carries the run's row twice: once named by the shell id the
        // record keys on (with the later call id the launch adds), and once by
        // that later call id ALONE — the id a concurrent writer holds.
        let mut transcript = SessionTranscript::new(vec![]).with_background_tasks(vec![
            shell_task("call_bg", Some("sh_call_bg")),
            shell_task("call_bg", None),
        ]);
        handle
            .runs()
            .observe("ses_a", &mut transcript, LiveRunSurface::full());
        assert!(
            transcript.background_tasks.is_empty(),
            "every row the read observed leaves the ledger: {transcript:?}"
        );

        // The concurrent writer holding ONLY the later call id resolves the one
        // record, never a second one for the run.
        chains.record_ending(
            "ses_a",
            &only_call_id_run("call_bg"),
            TaskEnding::owed(TaskEndingClass::Evidence, TaskRunIdentity::default()),
        );
        let records = chains.endings("ses_a");
        assert_eq!(records.len(), 1, "one record for the run: {records:?}");
        assert!(
            records[0].run.names("call_bg") && records[0].run.names("sh_call_bg"),
            "the record carries both ids the read observed: {:?}",
            records[0].run
        );
    }

    /// A run pruned by its durable ending settles the launch panel it came
    /// from: the card must not finish with a live panel beside the entry.
    #[test]
    fn a_pruned_run_settles_its_launch_panel() {
        let dir = tempfile::tempdir().unwrap();
        let chains = ChainRecords::load(dir.path().join("chain_records.json"));
        chains.record_ending(
            "ses_a",
            &RunKey {
                key: "ses_child".into(),
                call_id: Some("call_child".into()),
                shell_id: None,
                child_id: Some("ses_child".into()),
                job_id: None,
                wake_id: None,
            },
            TaskEnding::owed(TaskEndingClass::Evidence, TaskRunIdentity::default()),
        );
        let handle = TasksHandle::new();
        handle.attach_chains(std::sync::Arc::new(chains));
        let launch = ToolCall {
            identity: ToolIdentity {
                name: "spawnAgent".into(),
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
            ..ToolCall::default()
        };
        let mut transcript = SessionTranscript::new(vec![TranscriptMessage {
            id: MessageId::new("msg_spawn"),
            role: crate::backend::MessageRole::Assistant,
            time: Some(MessageTime {
                created: 1_000,
                completed: None,
            }),
            model: None,
            tokens: None,
            error: None,
            parts: vec![Part::Tool(launch)],
        }]);
        assert_eq!(transcript.runs.len(), 1, "the launch derives one run");
        handle
            .runs()
            .observe("ses_a", &mut transcript, LiveRunSurface::full());
        assert!(transcript.runs.is_empty(), "the durable ending prunes the run");
        let status = transcript
            .messages
            .iter()
            .flat_map(|message| &message.parts)
            .find_map(|part| match part {
                Part::Tool(call) if call.identity.call_id == "call_spawn" => Some(call.status.clone()),
                _ => None,
            })
            .expect("the launch call");
        assert_eq!(
            status,
            crate::backend::ToolStatus::Completed,
            "the panel settles with the pruned run"
        );
    }

    /// A childless run's Cleanup marker is keyed by its creating call. Once a
    /// later sighting fills the child id, the durable key moves: that marker no
    /// longer names the run's ending and must not make a live child read as
    /// unconfirmed.
    #[test]
    fn an_adopted_run_drops_its_childless_marker() {
        let handle = TasksHandle::new();
        handle
            .runs()
            .set_unconfirmed("ses_a", &["call_activity".to_string()]);
        let mut transcript = SessionTranscript::new(vec![]);
        transcript.runs = vec![AgentRun {
            run_id: "call_activity".into(),
            task_run_id: Some("ses_child".into()),
            child_id: Some("ses_child".into()),
            label: "review the diff".into(),
            agent: None,
            started_at: Some(1_000),
            status: RunStatus::Running,
            activity: None,
            observed_at: Some(1_000),
            launched_by: vec![ToolCallRef {
                call_id: "spawn_1".into(),
            }],
            addressed_by: Vec::new(),
            reported_by: Vec::new(),
        }];
        handle
            .runs()
            .observe("ses_a", &mut transcript, LiveRunSurface::full());
        assert!(
            transcript.unconfirmed_tasks.is_empty(),
            "the child-bearing run is evidence-owned now: {:?}",
            transcript.unconfirmed_tasks
        );
    }
}
