#![allow(dead_code)] // protocol types — field coverage matches server contract

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
    /// Build a client bound to a server, or serverless (None) when Lazy Start
    /// hasn't spawned one yet (ADR-0013). A serverless client has an empty
    /// `base_url` and does no requests until `reconnect` points it at a real
    /// server; the username is still pinned so a later reconnect carries Basic
    /// auth with both parts.
    pub fn new(model: Option<&str>, server: Option<crate::bridge::discovery::ResolvedServer>) -> Self {
        let username = server
            .as_ref()
            .map(|s| s.username.clone())
            .unwrap_or_else(|| crate::bridge::discovery::DEFAULT_SERVER_USERNAME.to_string());
        Self {
            http: Arc::new(std::sync::RwLock::new(HttpHandle {
                client: build_http_client(
                    &Some(username.clone()),
                    &server.as_ref().map(|s| s.password.clone()),
                ),
                base_url: server
                    .as_ref()
                    .map(|s| s.url.trim_end_matches('/').to_string())
                    .unwrap_or_default(),
            })),
            username: Some(username),
            model: model.and_then(parse_model),
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

    /// List all sessions in the shared store (canonical: `GET /session`, no
    /// `/api` prefix — the legacy `/api/session` path is dead code). Without a
    /// `directory` query param the whole shared store is returned, one
    /// `Session.Info` per session (camelCase: `id`, `title`, `directory`,
    /// `parentID`, `time.created/updated`, `agent`, `model`).
    pub async fn list_sessions(&self) -> crate::error::Result<Vec<SessionListInfo>> {
        let resp = self
            .http()
            .get(self.url("/session"))
            .send()
            .await?
            .error_for_status()?;
        Ok(resp.json().await?)
    }

    /// Rename a session server-side (canonical: `PATCH /session/{id}` with
    /// `{"title": ...}`). The change is visible to every client sharing the
    /// store (OpenChamber, CLI).
    pub async fn update_session_title(&self, session_id: &str, title: &str) -> crate::error::Result<()> {
        let body = serde_json::json!({ "title": title });
        self.http()
            .patch(self.url(&format!("/session/{}", session_id)))
            .json(&body)
            .send()
            .await?
            .error_for_status()?;
        Ok(())
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
    ///
    /// `model` is a per-session override from `/model` (a parsed
    /// "provider/model"); when None the configured default model applies, and
    /// when neither is set the server uses its own default.
    ///
    /// `agent` is a per-session override from `/agent`; when Some the server
    /// uses that agent for this turn (the session's own/default agent
    /// otherwise).
    pub async fn prompt(
        &self,
        session_id: &str,
        text: &str,
        images: &[ImageInput],
        model: Option<&ModelInfo>,
        agent: Option<&str>,
    ) -> crate::error::Result<PromptResponse> {
        let mut body = serde_json::json!({
            "parts": build_parts(text, images),
        });
        inject_model(&mut body, model, self.model.as_ref());
        inject_agent(&mut body, agent);
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
    pub async fn prompt_async(
        &self,
        session_id: &str,
        text: &str,
        images: &[ImageInput],
        model: Option<&ModelInfo>,
        agent: Option<&str>,
    ) -> crate::error::Result<()> {
        let mut body = serde_json::json!({
            "parts": build_parts(text, images),
        });
        inject_model(&mut body, model, self.model.as_ref());
        inject_agent(&mut body, agent);
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

    /// Available agents (`GET /agent`), each with a `name`. Best-effort: an
    /// unreadable/cached failure returns an empty list so the `/agent` card can
    /// degrade to a plain text prompt.
    pub async fn list_agents(&self) -> Vec<crate::opencode::client::AgentInfo> {
        let Ok(resp) = self.http().get(self.url("/agent")).send().await else {
            return Vec::new();
        };
        let Ok(text) = resp.text().await else {
            return Vec::new();
        };
        serde_json::from_str::<Vec<crate::opencode::client::AgentInfo>>(&text).unwrap_or_default()
    }

    /// Available models (`GET /provider`), grouped as `provider → model ids`.
    /// Best-effort: an unreadable/cached failure returns an empty map so the
    /// `/model` card can degrade to a plain text prompt.
    pub async fn list_models(&self) -> Vec<crate::opencode::client::ProviderModels> {
        let Ok(resp) = self.http().get(self.url("/provider")).send().await else {
            return Vec::new();
        };
        let Ok(text) = resp.text().await else {
            return Vec::new();
        };
        let Ok(v) = serde_json::from_str::<serde_json::Value>(&text) else {
            return Vec::new();
        };
        let Some(all) = v.get("all").and_then(|a| a.as_array()) else {
            return Vec::new();
        };
        all.iter()
            .filter_map(|prov| {
                let id = prov.get("id").and_then(|i| i.as_str())?.to_string();
                let models: Vec<String> = prov
                    .get("models")
                    .and_then(|m| m.as_object())
                    .map(|m| m.keys().cloned().collect())
                    .unwrap_or_default();
                if models.is_empty() {
                    None
                } else {
                    Some(crate::opencode::client::ProviderModels { provider: id, models })
                }
            })
            .collect()
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
}

#[derive(Debug, Deserialize)]
pub struct CreateSessionResponse {
    pub data: Session,
}

/// One entry from the canonical `GET /session` list — `Session.Info` with the
/// fields the discovery commands need. Server JSON uses camelCase; serde
/// renames are required (AGENTS.md pitfall #2).
#[derive(Debug, Clone, Deserialize)]
pub struct SessionListInfo {
    pub id: String,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub directory: String,
    #[serde(rename = "parentID", default)]
    pub parent_id: Option<String>,
    #[serde(default)]
    pub agent: Option<String>,
    #[serde(default)]
    pub model: Option<serde_json::Value>,
    #[serde(default)]
    pub time: Option<SessionTime>,
}

impl SessionListInfo {
    /// Sub-task child sessions (created by the `task` tool) keep a `parentID`
    /// and are excluded from `/switch` auto-adoption and the default `/list`.
    pub fn is_child(&self) -> bool {
        self.parent_id.is_some()
    }
}

/// An available agent (`GET /agent`, `Agent.Info`): `name`, `mode`
/// (`primary`/`subagent`/`all`), optional `description`. Used by the `/agent`
/// card to offer a picker.
#[derive(Debug, Clone, Deserialize)]
pub struct AgentInfo {
    pub name: String,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub mode: Option<String>,
    #[serde(default)]
    pub hidden: Option<bool>,
}

/// A provider's available models (`GET /provider`), rendered as one `/model`
/// card row per provider with each model as a selectable option.
#[derive(Debug, Clone)]
pub struct ProviderModels {
    pub provider: String,
    pub models: Vec<String>,
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
    /// When set, the session is archived (`time.archived`, a timestamp).
    #[serde(default)]
    pub archived: Option<i64>,
}

impl SessionTime {
    pub fn is_archived(&self) -> bool {
        self.archived.is_some()
    }
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

/// An image attached to a prompt, sent as a data-URL `file` part
/// (`{type:"file", mime, url:"data:<mime>;base64,..."}`). Requires a
/// vision-capable model; unsupported models surface an error.
#[derive(Debug, Clone)]
pub struct ImageInput {
    pub mime: String,
    pub data_base64: String,
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
pub(crate) fn parse_model(spec: &str) -> Option<ModelInfo> {
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

/// Attach the effective model to a prompt body: a per-session override wins
/// over the configured default; when neither is set the server uses its own
/// default. Shared by `prompt` and `prompt_async`.
fn inject_model(body: &mut serde_json::Value, override_: Option<&ModelInfo>, configured: Option<&ModelInfo>) {
    if let Some(model) = override_.or(configured) {
        body["model"] = serde_json::json!({
            "providerID": model.provider_id,
            "modelID": model.id,
        });
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_session_list_info_camelcase_fixture() {
        // A real `GET /session` entry (Session.Info, camelCase). ParentID/time
        // must map onto the serde-renamed fields (AGENTS.md pitfall #2).
        let json = r#"{
            "id": "ses_alpha01",
            "slug": "alpha",
            "projectID": "proj_x",
            "directory": "/work/cola",
            "parentID": "ses_parent",
            "title": "重写登录模块",
            "agent": "build",
            "model": {"id": "deepseek-v4-flash", "providerID": "opencode-go"},
            "time": {"created": 1700000000000, "updated": 1700000100000},
            "version": "1"
        }"#;
        let info: SessionListInfo = serde_json::from_str(json).unwrap();
        assert_eq!(info.id, "ses_alpha01");
        assert_eq!(info.title, "重写登录模块");
        assert_eq!(info.directory, "/work/cola");
        assert_eq!(info.parent_id.as_deref(), Some("ses_parent"));
        assert_eq!(info.agent.as_deref(), Some("build"));
        assert_eq!(info.time.as_ref().unwrap().created, 1700000000000);
        assert_eq!(info.time.as_ref().unwrap().updated, 1700000100000);
        assert!(info.is_child());
    }

    #[test]
    fn child_session_list_info_is_child() {
        let info: SessionListInfo = serde_json::from_str(
            r#"{"id":"ses_child","title":"Child session - x","directory":"/w",
                "parentID":"ses_parent","time":{"created":1,"updated":2}}"#,
        )
        .unwrap();
        assert!(info.is_child());
        assert_eq!(info.parent_id.as_deref(), Some("ses_parent"));
    }

    #[test]
    fn archived_session_time_reports_archived() {
        let info: SessionListInfo = serde_json::from_str(
            r#"{"id":"ses_a","title":"t","directory":"/w",
                "time":{"created":1,"updated":2,"archived":3}}"#,
        )
        .unwrap();
        assert!(info.time.as_ref().unwrap().is_archived());
    }

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
}
