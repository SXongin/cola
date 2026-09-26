//! The V2 generation strategy (ADR-0055): OpenCode 2.0.x's `/api`-only surface.
//!
//! This module is where every V2 path, payload, decoder and semantic lands.
//! Attach detection (slice S3, [`super::generation`]) can already select it,
//! but its capabilities arrive slice by slice (S4a session reads, S4b
//! transcript, S5 writes, S6 permissions/forms, S7 session-scoped switches).
//! Until a capability lands, its method fails with an explicit
//! "not implemented yet" error naming the method — attaching to a V2 server is
//! never silently broken, and the failing call is visible in the log and on the
//! card.
//!
//! The shared `/api/session` create/compact calls are NOT here: both
//! generations serve them, so they live on the generation-blind adapter
//! (`OpenCodeBackend::create_session` / `compact`).

use async_trait::async_trait;

use crate::backend::SessionTranscript;
use crate::error::Result;

use super::strategy::GenerationStrategy;
use super::transport::Transport;
use super::types::{
    AgentInfo, ImageInput, ModelInfo, PermissionRequest, PromptResponse, ProviderModels, QuestionRequest,
    SessionInfo, SessionListInfo, SessionStatus,
};

/// The strategy that speaks the V2 generation.
pub(crate) struct V2Strategy;

/// The visible failure of a capability that has not landed yet: an
/// `OpenCode` error naming the method, so an attached V2 server's missing
/// surface is diagnosable from the log and the card (spec #364, S3 "errors are
/// visible, never silent").
fn not_implemented(method: &str) -> crate::error::BridgeError {
    crate::error::BridgeError::OpenCode(format!(
        "OpenCode V2 strategy: {method} is not implemented yet (spec #364 slice S4+); \
         cola is attached to a V2 server"
    ))
}

#[async_trait]
impl GenerationStrategy for V2Strategy {
    async fn list_sessions(&self, _http: &Transport) -> Result<Vec<SessionListInfo>> {
        Err(not_implemented("list_sessions"))
    }

    async fn update_session_title(&self, _http: &Transport, _session_id: &str, _title: &str) -> Result<()> {
        Err(not_implemented("update_session_title"))
    }

    #[allow(clippy::too_many_arguments)] // matches the trait's prompt axes
    async fn prompt(
        &self,
        _http: &Transport,
        _session_id: &str,
        _text: &str,
        _images: &[ImageInput],
        _model: Option<&ModelInfo>,
        _variant: Option<&str>,
        _agent: Option<&str>,
        _message_id: Option<&str>,
    ) -> Result<PromptResponse> {
        Err(not_implemented("prompt"))
    }

    #[allow(clippy::too_many_arguments)] // matches the trait's prompt axes
    async fn prompt_async(
        &self,
        _http: &Transport,
        _session_id: &str,
        _text: &str,
        _images: &[ImageInput],
        _model: Option<&ModelInfo>,
        _variant: Option<&str>,
        _agent: Option<&str>,
        _message_id: Option<&str>,
    ) -> Result<()> {
        Err(not_implemented("prompt_async"))
    }

    async fn reply_permission(
        &self,
        _http: &Transport,
        _request_id: &str,
        _reply: &str,
        _directory: Option<&str>,
    ) -> Result<()> {
        Err(not_implemented("reply_permission"))
    }

    async fn list_permissions(
        &self,
        _http: &Transport,
        _directory: Option<&str>,
    ) -> Result<Vec<PermissionRequest>> {
        Err(not_implemented("list_permissions"))
    }

    async fn list_questions(
        &self,
        _http: &Transport,
        _directory: Option<&str>,
    ) -> Result<Vec<QuestionRequest>> {
        Err(not_implemented("list_questions"))
    }

    async fn reply_question(
        &self,
        _http: &Transport,
        _request_id: &str,
        _answers: &[Vec<String>],
        _directory: Option<&str>,
    ) -> Result<()> {
        Err(not_implemented("reply_question"))
    }

    async fn reject_question(
        &self,
        _http: &Transport,
        _request_id: &str,
        _directory: Option<&str>,
    ) -> Result<()> {
        Err(not_implemented("reject_question"))
    }

    async fn transcript(&self, _http: &Transport, _session_id: &str) -> Result<SessionTranscript> {
        Err(not_implemented("transcript"))
    }

    async fn session_status(
        &self,
        _http: &Transport,
        _session_id: &str,
        _directory: Option<&str>,
    ) -> Result<Option<SessionStatus>> {
        Err(not_implemented("session_status"))
    }

    async fn model_context_window(
        &self,
        _http: &Transport,
        _provider: &str,
        _model: &str,
    ) -> Result<Option<i64>> {
        Err(not_implemented("model_context_window"))
    }

    async fn list_agents(&self, _http: &Transport) -> Vec<AgentInfo> {
        tracing::warn!("OpenCode V2 strategy: list_agents is not implemented yet (spec #364 slice S4+)");
        Vec::new()
    }

    async fn list_models(&self, _http: &Transport) -> Vec<ProviderModels> {
        tracing::warn!("OpenCode V2 strategy: list_models is not implemented yet (spec #364 slice S4+)");
        Vec::new()
    }

    async fn session_info(
        &self,
        _http: &Transport,
        _session_id: &str,
        _directory: Option<&str>,
    ) -> Result<SessionInfo> {
        Err(not_implemented("session_info"))
    }

    async fn interrupt(&self, _http: &Transport, _session_id: &str) -> Result<()> {
        Err(not_implemented("interrupt"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn every_capability_fails_loudly_until_its_slice_lands() {
        // Attaching to V2 must never pretend a missing capability worked: a
        // caller gets an error naming the method and the generation (spec
        // #364, S3).
        let strategy = V2Strategy;
        let http = Transport::new(Some("opencode"), Some("pw"), "http://127.0.0.1:1");
        let error = strategy.list_sessions(&http).await.unwrap_err().to_string();
        assert!(error.contains("V2 strategy"), "unexpected: {error}");
        assert!(error.contains("list_sessions"), "unexpected: {error}");
        assert!(error.contains("not implemented"), "unexpected: {error}");
        // The two catalog reads degrade to empty with a warning instead of an
        // error (their card surfaces tolerate emptiness).
        assert!(strategy.list_agents(&http).await.is_empty());
        assert!(strategy.list_models(&http).await.is_empty());
    }
}
