//! The Chain Record module (ADR-0069): one home for the durable per-Session
//! facts about a Card Chain.
//!
//! [`ChainRecords`] owns the sidecar — `chain_records.json`, two sections:
//! the live card record (ADR-0063) and the Wake Watermark (ADR-0061) — and
//! answers every reader through one seam. The restart reap that reconciles
//! those records against each Session's own reads lives beside them.

mod decision;
mod reconcile;
mod records;

pub(crate) use decision::{FreshDisposition, FreshReads, fresh};
pub(crate) use reconcile::{collect_orphan, collect_orphan_after_carry, reconcile};
pub(crate) use records::{
    ChainRecord, ChainRecords, CursorFrontier, CursorPartKind, RenderedCursor, release_spent,
};
