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
//! - [`lifecycle`] — [`RunLifecycle`], the live/ended fact's one writer
//!   (ADR-0111 §2): `observe`, `retire`, `evidence_waits`, `live`.
//!
//! [`AgentRun`]: crate::backend::AgentRun

mod derive;
mod ending;
mod key;
mod lifecycle;
mod store;
#[cfg(test)]
pub(crate) mod test_seam;

pub use derive::{child_run_activity, child_run_finished};
pub(crate) use derive::{derive_runs, status_of};
// `child_run_status` is the child's own statement, kept reachable for a caller
// that reads it separately from `child_run_finished` — the read surface's
// liveness projection (#1171 consumes it).
#[allow(unused_imports)]
pub use derive::child_run_status;
pub(crate) use ending::{
    TaskEnding, TaskEndingClass, TaskEndingEvidence, TaskEndingState, TaskRunIdentity, TaskRunRecord,
};
pub(crate) use key::RunKey;
pub(crate) use lifecycle::{LiveRunSurface, Pass, RetireCause, RunLifecycle};
pub(crate) use store::RunEndingStore;
