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
/// [`RunLifecycle`](super::RunLifecycle) is the seam's one consumer and
/// `ChainRecords` its one implementation: the sidecar keeps the bytes, the
/// resolution order and the per-class dedup.
///
/// Two of the methods exist because ADR-0080's facts are the sidecar's and not
/// the owner's: [`RunEndingStore::observe_run`] is the read's resolve-and-fold
/// (one critical section, so a read's observation cannot race a writer's mint),
/// and [`RunEndingStore::record_retirement`] carries the
/// retirement → ending identity mapping, whose label is the ledger's rule. The
/// owner stays neutral and never names a card type.
pub(crate) trait RunEndingStore: Send + Sync {
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

    /// Observe the run `held` names for a read (ADR-0080): resolve it by the
    /// writers' own alias order and fold every id `held` carries into its record,
    /// in ONE critical section, so a writer holding only one of them resolves the
    /// one record instead of minting a second one for one run. `true` when the
    /// session's endings name the run; an observation never mints.
    fn observe_run(&self, session_id: &str, held: &RunKey) -> bool;

    /// Record the ending one derived [`TaskRetirement`] names — its class, its
    /// evidence, its clock and the identity its entry is re-synthesized from.
    /// The store owns the identity mapping, because the run's label is the
    /// ledger's rule. Returns whether the section changed; an already-recorded
    /// `(RunKey, generation, class)` answers `false`.
    ///
    /// [`TaskRetirement`]: crate::backend::TaskRetirement
    fn record_retirement(
        &self,
        session_id: &str,
        retirement: &crate::backend::TaskRetirement,
        input: Option<&serde_json::Value>,
    ) -> bool;
}
