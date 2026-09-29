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
use crate::bridge::turn::{CardSession, CollectReason, Turn};
use crate::config::{ServerStartPolicy, SessionEntry, ThreadKey};
use crate::{feishu, opencode};

/// The session map, the session-list cache its write paths invalidate, and the
/// settings/model resolution the conversation reads.
///
/// The store and cache travel together because a create/remove/activate changes
/// what `/switch` must show; a caller that only reads sessions still
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
    /// The 30 s session-list cache (`/switch`/`/attach`), invalidated by
    /// the write paths below. Private: [`Self::cached_session_list`] and
    /// [`Self::invalidate_cache`] are the accessors.
    cache: Arc<Mutex<Option<SessionListCache>>>,
}

/// The result of applying a settings pick (`/model`, `/think`, `/agent`): one
/// code path mirrors it locally and — on a generation whose selection is
/// durable (V2) — switches the session server-side, so the two can never
/// half-land differently.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum PickOutcome {
    /// The pick landed.
    Applied {
        /// The `/think` variant a model pick cleared (ADR-0020), if any.
        cleared_variant: Option<String>,
    },
    /// The conversation has no target session (no mapping, no Pending Session).
    NoTarget,
    /// A durable session has no resolvable model, so a variant pick cannot be
    /// attached; nothing was written (the pick could never apply).
    NoModel,
    /// A durable session's agent reset found no default agent to switch to;
    /// nothing was written.
    NoDefaultAgent,
}

/// A pick result turned into what a surface renders: the applied payload, or
/// the one failure phrasing every surface shares (text reply and card toast).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum PickReply {
    Applied { cleared_variant: Option<String> },
    Failed(String),
}

/// Classify a pick result for its surface. `noun` names the switched thing in
/// the failure line ("模型" / "思考等级" / "Agent"), `thread_label` decorates
/// the no-session case. All six pick sites (three text commands, three card
/// actions) go through here, so their phrasing cannot drift.
pub(crate) fn classify_pick(
    result: crate::error::Result<PickOutcome>,
    noun: &str,
    thread_label: &str,
) -> PickReply {
    match result {
        Ok(PickOutcome::Applied { cleared_variant }) => PickReply::Applied { cleared_variant },
        Ok(PickOutcome::NoTarget) => PickReply::Failed(format!(
            "⚠️ {thread_label}还没有会话，先用 `/new` 或 `/dir` 创建。"
        )),
        Ok(PickOutcome::NoModel) => {
            PickReply::Failed("⚠️ 无法确定当前模型，请先用 `/model` 选择模型。".to_string())
        }
        Ok(PickOutcome::NoDefaultAgent) => {
            PickReply::Failed("⚠️ 无法确定服务器默认 Agent；请用 `/agent <名字>` 从列表中选择。".to_string())
        }
        Err(error) => PickReply::Failed(format!("⚠️ 切换{noun}失败：{error}")),
    }
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
    /// server session, so `/switch` has nothing new to show.
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
    /// cache: creating or adopting a session changes what `/switch`
    /// should offer. The cache is dropped even when the save fails, because the
    /// in-memory mapping already changed.
    ///
    /// **Raw mutator — module-private on purpose.** Every activation a caller
    /// outside this module performs goes through [`FlowHandles::activate`],
    /// which collects the displaced Session's waiting card first (ADR-0059,
    /// spec #405); keeping this private is what makes that collect an
    /// invariant instead of a rule each call site must remember.
    pub(in crate::bridge::handles) async fn activate(&self, entry: SessionEntry) -> crate::error::Result<()> {
        let result = self.store.lock().await.activate(entry);
        *self.cache.lock().await = None;
        result
    }

    /// Remove a mapping and persist, dropping the session-list cache (the
    /// `/switch` view may no longer mention it).
    ///
    /// **Raw mutator — module-private on purpose.** Callers outside this module
    /// use [`FlowHandles::unmap`] / [`FlowHandles::unmap_thread`], which collect
    /// the removed Session's waiting card (ADR-0059, spec #405).
    pub(in crate::bridge::handles) async fn remove_session(
        &self,
        session_id: &str,
    ) -> crate::error::Result<Option<SessionEntry>> {
        let result = self.store.lock().await.remove_persist(session_id);
        *self.cache.lock().await = None;
        result
    }

    /// Remove every mapping of a thread and persist, dropping the session-list
    /// cache (`/switch forget`).
    ///
    /// **Raw mutator — module-private on purpose.** See [`Self::remove_session`].
    pub(in crate::bridge::handles) async fn remove_thread_sessions(
        &self,
        key: &ThreadKey,
    ) -> crate::error::Result<Vec<SessionEntry>> {
        let result = self.store.lock().await.remove_thread_persist(key);
        *self.cache.lock().await = None;
        result
    }

    /// The session ids mapped to `thread_key`, in the store's order (the active
    /// entry first) — the Turn's supersede collect reads every mapped session,
    /// so no waiting card a new Turn displaces is left behind (ADR-0059, spec
    /// #405).
    pub(crate) async fn session_ids_for_thread(&self, thread_key: &ThreadKey) -> Vec<String> {
        self.store
            .lock()
            .await
            .list_thread(thread_key)
            .into_iter()
            .map(|entry| entry.session_id.clone())
            .collect()
    }

    /// Drop the session-list cache. Called whenever cola creates, adopts, forgets or
    /// renames a session, so the next `/switch`/`/attach` is fresh.
    pub(crate) async fn invalidate_cache(&self) {
        *self.cache.lock().await = None;
    }

    /// The current `GET /session` snapshot, fetching (and caching for 30 s) when
    /// missing or stale. Used by `/switch` and `/attach` so rapid
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

    /// The model the NEXT turn will actually run, resolved generation-natively.
    ///
    /// On a generation that keeps the selection server-side (V2), the session's
    /// durable selection IS the answer — what the next prompt will use, shared
    /// state another client may have changed. A failed read says "unknown", so
    /// it yields `None`: cola's local mirror is never presented as if the
    /// server had it (one rule for one fact, ADR-0055). When the server
    /// recorded no model, the configured default (`[opencode] model`) is what
    /// the server's own resolver would fall back to — and when even that is
    /// unset, the model the session last ran with (the newest assistant
    /// message) names its effective model.
    ///
    /// On V1 (and for a Pending Session, which has no server identity yet,
    /// ADR-0041) the `/model` mirror is the selection cola sends per prompt,
    /// so the ladder is mirror → configured default → server-recorded model.
    /// Every remote rung is bounded: a hung server degrades the ladder (no
    /// current-model line / a `/think` "pick a model" prompt), never the card
    /// send.
    pub(crate) async fn effective_selection(
        &self,
        backend: &Arc<dyn crate::backend::Backend>,
        settings: &SessionSettings,
    ) -> Option<opencode::types::ModelInfo> {
        if let Some(session_id) = settings.session_id.as_deref()
            && backend.keeps_session_selection()
        {
            return match self
                .durable_selection(backend, session_id, Some(&settings.directory))
                .await
            {
                Ok(Some(selection)) => match selection.model.or_else(|| backend.configured_default_model()) {
                    Some(model) => Some(model),
                    // A durable session records a model only after an explicit
                    // switch, so a session started from the server default has
                    // no selection to show — but it names the model it ran in
                    // the message history. Fall back to that (what the
                    // server's own next-turn resolution keeps using) instead
                    // of asking for `/model` on a working session; V1 parity:
                    // its ladder's last rung is the server-recorded model.
                    None => {
                        self.last_run_model(backend, session_id, &settings.directory)
                            .await
                    }
                },
                // V2 always reports a selection; a failed read is unknown.
                Ok(None) | Err(_) => None,
            };
        }
        // The `/model` mirror. Its variant is the separate `/think` field on
        // the settings, so the model ref is assembled here.
        if let Some(mut model) = settings.model.as_deref().and_then(opencode::parsing::parse_model) {
            model.variant = settings.variant.clone();
            return Some(model);
        }
        // The configured default (`[opencode] model`).
        if let Some(mut model) = backend.configured_default_model() {
            model.variant = settings.variant.clone();
            return Some(model);
        }
        // What the server actually recorded for the session. Bounded: a
        // hung server degrades the ladder, never the card send.
        let session_id = settings.session_id.as_deref()?;
        if !settings.directory.is_empty()
            && let Ok(Ok(info)) = tokio::time::timeout(
                SESSION_INFO_TIMEOUT,
                backend.session_info(session_id, Some(&settings.directory)),
            )
            .await
            && let Some(model) = info.model
        {
            return Some(opencode::types::ModelInfo {
                id: model.id,
                provider_id: model.provider_id,
                variant: settings.variant.clone(),
            });
        }
        None
    }

    /// The model the NEXT turn will actually run, as `(provider, model)`.
    /// [`Self::effective_selection`]'s variant-blind view for the callers that
    /// only need the pair.
    pub(crate) async fn effective_model(
        &self,
        backend: &Arc<dyn crate::backend::Backend>,
        settings: &SessionSettings,
    ) -> Option<(String, String)> {
        self.effective_selection(backend, settings)
            .await
            .map(|model| (model.provider_id, model.id))
    }

    /// The variant the next prompt will actually use. On a generation with a
    /// durable selection (V2) it is the session's own — possibly set by another
    /// client — and a failed read says unknown (`None`), never the mirror: the
    /// footer then falls back to the transcript, which carries what ran. On V1
    /// it is the `/think` mirror, which is what the next prompt sends.
    pub(crate) async fn effective_variant(
        &self,
        backend: &Arc<dyn crate::backend::Backend>,
        session_id: &str,
        directory: Option<&str>,
    ) -> Option<String> {
        if !backend.keeps_session_selection() {
            return self.variant_override(session_id).await;
        }
        match self.durable_selection(backend, session_id, directory).await {
            Ok(Some(selection)) => selection.model.and_then(|model| model.variant),
            Ok(None) | Err(_) => None,
        }
    }

    /// Read the session's durable selection (V2). A failed or timed-out read is
    /// an error the caller must decide about — never a silent `None` that reads
    /// like "this generation has no selection". Every failure is logged at WARN
    /// (the adapter's "errors are visible, never silent" rule).
    async fn durable_selection(
        &self,
        backend: &Arc<dyn crate::backend::Backend>,
        session_id: &str,
        directory: Option<&str>,
    ) -> crate::error::Result<Option<opencode::types::SessionSelection>> {
        match tokio::time::timeout(
            SESSION_INFO_TIMEOUT,
            backend.session_selection(session_id, directory),
        )
        .await
        {
            Ok(result) => {
                if let Err(error) = &result {
                    tracing::warn!("session {session_id}: durable selection read failed: {error}");
                }
                result
            }
            Err(_) => {
                let error = crate::error::BridgeError::OpenCode(format!(
                    "session {session_id} selection read timed out"
                ));
                tracing::warn!("{error}");
                Err(error)
            }
        }
    }

    /// The durable-generation ladder's last rung: the model the session last
    /// actually ran with (its newest assistant message). Bounded like every
    /// other remote rung — a hung or failing server degrades the ladder to "no
    /// current model", never the card send — with every failure logged at WARN
    /// (the adapter's "errors are visible, never silent" rule).
    async fn last_run_model(
        &self,
        backend: &Arc<dyn crate::backend::Backend>,
        session_id: &str,
        directory: &str,
    ) -> Option<opencode::types::ModelInfo> {
        match tokio::time::timeout(
            SESSION_INFO_TIMEOUT,
            backend.session_last_run_model(session_id, Some(directory)),
        )
        .await
        {
            Ok(Ok(model)) => model,
            Ok(Err(error)) => {
                tracing::warn!("session {session_id}: last-run model read failed: {error}");
                None
            }
            Err(_) => {
                tracing::warn!("session {session_id}: last-run model read timed out");
                None
            }
        }
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

    /// Put a settled model selection onto a session: clear a variant the model
    /// does not declare (ADR-0020), then switch the model ref (the variant
    /// inside it). One implementation for the live picks and for a Pending
    /// Session's materialisation, so the two cannot drift. Returns the cleared
    /// variant. The caller owns the local write; V1's switch is a no-op and
    /// only its clear applies.
    pub(crate) async fn apply_model_selection(
        &self,
        backend: &Arc<dyn crate::backend::Backend>,
        session_id: &str,
        model_spec: &str,
        variant: &mut Option<String>,
    ) -> crate::error::Result<Option<String>> {
        let cleared = self.clear_variant_for_model(backend, variant, model_spec).await;
        let model = opencode::parsing::parse_model(model_spec).ok_or_else(|| {
            crate::error::BridgeError::OpenCode(format!("invalid model reference `{model_spec}`"))
        })?;
        backend
            .switch_session_model(
                session_id,
                &opencode::types::ModelInfo {
                    id: model.id,
                    provider_id: model.provider_id,
                    variant: variant.clone(),
                },
            )
            .await?;
        Ok(cleared)
    }

    /// Apply a `/model` pick (ADR-0020 and V2's durable switches): mirror the
    /// pick, carry or clear the variant, and switch the session's model where
    /// the generation keeps a durable selection. The switch happens BEFORE the
    /// local write, so a failed switch leaves the mirror untouched rather than
    /// claiming a pick that never landed.
    ///
    /// On a durable generation (V2) the surviving variant is the SESSION's own
    /// (another client may have changed it); a failed selection read means
    /// unknown, and since the switch replaces the whole ref, the variant is
    /// dropped rather than revived from the mirror.
    pub(crate) async fn pick_model(
        &self,
        backend: &Arc<dyn crate::backend::Backend>,
        thread_key: &ThreadKey,
        model_spec: &str,
    ) -> crate::error::Result<PickOutcome> {
        let Some(mut settings) = self.session_settings(thread_key).await else {
            return Ok(PickOutcome::NoTarget);
        };
        settings.model = Some(model_spec.to_string());
        let cleared_variant = match settings.session_id.clone() {
            Some(session_id) => {
                if backend.keeps_session_selection() {
                    settings.variant = match self
                        .durable_selection(backend, &session_id, Some(&settings.directory))
                        .await
                    {
                        Ok(Some(selection)) => selection.model.and_then(|model| model.variant),
                        // Unknown: the switch rewrites the ref wholesale, so
                        // dropping it is the one honest outcome.
                        Ok(None) | Err(_) => None,
                    };
                }
                self.apply_model_selection(backend, &session_id, model_spec, &mut settings.variant)
                    .await?
            }
            None => {
                // A Pending Session has no session to switch yet; the clear
                // rule is recorded and applied when the session materialises.
                self.clear_variant_for_model(backend, &mut settings.variant, model_spec)
                    .await
            }
        };
        if !self.set_session_settings(thread_key, settings).await? {
            return Ok(PickOutcome::NoTarget);
        }
        Ok(PickOutcome::Applied { cleared_variant })
    }

    /// Apply a `/think` pick: `variant: None` clears it (the model's own
    /// default). On a durable generation the variant is written into the
    /// session's model ref — the effective model is resolved first, so
    /// [`PickOutcome::NoModel`] reports a session that cannot carry one (never
    /// a mirror-only write the server would ignore) — and on V1 it stays the
    /// per-prompt mirror.
    pub(crate) async fn pick_think(
        &self,
        backend: &Arc<dyn crate::backend::Backend>,
        thread_key: &ThreadKey,
        variant: Option<String>,
    ) -> crate::error::Result<PickOutcome> {
        let Some(mut settings) = self.session_settings(thread_key).await else {
            return Ok(PickOutcome::NoTarget);
        };
        settings.variant = variant;
        if let Some(session_id) = settings.session_id.clone()
            && backend.keeps_session_selection()
        {
            let Some(model) = self.effective_selection(backend, &settings).await else {
                return Ok(PickOutcome::NoModel);
            };
            backend
                .switch_session_model(
                    &session_id,
                    &opencode::types::ModelInfo {
                        id: model.id,
                        provider_id: model.provider_id,
                        variant: settings.variant.clone(),
                    },
                )
                .await?;
        }
        if !self.set_session_settings(thread_key, settings).await? {
            return Ok(PickOutcome::NoTarget);
        }
        Ok(PickOutcome::Applied {
            cleared_variant: None,
        })
    }

    /// Apply an `/agent` pick: `agent: None` clears it (back to the server's
    /// default). On a durable generation (V2) the selection is switched on the
    /// session — a reset resolves the default from the agent catalog and
    /// switches to that id explicitly, because V2 has no "unset" arm. On V1
    /// clearing is purely local: absence of the per-prompt agent is the clear.
    /// The capability, never a selection read, decides which path applies.
    pub(crate) async fn pick_agent(
        &self,
        backend: &Arc<dyn crate::backend::Backend>,
        thread_key: &ThreadKey,
        agent: Option<String>,
    ) -> crate::error::Result<PickOutcome> {
        let Some(mut settings) = self.session_settings(thread_key).await else {
            return Ok(PickOutcome::NoTarget);
        };
        if let Some(session_id) = settings.session_id.clone()
            && backend.keeps_session_selection()
        {
            match &agent {
                Some(picked) => backend.switch_session_agent(&session_id, picked).await?,
                None => {
                    let Some(default) =
                        opencode::types::AgentInfo::default_agent(&backend.list_agents().await)
                    else {
                        return Ok(PickOutcome::NoDefaultAgent);
                    };
                    backend.switch_session_agent(&session_id, &default).await?;
                }
            }
        }
        settings.agent = agent;
        if !self.set_session_settings(thread_key, settings).await? {
            return Ok(PickOutcome::NoTarget);
        }
        Ok(PickOutcome::Applied {
            cleared_variant: None,
        })
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
    /// and `split_chain_for_wake` (which enqueue their split and flush it under
    /// the same lock; `split_chain_for_wake` also writes the outgoing card's
    /// ledger handover), `refresh_yielded_ledger` (a yielded card's in-place
    /// ledger update and quiet true-end settle) and `resolve_blocks` are the
    /// holders.
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

impl WaitsHandle {
    /// Whether this session's run was stopped with `/stop` — the sticky marker
    /// the drain rule (ADR-0043) and the stop terminal (#394) read. One
    /// accessor so the lock-and-check exists in one place, and every reader
    /// sees the same fact.
    pub(crate) async fn is_stopped(&self, session_id: &str) -> bool {
        self.stopped_sessions.lock().await.contains(session_id)
    }
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
    /// The lost-contact / stuck-panel grace on the out-of-turn drain follow
    /// (ms, #284/#386); injectable for tests.
    pub(crate) turn_follow_grace_ms: Arc<AtomicU64>,
    /// Per-read bound on the out-of-turn drain follow (ms, #386); injectable
    /// for tests.
    pub(crate) turn_follow_read_timeout_ms: Arc<AtomicU64>,
    /// Default directory for new sessions (from `[bridge] work_dir`).
    work_dir: Option<String>,
}

impl TurnConfig {
    #[allow(clippy::too_many_arguments)] // the turn knobs are a flat wiring list
    pub(crate) fn new(
        group_completion_notice: bool,
        long_task_notice: bool,
        long_task_notice_ms: Arc<AtomicU64>,
        turn_render_poll_ms: Arc<AtomicU64>,
        turn_drain_timeout_ms: Arc<AtomicU64>,
        turn_follow_grace_ms: Arc<AtomicU64>,
        turn_follow_read_timeout_ms: Arc<AtomicU64>,
        work_dir: Option<String>,
    ) -> Self {
        Self {
            group_completion_notice,
            long_task_notice,
            long_task_notice_ms,
            turn_render_poll_ms,
            turn_drain_timeout_ms,
            turn_follow_grace_ms,
            turn_follow_read_timeout_ms,
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

    /// The out-of-turn drain follow's lost-contact / stuck-panel grace (ms,
    /// #284/#386).
    pub(crate) fn follow_grace_ms(&self) -> u64 {
        self.turn_follow_grace_ms.load(Ordering::Relaxed)
    }

    /// The per-read bound on the out-of-turn drain follow (ms, #386).
    pub(crate) fn follow_read_timeout_ms(&self) -> u64 {
        self.turn_follow_read_timeout_ms.load(Ordering::Relaxed)
    }

    /// The Completion Notice's opt-in rules as a bundle of their own, so
    /// Session Sync — which settles a yielded card's quiet true end and sends
    /// the same notice, without a Turn's config or its cadence knobs — can
    /// carry exactly what the notice decides on (ADR-0060).
    pub(crate) fn notice_rules(&self) -> NoticeRules {
        NoticeRules::new(
            self.group_completion_notice,
            self.long_task_notice,
            Arc::clone(&self.long_task_notice_ms),
        )
    }
}

/// The Completion Notice's opt-in rules (ADR-0043 amendment 2026-09-21), split
/// out of [`TurnConfig`] for callers that are not a Turn: whether groups notify,
/// whether a long p2p run notifies, and the injectable threshold. The threshold
/// stays the SAME atomic as the Turn config's, so a test that stores a tiny
/// value moves both clock readers at once.
#[derive(Clone)]
pub(crate) struct NoticeRules {
    pub(crate) group_completion_notice: bool,
    pub(crate) long_task_notice: bool,
    long_task_notice_ms: Arc<AtomicU64>,
}

impl NoticeRules {
    pub(crate) fn new(
        group_completion_notice: bool,
        long_task_notice: bool,
        long_task_notice_ms: Arc<AtomicU64>,
    ) -> Self {
        Self {
            group_completion_notice,
            long_task_notice,
            long_task_notice_ms,
        }
    }

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

impl TurnHandles {
    /// The same handles as the flow bundle — every concern but the turn
    /// config — for the flows that run under a Turn (the shared out-of-turn
    /// settle loop the follow and the Wake continuation both run takes this).
    pub(crate) fn flow(&self) -> FlowHandles {
        FlowHandles {
            sessions: self.sessions.clone(),
            cards: self.cards.clone(),
            requests: self.requests.clone(),
            waits: self.waits.clone(),
            backend: Arc::clone(&self.backend),
            platform: Arc::clone(&self.platform),
        }
    }
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

impl FlowHandles {
    /// Promote `entry` as its thread's Active Session, collecting the displaced
    /// Session's waiting card first (ADR-0059, spec #405): the displaced
    /// Session can no longer be continued by a message in this thread, so a
    /// card it left on 「⏳ 等待后台任务」 is collected as
    /// 「⏳ 已切换会话 · 后台任务仍在运行」 and stops updating, while its
    /// background work runs on. The collect runs BEFORE the activation, so a
    /// polling Wake cannot resume a chain the switch is leaving behind.
    /// Re-activating the already-active session collects nothing.
    ///
    /// This and the other operations below are the ONLY way to change a
    /// thread's Active Session mapping outside [`SessionsHandle`]: the raw
    /// mutators are module-private, so no switch/adopt/steal path can forget
    /// the collect. A **Pending Session** declaration deliberately does not
    /// collect: the superseded session stays mapped and switchable, and the
    /// next Turn in the thread collects its card — the Turn's supersede collect
    /// reads every mapped session.
    pub(crate) async fn activate(&self, entry: SessionEntry) -> crate::error::Result<()> {
        if let Some(previous) = self.sessions.active_entry(&entry.thread_key).await
            && previous.session_id != entry.session_id
        {
            Turn::collect_waiting(&self.cards, &previous.session_id, CollectReason::SwitchedAway).await;
        }
        self.sessions.activate(entry).await
    }

    /// Unmap `session_id` — a `--force` steal by another thread, or a dead
    /// mapping being replaced — collecting the waiting card it leaves behind
    /// first (ADR-0059, spec #405): the Session stops being any thread's Active
    /// Session, so its card must not keep sitting on 「⏳ 等待后台任务」. The
    /// returned entry is the removed mapping, `None` when nothing was mapped
    /// (and then nothing is collected).
    pub(crate) async fn unmap(&self, session_id: &str) -> crate::error::Result<Option<SessionEntry>> {
        let removed = self.sessions.remove_session(session_id).await?;
        if removed.is_some() {
            Turn::collect_waiting(&self.cards, session_id, CollectReason::SwitchedAway).await;
        }
        Ok(removed)
    }

    /// Unmap every mapping of `thread_key` (`/switch forget`), collecting each
    /// removed Session's waiting card (ADR-0059, spec #405).
    pub(crate) async fn unmap_thread(
        &self,
        thread_key: &ThreadKey,
    ) -> crate::error::Result<Vec<SessionEntry>> {
        let removed = self.sessions.remove_thread_sessions(thread_key).await?;
        for entry in &removed {
            Turn::collect_waiting(&self.cards, &entry.session_id, CollectReason::SwitchedAway).await;
        }
        Ok(removed)
    }

    /// Create a brand-new session on the current server and make it the
    /// thread's Active Session through [`Self::activate`] (so a session it
    /// displaces is collected: a 404 recreate can leave another mapped session
    /// active until this activation takes over). The per-session overrides
    /// reset to defaults, but the topic's creation messages
    /// (`topic_anchor`/`topic_root`, ADR-0023) are Feishu message ids — not
    /// session state — and survive so the quote-injection guard keeps working
    /// after the recreate.
    pub(crate) async fn create_fresh_session(
        &self,
        thread_key: &ThreadKey,
        directory: String,
        topic_anchor: Option<String>,
        topic_root: Option<String>,
    ) -> crate::error::Result<String> {
        let session = self
            .backend
            .create_session(&self.backend.new_session_input(Some(&directory)))
            .await?;
        let mut entry = SessionEntry::new(thread_key.clone(), session.id.clone(), directory);
        entry.topic_anchor = topic_anchor;
        entry.topic_root = topic_root;
        self.activate(entry).await?;
        Ok(session.id)
    }
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
    /// `[opencode] generation`: the override the reconnect loop applies when
    /// it re-probes a changed server (spec #364 §2).
    pub(crate) generation: crate::config::GenerationOverride,
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
