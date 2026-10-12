//! The Background Task State handle (ADR-0079, ticket #681).
//!
//! [`TasksHandle`] is the module's entry point, created beside the durable
//! **Chain Record** it seeds from: it rides `SharedCore` / `CardsHandle` next
//! to `chains` (there is no `ChainRecords` field on `FlowHandles`, so "beside
//! `ChainRecords`" is where `ChainRecords` actually lives).
//!
//! The volatile per-Session cell ([`super::TaskState`]) is owned by value by
//! the session's `StreamAccumulator`, not by a registry here: that is what
//! keeps its lifetime (it dies with the `CardSession`), its reset point
//! (`Turn::install_fresh_session`) and its deep-copy on `Clone` byte-identical
//! to the inline fields it replaced. This handle is therefore the durable-tier
//! seam — the module's `restore(record, scope)`, which a later slice grows into
//! the read/apply interface.

use std::sync::Arc;

use crate::bridge::chain::ChainRecords;

/// The durable facts an earlier card of this Session already announced, read
/// from the Chain Record — ADR-0079's `restore(record, scope)`. The two stay
/// separate facts on purpose: the Wake floor gates Wakes, the render marks gate
/// retirements, and a retirement must never move the floor a late Wake is
/// judged against (#590).
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

/// The module's handle to a Session's Background Task facts: the durable seed
/// seam today. Cheap to clone (one shared `Arc`) and held beside the Chain
/// Record it reads.
#[derive(Clone)]
pub(crate) struct TasksHandle {
    /// The durable Chain Record (ADR-0069): the Wake Watermark and the
    /// session-scoped `rendered` marks this handle seeds a fresh cell from.
    chains: Arc<ChainRecords>,
}

impl TasksHandle {
    /// Build the handle around the shared Chain Record.
    pub(crate) fn new(chains: Arc<ChainRecords>) -> Self {
        Self { chains }
    }

    /// Read the durable tier a fresh per-Session cell is born from — ADR-0079's
    /// `restore(record, scope)`. The Chain Record's own lock is a plain
    /// [`std::sync::Mutex`], taken and released inside this call — never
    /// awaited, never held across an await, so it obeys the one-way
    /// `cards` → task-state order (ADR-0079).
    pub(crate) fn seed(&self, session_id: &str) -> DurableSeed {
        DurableSeed {
            watermark_ms: self.chains.announced(session_id).map(|mark| mark.created_ms),
            rendered_keys: self.chains.rendered(session_id),
        }
    }
}
