use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use tokio::sync::Mutex;

use crate::bridge::session::SessionStore;
use crate::bridge::streaming::StreamAccumulator;
use crate::feishu;
use crate::opencode;

/// A cached `GET /session` snapshot with its fetch time. The 30 s TTL keeps
/// `/list`/`/switch`/`/attach` off the wire for rapid reuse; cola invalidates
/// it immediately on create/adopt/rename.
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

/// State shared across every flow: the session map, the per-session streaming
/// accumulators, the card-id map, the double-click guard, prompt serialization,
/// and the two adapters. Owned by the bridge coordinator ([`super::App`]) and
/// passed by handle to the flow modules that need it.
pub struct SharedCore {
    pub sessions: Arc<Mutex<SessionStore>>,
    pub accumulators: Arc<Mutex<HashMap<String, StreamAccumulator>>>,
    pub card_message_ids: Arc<Mutex<HashMap<String, String>>>,
    /// request ids already answered on the permission/question cards. Guards
    /// against double-click races (two card callbacks before the result card
    /// replaces the buttons): a second click on the same request is ignored
    /// server-side instead of re-replying.
    pub answered_requests: Arc<Mutex<HashSet<String>>>,
    /// Session ids with a prompt currently in flight (serializes prompts per
    /// session so concurrent messages don't clobber each other's accumulators).
    pub inflight: Arc<Mutex<HashSet<String>>>,
    /// Default directory for new sessions (from `[bridge] work_dir`).
    pub work_dir: Option<String>,
    /// Whether to send the group completion notice (from `[bridge] group_completion_notice`).
    pub group_completion_notice: bool,
    /// Cached `GET /session` snapshot for `/list`, `/switch`, `/attach`
    /// (30 s TTL; invalidated on create/adopt/rename).
    pub session_list_cache: Arc<Mutex<Option<SessionListCache>>>,
    pub opencode: Arc<dyn opencode::Backend>,
    pub feishu: Arc<dyn feishu::Platform>,
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
            accumulators: Arc::new(Mutex::new(HashMap::new())),
            card_message_ids: Arc::new(Mutex::new(HashMap::new())),
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
}
