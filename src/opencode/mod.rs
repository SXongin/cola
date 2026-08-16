pub mod client;
pub mod types;

pub use client::{
    Client, CreateSessionInput, PermissionRequest, PromptResponse, QuestionRequest, Session, SessionInfo,
    SessionMessage,
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

    async fn prompt(&self, session_id: &str, text: &str) -> Result<PromptResponse>;

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

    async fn switch_agent(&self, session_id: &str, agent: &str) -> Result<()>;

    async fn switch_model(&self, session_id: &str, model: &str) -> Result<()>;
}

#[async_trait]
impl Backend for Client {
    fn new_session_input(&self, directory: Option<&str>) -> CreateSessionInput {
        Client::new_session_input(self, directory)
    }

    async fn create_session(&self, input: &CreateSessionInput) -> Result<Session> {
        Client::create_session(self, input).await
    }

    async fn prompt(&self, session_id: &str, text: &str) -> Result<PromptResponse> {
        Client::prompt(self, session_id, text).await
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

    async fn switch_agent(&self, session_id: &str, agent: &str) -> Result<()> {
        Client::switch_agent(self, session_id, agent).await
    }

    async fn switch_model(&self, session_id: &str, model: &str) -> Result<()> {
        Client::switch_model(self, session_id, model).await
    }
}
