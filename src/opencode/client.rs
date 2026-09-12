use base64::Engine;
use std::sync::Arc;

// Compatibility shims: every `crate::opencode::client::X` path that predates the
// DTO/parser move keeps resolving, so call sites did not have to churn (C3
// precedent).
pub(crate) use super::parsing::*;
pub use super::types::*;

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
    build_http_client_with(username, password, false)
}

/// The shared transport builder. `no_proxy` is set only by wire tests: their
/// fake server is loopback, and a developer shell that exports `http_proxy`
/// must not intercept it — production keeps honoring env proxies (ticket 09).
fn build_http_client_with(
    username: &Option<String>,
    password: &Option<String>,
    no_proxy: bool,
) -> reqwest::Client {
    let mut headers = reqwest::header::HeaderMap::new();
    headers.insert(reqwest::header::CONTENT_TYPE, "application/json".parse().unwrap());

    let mut builder = reqwest::Client::builder()
        .default_headers(headers)
        // Bound TCP connect only — NOT a total request timeout. A prompt POST
        // legitimately runs for many minutes, and a total timeout would abort
        // real turns. Long waits on an established connection are bounded by
        // the callers that can afford to give up (e.g. the request poller).
        .connect_timeout(std::time::Duration::from_secs(10));

    if no_proxy {
        builder = builder.no_proxy();
    }

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

/// Apply the shared policy for a request-reply endpoint's response: a 404
/// means the request was already resolved elsewhere — benign and expected (a
/// double-click, another client, or a click replayed after a cola restart) —
/// so it maps to `BridgeError::NotFound`, which the bridge renders as a
/// neutral "already handled" card rather than a failure. Any other error
/// status stays a real failure. Shared by the permission and question reply
/// endpoints so their behaviour cannot drift apart.
fn check_reply_status(resp: reqwest::Response, what: &str, request_id: &str) -> crate::error::Result<()> {
    if resp.status() == reqwest::StatusCode::NOT_FOUND {
        return Err(crate::error::BridgeError::NotFound(format!(
            "{what} {request_id}"
        )));
    }
    resp.error_for_status()?;
    Ok(())
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
        match server {
            Some(crate::bridge::discovery::ResolvedServer {
                url,
                username,
                password,
            }) => Self::with_base_url(model, url, Some(&username), Some(&password)),
            None => Self::with_base_url(
                model,
                String::new(),
                Some(crate::bridge::discovery::DEFAULT_SERVER_USERNAME),
                None,
            ),
        }
    }

    /// Build a client against an explicit base URL and credentials. Production
    /// normally goes through [`Client::new`] (a discovered server) or
    /// [`Client::reconnect`] (the live endpoint was replaced); this constructor
    /// also lets tests point the real client at a local fake server so the
    /// HTTP layer is exercised end to end (ADR-0031). A trailing slash is
    /// tolerated.
    ///
    /// Basic auth is attached only when BOTH username and password are present
    /// — a half-credential must never reach the wire (the server checks the
    /// username too, so a password-only request 401s).
    pub fn with_base_url(
        model: Option<&str>,
        base_url: impl Into<String>,
        username: Option<&str>,
        password: Option<&str>,
    ) -> Self {
        Self {
            http: Arc::new(std::sync::RwLock::new(HttpHandle {
                client: build_http_client(&username.map(str::to_string), &password.map(str::to_string)),
                base_url: base_url.into().trim_end_matches('/').to_string(),
            })),
            username: username.map(str::to_string),
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

    /// The configured default model (`[opencode] model`), parsed at startup.
    /// The second rung of the `/think` effective-model resolution.
    pub fn configured_default_model(&self) -> Option<ModelInfo> {
        self.model.clone()
    }

    /// List sessions across the shared store, most recently active first.
    ///
    /// Uses the cross-project list `GET /experimental/session`
    /// (`Session.GlobalInfo` — camelCase: `id`, `title`, `directory`,
    /// `parentID`, `time.created/updated`, `agent`, `model`; sub-task children
    /// and archived sessions excluded by default). The plain `GET /session` is
    /// PROJECT-scoped: it only returns the server's *own* directory's project
    /// (the instance's cwd), so cola's sessions in another project never
    /// appear — the "recent" list instead shows stale sessions from the server's
    /// project. We fall back to it only for servers too old to expose the
    /// experimental route.
    pub async fn list_sessions(&self) -> crate::error::Result<Vec<SessionListInfo>> {
        let resp = self.http().get(self.url("/experimental/session")).send().await?;
        if resp.status() == reqwest::StatusCode::NOT_FOUND {
            tracing::warn!(
                "server lacks GET /experimental/session; falling back to project-scoped GET /session"
            );
            let resp = self
                .http()
                .get(self.url("/session"))
                .send()
                .await?
                .error_for_status()?;
            return Ok(resp.json().await?);
        }
        Ok(resp.error_for_status()?.json().await?)
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
    pub async fn prompt(
        &self,
        session_id: &str,
        text: &str,
        images: &[ImageInput],
        model: Option<&ModelInfo>,
        variant: Option<&str>,
        agent: Option<&str>,
        message_id: Option<&str>,
    ) -> crate::error::Result<PromptResponse> {
        let mut body = serde_json::json!({
            "parts": build_parts(text, images),
        });
        inject_message_id(&mut body, message_id);
        inject_model(&mut body, model, self.model.as_ref(), variant);
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
    pub async fn prompt_async(
        &self,
        session_id: &str,
        text: &str,
        images: &[ImageInput],
        model: Option<&ModelInfo>,
        variant: Option<&str>,
        agent: Option<&str>,
        message_id: Option<&str>,
    ) -> crate::error::Result<()> {
        let mut body = serde_json::json!({
            "parts": build_parts(text, images),
        });
        inject_message_id(&mut body, message_id);
        inject_model(&mut body, model, self.model.as_ref(), variant);
        inject_agent(&mut body, agent);
        let resp = self
            .http()
            .post(self.url(&format!("/session/{}/prompt_async", session_id)))
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
        let resp = self.http().post(url).json(&body).send().await?;
        check_reply_status(resp, "permission", request_id)?;
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
    pub async fn session_status(
        &self,
        session_id: &str,
        directory: Option<&str>,
    ) -> crate::error::Result<Option<SessionStatus>> {
        let mut url = reqwest::Url::parse(&self.url("/session/status"))?;
        if let Some(d) = directory {
            url.query_pairs_mut().append_pair("directory", d);
        }
        let resp = self.http().get(url).send().await?;
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
        let resp = self.http().post(url).json(&body).send().await?;
        check_reply_status(resp, "question", request_id)?;
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
        parse_provider_models(&v)
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
        let resp = self.http().post(url).send().await?;
        check_reply_status(resp, "question", request_id)?;
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

/// Wire tests: the real `reqwest` client against a local fake server
/// (ADR-0031). Each test asserts both sides of the exchange — the recorded
/// request (method / path / query / headers / body) and what the client parsed
/// back. No HTTP library is mocked and no private state is touched.
#[cfg(test)]
mod wire_tests {
    use super::*;
    use crate::error::BridgeError;
    use crate::test_http::{RecordedRequest, TestHttpServer};

    /// A client pointed at the fake server with both Basic-auth parts set —
    /// what discovery hands production. The transport is swapped for a
    /// no-proxy one so a developer shell's `http_proxy` cannot intercept the
    /// loopback fake server (ticket 09); construction itself stays production's.
    fn wire_client(server: &TestHttpServer, model: Option<&str>) -> Client {
        without_env_proxy(
            Client::with_base_url(model, server.base_url(), Some("opencode"), Some("secret")),
            Some("opencode"),
            Some("secret"),
        )
    }

    /// Point a wire-test client at the no-proxy transport, keeping every other
    /// byte of its construction exactly as production built it (ADR-0031).
    fn without_env_proxy(client: Client, username: Option<&str>, password: Option<&str>) -> Client {
        client.http.write().unwrap().client =
            build_http_client_with(&username.map(str::to_string), &password.map(str::to_string), true);
        client
    }

    /// The request at `index` in arrival order.
    fn request_at(server: &TestHttpServer, index: usize) -> RecordedRequest {
        server
            .requests()
            .get(index)
            .cloned()
            .unwrap_or_else(|| panic!("request {index} should have been sent"))
    }

    fn last_request(server: &TestHttpServer) -> RecordedRequest {
        server.requests().pop().expect("a request should have been sent")
    }

    fn body_json(request: &RecordedRequest) -> serde_json::Value {
        serde_json::from_str(&request.body).expect("request body should be JSON")
    }

    fn expected_basic(username: &str, password: &str) -> String {
        format!(
            "Basic {}",
            base64::engine::general_purpose::STANDARD.encode(format!("{username}:{password}"))
        )
    }

    fn opencode_error(err: BridgeError) -> String {
        match err {
            BridgeError::OpenCode(message) => message,
            other => panic!("expected BridgeError::OpenCode, got: {other:?}"),
        }
    }

    fn not_found_error(err: BridgeError) -> String {
        match err {
            BridgeError::NotFound(message) => message,
            other => panic!("expected BridgeError::NotFound, got: {other:?}"),
        }
    }

    #[tokio::test]
    async fn new_binds_the_resolved_server_url_and_credentials() {
        let server = TestHttpServer::start().await;
        server.route("GET", "/provider", 200, r#"{"all":[],"connected":[]}"#);
        let client = without_env_proxy(
            Client::new(
                None,
                Some(crate::bridge::discovery::ResolvedServer {
                    url: server.base_url(),
                    username: "custom-user".to_string(),
                    password: "custom-pass".to_string(),
                }),
            ),
            Some("custom-user"),
            Some("custom-pass"),
        );

        assert_eq!(client.base_url(), server.base_url());
        let _ = client.list_models().await;
        let expected = expected_basic("custom-user", "custom-pass");
        assert_eq!(
            last_request(&server).header("authorization"),
            Some(expected.as_str())
        );
    }

    #[tokio::test]
    async fn with_base_url_trims_a_trailing_slash() {
        let server = TestHttpServer::start().await;
        server.route("GET", "/provider", 200, r#"{"all":[],"connected":[]}"#);
        let client = without_env_proxy(
            Client::with_base_url(None, format!("{}/", server.base_url()), None, None),
            None,
            None,
        );

        assert_eq!(client.base_url(), server.base_url());
        assert_eq!(client.model_context_window("p", "m").await.unwrap(), None);
        assert_eq!(last_request(&server).path, "/provider");
    }

    #[tokio::test]
    async fn basic_auth_is_sent_only_when_both_credentials_are_present() {
        let server = TestHttpServer::start().await;
        server.route("GET", "/provider", 200, r#"{"all":[],"connected":[]}"#);

        struct Case {
            label: &'static str,
            username: Option<&'static str>,
            password: Option<&'static str>,
            expected_auth: Option<String>,
        }
        let cases = [
            Case {
                label: "both",
                username: Some("opencode"),
                password: Some("secret"),
                expected_auth: Some(expected_basic("opencode", "secret")),
            },
            Case {
                label: "username only",
                username: Some("opencode"),
                password: None,
                expected_auth: None,
            },
            Case {
                label: "password only",
                username: None,
                password: Some("secret"),
                expected_auth: None,
            },
            Case {
                label: "neither",
                username: None,
                password: None,
                expected_auth: None,
            },
        ];
        for case in &cases {
            let client = without_env_proxy(
                Client::with_base_url(None, server.base_url(), case.username, case.password),
                case.username,
                case.password,
            );
            let _ = client.list_models().await;
        }

        assert_eq!(server.request_count(), cases.len());
        for (index, case) in cases.iter().enumerate() {
            assert_eq!(
                request_at(&server, index).header("authorization"),
                case.expected_auth.as_deref(),
                "case: {}",
                case.label
            );
        }
    }

    #[tokio::test]
    async fn prompt_posts_the_message_endpoint_with_parts_model_variant_agent_and_message_id() {
        let server = TestHttpServer::start().await;
        server.route(
            "POST",
            "/session/ses_1/message",
            200,
            serde_json::json!({
                "info": {"id": "msg_a1", "parentID": "msg_u1"},
                "parts": [{"type": "text", "text": "reply"}],
            })
            .to_string(),
        );
        let client = wire_client(&server, None);
        let model = parse_model("opencode-go/deepseek-v4-flash").unwrap();
        let images = vec![ImageInput {
            mime: "image/png".to_string(),
            data_base64: "QUJD".to_string(),
        }];

        let response = client
            .prompt(
                "ses_1",
                "hello",
                &images,
                Some(&model),
                Some("high"),
                Some("build"),
                Some("msg_cola_abc"),
            )
            .await
            .unwrap();

        assert_eq!(response.id, "msg_a1");
        assert_eq!(response.parent_id.as_deref(), Some("msg_u1"));
        assert_eq!(
            response.parts,
            serde_json::json!([{"type": "text", "text": "reply"}])
        );
        assert!(response.error.is_none());

        let request = last_request(&server);
        assert_eq!(request.method, "POST");
        assert_eq!(request.path, "/session/ses_1/message");
        assert_eq!(request.query, "");
        assert_eq!(request.header("content-type"), Some("application/json"));
        let expected = expected_basic("opencode", "secret");
        assert_eq!(request.header("authorization"), Some(expected.as_str()));
        let body = body_json(&request);
        assert_eq!(body["parts"][0]["type"], "text");
        assert_eq!(body["parts"][0]["text"], "hello");
        assert_eq!(body["parts"][1]["type"], "file");
        assert_eq!(body["parts"][1]["mime"], "image/png");
        assert_eq!(body["parts"][1]["url"], "data:image/png;base64,QUJD");
        assert_eq!(body["model"]["providerID"], "opencode-go");
        assert_eq!(body["model"]["modelID"], "deepseek-v4-flash");
        assert_eq!(body["variant"], "high");
        assert_eq!(body["agent"], "build");
        assert_eq!(body["messageID"], "msg_cola_abc");
    }

    #[tokio::test]
    async fn prompt_model_prefers_the_override_then_the_configured_default_then_the_server() {
        let server = TestHttpServer::start().await;
        server.route(
            "POST",
            "/session/ses_override/message",
            200,
            r#"{"info":{"id":"msg_1"},"parts":[]}"#,
        );
        server.route(
            "POST",
            "/session/ses_default/message",
            200,
            r#"{"info":{"id":"msg_2"},"parts":[]}"#,
        );
        server.route(
            "POST",
            "/session/ses_server/message",
            200,
            r#"{"info":{"id":"msg_3"},"parts":[]}"#,
        );
        let client = wire_client(&server, Some("opencode-go/configured-model"));
        let override_model = parse_model("other/override-model").unwrap();

        client
            .prompt("ses_override", "a", &[], Some(&override_model), None, None, None)
            .await
            .unwrap();
        client
            .prompt("ses_default", "b", &[], None, None, None, None)
            .await
            .unwrap();
        let client_without_default = wire_client(&server, None);
        client_without_default
            .prompt("ses_server", "c", &[], None, None, None, None)
            .await
            .unwrap();

        let override_body = body_json(&request_at(&server, 0));
        assert_eq!(override_body["model"]["providerID"], "other");
        assert_eq!(override_body["model"]["modelID"], "override-model");
        let default_body = body_json(&request_at(&server, 1));
        assert_eq!(default_body["model"]["providerID"], "opencode-go");
        assert_eq!(default_body["model"]["modelID"], "configured-model");
        let server_body = body_json(&request_at(&server, 2));
        assert!(
            server_body.get("model").is_none(),
            "neither override nor default must leave the model to the server: {server_body}"
        );
    }

    #[tokio::test]
    async fn prompt_surfaces_a_provider_error_carried_on_a_200_response() {
        let server = TestHttpServer::start().await;
        server.route(
            "POST",
            "/session/ses_1/message",
            200,
            serde_json::json!({
                "info": {"id": "msg_a1", "error": {"data": {"message": "provider 503"}}},
                "parts": [],
            })
            .to_string(),
        );
        let client = wire_client(&server, None);

        let response = client
            .prompt("ses_1", "hi", &[], None, None, None, None)
            .await
            .unwrap();

        assert_eq!(response.error.as_deref(), Some("provider 503"));
    }

    #[tokio::test]
    async fn prompt_maps_404_to_session_not_found() {
        let server = TestHttpServer::start().await; // no route -> 404
        let client = wire_client(&server, None);

        let err = client
            .prompt("ses_gone", "hi", &[], None, None, None, None)
            .await
            .unwrap_err();

        match err {
            BridgeError::SessionNotFound(id) => assert_eq!(id, "ses_gone"),
            other => panic!("expected BridgeError::SessionNotFound, got: {other:?}"),
        }
        assert_eq!(last_request(&server).path, "/session/ses_gone/message");
    }

    #[tokio::test]
    async fn prompt_maps_a_failed_status_to_a_diagnostic_opencode_error() {
        let server = TestHttpServer::start().await;
        server.route("POST", "/session/ses_1/message", 500, r#"{"error":"boom"}"#);
        let client = wire_client(&server, None);

        let message = opencode_error(
            client
                .prompt("ses_1", "hi", &[], None, None, None, None)
                .await
                .unwrap_err(),
        );

        assert!(message.contains("prompt ses_1 failed"), "unexpected: {message}");
        assert!(message.contains("500"), "unexpected: {message}");
        assert!(message.contains("boom"), "unexpected: {message}");
    }

    #[tokio::test]
    async fn prompt_reports_a_non_json_success_body_as_a_decode_error() {
        let server = TestHttpServer::start().await;
        server.route_raw(
            "POST",
            "/session/ses_1/message",
            200,
            "text/html",
            "<html>oops</html>",
        );
        let client = wire_client(&server, None);

        let message = opencode_error(
            client
                .prompt("ses_1", "hi", &[], None, None, None, None)
                .await
                .unwrap_err(),
        );

        assert!(message.contains("prompt decode"), "unexpected: {message}");
        assert!(message.contains("oops"), "unexpected: {message}");
    }

    #[tokio::test]
    async fn prompt_async_posts_fire_and_forget_with_the_same_payload() {
        let server = TestHttpServer::start().await;
        server.route("POST", "/session/ses_1/prompt_async", 204, "");
        let client = wire_client(&server, None);
        let model = parse_model("opencode-go/deepseek-v4-flash").unwrap();

        client
            .prompt_async(
                "ses_1",
                "supplement",
                &[],
                Some(&model),
                Some("low"),
                Some("build"),
                Some("msg_cola_def"),
            )
            .await
            .unwrap();

        let request = last_request(&server);
        assert_eq!(request.method, "POST");
        assert_eq!(request.path, "/session/ses_1/prompt_async");
        assert_eq!(request.query, "");
        let body = body_json(&request);
        assert_eq!(body["parts"][0]["type"], "text");
        assert_eq!(body["parts"][0]["text"], "supplement");
        assert_eq!(body["model"]["modelID"], "deepseek-v4-flash");
        assert_eq!(body["variant"], "low");
        assert_eq!(body["agent"], "build");
        assert_eq!(body["messageID"], "msg_cola_def");
    }

    #[tokio::test]
    async fn prompt_async_maps_a_failed_status_to_a_diagnostic_opencode_error() {
        let server = TestHttpServer::start().await;
        server.route("POST", "/session/ses_1/prompt_async", 500, "nope");
        let client = wire_client(&server, None);

        let message = opencode_error(
            client
                .prompt_async("ses_1", "hi", &[], None, None, None, None)
                .await
                .unwrap_err(),
        );

        assert!(message.contains("prompt_async ses_1"), "unexpected: {message}");
        assert!(message.contains("500"), "unexpected: {message}");
        assert!(message.contains("nope"), "unexpected: {message}");
    }

    #[tokio::test]
    async fn prompt_async_maps_404_to_session_not_found() {
        let server = TestHttpServer::start().await; // no route -> 404
        let client = wire_client(&server, None);

        let err = client
            .prompt_async("ses_gone", "hi", &[], None, None, None, None)
            .await
            .unwrap_err();

        match err {
            BridgeError::SessionNotFound(id) => assert_eq!(id, "ses_gone"),
            other => panic!("expected BridgeError::SessionNotFound, got: {other:?}"),
        }
    }

    #[tokio::test]
    async fn create_session_posts_the_input_and_parses_the_data_envelope() {
        let server = TestHttpServer::start().await;
        server.route(
            "POST",
            "/api/session",
            200,
            serde_json::json!({
                "data": {
                    "id": "ses_new",
                    "projectID": "proj_x",
                    "agent": "build",
                    "cost": 0.0,
                    "time": {"created": 1700000000000i64, "updated": 1700000100000i64},
                    "title": "新会话",
                    "location": {"directory": "/work/cola"},
                },
            })
            .to_string(),
        );
        let client = wire_client(&server, None);
        let input = CreateSessionInput {
            id: None,
            agent: Some("build".to_string()),
            model: parse_model("opencode-go/deepseek-v4-flash"),
            location: Some(Location {
                directory: "/work/cola".to_string(),
            }),
        };

        let session = client.create_session(&input).await.unwrap();

        assert_eq!(session.id, "ses_new");
        assert_eq!(session.project_id.as_deref(), Some("proj_x"));
        assert_eq!(session.title.as_deref(), Some("新会话"));
        assert_eq!(session.agent.as_deref(), Some("build"));
        assert_eq!(
            session.time.as_ref().map(|time| time.created),
            Some(1700000000000)
        );

        let request = last_request(&server);
        assert_eq!(request.method, "POST");
        assert_eq!(request.path, "/api/session");
        assert_eq!(request.query, "");
        let body = body_json(&request);
        assert!(body.get("id").is_none(), "an unset id must be omitted: {body}");
        assert_eq!(body["agent"], "build");
        assert_eq!(body["model"]["providerID"], "opencode-go");
        assert_eq!(body["model"]["id"], "deepseek-v4-flash");
        assert_eq!(body["location"]["directory"], "/work/cola");
    }

    #[tokio::test]
    async fn list_sessions_gets_the_experimental_route_and_parses_camelcase_entries() {
        let server = TestHttpServer::start().await;
        server.route(
            "GET",
            "/experimental/session",
            200,
            serde_json::json!([{
                "id": "ses_a",
                "title": "重构",
                "directory": "/work/cola",
                "parentID": "ses_parent",
                "agent": "build",
                "model": {"id": "deepseek-v4-flash", "providerID": "opencode-go"},
                "time": {"created": 1700000000000i64, "updated": 1700000100000i64},
            }])
            .to_string(),
        );
        let client = wire_client(&server, None);

        let sessions = client.list_sessions().await.unwrap();

        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].id, "ses_a");
        assert_eq!(sessions[0].title, "重构");
        assert_eq!(sessions[0].directory, "/work/cola");
        assert_eq!(sessions[0].parent_id.as_deref(), Some("ses_parent"));
        assert!(sessions[0].is_child());

        let request = last_request(&server);
        assert_eq!(request.method, "GET");
        assert_eq!(request.path, "/experimental/session");
        assert_eq!(request.query, "");
    }

    #[tokio::test]
    async fn list_sessions_falls_back_to_the_project_scoped_route_on_404() {
        let server = TestHttpServer::start().await;
        server.route("GET", "/experimental/session", 404, r#"{"error":"no"}"#);
        server.route(
            "GET",
            "/session",
            200,
            serde_json::json!([{
                "id": "ses_old",
                "title": "old",
                "directory": "/w",
                "time": {"created": 1, "updated": 2},
            }])
            .to_string(),
        );
        let client = wire_client(&server, None);

        let sessions = client.list_sessions().await.unwrap();

        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].id, "ses_old");
        assert_eq!(server.request_count(), 2);
        assert_eq!(request_at(&server, 0).path, "/experimental/session");
        assert_eq!(request_at(&server, 1).path, "/session");
    }

    #[tokio::test]
    async fn list_sessions_only_falls_back_on_404() {
        let server = TestHttpServer::start().await;
        server.route("GET", "/experimental/session", 500, r#"{"error":"boom"}"#);
        let client = wire_client(&server, None);

        let err = client.list_sessions().await.unwrap_err();

        assert!(matches!(err, BridgeError::Http(_)), "unexpected: {err:?}");
        assert_eq!(server.request_count(), 1, "a 500 must not fall back to /session");
        assert_eq!(request_at(&server, 0).path, "/experimental/session");
    }

    #[tokio::test]
    async fn update_session_title_patches_the_canonical_session_route() {
        let server = TestHttpServer::start().await;
        server.route("PATCH", "/session/ses_1", 200, r#"{"id":"ses_1"}"#);
        let client = wire_client(&server, None);

        client.update_session_title("ses_1", "新标题").await.unwrap();

        let request = last_request(&server);
        assert_eq!(request.method, "PATCH");
        assert_eq!(request.path, "/session/ses_1");
        assert_eq!(request.query, "");
        assert_eq!(body_json(&request), serde_json::json!({"title": "新标题"}));
    }

    #[tokio::test]
    async fn session_info_sends_the_directory_scope_and_parses_the_parent_chain() {
        let server = TestHttpServer::start().await;
        server.route(
            "GET",
            "/session/ses_child",
            200,
            serde_json::json!({
                "id": "ses_child",
                "parentID": "ses_parent",
                "title": "子会话",
                "model": {"providerID": "opencode-go", "id": "deepseek-v4-flash"},
            })
            .to_string(),
        );
        let client = wire_client(&server, None);

        let info = client
            .session_info("ses_child", Some("/work/cola"))
            .await
            .unwrap();

        assert_eq!(info.id, "ses_child");
        assert_eq!(info.parent_id.as_deref(), Some("ses_parent"));
        assert_eq!(info.title.as_deref(), Some("子会话"));
        let model = info.model.as_ref().expect("model should parse");
        assert_eq!(model.provider_id, "opencode-go");
        assert_eq!(model.id, "deepseek-v4-flash");

        let request = last_request(&server);
        assert_eq!(request.method, "GET");
        assert_eq!(request.path, "/session/ses_child");
        assert_eq!(request.query_param("directory").as_deref(), Some("/work/cola"));

        client.session_info("ses_child", None).await.unwrap();
        assert_eq!(request_at(&server, 1).query, "", "no directory means no query");
    }

    #[tokio::test]
    async fn session_status_parses_each_status_and_treats_an_absent_session_as_idle() {
        let server = TestHttpServer::start().await;
        server.route(
            "GET",
            "/session/status",
            200,
            serde_json::json!({
                "ses_idle": {"type": "idle"},
                "ses_busy": {"type": "busy"},
                "ses_retry": {"type": "retry", "attempt": 1, "message": "boom", "next": 5000},
                "ses_weird": {"type": "zombie"},
            })
            .to_string(),
        );
        let client = wire_client(&server, None);

        assert_eq!(
            client
                .session_status("ses_idle", Some("/work/cola"))
                .await
                .unwrap(),
            Some(SessionStatus::Idle)
        );
        assert_eq!(
            client.session_status("ses_busy", None).await.unwrap(),
            Some(SessionStatus::Busy)
        );
        assert_eq!(
            client.session_status("ses_retry", None).await.unwrap(),
            Some(SessionStatus::Retry)
        );
        assert_eq!(client.session_status("ses_weird", None).await.unwrap(), None);
        assert_eq!(
            client.session_status("ses_absent", None).await.unwrap(),
            Some(SessionStatus::Idle)
        );

        assert_eq!(request_at(&server, 0).path, "/session/status");
        assert_eq!(
            request_at(&server, 0).query_param("directory").as_deref(),
            Some("/work/cola")
        );
        assert_eq!(request_at(&server, 1).query, "", "no directory means no query");
    }

    #[tokio::test]
    async fn session_status_surfaces_http_and_decode_failures() {
        let down = TestHttpServer::start().await;
        down.route("GET", "/session/status", 500, r#"{"error":"boom"}"#);
        let client = wire_client(&down, None);
        let message = opencode_error(client.session_status("ses_1", None).await.unwrap_err());
        assert!(message.contains("session status failed"), "unexpected: {message}");
        assert!(message.contains("500"), "unexpected: {message}");

        let garbled = TestHttpServer::start().await;
        garbled.route_raw("GET", "/session/status", 200, "text/html", "<html>nope</html>");
        let client = wire_client(&garbled, None);
        let message = opencode_error(client.session_status("ses_1", None).await.unwrap_err());
        assert!(message.contains("session status parse"), "unexpected: {message}");
        assert!(message.contains("nope"), "unexpected: {message}");
    }

    #[tokio::test]
    async fn list_permissions_sends_the_directory_scope_and_parses_requests() {
        let server = TestHttpServer::start().await;
        server.route(
            "GET",
            "/permission",
            200,
            serde_json::json!([{
                "id": "per_1",
                "sessionID": "ses_1",
                "permission": "bash",
                "patterns": ["rm -rf *"],
                "always": [],
                "metadata": {"command": "rm"},
            }])
            .to_string(),
        );
        let client = wire_client(&server, None);

        let permissions = client.list_permissions(Some("/work/cola")).await.unwrap();

        assert_eq!(permissions.len(), 1);
        assert_eq!(permissions[0].request_id, "per_1");
        assert_eq!(permissions[0].session_id.as_deref(), Some("ses_1"));
        assert_eq!(permissions[0].permission.as_deref(), Some("bash"));
        assert_eq!(permissions[0].patterns, vec!["rm -rf *"]);

        let request = last_request(&server);
        assert_eq!(request.method, "GET");
        assert_eq!(request.path, "/permission");
        assert_eq!(request.query_param("directory").as_deref(), Some("/work/cola"));
    }

    #[tokio::test]
    async fn list_permissions_maps_a_failed_status_to_a_diagnostic_opencode_error() {
        let server = TestHttpServer::start().await;
        server.route("GET", "/permission", 502, r#"{"error":"bad gateway"}"#);
        let client = wire_client(&server, None);

        let message = opencode_error(client.list_permissions(None).await.unwrap_err());

        assert!(
            message.contains("permission list failed"),
            "unexpected: {message}"
        );
        assert!(message.contains("502"), "unexpected: {message}");
    }

    #[tokio::test]
    async fn reply_permission_posts_the_reply_with_the_directory_scope() {
        let server = TestHttpServer::start().await;
        server.route("POST", "/permission/per_1/reply", 200, r#"{"code":0}"#);
        server.route(
            "POST",
            "/permission/per_gone/reply",
            404,
            r#"{"error":"not found"}"#,
        );
        let client = wire_client(&server, None);

        client
            .reply_permission("per_1", "always", Some("/work/cola"))
            .await
            .unwrap();

        let request = last_request(&server);
        assert_eq!(request.method, "POST");
        assert_eq!(request.path, "/permission/per_1/reply");
        assert_eq!(request.query_param("directory").as_deref(), Some("/work/cola"));
        assert_eq!(body_json(&request), serde_json::json!({"reply": "always"}));

        // A 404 means the request was already resolved elsewhere — the benign
        // NotFound variant, not a transport failure.
        let message = not_found_error(
            client
                .reply_permission("per_gone", "once", None)
                .await
                .unwrap_err(),
        );
        assert!(message.contains("permission per_gone"), "unexpected: {message}");
    }

    #[tokio::test]
    async fn list_questions_sends_the_directory_scope_and_parses_questions() {
        let server = TestHttpServer::start().await;
        server.route(
            "GET",
            "/question",
            200,
            serde_json::json!([{
                "id": "q_1",
                "sessionID": "ses_1",
                "questions": [{
                    "question": "选哪个？",
                    "header": "选择",
                    "options": [
                        {"label": "A", "description": "first"},
                        {"label": "B", "description": "second"},
                    ],
                    "multiple": true,
                    "custom": false,
                }],
            }])
            .to_string(),
        );
        let client = wire_client(&server, None);

        let questions = client.list_questions(Some("/work/cola")).await.unwrap();

        assert_eq!(questions.len(), 1);
        assert_eq!(questions[0].id, "q_1");
        assert_eq!(questions[0].session_id, "ses_1");
        assert_eq!(questions[0].questions[0].question, "选哪个？");
        assert_eq!(questions[0].questions[0].options[0].label, "A");
        assert_eq!(questions[0].questions[0].multiple, Some(true));

        let request = last_request(&server);
        assert_eq!(request.method, "GET");
        assert_eq!(request.path, "/question");
        assert_eq!(request.query_param("directory").as_deref(), Some("/work/cola"));
    }

    #[tokio::test]
    async fn reply_question_posts_answers_with_the_directory_scope() {
        let server = TestHttpServer::start().await;
        server.route("POST", "/question/q_1/reply", 200, r#"{"code":0}"#);
        server.route("POST", "/question/q_gone/reply", 404, r#"{"error":"not found"}"#);
        let client = wire_client(&server, None);

        client
            .reply_question(
                "q_1",
                &[vec!["A".to_string()], vec!["B".to_string(), "C".to_string()]],
                Some("/work/cola"),
            )
            .await
            .unwrap();

        let request = last_request(&server);
        assert_eq!(request.method, "POST");
        assert_eq!(request.path, "/question/q_1/reply");
        assert_eq!(request.query_param("directory").as_deref(), Some("/work/cola"));
        assert_eq!(
            body_json(&request),
            serde_json::json!({"answers": [["A"], ["B", "C"]]})
        );

        let message = not_found_error(client.reply_question("q_gone", &[], None).await.unwrap_err());
        assert!(message.contains("question q_gone"), "unexpected: {message}");
    }

    #[tokio::test]
    async fn reject_question_posts_with_the_directory_scope() {
        let server = TestHttpServer::start().await;
        server.route("POST", "/question/q_1/reject", 200, r#"{"code":0}"#);
        server.route("POST", "/question/q_gone/reject", 404, r#"{"error":"not found"}"#);
        let client = wire_client(&server, None);

        client.reject_question("q_1", Some("/work/cola")).await.unwrap();

        let request = last_request(&server);
        assert_eq!(request.method, "POST");
        assert_eq!(request.path, "/question/q_1/reject");
        assert_eq!(request.query_param("directory").as_deref(), Some("/work/cola"));
        assert_eq!(request.body, "", "reject carries no body");

        let message = not_found_error(client.reject_question("q_gone", None).await.unwrap_err());
        assert!(message.contains("question q_gone"), "unexpected: {message}");
    }
}
