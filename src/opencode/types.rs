//! Wire types for the OpenCode Server REST API.
//!
//! Every type mirrors the server's JSON contract; camelCase fields carry
//! `#[serde(rename)]` (AGENTS.md pitfall #2). Request-body builders and
//! payload parsing live with the generation strategy (`v1`), never here.
//!
//! These DTOs are the [`crate::backend`] contract's shapes and the documented
//! out-of-scope exception to the generation seam (spec #332): a strategy
//! decodes its generation's payloads into them, so a few generation-flavoured
//! shapes (the session-list entry, the permission request's V1 field names, the
//! status fan-out) still surface here. The typed form model
//! ([`QuestionRequest`]/[`QuestionInfo`]) is genuinely neutral: the V1 strategy
//! maps its positional questions onto it and the V2 strategy decodes its
//! `Form.Field` union into it, so the card layer never learns a generation
//! (spec #364, S6). Generation-private wire structs live with the strategy
//! instead (the V2 read model's landed in `opencode::v2::wire`, spec #364 S4a)
//! and convert into these neutral shapes.

#![allow(dead_code)] // protocol types — field coverage matches server contract

use serde::{Deserialize, Serialize};

use crate::backend::Part;

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
    /// and are excluded from `/switch` auto-adoption and the `/switch` card.
    pub fn is_child(&self) -> bool {
        self.parent_id.is_some()
    }

    /// Whether this session is a direct child of `parent_id` (`parentID`
    /// equals it) — the scoped lookup behind `/sub` and the `/sub attach`
    /// takeover.
    pub fn is_child_of(&self, parent_id: &str) -> bool {
        self.parent_id.as_deref() == Some(parent_id)
    }

    /// Whether the session is archived (`time.archived` set). A missing `time`
    /// reads as not archived, like every session surface that excludes them.
    pub fn is_archived(&self) -> bool {
        self.time.as_ref().is_some_and(|t| t.is_archived())
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

/// An image attached to a prompt, sent as a data-URL `file` part
/// (`{type:"file", mime, url:"data:<mime>;base64,..."}`). Requires a
/// vision-capable model; unsupported models surface an error.
#[derive(Debug, Clone)]
pub struct ImageInput {
    pub mime: String,
    pub data_base64: String,
}

#[derive(Debug)]
pub struct PromptResponse {
    pub id: String,
    pub session_id: Option<String>,
    pub admitted_seq: Option<i64>,
    /// The user message this turn answers (from `info.parentID`).
    pub parent_id: Option<String>,
    /// Error on the assistant message (e.g. provider 503), from `info.error`.
    pub error: Option<String>,
    /// Parts of the assistant response, decoded into the neutral read model by
    /// the adapter's wire decoder — the same seam a polled message goes
    /// through (ADR-0053).
    pub parts: Vec<Part>,
}

/// The typed kind of a form field (V2 `Form.Field`). V1 questions map onto
/// [`Self::String`] and [`Self::Multiselect`]; the other kinds exist for V2
/// forms and render as their own controls (spec #364, S6).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum FormFieldKind {
    #[default]
    String,
    Number,
    Integer,
    Boolean,
    Multiselect,
    External,
}

/// One selectable option of a form field (or a V1 question).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct QuestionOption {
    /// The value submitted when this option is chosen. V1 options carry no
    /// separate value, so an empty `value` reads as the label (the V2 question
    /// tool builds both from the label); the display always uses `label`.
    pub value: String,
    pub label: String,
    pub description: String,
}

impl QuestionOption {
    /// The answer value this option submits (the label when no value is set).
    pub fn answer_value(&self) -> &str {
        if self.value.is_empty() {
            &self.label
        } else {
            &self.value
        }
    }
}

/// One field of a pending form (V2) / question (V1): a typed input with an
/// answer key. V1 has no keys or types — its questions become `String` or
/// `Multiselect` fields keyed positionally (`q0`, `q1`, …) at reply time.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct QuestionInfo {
    /// The answer key (V2 `Form.Field.key`). Empty synthesizes `q{index}` for
    /// the V1 questions, which carry no key.
    pub key: String,
    /// The full question/description text.
    pub question: String,
    /// The short header/title shown first.
    pub header: String,
    pub kind: FormFieldKind,
    pub options: Vec<QuestionOption>,
    /// Whether a free-text answer is allowed (default true, V1's `custom`).
    pub custom: Option<bool>,
    pub required: bool,
    /// The link of an `external` field (not answerable; acknowledged).
    pub url: Option<String>,
}

impl QuestionInfo {
    /// Whether this field's answer is a list of values (V2 `multiselect`).
    pub fn is_multi(&self) -> bool {
        self.kind == FormFieldKind::Multiselect
    }

    /// Whether a custom (typed) answer may be given.
    pub fn custom_allowed(&self) -> bool {
        self.custom.unwrap_or(true)
    }

    /// The display title: the header when set, else the full question.
    pub fn title(&self) -> &str {
        if self.header.is_empty() {
            &self.question
        } else {
            &self.header
        }
    }

    /// The display labels for submitted answer values: an option's label when
    /// the value names one of its values, else the raw value collapsed to one
    /// line (a custom answer, kept verbatim on the wire but never allowed to
    /// break the card's markdown).
    pub fn display_values(&self, values: &[String]) -> Vec<String> {
        values
            .iter()
            .map(|value| {
                self.options
                    .iter()
                    .find(|option| option.answer_value() == value)
                    .map(|option| option.label.clone())
                    .unwrap_or_else(|| value.split_whitespace().collect::<Vec<_>>().join(" "))
            })
            .collect()
    }
}

/// One keyed answer of a form reply (V2 `Form.Answer`'s value union).
#[derive(Debug, Clone, PartialEq)]
pub enum FormValue {
    Text(String),
    Number(f64),
    Bool(bool),
    List(Vec<String>),
}

impl FormValue {
    /// The positional string form V1's `answers: string[][]` carries (and the
    /// test recordings use). The reverse of a V2 keyed answer is not needed:
    /// V2 decodes its own keys.
    pub fn to_strings(&self) -> Vec<String> {
        match self {
            FormValue::Text(text) => vec![text.clone()],
            FormValue::Number(number) => vec![number.to_string()],
            FormValue::Bool(flag) => vec![flag.to_string()],
            FormValue::List(values) => values.clone(),
        }
    }
}

/// One keyed form answer, in field order. `value` is `None` for a field the
/// user never finalized: V1 keeps its positional slot (`answers: [[]]`) and V2
/// omits the key (the server applies its own `required` rule).
#[derive(Debug, Clone, PartialEq)]
pub struct FormAnswer {
    pub key: String,
    pub value: Option<FormValue>,
}

/// Build the keyed answers a form reply carries from the positional string
/// answers the card accumulates, one entry per field in order. `answers[i]` is
/// `None` until field `i` is finalized (a confirmed multi-select may be
/// `Some(vec![])` — "不选"); a numeric field whose typed text cannot parse is
/// left unanswered — never sent as a value the server's schema would reject.
pub fn build_form_answers(questions: &[QuestionInfo], answers: &[Option<Vec<String>>]) -> Vec<FormAnswer> {
    let mut out = Vec::new();
    for (index, field) in questions.iter().enumerate() {
        let key = if field.key.is_empty() {
            format!("q{index}")
        } else {
            field.key.clone()
        };
        let value = answers
            .get(index)
            .and_then(|slot| slot.as_ref())
            .and_then(|values| {
                match field.kind {
                    // External fields are acknowledged (not answered): a `true`
                    // answer records the acknowledgement.
                    FormFieldKind::External => values
                        .iter()
                        .any(|value| value == "true")
                        .then_some(FormValue::Bool(true)),
                    FormFieldKind::Multiselect => Some(FormValue::List(values.clone())),
                    FormFieldKind::Boolean => {
                        Some(FormValue::Bool(values.first().is_some_and(|v| v == "true")))
                    }
                    FormFieldKind::Number | FormFieldKind::Integer => values
                        .first()
                        .and_then(|v| v.parse::<f64>().ok())
                        .map(FormValue::Number),
                    FormFieldKind::String => {
                        let text = values.first().cloned().unwrap_or_default();
                        if text.is_empty() && field.required {
                            // A required string must not be submitted empty; leave
                            // it unanswered so the server reports the missing field.
                            None
                        } else {
                            Some(FormValue::Text(text))
                        }
                    }
                }
            });
        out.push(FormAnswer { key, value });
    }
    out
}

/// A pending question (V1) / form (V2) request: the AI asks the user one or
/// more typed fields and blocks until answered or cancelled.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct QuestionRequest {
    pub id: String,
    pub session_id: String,
    /// The form's title (V2 `Form.Info.title`); empty for V1 questions.
    pub title: String,
    pub questions: Vec<QuestionInfo>,
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
        assert!(info.is_child_of("ses_parent"));
        assert!(!info.is_archived());
    }

    #[test]
    fn child_session_list_info_is_child() {
        let info: SessionListInfo = serde_json::from_str(
            r#"{"id":"ses_child","title":"Child session - x","directory":"/w",
                "parentID":"ses_parent","time":{"created":1,"updated":2}}"#,
        )
        .unwrap();
        assert!(info.is_child());
        assert!(info.is_child_of("ses_parent"));
        assert!(!info.is_child_of("ses_other"));
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
        assert!(info.is_archived());
    }

    /// A missing `time` (or one without `archived`) reads as not archived,
    /// which the session surfaces rely on when they exclude archived entries.
    #[test]
    fn missing_time_is_not_archived() {
        let info: SessionListInfo =
            serde_json::from_str(r#"{"id":"ses_a","title":"t","directory":"/w"}"#).unwrap();
        assert!(!info.is_archived());
        assert!(!info.is_child());
    }
}
