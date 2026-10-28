//! The per-session serialization point for a session's model/provider state
//! (issue #910).
//!
//! # The invariant
//!
//! > For one session, the model/provider pair the **next prompt** will run with
//! > is decided and durable **before that prompt is submitted**, and any
//! > thread-level switch (release → resume → read-back) either completes with
//! > read-back proof, defers to exactly that point, or fails by name with the
//! > previous state intact — under **one per-session serialization point**, with
//! > no app-server RPC held under a global lock.
//!
//! # The shape: record first, apply at pre-flight
//!
//! A confirmed provider change is *recorded* — the pair goes into the session
//! mirror — and applied by the next prompt's pre-flight (`Turn::attempt`), which
//! runs the three steps and reports the outcome. Nothing is applied from a card
//! callback, so this gate is only the **write** serialization point: it makes
//! every mirror write a read-decide-write that cannot interleave with another
//! writer (a `/model` pick, a `/think` pick, the recorded switch) or with the
//! pre-flight's read of the pair.
//!
//! The gate is deliberately **per session**: a write or a pre-flight in one chat
//! never holds up another, and it never holds the global turn-admission set
//! ([`super::core::SharedCore::inflight`]).
//!
//! The registry holds **weak** references, so only sessions with a live gate
//! keep an entry; a dead entry is pruned when the next gate is created.

use std::collections::HashMap;
use std::sync::{Arc, Weak};

/// One session's serialization point, taken around every read-decide-write of
/// its model/provider mirror.
#[derive(Default)]
pub(crate) struct SessionGate {
    lock: tokio::sync::Mutex<()>,
    /// Callers queued on [`Self::lock`] (test-only): lets a test park a writer
    /// deterministically instead of sleeping.
    #[cfg(test)]
    waiting: std::sync::atomic::AtomicUsize,
}

impl std::fmt::Debug for SessionGate {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.debug_struct("SessionGate").finish_non_exhaustive()
    }
}

impl SessionGate {
    /// Take the session's serialization point. Hold it across the read, the
    /// decision and the write; never across a catalog lookup or an RPC.
    pub(crate) async fn enter(&self) -> tokio::sync::MutexGuard<'_, ()> {
        #[cfg(test)]
        {
            self.waiting.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let guard = self.lock.lock().await;
            self.waiting.fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
            guard
        }
        #[cfg(not(test))]
        {
            self.lock.lock().await
        }
    }

    /// How many callers are queued on this gate (test-only): a test can wait
    /// until the writer it spawned is parked on the session's serialization
    /// point.
    #[cfg(test)]
    pub(crate) fn waiting(&self) -> usize {
        self.waiting.load(std::sync::atomic::Ordering::SeqCst)
    }
}

/// Cloneable handle to the per-session gates. One registry per process, owned by
/// [`super::core::SharedCore`] and cloned into the handles.
#[derive(Clone, Default)]
pub(crate) struct SessionGates {
    gates: Arc<std::sync::Mutex<HashMap<String, Weak<SessionGate>>>>,
}

impl SessionGates {
    /// The session's gate, created on first use. The registry lock is held only
    /// for the lookup/insert; the caller then awaits the per-session mutex.
    pub(crate) fn gate(&self, session_id: &str) -> Arc<SessionGate> {
        let mut gates = self
            .gates
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(gate) = gates.get(session_id).and_then(Weak::upgrade) {
            return gate;
        }
        // Only live gates are retained: a session with no write or pre-flight in
        // flight contributes nothing.
        gates.retain(|_, gate| gate.strong_count() > 0);
        let gate = Arc::new(SessionGate::default());
        gates.insert(session_id.to_string(), Arc::downgrade(&gate));
        gate
    }

    /// How many gate entries the registry retains (test-only: proves a dead gate
    /// is reclaimed).
    #[cfg(test)]
    pub(crate) fn tracked(&self) -> usize {
        let mut gates = self
            .gates
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        gates.retain(|_, gate| gate.strong_count() > 0);
        gates.len()
    }
}

impl std::fmt::Debug for SessionGates {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.debug_struct("SessionGates").finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The registry keeps only live gates: a session whose gate nobody holds is
    /// reclaimed by the next creation, so a long-lived process does not retain a
    /// mutex per session id it ever gated.
    #[test]
    fn a_dead_gate_is_reclaimed() {
        let gates = SessionGates::default();
        let first = gates.gate("ses_a");
        assert_eq!(gates.tracked(), 1);
        drop(first);
        let _second = gates.gate("ses_b");
        assert_eq!(gates.tracked(), 1, "the dead `ses_a` entry was pruned");
    }
}
