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

use crate::backend::SessionTranscript;

use super::parsing::parse_model;
use super::strategy::{Generation, GenerationStrategy};
use super::transport::Transport;
use super::types::{
    AgentInfo, CreateSessionInput, CreateSessionResponse, ImageInput, Location, ModelInfo, PermissionRequest,
    PromptResponse, ProviderModels, QuestionRequest, Session, SessionInfo, SessionListInfo, SessionStatus,
};

/// Session creation and compaction, on the `/api` surface both generations
/// serve. These are current-generation calls, not V1 coupling, so they live on
/// the generation-blind adapter rather than in a strategy.
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
}

impl Clone for OpenCodeBackend {
    fn clone(&self) -> Self {
        Self {
            transport: self.transport.clone(),
            username: self.username.clone(),
            model: self.model.clone(),
            strategy: Arc::clone(&self.strategy),
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
                generation,
            }) => Self::with_generation(model, url, Some(&username), Some(&password), generation),
            None => Self::with_generation(
                model,
                String::new(),
                Some(crate::bridge::discovery::DEFAULT_SERVER_USERNAME),
                None,
                Generation::V1,
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
        Self::with_generation(model, base_url, username, password, Generation::V1)
    }

    /// [`Self::with_base_url`] with the generation strategy made explicit.
    pub(crate) fn with_generation(
        model: Option<&str>,
        base_url: impl Into<String>,
        username: Option<&str>,
        password: Option<&str>,
        generation: Generation,
    ) -> Self {
        Self {
            transport: Transport::new(username, password, base_url),
            username: username.map(str::to_string),
            model: model.and_then(parse_model),
            strategy: Arc::new(RwLock::new(generation.strategy())),
        }
    }

    /// The strategy that speaks the currently attached generation.
    fn strategy(&self) -> Arc<dyn GenerationStrategy> {
        self.strategy
            .read()
            .expect("the strategy lock is never poisoned")
            .clone()
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
                tracing::info!(
                    "reconnected opencode backend to {} (generation={})",
                    server.url,
                    server.generation.as_str()
                );
            }
            None => {
                self.transport.repoint("", "", self.username.as_deref());
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
    ) -> crate::error::Result<PromptResponse> {
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

    #[allow(clippy::too_many_arguments)] // same prompt axes as `prompt`
    pub async fn prompt_async(
        &self,
        session_id: &str,
        text: &str,
        images: &[ImageInput],
        model: Option<&ModelInfo>,
        variant: Option<&str>,
        agent: Option<&str>,
        message_id: Option<&str>,
    ) -> crate::error::Result<()> {
        let model = model.or(self.model.as_ref());
        self.strategy()
            .prompt_async(
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
        request_id: &str,
        reply: &str,
        directory: Option<&str>,
    ) -> crate::error::Result<()> {
        self.strategy()
            .reply_permission(&self.transport, request_id, reply, directory)
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
        request_id: &str,
        answers: &[Vec<String>],
        directory: Option<&str>,
    ) -> crate::error::Result<()> {
        self.strategy()
            .reply_question(&self.transport, request_id, answers, directory)
            .await
    }

    pub async fn reject_question(
        &self,
        request_id: &str,
        directory: Option<&str>,
    ) -> crate::error::Result<()> {
        self.strategy()
            .reject_question(&self.transport, request_id, directory)
            .await
    }

    pub async fn transcript(&self, session_id: &str) -> crate::error::Result<SessionTranscript> {
        self.strategy().transcript(&self.transport, session_id).await
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

    /// Compact a session's context, on the current-generation `/api/session`
    /// route (shared by both generations).
    pub async fn compact(&self, session_id: &str) -> crate::error::Result<()> {
        self.transport
            .client()
            .post(self.transport.url(&format!("{API_SESSION}/{session_id}/compact")))
            .send()
            .await?
            .error_for_status()?;
        Ok(())
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
