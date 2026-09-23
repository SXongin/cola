//! Narrow handles (spec #298, ticket A1): per-concern views over the state the
//! coordinator owns.
//!
//! [`SharedCore`](crate::bridge::core::SharedCore) stays the aggregate root and
//! the one construction point; a flow receives only the handles it uses, so its
//! dependencies — and the locks it may take — are visible at its interface.
//! Handles delegate to the existing stores ([`SessionStore`], the card-handle
//! registry, the request flows); they never duplicate state.
//!
//! The first application is the Turn ([`TurnHandles`]); ticket B extends the
//! pattern to the other flows and pollers.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use tokio::sync::Mutex;

use crate::bridge::card_handles::CardHandles;
use crate::bridge::core::{CoverTitle, SessionListCache};
use crate::bridge::reminder::ReminderState;
use crate::bridge::request::RequestFlow;
use crate::bridge::session::SessionStore;
use crate::bridge::snapshot_claims::SnapshotClaims;
use crate::bridge::turn::CardSession;
use crate::config::{SessionEntry, ThreadKey};
use crate::{feishu, opencode};

/// The session map and the session-list cache its write paths invalidate.
///
/// The two travel together because a create/remove/activate changes what
/// `/list` and `/switch` must show; a caller that only reads sessions still
/// goes through the same handle.
#[derive(Clone)]
pub(crate) struct SessionsHandle {
    pub(crate) store: Arc<Mutex<SessionStore>>,
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

    /// Create a brand-new session on the current server and make it the active
    /// one for the thread. Used when a mapped session no longer exists (404).
    /// The per-session overrides reset to defaults, but the topic's creation
    /// messages (`topic_anchor`/`topic_root`, ADR-0023) are Feishu message ids —
    /// not session state — and survive so the quote-injection guard keeps
    /// working after the recreate.
    pub(crate) async fn create_fresh_session(
        &self,
        backend: &Arc<dyn opencode::Backend>,
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
        backend: &Arc<dyn opencode::Backend>,
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
    /// must hold this across the whole sequence: `flush_card` and
    /// `resolve_blocks` are the two.
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
    /// Request ids whose settlement cola has STARTED — see
    /// `SharedCore::settling_requests`.
    pub(crate) settling_requests: Arc<Mutex<HashMap<String, std::time::Instant>>>,
    /// ADR-0028 snapshot claim registry: which snapshot card hosts which
    /// adopt-time pending block.
    pub(crate) snapshot_claims: Arc<Mutex<SnapshotClaims>>,
}

/// The per-session wait state: prompt serialization, the `/stop` marker, and
/// the reminder/pin machinery for pending requests.
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
}

/// The turn knobs: the completion-notice flags, the injectable cadences and
/// thresholds, and the default work directory.
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
        work_dir: Option<String>,
    ) -> Self {
        Self {
            group_completion_notice,
            long_task_notice,
            long_task_notice_ms,
            turn_render_poll_ms,
            turn_drain_timeout_ms,
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
    pub(crate) backend: Arc<dyn opencode::Backend>,
    pub(crate) platform: Arc<dyn feishu::Platform>,
    pub(crate) config: TurnConfig,
}
