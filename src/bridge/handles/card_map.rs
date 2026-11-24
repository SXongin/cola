//! The guarded map of live card sessions (ticket #1136).
//!
//! [`CardMap`] owns the raw `HashMap<String, CardSession>`; the field is
//! private to this module, so a caller outside reaches a session only through
//! the doors below. Field privacy is what `cargo check` enforces: every call
//! site moved. It does not by itself bound how long a guard is held — see
//! "The concurrency contract" below.
//!
//! # Doors
//!
//! - [`CardMap::card`] — the **per-session door**: one acquisition of the map
//!   mutex, mapped to that session's card, still locked. Almost every caller
//!   uses this: it takes one step against one session and drops the guard.
//! - [`CardMap::has`], [`CardMap::insert`], [`CardMap::remove`] — the
//!   per-session facts that need no guard at all.
//! - [`CardMap::with_map`] — the **privileged whole-map door**. It exists for
//!   the few facts that are about the map itself, and its callers are named in
//!   its documentation; a per-session caller cannot use it.
//!
//! # The concurrency contract
//!
//! The card-write order stays `write_lock` → `cards` → task state (see
//! [`CardsHandle`]'s docs for the full rule). `cards`, `sent_cards` and
//! `card_handles` are never held at the same time, and a `card()` guard is
//! taken for one step and released before any await on the platform.
//!
//! That one-step/no-await rule is the **caller's duty**, not something the
//! type enforces: a [`MappedMutexGuard`] returned by `card()` keeps the
//! underlying map mutex held for **every** session while it is alive, exactly
//! like the raw map guard it replaced. A caller that holds it across an await
//! blocks all sessions' map access for that span, even though it only reads or
//! mutates one. The ticket's concurrency tests pin both halves of the duty: a
//! parked write lock does not hold the map, and a held guard keeps every other
//! per-session acquisition out until it drops.

use std::collections::HashMap;
use std::sync::Arc;

use tokio::sync::{MappedMutexGuard, Mutex, MutexGuard};

use super::CardsHandle;
use crate::bridge::turn::CardSession;

/// The live card sessions, guarded by one map mutex.
///
/// Cloned freely: every clone is a handle to the same map. Construct one with
/// [`CardMap::new`]; the aggregate owns it and hands clones to the handles.
#[derive(Clone, Default)]
pub(crate) struct CardMap {
    /// session_id → the session's one live card. Private: [`Self::card`] and
    /// [`Self::with_map`] are the doors.
    live: Arc<Mutex<HashMap<String, CardSession>>>,
}

impl CardMap {
    /// An empty card map.
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// `session_id`'s card, still locked — the per-session door.
    ///
    /// One acquisition of the map mutex, mapped to the one session. The caller
    /// holds the guard for one step and drops it before any platform await:
    /// while it lives it keeps the map mutex — and so every session's map
    /// access — held, even though it only reaches the one session.
    ///
    /// `None` means the session has no card (not "the map is busy"): a closed
    /// session and a never-installed one read the same to a caller whose
    /// decision is "is there a card to act on".
    pub(crate) async fn card(&self, session_id: &str) -> Option<MappedMutexGuard<'_, CardSession>> {
        let live = self.live.lock().await;
        MutexGuard::try_map(live, |map| map.get_mut(session_id)).ok()
    }

    /// Whether `session_id` has a live card. The key-only probe: a terminal
    /// card stays in the map until the next Turn replaces it, so ask
    /// [`CardSession::is_terminal`] when the question is liveness rather than
    /// presence.
    pub(crate) async fn has(&self, session_id: &str) -> bool {
        self.live.lock().await.contains_key(session_id)
    }

    /// Install `card` as `session_id`'s live card, replacing any predecessor.
    ///
    /// Production paths that must keep a check and the insert in one critical
    /// section (the durable-announcement seed, the `/card` compare-and-swap)
    /// use [`Self::with_map`]; this is the plain door the test harness uses
    /// until its rewrite (#1137).
    #[allow(dead_code)] // used by the test harness, not the binary target
    pub(crate) async fn insert(&self, session_id: impl Into<String>, card: CardSession) {
        self.live.lock().await.insert(session_id.into(), card);
    }

    /// Take `session_id`'s card out of the map, returning it for a caller that
    /// must re-key or inspect it after the map no longer holds it.
    pub(crate) async fn remove(&self, session_id: &str) -> Option<CardSession> {
        self.live.lock().await.remove(session_id)
    }

    /// The privileged **whole-map** door: run `f` over the entire map inside
    /// one critical section.
    ///
    /// A per-session caller cannot use this — [`Self::card`] is its door — and
    /// must not: the guard it hands `f` is the map-wide one, so taking it for
    /// a single session would hold every session's writes behind one caller's
    /// work. Its consumers are:
    ///
    /// - `reconcile_staged_cursors` (`src/bridge/turn/flush.rs`) collects the
    ///   due staged cursors by walking every session;
    /// - `Turn::flush_owned_blocks`, `Turn::has_live_interaction` and
    ///   `Turn::has_receipt_prefix` (`src/bridge/turn/mod.rs`) answer their
    ///   whole-card questions over every session;
    /// - `Turn::has_interaction`, `Turn::vanished_block_sessions` and
    ///   `Turn::resolve_vanished` (`src/bridge/turn/interaction.rs`) — the
    ///   Interaction Block sweep, read-only and mutating;
    /// - the projection/wake installs (`src/bridge/turn/wake.rs`: the armed
    ///   projection's plan and install, `commit_external_render_locked`,
    ///   `arm_wake_continuation`, `attach_and_repoint`, `drop_armed_card` and
    ///   `drop_armed_session`) — the durable-announcement seed, the identity
    ///   check and the insert must be one critical section;
    /// - `Turn::recreate`, `Turn::record_idle_card`, `Turn::install_fresh_session`
    ///   and the orphan seed (`src/bridge/turn/mod.rs`): `Turn::recreate`
    ///   re-keys a card and its Background Task binding in ONE critical section
    ///   (ADR-0079, #681: the card and the task store must not be observed half
    ///   re-keyed), `record_idle_card` is a compare-and-swap, and the installs
    ///   seed before inserting;
    /// - the integration tests' whole-map peeks (`src/bridge/tests/viewer.rs`,
    ///   `src/bridge/tests/surfaces.rs`).
    ///
    /// `f` is synchronous by construction: nothing may await while the
    /// map-wide guard is held.
    pub(crate) async fn with_map<R>(&self, f: impl FnOnce(&mut HashMap<String, CardSession>) -> R) -> R {
        let mut live = self.live.lock().await;
        f(&mut live)
    }
}

impl CardsHandle {
    /// [`CardMap::card`] for the handle's own map.
    pub(crate) async fn card(&self, session_id: &str) -> Option<MappedMutexGuard<'_, CardSession>> {
        self.cards.card(session_id).await
    }

    /// [`CardMap::has`] for the handle's own map.
    pub(crate) async fn has(&self, session_id: &str) -> bool {
        self.cards.has(session_id).await
    }

    /// [`CardMap::insert`] for the handle's own map.
    #[allow(dead_code)] // used by the test harness, not the binary target
    pub(crate) async fn insert(&self, session_id: impl Into<String>, card: CardSession) {
        self.cards.insert(session_id, card).await;
    }

    /// [`CardMap::remove`] for the handle's own map.
    pub(crate) async fn remove(&self, session_id: &str) -> Option<CardSession> {
        self.cards.remove(session_id).await
    }

    /// [`CardMap::with_map`] for the handle's own map.
    pub(crate) async fn with_map<R>(&self, f: impl FnOnce(&mut HashMap<String, CardSession>) -> R) -> R {
        self.cards.with_map(f).await
    }
}
