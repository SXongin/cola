//! The generation-blind OpenCode adapter (ADR-0055).
//!
//! [`OpenCodeBackend`] owns the HTTP transport and the generation strategy
//! selected for the attached server. Every protocol call is forwarded to that
//! strategy (the boundary is [`crate::opencode::strategy`]), so this adapter —
//! and everything above it — never branches on a protocol generation. The
//! strategy for a generation owns its endpoint paths, request bodies and
//! response decoders; V1's lives under [`crate::opencode::v1`], and deleting
//! that module is V1 retirement.

use std::sync::{Arc, RwLock};

use crate::backend::{SessionTranscript, TaskRetirements, TaskRuntime};

use super::parsing::parse_model;
use super::strategy::{Generation, GenerationStrategy};
use super::transport::Transport;
use super::types::{
    AgentInfo, CreateSessionInput, CreateSessionResponse, FormAnswer, ImageInput, Location, ModelInfo,
    PermissionRequest, ProviderModels, QuestionRequest, Session, SessionInfo, SessionListInfo,
    SessionSelection, SessionStatus,
};

/// Session creation, on the `/api` surface both generations serve. It is a
/// current-generation call, not V1 coupling, so it lives on the
/// generation-blind adapter rather than in a strategy.
const API_SESSION: &str = "/api/session";

/// One OpenCode server attachment, generation-blind.
///
/// The server cola talks to can be restarted/replaced at runtime (another tool
/// like OpenChamber manages it), which may change its port and password. So the
/// endpoint — base URL + auth — is held by the shared [`Transport`] and can be
/// swapped via [`OpenCodeBackend::reconnect`] without dropping the shared
/// `Arc<dyn Backend>`.
pub struct OpenCodeBackend {
    transport: Transport,
    /// The username used for Basic auth (reused when going serverless).
    username: Option<String>,
    /// Default model for new sessions, e.g. "opencode/deepseek-v4-flash-free"
    pub model: Option<ModelInfo>,
    /// The strategy that speaks the attached server's generation. Attach
    /// detection resolves the generation (spec #364 §2); [`Self::reconnect`]
    /// swaps this arm without dropping a backend handle, so it lives behind a
    /// lock shared across clones.
    strategy: Arc<RwLock<Arc<dyn GenerationStrategy>>>,
    /// The pid of the attached server, when the attach path knew it — the
    /// neutral identity token the reconnect loop compares to notice a
    /// replacement on the same port (a new generation or password). Shared
    /// across clones like the strategy.
    attached_pid: Arc<RwLock<Option<i32>>>,
    /// The Background Tasks a runtime reconciliation retired this cola life
    /// (issue #454), applied to every transcript read so the durable launch
    /// record cannot resurrect them. Shared across clones like the strategy.
    retirements: Arc<TaskRetirements>,
}

impl Clone for OpenCodeBackend {
    fn clone(&self) -> Self {
        Self {
            transport: self.transport.clone(),
            username: self.username.clone(),
            model: self.model.clone(),
            strategy: Arc::clone(&self.strategy),
            attached_pid: Arc::clone(&self.attached_pid),
            retirements: Arc::clone(&self.retirements),
        }
    }
}

impl OpenCodeBackend {
    /// Build a backend bound to a server, or serverless (None) when Lazy Start
    /// hasn't spawned one yet (ADR-0013). A serverless backend has an empty
    /// base URL and does no requests until `reconnect` points it at a real
    /// server; the username is still pinned so a later reconnect carries Basic
    /// auth with both parts.
    ///
    /// The attachment's generation was resolved by attach detection (spec #364
    /// §2), which selects the generation strategy here.
    pub fn new(model: Option<&str>, server: Option<crate::bridge::discovery::ResolvedServer>) -> Self {
        match server {
            Some(crate::bridge::discovery::ResolvedServer {
                url,
                username,
                password,
                pid,
                generation,
            }) => Self::with_generation(model, url, Some(&username), Some(&password), generation, pid),
            None => Self::with_generation(
                model,
                String::new(),
                Some(crate::bridge::discovery::DEFAULT_SERVER_USERNAME),
                None,
                Generation::V1,
                None,
            ),
        }
    }

    /// Build a backend against an explicit base URL and credentials, speaking
    /// V1 — the constructor tests use to point the real backend at a local fake
    /// server so the HTTP layer is exercised end to end (ADR-0031). A trailing
    /// slash is tolerated.
    ///
    /// Basic auth is attached only when BOTH username and password are present
    /// — a half-credential must never reach the wire (the server checks the
    /// username too, so a password-only request 401s).
    #[cfg(test)]
    pub fn with_base_url(
        model: Option<&str>,
        base_url: impl Into<String>,
        username: Option<&str>,
        password: Option<&str>,
    ) -> Self {
        Self::with_generation(model, base_url, username, password, Generation::V1, None)
    }

    /// [`Self::with_base_url`] with the generation strategy and the attached
    /// server's identity made explicit.
    pub(crate) fn with_generation(
        model: Option<&str>,
        base_url: impl Into<String>,
        username: Option<&str>,
        password: Option<&str>,
        generation: Generation,
        attached_pid: Option<i32>,
    ) -> Self {
        Self {
            transport: Transport::new(username, password, base_url),
            username: username.map(str::to_string),
            model: model.and_then(parse_model),
            strategy: Arc::new(RwLock::new(generation.strategy())),
            attached_pid: Arc::new(RwLock::new(attached_pid)),
            retirements: Arc::new(TaskRetirements::default()),
        }
    }

    /// The strategy that speaks the currently attached generation. The clone
    /// completes before the guard's temporary is dropped at the end of the
    /// statement, and nothing is awaited while it is held.
    fn strategy(&self) -> Arc<dyn GenerationStrategy> {
        self.strategy
            .read()
            .expect("the strategy lock is never poisoned")
            .clone()
    }

    /// The pid of the attached server, when the attach path knew it: the
    /// reconnect loop's identity check for a same-URL replacement.
    pub fn attached_server_pid(&self) -> Option<i32> {
        *self
            .attached_pid
            .read()
            .expect("the attached-pid lock is never poisoned")
    }

    /// Point this backend at a (re)discovered attachment, selecting its
    /// generation strategy; `None` goes serverless. The generation was
    /// resolved by attach detection before this call (spec #364 §2) — the
    /// backend never probes and never guesses.
    ///
    /// Rare; only the reconnect loop (a server restarted/replaced) and Lazy
    /// Start call it.
    pub fn reconnect(&self, server: Option<&crate::bridge::discovery::ResolvedServer>) {
        match server {
            Some(server) => {
                self.transport
                    .repoint(&server.url, &server.password, Some(&server.username));
                *self
                    .strategy
                    .write()
                    .expect("the strategy lock is never poisoned") = server.generation.strategy();
                *self
                    .attached_pid
                    .write()
                    .expect("the attached-pid lock is never poisoned") = server.pid;
                tracing::info!(
                    "reconnected opencode backend to {} (generation={})",
                    server.url,
                    server.generation.as_str()
                );
            }
            None => {
                self.transport.repoint("", "", self.username.as_deref());
                *self
                    .attached_pid
                    .write()
                    .expect("the attached-pid lock is never poisoned") = None;
                tracing::info!("opencode backend is serverless (the attached server is gone)");
            }
        }
    }

    /// The current base URL.
    pub fn base_url(&self) -> String {
        self.transport.base_url()
    }

    /// The configured default model (`[opencode] model`), parsed at startup.
    /// The second rung of the `/think` effective-model resolution.
    pub fn configured_default_model(&self) -> Option<ModelInfo> {
        self.model.clone()
    }

    /// The request body for a new session, applying the configured default
    /// model and an optional directory.
    pub fn new_session_input(&self, directory: Option<&str>) -> CreateSessionInput {
        CreateSessionInput {
            id: None,
            agent: None,
            model: self.model.clone(),
            location: directory.map(|d| Location {
                directory: d.to_string(),
            }),
        }
    }

    /// Create a new session with an optional directory and agent, on the
    /// current-generation `/api/session` route (where `location.directory`
    /// lives; both generations serve it).
    pub async fn create_session(&self, input: &CreateSessionInput) -> crate::error::Result<Session> {
        let resp = self
            .transport
            .client()
            .post(self.transport.url(API_SESSION))
            .json(input)
            .send()
            .await?
            .error_for_status()?;
        let body: CreateSessionResponse = resp.json().await?;
        Ok(body.data)
    }

    pub async fn list_sessions(&self) -> crate::error::Result<Vec<SessionListInfo>> {
        self.strategy().list_sessions(&self.transport).await
    }

    pub async fn update_session_title(&self, session_id: &str, title: &str) -> crate::error::Result<()> {
        self.strategy()
            .update_session_title(&self.transport, session_id, title)
            .await
    }

    /// Delete a session server-side (V2 answers 204 and cascades to children;
    /// V1 answers a boolean body).
    pub async fn delete_session(&self, session_id: &str) -> crate::error::Result<()> {
        self.strategy().delete_session(&self.transport, session_id).await
    }

    /// Submit a prompt through the attached generation's strategy (ADR-0056's
    /// submit+observe contract: the call returns once the message is durable,
    /// and the Turn observes completion from the transcript + run state).
    #[allow(clippy::too_many_arguments)] // prompt axes: session/text/images + model/variant/agent/message-id
    pub async fn prompt(
        &self,
        session_id: &str,
        text: &str,
        images: &[ImageInput],
        model: Option<&ModelInfo>,
        variant: Option<&str>,
        agent: Option<&str>,
        message_id: Option<&str>,
    ) -> crate::error::Result<()> {
        // Resolve the effective model here (per-session override → configured
        // default): which model runs is cola policy, how it is spelled on the
        // wire is the strategy's business.
        let model = model.or(self.model.as_ref());
        self.strategy()
            .prompt(
                &self.transport,
                session_id,
                text,
                images,
                model,
                variant,
                agent,
                message_id,
            )
            .await
    }

    pub async fn reply_permission(
        &self,
        session_id: &str,
        request_id: &str,
        reply: &str,
        directory: Option<&str>,
    ) -> crate::error::Result<()> {
        self.strategy()
            .reply_permission(&self.transport, session_id, request_id, reply, directory)
            .await
    }

    pub async fn list_permissions(
        &self,
        directory: Option<&str>,
    ) -> crate::error::Result<Vec<PermissionRequest>> {
        self.strategy().list_permissions(&self.transport, directory).await
    }

    pub async fn list_questions(
        &self,
        directory: Option<&str>,
    ) -> crate::error::Result<Vec<QuestionRequest>> {
        self.strategy().list_questions(&self.transport, directory).await
    }

    pub async fn reply_question(
        &self,
        session_id: &str,
        request_id: &str,
        answers: &[FormAnswer],
        directory: Option<&str>,
    ) -> crate::error::Result<()> {
        self.strategy()
            .reply_question(&self.transport, session_id, request_id, answers, directory)
            .await
    }

    pub async fn reject_question(
        &self,
        session_id: &str,
        request_id: &str,
        directory: Option<&str>,
    ) -> crate::error::Result<()> {
        self.strategy()
            .reject_question(&self.transport, session_id, request_id, directory)
            .await
    }

    /// The session transcript, with this cola life's runtime retirements
    /// applied ([`TaskRetirements`], issue #454): a Background Task the runtime
    /// confirmed ended leaves the live list here, so no read path resurrects it.
    pub async fn transcript(&self, session_id: &str) -> crate::error::Result<SessionTranscript> {
        let mut transcript = self.strategy().transcript(&self.transport, session_id).await?;
        self.retirements.apply(session_id, &mut transcript);
        Ok(transcript)
    }

    /// Record the Background Tasks a runtime reconciliation retired (issue
    /// #454). See [`crate::backend::Backend::retire_background_tasks`].
    pub fn retire_background_tasks(&self, session_id: &str, call_ids: &[String]) {
        self.retirements.record(session_id, call_ids);
    }

    /// The session's durable model/agent selection where the generation keeps
    /// one server-side (V2); `None` on V1 (its picks ride each prompt).
    pub async fn session_selection(
        &self,
        session_id: &str,
        directory: Option<&str>,
    ) -> crate::error::Result<Option<SessionSelection>> {
        self.strategy()
            .session_selection(&self.transport, session_id, directory)
            .await
    }

    /// The model the session last actually ran with where the generation can
    /// name one (V2's newest assistant message); `None` on V1, whose ladder
    /// reads the server-recorded session model instead.
    pub async fn session_last_run_model(
        &self,
        session_id: &str,
        directory: Option<&str>,
    ) -> crate::error::Result<Option<ModelInfo>> {
        self.strategy()
            .session_last_run_model(&self.transport, session_id, directory)
            .await
    }

    /// Make the session's model selection durable (V2); a no-op on V1.
    pub async fn switch_session_model(
        &self,
        session_id: &str,
        model: &ModelInfo,
    ) -> crate::error::Result<()> {
        self.strategy()
            .switch_session_model(&self.transport, session_id, model)
            .await
    }

    /// Make the session's agent selection durable (V2); a no-op on V1.
    pub async fn switch_session_agent(&self, session_id: &str, agent: &str) -> crate::error::Result<()> {
        self.strategy()
            .switch_session_agent(&self.transport, session_id, agent)
            .await
    }

    /// Whether the attached generation keeps model/agent choices as durable
    /// session state (V2) rather than sending them per prompt (V1).
    pub fn keeps_session_selection(&self) -> bool {
        self.strategy().keeps_session_selection()
    }

    /// Whether a same-id re-post of an admitted user message can continue the
    /// turn on the attached generation (V1's upsert-continue) or is a no-op
    /// (V2's admission key). See
    /// [`crate::backend::Backend::reuse_continues_an_admitted_turn`].
    pub fn reuse_continues_an_admitted_turn(&self) -> bool {
        self.strategy().reuse_continues_an_admitted_turn()
    }

    /// Whether the attached generation serves the durable resume write
    /// (`POST /api/session/{id}/resume`, V2). See
    /// [`crate::backend::Backend::resume_supported`].
    pub fn resume_supported(&self) -> bool {
        self.strategy().resume_supported()
    }

    pub async fn session_status(
        &self,
        session_id: &str,
        directory: Option<&str>,
    ) -> crate::error::Result<Option<SessionStatus>> {
        self.strategy()
            .session_status(&self.transport, session_id, directory)
            .await
    }

    /// The runtime verdict for the named Background Tasks of one session (V2's
    /// live shell/session registries; issue #454). See
    /// [`crate::backend::Backend::task_runtime`].
    pub async fn task_runtime(
        &self,
        session_id: &str,
        directory: Option<&str>,
        shells: &[String],
        children: &[String],
    ) -> crate::error::Result<TaskRuntime> {
        self.strategy()
            .task_runtime(&self.transport, session_id, directory, shells, children)
            .await
    }

    /// One shell's captured output window (spec #588, #592). See
    /// [`crate::backend::Backend::shell_output`].
    pub async fn shell_output(
        &self,
        shell_id: &str,
        directory: Option<&str>,
    ) -> crate::error::Result<Option<crate::backend::ShellOutputWindow>> {
        self.strategy()
            .shell_output(&self.transport, shell_id, directory)
            .await
    }

    pub async fn model_context_window(
        &self,
        provider: &str,
        model: &str,
    ) -> crate::error::Result<Option<i64>> {
        self.strategy()
            .model_context_window(&self.transport, provider, model)
            .await
    }

    pub async fn list_agents(&self) -> Vec<AgentInfo> {
        self.strategy().list_agents(&self.transport).await
    }

    pub async fn list_models(&self) -> Vec<ProviderModels> {
        self.strategy().list_models(&self.transport).await
    }

    pub async fn session_info(
        &self,
        session_id: &str,
        directory: Option<&str>,
    ) -> crate::error::Result<SessionInfo> {
        self.strategy()
            .session_info(&self.transport, session_id, directory)
            .await
    }

    pub async fn interrupt(&self, session_id: &str) -> crate::error::Result<()> {
        self.strategy().interrupt(&self.transport, session_id).await
    }

    /// Resume a session (`POST /api/session/{id}/resume`, V2): promote a queued
    /// steer at a new run's start. Only called where
    /// [`Self::resume_supported`] is true.
    pub async fn resume(&self, session_id: &str) -> crate::error::Result<()> {
        self.strategy().resume(&self.transport, session_id).await
    }

    /// Compact a session's context. The `/api/session/{id}/compact` path is
    /// shared, but the bodies around it are not (V1 sends none and answers 204;
    /// V2 sends `{}` and answers `{data}`), so the strategy owns the call.
    pub async fn compact(&self, session_id: &str) -> crate::error::Result<()> {
        self.strategy().compact(&self.transport, session_id).await
    }

    /// Test-only: point the live transport at a no-proxy HTTP client so the
    /// wire tests' loopback fake server is never intercepted by a developer
    /// shell's `http_proxy` (ticket 09). Every other byte of the construction
    /// stays production's (ADR-0031).
    #[cfg(test)]
    pub(crate) fn disable_env_proxy(&self, username: Option<&str>, password: Option<&str>) {
        self.transport.disable_env_proxy(username, password);
    }
}
