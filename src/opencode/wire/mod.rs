//! Private protocol decoders: one per API generation (ADR-0053).
//!
//! Backend protocol field names live ONLY here. The adapter selects the
//! generation; the Bridge consumes the neutral
//! [`SessionTranscript`](crate::backend::SessionTranscript) and never branches
//! on a generation. This module ships both generations: [`legacy`] decodes the
//! unprefixed V1 routes and [`v2`] the current `/api` routes.
//!
//! The wire types are private to their generation's decoder: from outside the
//! adapter only the decode functions and the generation-neutral [`Page`] are
//! callable, so no wire shape or protocol field name can escape the seam
//! (spec #332).
//!
//! The normalizations both generations share — tool statuses, finish reasons,
//! failure shapes, tool identity, string lists, output text and its existence,
//! server times — live in this module so their tolerant arms cannot drift
//! apart.

pub(crate) mod legacy;
pub(crate) mod v2;

pub(crate) use v2::Page;

use crate::backend::{
    ContentBlock, FinishReason, OtherPart, Part, SessionTranscript, TextPart, ToolIdentity, ToolStatus,
};
use serde_json::Value;

/// Decode one session's wire message read — the JSON a
/// `GET /session/{id}/message` response carries — through the legacy
/// generation's decoder.
pub(crate) fn decode_response(json: &Value) -> crate::error::Result<SessionTranscript> {
    legacy::decode_response(json)
}

/// Decode one raw parts array — a prompt response's `parts` — through the same
/// legacy decoder as a polled message's, so the prompt-response fallback
/// cannot smuggle raw protocol shapes back into the Bridge (spec #332).
/// Prompts stay on the legacy route while it remains mounted.
pub(crate) fn decode_parts(parts: &Value) -> Vec<Part> {
    legacy::decode_parts(parts)
}

/// Decode one `/api` (V2) page — the `{data, cursor}` envelope a
/// `GET /api/session/{id}/message` response carries.
pub(crate) fn decode_page(json: &Value) -> crate::error::Result<Page> {
    v2::decode_page(json)
}

/// One `text` field as a text part: a string becomes [`Part::Text`]; a missing
/// or non-string field is malformed and stays raw as the WHOLE payload, so a
/// valid+malformed pair never joins an extra line and nothing is lost. An
/// explicit empty string is still a valid empty text part. Both generations
/// share this rule (a legacy part, a `/api` content item, or a `/api`
/// message-level text field), so their tolerant arms cannot drift.
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

/// A tool lifecycle status. Both generations use the same names; anything
/// else keeps its spelling and a missing one is its own arm.
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

/// Why a step finished. Both generations spell the reasons the same way; an
/// unknown one keeps its name and a missing one never declares completion.
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

/// Normalize a failure payload. The legacy generation serves a plain string
/// (`"Could not find ..."`) or an object carrying `message`; the `/api`
/// generation serves `{type, message}` — the same object arm reads both.
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
/// name so a call without one still folds onto a single panel. Both
/// generations carry the pair under different field names but with the same
/// fallback order, so sharing it keeps the arms from drifting apart.
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
/// server saying "no output", not an output. Both decoders apply this when
/// they pick the raw payload, so an empty container neither masks a later
/// source nor counts as output on its own.
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

/// Assemble a tool state's shared output side from the two sources both
/// generations carry: the `content` items and a string `result`. Text runs
/// join verbatim; a string `result` follows on a new line; every non-text item
/// (a text item that lost its text included) stays raw. Returns the joined
/// text — empty when nothing contributed — and the raw blocks, so each
/// generation layers its own precedence around the same assembly.
fn assemble_tool_output(content: Option<&Value>, result: Option<&Value>) -> (String, Vec<ContentBlock>) {
    let mut text = String::new();
    let mut raw_blocks = Vec::new();
    if let Some(items) = content.and_then(Value::as_array) {
        for item in items {
            match content_text(item) {
                Some(part) => text.push_str(part),
                None => raw_blocks.push(ContentBlock::Other(item.clone())),
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
