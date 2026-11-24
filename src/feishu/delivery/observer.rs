//! The delivery contract's one owner (charter C7, ticket #1139; ADR-0067,
//! ADR-0072): the keyed submission queue, the Pending Card Update drain and
//! every delivery query. `Platform` reaches it through `Platform::delivery`;
//! [`CardDelivery`] is the production implementor and [`Unobserved`] is the one
//! documented no-op a platform gets when it does not implement the trait.
//!
//! The rules themselves — stale-generation drop, the settled
//! `(generation, intent)` collapse, the owed/retry schedule, newest-wins, the
//! caps and the fallback-under-the-same-lock ordering — are the state
//! machine's in [`crate::feishu::delivery`]; this module only declares the
//! contract, its vocabulary, and the adapter from the trait to that state.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use serde_json::Value;
use tokio::sync::oneshot;

use super::{CardDelivery, KEYED_TICKET_AWAIT, KeylessRetry, LockMode};
use crate::error::BridgeError;
use crate::feishu::{DeferredNotice, NoticeGate};

/// Which logical write a keyed submission is (spec #571). The intent is the
/// second half of the ordering key: `(generation, intent)` names one logical
/// write, so a writer's per-tick re-decision collapses to its newest payload
/// — or, once the key settled, is dropped — instead of writing again.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) enum CardWriteIntent {
    /// The restart stamp (#443, ADR-0063): mark a card a cola restart
    /// orphaned while its session still reads live.
    Stamp,
    /// The **ordinary collect** — the reap's successor collect, the Wake
    /// continuation's arm, the external arm: bring an orphaned card to its
    /// taken-over state keeping its whole preserved body
    /// (`KeepBody::Everything`).
    Collect,
    /// The **fresh-Turn message takeover's collect** (ADR-0068): the same
    /// taken-over card, but the orphan's live tail — the Background Task
    /// Ledger and the running panels the successor carried — leaves with the
    /// successor (`KeepBody::WithoutLiveTail`). A different logical write from
    /// [`Self::Collect`] (spec #571 review): the two may share a generation
    /// and must both land, the strip collect last.
    TakeoverCollect,
    /// The **late projection's collect** (spec #561, review #569): the late
    /// card is reduced to the bare taken-over marker (`KeepBody::Nothing`)
    /// because the winning Turn already re-rendered its tail. A different
    /// logical write again (spec #571 review).
    LateProjectionCollect,
    /// The ending settle (spec #571's amendment): the terminal card's ending
    /// write. Accepting one at generation G **closes** G — the card's floor
    /// rises to G+1 — so a stamp whose pre-submission read outlived the
    /// ending is dropped instead of painting 「已重启」 over the ✅.
    Settle,
    /// The waiting yield (「⏳ 等待后台任务」, spec #571's amendment 2): the
    /// Waiting ending, which is not the terminal one. Accepting one at
    /// generation G **shadows** G — a later `Stamp` at ≤ G is dropped — but it
    /// does **not** close the generation: the true end's `Settle` still lands
    /// after it and owns the card's last word.
    Yield,
}

/// One **keyed submission**: an already-composed card payload with the ordering
/// key its writer decided under (spec #571) — the **chain generation** the
/// decision read, and the intent naming the logical write.
pub(crate) struct KeyedSubmission<'a> {
    /// The card this write targets.
    pub message_id: &'a str,
    /// The chain generation the writer's decision read (ticket #572).
    pub generation: u64,
    /// Which logical write this is.
    pub intent: CardWriteIntent,
    /// The composed payload.
    pub card: &'a Value,
    /// The composed **fallback** payload, tried by the driver when the primary
    /// is refused as card content (a typed `CardContentRejected`, e.g.
    /// `230099`) — under the same held card lock and the same
    /// `(generation, intent)` key (spec #571 review), so a degradation can
    /// never jump the queue's ordering. `None` for a write with no degradation
    /// (the stamp, a collect whose bare ending is only the read fallback).
    pub fallback: Option<&'a Value>,
}

/// What became of one keyed submission — its [`CardWriteTicket`]'s verdict.
pub(crate) enum WriteOutcome {
    /// The card holds this logical write: this submission's own payload
    /// reached Feishu, or the same key had already delivered.
    Delivered,
    /// This submission's own attempt failed. A recoverable failure stays owed
    /// in the queue and is retried with backoff; a permanent refusal settles
    /// the key, so later same-key submissions are dropped without a call.
    Failed(BridgeError),
    /// Nothing was written for this submission: a newer submission owns the
    /// card, its generation was stale, or its key was permanently refused.
    Superseded,
}

// Still hand-written rather than derived: it reads the failure the variant
// carries, which the derived impl would leave as an unread field — and the
// writers now log that failure (the stamp's and the collect's own WARN lines).
impl std::fmt::Debug for WriteOutcome {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Delivered => write!(f, "Delivered"),
            Self::Failed(error) => write!(f, "Failed({error})"),
            Self::Superseded => write!(f, "Superseded"),
        }
    }
}

/// The completion ticket of one keyed submission (spec #571, decision 7):
/// callers that need the write's own timing await [`Self::settled`]; a
/// fire-and-forget caller may drop it — the queue owns the write either way.
pub(crate) struct CardWriteTicket {
    rx: oneshot::Receiver<WriteOutcome>,
}

impl CardWriteTicket {
    fn pending(rx: oneshot::Receiver<WriteOutcome>) -> Self {
        Self { rx }
    }

    /// A ticket already settled — [`Unobserved`]'s visible-failure ticket, and
    /// the same-key/settled-key answers that never reach the wire.
    pub(crate) fn settled_now(outcome: WriteOutcome) -> Self {
        let (ticket, rx) = oneshot::channel();
        let _ = ticket.send(outcome);
        Self { rx }
    }

    /// Await this submission's own outcome. A ticket whose queue state was
    /// forgotten (the entry evicted under the cap) reports
    /// [`WriteOutcome::Superseded`]: nothing will be written for it.
    pub(crate) async fn settled(self) -> WriteOutcome {
        self.rx.await.unwrap_or(WriteOutcome::Superseded)
    }

    /// Await this submission's own outcome for at most `bound` (spec #571
    /// review): `None` when the bound elapses first. An issued keyed write is
    /// never cancelled, so a slow one can outlive its caller — the caller must
    /// then proceed without a verdict and treat `None` as indeterminate: the
    /// write stays owned by the queue and may still land. Dropping the ticket
    /// does not cancel the write either; the driver task owns it.
    pub(crate) async fn settled_within(self, bound: Duration) -> Option<WriteOutcome> {
        tokio::time::timeout(bound, self.settled()).await.ok()
    }
}

/// The one owner of the card-delivery contract (charter C7, ticket #1139):
/// ordered keyed submissions, the Pending Card Update drain and every delivery
/// query. [`CardDelivery`] implements it; a platform reaches it through
/// `Platform::delivery`, whose default is [`UNOBSERVED`].
///
/// Every method is required — there is no default delivery behaviour left
/// anywhere: a platform either implements this trait or gets [`Unobserved`],
/// which cannot quietly deliver a write differently.
#[async_trait]
pub trait DeliveryObserver: Send + Sync {
    /// Submit a **keyed submission** for ordered delivery (spec #571): an
    /// already-composed payload carrying the **chain generation** its writer
    /// decided under and the intent naming the logical write, with an optional
    /// **fallback** payload for a platform that refuses the primary as card
    /// content (spec #571 review). The delivery
    /// decorator serializes it against the card's other writers — dropping a
    /// stale generation, collapsing a settled `(generation, intent)`, keeping a
    /// recoverable failure owed, trying the fallback inside the same key — and
    /// returns the submission's completion ticket.
    async fn submit_ordered(&self, submission: KeyedSubmission<'_>) -> CardWriteTicket;

    /// Whether the keyed queue already **covers** `(generation, intent)` for
    /// `message_id` (spec #571): the generation is closed (a settle ended it),
    /// a `Stamp` at or below an accepted ending's generation is shadowed (the
    /// waiting yield / the settle is the card's state), the key settled —
    /// delivered, so it is on the card, or permanently refused, so it is final
    /// (#522) — or a write of the exact key is already in flight or waiting in
    /// the queue. Either way a writer's re-decision owes no work, and the card
    /// must not be read again for it. A recoverable failure's payload is left
    /// **owed** with no driver: a later re-decision may still replace it (and
    /// the drain retries it), so that reports `false`. The query only
    /// remembers; it never orders or claims anything, so consulting it before
    /// a read cannot resurrect the retired in-flight claim.
    fn keyed_write_covered(&self, message_id: &str, generation: u64, intent: CardWriteIntent) -> bool;

    /// How long a writer awaits an issued keyed submission's completion ticket
    /// before proceeding without a verdict (spec #571 review): an issued keyed
    /// write is **never cancelled** — a cancelled PATCH could still commit at
    /// Feishu and land over a newer generation — so a slow or hung one can
    /// outlive its caller. The writer bounds its own wait instead and then
    /// proceeds, leaving the write owned by the queue, which may still land it.
    /// A decorator forwards this to the platform it wraps, so a test fake may
    /// shorten it; production uses the delivery layer's bound.
    fn keyed_ticket_await(&self) -> Duration;

    /// Whether `(generation, intent)` already **settled** on `message_id`'s
    /// keyed queue (spec #571 review): `Some(true)` delivered — the card holds
    /// the logical write — `Some(false)` permanently refused, so it will never
    /// write again (#522), and `None` unsettled. A writer that re-decides every
    /// tick consults this before its card-view read, so a settled key costs no
    /// read and no submission. The query only reports.
    fn keyed_write_settled(&self, message_id: &str, generation: u64, intent: CardWriteIntent)
    -> Option<bool>;

    /// Retry the Pending Card Updates that are due now (ADR-0067). The Session
    /// Sync pass calls this every tick, and the WS loop calls it with `force`
    /// after a reconnect so a restored REST path converges immediately.
    async fn drain_pending_card_updates(&self, force: bool);

    /// Whether `message_id` still owes an undelivered write — the Live Card
    /// record's removal consults this (ADR-0063 amendment): a terminal card's
    /// record stays until its ending is confirmed. Both write classes count
    /// (spec #571's amendment): the keyless Pending Card Update and a keyed
    /// ending write (`Settle`) the queue still holds.
    fn has_pending_card_update(&self, message_id: &str) -> bool;

    /// The sequence of `message_id`'s newest card write when it is still owed
    /// as a Pending Card Update carrying exactly `card` — the Rendered
    /// Cursor's tie to the payload a drain will retry (spec #561). `None` when
    /// nothing is owed or the newest owed payload is a different one.
    fn pending_card_write(&self, message_id: &str, card: &Value) -> Option<u64>;

    /// The sequence of `message_id`'s card write `seq` settled **delivered** —
    /// the confirmation the drain reconcile advances the Rendered Cursor on
    /// (spec #561, review #569): ONLY this exact sequence's own verdict, never
    /// a newer write's. `false` while the write is still owed, was permanently
    /// refused, or was superseded or evicted.
    fn card_write_delivered(&self, message_id: &str, seq: u64) -> bool;

    /// The sequence of `message_id`'s most recent RECOVERABLE failure when the
    /// payload it carried is exactly `card` (spec #561, review #569): the write
    /// a failure note asks about even after a newer write — a cached repaint
    /// that may omit this payload's delta — replaced the card's entry. `None`
    /// when the newest failure carried another payload.
    fn failed_card_write(&self, message_id: &str, card: &Value) -> Option<u64>;

    /// The settled delivery verdict of `message_id`'s newest card write once
    /// it is no longer owed (spec #561, review #569): `Some(true)` when the
    /// settled write delivered, `Some(false)` when it settled otherwise (a
    /// permanent refusal). `None` while a payload is still owed or the card has
    /// no entry. A failure note whose payload a drain delivered in the meantime
    /// reads `Some(true)` and may confirm its staged cursor immediately instead
    /// of discarding it.
    fn settled_card_write_delivered(&self, message_id: &str) -> Option<bool>;

    /// Gate a Completion Notice on the terminal write being **accepted** (spec
    /// #602, ticket #607): delivered now, or owed by the delivery layer's
    /// retry. `message_id` is the card carrying the terminal slice and `seq` is
    /// that terminal write's **own** keyless sequence (from
    /// [`Self::pending_card_write`] / [`Self::failed_card_write`]) — the gate
    /// binds to it, never to the entry's current newest, so a newer unrelated
    /// repaint can never answer for it. [`NoticeGate::Armed`] stores `notice` to
    /// fire once that exact write drains; [`NoticeGate::Delivered`] means it
    /// already delivered and the caller sends at once; [`NoticeGate::Never`]
    /// means it was superseded, evicted or permanently refused, so the notice is
    /// suppressed — a write that will never land must not announce the end.
    fn defer_notice_until_delivered(&self, message_id: &str, seq: u64, notice: DeferredNotice) -> NoticeGate;
}

/// The one documented no-op a `Platform` gets when it does not implement
/// [`DeliveryObserver`] (charter C7, ticket #1139), returned by
/// `Platform::delivery`'s default. No queue exists, so every query answers
/// "nothing owed / never delivered" and a keyed submission **fails visibly**
/// instead of silently taking an unordered write path.
pub(crate) struct Unobserved;

/// The shared no-op observer: one instance, never one per platform, so the
/// documented no-op exists exactly once.
pub(crate) static UNOBSERVED: Unobserved = Unobserved;

#[async_trait]
impl DeliveryObserver for Unobserved {
    async fn submit_ordered(&self, submission: KeyedSubmission<'_>) -> CardWriteTicket {
        // The visible failure: a platform that skips ordered delivery must not
        // look like it wrote the card. `BridgeError::Feishu` is a permanent
        // refusal (`is_recoverable_card_write`), so no caller retries against a
        // platform that cannot retry; every keyed caller already WARNs on it.
        tracing::warn!(
            "ordered card write for {} not delivered: this Platform does not observe delivery",
            submission.message_id
        );
        CardWriteTicket::settled_now(WriteOutcome::Failed(BridgeError::Feishu(
            "ordered delivery is unsupported by this Platform".into(),
        )))
    }

    fn keyed_write_covered(&self, _message_id: &str, _generation: u64, _intent: CardWriteIntent) -> bool {
        false
    }

    fn keyed_ticket_await(&self) -> Duration {
        KEYED_TICKET_AWAIT
    }

    fn keyed_write_settled(
        &self,
        _message_id: &str,
        _generation: u64,
        _intent: CardWriteIntent,
    ) -> Option<bool> {
        None
    }

    async fn drain_pending_card_updates(&self, _force: bool) {
        // No queue exists, so there is nothing owed to retry: this call drops
        // no write. Kept observable (not silent) for a mis-assembled platform.
        tracing::debug!("card delivery drain requested on a Platform that does not observe delivery");
    }

    fn has_pending_card_update(&self, _message_id: &str) -> bool {
        false
    }

    fn pending_card_write(&self, _message_id: &str, _card: &Value) -> Option<u64> {
        None
    }

    fn card_write_delivered(&self, _message_id: &str, _seq: u64) -> bool {
        false
    }

    fn failed_card_write(&self, _message_id: &str, _card: &Value) -> Option<u64> {
        None
    }

    fn settled_card_write_delivered(&self, _message_id: &str) -> Option<bool> {
        None
    }

    fn defer_notice_until_delivered(
        &self,
        _message_id: &str,
        _seq: u64,
        notice: DeferredNotice,
    ) -> NoticeGate {
        let _ = notice;
        // No queue can owe the terminal write: the keyless `update_message`
        // already answered the caller, and `NoticeGate` is only consulted for a
        // sequence this observer itself reported (it reports none), so the
        // ungated notice is the correct no-queue answer.
        NoticeGate::Delivered
    }
}

#[async_trait]
impl DeliveryObserver for CardDelivery {
    async fn submit_ordered(&self, submission: KeyedSubmission<'_>) -> CardWriteTicket {
        let (ticket, ticket_rx) = oneshot::channel();
        let message_id = submission.message_id.to_string();
        let drive = {
            let mut state = self.state.lock().unwrap();
            let drive = Self::admit(&mut state, submission, ticket);
            // Keyed-only traffic bounds the map like the keyless observe does
            // (spec #571 review): every admission runs the cap check, so
            // a life that only ever settles or collects cards cannot retain
            // one payload-less entry per card forever.
            self.evict_over_cap(&mut state, Some(&message_id));
            Self::prune_locks(&mut state);
            drive
        };
        if let Some(token) = drive {
            // The write is owned by the queue, not the caller: a driver task
            // outlives a cancelled submitter, so an admitted submission always
            // settles. The ticket just observes its outcome. A driver holds one
            // of the global pool's permits for its whole life (spec #571
            // review): with every permit taken by a hung write, no driver
            // starts and the submission stays owed in its entry — the next
            // drain resumes it once a permit frees.
            let inner = self.inner.clone();
            let state = self.state.clone();
            let limits = self.limits;
            match Arc::clone(&self.drivers).try_acquire_owned() {
                Ok(permit) => {
                    tokio::spawn(async move {
                        let _driver = permit;
                        Self::drive_keyed(inner, state, message_id, token, LockMode::Await, limits).await;
                    });
                }
                Err(_) => Self::release_driver(&self.state, &message_id, token),
            }
        }
        CardWriteTicket::pending(ticket_rx)
    }

    fn keyed_write_covered(&self, message_id: &str, generation: u64, intent: CardWriteIntent) -> bool {
        self.state
            .lock()
            .unwrap()
            .entries
            .get(message_id)
            .is_some_and(|entry| entry.covers(generation, intent))
    }

    fn keyed_ticket_await(&self) -> Duration {
        // The transport decides: production's client keeps the delivery
        // layer's bound, a test fake may shorten it (spec #571 review).
        self.inner.delivery().keyed_ticket_await()
    }

    fn keyed_write_settled(
        &self,
        message_id: &str,
        generation: u64,
        intent: CardWriteIntent,
    ) -> Option<bool> {
        let state = self.state.lock().unwrap();
        let entry = state.entries.get(message_id)?;
        // Only the newest generation's key states are remembered; an older
        // generation's keys were forgotten with its submissions.
        if entry.generation != generation {
            return None;
        }
        entry
            .keys
            .get(&intent)
            .map(|key| matches!(key, super::KeyState::Delivered))
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
        // Snapshot what the retries need; the state lock is never held across
        // an await.
        let keyless: Vec<KeylessRetry> = {
            let state = self.state.lock().unwrap();
            state
                .entries
                .iter()
                .filter(|(_, entry)| entry.card.is_some() && (force || entry.next_attempt <= now))
                .map(|(message_id, entry)| KeylessRetry {
                    message_id: message_id.clone(),
                    card: entry.card.clone().expect("filtered to a pending payload"),
                    lock: entry.lock.clone(),
                    seq: entry.seq,
                    attempts: entry.attempts,
                })
                .collect()
        };
        // The keyed queue's owed write: a submission whose recoverable failure
        // released its driver, due for another attempt.
        let keyed: Vec<String> = {
            let state = self.state.lock().unwrap();
            state
                .entries
                .iter()
                .filter(|(_, entry)| {
                    entry.driver.is_none()
                        && entry
                            .in_flight
                            .as_ref()
                            .is_some_and(|write| force || write.next_attempt <= now)
                })
                .map(|(message_id, _)| message_id.clone())
                .collect()
        };
        for retry in keyless {
            self.retry_pending_card_update(retry).await;
        }
        for message_id in keyed {
            self.drive_owed_keyed(&message_id, force, now);
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
        // payload's delta — must never answer for it, while a DELIVERED one
        // keeps answering even after a newer write replaces the owed payload.
        self.state
            .lock()
            .unwrap()
            .entries
            .get(message_id)
            .is_some_and(|entry| entry.delivered.contains(&seq))
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
        // A keyed submission can create the entry before any keyless write is
        // observed: sequence 0 is "no keyless write yet", never a settled one,
        // so the card's keyed queue cannot answer for the Rendered Cursor.
        (entry.seq > 0 && entry.card.is_none()).then(|| entry.delivered.contains(&entry.seq))
    }

    fn defer_notice_until_delivered(&self, message_id: &str, seq: u64, notice: DeferredNotice) -> NoticeGate {
        let mut state = self.state.lock().unwrap();
        let Some(entry) = state.entries.get_mut(message_id) else {
            // No delivery state: the terminal write's entry was evicted (or
            // never existed). Nothing can carry exactly `seq` now, so suppress
            // rather than announce over a slice that may never have landed.
            return NoticeGate::Never;
        };
        if entry.delivered.contains(&seq) {
            // THIS sequence already delivered: the terminal slice is on the
            // card — announce now, whatever newer write followed (spec #607).
            return NoticeGate::Delivered;
        }
        if entry.card.is_some() && entry.seq == seq {
            // The card still owes exactly `seq` as a Pending Card Update: arm
            // the notice against that sequence, so a newer unrelated repaint
            // can never answer for it (spec #607).
            entry.deferred_notices.push((seq, notice));
            return NoticeGate::Armed;
        }
        // A newer write superseded `seq`, or it settled permanently refused:
        // the terminal slice's own write will never land — suppress.
        NoticeGate::Never
    }
}
