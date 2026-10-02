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

/// One card's newest payload whose delivery failed and has not been
/// superseded.
#[derive(Clone)]
struct PendingEntry {
    /// The monotonic write sequence this payload was sent with. A retry may
    /// clear the entry only while it still names this sequence.
    seq: u64,
    /// The newest card JSON.
    card: Value,
    /// Failed retry attempts so far — the backoff exponent.
    attempts: u32,
    /// The earliest instant the next retry may go out (unless forced).
    next_attempt: tokio::time::Instant,
}

#[derive(Default)]
struct State {
    entries: HashMap<String, PendingEntry>,
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
        self.state.lock().unwrap().entries.contains_key(message_id)
    }

    /// Fold one write's outcome into the pending set. A delivery (or a
    /// permanent refusal) of sequence `seq` clears whatever entry `seq` or an
    /// older write left; a recoverable failure records the payload as the
    /// card's newest pending state, never displacing a newer one.
    fn observe(&self, message_id: &str, seq: u64, card: &Value, result: &Result<()>) {
        let mut state = self.state.lock().unwrap();
        let superseded = match state.entries.get(message_id) {
            Some(entry) => entry.seq <= seq,
            None => true,
        };
        match result {
            Ok(()) => {
                if superseded {
                    state.entries.remove(message_id);
                }
            }
            Err(e) if e.is_recoverable_card_write() => {
                let newest = match state.entries.get(message_id) {
                    Some(entry) => entry.seq < seq,
                    None => true,
                };
                if newest {
                    state.entries.insert(
                        message_id.to_string(),
                        PendingEntry {
                            seq,
                            card: card.clone(),
                            attempts: 0,
                            next_attempt: tokio::time::Instant::now() + self.backoff_base,
                        },
                    );
                    self.evict_over_cap(&mut state);
                }
            }
            // A permanent refusal (a rejected card, a gone message, a
            // non-recoverable 4xx) can never deliver: drop whatever this
            // write superseded instead of retrying it forever.
            Err(_) => {
                if superseded {
                    state.entries.remove(message_id);
                }
            }
        }
    }

    /// Keep the set under the cap, evicting the oldest pending state first
    /// (the lowest sequence) with a WARN naming what was lost.
    fn evict_over_cap(&self, state: &mut State) {
        while state.entries.len() > self.max_pending {
            let Some(oldest) = state
                .entries
                .iter()
                .min_by_key(|(_, entry)| entry.seq)
                .map(|(message_id, _)| message_id.clone())
            else {
                return;
            };
            state.entries.remove(&oldest);
            tracing::warn!(
                "pending card update for {oldest} evicted: {} pending cards reached",
                self.max_pending
            );
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
                .filter(|(_, entry)| force || entry.next_attempt <= now)
                .map(|(message_id, entry)| (message_id.clone(), entry.clone()))
                .collect()
        };
        for (message_id, entry) in due {
            let result = self.inner.update_message(&message_id, &entry.card).await;
            let mut state = self.state.lock().unwrap();
            // A write raced this retry and replaced the entry (a newer failure
            // or a newer delivery): the in-flight retry's outcome belongs to
            // the payload it carried, never to the newer state.
            let still_current = state
                .entries
                .get(&message_id)
                .is_some_and(|current| current.seq == entry.seq);
            if !still_current {
                continue;
            }
            match result {
                Ok(()) => {
                    state.entries.remove(&message_id);
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
                    state.entries.remove(&message_id);
                    tracing::warn!("pending card update for {message_id} dropped: {e}");
                }
            }
        }
    }

    fn has_pending_card_update(&self, message_id: &str) -> bool {
        self.pending(message_id)
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

    /// Records every `update_message` attempt and serves a queued script;
    /// an empty script means success. Every other Platform method is
    /// unreachable in these tests.
    struct FakePlatform {
        attempts: Mutex<Vec<(String, Value)>>,
        script: Mutex<VecDeque<Fail>>,
        /// A one-shot mid-call park: the next `update_message` signals
        /// `entered` and waits for `release` before deciding its result.
        gate: Mutex<Option<(Arc<tokio::sync::Notify>, Arc<tokio::sync::Notify>)>>,
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
            *self.gate.lock().unwrap() = Some((entered.clone(), release.clone()));
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
            if let Some((entered, release)) = gate {
                entered.notify_one();
                release.notified().await;
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

    /// A slow failed retry must never resurrect the payload it carried: while
    /// it is in flight a newer failure replaces the entry, and the retry's
    /// success leaves that newer entry alone.
    #[tokio::test]
    async fn a_slow_retry_never_resurrects_a_superseded_payload() {
        let inner = Arc::new(FakePlatform::new());
        inner.fail_next(Fail::Transport);
        let delivery = Arc::new(CardDelivery::new(inner.clone()));
        let first = serde_json::json!({ "body": "first" });
        let second = serde_json::json!({ "body": "second" });
        let _ = delivery.update_message("om_1", &first).await;

        let (entered, release) = inner.park_next();
        let draining = tokio::spawn({
            let delivery = Arc::clone(&delivery);
            async move { delivery.drain_pending_card_updates(true).await }
        });
        entered.notified().await; // the retry is parked mid-flight

        inner.fail_next(Fail::Transport);
        let _ = delivery.update_message("om_1", &second).await;
        release.notify_one();
        draining.await.unwrap();

        assert!(delivery.pending("om_1"), "the newer failure stays pending");
        delivery.drain_pending_card_updates(true).await;
        let attempts = inner.attempts();
        assert_eq!(
            attempts.last(),
            Some(&("om_1".to_string(), second.clone())),
            "the newer payload is what converges"
        );
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
