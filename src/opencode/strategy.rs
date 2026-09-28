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
//! retirement; [`super::v2`] is the V2 strategy, whose capabilities have all
//! landed.

use std::sync::Arc;

use async_trait::async_trait;

use crate::backend::SessionTranscript;
use crate::error::Result;

use super::transport::Transport;
use super::types::{
    AgentInfo, FormAnswer, ImageInput, ModelInfo, PermissionRequest, ProviderModels, QuestionRequest,
    SessionInfo, SessionListInfo, SessionSelection, SessionStatus,
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
    /// boolean body. Un-mapping a chat is a separate, local act.
    async fn delete_session(&self, http: &Transport, session_id: &str) -> Result<()>;

    /// Submit a prompt: the message is persisted and a run is scheduled, then
    /// the call returns without waiting for the turn. V1 posts its native
    /// `prompt_async`; V2 durably admits through its native prompt and returns.
    /// The Turn observes completion from the transcript and run state
    /// (ADR-0056's submit+observe end state); `model` is already the effective
    /// model (the adapter resolved the configured default before dispatch).
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
    ) -> Result<()>;

    /// Reply to a pending permission request with a decision (`once` /
    /// `always` / `reject`). V1 answers the global request id under the owning
    /// directory's instance; V2's reply is session-scoped, so its strategy
    /// needs the session id as well.
    async fn reply_permission(
        &self,
        http: &Transport,
        session_id: &str,
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

    /// Answer a pending form (V2) / question (V1). V1 takes positional string
    /// arrays; V2 takes a keyed answer object assembled from the same neutral
    /// [`FormAnswer`] list, so the strategy owns the generation's body shape.
    async fn reply_question(
        &self,
        http: &Transport,
        session_id: &str,
        request_id: &str,
        answers: &[FormAnswer],
        directory: Option<&str>,
    ) -> Result<()>;

    /// Cancel a pending form (V2) / reject a question (V1) by request id.
    async fn reject_question(
        &self,
        http: &Transport,
        session_id: &str,
        request_id: &str,
        directory: Option<&str>,
    ) -> Result<()>;

    async fn transcript(&self, http: &Transport, session_id: &str) -> Result<SessionTranscript>;

    /// The session's durable model/agent selection (`GET /api/session/{id}` on
    /// V2). V1 has no session-scoped selection — its picks ride each prompt —
    /// so it always answers `None` without a request.
    async fn session_selection(
        &self,
        http: &Transport,
        session_id: &str,
        directory: Option<&str>,
    ) -> Result<Option<SessionSelection>>;

    /// The model the session last actually ran with (V2: the newest assistant
    /// message's model ref, `type=assistant&order=desc&limit=1`). V1 answers
    /// `None` without a request — its effective-model ladder already reads the
    /// server-recorded session model through
    /// [`Self::session_info`](Self::session_info).
    async fn session_last_run_model(
        &self,
        http: &Transport,
        session_id: &str,
        directory: Option<&str>,
    ) -> Result<Option<ModelInfo>>;

    /// Switch the session's model (`POST /api/session/{id}/model`, 204); V1 is
    /// a no-op because its model rides the next prompt.
    async fn switch_session_model(&self, http: &Transport, session_id: &str, model: &ModelInfo)
    -> Result<()>;

    /// Switch the session's agent (`POST /api/session/{id}/agent`, 204); V1 is
    /// a no-op because its agent rides the next prompt.
    async fn switch_session_agent(&self, http: &Transport, session_id: &str, agent: &str) -> Result<()>;

    /// Whether the generation keeps model/agent choices as durable session
    /// state (V2) rather than sending them per prompt (V1). Unlike
    /// [`Self::session_selection`], this answers without a wire read, so a
    /// caller can tell "this generation needs a switch" from "the read
    /// failed".
    fn keeps_session_selection(&self) -> bool;

    /// Whether a same-id re-post of an admitted user message can still
    /// continue that turn on this generation (V1's upsert-continue) or is a
    /// no-op (V2's admission key). See
    /// [`crate::backend::Backend::reuse_continues_an_admitted_turn`].
    fn reuse_continues_an_admitted_turn(&self) -> bool;

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
