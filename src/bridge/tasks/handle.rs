//! The Background Task State store (ADR-0079, tickets #681, #682).
//!
//! [`TasksHandle`] is the module's entry point and its per-Session store: it
//! maps a `(session, chain generation)` key to the [`TaskCell`] that
//! generation's live card owns — the generation **selects** the entry — and
//! reads the durable **Chain Record** a fresh cell is seeded from. It also owns
//! the module's volatile liveness tier (#682): the process-local memoization of
//! the Background Task facts a **Runtime Reconciliation** established this cola
//! life — the tasks it confirmed ended or the user cleared, and the live tasks
//! it could not confirm as running.
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
//! [`crate::backend::Backend::tasks`] and records through [`Self::record`] /
//! [`Self::set_unconfirmed`]. There is therefore exactly one owner of the
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

use crate::backend::SessionTranscript;
use crate::bridge::chain::ChainRecords;

use super::TaskCell;

/// The durable facts an earlier card of this Session already announced, read
/// from the Chain Record — the durable half of ADR-0079's `restore(record,
/// scope)`. The two stay separate facts on purpose: the Wake floor gates Wakes,
/// the render marks gate retirements, and a retirement must never move the
/// floor a late Wake is judged against (#590).
pub(crate) struct DurableSeed {
    /// The Wake Watermark as a floor (spec #561, review #569; ADR-0061): every
    /// Wake at or below this `created_ms` was announced by an earlier card —
    /// the mark outlives the record it was announced on — so the fresh cell's
    /// render neither re-inserts its completion entry nor stages a watermark
    /// advance for it. A strictly newer Wake still renders and announces.
    /// `None` when the session announced no Wake.
    pub(crate) watermark_ms: Option<i64>,
    /// The synthetic retirement entries an earlier card really showed (issue
    /// #671, ticket #672), which the fresh cell never re-renders. Empty for a
    /// session that rendered no retirement.
    pub(crate) rendered_keys: Vec<String>,
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
    /// The durable Chain Record (ADR-0069): the Wake Watermark and the
    /// session-scoped `rendered` marks a fresh cell is seeded from. Late-bound
    /// ([`Self::attach_chains`]) because the handle is created before the
    /// record; `None` until then, which reads as the recordless-session shape.
    chains: Arc<Mutex<Option<Arc<ChainRecords>>>>,
    /// session_id → chain generation → the cell that generation's live card
    /// owns. Nested rather than a flat `(String, u64)` key so the per-session
    /// pruning in [`Self::install`] is one `retain` on the session's own map,
    /// and the session → generation hierarchy reads as it is.
    store: Arc<Mutex<HashMap<String, HashMap<u64, TaskCell>>>>,
    /// The process-local overlay of the Background Task facts a **Runtime
    /// Reconciliation** established this cola life (issue #454): the tasks the
    /// runtime confirmed ended or the user cleared (spec #588, #590), and the
    /// live tasks the runtime could not confirm as running (the `⚠️ 状态待确认`
    /// markers). The transcript's launch record never flips, so without the
    /// first every re-read would resurrect a task the runtime already retired
    /// — a live turn's ledger would show it again, and the next Turn would
    /// yield waiting on it, forever. And the marker state is derived per read
    /// ([`SessionTranscript::apply_task_runtime`]) from a verdict the shared
    /// throttle rations, so without the second any read the runtime was not
    /// asked about — a Session Sync refresh, a drain tick, a follow — would
    /// drop the marker (and the cleanup button it gates) with no evidence the
    /// child is running (review, spec #588).
    ///
    /// The Bridge records every retirement ([`Self::record`]) and replaces the
    /// unconfirmed set ([`Self::set_unconfirmed`]) only after the flush that
    /// carried a reconciliation read's entries was accepted — every caller's
    /// pass commits then (review, PR #595), so a refused carrier records
    /// nothing and the task stays live; the adapter applies the overlay to
    /// every transcript it returns ([`Self::apply`]), so no read path can
    /// disagree. A marker is re-applied only to a task still live in the read,
    /// so a retired task's marker never surfaces as live state. The overlay is
    /// deliberately in-memory: a cola restart loses it, the next read
    /// re-derives the same retirements and the next verdict the same markers,
    /// and the runtime explains the task again. A manual cleanup is no
    /// different — it is a cola-life dismissal, re-derived at restart.
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

    /// Read the durable tier a fresh cell is born from: the Wake floor and the
    /// rendered marks. The Chain Record's own lock is a plain
    /// [`std::sync::Mutex`], taken and released inside this call.
    pub(crate) fn durable_seed(&self, session_id: &str) -> DurableSeed {
        match self.chains() {
            Some(chains) => DurableSeed {
                watermark_ms: chains.announced(session_id).map(|mark| mark.created_ms),
                rendered_keys: chains.rendered(session_id),
            },
            None => DurableSeed {
                watermark_ms: None,
                rendered_keys: Vec::new(),
            },
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
    /// caller seeds them through the accumulator's forwarding setters
    /// (`set_wake_floor` / `set_render_marks`), which reach the cell the store
    /// owns for that binding, not a side door.
    pub(crate) fn restore(&self, session_id: &str, cell: &TaskCell) -> DurableSeed {
        self.install(session_id, cell);
        self.durable_seed(session_id)
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

    /// The call ids one session's liveness tier has recorded as retired, sorted
    /// — the test seam for "a refused refresh records nothing" (spec #588,
    /// review PR #595). Production reads the tier only through [`Self::apply`].
    #[cfg(test)]
    pub(crate) fn retired_call_ids(&self, session_id: &str) -> Vec<String> {
        self.sorted_ids(|state| state.retired.get(session_id))
    }

    /// The call ids one session's liveness tier carries as unconfirmed, sorted
    /// — [`Self::retired_call_ids`]'s marker-side twin.
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

    /// Apply the liveness tier to one read: every recorded retirement leaves
    /// `transcript`'s live list, and every recorded unconfirmed marker is
    /// re-inserted for a task still live in it. A session with nothing
    /// recorded is untouched (the common case does no set lookup per task).
    pub(crate) fn apply(&self, session_id: &str, transcript: &mut SessionTranscript) {
        let state = self.liveness();
        if let Some(ids) = state.retired.get(session_id) {
            transcript
                .background_tasks
                .retain(|task| !ids.contains(task.tool.call_id.as_str()));
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
