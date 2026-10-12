//! The Background Task liveness overlay (ADR-0079, ticket #682).
//!
//! The overlay's bytes live here now: they moved out of
//! `crate::backend::transcript` so the module owns them, while the OpenCode
//! adapter keeps holding one instance and applying it to **every** transcript
//! read — the apply point ADR-0079 pins in place, so the reap's settle, the
//! Snapshot status line and every other reader still see the corrected read
//! without calling anything.
//!
//! The Bridge reaches the SAME instance the adapter applies, through the
//! [`crate::backend::Backend::task_overlay`] accessor, and records through the
//! module's own API ([`BackgroundTaskOverlay::record`] /
//! [`BackgroundTaskOverlay::set_unconfirmed`]) — the two `Backend` methods that
//! used to carry those writes left the trait in #682.
//!
//! Like the rest of the module's state, the overlay is deliberately in-memory:
//! it is a cola-life fact, never durable state (ADR-0069's invariant), and its
//! lock is a plain synchronous mutex, taken and released inside each call.

use std::collections::{HashMap, HashSet};
use std::sync::Mutex;

use crate::backend::SessionTranscript;

/// The process-local overlay of the Background Task facts a **Runtime
/// Reconciliation** established this cola life (issue #454): the tasks the
/// runtime confirmed ended or the user cleared (spec #588, #590), and the live
/// tasks the runtime could not confirm as running (the `⚠️ 状态待确认`
/// markers). The transcript's launch record never flips, so without the first
/// every re-read would resurrect a task the runtime already retired — a live
/// turn's ledger would show it again, and the next Turn would yield waiting on
/// it, forever. And the marker state is derived per read
/// ([`SessionTranscript::apply_task_runtime`]) from a verdict the shared
/// throttle rations, so without the second any read the runtime was not asked
/// about — a Session Sync refresh, a drain tick, a follow — would drop the
/// marker (and the cleanup button it gates) with no evidence the child is
/// running (review, spec #588).
///
/// The Bridge records every retirement ([`Self::record`]) and replaces the
/// unconfirmed set ([`Self::set_unconfirmed`]) only after the flush that
/// carried a reconciliation read's entries was accepted — every caller's pass
/// commits then (review, PR #595), so a refused carrier records nothing and
/// the task stays live; the adapter applies the overlay to every transcript it
/// returns, so no read path can disagree. A marker is re-applied only to a task still live in
/// the read, so a retired task's marker never surfaces as live state. The
/// overlay is deliberately in-memory: a cola restart loses it, the next read
/// re-derives the same retirements and the next verdict the same markers, and
/// the runtime explains the task again. A manual cleanup is no different — it
/// is a cola-life dismissal, re-derived at restart.
#[derive(Default)]
pub(crate) struct BackgroundTaskOverlay {
    state: Mutex<OverlayState>,
}

#[derive(Default)]
struct OverlayState {
    /// session_id → the call ids the runtime (or a cleanup) retired.
    retired: HashMap<String, HashSet<String>>,
    /// session_id → the live call ids the runtime could not confirm as
    /// running.
    unconfirmed: HashMap<String, HashSet<String>>,
}

impl BackgroundTaskOverlay {
    /// Record the call ids the runtime retired for one session. Idempotent.
    pub(crate) fn record(&self, session_id: &str, call_ids: &[String]) {
        let mut state = self
            .state
            .lock()
            .expect("the task-overlay lock is never poisoned");
        let session = state.retired.entry(session_id.to_string()).or_default();
        for call_id in call_ids {
            session.insert(call_id.clone());
        }
    }

    /// Replace one session's unconfirmed-marker set with exactly `call_ids` —
    /// the reconcile's post-verdict set of live tasks it could not confirm as
    /// running. Replace semantics let a verdict clear a marker: a set without
    /// the id stops riding later reads, while a failed read never calls this
    /// and so clears nothing.
    pub(crate) fn set_unconfirmed(&self, session_id: &str, call_ids: &[String]) {
        let mut state = self
            .state
            .lock()
            .expect("the task-overlay lock is never poisoned");
        if call_ids.is_empty() {
            state.unconfirmed.remove(session_id);
            return;
        }
        state
            .unconfirmed
            .insert(session_id.to_string(), call_ids.iter().cloned().collect());
    }

    /// The call ids one session's overlay has recorded as retired, sorted —
    /// the test seam for "a refused refresh records nothing" (spec #588,
    /// review PR #595). Production reads the overlay only through
    /// [`Self::apply`].
    #[cfg(test)]
    pub(crate) fn retired_call_ids(&self, session_id: &str) -> Vec<String> {
        Self::sorted_ids(&self.state, |state| state.retired.get(session_id))
    }

    /// The call ids one session's overlay carries as unconfirmed, sorted —
    /// [`Self::retired_call_ids`]'s marker-side twin.
    #[cfg(test)]
    pub(crate) fn unconfirmed_call_ids(&self, session_id: &str) -> Vec<String> {
        Self::sorted_ids(&self.state, |state| state.unconfirmed.get(session_id))
    }

    /// A session's recorded id set, cloned and sorted for an order-free test
    /// assertion.
    #[cfg(test)]
    fn sorted_ids(
        state: &Mutex<OverlayState>,
        pick: impl Fn(&OverlayState) -> Option<&HashSet<String>>,
    ) -> Vec<String> {
        let state = state.lock().expect("the task-overlay lock is never poisoned");
        let mut ids: Vec<String> = pick(&state)
            .map(|ids| ids.iter().cloned().collect())
            .unwrap_or_default();
        ids.sort();
        ids
    }

    /// Apply the overlay to one read: every recorded retirement leaves
    /// `transcript`'s live list, and every recorded unconfirmed marker is
    /// re-inserted for a task still live in it. A session with nothing
    /// recorded is untouched (the common case does no set lookup per task).
    pub(crate) fn apply(&self, session_id: &str, transcript: &mut SessionTranscript) {
        let state = self
            .state
            .lock()
            .expect("the task-overlay lock is never poisoned");
        if let Some(ids) = state.retired.get(session_id) {
            transcript
                .background_tasks
                .retain(|task| !ids.contains(task.tool.call_id.as_str()));
        }
        let Some(ids) = state.unconfirmed.get(session_id) else {
            return;
        };
        for task in &transcript.background_tasks {
            if ids.contains(task.tool.call_id.as_str()) {
                transcript.unconfirmed_tasks.insert(task.tool.call_id.clone());
            }
        }
    }
}
