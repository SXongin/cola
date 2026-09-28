//! V2 wire shapes for the read surface (spec #364, S4a session reads + S4b
//! transcript decode).
//!
//! V2's session payloads differ from V1's in ways the neutral DTOs cannot
//! express: every read is wrapped in a `{data: ...}` envelope, the directory
//! rides in `location.directory` (not a top-level `directory`), and the list
//! cursor is an opaque body field (`cursor.next`) instead of a response
//! header. The run state has no `retry` entry at all, so the strategy derives
//! it from the newest assistant message's `retry` field.
//!
//! The transcript read is not V1's `{info, parts}` envelope: a projected
//! message is a tagged union (`type: user | assistant | …`), an assistant's
//! body is `content[]` (text / reasoning / tool — only a tool carries an id),
//! and a step's completion is the message's `finish` field rather than a
//! `step-finish` part. The decode below re-derives the neutral transcript from
//! those shapes exactly as tolerantly as the V1 decoder: unknown message kinds
//! and content items keep their raw payload, and a missing field never fails
//! the read (ADR-0053).
//!
//! These private shapes decode that wire into the generation-blind DTOs
//! ([`SessionListInfo`], [`SessionInfo`], [`SessionTranscript`]); nothing here
//! escapes the strategy (ADR-0055). The V1 coupling guard's denylist never
//! applies to V2 literals, but generation-distinctive names still stay inside
//! this module.

use std::collections::HashMap;

use serde::Deserialize;
use serde_json::Value;

use crate::backend::{
    ContentBlock, FinishReason, MessageId, MessageRole, MessageTime, ModelIdentity, OtherPart, Part,
    ReasoningPart, SessionTranscript, StepFinish, TextPart, TokenUsage, ToolCall, ToolIdentity, ToolOutput,
    ToolStatus, TranscriptMessage,
};
use crate::opencode::types::{
    AgentInfo, FormFieldKind, ModelInfo, ModelOption, PermissionRequest, QuestionInfo, QuestionOption,
    QuestionRequest, SessionInfo, SessionListInfo, SessionModel, SessionSelection, SessionTime,
};

/// `{data: T}` — the envelope most V2 reads share (R2's "unwrap per route —
/// there is no single rule").
#[derive(Debug, Deserialize)]
pub(super) struct DataEnvelope<T> {
    pub(super) data: T,
}

/// `GET /api/session` — `{data: Session.Info[], cursor: {previous?, next?}}`.
#[derive(Debug, Deserialize)]
pub(super) struct SessionListPage {
    pub(super) data: Vec<RawSessionInfo>,
    #[serde(default)]
    pub(super) cursor: PageCursor,
}

/// The page cursor. It is generated for every non-empty page, so a client
/// following `cursor.next` always makes one final request that yields an empty
/// `data` and no `next` (the store's anchor is exclusive).
#[derive(Debug, Default, Deserialize)]
pub(super) struct PageCursor {
    #[serde(default)]
    pub(super) next: Option<String>,
}

/// One `Session.Info` as the list/get envelopes serialize it.
#[derive(Debug, Deserialize)]
pub(super) struct RawSessionInfo {
    pub(super) id: String,
    #[serde(rename = "parentID", default)]
    pub(super) parent_id: Option<String>,
    #[serde(default)]
    pub(super) title: Option<String>,
    #[serde(default)]
    pub(super) agent: Option<String>,
    #[serde(default)]
    pub(super) model: Option<Value>,
    #[serde(default)]
    pub(super) location: Option<RawLocation>,
    #[serde(default)]
    pub(super) time: Option<SessionTime>,
}

/// `Location.PublicRef` — the only location field the public session surfaces.
#[derive(Debug, Deserialize)]
pub(super) struct RawLocation {
    pub(super) directory: String,
}

impl RawSessionInfo {
    /// The neutral `/switch`/`/dir` row: V1's top-level `directory` becomes
    /// `location.directory`, and the optional title reads as empty (the same
    /// tolerance [`SessionListInfo`] has for a missing one).
    pub(super) fn into_list_info(self) -> SessionListInfo {
        SessionListInfo {
            id: self.id,
            title: self.title.unwrap_or_default(),
            directory: self
                .location
                .map(|location| location.directory)
                .unwrap_or_default(),
            parent_id: self.parent_id,
            agent: self.agent,
            model: self.model,
            time: self.time,
        }
    }

    /// The neutral parent-chain/effective-model read. A malformed `model` is a
    /// decode error, exactly as it would be for V1's direct deserialize.
    pub(super) fn into_session_info(self) -> serde_json::Result<SessionInfo> {
        Ok(SessionInfo {
            id: self.id,
            parent_id: self.parent_id,
            title: self.title,
            model: decode_model_ref(self.model)?.map(RawModelRef::into_session_model),
        })
    }

    /// The neutral durable-selection read: the session's model ref (variant
    /// inside it) and agent. The same [`RawModelRef`] decode as
    /// [`Self::into_session_info`] — one model decoder, so the two reads cannot
    /// drift.
    pub(super) fn into_selection(self) -> serde_json::Result<SessionSelection> {
        Ok(SessionSelection {
            model: decode_model_ref(self.model)?.map(RawModelRef::into_model_info),
            agent: self.agent,
        })
    }
}

/// Decode `Session.Info.model` through one shared [`RawModelRef`] decoder.
fn decode_model_ref(model: Option<Value>) -> serde_json::Result<Option<RawModelRef>> {
    model.map(serde_json::from_value::<RawModelRef>).transpose()
}

/// `Session.Info.model` — the `Model.Ref` a durable selection carries. The
/// variant is part of the ref (V2's `/think` semantics).
#[derive(Debug, Deserialize)]
struct RawModelRef {
    #[serde(rename = "providerID")]
    provider_id: String,
    id: String,
    #[serde(default)]
    variant: Option<String>,
}

impl RawModelRef {
    /// The neutral session-info model (the variant-blind DTO the parent-chain
    /// and display readers consume).
    fn into_session_model(self) -> SessionModel {
        SessionModel {
            provider_id: self.provider_id,
            id: self.id,
        }
    }

    /// The neutral model ref, variant included.
    fn into_model_info(self) -> ModelInfo {
        ModelInfo {
            id: self.id,
            provider_id: self.provider_id,
            variant: real_variant(self.variant),
        }
    }
}

/// The one "no variant" spelling: V2 normalizes an absent variant to the
/// literal `"default"` on session and message model refs and treats that
/// spelling as unset everywhere else, so the neutral refs must never carry it
/// as a real variant (ADR-0020: clearing is a mechanism, never a value word).
fn real_variant(variant: Option<String>) -> Option<String> {
    variant.filter(|variant| variant != "default")
}

/// `GET /api/agent` — one `Agent.Info`. `id` is the wire identity a session
/// switch takes; the neutral view carries it as the agent's selectable name
/// (V1's agent name IS its id). `name` is the display name, which only the
/// server-side catalog knows and cola does not show on the picker today.
#[derive(Debug, Deserialize)]
pub(super) struct RawAgentInfo {
    pub(super) id: String,
    #[serde(default)]
    pub(super) description: Option<String>,
    #[serde(default)]
    pub(super) mode: Option<String>,
    #[serde(default)]
    pub(super) hidden: Option<bool>,
}

impl RawAgentInfo {
    pub(super) fn into_neutral(self) -> AgentInfo {
        AgentInfo {
            name: self.id,
            description: self.description,
            mode: self.mode,
            hidden: self.hidden,
        }
    }
}

/// `GET /api/model` — one `Model.Info`. Only the catalog fields the `/model`
/// picker and the context-window footer read; `GET /api/model` already serves
/// the enabled set, so no `enabled`/`status` filter is applied here.
#[derive(Debug, Deserialize)]
pub(super) struct RawModelInfo {
    pub(super) id: String,
    #[serde(rename = "providerID")]
    pub(super) provider_id: String,
    #[serde(default)]
    pub(super) variants: Vec<RawModelVariant>,
    #[serde(default)]
    pub(super) limit: Option<RawModelLimit>,
}

impl RawModelInfo {
    pub(super) fn into_option(self) -> ModelOption {
        ModelOption {
            id: self.id,
            variants: self.variants.into_iter().map(|variant| variant.id).collect(),
        }
    }
}

/// One declared thinking-level variant (`Model.Variant`).
#[derive(Debug, Deserialize)]
pub(super) struct RawModelVariant {
    pub(super) id: String,
}

/// The model's declared limits; only the context window is read.
#[derive(Debug, Deserialize)]
pub(super) struct RawModelLimit {
    #[serde(default)]
    pub(super) context: Option<i64>,
}

/// `POST /api/session/{id}/prompt` — `{data: SessionInbox.User}`. The admitted
/// item's `id` is the durable user-message id the caller chose (the server
/// persists it and reconciles a retry onto the same row), and the only field
/// the write path reads.
#[derive(Debug, Deserialize)]
pub(super) struct RawAdmittedPrompt {
    pub(super) id: String,
}

/// `GET /api/session/active` — `{data: Record<SessionID, {type:"running"}>}`.
/// Only `running` exists; absence means inactive.
#[derive(Debug, Deserialize)]
pub(super) struct ActiveSessions {
    pub(super) data: HashMap<String, Value>,
}

impl ActiveSessions {
    /// `Some(true)` = the session is running, `Some(false)` = the server
    /// reported an active entry whose type cola does not know (never guessed),
    /// `None` = the session is absent (not active).
    pub(super) fn state(&self, session_id: &str) -> Option<bool> {
        self.data
            .get(session_id)
            .map(|entry| entry.get("type").and_then(Value::as_str) == Some("running"))
    }
}

/// `GET /api/permission/request` — `{location, data: Permission.Request[]}`.
/// The request field names are V2's own: `action`/`resources`/`save` where V1
/// spells `permission`/`patterns`/`always`.
#[derive(Debug, Deserialize)]
pub(super) struct RawPermission {
    pub(super) id: String,
    #[serde(rename = "sessionID")]
    pub(super) session_id: String,
    pub(super) action: String,
    #[serde(default)]
    pub(super) resources: Vec<String>,
    #[serde(default)]
    pub(super) save: Vec<String>,
    #[serde(default)]
    pub(super) metadata: Option<Value>,
}

impl RawPermission {
    pub(super) fn into_neutral(self) -> PermissionRequest {
        PermissionRequest {
            request_id: self.id,
            session_id: Some(self.session_id),
            permission: Some(self.action),
            patterns: self.resources,
            metadata: self.metadata,
            always: self.save,
        }
    }
}

/// `GET /api/form` — `{location, data: Form.Info[]}`.
#[derive(Debug, Deserialize)]
pub(super) struct RawForm {
    pub(super) id: String,
    #[serde(rename = "sessionID")]
    pub(super) session_id: String,
    #[serde(default)]
    pub(super) title: String,
    #[serde(default)]
    pub(super) fields: Vec<RawFormField>,
}

impl RawForm {
    /// The neutral form: the title is the card header, each field keeps its
    /// answer key and typed kind. The `when` visibility conditions are
    /// deliberately not carried — cola renders every field (spec #364, out of
    /// scope: V2-only form behaviour beyond parity).
    pub(super) fn into_neutral(self) -> QuestionRequest {
        QuestionRequest {
            id: self.id,
            session_id: self.session_id,
            title: self.title,
            questions: self.fields.into_iter().map(RawFormField::into_neutral).collect(),
        }
    }
}

/// One `Form.Field` — V2's tagged union keyed by `type`. Only the fields cola
/// renders are decoded; unknown extra fields (formats, bounds, `when`) are
/// ignored, so a form with a shape this build does not know still lists.
#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub(super) enum RawFormField {
    String {
        key: String,
        #[serde(default)]
        title: Option<String>,
        #[serde(default)]
        description: Option<String>,
        #[serde(default)]
        required: bool,
        #[serde(default)]
        custom: Option<bool>,
        #[serde(default)]
        options: Vec<RawFormOption>,
    },
    Number {
        key: String,
        #[serde(default)]
        title: Option<String>,
        #[serde(default)]
        description: Option<String>,
        #[serde(default)]
        required: bool,
    },
    Integer {
        key: String,
        #[serde(default)]
        title: Option<String>,
        #[serde(default)]
        description: Option<String>,
        #[serde(default)]
        required: bool,
    },
    Boolean {
        key: String,
        #[serde(default)]
        title: Option<String>,
        #[serde(default)]
        description: Option<String>,
        #[serde(default)]
        required: bool,
    },
    Multiselect {
        key: String,
        #[serde(default)]
        title: Option<String>,
        #[serde(default)]
        description: Option<String>,
        #[serde(default)]
        required: bool,
        #[serde(default)]
        custom: Option<bool>,
        #[serde(default)]
        options: Vec<RawFormOption>,
    },
    External {
        key: String,
        #[serde(default)]
        title: Option<String>,
        #[serde(default)]
        description: Option<String>,
        url: String,
    },
}

impl RawFormField {
    pub(super) fn into_neutral(self) -> QuestionInfo {
        let field = |key: String,
                     title: Option<String>,
                     description: Option<String>,
                     kind: FormFieldKind,
                     options: Vec<RawFormOption>,
                     custom: Option<bool>,
                     required: bool| QuestionInfo {
            key,
            header: title.unwrap_or_default(),
            question: description.unwrap_or_default(),
            kind,
            options: options.into_iter().map(RawFormOption::into_neutral).collect(),
            custom,
            required,
            url: None,
        };
        match self {
            RawFormField::String {
                key,
                title,
                description,
                required,
                custom,
                options,
            } => field(
                key,
                title,
                description,
                FormFieldKind::String,
                options,
                custom,
                required,
            ),
            RawFormField::Number {
                key,
                title,
                description,
                required,
            } => field(
                key,
                title,
                description,
                FormFieldKind::Number,
                Vec::new(),
                None,
                required,
            ),
            RawFormField::Integer {
                key,
                title,
                description,
                required,
            } => field(
                key,
                title,
                description,
                FormFieldKind::Integer,
                Vec::new(),
                None,
                required,
            ),
            RawFormField::Boolean {
                key,
                title,
                description,
                required,
            } => field(
                key,
                title,
                description,
                FormFieldKind::Boolean,
                Vec::new(),
                None,
                required,
            ),
            RawFormField::Multiselect {
                key,
                title,
                description,
                required,
                custom,
                options,
            } => field(
                key,
                title,
                description,
                FormFieldKind::Multiselect,
                options,
                custom,
                required,
            ),
            RawFormField::External {
                key,
                title,
                description,
                url,
            } => QuestionInfo {
                key,
                header: title.unwrap_or_default(),
                question: description.unwrap_or_default(),
                kind: FormFieldKind::External,
                options: Vec::new(),
                custom: None,
                required: false,
                url: Some(url),
            },
        }
    }
}

/// One `Form.Option`: the submitted `value` and the display `label` differ.
#[derive(Debug, Deserialize)]
pub(super) struct RawFormOption {
    pub(super) value: String,
    pub(super) label: String,
    #[serde(default)]
    pub(super) description: Option<String>,
}

impl RawFormOption {
    pub(super) fn into_neutral(self) -> QuestionOption {
        QuestionOption {
            value: self.value,
            label: self.label,
            description: self.description.unwrap_or_default(),
        }
    }
}

/// `GET /api/session/{id}/message` — `{data: Session.Message.Info[], cursor}`.
///
/// Items stay raw [`Value`]s: the projected message is an eleven-arm tagged
/// union whose bodies differ per type, and the decode below must preserve
/// unknown arms verbatim (the V1 decoder's policy, for the same reason).
#[derive(Debug, Deserialize)]
pub(super) struct MessagesPage {
    #[serde(default)]
    pub(super) data: Vec<Value>,
    #[serde(default)]
    pub(super) cursor: PageCursor,
}

impl MessagesPage {
    /// The first assistant message in the page. A `type=assistant`-filtered
    /// one-message read makes this the newest assistant; the tolerant scan
    /// keeps the answer correct if a server ever ignores the filter (the first
    /// assistant present is still "the newest one present").
    pub(super) fn newest_assistant(&self) -> Option<&Value> {
        self.data
            .iter()
            .find(|message| message.get("type").and_then(Value::as_str) == Some("assistant"))
    }

    /// Whether the newest assistant message is scheduled for retry. The
    /// strategy requests `type=assistant&order=desc&limit=1`, so this is the
    /// latest assistant; the tolerant scan keeps the answer correct if a server
    /// ever ignores the filter (the first assistant is still "the newest one
    /// present"). An absent `retry` — including an explicit JSON null, which is
    /// how V2 serializes the cleared field — means no retry.
    pub(super) fn newest_assistant_retrying(&self) -> bool {
        self.newest_assistant()
            .and_then(|message| message.get("retry"))
            .is_some_and(|retry| !retry.is_null())
    }
}

/// Decode a session's projected messages — the `data` arrays a
/// `GET /api/session/{id}/message` read carries, in server order — into the
/// neutral [`SessionTranscript`].
///
/// The neutral model's completion contract is an assistant message's terminal
/// step finish, so each assistant message's `finish` becomes a
/// [`Part::StepFinish`]: on V2 the finish reason is a message field, not a
/// part. An explicit `"unknown"` is a spelled-out reason (the provider ended
/// the step without naming why) and stays terminal, unlike a missing reason —
/// the same distinction the neutral [`FinishReason::Unknown`] documents.
pub(super) fn decode_messages(data: &[Value]) -> SessionTranscript {
    SessionTranscript::new(data.iter().map(decode_message).collect())
}

/// Decode one projected message. Its identity, role, server time and recorded
/// failure are the neutral facts; the body depends on the message type.
fn decode_message(message: &Value) -> TranscriptMessage {
    let kind = message.get("type").and_then(Value::as_str);
    let (role, parts) = decode_body(kind, message);
    TranscriptMessage {
        id: MessageId::new(
            message
                .get("id")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
        ),
        role,
        time: decode_time(message),
        model: decode_model(message.get("model")),
        tokens: decode_tokens(message.get("tokens")),
        error: message.get("error").and_then(decode_error),
        parts,
    }
}

/// The neutral server time. A message without a numeric `time.created` cannot
/// be ordered or anchored and reads as timeless — never as epoch 0.
fn decode_time(message: &Value) -> Option<MessageTime> {
    let created = message.pointer("/time/created").and_then(Value::as_i64)?;
    Some(MessageTime {
        created,
        // `streamed` (the provider body ended, tools may still settle) is not a
        // completion stamp: a message without `completed` is still in flight.
        completed: message.pointer("/time/completed").and_then(Value::as_i64),
    })
}

/// The model that produced an assistant message (`Model.Ref`), when named. The
/// variant is normalized through [`real_variant`] so the reserved `"default"`
/// sentinel never reads as a real thinking level (ADR-0020).
fn decode_model(model: Option<&Value>) -> Option<ModelIdentity> {
    let model = model?;
    let model_id = model.get("id").and_then(Value::as_str)?;
    Some(ModelIdentity {
        provider_id: model
            .get("providerID")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        model_id: model_id.to_string(),
        variant: real_variant(model.get("variant").and_then(Value::as_str).map(str::to_string)),
    })
}

/// The model ref one projected message names, decoded to the neutral
/// variant-carrying [`ModelInfo`] the effective-model ladder speaks. Shares
/// [`decode_model`] with the transcript decode, so the two cannot drift.
pub(super) fn decode_message_model(message: &Value) -> Option<ModelInfo> {
    decode_model(message.get("model")).map(|model| ModelInfo {
        id: model.model_id,
        provider_id: model.provider_id,
        variant: model.variant,
    })
}

/// The token usage an assistant message reports. V2 carries no `total`, so the
/// neutral total is the server's own sum — `input + output + reasoning +
/// cache.read + cache.write` (`TokenUsage.total()` upstream) — which is what
/// the footer's context figure reads.
fn decode_tokens(tokens: Option<&Value>) -> Option<TokenUsage> {
    let tokens = tokens?;
    let read = |field: &str| tokens.get(field).and_then(Value::as_i64).unwrap_or(0);
    let cache_read = tokens.pointer("/cache/read").and_then(Value::as_i64).unwrap_or(0);
    let cache_write = tokens
        .pointer("/cache/write")
        .and_then(Value::as_i64)
        .unwrap_or(0);
    let input = read("input");
    let output = read("output");
    let reasoning = read("reasoning");
    Some(TokenUsage {
        input,
        output,
        total: input + output + reasoning + cache_read + cache_write,
        cache_read,
        cache_write,
    })
}

/// Decode a message body into its neutral role and parts. Conversation is only
/// ever user/assistant; the message types that are neither (system, synthetic,
/// skill, shell, compaction, idle, the switch markers) stay out of the
/// conversation projections by role, and unknown kinds keep their raw payload
/// so a newer server never breaks the read.
fn decode_body(kind: Option<&str>, message: &Value) -> (MessageRole, Vec<Part>) {
    match kind {
        Some("user") => (MessageRole::User, decode_user_parts(message)),
        Some("assistant") => (MessageRole::Assistant, decode_assistant_parts(message)),
        Some("system") => (MessageRole::System, decode_text_body(message)),
        // A synthetic/system-style message is not a User: it must never become
        // a Turn anchor. The verbatim type keeps it distinguishable.
        Some("synthetic") => (
            MessageRole::Other("synthetic".to_string()),
            decode_text_body(message),
        ),
        Some("skill") => (MessageRole::Other("skill".to_string()), decode_text_body(message)),
        Some(other) => (
            MessageRole::Other(other.to_string()),
            vec![raw_message_part(other, message)],
        ),
        None => (MessageRole::Unknown, vec![raw_message_part("", message)]),
    }
}

/// A user message's parts: its `text` (a malformed one stays raw, like V1's
/// decoder) followed by one raw part per `files` attachment, so the payload is
/// preserved without growing a file part kind the neutral model does not have.
fn decode_user_parts(message: &Value) -> Vec<Part> {
    let mut parts = Vec::new();
    match message.get("text") {
        Some(Value::String(text)) => parts.push(Part::Text(TextPart {
            text: text.clone(),
            started_at: None,
        })),
        Some(other) => parts.push(Part::Other(OtherPart {
            kind: "text".to_string(),
            raw: other.clone(),
        })),
        None => {}
    }
    if let Some(files) = message.get("files").and_then(Value::as_array) {
        for file in files {
            parts.push(Part::Other(OtherPart {
                kind: "file".to_string(),
                raw: file.clone(),
            }));
        }
    }
    parts
}

/// An assistant message's parts: its `content[]` in order, then the message's
/// step completion as a [`Part::StepFinish`] (absent while in flight).
fn decode_assistant_parts(message: &Value) -> Vec<Part> {
    let mut parts = decode_content(message.get("content"));
    if let Some(reason) = message.get("finish").and_then(Value::as_str) {
        parts.push(Part::StepFinish(StepFinish {
            reason: decode_finish_reason(Some(reason)),
        }));
    }
    parts
}

/// A message whose conversational body is a plain `text` field (system,
/// synthetic, skill): the guarded text arm, or nothing when it carries none.
fn decode_text_body(message: &Value) -> Vec<Part> {
    match message.get("text") {
        Some(Value::String(text)) => vec![Part::Text(TextPart {
            text: text.clone(),
            started_at: None,
        })],
        Some(other) => vec![Part::Other(OtherPart {
            kind: "text".to_string(),
            raw: other.clone(),
        })],
        None => Vec::new(),
    }
}

/// A message kind this build does not model, kept raw as one part so the
/// payload survives for later slices.
fn raw_message_part(kind: &str, message: &Value) -> Part {
    Part::Other(OtherPart {
        kind: kind.to_string(),
        raw: message.clone(),
    })
}

/// Decode an assistant's `content[]`.
fn decode_content(content: Option<&Value>) -> Vec<Part> {
    content
        .and_then(Value::as_array)
        .map(|items| items.iter().map(decode_content_item).collect())
        .unwrap_or_default()
}

/// One content item: text, reasoning, or a tool call. A text item whose `text`
/// is not a string is malformed and stays raw (V1's arm, so a valid+malformed
/// pair never joins an extra line); reasoning has no id anywhere, so the
/// decoded order is the whole identity, and dedup stays content-based in the
/// renderer (AGENTS.md #9).
fn decode_content_item(item: &Value) -> Part {
    match item.get("type").and_then(Value::as_str) {
        Some("text") => match item.get("text").and_then(Value::as_str) {
            Some(text) => Part::Text(TextPart {
                text: text.to_string(),
                started_at: None,
            }),
            None => Part::Other(OtherPart {
                kind: "text".to_string(),
                raw: item.clone(),
            }),
        },
        Some("reasoning") => Part::Reasoning(ReasoningPart {
            text: item
                .get("text")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
            started_at: item.pointer("/time/created").and_then(Value::as_i64),
        }),
        Some("tool") => Part::Tool(decode_tool(item)),
        Some(other) => Part::Other(OtherPart {
            kind: other.to_string(),
            raw: item.clone(),
        }),
        None => Part::Other(OtherPart {
            kind: String::new(),
            raw: item.clone(),
        }),
    }
}

/// One assistant `tool` content item. `id` is the tool call's own id (the only
/// content identity that exists), and the item carries the call's timing;
/// `state` carries status/input/metadata/content.
fn decode_tool(item: &Value) -> ToolCall {
    let state = non_null(item.get("state"));
    let identity = decode_tool_identity(item.get("name"), item.get("id"));
    let status = decode_tool_status(state.and_then(|state| state.get("status")));
    ToolCall {
        identity,
        status: status.clone(),
        started_at: item.pointer("/time/created").and_then(Value::as_i64),
        // The raw structured input (a string while `streaming`: the arguments
        // are still arriving, so there is nothing structured to read yet).
        input: state.and_then(|state| non_null(state.get("input"))).cloned(),
        metadata: state.and_then(|state| non_null(state.get("metadata"))).cloned(),
        output: decode_tool_output(state),
    }
}

/// A tool's identity: its built-in name — the historical `"tool"` fallback
/// when the payload lost it — and the call's id, falling back to the name so a
/// call without one still folds onto a single panel.
fn decode_tool_identity(name: Option<&Value>, call_id: Option<&Value>) -> ToolIdentity {
    let name = name.and_then(Value::as_str).unwrap_or("tool").to_string();
    let call_id = call_id
        .and_then(Value::as_str)
        .unwrap_or(name.as_str())
        .to_string();
    ToolIdentity { name, call_id }
}

/// A tool lifecycle status. `streaming` (the model is still sending the call's
/// arguments) maps onto the neutral pending arm: the call has not executed yet.
/// Anything else keeps its spelling; a missing one is its own arm.
fn decode_tool_status(status: Option<&Value>) -> ToolStatus {
    match status.and_then(Value::as_str) {
        Some("streaming") => ToolStatus::Pending,
        Some("pending") => ToolStatus::Pending,
        Some("running") => ToolStatus::Running,
        Some("completed") => ToolStatus::Completed,
        Some("error") => ToolStatus::Error,
        Some(other) => ToolStatus::Other(other.to_string()),
        None => ToolStatus::Unknown,
    }
}

/// Why a step finished. V2 spells the reason out (its `unknown` literal is a
/// reported reason, so it stays terminal); a missing one never declares
/// completion.
fn decode_finish_reason(reason: Option<&str>) -> FinishReason {
    match reason {
        Some("tool-calls") => FinishReason::ToolCalls,
        Some("stop") => FinishReason::Stop,
        Some("length") => FinishReason::Length,
        Some("content-filter") => FinishReason::ContentFilter,
        Some("error") => FinishReason::Error,
        Some(other) => FinishReason::Other(other.to_string()),
        None => FinishReason::Unknown,
    }
}

/// Decode a tool state's output side: its `content` blocks and the failure
/// message. Text blocks carry their text verbatim; any other block stays raw
/// (ADR-0042's per-tool presentation reads [`ToolOutput::blocks`]). A decoded
/// failure's message lives apart, exactly as V1's does, so the panel appends
/// its `❌ …` line once. V2 has no `metadata.output` fallback rung: the state's
/// `content` is the output source.
fn decode_tool_output(state: Option<&Value>) -> ToolOutput {
    let Some(state) = state else {
        return ToolOutput::default();
    };
    let mut blocks = Vec::new();
    let content = non_null(state.get("content"));
    if let Some(items) = content.and_then(Value::as_array) {
        for item in items {
            match item.get("text").and_then(Value::as_str) {
                Some(text) if item.get("type").and_then(Value::as_str) == Some("text") => {
                    blocks.push(ContentBlock::Text(text.to_string()));
                }
                _ => blocks.push(ContentBlock::Other(item.clone())),
            }
        }
    }
    ToolOutput {
        // The output payload the state actually carries; an empty content array
        // is the server saying "no output".
        raw: content.filter(|value| has_payload(value)).cloned(),
        blocks,
        error: state.get("error").and_then(decode_error),
    }
}

/// Normalize a failure payload: a plain string message or an object carrying
/// `message` (V2 serializes `SessionError.Error` as `{type, message, status?}`).
fn decode_error(error: &Value) -> Option<String> {
    match error {
        Value::String(message) => Some(message.clone()),
        Value::Object(fields) => fields.get("message").and_then(Value::as_str).map(str::to_string),
        _ => None,
    }
}

/// A JSON field that was actually reported: absent and explicit `null` both
/// read as "not there".
fn non_null(value: Option<&Value>) -> Option<&Value> {
    value.filter(|value| !value.is_null())
}

/// Whether a payload field carries anything: an empty array or object is the
/// server saying "no output", not an output.
fn has_payload(value: &Value) -> bool {
    match value {
        Value::Array(items) => !items.is_empty(),
        Value::Object(fields) => !fields.is_empty(),
        _ => true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn raw_session_info_maps_location_parent_and_model_onto_the_neutral_dtos() {
        // A real `session.list`/`session.get` entry: directory in
        // `location.directory` (not V1's top-level field), `parentID`
        // camelCase, `model` a `Model.Ref`.
        let raw: RawSessionInfo = serde_json::from_value(serde_json::json!({
            "id": "ses_child",
            "parentID": "ses_parent",
            "projectID": "proj_x",
            "title": "子会话",
            "agent": "build",
            "model": {"id": "deepseek-v4-flash", "providerID": "opencode-go"},
            "location": {"directory": "/work/cola"},
            "time": {"created": 1, "updated": 2},
            "cost": 0.0,
        }))
        .unwrap();

        let list = raw.into_list_info();
        assert_eq!(list.id, "ses_child");
        assert_eq!(list.title, "子会话");
        assert_eq!(list.directory, "/work/cola");
        assert_eq!(list.parent_id.as_deref(), Some("ses_parent"));
        assert!(list.is_child());
        assert_eq!(list.model.as_ref().unwrap()["providerID"], "opencode-go");

        let raw: RawSessionInfo = serde_json::from_value(serde_json::json!({
            "id": "ses_child",
            "parentID": "ses_parent",
            "title": "子会话",
            "model": {"id": "deepseek-v4-flash", "providerID": "opencode-go"},
            "location": {"directory": "/work/cola"},
        }))
        .unwrap();
        let info = raw.into_session_info().unwrap();
        assert_eq!(info.parent_id.as_deref(), Some("ses_parent"));
        let model = info.model.expect("the model must survive the envelope");
        assert_eq!(model.provider_id, "opencode-go");
        assert_eq!(model.id, "deepseek-v4-flash");
    }

    /// A missing title/location reads empty, like every tolerant session row;
    /// a malformed model is a loud decode error, never a silently dropped
    /// field.
    #[test]
    fn raw_session_info_tolerates_missing_optionals_but_not_a_malformed_model() {
        let raw: RawSessionInfo = serde_json::from_value(serde_json::json!({"id": "ses_root"})).unwrap();
        let list = raw.into_list_info();
        assert_eq!(list.title, "");
        assert_eq!(list.directory, "");
        assert!(list.parent_id.is_none());

        let raw: RawSessionInfo = serde_json::from_value(serde_json::json!({
            "id": "ses_root",
            "model": {"id": "only-an-id"},
        }))
        .unwrap();
        assert!(raw.into_session_info().is_err());
    }

    #[test]
    fn active_state_distinguishes_running_unknown_and_absent() {
        let active: ActiveSessions =
            serde_json::from_value(serde_json::json!({"data": {"ses_run": {"type": "running"}}})).unwrap();
        assert_eq!(active.state("ses_run"), Some(true));
        assert_eq!(active.state("ses_gone"), None);

        let odd: ActiveSessions =
            serde_json::from_value(serde_json::json!({"data": {"ses_odd": {"type": "zombie"}}})).unwrap();
        assert_eq!(
            odd.state("ses_odd"),
            Some(false),
            "unknown is never guessed as running"
        );
    }

    #[test]
    fn newest_assistant_retrying_reads_the_retry_field_not_a_null_or_a_absence() {
        let page: MessagesPage = serde_json::from_value(serde_json::json!({
            "data": [{"type": "assistant", "retry": {"attempt": 2, "at": 5000, "error": {"message": "boom"}}}]
        }))
        .unwrap();
        assert!(page.newest_assistant_retrying());

        let cleared: MessagesPage = serde_json::from_value(serde_json::json!({
            "data": [{"type": "assistant", "retry": null}]
        }))
        .unwrap();
        assert!(!cleared.newest_assistant_retrying());

        let none: MessagesPage = serde_json::from_value(serde_json::json!({
            "data": [{"type": "assistant", "content": []}]
        }))
        .unwrap();
        assert!(!none.newest_assistant_retrying());

        let empty: MessagesPage = serde_json::from_value(serde_json::json!({"data": []})).unwrap();
        assert!(!empty.newest_assistant_retrying());
    }

    /// The list cursor is a body field: a missing `cursor` object and a cursor
    /// without `next` both mean "the end".
    #[test]
    fn page_cursor_reads_next_or_the_end() {
        let page: SessionListPage = serde_json::from_value(serde_json::json!({
            "data": [],
            "cursor": {"previous": "p", "next": "n"}
        }))
        .unwrap();
        assert_eq!(page.cursor.next.as_deref(), Some("n"));

        let no_cursor: SessionListPage = serde_json::from_value(serde_json::json!({"data": []})).unwrap();
        assert!(no_cursor.cursor.next.is_none());
    }

    /// Decode one message's parts, for the content-level tests.
    fn parts_of(message: Value) -> Vec<Part> {
        decode_messages(&[message])
            .messages
            .into_iter()
            .next()
            .unwrap()
            .parts
    }

    /// A user message's `text` is its text part; a message with no text has no
    /// text part (never a manufactured empty one).
    #[test]
    fn a_user_message_decodes_its_text_and_never_invents_one() {
        let parts = parts_of(serde_json::json!({"id": "msg_u", "type": "user", "text": "你好"}));
        assert_eq!(
            parts,
            vec![Part::Text(TextPart {
                text: "你好".into(),
                started_at: None
            })]
        );

        assert!(parts_of(serde_json::json!({"id": "msg_u", "type": "user"})).is_empty());
        // A non-string text is malformed and stays raw, as in V1.
        assert_eq!(
            parts_of(serde_json::json!({"id": "msg_u", "type": "user", "text": 7})),
            vec![Part::Other(OtherPart {
                kind: "text".into(),
                raw: Value::Number(7.into())
            })]
        );
    }

    /// A synthetic message is NOT a user message: it must never become a Turn
    /// anchor. Its verbatim type and text survive into the neutral view.
    #[test]
    fn a_synthetic_message_keeps_its_type_and_stays_out_of_the_user_role() {
        let transcript = decode_messages(&[serde_json::json!({
            "id": "msg_s", "type": "synthetic", "text": "continue"
        })]);
        let message = &transcript.messages[0];
        assert_eq!(message.role, MessageRole::Other("synthetic".into()));
        assert_eq!(message.text(), "continue");
        assert!(transcript.newest_user().is_none());
    }

    /// Assistant content decodes in order: text, reasoning (with its per-item
    /// start time) and tool; the message's `finish` closes the step.
    #[test]
    fn assistant_content_decodes_in_order_with_finish_as_a_step_finish_part() {
        let transcript = decode_messages(&[serde_json::json!({
            "id": "msg_a",
            "type": "assistant",
            "time": {"created": 1000, "streamed": 1100, "completed": 1200},
            "model": {"id": "deepseek-v4-flash", "providerID": "opencode-go", "variant": "high"},
            "tokens": {"input": 11, "output": 7, "reasoning": 3, "cache": {"read": 5, "write": 2}},
            "content": [
                {"type": "text", "text": "答案"},
                {"type": "reasoning", "text": "想想", "time": {"created": 1050, "completed": 1060}},
                {"type": "tool", "id": "call_1", "name": "shell",
                 "state": {"status": "completed", "input": {"command": "ls"},
                           "content": [{"type": "text", "text": "a.rs"}],
                           "metadata": {"exit": 0}},
                 "time": {"created": 1070, "ran": 1080, "completed": 1190}}
            ],
            "finish": "stop"
        })]);
        let message = &transcript.messages[0];

        assert_eq!(message.role, MessageRole::Assistant);
        assert_eq!(message.time.unwrap().created, 1000);
        assert_eq!(message.time.unwrap().completed, Some(1200));
        let model = message.model.as_ref().unwrap();
        assert_eq!(model.provider_id, "opencode-go");
        assert_eq!(model.model_id, "deepseek-v4-flash");
        assert_eq!(model.variant.as_deref(), Some("high"));
        assert_eq!(
            message.tokens.unwrap(),
            TokenUsage {
                input: 11,
                output: 7,
                total: 28,
                cache_read: 5,
                cache_write: 2,
            },
            "V2's token shape has no total; the neutral total is upstream's own sum"
        );

        assert_eq!(
            message.parts[0],
            Part::Text(TextPart {
                text: "答案".into(),
                started_at: None
            })
        );
        assert_eq!(
            message.parts[1],
            Part::Reasoning(ReasoningPart {
                text: "想想".into(),
                started_at: Some(1050)
            })
        );
        let Part::Tool(tool) = &message.parts[2] else {
            panic!("expected a tool part: {:?}", message.parts[2]);
        };
        assert_eq!(tool.identity.name, "shell");
        assert_eq!(tool.identity.call_id, "call_1");
        assert_eq!(tool.status, ToolStatus::Completed);
        assert_eq!(tool.started_at, Some(1070));
        assert_eq!(tool.input.as_ref().unwrap()["command"], "ls");
        assert_eq!(tool.metadata.as_ref().unwrap()["exit"], 0);
        assert_eq!(tool.output.blocks, vec![ContentBlock::Text("a.rs".into())]);
        assert_eq!(
            message.parts[3],
            Part::StepFinish(StepFinish {
                reason: FinishReason::Stop
            })
        );
    }

    /// The reserved `"default"` variant sentinel is unset on message refs too
    /// (ADR-0020: clearing is a mechanism, never a value word) — the same
    /// normalization the session read applies — while a real level and a
    /// missing variant keep their meaning.
    #[test]
    fn message_model_refs_never_carry_the_reserved_default_variant() {
        fn assistant(variant: Option<&str>) -> Value {
            let mut model = serde_json::json!({"id": "deepseek-v4-flash", "providerID": "opencode-go"});
            if let Some(variant) = variant {
                model["variant"] = Value::String(variant.to_string());
            }
            serde_json::json!({"id": "msg_a", "type": "assistant", "model": model})
        }

        let defaulted = decode_messages(&[assistant(Some("default"))]);
        assert!(
            defaulted.messages[0].model.as_ref().unwrap().variant.is_none(),
            "the transcript ref must not carry the sentinel"
        );
        assert!(
            decode_message_model(&assistant(Some("default")))
                .expect("a model")
                .variant
                .is_none(),
            "the message-ref decode must not carry the sentinel"
        );
        assert_eq!(
            decode_message_model(&assistant(Some("high")))
                .expect("a model")
                .variant
                .as_deref(),
            Some("high")
        );
        assert!(
            decode_message_model(&assistant(None))
                .expect("a model")
                .variant
                .is_none()
        );
    }

    /// An assistant still in flight has no completion stamp and no finish: it
    /// belongs to whatever turn is current.
    #[test]
    fn an_inflight_assistant_has_no_completion_and_no_finish_part() {
        let transcript = decode_messages(&[serde_json::json!({
            "id": "msg_a",
            "type": "assistant",
            "time": {"created": 1000, "streamed": 1100},
            "content": [{"type": "reasoning", "text": "还在想"}]
        })]);
        let message = &transcript.messages[0];
        assert_eq!(message.time.unwrap().completed, None);
        assert!(
            !message
                .parts
                .iter()
                .any(|part| matches!(part, Part::StepFinish(_))),
            "an unreported finish is not a completion: {:?}",
            message.parts
        );
    }

    /// The tool states: `streaming` is the not-yet-executed pending arm,
    /// `running`/`completed`/`error` keep their neutral arms, and an unknown or
    /// missing one is never guessed.
    #[test]
    fn tool_states_map_onto_the_neutral_arms() {
        let tool = |state: Value| {
            let parts = parts_of(serde_json::json!({
                "id": "msg_a", "type": "assistant", "content": [
                    {"type": "tool", "id": "call_1", "name": "shell", "state": state}
                ]
            }));
            let Part::Tool(call) = &parts[0] else {
                panic!("expected a tool part: {:?}", parts[0]);
            };
            call.clone()
        };

        let streaming = tool(serde_json::json!({"status": "streaming", "input": "{\"command\":"}));
        assert_eq!(streaming.status, ToolStatus::Pending);
        assert_eq!(
            streaming.input.as_ref().unwrap().as_str().unwrap(),
            "{\"command\":"
        );
        assert!(streaming.output.blocks.is_empty());

        let running = tool(serde_json::json!({"status": "running", "input": {}, "metadata": {}}));
        assert_eq!(running.status, ToolStatus::Running);
        assert!(running.status.is_live());

        let failed = tool(serde_json::json!({
            "status": "error",
            "input": {},
            "error": {"type": "Tool.Error", "message": "boom"},
            "content": [{"type": "text", "text": "partial"}]
        }));
        assert_eq!(failed.status, ToolStatus::Error);
        assert_eq!(failed.output.error.as_deref(), Some("boom"));
        assert_eq!(failed.output.blocks, vec![ContentBlock::Text("partial".into())]);

        assert_eq!(
            tool(serde_json::json!({"status": "weird"})).status,
            ToolStatus::Other("weird".into())
        );
        assert_eq!(tool(serde_json::json!({})).status, ToolStatus::Unknown);
    }

    /// A `file` output block stays raw: only `text` content is text, and
    /// per-tool payload knowledge is the Platform's (ADR-0042).
    #[test]
    fn a_file_output_block_stays_raw() {
        let file = serde_json::json!({"type": "file", "uri": "file:///a", "mime": "text/plain"});
        let parts = parts_of(serde_json::json!({
            "id": "msg_a", "type": "assistant", "content": [
                {"type": "tool", "id": "call_1", "name": "read",
                 "state": {"status": "completed", "input": {},
                           "content": [file.clone(), {"type": "text", "text": "a.rs"}]}}
            ]
        }));
        let Part::Tool(call) = &parts[0] else {
            panic!("expected a tool part: {:?}", parts[0]);
        };
        assert_eq!(
            call.output.blocks,
            vec![ContentBlock::Other(file), ContentBlock::Text("a.rs".into())]
        );
        assert_eq!(call.output.raw.as_ref().unwrap().as_array().unwrap().len(), 2);
    }

    /// Every message type this build does not model (shell, compaction, idle,
    /// the switch markers) keeps its raw payload as one part and stays out of
    /// the conversation roles.
    #[test]
    fn unmodelled_message_types_keep_their_raw_payload() {
        let transcript = decode_messages(&[
            serde_json::json!({"id": "msg_c", "type": "compaction", "status": "completed", "summary": "s"}),
            serde_json::json!({"id": "msg_i", "type": "idle", "outcome": "succeeded", "time": {"created": 1}}),
        ]);

        for (message, kind) in transcript.messages.iter().zip(["compaction", "idle"]) {
            assert_eq!(message.role, MessageRole::Other(kind.into()));
            let Part::Other(raw) = &message.parts[0] else {
                panic!("expected a raw part: {:?}", message.parts[0]);
            };
            assert_eq!(raw.kind, kind);
            assert_eq!(raw.raw.get("type").and_then(Value::as_str), Some(kind));
        }
    }

    /// The message id survives verbatim (V2's `msg_` rule is tighter than V1's
    /// but the neutral model only carries it), and a message without a numeric
    /// `time.created` is timeless rather than anchored at epoch 0.
    #[test]
    fn id_survives_and_a_message_without_a_numeric_time_is_timeless() {
        let transcript = decode_messages(&[serde_json::json!({
            "id": "msg_cola_live_fixture",
            "type": "user",
            "text": "hi",
            "time": {"created": "not-a-number"}
        })]);
        let message = &transcript.messages[0];
        assert_eq!(message.id.as_str(), "msg_cola_live_fixture");
        assert!(message.time.is_none());
        assert!(message.anchor().is_none());
    }

    /// The admitted prompt keeps its id and nothing else about the envelope
    /// matters to the write path.
    #[test]
    fn admitted_prompt_reads_the_inbox_id() {
        let admitted: RawAdmittedPrompt = serde_json::from_value(serde_json::json!({
            "id": "msg_cola_1",
            "sessionID": "ses_1",
            "type": "user",
            "delivery": "steer",
            "time": {"created": 1},
            "payload": {"text": "hi"},
        }))
        .unwrap();
        assert_eq!(admitted.id, "msg_cola_1");
    }

    /// A projected message's `error` field lands on the neutral message — the
    /// async-native Turn derives a settled turn's failure from it (the
    /// turn-level membership/newest rule is the neutral transcript's).
    #[test]
    fn message_errors_decode_onto_the_neutral_message() {
        let transcript = decode_messages(&[
            serde_json::json!({
                "id": "msg_new", "type": "assistant",
                "time": {"created": 110, "completed": 120},
                "error": {"type": "ProviderError", "message": "new boom"},
            }),
            serde_json::json!({
                "id": "msg_null", "type": "assistant",
                "time": {"created": 130, "completed": 140},
                "error": null,
            }),
            serde_json::json!({
                "id": "msg_str", "type": "assistant",
                "time": {"created": 150, "completed": 160},
                "error": "just a string",
            }),
        ]);
        assert_eq!(
            transcript.messages[0].error.as_deref(),
            Some("new boom"),
            "an object error normalizes to its message"
        );
        assert!(
            transcript.messages[1].error.is_none(),
            "an explicit null is no error"
        );
        assert_eq!(transcript.messages[2].error.as_deref(), Some("just a string"));
    }
}
