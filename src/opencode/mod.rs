//! The OpenCode HTTP adapter: it implements the backend contract
//! ([`crate::backend::Backend`]) over the server's REST API.
//!
//! The adapter ([`client::OpenCodeBackend`]) is generation-blind. Protocol
//! differences live behind the per-generation **strategy** boundary
//! ([`strategy`], ADR-0055): a strategy owns that generation's endpoint paths,
//! request bodies and response decoders, and the adapter forwards every call to
//! the selected one. The V1 strategy (the unprefixed 1.18.x surface) lives under
//! [`v1`], together with the legacy wire decoder — deleting that module is V1
//! retirement. The V2 strategy (the `/api`-only 2.0.x surface) lives under
//! [`v2`], and attach-time detection lives in [`generation`] (spec #364 §2).
//!
//! Protocol field names for the message/transcript read live only in the
//! strategy's private wire decoder; everything else imports the contract from
//! [`crate::backend`], never from here.

pub mod client;
#[cfg(test)]
mod conformance;
pub(crate) mod generation;
#[cfg(test)]
mod live;
pub(crate) mod parsing;
pub(crate) mod strategy;
#[cfg(test)]
mod tests;
pub(crate) mod transport;
pub mod types;
pub(crate) mod v1;
pub(crate) mod v2;
#[cfg(test)]
mod wire;

use std::sync::Arc;

use crate::backend::{Backend, BackendDirectory, DirectoryBackend, SessionTranscript};
use crate::error::Result;
use async_trait::async_trait;
use client::OpenCodeBackend;
use types::{
    AgentInfo, CreateSessionInput, FormAnswer, ImageInput, ModelInfo, PermissionRequest, ProviderModels,
    QuestionRequest, Session, SessionInfo, SessionListInfo, SessionSelection, SessionStatus,
};

#[async_trait]
impl Backend for OpenCodeBackend {
    fn new_session_input(&self, directory: Option<&str>) -> CreateSessionInput {
        OpenCodeBackend::new_session_input(self, directory)
    }

    async fn create_session(&self, input: &CreateSessionInput) -> Result<Session> {
        OpenCodeBackend::create_session(self, input).await
    }

    async fn list_sessions(&self) -> Result<Vec<SessionListInfo>> {
        OpenCodeBackend::list_sessions(self).await
    }

    async fn update_session_title(&self, session_id: &str, title: &str) -> Result<()> {
        OpenCodeBackend::update_session_title(self, session_id, title).await
    }

    async fn delete_session(&self, session_id: &str) -> Result<()> {
        OpenCodeBackend::delete_session(self, session_id).await
    }

    async fn prompt(
        &self,
        session_id: &str,
        text: &str,
        images: &[ImageInput],
        model: Option<&ModelInfo>,
        variant: Option<&str>,
        agent: Option<&str>,
        message_id: Option<&str>,
    ) -> Result<()> {
        OpenCodeBackend::prompt(self, session_id, text, images, model, variant, agent, message_id).await
    }

    async fn reply_permission(
        &self,
        session_id: &str,
        request_id: &str,
        reply: &str,
        directory: Option<&str>,
    ) -> Result<()> {
        OpenCodeBackend::reply_permission(self, session_id, request_id, reply, directory).await
    }

    async fn list_permissions(&self, directory: Option<&str>) -> Result<Vec<PermissionRequest>> {
        OpenCodeBackend::list_permissions(self, directory).await
    }

    async fn list_questions(&self, directory: Option<&str>) -> Result<Vec<QuestionRequest>> {
        OpenCodeBackend::list_questions(self, directory).await
    }

    async fn reply_question(
        &self,
        session_id: &str,
        request_id: &str,
        answers: &[FormAnswer],
        directory: Option<&str>,
    ) -> Result<()> {
        OpenCodeBackend::reply_question(self, session_id, request_id, answers, directory).await
    }

    async fn reject_question(
        &self,
        session_id: &str,
        request_id: &str,
        directory: Option<&str>,
    ) -> Result<()> {
        OpenCodeBackend::reject_question(self, session_id, request_id, directory).await
    }

    async fn transcript(&self, session_id: &str) -> Result<SessionTranscript> {
        OpenCodeBackend::transcript(self, session_id).await
    }

    async fn session_selection(
        &self,
        session_id: &str,
        directory: Option<&str>,
    ) -> Result<Option<SessionSelection>> {
        OpenCodeBackend::session_selection(self, session_id, directory).await
    }

    async fn switch_session_model(&self, session_id: &str, model: &ModelInfo) -> Result<()> {
        OpenCodeBackend::switch_session_model(self, session_id, model).await
    }

    async fn switch_session_agent(&self, session_id: &str, agent: &str) -> Result<()> {
        OpenCodeBackend::switch_session_agent(self, session_id, agent).await
    }

    fn keeps_session_selection(&self) -> bool {
        OpenCodeBackend::keeps_session_selection(self)
    }

    fn reuse_continues_an_admitted_turn(&self) -> bool {
        OpenCodeBackend::reuse_continues_an_admitted_turn(self)
    }

    async fn session_status(
        &self,
        session_id: &str,
        directory: Option<&str>,
    ) -> Result<Option<SessionStatus>> {
        OpenCodeBackend::session_status(self, session_id, directory).await
    }

    async fn model_context_window(&self, provider: &str, model: &str) -> Result<Option<i64>> {
        OpenCodeBackend::model_context_window(self, provider, model).await
    }

    fn configured_default_model(&self) -> Option<ModelInfo> {
        OpenCodeBackend::configured_default_model(self)
    }

    async fn list_agents(&self) -> Vec<AgentInfo> {
        OpenCodeBackend::list_agents(self).await
    }

    async fn list_models(&self) -> Vec<ProviderModels> {
        OpenCodeBackend::list_models(self).await
    }

    async fn session_info(&self, session_id: &str, directory: Option<&str>) -> Result<SessionInfo> {
        OpenCodeBackend::session_info(self, session_id, directory).await
    }

    async fn interrupt(&self, session_id: &str) -> Result<()> {
        OpenCodeBackend::interrupt(self, session_id).await
    }

    async fn compact(&self, session_id: &str) -> Result<()> {
        OpenCodeBackend::compact(self, session_id).await
    }

    async fn reconnect(&self, server: Option<&crate::bridge::discovery::ResolvedServer>) -> Result<()> {
        OpenCodeBackend::reconnect(self, server);
        Ok(())
    }

    fn base_url(&self) -> String {
        OpenCodeBackend::base_url(self)
    }

    fn attached_server_pid(&self) -> Option<i32> {
        OpenCodeBackend::attached_server_pid(self)
    }

    fn can_self_start_server(&self) -> bool {
        true
    }

    fn for_directory(self: Arc<Self>, directory: &str) -> Arc<dyn DirectoryBackend> {
        Arc::new(BackendDirectory::new(self, directory.to_string()))
    }
}
