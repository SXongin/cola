//! Feishu's **Instant Reminder** (`time_sensitive`) lifecycle (ADR-0043).
//!
//! The Chat/Topic is pinned at the top of the requester's message list while a
//! Permission or Question is pending, and unpins when the wait resolves. The
//! pin is a **state** — it stays until the user acts — not an event; a
//! long-task completion is announced by a **Completion Notice** message
//! instead (ADR-0043 amendment 2026-09-21). The pin is best-effort: every
//! failure logs and the turn is unaffected (a missing
//! `im:datasync.feed_card.time_sensitive:write` scope disables only pinning).
//! A failing Chat/Topic is retried on every sweep but warns once — the
//! repeats are DEBUG, and a recovery is INFO (ADR-0048; see [`FailureLatch`]).
//!
//! Pins are generation-scoped: every pin records the **turn generation** that
//! owns it, and a clear from an older generation can never unpin a newer
//! turn's pin.
//!
//! Every pin is also **persisted** beside `sessions.json` (ADR-0043 amendment
//! 2026-09-21, #249): the Feishu client offers the user no way to cancel an
//! app's reminder, so a pin orphaned by a crash must be cleared by cola itself
//! on the next startup. [`ReminderState::clear_orphans`] clears every recorded
//! pin and keeps only the entries whose clear failed for the next startup; the
//! per-turn self-heal in [`ReminderState::begin_turn`] stays as the second net.
//!
//! A Chat/Topic has **one deterministic reminder owner** (ADR-0043 amendment
//! 2026-09-21, #247): the newest pending wins, and when the owning wait
//! resolves the next pending takes the pin in the same sweep.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;

use crate::bridge::core::SharedCore;
use crate::bridge::failure_latch::FailureLatch;
use crate::bridge::snapshot_claims::ClaimKind;
use crate::feishu::Platform;
use crate::feishu::client::INSTANT_REMINDER_SCOPE;

/// What a pending request needs to pin its Chat/Topic: the Feishu chat, the
/// turn's requester (the pinned user) and the turn generation the pin belongs
/// to.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ReminderTarget {
    pub(crate) chat_id: String,
    pub(crate) is_group: bool,
    /// The requester's open_id — Feishu requires at least one user id.
    pub(crate) user_ids: Vec<String>,
    pub(crate) generation: u64,
}

/// Deterministic owner order (#247): the newest turn wins; on a generation tie
/// the lexicographically smaller requester wins, so the owner never flips
/// between sweeps whichever flow reports first.
fn target_order(a: &ReminderTarget, b: &ReminderTarget) -> std::cmp::Ordering {
    a.generation
        .cmp(&b.generation)
        .then_with(|| b.user_ids.cmp(&a.user_ids))
}

/// Write the persisted pin set atomically (temp file + rename), best-effort:
/// it only feeds the next startup's orphan sweep, so a failure logs and
/// changes nothing else. An empty set removes the file.
fn write_pins_file(path: &Path, pins: &[PinnedChat]) {
    if pins.is_empty() {
        if let Err(e) = std::fs::remove_file(path)
            && e.kind() != std::io::ErrorKind::NotFound
        {
            tracing::warn!("instant reminder: could not remove {}: {}", path.display(), e);
        }
        return;
    }
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let data = match serde_json::to_string(&PinnedChatsFile { pins: pins.to_vec() }) {
        Ok(data) => data,
        Err(e) => {
            tracing::warn!("instant reminder: could not serialize the pin file: {e}");
            return;
        }
    };
    let tmp = path.with_extension("tmp");
    if let Err(e) = std::fs::write(&tmp, data) {
        tracing::warn!("instant reminder: could not write {}: {}", tmp.display(), e);
        return;
    }
    if let Err(e) = std::fs::rename(&tmp, path) {
        tracing::warn!("instant reminder: could not replace {}: {}", path.display(), e);
    }
}

/// The reminder cola believes is currently ON at Feishu, the generation that
/// owns it, and the pinned user(s). The generation is the highest ever
/// recorded for the Chat/Topic (a retarget never downgrades it), so a stale
/// clear from any earlier turn can never unpin it.
#[derive(Clone)]
struct LiveReminder {
    generation: u64,
    is_group: bool,
    user_ids: Vec<String>,
}

/// One persisted pin: the Feishu chat, its kind, and the pinned user(s) — the
/// exact key Feishu's `time_sensitive` state is addressed by, so startup can
/// clear it without any in-memory state.
#[derive(Clone, Serialize, Deserialize)]
struct PinnedChat {
    chat_id: String,
    is_group: bool,
    user_ids: Vec<String>,
}

/// The persisted pin set (`pinned_chats.json`). A missing file means "nothing
/// recorded", never an error.
#[derive(Default, Serialize, Deserialize)]
struct PinnedChatsFile {
    #[serde(default)]
    pins: Vec<PinnedChat>,
}

#[derive(Default)]
struct Inner {
    /// chat_id → the generation counter, bumped at every turn start.
    generations: HashMap<String, u64>,
    /// chat_id → the reminder currently ON at Feishu.
    live: HashMap<String, LiveReminder>,
    /// Chats/Topics whose Feishu reminder cola confirmed OFF in this process
    /// (a clear landed, or the startup-orphan self-heal ran) — never cleared
    /// again without a new pin.
    confirmed_off: HashSet<String>,
    /// chat_id → the request kinds with pending requests in that Chat/Topic,
    /// each carrying the best (newest) target **this** kind's last complete
    /// sweep saw. Both flows update their own kind; the union selects the one
    /// deterministic owner (ADR-0043 amendment 2026-09-21, #247).
    pending: HashMap<String, HashMap<ClaimKind, ReminderTarget>>,
    /// The warn-once policy for this state machine's best-effort calls, keyed
    /// by Chat/Topic (ADR-0048).
    latch: FailureLatch,
}

/// The Instant Reminder state machine: `[bridge] instant_reminder` opt-in,
/// generation counters, the tracked live pin, and the pending membership of
/// both request flows.
pub(crate) struct ReminderState {
    enabled: bool,
    /// Where the live pins are mirrored (`pinned_chats.json`, beside
    /// `sessions.json`). `None` in unit tests that never touch disk.
    pins_file: Option<PathBuf>,
    inner: Mutex<Inner>,
}

impl ReminderState {
    pub(crate) fn new(enabled: bool, pins_file: Option<PathBuf>) -> Self {
        Self {
            enabled,
            pins_file,
            inner: Mutex::new(Inner::default()),
        }
    }

    /// Whether the `[bridge] instant_reminder` opt-in is on. Off means every method is a
    /// no-op: no reminder call is ever made.
    pub(crate) fn enabled(&self) -> bool {
        self.enabled
    }

    /// Register a new turn in `chat_id` and return its generation. Called at
    /// turn start, before any request of that turn can be surfaced, so every
    /// pin a request lands carries the turn it belongs to.
    ///
    /// This is also the startup-orphan self-heal's second net: a pin whose
    /// persisted startup clear failed (or predates the persistence) is cleared
    /// on the Chat/Topic's next turn once, best-effort. Once that landed
    /// (`confirmed_off`), later turns do not repeat it.
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
            && !inner.confirmed_off.contains(chat_id)
        {
            let user_ids = vec![requester.to_string()];
            match feishu
                .set_instant_reminder(chat_id, is_group, &user_ids, false)
                .await
            {
                Ok(()) => {
                    inner.confirmed_off.insert(chat_id.to_string());
                    inner
                        .latch
                        .succeeded(chat_id, "instant reminder: startup-orphan clear");
                    tracing::debug!("instant reminder: startup-orphan clear sent for {}", chat_id);
                }
                Err(e) => inner.latch.failed(
                    chat_id,
                    "instant reminder: startup-orphan clear",
                    INSTANT_REMINDER_SCOPE,
                    &e,
                ),
            }
        }
        generation
    }

    /// Ensure the Chat/Topic is pinned towards `target` (a pending wait).
    /// Idempotent: while the same principal is pinned, another ensure only
    /// adopts the newer generation — no duplicate call. A different principal
    /// retargets (release, then pin), which is also #247's handover to a
    /// still-waiting older request.
    ///
    /// Test-only seam: production reconciles through [`Self::sync`], which owns
    /// the lock across the whole decision.
    #[cfg(test)]
    pub(crate) async fn ensure(&self, feishu: &Arc<dyn Platform>, target: &ReminderTarget) {
        if !self.enabled {
            return;
        }
        let mut inner = self.inner.lock().await;
        self.ensure_locked(&mut inner, feishu, target).await;
    }

    async fn ensure_locked(&self, inner: &mut Inner, feishu: &Arc<dyn Platform>, target: &ReminderTarget) {
        if target.user_ids.is_empty() {
            return;
        }
        if let Some(live) = inner.live.get_mut(&target.chat_id) {
            if live.is_group == target.is_group && live.user_ids == target.user_ids {
                // Already on for this principal: adopt the (never older)
                // generation so a stale clear cannot unpin it. No call —
                // idempotent.
                live.generation = live.generation.max(target.generation);
                return;
            }
            // A different principal needs the pin now: release the old target
            // first (one `time_sensitive` value per call), then pin the new.
            // This is also #247's handover: a pending wait that outlives the
            // old owner may be older than it, and the retarget must still
            // happen — so the recorded generation is the maximum ever seen
            // (never a downgrade), keeping every earlier turn's stale clear
            // outside the guard.
            let (old_is_group, old_users, old_generation) =
                (live.is_group, live.user_ids.clone(), live.generation);
            if let Err(e) = feishu
                .set_instant_reminder(&target.chat_id, old_is_group, &old_users, false)
                .await
            {
                inner.latch.failed(
                    &target.chat_id,
                    "instant reminder: re-target clear",
                    INSTANT_REMINDER_SCOPE,
                    &e,
                );
            } else {
                inner
                    .latch
                    .succeeded(&target.chat_id, "instant reminder: re-target clear");
            }
            inner.live.remove(&target.chat_id);
            let generation = old_generation.max(target.generation);
            match feishu
                .set_instant_reminder(&target.chat_id, target.is_group, &target.user_ids, true)
                .await
            {
                Ok(()) => {
                    inner.latch.succeeded(&target.chat_id, "instant reminder: pin");
                    inner.live.insert(
                        target.chat_id.clone(),
                        LiveReminder {
                            generation,
                            is_group: target.is_group,
                            user_ids: target.user_ids.clone(),
                        },
                    );
                    inner.confirmed_off.remove(&target.chat_id);
                    self.persist_locked(inner);
                }
                // Best-effort: the turn is unaffected; a later sweep retries
                // while the request is still pending.
                Err(e) => inner.latch.failed(
                    &target.chat_id,
                    "instant reminder: pin",
                    INSTANT_REMINDER_SCOPE,
                    &e,
                ),
            }
            return;
        }
        match feishu
            .set_instant_reminder(&target.chat_id, target.is_group, &target.user_ids, true)
            .await
        {
            Ok(()) => {
                inner.latch.succeeded(&target.chat_id, "instant reminder: pin");
                inner.live.insert(
                    target.chat_id.clone(),
                    LiveReminder {
                        generation: target.generation,
                        is_group: target.is_group,
                        user_ids: target.user_ids.clone(),
                    },
                );
                inner.confirmed_off.remove(&target.chat_id);
                self.persist_locked(inner);
            }
            // Best-effort: the turn is unaffected; a later sweep retries
            // while the request is still pending.
            Err(e) => inner.latch.failed(
                &target.chat_id,
                "instant reminder: pin",
                INSTANT_REMINDER_SCOPE,
                &e,
            ),
        }
    }

    /// Release `chat_id`'s pending pin. Never clears when a strictly newer turn
    /// owns it (ADR-0043: a stale clear can never unpin a newer turn's pin).
    /// `generation` is the turn the caller believes owns the pin. Idempotent:
    /// nothing tracked means no call.
    ///
    /// Test-only seam: production reconciles through [`Self::sync`].
    #[cfg(test)]
    pub(crate) async fn clear(&self, feishu: &Arc<dyn Platform>, chat_id: &str, generation: u64) {
        if !self.enabled {
            return;
        }
        let mut inner = self.inner.lock().await;
        self.clear_locked(&mut inner, feishu, chat_id, generation).await;
    }

    async fn clear_locked(
        &self,
        inner: &mut Inner,
        feishu: &Arc<dyn Platform>,
        chat_id: &str,
        generation: u64,
    ) {
        let Some(live) = inner.live.get(chat_id).cloned() else {
            return;
        };
        // A strictly newer generation owns the pin: a stale clear must not
        // touch it.
        if live.generation > generation {
            return;
        }
        match feishu
            .set_instant_reminder(chat_id, live.is_group, &live.user_ids, false)
            .await
        {
            Ok(()) => {
                inner.latch.succeeded(chat_id, "instant reminder: clear");
                inner.live.remove(chat_id);
                inner.confirmed_off.insert(chat_id.to_string());
                self.persist_locked(inner);
            }
            // Keep the pin tracked so the next sweep retries; the failure is
            // logged and nothing else is affected.
            Err(e) => inner
                .latch
                .failed(chat_id, "instant reminder: clear", INSTANT_REMINDER_SCOPE, &e),
        }
    }

    /// Reconcile one flow's pending requests with the pin state: pin every
    /// Chat/Topic with pending work of either kind towards its one
    /// deterministic owner, clear every Chat/Topic whose work is gone. Both
    /// flows call this for their own kind; the union across them decides (a
    /// permission resolving never unpins a Chat/Topic that still has a
    /// question pending, and when the owning wait resolves the next pending —
    /// either kind — takes the pin in this same sweep).
    pub(crate) async fn sync(&self, feishu: &Arc<dyn Platform>, kind: ClaimKind, targets: &[ReminderTarget]) {
        if !self.enabled {
            return;
        }
        let mut inner = self.inner.lock().await;
        // This flow's kind entry is exactly this sweep's best target per
        // Chat/Topic: a Chat/Topic it no longer sees pending drops the entry.
        for kinds in inner.pending.values_mut() {
            kinds.remove(&kind);
        }
        inner.pending.retain(|_, kinds| !kinds.is_empty());
        let mut best: HashMap<String, ReminderTarget> = HashMap::new();
        for target in targets {
            if target.user_ids.is_empty() {
                continue;
            }
            best.entry(target.chat_id.clone())
                .and_modify(|current| {
                    if target_order(target, current) == std::cmp::Ordering::Greater {
                        *current = target.clone();
                    }
                })
                .or_insert_with(|| target.clone());
        }
        for (chat_id, target) in best {
            inner.pending.entry(chat_id).or_default().insert(kind, target);
        }
        // Decide per Chat/Topic over the union of both flows: the newest
        // pending owns the pin, and a resolved owner hands it over here.
        let mut chats: Vec<String> = inner.pending.keys().cloned().collect();
        for chat_id in inner.live.keys() {
            if !chats.contains(chat_id) {
                chats.push(chat_id.clone());
            }
        }
        for chat_id in chats {
            let owner = inner
                .pending
                .get(&chat_id)
                .and_then(|kinds| kinds.values().max_by(|a, b| target_order(a, b)).cloned());
            match owner {
                Some(target) => {
                    self.ensure_locked(&mut inner, feishu, &target).await;
                }
                None => {
                    if let Some(live) = inner.live.get(&chat_id).cloned() {
                        // Clear with the live pin's own generation: the
                        // generation guard protects against stale clears, not
                        // against the flow that owns the pin ending its wait.
                        self.clear_locked(&mut inner, feishu, &chat_id, live.generation)
                            .await;
                    }
                }
            }
        }
    }

    /// Mirror the live pins to disk (ADR-0043 amendment 2026-09-21): the file
    /// is what a restarted cola clears at startup. Best-effort — the pin state
    /// itself is unaffected by a failed write. An empty set removes the file.
    fn persist_locked(&self, inner: &Inner) {
        let Some(path) = &self.pins_file else {
            return;
        };
        let pins: Vec<PinnedChat> = inner
            .live
            .iter()
            .map(|(chat_id, live)| PinnedChat {
                chat_id: chat_id.clone(),
                is_group: live.is_group,
                user_ids: live.user_ids.clone(),
            })
            .collect();
        write_pins_file(path, &pins);
    }

    /// Clear the pins a previous cola process left at Feishu (#249): the
    /// Feishu client offers the user no way to cancel an app's reminder, so
    /// cola must clear them itself. Best-effort per entry — an entry whose
    /// clear fails stays in the file for the next startup. A Chat/Topic
    /// re-pinned since (same principal) is left alone, its record dropped: the
    /// new pin is live state, not an orphan.
    pub(crate) async fn clear_orphans(&self, feishu: &Arc<dyn Platform>) {
        if !self.enabled {
            return;
        }
        let Some(path) = &self.pins_file else {
            return;
        };
        let Ok(raw) = std::fs::read_to_string(path) else {
            return; // Missing (or unreadable) means nothing recorded.
        };
        let file: PinnedChatsFile = match serde_json::from_str(&raw) {
            Ok(file) => file,
            Err(e) => {
                tracing::warn!(
                    "instant reminder: could not parse {} ({e}); leaving it untouched",
                    path.display()
                );
                return;
            }
        };
        let mut remaining = Vec::new();
        for entry in file.pins {
            let mut inner = self.inner.lock().await;
            let repinned = inner
                .live
                .get(&entry.chat_id)
                .is_some_and(|live| live.is_group == entry.is_group && live.user_ids == entry.user_ids);
            if repinned {
                continue;
            }
            match feishu
                .set_instant_reminder(&entry.chat_id, entry.is_group, &entry.user_ids, false)
                .await
            {
                Ok(()) => {
                    inner.confirmed_off.insert(entry.chat_id.clone());
                    inner
                        .latch
                        .succeeded(&entry.chat_id, "instant reminder: startup-orphan clear");
                    tracing::debug!("instant reminder: cleared startup orphan in {}", entry.chat_id);
                }
                Err(e) => {
                    inner.latch.failed(
                        &entry.chat_id,
                        "instant reminder: startup-orphan clear",
                        INSTANT_REMINDER_SCOPE,
                        &e,
                    );
                    remaining.push(entry);
                }
            }
        }
        write_pins_file(path, &remaining);
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
) -> Option<ReminderTarget> {
    let (host, is_group, requester, generation) =
        crate::bridge::pollers::walk_parent_chain(&core.opencode, session_id, Some(directory), |current| {
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
    Some(ReminderTarget {
        chat_id,
        is_group,
        user_ids: vec![requester],
        generation,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bridge::test_support::{RecordingPlatform, assert_line_level, capture_logs, level_count};

    fn target_for(chat_id: &str, generation: u64, requester: &str) -> ReminderTarget {
        ReminderTarget {
            chat_id: chat_id.into(),
            is_group: false,
            user_ids: vec![requester.into()],
            generation,
        }
    }

    fn target(chat_id: &str, generation: u64) -> ReminderTarget {
        target_for(chat_id, generation, "ou_host")
    }

    fn group_target(chat_id: &str, generation: u64, requester: &str) -> ReminderTarget {
        ReminderTarget {
            chat_id: chat_id.into(),
            is_group: true,
            user_ids: vec![requester.into()],
            generation,
        }
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
    /// older turn can never unpin the pin a newer turn owns.
    #[tokio::test]
    async fn a_stale_clear_never_unpins_a_newer_turns_pin() {
        let platform = Arc::new(RecordingPlatform::new());
        let feishu: Arc<dyn Platform> = platform.clone();
        let pins = ReminderState::new(true, None);

        pins.ensure(&feishu, &target("chat_1", 1)).await;
        // Turn 2 begins while the wait is still pending: ensure adopts the
        // newer generation without a duplicate call.
        pins.ensure(&feishu, &target("chat_1", 2)).await;
        assert_eq!(platform.reminders().await.len(), 1, "one pin, no duplicates");

        // A stale clear from turn 1 must not touch turn 2's pin.
        pins.clear(&feishu, "chat_1", 1).await;
        assert_eq!(
            platform.reminders().await.len(),
            1,
            "a stale clear must not call the platform"
        );

        // The owning generation still clears.
        pins.clear(&feishu, "chat_1", 2).await;
        let calls = reminder_calls(&platform).await;
        assert_eq!(calls.len(), 2);
        assert!(calls[0].1, "the pin call comes first");
        assert!(!calls[1].1, "the clear follows");
    }

    /// An older-generation ensure must not downgrade a newer turn's pin; only
    /// the newer turn's clear may release it.
    #[tokio::test]
    async fn a_stale_ensure_never_downgrades_a_newer_turns_pin() {
        let platform = Arc::new(RecordingPlatform::new());
        let feishu: Arc<dyn Platform> = platform.clone();
        let pins = ReminderState::new(true, None);

        // Turn 2 pins first (two turns of one chat can overlap: two topics of
        // one group each carry their own generation).
        pins.ensure(&feishu, &target("chat_1", 2)).await;
        // A delayed sweep for turn 1 must not touch the pin...
        pins.ensure(&feishu, &target("chat_1", 1)).await;
        assert_eq!(platform.reminders().await.len(), 1, "no duplicate, no retarget");

        // ...and must not have downgraded the generation, or its own stale
        // clear would pass the guard and unpin turn 2's pin.
        pins.clear(&feishu, "chat_1", 1).await;
        assert_eq!(
            platform.reminders().await.len(),
            1,
            "a stale clear must not unpin the newer pin"
        );
        // Turn 2's own clear still releases it.
        pins.clear(&feishu, "chat_1", 2).await;
        assert_eq!(platform.reminders().await.len(), 2);
    }

    /// #247 handover: when the pin's requester is no longer the newest wait
    /// (the newer pending resolved, an older one still waits), the sweep's
    /// owner retargets to it — the recorded generation is the maximum ever
    /// seen, so no earlier turn's stale clear can shadow the older wait's pin.
    #[tokio::test]
    async fn a_handover_retargets_the_pin_to_a_still_waiting_older_pending() {
        let platform = Arc::new(RecordingPlatform::new());
        let feishu: Arc<dyn Platform> = platform.clone();
        let pins = ReminderState::new(true, None);

        // Turn 2's wait owns the pin (a group: distinct requesters).
        pins.ensure(&feishu, &group_target("chat_1", 2, "ou_new")).await;
        // Turn 2 resolves; turn 1's wait remains and takes the pin.
        pins.ensure(&feishu, &group_target("chat_1", 1, "ou_old")).await;

        let calls = reminder_calls(&platform).await;
        assert_eq!(
            calls.len(),
            3,
            "clear the old target, then pin the new: {calls:?}"
        );
        assert!(calls[0].1 && calls[0].2 == vec!["ou_new".to_string()]);
        assert!(!calls[1].1 && calls[1].2 == vec!["ou_new".to_string()]);
        assert!(calls[2].1 && calls[2].2 == vec!["ou_old".to_string()]);

        // The recorded generation is the max ever seen, so a clear carrying
        // the older owner's generation is outside the guard...
        pins.clear(&feishu, "chat_1", 1).await;
        assert_eq!(
            platform.reminders().await.len(),
            3,
            "the older-generation clear is a no-op"
        );
        // ...while a clear at the live generation (exactly as `sync` issues
        // it) releases the pin.
        pins.clear(&feishu, "chat_1", 2).await;
        assert_eq!(platform.reminders().await.len(), 4);
    }

    /// A same-turn requester change still retargets: two requests of one turn
    /// may name different users (a group), and the sweep's deterministic owner
    /// picks one of them consistently.
    #[tokio::test]
    async fn a_same_generation_requester_change_retargets() {
        let platform = Arc::new(RecordingPlatform::new());
        let feishu: Arc<dyn Platform> = platform.clone();
        let pins = ReminderState::new(true, None);

        pins.ensure(&feishu, &group_target("chat_1", 2, "ou_new")).await;
        pins.ensure(&feishu, &group_target("chat_1", 2, "ou_other")).await;
        let calls = reminder_calls(&platform).await;
        assert_eq!(
            calls.len(),
            3,
            "clear the old target, then pin the new: {calls:?}"
        );
        assert!(!calls[1].1 && calls[1].2 == vec!["ou_new".to_string()]);
        assert!(calls[2].1 && calls[2].2 == vec!["ou_other".to_string()]);
    }

    /// The startup-orphan self-heal: the Chat/Topic's next turn clears a
    /// pin left by a crash/restart once, then stops re-clearing.
    #[tokio::test]
    async fn a_turn_start_clears_a_possible_startup_orphan_once() {
        let platform = Arc::new(RecordingPlatform::new());
        let feishu: Arc<dyn Platform> = platform.clone();
        let pins = ReminderState::new(true, None);

        pins.begin_turn(&feishu, "chat_1", false, Some("ou_host")).await;
        pins.begin_turn(&feishu, "chat_1", false, Some("ou_host")).await;

        let calls = reminder_calls(&platform).await;
        assert_eq!(calls.len(), 1, "the orphan self-heal runs once: {calls:?}");
        assert!(!calls[0].1, "it clears");
        assert_eq!(calls[0].0, "chat_1");
    }

    /// The opt-in gate: with the feature off, every method is a no-op.
    #[tokio::test]
    async fn a_disabled_pin_state_never_calls_the_platform() {
        let platform = Arc::new(RecordingPlatform::new());
        let feishu: Arc<dyn Platform> = platform.clone();
        let pins = ReminderState::new(false, None);

        pins.begin_turn(&feishu, "chat_1", false, Some("ou_host")).await;
        pins.ensure(&feishu, &target("chat_1", 1)).await;
        pins.clear(&feishu, "chat_1", 1).await;

        assert!(
            platform.reminders().await.is_empty(),
            "instant_reminder = false must make no Instant Reminder call"
        );
    }

    /// #249: a landed pin is mirrored to disk (the orphan-sweep key), a
    /// confirmed clear drops it, and a fresh process clears what a crashed one
    /// left behind.
    #[tokio::test]
    async fn pins_are_persisted_and_cleared_as_startup_orphans() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("pinned_chats.json");
        let platform = Arc::new(RecordingPlatform::new());
        let feishu: Arc<dyn Platform> = platform.clone();

        let pins = ReminderState::new(true, Some(path.clone()));
        pins.ensure(&feishu, &target("chat_1", 1)).await;
        let raw = std::fs::read_to_string(&path).expect("the pin is persisted");
        assert!(raw.contains("chat_1"), "the file records the chat: {raw}");

        pins.clear(&feishu, "chat_1", 1).await;
        assert!(!path.exists(), "the confirmed clear removes the record");

        // A crash left a pin: the next process clears it, then drops the file.
        std::fs::write(
            &path,
            serde_json::json!({
                "pins": [{ "chat_id": "chat_9", "is_group": true, "user_ids": ["ou_host"] }]
            })
            .to_string(),
        )
        .unwrap();
        let restarted = ReminderState::new(true, Some(path.clone()));
        restarted.clear_orphans(&feishu).await;
        let calls = reminder_calls(&platform).await;
        assert_eq!(calls.len(), 3, "pin, clear, startup orphan clear: {calls:?}");
        assert!(!calls[2].1, "the startup sweep clears");
        assert_eq!(calls[2].0, "chat_9");
        assert!(!path.exists(), "a successful orphan clear drops the record");
    }

    /// #249: an orphan clear that fails stays recorded for the next startup.
    #[tokio::test]
    async fn a_failed_orphan_clear_stays_recorded_for_the_next_startup() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("pinned_chats.json");
        std::fs::write(
            &path,
            serde_json::json!({
                "pins": [{ "chat_id": "chat_9", "is_group": false, "user_ids": ["ou_host"] }]
            })
            .to_string(),
        )
        .unwrap();

        let failing = RecordingPlatform::new();
        failing
            .fail_instant_reminder
            .store(true, std::sync::atomic::Ordering::SeqCst);
        let failing: Arc<dyn Platform> = Arc::new(failing);
        let restarted = ReminderState::new(true, Some(path.clone()));
        restarted.clear_orphans(&failing).await;
        let raw = std::fs::read_to_string(&path).expect("the failed clear is kept");
        assert!(
            raw.contains("chat_9"),
            "the record survives a failed clear: {raw}"
        );

        // The next startup retries and succeeds.
        let platform = Arc::new(RecordingPlatform::new());
        let feishu: Arc<dyn Platform> = platform.clone();
        let next = ReminderState::new(true, Some(path.clone()));
        next.clear_orphans(&feishu).await;
        let calls = reminder_calls(&platform).await;
        assert_eq!(calls.len(), 1, "the retry clears: {calls:?}");
        assert!(!calls[0].1);
        assert!(!path.exists(), "the retry drops the record");
    }

    /// #249: a Chat/Topic re-pinned since the crash is live state, not an
    /// orphan — the startup sweep leaves it alone and drops the record.
    #[tokio::test]
    async fn a_re_pinned_chat_is_not_cleared_at_startup() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("pinned_chats.json");
        std::fs::write(
            &path,
            serde_json::json!({
                "pins": [{ "chat_id": "chat_1", "is_group": false, "user_ids": ["ou_host"] }]
            })
            .to_string(),
        )
        .unwrap();

        let platform = Arc::new(RecordingPlatform::new());
        let feishu: Arc<dyn Platform> = platform.clone();
        let restarted = ReminderState::new(true, Some(path.clone()));
        restarted.ensure(&feishu, &target("chat_1", 1)).await;
        restarted.clear_orphans(&feishu).await;

        let calls = reminder_calls(&platform).await;
        assert_eq!(calls.len(), 1, "only the re-pin: the orphan sweep makes no call");
        assert!(calls[0].1);
        assert!(!path.exists(), "the record is dropped for the live pin");
    }

    /// ADR-0048's warn-once policy for the reminder: a Chat/Topic whose pin
    /// keeps failing warns once (with the cause and the actionable scope), the
    /// identical repeats are DEBUG, and the sweep that lands the pin logs INFO
    /// recovery — while every sweep still retries.
    #[tokio::test]
    async fn a_repeated_pin_failure_warns_once_and_recovery_logs_info() {
        let platform = Arc::new(RecordingPlatform::new());
        platform
            .fail_instant_reminder
            .store(true, std::sync::atomic::Ordering::SeqCst);
        let feishu: Arc<dyn Platform> = platform.clone();
        let pins = ReminderState::new(true, None);
        let pin_target = target("chat_1", 1);

        let (_, logs) = capture_logs(async {
            pins.sync(&feishu, ClaimKind::Permission, std::slice::from_ref(&pin_target))
                .await;
            pins.sync(&feishu, ClaimKind::Permission, std::slice::from_ref(&pin_target))
                .await;
        })
        .await;

        assert_eq!(
            reminder_calls(&platform).await.len(),
            2,
            "the failed pin is still retried every sweep"
        );
        assert_line_level(&logs, "instant reminder: pin chat_1 failed", "WARN");
        assert_eq!(
            level_count(&logs, "instant reminder: pin chat_1", "WARN"),
            1,
            "one warning, not one per sweep:\n{logs}"
        );
        assert_eq!(
            level_count(&logs, "instant reminder: pin chat_1", "DEBUG"),
            1,
            "the repeat is DEBUG:\n{logs}"
        );
        assert!(
            logs.contains("im:datasync.feed_card.time_sensitive:write"),
            "the warning names the actionable scope:\n{logs}"
        );
        assert!(
            logs.contains("simulated set_instant_reminder failure"),
            "the warning carries the cause:\n{logs}"
        );

        // The pin lands: INFO recovery, and the retry still happened.
        platform
            .fail_instant_reminder
            .store(false, std::sync::atomic::Ordering::SeqCst);
        let (_, logs) = capture_logs(async {
            pins.sync(&feishu, ClaimKind::Permission, std::slice::from_ref(&pin_target))
                .await;
        })
        .await;
        assert_eq!(reminder_calls(&platform).await.len(), 3);
        assert_line_level(&logs, "instant reminder: pin chat_1 recovered", "INFO");
    }
}
