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
//!
//! Only observation seams live here. A test-only TRANSITION twin — the deleted
//! `apply_*` names, or a `*_for_test` forwarding shim — is banned
//! (`CODING_STANDARDS.md`, ADR-0110 §2): the tests drive the owner directly, so
//! the four retired paths cannot return under a test-only door.

use std::collections::HashSet;

use super::{LiveRunSurface, RunLifecycle};

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
