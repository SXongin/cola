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
//! # What a switch that races a submit means
//!
//! The pre-flight holds the switch point across its snapshot and releases it
//! immediately before `Backend::prompt` (a submit has no total response bound).
//! On a generation whose selection is durable (V2) the **server** is the
//! authority: a switch that lands while the turn is being submitted is a user
//! action applied from the next turn, while the snapshot governs this turn's
//! variant. It is not a mirror/mirror disagreement — the switch writes the
//! server and the mirror inside the point, so those two never diverge.
//!
//! # Waits are bounded
//!
//! Every caller takes the point with a bound ([`SWITCH_SETTLE_BOUND`]), and the
//! work inside it is bounded too ([`SWITCH_RPC_BOUND`]): the durable switch RPC,
//! the session read that precedes it, and the `/think` catalog clear. A server
//! that stops answering fails the caller **by name** instead of holding the
//! point, and therefore every prompt queued behind it, forever.
//!
//! The registry holds **weak** references, so only sessions with a live gate
//! keep an entry; a dead entry is pruned when the next gate is created.

use std::collections::HashMap;
use std::sync::{Arc, Weak};
use std::time::Duration;

/// The **total** bound on one durable switch RPC (issue #910). The transport
/// only bounds the *connect*, so a server that accepts `switch_session_model`
/// and then stops answering would hold the session's switch point — and every
/// prompt queued behind it — for as long as the process runs. A switch is a
/// small, single-call operation; past this it failed by name and the point is
/// released. Test builds use a short value so the wedge itself is testable.
#[cfg(not(test))]
pub(crate) const SWITCH_RPC_BOUND: Duration = Duration::from_secs(10);
#[cfg(test)]
pub(crate) const SWITCH_RPC_BOUND: Duration = Duration::from_millis(600);

/// The hard backstop on a caller's wait for the switch point: the switch RPC
/// above is bounded, so this covers a point held by a pick's read + catalog +
/// switch and still lets a prompt refuse by name instead of waiting forever.
/// A prompt that hits it is **not** sent.
#[cfg(not(test))]
pub(crate) const SWITCH_SETTLE_BOUND: Duration = Duration::from_secs(30);
#[cfg(test)]
pub(crate) const SWITCH_SETTLE_BOUND: Duration = Duration::from_millis(1_000);

/// One session's serialization point, taken around every read-decide-write of
/// its model/provider mirror.
#[derive(Default)]
pub(crate) struct SessionGate {
    lock: tokio::sync::Mutex<()>,
    /// The session's **durable-switch point** (issue #910): a generation whose
    /// model selection lives on the session (V2) changes it with an RPC, and
    /// that RPC plus the mirror write must be one ordered step. Held across
    /// them so an older switch cannot land after a newer one while the mirror
    /// keeps the newer one (server ≠ mirror). It is deliberately **not** the
    /// state gate: settings reads and the next prompt's pre-flight never wait
    /// on a backend call.
    switch: tokio::sync::Mutex<()>,
    /// Callers queued on [`Self::lock`] (test-only): lets a test park a writer
    /// deterministically instead of sleeping.
    #[cfg(test)]
    waiting: std::sync::atomic::AtomicUsize,
    /// Callers queued on [`Self::switch`] (test-only).
    #[cfg(test)]
    switch_waiting: std::sync::atomic::AtomicUsize,
}

impl std::fmt::Debug for SessionGate {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.debug_struct("SessionGate").finish_non_exhaustive()
    }
}

/// Test-only: keeps a waiter count honest even when the acquire is cancelled
/// (a bounded acquire that times out), so a phantom waiter cannot make a later
/// test believe the point is still contended (issue #910).
#[cfg(test)]
struct WaitingGuard<'a>(&'a std::sync::atomic::AtomicUsize);

#[cfg(test)]
impl Drop for WaitingGuard<'_> {
    fn drop(&mut self) {
        self.0.fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
    }
}

impl SessionGate {
    /// Take the session's serialization point. Hold it across the read, the
    /// decision and the write; never across a catalog lookup or an RPC.
    pub(crate) async fn enter(&self) -> tokio::sync::MutexGuard<'_, ()> {
        #[cfg(test)]
        {
            self.waiting.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            // The guard decrements on **drop**, so a cancelled acquire (a
            // bounded `timeout`) leaves no phantom waiter behind.
            let _queued = WaitingGuard(&self.waiting);
            self.lock.lock().await
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

    /// Take the session's durable-switch point. Hold it across the model
    /// selection's RPC **and** the mirror write that follows; never across
    /// anything else. Lock order is switch → state gate, never the reverse.
    pub(crate) async fn enter_switch(&self) -> tokio::sync::MutexGuard<'_, ()> {
        #[cfg(test)]
        {
            self.switch_waiting
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let _queued = WaitingGuard(&self.switch_waiting);
            self.switch.lock().await
        }
        #[cfg(not(test))]
        {
            self.switch.lock().await
        }
    }

    /// How many callers are queued on the switch point (test-only).
    #[cfg(test)]
    pub(crate) fn switch_waiting(&self) -> usize {
        self.switch_waiting.load(std::sync::atomic::Ordering::SeqCst)
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

/// Take one session's switch point under `bound`, or refuse **by name** (issue
/// #910): the point can be held across a backend call (a durable switch, a
/// prompt submit), and every caller of it is a path a user is waiting on, so
/// none of them may wait forever.
pub(crate) async fn acquire_switch_bounded<'a>(
    gate: &'a SessionGate,
    bound: Duration,
) -> crate::error::Result<tokio::sync::MutexGuard<'a, ()>> {
    tokio::time::timeout(bound, gate.enter_switch())
        .await
        .map_err(|_| {
            crate::error::BridgeError::State(format!(
                "另一条模型/思考等级切换在 {bound:?} 内没有完成，这次操作没有生效；\
                 请稍后重试，或先 `/model` 确认当前模型。"
            ))
        })
}

impl std::fmt::Debug for SessionGates {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.debug_struct("SessionGates").finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Issue #910, items 1 and 3: every holder of the switch point is bounded
    /// now, so the pre-flight's acquire bound is the backstop for a future
    /// unbounded step. It refuses **by name**, and a cancelled acquire leaves
    /// no phantom waiter behind (the test-only counter is drop-guarded).
    #[tokio::test]
    async fn a_bounded_acquire_refuses_by_name_and_leaves_no_waiter() {
        let gates = SessionGates::default();
        let gate = gates.gate("ses_a");
        let held = gate.enter_switch().await;
        let started = std::time::Instant::now();
        let error = acquire_switch_bounded(&gate, Duration::from_millis(50))
            .await
            .expect_err("the point is held");
        assert!(started.elapsed() >= Duration::from_millis(50));
        assert!(error.to_string().contains("没有完成"), "{error}");
        assert_eq!(
            gate.switch_waiting(),
            0,
            "a cancelled acquire leaves no phantom waiter"
        );
        drop(held);
        assert!(
            acquire_switch_bounded(&gate, Duration::from_secs(1))
                .await
                .is_ok()
        );
    }

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
