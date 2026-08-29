pub mod client;
pub mod types;

pub use client::{
    Client, CreateSessionInput, ModelInfo, PermissionRequest, PromptResponse, QuestionRequest, Session,
    SessionInfo, SessionListInfo, SessionMessage,
};

use crate::error::Result;
use async_trait::async_trait;

/// The OpenCode backend, abstracted so tests can drive the bridge with canned
/// responses instead of a live server. The real implementation is
/// [`Client`]; mock implementations feed scripted parts/permissions and verify
/// what cola renders from them.
#[async_trait]
pub trait Backend: Send + Sync {
    fn new_session_input(&self, directory: Option<&str>) -> CreateSessionInput;

    async fn create_session(&self, input: &CreateSessionInput) -> Result<Session>;

    /// List every session in the shared store (canonical `GET /session`).
    async fn list_sessions(&self) -> Result<Vec<SessionListInfo>>;

    /// Rename a session server-side (`PATCH /session/{id}` with a title).
    async fn update_session_title(&self, session_id: &str, title: &str) -> Result<()>;

    /// `model` is the per-session `/model` override (parsed "provider/model");
    /// None → the configured default applies, and if that's also unset the
    /// server uses its own default model.
    ///
    /// `agent` is the per-session `/agent` override; None → the server uses the
    /// session's own/default agent.
    ///
    /// `images` are attached as data-URL `file` parts; requires a vision-capable
    /// model (unsupported models surface an error).
    async fn prompt(
        &self,
        session_id: &str,
        text: &str,
        images: &[client::ImageInput],
        model: Option<&ModelInfo>,
        agent: Option<&str>,
    ) -> Result<PromptResponse>;

    /// Fire-and-forget prompt (OpenCode `prompt_async`): message persisted and
    /// a run forked, returns immediately. Used by the supplement path so a
    /// message sent mid-turn doesn't block. Same `images` semantics as `prompt`.
    async fn prompt_async(
        &self,
        session_id: &str,
        text: &str,
        images: &[client::ImageInput],
        model: Option<&ModelInfo>,
        agent: Option<&str>,
    ) -> Result<()>;

    async fn reply_permission(&self, request_id: &str, reply: &str, directory: Option<&str>) -> Result<()>;

    async fn list_permissions(&self, directory: Option<&str>) -> Result<Vec<PermissionRequest>>;

    async fn list_questions(&self, directory: Option<&str>) -> Result<Vec<QuestionRequest>>;

    async fn reply_question(
        &self,
        request_id: &str,
        answers: &[Vec<String>],
        directory: Option<&str>,
    ) -> Result<()>;

    async fn reject_question(&self, request_id: &str, directory: Option<&str>) -> Result<()>;

    async fn messages(&self, session_id: &str) -> Result<Vec<SessionMessage>>;

    /// The model's context-window size (tokens), from `GET /provider`. Used to
    /// compute the context-usage ratio for the card footer. Best-effort: None
    /// when the provider/model can't be resolved.
    async fn model_context_window(&self, provider: &str, model: &str) -> Result<Option<i64>>;

    /// Fetch a session's info (exposes the parent chain for sub-task sessions).
    async fn session_info(&self, session_id: &str, directory: Option<&str>) -> Result<SessionInfo>;

    async fn interrupt(&self, session_id: &str) -> Result<()>;

    async fn compact(&self, session_id: &str) -> Result<()>;

    /// Re-point the backend at a different OpenCode server (port/password
    /// changed because the server was restarted/replaced at runtime). No-op for
    /// mocks.
    async fn reconnect(&self, url: &str, password: &str) -> Result<()>;

    /// The base URL this backend currently targets (used by the reconnect loop
    /// to detect a changed server).
    fn base_url(&self) -> String;
}

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
        images: &[client::ImageInput],
        model: Option<&ModelInfo>,
        agent: Option<&str>,
    ) -> Result<PromptResponse> {
        Client::prompt(self, session_id, text, images, model, agent).await
    }

    async fn prompt_async(
        &self,
        session_id: &str,
        text: &str,
        images: &[client::ImageInput],
        model: Option<&ModelInfo>,
        agent: Option<&str>,
    ) -> Result<()> {
        Client::prompt_async(self, session_id, text, images, model, agent).await
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

    async fn messages(&self, session_id: &str) -> Result<Vec<SessionMessage>> {
        Client::messages(self, session_id).await
    }

    async fn model_context_window(&self, provider: &str, model: &str) -> Result<Option<i64>> {
        Client::model_context_window(self, provider, model).await
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
}
