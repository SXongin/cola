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
//! # What the gate protects
//!
//! The same [`SessionGates::gate`] is held by:
//!
//! - `Turn::attempt`'s pre-flight, which decides and (when needed) applies the
//!   pair immediately before `Backend::prompt` — so a prompt is never submitted
//!   on a pair a switch is in the middle of changing;
//! - the confirmed thread-level switch (`App::run_provider_switch`), across the
//!   decision, the pair write (or the deferred record) and the
//!   release/resume/read-back — so a prompt for **that session** waits for the
//!   verdict instead of racing it;
//! - a pick's deferred-pair record (`App::defer_pair`).
//!
//! Every write to the mirror is a compare-and-set against the state the caller
//! read (`SessionStore::set_live_pair` / `set_pending_pair`): a newer `/model`,
//! `/think`, remap or materialisation makes the write a named no-op instead of
//! being overwritten. The catalog/variant lookup runs **outside** the gate
//! ("no async work under a lock others wait on"); only the re-read, the write
//! and the switch are inside it.
//!
//! It is deliberately **per session**: a switch in one chat never holds up
//! prompt admission in another, and it never holds the global turn-admission
//! set ([`super::core::SharedCore::inflight`]) — that set stays what it is, the
//! bookkeeping of which sessions have a turn, read briefly and never held across
//! an RPC.

use std::collections::HashMap;
use std::sync::Arc;

/// Cloneable handle to the per-session gates. One registry per process, owned by
/// [`super::core::SharedCore`] and cloned into the turn handles.
#[derive(Clone, Default)]
pub(crate) struct SessionGates {
    gates: Arc<std::sync::Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>>,
}

impl SessionGates {
    /// The session's gate. Await it to enter the session's serialization point,
    /// and hold it across every read-decide-write of that session's
    /// model/provider state.
    pub(crate) fn gate(&self, session_id: &str) -> Arc<tokio::sync::Mutex<()>> {
        let mut gates = self
            .gates
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        Arc::clone(gates.entry(session_id.to_string()).or_default())
    }
}

impl std::fmt::Debug for SessionGates {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.debug_struct("SessionGates").finish_non_exhaustive()
    }
}
