//! The generation strategy boundary (ADR-0055).
//!
//! One generation-blind adapter ([`super::client::OpenCodeBackend`]) sits in
//! front of a per-generation **strategy**. A strategy owns everything that
//! differs between OpenCode protocol generations: endpoint paths, request
//! bodies, response decoding and status semantics. The adapter, the Bridge,
//! the Turn, the pollers, the cards and the SessionStore never branch on a
//! generation; they call the adapter, which forwards to the selected strategy.
//!
//! The selection arm ([`Generation::strategy`]) is driven by attach-time
//! detection ([`super::generation`], spec #364 §2): every attach/reconnect
//! probes `GET /api/info` and selects the generation the server reported (or
//! the `[opencode] generation` override forces). Deleting [`super::v1`] is V1
//! retirement; [`super::v2`] is the V2 strategy, whose capabilities land slice
//! by slice.

use std::sync::Arc;

use async_trait::async_trait;

use crate::backend::SessionTranscript;
use crate::error::Result;

use super::transport::Transport;
use super::types::{
    AgentInfo, ImageInput, ModelInfo, PermissionRequest, PromptResponse, ProviderModels, QuestionRequest,
    SessionInfo, SessionListInfo, SessionStatus,
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
    async fn list_sessions(&self, http: &Transport) -> Result<Vec<SessionListInfo>>;

    async fn update_session_title(&self, http: &Transport, session_id: &str, title: &str) -> Result<()>;

    /// Delete a session server-side. V2 cascades to the session's child
    /// sessions and answers 204; V1 deletes the one session and answers a
    /// boolean body. Un-mapping a chat is a separate, local act. No Bridge
    /// command calls it yet; the generation wire tests pin the route and the
    /// 204 handling (spec #364, S4a).
    #[allow(dead_code)] // no bridge caller yet; the generation wire tests drive it
    async fn delete_session(&self, http: &Transport, session_id: &str) -> Result<()>;

    /// A synchronous prompt: the call returns only when the turn is done, with
    /// the assistant response inline. `model` is already the effective model
    /// (the adapter resolved the configured default before dispatch). V1 blocks
    /// natively; V2 has no synchronous prompt and polyfills the block with the
    /// experimental `session.wait` endpoint plus a poll fallback (ADR-0056), so
    /// the trait's contract is the same on both generations.
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

    /// Compact a session's context. The endpoint path is the `/api` surface
    /// both generations serve, but the contract around it is not shared: V1
    /// takes no body and answers 204, V2 takes an all-optional payload and
    /// answers a `{data}` envelope. The strategy owns which body is sent (and
    /// whether one is sent at all).
    async fn compact(&self, http: &Transport, session_id: &str) -> Result<()>;
}

/// An OpenCode protocol generation cola can speak (CONTEXT.md "Generation").
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Generation {
    /// The 1.18.x unprefixed compatibility surface.
    V1,
    /// The 2.0.x `/api`-only surface.
    V2,
}

impl Generation {
    /// The log/config spelling of this generation (`v1` / `v2`).
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Generation::V1 => "v1",
            Generation::V2 => "v2",
        }
    }

    /// The selection arm: the strategy that speaks this generation. Choosing
    /// the arm is the adapter's only generation-aware act; detection lives in
    /// [`super::generation`].
    pub(crate) fn strategy(self) -> Arc<dyn GenerationStrategy> {
        match self {
            Generation::V1 => Arc::new(super::v1::V1Strategy),
            Generation::V2 => Arc::new(super::v2::V2Strategy),
        }
    }
}
