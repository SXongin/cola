//! Private protocol decoder for the unprefixed message/transcript read
//! (ADR-0053).
//!
//! cola follows the attached generation for the conversation: V1's routes are
//! unprefixed (prompts, permissions, questions, the transcript read), V2's are
//! under `/api`. This module decodes the unprefixed message/transcript read
//! for V1; V2's decoder is its sibling inside the V2 strategy
//! ([`crate::opencode::v2`]), because the two generations' message shapes
//! share no wire spellings. The current-generation calls both generations
//! serve live on the generation-blind adapter ([`crate::opencode::client`]):
//! session creation posts `POST /api/session` there, and compaction rides
//! `/api/session/{id}/compact` through each strategy (the path is shared, the
//! body/response contract is not).
//!
//! Backend protocol field names for the message/transcript read live ONLY
//! here: from outside the adapter only the decode functions are callable, so
//! no wire shape or protocol field name can escape the seam (spec #332). The
//! Bridge consumes the neutral
//! [`SessionTranscript`](crate::backend::SessionTranscript) and never branches
//! on a generation.
//!
//! The public session/permission/provider/agent DTOs in
//! [`crate::opencode::types`] are the known out-of-scope exception (spec
//! #332): they still spell the list/read wire fields, and the session list's
//! raw `model` payload is normalized where it is displayed
//! (`crate::bridge::display::model_display`).
//!
//! The normalizations the decoder uses — tool statuses, finish reasons,
//! failure shapes, tool identity, string lists, output text and its existence,
//! server times — live in this module so their tolerant arms cannot drift
//! apart.

pub(crate) mod legacy;

use crate::backend::{
    ContentBlock, FileContent, FinishReason, OtherPart, Part, SessionTranscript, TextPart, ToolIdentity,
    ToolStatus,
};
use serde_json::Value;

/// Decode one session's wire message read — the JSON a
/// `GET /session/{id}/message` response carries — through the wire decoder.
pub(crate) fn decode_response(json: &Value) -> crate::error::Result<SessionTranscript> {
    legacy::decode_response(json)
}

/// One `text` field as a text part: a string becomes [`Part::Text`]; a missing
/// or non-string field is malformed and stays raw as the WHOLE payload, so a
/// valid+malformed pair never joins an extra line and nothing is lost. An
/// explicit empty string is still a valid empty text part.
fn decode_text_part(value: &Value, started_at: Option<i64>) -> Part {
    match value.get("text").and_then(Value::as_str) {
        Some(text) => Part::Text(TextPart {
            text: text.to_string(),
            started_at,
        }),
        None => Part::Other(OtherPart {
            kind: "text".to_string(),
            raw: value.clone(),
        }),
    }
}

/// A tool lifecycle status. Anything else keeps its spelling and a missing
/// one is its own arm.
fn decode_tool_status(status: Option<&Value>) -> ToolStatus {
    match status.and_then(Value::as_str) {
        Some("pending") => ToolStatus::Pending,
        Some("running") => ToolStatus::Running,
        Some("completed") => ToolStatus::Completed,
        Some("error") => ToolStatus::Error,
        Some(other) => ToolStatus::Other(other.to_string()),
        None => ToolStatus::Unknown,
    }
}

/// Why a step finished. An unknown reason keeps its name and a missing one
/// never declares completion.
fn decode_finish_reason(reason: Option<&Value>) -> FinishReason {
    match reason.and_then(Value::as_str) {
        Some("tool-calls") => FinishReason::ToolCalls,
        Some("stop") => FinishReason::Stop,
        Some("length") => FinishReason::Length,
        Some("content-filter") => FinishReason::ContentFilter,
        Some("error") => FinishReason::Error,
        Some(other) => FinishReason::Other(other.to_string()),
        None => FinishReason::Unknown,
    }
}

/// Normalize a failure payload: a plain string message or an object carrying
/// `message`.
fn decode_error(error: &Value) -> Option<String> {
    match error {
        Value::String(message) => Some(message.clone()),
        Value::Object(fields) => fields.get("message").and_then(Value::as_str).map(str::to_string),
        _ => None,
    }
}

/// A JSON field that was actually reported: absent and explicit `null` both
/// read as "not there", so neither shadows another output source.
fn non_null(value: Option<&Value>) -> Option<&Value> {
    value.filter(|value| !value.is_null())
}

/// The text of a `content` item, when it carries any: any item with a string
/// `text` contributes, regardless of its kind (the renderer never checks the
/// kind either).
fn content_text(item: &Value) -> Option<&str> {
    item.get("text").and_then(Value::as_str)
}

/// A JSON pointer to a server epoch-millis number, when the payload carries
/// one. A float or out-of-range number is not a usable clock and reads as
/// absent.
fn started_at(value: &Value, pointer: &str) -> Option<i64> {
    value.pointer(pointer).and_then(Value::as_i64)
}

/// A tool's identity: its built-in name — the historical `"tool"` when the
/// payload lost it — and the call's opaque correlation id, falling back to the
/// name so a call without one still folds onto a single panel.
fn decode_tool_identity(name: Option<&Value>, call_id: Option<&Value>) -> ToolIdentity {
    let name = name.and_then(Value::as_str).unwrap_or("tool").to_string();
    let call_id = call_id
        .and_then(Value::as_str)
        .unwrap_or(name.as_str())
        .to_string();
    ToolIdentity { name, call_id }
}

/// The strings of an array field (a patch's `files`, a snapshot's `files`):
/// non-string items are skipped and a missing or non-array field yields none.
fn string_list(field: Option<&Value>) -> Vec<String> {
    field
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

/// Whether a payload field carries anything: an empty array or object is the
/// server saying "no output", not an output. The decoder applies this when it
/// picks the raw payload, so an empty container neither masks a later source
/// nor counts as output on its own.
fn has_payload(value: &Value) -> bool {
    match value {
        Value::Array(items) => !items.is_empty(),
        Value::Object(fields) => !fields.is_empty(),
        _ => true,
    }
}

/// Whether a decoded failure message suppresses the `metadata.output` last
/// resort: the panel appends `❌ …` after the output text, and the historical
/// extractor counted that line as output — so the fallback never rendered
/// beside it. An error status without a decoded message still falls back.
fn error_suppresses_fallback(status: &ToolStatus, error: Option<&str>) -> bool {
    *status == ToolStatus::Error && error.is_some()
}

/// One non-text tool-output content item: the typed File Content when it
/// inlines its payload — the `file` block `read` returns for an image or PDF,
/// the same shape an MCP resource part carries — and the raw item otherwise, so
/// a `file://`/`https://` reference is preserved rather than decoded.
fn content_block(item: &Value) -> ContentBlock {
    if item.get("type").and_then(Value::as_str) == Some("file")
        && let Some(uri) = non_null(item.get("uri")).and_then(Value::as_str)
        && let Some(content) = FileContent::decode(
            uri,
            non_null(item.get("mime")).and_then(Value::as_str),
            non_null(item.get("name")).and_then(Value::as_str),
        )
    {
        return ContentBlock::File(content);
    }
    ContentBlock::Other(item.clone())
}

/// Assemble a tool state's output side from the two sources the state carries:
/// the `content` items and a string `result`. Text runs join verbatim; a
/// string `result` follows on a new line; every non-text item (a text item
/// that lost its text included) stays raw. Returns the joined text — empty
/// when nothing contributed — and the raw blocks, so the caller layers its
/// precedence around the assembly.
fn assemble_tool_output(content: Option<&Value>, result: Option<&Value>) -> (String, Vec<ContentBlock>) {
    let mut text = String::new();
    let mut raw_blocks = Vec::new();
    if let Some(items) = content.and_then(Value::as_array) {
        for item in items {
            match content_text(item) {
                Some(part) => text.push_str(part),
                None => raw_blocks.push(content_block(item)),
            }
        }
    }
    if let Some(result) = non_null(result).and_then(Value::as_str) {
        if !text.is_empty() {
            text.push('\n');
        }
        text.push_str(result);
    }
    (text, raw_blocks)
}
