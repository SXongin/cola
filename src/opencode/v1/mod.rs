//! The V1 generation strategy (ADR-0055): OpenCode 1.18.x's unprefixed
//! compatibility surface.
//!
//! Everything V1 lives here — route constants, request bodies, response
//! decoding and status fan-out — together with the legacy wire decoder under
//! [`wire`]. The adapter ([`super::client::OpenCodeBackend`]) is
//! generation-blind and reaches this module only through the
//! [`GenerationStrategy`] trait, so **deleting this module is V1 retirement**.
//!
//! Session creation is the one deliberate exception to "unprefixed only": the
//! mounted `/api/session` surface is where `location.directory` (the session's
//! working directory) lives, and 1.18.x serves it alongside the unprefixed
//! routes (ADR-0026 / ADR-0053 record the choice). Compact has no V1 route cola
//! has ever used.

mod wire;

#[cfg(test)]
mod tests;

use async_trait::async_trait;

use crate::backend::SessionTranscript;
use crate::error::Result;

use super::strategy::GenerationStrategy;
use super::transport::{REPLY_TIMEOUT, Transport};
use super::types::{
    AgentInfo, CreateSessionInput, CreateSessionResponse, ImageInput, Location, ModelInfo, ModelOption,
    PermissionRequest, PromptResponse, ProviderModels, QuestionRequest, Session, SessionInfo,
    SessionListInfo, SessionStatus,
};

/// Hard stop for the `x-next-cursor` follow in [`V1Strategy::list_sessions`]: a
/// misbehaving server must not spin the client forever. 100 pages of the
/// server's 100-row default is 10k sessions, far past any real store.
const MAX_SESSION_PAGES: usize = 100;

/// The cross-project session list (the experimental V1 read).
const SESSION_LIST: &str = "/experimental/session";
/// The project-scoped session list, the fallback for servers too old to expose
/// the experimental route.
const SESSION_LIST_FALLBACK: &str = "/session";
/// The session root (info, message read, prompt, abort).
const SESSION: &str = "/session";
/// The per-session run-state map.
const SESSION_STATUS: &str = "/session/status";
/// The global permission list / reply root.
const PERMISSION: &str = "/permission";
/// The global question list / reply / reject root.
const QUESTION: &str = "/question";
/// The provider catalog (models, context windows).
const PROVIDER: &str = "/provider";
/// The agent catalog.
const AGENT: &str = "/agent";
/// Session creation, on the mounted `/api` surface where `location.directory`
/// lives (ADR-0053 amendment).
const API_SESSION: &str = "/api/session";

/// The strategy that speaks the V1 generation.
pub(crate) struct V1Strategy;

#[async_trait]
impl GenerationStrategy for V1Strategy {
    fn new_session_input(&self, model: Option<&ModelInfo>, directory: Option<&str>) -> CreateSessionInput {
        CreateSessionInput {
            id: None,
            agent: None,
            model: model.cloned(),
            location: directory.map(|d| Location {
                directory: d.to_string(),
            }),
        }
    }

    /// Create a new session with an optional directory and agent.
    async fn create_session(&self, http: &Transport, input: &CreateSessionInput) -> Result<Session> {
        let resp = http
            .client()
            .post(http.url(API_SESSION))
            .json(input)
            .send()
            .await?
            .error_for_status()?;
        let body: CreateSessionResponse = resp.json().await?;
        Ok(body.data)
    }

    /// List sessions across the shared store, most recently active first.
    ///
    /// Uses the cross-project list `GET /experimental/session`
    /// (`Session.GlobalInfo` — camelCase: `id`, `title`, `directory`,
    /// `parentID`, `time.created/updated`, `agent`, `model`; archived sessions
    /// excluded server-side by default). The server caps a response at its page
    /// limit (default 100) and reports the cutoff in `x-next-cursor`; the
    /// cursor is followed to the end, so the limit can never hide rows —
    /// recently updated sub-task children used to fill the newest page and
    /// crowd roots out of the window (issue #325). Children stay in the
    /// returned set — the child policy belongs to each caller (ADR-0008), so a
    /// roots-only view is a per-surface filter, not this listing's job.
    ///
    /// The plain `GET /session` is PROJECT-scoped: it only returns the server's
    /// *own* directory's project (the instance's cwd), so cola's sessions in
    /// another project never appear — the "recent" list instead shows stale
    /// sessions from the server's project. We fall back to it only for servers
    /// too old to expose the experimental route.
    async fn list_sessions(&self, http: &Transport) -> Result<Vec<SessionListInfo>> {
        let mut sessions = Vec::new();
        let mut cursor: Option<String> = None;
        for _ in 0..MAX_SESSION_PAGES {
            let mut url = reqwest::Url::parse(&http.url(SESSION_LIST))?;
            if let Some(cursor) = &cursor {
                url.query_pairs_mut().append_pair("cursor", cursor);
            }
            let resp = http.client().get(url).send().await?;
            if resp.status() == reqwest::StatusCode::NOT_FOUND && cursor.is_none() {
                tracing::warn!(
                    "server lacks GET /experimental/session; falling back to project-scoped GET /session"
                );
                let resp = http
                    .client()
                    .get(http.url(SESSION_LIST_FALLBACK))
                    .send()
                    .await?
                    .error_for_status()?;
                return Ok(resp.json().await?);
            }
            let next = resp
                .headers()
                .get("x-next-cursor")
                .and_then(|value| value.to_str().ok())
                .filter(|value| !value.is_empty())
                .map(str::to_string);
            let page: Vec<SessionListInfo> = resp.error_for_status()?.json().await?;
            let empty = page.is_empty();
            sessions.extend(page);
            match next {
                Some(next) if !empty => cursor = Some(next),
                _ => return Ok(sessions),
            }
        }
        tracing::warn!(
            "session list: x-next-cursor still present after {MAX_SESSION_PAGES} pages; \
             returning the {} sessions fetched so far",
            sessions.len()
        );
        Ok(sessions)
    }

    /// Rename a session server-side (canonical: `PATCH /session/{id}` with
    /// `{"title": ...}`). The change is visible to every client sharing the
    /// store (OpenChamber, CLI).
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

    /// Send a prompt to a session using the canonical OpenCode API:
    /// `POST /session/{id}/message` with `parts`. This is the same protocol
    /// OpenChamber/TUI use, so messages land in the shared message store.
    ///
    /// `model` is the effective model (the per-session override or the
    /// configured default, already resolved by the adapter); when None the
    /// server uses its own default.
    ///
    /// `agent` is a per-session override from `/agent`; when Some the server
    /// uses that agent for this turn (the session's own/default agent
    /// otherwise).
    ///
    /// `variant` is the per-session `/think` override; when Some the server
    /// applies that model-declared variant (e.g. "high") to whatever model runs
    /// this turn. Independent of `model` — a variant can be set even when no
    /// model override exists (the server default model then carries it).
    ///
    /// `message_id` is the id cola chose for the user message this prompt will
    /// create (ADR-0026). The server persists it, and a retry that reuses it is
    /// idempotent — never a duplicate user message. None falls back to a
    /// server-generated id (used only by tests/other clients).
    #[allow(clippy::too_many_arguments)] // prompt axes: session/text/images + model/variant/agent/message-id
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
        let mut body = serde_json::json!({
            "parts": build_parts(text, images),
        });
        inject_message_id(&mut body, message_id);
        inject_model(&mut body, model, variant);
        inject_agent(&mut body, agent);
        let resp = http
            .client()
            .post(http.url(&format!("{SESSION}/{session_id}/message")))
            .json(&body)
            .send()
            .await?;
        if resp.status().is_success() {
            let text_body = resp.text().await?;
            // ADR-0048: the prompt-response body is a full payload dump — DEBUG
            // only. The Turn's own lines are what an INFO trace reads for.
            tracing::debug!("prompt response: {}", &text_body[..text_body.len().min(500)]);
            let parsed: serde_json::Value = serde_json::from_str(&text_body).map_err(|e| {
                crate::error::BridgeError::OpenCode(format!(
                    "prompt decode: {e} — body: {}",
                    &text_body[..text_body.len().min(300)]
                ))
            })?;
            let message_id = parsed
                .get("info")
                .and_then(|i| i.get("id"))
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let parent_id = parsed
                .get("info")
                .and_then(|i| i.get("parentID"))
                .and_then(|v| v.as_str())
                .map(|s| s.to_string());
            let error = parsed
                .get("info")
                .and_then(|i| i.get("error"))
                .and_then(|e| e.get("data"))
                .and_then(|d| d.get("message"))
                .and_then(|v| v.as_str())
                .map(|s| s.to_string())
                .or_else(|| {
                    parsed
                        .get("info")
                        .and_then(|i| i.get("error"))
                        .and_then(|e| e.get("message"))
                        .and_then(|v| v.as_str())
                        .map(|s| s.to_string())
                });
            return Ok(PromptResponse {
                id: message_id.clone(),
                session_id: Some(session_id.to_string()),
                admitted_seq: None,
                parent_id,
                error,
                // The response's parts decode through the same wire seam as a
                // polled message, so no raw protocol shape reaches the Bridge.
                parts: wire::decode_parts(
                    parsed
                        .get("parts")
                        .unwrap_or(&serde_json::Value::Array(Vec::new())),
                ),
            });
        }

        // Canonical 404: the session doesn't exist on this server (e.g. it was
        // created before a server restart/replacement, or another client removed
        // it). Report SessionNotFound so the bridge recreates the session —
        // falling back to the legacy path here would surface a confusing 502 and
        // the recreate never fires (see AGENTS.md pitfall #1).
        if resp.status() == reqwest::StatusCode::NOT_FOUND {
            return Err(crate::error::BridgeError::SessionNotFound(session_id.to_string()));
        }
        let status = resp.status();

        // Any other failure (5xx, network drop) surfaces as a prompt error —
        // the error card's retry re-submits with the SAME message_id, which the
        // server deduplicates. There is deliberately NO legacy `/api/.../prompt`
        // fallback: it cannot carry `message_id`, appends a fresh user message,
        // and re-runs the model, so retrying through it would duplicate the
        // message + double-run (ADR-0026).
        let text_body = resp.text().await?;
        Err(crate::error::BridgeError::OpenCode(format!(
            "prompt {session_id} failed: HTTP {} — {}",
            status,
            &text_body[..text_body.len().min(300)]
        )))
    }

    /// Fire-and-forget prompt: `POST /session/{id}/prompt_async`. OpenCode
    /// immediately persists the user message (`createUserMessage`) and forks a
    /// run (`Effect.forkIn`), returning 204 — the caller doesn't block until the
    /// turn finishes. Used by the supplement path: while a turn is in flight we
    /// send the new message here so it lands in the DB and the running loop
    /// picks it up at the next tool boundary (merged into the current turn),
    /// without a second synchronous prompt blocking the WS read loop.
    #[allow(clippy::too_many_arguments)] // same prompt axes as `prompt`
    async fn prompt_async(
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
        let mut body = serde_json::json!({
            "parts": build_parts(text, images),
        });
        inject_message_id(&mut body, message_id);
        inject_model(&mut body, model, variant);
        inject_agent(&mut body, agent);
        let resp = http
            .client()
            .post(http.url(&format!("{SESSION}/{session_id}/prompt_async")))
            .json(&body)
            .send()
            .await?;
        // Same canonical 404 meaning as `prompt`: the session does not exist on
        // this server. Keep the taxonomy aligned (`is_session_not_found`) even
        // though today's only caller replies the same way for every error —
        // the wire client reports what the server said, the bridge decides.
        if resp.status() == reqwest::StatusCode::NOT_FOUND {
            return Err(crate::error::BridgeError::SessionNotFound(session_id.to_string()));
        }
        if !resp.status().is_success() {
            return Err(crate::error::BridgeError::OpenCode(format!(
                "prompt_async {}: {} {}",
                session_id,
                resp.status(),
                resp.text().await.unwrap_or_default()
            )));
        }
        tracing::info!("prompt_async sent to session {}", session_id);
        Ok(())
    }

    /// Fetch a session's info (canonical: `GET /session/{id}`), used to resolve
    /// a sub-task (child) session's parent chain so permission cards for subtask
    /// sessions can be routed to the chat the parent is mapped to.
    async fn session_info(
        &self,
        http: &Transport,
        session_id: &str,
        directory: Option<&str>,
    ) -> Result<SessionInfo> {
        let mut url = reqwest::Url::parse(&http.url(&format!("{SESSION}/{session_id}")))?;
        if let Some(d) = directory {
            url.query_pairs_mut().append_pair("directory", d);
        }
        let resp = http.client().get(url).send().await?.error_for_status()?;
        Ok(resp.json().await?)
    }

    /// Reply to a permission request (canonical: `POST /permission/{id}/reply`).
    /// `directory` routes the request to the instance that owns the permission —
    /// without it the server checks the cwd instance and returns 404/400.
    async fn reply_permission(
        &self,
        http: &Transport,
        request_id: &str,
        reply: &str,
        directory: Option<&str>,
    ) -> Result<()> {
        let body = serde_json::json!({ "reply": reply });
        let mut url = reqwest::Url::parse(&http.url(&format!("{PERMISSION}/{request_id}/reply")))?;
        if let Some(d) = directory {
            url.query_pairs_mut().append_pair("directory", d);
        }
        let resp = http
            .client()
            .post(url)
            .json(&body)
            .timeout(REPLY_TIMEOUT)
            .send()
            .await?;
        check_reply_status(resp, "permission", request_id)?;
        Ok(())
    }

    /// List pending permissions for an instance (canonical: `GET /permission`).
    /// `directory` selects the instance; without it only the server cwd's
    /// instance is checked, so permissions for sessions in other directories
    /// would be missed.
    async fn list_permissions(
        &self,
        http: &Transport,
        directory: Option<&str>,
    ) -> Result<Vec<PermissionRequest>> {
        let mut url = reqwest::Url::parse(&http.url(PERMISSION))?;
        if let Some(d) = directory {
            url.query_pairs_mut().append_pair("directory", d);
        }
        let resp = http.client().get(url).send().await?;
        let status = resp.status();
        if !status.is_success() {
            let text = resp.text().await.unwrap_or_default();
            tracing::warn!(
                "GET /permission failed: {} — body: {}",
                status,
                &text[..text.len().min(500)]
            );
            return Err(crate::error::BridgeError::OpenCode(format!(
                "permission list failed: {}",
                status
            )));
        }
        Ok(resp.json().await?)
    }

    /// Fetch one session's Session Transcript through the unprefixed
    /// `GET /session/{id}/message` read, decoded by the legacy wire decoder.
    /// That is the store cola's own prompts and the other mounted clients
    /// (OpenChamber) append to, so it is the one source the conversation
    /// actually flows through. A failed read surfaces; there is no silent
    /// fallback.
    async fn transcript(&self, http: &Transport, session_id: &str) -> Result<SessionTranscript> {
        let resp = http
            .client()
            .get(http.url(&format!("{SESSION}/{session_id}/message")))
            .send()
            .await?
            .error_for_status()?;
        let body: serde_json::Value = resp.json().await?;
        wire::decode_response(&body)
    }

    /// The server's per-session run state for ONE session (canonical:
    /// `GET /session/status`, which returns `Record<sessionID, SessionStatus>`).
    ///
    /// Semantics mirror the server's own status service: a session the server
    /// has no record for is idle (a finished run is removed from the map; the
    /// server's `get` returns `{type:"idle"}` for an absent session). So a
    /// successful read always yields a status — `Idle` for an absent session —
    /// and `Ok(None)` is reserved for a session that IS present but whose
    /// status `type` this client does not recognise (unknown → never guessed).
    ///
    /// `directory` selects the server instance (ADR-0010); without it only the
    /// cwd instance is checked.
    async fn session_status(
        &self,
        http: &Transport,
        session_id: &str,
        directory: Option<&str>,
    ) -> Result<Option<SessionStatus>> {
        let mut url = reqwest::Url::parse(&http.url(SESSION_STATUS))?;
        if let Some(d) = directory {
            url.query_pairs_mut().append_pair("directory", d);
        }
        let resp = http.client().get(url).send().await?;
        let status = resp.status();
        if !status.is_success() {
            let text = resp.text().await.unwrap_or_default();
            tracing::warn!(
                "GET /session/status failed: {} — body: {}",
                status,
                &text[..text.len().min(500)]
            );
            return Err(crate::error::BridgeError::OpenCode(format!(
                "session status failed: {}",
                status
            )));
        }
        let text = resp.text().await?;
        let map: std::collections::HashMap<String, serde_json::Value> =
            serde_json::from_str(&text).map_err(|e| {
                crate::error::BridgeError::OpenCode(format!(
                    "session status parse: {e} — body: {}",
                    &text[..text.len().min(300)]
                ))
            })?;
        Ok(match map.get(session_id) {
            None => Some(SessionStatus::Idle),
            Some(entry) => parse_session_status_entry(entry),
        })
    }

    /// List pending question requests for an instance (canonical:
    /// `GET /question`).
    async fn list_questions(
        &self,
        http: &Transport,
        directory: Option<&str>,
    ) -> Result<Vec<QuestionRequest>> {
        let mut url = reqwest::Url::parse(&http.url(QUESTION))?;
        if let Some(d) = directory {
            url.query_pairs_mut().append_pair("directory", d);
        }
        let resp = http.client().get(url).send().await?;
        let status = resp.status();
        if !status.is_success() {
            let text = resp.text().await.unwrap_or_default();
            eprintln!(
                "list_questions failed: {} — body: {}",
                status,
                &text[..text.len().min(500)]
            );
            return Err(crate::error::BridgeError::OpenCode(format!(
                "question list failed: {}",
                status
            )));
        }
        Ok(resp.json().await?)
    }

    /// The model's context-window size (tokens), from `GET /provider`. Best
    /// effort: any failure returns Ok(None) so the footer just omits the ratio.
    async fn model_context_window(
        &self,
        http: &Transport,
        provider: &str,
        model: &str,
    ) -> Result<Option<i64>> {
        let resp = http.client().get(http.url(PROVIDER)).send().await?;
        if !resp.status().is_success() {
            return Ok(None);
        }
        let text = resp.text().await?;
        let Ok(v) = serde_json::from_str::<serde_json::Value>(&text) else {
            return Ok(None);
        };
        let all = v.get("all").and_then(|a| a.as_array());
        let Some(all) = all else { return Ok(None) };
        for prov in all {
            if prov.get("id").and_then(|i| i.as_str()) != Some(provider) {
                continue;
            }
            let Some(models) = prov.get("models").and_then(|m| m.as_object()) else {
                continue;
            };
            if let Some(m) = models.get(model)
                && let Some(ctx) = m
                    .get("limit")
                    .and_then(|l| l.get("context"))
                    .and_then(|c| c.as_i64())
            {
                return Ok(Some(ctx));
            }
        }
        Ok(None)
    }

    /// Answer a question request (canonical: `POST /question/{id}/reply`).
    async fn reply_question(
        &self,
        http: &Transport,
        request_id: &str,
        answers: &[Vec<String>],
        directory: Option<&str>,
    ) -> Result<()> {
        let body = serde_json::json!({ "answers": answers });
        let mut url = reqwest::Url::parse(&http.url(&format!("{QUESTION}/{request_id}/reply")))?;
        if let Some(d) = directory {
            url.query_pairs_mut().append_pair("directory", d);
        }
        let resp = http
            .client()
            .post(url)
            .json(&body)
            .timeout(REPLY_TIMEOUT)
            .send()
            .await?;
        check_reply_status(resp, "question", request_id)?;
        Ok(())
    }

    /// Available agents (`GET /agent`), each with a `name`. Best-effort: an
    /// unreadable/cached failure returns an empty list so the `/agent` card can
    /// degrade to a plain text prompt.
    async fn list_agents(&self, http: &Transport) -> Vec<AgentInfo> {
        let Ok(resp) = http.client().get(http.url(AGENT)).send().await else {
            return Vec::new();
        };
        let Ok(text) = resp.text().await else {
            return Vec::new();
        };
        serde_json::from_str::<Vec<AgentInfo>>(&text).unwrap_or_default()
    }

    /// Available models (`GET /provider`), grouped as `provider → models`,
    /// each with its declared variants. Best-effort: an unreadable/cached
    /// failure returns an empty map so the `/model` card can degrade to a
    /// plain text prompt.
    ///
    /// Only providers the server reports as `connected` (real credentials —
    /// `GET /provider` returns `connected: [ids]`) are surfaced: a shared
    /// server advertises hundreds of providers most of which have no API key,
    /// and a `/model` picker over all of them is unusable. When the response
    /// has no `connected` field (older server) every provider is kept.
    async fn list_models(&self, http: &Transport) -> Vec<ProviderModels> {
        let Ok(resp) = http.client().get(http.url(PROVIDER)).send().await else {
            return Vec::new();
        };
        let Ok(text) = resp.text().await else {
            return Vec::new();
        };
        let Ok(v) = serde_json::from_str::<serde_json::Value>(&text) else {
            return Vec::new();
        };
        parse_provider_models(&v)
    }

    /// Reject a question request (canonical: `POST /question/{id}/reject`).
    async fn reject_question(
        &self,
        http: &Transport,
        request_id: &str,
        directory: Option<&str>,
    ) -> Result<()> {
        let mut url = reqwest::Url::parse(&http.url(&format!("{QUESTION}/{request_id}/reject")))?;
        if let Some(d) = directory {
            url.query_pairs_mut().append_pair("directory", d);
        }
        let resp = http.client().post(url).timeout(REPLY_TIMEOUT).send().await?;
        check_reply_status(resp, "question", request_id)?;
        Ok(())
    }

    /// Interrupt an active session.
    async fn interrupt(&self, http: &Transport, session_id: &str) -> Result<()> {
        // Canonical path has NO `/api` prefix (see AGENTS.md pitfall #1); the
        // old `/api/session/{id}/interrupt` 404'd so `/stop` silently failed.
        http.client()
            .post(http.url(&format!("{SESSION}/{session_id}/abort")))
            .send()
            .await?
            .error_for_status()?;
        Ok(())
    }

    /// Compact a session's context.
    async fn compact(&self, http: &Transport, session_id: &str) -> Result<()> {
        http.client()
            .post(http.url(&format!("{API_SESSION}/{session_id}/compact")))
            .send()
            .await?
            .error_for_status()?;
        Ok(())
    }
}

/// The prompt `parts` array: a text part followed by one data-URL `file` part
/// per image. OpenCode decodes the data URL and normalizes the image before
/// handing it to a vision-capable model (FilePartInput). Shared by `prompt`
/// and `prompt_async`.
fn build_parts(text: &str, images: &[ImageInput]) -> Vec<serde_json::Value> {
    let mut parts = vec![serde_json::json!({ "type": "text", "text": text })];
    for img in images {
        parts.push(serde_json::json!({
            "type": "file",
            "mime": img.mime,
            "url": format!("data:{};base64,{}", img.mime, img.data_base64),
        }));
    }
    parts
}

/// Attach the effective model to a prompt body. The adapter already resolved
/// the per-session override and the configured default; when neither is set
/// the server uses its own default. `variant` is independent — it is written
/// whenever set, applying to whatever model the server runs this turn. Shared
/// by `prompt` and `prompt_async`.
fn inject_model(body: &mut serde_json::Value, model: Option<&ModelInfo>, variant: Option<&str>) {
    if let Some(model) = model {
        body["model"] = serde_json::json!({
            "providerID": model.provider_id,
            "modelID": model.id,
        });
    }
    if let Some(v) = variant {
        body["variant"] = serde_json::json!(v);
    }
}

/// Attach the per-session agent override to a prompt body. When unset the
/// server uses the session's own/default agent. Shared by `prompt` and
/// `prompt_async`.
fn inject_agent(body: &mut serde_json::Value, agent: Option<&str>) {
    if let Some(a) = agent {
        body["agent"] = serde_json::json!(a);
    }
}

/// Attach the cola-chosen user-message id to a prompt body (`msg_cola_…`,
/// ADR-0026). When set the server persists that id (idempotent on retries);
/// when None it generates one. Shared by `prompt` and `prompt_async`.
fn inject_message_id(body: &mut serde_json::Value, message_id: Option<&str>) {
    if let Some(mid) = message_id {
        body["messageID"] = serde_json::json!(mid);
    }
}

/// Apply the V1 reply-endpoint policy to a response: on this generation a 404
/// means the request was already resolved elsewhere — benign and expected (a
/// double-click, another client, or a click replayed after a cola restart) —
/// so it maps to `BridgeError::NotFound`, which the bridge renders as a
/// neutral "already handled" card rather than a failure. Any other error
/// status stays a real failure. Shared by the permission and question reply
/// endpoints so their behaviour cannot drift apart. V2's session-scoped reply
/// routes need their own read of a 404 (a gone session is not an answered
/// request), so this policy lives with the V1 generation.
fn check_reply_status(resp: reqwest::Response, what: &str, request_id: &str) -> Result<()> {
    if resp.status() == reqwest::StatusCode::NOT_FOUND {
        return Err(crate::error::BridgeError::NotFound(format!(
            "{what} {request_id}"
        )));
    }
    resp.error_for_status()?;
    Ok(())
}

/// Parse one `GET /session/status` map entry (e.g. `{"type":"busy"}`) into a
/// [`SessionStatus`]. `None` for an unrecognised/absent `type` — never guess.
fn parse_session_status_entry(entry: &serde_json::Value) -> Option<SessionStatus> {
    match entry.get("type").and_then(|t| t.as_str()) {
        Some("idle") => Some(SessionStatus::Idle),
        Some("busy") => Some(SessionStatus::Busy),
        Some("retry") => Some(SessionStatus::Retry),
        _ => None,
    }
}

/// Parse a `GET /provider` response body into the provider → models map the
/// `/model` and `/think` cards render. Only providers the server reports as
/// `connected` are surfaced (see [`V1Strategy::list_models`]). Pure so it is
/// unit-testable against a captured fixture.
fn parse_provider_models(v: &serde_json::Value) -> Vec<ProviderModels> {
    let Some(all) = v.get("all").and_then(|a| a.as_array()) else {
        return Vec::new();
    };
    let connected: Option<std::collections::HashSet<String>> = v
        .get("connected")
        .and_then(|c| c.as_array())
        .map(|ids| ids.iter().filter_map(|i| i.as_str()).map(String::from).collect());
    all.iter()
        .filter(|prov| {
            connected
                .as_ref()
                .is_none_or(|ids| ids.contains(prov.get("id").and_then(|i| i.as_str()).unwrap_or("")))
        })
        .filter_map(|prov| {
            let id = prov.get("id").and_then(|i| i.as_str())?.to_string();
            let models: Vec<ModelOption> = prov
                .get("models")
                .and_then(|m| m.as_object())
                .map(|m| {
                    m.iter()
                        .map(|(model_id, m_info)| ModelOption {
                            id: model_id.clone(),
                            variants: declared_variants(m_info),
                        })
                        .collect()
                })
                .unwrap_or_default();
            if models.is_empty() {
                None
            } else {
                Some(ProviderModels { provider: id, models })
            }
        })
        .collect()
}

/// The variant names a model declares, per `GET /provider`. The server
/// serializes `model.variants` as a Record keyed by variant id (`{"low": {...},
/// "high": {...}, ...}`); very old servers sent an array of `{id, ...}` objects.
/// Accept both so `/think` and the `/model` auto-clear resolve the same set
/// either way.
fn declared_variants(m_info: &serde_json::Value) -> Vec<String> {
    let Some(variants) = m_info.get("variants") else {
        return Vec::new();
    };
    if let Some(obj) = variants.as_object() {
        return obj.keys().cloned().collect();
    }
    variants
        .as_array()
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.get("id").and_then(|i| i.as_str()).map(String::from))
                .collect()
        })
        .unwrap_or_default()
}
