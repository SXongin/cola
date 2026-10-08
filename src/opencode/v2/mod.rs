//! The V2 generation strategy (ADR-0055): OpenCode 2.0.x's `/api`-only surface.
//!
//! This module is where every V2 path, payload, decoder and semantic lives.
//! Attach detection (slice S3, [`super::generation`]) selects it for a `/api`
//! server, and the capabilities landed slice by slice (S4a session reads, S4b
//! transcript, S5 writes, S6 permissions/forms, S7 session-scoped switches).
//! Every strategy method is implemented here — no "not implemented yet" arm
//! remains.
//!
//! S4a is the session-level read surface: list/get/update/delete, and the run
//! state (`session.active`) with the retry status derived from the newest
//! assistant message's `retry` field. S4b is the transcript decode proper,
//! behind [`V2Strategy::transcript`] and the private [`wire`] module. S5 is
//! the write surface: the native admit-then-return prompt, interrupt and
//! compact. S8 removed the temporary synchronous polyfill (the experimental
//! `session.wait` endpoint plus its `session.active` poll fallback, ADR-0056):
//! the Turn submits through the native prompt here and observes the turn's
//! completion from the transcript + run state. S6 is permissions and forms:
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

mod progress;
mod wire;

#[cfg(test)]
pub(crate) mod conformance;
#[cfg(test)]
mod tests;

use async_trait::async_trait;

use crate::backend::{ChildEvidence, ChildRuntime, SessionTranscript, ShellRuntime, TaskRuntime};
use crate::error::Result;

use super::strategy::GenerationStrategy;
use super::transport::{Transport, body_preview, read_failure, read_failure_quiet};
use super::types::{
    AgentInfo, FormAnswer, FormValue, ImageInput, ModelInfo, PermissionRequest, ProviderModels,
    QuestionRequest, SessionInfo, SessionListInfo, SessionSelection, SessionStatus,
};

/// The session root: list, get, update (PATCH), delete, and the per-session
/// sub-routes. Unlike V1's unprefixed surface, every V2 route lives under
/// `/api`.
const SESSION: &str = "/api/session";
/// The durable prompt admit (`POST /api/session/{id}/prompt`) — V2's native
/// admit-then-return prompt, used for both the main dispatch and a mid-turn
/// supplement (the server's default delivery is `steer`).
const SESSION_PROMPT_SUFFIX: &str = "/prompt";
/// The delivery mode every prompt carries: `steer` merges a mid-turn
/// supplement at the next step boundary while the session is busy, and starts
/// the run when it is idle. Single-sourced so the live retry-id chain's raw
/// same-id re-post speaks the exact body this strategy sends.
pub(crate) const PROMPT_DELIVERY: &str = "steer";
/// The interrupt endpoint (`POST /api/session/{id}/interrupt`).
const SESSION_INTERRUPT_SUFFIX: &str = "/interrupt";
/// The resume endpoint (`POST /api/session/{id}/resume`, 204): promotes a
/// message queued in the session inbox at a new run's start.
const SESSION_RESUME_SUFFIX: &str = "/resume";
/// The durable model switch (`POST /api/session/{id}/model`, 204).
const SESSION_MODEL_SUFFIX: &str = "/model";
/// The durable agent switch (`POST /api/session/{id}/agent`, 204).
const SESSION_AGENT_SUFFIX: &str = "/agent";
/// The active-session run-state map (`{data: Record<SessionID, {type:"running"}>}`).
const SESSION_ACTIVE: &str = "/api/session/active";
/// The location-scoped shell registry: `GET` lists the running shells, and
/// `GET /api/shell/{id}` reads one retained shell (running or terminated).
/// The Background Task runtime reconciliation (issue #454) is its one caller.
const SHELL: &str = "/api/shell";
/// The probe cursor of a shell output read (spec #588, #592): a sentinel past
/// any real capture, so the first page answers the record's total size with an
/// empty body. The server's own tail idiom uses `Number.MAX_SAFE_INTEGER`;
/// keeping that value avoids a server-side integer overflow.
const SHELL_OUTPUT_PROBE_CURSOR: u64 = (1 << 53) - 1;
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
pub(crate) struct V2Strategy {
    /// V2's live tool progress, fed by the server's event stream and overlaid
    /// onto running tool parts at transcript decode (issue #470): the message
    /// record carries no progress while a call runs, so this is where a live
    /// `subagent`'s child session id comes from.
    progress: progress::Progress,
}

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

/// One session's projected-message page URL: the session's message route plus
/// exactly `query`, so each minimal read's own wire shape stays its own
/// (`message_page` and the child-evidence read share the transport half only).
fn message_page_url(http: &Transport, session_id: &str, query: &[(&str, &str)]) -> Result<reqwest::Url> {
    let mut url =
        reqwest::Url::parse(&http.url(&format!("{SESSION}/{session_id}{SESSION_MESSAGES_SUFFIX}")))?;
    {
        let mut pairs = url.query_pairs_mut();
        for (key, value) in query {
            pairs.append_pair(key, value);
        }
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

    /// Submit a prompt through V2's native admit-then-return route.
    ///
    /// `POST /api/session/{id}/prompt` durably admits the message (under
    /// cola's `msg_cola_…` id, which the server persists and reconciles a
    /// retry onto) and returns immediately — there is no synchronous V2
    /// prompt. The Turn observes the submitted turn's completion from the
    /// transcript + run state (ADR-0056's submit+observe end state).
    ///
    /// Delivery is explicit `steer` (also the server's current default), kept
    /// on the wire because the same submit serves the main dispatch and a
    /// mid-turn Supplement: when the session is idle it starts the run, and
    /// while a turn is in flight it merges in at the next step boundary rather
    /// than queueing behind it — the merge semantics cola wants either way.
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
    ) -> Result<()> {
        if model.is_some() || variant.is_some() || agent.is_some() {
            tracing::debug!(
                "session {session_id}: V2 uses the session's durable selection, \
                 not the per-prompt model/variant/agent"
            );
        }
        let admitted = self.dispatch(http, session_id, text, images, message_id).await?;
        tracing::info!("prompt sent to session {session_id} (V2 admitted {admitted})");
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
            return Err(read_failure_quiet(resp, "permission list").await);
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
            return Err(read_failure_quiet(resp, "form list").await);
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
        // The request read is the source of truth; the event stream only fills
        // RUNNING tool parts' ephemeral progress (issue #470). Start the reader
        // lazily: the first transcript read is where the live transport exists.
        self.progress.ensure_started(http);
        let (data, truncated) = self.read_messages(http, session_id).await?;
        let mut transcript = wire::decode_messages(&data);
        transcript.truncated = truncated;
        self.progress.apply(session_id, &mut transcript);
        Ok(transcript)
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
        let active = self.active_sessions(http, "session status").await?;
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

    /// The runtime verdict for the named Background Tasks (issue #454).
    ///
    /// One location-scoped `GET /api/shell` carries every currently running
    /// shell of the session's directory; each shell it does not list is asked
    /// for individually — a retained terminal record becomes its `Ended`
    /// verdict, a 404 the runtime's own `Missing` (the record is gone: the
    /// serving process restarted, or it was removed), and any other failure
    /// leaves that shell without a verdict, never guessed. The active map
    /// (`GET /api/session/active`) answers the subagent children: a child
    /// absent from it is `Inactive`, a child present with an unrecognised type
    /// gets no verdict. Both maps are process-local to the attached server —
    /// absence can mean "a different process hosted it", which is exactly why
    /// only a positive verdict ever retires a task.
    async fn task_runtime(
        &self,
        http: &Transport,
        session_id: &str,
        directory: Option<&str>,
        shells: &[String],
        children: &[String],
    ) -> Result<TaskRuntime> {
        let mut runtime = TaskRuntime::default();
        if !shells.is_empty() {
            let resp = http
                .client()
                .get(location_url(&http.url(SHELL), directory)?)
                .send()
                .await?;
            if !resp.status().is_success() {
                return Err(read_failure(resp, "shell list").await);
            }
            let text = resp.text().await?;
            let list: wire::ShellList = serde_json::from_str(&text).map_err(|e| {
                crate::error::BridgeError::OpenCode(format!(
                    "shell list parse: {e} — body: {}",
                    body_preview(&text)
                ))
            })?;
            // The list is running-only, so membership is the live verdict; a
            // non-running entry (a race, or a future server that lists more)
            // falls through to the per-shell read below.
            let running: std::collections::HashSet<&str> = list
                .data
                .iter()
                .filter(|shell| shell.status == "running")
                .map(|shell| shell.id.as_str())
                .collect();
            for shell_id in shells {
                if running.contains(shell_id.as_str()) {
                    runtime.shells.push((shell_id.clone(), ShellRuntime::Running));
                    continue;
                }
                match self.shell_runtime(http, directory, shell_id).await {
                    Ok(verdict) => runtime.shells.push((shell_id.clone(), verdict)),
                    // No verdict: the read failed — the task stays as the
                    // transcript read it, and the next pass retries.
                    Err(error) => {
                        tracing::debug!("session {session_id} task runtime: shell {shell_id}: {error}");
                    }
                }
            }
        }
        if !children.is_empty() {
            let active = self.active_sessions(http, "task runtime active map").await?;
            for child_id in children {
                match active.state(child_id) {
                    Some(true) => runtime.children.push((child_id.clone(), ChildRuntime::Running)),
                    // Present with an unrecognised type: no verdict, never a
                    // guessed `Inactive`.
                    Some(false) => {}
                    None => runtime.children.push((child_id.clone(), ChildRuntime::Inactive)),
                }
            }
        }
        Ok(runtime)
    }

    /// The evidence one child session's newest assistant message carries (#591,
    /// issue #464): ONE page, newest assistant first — the same minimal wire
    /// shape as the retry read, so it never decodes the child transcript. A
    /// terminal step finish together with the message's completion stamp is
    /// terminal evidence; a 404 is the session-gone evidence (the child cannot
    /// be running under the attached server); anything else is no evidence at
    /// all, never guessed. The decode lives with the message page
    /// ([`wire::MessagesPage::newest_assistant_evidence`]).
    async fn child_evidence(&self, http: &Transport, session_id: &str) -> Result<ChildEvidence> {
        let url = message_page_url(
            http,
            session_id,
            &[("type", "assistant"), ("order", "desc"), ("limit", "1")],
        )?;
        let resp = http.client().get(url).send().await?;
        if resp.status() == reqwest::StatusCode::NOT_FOUND {
            return Ok(ChildEvidence::Gone);
        }
        if !resp.status().is_success() {
            return Err(read_failure(resp, "child evidence").await);
        }
        let text = resp.text().await?;
        let page: wire::MessagesPage = serde_json::from_str(&text).map_err(|e| {
            crate::error::BridgeError::OpenCode(format!(
                "session child evidence read parse: {e} — body: {}",
                body_preview(&text)
            ))
        })?;
        Ok(page.newest_assistant_evidence())
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

    /// The model the session last actually ran with: the newest assistant
    /// message's ref (`type=assistant&order=desc&limit=1`). The session read
    /// carries a model only after an explicit switch, so this is the public
    /// record of what a session started from the server default is running —
    /// the effective-model ladder's last rung.
    async fn session_last_run_model(
        &self,
        http: &Transport,
        session_id: &str,
        _directory: Option<&str>,
    ) -> Result<Option<ModelInfo>> {
        let page = self
            .message_page(
                http,
                session_id,
                "last-run model",
                &[("type", "assistant"), ("order", "desc"), ("limit", "1")],
            )
            .await?;
        Ok(page.newest_assistant().and_then(wire::decode_message_model))
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

    /// V2's prompt admits by `messageID`: once admitted, a same-id re-post is
    /// a 200 no-op that runs nothing (only a never-admitted id runs).
    fn reuse_continues_an_admitted_turn(&self) -> bool {
        false
    }

    /// V2 serves the durable resume write.
    fn resume_supported(&self) -> bool {
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

    /// Resume the session (`POST /api/session/{id}/resume`): a message queued
    /// in the session inbox (cola's steered submit, `delivery:"steer"`) is
    /// promoted at the new run's start. Answers 204 — the body is ignored like
    /// the other status-only writes. Failures go through the same mapping as
    /// the interrupt route's: the tagged `SessionNotFoundError` 404 becomes the
    /// bridge's SessionNotFound, anything else an OpenCode error naming the
    /// operation.
    async fn resume(&self, http: &Transport, session_id: &str) -> Result<()> {
        let response = http
            .client()
            .post(http.url(&format!("{SESSION}/{session_id}{SESSION_RESUME_SUFFIX}")))
            .send()
            .await?;
        if !response.status().is_success() {
            return Err(write_failure(response, session_id, "resume").await);
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

    /// One shell's captured output window (spec #588, ticket #592), the
    /// server's own tail idiom: a first page with a cursor past the end learns
    /// the record's total size, a second reads its last
    /// [`SHELL_OUTPUT_WINDOW_BYTES`] bytes, and [`output_window`] decodes and
    /// clips that page to its last [`SHELL_OUTPUT_WINDOW_LINES`] lines. A 404
    /// is the runtime's positive "no record" (like [`Self::shell_runtime`]) and
    /// maps to the no-record case ([`Ok(None)`]) — the entry's
    /// 「输出已不可用」 — while a successful read of an empty capture answers a
    /// window with no text: the record exists and holds nothing, and the row
    /// and the entry omit it (spec #588, review). Any other failure is an
    /// `Err` the caller omits the window for, never guesses. Display-only:
    /// the read touches no session state.
    async fn shell_output(
        &self,
        http: &Transport,
        shell_id: &str,
        directory: Option<&str>,
    ) -> Result<Option<crate::backend::ShellOutputWindow>> {
        let Some(probe) = self
            .shell_output_page(http, directory, shell_id, SHELL_OUTPUT_PROBE_CURSOR, None)
            .await?
        else {
            return Ok(None);
        };
        if probe.size == 0 {
            return Ok(Some(crate::backend::ShellOutputWindow {
                text: String::new(),
                clipped: false,
                captured_ms: chrono::Utc::now().timestamp_millis(),
            }));
        }
        let start = probe
            .size
            .saturating_sub(crate::backend::SHELL_OUTPUT_WINDOW_BYTES as u64);
        let Some(tail) = self
            .shell_output_page(
                http,
                directory,
                shell_id,
                start,
                Some(crate::backend::SHELL_OUTPUT_WINDOW_BYTES as u64),
            )
            .await?
        else {
            return Ok(None);
        };
        Ok(Some(output_window(tail, start > 0)))
    }
}

impl V2Strategy {
    /// A fresh strategy, with an empty live-progress cache (the reader starts
    /// lazily on the first transcript read).
    pub(crate) fn new() -> Self {
        Self {
            progress: progress::Progress::new(),
        }
    }

    /// Durably admit one prompt (`POST /api/session/{id}/prompt`) with
    /// `delivery: "steer"` — the server's own default, sent explicitly: while
    /// the session is idle it starts the run, and while a turn is in flight it
    /// merges in at the next step boundary (the main dispatch and a mid-turn
    /// Supplement are the same call). Returns the admitted user-message id: the
    /// caller's `message_id` when set, else the server's.
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
        body["delivery"] = serde_json::json!(PROMPT_DELIVERY);
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

    /// The active-session run-state map (`GET /api/session/active`), shared by
    /// the run-state read and the Background Task reconciliation. `what` names
    /// the caller in the failure text — each read's diagnostics stay its own.
    async fn active_sessions(&self, http: &Transport, what: &str) -> Result<wire::ActiveSessions> {
        let resp = http.client().get(http.url(SESSION_ACTIVE)).send().await?;
        if !resp.status().is_success() {
            return Err(read_failure(resp, what).await);
        }
        let text = resp.text().await?;
        serde_json::from_str(&text).map_err(|e| {
            crate::error::BridgeError::OpenCode(format!("{what} parse: {e} — body: {}", body_preview(&text)))
        })
    }

    /// One shell's runtime verdict (`GET /api/shell/{id}`, issue #454). 404 is
    /// the runtime's positive "no record" ([`ShellRuntime::Missing`]); a 200
    /// decodes the retained shell, which may still race back to `running`; any
    /// other failure is an `Err` the caller degrades to "no verdict" after a
    /// debug log, so one bad shell read cannot fail the whole reconciliation.
    async fn shell_runtime(
        &self,
        http: &Transport,
        directory: Option<&str>,
        shell_id: &str,
    ) -> Result<ShellRuntime> {
        let url = location_url(&http.url(&format!("{SHELL}/{shell_id}")), directory)?;
        let resp = http.client().get(url).send().await?;
        if resp.status() == reqwest::StatusCode::NOT_FOUND {
            return Ok(ShellRuntime::Missing);
        }
        if !resp.status().is_success() {
            return Err(read_failure(resp, "shell read").await);
        }
        let body: wire::DataEnvelope<wire::RawShell> = resp.json().await?;
        Ok(body.data.into_runtime())
    }

    /// One output page (`GET /api/shell/{id}/output?location[directory]=…&
    /// cursor=…[&limit=…]`). `None` is the record the runtime no longer keeps
    /// (404) — the caller's no-output case; any other non-success is an `Err`,
    /// so a vanished record and a failed read stay distinguishable.
    async fn shell_output_page(
        &self,
        http: &Transport,
        directory: Option<&str>,
        shell_id: &str,
        cursor: u64,
        limit: Option<u64>,
    ) -> Result<Option<wire::ShellOutputPage>> {
        let mut url = location_url(&http.url(&format!("{SHELL}/{shell_id}/output")), directory)?;
        {
            let mut query = url.query_pairs_mut();
            query.append_pair("cursor", &cursor.to_string());
            if let Some(limit) = limit {
                query.append_pair("limit", &limit.to_string());
            }
        }
        let resp = http.client().get(url).send().await?;
        if resp.status() == reqwest::StatusCode::NOT_FOUND {
            return Ok(None);
        }
        if !resp.status().is_success() {
            return Err(read_failure(resp, "shell output").await);
        }
        let text = resp.text().await?;
        let body: wire::DataEnvelope<wire::ShellOutputPage> = serde_json::from_str(&text).map_err(|e| {
            crate::error::BridgeError::OpenCode(format!(
                "shell output parse: {e} — body: {}",
                body_preview(&text)
            ))
        })?;
        Ok(Some(body.data))
    }

    /// Read the session's projected messages, decoded-ready: the raw `data`
    /// arrays a `GET /api/session/{id}/message` read carries, in server order.
    /// The transcript decode's raw input.
    async fn read_messages(
        &self,
        http: &Transport,
        session_id: &str,
    ) -> Result<(Vec<serde_json::Value>, bool)> {
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
                // The read reached a page with no successor: a COMPLETE read.
                _ => return Ok((data, false)),
            }
        }
        tracing::warn!(
            "transcript {session_id}: cursor.next still present after {MAX_MESSAGE_PAGES} pages; \
             returning the {} messages fetched so far",
            data.len()
        );
        // The page cap stopped the read with content behind it (spec #561,
        // review #569): the neutral transcript must say so, or an ended
        // projection would settle on a prefix and lose the tail.
        Ok((data, true))
    }

    /// Fetch ONE page of a session's projected messages with `query` pairs —
    /// the shared transport/parse half of the minimal message reads
    /// (`type=assistant&order=desc&limit=1`). `read` names the failure, so a
    /// caller's errors stay distinguishable in logs.
    async fn message_page(
        &self,
        http: &Transport,
        session_id: &str,
        read: &str,
        query: &[(&str, &str)],
    ) -> Result<wire::MessagesPage> {
        let url = message_page_url(http, session_id, query)?;
        let resp = http.client().get(url).send().await?;
        let status = resp.status();
        if !status.is_success() {
            // A body-read failure degrades to an empty preview rather than
            // replacing the status-named error with a transport one.
            let text = resp.text().await.unwrap_or_default();
            return Err(crate::error::BridgeError::OpenCode(format!(
                "session {read} read failed: {status} — body: {}",
                body_preview(&text)
            )));
        }
        let text = resp.text().await?;
        serde_json::from_str(&text).map_err(|e| {
            crate::error::BridgeError::OpenCode(format!(
                "session {read} read parse: {e} — body: {}",
                body_preview(&text)
            ))
        })
    }

    /// Whether the newest assistant message of `session_id` carries a scheduled
    /// `retry`. Reads ONE projected message (`type=assistant&order=desc&
    /// limit=1`) — the minimal wire read that answers the run-state question
    /// without decoding the whole transcript. Failures name the retry read, so
    /// a caller can tell them apart from the active-map read.
    async fn newest_assistant_retrying(&self, http: &Transport, session_id: &str) -> Result<bool> {
        let page = self
            .message_page(
                http,
                session_id,
                "retry",
                &[("type", "assistant"), ("order", "desc"), ("limit", "1")],
            )
            .await?;
        Ok(page.newest_assistant_retrying())
    }
}

/// One output page as its neutral window (spec #588, ticket #592): the last
/// [`SHELL_OUTPUT_WINDOW_LINES`] lines of the page, with the byte-boundary
/// artifact trimmed and the clipped flag derived. The window's text is EMPTY
/// when the page carries nothing to render (a successful read of a record that
/// captured nothing, or a race that shrank the file): a readable-empty window,
/// which the row and the completion entry omit — never 「输出已不可用」, which
/// belongs to a record that is gone or unreadable (`Ok(None)` from
/// [`V2Strategy::shell_output`]).
///
/// `started_mid_record` is the tail read's own cursor > 0: it means the window
/// begins inside the capture, so the head line is partial and the window is
/// clipped by definition. The server decodes a cursor that splits a multi-byte
/// character to a leading U+FFFD — that single artifact is trimmed; a
/// replacement character anywhere else is the command's own bytes and stays.
/// The server's `truncated` flag is decoded too (always false on today's
/// server, but a future one may clip a page) and counts as clipped.
fn output_window(page: wire::ShellOutputPage, started_mid_record: bool) -> crate::backend::ShellOutputWindow {
    let mut lines: Vec<&str> = page.output.lines().collect();
    let mut clipped = started_mid_record || page.truncated;
    if lines.len() > crate::backend::SHELL_OUTPUT_WINDOW_LINES {
        clipped = true;
        lines.drain(..lines.len() - crate::backend::SHELL_OUTPUT_WINDOW_LINES);
    }
    let mut text = lines.join("\n");
    if started_mid_record && text.starts_with('\u{FFFD}') {
        text.remove(0);
    }
    crate::backend::ShellOutputWindow {
        text,
        clipped,
        captured_ms: chrono::Utc::now().timestamp_millis(),
    }
}
