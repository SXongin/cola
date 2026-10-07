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
//!
//! Besides the keyless `update_message`, the decorator owns a **keyed
//! submission** queue (spec #571): a writer that needs its write ordered against
//! the card's other writers submits a composed payload with `(generation,
//! intent)` — the **chain generation** its decision read and the intent naming
//! the logical write. The same per-card state machine then:
//!
//! - drops a submission below the card's floor: the highest generation seen,
//!   raised past a generation an accepted `Settle` closed (spec #571's
//!   amendment), so the ending beats a stamp whose read outlived it — and
//!   drops a `Stamp` shadowed by an accepted `Yield`, the waiting ending that
//!   closed nothing (amendment 2), so the interim restart status never lands
//!   over 「⏳ 等待后台任务」;
//! - lands same-generation submissions in submission order — one in-flight
//!   submission at a time, with a single waiting slot a newer submission
//!   replaces (older generations are forgotten as the key bumps);
//! - collapses a settled `(generation, intent)` pair: a delivered key never
//!   writes again (per-tick re-decisions are free) and a **permanently refused**
//!   key stays refused, so its re-submissions are dropped without a Feishu
//!   call (#522);
//! - keeps a recoverable failure owed with its key and its backoff, to be
//!   retried by the drain;
//! - serializes keyless and keyed writes on the same per-card delivery lock —
//!   a keyless write is never dropped for staleness.
//!
//! The queue also answers whether a card's **ending** is still owed
//! ([`CardDelivery::pending`] counts a keyed `Settle` the queue holds, not only
//! the keyless payload): the Live Card record is released only once the ending
//! write is confirmed (ADR-0063's amendment, spec #571's amendment).
//!
//! Readers can ask the queue what it already covers — [`Platform::keyed_write_covered`]
//! (spec #571, ticket #575): a settled key or one already being written needs
//! no re-decision, so a writer that re-decides every tick (the reap's stamp)
//! reads and submits nothing for it. The query only reports; it never orders,
//! claims or drops anything.
//!
//! Each keyed submission returns a [`CardWriteTicket`] settling on that
//! submission's own attempt, so a caller that needs the write's timing (a
//! collect releasing its cached card on delivery, warning on failure) awaits
//! it while Session Sync's submission stays fire-and-forget. The write itself
//! is owned by the queue — a spawned driver task takes the card's delivery
//! lock and settles the state — not by the submitting task, so cancelling a
//! submitter cannot strand the payload it already submitted.

use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;

use async_trait::async_trait;
use serde_json::Value;
use tokio::sync::oneshot;

use crate::error::{BridgeError, Result};
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
/// How many delivered write sequences one card remembers (spec #561, review
/// #569): enough to cover the writes between a payload's delivery and its
/// cursor confirmation — a handful — while the memory stays bounded.
const MAX_DELIVERED_SEQS: usize = 16;

/// One retry's own bound. The client has no default timeout, and the drain is
/// a background convergence path: a hung PATCH must not hold the card's
/// delivery lock (blocking new writes), the Session Sync pass, or all future
/// drains. On expiry the payload stays pending and is retried later — a
/// cancelled update may have landed, and re-sending one is idempotent.
const DRAIN_RETRY_TIMEOUT: Duration = Duration::from_secs(30);

/// How long an entry's order state (a closed generation's floor, an ending
/// shadow) outranks the cap (spec #571 review). The state only guards
/// against a **stale stamp submission**: one whose card-view read outlived its
/// decision. That read is bounded at 30 s and the reap ticks every 8 s, so
/// nothing can still be composing after this window — generously, an order of
/// magnitude past the bound — and only then may the cap evict such an entry as
/// a last resort rather than letting the map grow without bound.
const ORDER_STATE_PROTECTION: Duration = Duration::from_secs(300);

/// Which logical write a keyed submission is (spec #571). The intent is the
/// second half of the ordering key: `(generation, intent)` names one logical
/// write, so a writer's per-tick re-decision collapses to its newest payload
/// — or, once the key settled, is dropped — instead of writing again.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) enum CardWriteIntent {
    /// The restart stamp (#443, ADR-0063): mark a card a cola restart
    /// orphaned while its session still reads live.
    Stamp,
    /// A collect: the fresh-Turn takeover, a late projection, and the reap's
    /// successor collect — bring an orphaned card to its taken-over state.
    Collect,
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

    /// A ticket already settled — the unordered fallback's ticket.
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
}

/// The newest generation's settled key state (spec #571, rule (d)): once a
/// `(generation, intent)` pair settled it never writes again — a delivered key
/// is already on the card, a refused one is final — so a re-decision of the
/// same key is dropped without a Feishu call. A pending key is a slot below,
/// not an entry here.
#[derive(Clone, Copy)]
enum KeyState {
    Delivered,
    Refused,
}

/// The newest generation's key states, keyed by intent: bounded by the intent
/// vocabulary, and forgotten when a newer generation arrives.
type KeyStates = HashMap<CardWriteIntent, KeyState>;

/// One admitted keyed submission: the in-flight slot's occupant, or the single
/// submission waiting behind it.
struct QueuedWrite {
    /// Identifies this occupancy: a driver's outcome is ignored once the slot
    /// was superseded or the entry was evicted.
    token: u64,
    /// The generation the submission was admitted under.
    generation: u64,
    /// The logical write this submission is.
    intent: CardWriteIntent,
    /// The composed payload.
    card: Value,
    /// This submission's own failed attempts so far: each further failure
    /// doubles the delay before the next retry.
    attempts: u32,
    /// The earliest instant a retry may go out (unless forced).
    next_attempt: tokio::time::Instant,
}

#[allow(dead_code)]
impl QueuedWrite {
    fn new(token: u64, submission: KeyedSubmission<'_>, now: tokio::time::Instant) -> Self {
        Self {
            token,
            generation: submission.generation,
            intent: submission.intent,
            card: submission.card.clone(),
            attempts: 0,
            next_attempt: now,
        }
    }
}

/// The queue's retry bounds: each failed attempt waits the base delay doubled
/// per further failure (capped), and one attempt is bounded by the retry
/// timeout, so a hung PATCH cannot hold a card's delivery lock forever.
#[derive(Clone, Copy)]
struct Limits {
    backoff_base: Duration,
    backoff_max: Duration,
    retry_timeout: Duration,
    /// How long entry order state outranks the cap (spec #571 review);
    /// injectable so tests can watch the window pass.
    order_state_protection: Duration,
}

/// One card's newest write state.
///
/// `card` is `Some` while that write's payload is undelivered — the Pending
/// Card Update proper — and `None` once a write of the same sequence settled
/// (delivered, or permanently refused). The settled form is kept as a
/// sequence tombstone: an outcome that is not newer than the stored sequence
/// must not re-register its older payload (ADR-0067: a slow failure may never
/// resurrect a superseded payload over a newer delivered one). Tombstones age
/// out under the same cap as pending payloads.
struct PendingEntry {
    /// The monotonic write sequence this state belongs to. A retry may clear
    /// the payload only while the entry still names this sequence.
    seq: u64,
    /// The undelivered newest card JSON; `None` for a settled tombstone.
    card: Option<Value>,
    /// The exact sequences PROVEN delivered for this card (spec #561, review
    /// #569): a successful write adds its own sequence, and a drain retry adds
    /// the payload's. A confirmation asks whether ITS sequence is in here, so
    /// a newer write — a repaint, or another failure that replaces the owed
    /// payload — can never answer for an older one. Bounded oldest-first: a
    /// sequence no stage can still name ages out.
    delivered: VecDeque<u64>,
    /// Failed retry attempts so far — the backoff exponent.
    attempts: u32,
    /// The earliest instant the next retry may go out (unless forced).
    next_attempt: tokio::time::Instant,
    /// The card's delivery lock (shared with [`State::locks`]): every write to
    /// this message — a normal `update_message`, a keyed submission's driver
    /// and a drain retry alike — holds it across the Feishu call, so an older
    /// retry can never land after a newer write (ADR-0038's ordering, for the
    /// outbox writer).
    lock: Arc<tokio::sync::Mutex<()>>,
    /// The highest **chain generation** seen for this card (spec #571): a
    /// submission below it is dropped, and a newer one forgets the older
    /// generation's settled keys — every older submission is dropped by the
    /// generation rule anyway.
    generation: u64,
    /// The lowest generation this card still accepts (spec #571's amendment).
    /// A plain submission raises it to its own generation; an accepted
    /// `Settle` at generation G closes G, so it rises to G+1 and a later
    /// submission at ≤ G — the stamp whose read outlived the ending — is
    /// dropped. It lives as long as the entry (and its tombstone) does: an
    /// evicted floor would reopen the window it exists to close.
    floor: u64,
    /// The highest generation an accepted **ending** write (`Yield` or
    /// `Settle`) belonged to (spec #571's amendment 2): `None` until one was
    /// accepted. It shadows later `Stamp`s at ≤ it — the interim restart
    /// status can never land over an ending — without closing the generation,
    /// so the true end's terminal settle still lands after a yield. Lives with
    /// the entry, like [`Self::floor`].
    ending_gen: Option<u64>,
    /// When this entry's order state (a floor above the newest generation, or
    /// an ending shadow) was (re)established; `None` while it carries none.
    /// The cap may evict such an entry as a last resort once this is older
    /// than the protection window (spec #571 review): past it, no stale
    /// submission it guards against can still be composing.
    order_state_since: Option<tokio::time::Instant>,
    /// This entry's age, a monotonic stamp from [`State::age`], refreshed by
    /// every accepted write. The cap evicts the oldest ordinary entry, never
    /// the newest submission (whose keyed-only entry has no keyless sequence
    /// to rank by).
    age: u64,
    /// The newest generation's settled key states (rule (d)).
    keys: KeyStates,
    /// The keyed submission this card's driver is writing, or the one left
    /// owed by a recoverable failure — at most one per card.
    in_flight: Option<QueuedWrite>,
    /// The one submission waiting behind the in-flight one. A newer
    /// submission replaces it, so the queue stays bounded at 1 + 1.
    waiting: Option<QueuedWrite>,
    /// The token of the driver that owns this card's keyed queue right now;
    /// `None` when no write is reserved. A submission that finds a driver
    /// parks in `waiting`; one that finds none takes the in-flight slot and
    /// starts a driver.
    driver: Option<u64>,
    /// The completion tickets of the queued submissions, by token: a slot's
    /// ticket leaves with its outcome, so a dropped or superseded submission
    /// still settles its caller.
    tickets: HashMap<u64, oneshot::Sender<WriteOutcome>>,
}

impl PendingEntry {
    fn new(lock: Arc<tokio::sync::Mutex<()>>) -> Self {
        Self {
            seq: 0,
            card: None,
            delivered: VecDeque::new(),
            attempts: 0,
            next_attempt: tokio::time::Instant::now(),
            lock,
            generation: 0,
            floor: 0,
            ending_gen: None,
            order_state_since: None,
            age: 0,
            keys: KeyStates::new(),
            in_flight: None,
            waiting: None,
            driver: None,
            tickets: HashMap::new(),
        }
    }

    /// Whether this card's queue already owns `(generation, intent)` (spec
    /// #571, tickets #575): the generation is closed (a settle ended it, so
    /// no submission of it will ever write), a `Stamp` at or below an accepted
    /// ending's generation is shadowed (the waiting yield / the settle is the
    /// card's state), the key settled, or a write of that exact key is in
    /// flight (its driver running) or waiting. An owed write with no driver is
    /// not covered — a later re-decision may still replace it — and neither is
    /// an older generation, whose keys were forgotten and whose submissions
    /// are dropped by the generation rule.
    fn covers(&self, generation: u64, intent: CardWriteIntent) -> bool {
        if generation < self.floor {
            return true;
        }
        if intent == CardWriteIntent::Stamp && self.ending_gen.is_some_and(|ending| generation <= ending) {
            return true;
        }
        if self.generation != generation {
            return false;
        }
        if self.keys.contains_key(&intent) {
            return true;
        }
        let same_key = |write: &QueuedWrite| write.generation == generation && write.intent == intent;
        self.in_flight
            .as_ref()
            .is_some_and(|write| self.driver == Some(write.token) && same_key(write))
            || self.waiting.as_ref().is_some_and(same_key)
    }

    /// Whether this card's keyed ending write is still owed: a `Settle` in
    /// flight, waiting, or left owed by a recoverable failure. The card is not
    /// confirmed until it settles — the record-release gate consults this
    /// (spec #571's amendment).
    fn settling_owed(&self) -> bool {
        let settle = |write: &QueuedWrite| write.intent == CardWriteIntent::Settle;
        self.in_flight.as_ref().is_some_and(settle) || self.waiting.as_ref().is_some_and(settle)
    }

    /// Whether this entry still carries order state a stale writer may need
    /// (spec #571's amendments): a closed generation (`floor` above the newest
    /// seen — a settle ended it), or an ending shadow still sitting at the
    /// card's floor (a yield landed and nothing newer has pushed the floor
    /// past it; above that the floor's own drop covers every stamp the shadow
    /// would). Such an entry holds no payload. It outranks the cap while a
    /// stale writer could still be composing (see [`Self::order_state_expired`]).
    fn keeps_order_state(&self) -> bool {
        self.floor > self.generation || self.ending_gen == Some(self.floor)
    }

    /// Whether this entry's order state has outlived the window in which a
    /// stale writer could still arrive (spec #571 review): the state
    /// only guards against a submission — a stamp whose card-view read
    /// outlived its decision — that is composing, and that read is bounded;
    /// past the window the cap may evict the entry as a last resort instead of
    /// letting the map grow without bound.
    fn order_state_expired(&self, protection: Duration, now: tokio::time::Instant) -> bool {
        self.keeps_order_state()
            && self
                .order_state_since
                .is_some_and(|since| now.saturating_duration_since(since) >= protection)
    }
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
    /// Allocates the tokens that identify one keyed slot occupancy.
    tokens: u64,
    /// The monotonic age stamped onto an entry by every accepted write
    /// (keyless or keyed): the cap's oldest-first victim order, meaningful for
    /// keyed-only entries too (which carry no keyless sequence).
    age: u64,
}

/// A [`Platform`] decorator that turns a failed `update_message` into a
/// Pending Card Update and drains them on demand.
pub(crate) struct CardDelivery {
    inner: Arc<dyn Platform>,
    state: Arc<Mutex<State>>,
    /// One drain at a time: a Session Sync pass and a WS reconnect must not
    /// both walk the set (a superseded retry is harmless, but a shared walk
    /// keeps the bookkeeping single-writer).
    drain_lock: tokio::sync::Mutex<()>,
    max_pending: usize,
    limits: Limits,
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
            state: Arc::new(Mutex::new(State::default())),
            drain_lock: tokio::sync::Mutex::new(()),
            max_pending,
            limits: Limits {
                backoff_base,
                backoff_max,
                retry_timeout: DRAIN_RETRY_TIMEOUT,
                order_state_protection: ORDER_STATE_PROTECTION,
            },
        }
    }

    /// [`Self::with_limits`] with a shorter order-state protection window, so
    /// a test can watch an entry age out of it (spec #571 review).
    #[cfg(test)]
    fn with_order_state_protection(mut self, protection: Duration) -> Self {
        self.limits.order_state_protection = protection;
        self
    }

    /// Whether `message_id` has an undelivered write — the gate the Live Card
    /// record's removal consults (ADR-0063 amendment): a terminal card's
    /// record stays until its ending write is confirmed. Both classes count
    /// (spec #571's amendment): the keyless Pending Card Update, and a keyed
    /// ending write the queue still owes.
    pub(crate) fn pending(&self, message_id: &str) -> bool {
        self.state
            .lock()
            .unwrap()
            .entries
            .get(message_id)
            .is_some_and(|entry| entry.card.is_some() || entry.settling_owed())
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
    ///
    /// The keyed queue lives in the same entry and is left untouched: the
    /// sequence governs the keyless payload alone.
    fn observe(&self, message_id: &str, seq: u64, card: &Value, result: &Result<()>) {
        let mut state = self.state.lock().unwrap();
        let newest = state.entries.get(message_id).is_none_or(|entry| entry.seq < seq);
        if !newest {
            return;
        }
        let recoverable = matches!(result, Err(e) if e.is_recoverable_card_write());
        let age = {
            state.age += 1;
            state.age
        };
        if recoverable {
            // Remember the exact write a failure note will ask about (spec
            // #561, review #569): a newer write may replace the entry before
            // the note runs, and the note must still be able to tell whether
            // THIS payload's sequence delivered.
            state.failed.insert(message_id.to_string(), (seq, card.clone()));
        }
        let lock = state.locks.entry(message_id.to_string()).or_default().clone();
        let entry = state
            .entries
            .entry(message_id.to_string())
            .or_insert_with(|| PendingEntry::new(lock));
        // The delivered sequences outlive the entry (spec #561, review #569):
        // a newer write's outcome replaces the owed payload, and the older
        // delivery must still answer for its own sequence.
        entry.seq = seq;
        entry.attempts = 0;
        entry.age = age;
        if result.is_ok() {
            Self::remember_delivered(&mut entry.delivered, seq);
        }
        if recoverable {
            entry.card = Some(card.clone());
            entry.next_attempt = tokio::time::Instant::now() + self.limits.backoff_base;
        } else {
            // Delivered, or a permanent refusal (a rejected card, a gone
            // message, a non-recoverable 4xx): the payload can never usefully
            // be retried, so only the sequences are remembered — the delivered
            // set the Rendered Cursor's drain reconcile reads, and the
            // tombstone that keeps an older slow outcome from re-registering
            // (spec #561).
            entry.card = None;
            entry.next_attempt = tokio::time::Instant::now();
        }
        // The write just recorded is never this admission's own victim: the
        // cap evicts another entry, or waits for one to age out.
        self.evict_over_cap(&mut state, Some(message_id));
        Self::prune_locks(&mut state);
    }

    /// Add one proven-delivered sequence to a card's bounded set (spec #561,
    /// review #569), oldest first out.
    fn remember_delivered(delivered: &mut VecDeque<u64>, seq: u64) {
        if delivered.contains(&seq) {
            return;
        }
        delivered.push_back(seq);
        while delivered.len() > MAX_DELIVERED_SEQS {
            delivered.pop_front();
        }
    }

    /// Keep the state under the cap. A settled tombstone leaves before an
    /// undelivered payload (the cap bounds undelivered payloads; a tombstone
    /// only guards against a still-in-flight older write), oldest first by
    /// entry age. An entry that still carries order state is never chosen
    /// while another victim exists (spec #571's amendments); once **every**
    /// entry carries order state, one past its protection window leaves as
    /// the last resort, oldest state first (spec #571 review) — its
    /// stale writer can no longer be composing, so the map stays bounded
    /// under keyed-only traffic too. `protect` is the entry the caller just
    /// admitted: dropping the very write being recorded is never the cap's
    /// answer to its own growth. If every other entry is fresh order state
    /// the map may sit above the cap until one ages out. A pending payload
    /// evicted (only when more than the cap are owed) warns; a tombstone
    /// leaves quietly.
    fn evict_over_cap(&self, state: &mut State, protect: Option<&str>) {
        while state.entries.len() > self.max_pending {
            let now = tokio::time::Instant::now();
            let evictable = |message_id: &String| protect != Some(message_id.as_str());
            // A settled, unneeded tombstone is the preferred victim; then any
            // entry without order state, oldest first.
            let ordinary = state
                .entries
                .iter()
                .filter(|(message_id, entry)| !entry.keeps_order_state() && evictable(message_id))
                .min_by_key(|(_, entry)| {
                    let settled =
                        entry.card.is_none() && entry.in_flight.is_none() && entry.waiting.is_none();
                    (!settled, entry.age)
                })
                .map(|(message_id, _)| message_id.clone());
            // Nothing ordinary is left: an order state past its window is the
            // last-resort victim, the oldest first. A fresh one stays — the
            // window it protects is still open.
            let victim = ordinary.or_else(|| {
                state
                    .entries
                    .iter()
                    .filter(|(message_id, entry)| {
                        entry.order_state_expired(self.limits.order_state_protection, now)
                            && evictable(message_id)
                    })
                    .min_by_key(|(_, entry)| entry.order_state_since)
                    .map(|(message_id, _)| message_id.clone())
            });
            let Some(victim) = victim else {
                // Every remaining entry carries order state inside its
                // protection window (or is the admission itself): dropping one
                // would reopen the window its floor or shadow exists to close.
                // It ages into evictability.
                return;
            };
            let evicted = state.entries.remove(&victim);
            // The failure memory leaves with its card's entry: the note it
            // serves names the same payload, and the entry is gone.
            state.failed.remove(&victim);
            if let Some(mut entry) = evicted {
                // A keyed submission whose slot leaves with the entry is
                // dropped: nothing will be written for it, so its ticket says
                // so instead of waiting forever.
                for (_, ticket) in entry.tickets.drain() {
                    let _ = ticket.send(WriteOutcome::Superseded);
                }
                if entry.card.is_some() {
                    tracing::warn!(
                        "pending card update for {victim} evicted: {} card writes reached",
                        self.max_pending
                    );
                }
            }
        }
    }

    /// The delay before the next retry after `attempts` failed ones.
    fn backoff(&self, attempts: u32) -> Duration {
        backoff_delay(self.limits.backoff_base, self.limits.backoff_max, attempts)
    }

    /// Admit one keyed submission into `message_id`'s queue. Returns the token
    /// of the driver to start when the submission took the in-flight slot;
    /// `None` when it was dropped or parked behind the running write's driver.
    fn admit(
        state: &mut State,
        submission: KeyedSubmission<'_>,
        ticket: oneshot::Sender<WriteOutcome>,
    ) -> Option<u64> {
        let now = tokio::time::Instant::now();
        let lock = state
            .locks
            .entry(submission.message_id.to_string())
            .or_default()
            .clone();
        state.tokens += 1;
        let token = state.tokens;
        state.age += 1;
        let age = state.age;
        let entry = state
            .entries
            .entry(submission.message_id.to_string())
            .or_insert_with(|| PendingEntry::new(lock));
        // A newer generation forgets the older generation's settled keys:
        // every older submission is dropped by the rules below, and only the
        // newest generation's states are remembered.
        if submission.generation > entry.generation {
            entry.generation = submission.generation;
            entry.keys.clear();
        }
        // (d) A settled key never writes again: a delivered key is already on
        // the card (so per-tick re-decisions are free, and their callers learn
        // the key landed), and a permanent refusal is final for this
        // generation (#522). Both are dropped without a Feishu call. A `Stamp`
        // shadowed by an accepted ending is answered the same way: the card
        // shows the ending, not the interim restart status (spec #571's
        // amendment 2).
        if submission.intent == CardWriteIntent::Stamp
            && entry
                .ending_gen
                .is_some_and(|ending| submission.generation <= ending)
        {
            let _ = ticket.send(WriteOutcome::Superseded);
            return None;
        }
        if submission.generation == entry.generation {
            match entry.keys.get(&submission.intent) {
                Some(KeyState::Delivered) => {
                    let _ = ticket.send(WriteOutcome::Delivered);
                    return None;
                }
                Some(KeyState::Refused) => {
                    let _ = ticket.send(WriteOutcome::Superseded);
                    return None;
                }
                None => {}
            }
        }
        // (a) Below the card's floor: a newer generation already owns the
        // card, or a settle closed this generation — its ending landed, so
        // nothing submitted at or below it (the late stamp whose read
        // outlived the ending) may write. The submission never reaches Feishu.
        if submission.generation < entry.floor {
            let _ = ticket.send(WriteOutcome::Superseded);
            return None;
        }
        entry.floor = entry.floor.max(submission.generation);
        // An accepted `Settle` closes its own generation (spec #571's
        // amendment): the floor rises to G+1, so any LATER submission at ≤ G
        // is dropped. The settle itself is admitted below, so its key settles
        // the card's ending under rule (d).
        if submission.intent == CardWriteIntent::Settle {
            entry.floor = entry.floor.max(submission.generation.saturating_add(1));
        }
        // An accepted ending — the `Settle` or the waiting `Yield` (spec
        // #571's amendment 2) — shadows later `Stamp`s at ≤ its generation:
        // the interim restart status can never land over the card's ending
        // state. Only a settle closes the generation, so the true end's
        // terminal settle still lands after a yield.
        if matches!(
            submission.intent,
            CardWriteIntent::Settle | CardWriteIntent::Yield
        ) {
            entry.ending_gen = Some(
                entry
                    .ending_gen
                    .map_or(submission.generation, |ending| ending.max(submission.generation)),
            );
        }
        // An accepted submission refreshes the entry's age — the cap never
        // picks the newest submission first — and (re)stamps the order
        // state's clock. Only established state starts the protection window
        // (spec #571 review): a later submission the state DROPS never
        // reaches here, so a stream of dropped stamps cannot keep an old
        // floor alive past its window.
        entry.age = age;
        entry.order_state_since = entry.keeps_order_state().then_some(now);
        let write = QueuedWrite::new(token, submission, now);
        // The ticket lives with the queue until the submission settles: a
        // displaced or dropped slot still answers its caller.
        entry.tickets.insert(token, ticket);
        // A driver owns the card right now: the newest submission takes the
        // single waiting slot, replacing whatever waited there.
        if entry.driver.is_some() {
            if let Some(displaced) = entry.waiting.replace(write) {
                Self::settle_ticket(entry, displaced.token, WriteOutcome::Superseded);
            }
            return None;
        }
        // No driver owns the card: this submission takes the in-flight slot —
        // a newer payload supersedes an owed write (a recoverable failure's
        // payload the newest decision replaces) — and starts its driver.
        if let Some(displaced) = entry.in_flight.replace(write) {
            Self::settle_ticket(entry, displaced.token, WriteOutcome::Superseded);
        }
        entry.driver = Some(token);
        Some(token)
    }

    /// Settle one queued submission's ticket, if its caller still waits.
    fn settle_ticket(entry: &mut PendingEntry, token: u64, outcome: WriteOutcome) {
        if let Some(ticket) = entry.tickets.remove(&token) {
            let _ = ticket.send(outcome);
        }
    }

    /// Drive `message_id`'s keyed queue: attempt its reserved in-flight
    /// submission, settle it, promote the waiting slot and write that too, so
    /// a submission admitted behind a running write lands as soon as the write
    /// settles.
    ///
    /// A recoverable failure keeps the submission owed with its backoff (the
    /// drain retries it) unless a newer submission waits behind it — that one
    /// supersedes it and is written now. A permanent refusal settles the key,
    /// so later same-key submissions are dropped without a call.
    async fn drive_keyed(
        inner: Arc<dyn Platform>,
        state: Arc<Mutex<State>>,
        message_id: String,
        mut token: u64,
        mode: LockMode,
        limits: Limits,
    ) {
        loop {
            // Confirm this driver still owns the queue and snapshot the
            // payload to write. The state lock is never held across an await.
            let payload = {
                let mut state = state.lock().unwrap();
                let Some(entry) = state.entries.get_mut(&message_id) else {
                    return;
                };
                if entry.driver != Some(token) {
                    return;
                }
                match entry.in_flight.as_ref() {
                    Some(write) => write.card.clone(),
                    None => {
                        entry.driver = None;
                        return;
                    }
                }
            };
            // Serialize with every other writer of this card: the keyless
            // `update_message`, the outbox's retry, another driver. A
            // submission's driver waits for the lock (its write must land
            // promptly); the drain's retry skips a card a writer already holds
            // (the Session Sync pass never blocks on one).
            let lock = card_lock(&state, &message_id);
            let guard = match mode {
                LockMode::Await => Some(lock.lock().await),
                LockMode::Try => lock.try_lock().ok(),
            };
            let Some(_delivery) = guard else {
                // The card is being written right now: leave the submission
                // owed and release the reservation, so the active writer's own
                // settlement — or the next drain — picks it up.
                Self::release_driver(&state, &message_id, token);
                return;
            };
            // Under the lock, re-check the reservation: the entry may have
            // been evicted under the cap while this driver waited.
            {
                let state = state.lock().unwrap();
                if state
                    .entries
                    .get(&message_id)
                    .is_none_or(|entry| entry.driver != Some(token))
                {
                    return;
                }
            }
            // Bounded like the outbox's retry: a hung PATCH must not hold the
            // card's delivery lock — and so its queue — forever.
            let result = bounded_update(&inner, &message_id, &payload, limits.retry_timeout).await;
            let now = tokio::time::Instant::now();
            let mut state = state.lock().unwrap();
            let Some(entry) = state.entries.get_mut(&message_id) else {
                return;
            };
            if entry.driver != Some(token) {
                return;
            }
            let Some(mut write) = entry.in_flight.take() else {
                entry.driver = None;
                return;
            };
            match result {
                Ok(()) => {
                    // Only when no newer payload of the same key waits behind
                    // this write: that one is the key's newest payload and will
                    // settle it.
                    if write.generation == entry.generation
                        && entry.waiting.as_ref().is_none_or(|w| w.intent != write.intent)
                    {
                        entry.keys.insert(write.intent, KeyState::Delivered);
                    }
                    Self::settle_ticket(entry, write.token, WriteOutcome::Delivered);
                }
                Err(e) if e.is_recoverable_card_write() => {
                    if entry.waiting.is_some() {
                        // A newer submission supersedes the failed one (newest
                        // wins): the newest payload lands instead.
                        Self::settle_ticket(entry, write.token, WriteOutcome::Failed(e));
                    } else {
                        // Stay owed with the backoff: the drain retries it, and
                        // a newer submission replaces it if one arrives. The
                        // first failure waits the base delay, each further one
                        // doubles it — the outbox's own cadence.
                        write.next_attempt =
                            now + backoff_delay(limits.backoff_base, limits.backoff_max, write.attempts);
                        write.attempts += 1;
                        Self::settle_ticket(entry, write.token, WriteOutcome::Failed(e));
                        entry.in_flight = Some(write);
                        entry.driver = None;
                        return;
                    }
                }
                Err(e) => {
                    if write.generation == entry.generation {
                        entry.keys.insert(write.intent, KeyState::Refused);
                        // A newer payload of the SAME key already waiting is a
                        // re-submission of the refused key: drop it too, so an
                        // unrenderable card is never written again (#522).
                        // Another key's waiting payload is a different logical
                        // write and still lands.
                        if entry
                            .waiting
                            .as_ref()
                            .is_some_and(|w| w.generation == write.generation && w.intent == write.intent)
                        {
                            let waiting = entry.waiting.take().expect("checked above");
                            Self::settle_ticket(entry, waiting.token, WriteOutcome::Superseded);
                        }
                    }
                    Self::settle_ticket(entry, write.token, WriteOutcome::Failed(e));
                }
            }
            // Promote the waiting submission, if one waits: it is the newer
            // decision, and its own driver continues the queue.
            match entry.waiting.take() {
                Some(next) => {
                    token = next.token;
                    entry.driver = Some(token);
                    entry.in_flight = Some(next);
                }
                None => {
                    entry.driver = None;
                    return;
                }
            }
        }
    }

    /// Release a driver's reservation, if it still holds it.
    fn release_driver(state: &Mutex<State>, message_id: &str, token: u64) {
        if let Some(entry) = state.lock().unwrap().entries.get_mut(message_id)
            && entry.driver == Some(token)
        {
            entry.driver = None;
        }
    }

    /// Retry one card's owed keyless payload — the Pending Card Update proper —
    /// as the drain found it due.
    async fn retry_pending_card_update(&self, retry: KeylessRetry) {
        let KeylessRetry {
            message_id,
            card,
            lock,
            seq,
            attempts,
        } = retry;
        // Serialize with the card's other writers: the retry must land at
        // Feishu before any newer write, or an older payload would be the
        // last state on the card (ADR-0067's newest-wins at the wire).
        // A live writer already mid-flight makes the retry unnecessary —
        // it is about to deliver a newer state — so skip and let the next
        // pass retry anything its outcome left owed.
        let Ok(_delivery) = lock.try_lock() else {
            return;
        };
        // Re-check under the lock: a write that settled (or replaced) the
        // entry while this drain was waiting must not be overwritten by
        // the payload we snapshotted.
        {
            let state = self.state.lock().unwrap();
            if !state
                .entries
                .get(&message_id)
                .is_some_and(|current| current.seq == seq && current.card.is_some())
            {
                return;
            }
        }
        // Bounded: a hung PATCH must not hold the card's delivery lock —
        // and so the Session Sync pass — forever. On expiry the payload
        // stays pending and is retried later.
        let result = bounded_update(&self.inner, &message_id, &card, DRAIN_RETRY_TIMEOUT).await;
        let mut state = self.state.lock().unwrap();
        // The entry could still have left the set under cap eviction while
        // the retry was in flight; its outcome belongs to the payload it
        // carried, not to a newer state.
        let still_current = state
            .entries
            .get(&message_id)
            .is_some_and(|current| current.seq == seq && current.card.is_some());
        if !still_current {
            Self::prune_locks(&mut state);
            return;
        }
        match result {
            Ok(()) => {
                // Keep the sequence as a settled tombstone: an outcome that
                // is not newer than the stored one must not re-register
                // behind it — and remember THIS payload's own sequence as
                // delivered, which is what the Rendered Cursor's drain
                // reconcile confirms against, even after a newer write
                // replaces the entry (spec #561, review #569).
                if let Some(current) = state.entries.get_mut(&message_id) {
                    current.card = None;
                    Self::remember_delivered(&mut current.delivered, seq);
                }
                if attempts > 0 {
                    tracing::info!("pending card update for {message_id} delivered after {attempts} retries");
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
                }
                tracing::warn!("pending card update for {message_id} dropped: {e}");
            }
        }
        Self::prune_locks(&mut state);
    }

    /// Retry a card's owed keyed write when it is due: reserve it and drive it
    /// inline with the drain's lock mode, so a card another writer holds is
    /// skipped rather than awaited.
    async fn drive_owed_keyed(&self, message_id: &str, force: bool, now: tokio::time::Instant) {
        let token = {
            let mut state = self.state.lock().unwrap();
            let Some(entry) = state.entries.get_mut(message_id) else {
                return;
            };
            if entry.driver.is_some() {
                return;
            }
            let Some(due) = entry
                .in_flight
                .as_ref()
                .map(|write| (write.token, force || write.next_attempt <= now))
                .filter(|(_, due)| *due)
            else {
                return;
            };
            entry.driver = Some(due.0);
            due.0
        };
        Self::drive_keyed(
            self.inner.clone(),
            self.state.clone(),
            message_id.to_string(),
            token,
            LockMode::Try,
            self.limits,
        )
        .await;
    }
}

/// One card's owed keyless payload, snapshotted by the drain so the state lock
/// is never held across an await.
struct KeylessRetry {
    message_id: String,
    card: Value,
    lock: Arc<tokio::sync::Mutex<()>>,
    seq: u64,
    attempts: u32,
}

/// How a driver takes the card's delivery lock: a submission's driver waits
/// for it (the write must land promptly), the drain's retry only tries it (a
/// card a writer already holds is skipped, never awaited).
#[derive(Clone, Copy)]
enum LockMode {
    Await,
    Try,
}

/// The delay before the next retry after `attempts` failed ones.
fn backoff_delay(base: Duration, max: Duration, attempts: u32) -> Duration {
    base.saturating_mul(1u32 << attempts.min(16)).min(max)
}

/// The card's delivery lock, created on first use. Stable while an entry names
/// the card or any call holds the lock.
fn card_lock(state: &Mutex<State>, message_id: &str) -> Arc<tokio::sync::Mutex<()>> {
    state
        .lock()
        .unwrap()
        .locks
        .entry(message_id.to_string())
        .or_default()
        .clone()
}

/// One retry's card PATCH, bounded: the client has no default timeout, and a
/// hung PATCH must not hold the card's delivery lock — and so the card's queue
/// and the Session Sync pass — forever. On expiry the write reports the one
/// recoverable transport failure ([`BridgeError::Io`] / `TimedOut`) both retry
/// paths answer with: the payload stays pending and is retried later — a
/// cancelled update may have landed, and re-sending one is idempotent.
async fn bounded_update(
    platform: &Arc<dyn Platform>,
    message_id: &str,
    card: &Value,
    timeout: Duration,
) -> Result<()> {
    match tokio::time::timeout(timeout, platform.update_message(message_id, card)).await {
        Ok(result) => result,
        Err(_) => Err(BridgeError::Io(std::io::Error::new(
            std::io::ErrorKind::TimedOut,
            "card update timed out",
        ))),
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
            // settles. The ticket just observes its outcome.
            tokio::spawn(Self::drive_keyed(
                self.inner.clone(),
                self.state.clone(),
                message_id,
                token,
                LockMode::Await,
                self.limits,
            ));
        }
        CardWriteTicket::pending(ticket_rx)
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
            self.drive_owed_keyed(&message_id, force, now).await;
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

    fn keyed_write_covered(&self, message_id: &str, generation: u64, intent: CardWriteIntent) -> bool {
        self.state
            .lock()
            .unwrap()
            .entries
            .get(message_id)
            .is_some_and(|entry| entry.covers(generation, intent))
    }

    fn settled_card_write_delivered(&self, message_id: &str) -> Option<bool> {
        let state = self.state.lock().unwrap();
        let entry = state.entries.get(message_id)?;
        // A keyed submission can create the entry before any keyless write is
        // observed: sequence 0 is "no keyless write yet", never a settled one,
        // so the card's keyed queue cannot answer for the Rendered Cursor.
        (entry.seq > 0 && entry.card.is_none()).then(|| entry.delivered.contains(&entry.seq))
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

    /// Submit one keyed write and hand back its completion ticket.
    async fn submit(
        delivery: &CardDelivery,
        message_id: &str,
        generation: u64,
        intent: CardWriteIntent,
        card: &Value,
    ) -> CardWriteTicket {
        delivery
            .submit_ordered(KeyedSubmission {
                message_id,
                generation,
                intent,
                card,
            })
            .await
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

        // A newer write fails: the entry now names another sequence, but the
        // DELIVERED sequence's own evidence survives (spec #561, review #569)
        // — its content is on the card, so a restart that could not confirm it
        // would re-render it. Only the newer payload's own sequence is a
        // delivery.
        inner.fail_next(Fail::Transport);
        let _ = delivery.update_message("om_1", &second).await;
        assert!(
            delivery.card_write_delivered("om_1", seq),
            "a delivered sequence stays confirmable across a newer failure"
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

    /// A repaint (a newer SUCCESSFUL write with a different payload) answers
    /// only for its own sequence (spec #561, review #569): the older write's
    /// failure keeps its payload owed, so its stage must not be confirmed by
    /// someone else's delivery.
    #[tokio::test]
    async fn a_repaint_never_answers_for_an_older_failed_payload() {
        let inner = Arc::new(FakePlatform::new());
        let delivery = CardDelivery::new(inner.clone());
        let first = serde_json::json!({ "body": "first" });
        let second = serde_json::json!({ "body": "second" });

        inner.fail_next(Fail::Transport);
        let _ = delivery.update_message("om_1", &first).await;
        let failed_seq = delivery.pending_card_write("om_1", &first).expect("owed");
        // A newer, DIFFERENT payload lands: a repaint that need not carry the
        // failed payload's delta.
        let _ = delivery.update_message("om_1", &second).await;
        assert!(
            !delivery.card_write_delivered("om_1", failed_seq),
            "a repaint's own delivery never answers for the failed payload"
        );
        let repaint_seq = delivery
            .pending_card_write("om_1", &second)
            .unwrap_or(failed_seq + 1);
        assert!(delivery.card_write_delivered("om_1", repaint_seq));
        assert_eq!(
            delivery.failed_card_write("om_1", &first),
            Some(failed_seq),
            "the failed payload's own sequence is still what a note asks about"
        );
    }

    /// A drain-delivered payload stays confirmable after a NEWER write fails
    /// before any cursor reconciliation (spec #561, review #569): the newer
    /// failure replaces the owed payload, and without the delivered set the
    /// earlier write's evidence would vanish — a restart would then re-render
    /// content that is already visible on the card.
    #[tokio::test]
    async fn a_delivered_retry_survives_a_newer_failed_write() {
        let inner = Arc::new(FakePlatform::new());
        let delivery = CardDelivery::new(inner.clone());
        let first = serde_json::json!({ "body": "first" });
        let second = serde_json::json!({ "body": "second" });

        inner.fail_next(Fail::Transport);
        let _ = delivery.update_message("om_1", &first).await;
        let first_seq = delivery.pending_card_write("om_1", &first).expect("owed");
        // The owed payload's own retry delivers it.
        delivery.drain_pending_card_updates(true).await;
        assert!(delivery.card_write_delivered("om_1", first_seq));

        // A newer write fails BEFORE the cursor reconciliation.
        inner.fail_next(Fail::Transport);
        let _ = delivery.update_message("om_1", &second).await;
        let second_seq = delivery.pending_card_write("om_1", &second).expect("owed");
        assert_ne!(second_seq, first_seq);
        assert!(
            delivery.card_write_delivered("om_1", first_seq),
            "the delivered retry's sequence stays confirmable"
        );
        assert!(
            !delivery.card_write_delivered("om_1", second_seq),
            "the newer failure is no delivery"
        );
    }

    /// The delivered set is BOUNDED (spec #561, review #569): a card's write
    /// history cannot grow without limit, and an ancient sequence a stage can
    /// no longer name ages out.
    #[tokio::test]
    async fn the_delivered_set_is_bounded() {
        let inner = Arc::new(FakePlatform::new());
        let delivery = CardDelivery::new(inner.clone());
        for seq in 0..(MAX_DELIVERED_SEQS + 2) {
            let card = serde_json::json!({ "body": seq });
            let _ = delivery.update_message("om_1", &card).await;
        }
        let state = delivery.state.lock().unwrap();
        let entry = state.entries.get("om_1").expect("the newest entry");
        assert_eq!(entry.delivered.len(), MAX_DELIVERED_SEQS, "the set stays bounded");
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

    // ---- The ordered queue (spec #571): keyed submissions ----

    /// A keyed submission reaches Feishu with its composed payload, and its
    /// ticket reports this submission's own delivery.
    #[tokio::test]
    async fn a_keyed_submission_reaches_the_wire_and_settles_delivered() {
        let inner = Arc::new(FakePlatform::new());
        let delivery = CardDelivery::new(inner.clone());
        let card = serde_json::json!({ "body": "collected" });

        let ticket = submit(&delivery, "om_1", 1, CardWriteIntent::Collect, &card).await;
        assert!(matches!(ticket.settled().await, WriteOutcome::Delivered));
        assert_eq!(inner.attempts(), vec![("om_1".to_string(), card)]);
    }

    /// A submission below the highest generation seen for the card never
    /// reaches Feishu: the newer decision owns the card (rule (a)).
    #[tokio::test]
    async fn a_stale_generation_is_dropped_without_a_call() {
        let inner = Arc::new(FakePlatform::new());
        let delivery = CardDelivery::new(inner.clone());
        let newer = serde_json::json!({ "body": "newer" });
        let stale = serde_json::json!({ "body": "stale" });

        let ticket = submit(&delivery, "om_1", 2, CardWriteIntent::Collect, &newer).await;
        assert!(matches!(ticket.settled().await, WriteOutcome::Delivered));

        let ticket = submit(&delivery, "om_1", 1, CardWriteIntent::Stamp, &stale).await;
        assert!(matches!(ticket.settled().await, WriteOutcome::Superseded));
        assert_eq!(
            inner.attempts().len(),
            1,
            "the stale submission never reached Feishu"
        );
    }

    /// Same-generation submissions land in submission order: while a write is
    /// in flight the newer submission waits, and the newer decision is the
    /// card's last write (rule (b)).
    #[tokio::test]
    async fn an_in_flight_submission_lands_before_the_newer_one() {
        let inner = Arc::new(FakePlatform::new());
        let delivery = Arc::new(CardDelivery::new(inner.clone()));
        let stamp = serde_json::json!({ "body": "stamped" });
        let collect = serde_json::json!({ "body": "collected" });

        let (entered, release) = inner.park_next();
        let first = submit(&delivery, "om_1", 1, CardWriteIntent::Stamp, &stamp).await;
        entered.notified().await;

        let second = submit(&delivery, "om_1", 1, CardWriteIntent::Collect, &collect).await;
        // The newer submission must not overtake the write in flight.
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert_eq!(
            inner.attempts().len(),
            1,
            "the newer submission waits for the in-flight one"
        );

        release.notify_one();
        assert!(matches!(first.settled().await, WriteOutcome::Delivered));
        assert!(matches!(second.settled().await, WriteOutcome::Delivered));
        assert_eq!(
            inner.attempts(),
            vec![("om_1".to_string(), stamp), ("om_1".to_string(), collect)],
            "the newer submission landed after the in-flight one"
        );
    }

    /// The waiting slot is a single slot: a newer submission replaces the one
    /// waiting there, whose ticket settles superseded — it never reaches the
    /// wire. The newest payload of a key is what lands (rule (c)).
    #[tokio::test]
    async fn the_newest_submission_replaces_the_waiting_slot() {
        let inner = Arc::new(FakePlatform::new());
        let delivery = Arc::new(CardDelivery::new(inner.clone()));
        let collect = serde_json::json!({ "body": "collected" });
        let replanned = serde_json::json!({ "body": "replanned" });
        let newest = serde_json::json!({ "body": "newest" });

        let (entered, release) = inner.park_next();
        let in_flight = submit(&delivery, "om_1", 1, CardWriteIntent::Collect, &collect).await;
        entered.notified().await;

        // Both are the same logical write (Stamp at generation 1): the newest
        // payload is the one that lands.
        let displaced = submit(&delivery, "om_1", 1, CardWriteIntent::Stamp, &replanned).await;
        let waiting = submit(&delivery, "om_1", 1, CardWriteIntent::Stamp, &newest).await;

        release.notify_one();
        assert!(matches!(displaced.settled().await, WriteOutcome::Superseded));
        assert!(matches!(waiting.settled().await, WriteOutcome::Delivered));
        assert!(matches!(in_flight.settled().await, WriteOutcome::Delivered));
        assert_eq!(
            inner.attempts(),
            vec![("om_1".to_string(), collect), ("om_1".to_string(), newest)],
            "the displaced payload never reached Feishu"
        );
    }

    /// A key that already delivered never writes again: a writer's per-tick
    /// re-decision of the same generation and intent is free (rule (d)).
    #[tokio::test]
    async fn a_settled_key_is_never_written_again() {
        let inner = Arc::new(FakePlatform::new());
        let delivery = CardDelivery::new(inner.clone());
        let first = serde_json::json!({ "body": "stamped once" });
        let again = serde_json::json!({ "body": "stamped twice" });

        let ticket = submit(&delivery, "om_1", 1, CardWriteIntent::Stamp, &first).await;
        assert!(matches!(ticket.settled().await, WriteOutcome::Delivered));

        let ticket = submit(&delivery, "om_1", 1, CardWriteIntent::Stamp, &again).await;
        assert!(
            matches!(ticket.settled().await, WriteOutcome::Delivered),
            "the card already holds this logical write"
        );
        assert_eq!(inner.attempts().len(), 1, "the re-decision never reached Feishu");
    }

    /// A newer generation forgets the older one's settled keys: the same intent
    /// at a newer generation is a newer chain state and writes.
    #[tokio::test]
    async fn a_newer_generation_forgets_the_older_keys() {
        let inner = Arc::new(FakePlatform::new());
        let delivery = CardDelivery::new(inner.clone());
        let old = serde_json::json!({ "body": "old chain" });
        let new = serde_json::json!({ "body": "new chain" });

        let ticket = submit(&delivery, "om_1", 1, CardWriteIntent::Collect, &old).await;
        assert!(matches!(ticket.settled().await, WriteOutcome::Delivered));
        let ticket = submit(&delivery, "om_1", 2, CardWriteIntent::Collect, &new).await;
        assert!(matches!(ticket.settled().await, WriteOutcome::Delivered));
        assert_eq!(
            inner.attempts(),
            vec![("om_1".to_string(), old), ("om_1".to_string(), new)],
            "the newer generation's collect wrote again"
        );
    }

    /// A permanent refusal settles the key, and later same-key submissions are
    /// dropped without a Feishu call (#522, rule (d)).
    #[tokio::test]
    async fn a_permanent_refusal_settles_the_key_and_drops_resubmissions() {
        let inner = Arc::new(FakePlatform::new());
        inner.fail_next(Fail::ContentRejected);
        let delivery = CardDelivery::new(inner.clone());
        let refused = serde_json::json!({ "body": "refused" });
        let again = serde_json::json!({ "body": "refused again" });

        let ticket = submit(&delivery, "om_1", 1, CardWriteIntent::Collect, &refused).await;
        assert!(matches!(ticket.settled().await, WriteOutcome::Failed(_)));

        let ticket = submit(&delivery, "om_1", 1, CardWriteIntent::Collect, &again).await;
        assert!(matches!(ticket.settled().await, WriteOutcome::Superseded));
        assert_eq!(inner.attempts().len(), 1, "the refused key never wrote again");
    }

    /// A permanent refusal also drops a newer payload of the same key that was
    /// already waiting: the key is refused, so nothing of it is written again
    /// — one call, not a retry loop behind an unrenderable card (#522).
    #[tokio::test]
    async fn a_permanent_refusal_drops_a_waiting_payload_of_the_same_key() {
        let inner = Arc::new(FakePlatform::new());
        inner.fail_next(Fail::ContentRejected);
        let (entered, release) = inner.park_next();
        let delivery = Arc::new(CardDelivery::new(inner.clone()));
        let refused = serde_json::json!({ "body": "refused" });
        let newer = serde_json::json!({ "body": "refused too" });

        let first = submit(&delivery, "om_1", 1, CardWriteIntent::Collect, &refused).await;
        entered.notified().await;
        let second = submit(&delivery, "om_1", 1, CardWriteIntent::Collect, &newer).await;
        release.notify_one();

        assert!(matches!(first.settled().await, WriteOutcome::Failed(_)));
        assert!(matches!(second.settled().await, WriteOutcome::Superseded));
        assert_eq!(inner.attempts().len(), 1, "the refused key never wrote again");
    }

    /// Another key's waiting payload is a different logical write: a refusal
    /// settles only its own key.
    #[tokio::test]
    async fn a_refusal_settles_only_its_own_key() {
        let inner = Arc::new(FakePlatform::new());
        inner.fail_next(Fail::ContentRejected);
        let (entered, release) = inner.park_next();
        let delivery = Arc::new(CardDelivery::new(inner.clone()));
        let refused = serde_json::json!({ "body": "refused" });
        let stamp = serde_json::json!({ "body": "stamp" });

        let first = submit(&delivery, "om_1", 1, CardWriteIntent::Collect, &refused).await;
        entered.notified().await;
        let second = submit(&delivery, "om_1", 1, CardWriteIntent::Stamp, &stamp).await;
        release.notify_one();

        assert!(matches!(first.settled().await, WriteOutcome::Failed(_)));
        assert!(matches!(second.settled().await, WriteOutcome::Delivered));
        assert_eq!(
            inner.attempts(),
            vec![("om_1".to_string(), refused), ("om_1".to_string(), stamp)],
            "the other key's payload still landed"
        );
    }

    /// The read-only covered-key query a writer's per-tick re-decision
    /// consults before doing any work (spec #571, tickets #575): a settled key
    /// (delivered or permanently refused), an in-flight or waiting write of
    /// the exact key, and any generation at or below the card's floor are
    /// covered; an owed recoverable failure is not.
    #[tokio::test]
    async fn the_covered_key_query_reports_settled_and_in_flight_keys() {
        let inner = Arc::new(FakePlatform::new());
        let delivery = CardDelivery::new(inner.clone());
        let stamp = serde_json::json!({ "body": "stamp" });
        let collect = serde_json::json!({ "body": "collect" });

        assert!(
            !delivery.keyed_write_covered("om_1", 1, CardWriteIntent::Stamp),
            "a card the queue never saw reports nothing covered"
        );

        // An in-flight write covers its own key (the writer's re-decision
        // would only queue the same payload behind it) while a different key
        // is not covered.
        let (entered, release) = inner.park_next();
        let in_flight = submit(&delivery, "om_1", 1, CardWriteIntent::Collect, &collect).await;
        entered.notified().await;
        assert!(
            delivery.keyed_write_covered("om_1", 1, CardWriteIntent::Collect),
            "a write in flight covers its key"
        );
        assert!(
            !delivery.keyed_write_covered("om_1", 1, CardWriteIntent::Stamp),
            "another key's in-flight write covers nothing of this key"
        );
        release.notify_one();
        assert!(matches!(in_flight.settled().await, WriteOutcome::Delivered));
        assert!(
            delivery.keyed_write_covered("om_1", 1, CardWriteIntent::Collect),
            "a delivered key is covered"
        );
        assert!(
            !delivery.keyed_write_covered("om_1", 2, CardWriteIntent::Collect),
            "another generation's key is not this key"
        );

        // A recoverable failure leaves the key owed with no driver: a later
        // re-decision may still replace it, so it is not covered.
        let (entered, release) = inner.park_next();
        inner.fail_next(Fail::Transport);
        let failed = submit(&delivery, "om_2", 3, CardWriteIntent::Stamp, &stamp).await;
        entered.notified().await;
        release.notify_one();
        assert!(matches!(failed.settled().await, WriteOutcome::Failed(_)));
        assert!(
            !delivery.keyed_write_covered("om_2", 3, CardWriteIntent::Stamp),
            "an owed recoverable failure is not covered"
        );

        // A permanent refusal is covered, like a delivery (#522).
        inner.fail_next(Fail::ContentRejected);
        let ticket = submit(&delivery, "om_1", 2, CardWriteIntent::Stamp, &stamp).await;
        assert!(matches!(ticket.settled().await, WriteOutcome::Failed(_)));
        assert!(
            delivery.keyed_write_covered("om_1", 2, CardWriteIntent::Stamp),
            "a permanently refused key is covered"
        );
        // A newer generation forgets the older generation's keys — and the
        // query reports the older generation covered all the same: nothing
        // submitted at or below the card's floor can ever write.
        assert!(
            delivery.keyed_write_covered("om_1", 1, CardWriteIntent::Collect),
            "a generation below the floor is covered"
        );
        let ticket = submit(&delivery, "om_1", 1, CardWriteIntent::Collect, &collect).await;
        assert!(matches!(ticket.settled().await, WriteOutcome::Superseded));
    }

    /// The ending settle joins the keyed vocabulary (spec #571's amendment): a
    /// settle accepted at generation G closes G — the card's floor rises to
    /// G+1 — so a late stamp whose read outlived the ending never reaches
    /// Feishu, while a newer generation (a new chain state) still writes.
    #[tokio::test]
    async fn an_accepted_settle_closes_its_generation() {
        let inner = Arc::new(FakePlatform::new());
        let delivery = CardDelivery::new(inner.clone());
        let stamp = serde_json::json!({ "body": "late stamp" });
        let settle = serde_json::json!({ "body": "ending" });

        let ticket = submit(&delivery, "om_1", 1, CardWriteIntent::Settle, &settle).await;
        assert!(matches!(ticket.settled().await, WriteOutcome::Delivered));
        assert_eq!(inner.attempts().len(), 1);

        // The late stamp — decided under the closed generation — is dropped.
        let ticket = submit(&delivery, "om_1", 1, CardWriteIntent::Stamp, &stamp).await;
        assert!(matches!(ticket.settled().await, WriteOutcome::Superseded));
        assert_eq!(inner.attempts().len(), 1, "the late stamp never reached Feishu");
        assert!(
            delivery.keyed_write_covered("om_1", 1, CardWriteIntent::Stamp),
            "a closed generation is covered: nothing of it can ever write"
        );

        // A newer generation is a newer chain state: it may write.
        let ticket = submit(&delivery, "om_1", 2, CardWriteIntent::Collect, &stamp).await;
        assert!(matches!(ticket.settled().await, WriteOutcome::Delivered));
        assert_eq!(
            inner.attempts(),
            vec![("om_1".to_string(), settle), ("om_1".to_string(), stamp)],
            "the newer generation wrote"
        );
    }

    /// A settle that is itself stale relative to a newer generation is dropped
    /// like any stale submission (spec #571's amendment): a takeover's collect
    /// is never overwritten by a settle the takeover outran before it
    /// submitted.
    #[tokio::test]
    async fn a_stale_settle_never_lands_over_a_collect() {
        let inner = Arc::new(FakePlatform::new());
        let delivery = CardDelivery::new(inner.clone());
        let collect = serde_json::json!({ "body": "collected" });
        let settle = serde_json::json!({ "body": "ending" });

        let ticket = submit(&delivery, "om_1", 2, CardWriteIntent::Collect, &collect).await;
        assert!(matches!(ticket.settled().await, WriteOutcome::Delivered));

        let ticket = submit(&delivery, "om_1", 1, CardWriteIntent::Settle, &settle).await;
        assert!(matches!(ticket.settled().await, WriteOutcome::Superseded));
        assert_eq!(inner.attempts().len(), 1, "the stale settle never reached Feishu");
        assert_eq!(
            inner.attempts()[0].1,
            collect,
            "the collect is the card's last write"
        );
    }

    /// The record-release gate (ADR-0063's amendment) sees the keyed ending
    /// write's owed state: a settle in flight or left owed by a recoverable
    /// failure keeps the card pending, and only a settled key — delivered or
    /// permanently refused — reports it confirmed. Keyless pending is
    /// untouched.
    #[tokio::test]
    async fn a_pending_settle_keeps_the_release_gate_closed_until_it_settles() {
        let inner = Arc::new(FakePlatform::new());
        let delivery = Arc::new(CardDelivery::new(inner.clone()));
        let settle = serde_json::json!({ "body": "ending" });
        let keyless = serde_json::json!({ "body": "keyless" });
        assert!(!delivery.pending("om_1"));

        // In flight: the ending write is owed, so the card is not confirmed.
        let (entered, release) = inner.park_next();
        let ticket = submit(&delivery, "om_1", 1, CardWriteIntent::Settle, &settle).await;
        entered.notified().await;
        assert!(
            delivery.pending("om_1"),
            "an in-flight ending keeps the card pending"
        );
        release.notify_one();
        assert!(matches!(ticket.settled().await, WriteOutcome::Delivered));
        assert!(!delivery.pending("om_1"), "a delivered ending is confirmed");

        // A recoverable failure stays owed: still pending until the drain lands.
        let (entered, release) = inner.park_next();
        inner.fail_next(Fail::Transport);
        let ticket = submit(&delivery, "om_2", 1, CardWriteIntent::Settle, &settle).await;
        entered.notified().await;
        release.notify_one();
        assert!(matches!(ticket.settled().await, WriteOutcome::Failed(_)));
        assert!(delivery.pending("om_2"), "an owed ending keeps the card pending");
        delivery.drain_pending_card_updates(true).await;
        assert!(!delivery.pending("om_2"), "the drain confirmed the ending");

        // A permanent refusal is settled too — the ending can never land, and
        // the keyless gate has always released a refused write.
        inner.fail_next(Fail::ContentRejected);
        let ticket = submit(&delivery, "om_3", 1, CardWriteIntent::Settle, &settle).await;
        assert!(matches!(ticket.settled().await, WriteOutcome::Failed(_)));
        assert!(!delivery.pending("om_3"), "a refused ending is settled, not owed");

        // The keyless half is untouched: only a keyless payload reports a
        // keyless pending card.
        inner.fail_next(Fail::Transport);
        let _ = delivery.update_message("om_4", &keyless).await;
        assert!(delivery.pending("om_4"));
    }

    /// The cap never evicts a closed generation while another victim exists
    /// (spec #571's amendment): forgetting the floor would reopen the window
    /// for a stale writer whose read outlived the settle.
    #[tokio::test]
    async fn the_cap_does_not_evict_a_closed_generation_while_another_victim_exists() {
        let inner = Arc::new(FakePlatform::new());
        let delivery = CardDelivery::with_limits(inner.clone(), 2, BACKOFF_BASE, BACKOFF_MAX);
        let stamp = serde_json::json!({ "body": "late stamp" });
        let settle = serde_json::json!({ "body": "ending" });

        // om_closed: a settle accepted (and delivered) at generation 1 closes
        // the generation.
        let ticket = submit(&delivery, "om_closed", 1, CardWriteIntent::Settle, &settle).await;
        assert!(matches!(ticket.settled().await, WriteOutcome::Delivered));
        // Two keyless writes push the map over the cap; the closed entry must
        // not be chosen while a non-closed victim exists.
        delivery.update_message("om_a", &stamp).await.unwrap();
        delivery.update_message("om_b", &stamp).await.unwrap();

        let ticket = submit(&delivery, "om_closed", 1, CardWriteIntent::Stamp, &stamp).await;
        assert!(matches!(ticket.settled().await, WriteOutcome::Superseded));
        assert_eq!(
            inner.attempts().len(),
            3,
            "the floor survived the cap eviction: the late stamp never reached Feishu"
        );
    }

    /// The waiting yield joins the keyed vocabulary (spec #571's amendment 2)
    /// with a weak shadow rule: accepting a `Yield` at generation G shadows a
    /// later `Stamp` at ≤ G — it never reaches Feishu — but it does NOT close
    /// the generation, so the same-generation terminal `Settle` still lands
    /// after it (FIFO) and a newer generation's stamp is not shadowed.
    #[tokio::test]
    async fn an_accepted_yield_shadows_a_late_stamp_without_closing_the_generation() {
        let inner = Arc::new(FakePlatform::new());
        let delivery = CardDelivery::new(inner.clone());
        let stamp = serde_json::json!({ "body": "late stamp" });
        let waiting = serde_json::json!({ "body": "waiting" });
        let settle = serde_json::json!({ "body": "ending" });

        let ticket = submit(&delivery, "om_1", 1, CardWriteIntent::Yield, &waiting).await;
        assert!(matches!(ticket.settled().await, WriteOutcome::Delivered));
        assert_eq!(inner.attempts().len(), 1);

        // The late stamp at the yield's own generation is shadowed.
        let ticket = submit(&delivery, "om_1", 1, CardWriteIntent::Stamp, &stamp).await;
        assert!(matches!(ticket.settled().await, WriteOutcome::Superseded));
        assert_eq!(
            inner.attempts().len(),
            1,
            "the shadowed stamp never reached Feishu"
        );
        assert!(
            delivery.keyed_write_covered("om_1", 1, CardWriteIntent::Stamp),
            "an ending shadow covers the stamp"
        );

        // The generation is not closed: the true end's settle still lands after
        // the yield — same generation, FIFO.
        let ticket = submit(&delivery, "om_1", 1, CardWriteIntent::Settle, &settle).await;
        assert!(matches!(ticket.settled().await, WriteOutcome::Delivered));
        assert_eq!(
            inner.attempts(),
            vec![("om_1".to_string(), waiting), ("om_1".to_string(), settle)],
            "the terminal ending landed after the yield"
        );

        // A newer generation is a newer chain state: its stamp is not shadowed.
        let ticket = submit(&delivery, "om_1", 2, CardWriteIntent::Stamp, &stamp).await;
        assert!(matches!(ticket.settled().await, WriteOutcome::Delivered));
        assert_eq!(inner.attempts().len(), 3, "the newer generation's stamp wrote");
    }

    /// The cap keeps the ending shadow alive while a stale stamp could still
    /// arrive, exactly like the closed floor (spec #571's amendment 2):
    /// forgetting it would let the late stamp paint 「已重启」 over the ⏳.
    #[tokio::test]
    async fn the_cap_does_not_evict_an_ending_shadow_while_another_victim_exists() {
        let inner = Arc::new(FakePlatform::new());
        let delivery = CardDelivery::with_limits(inner.clone(), 2, BACKOFF_BASE, BACKOFF_MAX);
        let stamp = serde_json::json!({ "body": "late stamp" });
        let waiting = serde_json::json!({ "body": "waiting" });

        // om_shadow: a yield accepted (and delivered) at generation 1 shadows
        // its generation without closing it.
        let ticket = submit(&delivery, "om_shadow", 1, CardWriteIntent::Yield, &waiting).await;
        assert!(matches!(ticket.settled().await, WriteOutcome::Delivered));
        // Two keyless writes push the map over the cap; the shadowed entry must
        // not be chosen while a non-shadowed victim exists.
        delivery.update_message("om_a", &stamp).await.unwrap();
        delivery.update_message("om_b", &stamp).await.unwrap();

        let ticket = submit(&delivery, "om_shadow", 1, CardWriteIntent::Stamp, &stamp).await;
        assert!(matches!(ticket.settled().await, WriteOutcome::Superseded));
        assert_eq!(
            inner.attempts().len(),
            3,
            "the shadow survived the cap eviction: the late stamp never reached Feishu"
        );
    }

    /// The cap bounds KEYED-ONLY traffic too (spec #571 review): a
    /// process life that only ever admits keyed writes — a collect or an
    /// ending per card — must not retain one payload-less entry per card
    /// forever. Every admission runs the same cap check the keyless observe
    /// runs, so the map stays at the injected cap.
    #[tokio::test]
    async fn the_cap_bounds_keyed_only_cards() {
        let inner = Arc::new(FakePlatform::new());
        let delivery = CardDelivery::with_limits(inner.clone(), 2, BACKOFF_BASE, BACKOFF_MAX);

        for n in 0..5 {
            let card = serde_json::json!({ "body": n });
            let ticket = submit(&delivery, &format!("om_{n}"), 1, CardWriteIntent::Collect, &card).await;
            assert!(matches!(ticket.settled().await, WriteOutcome::Delivered));
        }

        let state = delivery.state.lock().unwrap();
        assert!(
            state.entries.len() <= 2,
            "keyed-only admission is bounded by the cap: {:?}",
            state.entries.keys().collect::<Vec<_>>()
        );
    }

    /// Order state is protected only while a stale writer could still be
    /// composing (spec #571 review): inside the protection window
    /// another victim is preferred, past it the oldest order state is the
    /// last-resort victim — so the cap keeps the map bounded without ever
    /// dropping a floor a live late stamp still needs.
    #[tokio::test(start_paused = true)]
    async fn the_cap_protects_order_state_only_inside_its_protection_window() {
        let stamp = serde_json::json!({ "body": "late stamp" });
        let settle = serde_json::json!({ "body": "ending" });

        // Inside the window: a non-order entry leaves first, so the floor
        // survives and the late stamp it closes is still dropped.
        let inner = Arc::new(FakePlatform::new());
        let delivery = CardDelivery::with_limits(inner.clone(), 2, BACKOFF_BASE, BACKOFF_MAX);
        let ticket = submit(&delivery, "om_closed", 1, CardWriteIntent::Settle, &settle).await;
        assert!(matches!(ticket.settled().await, WriteOutcome::Delivered));
        let ticket = submit(&delivery, "om_a", 1, CardWriteIntent::Collect, &stamp).await;
        assert!(matches!(ticket.settled().await, WriteOutcome::Delivered));
        let ticket = submit(&delivery, "om_b", 1, CardWriteIntent::Collect, &stamp).await;
        assert!(matches!(ticket.settled().await, WriteOutcome::Delivered));
        assert!(
            delivery.state.lock().unwrap().entries.contains_key("om_closed"),
            "a fresh closed generation is never chosen while another victim exists"
        );
        let ticket = submit(&delivery, "om_closed", 1, CardWriteIntent::Stamp, &stamp).await;
        assert!(matches!(ticket.settled().await, WriteOutcome::Superseded));
        assert_eq!(
            inner.attempts().len(),
            3,
            "the surviving floor dropped the late stamp: never a Feishu call"
        );

        // Past the window: with nothing but order-state entries left, the
        // oldest expired one is the last-resort victim; a fresh one survives.
        let inner = Arc::new(FakePlatform::new());
        let delivery = CardDelivery::with_limits(inner.clone(), 2, BACKOFF_BASE, BACKOFF_MAX)
            .with_order_state_protection(Duration::from_secs(30));
        let ticket = submit(&delivery, "om_closed_1", 1, CardWriteIntent::Settle, &settle).await;
        assert!(matches!(ticket.settled().await, WriteOutcome::Delivered));
        tokio::time::advance(Duration::from_secs(10)).await;
        let ticket = submit(&delivery, "om_closed_2", 1, CardWriteIntent::Settle, &settle).await;
        assert!(matches!(ticket.settled().await, WriteOutcome::Delivered));
        tokio::time::advance(Duration::from_secs(25)).await;
        let ticket = submit(&delivery, "om_closed_3", 1, CardWriteIntent::Settle, &settle).await;
        assert!(matches!(ticket.settled().await, WriteOutcome::Delivered));

        {
            let state = delivery.state.lock().unwrap();
            assert!(
                !state.entries.contains_key("om_closed_1"),
                "the oldest expired order state is evicted: {:?}",
                state.entries.keys().collect::<Vec<_>>()
            );
            assert!(
                state.entries.contains_key("om_closed_2") && state.entries.contains_key("om_closed_3"),
                "the fresh floors survive: {:?}",
                state.entries.keys().collect::<Vec<_>>()
            );
        }

        // The evicted floor is forgotten — the late stamp it was protecting
        // against can no longer be composing (its read is bounded) — while the
        // fresh floor still drops its own late stamp.
        let ticket = submit(&delivery, "om_closed_1", 1, CardWriteIntent::Stamp, &stamp).await;
        assert!(matches!(ticket.settled().await, WriteOutcome::Delivered));
        let ticket = submit(&delivery, "om_closed_2", 1, CardWriteIntent::Stamp, &stamp).await;
        assert!(matches!(ticket.settled().await, WriteOutcome::Superseded));
        assert_eq!(
            inner.attempts().len(),
            4,
            "one settle per card and the evicted floor's late stamp: never the protected one"
        );
    }

    /// A keyed submission's entry never answers for a keyless write that has
    /// not happened: the Rendered Cursor's views stay unobserved, not settled,
    /// until a keyless `update_message` is written for the card.
    #[tokio::test]
    async fn a_keyed_only_entry_never_reports_a_settled_keyless_write() {
        let inner = Arc::new(FakePlatform::new());
        let delivery = CardDelivery::new(inner.clone());
        let card = serde_json::json!({ "body": "keyed" });

        let ticket = submit(&delivery, "om_1", 1, CardWriteIntent::Collect, &card).await;
        assert!(matches!(ticket.settled().await, WriteOutcome::Delivered));
        assert_eq!(
            delivery.settled_card_write_delivered("om_1"),
            None,
            "the keyed queue is not a keyless write's settled verdict"
        );
        assert_eq!(
            delivery.pending_card_write("om_1", &card),
            None,
            "a keyed payload is never a Pending Card Update"
        );
        assert!(!delivery.pending("om_1"), "and owes nothing to the record gate");
    }

    /// Keyless writes keep today's semantics: they are never dropped for
    /// staleness and serialize on the same per-card lock as the queue.
    #[tokio::test]
    async fn a_keyless_write_is_never_dropped_and_serializes_with_the_queue() {
        let inner = Arc::new(FakePlatform::new());
        let delivery = Arc::new(CardDelivery::new(inner.clone()));
        let keyed = serde_json::json!({ "body": "keyed" });
        let keyless = serde_json::json!({ "body": "keyless" });

        let (entered, release) = inner.park_next();
        let ticket = submit(&delivery, "om_1", 5, CardWriteIntent::Collect, &keyed).await;
        entered.notified().await;

        let writing = tokio::spawn({
            let delivery = Arc::clone(&delivery);
            let keyless = keyless.clone();
            async move { delivery.update_message("om_1", &keyless).await }
        });
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert_eq!(
            inner.attempts().len(),
            1,
            "the keyless write waits for the card's write in flight"
        );

        release.notify_one();
        assert!(matches!(ticket.settled().await, WriteOutcome::Delivered));
        writing.await.unwrap().unwrap();
        assert_eq!(
            inner.attempts(),
            vec![("om_1".to_string(), keyed), ("om_1".to_string(), keyless)],
            "the keyless write landed, never dropped for the keyed write's generation"
        );
    }

    /// A platform with no delivery decorator keeps compiling and writes
    /// through the unordered fallback: the default `submit_ordered` delegates
    /// to its own `update_message`.
    #[tokio::test]
    async fn the_unordered_fallback_writes_through_update_message() {
        let inner = Arc::new(FakePlatform::new());
        let card = serde_json::json!({ "body": "fallback" });

        let ticket = inner
            .submit_ordered(KeyedSubmission {
                message_id: "om_1",
                generation: 3,
                intent: CardWriteIntent::Collect,
                card: &card,
            })
            .await;
        assert!(matches!(ticket.settled().await, WriteOutcome::Delivered));
        assert_eq!(inner.attempts(), vec![("om_1".to_string(), card.clone())]);

        inner.fail_next(Fail::ContentRejected);
        let ticket = inner
            .submit_ordered(KeyedSubmission {
                message_id: "om_2",
                generation: 3,
                intent: CardWriteIntent::Stamp,
                card: &card,
            })
            .await;
        assert!(matches!(ticket.settled().await, WriteOutcome::Failed(_)));
        assert_eq!(inner.attempts().len(), 2, "the fallback writes straight through");
        assert!(
            !inner.keyed_write_covered("om_2", 3, CardWriteIntent::Stamp),
            "an unwrapped platform remembers no covered key"
        );
    }

    /// A hung keyed write gives up at the same bound the keyless retry uses
    /// (spec #571 review's shared helper): the submission reports the
    /// recoverable timeout instead of hanging its caller, the payload stays
    /// owed, and the next drain lands it.
    #[tokio::test(start_paused = true)]
    async fn a_hung_keyed_write_times_out_and_stays_owed() {
        let inner = Arc::new(FakePlatform::new());
        let delivery = CardDelivery::new(inner.clone());
        let card = serde_json::json!({ "body": "collect" });

        let (entered, _never_released) = inner.park_next();
        let ticket = submit(&delivery, "om_1", 1, CardWriteIntent::Collect, &card).await;
        entered.notified().await;

        // The paused clock elapses the retry bound; the driver must settle the
        // ticket with the recoverable timeout, not hang.
        let outcome = tokio::time::timeout(Duration::from_secs(120), ticket.settled())
            .await
            .expect("the hung keyed write must time out, not hang its caller");
        assert!(matches!(outcome, WriteOutcome::Failed(_)));

        // The payload is still owed: a forced drain retries it, and the
        // second attempt lands.
        delivery.drain_pending_card_updates(true).await;
        assert_eq!(
            inner.attempts(),
            vec![("om_1".to_string(), card.clone()), ("om_1".to_string(), card)],
            "the owed keyed write was retried with its own payload"
        );
    }

    /// A recoverable failure stays owed with its key: the caller sees its own
    /// attempt's failure, and the drain retries the same payload.
    #[tokio::test]
    async fn a_recoverable_failure_stays_owed_for_the_drain() {
        let inner = Arc::new(FakePlatform::new());
        inner.fail_next(Fail::Transport);
        let delivery = CardDelivery::new(inner.clone());
        let card = serde_json::json!({ "body": "collect" });

        let ticket = submit(&delivery, "om_1", 1, CardWriteIntent::Collect, &card).await;
        assert!(matches!(ticket.settled().await, WriteOutcome::Failed(_)));

        delivery.drain_pending_card_updates(true).await;
        assert_eq!(
            inner.attempts(),
            vec![("om_1".to_string(), card.clone()), ("om_1".to_string(), card)],
            "the owed keyed write was retried with its own payload"
        );
    }

    /// The pass drain waits for the keyed write's backoff — doubled on each
    /// failure like the outbox's — and a forced drain ignores it.
    #[tokio::test(start_paused = true)]
    async fn the_keyed_retry_waits_for_the_backoff_unless_forced() {
        let inner = Arc::new(FakePlatform::new());
        inner.fail_next(Fail::Transport);
        let delivery = CardDelivery::with_limits(
            inner.clone(),
            MAX_PENDING,
            Duration::from_secs(5),
            Duration::from_secs(20),
        );
        let card = serde_json::json!({ "body": "collect" });
        let ticket = submit(&delivery, "om_1", 1, CardWriteIntent::Collect, &card).await;
        assert!(matches!(ticket.settled().await, WriteOutcome::Failed(_)));

        delivery.drain_pending_card_updates(false).await;
        assert_eq!(inner.attempts().len(), 1, "not due yet");

        tokio::time::advance(Duration::from_secs(5)).await;
        delivery.drain_pending_card_updates(false).await;
        assert_eq!(inner.attempts().len(), 2, "due after the base backoff");

        // A failed retry doubles the delay: 10s after it, not due; at 10s, due.
        inner.fail_next(Fail::Transport);
        let ticket = submit(&delivery, "om_1", 2, CardWriteIntent::Collect, &card).await;
        assert!(matches!(ticket.settled().await, WriteOutcome::Failed(_)));
        inner.fail_next(Fail::Transport); // the forced retry fails too
        delivery.drain_pending_card_updates(true).await; // forced: retries at once
        assert_eq!(inner.attempts().len(), 4);
        delivery.drain_pending_card_updates(false).await;
        assert_eq!(inner.attempts().len(), 4, "not due at the doubled backoff yet");
        tokio::time::advance(Duration::from_secs(10)).await;
        delivery.drain_pending_card_updates(false).await;
        assert_eq!(inner.attempts().len(), 5, "due at the doubled backoff");
    }

    /// A newer submission supersedes an owed keyed write: the newest decision
    /// writes promptly instead of waiting for the older one's retry backoff.
    #[tokio::test]
    async fn a_newer_submission_supersedes_an_owed_write() {
        let inner = Arc::new(FakePlatform::new());
        inner.fail_next(Fail::Transport);
        let delivery = CardDelivery::new(inner.clone());
        let stamp = serde_json::json!({ "body": "stamp" });
        let collect = serde_json::json!({ "body": "collect" });

        let ticket = submit(&delivery, "om_1", 2, CardWriteIntent::Stamp, &stamp).await;
        assert!(matches!(ticket.settled().await, WriteOutcome::Failed(_)));

        let ticket = submit(&delivery, "om_1", 3, CardWriteIntent::Collect, &collect).await;
        assert!(matches!(ticket.settled().await, WriteOutcome::Delivered));
        assert_eq!(
            inner.attempts(),
            vec![("om_1".to_string(), stamp), ("om_1".to_string(), collect)],
            "the newest decision wrote without waiting for the older backoff"
        );
    }

    /// A recoverable failure with a newer submission waiting is superseded by
    /// it (newest wins): the newest payload lands instead of the retry.
    #[tokio::test]
    async fn a_failed_write_is_superseded_by_the_waiting_one() {
        let inner = Arc::new(FakePlatform::new());
        inner.fail_next(Fail::Transport);
        let (entered, release) = inner.park_next();
        let delivery = Arc::new(CardDelivery::new(inner.clone()));
        let stamp = serde_json::json!({ "body": "stamp" });
        let collect = serde_json::json!({ "body": "collect" });

        let first = submit(&delivery, "om_1", 1, CardWriteIntent::Stamp, &stamp).await;
        entered.notified().await;
        let second = submit(&delivery, "om_1", 1, CardWriteIntent::Collect, &collect).await;
        release.notify_one();

        assert!(matches!(first.settled().await, WriteOutcome::Failed(_)));
        assert!(matches!(second.settled().await, WriteOutcome::Delivered));
        assert_eq!(
            inner.attempts(),
            vec![("om_1".to_string(), stamp), ("om_1".to_string(), collect)],
            "the waiting newer payload landed, not the failed retry"
        );
    }

    /// The keyless outbox and the keyed queue share one entry without
    /// disturbing each other: a failed keyless payload stays owed and is still
    /// delivered — keyless writes are never dropped — while a keyed write
    /// lands on its own.
    #[tokio::test]
    async fn a_keyless_failure_and_a_keyed_write_share_the_card() {
        let inner = Arc::new(FakePlatform::new());
        inner.fail_next(Fail::Transport);
        let delivery = CardDelivery::new(inner.clone());
        let owed = serde_json::json!({ "body": "keyless owed" });
        let keyed = serde_json::json!({ "body": "keyed" });

        let _ = delivery.update_message("om_1", &owed).await;
        assert!(delivery.pending("om_1"), "the keyless failure is owed");

        let ticket = submit(&delivery, "om_1", 1, CardWriteIntent::Collect, &keyed).await;
        assert!(matches!(ticket.settled().await, WriteOutcome::Delivered));
        assert!(
            delivery.pending("om_1"),
            "the keyed delivery never settles the keyless outbox entry"
        );

        delivery.drain_pending_card_updates(true).await;
        assert_eq!(
            inner.attempts().last(),
            Some(&("om_1".to_string(), owed)),
            "the owed keyless payload was still delivered"
        );
        assert!(!delivery.pending("om_1"));
    }

    /// The drain's keyed retry never blocks on a card another writer holds:
    /// the pass skips it and a later drain converges.
    #[tokio::test]
    async fn the_drain_skips_a_card_another_writer_holds() {
        let inner = Arc::new(FakePlatform::new());
        inner.fail_next(Fail::Transport);
        let delivery = Arc::new(CardDelivery::new(inner.clone()));
        let card = serde_json::json!({ "body": "stamp" });

        let ticket = submit(&delivery, "om_1", 1, CardWriteIntent::Stamp, &card).await;
        assert!(matches!(ticket.settled().await, WriteOutcome::Failed(_)));

        // A keyless writer holds the card's delivery lock mid-flight.
        let (entered, release) = inner.park_next();
        let writing = tokio::spawn({
            let delivery = Arc::clone(&delivery);
            async move {
                delivery
                    .update_message("om_1", &serde_json::json!({ "body": "keyless" }))
                    .await
            }
        });
        entered.notified().await;

        delivery.drain_pending_card_updates(true).await;
        assert_eq!(
            inner.attempts().len(),
            2,
            "the keyed retry was skipped, not awaited behind the live writer"
        );

        release.notify_one();
        writing.await.unwrap().unwrap();
        delivery.drain_pending_card_updates(true).await;
        assert_eq!(
            inner.attempts().len(),
            3,
            "the keyed write converged on the next drain"
        );
    }
}
