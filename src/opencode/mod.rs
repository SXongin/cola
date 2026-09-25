//! The OpenCode HTTP adapter: it implements the backend contract
//! ([`crate::backend::Backend`]) over the server's REST API, and owns the
//! private per-generation wire decoders that turn payloads into the neutral
//! read model (ADR-0010, ADR-0053).
//!
//! Protocol field names live only in [`wire`]; everything else imports the
//! contract from [`crate::backend`], never from here.

pub mod client;
pub(crate) mod parsing;
pub mod types;
pub(crate) mod wire;

use std::sync::Arc;

use crate::backend::{Backend, BackendDirectory, DirectoryBackend, SessionTranscript};
use crate::error::Result;
use async_trait::async_trait;
use client::Client;
use types::{
    AgentInfo, CreateSessionInput, ImageInput, ModelInfo, PermissionRequest, PromptResponse, ProviderModels,
    QuestionRequest, Session, SessionInfo, SessionListInfo, SessionStatus,
};

#[async_trait]
impl Backend for Client {
    fn new_session_input(&self, directory: Option<&str>) -> CreateSessionInput {
        Client::new_session_input(self, directory)
    }

    async fn create_session(&self, input: &CreateSessionInput) -> Result<Session> {
        Client::create_session(self, input).await
    }

    async fn list_sessions(&self) -> Result<Vec<SessionListInfo>> {
        Client::list_sessions(self).await
    }

    async fn update_session_title(&self, session_id: &str, title: &str) -> Result<()> {
        Client::update_session_title(self, session_id, title).await
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
    ) -> Result<PromptResponse> {
        Client::prompt(self, session_id, text, images, model, variant, agent, message_id).await
    }

    async fn prompt_async(
        &self,
        session_id: &str,
        text: &str,
        images: &[ImageInput],
        model: Option<&ModelInfo>,
        variant: Option<&str>,
        agent: Option<&str>,
        message_id: Option<&str>,
    ) -> Result<()> {
        Client::prompt_async(self, session_id, text, images, model, variant, agent, message_id).await
    }

    async fn reply_permission(&self, request_id: &str, reply: &str, directory: Option<&str>) -> Result<()> {
        Client::reply_permission(self, request_id, reply, directory).await
    }

    async fn list_permissions(&self, directory: Option<&str>) -> Result<Vec<PermissionRequest>> {
        Client::list_permissions(self, directory).await
    }

    async fn list_questions(&self, directory: Option<&str>) -> Result<Vec<QuestionRequest>> {
        Client::list_questions(self, directory).await
    }

    async fn reply_question(
        &self,
        request_id: &str,
        answers: &[Vec<String>],
        directory: Option<&str>,
    ) -> Result<()> {
        Client::reply_question(self, request_id, answers, directory).await
    }

    async fn reject_question(&self, request_id: &str, directory: Option<&str>) -> Result<()> {
        Client::reject_question(self, request_id, directory).await
    }

    async fn transcript(&self, session_id: &str) -> Result<SessionTranscript> {
        Client::transcript(self, session_id).await
    }

    async fn session_status(
        &self,
        session_id: &str,
        directory: Option<&str>,
    ) -> Result<Option<SessionStatus>> {
        Client::session_status(self, session_id, directory).await
    }

    async fn model_context_window(&self, provider: &str, model: &str) -> Result<Option<i64>> {
        Client::model_context_window(self, provider, model).await
    }

    fn configured_default_model(&self) -> Option<ModelInfo> {
        Client::configured_default_model(self)
    }

    async fn list_agents(&self) -> Vec<AgentInfo> {
        Client::list_agents(self).await
    }

    async fn list_models(&self) -> Vec<ProviderModels> {
        Client::list_models(self).await
    }

    async fn session_info(&self, session_id: &str, directory: Option<&str>) -> Result<SessionInfo> {
        Client::session_info(self, session_id, directory).await
    }

    async fn interrupt(&self, session_id: &str) -> Result<()> {
        Client::interrupt(self, session_id).await
    }

    async fn compact(&self, session_id: &str) -> Result<()> {
        Client::compact(self, session_id).await
    }

    async fn reconnect(&self, url: &str, password: &str) -> Result<()> {
        Client::reconnect(self, url, password).await;
        Ok(())
    }

    fn base_url(&self) -> String {
        Client::base_url(self)
    }

    fn can_self_start_server(&self) -> bool {
        true
    }

    fn for_directory(self: Arc<Self>, directory: &str) -> Arc<dyn DirectoryBackend> {
        Arc::new(BackendDirectory::new(self, directory.to_string()))
    }
}
