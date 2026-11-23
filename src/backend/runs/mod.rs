//! The run lifecycle's neutral home (ADR-0111).
//!
//! A **Background Task** run and a child-only **Agent Run** share one durable
//! identity ([`RunKey`]) and one **Task Ending** fact per class. This module
//! owns that vocabulary in the neutral read model, so the bridge's durable
//! store implements the seam instead of the backend reaching into it:
//!
//! - [`derive`] — the pure derivation of a read's [`AgentRun`]s
//!   (ADR-0105, spec #1057 P4: no vendor id is read here).
//! - [`key`] — [`RunKey`], the id set one run is known by, and the key its
//!   endings are recorded under.
//! - [`ending`] — the ending classes, entry state, evidence and the identity
//!   an entry is re-synthesized from.
//! - [`store`] — [`RunEndingStore`], the narrow durable seam the owner writes
//!   through; [`crate::bridge::chain::ChainRecords`] implements it.
//!
//! [`AgentRun`]: crate::backend::AgentRun

mod derive;
mod ending;
mod key;
mod store;

pub use derive::{child_run_activity, child_run_finished};
pub(crate) use derive::{derive_runs, status_of};
// `child_run_status` is the child's own statement, kept reachable for the
// owner landing that reads it separately from `child_run_finished` (#1169).
#[allow(unused_imports)]
pub use derive::child_run_status;
pub(crate) use ending::{
    TaskEnding, TaskEndingClass, TaskEndingEvidence, TaskEndingState, TaskRunIdentity, TaskRunRecord,
};
pub(crate) use key::RunKey;
pub(crate) use store::RunEndingStore;
