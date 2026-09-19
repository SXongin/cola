use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use tokio::sync::Mutex;

use crate::bridge::session::{PendingEntry, SessionSettings, SessionStore};
use crate::config::{SessionEntry, ThreadKey};
use crate::feishu;
use crate::opencode;

/// A cached session-list snapshot (cross-store, most recently active first)
/// with its fetch time. The 30 s TTL keeps `/list`/`/switch`/`/attach` off the
/// wire for rapid reuse; cola invalidates it immediately on create/adopt/rename.
#[derive(Clone)]
pub struct SessionListCache {
    pub fetched_at: std::time::Instant,
    pub sessions: Vec<opencode::types::SessionListInfo>,
}

impl SessionListCache {
    /// Whether the snapshot is still fresh (fetched within the TTL).
    pub fn fresh(&self) -> bool {
        self.fetched_at.elapsed() < std::time::Duration::from_secs(30)
    }
}

/// The title shown on a topic's cover card, plus the model line it carries —
/// the post-turn hook compares the server title against this and patches the
/// card in place when it changed (ADR-0023).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CoverTitle {
    pub title: String,
    pub model: Option<String>,
    /// The cover was recorded while the topic's session was still a Pending
    /// Session (ADR-0041), so it still says 「下一条消息创建」. The first sync
    /// after materialisation must re-render the full brief even when the title
    /// matches (the pending title and the server title can be identical, e.g.
    /// `/topic <dir> <name>`).
    pub pending: bool,
}

/// Bound on a server-side session-info fetch (`GET /session/{id}`) — the
/// prompt subtitle's title read and the effective-model ladder's
/// server-recorded rung both go through it. A freshly spawned Owned Server
/// (Lazy Start) can swallow the first requests in its startup window, and a
/// hung fetch must degrade that one field — not hang the whole turn or the
/// `/think`/`/model` cards (the Lazy Start silent-hang incident).
pub(crate) const SESSION_INFO_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(3);

/// How long a settlement claim may sit before it is treated as abandoned (see
/// `SharedCore::claimed_requests`). A live settlement renders its receipt within
/// one Feishu round trip; the bound only ever fires when the task that took the
/// claim was cancelled.
pub(crate) const SETTLING_CLAIM_TTL: std::time::Duration = std::time::Duration::from_secs(30);

/// State shared across every flow: the session map, the per-session live cards
/// ([`CardSession`] — accumulator + card identity in one place), the two
/// request flows (permission/question pollers + card actions, on the core so
/// snapshot claims and question remembering are reachable from the command
/// layer, ADR-0028), the double-click guard, prompt serialization, and the two
/// adapters. Owned by the bridge coordinator ([`super::App`]) and passed by
/// handle to the flow modules that need it.
pub struct SharedCore {
    pub sessions: Arc<Mutex<SessionStore>>,
    /// session_id → the session's one live card (accumulator + card id chain).
    pub cards: Arc<Mutex<HashMap<String, crate::bridge::streaming::CardSession>>>,
    /// The card handles (ADR-0038, rule 2): `request_id → message_id` plus, for
    /// every card that shows a live interaction block, the card JSON as last
    /// rendered. Lets any card showing a block be repainted — a click's ack, a
    /// remote resolution, a sweep strip — whether or not it is still the
    /// accumulator's current card.
    pub card_handles: Arc<Mutex<crate::bridge::card_handles::CardHandles>>,
    /// Permission flow: owns `sent_cards`, polls pending requests, auto-accepts
    /// for `/autoaccept` sessions, and handles the "perm" card action.
    pub permission: crate::bridge::request::RequestFlow,
    /// Question flow: owns `sent_cards` + the question kind's request/partial
    /// state, polls pending questions, and handles the "question" card action.
    pub question: crate::bridge::request::RequestFlow,
    /// External-message flow: owns `last_user_msg_epoch`, notifies Feishu when
    /// another shared-store client posts while cola is idle, and arms the
    /// external-reply renderers (including the busy-adopt follow, ADR-0028).
    pub external: crate::bridge::external::ExternalFlow,
    /// ADR-0028 snapshot claim registry: which snapshot card hosts which
    /// adopt-time pending block, what each snapshot was built from, and the
    /// tombstones for late second clicks. One Mutex keeps claim/host/tombstone
    /// mutations atomic (they always change together). The poll loop treats a
    /// claimed id as already-surfaced (no standalone card, no re-inline); a
    /// claimed id leaving the pending list drops its block.
    pub snapshot_claims: Arc<Mutex<crate::bridge::snapshot_claims::SnapshotClaims>>,
    /// request ids already answered on the permission/question cards. Guards
    /// against double-click races (two card callbacks before the result card
    /// replaces the buttons): a second click on the same request is ignored
    /// server-side instead of re-replying. Also tells `mark_stale_cards` that
    /// cola's answer already settled a standalone card.
    pub answered_requests: Arc<Mutex<HashSet<String>>>,
    /// Request ids whose settlement cola has STARTED — the auto-accept
    /// approval's reply has landed (or is landing) but the mode receipt has
    /// not rendered yet. While an id sits here its disappearance from the
    /// pending list is cola's own doing, so the sweep's vanished passes must
    /// not read it as another client's resolution (`resolve_blocks` clears the
    /// claim once it settled). Kept apart from `answered_requests`: that one
    /// makes `mark_stale_cards` skip a card, and a standalone card approved by
    /// `/autoaccept on` still needs the sweep to repaint its buttons away.
    /// Each entry carries its claim time; a claim older than
    /// [`SETTLING_CLAIM_TTL`] is abandoned (the settlement task was cancelled
    /// between the approval and the receipt) and `claimed_requests` drops it,
    /// so the sweep can finish the request instead of suppressing it forever.
    pub settling_requests: Arc<Mutex<HashMap<String, std::time::Instant>>>,
    /// Session ids with a prompt currently in flight (serializes prompts per
    /// session so concurrent messages don't clobber each other's cards).
    pub inflight: Arc<Mutex<HashSet<String>>>,
    /// Session ids whose run was interrupted by `/stop`. The post-prompt drain
    /// (ADR-0043) reads it so a stopped session finalizes promptly instead of
    /// waiting out its bound on a Supplement the abort left unanswered; a new
    /// Turn clears the marker when it starts.
    pub stopped_sessions: Arc<Mutex<HashSet<String>>>,
    /// A Turn's render poll cadence (ms): the incremental renderer while the
    /// prompt is in flight, and the post-prompt drain that keeps rendering a
    /// Supplement's new Turn (ADR-0043). Defaults to 1.5 s; tests store a small
    /// value so the drain's branches run without real seconds (the
    /// external-poller atomics pattern).
    pub turn_render_poll_ms: std::sync::atomic::AtomicU64,
    /// Bound on a Turn's post-prompt drain (ms): how long the render poll (and
    /// the inflight guard) stays alive waiting for a Supplement's new Turn
    /// before finalization (ADR-0043). Defaults to the external renderer's
    /// 10 min; tests store a small value to exercise the bound.
    pub turn_drain_timeout_ms: std::sync::atomic::AtomicU64,
    /// session_id → the cover card's current title for topics created with a
    /// bot cover card as their root (ADR-0023). In-memory only: the post-turn
    /// hook compares the server title and patches the cover card in place when
    /// it changed; after a restart the next completed turn re-syncs the card
    /// once (same content, harmless).
    pub cover_titles: Arc<Mutex<HashMap<String, CoverTitle>>>,
    /// Default directory for new sessions (from `[bridge] work_dir`).
    pub work_dir: Option<String>,
    /// Whether to send the group completion notice (from `[bridge] group_completion_notice`).
    pub group_completion_notice: bool,
    /// The Instant Reminder lifecycle (ADR-0043, from `[bridge] instant_reminder`): pins a
    /// Chat/Topic while a Permission/Question is pending. Off means every
    /// method is a no-op — no reminder call is ever made.
    pub pins: crate::bridge::pin::PinState,
    /// Cached session-list snapshot for `/list`, `/switch`, `/attach`
    /// (30 s TTL; invalidated on create/adopt/rename). Private: the core's
    /// write wrappers and `invalidate_session_list_cache` own it.
    session_list_cache: Arc<Mutex<Option<SessionListCache>>>,
    pub opencode: Arc<dyn opencode::Backend>,
    pub feishu: Arc<dyn feishu::Platform>,
    /// When cola may spawn its own `opencode serve` (`auto`/`never`/`eager`,
    /// ADR-0013). Drives the Lazy Start hook and the yield decision.
    pub server_start: crate::config::ServerStartPolicy,
    /// Preferred port from `[opencode] url`, a tiebreaker among servers of the
    /// same class in `pick_server` (ADR-0013).
    pub preferred_port: Option<u16>,
    /// Serializes every server mutation — Lazy Start spawns, the reconnect
    /// loop's re-attach/yield — so concurrent first messages can't double-spawn
    /// or race a yield with a reconnect.
    pub server_lock: Arc<tokio::sync::Mutex<()>>,
    /// session_id → the lock serializing that session's card writes. A card
    /// write is a read-send-record sequence (the flush's build→PATCH→record, a
    /// resolution's mutate→ack/PATCH), and two of them interleaving land out of
    /// order: a stale flush PATCHes a resolution away, or a split's
    /// continuation content is written onto the card it just finalized. One
    /// writer per session at a time; entries are never evicted (bounded by the
    /// session store). Private: `card_write_lock` is the accessor.
    card_write_locks: Arc<Mutex<HashMap<String, Arc<Mutex<()>>>>>,
}

impl SharedCore {
    pub fn new(
        cfg: &crate::config::Config,
        opencode: Arc<dyn opencode::Backend>,
        feishu: Arc<dyn feishu::Platform>,
    ) -> anyhow::Result<Self> {
        let session_store = SessionStore::new(cfg.bridge.session_file.clone())?;
        Ok(Self {
            sessions: Arc::new(Mutex::new(session_store)),
            cards: Arc::new(Mutex::new(HashMap::new())),
            card_handles: Arc::new(Mutex::new(crate::bridge::card_handles::CardHandles::default())),
            permission: crate::bridge::request::RequestFlow::new(Box::new(
                crate::bridge::request::PermissionKind,
            )),
            question: crate::bridge::request::RequestFlow::new(Box::new(
                crate::bridge::request::QuestionKind,
            )),
            external: crate::bridge::external::ExternalFlow::new(),
            snapshot_claims: Arc::new(Mutex::new(
                crate::bridge::snapshot_claims::SnapshotClaims::default(),
            )),
            answered_requests: Arc::new(Mutex::new(HashSet::new())),
            settling_requests: Arc::new(Mutex::new(HashMap::new())),
            inflight: Arc::new(Mutex::new(HashSet::new())),
            stopped_sessions: Arc::new(Mutex::new(HashSet::new())),
            turn_render_poll_ms: std::sync::atomic::AtomicU64::new(1_500),
            turn_drain_timeout_ms: std::sync::atomic::AtomicU64::new(600_000),
            cover_titles: Arc::new(Mutex::new(HashMap::new())),
            work_dir: cfg
                .bridge
                .work_dir
                .clone()
                .map(|p| p.to_string_lossy().to_string()),
            group_completion_notice: cfg.bridge.group_completion_notice,
            pins: crate::bridge::pin::PinState::new(cfg.bridge.instant_reminder),
            session_list_cache: Arc::new(Mutex::new(None)),
            opencode,
            feishu,
            server_start: cfg.opencode.start_server,
            preferred_port: cfg.opencode.preferred_port(),
            server_lock: Arc::new(tokio::sync::Mutex::new(())),
            card_write_locks: Arc::new(Mutex::new(HashMap::new())),
        })
    }

    /// The lock serializing card writes for `session_id` (see
    /// `card_write_locks`). Every path that reads a session's card state, sends
    /// the result to Feishu, and then records it must hold this across the
    /// whole sequence: `flush_card` and `resolve_blocks` are the two.
    pub(crate) async fn card_write_lock(&self, session_id: &str) -> Arc<Mutex<()>> {
        self.card_write_locks
            .lock()
            .await
            .entry(session_id.to_string())
            .or_insert_with(|| Arc::new(Mutex::new(())))
            .clone()
    }

    /// The requests cola itself is answering or has answered — `answered_requests`
    /// plus the live `settling_requests` claims. The sweep's vanished passes
    /// take one snapshot of this per pass: a request that disappeared from the
    /// pending list because cola handled it must never be read as another
    /// client's resolution. A settlement claim older than
    /// [`SETTLING_CLAIM_TTL`] is dropped here: its task died before rendering
    /// the receipt, and suppressing the request forever would strand its card.
    pub(crate) async fn claimed_requests(&self) -> HashSet<String> {
        let mut claimed = self.answered_requests.lock().await.clone();
        let mut settling = self.settling_requests.lock().await;
        let now = std::time::Instant::now();
        settling.retain(|_, at| now.duration_since(*at) < SETTLING_CLAIM_TTL);
        claimed.extend(settling.keys().cloned());
        claimed
    }

    /// The directory a brand-new session starts in: `[bridge] work_dir` when
    /// configured, else the process working directory. `/dir` still overrides
    /// per session.
    pub fn default_session_directory(&self) -> String {
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

    /// The conversation's current project (ADR-0012): the Pending Session's
    /// directory when one is declared, else the active session's, falling back
    /// to the default directory only when the conversation has neither.
    /// Single definition of "current project", shared by `/new`, the bare
    /// `/topic` form, and the `/switch` card's "new session" action.
    pub async fn current_project_directory(&self, thread_key: &ThreadKey) -> String {
        self.sessions
            .lock()
            .await
            .current_directory(thread_key)
            .filter(|d| !d.is_empty())
            .unwrap_or_else(|| self.default_session_directory())
    }

    /// The per-session agent override set by `/agent` (from the persisted
    /// `SessionEntry`). `None` when the session has no override — the server
    /// then uses the session's own/default agent.
    pub async fn session_agent_override(&self, session_id: &str) -> Option<String> {
        self.sessions
            .lock()
            .await
            .entry_for_session(session_id)
            .and_then(|e| e.agent.clone())
    }

    /// The per-session model override set by `/model`, parsed from the
    /// persisted "provider/model" string. `None` when the session has no
    /// override (the client then falls back to the configured default model, or
    /// the server's own default if none is configured).
    pub async fn session_model_override(&self, session_id: &str) -> Option<opencode::types::ModelInfo> {
        self.sessions
            .lock()
            .await
            .entry_for_session(session_id)
            .and_then(|e| e.model.as_deref())
            .and_then(opencode::parsing::parse_model)
    }

    /// The per-session `/think` variant override (from the persisted
    /// `SessionEntry`). `None` when unset — the server's default for whatever
    /// model runs this turn.
    pub async fn session_variant_override(&self, session_id: &str) -> Option<String> {
        self.sessions
            .lock()
            .await
            .entry_for_session(session_id)
            .and_then(|e| e.variant.clone())
    }

    /// The model the NEXT turn will actually run, resolved settings override →
    /// configured default → server-recorded session model (`GET /session/{id}`).
    /// `None` only when every rung fails (no override, no config, server
    /// unreachable) — the `/think` card then tells the user to `/model` first,
    /// and the `/model` picker omits its current-model line. Returns
    /// `(provider, model)`. A Pending Session has no server-recorded rung
    /// (nothing exists on the server yet, ADR-0041).
    pub async fn effective_model(&self, settings: &SessionSettings) -> Option<(String, String)> {
        // 1. The `/model` override in the snapshot.
        if let Some(m) = settings.model.as_deref().and_then(opencode::parsing::parse_model) {
            return Some((m.provider_id, m.id));
        }
        // 2. The configured default (`[opencode] model`).
        if let Some(m) = self.opencode.configured_default_model() {
            return Some((m.provider_id, m.id));
        }
        // 3. What the server actually recorded for the session. Bounded: a
        //    hung server degrades the ladder (no current-model line / a
        //    `/think` "pick a model" prompt), never the card send.
        let session_id = settings.session_id.as_deref()?;
        if !settings.directory.is_empty()
            && let Ok(Ok(info)) = tokio::time::timeout(
                SESSION_INFO_TIMEOUT,
                self.opencode.session_info(session_id, Some(&settings.directory)),
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
    pub async fn model_variants(&self, provider: &str, model: &str) -> Option<Vec<String>> {
        self.opencode
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
    /// the advertised catalog is left alone (can't be judged, so it is not
    /// destroyed — the server error is the fallback).
    pub async fn clear_variant_for_model(
        &self,
        variant: &mut Option<String>,
        model_spec: &str,
    ) -> Option<String> {
        if let Some(v) = variant.clone()
            && let Some(m) = crate::opencode::parsing::parse_model(model_spec)
            && let Some(variants) = self.model_variants(&m.provider_id, &m.id).await
            && !variants.iter().any(|x| x == &v)
        {
            *variant = None;
            Some(v)
        } else {
            None
        }
    }

    /// The session mapped to a thread (if any).
    pub async fn get_session_id(&self, thread_key: &ThreadKey) -> Option<String> {
        self.sessions
            .lock()
            .await
            .get_active(thread_key)
            .map(|e| e.session_id.clone())
    }

    /// The current `GET /session` snapshot, fetching (and caching for 30 s) when
    /// missing or stale. Used by `/list`, `/switch` and `/attach` so rapid
    /// reuse stays off the wire.
    pub(crate) async fn cached_session_list(
        &self,
    ) -> crate::error::Result<Vec<opencode::types::SessionListInfo>> {
        let now = std::time::Instant::now();
        {
            let cache = self.session_list_cache.lock().await;
            if let Some(c) = cache.as_ref()
                && c.fresh()
            {
                return Ok(c.sessions.clone());
            }
        }
        let sessions = self.opencode.list_sessions().await?;
        *self.session_list_cache.lock().await = Some(SessionListCache {
            fetched_at: now,
            sessions: sessions.clone(),
        });
        Ok(sessions)
    }

    /// Drop the `/list` cache. Called whenever cola creates, adopts, forgets or
    /// renames a session, so the next `/list`/`/switch`/`/attach` is fresh.
    pub(crate) async fn invalidate_session_list_cache(&self) {
        *self.session_list_cache.lock().await = None;
    }

    /// Persist `entry` as its thread's active session and drop the session-list
    /// cache: creating or adopting a session changes what `/list` and `/switch`
    /// should offer. The cache is dropped even when the save fails, because the
    /// in-memory mapping already changed.
    pub(crate) async fn activate_session(&self, entry: SessionEntry) -> crate::error::Result<()> {
        let result = self.sessions.lock().await.activate(entry);
        self.invalidate_session_list_cache().await;
        result
    }

    /// Declare (or replace) the conversation's Pending Session and persist
    /// (ADR-0041). The session-list cache is untouched: a pending is not a
    /// server session, so `/list`/`/switch` have nothing new to show.
    pub(crate) async fn set_pending_session(
        &self,
        pending: crate::bridge::session::PendingEntry,
    ) -> crate::error::Result<()> {
        self.sessions.lock().await.set_pending(pending)
    }

    /// Declare (or replace) a Pending Session rooted at an explicit
    /// `directory` (ADR-0041) — the shape `/dir`, its card pick and the other
    /// explicit-directory forms share. Returns the declared pending, whose
    /// directory the confirmation names. Replacing a topic's pending keeps its
    /// `topic_root`/`topic_anchor` (ADR-0023): they are properties of the
    /// Feishu topic, not of the abandoned directory intent, so a corrected
    /// pending still routes fallback cards into the topic and still suppresses
    /// the topic's own creation messages from Quoted Context.
    pub(crate) async fn declare_pending(
        &self,
        thread_key: &ThreadKey,
        directory: impl Into<String>,
        title: Option<String>,
    ) -> crate::error::Result<PendingEntry> {
        let mut pending = PendingEntry::new(thread_key.clone(), directory);
        pending.title = title;
        {
            let store = self.sessions.lock().await;
            if let Some(replaced) = store.pending_for(thread_key) {
                pending.topic_anchor = replaced.topic_anchor.clone();
                pending.topic_root = replaced.topic_root.clone();
            }
        }
        self.set_pending_session(pending.clone()).await?;
        Ok(pending)
    }

    /// Declare (or replace) the conversation's Pending Session in its current
    /// project (ADR-0041) — the shape `/new` and the switch card's 新建 share.
    /// Returns the declared pending, whose directory the confirmation names.
    pub(crate) async fn declare_pending_in_current_project(
        &self,
        thread_key: &ThreadKey,
        title: Option<String>,
    ) -> crate::error::Result<PendingEntry> {
        let directory = self.current_project_directory(thread_key).await;
        self.declare_pending(thread_key, directory, title).await
    }

    /// Mutate the conversation's Pending Session and persist (ADR-0041:
    /// `/name` and the settings commands configure a pending). `false` when
    /// the thread has none.
    pub(crate) async fn update_pending<F>(&self, thread_key: &ThreadKey, f: F) -> crate::error::Result<bool>
    where
        F: FnOnce(&mut PendingEntry),
    {
        self.sessions.lock().await.update_pending(thread_key, f)
    }

    /// The settings the conversation's next prompt will use (ADR-0041): the
    /// Pending Session's when one exists, else the active session's. `None`
    /// when the thread has neither. The four settings commands
    /// (`/agent` `/model` `/think` `/autoaccept`), their cards and the
    /// effective-model ladder all read through this one accessor.
    pub(crate) async fn session_settings(&self, thread_key: &ThreadKey) -> Option<SessionSettings> {
        self.sessions.lock().await.settings(thread_key)
    }

    /// Write a [`SessionSettings`] snapshot back to its target (a real session
    /// by id, else the thread's Pending Session) and persist. `false` when the
    /// target is gone.
    pub(crate) async fn set_session_settings(
        &self,
        thread_key: &ThreadKey,
        settings: SessionSettings,
    ) -> crate::error::Result<bool> {
        self.sessions.lock().await.set_settings(thread_key, settings)
    }

    /// Mutate the mapped session in place and persist, returning the updated
    /// entry (`None` when `session_id` is not mapped). The session-list cache
    /// is untouched: per-session overrides are not server-list state.
    pub(crate) async fn update_session<F>(
        &self,
        session_id: &str,
        f: F,
    ) -> crate::error::Result<Option<SessionEntry>>
    where
        F: FnOnce(&mut SessionEntry),
    {
        self.sessions.lock().await.update(session_id, f)
    }

    /// Remove a mapping and persist, dropping the session-list cache (the
    /// `/list`/`/switch` view may no longer mention it).
    pub(crate) async fn remove_session(
        &self,
        session_id: &str,
    ) -> crate::error::Result<Option<SessionEntry>> {
        let result = self.sessions.lock().await.remove_persist(session_id);
        self.invalidate_session_list_cache().await;
        result
    }

    /// Remove every mapping of a thread and persist, dropping the
    /// session-list cache (`/switch forget`).
    pub(crate) async fn remove_thread_sessions(
        &self,
        key: &ThreadKey,
    ) -> crate::error::Result<Vec<SessionEntry>> {
        let result = self.sessions.lock().await.remove_thread_persist(key);
        self.invalidate_session_list_cache().await;
        result
    }

    /// Turn a session's Auto-Accept flag on/off, resolving the owning session
    /// (which may be a parent of a sub-task child) and approving any
    /// already-pending permissions when turning on. Mirrors `/autoaccept` and is
    /// shared by the permission-card toggle so both paths stay in lockstep.
    /// Returns the ids of the pending requests that were approved (empty when
    /// `on` is false), so the caller can drop their inline card sections.
    pub(crate) async fn set_auto_accept(&self, session_id: &str, directory: &str, on: bool) -> Vec<String> {
        let approved = if on {
            self.approve_pending_for_session(session_id, directory).await
        } else {
            Vec::new()
        };
        // Resolve the SessionStore entry that owns the flag: `session_id`
        // itself, or its nearest ancestor (sub-task children are not in the
        // store, ADR-0010). Walking the chain makes a child's card flip the
        // parent's flag, consistent with `should_auto_accept`.
        let owner = crate::bridge::pollers::walk_parent_chain(self, session_id, Some(directory), |current| {
            let current = current.to_string();
            async move {
                let sessions = self.sessions.lock().await;
                sessions.entry_for_session(&current).cloned()
            }
        })
        .await;
        if let Some(entry) = owner
            && let Err(e) = self
                .update_session(&entry.session_id, |e| e.auto_accept = on)
                .await
        {
            tracing::warn!("set_auto_accept: persist failed: {}", e);
        }
        approved
    }

    /// After `/autoaccept on`: answer every permission request that is ALREADY
    /// pending for `session_id` (or one of its sub-task child sessions) with
    /// "once". The permission poller's `seen` set skips requests it has already
    /// surfaced, so enabling autoaccept would otherwise leave old cards hanging
    /// forever. Returns the ids of the requests that were approved.
    pub(crate) async fn approve_pending_for_session(&self, session_id: &str, directory: &str) -> Vec<String> {
        let Ok(perms) = self
            .opencode
            .clone()
            .for_directory(directory)
            .list_permissions()
            .await
        else {
            return Vec::new();
        };
        let mut approved = Vec::new();
        for p in &perms {
            // Match the session itself or a sub-task child (its parent chain).
            let sid = p.session_id.clone().unwrap_or_default();
            if !crate::bridge::request::session_belongs_to(self, &sid, session_id, directory).await {
                continue;
            }
            // Take the settlement claim BEFORE the reply lands: the request
            // leaves the server's pending list the moment it is applied, and a
            // sweep landing in that window would otherwise read the
            // disappearance as another client's resolution and stamp
            // `⏱ 已由其他客户端处理` on the card — the lie the Host saw when
            // enabling auto-accept. `resolve_blocks` clears the claim once the
            // true receipt rendered; a failed reply releases it right here.
            let claimed_here = self
                .settling_requests
                .lock()
                .await
                .insert(p.request_id.clone(), std::time::Instant::now())
                .is_none();
            match self
                .opencode
                .clone()
                .for_directory(directory)
                .reply_permission(&p.request_id, "once")
                .await
            {
                Ok(()) => {
                    tracing::info!(
                        "Auto-accepted pending permission {} on session {} ({})",
                        p.request_id,
                        sid,
                        p.permission.as_deref().unwrap_or("?")
                    );
                    approved.push(p.request_id.clone());
                }
                Err(e) => {
                    if claimed_here {
                        self.settling_requests.lock().await.remove(&p.request_id);
                    }
                    tracing::warn!("auto-accept pending {} on session {}: {}", p.request_id, sid, e);
                }
            }
        }
        approved
    }

    /// Whether `candidate` is `root` or a sub-task child reachable by walking
    /// up its parent chain (sub-task child sessions carry their own sessionID).
    /// Shared with the turn-end leftover rejection (#187), which filters the
    /// same way when deciding whose requests a dead turn owns.
    pub(crate) async fn session_descends_from(&self, candidate: &str, root: &str, directory: &str) -> bool {
        crate::bridge::pollers::walk_parent_chain(self, candidate, Some(directory), |current| {
            let current = current.to_string();
            async move { (current == root).then_some(true) }
        })
        .await
        .unwrap_or(false)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bridge::test_support::{MockBackend, build_app, realistic_parts, test_config, test_work_dir};
    use std::sync::atomic::Ordering;

    /// The cache rule is part of the session write interface: creates and
    /// removes change what `/list` should show, overrides do not.
    #[tokio::test]
    async fn activate_invalidates_list_cache_but_update_does_not() {
        let _wd = test_work_dir();
        let dir = tempfile::tempdir().unwrap();
        let cfg = test_config(&dir.path().join("sessions.json"));
        let backend = MockBackend::new(realistic_parts());
        let sessions_fetches = backend.list_sessions_calls.clone();
        let (app, _platform) = build_app(cfg, backend).await;
        let key = ThreadKey::new("chat_1".into(), "chat_1".into());

        app.cached_session_list().await.unwrap();
        assert_eq!(sessions_fetches.load(Ordering::SeqCst), 1);
        app.cached_session_list().await.unwrap();
        assert_eq!(
            sessions_fetches.load(Ordering::SeqCst),
            1,
            "second read is cached"
        );

        app.activate_session(SessionEntry::new(key, "ses_x", "/work/x"))
            .await
            .unwrap();
        app.cached_session_list().await.unwrap();
        assert_eq!(sessions_fetches.load(Ordering::SeqCst), 2, "activate invalidates");

        let updated = app
            .update_session("ses_x", |e| e.agent = Some("build".into()))
            .await
            .unwrap()
            .expect("mapped session");
        assert_eq!(updated.agent.as_deref(), Some("build"));
        app.cached_session_list().await.unwrap();
        assert_eq!(
            sessions_fetches.load(Ordering::SeqCst),
            2,
            "update keeps the cache"
        );

        assert!(app.remove_session("ses_x").await.unwrap().is_some());
        app.cached_session_list().await.unwrap();
        assert_eq!(sessions_fetches.load(Ordering::SeqCst), 3, "remove invalidates");
    }

    /// A settlement claim older than the TTL is abandoned: the sweep must not
    /// keep suppressing a request whose settlement task was cancelled between
    /// the reply and the receipt.
    #[tokio::test]
    async fn claimed_requests_drops_an_abandoned_settlement_claim() {
        let _wd = test_work_dir();
        let dir = tempfile::tempdir().unwrap();
        let cfg = test_config(&dir.path().join("sessions.json"));
        let (app, _platform) = build_app(cfg, MockBackend::new(realistic_parts())).await;

        let stale = std::time::Instant::now()
            .checked_sub(SETTLING_CLAIM_TTL + std::time::Duration::from_secs(1))
            .unwrap();
        app.settling_requests.lock().await.insert("per_old".into(), stale);
        let claimed = app.claimed_requests().await;

        assert!(!claimed.contains("per_old"));
        assert!(
            app.settling_requests.lock().await.is_empty(),
            "the abandoned claim is pruned, not re-read"
        );

        // A fresh claim is honoured.
        app.settling_requests
            .lock()
            .await
            .insert("per_new".into(), std::time::Instant::now());
        assert!(app.claimed_requests().await.contains("per_new"));
    }

    /// The override write path must not silently switch the Active Session:
    /// `set_auto_accept` targets any mapped session (it walks the parent
    /// chain), so a toggle on a non-active mapping updates it in place. The
    /// old clone-then-`set_active` shape promoted it.
    #[tokio::test]
    async fn set_auto_accept_does_not_promote_a_non_active_mapping() {
        let _wd = test_work_dir();
        let dir = tempfile::tempdir().unwrap();
        let cfg = test_config(&dir.path().join("sessions.json"));
        let (app, _platform) = build_app(cfg, MockBackend::new(realistic_parts())).await;
        let key = ThreadKey::new("chat_1".into(), "chat_1".into());

        // Two mappings for one thread; the last activated is the active one.
        app.activate_session(SessionEntry::new(key.clone(), "ses_b", "/work/b"))
            .await
            .unwrap();
        app.activate_session(SessionEntry::new(key.clone(), "ses_a", "/work/a"))
            .await
            .unwrap();
        assert_eq!(app.get_session_id(&key).await.as_deref(), Some("ses_a"));

        app.set_auto_accept("ses_b", "/work/b", true).await;

        assert_eq!(
            app.get_session_id(&key).await.as_deref(),
            Some("ses_a"),
            "toggling auto-accept on a non-active mapping keeps the active session"
        );
        let sessions = app.sessions.lock().await;
        assert!(sessions.entry_for_session("ses_b").unwrap().auto_accept);
    }
}
