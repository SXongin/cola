//! Test-only seams for the run lifecycle (ADR-0111 §2).
//!
//! The structure ratchet counts `#[cfg(test)]` attributes in a production file,
//! so a new module's test surface lives here, behind `#[cfg(test)] mod test_seam;`
//! in [`super`] — the guard treats a module its parent declares `#[cfg(test)]`
//! as test code.
//!
//! Nothing here is a production door: the owner's interface
//! ([`RunLifecycle::observe`], [`RunLifecycle::retire`],
//! [`RunLifecycle::evidence_waits`], [`RunLifecycle::live`]) is the only one.

use std::collections::HashSet;

use crate::backend::{ChildEvidence, SessionTranscript, TaskRuntime};

use super::{LiveRunSurface, RetireCause, RunLifecycle};

impl LiveRunSurface {
    /// The full surface: a caller that is not an adapter (a test driving the
    /// owner directly) observes everything.
    pub(crate) fn full() -> Self {
        Self {
            reports_live_runs: true,
            child_evidence: true,
        }
    }
}

impl RunLifecycle {
    /// Every id one session's overlay has recorded as retired, sorted — the test
    /// seam for "a refused refresh records nothing" (spec #588, review PR #595).
    /// The set holds EVERY id a run is known by, so it matches a row the read
    /// names by any of them.
    pub(crate) fn retired_ids(&self, session_id: &str) -> Vec<String> {
        self.sorted_ids(|state| state.retired.get(session_id))
    }

    /// The call ids one session's overlay carries as unconfirmed, sorted —
    /// [`Self::retired_ids`]'s marker-side twin.
    pub(crate) fn unconfirmed_call_ids(&self, session_id: &str) -> Vec<String> {
        self.sorted_ids(|state| state.unconfirmed.get(session_id))
    }

    fn sorted_ids(
        &self,
        pick: impl Fn(&super::lifecycle::OverlayState) -> Option<&HashSet<String>>,
    ) -> Vec<String> {
        let state = self.overlay();
        let mut ids: Vec<String> = pick(&state)
            .map(|ids| ids.iter().cloned().collect())
            .unwrap_or_default();
        ids.sort();
        ids
    }
}

/// The four derivation transitions as PURE transcript transforms, for the unit
/// tests that assert one derivation's effect on a read (the transcript, derive,
/// render and drain test modules). Production reaches the same bodies through
/// the backend's ONE owner — [`RunLifecycle::retire`], [`RunLifecycle::cleanup`]
/// and [`RunLifecycle::apply_task_runtime`]; these tests attach no durable tier,
/// so only the read's shape is under test.
///
/// Test-only on purpose: a second production door would be a second writer
/// (ADR-0111 §2), which is exactly what this landing removes.
pub(crate) trait TranscriptRunTransitions {
    fn apply_task_runtime(&mut self, runtime: &TaskRuntime);
    fn apply_cleanup(&mut self, call_ids: &[String], now_ms: i64);
    fn apply_child_evidence(&mut self, call_id: &str, evidence: ChildEvidence);
    fn apply_run_child_evidence(&mut self, run_id: &str, evidence: ChildEvidence) -> bool;
}

impl TranscriptRunTransitions for SessionTranscript {
    fn apply_task_runtime(&mut self, runtime: &TaskRuntime) {
        RunLifecycle::new().apply_task_runtime("", self, runtime);
    }

    fn apply_cleanup(&mut self, call_ids: &[String], now_ms: i64) {
        RunLifecycle::new().cleanup("", self, call_ids, now_ms);
    }

    fn apply_child_evidence(&mut self, call_id: &str, evidence: ChildEvidence) {
        RunLifecycle::new().retire("", self, call_id, RetireCause::ChildEvidence(evidence));
    }

    fn apply_run_child_evidence(&mut self, run_id: &str, evidence: ChildEvidence) -> bool {
        RunLifecycle::new()
            .retire("", self, run_id, RetireCause::RunChildEvidence(evidence))
            .is_some()
    }
}
