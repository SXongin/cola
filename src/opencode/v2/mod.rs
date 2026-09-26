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
//! S4a is the session-level read surface: list/get/update/delete, and the run
//! state (`session.active`) with the retry status derived from the newest
//! assistant message's `retry` field. The transcript decode proper is S4b.
//!
//! The shared `/api/session` create/compact calls are NOT here: both
//! generations serve them, so they live on the generation-blind adapter
//! (`OpenCodeBackend::create_session` / `compact`).

mod wire;

#[cfg(test)]
pub(crate) mod conformance;
#[cfg(test)]
mod tests;

use async_trait::async_trait;

use crate::backend::SessionTranscript;
use crate::error::Result;

use super::strategy::GenerationStrategy;
use super::transport::Transport;
use super::types::{
    AgentInfo, ImageInput, ModelInfo, PermissionRequest, PromptResponse, ProviderModels, QuestionRequest,
    SessionInfo, SessionListInfo, SessionStatus,
};

/// The session root: list, get, update (PATCH), delete, and the per-session
/// sub-routes. Unlike V1's unprefixed surface, every V2 route lives under
/// `/api`.
const SESSION: &str = "/api/session";
/// The active-session run-state map (`{data: Record<SessionID, {type:"running"}>}`).
const SESSION_ACTIVE: &str = "/api/session/active";
/// The per-session projected-message read; S4a uses it only for the retry
/// derivation (S4b decodes the transcript).
const SESSION_MESSAGES_SUFFIX: &str = "/message";

/// Hard stop for the body-cursor follow in [`V2Strategy::list_sessions`]: a
/// misbehaving server must not spin the client forever. The server's default
/// page is 50 rows, so 100 pages is 5k sessions — far past any real store.
const MAX_SESSION_PAGES: usize = 100;

/// The strategy that speaks the V2 generation.
pub(crate) struct V2Strategy;

/// The visible failure text of a capability that has not landed yet: names the
/// method and the generation, so an attached V2 server's missing surface is
/// diagnosable from the log and the card (spec #364, S3 "errors are visible,
/// never silent"). One message for both the `Result` methods and the two
/// catalog warns.
fn not_implemented(method: &str) -> String {
    format!(
        "OpenCode V2 strategy: {method} is not implemented yet (spec #364; a later slice); \
         cola is attached to a V2 server"
    )
}

/// [`not_implemented`] as the error the `Result` methods return.
fn not_implemented_error(method: &str) -> crate::error::BridgeError {
    crate::error::BridgeError::OpenCode(not_implemented(method))
}

#[async_trait]
impl GenerationStrategy for V2Strategy {
    /// List sessions across the shared store, most recently active first.
    ///
    /// `GET /api/session` returns `{data: Session.Info[], cursor: {previous,
    /// next}}` with an **opaque body cursor** (not V1's `x-next-cursor`
    /// header). The server generates `cursor.next` for every non-empty page, so
    /// the follow-up loop stops on the first empty page. Unlike the V1 read
    /// there is no 404 fallback: a server with no `/api/session` is not a V2
    /// server, and attach detection already decided the generation.
    ///
    /// Children stay in the returned set — the child policy belongs to each
    /// caller (ADR-0008) — and no `parentID` filter is sent.
    async fn list_sessions(&self, http: &Transport) -> Result<Vec<SessionListInfo>> {
        let mut sessions = Vec::new();
        let mut cursor: Option<String> = None;
        for _ in 0..MAX_SESSION_PAGES {
            let mut url = reqwest::Url::parse(&http.url(SESSION))?;
            if let Some(cursor) = &cursor {
                url.query_pairs_mut().append_pair("cursor", cursor);
            }
            let resp = http.client().get(url).send().await?;
            let page: wire::SessionListPage = resp.error_for_status()?.json().await?;
            let next = page.cursor.next.clone();
            let empty = page.data.is_empty();
            sessions.extend(page.data.into_iter().map(wire::RawSessionInfo::into_list_info));
            match next {
                Some(next) if !empty => cursor = Some(next),
                _ => return Ok(sessions),
            }
        }
        tracing::warn!(
            "session list: cursor.next still present after {MAX_SESSION_PAGES} pages; \
             returning the {} sessions fetched so far",
            sessions.len()
        );
        Ok(sessions)
    }

    /// Rename a session server-side (`PATCH /api/session/{id}` with
    /// `{"title": ...}`). V2 answers **204 No Content** (V1 returned the
    /// updated info), so only the status is read. The change is durable session
    /// state, visible to every client sharing the store.
    async fn update_session_title(&self, http: &Transport, session_id: &str, title: &str) -> Result<()> {
        let body = serde_json::json!({ "title": title });
        http.client()
            .patch(http.url(&format!("{SESSION}/{session_id}")))
            .json(&body)
            .send()
            .await?
            .error_for_status()?;
        Ok(())
    }

    /// Delete a session server-side (`DELETE /api/session/{id}`). V2 answers
    /// **204 No Content** and cascades to the session's child sessions.
    async fn delete_session(&self, http: &Transport, session_id: &str) -> Result<()> {
        http.client()
            .delete(http.url(&format!("{SESSION}/{session_id}")))
            .send()
            .await?
            .error_for_status()?;
        Ok(())
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
        Err(not_implemented_error("prompt"))
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
        Err(not_implemented_error("prompt_async"))
    }

    async fn reply_permission(
        &self,
        _http: &Transport,
        _request_id: &str,
        _reply: &str,
        _directory: Option<&str>,
    ) -> Result<()> {
        Err(not_implemented_error("reply_permission"))
    }

    async fn list_permissions(
        &self,
        _http: &Transport,
        _directory: Option<&str>,
    ) -> Result<Vec<PermissionRequest>> {
        Err(not_implemented_error("list_permissions"))
    }

    async fn list_questions(
        &self,
        _http: &Transport,
        _directory: Option<&str>,
    ) -> Result<Vec<QuestionRequest>> {
        Err(not_implemented_error("list_questions"))
    }

    async fn reply_question(
        &self,
        _http: &Transport,
        _request_id: &str,
        _answers: &[Vec<String>],
        _directory: Option<&str>,
    ) -> Result<()> {
        Err(not_implemented_error("reply_question"))
    }

    async fn reject_question(
        &self,
        _http: &Transport,
        _request_id: &str,
        _directory: Option<&str>,
    ) -> Result<()> {
        Err(not_implemented_error("reject_question"))
    }

    async fn transcript(&self, _http: &Transport, _session_id: &str) -> Result<SessionTranscript> {
        Err(not_implemented_error("transcript"))
    }

    /// The run state for ONE session (V2 `GET /api/session/active`), with the
    /// retry state derived from the newest assistant message's `retry` field.
    ///
    /// V2 has no retry entry: the active map only carries `{type:"running"}`,
    /// and a scheduled retry keeps the session's drain active, so a retrying
    /// session is indistinguishable from a running one by the map alone. The
    /// neutral view keeps Idle/Busy/Retry (the Session Snapshot's status line),
    /// so an ACTIVE session's newest assistant message is read and a scheduled
    /// `retry` wins over Running. That second read is paid only for active
    /// sessions — absence from the map is idle (the server's own contract)
    /// with no extra request — and a failed assistant read degrades to Running
    /// with a warning rather than failing the run state: the active map already
    /// answered the primary question, and a transient read failure must not
    /// finalize a live Turn. No transcript decode is involved (slice S4b).
    ///
    /// An active entry whose type cola does not recognise yields `Ok(None)` —
    /// never guessed — matching the V1 read; a failed active-map read is an
    /// error, as on V1. Both paths are global across locations; `directory` is
    /// ignored (V2 scopes by session id).
    async fn session_status(
        &self,
        http: &Transport,
        session_id: &str,
        _directory: Option<&str>,
    ) -> Result<Option<SessionStatus>> {
        let resp = http.client().get(http.url(SESSION_ACTIVE)).send().await?;
        let status = resp.status();
        if !status.is_success() {
            let text = resp.text().await.unwrap_or_default();
            tracing::warn!(
                "GET /api/session/active failed: {} — body: {}",
                status,
                &text[..text.len().min(500)]
            );
            return Err(crate::error::BridgeError::OpenCode(format!(
                "session status failed: {}",
                status
            )));
        }
        let text = resp.text().await?;
        let active: wire::ActiveSessions = serde_json::from_str(&text).map_err(|e| {
            crate::error::BridgeError::OpenCode(format!(
                "session status parse: {e} — body: {}",
                &text[..text.len().min(300)]
            ))
        })?;
        Ok(match active.state(session_id) {
            // Running: the retry field can only be set while the drain owns the
            // session, so this is where the second read is worth paying.
            Some(true) => match self.newest_assistant_retrying(http, session_id).await {
                Ok(true) => Some(SessionStatus::Retry),
                Ok(false) => Some(SessionStatus::Busy),
                // The active map already said the session is running; a failed
                // retry read degrades to that, with the retry-read error (and
                // its body) in the warning.
                Err(error) => {
                    tracing::warn!("session {session_id} run state: {error}; reporting running");
                    Some(SessionStatus::Busy)
                }
            },
            // Present but with an unrecognised type: never guessed.
            Some(false) => None,
            None => Some(SessionStatus::Idle),
        })
    }

    async fn model_context_window(
        &self,
        _http: &Transport,
        _provider: &str,
        _model: &str,
    ) -> Result<Option<i64>> {
        Err(not_implemented_error("model_context_window"))
    }

    async fn list_agents(&self, _http: &Transport) -> Vec<AgentInfo> {
        tracing::warn!("{}", not_implemented("list_agents"));
        Vec::new()
    }

    async fn list_models(&self, _http: &Transport) -> Vec<ProviderModels> {
        tracing::warn!("{}", not_implemented("list_models"));
        Vec::new()
    }

    /// Fetch a session's info (`GET /api/session/{id}`), unwrapping the
    /// `{data: ...}` envelope. V2 resolves the session id globally, so the
    /// handle's `directory` is not sent; the directory itself lives in
    /// `location.directory`, which the parent-chain/card consumers do not read
    /// from this DTO (the list read carries it for the cards).
    async fn session_info(
        &self,
        http: &Transport,
        session_id: &str,
        _directory: Option<&str>,
    ) -> Result<SessionInfo> {
        let resp = http
            .client()
            .get(http.url(&format!("{SESSION}/{session_id}")))
            .send()
            .await?
            .error_for_status()?;
        let body: wire::DataEnvelope<wire::RawSessionInfo> = resp.json().await?;
        Ok(body.data.into_session_info()?)
    }

    async fn interrupt(&self, _http: &Transport, _session_id: &str) -> Result<()> {
        Err(not_implemented_error("interrupt"))
    }
}

impl V2Strategy {
    /// Whether the newest assistant message of `session_id` carries a scheduled
    /// `retry`. Reads ONE projected message (`type=assistant&order=desc&
    /// limit=1`) — the minimal wire read that answers the run-state question
    /// without decoding the transcript (S4b). Failures name the retry read, so
    /// a caller can tell them apart from the active-map read.
    async fn newest_assistant_retrying(&self, http: &Transport, session_id: &str) -> Result<bool> {
        let mut url =
            reqwest::Url::parse(&http.url(&format!("{SESSION}/{session_id}{SESSION_MESSAGES_SUFFIX}")))?;
        url.query_pairs_mut()
            .append_pair("type", "assistant")
            .append_pair("order", "desc")
            .append_pair("limit", "1");
        let resp = http.client().get(url).send().await?;
        let status = resp.status();
        if !status.is_success() {
            let text = resp.text().await.unwrap_or_default();
            return Err(crate::error::BridgeError::OpenCode(format!(
                "session retry read failed: {status} — body: {}",
                &text[..text.len().min(300)]
            )));
        }
        let text = resp.text().await?;
        let page: wire::MessagesPage = serde_json::from_str(&text).map_err(|e| {
            crate::error::BridgeError::OpenCode(format!(
                "session retry read parse: {e} — body: {}",
                &text[..text.len().min(300)]
            ))
        })?;
        Ok(page.newest_assistant_retrying())
    }
}
