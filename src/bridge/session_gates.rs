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
//! # The one rule: intent first
//!
//! Every decision that will change the pair registers an **intent** under the
//! session gate *before its first await*. An intent is the pair it is about to
//! apply (or record) plus a generation. It makes the window between "I am about
//! to change the pair" and "the change is durable" impossible for a prompt to
//! observe:
//!
//! - `Turn::attempt` takes the gate, sees a live intent, and **waits** for its
//!   verdict before deciding the pair and submitting the prompt. It never
//!   submits the old pair while a switch is in flight.
//! - The switch (`App::run_provider_switch`) and a deferred-pair record
//!   (`App::defer_pair`) register their intent under the gate, release it for the
//!   catalog lookup ("no async work under a lock others wait on"), re-acquire,
//!   compare-and-set the mirror, and resolve the intent **before** releasing the
//!   gate — so a waiter always wakes to the final state.
//! - A `/model`/`/think` write (`SessionsHandle::pick_model`/`pick_think`) takes
//!   the same gate and re-reads before writing; a live intent makes it an honest
//!   "state changed" no-op instead of a stale overwrite.
//!
//! An intent that is never resolved (a cancelled task) is resolved by its drop
//! guard, so a warped switch cannot wedge the session's prompts forever.
//!
//! # What the gate protects
//!
//! The same [`SessionGates::gate`] is held by every read-decide-write of the
//! session's mirror, the deferred record and the switch state. It is deliberately
//! **per session**: a switch in one chat never holds up prompt admission in
//! another, and it never holds the global turn-admission set
//! ([`super::core::SharedCore::inflight`]) — that set stays the bookkeeping of
//! which sessions have a turn, read briefly and never held across an RPC.
//!
//! The registry holds **weak** references, so only sessions with a live gate
//! (a prompt in flight, a switch running, or a pick writing) keep an entry; a
//! dead entry is pruned when the next gate is created.

use std::collections::HashMap;
use std::sync::{Arc, Weak};

/// One session's serialization point.
#[derive(Default)]
pub(crate) struct SessionGate {
    /// The async mutex every read-decide-write of this session's model/provider
    /// state takes.
    lock: tokio::sync::Mutex<()>,
    /// The live intent, if any. A `std` mutex so reading it cannot await; only a
    /// holder of `lock` touches it (the drop guard is the one exception, and a
    /// generation check keeps it from clearing a newer intent).
    intent: std::sync::Mutex<Option<ModelIntent>>,
    /// Intent generations, so a stale resolver cannot clear a newer intent.
    next_generation: std::sync::atomic::AtomicU64,
}

/// The intent a switch or a deferred record registered under the gate.
struct ModelIntent {
    generation: u64,
    /// The pair the intent will apply (or record). Kept for diagnostics and so
    /// a future reader can tell *what* is in flight.
    #[allow(dead_code)]
    pair: String,
    /// Fires (once) when the intent resolves — applied, deferred or failed.
    finished: tokio::sync::watch::Sender<bool>,
}

impl std::fmt::Debug for SessionGate {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.debug_struct("SessionGate").finish_non_exhaustive()
    }
}

impl SessionGate {
    /// Take the session's serialization point. Hold it across the read, the
    /// decision and the write; never across a catalog lookup.
    pub(crate) async fn enter(&self) -> tokio::sync::MutexGuard<'_, ()> {
        self.lock.lock().await
    }

    /// Register the intent for `pair` (the caller holds [`Self::enter`]).
    /// Returns the intent's generation and the signal its waiters use.
    pub(crate) fn begin_intent(&self, pair: &str) -> (u64, tokio::sync::watch::Receiver<bool>) {
        let generation = self
            .next_generation
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst)
            + 1;
        let (finished, signal) = tokio::sync::watch::channel(false);
        *self
            .intent
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(ModelIntent {
            generation,
            pair: pair.to_string(),
            finished,
        });
        (generation, signal)
    }

    /// Resolve the intent of `generation` and wake its waiters. A newer intent
    /// is untouched. The normal path calls this while still holding the gate, so
    /// a woken prompt always re-reads the final state.
    pub(crate) fn resolve_intent(&self, generation: u64) {
        let mut intent = self
            .intent
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if !intent
            .as_ref()
            .is_some_and(|intent| intent.generation == generation)
        {
            return;
        }
        if let Some(intent) = intent.take() {
            // `watch` keeps the value, so a receiver created before this send
            // always observes the change (no lost wakeup).
            let _ = intent.finished.send(true);
        }
    }

    /// The signal a prompt must wait on before reading the pair (the caller
    /// holds the gate).
    pub(crate) fn intent_signal(&self) -> Option<tokio::sync::watch::Receiver<bool>> {
        self.intent
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .as_ref()
            .map(|intent| intent.finished.subscribe())
    }

    /// Whether a switch/deferred record is in flight (test-only introspection
    /// and the gated writers' honest refusal).
    pub(crate) fn has_intent(&self) -> bool {
        self.intent
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .is_some()
    }
}

/// Registers an intent and resolves it on every exit, including a cancelled
/// task: a switch that never finishes cannot wedge the session's prompts.
pub(crate) struct IntentGuard {
    gate: Arc<SessionGate>,
    generation: u64,
    resolved: bool,
}

impl IntentGuard {
    pub(crate) fn new(gate: Arc<SessionGate>, generation: u64) -> Self {
        Self {
            gate,
            generation,
            resolved: false,
        }
    }

    /// Resolve now (the normal path, under the gate). Idempotent.
    pub(crate) fn resolve(mut self) {
        self.resolved = true;
        self.gate.resolve_intent(self.generation);
    }
}

impl Drop for IntentGuard {
    fn drop(&mut self) {
        if !self.resolved {
            self.gate.resolve_intent(self.generation);
        }
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
        // Only live gates are retained: a session with no prompt, switch or pick
        // in flight contributes nothing.
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

    /// An intent resolves exactly once and only for its own generation: a stale
    /// resolver cannot clear a newer switch's intent.
    #[tokio::test]
    async fn only_the_owning_generation_resolves_an_intent() {
        let gate = SessionGate::default();
        let _held = gate.enter().await;
        let (generation, mut signal) = gate.begin_intent("deepseek/x");
        assert!(gate.has_intent());
        assert!(signal.has_changed().is_ok());
        // A stale generation is a no-op.
        gate.resolve_intent(generation + 7);
        assert!(gate.has_intent());
        gate.resolve_intent(generation);
        assert!(!gate.has_intent());
        // The sender was dropped by the resolve: the waiter returns.
        assert!(signal.changed().await.is_ok());
    }
}
