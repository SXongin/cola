//! Feishu's **Instant Reminder** (`time_sensitive`) lifecycle (ADR-0043).
//!
//! While a Permission or Question is pending, the conversation is pinned at
//! the top of the requester's message list; the moment the wait resolves the
//! pin is cleared. The pin is best-effort: every failure logs and the turn is
//! unaffected (a missing `im:datasync.feed_card.time_sensitive:write` scope
//! disables only pinning).
//!
//! Pins are generation-scoped: every pin records the **turn generation** that
//! owns it, and a clear from an older generation can never unpin a newer
//! turn's pin — so the long-turn threshold/TTL pin (ticket #236) can fire a
//! timer that captured its turn's generation without a stale timer ever
//! clearing a newer turn's pin. #236 plugs into the same [`PinState::ensure`] /
//! [`PinState::clear`] seam, with the generation it captured at turn start.
//!
//! State is in-memory and not reconciled at startup: a pin orphaned by a crash
//! or restart is not tracked, so the conversation's next turn issues one
//! best-effort clear for its requester (the self-heal in
//! [`PinState::begin_turn`]) and records that the conversation is known
//! unpinned — never a permanent pin.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use tokio::sync::Mutex;

use crate::bridge::core::SharedCore;
use crate::bridge::snapshot_claims::ClaimKind;
use crate::feishu::Platform;

/// What a pending request needs to pin its conversation: the Feishu chat, the
/// turn's requester (the pinned user) and the turn generation the pin belongs
/// to.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct PinTarget {
    pub(crate) chat_id: String,
    pub(crate) is_group: bool,
    /// The requester's open_id — Feishu requires at least one user id.
    pub(crate) user_ids: Vec<String>,
    pub(crate) generation: u64,
}

/// Why a conversation is currently pinned. The pending-request lifecycle owns
/// [`PinReason::Pending`]; the long-turn threshold/TTL lifecycle (ticket #236)
/// adds its own variant beside it, and the reminder is only actually cleared
/// once every owner has released it.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum PinReason {
    /// A Permission/Question is pending.
    Pending,
    /// A Turn has run past the long-turn threshold (ticket #236's timer —
    /// this ticket only carries the variant so the two owners compose).
    #[allow(dead_code)] // #236 wires the timer; the enum owns the seam now.
    LongTurn,
}

/// The reminder cola believes is currently ON at Feishu, the generation that
/// owns it, and the reasons currently holding it on.
#[derive(Clone)]
struct LivePin {
    generation: u64,
    is_group: bool,
    user_ids: Vec<String>,
    reasons: HashSet<PinReason>,
}

#[derive(Default)]
struct Inner {
    /// chat_id → the generation counter, bumped at every turn start.
    generations: HashMap<String, u64>,
    /// chat_id → the reminder currently ON at Feishu.
    live: HashMap<String, LivePin>,
    /// Conversations cola confirmed OFF in this process (a clear landed, or
    /// the startup-orphan self-heal ran) — never cleared again without a new
    /// pin.
    known_clear: HashSet<String>,
    /// chat_id → the request kinds with pending requests in that conversation.
    /// Both flows' sweeps update their own kind; the union decides pin/unpin.
    pending: HashMap<String, HashSet<ClaimKind>>,
}

/// The Instant Reminder state machine: `[bridge] pin` opt-in, generation
/// counters, the tracked live pin and the pending membership of both request
/// flows.
pub(crate) struct PinState {
    enabled: bool,
    inner: Mutex<Inner>,
}

impl PinState {
    pub(crate) fn new(enabled: bool) -> Self {
        Self {
            enabled,
            inner: Mutex::new(Inner::default()),
        }
    }

    /// Whether the `[bridge] pin` opt-in is on. Off means every method is a
    /// no-op: no reminder call is ever made.
    pub(crate) fn enabled(&self) -> bool {
        self.enabled
    }

    /// Register a new turn in `chat_id` and return its generation. Called at
    /// turn start, before any request of that turn can be surfaced.
    ///
    /// This is also the startup-orphan self-heal: a pin orphaned by a crash or
    /// restart is not reconciled at startup, so the conversation's next turn
    /// issues one best-effort clear for this turn's requester. Once that
    /// landed (`known_clear`), later turns do not repeat it.
    pub(crate) async fn begin_turn(
        &self,
        feishu: &Arc<dyn Platform>,
        chat_id: &str,
        is_group: bool,
        requester: Option<&str>,
    ) -> u64 {
        let mut inner = self.inner.lock().await;
        let generation = {
            let next = inner.generations.entry(chat_id.to_string()).or_insert(0);
            *next += 1;
            *next
        };
        if let Some(requester) = requester.filter(|open_id| !open_id.is_empty())
            && self.enabled
            && !inner.live.contains_key(chat_id)
            && !inner.known_clear.contains(chat_id)
        {
            let user_ids = vec![requester.to_string()];
            match feishu
                .set_instant_reminder(chat_id, is_group, &user_ids, false)
                .await
            {
                Ok(()) => {
                    inner.known_clear.insert(chat_id.to_string());
                    tracing::debug!("instant reminder: startup-orphan clear sent for {}", chat_id);
                }
                Err(e) => tracing::warn!(
                    "instant reminder: startup-orphan clear for {} failed (best-effort): {}",
                    chat_id,
                    e
                ),
            }
        }
        generation
    }

    /// Ensure the conversation is pinned towards `target` (a pending
    /// request's turn) for `reason`. Idempotent: while the same principal is
    /// pinned, another ensure only adopts the newer generation — no duplicate
    /// call.
    ///
    /// Test-only seam: production reconciles through [`Self::sync`] (which owns
    /// the lock across the whole decision); the long-turn threshold pin
    /// (ticket #236) decides atomically through its own entry point.
    #[cfg(test)]
    pub(crate) async fn ensure(&self, feishu: &Arc<dyn Platform>, target: &PinTarget, reason: PinReason) {
        if !self.enabled {
            return;
        }
        let mut inner = self.inner.lock().await;
        Self::ensure_locked(&mut inner, feishu, target, reason).await;
    }

    async fn ensure_locked(
        inner: &mut Inner,
        feishu: &Arc<dyn Platform>,
        target: &PinTarget,
        reason: PinReason,
    ) {
        if target.user_ids.is_empty() {
            return;
        }
        if let Some(live) = inner.live.get_mut(&target.chat_id) {
            // A newer turn already owns this conversation's reminder: a
            // delayed ensure from an older turn must neither downgrade the
            // generation (which would let that older turn's clear pass the
            // guard and unpin the newer pin) nor retarget it to another
            // requester. Equal or newer targets may proceed — a same-turn
            // requester change retargets, and a newer turn supersedes the
            // old pin.
            if live.generation > target.generation {
                return;
            }
            if live.is_group == target.is_group && live.user_ids == target.user_ids {
                // Already on for this principal: adopt the (never older)
                // generation so a stale clear (an older turn's timer, #236)
                // cannot unpin it, and add this reason beside any other
                // owner. No call — idempotent.
                live.generation = live.generation.max(target.generation);
                live.reasons.insert(reason);
                return;
            }
            // A different principal needs the pin now: release the old target
            // first (one `time_sensitive` value per call), then pin the new.
            let (old_is_group, old_users) = (live.is_group, live.user_ids.clone());
            if let Err(e) = feishu
                .set_instant_reminder(&target.chat_id, old_is_group, &old_users, false)
                .await
            {
                tracing::warn!(
                    "instant reminder: re-target clear for {} failed (best-effort): {}",
                    target.chat_id,
                    e
                );
            }
            inner.live.remove(&target.chat_id);
        }
        match feishu
            .set_instant_reminder(&target.chat_id, target.is_group, &target.user_ids, true)
            .await
        {
            Ok(()) => {
                inner.live.insert(
                    target.chat_id.clone(),
                    LivePin {
                        generation: target.generation,
                        is_group: target.is_group,
                        user_ids: target.user_ids.clone(),
                        reasons: HashSet::from([reason]),
                    },
                );
                inner.known_clear.remove(&target.chat_id);
            }
            // Best-effort: the turn is unaffected; a later sweep retries
            // while the request is still pending.
            Err(e) => tracing::warn!(
                "instant reminder: pin {} failed (best-effort; the turn is unaffected): {}",
                target.chat_id,
                e
            ),
        }
    }

    /// Release `reason`'s hold on `chat_id`'s reminder. The pin is only
    /// actually cleared once no reason holds it any more, and never when a
    /// strictly newer turn owns it (ADR-0043: a stale clear can never unpin a
    /// newer turn's pin). `generation` is the turn the caller believes owns
    /// the pin. Idempotent: nothing tracked means no call.
    ///
    /// The generation guard is #236's seam: a TTL timer passes the generation
    /// it captured at turn start, and a newer turn's pin survives a stale
    /// timer.
    #[allow(dead_code)] // #236's timer path; the sweeps use `sync`.
    pub(crate) async fn clear(
        &self,
        feishu: &Arc<dyn Platform>,
        chat_id: &str,
        generation: u64,
        reason: PinReason,
    ) {
        if !self.enabled {
            return;
        }
        let mut inner = self.inner.lock().await;
        Self::clear_locked(&mut inner, feishu, chat_id, generation, reason).await;
    }

    async fn clear_locked(
        inner: &mut Inner,
        feishu: &Arc<dyn Platform>,
        chat_id: &str,
        generation: u64,
        reason: PinReason,
    ) {
        let Some(live) = inner.live.get(chat_id).cloned() else {
            return;
        };
        // A strictly newer generation owns the pin: a stale clear (a timer
        // from an older turn) must not touch it.
        if live.generation > generation {
            return;
        }
        // Release this reason; only the last owner actually unpins.
        let mut reasons = live.reasons.clone();
        reasons.remove(&reason);
        if !reasons.is_empty() {
            if let Some(tracked) = inner.live.get_mut(chat_id) {
                tracked.reasons = reasons;
            }
            return;
        }
        match feishu
            .set_instant_reminder(chat_id, live.is_group, &live.user_ids, false)
            .await
        {
            Ok(()) => {
                inner.live.remove(chat_id);
                inner.known_clear.insert(chat_id.to_string());
            }
            // Keep the pin (and this reason) tracked so the next sweep
            // retries; the failure is logged and nothing else is affected.
            Err(e) => tracing::warn!(
                "instant reminder: clear {} failed (best-effort; kept for a later sweep): {}",
                chat_id,
                e
            ),
        }
    }

    /// Reconcile one flow's pending requests with the pin state: pin every
    /// conversation with pending work of either kind, clear every conversation
    /// whose work is gone. Both flows call this for their own kind; the union
    /// across them decides (a permission resolving never unpins a conversation
    /// that still has a question pending).
    pub(crate) async fn sync(
        &self,
        feishu: &Arc<dyn Platform>,
        kind: ClaimKind,
        targets: &HashMap<String, PinTarget>,
    ) {
        if !self.enabled {
            return;
        }
        let mut inner = self.inner.lock().await;
        // This flow's membership is exactly `targets`: a conversation it no
        // longer sees pending drops this kind's marker.
        inner.pending.retain(|_, kinds| {
            kinds.remove(&kind);
            !kinds.is_empty()
        });
        for chat_id in targets.keys() {
            inner.pending.entry(chat_id.clone()).or_default().insert(kind);
        }
        // Decide per conversation over the union of both flows.
        let mut chats: Vec<String> = inner.pending.keys().cloned().collect();
        for chat_id in inner.live.keys() {
            if !chats.contains(chat_id) {
                chats.push(chat_id.clone());
            }
        }
        for chat_id in chats {
            let pending = inner
                .pending
                .get(&chat_id)
                .map(|kinds| !kinds.is_empty())
                .unwrap_or(false);
            if pending {
                if let Some(target) = targets.get(&chat_id) {
                    Self::ensure_locked(&mut inner, feishu, target, PinReason::Pending).await;
                }
                // Pending in the OTHER flow only: that flow's own sync owns
                // the target; leave the tracked pin as it is.
            } else if let Some(live) = inner.live.get(&chat_id).cloned() {
                // Clear with the live pin's own generation: the generation
                // guard protects against stale timers, not against the flow
                // that owns the pin ending its wait.
                Self::clear_locked(&mut inner, feishu, &chat_id, live.generation, PinReason::Pending).await;
            }
        }
    }
}

/// Resolve a pending request's Instant Reminder target: walk the session's
/// parent chain (sub-task children carry their own id, ADR-0010) to the cola
/// turn's accumulator, which carries the turn generation, the requester and
/// the chat type; the session mapping supplies the Feishu chat id.
///
/// `None` when no cola turn registered one — an external turn, or a request
/// pending across a restart. Without a requester there is nothing to pin, so
/// pinning is skipped for that request (best-effort).
pub(crate) async fn reminder_target(
    core: &Arc<SharedCore>,
    session_id: &str,
    directory: &str,
) -> Option<PinTarget> {
    let (host, is_group, requester, generation) =
        crate::bridge::pollers::walk_parent_chain(core, session_id, Some(directory), |current| {
            let current = current.to_string();
            async move {
                let cards = core.cards.lock().await;
                cards.get(&current).and_then(|card| {
                    let generation = card.acc.turn_generation?;
                    let requester = card.acc.requester_open_id.clone()?;
                    if requester.is_empty() {
                        return None;
                    }
                    Some((current.clone(), card.acc.is_group, requester, generation))
                })
            }
        })
        .await?;
    let chat_id = {
        let sessions = core.sessions.lock().await;
        sessions.entry_for_session(&host)?.thread_key.chat_id.clone()
    };
    if chat_id.is_empty() {
        return None;
    }
    Some(PinTarget {
        chat_id,
        is_group,
        user_ids: vec![requester],
        generation,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bridge::test_support::RecordingPlatform;

    fn target_for(chat_id: &str, generation: u64, requester: &str) -> PinTarget {
        PinTarget {
            chat_id: chat_id.into(),
            is_group: false,
            user_ids: vec![requester.into()],
            generation,
        }
    }

    fn target(chat_id: &str, generation: u64) -> PinTarget {
        target_for(chat_id, generation, "ou_host")
    }

    /// (chat_id, on, user_ids) of every reminder call, for compact assertions.
    async fn reminder_calls(platform: &RecordingPlatform) -> Vec<(String, bool, Vec<String>)> {
        platform
            .reminders()
            .await
            .into_iter()
            .map(|(chat_id, _is_group, user_ids, on)| (chat_id, on, user_ids))
            .collect()
    }

    /// The core AC: a pin carries its turn's generation, so a clear from an
    /// older turn can never unpin the pin a newer turn owns (the seam #236's
    /// TTL timer uses).
    #[tokio::test]
    async fn a_stale_clear_never_unpins_a_newer_turns_pin() {
        let platform = Arc::new(RecordingPlatform::new());
        let feishu: Arc<dyn Platform> = platform.clone();
        let pins = PinState::new(true);

        pins.ensure(&feishu, &target("chat_1", 1), PinReason::Pending)
            .await;
        // Turn 2 begins while the wait is still pending: ensure adopts the
        // newer generation without a duplicate call.
        pins.ensure(&feishu, &target("chat_1", 2), PinReason::Pending)
            .await;
        assert_eq!(platform.reminders().await.len(), 1, "one pin, no duplicates");

        // A stale clear from turn 1 must not touch turn 2's pin.
        pins.clear(&feishu, "chat_1", 1, PinReason::Pending).await;
        assert_eq!(
            platform.reminders().await.len(),
            1,
            "a stale clear must not call the platform"
        );

        // The owning generation still clears.
        pins.clear(&feishu, "chat_1", 2, PinReason::Pending).await;
        let calls = reminder_calls(&platform).await;
        assert_eq!(calls.len(), 2);
        assert!(calls[0].1, "the pin call comes first");
        assert!(!calls[1].1, "the clear follows");
    }

    /// A delayed ensure from an older turn must not downgrade a newer turn's
    /// pin; only the newer turn's clear may release it.
    #[tokio::test]
    async fn a_stale_ensure_never_downgrades_a_newer_turns_pin() {
        let platform = Arc::new(RecordingPlatform::new());
        let feishu: Arc<dyn Platform> = platform.clone();
        let pins = PinState::new(true);

        // Turn 2 pins first (two turns of one chat can overlap: two topics of
        // one group each carry their own generation).
        pins.ensure(&feishu, &target("chat_1", 2), PinReason::Pending)
            .await;
        // A delayed sweep for turn 1 must not touch the pin...
        pins.ensure(&feishu, &target("chat_1", 1), PinReason::Pending)
            .await;
        assert_eq!(platform.reminders().await.len(), 1, "no duplicate, no retarget");

        // ...and must not have downgraded the generation, or its own stale
        // clear would pass the guard and unpin turn 2's pin.
        pins.clear(&feishu, "chat_1", 1, PinReason::Pending).await;
        assert_eq!(
            platform.reminders().await.len(),
            1,
            "a stale clear must not unpin the newer pin"
        );
        // Turn 2's own clear still releases it.
        pins.clear(&feishu, "chat_1", 2, PinReason::Pending).await;
        assert_eq!(platform.reminders().await.len(), 2);
    }

    /// A delayed ensure from an older turn with a different requester must
    /// leave a newer turn's pin and requester untouched; a same-generation
    /// requester change still retargets.
    #[tokio::test]
    async fn a_stale_retarget_leaves_a_newer_turns_pin_untouched() {
        let platform = Arc::new(RecordingPlatform::new());
        let feishu: Arc<dyn Platform> = platform.clone();
        let pins = PinState::new(true);

        pins.ensure(&feishu, &target_for("chat_1", 2, "ou_new"), PinReason::Pending)
            .await;
        pins.ensure(&feishu, &target_for("chat_1", 1, "ou_old"), PinReason::Pending)
            .await;

        let calls = reminder_calls(&platform).await;
        assert_eq!(calls.len(), 1, "the stale retarget must not clear or re-pin");
        assert!(calls[0].1, "the newer turn's pin is still on");
        assert_eq!(
            calls[0].2,
            vec!["ou_new".to_string()],
            "still the newer requester"
        );

        // A same-generation requester change is legitimate and retargets.
        pins.ensure(&feishu, &target_for("chat_1", 2, "ou_other"), PinReason::Pending)
            .await;
        let calls = reminder_calls(&platform).await;
        assert_eq!(
            calls.len(),
            3,
            "clear the old target, then pin the new: {calls:?}"
        );
        assert!(!calls[1].1 && calls[1].2 == vec!["ou_new".to_string()]);
        assert!(calls[2].1 && calls[2].2 == vec!["ou_other".to_string()]);
    }

    /// The reason seam: ending the pending wait does not release a pin
    /// another lifecycle still holds (ticket #236's long-turn pin rides
    /// beside it); the last owner releases the reminder.
    #[tokio::test]
    async fn a_pending_clear_keeps_a_pin_another_reason_holds() {
        let platform = Arc::new(RecordingPlatform::new());
        let feishu: Arc<dyn Platform> = platform.clone();
        let pins = PinState::new(true);

        pins.ensure(&feishu, &target("chat_1", 1), PinReason::LongTurn)
            .await;
        pins.ensure(&feishu, &target("chat_1", 1), PinReason::Pending)
            .await;
        pins.clear(&feishu, "chat_1", 1, PinReason::Pending).await;
        assert_eq!(
            platform.reminders().await.len(),
            1,
            "the long-turn pin stays on: the pending wait ending is not the last owner"
        );

        pins.clear(&feishu, "chat_1", 1, PinReason::LongTurn).await;
        let calls = platform.reminders().await;
        assert_eq!(calls.len(), 2);
        assert!(!calls[1].3, "the last owner releases the reminder");
    }

    /// The startup-orphan self-heal: the conversation's next turn clears a
    /// pin left by a crash/restart once, then stops re-clearing.
    #[tokio::test]
    async fn a_turn_start_clears_a_possible_startup_orphan_once() {
        let platform = Arc::new(RecordingPlatform::new());
        let feishu: Arc<dyn Platform> = platform.clone();
        let pins = PinState::new(true);

        assert_eq!(
            pins.begin_turn(&feishu, "chat_1", false, Some("ou_host")).await,
            1
        );
        assert_eq!(
            pins.begin_turn(&feishu, "chat_1", false, Some("ou_host")).await,
            2
        );

        let calls = reminder_calls(&platform).await;
        assert_eq!(calls.len(), 1, "one orphan sweep per conversation per process");
        assert_eq!(calls[0], ("chat_1".into(), false, vec!["ou_host".into()]));
    }

    /// The opt-in default: a disabled `PinState` never makes a reminder call.
    #[tokio::test]
    async fn a_disabled_pin_state_never_calls_the_platform() {
        let platform = Arc::new(RecordingPlatform::new());
        let feishu: Arc<dyn Platform> = platform.clone();
        let pins = PinState::new(false);

        pins.begin_turn(&feishu, "chat_1", false, Some("ou_host")).await;
        pins.ensure(&feishu, &target("chat_1", 1), PinReason::Pending)
            .await;
        pins.clear(&feishu, "chat_1", 1, PinReason::Pending).await;

        assert!(platform.reminders().await.is_empty());
    }
}
