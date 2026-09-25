//! Decoder for the legacy (unprefixed) route generation.
//!
//! `GET /session/{id}/message` returns `{info, parts}` envelopes whose parts
//! are raw JSON: the generation's field names (`callID`, `state.status`,
//! `state.output`, `metadata.diff`, ...) live in this module alone. Every
//! shape the server has ever served is decoded tolerantly: unknown parts and
//! statuses keep their raw payload, and a missing field never fails the read.

use serde_json::Value;

use crate::backend::{
    ContentBlock, FinishReason, MessageId, MessageRole, MessageTime, ModelIdentity, OtherPart, Part, Patch,
    ReasoningPart, SessionTranscript, StepFinish, StepStart, TextPart, TokenUsage, ToolCall, ToolIdentity,
    ToolOutput, ToolStatus, TranscriptMessage,
};
use crate::opencode::types::{MessageTokens, SessionMessage};

/// Decode every message of one session, preserving the server's order.
pub(super) fn decode(messages: &[SessionMessage]) -> SessionTranscript {
    SessionTranscript::new(messages.iter().map(decode_message).collect())
}

fn decode_message(message: &SessionMessage) -> TranscriptMessage {
    let info = &message.info;
    TranscriptMessage {
        id: MessageId::new(info.id.clone()),
        role: decode_role(info.role.as_deref()),
        time: info.time.as_ref().map(|time| MessageTime {
            created: time.created,
            completed: time.completed,
        }),
        model: info.model_id.as_ref().map(|model_id| ModelIdentity {
            provider_id: info.provider_id.clone().unwrap_or_default(),
            model_id: model_id.clone(),
            // The legacy message envelope carries no variant; the `/think`
            // variant cola sent is session state, not message state.
            variant: None,
        }),
        tokens: info.tokens.as_ref().map(decode_tokens),
        parts: message
            .parts
            .as_array()
            .map(|parts| parts.iter().map(decode_part).collect())
            .unwrap_or_default(),
    }
}

fn decode_role(role: Option<&str>) -> MessageRole {
    match role {
        Some("user") => MessageRole::User,
        Some("assistant") => MessageRole::Assistant,
        Some("system") => MessageRole::System,
        Some(other) => MessageRole::Other(other.to_string()),
        None => MessageRole::Unknown,
    }
}

fn decode_tokens(tokens: &MessageTokens) -> TokenUsage {
    TokenUsage {
        input: tokens.input,
        output: tokens.output,
        total: tokens.total,
        cache_read: tokens.cache.as_ref().map(|cache| cache.read).unwrap_or(0),
        cache_write: tokens.cache.as_ref().map(|cache| cache.write).unwrap_or(0),
    }
}

fn decode_part(part: &Value) -> Part {
    match part.get("type").and_then(Value::as_str) {
        Some("text") => Part::Text(TextPart {
            text: part
                .get("text")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
            started_at: started_at(part, "/time/start"),
        }),
        Some("reasoning") => Part::Reasoning(ReasoningPart {
            text: part
                .get("text")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
            started_at: started_at(part, "/time/start"),
        }),
        Some("tool") => Part::Tool(decode_tool(part)),
        Some("step-start") => Part::StepStart(StepStart),
        Some("step-finish") => Part::StepFinish(StepFinish {
            reason: decode_finish_reason(part.get("reason")),
        }),
        Some("patch") => Part::Patch(Patch {
            hash: part.get("hash").and_then(Value::as_str).map(str::to_string),
            files: part
                .get("files")
                .and_then(Value::as_array)
                .map(|files| {
                    files
                        .iter()
                        .filter_map(Value::as_str)
                        .map(str::to_string)
                        .collect()
                })
                .unwrap_or_default(),
        }),
        Some(other) => Part::Other(OtherPart {
            kind: other.to_string(),
            raw: part.clone(),
        }),
        None => Part::Other(OtherPart {
            kind: String::new(),
            raw: part.clone(),
        }),
    }
}

fn decode_tool(part: &Value) -> ToolCall {
    let name = part
        .get("tool")
        .and_then(Value::as_str)
        .unwrap_or("tool")
        .to_string();
    // The correlation id is the call's, never the tool's: it is what folds a
    // running call's later updates onto the same panel.
    let call_id = part
        .get("callID")
        .and_then(Value::as_str)
        .unwrap_or(name.as_str())
        .to_string();
    let state = part.get("state");
    ToolCall {
        identity: ToolIdentity { name, call_id },
        status: decode_tool_status(state.and_then(|state| state.get("status"))),
        started_at: state.and_then(|state| started_at(state, "/time/start")),
        input: state.and_then(|state| state.get("input")).cloned(),
        metadata: state.and_then(|state| state.get("metadata")).cloned(),
        output: decode_tool_output(state),
    }
}

fn decode_tool_status(status: Option<&Value>) -> ToolStatus {
    match status.and_then(Value::as_str) {
        Some("pending") => ToolStatus::Pending,
        Some("running") => ToolStatus::Running,
        Some("completed") => ToolStatus::Completed,
        Some("error") => ToolStatus::Error,
        Some(other) => ToolStatus::Unknown(other.to_string()),
        None => ToolStatus::Unknown(String::new()),
    }
}

/// Decode a tool state's output side. Two server generations meet here: the
/// legacy `state.output` string and the current generation's `state.content`
/// blocks plus `state.result`. Both normalize into content blocks; the raw
/// payload is preserved so per-tool presentation stays in the Platform
/// (ADR-0042). A failure's reason lives apart from the output as
/// [`ToolOutput::error`].
fn decode_tool_output(state: Option<&Value>) -> ToolOutput {
    let Some(state) = state else {
        return ToolOutput::default();
    };
    let mut blocks = Vec::new();
    let raw = if let Some(output) = state.get("output") {
        if let Some(text) = output.as_str() {
            blocks.push(ContentBlock::Text(text.to_string()));
        } else {
            blocks.push(ContentBlock::Other(output.clone()));
        }
        Some(output.clone())
    } else if let Some(content) = state.get("content") {
        if let Some(items) = content.as_array() {
            for item in items {
                if item.get("type").and_then(Value::as_str) == Some("text") {
                    if let Some(text) = item.get("text").and_then(Value::as_str) {
                        blocks.push(ContentBlock::Text(text.to_string()));
                    }
                } else {
                    blocks.push(ContentBlock::Other(item.clone()));
                }
            }
        }
        Some(content.clone())
    } else if let Some(result) = state.get("result") {
        if let Some(text) = result.as_str() {
            blocks.push(ContentBlock::Text(text.to_string()));
        } else {
            blocks.push(ContentBlock::Other(result.clone()));
        }
        Some(result.clone())
    } else {
        None
    };
    ToolOutput {
        raw,
        blocks,
        error: state.get("error").and_then(decode_error),
    }
}

/// Normalize a failure payload: a plain string (`"Could not find ..."`) or an
/// object carrying `message`.
fn decode_error(error: &Value) -> Option<String> {
    match error {
        Value::String(message) => Some(message.clone()),
        Value::Object(fields) => fields.get("message").and_then(Value::as_str).map(str::to_string),
        _ => None,
    }
}

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

/// A JSON pointer to a server epoch-millis number, when the payload carries
/// one. A float or out-of-range number is not a usable clock and reads as
/// absent.
fn started_at(value: &Value, pointer: &str) -> Option<i64> {
    value.pointer(pointer).and_then(Value::as_i64)
}
