//! Narrow handles (spec #298, tickets A1/B): per-concern views over the state
//! the coordinator owns.
//!
//! [`SharedCore`](crate::bridge::core::SharedCore) stays the aggregate root and
//! the one construction point; a flow receives only the handles it uses, so its
//! dependencies — and the locks it may take — are visible at its interface.
//! Handles delegate to the existing stores ([`SessionStore`], the card-handle
//! registry, the request flows); they never duplicate state.
//!
//! Each handle documents the locks it owns and the ordering those locks obey
//! (see the type-level docs below); a bundle is just a named combination of
//! handles plus the two adapters, and carries no locks of its own.
//!
//! Bundles in use:
//!
//! - [`FlowHandles`] — the four per-concern handles plus the backend and the
//!   platform: the request flow and the external-message flow run on it.
//! - [`PollHandles`] — [`FlowHandles`] plus the server-ownership state the
//!   reconcile poller mutates ([`ServerHandle`]).
//! - [`SnapshotHandles`] — the read-side subset a Session Snapshot gather and
//!   its claim filter need.
//! - [`TopicHandles`] — [`FlowHandles`] plus the external flow the topic
//!   opening settles an adopted snapshot through.
//! - [`CommandHandles`] — [`FlowHandles`] plus the turn config (the default
//!   session directory) and the external flow the adoption tails settle
//!   through.
//! - [`TurnHandles`] — the Turn's bundle (A1).

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use tokio::sync::Mutex;

use crate::bridge::card_handles::CardHandles;
use crate::bridge::core::{CoverTitle, SESSION_INFO_TIMEOUT, SETTLING_CLAIM_TTL, SessionListCache};
use crate::bridge::external::ExternalFlow;
use crate::bridge::message_pins::MessagePins;
use crate::bridge::reminder::ReminderState;
use crate::bridge::request::flow::RequestFlow;
use crate::bridge::session::{PendingEntry, SessionSettings, SessionStore};
use crate::bridge::snapshot_claims::{ClaimKind, SnapshotClaims};
use crate::bridge::turn::CardSession;
use crate::config::{ServerStartPolicy, SessionEntry, ThreadKey};
use crate::{feishu, opencode};

/// The session map, the session-list cache its write paths invalidate, and the
/// settings/model resolution the conversation reads.
///
/// The store and cache travel together because a create/remove/activate changes
/// what `/list` and `/switch` must show; a caller that only reads sessions still
/// goes through the same handle. The settings ladder lives here too because
/// every rung (a persisted override, the server-recorded model) is read from
/// the same store.
///
/// **Locks owned:** the `store` mutex (the persisted [`SessionStore`]) and the
/// private list-cache mutex. The two are never held at once — a write takes the
/// store lock, drops it, then clears the cache — so no ordering between them
/// can invert. Every accessor clones what it needs out of the store and
/// releases it before returning.
#[derive(Clone)]
pub(crate) struct SessionsHandle {
    pub(crate) store: Arc<Mutex<SessionStore>>,
    /// The 30 s session-list cache (`/list`/`/switch`/`/attach`), invalidated by
    /// the write paths below. Private: [`Self::cached_session_list`] and
    /// [`Self::invalidate_cache`] are the accessors.
    cache: Arc<Mutex<Option<SessionListCache>>>,
}

impl SessionsHandle {
    pub(crate) fn new(store: Arc<Mutex<SessionStore>>, cache: Arc<Mutex<Option<SessionListCache>>>) -> Self {
        Self { store, cache }
    }

    /// The mapped entry for `session_id`, cloned out of the store.
    pub(crate) async fn entry_for_session(&self, session_id: &str) -> Option<SessionEntry> {
        self.store.lock().await.entry_for_session(session_id).cloned()
    }

    /// The active entry of `thread_key`, cloned out of the store.
    pub(crate) async fn active_entry(&self, thread_key: &ThreadKey) -> Option<SessionEntry> {
        self.store.lock().await.get_active(thread_key).cloned()
    }

    /// The active session's id for `thread_key`, if mapped.
    pub(crate) async fn get_session_id(&self, thread_key: &ThreadKey) -> Option<String> {
        self.store
            .lock()
            .await
            .get_active(thread_key)
            .map(|e| e.session_id.clone())
    }

    /// The Chat/Topic a session is mapped to, if any.
    pub(crate) async fn thread_for_session(&self, session_id: &str) -> Option<ThreadKey> {
        self.store.lock().await.thread_for_session(session_id)
    }

    /// The working directory recorded for a session, if any.
    pub(crate) async fn directory_for_session(&self, session_id: &str) -> Option<String> {
        self.store.lock().await.directory_for_session(session_id)
    }

    /// The per-session `/think` variant override (ADR-0020).
    pub(crate) async fn variant_override(&self, session_id: &str) -> Option<String> {
        self.store
            .lock()
            .await
            .entry_for_session(session_id)
            .and_then(|e| e.variant.clone())
    }

    /// The per-session model override set by `/model`, parsed from the persisted
    /// "provider/model" string.
    pub(crate) async fn model_override(&self, session_id: &str) -> Option<opencode::types::ModelInfo> {
        self.store
            .lock()
            .await
            .entry_for_session(session_id)
            .and_then(|e| e.model.as_deref())
            .and_then(opencode::parsing::parse_model)
    }

    /// The per-session agent override set by `/agent`.
    pub(crate) async fn agent_override(&self, session_id: &str) -> Option<String> {
        self.store
            .lock()
            .await
            .entry_for_session(session_id)
            .and_then(|e| e.agent.clone())
    }

    /// The settings the conversation's next prompt will use (ADR-0041): the
    /// Pending Session's when one exists, else the active session's. `None`
    /// when the thread has neither.
    pub(crate) async fn session_settings(&self, thread_key: &ThreadKey) -> Option<SessionSettings> {
        self.store.lock().await.settings(thread_key)
    }

    /// Write a [`SessionSettings`] snapshot back to its target and persist.
    /// `false` when the target is gone.
    pub(crate) async fn set_session_settings(
        &self,
        thread_key: &ThreadKey,
        settings: SessionSettings,
    ) -> crate::error::Result<bool> {
        self.store.lock().await.set_settings(thread_key, settings)
    }

    /// Mutate the mapped session in place and persist, returning the updated
    /// entry (`None` when `session_id` is not mapped). The session-list cache
    /// is untouched: per-session overrides are not server-list state.
    pub(crate) async fn update<F>(&self, session_id: &str, f: F) -> crate::error::Result<Option<SessionEntry>>
    where
        F: FnOnce(&mut SessionEntry),
    {
        self.store.lock().await.update(session_id, f)
    }

    /// Mutate the conversation's Pending Session and persist (ADR-0041).
    /// `false` when the thread has none.
    pub(crate) async fn update_pending<F>(&self, thread_key: &ThreadKey, f: F) -> crate::error::Result<bool>
    where
        F: FnOnce(&mut PendingEntry),
    {
        self.store.lock().await.update_pending(thread_key, f)
    }

    /// Declare (or replace) the conversation's Pending Session and persist
    /// (ADR-0041). The session-list cache is untouched: a pending is not a
    /// server session, so `/list`/`/switch` have nothing new to show.
    pub(crate) async fn set_pending(&self, pending: PendingEntry) -> crate::error::Result<()> {
        self.store.lock().await.set_pending(pending)
    }

    /// Declare (or replace) a Pending Session rooted at an explicit `directory`
    /// (ADR-0041) — the shape `/dir`, its card pick and the other
    /// explicit-directory forms share. Replacing a topic's pending keeps its
    /// `topic_root`/`topic_anchor` (ADR-0023): they are properties of the
    /// Feishu topic, not of the abandoned directory intent.
    pub(crate) async fn declare_pending(
        &self,
        thread_key: &ThreadKey,
        directory: impl Into<String>,
        title: Option<String>,
    ) -> crate::error::Result<PendingEntry> {
        let mut pending = PendingEntry::new(thread_key.clone(), directory);
        pending.title = title;
        {
            let store = self.store.lock().await;
            if let Some(replaced) = store.pending_for(thread_key) {
                pending.topic_anchor = replaced.topic_anchor.clone();
                pending.topic_root = replaced.topic_root.clone();
            }
        }
        self.set_pending(pending.clone()).await?;
        Ok(pending)
    }

    /// The conversation's current directory: the Pending Session's when one is
    /// declared, else the active session's.
    pub(crate) async fn current_directory(&self, thread_key: &ThreadKey) -> Option<String> {
        self.store.lock().await.current_directory(thread_key)
    }

    /// Persist `entry` as its thread's active session and drop the session-list
    /// cache: creating or adopting a session changes what `/list` and `/switch`
    /// should offer. The cache is dropped even when the save fails, because the
    /// in-memory mapping already changed.
    pub(crate) async fn activate(&self, entry: SessionEntry) -> crate::error::Result<()> {
        let result = self.store.lock().await.activate(entry);
        *self.cache.lock().await = None;
        result
    }

    /// Remove a mapping and persist, dropping the session-list cache (the
    /// `/list`/`/switch` view may no longer mention it).
    pub(crate) async fn remove_session(
        &self,
        session_id: &str,
    ) -> crate::error::Result<Option<SessionEntry>> {
        let result = self.store.lock().await.remove_persist(session_id);
        *self.cache.lock().await = None;
        result
    }

    /// Remove every mapping of a thread and persist, dropping the session-list
    /// cache (`/switch forget`).
    pub(crate) async fn remove_thread_sessions(
        &self,
        key: &ThreadKey,
    ) -> crate::error::Result<Vec<SessionEntry>> {
        let result = self.store.lock().await.remove_thread_persist(key);
        *self.cache.lock().await = None;
        result
    }

    /// Drop the `/list` cache. Called whenever cola creates, adopts, forgets or
    /// renames a session, so the next `/list`/`/switch`/`/attach` is fresh.
    pub(crate) async fn invalidate_cache(&self) {
        *self.cache.lock().await = None;
    }

    /// The current `GET /session` snapshot, fetching (and caching for 30 s) when
    /// missing or stale. Used by `/list`, `/switch` and `/attach` so rapid
    /// reuse stays off the wire.
    pub(crate) async fn cached_session_list(
        &self,
        backend: &Arc<dyn crate::backend::Backend>,
    ) -> crate::error::Result<Vec<opencode::types::SessionListInfo>> {
        let now = std::time::Instant::now();
        {
            let cache = self.cache.lock().await;
            if let Some(c) = cache.as_ref()
                && c.fresh()
            {
                return Ok(c.sessions.clone());
            }
        }
        let sessions = backend.list_sessions().await?;
        *self.cache.lock().await = Some(SessionListCache {
            fetched_at: now,
            sessions: sessions.clone(),
        });
        Ok(sessions)
    }

    /// The model the NEXT turn will actually run, resolved settings override →
    /// configured default → server-recorded session model (`GET /session/{id}`).
    /// `None` only when every rung fails (no override, no config, server
    /// unreachable) — the `/think` card then tells the user to `/model` first.
    /// Returns `(provider, model)`. A Pending Session has no server-recorded
    /// rung (nothing exists on the server yet, ADR-0041).
    pub(crate) async fn effective_model(
        &self,
        backend: &Arc<dyn crate::backend::Backend>,
        settings: &SessionSettings,
    ) -> Option<(String, String)> {
        // 1. The `/model` override in the snapshot.
        if let Some(m) = settings.model.as_deref().and_then(opencode::parsing::parse_model) {
            return Some((m.provider_id, m.id));
        }
        // 2. The configured default (`[opencode] model`).
        if let Some(m) = backend.configured_default_model() {
            return Some((m.provider_id, m.id));
        }
        // 3. What the server actually recorded for the session. Bounded: a
        //    hung server degrades the ladder (no current-model line / a
        //    `/think` "pick a model" prompt), never the card send.
        let session_id = settings.session_id.as_deref()?;
        if !settings.directory.is_empty()
            && let Ok(Ok(info)) = tokio::time::timeout(
                SESSION_INFO_TIMEOUT,
                backend.session_info(session_id, Some(&settings.directory)),
            )
            .await
            && let Some(m) = info.model
        {
            return Some((m.provider_id, m.id));
        }
        None
    }

    /// The declared variants of a provider/model, per `GET /provider`.
    /// Best-effort: `None` when the model can't be found in the advertised
    /// catalog (callers then leave a stored variant in place rather than
    /// destroying it on an unknown).
    pub(crate) async fn model_variants(
        &self,
        backend: &Arc<dyn crate::backend::Backend>,
        provider: &str,
        model: &str,
    ) -> Option<Vec<String>> {
        backend
            .list_models()
            .await
            .into_iter()
            .find(|p| p.provider == provider)
            .and_then(|p| p.models.into_iter().find(|m| m.id == model))
            .map(|m| m.variants)
    }

    /// Auto-clear a `/think` variant when switching to a model that doesn't
    /// declare it (ADR-0020): a leftover variant would make every prompt fail
    /// with a server `VariantUnavailableError`. Works on the variant field of
    /// either a real session or a Pending Session — the settings commands
    /// choose the target, this owns the rule. Returns the cleared variant name,
    /// or `None` when the variant survives. Best-effort: a model not found in
    /// the advertised catalog is left alone.
    pub(crate) async fn clear_variant_for_model(
        &self,
        backend: &Arc<dyn crate::backend::Backend>,
        variant: &mut Option<String>,
        model_spec: &str,
    ) -> Option<String> {
        if let Some(v) = variant.clone()
            && let Some(m) = opencode::parsing::parse_model(model_spec)
            && let Some(variants) = self.model_variants(backend, &m.provider_id, &m.id).await
            && !variants.iter().any(|x| x == &v)
        {
            *variant = None;
            Some(v)
        } else {
            None
        }
    }

    /// Create a brand-new session on the current server and make it the active
    /// one for the thread. Used when a mapped session no longer exists (404).
    /// The per-session overrides reset to defaults, but the topic's creation
    /// messages (`topic_anchor`/`topic_root`, ADR-0023) are Feishu message ids —
    /// not session state — and survive so the quote-injection guard keeps
    /// working after the recreate.
    pub(crate) async fn create_fresh_session(
        &self,
        backend: &Arc<dyn crate::backend::Backend>,
        thread_key: &ThreadKey,
        directory: String,
        topic_anchor: Option<String>,
        topic_root: Option<String>,
    ) -> crate::error::Result<String> {
        let session = backend
            .create_session(&backend.new_session_input(Some(&directory)))
            .await?;
        let mut entry = SessionEntry::new(thread_key.clone(), session.id.clone(), directory);
        entry.topic_anchor = topic_anchor;
        entry.topic_root = topic_root;
        self.activate(entry).await?;
        Ok(session.id)
    }

    /// Whether `candidate` is `root` or a sub-task child reachable by walking
    /// up its parent chain (sub-task child sessions carry their own sessionID).
    /// Shared with the turn-end leftover rejection (#187), which filters the
    /// same way when deciding whose requests a dead turn owns.
    pub(crate) async fn descends_from(
        &self,
        backend: &Arc<dyn crate::backend::Backend>,
        candidate: &str,
        root: &str,
        directory: &str,
    ) -> bool {
        crate::bridge::pollers::walk_parent_chain(backend, candidate, Some(directory), |current| {
            let current = current.to_string();
            async move { (current == root).then_some(true) }
        })
        .await
        .unwrap_or(false)
    }
}

/// The live cards, the card-handle registry, the per-session card-write locks,
/// the topic cover records, and the platform that sends them.
///
/// Card delivery is inherently a platform call, so the platform rides this
/// handle: every card path (flush, split, resolution, cover sync) reaches
/// Feishu through it rather than through the aggregate.
///
/// **Locks owned:** `cards` (the per-session accumulator + card identity),
/// `card_handles` (ADR-0038's registry), `cover_titles`, and the private
/// per-session `write_locks`. Ordering:
///
/// 1. A card write takes the session's [`Self::write_lock`] FIRST and holds it
///    across the whole read-send-record sequence (`flush_card`,
///    `split_card_chain`, `resolve_blocks`); everything else is taken inside
///    that sequence. The `write_locks` map is only the lookup from a session id
///    to its lock, held for that step alone.
/// 2. Inside the sequence, `cards` and `card_handles` are each taken for one
///    step and released. The resolution path (`resolve_blocks`) also snapshots
///    the request flow's `sent_cards`, briefly, while it holds `write_lock`: the
///    documented order is `write_lock` → `sent_cards` → `card_handles`, with no
///    two of `cards`, `sent_cards` and `card_handles` ever held at the same
///    time.
/// 3. `cover_titles` is independent of both.
#[derive(Clone)]
pub(crate) struct CardsHandle {
    /// session_id → the session's one live card (accumulator + card id chain).
    pub(crate) cards: Arc<Mutex<HashMap<String, CardSession>>>,
    /// The card handles (ADR-0038, rule 2): `request_id → message_id` plus, for
    /// every card that shows a live interaction block, the card JSON as last
    /// rendered.
    pub(crate) card_handles: Arc<Mutex<CardHandles>>,
    /// session_id → the cover card's current title for topics created with a
    /// bot cover card as their root (ADR-0023).
    pub(crate) cover_titles: Arc<Mutex<HashMap<String, CoverTitle>>>,
    pub(crate) feishu: Arc<dyn feishu::Platform>,
    /// session_id → the lock serializing that session's card writes. Private:
    /// [`CardsHandle::write_lock`] is the accessor.
    write_locks: Arc<Mutex<HashMap<String, Arc<Mutex<()>>>>>,
}

impl CardsHandle {
    pub(crate) fn new(
        cards: Arc<Mutex<HashMap<String, CardSession>>>,
        card_handles: Arc<Mutex<CardHandles>>,
        cover_titles: Arc<Mutex<HashMap<String, CoverTitle>>>,
        feishu: Arc<dyn feishu::Platform>,
        write_locks: Arc<Mutex<HashMap<String, Arc<Mutex<()>>>>>,
    ) -> Self {
        Self {
            cards,
            card_handles,
            cover_titles,
            feishu,
            write_locks,
        }
    }

    /// The lock serializing card writes for `session_id`. Every path that reads
    /// a session's card state, sends the result to Feishu, and then records it
    /// must hold this across the whole sequence: `flush_card`, `split_card_chain`
    /// (which enqueues its split and flushes under the same lock) and
    /// `resolve_blocks` are the holders.
    pub(crate) async fn write_lock(&self, session_id: &str) -> Arc<Mutex<()>> {
        self.write_locks
            .lock()
            .await
            .entry(session_id.to_string())
            .or_insert_with(|| Arc::new(Mutex::new(())))
            .clone()
    }
}

/// The two request flows and the claim state their settlements use.
///
/// **Locks owned:** the flows' own state (`sent_cards`, `question_state`,
/// `answered_results`), the double-click guard `answered_requests`, the
/// settlement claims `settling_requests`, and the ADR-0028 `snapshot_claims`
/// registry. The claim sets are taken briefly and never held across a backend
/// or Feishu call; the snapshot registry is released before the PATCH its
/// rebuilt card produces. When a path needs both a flow's `sent_cards` and the
/// card-handle registry it snapshots one, drops it, then takes the other (one
/// lock order, no deadlock surface).
#[derive(Clone)]
pub(crate) struct RequestsHandle {
    /// Permission flow: owns `sent_cards`, polls pending requests, auto-accepts
    /// for `/autoaccept` sessions, and handles the "perm" card action.
    pub(crate) permission: Arc<RequestFlow>,
    /// Question flow: owns `sent_cards` + the question kind's request/partial
    /// state, polls pending questions, and handles the "question" card action.
    pub(crate) question: Arc<RequestFlow>,
    /// request ids already answered on the permission/question cards. Guards
    /// against double-click races.
    pub(crate) answered_requests: Arc<Mutex<HashSet<String>>>,
    /// Request ids whose settlement cola has STARTED, each with its claim time.
    /// While an id sits here its disappearance from the pending list is cola's
    /// own doing, so the sweep's vanished passes must not read it as another
    /// client's resolution (`resolve_blocks` clears the claim once it settled);
    /// a claim older than [`SETTLING_CLAIM_TTL`] is abandoned and dropped by
    /// [`Self::claimed_requests`].
    pub(crate) settling_requests: Arc<Mutex<HashMap<String, std::time::Instant>>>,
    /// ADR-0028 snapshot claim registry: which snapshot card hosts which
    /// adopt-time pending block.
    pub(crate) snapshot_claims: Arc<Mutex<SnapshotClaims>>,
}

impl RequestsHandle {
    /// The flow that owns requests of `kind` — the registry pairing every
    /// snapshot-claim kind with its poller and settlement state.
    pub(crate) fn flow_for(&self, kind: ClaimKind) -> &Arc<RequestFlow> {
        match kind {
            ClaimKind::Permission => &self.permission,
            ClaimKind::Question => &self.question,
        }
    }

    /// The wait blocking `session_id`, if any (ADR-0054): the two kinds'
    /// pending records combine into the card's wait vocabulary.
    pub(crate) async fn wait_for(&self, session_id: &str) -> Option<crate::feishu::card::AwaitingAction> {
        use crate::feishu::card::AwaitingAction;
        match (
            self.permission.is_pending_for(session_id).await,
            self.question.is_pending_for(session_id).await,
        ) {
            (true, true) => Some(AwaitingAction::Both),
            (true, false) => Some(AwaitingAction::Permission),
            (false, true) => Some(AwaitingAction::Question),
            (false, false) => None,
        }
    }

    /// The requests cola itself is answering or has answered — `answered_requests`
    /// plus the live `settling_requests` claims. The sweep's vanished passes
    /// take one snapshot of this per pass: a request that disappeared from the
    /// pending list because cola handled it must never be read as another
    /// client's resolution. A settlement claim older than [`SETTLING_CLAIM_TTL`]
    /// is dropped here: its task died before rendering the receipt, and
    /// suppressing the request forever would strand its card.
    pub(crate) async fn claimed_requests(&self) -> HashSet<String> {
        let mut claimed = self.answered_requests.lock().await.clone();
        let mut settling = self.settling_requests.lock().await;
        let now = std::time::Instant::now();
        settling.retain(|_, at| now.duration_since(*at) < SETTLING_CLAIM_TTL);
        claimed.extend(settling.keys().cloned());
        claimed
    }
}

/// The per-session wait state: prompt serialization, the `/stop` marker, and
/// the reminder/pin machinery for pending requests.
///
/// **Locks owned:** `inflight` and `stopped_sessions` (plain sets, taken alone
/// and held briefly, never across an await), plus the reminder's and the
/// message-pin registry's own inner mutexes. Those two inner mutexes ARE held
/// across the platform's reminder/pin calls (`set_instant_reminder`,
/// `pin_message`/`unpin_message`): the decision, the call and the state update
/// are one transition, with the failure latch updated inside the guard, so a
/// concurrent sweep cannot interleave a second pin or clear. No lock here is
/// ever nested with another handle's lock.
#[derive(Clone)]
pub(crate) struct WaitsHandle {
    /// Session ids with a prompt currently in flight (serializes prompts per
    /// session so concurrent messages don't clobber each other's cards).
    pub(crate) inflight: Arc<Mutex<HashSet<String>>>,
    /// Session ids whose run was interrupted by `/stop`; the post-prompt drain
    /// (ADR-0043) reads it so a stopped session finalizes promptly.
    pub(crate) stopped_sessions: Arc<Mutex<HashSet<String>>>,
    /// The Instant Reminder lifecycle (ADR-0043): pins a Chat/Topic while a
    /// Permission/Question is pending. Off means every method is a no-op.
    pub(crate) reminder: Arc<ReminderState>,
    /// The waiting-card pin registry (ADR-0043 amendment): pins the exact card a
    /// pending Permission/Question lives on. Same `[bridge] instant_reminder`
    /// opt-in as the reminder itself.
    pub(crate) message_pins: Arc<MessagePins>,
}

/// The turn knobs: the completion-notice flags, the injectable cadences and
/// thresholds, and the default work directory. Holds no locks (the cadences are
/// atomics).
#[derive(Clone)]
pub(crate) struct TurnConfig {
    /// Whether to send the group completion notice.
    pub(crate) group_completion_notice: bool,
    /// Whether to send the long-task completion notice in p2p.
    pub(crate) long_task_notice: bool,
    /// The long-task notice threshold (ms); injectable for tests.
    pub(crate) long_task_notice_ms: Arc<AtomicU64>,
    /// A Turn's render poll cadence (ms); injectable for tests.
    pub(crate) turn_render_poll_ms: Arc<AtomicU64>,
    /// Bound on a Turn's post-prompt drain (ms); injectable for tests.
    pub(crate) turn_drain_timeout_ms: Arc<AtomicU64>,
    /// Ceiling on the out-of-turn drain follow (ms, #284); injectable for tests.
    pub(crate) turn_follow_timeout_ms: Arc<AtomicU64>,
    /// Default directory for new sessions (from `[bridge] work_dir`).
    work_dir: Option<String>,
}

impl TurnConfig {
    pub(crate) fn new(
        group_completion_notice: bool,
        long_task_notice: bool,
        long_task_notice_ms: Arc<AtomicU64>,
        turn_render_poll_ms: Arc<AtomicU64>,
        turn_drain_timeout_ms: Arc<AtomicU64>,
        turn_follow_timeout_ms: Arc<AtomicU64>,
        work_dir: Option<String>,
    ) -> Self {
        Self {
            group_completion_notice,
            long_task_notice,
            long_task_notice_ms,
            turn_render_poll_ms,
            turn_drain_timeout_ms,
            turn_follow_timeout_ms,
            work_dir,
        }
    }

    /// The directory a brand-new session starts in: `[bridge] work_dir` when
    /// configured, else the process working directory. `/dir` still overrides
    /// per session.
    pub(crate) fn default_session_directory(&self) -> String {
        self.work_dir
            .clone()
            .filter(|d| !d.is_empty())
            .unwrap_or_else(|| {
                std::env::current_dir()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .to_string()
            })
    }

    /// The render poll cadence for a Turn (ms).
    pub(crate) fn render_poll_ms(&self) -> u64 {
        self.turn_render_poll_ms.load(Ordering::Relaxed)
    }

    /// The drain bound for a Turn (ms).
    pub(crate) fn drain_timeout_ms(&self) -> u64 {
        self.turn_drain_timeout_ms.load(Ordering::Relaxed)
    }

    /// The out-of-turn drain follow's ceiling (ms, #284).
    pub(crate) fn follow_timeout_ms(&self) -> u64 {
        self.turn_follow_timeout_ms.load(Ordering::Relaxed)
    }

    /// The long-task completion-notice threshold (ms).
    pub(crate) fn long_task_notice_ms(&self) -> u64 {
        self.long_task_notice_ms.load(Ordering::Relaxed)
    }
}

/// The narrow handle bundle a Turn runs on (spec #298, A1): exactly the
/// concerns [`Turn::run`](crate::bridge::turn::Turn::run) uses — sessions,
/// cards, the request and wait state, the backend, the platform and the turn
/// config. Built by the coordinator, never by the Turn.
#[derive(Clone)]
pub(crate) struct TurnHandles {
    pub(crate) sessions: SessionsHandle,
    pub(crate) cards: CardsHandle,
    pub(crate) requests: RequestsHandle,
    pub(crate) waits: WaitsHandle,
    pub(crate) backend: Arc<dyn crate::backend::Backend>,
    pub(crate) platform: Arc<dyn feishu::Platform>,
    pub(crate) config: TurnConfig,
}

/// The bundle every flow runs on (spec #298, B): the four per-concern handles
/// plus the two adapters. The request flow and the external-message flow use it
/// whole; the command, topic and poll bundles embed it and add their own state.
///
/// A bundle holds no locks of its own — it is a named combination of the
/// handles above, each of which documents its own locks.
#[derive(Clone)]
pub(crate) struct FlowHandles {
    pub(crate) sessions: SessionsHandle,
    pub(crate) cards: CardsHandle,
    pub(crate) requests: RequestsHandle,
    pub(crate) waits: WaitsHandle,
    pub(crate) backend: Arc<dyn crate::backend::Backend>,
    pub(crate) platform: Arc<dyn feishu::Platform>,
}

/// The OpenCode server-ownership concern (ADR-0013): the start policy, the
/// preferred port, and the lock serializing every server mutation.
///
/// **Locks owned:** `lock`, taken for a whole reconcile pass or a demand
/// spawn. It is the outermost lock of the bridge: it is never acquired while
/// holding any handle lock, so no ordering can invert against them. Under it
/// the reconcile pass reads `waits.inflight` (the only handle lock taken while
/// it is held) and calls `backend.reconnect`.
#[derive(Clone)]
pub(crate) struct ServerHandle {
    /// When cola may spawn its own `opencode serve` (`auto`/`never`/`eager`).
    pub(crate) start_policy: ServerStartPolicy,
    /// Preferred port from `[opencode] url`, a tiebreaker in `pick_server`.
    pub(crate) preferred_port: Option<u16>,
    /// Serializes every server mutation — Lazy Start spawns, the reconnect
    /// loop's re-attach/yield.
    pub(crate) lock: Arc<Mutex<()>>,
}

/// The poller bundle: the flow handles plus the server-ownership state the
/// reconcile loop mutates.
#[derive(Clone)]
pub(crate) struct PollHandles {
    pub(crate) flow: FlowHandles,
    pub(crate) server: ServerHandle,
}

/// The read-side bundle a Session Snapshot gather and its claim filter need:
/// no platform (the card is built as JSON and sent by the caller) and no wait
/// state (nothing here blocks on a prompt).
#[derive(Clone)]
pub(crate) struct SnapshotHandles {
    pub(crate) sessions: SessionsHandle,
    pub(crate) cards: CardsHandle,
    pub(crate) requests: RequestsHandle,
    pub(crate) backend: Arc<dyn crate::backend::Backend>,
    /// Whether Message Pin is on (`[bridge] instant_reminder`): the snapshot's
    /// 等待你的确认 pointer may promise 置顶 only then (ADR-0028 update
    /// 2026-09-25). Config, not wait state — nothing here blocks.
    pub(crate) pins_enabled: bool,
}

impl SnapshotHandles {
    /// The snapshot subset of a flow bundle.
    pub(crate) fn from_flow(flow: &FlowHandles) -> Self {
        Self {
            sessions: flow.sessions.clone(),
            cards: flow.cards.clone(),
            requests: flow.requests.clone(),
            backend: Arc::clone(&flow.backend),
            pins_enabled: flow.waits.message_pins.enabled(),
        }
    }
}

/// The topic-opening bundle: the flow handles plus the external flow whose
/// snapshot-settle helper an adopted topic calls after its in-topic seed lands.
#[derive(Clone)]
pub(crate) struct TopicHandles {
    pub(crate) flow: FlowHandles,
    pub(crate) external: Arc<ExternalFlow>,
}

impl TopicHandles {
    /// The snapshot subset of this bundle.
    pub(crate) fn snapshot_handles(&self) -> SnapshotHandles {
        SnapshotHandles::from_flow(&self.flow)
    }
}

/// The command bundle: the flow handles plus the turn config (the default
/// session directory a bare `/topic`/`/new` falls back to) and the external
/// flow the adoption tails settle a sent snapshot through.
#[derive(Clone)]
pub(crate) struct CommandHandles {
    pub(crate) flow: FlowHandles,
    pub(crate) config: TurnConfig,
    pub(crate) external: Arc<ExternalFlow>,
}

impl CommandHandles {
    /// The conversation's current project (ADR-0012): the Pending Session's
    /// directory when one is declared, else the active session's, falling back
    /// to the default directory only when the conversation has neither.
    pub(crate) async fn current_project_directory(&self, thread_key: &ThreadKey) -> String {
        self.flow
            .sessions
            .current_directory(thread_key)
            .await
            .filter(|d| !d.is_empty())
            .unwrap_or_else(|| self.config.default_session_directory())
    }

    /// Declare (or replace) the conversation's Pending Session in its current
    /// project (ADR-0041) — the shape `/new` and the switch card's 新建 share.
    pub(crate) async fn declare_pending_in_current_project(
        &self,
        thread_key: &ThreadKey,
        title: Option<String>,
    ) -> crate::error::Result<PendingEntry> {
        let directory = self.current_project_directory(thread_key).await;
        self.flow
            .sessions
            .declare_pending(thread_key, directory, title)
            .await
    }

    /// The topic-opening subset of this bundle.
    pub(crate) fn topic_handles(&self) -> TopicHandles {
        TopicHandles {
            flow: self.flow.clone(),
            external: Arc::clone(&self.external),
        }
    }

    /// The snapshot subset of this bundle.
    pub(crate) fn snapshot_handles(&self) -> SnapshotHandles {
        SnapshotHandles::from_flow(&self.flow)
    }
}
