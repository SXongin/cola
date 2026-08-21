use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use tokio::sync::Mutex;

use crate::bridge::session::SessionStore;
use crate::bridge::streaming::StreamAccumulator;
use crate::feishu;
use crate::opencode;

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
