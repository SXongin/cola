//! The generation-blind OpenCode adapter (ADR-0055).
//!
//! [`OpenCodeBackend`] owns the HTTP transport and the generation strategy
//! selected for the attached server. Every protocol call is forwarded to that
//! strategy (the boundary is [`crate::opencode::strategy`]), so this adapter —
//! and everything above it — never branches on a protocol generation. The
//! strategy for a generation owns its endpoint paths, request bodies and
//! response decoders; V1's lives under [`crate::opencode::v1`], and deleting
//! that module is V1 retirement.

use std::sync::Arc;

use crate::backend::SessionTranscript;

use super::parsing::parse_model;
use super::strategy::{Generation, GenerationStrategy};
use super::transport::Transport;
use super::types::{
    AgentInfo, CreateSessionInput, ImageInput, ModelInfo, PermissionRequest, PromptResponse, ProviderModels,
    QuestionRequest, Session, SessionInfo, SessionListInfo, SessionStatus,
};

/// One OpenCode server attachment, generation-blind.
///
/// The server cola talks to can be restarted/replaced at runtime (another tool
/// like OpenChamber manages it), which may change its port and password. So the
/// endpoint — base URL + auth — is held by the shared [`Transport`] and can be
/// swapped via [`OpenCodeBackend::reconnect`] without dropping the shared
/// `Arc<dyn Backend>`.
pub struct OpenCodeBackend {
    transport: Transport,
    /// The username used for Basic auth (reused on reconnect).
    username: Option<String>,
    /// Default model for new sessions, e.g. "opencode/deepseek-v4-flash-free"
    pub model: Option<ModelInfo>,
    /// The strategy that speaks the attached server's generation. The
    /// selection arm ([`Generation::V1`]) is fixed until attach detection
    /// lands (spec #364, S3).
    strategy: Arc<dyn GenerationStrategy>,
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
    pub fn new(model: Option<&str>, server: Option<crate::bridge::discovery::ResolvedServer>) -> Self {
        match server {
            Some(crate::bridge::discovery::ResolvedServer {
                url,
                username,
                password,
            }) => Self::with_base_url(model, url, Some(&username), Some(&password)),
            None => Self::with_base_url(
                model,
                String::new(),
                Some(crate::bridge::discovery::DEFAULT_SERVER_USERNAME),
                None,
            ),
        }
    }

    /// Build a backend against an explicit base URL and credentials.
    /// Production normally goes through [`OpenCodeBackend::new`] (a discovered
    /// server) or [`OpenCodeBackend::reconnect`] (the live endpoint was
    /// replaced); this constructor also lets tests point the real backend at a
    /// local fake server so the HTTP layer is exercised end to end (ADR-0031).
    /// A trailing slash is tolerated.
    ///
    /// Basic auth is attached only when BOTH username and password are present
    /// — a half-credential must never reach the wire (the server checks the
    /// username too, so a password-only request 401s).
    pub fn with_base_url(
        model: Option<&str>,
        base_url: impl Into<String>,
        username: Option<&str>,
        password: Option<&str>,
    ) -> Self {
        Self {
            transport: Transport::new(username, password, base_url),
            username: username.map(str::to_string),
            model: model.and_then(parse_model),
            // The selection arm: detection is not wired yet, so every
            // attachment speaks V1 (spec #364, S3 adds the probe).
            strategy: Generation::V1.strategy(),
        }
    }

    /// Point this backend at a different server (port/password changed because
    /// the old one was restarted/replaced). Rare; only called by the reconnect
    /// loop when discovery finds the attached server is gone.
    ///
    /// The generation strategy is deliberately NOT re-selected here yet:
    /// re-probing on reconnect arrives with attach detection (spec #364, S3).
    pub async fn reconnect(&self, url: &str, password: &str) {
        self.transport.repoint(url, password, self.username.as_deref());
        tracing::info!("reconnected opencode client to {}", url);
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
        self.strategy.new_session_input(self.model.as_ref(), directory)
    }

    pub async fn create_session(&self, input: &CreateSessionInput) -> crate::error::Result<Session> {
        self.strategy.create_session(&self.transport, input).await
    }

    pub async fn list_sessions(&self) -> crate::error::Result<Vec<SessionListInfo>> {
        self.strategy.list_sessions(&self.transport).await
    }

    pub async fn update_session_title(&self, session_id: &str, title: &str) -> crate::error::Result<()> {
        self.strategy
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
        self.strategy
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
        self.strategy
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
        self.strategy
            .reply_permission(&self.transport, request_id, reply, directory)
            .await
    }

    pub async fn list_permissions(
        &self,
        directory: Option<&str>,
    ) -> crate::error::Result<Vec<PermissionRequest>> {
        self.strategy.list_permissions(&self.transport, directory).await
    }

    pub async fn list_questions(
        &self,
        directory: Option<&str>,
    ) -> crate::error::Result<Vec<QuestionRequest>> {
        self.strategy.list_questions(&self.transport, directory).await
    }

    pub async fn reply_question(
        &self,
        request_id: &str,
        answers: &[Vec<String>],
        directory: Option<&str>,
    ) -> crate::error::Result<()> {
        self.strategy
            .reply_question(&self.transport, request_id, answers, directory)
            .await
    }

    pub async fn reject_question(
        &self,
        request_id: &str,
        directory: Option<&str>,
    ) -> crate::error::Result<()> {
        self.strategy
            .reject_question(&self.transport, request_id, directory)
            .await
    }

    pub async fn transcript(&self, session_id: &str) -> crate::error::Result<SessionTranscript> {
        self.strategy.transcript(&self.transport, session_id).await
    }

    pub async fn session_status(
        &self,
        session_id: &str,
        directory: Option<&str>,
    ) -> crate::error::Result<Option<SessionStatus>> {
        self.strategy
            .session_status(&self.transport, session_id, directory)
            .await
    }

    pub async fn model_context_window(
        &self,
        provider: &str,
        model: &str,
    ) -> crate::error::Result<Option<i64>> {
        self.strategy
            .model_context_window(&self.transport, provider, model)
            .await
    }

    pub async fn list_agents(&self) -> Vec<AgentInfo> {
        self.strategy.list_agents(&self.transport).await
    }

    pub async fn list_models(&self) -> Vec<ProviderModels> {
        self.strategy.list_models(&self.transport).await
    }

    pub async fn session_info(
        &self,
        session_id: &str,
        directory: Option<&str>,
    ) -> crate::error::Result<SessionInfo> {
        self.strategy
            .session_info(&self.transport, session_id, directory)
            .await
    }

    pub async fn interrupt(&self, session_id: &str) -> crate::error::Result<()> {
        self.strategy.interrupt(&self.transport, session_id).await
    }

    pub async fn compact(&self, session_id: &str) -> crate::error::Result<()> {
        self.strategy.compact(&self.transport, session_id).await
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
