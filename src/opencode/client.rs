#![allow(dead_code)] // protocol types — field coverage matches server contract

use crate::config::OpenCodeConfig;
use base64::Engine;
use serde::{Deserialize, Serialize};
use std::sync::Arc;

/// Lightweight HTTP client for the OpenCode Server REST API.
///
/// The server cola talks to can be restarted/replaced at runtime (another tool
/// like OpenChamber manages it), which may change its port and password. So the
/// endpoint — base URL + auth — is held in a `RwLock` and can be swapped via
/// [`Client::reconnect`] without dropping the shared `Arc<dyn Backend>`.
pub struct Client {
    http: Arc<std::sync::RwLock<HttpHandle>>,
    /// The username used for Basic auth (reused on reconnect).
    username: Option<String>,
    /// Default model for new sessions, e.g. "opencode/deepseek-v4-flash-free"
    pub model: Option<ModelInfo>,
}

/// The live HTTP handle: the reqwest client (with its baked-in auth headers)
/// and the base URL. Replaced wholesale on reconnect.
struct HttpHandle {
    client: reqwest::Client,
    base_url: String,
}

/// Build a reqwest client with the standard JSON content-type and optional
/// Basic auth (OpenCode server password). Reused on reconnect so a changed
/// password produces a fresh client.
fn build_http_client(username: &Option<String>, password: &Option<String>) -> reqwest::Client {
    let mut headers = reqwest::header::HeaderMap::new();
    headers.insert(reqwest::header::CONTENT_TYPE, "application/json".parse().unwrap());

    let mut builder = reqwest::Client::builder().default_headers(headers);

    if let (Some(user), Some(pass)) = (username, password) {
        let auth = format!("{}:{}", user, pass);
        let encoded = base64::engine::general_purpose::STANDARD.encode(auth);
        let auth_value = reqwest::header::HeaderValue::from_str(&format!("Basic {}", encoded)).unwrap();
        let mut default_headers = reqwest::header::HeaderMap::new();
        default_headers.insert(reqwest::header::AUTHORIZATION, auth_value);
        builder = builder.default_headers(default_headers);
    }

    builder.build().expect("failed to build reqwest client")
}

impl Clone for Client {
    fn clone(&self) -> Self {
        Self {
            http: Arc::clone(&self.http),
            username: self.username.clone(),
            model: self.model.clone(),
        }
    }
}

impl Client {
    pub fn new(cfg: OpenCodeConfig) -> Self {
        Self {
            http: Arc::new(std::sync::RwLock::new(HttpHandle {
                client: build_http_client(&cfg.username, &cfg.password),
                base_url: cfg.url.trim_end_matches('/').to_string(),
            })),
            username: cfg.username.clone(),
            model: parse_model(&cfg.model),
        }
    }

    /// Point this client at a different server (port/password changed because
    /// the old one was restarted/replaced). Rare; only called by the reconnect
    /// loop when discovery finds the attached server is gone.
    pub async fn reconnect(&self, url: &str, password: &str) {
        let handle = HttpHandle {
            client: build_http_client(&self.username, &Some(password.to_string())),
            base_url: url.trim_end_matches('/').to_string(),
        };
        let mut w = self.http.write().unwrap();
        *w = handle;
        drop(w);
        tracing::info!("reconnected opencode client to {}", url);
    }

    /// The current base URL.
    pub fn base_url(&self) -> String {
        self.http.read().unwrap().base_url.clone()
    }

    pub fn url(&self, path: &str) -> String {
        format!("{}{}", self.base_url(), path)
    }

    fn http(&self) -> reqwest::Client {
        self.http.read().unwrap().client.clone()
    }

    /// Create the session input for a new session, applying the configured
    /// default model and an optional directory.
    pub fn new_session_input(&self, directory: Option<&str>) -> CreateSessionInput {
        CreateSessionInput {
            id: None,
            agent: None,
            model: self.model.clone(),
            location: directory.map(|d| Location {
                directory: d.to_string(),
            }),
        }
    }

    /// List all sessions.
    pub async fn list_sessions(&self) -> crate::error::Result<SessionsListOutput> {
        let resp = self
            .http()
            .get(self.url("/api/session"))
            .send()
            .await?
            .error_for_status()?;
        Ok(resp.json().await?)
    }

    /// Create a new session with an optional directory and agent.
    pub async fn create_session(&self, input: &CreateSessionInput) -> crate::error::Result<Session> {
        let resp = self
            .http()
            .post(self.url("/api/session"))
            .json(input)
            .send()
            .await?
            .error_for_status()?;
        let body: CreateSessionResponse = resp.json().await?;
        Ok(body.data)
    }

    /// Send a prompt to a session using the canonical OpenCode API:
    /// `POST /session/{id}/message` with `parts`. This is the same protocol
    /// OpenChamber/TUI use, so messages land in the shared message store.
    pub async fn prompt(&self, session_id: &str, text: &str) -> crate::error::Result<PromptResponse> {
        let mut body = serde_json::json!({
            "parts": [{"type": "text", "text": text}],
        });
        if let Some(model) = &self.model {
            body["model"] = serde_json::json!({
                "providerID": model.provider_id,
                "modelID": model.id,
            });
        }
        let resp = self
            .http()
            .post(self.url(&format!("/session/{}/message", session_id)))
            .json(&body)
            .send()
            .await?;
        if resp.status().is_success() {
            let text_body = resp.text().await?;
            tracing::info!("prompt response: {}", &text_body[..text_body.len().min(500)]);
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
                parts: parsed
                    .get("parts")
                    .cloned()
                    .unwrap_or(serde_json::Value::Array(vec![])),
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

        // Fallback: older servers expose `/api/session/{id}/prompt` with `prompt` payload.
        tracing::warn!(
            "canonical /session/{}/message failed ({}), falling back to /api/session/{}/prompt",
            session_id,
            resp.status(),
            session_id
        );
        let body = serde_json::json!({
            "prompt": {
                "text": text,
            },
            "delivery": "steer",
        });
        let resp = self
            .http()
            .post(self.url(&format!("/api/session/{}/prompt", session_id)))
            .json(&body)
            .send()
            .await?
            .error_for_status()?;
        let text_body = resp.text().await?;
        tracing::info!("prompt response: {}", &text_body[..text_body.len().min(300)]);
        let body: PromptOutput = serde_json::from_str(&text_body).map_err(|e| {
            crate::error::BridgeError::OpenCode(format!(
                "prompt decode: {e} — body: {}",
                &text_body[..text_body.len().min(300)]
            ))
        })?;
        Ok(body.data)
    }

    /// Fire-and-forget prompt: `POST /session/{id}/prompt_async`. OpenCode
    /// immediately persists the user message (`createUserMessage`) and forks a
    /// run (`Effect.forkIn`), returning 204 — the caller doesn't block until the
    /// turn finishes. Used by the supplement path: while a turn is in flight we
    /// send the new message here so it lands in the DB and the running loop
    /// picks it up at the next tool boundary (merged into the current turn),
    /// without a second synchronous prompt blocking the WS read loop.
    pub async fn prompt_async(&self, session_id: &str, text: &str) -> crate::error::Result<()> {
        let mut body = serde_json::json!({
            "parts": [{"type": "text", "text": text}],
        });
        if let Some(model) = &self.model {
            body["model"] = serde_json::json!({
                "providerID": model.provider_id,
                "modelID": model.id,
            });
        }
        let resp = self
            .http()
            .post(self.url(&format!("/session/{}/prompt_async", session_id)))
            .json(&body)
            .send()
            .await?;
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
    pub async fn session_info(
        &self,
        session_id: &str,
        directory: Option<&str>,
    ) -> crate::error::Result<SessionInfo> {
        let mut url = reqwest::Url::parse(&self.url(&format!("/session/{}", session_id)))?;
        if let Some(d) = directory {
            url.query_pairs_mut().append_pair("directory", d);
        }
        let resp = self.http().get(url).send().await?.error_for_status()?;
        Ok(resp.json().await?)
    }

    /// Reply to a permission request (canonical: `POST /permission/{id}/reply`).
    /// `directory` routes the request to the instance that owns the permission —
    /// without it the server checks the cwd instance and returns 404/400.
    pub async fn reply_permission(
        &self,
        request_id: &str,
        reply: &str,
        directory: Option<&str>,
    ) -> crate::error::Result<()> {
        let body = serde_json::json!({ "reply": reply });
        let mut url = reqwest::Url::parse(&self.url(&format!("/permission/{}/reply", request_id)))?;
        if let Some(d) = directory {
            url.query_pairs_mut().append_pair("directory", d);
        }
        self.http()
            .post(url)
            .json(&body)
            .send()
            .await?
            .error_for_status()?;
        Ok(())
    }

    /// List pending permissions for an instance (canonical: `GET /permission`).
    /// `directory` selects the instance; without it only the server cwd's
    /// instance is checked, so permissions for sessions in other directories
    /// would be missed.
    pub async fn list_permissions(
        &self,
        directory: Option<&str>,
    ) -> crate::error::Result<Vec<PermissionRequest>> {
        let mut url = reqwest::Url::parse(&self.url("/permission"))?;
        if let Some(d) = directory {
            url.query_pairs_mut().append_pair("directory", d);
        }
        let resp = self.http().get(url).send().await?;
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

    /// Fetch all messages (with parts) for a session (canonical: `GET /session/{id}/message`).
    pub async fn messages(&self, session_id: &str) -> crate::error::Result<Vec<SessionMessage>> {
        let resp = self
            .http()
            .get(self.url(&format!("/session/{}/message", session_id)))
            .send()
            .await?
            .error_for_status()?;
        Ok(resp.json().await?)
    }

    /// List pending question requests for an instance (canonical: `GET /question`).
    pub async fn list_questions(
        &self,
        directory: Option<&str>,
    ) -> crate::error::Result<Vec<QuestionRequest>> {
        let mut url = reqwest::Url::parse(&self.url("/question"))?;
        if let Some(d) = directory {
            url.query_pairs_mut().append_pair("directory", d);
        }
        let resp = self.http().get(url).send().await?;
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
    pub async fn model_context_window(
        &self,
        provider: &str,
        model: &str,
    ) -> crate::error::Result<Option<i64>> {
        let resp = self.http().get(self.url("/provider")).send().await?;
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
    pub async fn reply_question(
        &self,
        request_id: &str,
        answers: &[Vec<String>],
        directory: Option<&str>,
    ) -> crate::error::Result<()> {
        let body = serde_json::json!({ "answers": answers });
        let mut url = reqwest::Url::parse(&self.url(&format!("/question/{}/reply", request_id)))?;
        if let Some(d) = directory {
            url.query_pairs_mut().append_pair("directory", d);
        }
        self.http()
            .post(url)
            .json(&body)
            .send()
            .await?
            .error_for_status()?;
        Ok(())
    }

    /// Reject a question request (canonical: `POST /question/{id}/reject`).
    pub async fn reject_question(
        &self,
        request_id: &str,
        directory: Option<&str>,
    ) -> crate::error::Result<()> {
        let mut url = reqwest::Url::parse(&self.url(&format!("/question/{}/reject", request_id)))?;
        if let Some(d) = directory {
            url.query_pairs_mut().append_pair("directory", d);
        }
        self.http().post(url).send().await?.error_for_status()?;
        Ok(())
    }

    /// Interrupt an active session.
    pub async fn interrupt(&self, session_id: &str) -> crate::error::Result<()> {
        // Canonical path has NO `/api` prefix (see AGENTS.md pitfall #1); the
        // old `/api/session/{id}/interrupt` 404'd so `/stop` silently failed.
        self.http()
            .post(self.url(&format!("/session/{}/abort", session_id)))
            .send()
            .await?
            .error_for_status()?;
        Ok(())
    }

    /// Compact a session's context.
    pub async fn compact(&self, session_id: &str) -> crate::error::Result<()> {
        self.http()
            .post(self.url(&format!("/api/session/{}/compact", session_id)))
            .send()
            .await?
            .error_for_status()?;
        Ok(())
    }

    /// Switch the agent for a session.
    pub async fn switch_agent(&self, session_id: &str, agent: &str) -> crate::error::Result<()> {
        let body = serde_json::json!({ "agent": agent });
        self.http()
            .post(self.url(&format!("/api/session/{}/agent", session_id)))
            .json(&body)
            .send()
            .await?
            .error_for_status()?;
        Ok(())
    }

    /// Switch the model for a session.
    pub async fn switch_model(&self, session_id: &str, model: &str) -> crate::error::Result<()> {
        let body = serde_json::json!({ "model": model });
        self.http()
            .post(self.url(&format!("/api/session/{}/model", session_id)))
            .json(&body)
            .send()
            .await?
            .error_for_status()?;
        Ok(())
    }
}

#[derive(Debug, Deserialize)]
pub struct CreateSessionResponse {
    pub data: Session,
}

#[derive(Debug, Deserialize)]
pub struct SessionsListOutput {
    pub data: Vec<Session>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Session {
    pub id: String,
    pub project_id: Option<String>,
    pub agent: Option<String>,
    pub title: Option<String>,
    pub location: Option<serde_json::Value>,
    pub cost: Option<f64>,
    pub time: Option<SessionTime>,
}

/// Minimal `Session.Info` (canonical `GET /session/{id}`) — enough to resolve
/// a sub-task session's parent.
#[derive(Debug, Clone, Deserialize)]
pub struct SessionInfo {
    pub id: String,
    #[serde(rename = "parentID")]
    pub parent_id: Option<String>,
    /// The server-managed session title (what OpenChamber shows).
    #[serde(default)]
    pub title: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionTime {
    pub created: i64,
    pub updated: i64,
}

#[derive(Debug, Serialize)]
pub struct CreateSessionInput {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agent: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<ModelInfo>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub location: Option<Location>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ModelInfo {
    pub id: String,
    #[serde(rename = "providerID")]
    pub provider_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub variant: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct Location {
    pub directory: String,
}

#[derive(Debug, Deserialize)]
pub struct PromptOutput {
    pub data: PromptResponse,
}

#[derive(Debug, Deserialize)]
pub struct PromptResponse {
    pub id: String,
    #[serde(rename = "sessionID")]
    pub session_id: Option<String>,
    #[serde(rename = "admittedSeq")]
    pub admitted_seq: Option<i64>,
    /// The user message this turn answers (from `info.parentID`).
    #[serde(rename = "parentID")]
    pub parent_id: Option<String>,
    /// Error on the assistant message (e.g. provider 503), from `info.error`.
    pub error: Option<String>,
    /// Parts of the assistant response (from the canonical API).
    #[serde(default)]
    pub parts: serde_json::Value,
}

/// A message returned by `GET /session/{id}/message`: `{ info, parts }`.
#[derive(Debug, Deserialize)]
pub struct SessionMessage {
    pub info: MessageInfo,
    #[serde(default)]
    pub parts: serde_json::Value,
}

#[derive(Debug, Deserialize)]
pub struct MessageInfo {
    pub id: String,
    #[serde(default)]
    pub role: Option<String>,
    #[serde(rename = "parentID")]
    pub parent_id: Option<String>,
    #[serde(default)]
    pub time: Option<MessageTime>,
    #[serde(rename = "modelID", default)]
    pub model_id: Option<String>,
    #[serde(rename = "providerID", default)]
    pub provider_id: Option<String>,
    #[serde(default)]
    pub tokens: Option<MessageTokens>,
}

/// Token usage carried on an assistant message's `info.tokens`.
#[derive(Debug, Default, Deserialize)]
pub struct MessageTokens {
    #[serde(default)]
    pub input: i64,
    #[serde(default)]
    pub output: i64,
    #[serde(default)]
    pub total: i64,
    #[serde(default)]
    pub cache: Option<MessageTokenCache>,
}

#[derive(Debug, Default, Deserialize)]
pub struct MessageTokenCache {
    #[serde(default)]
    pub read: i64,
    #[serde(default)]
    pub write: i64,
}

impl MessageTokens {
    /// The context the model actually consumed: `total` when the server reports
    /// it, else the cached prefix + the fresh input. (`input` alone is only the
    /// per-message delta — mostly cache reads — so it understates context a lot.)
    pub fn context_used(&self) -> i64 {
        let cache_read = self.cache.as_ref().map(|c| c.read).unwrap_or(0);
        let fallback = self.input + cache_read;
        if self.total > 0 { self.total } else { fallback }
    }
}

#[derive(Debug, Deserialize)]
pub struct MessageTime {
    pub created: i64,
}

/// SSE event types for the global event stream.
#[derive(Debug, Deserialize)]
#[allow(dead_code)] // protocol definition — variant fields match server contract
#[serde(tag = "type")]
pub enum OpenCodeEvent {
    #[serde(rename = "server.connected")]
    ServerConnected,
    #[serde(rename = "session.next.prompted")]
    SessionPrompted { id: String, data: PromptedData },
    #[serde(rename = "session.next.prompt.admitted")]
    SessionPromptAdmitted { id: String, data: PromptedData },
    #[serde(rename = "session.next.step.started")]
    StepStarted { id: String, data: StepStartedData },
    #[serde(rename = "session.next.step.ended")]
    StepEnded { id: String, data: StepEndedData },
    #[serde(rename = "session.next.step.failed")]
    StepFailed { id: String, data: StepFailedData },
    #[serde(rename = "session.next.text.started")]
    TextStarted { id: String, data: TextStartedData },
    #[serde(rename = "session.next.text.delta")]
    TextDelta { id: String, data: TextDeltaData },
    #[serde(rename = "session.next.text.ended")]
    TextEnded { id: String, data: TextEndedData },
    #[serde(rename = "session.next.reasoning.started")]
    ReasoningStarted { id: String, data: ReasoningStartedData },
    #[serde(rename = "session.next.reasoning.delta")]
    ReasoningDelta { id: String, data: ReasoningDeltaData },
    #[serde(rename = "session.next.reasoning.ended")]
    ReasoningEnded { id: String, data: ReasoningEndedData },
    #[serde(rename = "session.next.tool.called")]
    ToolCalled { id: String, data: ToolCalledData },
    #[serde(rename = "session.next.tool.success")]
    ToolSuccess { id: String, data: ToolSuccessData },
    #[serde(rename = "session.next.tool.failed")]
    ToolFailed { id: String, data: ToolFailedData },
    #[serde(rename = "session.next.tool.progress")]
    ToolProgress { id: String, data: ToolProgressData },
    #[serde(rename = "session.next.shell.started")]
    ShellStarted { id: String, data: ShellStartedData },
    #[serde(rename = "session.next.shell.ended")]
    ShellEnded { id: String, data: ShellEndedData },
    #[serde(rename = "permission.v2.asked")]
    PermissionAsked { id: String, data: PermissionAskedData },
    #[serde(rename = "permission.v2.replied")]
    PermissionReplied { id: String, data: PermissionRepliedData },
    #[serde(rename = "question.v2.asked")]
    QuestionAsked { id: String, data: QuestionAskedData },
    #[serde(rename = "question.v2.replied")]
    QuestionReplied { id: String, data: QuestionRepliedData },
    #[serde(rename = "question.v2.rejected")]
    QuestionRejected { id: String, data: QuestionRejectedData },
    #[serde(rename = "session.next.compaction.started")]
    CompactionStarted { id: String, data: CompactionStartedData },
    #[serde(rename = "session.next.compaction.ended")]
    CompactionEnded { id: String, data: CompactionEndedData },
    #[serde(other)]
    Unknown,
}

#[allow(dead_code)] // protocol definition
#[derive(Debug, Deserialize)]
pub struct PromptedData {
    #[serde(rename = "sessionID")]
    pub session_id: Option<String>,
    #[serde(rename = "messageID")]
    pub message_id: Option<String>,
    pub prompt: Option<serde_json::Value>,
    pub delivery: Option<String>,
    pub timestamp: Option<i64>,
}

#[derive(Debug, Deserialize)]
pub struct PromptAdmittedData {
    #[serde(rename = "sessionID")]
    pub session_id: Option<String>,
    #[serde(rename = "messageID")]
    pub message_id: Option<String>,
    pub timestamp: Option<i64>,
}

#[derive(Debug, Deserialize)]
pub struct StepStartedData {
    #[serde(rename = "sessionID")]
    pub session_id: Option<String>,
    #[serde(rename = "assistantMessageID")]
    pub assistant_message_id: Option<String>,
    pub agent: Option<String>,
    pub model: Option<serde_json::Value>,
    pub snapshot: Option<String>,
    pub timestamp: Option<i64>,
}

#[derive(Debug, Deserialize)]
pub struct StepEndedData {
    #[serde(rename = "sessionID")]
    pub session_id: Option<String>,
    #[serde(rename = "assistantMessageID")]
    pub assistant_message_id: Option<String>,
    pub finish: Option<String>,
    pub cost: Option<f64>,
    pub tokens: Option<TokenCount>,
    pub timestamp: Option<i64>,
}

#[derive(Debug, Deserialize)]
pub struct StepFailedData {
    #[serde(rename = "sessionID")]
    pub session_id: Option<String>,
    #[serde(rename = "assistantMessageID")]
    pub assistant_message_id: Option<String>,
    pub error: Option<ErrorMessage>,
    pub timestamp: Option<i64>,
}

#[derive(Debug, Deserialize)]
pub struct TextStartedData {
    #[serde(rename = "sessionID")]
    pub session_id: Option<String>,
    #[serde(rename = "assistantMessageID")]
    pub assistant_message_id: Option<String>,
    #[serde(rename = "textID")]
    pub text_id: Option<String>,
    pub timestamp: Option<i64>,
}

#[derive(Debug, Deserialize)]
pub struct TextDeltaData {
    #[serde(rename = "sessionID")]
    pub session_id: Option<String>,
    #[serde(rename = "assistantMessageID")]
    pub assistant_message_id: Option<String>,
    #[serde(rename = "textID")]
    pub text_id: Option<String>,
    pub delta: Option<String>,
    pub timestamp: Option<i64>,
}

#[derive(Debug, Deserialize)]
pub struct TextEndedData {
    #[serde(rename = "sessionID")]
    pub session_id: Option<String>,
    #[serde(rename = "assistantMessageID")]
    pub assistant_message_id: Option<String>,
    #[serde(rename = "textID")]
    pub text_id: Option<String>,
    pub text: Option<String>,
    pub timestamp: Option<i64>,
}

#[derive(Debug, Deserialize)]
pub struct ReasoningStartedData {
    #[serde(rename = "sessionID")]
    pub session_id: Option<String>,
    #[serde(rename = "assistantMessageID")]
    pub assistant_message_id: Option<String>,
    #[serde(rename = "reasoningID")]
    pub reasoning_id: Option<String>,
    pub timestamp: Option<i64>,
}

#[derive(Debug, Deserialize)]
pub struct ReasoningDeltaData {
    #[serde(rename = "sessionID")]
    pub session_id: Option<String>,
    #[serde(rename = "assistantMessageID")]
    pub assistant_message_id: Option<String>,
    #[serde(rename = "reasoningID")]
    pub reasoning_id: Option<String>,
    pub delta: Option<String>,
    pub timestamp: Option<i64>,
}

#[derive(Debug, Deserialize)]
pub struct ReasoningEndedData {
    #[serde(rename = "sessionID")]
    pub session_id: Option<String>,
    #[serde(rename = "assistantMessageID")]
    pub assistant_message_id: Option<String>,
    #[serde(rename = "reasoningID")]
    pub reasoning_id: Option<String>,
    pub text: Option<String>,
    pub timestamp: Option<i64>,
}

#[derive(Debug, Deserialize)]
pub struct ToolCalledData {
    #[serde(rename = "sessionID")]
    pub session_id: Option<String>,
    #[serde(rename = "assistantMessageID")]
    pub assistant_message_id: Option<String>,
    #[serde(rename = "callID")]
    pub call_id: Option<String>,
    pub tool: Option<String>,
    pub input: Option<serde_json::Value>,
    pub timestamp: Option<i64>,
}

#[derive(Debug, Deserialize)]
pub struct ToolSuccessData {
    #[serde(rename = "sessionID")]
    pub session_id: Option<String>,
    #[serde(rename = "assistantMessageID")]
    pub assistant_message_id: Option<String>,
    #[serde(rename = "callID")]
    pub call_id: Option<String>,
    pub content: Option<Vec<ContentPart>>,
    pub result: Option<serde_json::Value>,
    pub timestamp: Option<i64>,
}

#[derive(Debug, Deserialize)]
pub struct ToolFailedData {
    #[serde(rename = "sessionID")]
    pub session_id: Option<String>,
    #[serde(rename = "assistantMessageID")]
    pub assistant_message_id: Option<String>,
    #[serde(rename = "callID")]
    pub call_id: Option<String>,
    pub error: Option<ErrorMessage>,
    pub timestamp: Option<i64>,
}

#[derive(Debug, Deserialize)]
pub struct ToolProgressData {
    #[serde(rename = "sessionID")]
    pub session_id: Option<String>,
    #[serde(rename = "assistantMessageID")]
    pub assistant_message_id: Option<String>,
    #[serde(rename = "callID")]
    pub call_id: Option<String>,
    pub content: Option<Vec<ContentPart>>,
    pub timestamp: Option<i64>,
}

#[derive(Debug, Deserialize)]
pub struct ShellStartedData {
    #[serde(rename = "sessionID")]
    pub session_id: Option<String>,
    #[serde(rename = "messageID")]
    pub message_id: Option<String>,
    #[serde(rename = "callID")]
    pub call_id: Option<String>,
    pub command: Option<String>,
    pub timestamp: Option<i64>,
}

#[derive(Debug, Deserialize)]
pub struct ShellEndedData {
    #[serde(rename = "sessionID")]
    pub session_id: Option<String>,
    #[serde(rename = "callID")]
    pub call_id: Option<String>,
    pub output: Option<String>,
    pub timestamp: Option<i64>,
}

#[derive(Debug, Deserialize)]
pub struct PermissionAskedData {
    #[serde(rename = "sessionID")]
    pub session_id: Option<String>,
    #[serde(default)]
    pub id: Option<String>,
    #[serde(default)]
    pub action: Option<String>,
    #[serde(default)]
    pub resources: Option<Vec<String>>,
    #[serde(default)]
    pub save: Option<Vec<String>>,
}

#[derive(Debug, Deserialize)]
pub struct PermissionRepliedData {
    #[serde(rename = "sessionID")]
    pub session_id: Option<String>,
    pub request_id: Option<String>,
    pub reply: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct QuestionAskedData {
    #[serde(rename = "sessionID")]
    pub session_id: Option<String>,
    pub questions: Option<Vec<QuestionInfo>>,
    pub tool: Option<serde_json::Value>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct QuestionInfo {
    pub question: String,
    pub header: String,
    pub options: Vec<QuestionOption>,
    pub multiple: Option<bool>,
    pub custom: Option<bool>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct QuestionOption {
    pub label: String,
    pub description: String,
}

#[derive(Debug, Deserialize)]
pub struct QuestionRepliedData {
    #[serde(rename = "sessionID")]
    pub session_id: Option<String>,
    pub request_id: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct QuestionRejectedData {
    #[serde(rename = "sessionID")]
    pub session_id: Option<String>,
    pub request_id: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct CompactionStartedData {
    #[serde(rename = "sessionID")]
    pub session_id: Option<String>,
    #[serde(rename = "messageID")]
    pub message_id: Option<String>,
    pub reason: Option<String>,
    pub timestamp: Option<i64>,
}

#[derive(Debug, Deserialize)]
pub struct CompactionEndedData {
    #[serde(rename = "sessionID")]
    pub session_id: Option<String>,
    #[serde(rename = "messageID")]
    pub message_id: Option<String>,
    pub reason: Option<String>,
    pub text: Option<String>,
    pub recent: Option<String>,
    pub timestamp: Option<i64>,
}

#[derive(Debug, Deserialize)]
pub struct ContentPart {
    #[serde(rename = "type")]
    pub content_type: String,
    pub text: Option<String>,
    pub file: Option<serde_json::Value>,
}

#[derive(Debug, Deserialize)]
pub struct TokenCount {
    pub input: Option<i64>,
    pub output: Option<i64>,
    pub reasoning: Option<i64>,
    pub cache: Option<serde_json::Value>,
}

#[derive(Debug, Deserialize)]
pub struct ErrorMessage {
    #[serde(rename = "type")]
    pub error_type: Option<String>,
    pub message: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct PermissionListResponse {
    pub data: Vec<PermissionRequest>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct PermissionRequest {
    #[serde(rename = "id")]
    pub request_id: String,
    #[serde(rename = "sessionID")]
    pub session_id: Option<String>,
    #[serde(default)]
    pub permission: Option<String>,
    #[serde(default)]
    pub patterns: Vec<String>,
    #[serde(default)]
    pub metadata: Option<serde_json::Value>,
    #[serde(default)]
    pub always: Vec<String>,
}

/// A pending question request (`GET /question`): the AI asks the user one or
/// more questions and blocks until answered.
#[derive(Debug, Clone, Deserialize)]
pub struct QuestionRequest {
    pub id: String,
    #[serde(rename = "sessionID")]
    pub session_id: String,
    pub questions: Vec<QuestionInfo>,
}

/// Parse "provider/model" (optionally "provider/model/variant") into ModelInfo.
fn parse_model(spec: &str) -> Option<ModelInfo> {
    let mut parts = spec.splitn(3, '/');
    let provider = parts.next()?;
    let id = parts.next()?;
    if provider.is_empty() || id.is_empty() {
        return None;
    }
    Some(ModelInfo {
        id: id.to_string(),
        provider_id: provider.to_string(),
        variant: parts.next().map(|s| s.to_string()),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn context_used_prefers_total_over_input_delta() {
        // Real shape: `input` is only the per-message delta; the cached prefix
        // is the bulk of the context. Using `input` alone understates usage.
        let tokens: MessageTokens = serde_json::from_str(
            r#"{"total":612920,"input":263,"output":308,"reasoning":253,
                "cache":{"write":0,"read":612096}}"#,
        )
        .unwrap();
        assert_eq!(tokens.context_used(), 612920);

        // No `total` (older server): input + cache.read.
        let tokens: MessageTokens = serde_json::from_str(r#"{"input":263,"cache":{"read":612096}}"#).unwrap();
        assert_eq!(tokens.context_used(), 612359);

        // Degenerate: neither present.
        let tokens: MessageTokens = serde_json::from_str(r#"{}"#).unwrap();
        assert_eq!(tokens.context_used(), 0);
    }

    #[test]
    fn parse_server_connected() {
        let json = r#"{"type":"server.connected","id":"evt_1","data":{}}"#;
        let event = serde_json::from_str::<OpenCodeEvent>(json).unwrap();
        match event {
            OpenCodeEvent::ServerConnected => {}
            other => panic!("expected ServerConnected, got {:?}", other),
        }
    }

    #[test]
    fn parse_step_started() {
        let json = r#"{
            "id": "evt_2",
            "type": "session.next.step.started",
            "data": {
                "sessionID": "ses_abc",
                "assistantMessageID": "msg_123",
                "agent": "primary",
                "model": {"id": "claude-sonnet-4-5", "providerID": "anthropic"},
                "snapshot": "abc123",
                "timestamp": 1700000000000
            }
        }"#;
        let event = serde_json::from_str::<OpenCodeEvent>(json).unwrap();
        match event {
            OpenCodeEvent::StepStarted { data, .. } => {
                assert_eq!(data.session_id.as_deref(), Some("ses_abc"));
                assert_eq!(data.assistant_message_id.as_deref(), Some("msg_123"));
                assert_eq!(data.agent.as_deref(), Some("primary"));
            }
            other => panic!("expected StepStarted, got {:?}", other),
        }
    }

    #[test]
    fn parse_text_ended() {
        let json = r#"{
            "id": "evt_3",
            "type": "session.next.text.ended",
            "data": {
                "sessionID": "ses_abc",
                "assistantMessageID": "msg_123",
                "textID": "txt_1",
                "text": "Hello, world!",
                "timestamp": 1700000001000
            }
        }"#;
        let event = serde_json::from_str::<OpenCodeEvent>(json).unwrap();
        match event {
            OpenCodeEvent::TextEnded { data, .. } => {
                assert_eq!(data.session_id.as_deref(), Some("ses_abc"));
                assert_eq!(data.text.as_deref(), Some("Hello, world!"));
            }
            other => panic!("expected TextEnded, got {:?}", other),
        }
    }

    #[test]
    fn parse_reasoning_ended() {
        let json = r#"{
            "id": "evt_4",
            "type": "session.next.reasoning.ended",
            "data": {
                "sessionID": "ses_abc",
                "assistantMessageID": "msg_123",
                "reasoningID": "rsn_1",
                "text": "Let me think about this...",
                "timestamp": 1700000002000
            }
        }"#;
        let event = serde_json::from_str::<OpenCodeEvent>(json).unwrap();
        match event {
            OpenCodeEvent::ReasoningEnded { data, .. } => {
                assert_eq!(data.session_id.as_deref(), Some("ses_abc"));
                assert_eq!(data.text.as_deref(), Some("Let me think about this..."));
            }
            other => panic!("expected ReasoningEnded, got {:?}", other),
        }
    }

    #[test]
    fn parse_tool_called() {
        let json = r#"{
            "id": "evt_5",
            "type": "session.next.tool.called",
            "data": {
                "sessionID": "ses_abc",
                "assistantMessageID": "msg_123",
                "callID": "call_1",
                "tool": "bash",
                "input": {"command": "ls -la", "cwd": "/tmp"},
                "timestamp": 1700000003000
            }
        }"#;
        let event = serde_json::from_str::<OpenCodeEvent>(json).unwrap();
        match event {
            OpenCodeEvent::ToolCalled { data, .. } => {
                assert_eq!(data.session_id.as_deref(), Some("ses_abc"));
                assert_eq!(data.call_id.as_deref(), Some("call_1"));
                assert_eq!(data.tool.as_deref(), Some("bash"));
                assert!(data.input.is_some());
            }
            other => panic!("expected ToolCalled, got {:?}", other),
        }
    }

    #[test]
    fn parse_tool_success() {
        let json = r#"{
            "id": "evt_6",
            "type": "session.next.tool.success",
            "data": {
                "sessionID": "ses_abc",
                "assistantMessageID": "msg_123",
                "callID": "call_1",
                "result": {"status": "success"},
                "timestamp": 1700000004000
            }
        }"#;
        let event = serde_json::from_str::<OpenCodeEvent>(json).unwrap();
        match event {
            OpenCodeEvent::ToolSuccess { data, .. } => {
                assert_eq!(data.call_id.as_deref(), Some("call_1"));
            }
            other => panic!("expected ToolSuccess, got {:?}", other),
        }
    }

    #[test]
    fn parse_step_ended_with_tokens() {
        let json = r#"{
            "id": "evt_7",
            "type": "session.next.step.ended",
            "data": {
                "sessionID": "ses_abc",
                "assistantMessageID": "msg_123",
                "finish": "stop",
                "cost": 0.015,
                "tokens": {
                    "input": 1200,
                    "output": 300,
                    "reasoning": 150
                },
                "timestamp": 1700000005000
            }
        }"#;
        let event = serde_json::from_str::<OpenCodeEvent>(json).unwrap();
        match event {
            OpenCodeEvent::StepEnded { data, .. } => {
                assert_eq!(data.session_id.as_deref(), Some("ses_abc"));
                assert_eq!(data.finish.as_deref(), Some("stop"));
                assert_eq!(data.cost, Some(0.015));
                let tokens = data.tokens.as_ref().unwrap();
                assert_eq!(tokens.input, Some(1200));
                assert_eq!(tokens.output, Some(300));
            }
            other => panic!("expected StepEnded, got {:?}", other),
        }
    }

    #[test]
    fn parse_step_failed() {
        let json = r#"{
            "id": "evt_8",
            "type": "session.next.step.failed",
            "data": {
                "sessionID": "ses_abc",
                "assistantMessageID": "msg_123",
                "error": {
                    "type": "MessageOutputLengthError",
                    "message": "Output too long"
                },
                "timestamp": 1700000006000
            }
        }"#;
        let event = serde_json::from_str::<OpenCodeEvent>(json).unwrap();
        match event {
            OpenCodeEvent::StepFailed { data, .. } => {
                assert_eq!(data.session_id.as_deref(), Some("ses_abc"));
                let err = data.error.as_ref().unwrap();
                assert_eq!(err.message.as_deref(), Some("Output too long"));
            }
            other => panic!("expected StepFailed, got {:?}", other),
        }
    }

    #[test]
    fn parse_permission_asked() {
        let json = r#"{
            "id": "evt_9",
            "type": "permission.v2.asked",
            "data": {
                "sessionID": "ses_abc",
                "action": "bash",
                "resources": ["git status", "git diff"],
                "save": ["git status"]
            }
        }"#;
        let event = serde_json::from_str::<OpenCodeEvent>(json).unwrap();
        match event {
            OpenCodeEvent::PermissionAsked { data, .. } => {
                assert_eq!(data.session_id.as_deref(), Some("ses_abc"));
                assert_eq!(data.action.as_deref(), Some("bash"));
                assert_eq!(
                    data.resources.as_deref(),
                    Some(vec!["git status".to_string(), "git diff".to_string()].as_ref())
                );
                assert_eq!(
                    data.save.as_deref(),
                    Some(vec!["git status".to_string()].as_ref())
                );
            }
            other => panic!("expected PermissionAsked, got {:?}", other),
        }
    }

    #[test]
    fn parse_question_asked() {
        let json = r#"{
            "id": "evt_10",
            "type": "question.v2.asked",
            "data": {
                "sessionID": "ses_abc",
                "questions": [
                    {
                        "question": "Which architecture?",
                        "header": "Architecture",
                        "options": [
                            {"label": "Monolith", "description": "Single service"},
                            {"label": "Microservices", "description": "Distributed"}
                        ],
                        "multiple": false,
                        "custom": true
                    }
                ]
            }
        }"#;
        let event = serde_json::from_str::<OpenCodeEvent>(json).unwrap();
        match event {
            OpenCodeEvent::QuestionAsked { data, .. } => {
                assert_eq!(data.session_id.as_deref(), Some("ses_abc"));
                let questions = data.questions.as_ref().unwrap();
                assert_eq!(questions.len(), 1);
                assert_eq!(questions[0].header, "Architecture");
                assert_eq!(questions[0].options.len(), 2);
            }
            other => panic!("expected QuestionAsked, got {:?}", other),
        }
    }

    #[test]
    fn parse_shell_started() {
        let json = r#"{
            "id": "evt_11",
            "type": "session.next.shell.started",
            "data": {
                "sessionID": "ses_abc",
                "messageID": "msg_456",
                "callID": "sh_1",
                "command": "npm run build",
                "timestamp": 1700000007000
            }
        }"#;
        let event = serde_json::from_str::<OpenCodeEvent>(json).unwrap();
        match event {
            OpenCodeEvent::ShellStarted { data, .. } => {
                assert_eq!(data.session_id.as_deref(), Some("ses_abc"));
                assert_eq!(data.command.as_deref(), Some("npm run build"));
            }
            other => panic!("expected ShellStarted, got {:?}", other),
        }
    }

    #[test]
    fn parse_shell_ended() {
        let json = r#"{
            "id": "evt_12",
            "type": "session.next.shell.ended",
            "data": {
                "sessionID": "ses_abc",
                "callID": "sh_1",
                "output": "Build succeeded",
                "timestamp": 1700000008000
            }
        }"#;
        let event = serde_json::from_str::<OpenCodeEvent>(json).unwrap();
        match event {
            OpenCodeEvent::ShellEnded { data, .. } => {
                assert_eq!(data.output.as_deref(), Some("Build succeeded"));
            }
            other => panic!("expected ShellEnded, got {:?}", other),
        }
    }

    #[test]
    fn parse_unknown_event() {
        let json = r#"{"type":"some.unknown.event","id":"evt_x","data":{"foo":"bar"}}"#;
        let event = serde_json::from_str::<OpenCodeEvent>(json).unwrap();
        match event {
            OpenCodeEvent::Unknown => {}
            other => panic!("expected Unknown, got {:?}", other),
        }
    }
}
