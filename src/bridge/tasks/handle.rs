//! The Background Task State store (ADR-0079, ticket #681).
//!
//! [`TasksHandle`] is the module's entry point and its per-Session store: it
//! maps a `(session, chain generation)` key to the [`TaskCell`] that
//! generation's live card owns — the generation **selects** the entry — and
//! reads the durable **Chain Record** a fresh cell is seeded from.
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
//! It is created beside the Chain Record it seeds from: it rides `SharedCore` /
//! `CardsHandle` next to `chains` (there is no `ChainRecords` field on
//! `FlowHandles`, so "beside `ChainRecords`" is where `ChainRecords` actually
//! lives).
//!
//! The lock order is recorded once, canonically, beside
//! `crate::bridge::handles::CardsHandle`'s lock doc. The store and cell mutexes
//! here are plain [`std::sync::Mutex`]es, taken and released inside each call —
//! never awaited, never held across an await; the Chain Record's own lock is
//! taken before the store lock, never after.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard};

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

/// The module's per-Session store and its handle to the durable tier. Cheap to
/// clone (the map and the Chain Record are both behind `Arc`s) and held beside
/// the Chain Record it reads.
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
    /// session-scoped `rendered` marks a fresh cell is seeded from.
    chains: Arc<ChainRecords>,
    /// session_id → chain generation → the cell that generation's live card
    /// owns. Nested rather than a flat `(String, u64)` key so the per-session
    /// pruning in [`Self::install`] is one `retain` on the session's own map,
    /// and the session → generation hierarchy reads as it is.
    store: Arc<Mutex<HashMap<String, HashMap<u64, TaskCell>>>>,
}

impl TasksHandle {
    /// Build the handle around the shared Chain Record.
    pub(crate) fn new(chains: Arc<ChainRecords>) -> Self {
        Self {
            chains,
            store: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Read the durable tier a fresh cell is born from: the Wake floor and the
    /// rendered marks. The Chain Record's own lock is a plain
    /// [`std::sync::Mutex`], taken and released inside this call.
    pub(crate) fn durable_seed(&self, session_id: &str) -> DurableSeed {
        DurableSeed {
            watermark_ms: self.chains.announced(session_id).map(|mark| mark.created_ms),
            rendered_keys: self.chains.rendered(session_id),
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
        let generation = self.chains.generation(session_id);
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
        let current = self.chains.generation(session_id);
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

    fn store(&self) -> MutexGuard<'_, HashMap<String, HashMap<u64, TaskCell>>> {
        self.store
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}
