//! The durable tier the run lifecycle writes through (ADR-0111 §2).
//!
//! The owner is neutral, so it cannot name the Chain Record: it asks this
//! narrow seam for the three facts it needs. [`crate::bridge::chain::ChainRecords`]
//! is the one implementation — the sidecar keeps the bytes, the resolution
//! order and the per-class dedup.

use super::{RunKey, TaskEnding, TaskRunRecord};

/// One Session's durable **Task Ending** facts (ADR-0080), as the run
/// lifecycle sees them.
///
/// Nothing calls this yet: the owner (`RunLifecycle`, #1169) is the seam's
/// consumer, and `ChainRecords` is its one implementation.
#[allow(dead_code)]
pub(crate) trait RunEndingStore {
    /// The newest ending clock the run's record carries, when a record exists:
    /// `Some(None)` is a record whose ending has no clock. A resumed run is
    /// contrasted against it, which is why this reads the clock rather than
    /// only "has an ending".
    fn ended_at(&self, session_id: &str, run: &RunKey) -> Option<(Option<i64>, Option<i64>)>;

    /// Record one ending, resolving its run first. The first derivation of a
    /// class is the fact: a class the run already carries is not recorded
    /// again, and re-deriving it can never clear its shown bit. Returns whether
    /// the section changed.
    fn record(&self, session_id: &str, run: &RunKey, ending: TaskEnding) -> bool;

    /// The Session's run records that still owe an entry — the ones a
    /// retirement may synthesize from.
    fn owed(&self, session_id: &str) -> Vec<TaskRunRecord>;
}
