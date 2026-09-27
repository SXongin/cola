//! The V2 generation strategy (ADR-0055): OpenCode 2.0.x's `/api`-only surface.
//!
//! This module is where every V2 path, payload, decoder and semantic lands.
//! Attach detection (slice S3, [`super::generation`]) can already select it,
//! and its capabilities arrived slice by slice (S4a session reads, S4b
//! transcript, S5 writes, S6 permissions/forms, S7 session-scoped switches).
//! Until a capability lands its method fails with an explicit "not implemented
//! yet" error naming the method — attaching to a V2 server is never silently
//! broken, and the failing call is visible in the log and on the card.
//!
//! S4a is the session-level read surface: list/get/update/delete, and the run
//! state (`session.active`) with the retry status derived from the newest
//! assistant message's `retry` field. S4b is the transcript decode proper,
//! behind [`V2Strategy::transcript`] and the private [`wire`] module. S5 is
//! the write surface: prompt dispatch (admit + `session.wait`/poll polyfill),
//! the steer supplement, interrupt and compact. S6 is permissions and forms:
//! the location-scoped pending lists, the session-scoped decision/keyed-answer
//! replies, and the cancel-by-delete path. S7 is the session-scoped selection:
//! `/model`, `/think` and `/agent` become durable switches
//! (`POST /api/session/{id}/model|agent`, the variant inside the model ref),
//! plus the `/api/model` and `/api/agent` catalogs their cards read.
//!
//! Session creation (`POST /api/session`) is NOT here: both generations serve
//! that request identically, so it lives on the generation-blind adapter
//! (`OpenCodeBackend::create_session`). Compaction is here — the path is
//! shared, but V2's body/response contract is not.

mod wire;

#[cfg(test)]
pub(crate) mod conformance;
#[cfg(test)]
mod tests;

use async_trait::async_trait;

use crate::backend::SessionTranscript;
use crate::error::Result;

use super::strategy::GenerationStrategy;
use super::transport::{Transport, body_preview, read_failure, read_failure_detailed};
use super::types::{
    AgentInfo, FormAnswer, FormValue, ImageInput, ModelInfo, PermissionRequest, PromptResponse,
    ProviderModels, QuestionRequest, SessionInfo, SessionListInfo, SessionSelection, SessionStatus,
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
/// The durable model switch (`POST /api/session/{id}/model`, 204).
const SESSION_MODEL_SUFFIX: &str = "/model";
/// The durable agent switch (`POST /api/session/{id}/agent`, 204).
const SESSION_AGENT_SUFFIX: &str = "/agent";
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
/// The location-scoped pending-permission list (`GET
/// /api/permission/request?location[directory]=…`).
const PERMISSION_REQUEST: &str = "/api/permission/request";
/// The location-scoped pending-form list (`GET /api/form?location[directory]=…`).
const FORM: &str = "/api/form";
/// The agent catalog (`GET /api/agent`) the `/agent` picker reads.
const AGENT: &str = "/api/agent";
/// The model catalog (`GET /api/model`) the `/model` picker and the footer's
/// context-window lookup read. It serves the enabled models only, so the
/// picker needs no credential/`connected` filter like V1's `GET /provider`.
const MODEL: &str = "/api/model";

/// How long the poll fallback waits between `session.active` reads. The wait
/// endpoint resolves as soon as the drain settles; the fallback matches that
/// latency closely enough for a card that renders on its own poll anyway.
const IDLE_POLL_INTERVAL: std::time::Duration = std::time::Duration::from_millis(500);

/// How many consecutive absent reads the fallback requires before it trusts
/// "idle". A single absence cannot distinguish "the run finished before the
/// first read" from "the server has not registered the run yet": the prompt
/// admit only *schedules* execution, so absence is confirmed across a bounded
/// window ([`IDLE_CONFIRMATIONS`] × [`IDLE_POLL_INTERVAL`]) instead of being
/// believed once. v2.0.18 registers the execution synchronously inside the
/// prompt handler, but the fallback must stay correct for a server that
/// schedules asynchronously.
const IDLE_CONFIRMATIONS: usize = 3;

/// The poll fallback's overall backstop: how long it follows `session.active`
/// before giving up. This is not a turn limit — V1's blocking prompt has no
/// client deadline at all and real turns run long — so it is sized past any
/// plausible turn. On expiry the fallback stops observing with a WARN and the
/// prompt returns the turn as read so far; the Bridge's own drain and status
/// read then decide completion (a still-running turn is followed, never
/// falsely finalized).
const IDLE_POLL_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60 * 60);

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

/// The strategy that speaks the V2 generation.
pub(crate) struct V2Strategy;

/// The location-scoped list URL: V2 selects an instance/workspace with the
/// deepObject query `location[directory]=…` (V1's flat `directory=`). Without a
/// directory the server's default location answers, exactly like V1's cwd
/// instance.
fn location_url(base: &str, directory: Option<&str>) -> Result<reqwest::Url> {
    let mut url = reqwest::Url::parse(base)?;
    if let Some(directory) = directory {
        url.query_pairs_mut()
            .append_pair("location[directory]", directory);
    }
    Ok(url)
}

/// Encode one neutral form answer into V2's `Form.Value` union: a string, a
/// number, an integer, a boolean or a string array.
fn answer_json(value: &FormValue) -> serde_json::Value {
    match value {
        FormValue::Text(text) => serde_json::Value::String(text.clone()),
        FormValue::Number(number) => serde_json::json!(number),
        FormValue::Integer(integer) => serde_json::json!(integer),
        FormValue::Bool(flag) => serde_json::Value::Bool(*flag),
        FormValue::List(values) => serde_json::json!(values),
    }
}

/// Consume a failed permission/form reply into the neutral error. A 404 (the
/// request, form or session is gone) and a 409 (the form is already settled)
/// both mean "already handled" to the bridge's neutral card, so they become
/// [`crate::error::BridgeError::NotFound`]; anything else is a real failure
/// naming the operation and carrying a body preview. One mapping for both reply
/// families, so their diagnostics cannot drift.
async fn reply_failure(
    response: reqwest::Response,
    what: &str,
    request_id: &str,
) -> crate::error::BridgeError {
    let status = response.status();
    let body = response.text().await.unwrap_or_default();
    if matches!(
        status,
        reqwest::StatusCode::NOT_FOUND | reqwest::StatusCode::CONFLICT
    ) {
        return crate::error::BridgeError::NotFound(format!("{what} {request_id}"));
    }
    crate::error::BridgeError::OpenCode(format!(
        "{what} reply {request_id} failed: HTTP {status} — {}",
        body_preview(&body)
    ))
}

/// Whether a V2 error body names the session as missing (`{_tag:
/// "SessionNotFoundError", …}`). V2's 404s are tagged, so the bridge's
/// recreate-the-session heal must key on the tag — a bare 404 from a proxy (or
/// a missing route) is not a missing session and must not recreate anything.
fn is_session_not_found(body: &str) -> bool {
    serde_json::from_str::<serde_json::Value>(body)
        .is_ok_and(|value| value.get("_tag").and_then(|tag| tag.as_str()) == Some("SessionNotFoundError"))
}

/// Consume a failed write-path response into the neutral error: a tagged
/// `SessionNotFoundError` becomes the bridge's SessionNotFound (the taxonomy
/// the recreate-and-retry heal reads), anything else an OpenCode error naming
/// the operation (`"prompt"`, `"interrupt"`), the session and a body preview.
/// One mapping for every write call, so their diagnostics cannot drift.
async fn write_failure(
    response: reqwest::Response,
    session_id: &str,
    what: &str,
) -> crate::error::BridgeError {
    let status = response.status();
    let body = response.text().await.unwrap_or_default();
    if status == reqwest::StatusCode::NOT_FOUND && is_session_not_found(&body) {
        return crate::error::BridgeError::SessionNotFound(session_id.to_string());
    }
    crate::error::BridgeError::OpenCode(format!(
        "{what} {session_id} failed: HTTP {status} — {}",
        body_preview(&body)
    ))
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
    /// and durable (S7), applied through `POST /api/session/{id}/model|agent`
    /// when the user picks them. A caller-supplied override is deliberately
    /// dropped — the session's recorded selection is what runs, and re-sending
    /// it per prompt is nothing the protocol can carry.
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
                "session {session_id}: V2 uses the session's durable selection, \
                 not the per-prompt model/variant/agent"
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

    /// Reply to one pending permission request (`POST
    /// /api/session/{id}/permission/{requestID}/reply` with
    /// `{"decision": once|always|reject}`). V2 answers 204; a 404 (request or
    /// session gone) and a 409 (settled) both mean "already handled" to the
    /// bridge's neutral card, so both map to [`crate::error::BridgeError::NotFound`].
    async fn reply_permission(
        &self,
        http: &Transport,
        session_id: &str,
        request_id: &str,
        reply: &str,
        _directory: Option<&str>,
    ) -> Result<()> {
        let response = http
            .client()
            .post(http.url(&format!("{SESSION}/{session_id}/permission/{request_id}/reply")))
            .json(&serde_json::json!({ "decision": reply }))
            .send()
            .await?;
        if response.status().is_success() {
            return Ok(());
        }
        Err(reply_failure(response, "permission", request_id).await)
    }

    /// List pending permissions for one location (`GET
    /// /api/permission/request?location[directory]=…`) — one call per known
    /// session directory, exactly like the V1 read, never one per session. The
    /// response is V2's `{location, data: Permission.Request[]}` envelope whose
    /// request fields are renamed (`action`/`resources`/`save`).
    async fn list_permissions(
        &self,
        http: &Transport,
        directory: Option<&str>,
    ) -> Result<Vec<PermissionRequest>> {
        let url = location_url(&http.url(PERMISSION_REQUEST), directory)?;
        let resp = http.client().get(url).send().await?;
        if !resp.status().is_success() {
            return Err(read_failure(resp, "permission list").await);
        }
        let page: wire::DataEnvelope<Vec<wire::RawPermission>> = resp.json().await?;
        Ok(page
            .data
            .into_iter()
            .map(wire::RawPermission::into_neutral)
            .collect())
    }

    /// List pending forms for one location (`GET /api/form?location[directory]=…`).
    /// The same one-call-per-directory shape as the permission read; the visible
    /// units are V2's typed `Form.Field`s, not V1's positional questions.
    async fn list_questions(
        &self,
        http: &Transport,
        directory: Option<&str>,
    ) -> Result<Vec<QuestionRequest>> {
        let url = location_url(&http.url(FORM), directory)?;
        let resp = http.client().get(url).send().await?;
        if !resp.status().is_success() {
            return Err(read_failure(resp, "form list").await);
        }
        let page: wire::DataEnvelope<Vec<wire::RawForm>> = resp.json().await?;
        Ok(page.data.into_iter().map(wire::RawForm::into_neutral).collect())
    }

    /// Answer a pending form (`POST /api/session/{id}/form/{formID}/reply`)
    /// with V2's keyed `{"answer": {key: value}}` body. The neutral answers are
    /// rendered into the typed JSON values the form schema expects.
    async fn reply_question(
        &self,
        http: &Transport,
        session_id: &str,
        request_id: &str,
        answers: &[FormAnswer],
        _directory: Option<&str>,
    ) -> Result<()> {
        let answer: serde_json::Map<String, serde_json::Value> = answers
            .iter()
            .filter_map(|answer| {
                answer
                    .value
                    .as_ref()
                    .map(|value| (answer.key.clone(), answer_json(value)))
            })
            .collect();
        let response = http
            .client()
            .post(http.url(&format!("{SESSION}/{session_id}/form/{request_id}/reply")))
            .json(&serde_json::json!({ "answer": answer }))
            .send()
            .await?;
        if response.status().is_success() {
            return Ok(());
        }
        Err(reply_failure(response, "form", request_id).await)
    }

    /// Cancel a pending form (`DELETE /api/session/{id}/form/{formID}`) — V2's
    /// replacement for V1's `reject` (there is no reject endpoint). A 404/409
    /// is the same benign "already settled" outcome as a reply.
    async fn reject_question(
        &self,
        http: &Transport,
        session_id: &str,
        request_id: &str,
        _directory: Option<&str>,
    ) -> Result<()> {
        let response = http
            .client()
            .delete(http.url(&format!("{SESSION}/{session_id}/form/{request_id}")))
            .send()
            .await?;
        if response.status().is_success() {
            return Ok(());
        }
        Err(reply_failure(response, "form", request_id).await)
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
        if !resp.status().is_success() {
            return Err(read_failure(resp, "session status").await);
        }
        let text = resp.text().await?;
        let active: wire::ActiveSessions = serde_json::from_str(&text).map_err(|e| {
            crate::error::BridgeError::OpenCode(format!(
                "session status parse: {e} — body: {}",
                body_preview(&text)
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

    /// The session's durable selection (`GET /api/session/{id}`): the model
    /// ref (variant inside it) and agent the server will use for the next
    /// turn, visible to every client sharing the store. This is the read the
    /// effective-model ladder prefers over cola's local mirror.
    async fn session_selection(
        &self,
        http: &Transport,
        session_id: &str,
        _directory: Option<&str>,
    ) -> Result<Option<SessionSelection>> {
        Ok(Some(
            self.read_raw_session(http, session_id).await?.into_selection()?,
        ))
    }

    /// Switch the session's model (`POST /api/session/{id}/model`, 204) so
    /// subsequent turns use it with nothing re-sent. The variant rides inside
    /// the `Model.Ref`; the server short-circuits an unchanged selection.
    async fn switch_session_model(
        &self,
        http: &Transport,
        session_id: &str,
        model: &ModelInfo,
    ) -> Result<()> {
        let response = http
            .client()
            .post(http.url(&format!("{SESSION}/{session_id}{SESSION_MODEL_SUFFIX}")))
            .json(&serde_json::json!({ "model": model }))
            .send()
            .await?;
        if !response.status().is_success() {
            return Err(write_failure(response, session_id, "model switch").await);
        }
        Ok(())
    }

    /// Switch the session's agent (`POST /api/session/{id}/agent`, 204). V2
    /// has no "clear" arm — the server default is the agent the server itself
    /// would select, so a reset switches to that id.
    async fn switch_session_agent(&self, http: &Transport, session_id: &str, agent: &str) -> Result<()> {
        let response = http
            .client()
            .post(http.url(&format!("{SESSION}/{session_id}{SESSION_AGENT_SUFFIX}")))
            .json(&serde_json::json!({ "agent": agent }))
            .send()
            .await?;
        if !response.status().is_success() {
            return Err(write_failure(response, session_id, "agent switch").await);
        }
        Ok(())
    }

    /// V2 keeps model/agent choices as durable session state.
    fn keeps_session_selection(&self) -> bool {
        true
    }

    /// The model's context-window size (tokens), from `GET /api/model`. Best
    /// effort like V1's provider read: any failure returns Ok(None) so the
    /// footer just omits the ratio.
    async fn model_context_window(
        &self,
        http: &Transport,
        provider: &str,
        model: &str,
    ) -> Result<Option<i64>> {
        let Ok(resp) = http.client().get(http.url(MODEL)).send().await else {
            return Ok(None);
        };
        if !resp.status().is_success() {
            return Ok(None);
        }
        let Ok(page) = resp.json::<wire::DataEnvelope<Vec<wire::RawModelInfo>>>().await else {
            return Ok(None);
        };
        Ok(page
            .data
            .into_iter()
            .find(|entry| entry.provider_id == provider && entry.id == model)
            .and_then(|entry| entry.limit.and_then(|limit| limit.context)))
    }

    /// Available agents (`GET /api/agent`), each with its selectable identity.
    /// Best-effort: an unreadable failure returns an empty list so the
    /// `/agent` card can degrade to a plain text prompt.
    async fn list_agents(&self, http: &Transport) -> Vec<AgentInfo> {
        let Ok(resp) = http.client().get(http.url(AGENT)).send().await else {
            return Vec::new();
        };
        let Ok(page) = resp.json::<wire::DataEnvelope<Vec<wire::RawAgentInfo>>>().await else {
            return Vec::new();
        };
        page.data
            .into_iter()
            .map(wire::RawAgentInfo::into_neutral)
            .collect()
    }

    /// Available models (`GET /api/model`), grouped as `provider → models`,
    /// each with its declared variants. The endpoint serves the enabled set
    /// already, so no `connected` filter is needed. Best-effort: an unreadable
    /// failure returns an empty list so the `/model` card can degrade to a
    /// plain text prompt.
    async fn list_models(&self, http: &Transport) -> Vec<ProviderModels> {
        let Ok(resp) = http.client().get(http.url(MODEL)).send().await else {
            return Vec::new();
        };
        let Ok(page) = resp.json::<wire::DataEnvelope<Vec<wire::RawModelInfo>>>().await else {
            return Vec::new();
        };
        let mut grouped: Vec<ProviderModels> = Vec::new();
        for model in page.data {
            match grouped
                .iter_mut()
                .find(|group| group.provider == model.provider_id)
            {
                Some(group) => group.models.push(model.into_option()),
                None => grouped.push(ProviderModels {
                    provider: model.provider_id.clone(),
                    models: vec![model.into_option()],
                }),
            }
        }
        grouped
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
        Ok(self
            .read_raw_session(http, session_id)
            .await?
            .into_session_info()?)
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
        if !response.status().is_success() {
            return Err(write_failure(response, session_id, "interrupt").await);
        }
        Ok(())
    }

    /// Compact the session's context (`POST /api/session/{id}/compact`). V2
    /// declares an all-optional payload, so an absent body fails the server's
    /// payload decode and `{}` is the empty request; the `{data}` response is
    /// ignored (any 2xx is success, as V1's 204 is). Failures go through the
    /// same mapping as the other write calls: the tagged 404 the endpoint
    /// declares is session-not-found, and an untagged proxy 404 is not — a raw
    /// `error_for_status()` would leak it as an Http 404, which
    /// `is_session_not_found()` reads as a missing session regardless of tag.
    async fn compact(&self, http: &Transport, session_id: &str) -> Result<()> {
        let response = http
            .client()
            .post(http.url(&format!("{SESSION}/{session_id}/compact")))
            .json(&serde_json::json!({}))
            .send()
            .await?;
        if !response.status().is_success() {
            return Err(write_failure(response, session_id, "compact").await);
        }
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
        if !response.status().is_success() {
            return Err(write_failure(response, session_id, "prompt").await);
        }
        let text_body = response.text().await?;
        let admitted: wire::DataEnvelope<wire::RawAdmittedPrompt> = serde_json::from_str(&text_body)
            .map_err(|error| {
                crate::error::BridgeError::OpenCode(format!(
                    "prompt admit decode: {error} — body: {}",
                    body_preview(&text_body)
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
                self.poll_until_idle(http, session_id, IDLE_POLL_TIMEOUT).await
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
            body_preview(&text)
        )))
    }

    /// Poll `GET /api/session/active` until the session has been absent for
    /// [`IDLE_CONFIRMATIONS`] consecutive reads — the same event the `wait`
    /// endpoint resolves on (an execution owns the session until its last
    /// successor settles). An entry with a type this build does not recognise
    /// still counts as active: only confirmed absence means idle, never a guess.
    ///
    /// Absence is confirmed rather than believed once: the prompt admit only
    /// schedules execution, so a first read can precede the run's registration
    /// on a server that schedules asynchronously (v2.0.18 registers
    /// synchronously, but the fallback must not depend on that), and believing
    /// it would return a live turn as an empty reply.
    ///
    /// `timeout` bounds the whole fallback (see [`IDLE_POLL_TIMEOUT`] for the
    /// production value): when it passes, the loop warns and stops observing —
    /// the caller returns the turn as read so far and the Bridge's own
    /// drain/status read decides completion instead of this call hanging the
    /// Turn forever. Returns `Err` only when a read itself fails; a garbled
    /// active map is an error, never a silent idle.
    async fn poll_until_idle(
        &self,
        http: &Transport,
        session_id: &str,
        timeout: std::time::Duration,
    ) -> Result<()> {
        let deadline = tokio::time::Instant::now() + timeout;
        let mut absent_reads = 0usize;
        // What the LAST read saw, so the expiry WARN describes the state the
        // loop actually gave up on: an active session, or an absence it could
        // not confirm inside the window. Assigned by every loop iteration
        // before its first read.
        let mut last_read_active;
        loop {
            let response = http.client().get(http.url(SESSION_ACTIVE)).send().await?;
            if !response.status().is_success() {
                // This error leaves `prompt` as the surfaced diagnostic, so it
                // keeps the body preview the other status reads only log.
                return Err(read_failure_detailed(response, "session status").await);
            }
            let text = response.text().await?;
            let active: wire::ActiveSessions = serde_json::from_str(&text).map_err(|error| {
                crate::error::BridgeError::OpenCode(format!(
                    "session status parse: {error} — body: {}",
                    body_preview(&text)
                ))
            })?;
            last_read_active = active.state(session_id).is_some();
            if last_read_active {
                absent_reads = 0;
            } else {
                absent_reads += 1;
                if absent_reads >= IDLE_CONFIRMATIONS {
                    return Ok(());
                }
            }
            if tokio::time::Instant::now() >= deadline {
                if last_read_active {
                    tracing::warn!(
                        "session {session_id}: still active after {}s of session.active polling; \
                         returning to the caller instead of polling forever",
                        timeout.as_secs()
                    );
                } else {
                    tracing::warn!(
                        "session {session_id}: idle not confirmed after {}s of session.active \
                         polling (the last read was absent, but the confirmation window did not \
                         complete); returning to the caller instead of polling forever",
                        timeout.as_secs()
                    );
                }
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

    /// Read the session's durable selection (`GET /api/session/{id}`, the same
    /// envelope `session_info` unwraps), decoding the `Model.Ref` with its
    /// variant and the selected agent. A failed read names itself and carries a
    /// body preview, like the module's other reads; the ladder treats a
    /// failure as "unknown" and never falls back to a client-side mirror.
    async fn read_raw_session(&self, http: &Transport, session_id: &str) -> Result<wire::RawSessionInfo> {
        let resp = http
            .client()
            .get(http.url(&format!("{SESSION}/{session_id}")))
            .send()
            .await?;
        if !resp.status().is_success() {
            return Err(read_failure(resp, "session read").await);
        }
        let body: wire::DataEnvelope<wire::RawSessionInfo> = resp.json().await?;
        Ok(body.data)
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
                    body_preview(&text)
                )));
            }
            let page: wire::MessagesPage = serde_json::from_str(&text).map_err(|e| {
                crate::error::BridgeError::OpenCode(format!(
                    "transcript read parse: {e} — body: {}",
                    body_preview(&text)
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
                body_preview(&text)
            )));
        }
        let text = resp.text().await?;
        let page: wire::MessagesPage = serde_json::from_str(&text).map_err(|e| {
            crate::error::BridgeError::OpenCode(format!(
                "session retry read parse: {e} — body: {}",
                body_preview(&text)
            ))
        })?;
        Ok(page.newest_assistant_retrying())
    }
}
