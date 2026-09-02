use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use tokio::sync::Mutex;

use crate::bridge::session::SessionStore;
use crate::config::ThreadKey;
use crate::feishu;
use crate::opencode;

/// A cached session-list snapshot (cross-store, most recently active first)
/// with its fetch time. The 30 s TTL keeps `/list`/`/switch`/`/attach` off the
/// wire for rapid reuse; cola invalidates it immediately on create/adopt/rename.
#[derive(Clone)]
pub struct SessionListCache {
    pub fetched_at: std::time::Instant,
    pub sessions: Vec<opencode::client::SessionListInfo>,
}

impl SessionListCache {
    /// Whether the snapshot is still fresh (fetched within the TTL).
    pub fn fresh(&self) -> bool {
        self.fetched_at.elapsed() < std::time::Duration::from_secs(30)
    }
}

/// State shared across every flow: the session map, the per-session live cards
/// ([`CardSession`] — accumulator + card identity in one place), the
/// double-click guard, prompt serialization, and the two adapters. Owned by the
/// bridge coordinator ([`super::App`]) and passed by handle to the flow modules
/// that need it.
pub struct SharedCore {
    pub sessions: Arc<Mutex<SessionStore>>,
    /// session_id → the session's one live card (accumulator + card id chain).
    pub cards: Arc<Mutex<HashMap<String, crate::bridge::streaming::CardSession>>>,
    /// request ids already answered on the permission/question cards. Guards
    /// against double-click races (two card callbacks before the result card
    /// replaces the buttons): a second click on the same request is ignored
    /// server-side instead of re-replying.
    pub answered_requests: Arc<Mutex<HashSet<String>>>,
    /// Session ids with a prompt currently in flight (serializes prompts per
    /// session so concurrent messages don't clobber each other's cards).
    pub inflight: Arc<Mutex<HashSet<String>>>,
    /// Default directory for new sessions (from `[bridge] work_dir`).
    pub work_dir: Option<String>,
    /// Whether to send the group completion notice (from `[bridge] group_completion_notice`).
    pub group_completion_notice: bool,
    /// Cached session-list snapshot for `/list`, `/switch`, `/attach`
    /// (30 s TTL; invalidated on create/adopt/rename).
    pub session_list_cache: Arc<Mutex<Option<SessionListCache>>>,
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
            answered_requests: Arc::new(Mutex::new(HashSet::new())),
            inflight: Arc::new(Mutex::new(HashSet::new())),
            work_dir: cfg
                .bridge
                .work_dir
                .clone()
                .map(|p| p.to_string_lossy().to_string()),
            group_completion_notice: cfg.bridge.group_completion_notice,
            session_list_cache: Arc::new(Mutex::new(None)),
            opencode,
            feishu,
            server_start: cfg.opencode.start_server,
            preferred_port: cfg.opencode.preferred_port(),
            server_lock: Arc::new(tokio::sync::Mutex::new(())),
        })
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

    /// The conversation's current project (ADR-0012): the active session's
    /// directory, falling back to the default directory only when the
    /// conversation has no session. Single definition of "current project",
    /// shared by `/new`, the bare `/topic` form, and the `/switch` card's
    /// "new session" action.
    pub async fn current_project_directory(&self, thread_key: &ThreadKey) -> String {
        self.sessions
            .lock()
            .await
            .get_active(thread_key)
            .map(|e| e.directory.clone())
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
    pub async fn session_model_override(&self, session_id: &str) -> Option<opencode::client::ModelInfo> {
        self.sessions
            .lock()
            .await
            .entry_for_session(session_id)
            .and_then(|e| e.model.as_deref())
            .and_then(opencode::client::parse_model)
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
    pub(crate) async fn cached_session_list(&self) -> crate::error::Result<Vec<opencode::SessionListInfo>> {
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
        if let Some(mut entry) = owner {
            entry.auto_accept = on;
            let mut store = self.sessions.lock().await;
            store.set_active(entry);
            if let Err(e) = store.persist() {
                tracing::warn!("set_auto_accept: persist failed: {}", e);
            }
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
            let Some(sid) = p.session_id.clone() else { continue };
            if sid != session_id && !self.session_descends_from(&sid, session_id, directory).await {
                continue;
            }
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
                Err(e) => tracing::warn!("auto-accept pending {} on session {}: {}", p.request_id, sid, e),
            }
        }
        approved
    }

    /// Whether `candidate` is `root` or a sub-task child reachable by walking
    /// up its parent chain (sub-task child sessions carry their own sessionID).
    async fn session_descends_from(&self, candidate: &str, root: &str, directory: &str) -> bool {
        crate::bridge::pollers::walk_parent_chain(self, candidate, Some(directory), |current| {
            let current = current.to_string();
            async move { (current == root).then_some(true) }
        })
        .await
        .unwrap_or(false)
    }
}
