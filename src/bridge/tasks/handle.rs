//! The Background Task State store (ADR-0079, ticket #681).
//!
//! [`TasksHandle`] is the module's entry point and its per-Session store: it
//! maps a session to the [`TaskCell`] its live card owns, recording the chain
//! **generation** the binding was made under, and reads the durable **Chain
//! Record** a fresh cell is seeded from.
//!
//! It is created beside the Chain Record it seeds from: it rides `SharedCore` /
//! `CardsHandle` next to `chains` (there is no `ChainRecords` field on
//! `FlowHandles`, so "beside `ChainRecords`" is where `ChainRecords` actually
//! lives).
//!
//! The lock order is recorded once, canonically, beside
//! `crate::bridge::handles::CardsHandle`'s lock doc. The store and cell mutexes
//! here are plain [`std::sync::Mutex`]es, taken and released inside each call —
//! never awaited, never held across an await.

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

/// One session's registered cell: a shallow handle to the live card's
/// [`TaskCell`] and the chain **generation** the binding was made under. The
/// generation is not the reset unit (the `CardSession` replacement is), but it
/// is what a lookup names, so a caller can tell whose chain the cell belongs
/// to.
struct SessionCell {
    generation: u64,
    state: TaskCell,
}

/// The module's per-Session store and its handle to the durable tier. Cheap to
/// clone (the map and the Chain Record are both behind `Arc`s) and held beside
/// the Chain Record it reads.
#[derive(Clone)]
pub(crate) struct TasksHandle {
    /// The durable Chain Record (ADR-0069): the Wake Watermark and the
    /// session-scoped `rendered` marks a fresh cell is seeded from.
    chains: Arc<ChainRecords>,
    /// session_id → the cell its live card owns, with the binding's generation.
    store: Arc<Mutex<HashMap<String, SessionCell>>>,
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
    /// #681), reached when a `CardSession` replaces the session's live card.
    /// The store keeps a shallow handle to the SAME cell, so the registered
    /// state is the live card's state — while the accumulator keeps its own
    /// handle, so a later generation bump can never orphan it.
    ///
    /// Returns the session's registered handle, resolved through the store —
    /// the store's read side, the same resolution a later slice's `apply` uses.
    pub(crate) fn install(&self, session_id: &str, generation: u64, cell: &TaskCell) -> TaskCell {
        let mut store = self.store();
        store.insert(
            session_id.to_string(),
            SessionCell {
                generation,
                state: cell.share(),
            },
        );
        // The read side: resolve the binding just made, by generation. It
        // cannot miss (the insert is under this same guard); the fallback keeps
        // the result total and is the caller's own cell in any case.
        store
            .get(session_id)
            .filter(|entry| entry.generation == generation)
            .map(|entry| entry.state.share())
            .unwrap_or_else(|| cell.share())
    }

    /// Rebind `session_id`'s cell and hand back the durable facts it is seeded
    /// with — the one call each of the four card-install sites makes. The bind
    /// ([`Self::install`]) resolves the session's cell through the store, so
    /// the facts come from [`Self::durable_seed`] read AFTER the binding; the
    /// caller seeds them through the accumulator's forwarding setters
    /// (`set_wake_floor` / `set_render_marks`), which reach the cell the store
    /// owns for that binding, not a side door.
    pub(crate) fn restore(&self, session_id: &str, generation: u64, cell: &TaskCell) -> DurableSeed {
        self.install(session_id, generation, cell);
        self.durable_seed(session_id)
    }

    fn store(&self) -> MutexGuard<'_, HashMap<String, SessionCell>> {
        self.store
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}
