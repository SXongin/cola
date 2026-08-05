pub mod client;
pub mod sse;
pub mod types;

pub use client::{
    Client, CreateSessionInput, PermissionRequest, PromptResponse, Session, SessionMessage,
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

    async fn reply_permission(
        &self,
        request_id: &str,
        reply: &str,
        directory: Option<&str>,
    ) -> Result<()>;

    async fn list_permissions(
        &self,
        directory: Option<&str>,
    ) -> Result<Vec<PermissionRequest>>;

    async fn messages(&self, session_id: &str) -> Result<Vec<SessionMessage>>;

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

    async fn reply_permission(
        &self,
        request_id: &str,
        reply: &str,
        directory: Option<&str>,
    ) -> Result<()> {
        Client::reply_permission(self, request_id, reply, directory).await
    }

    async fn list_permissions(
        &self,
        directory: Option<&str>,
    ) -> Result<Vec<PermissionRequest>> {
        Client::list_permissions(self, directory).await
    }

    async fn messages(&self, session_id: &str) -> Result<Vec<SessionMessage>> {
        Client::messages(self, session_id).await
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
