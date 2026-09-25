//! Private protocol decoders: one per API generation (ADR-0053).
//!
//! Backend protocol field names live ONLY here. The adapter selects the
//! generation; the Bridge consumes the neutral
//! [`SessionTranscript`](crate::backend::SessionTranscript) and never branches
//! on a generation. This module ships both generations: [`legacy`] decodes the
//! unprefixed V1 routes and [`v2`] the current `/api` routes.
//!
//! The wire types are private to their generation's decoder: from outside the
//! adapter only the decode functions are callable, so no protocol field name
//! can escape the seam (spec #332).
//!
//! The normalizations both generations share — tool statuses, finish reasons,
//! failure shapes, output text, server times — live in this module so their
//! tolerant arms cannot drift apart.

pub(crate) mod legacy;
pub(crate) mod v2;

use crate::backend::{FinishReason, Part, SessionTranscript, ToolStatus};
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
pub(crate) fn decode_page(json: &Value) -> crate::error::Result<v2::Page> {
    v2::decode_page(json)
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
