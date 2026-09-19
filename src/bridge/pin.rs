//! Feishu's **Instant Reminder** (`time_sensitive`) lifecycle (ADR-0043).
//!
//! The Chat/Topic is pinned at the top of the requester's message list
//! while a Permission or Question is pending — and while a Turn runs past the
//! long-turn threshold. A resolved wait unpins immediately; a long Turn that
//! completes keeps its pin for the completion TTL, then unpins — unless a new
//! Turn starts first, whose [`PinState::begin_turn`] releases the TTL hold at
//! once (the user is active again; a Pending hold survives). The pin is
//! best-effort: every failure logs and the turn is unaffected (a missing
//! `im:datasync.feed_card.time_sensitive:write` scope disables only pinning).
//!
//! The long-turn threshold measures **user silence** (ADR-0043, amendment
//! 2026-09-19): the checker pins only once the Chat/Topic has seen no inbound
//! user activity (a message, a card action, or turn start) for the threshold,
//! and any interaction releases a live `LongTurn` hold and restarts the clock
//! — so renewed silence can pin again while the turn keeps running.
//!
//! Pins are generation-scoped: every pin records the **turn generation** that
//! owns it, and a clear from an older generation can never unpin a newer
//! turn's pin — so the long-turn TTL timer can never clear a newer turn's
//! pin. The long-turn lifecycle ([`PinState::check_long_turn`] /
//! [`PinState::complete_turn`]) is decided under the pin lock, so a
//! completion or an interaction racing a tick resolves exactly one way: no
//! double pin, no leaked pin, no pin after completion.
//!
//! State is in-memory and not reconciled at startup: a pin orphaned by a crash
//! or restart is not tracked, so the Chat/Topic's next turn issues one
//! best-effort clear for its requester (the self-heal in
//! [`PinState::begin_turn`]) and records that the Chat/Topic is known
//! unpinned — never a permanent pin.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use tokio::sync::Mutex;

use crate::bridge::core::SharedCore;
use crate::bridge::snapshot_claims::ClaimKind;
use crate::feishu::Platform;

/// What a pending request needs to pin its Chat/Topic: the Feishu chat, the
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

/// Why a Chat/Topic is currently pinned. The pending-request lifecycle owns
/// [`PinReason::Pending`]; the long-turn threshold/TTL lifecycle owns
/// [`PinReason::LongTurn`]. The reminder is only actually cleared once every
/// owner has released it.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum PinReason {
    /// A Permission/Question is pending.
    Pending,
    /// A Turn has run past the long-turn threshold (still running, or within
    /// the completion TTL after it finished).
    LongTurn,
}

/// A Turn running past the silence threshold pins the Chat/Topic (ADR-0043).
const DEFAULT_LONG_TURN_MS: u64 = 60_000;

/// How often the long-turn checker re-evaluates the silence clock (ADR-0043).
const DEFAULT_LONG_TURN_TICK_MS: u64 = 5_000;

/// A completed long Turn's pin stays up this long, then clears.
const DEFAULT_TTL_MS: u64 = 120_000;

/// The reminder cola believes is currently ON at Feishu, the generation that
/// owns it, and the reasons currently holding it on.
#[derive(Clone)]
struct LivePin {
    generation: u64,
    is_group: bool,
    user_ids: Vec<String>,
    reasons: HashSet<PinReason>,
}

/// A Chat/Topic's newest Turn's long-turn lifecycle (ADR-0043). The
/// threshold timer and the turn's completion both read it under the pin lock:
/// whichever runs first decides — a completion before the timer makes the
/// timer a no-op, a completion after it sees the live pin and arms the TTL.
#[derive(Clone, Copy)]
struct LongTurn {
    generation: u64,
    completed: bool,
}

#[derive(Default)]
struct Inner {
    /// chat_id → the generation counter, bumped at every turn start.
    generations: HashMap<String, u64>,
    /// chat_id → the reminder currently ON at Feishu.
    live: HashMap<String, LivePin>,
    /// Chats/Topics whose Feishu reminder cola confirmed OFF in this process
    /// (a clear landed, or the startup-orphan self-heal ran) — never cleared
    /// again without a new pin.
    confirmed_off: HashSet<String>,
    /// chat_id → the request kinds with pending requests in that Chat/Topic.
    /// Both flows' sweeps update their own kind; the union decides pin/unpin.
    pending: HashMap<String, HashSet<ClaimKind>>,
    /// chat_id → the newest Turn's long-turn lifecycle (its generation and
    /// whether it has finished).
    long_turns: HashMap<String, LongTurn>,
    /// chat_id → when the last inbound user activity was seen (a message, a
    /// card action, or turn start). The long-turn checker measures its
    /// silence threshold from here; interactions outside Feishu never touch
    /// it.
    last_interaction: HashMap<String, tokio::time::Instant>,
}

/// The Instant Reminder state machine: `[bridge] instant_reminder` opt-in, generation
/// counters, the tracked live pin, the pending membership of both request
/// flows, and the long-turn silence/TTL lifecycle.
pub(crate) struct PinState {
    enabled: bool,
    /// The long-turn silence threshold (ms): a Turn whose Chat/Topic has seen
    /// no user activity for this long pins it. A field, not a constant, so
    /// tests inject a tiny value and run the whole lifecycle in milliseconds
    /// (the external poller's interval-atomics pattern).
    pub(crate) long_turn_ms: AtomicU64,
    /// The checker's tick (ms): how often the silence clock is re-evaluated.
    /// Injectable for tests, same pattern.
    pub(crate) long_turn_tick_ms: AtomicU64,
    /// The completion TTL (ms): after a long Turn completes, its pin stays up
    /// this long before clearing. Injectable for tests, same pattern.
    pub(crate) ttl_ms: AtomicU64,
    inner: Mutex<Inner>,
}

impl PinState {
    pub(crate) fn new(enabled: bool) -> Self {
        Self {
            enabled,
            long_turn_ms: AtomicU64::new(DEFAULT_LONG_TURN_MS),
            long_turn_tick_ms: AtomicU64::new(DEFAULT_LONG_TURN_TICK_MS),
            ttl_ms: AtomicU64::new(DEFAULT_TTL_MS),
            inner: Mutex::new(Inner::default()),
        }
    }

    /// Whether the `[bridge] instant_reminder` opt-in is on. Off means every method is a
    /// no-op: no reminder call is ever made.
    pub(crate) fn enabled(&self) -> bool {
        self.enabled
    }

    /// Register a new turn in `chat_id` and return its generation. Called at
    /// turn start, before any request of that turn can be surfaced. This also
    /// arms the long-turn lifecycle: an older turn's checker is superseded
    /// here, a completion from an older turn can no longer schedule a TTL,
    /// and — turn start being user activity — the silence clock restarts and
    /// a live long-turn hold is released (a Pending hold survives).
    ///
    /// This is also the startup-orphan self-heal: a pin orphaned by a crash or
    /// restart is not reconciled at startup, so the Chat/Topic's next turn
    /// issues one best-effort clear for this turn's requester. Once that
    /// landed (`confirmed_off`), later turns do not repeat it.
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
        inner.long_turns.insert(
            chat_id.to_string(),
            LongTurn {
                generation,
                completed: false,
            },
        );
        // A new Turn means the user is active again: the same path every other
        // interaction takes — restart the silence clock and release a live
        // long-turn hold (the previous turn's completion TTL, or an earlier
        // hold). Best-effort and awaited under the lock, like the
        // startup-orphan clear below.
        if self.enabled {
            Self::note_interaction_locked(&mut inner, feishu, chat_id).await;
        }
        if let Some(requester) = requester.filter(|open_id| !open_id.is_empty())
            && self.enabled
            && !inner.live.contains_key(chat_id)
            && !inner.confirmed_off.contains(chat_id)
        {
            let user_ids = vec![requester.to_string()];
            match feishu
                .set_instant_reminder(chat_id, is_group, &user_ids, false)
                .await
            {
                Ok(()) => {
                    inner.confirmed_off.insert(chat_id.to_string());
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

    /// One tick of a Turn's long-turn checker (spawned by
    /// [`spawn_long_turn_checker`]): pin the Chat/Topic once `target`'s turn
    /// is still the newest, not completed, and the Chat/Topic has been silent
    /// for the injected threshold. Returns `false` once the checker must stop
    /// — the turn completed, or a newer turn superseded its generation.
    ///
    /// The stop conditions, the silence clock and the pin are decided under
    /// the pin lock (the same lock as [`Self::complete_turn`] and
    /// [`Self::note_interaction`]), so a completion or interaction racing a
    /// tick resolves exactly one way: no pin after completion, no pin
    /// against a stale interaction.
    pub(crate) async fn check_long_turn(&self, feishu: &Arc<dyn Platform>, target: &PinTarget) -> bool {
        if !self.enabled {
            return false;
        }
        let mut inner = self.inner.lock().await;
        let (generation, completed) = match inner.long_turns.get(&target.chat_id) {
            Some(turn) => (turn.generation, turn.completed),
            None => return false,
        };
        if generation != target.generation || completed {
            return false;
        }
        let threshold = Duration::from_millis(self.long_turn_ms.load(Ordering::Relaxed));
        let silent = inner
            .last_interaction
            .get(&target.chat_id)
            .is_some_and(|at| at.elapsed() >= threshold);
        if silent {
            Self::ensure_locked(&mut inner, feishu, target, PinReason::LongTurn).await;
        }
        true
    }

    /// Note inbound user activity in `chat_id` (ADR-0043): restart the
    /// long-turn silence clock and release a live `LongTurn` hold — the user
    /// is active again, so the threshold is measured from this moment. A
    /// `Pending` hold survives (`clear_locked` removes only the named reason),
    /// and the release passes the live pin's own generation, so the
    /// newer-generation guard does not block it. Best-effort, like every other
    /// pin call; nothing is recorded while `[bridge] instant_reminder` is off.
    ///
    /// Called from every inbound Feishu seam — the message handler (commands,
    /// supplements and prompts alike), the card-action handler, and turn start
    /// ([`Self::begin_turn`]). External/non-Feishu activity never reaches it.
    pub(crate) async fn note_interaction(&self, feishu: &Arc<dyn Platform>, chat_id: &str) {
        if !self.enabled {
            return;
        }
        let mut inner = self.inner.lock().await;
        Self::note_interaction_locked(&mut inner, feishu, chat_id).await;
    }

    async fn note_interaction_locked(inner: &mut Inner, feishu: &Arc<dyn Platform>, chat_id: &str) {
        inner
            .last_interaction
            .insert(chat_id.to_string(), tokio::time::Instant::now());
        if let Some(live) = inner.live.get(chat_id).cloned() {
            Self::clear_locked(inner, feishu, chat_id, live.generation, PinReason::LongTurn).await;
        }
    }

    /// The turn finished (or can never finish): mark its long-turn lifecycle
    /// complete and report whether its threshold pin is live — the caller
    /// then keeps that pin for the TTL via [`spawn_pin_ttl`].
    ///
    /// Idempotent and generation-scoped: a second completion, or a completion
    /// from an older turn, reports `false` and arms no timer. Atomic with
    /// [`Self::check_long_turn`], so a completion before the checker's next
    /// tick stops it, and one after a pin keeps the live pin for the TTL.
    pub(crate) async fn complete_turn(&self, chat_id: &str, generation: u64) -> bool {
        let mut inner = self.inner.lock().await;
        let Some(turn) = inner.long_turns.get_mut(chat_id) else {
            return false;
        };
        if turn.generation != generation || turn.completed {
            return false;
        }
        turn.completed = true;
        inner
            .live
            .get(chat_id)
            .is_some_and(|live| live.generation == generation && live.reasons.contains(&PinReason::LongTurn))
    }

    /// Ensure the Chat/Topic is pinned towards `target` for `reason` — either
    /// owner (`Pending` or `LongTurn`); the reminder stays on until every
    /// reason is released. Idempotent: while the same principal is pinned,
    /// another ensure only adopts the newer generation — no duplicate call.
    ///
    /// Test-only seam: production reconciles `Pending` through [`Self::sync`]
    /// (which owns the lock across the whole decision) and decides the
    /// `LongTurn` path atomically through [`Self::check_long_turn`]; the tests
    /// drive both reasons through this entry.
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
            // A newer turn already owns this Chat/Topic's reminder: a
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
                inner.confirmed_off.remove(&target.chat_id);
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
    /// The long-turn TTL timer ([`spawn_pin_ttl`]) passes the generation it
    /// captured at turn start, so a newer turn's pin survives a stale timer.
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
                inner.confirmed_off.insert(chat_id.to_string());
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
    /// Chat/Topic with pending work of either kind, clear every Chat/Topic
    /// whose work is gone. Both flows call this for their own kind; the union
    /// across them decides (a permission resolving never unpins a Chat/Topic
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
        // This flow's membership is exactly `targets`: a Chat/Topic it no
        // longer sees pending drops this kind's marker.
        inner.pending.retain(|_, kinds| {
            kinds.remove(&kind);
            !kinds.is_empty()
        });
        for chat_id in targets.keys() {
            inner.pending.entry(chat_id.clone()).or_default().insert(kind);
        }
        // Decide per Chat/Topic over the union of both flows.
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

/// Spawn a Turn's long-turn checker (ADR-0043): every injected tick it
/// re-evaluates the Chat/Topic's silence under the pin lock, pinning once the
/// threshold has passed with no inbound user activity, and it exits as soon
/// as the turn completes or a newer turn supersedes its generation.
///
/// Because an interaction releases the hold and restarts the clock, renewed
/// silence pins again while the turn keeps running — firing is idempotent but
/// not once-only. The task holds only the shared core; all stop and pin
/// decisions are atomic there, so a delayed tick can never pin a finished
/// turn, a newer turn's Chat/Topic, or a Chat/Topic whose user just acted. A
/// turn with no requester has nobody to pin for, and with `[bridge] instant_reminder` off
/// nothing is ever spawned.
pub(crate) fn spawn_long_turn_checker(core: &Arc<SharedCore>, target: PinTarget) {
    if !core.pins.enabled || target.user_ids.is_empty() {
        return;
    }
    let tick_ms = core.pins.long_turn_tick_ms.load(Ordering::Relaxed);
    let core = Arc::clone(core);
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(Duration::from_millis(tick_ms)).await;
            if !core.pins.check_long_turn(&core.feishu, &target).await {
                return;
            }
        }
    });
}

/// Schedule a completed long turn's TTL clear (ADR-0043): after the injected
/// TTL the pin is released with the generation it captured at turn start.
/// Generation-scoped, so the timer can never unpin a newer turn.
pub(crate) fn spawn_pin_ttl(core: &Arc<SharedCore>, chat_id: String, generation: u64) {
    if !core.pins.enabled {
        return;
    }
    let ttl_ms = core.pins.ttl_ms.load(Ordering::Relaxed);
    let core = Arc::clone(core);
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(ttl_ms)).await;
        core.pins
            .clear(&core.feishu, &chat_id, generation, PinReason::LongTurn)
            .await;
    });
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

    /// A pin state whose silence threshold is zero: every recorded interaction
    /// is already old enough, so the lifecycle tests can drive the checker
    /// without real waits. The silence tests build their own state with a real
    /// (tiny) threshold.
    fn pins_with_instant_silence() -> PinState {
        let pins = PinState::new(true);
        pins.long_turn_ms.store(0, Ordering::Relaxed);
        pins
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
    /// another lifecycle still holds (a long-turn pin rides beside it); the
    /// last owner releases the reminder.
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

    /// The startup-orphan self-heal: the Chat/Topic's next turn clears a
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
        assert_eq!(calls.len(), 1, "one orphan sweep per Chat/Topic per process");
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

    /// The long-turn lifecycle end to end: the silence threshold pins,
    /// completion reports the live pin (the caller arms the TTL), the TTL
    /// clear releases it — and a duplicate check or completion adds nothing.
    #[tokio::test]
    async fn the_long_turn_lifecycle_pins_once_and_the_ttl_clears_once() {
        let platform = Arc::new(RecordingPlatform::new());
        let feishu: Arc<dyn Platform> = platform.clone();
        let pins = pins_with_instant_silence();

        // `None` requester: no startup-orphan clear, so the calls are exactly
        // the lifecycle's.
        let generation = pins.begin_turn(&feishu, "chat_1", false, None).await;
        assert!(pins.check_long_turn(&feishu, &target("chat_1", generation)).await);
        assert_eq!(platform.reminders().await.len(), 1, "the threshold pins");
        // A delayed duplicate tick is a no-op (idempotent).
        assert!(pins.check_long_turn(&feishu, &target("chat_1", generation)).await);
        assert_eq!(platform.reminders().await.len(), 1);

        assert!(
            pins.complete_turn("chat_1", generation).await,
            "completion reports the live pin: the caller arms the TTL"
        );
        assert!(
            !pins.complete_turn("chat_1", generation).await,
            "a second completion must not arm a second TTL"
        );

        pins.clear(&feishu, "chat_1", generation, PinReason::LongTurn)
            .await;
        let calls = reminder_calls(&platform).await;
        assert_eq!(calls.len(), 2, "one pin, one TTL clear: {calls:?}");
        assert!(calls[0].1, "the pin came first");
        assert!(!calls[1].1, "the TTL clear follows");
    }

    /// Completion racing the checker decides exactly once, whichever side
    /// wins: a completion before the tick stops the checker (nothing pinned,
    /// nothing to leak), and a completion after it keeps the pin for the TTL
    /// (exactly one pin call).
    #[tokio::test]
    async fn completion_racing_the_threshold_never_double_pins_or_leaks() {
        // The completion wins the race.
        let platform = Arc::new(RecordingPlatform::new());
        let feishu: Arc<dyn Platform> = platform.clone();
        let pins = pins_with_instant_silence();
        let generation = pins.begin_turn(&feishu, "chat_1", false, None).await;
        assert!(
            !pins.complete_turn("chat_1", generation).await,
            "nothing was pinned yet: no TTL to arm"
        );
        assert!(
            !pins.check_long_turn(&feishu, &target("chat_1", generation)).await,
            "the checker stops on a completed turn"
        );
        assert!(
            platform.reminders().await.is_empty(),
            "a completed turn must never pin"
        );

        // The threshold wins the race.
        let platform = Arc::new(RecordingPlatform::new());
        let feishu: Arc<dyn Platform> = platform.clone();
        let pins = pins_with_instant_silence();
        let generation = pins.begin_turn(&feishu, "chat_1", false, None).await;
        assert!(pins.check_long_turn(&feishu, &target("chat_1", generation)).await);
        assert!(
            pins.complete_turn("chat_1", generation).await,
            "the threshold pin is live and must be kept for the TTL"
        );
        assert_eq!(platform.reminders().await.len(), 1, "exactly one pin");
    }

    /// The threshold measures silence: an interaction resets the clock, so
    /// there is no pin before the threshold has passed since it — and a pin
    /// after. The sleep is well past the threshold (2×), so a loaded machine
    /// cannot turn the "no pin yet" check into a race.
    #[tokio::test]
    async fn the_threshold_measures_silence_since_the_last_interaction() {
        let platform = Arc::new(RecordingPlatform::new());
        let feishu: Arc<dyn Platform> = platform.clone();
        let pins = PinState::new(true);
        pins.long_turn_ms.store(200, Ordering::Relaxed);

        let generation = pins.begin_turn(&feishu, "chat_1", false, None).await;
        let target = target("chat_1", generation);
        // The user just acted: the turn is running but not silent yet.
        pins.note_interaction(&feishu, "chat_1").await;
        assert!(
            pins.check_long_turn(&feishu, &target).await,
            "the turn is still running: the checker goes on"
        );
        assert!(
            platform.reminders().await.is_empty(),
            "no pin before the threshold has passed since the interaction"
        );

        // Clearly past the threshold of silence: the checker pins.
        tokio::time::sleep(Duration::from_millis(400)).await;
        assert!(pins.check_long_turn(&feishu, &target).await);
        assert_eq!(platform.reminders().await.len(), 1, "the silence pins");
    }

    /// Any interaction releases a live long-turn hold: the user is active, so
    /// the silence clock restarts and the pin comes down.
    #[tokio::test]
    async fn an_interaction_releases_a_live_long_turn_hold() {
        let platform = Arc::new(RecordingPlatform::new());
        let feishu: Arc<dyn Platform> = platform.clone();
        let pins = pins_with_instant_silence();
        let generation = pins.begin_turn(&feishu, "chat_1", false, None).await;
        assert!(pins.check_long_turn(&feishu, &target("chat_1", generation)).await);
        assert_eq!(platform.reminders().await.len(), 1);

        pins.note_interaction(&feishu, "chat_1").await;
        let calls = reminder_calls(&platform).await;
        assert_eq!(calls.len(), 2, "the interaction unpins: {calls:?}");
        assert!(!calls[1].1, "the release is an unpin");
    }

    /// The interaction release is scoped to the long-turn hold: a pending
    /// Permission's hold survives, and only its resolution unpins — proving
    /// the `LongTurn` hold was released.
    #[tokio::test]
    async fn an_interaction_keeps_a_pending_hold() {
        let platform = Arc::new(RecordingPlatform::new());
        let feishu: Arc<dyn Platform> = platform.clone();
        let pins = pins_with_instant_silence();
        let generation = pins.begin_turn(&feishu, "chat_1", false, None).await;
        assert!(pins.check_long_turn(&feishu, &target("chat_1", generation)).await);
        pins.ensure(&feishu, &target("chat_1", generation), PinReason::Pending)
            .await;
        assert_eq!(platform.reminders().await.len(), 1);

        pins.note_interaction(&feishu, "chat_1").await;
        assert_eq!(
            platform.reminders().await.len(),
            1,
            "Pending holds the pin: the release makes no call"
        );

        // The wait resolves: the last hold is gone, so the reminder clears.
        pins.clear(&feishu, "chat_1", generation, PinReason::Pending)
            .await;
        let calls = reminder_calls(&platform).await;
        assert_eq!(
            calls.len(),
            2,
            "the LongTurn hold was released, so the pending resolution unpins: {calls:?}"
        );
        assert!(!calls[1].1);
    }

    /// Firing is idempotent but not once-only: an interaction releases the
    /// hold, and renewed silence pins again while the turn is still running.
    /// The sleeps are well past the threshold (2×), not at its boundary.
    #[tokio::test]
    async fn renewed_silence_re_pins_while_the_turn_runs() {
        let platform = Arc::new(RecordingPlatform::new());
        let feishu: Arc<dyn Platform> = platform.clone();
        let pins = PinState::new(true);
        pins.long_turn_ms.store(150, Ordering::Relaxed);
        let generation = pins.begin_turn(&feishu, "chat_1", false, None).await;
        let target = target("chat_1", generation);

        tokio::time::sleep(Duration::from_millis(300)).await;
        assert!(pins.check_long_turn(&feishu, &target).await);
        assert_eq!(platform.reminders().await.len(), 1);

        // The user acts: the hold is released and the clock restarts.
        pins.note_interaction(&feishu, "chat_1").await;
        let calls = reminder_calls(&platform).await;
        assert_eq!(calls.len(), 2);
        assert!(!calls[1].1, "the interaction unpins");

        // Renewed silence: the still-running turn pins again.
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert!(pins.check_long_turn(&feishu, &target).await);
        let calls = reminder_calls(&platform).await;
        assert_eq!(calls.len(), 3, "the renewed silence re-pins: {calls:?}");
        assert!(calls[2].1);
    }

    /// A completed turn never (re)pins: the checker stops, even with renewed
    /// silence and the pin still live for the TTL.
    #[tokio::test]
    async fn a_completed_turn_never_re_pins() {
        let platform = Arc::new(RecordingPlatform::new());
        let feishu: Arc<dyn Platform> = platform.clone();
        let pins = pins_with_instant_silence();
        let generation = pins.begin_turn(&feishu, "chat_1", false, None).await;
        let target = target("chat_1", generation);
        assert!(pins.check_long_turn(&feishu, &target).await);
        assert!(
            pins.complete_turn("chat_1", generation).await,
            "the pin is live and must be kept for the TTL"
        );

        assert!(
            !pins.check_long_turn(&feishu, &target).await,
            "the checker stops once the turn completed"
        );
        assert_eq!(platform.reminders().await.len(), 1, "no re-pin after completion");
    }

    /// A new Turn means the user is active again: `begin_turn` releases the
    /// previous turn's completion-TTL hold immediately — the old TTL timer is
    /// not what clears it.
    #[tokio::test]
    async fn a_new_turn_releases_the_previous_turns_ttl_pin() {
        let platform = Arc::new(RecordingPlatform::new());
        let feishu: Arc<dyn Platform> = platform.clone();
        let pins = pins_with_instant_silence();

        let first = pins.begin_turn(&feishu, "chat_1", false, None).await;
        assert!(pins.check_long_turn(&feishu, &target("chat_1", first)).await);
        assert!(pins.complete_turn("chat_1", first).await, "the TTL is armed");
        assert_eq!(platform.reminders().await.len(), 1);

        let second = pins.begin_turn(&feishu, "chat_1", false, Some("ou_host")).await;
        assert_eq!(second, 2);
        let calls = reminder_calls(&platform).await;
        assert_eq!(calls.len(), 2, "the new turn releases the TTL hold: {calls:?}");
        assert!(!calls[1].1, "the release is an unpin");
        assert_eq!(
            calls[1].2,
            vec!["ou_host".to_string()],
            "it targets the pin's requester"
        );

        // The live pin is gone: the old TTL timer (generation 1) makes no
        // further call when it fires.
        pins.clear(&feishu, "chat_1", first, PinReason::LongTurn).await;
        assert_eq!(platform.reminders().await.len(), 2);
    }

    /// The release is scoped to the long-turn hold: a pending Permission's
    /// hold survives the new Turn (the pin stays up with no platform call),
    /// and only once that wait resolves does the reminder clear — proving the
    /// TTL hold was already released by `begin_turn`.
    #[tokio::test]
    async fn a_new_turn_keeps_a_pending_hold() {
        let platform = Arc::new(RecordingPlatform::new());
        let feishu: Arc<dyn Platform> = platform.clone();
        let pins = pins_with_instant_silence();

        let first = pins.begin_turn(&feishu, "chat_1", false, None).await;
        assert!(pins.check_long_turn(&feishu, &target("chat_1", first)).await);
        pins.ensure(&feishu, &target("chat_1", first), PinReason::Pending)
            .await;
        assert!(pins.complete_turn("chat_1", first).await);
        assert_eq!(platform.reminders().await.len(), 1);

        let second = pins.begin_turn(&feishu, "chat_1", false, None).await;
        assert_eq!(second, 2);
        assert_eq!(
            platform.reminders().await.len(),
            1,
            "Pending holds the pin: the release makes no call"
        );

        // The wait resolves: the last hold is gone, so the reminder clears.
        // (Before the fix the `LongTurn` reason was still tracked and this
        // clear no-op'd, leaking the pin.)
        pins.clear(&feishu, "chat_1", first, PinReason::Pending).await;
        let calls = reminder_calls(&platform).await;
        assert_eq!(calls.len(), 2, "the pending resolution unpins: {calls:?}");
        assert!(!calls[1].1);
    }

    /// A TTL clear is generation-scoped: after a new Turn released the old
    /// TTL hold and pinned on its own threshold, the older turn's still-
    /// sleeping timer cannot unpin the newer turn.
    #[tokio::test]
    async fn a_stale_ttl_clear_leaves_a_newer_turns_pin() {
        let platform = Arc::new(RecordingPlatform::new());
        let feishu: Arc<dyn Platform> = platform.clone();
        let pins = pins_with_instant_silence();

        let first = pins.begin_turn(&feishu, "chat_1", false, None).await;
        assert!(pins.check_long_turn(&feishu, &target("chat_1", first)).await);
        assert!(pins.complete_turn("chat_1", first).await, "the TTL is armed");

        // A newer turn starts (releasing the old TTL hold) and pins on its
        // own threshold before the first turn's TTL fires.
        let second = pins.begin_turn(&feishu, "chat_1", false, None).await;
        assert!(pins.check_long_turn(&feishu, &target("chat_1", second)).await);
        let calls = reminder_calls(&platform).await;
        assert_eq!(
            calls.len(),
            3,
            "pin, the new turn's release, the new pin: {calls:?}"
        );
        assert!(calls[0].1 && !calls[1].1 && calls[2].1);

        pins.clear(&feishu, "chat_1", first, PinReason::LongTurn).await;
        assert_eq!(
            platform.reminders().await.len(),
            3,
            "the stale TTL clear must not unpin the newer turn"
        );

        pins.clear(&feishu, "chat_1", second, PinReason::LongTurn).await;
        assert_eq!(platform.reminders().await.len(), 4);
    }

    /// An older generation's checker stops and never pins once a newer
    /// `begin_turn` armed the Chat/Topic (the newer turn's own checker
    /// decides).
    #[tokio::test]
    async fn an_older_generations_checker_never_pins_after_a_newer_turn() {
        let platform = Arc::new(RecordingPlatform::new());
        let feishu: Arc<dyn Platform> = platform.clone();
        let pins = pins_with_instant_silence();

        let first = pins.begin_turn(&feishu, "chat_1", false, None).await;
        let second = pins.begin_turn(&feishu, "chat_1", false, None).await;
        assert!(
            !pins.check_long_turn(&feishu, &target("chat_1", first)).await,
            "the superseded checker must stop"
        );
        assert!(
            platform.reminders().await.is_empty(),
            "a checker for a superseded turn must not pin"
        );

        assert!(pins.check_long_turn(&feishu, &target("chat_1", second)).await);
        assert_eq!(platform.reminders().await.len(), 1);
    }
}
