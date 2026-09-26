//! The generation strategy boundary (ADR-0055).
//!
//! One generation-blind adapter ([`super::client::OpenCodeBackend`]) sits in
//! front of a per-generation **strategy**. A strategy owns everything that
//! differs between OpenCode protocol generations: endpoint paths, request
//! bodies, response decoding and status semantics. The adapter, the Bridge,
//! the Turn, the pollers, the cards and the SessionStore never branch on a
//! generation; they call the adapter, which forwards to the selected strategy.
//!
//! V1 is the only strategy today, and the selection arm
//! ([`Generation::strategy`]) defaults to it. Attach-time detection
//! (`GET /api/info`) arrives with a later slice (spec #364, S3); until then
//! every attachment speaks V1, and deleting [`super::v1`] is V1 retirement.

use std::sync::Arc;

use async_trait::async_trait;

use crate::backend::SessionTranscript;
use crate::error::Result;

use super::transport::Transport;
use super::types::{
    AgentInfo, CreateSessionInput, ImageInput, ModelInfo, PermissionRequest, PromptResponse, ProviderModels,
    QuestionRequest, Session, SessionInfo, SessionListInfo, SessionStatus,
};

/// One protocol generation's wire contract.
///
/// Every method is generation-specific by construction: it renders the
/// caller's neutral arguments into that generation's request shape, sends it
/// over the shared [`Transport`], and decodes that generation's response into
/// the neutral read model. The adapter implements the [`crate::backend::Backend`]
/// trait by forwarding here.
#[async_trait]
pub(crate) trait GenerationStrategy: Send + Sync {
    /// The request body for a new session, applying cola's configured default
    /// model when set.
    fn new_session_input(&self, model: Option<&ModelInfo>, directory: Option<&str>) -> CreateSessionInput;

    async fn create_session(&self, http: &Transport, input: &CreateSessionInput) -> Result<Session>;

    async fn list_sessions(&self, http: &Transport) -> Result<Vec<SessionListInfo>>;

    async fn update_session_title(&self, http: &Transport, session_id: &str, title: &str) -> Result<()>;

    /// A synchronous prompt: the call returns only when the turn is done, with
    /// the assistant response inline. `model` is already the effective model
    /// (the adapter resolved the configured default before dispatch).
    #[allow(clippy::too_many_arguments)] // prompt axes: session/text/images + model/variant/agent/message-id
    async fn prompt(
        &self,
        http: &Transport,
        session_id: &str,
        text: &str,
        images: &[ImageInput],
        model: Option<&ModelInfo>,
        variant: Option<&str>,
        agent: Option<&str>,
        message_id: Option<&str>,
    ) -> Result<PromptResponse>;

    /// A fire-and-forget prompt (message persisted, a run forked, returns
    /// immediately).
    #[allow(clippy::too_many_arguments)] // same prompt axes as `prompt`
    async fn prompt_async(
        &self,
        http: &Transport,
        session_id: &str,
        text: &str,
        images: &[ImageInput],
        model: Option<&ModelInfo>,
        variant: Option<&str>,
        agent: Option<&str>,
        message_id: Option<&str>,
    ) -> Result<()>;

    async fn reply_permission(
        &self,
        http: &Transport,
        request_id: &str,
        reply: &str,
        directory: Option<&str>,
    ) -> Result<()>;

    async fn list_permissions(
        &self,
        http: &Transport,
        directory: Option<&str>,
    ) -> Result<Vec<PermissionRequest>>;

    async fn list_questions(&self, http: &Transport, directory: Option<&str>)
    -> Result<Vec<QuestionRequest>>;

    async fn reply_question(
        &self,
        http: &Transport,
        request_id: &str,
        answers: &[Vec<String>],
        directory: Option<&str>,
    ) -> Result<()>;

    async fn reject_question(
        &self,
        http: &Transport,
        request_id: &str,
        directory: Option<&str>,
    ) -> Result<()>;

    async fn transcript(&self, http: &Transport, session_id: &str) -> Result<SessionTranscript>;

    async fn session_status(
        &self,
        http: &Transport,
        session_id: &str,
        directory: Option<&str>,
    ) -> Result<Option<SessionStatus>>;

    async fn model_context_window(
        &self,
        http: &Transport,
        provider: &str,
        model: &str,
    ) -> Result<Option<i64>>;

    async fn list_agents(&self, http: &Transport) -> Vec<AgentInfo>;

    async fn list_models(&self, http: &Transport) -> Vec<ProviderModels>;

    async fn session_info(
        &self,
        http: &Transport,
        session_id: &str,
        directory: Option<&str>,
    ) -> Result<SessionInfo>;

    async fn interrupt(&self, http: &Transport, session_id: &str) -> Result<()>;

    async fn compact(&self, http: &Transport, session_id: &str) -> Result<()>;
}

/// An OpenCode protocol generation cola can speak (CONTEXT.md "Generation").
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Generation {
    /// The 1.18.x unprefixed compatibility surface.
    V1,
}

impl Generation {
    /// The selection arm: the strategy that speaks this generation. Detection
    /// is not wired yet (spec #364, S3), so [`Generation::V1`] is the only arm
    /// the adapter can select; adding V2 is a variant plus a match arm, not an
    /// adapter change.
    pub(crate) fn strategy(self) -> Arc<dyn GenerationStrategy> {
        match self {
            Generation::V1 => Arc::new(super::v1::V1Strategy),
        }
    }
}
