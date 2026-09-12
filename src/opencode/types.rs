//! Wire types for the OpenCode Server REST API.
//!
//! Every type mirrors the server's JSON contract; camelCase fields carry
//! `#[serde(rename)]` (AGENTS.md pitfall #2). Pure parsing and request-body
//! builders live in the sibling `parsing` module, HTTP transport in `client`.

#![allow(dead_code)] // protocol types — field coverage matches server contract

use serde::{Deserialize, Serialize};

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
/// card row per provider. Each model carries its declared variants (the
/// thinking-level options `/think` surfaces).
#[derive(Debug, Clone)]
pub struct ProviderModels {
    pub provider: String,
    pub models: Vec<ModelOption>,
}

/// One selectable model under a provider.
#[derive(Debug, Clone)]
pub struct ModelOption {
    pub id: String,
    /// The model's declared variants (e.g. `["low", "medium", "high"]`),
    /// empty when it declares none. `GET /provider` serializes each model's
    /// variants as a Record keyed by variant id (`model.variants` →
    /// `{"high": {...}}`); the names are its keys. There is no universal
    /// scale — each model declares its own set (ADR-0020).
    pub variants: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Session {
    pub id: String,
    #[serde(rename = "projectID")]
    pub project_id: Option<String>,
    pub agent: Option<String>,
    pub title: Option<String>,
    pub location: Option<serde_json::Value>,
    pub cost: Option<f64>,
    pub time: Option<SessionTime>,
}

/// Minimal `Session.Info` (canonical `GET /session/{id}`) — enough to resolve
/// a sub-task session's parent, and the session's server-recorded model (the
/// last rung of the `/think` effective-model resolution).
#[derive(Debug, Clone, Deserialize)]
pub struct SessionInfo {
    pub id: String,
    #[serde(rename = "parentID")]
    pub parent_id: Option<String>,
    /// The server-managed session title (what OpenChamber shows).
    #[serde(default)]
    pub title: Option<String>,
    /// The model the server has recorded for the session
    /// (`Session.Info.model`: `{id, providerID, variant?}`).
    #[serde(default)]
    pub model: Option<SessionModel>,
}

/// The model reference on a session (`Session.Info.model`).
#[derive(Debug, Clone, Deserialize)]
pub struct SessionModel {
    #[serde(rename = "providerID")]
    pub provider_id: String,
    pub id: String,
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

/// A session's run state as reported by the server (`GET /session/status`).
/// cola consumes the server's own status service (ADR-0028): idle/busy/retry
/// per session. A read this client cannot interpret is never guessed — it
/// becomes `None` and the caller omits whatever depends on it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionStatus {
    /// The server has no running turn for the session (`{type:"idle"}`, or the
    /// session is absent from the status map — a finished run is removed).
    Idle,
    /// A turn is actively running (`{type:"busy"}`).
    Busy,
    /// The last turn failed and is scheduled for retry (`{type:"retry", …}`).
    Retry,
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
