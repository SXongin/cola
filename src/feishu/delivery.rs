//! The card-delivery decorator (ADR-0067): every `update_message` that fails
//! recoverably is remembered as a **Pending Card Update** — the newest payload
//! for that card — and retried until it lands, so a card whose write was lost
//! while Feishu was unreachable converges instead of staying frozen.
//!
//! The decorator wraps the platform where the bridge assembles it, so every
//! writer is covered: the flush, the reap's endings, interaction repaints,
//! command cards. Creates (`reply_card` / `send_card`) are deliberately NOT
//! retried — Feishu has no idempotency key, so retrying one can double-post.
//!
//! Entries carry a monotonic per-card sequence: a slow failed retry can never
//! resurrect a payload a newer write has superseded, and a newer delivery
//! clears the older pending state. Bounded by an entry cap (oldest-first
//! eviction with a WARN) and retried with per-entry exponential backoff by
//! [`CardDelivery::drain_pending_card_updates`] — called by the Session Sync
//! pass each tick, and with `force` immediately after a Feishu WS reconnect.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;

use async_trait::async_trait;
use serde_json::Value;

use crate::error::Result;
use crate::feishu::Platform;
use crate::feishu::client::{ChatMessage, FeishuMessage, ImageAttachment};

/// The most cards one process remembers failed writes for. Small: each entry
/// holds a full card payload, and a real outage touches a handful of cards,
/// never hundreds.
const MAX_PENDING: usize = 64;
/// The first retry waits this long; each further failure doubles it.
const BACKOFF_BASE: Duration = Duration::from_secs(5);
/// The backoff cap. Entries retry indefinitely at this cadence — a time limit
/// would strand the card permanently stale, which is the defect being fixed.
const BACKOFF_MAX: Duration = Duration::from_secs(300);
/// One retry's own bound. The client has no default timeout, and the drain is
/// a background convergence path: a hung PATCH must not hold the card's
/// delivery lock (blocking new writes), the Session Sync pass, or all future
/// drains. On expiry the payload stays pending and is retried later — a
/// cancelled update may have landed, and re-sending one is idempotent.
const DRAIN_RETRY_TIMEOUT: Duration = Duration::from_secs(30);

/// One card's newest write state.
///
/// `card` is `Some` while that write's payload is undelivered — the Pending
/// Card Update proper — and `None` once a write of the same sequence settled
/// (delivered, or permanently refused). The settled form is kept as a
/// sequence tombstone: an outcome that is not newer than the stored sequence
/// must not re-register its older payload (ADR-0067: a slow failure may never
/// resurrect a superseded payload over a newer delivered one). Tombstones age
/// out under the same cap as pending payloads.
#[derive(Clone)]
struct PendingEntry {
    /// The monotonic write sequence this state belongs to. A retry may clear
    /// the payload only while the entry still names this sequence.
    seq: u64,
    /// The undelivered newest card JSON; `None` for a settled tombstone.
    card: Option<Value>,
    /// Whether the settled write actually reached Feishu. Meaningless while
    /// `card` is `Some` (the payload is still owed); a settled tombstone
    /// carries it so the Rendered Cursor's drain reconcile can tell a
    /// delivery from a permanent refusal (spec #561).
    delivered: bool,
    /// Failed retry attempts so far — the backoff exponent.
    attempts: u32,
    /// The earliest instant the next retry may go out (unless forced).
    next_attempt: tokio::time::Instant,
    /// The card's delivery lock (shared with [`State::locks`]): every write to
    /// this message — a normal `update_message` and a drain retry alike —
    /// holds it across the Feishu call, so an older retry can never land after
    /// a newer write (ADR-0038's ordering, for the outbox writer).
    lock: Arc<tokio::sync::Mutex<()>>,
}

#[derive(Default)]
struct State {
    entries: HashMap<String, PendingEntry>,
    /// The most recent RECOVERABLE failure per card — `(seq, payload)` — the
    /// exact write a failure note asks about (spec #561, review #569): a newer
    /// write may replace `entries` before the note runs, and the note must
    /// still learn whether that payload's own sequence delivered.
    failed: HashMap<String, (u64, Value)>,
    /// One delivery lock per card while its entry lives — plus while any call
    /// still holds it. Every write to a message is serialized on it, so the
    /// outbox's retries and the live writers cannot reorder each other at
    /// Feishu. Pruned once no entry names the card and no call holds the lock
    /// (see [`CardDelivery::prune_locks`]), so it stays bounded.
    locks: HashMap<String, Arc<tokio::sync::Mutex<()>>>,
    seq: u64,
}

/// A [`Platform`] decorator that turns a failed `update_message` into a
/// Pending Card Update and drains them on demand.
pub(crate) struct CardDelivery {
    inner: Arc<dyn Platform>,
    state: Mutex<State>,
    /// One drain at a time: a Session Sync pass and a WS reconnect must not
    /// both walk the set (a superseded retry is harmless, but a shared walk
    /// keeps the bookkeeping single-writer).
    drain_lock: tokio::sync::Mutex<()>,
    max_pending: usize,
    backoff_base: Duration,
    backoff_max: Duration,
}

impl CardDelivery {
    /// Wrap `inner` with the production bounds.
    pub(crate) fn new(inner: Arc<dyn Platform>) -> Self {
        Self::with_limits(inner, MAX_PENDING, BACKOFF_BASE, BACKOFF_MAX)
    }

    /// [`Self::new`] with injectable bounds (the unit tests).
    fn with_limits(
        inner: Arc<dyn Platform>,
        max_pending: usize,
        backoff_base: Duration,
        backoff_max: Duration,
    ) -> Self {
        Self {
            inner,
            state: Mutex::new(State::default()),
            drain_lock: tokio::sync::Mutex::new(()),
            max_pending,
            backoff_base,
            backoff_max,
        }
    }

    /// Whether `message_id` has an undelivered newest payload — the gate the
    /// Live Card record's removal consults (ADR-0063 amendment): a terminal
    /// card's record stays until its ending write is confirmed.
    pub(crate) fn pending(&self, message_id: &str) -> bool {
        self.state
            .lock()
            .unwrap()
            .entries
            .get(message_id)
            .is_some_and(|entry| entry.card.is_some())
    }

    /// The card's delivery lock, created on first use. Stable while an entry
    /// names the card or any call holds the lock.
    fn card_lock(&self, message_id: &str) -> Arc<tokio::sync::Mutex<()>> {
        self.state
            .lock()
            .unwrap()
            .locks
            .entry(message_id.to_string())
            .or_default()
            .clone()
    }

    /// Drop locks nothing needs: a card with no entry and no holder.
    fn prune_locks(state: &mut State) {
        let State { entries, locks, .. } = state;
        locks.retain(|message_id, lock| entries.contains_key(message_id) || Arc::strong_count(lock) > 1);
    }

    /// Fold one write's outcome into the state. The newest sequence wins: a
    /// recoverable failure records the payload as the card's newest Pending
    /// Card Update; a delivery (or a permanent refusal) leaves a settled
    /// tombstone at that sequence instead, so an outcome that is not newer
    /// than the stored one cannot re-register a superseded payload.
    fn observe(&self, message_id: &str, seq: u64, card: &Value, result: &Result<()>) {
        let mut state = self.state.lock().unwrap();
        let newest = match state.entries.get(message_id) {
            Some(entry) => entry.seq < seq,
            None => true,
        };
        if !newest {
            return;
        }
        let recoverable = matches!(result, Err(e) if e.is_recoverable_card_write());
        if recoverable {
            // Remember the exact write a failure note will ask about (spec
            // #561, review #569): a newer write may replace the entry before
            // the note runs, and the note must still be able to tell whether
            // THIS payload's sequence delivered.
            state.failed.insert(message_id.to_string(), (seq, card.clone()));
        }
        let entry = if recoverable {
            PendingEntry {
                seq,
                card: Some(card.clone()),
                delivered: false,
                attempts: 0,
                next_attempt: tokio::time::Instant::now() + self.backoff_base,
                lock: state.locks.entry(message_id.to_string()).or_default().clone(),
            }
        } else {
            // Delivered, or a permanent refusal (a rejected card, a gone
            // message, a non-recoverable 4xx): the payload can never usefully
            // be retried, so only the sequence is remembered — together with
            // whether it actually landed, which the Rendered Cursor's drain
            // reconcile reads (spec #561).
            PendingEntry {
                seq,
                card: None,
                delivered: result.is_ok(),
                attempts: 0,
                next_attempt: tokio::time::Instant::now(),
                lock: state.locks.entry(message_id.to_string()).or_default().clone(),
            }
        };
        state.entries.insert(message_id.to_string(), entry);
        self.evict_over_cap(&mut state);
        Self::prune_locks(&mut state);
    }

    /// Keep the state under the cap. Settled tombstones leave first — the cap
    /// bounds undelivered payloads, and a tombstone only guards against a
    /// still-in-flight older write. A pending payload evicted (only when more
    /// than the cap are owed) warns; a tombstone leaves quietly.
    fn evict_over_cap(&self, state: &mut State) {
        while state.entries.len() > self.max_pending {
            let victim = state
                .entries
                .iter()
                .filter(|(_, entry)| entry.card.is_none())
                .min_by_key(|(_, entry)| entry.seq)
                .or_else(|| state.entries.iter().min_by_key(|(_, entry)| entry.seq))
                .map(|(message_id, _)| message_id.clone());
            let Some(victim) = victim else {
                return;
            };
            let evicted = state.entries.remove(&victim);
            // The failure memory leaves with its card's entry: the note it
            // serves names the same payload, and the entry is gone.
            state.failed.remove(&victim);
            if evicted.is_some_and(|entry| entry.card.is_some()) {
                tracing::warn!(
                    "pending card update for {victim} evicted: {} card writes reached",
                    self.max_pending
                );
            }
        }
    }

    /// The delay before the next retry after `attempts` failed ones.
    fn backoff(&self, attempts: u32) -> Duration {
        self.backoff_base
            .saturating_mul(1u32 << attempts.min(16))
            .min(self.backoff_max)
    }
}

#[async_trait]
impl Platform for CardDelivery {
    async fn get_ws_endpoint(&self) -> Result<String> {
        self.inner.get_ws_endpoint().await
    }

    async fn reply_card(&self, reply_to: &str, card: &Value) -> Result<String> {
        self.inner.reply_card(reply_to, card).await
    }

    async fn send_card(&self, receive_id_type: &str, receive_id: &str, card: &Value) -> Result<String> {
        self.inner.send_card(receive_id_type, receive_id, card).await
    }

    async fn update_message(&self, message_id: &str, card: &Value) -> Result<()> {
        let seq = {
            let mut state = self.state.lock().unwrap();
            state.seq += 1;
            state.seq
        };
        // Serialize with any in-flight drain retry for this card: the newest
        // write must land at Feishu last (ADR-0038's ordering; ADR-0067's
        // newest-wins).
        let lock = self.card_lock(message_id);
        let _delivery = lock.lock().await;
        let result = self.inner.update_message(message_id, card).await;
        self.observe(message_id, seq, card, &result);
        result
    }

    async fn reply_text(&self, message_id: &str, text: &str) -> Result<String> {
        self.inner.reply_text(message_id, text).await
    }

    async fn reply_card_in_thread(&self, message_id: &str, card: &Value) -> Result<(String, Option<String>)> {
        self.inner.reply_card_in_thread(message_id, card).await
    }

    async fn reply_in_thread(&self, message_id: &str, text: &str) -> Result<(String, Option<String>)> {
        self.inner.reply_in_thread(message_id, text).await
    }

    async fn reply_completion_notice(
        &self,
        message_id: &str,
        open_id: &str,
        name: Option<&str>,
        text: &str,
    ) -> Result<String> {
        self.inner
            .reply_completion_notice(message_id, open_id, name, text)
            .await
    }

    async fn user_name(&self, open_id: &str) -> Result<Option<String>> {
        self.inner.user_name(open_id).await
    }

    async fn chat_name(&self, chat_id: &str) -> Result<Option<String>> {
        self.inner.chat_name(chat_id).await
    }

    async fn bot_open_id(&self) -> Result<String> {
        self.inner.bot_open_id().await
    }

    async fn list_messages(&self, container_id_type: &str, container_id: &str) -> Result<Vec<ChatMessage>> {
        self.inner.list_messages(container_id_type, container_id).await
    }

    async fn get_message(&self, message_id: &str) -> Result<FeishuMessage> {
        self.inner.get_message(message_id).await
    }

    async fn get_card_view(&self, message_id: &str) -> Result<Value> {
        self.inner.get_card_view(message_id).await
    }

    async fn download_image(&self, message_id: &str, image_key: &str) -> Result<ImageAttachment> {
        self.inner.download_image(message_id, image_key).await
    }

    async fn set_instant_reminder(
        &self,
        chat_id: &str,
        is_group: bool,
        user_ids: &[String],
        on: bool,
    ) -> Result<()> {
        self.inner
            .set_instant_reminder(chat_id, is_group, user_ids, on)
            .await
    }

    async fn pin_message(&self, message_id: &str) -> Result<()> {
        self.inner.pin_message(message_id).await
    }

    async fn unpin_message(&self, message_id: &str) -> Result<()> {
        self.inner.unpin_message(message_id).await
    }

    async fn drain_pending_card_updates(&self, force: bool) {
        // One drain at a time: a Session Sync pass and a WS reconnect must not
        // walk the set together. A skipped call is fine — the running drain is
        // already doing the work.
        let _drain = match self.drain_lock.try_lock() {
            Ok(guard) => guard,
            Err(_) => return,
        };
        let now = tokio::time::Instant::now();
        let due: Vec<(String, PendingEntry)> = {
            let state = self.state.lock().unwrap();
            state
                .entries
                .iter()
                .filter(|(_, entry)| entry.card.is_some() && (force || entry.next_attempt <= now))
                .map(|(message_id, entry)| (message_id.clone(), entry.clone()))
                .collect()
        };
        for (message_id, entry) in due {
            // Only a pending entry is due (the filter above); the clone keeps
            // the payload for the retry.
            let Some(card) = entry.card.clone() else { continue };
            // Serialize with the card's other writers: the retry must land at
            // Feishu before any newer write, or an older payload would be the
            // last state on the card (ADR-0067's newest-wins at the wire).
            // A live writer already mid-flight makes the retry unnecessary —
            // it is about to deliver a newer state — so skip and let the next
            // pass retry anything its outcome left owed.
            let Ok(_delivery) = entry.lock.try_lock() else {
                continue;
            };
            // Re-check under the lock: a write that settled (or replaced) the
            // entry while this drain was waiting must not be overwritten by
            // the payload we snapshotted.
            {
                let state = self.state.lock().unwrap();
                if !state
                    .entries
                    .get(&message_id)
                    .is_some_and(|current| current.seq == entry.seq && current.card.is_some())
                {
                    continue;
                }
            }
            // Bounded: a hung PATCH must not hold the card's delivery lock —
            // and so the Session Sync pass — forever. On expiry the payload
            // stays pending and is retried later.
            let result = match tokio::time::timeout(
                DRAIN_RETRY_TIMEOUT,
                self.inner.update_message(&message_id, &card),
            )
            .await
            {
                Ok(result) => result,
                Err(_) => Err(crate::error::BridgeError::Io(std::io::Error::new(
                    std::io::ErrorKind::TimedOut,
                    "card update retry timed out",
                ))),
            };
            let mut state = self.state.lock().unwrap();
            // The entry could still have left the set under cap eviction while
            // the retry was in flight; its outcome belongs to the payload it
            // carried, not to a newer state.
            let still_current = state
                .entries
                .get(&message_id)
                .is_some_and(|current| current.seq == entry.seq && current.card.is_some());
            if !still_current {
                Self::prune_locks(&mut state);
                continue;
            }
            match result {
                Ok(()) => {
                    // Keep the sequence as a settled tombstone: an outcome that
                    // is not newer than the stored one must not re-register
                    // behind it. `delivered` is what the Rendered Cursor's
                    // drain reconcile confirms against, for this exact
                    // sequence (spec #561).
                    if let Some(current) = state.entries.get_mut(&message_id) {
                        current.card = None;
                        current.delivered = true;
                    }
                    if entry.attempts > 0 {
                        tracing::info!(
                            "pending card update for {message_id} delivered after {} retries",
                            entry.attempts
                        );
                    }
                }
                Err(e) if e.is_recoverable_card_write() => {
                    if let Some(current) = state.entries.get_mut(&message_id) {
                        current.attempts += 1;
                        current.next_attempt = tokio::time::Instant::now() + self.backoff(current.attempts);
                    }
                    tracing::debug!("pending card update for {message_id} still failing: {e}");
                }
                Err(e) => {
                    // A permanent refusal is settled too: the tombstone keeps
                    // an older slow failure from re-registering, and records
                    // that nothing was delivered (spec #561).
                    if let Some(current) = state.entries.get_mut(&message_id) {
                        current.card = None;
                        current.delivered = false;
                    }
                    tracing::warn!("pending card update for {message_id} dropped: {e}");
                }
            }
            Self::prune_locks(&mut state);
        }
    }

    fn has_pending_card_update(&self, message_id: &str) -> bool {
        self.pending(message_id)
    }

    fn pending_card_write(&self, message_id: &str, card: &Value) -> Option<u64> {
        self.state
            .lock()
            .unwrap()
            .entries
            .get(message_id)
            .and_then(|entry| (entry.card.as_ref() == Some(card)).then_some(entry.seq))
    }

    fn card_write_delivered(&self, message_id: &str, seq: u64) -> bool {
        // ONLY this exact sequence's own verdict (spec #561, review #569): a
        // newer write's delivery — a cached repaint that may omit this
        // payload's delta — must never answer for it.
        self.state
            .lock()
            .unwrap()
            .entries
            .get(message_id)
            .is_some_and(|entry| entry.seq == seq && entry.card.is_none() && entry.delivered)
    }

    fn failed_card_write(&self, message_id: &str, card: &Value) -> Option<u64> {
        // The sequence of the card's most recent RECOVERABLE failure, when the
        // payload it carried is exactly `card` (spec #561, review #569): the
        // failure note asks about ITS OWN write, whose sequence a newer write
        // may have replaced in the entry.
        self.state
            .lock()
            .unwrap()
            .failed
            .get(message_id)
            .and_then(|(seq, payload)| (payload == card).then_some(*seq))
    }

    fn settled_card_write_delivered(&self, message_id: &str) -> Option<bool> {
        let state = self.state.lock().unwrap();
        let entry = state.entries.get(message_id)?;
        entry.card.is_none().then_some(entry.delivered)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::BridgeError;
    use std::collections::VecDeque;

    /// A scripted `update_message` result: the tests queue one per attempt.
    #[derive(Clone)]
    enum Fail {
        Transport,
        ContentRejected,
        Http(u16),
    }

    /// A one-shot mid-call park: `entered` fires when the call is in flight,
    /// `release` lets it finish.
    struct Gate {
        entered: Arc<tokio::sync::Notify>,
        release: Arc<tokio::sync::Notify>,
    }

    /// Records every `update_message` attempt and serves a queued script;
    /// an empty script means success. Every other Platform method is
    /// unreachable in these tests.
    struct FakePlatform {
        attempts: Mutex<Vec<(String, Value)>>,
        script: Mutex<VecDeque<Fail>>,
        gate: Mutex<Option<Gate>>,
    }

    impl FakePlatform {
        fn new() -> Self {
            Self {
                attempts: Mutex::new(Vec::new()),
                script: Mutex::new(VecDeque::new()),
                gate: Mutex::new(None),
            }
        }

        fn fail_next(&self, fail: Fail) {
            self.script.lock().unwrap().push_back(fail);
        }

        /// Park the next `update_message` call, returning `(entered, release)`.
        fn park_next(&self) -> (Arc<tokio::sync::Notify>, Arc<tokio::sync::Notify>) {
            let entered = Arc::new(tokio::sync::Notify::new());
            let release = Arc::new(tokio::sync::Notify::new());
            *self.gate.lock().unwrap() = Some(Gate {
                entered: entered.clone(),
                release: release.clone(),
            });
            (entered, release)
        }

        fn attempts(&self) -> Vec<(String, Value)> {
            self.attempts.lock().unwrap().clone()
        }
    }

    #[async_trait]
    impl Platform for FakePlatform {
        async fn update_message(&self, message_id: &str, card: &Value) -> Result<()> {
            self.attempts
                .lock()
                .unwrap()
                .push((message_id.to_string(), card.clone()));
            let gate = self.gate.lock().unwrap().take();
            if let Some(gate) = gate {
                gate.entered.notify_one();
                gate.release.notified().await;
            }
            match self.script.lock().unwrap().pop_front() {
                None => Ok(()),
                Some(Fail::Transport) => Err(BridgeError::Io(std::io::Error::other("transport down"))),
                Some(Fail::ContentRejected) => Err(BridgeError::CardContentRejected {
                    code: 230099,
                    detail: "rejected".into(),
                }),
                Some(Fail::Http(status)) => Err(BridgeError::FeishuHttp {
                    status,
                    detail: format!("update message HTTP {status}"),
                }),
            }
        }

        async fn get_ws_endpoint(&self) -> Result<String> {
            unimplemented!("not used")
        }
        async fn reply_card(&self, _reply_to: &str, _card: &Value) -> Result<String> {
            unimplemented!("not used")
        }
        async fn send_card(
            &self,
            _receive_id_type: &str,
            _receive_id: &str,
            _card: &Value,
        ) -> Result<String> {
            unimplemented!("not used")
        }
        async fn reply_text(&self, _message_id: &str, _text: &str) -> Result<String> {
            unimplemented!("not used")
        }
        async fn reply_card_in_thread(
            &self,
            _message_id: &str,
            _card: &Value,
        ) -> Result<(String, Option<String>)> {
            unimplemented!("not used")
        }
        async fn reply_in_thread(&self, _message_id: &str, _text: &str) -> Result<(String, Option<String>)> {
            unimplemented!("not used")
        }
        async fn reply_completion_notice(
            &self,
            _message_id: &str,
            _open_id: &str,
            _name: Option<&str>,
            _text: &str,
        ) -> Result<String> {
            unimplemented!("not used")
        }
        async fn user_name(&self, _open_id: &str) -> Result<Option<String>> {
            unimplemented!("not used")
        }
        async fn chat_name(&self, _chat_id: &str) -> Result<Option<String>> {
            unimplemented!("not used")
        }
        async fn bot_open_id(&self) -> Result<String> {
            unimplemented!("not used")
        }
        async fn list_messages(
            &self,
            _container_id_type: &str,
            _container_id: &str,
        ) -> Result<Vec<ChatMessage>> {
            unimplemented!("not used")
        }
        async fn get_message(&self, _message_id: &str) -> Result<FeishuMessage> {
            unimplemented!("not used")
        }
        async fn get_card_view(&self, _message_id: &str) -> Result<Value> {
            unimplemented!("not used")
        }
        async fn download_image(&self, _message_id: &str, _image_key: &str) -> Result<ImageAttachment> {
            unimplemented!("not used")
        }
        async fn set_instant_reminder(
            &self,
            _chat_id: &str,
            _is_group: bool,
            _user_ids: &[String],
            _on: bool,
        ) -> Result<()> {
            unimplemented!("not used")
        }
        async fn pin_message(&self, _message_id: &str) -> Result<()> {
            unimplemented!("not used")
        }
        async fn unpin_message(&self, _message_id: &str) -> Result<()> {
            unimplemented!("not used")
        }
    }

    #[tokio::test]
    async fn a_failed_update_is_retried_on_drain() {
        let inner = Arc::new(FakePlatform::new());
        inner.fail_next(Fail::Transport);
        let delivery = CardDelivery::new(inner.clone());
        let card = serde_json::json!({ "body": "first" });

        let result = delivery.update_message("om_1", &card).await;
        assert!(result.is_err(), "the scripted failure surfaces to the caller");
        assert!(delivery.pending("om_1"), "the failure is remembered");

        delivery.drain_pending_card_updates(true).await;

        let attempts = inner.attempts();
        assert_eq!(attempts.len(), 2, "the drain retried once");
        assert_eq!(
            attempts[1],
            ("om_1".to_string(), card.clone()),
            "the newest payload went out"
        );
        assert!(!delivery.pending("om_1"), "a delivered retry clears the entry");
    }

    /// Spec #561: the Rendered Cursor's tie to the outbox. Only the owed
    /// payload's exact write reports a pending sequence, and only that
    /// sequence's delivered outcome confirms it — a refusal or a different
    /// payload/sequence never does.
    #[tokio::test]
    async fn the_pending_write_tie_reports_only_this_payloads_delivery() {
        let inner = Arc::new(FakePlatform::new());
        let delivery = CardDelivery::new(inner.clone());
        let card = serde_json::json!({ "body": "owed" });
        let other = serde_json::json!({ "body": "other" });

        inner.fail_next(Fail::Transport);
        let _ = delivery.update_message("om_1", &card).await;
        let seq = delivery
            .pending_card_write("om_1", &card)
            .expect("the owed payload is tied to its sequence");
        assert_eq!(
            delivery.pending_card_write("om_1", &other),
            None,
            "a different payload has no tie"
        );
        assert!(
            !delivery.card_write_delivered("om_1", seq),
            "a still-owed write is not a delivery"
        );

        delivery.drain_pending_card_updates(true).await;
        assert!(
            delivery.card_write_delivered("om_1", seq),
            "the drain's delivery confirms exactly that sequence"
        );
        assert!(
            !delivery.card_write_delivered("om_1", seq + 1),
            "another sequence is never confirmed"
        );

        // A permanent refusal settles without a delivery.
        inner.fail_next(Fail::ContentRejected);
        let _ = delivery.update_message("om_1", &other).await;
        assert!(
            !delivery.card_write_delivered("om_1", seq + 1),
            "a refused write is never a delivery"
        );
    }

    /// A newer write never answers for an OLDER sequence (spec #561, review
    /// #569): a failure note asks about its own payload's sequence, because a
    /// newer write — a cached repaint that may omit the older payload's delta —
    /// must never advance that payload's staged cursor. The failure memory
    /// names only the payload that actually failed.
    #[tokio::test]
    async fn a_newer_write_never_answers_for_an_older_sequence() {
        let inner = Arc::new(FakePlatform::new());
        let delivery = CardDelivery::new(inner.clone());
        let first = serde_json::json!({ "body": "first" });
        let second = serde_json::json!({ "body": "second" });

        inner.fail_next(Fail::Transport);
        let _ = delivery.update_message("om_1", &first).await;
        let seq = delivery
            .pending_card_write("om_1", &first)
            .expect("the owed payload is tied to its sequence");
        delivery.drain_pending_card_updates(true).await;
        assert!(
            delivery.card_write_delivered("om_1", seq),
            "the drained write delivered"
        );
        assert_eq!(
            delivery.failed_card_write("om_1", &first),
            Some(seq),
            "the failure memory names the failed payload's sequence"
        );

        // A newer write fails: the entry now names another sequence, so the
        // delivered one is no longer answered for — only the newer payload's
        // own sequence is.
        inner.fail_next(Fail::Transport);
        let _ = delivery.update_message("om_1", &second).await;
        assert!(
            !delivery.card_write_delivered("om_1", seq),
            "a newer write's outcome never answers for the older sequence"
        );
        assert!(
            !delivery.card_write_delivered("om_1", seq + 1),
            "the owed newer write is not a delivery"
        );
        assert_eq!(
            delivery.failed_card_write("om_1", &first),
            None,
            "the failure memory moved on to the newer payload"
        );
        assert_eq!(
            delivery.failed_card_write("om_1", &second),
            Some(seq + 1),
            "the newer payload's own failure is the one remembered"
        );
    }

    /// The newest failed payload wins: an older failure never overwrites it,
    /// and the drain re-sends only that one.
    #[tokio::test]
    async fn the_newest_failed_payload_wins() {
        let inner = Arc::new(FakePlatform::new());
        inner.fail_next(Fail::Transport);
        inner.fail_next(Fail::Transport);
        let delivery = CardDelivery::new(inner.clone());
        let first = serde_json::json!({ "body": "first" });
        let second = serde_json::json!({ "body": "second" });

        let _ = delivery.update_message("om_1", &first).await;
        let _ = delivery.update_message("om_1", &second).await;
        assert!(delivery.pending("om_1"));

        delivery.drain_pending_card_updates(true).await;

        let attempts = inner.attempts();
        assert_eq!(attempts.len(), 3, "two failures, one retry");
        assert_eq!(
            attempts[2],
            ("om_1".to_string(), second.clone()),
            "the newest payload went out, not the first"
        );
        assert!(!delivery.pending("om_1"));
    }

    /// The card's writers are serialized: a drain retry in flight blocks a
    /// newer write, so the newer payload always lands at Feishu last — the
    /// ADR-0067 order (ADR-0038's write serialization, extended to the outbox
    /// writer). Without the lock the newer write would overtake the retry and
    /// the older payload would be the card's final state.
    #[tokio::test]
    async fn a_drain_retry_serializes_with_a_newer_write() {
        let inner = Arc::new(FakePlatform::new());
        inner.fail_next(Fail::Transport);
        let delivery = Arc::new(CardDelivery::new(inner.clone()));
        let old = serde_json::json!({ "body": "old" });
        let new = serde_json::json!({ "body": "new" });
        let _ = delivery.update_message("om_1", &old).await;
        assert!(delivery.pending("om_1"));

        let (entered, release) = inner.park_next(); // the retry will succeed
        let retrying = tokio::spawn({
            let delivery = Arc::clone(&delivery);
            async move { delivery.drain_pending_card_updates(true).await }
        });
        entered.notified().await; // the retry is parked mid-flight

        let writing = tokio::spawn({
            let delivery = Arc::clone(&delivery);
            let new = new.clone();
            async move { delivery.update_message("om_1", &new).await }
        });
        // The newer write must not reach the wire while the retry is in
        // flight: it waits on the card's delivery lock.
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert_eq!(
            inner.attempts().len(),
            2,
            "the newer write waits for the retry to land"
        );

        release.notify_one();
        retrying.await.unwrap();
        writing.await.unwrap().unwrap();
        let attempts = inner.attempts();
        assert_eq!(
            attempts.last(),
            Some(&("om_1".to_string(), new.clone())),
            "the newest write landed last"
        );
        assert!(!delivery.pending("om_1"));
    }

    /// A hung retry gives up at the bound: the payload stays owed, the card's
    /// delivery lock is released, and a newer write can proceed.
    #[tokio::test(start_paused = true)]
    async fn a_hung_retry_times_out_and_keeps_the_payload_pending() {
        let inner = Arc::new(FakePlatform::new());
        inner.fail_next(Fail::Transport);
        let delivery = Arc::new(CardDelivery::new(inner.clone()));
        let old = serde_json::json!({ "body": "old" });
        let new = serde_json::json!({ "body": "new" });
        let _ = delivery.update_message("om_1", &old).await;

        let (entered, _never_released) = inner.park_next();
        let retrying = tokio::spawn({
            let delivery = Arc::clone(&delivery);
            async move { delivery.drain_pending_card_updates(true).await }
        });
        entered.notified().await;

        // The paused clock elapses the retry bound; the drain must return.
        tokio::time::timeout(std::time::Duration::from_secs(120), retrying)
            .await
            .expect("the hung retry must time out, not hang the drain")
            .unwrap();
        assert!(delivery.pending("om_1"), "the payload is still owed");

        // The lock is free again: a newer write goes through and settles.
        delivery.update_message("om_1", &new).await.unwrap();
        assert!(!delivery.pending("om_1"));
    }

    /// A content rejection is deterministic — it never enters the set and is
    /// never retried.
    #[tokio::test]
    async fn a_content_rejection_is_never_retried() {
        let inner = Arc::new(FakePlatform::new());
        inner.fail_next(Fail::ContentRejected);
        let delivery = CardDelivery::new(inner.clone());
        let card = serde_json::json!({ "body": "rejected" });

        let result = delivery.update_message("om_1", &card).await;
        assert!(result.is_err());
        assert!(!delivery.pending("om_1"), "a rejected card never enters the set");

        delivery.drain_pending_card_updates(true).await;
        assert_eq!(inner.attempts().len(), 1, "nothing was retried");
    }

    /// A non-recoverable HTTP status (a 4xx other than 408/429) is permanent:
    /// it supersedes and drops an older pending entry instead of retrying.
    #[tokio::test]
    async fn a_permanent_http_refusal_drops_the_older_pending_state() {
        let inner = Arc::new(FakePlatform::new());
        inner.fail_next(Fail::Transport);
        inner.fail_next(Fail::Http(400));
        let delivery = CardDelivery::new(inner.clone());
        let first = serde_json::json!({ "body": "first" });
        let second = serde_json::json!({ "body": "second" });

        let _ = delivery.update_message("om_1", &first).await;
        assert!(delivery.pending("om_1"));
        let _ = delivery.update_message("om_1", &second).await;
        assert!(
            !delivery.pending("om_1"),
            "the newer permanent refusal supersedes the older pending payload"
        );

        delivery.drain_pending_card_updates(true).await;
        assert_eq!(inner.attempts().len(), 2, "no retry followed");
    }

    /// A 5xx is recoverable: the entry stays and the next drain retries it.
    #[tokio::test]
    async fn a_5xx_is_recoverable() {
        let inner = Arc::new(FakePlatform::new());
        inner.fail_next(Fail::Http(503));
        let delivery = CardDelivery::new(inner.clone());
        let card = serde_json::json!({ "body": "first" });

        let _ = delivery.update_message("om_1", &card).await;
        assert!(delivery.pending("om_1"), "a 503 is retryable");

        delivery.drain_pending_card_updates(true).await;
        assert_eq!(inner.attempts().len(), 2);
        assert!(!delivery.pending("om_1"));
    }

    /// The set is bounded: once the cap is reached the oldest pending state is
    /// evicted, so the newest cards always converge.
    #[tokio::test]
    async fn the_cap_evicts_the_oldest_pending_state() {
        let inner = Arc::new(FakePlatform::new());
        let delivery = CardDelivery::with_limits(inner.clone(), 2, BACKOFF_BASE, BACKOFF_MAX);
        for id in ["om_1", "om_2", "om_3"] {
            inner.fail_next(Fail::Transport);
            let _ = delivery
                .update_message(id, &serde_json::json!({ "body": id }))
                .await;
        }

        assert!(!delivery.pending("om_1"), "the oldest was evicted");
        assert!(
            delivery.pending("om_2") && delivery.pending("om_3"),
            "the newest two stay"
        );

        delivery.drain_pending_card_updates(true).await;
        assert_eq!(
            inner.attempts().len(),
            5,
            "three failures plus two retries (om_1 was evicted)"
        );
    }

    /// Settled tombstones never crowd out an undelivered payload: under the
    /// cap, the oldest tombstone leaves first.
    #[tokio::test]
    async fn the_cap_evicts_tombstones_before_pending_payloads() {
        let inner = Arc::new(FakePlatform::new());
        let delivery = CardDelivery::with_limits(inner.clone(), 2, BACKOFF_BASE, BACKOFF_MAX);
        inner.fail_next(Fail::Transport);
        let owed = serde_json::json!({ "body": "owed" });
        let _ = delivery.update_message("om_pending", &owed).await;
        // Two newer successful writes settle into tombstones.
        delivery
            .update_message("om_a", &serde_json::json!({ "body": "a" }))
            .await
            .unwrap();
        delivery
            .update_message("om_b", &serde_json::json!({ "body": "b" }))
            .await
            .unwrap();

        assert!(
            delivery.pending("om_pending"),
            "the owed payload survived the cap"
        );
        delivery.drain_pending_card_updates(true).await;
        assert_eq!(
            inner.attempts().last(),
            Some(&("om_pending".to_string(), owed)),
            "and was delivered"
        );
    }

    /// The Session Sync pass drain retries only what is due; the backoff grows
    /// with each failed retry.
    #[tokio::test(start_paused = true)]
    async fn the_pass_drain_waits_for_the_backoff() {
        let inner = Arc::new(FakePlatform::new());
        inner.fail_next(Fail::Transport);
        let delivery = CardDelivery::with_limits(
            inner.clone(),
            MAX_PENDING,
            Duration::from_secs(5),
            Duration::from_secs(20),
        );
        let card = serde_json::json!({ "body": "first" });
        let _ = delivery.update_message("om_1", &card).await;

        delivery.drain_pending_card_updates(false).await;
        assert_eq!(inner.attempts().len(), 1, "not due yet");

        tokio::time::advance(Duration::from_secs(5)).await;
        delivery.drain_pending_card_updates(false).await;
        assert_eq!(inner.attempts().len(), 2, "due after the base backoff");
        assert!(!delivery.pending("om_1"));

        // A failed retry doubles the delay: 10s after the failure, not due;
        // at 10s exactly, due again.
        inner.fail_next(Fail::Transport);
        let _ = delivery.update_message("om_1", &card).await;
        inner.fail_next(Fail::Transport); // the forced retry fails too
        delivery.drain_pending_card_updates(true).await; // attempts = 1
        delivery.drain_pending_card_updates(false).await;
        assert_eq!(inner.attempts().len(), 4, "not due at the doubled backoff yet");
        tokio::time::advance(Duration::from_secs(10)).await;
        delivery.drain_pending_card_updates(false).await;
        assert_eq!(inner.attempts().len(), 5, "due at the doubled backoff");
        assert!(!delivery.pending("om_1"));
    }

    /// A forced drain (the WS reconnect) ignores the backoff and tries at once.
    #[tokio::test(start_paused = true)]
    async fn a_forced_drain_ignores_the_backoff() {
        let inner = Arc::new(FakePlatform::new());
        inner.fail_next(Fail::Transport);
        let delivery = CardDelivery::new(inner.clone());
        let _ = delivery
            .update_message("om_1", &serde_json::json!({ "body": "first" }))
            .await;

        delivery.drain_pending_card_updates(true).await;
        assert_eq!(
            inner.attempts().len(),
            2,
            "the reconnect drain retried immediately"
        );
        assert!(!delivery.pending("om_1"));
    }
}
