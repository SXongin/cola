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
        Some(other) => ToolStatus::Other(other.to_string()),
        None => ToolStatus::Unknown,
    }
}

/// Decode a tool state's output side. The unprefixed route has served more
/// than one tool-state shape — an `output` string, and `content` blocks plus a
/// `result` — so every shape is normalized tolerantly: a null or absent field
/// never shadows the others, each text source becomes a text block, and any
/// value this build does not model stays raw rather than being dropped. The
/// raw payload is preserved so per-tool presentation stays in the Platform
/// (ADR-0042). A failure's reason lives apart from the output as
/// [`ToolOutput::error`].
fn decode_tool_output(state: Option<&Value>) -> ToolOutput {
    let Some(state) = state else {
        return ToolOutput::default();
    };
    let mut blocks = Vec::new();
    if let Some(output) = present(state.get("output")) {
        blocks.push(content_block(output));
    }
    if let Some(items) = state.get("content").and_then(Value::as_array) {
        for item in items {
            blocks.push(content_item_block(item));
        }
    }
    if let Some(result) = present(state.get("result")) {
        blocks.push(content_block(result));
    }
    ToolOutput {
        // The first output-bearing field the payload actually carries.
        raw: present(state.get("output"))
            .or_else(|| present(state.get("content")))
            .or_else(|| present(state.get("result")))
            .cloned(),
        blocks,
        error: state.get("error").and_then(decode_error),
    }
}

/// A JSON field that was actually reported: absent and explicit `null` both
/// read as "not there", so neither shadows another output source.
fn present(value: Option<&Value>) -> Option<&Value> {
    value.filter(|value| !value.is_null())
}

/// Normalize one output value: a string is its text, anything else keeps its
/// raw payload.
fn content_block(value: &Value) -> ContentBlock {
    match value.as_str() {
        Some(text) => ContentBlock::Text(text.to_string()),
        None => ContentBlock::Other(value.clone()),
    }
}

/// Normalize one `content` block: a text block contributes its text; anything
/// else — including a text block that lost its text — stays raw rather than
/// vanishing.
fn content_item_block(item: &Value) -> ContentBlock {
    if item.get("type").and_then(Value::as_str) == Some("text")
        && let Some(text) = item.get("text").and_then(Value::as_str)
    {
        return ContentBlock::Text(text.to_string());
    }
    ContentBlock::Other(item.clone())
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::opencode::types::{MessageInfo, MessageTime};

    /// Decode one tool part's state into its typed call.
    fn tool(state: Value) -> ToolCall {
        let message = SessionMessage {
            info: MessageInfo {
                id: "msg_a1".into(),
                role: Some("assistant".into()),
                parent_id: None,
                time: Some(MessageTime {
                    created: 1_000,
                    completed: Some(1_000),
                }),
                model_id: None,
                provider_id: None,
                tokens: None,
            },
            parts: serde_json::json!([
                {"type": "tool", "tool": "bash", "callID": "call_1", "state": state}
            ]),
        };
        let transcript = decode(&[message]);
        let Part::Tool(call) = &transcript.messages[0].parts[0] else {
            panic!("expected a tool part: {:?}", transcript.messages[0].parts[0]);
        };
        call.clone()
    }

    /// An explicit `null` output (routine on this schema) must not shadow the
    /// `content`/`result` sources the state actually carries.
    #[test]
    fn null_output_does_not_shadow_content_and_result() {
        let call = tool(serde_json::json!({
            "status": "completed",
            "output": null,
            "content": [
                {"type": "text", "text": "line1"},
                {"type": "file", "uri": "file:///a"}
            ],
            "result": "done"
        }));

        assert_eq!(
            call.output.blocks,
            vec![
                ContentBlock::Text("line1".into()),
                ContentBlock::Other(serde_json::json!({"type": "file", "uri": "file:///a"})),
                ContentBlock::Text("done".into()),
            ]
        );
        assert_eq!(
            call.output.raw,
            Some(serde_json::json!([
                {"type": "text", "text": "line1"},
                {"type": "file", "uri": "file:///a"}
            ]))
        );
        assert!(call.output.error.is_none());
    }

    /// A string output is the legacy body; a `content` text block that lost
    /// its text stays raw instead of vanishing.
    #[test]
    fn string_output_decodes_and_content_missing_text_stays_raw() {
        let call = tool(serde_json::json!({"status": "completed", "output": "legacy text"}));
        assert_eq!(call.output.blocks, vec![ContentBlock::Text("legacy text".into())]);
        assert_eq!(call.output.raw, Some(Value::String("legacy text".into())));

        let call = tool(serde_json::json!({"status": "completed", "content": [{"type": "text"}]}));
        assert_eq!(
            call.output.blocks,
            vec![ContentBlock::Other(serde_json::json!({"type": "text"}))]
        );
    }

    /// The two failure shapes the server has used normalize to the message.
    #[test]
    fn error_shapes_normalize_to_the_failure_message() {
        let call = tool(serde_json::json!({"status": "error", "error": "boom"}));
        assert_eq!(call.output.error.as_deref(), Some("boom"));

        let call = tool(serde_json::json!({"status": "error", "error": {"message": "boom", "type": "x"}}));
        assert_eq!(call.output.error.as_deref(), Some("boom"));
    }

    /// A missing status is its own arm; an unrecognized one keeps its name.
    #[test]
    fn missing_status_is_unknown_and_other_statuses_keep_their_name() {
        assert_eq!(tool(serde_json::json!({"input": {}})).status, ToolStatus::Unknown);
        assert_eq!(
            tool(serde_json::json!({"status": "weird"})).status,
            ToolStatus::Other("weird".into())
        );
    }
}
