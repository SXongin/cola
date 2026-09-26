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
//! assistant message's `retry` field. S4b is the transcript decode proper,
//! behind [`V2Strategy::transcript`] and the private [`wire`] module.
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
/// The durable prompt admit (`POST /api/session/{id}/prompt`) — V2's only
/// prompt route, shared by the blocking dispatch and the supplement/steer path.
const SESSION_PROMPT_SUFFIX: &str = "/prompt";
/// The interrupt endpoint (`POST /api/session/{id}/interrupt`).
const SESSION_INTERRUPT_SUFFIX: &str = "/interrupt";
/// The experimental "wait for the agent loop to become idle" endpoint
/// (`POST /api/experimental/session/{id}/wait`, 204) — ADR-0056's recorded
/// dependency, deleted by the async-native Turn slice (S8). The poll fallback
/// in [`V2Strategy::wait_until_idle`] is its mandatory degradation path.
const SESSION_WAIT: &str = "/api/experimental/session";
/// The active-session run-state map (`{data: Record<SessionID, {type:"running"}>}`).
const SESSION_ACTIVE: &str = "/api/session/active";
/// The per-session projected-message read: the S4b transcript decode and, for
/// active sessions, the run state's retry derivation.
const SESSION_MESSAGES_SUFFIX: &str = "/message";

/// How long the poll fallback waits between `session.active` reads. The wait
/// endpoint resolves as soon as the drain settles; the fallback matches that
/// latency closely enough for a card that renders on its own poll anyway.
const IDLE_POLL_INTERVAL: std::time::Duration = std::time::Duration::from_millis(250);

/// Hard stop for the body-cursor follow in [`V2Strategy::list_sessions`]: a
/// misbehaving server must not spin the client forever. The server's default
/// page is 50 rows, so 100 pages is 5k sessions — far past any real store.
const MAX_SESSION_PAGES: usize = 100;

/// Hard stop for the body-cursor follow in [`V2Strategy::transcript`], for the
/// same reason: 100 pages of the endpoint's 200-row maximum is 20k messages,
/// far past any real session.
const MAX_MESSAGE_PAGES: usize = 100;

/// The page size the transcript read asks for — the endpoint's documented
/// maximum. The whole history is re-read every render poll, so the largest
/// page is the difference between one request and many on a long session.
const MESSAGE_PAGE_LIMIT: &str = "200";

/// How much of a failed response body a status diagnostic carries. One cap for
/// every warn and error in this module, so no two diagnostics of the same read
/// truncate differently; a body is a debug aid, not data.
const BODY_PREVIEW_CHARS: usize = 500;

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

/// Whether a V2 error body names the session as missing (`{_tag:
/// "SessionNotFoundError", …}`). V2's 404s are tagged, so the bridge's
/// recreate-the-session heal must key on the tag — a bare 404 from a proxy (or
/// a missing route) is not a missing session and must not recreate anything.
fn is_session_not_found(body: &str) -> bool {
    serde_json::from_str::<serde_json::Value>(body)
        .is_ok_and(|value| value.get("_tag").and_then(|tag| tag.as_str()) == Some("SessionNotFoundError"))
}

/// The response for an admitted prompt whose reply could not be assembled
/// (transcript read failed, or the admitted message is not in the read): the
/// neutral response with no parts and no error, so a completed turn is never
/// painted as failed by a read hiccup. `wait` already observed the run settle,
/// and the Bridge's own final read is the primary render source.
fn admitted_response(session_id: &str, admitted_id: &str) -> PromptResponse {
    PromptResponse {
        id: admitted_id.to_string(),
        session_id: Some(session_id.to_string()),
        admitted_seq: None,
        parent_id: Some(admitted_id.to_string()),
        error: None,
        parts: Vec::new(),
    }
}

/// Why [`V2Strategy::wait_for_idle`] could not observe the idle transition:
/// the session is gone (surface it — the bridge recreates the mapping), or the
/// wait endpoint is unusable (fall back to polling `session.active`).
enum WaitFailure {
    SessionNotFound,
    Unavailable(String),
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

    /// The blocking prompt contract, polyfilled for V2 (ADR-0056).
    ///
    /// V2's prompt is admit-then-return: `POST /api/session/{id}/prompt`
    /// durably admits the message (under cola's `msg_cola_…` id, which the
    /// server persists and reconciles a retry onto) and returns immediately.
    /// The synchronous shape is then reconstructed in two steps: wait for the
    /// agent loop to become idle through the experimental `session.wait`
    /// endpoint (with the `session.active` poll fallback), then read the
    /// transcript the turn produced through the same V2 decode the read
    /// strategy uses.
    ///
    /// V2 has no per-prompt `model`/`variant`/`agent`: those are session-scoped
    /// and durable, and slice S7 lands them. The session's recorded model
    /// applies (new sessions record the configured default at create time); a
    /// caller-supplied override is dropped, never silently sent in a shape the
    /// protocol cannot carry.
    #[allow(clippy::too_many_arguments)] // matches the trait's prompt axes
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
    ) -> Result<PromptResponse> {
        if model.is_some() || variant.is_some() || agent.is_some() {
            tracing::debug!(
                "session {session_id}: V2 ignores per-prompt model/variant/agent \
                 (session-scoped semantics land in S7)"
            );
        }
        let admitted = self
            .dispatch(http, session_id, text, images, message_id, None)
            .await?;
        self.wait_until_idle(http, session_id).await?;
        self.completed_turn(http, session_id, &admitted).await
    }

    /// Fire-and-forget prompt: the supplement path. V2 admits the message with
    /// `delivery: "steer"`, so a message sent while a turn is in flight merges
    /// into the running turn at its next step boundary — the same behaviour as
    /// V1's `prompt_async` (ADR-0043) — and returns as soon as the admit is
    /// durable. No wait is taken: the caller (the bridge) keeps its card live
    /// through the render poll.
    #[allow(clippy::too_many_arguments)] // matches the trait's prompt axes
    async fn prompt_async(
        &self,
        http: &Transport,
        session_id: &str,
        text: &str,
        images: &[ImageInput],
        _model: Option<&ModelInfo>,
        _variant: Option<&str>,
        _agent: Option<&str>,
        message_id: Option<&str>,
    ) -> Result<()> {
        self.dispatch(http, session_id, text, images, message_id, Some("steer"))
            .await?;
        tracing::info!("prompt_async sent to session {session_id} (V2 delivery=steer)");
        Ok(())
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

    /// Fetch one session's Session Transcript through the V2 projected-message
    /// read (`GET /api/session/{id}/message`), decoded by [`wire`].
    ///
    /// The read pages over `cursor.next` in `order=asc` (oldest first, the
    /// neutral transcript's order), with the largest page the endpoint allows.
    /// V2's cursor is opaque and **cannot be combined with `order`**, so the
    /// first request sends `order=asc&limit` and each follow-up only
    /// `cursor&limit` — the cursor carries the order it was minted with. The
    /// server emits `cursor.next` for every non-empty page (even the last one),
    /// so the loop stops on the first empty page; a failed read names itself
    /// and carries a body preview, like the module's other reads.
    async fn transcript(&self, http: &Transport, session_id: &str) -> Result<SessionTranscript> {
        Ok(wire::decode_messages(
            &self.read_messages(http, session_id).await?,
        ))
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
    /// finalize a live Turn. The read is minimal (`type=assistant&order=desc&
    /// limit=1`), so it never decodes the transcript.
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
                &text[..text.len().min(BODY_PREVIEW_CHARS)]
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
                &text[..text.len().min(BODY_PREVIEW_CHARS)]
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

    /// Interrupt the session's active execution (`POST
    /// /api/session/{id}/interrupt`). The 200 `{interrupted: bool}` body is
    /// deliberately ignored: `false` is the idle no-op (nothing to interrupt),
    /// which is a success, not a failure.
    async fn interrupt(&self, http: &Transport, session_id: &str) -> Result<()> {
        let response = http
            .client()
            .post(http.url(&format!("{SESSION}/{session_id}{SESSION_INTERRUPT_SUFFIX}")))
            .send()
            .await?;
        let status = response.status();
        if status.is_success() {
            return Ok(());
        }
        let text = response.text().await.unwrap_or_default();
        if status == reqwest::StatusCode::NOT_FOUND && is_session_not_found(&text) {
            return Err(crate::error::BridgeError::SessionNotFound(session_id.to_string()));
        }
        Err(crate::error::BridgeError::OpenCode(format!(
            "interrupt {session_id} failed: HTTP {status} — {}",
            &text[..text.len().min(BODY_PREVIEW_CHARS)]
        )))
    }

    /// Compact the session's context (`POST /api/session/{id}/compact`). V2
    /// declares an all-optional payload, so an absent body fails the server's
    /// payload decode and `{}` is the empty request; the `{data}` response is
    /// ignored (any 2xx is success, as V1's 204 is).
    async fn compact(&self, http: &Transport, session_id: &str) -> Result<()> {
        http.client()
            .post(http.url(&format!("{SESSION}/{session_id}/compact")))
            .json(&serde_json::json!({}))
            .send()
            .await?
            .error_for_status()?;
        Ok(())
    }
}

impl V2Strategy {
    /// Durably admit one prompt (`POST /api/session/{id}/prompt`). `delivery`
    /// is `Some("steer")` for the supplement path and `None` for the main
    /// dispatch (the server default is steer as well). Returns the admitted
    /// user-message id: the caller's `message_id` when set, else the server's.
    ///
    /// A 404 is mapped to [`crate::error::BridgeError::SessionNotFound`] only
    /// when its `_tag` says so — the bridge's recreate-and-retry heal reads
    /// that taxonomy, and an untagged proxy 404 must never trigger it.
    async fn dispatch(
        &self,
        http: &Transport,
        session_id: &str,
        text: &str,
        images: &[ImageInput],
        message_id: Option<&str>,
        delivery: Option<&str>,
    ) -> Result<String> {
        let mut body = serde_json::json!({ "text": text });
        if !images.is_empty() {
            body["files"] = serde_json::Value::Array(
                images
                    .iter()
                    .map(|image| {
                        // V2 attachments take a URI; a data URL is the inline
                        // form the server decodes itself (V1's `file` part
                        // carried the same URL).
                        serde_json::json!({
                            "uri": format!("data:{};base64,{}", image.mime, image.data_base64)
                        })
                    })
                    .collect(),
            );
        }
        if let Some(message_id) = message_id {
            body["id"] = serde_json::json!(message_id);
        }
        if let Some(delivery) = delivery {
            body["delivery"] = serde_json::json!(delivery);
        }
        let response = http
            .client()
            .post(http.url(&format!("{SESSION}/{session_id}{SESSION_PROMPT_SUFFIX}")))
            .json(&body)
            .send()
            .await?;
        let status = response.status();
        let text_body = response.text().await?;
        if !status.is_success() {
            if status == reqwest::StatusCode::NOT_FOUND && is_session_not_found(&text_body) {
                return Err(crate::error::BridgeError::SessionNotFound(session_id.to_string()));
            }
            return Err(crate::error::BridgeError::OpenCode(format!(
                "prompt {session_id} failed: HTTP {status} — {}",
                &text_body[..text_body.len().min(BODY_PREVIEW_CHARS)]
            )));
        }
        let admitted: wire::DataEnvelope<wire::RawAdmittedPrompt> = serde_json::from_str(&text_body)
            .map_err(|error| {
                crate::error::BridgeError::OpenCode(format!(
                    "prompt admit decode: {error} — body: {}",
                    &text_body[..text_body.len().min(BODY_PREVIEW_CHARS)]
                ))
            })?;
        Ok(admitted.data.id)
    }

    /// Wait until the session's agent loop is idle: the experimental `wait`
    /// endpoint first (ADR-0056), the `session.active` poll as its mandatory
    /// degradation path when the route is absent or failing. A missing session
    /// is surfaced either way.
    async fn wait_until_idle(&self, http: &Transport, session_id: &str) -> Result<()> {
        match self.wait_for_idle(http, session_id).await {
            Ok(()) => Ok(()),
            Err(WaitFailure::SessionNotFound) => {
                Err(crate::error::BridgeError::SessionNotFound(session_id.to_string()))
            }
            Err(WaitFailure::Unavailable(reason)) => {
                tracing::warn!(
                    "session {session_id}: wait endpoint unavailable ({reason}); \
                     falling back to session.active polling"
                );
                self.poll_until_idle(http, session_id).await
            }
        }
    }

    /// One `POST /api/experimental/session/{id}/wait` (204 = the agent loop is
    /// idle). A 404 is only "the session is gone" when its `_tag` says so;
    /// every other failure (an absent route, a 503 during migration, a
    /// transport error) degrades to the poll fallback.
    async fn wait_for_idle(
        &self,
        http: &Transport,
        session_id: &str,
    ) -> std::result::Result<(), WaitFailure> {
        let response = http
            .client()
            .post(http.url(&format!("{SESSION_WAIT}/{session_id}/wait")))
            .send()
            .await
            .map_err(|error| WaitFailure::Unavailable(format!("request failed: {error}")))?;
        let status = response.status();
        if status.is_success() {
            return Ok(());
        }
        let text = response.text().await.unwrap_or_default();
        if status == reqwest::StatusCode::NOT_FOUND && is_session_not_found(&text) {
            return Err(WaitFailure::SessionNotFound);
        }
        Err(WaitFailure::Unavailable(format!(
            "HTTP {status} — {}",
            &text[..text.len().min(BODY_PREVIEW_CHARS)]
        )))
    }

    /// Poll `GET /api/session/active` until the session is absent — the same
    /// event the `wait` endpoint resolves on (an execution owns the session
    /// until its last successor settles). An entry with a type this build does
    /// not recognise still counts as active: only absence means idle, never a
    /// guess.
    async fn poll_until_idle(&self, http: &Transport, session_id: &str) -> Result<()> {
        loop {
            let response = http.client().get(http.url(SESSION_ACTIVE)).send().await?;
            let status = response.status();
            let text = response.text().await?;
            if !status.is_success() {
                return Err(crate::error::BridgeError::OpenCode(format!(
                    "session status failed: {status} — body: {}",
                    &text[..text.len().min(BODY_PREVIEW_CHARS)]
                )));
            }
            let active: wire::ActiveSessions = serde_json::from_str(&text).map_err(|error| {
                crate::error::BridgeError::OpenCode(format!(
                    "session status parse: {error} — body: {}",
                    &text[..text.len().min(BODY_PREVIEW_CHARS)]
                ))
            })?;
            if active.state(session_id).is_none() {
                return Ok(());
            }
            tokio::time::sleep(IDLE_POLL_INTERVAL).await;
        }
    }

    /// The prompt response for a turn that has settled: read the transcript,
    /// anchor it at the admitted user message, and return the turn's assistant
    /// parts plus any failure the turn recorded — the neutral contract V1's
    /// blocking prompt answers inline.
    ///
    /// A read failure here is a downgrade, never a failed turn: `wait` already
    /// observed the drain settle and the Bridge's own final read is the primary
    /// render source, so an empty Ok response lets the turn complete there
    /// instead of painting a finished turn as an error.
    async fn completed_turn(
        &self,
        http: &Transport,
        session_id: &str,
        admitted_id: &str,
    ) -> Result<PromptResponse> {
        let raw = match self.read_messages(http, session_id).await {
            Ok(raw) => raw,
            Err(error) => {
                tracing::warn!(
                    "prompt {session_id}: transcript read failed after the wait ({error}); \
                     returning the admitted id only"
                );
                return Ok(admitted_response(session_id, admitted_id));
            }
        };
        let transcript = wire::decode_messages(&raw);
        let anchor = transcript
            .messages
            .iter()
            .find(|message| message.id.as_str() == admitted_id)
            .and_then(|message| message.anchor());
        let Some(anchor) = anchor else {
            tracing::warn!(
                "prompt {session_id}: admitted message {admitted_id} is not in the transcript; \
                 returning the admitted id only"
            );
            return Ok(admitted_response(session_id, admitted_id));
        };
        let turn = transcript.turn_for_user(&anchor);
        // The answer is the turn's newest assistant message; without one the
        // admitted id is the only identity the response can carry.
        let id = turn
            .messages
            .last()
            .map(|message| message.id.as_str())
            .unwrap_or(admitted_id)
            .to_string();
        Ok(PromptResponse {
            id,
            session_id: Some(session_id.to_string()),
            admitted_seq: None,
            // V1's `parent_id` is the user message the returned message
            // answers; here the admitted message itself.
            parent_id: Some(admitted_id.to_string()),
            error: wire::turn_error(&raw, anchor.created_ms),
            parts: turn
                .messages
                .iter()
                .flat_map(|message| message.parts.iter().cloned())
                .collect(),
        })
    }

    /// Read the session's projected messages, decoded-ready: the raw `data`
    /// arrays a `GET /api/session/{id}/message` read carries, in server order.
    /// Shared by the transcript decode and the prompt response's turn read.
    async fn read_messages(&self, http: &Transport, session_id: &str) -> Result<Vec<serde_json::Value>> {
        let mut data: Vec<serde_json::Value> = Vec::new();
        let mut cursor: Option<String> = None;
        let url = http.url(&format!("{SESSION}/{session_id}{SESSION_MESSAGES_SUFFIX}"));
        for _ in 0..MAX_MESSAGE_PAGES {
            let mut request = reqwest::Url::parse(&url)?;
            {
                let mut query = request.query_pairs_mut();
                if let Some(cursor) = &cursor {
                    query.append_pair("cursor", cursor);
                } else {
                    query.append_pair("order", "asc");
                }
                query.append_pair("limit", MESSAGE_PAGE_LIMIT);
            }
            let resp = http.client().get(request).send().await?;
            let status = resp.status();
            let text = resp.text().await?;
            if !status.is_success() {
                return Err(crate::error::BridgeError::OpenCode(format!(
                    "transcript read failed: {status} — body: {}",
                    &text[..text.len().min(BODY_PREVIEW_CHARS)]
                )));
            }
            let page: wire::MessagesPage = serde_json::from_str(&text).map_err(|e| {
                crate::error::BridgeError::OpenCode(format!(
                    "transcript read parse: {e} — body: {}",
                    &text[..text.len().min(BODY_PREVIEW_CHARS)]
                ))
            })?;
            let empty = page.data.is_empty();
            data.extend(page.data);
            match page.cursor.next {
                Some(next) if !empty => cursor = Some(next),
                _ => return Ok(data),
            }
        }
        tracing::warn!(
            "transcript {session_id}: cursor.next still present after {MAX_MESSAGE_PAGES} pages; \
             returning the {} messages fetched so far",
            data.len()
        );
        Ok(data)
    }

    /// Whether the newest assistant message of `session_id` carries a scheduled
    /// `retry`. Reads ONE projected message (`type=assistant&order=desc&
    /// limit=1`) — the minimal wire read that answers the run-state question
    /// without decoding the whole transcript. Failures name the retry read, so
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
                &text[..text.len().min(BODY_PREVIEW_CHARS)]
            )));
        }
        let text = resp.text().await?;
        let page: wire::MessagesPage = serde_json::from_str(&text).map_err(|e| {
            crate::error::BridgeError::OpenCode(format!(
                "session retry read parse: {e} — body: {}",
                &text[..text.len().min(BODY_PREVIEW_CHARS)]
            ))
        })?;
        Ok(page.newest_assistant_retrying())
    }
}
