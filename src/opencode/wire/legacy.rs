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

/// Decode a tool state's output side. Text sources follow the precedence the
/// presentation has always rendered: a string `output` is authoritative (the
/// other sources are never appended to it); otherwise the `content` text runs
/// and a string `result` are joined; otherwise `metadata.output` is the last
/// resort. A non-string `output` — and a null or absent one — falls through to
/// those sources. Non-text blocks (and a text block that lost its text) stay
/// raw instead of vanishing; the raw payload is preserved so per-tool
/// presentation stays in the Platform (ADR-0042). A failure's reason lives
/// apart from the output as [`ToolOutput::error`].
fn decode_tool_output(state: Option<&Value>) -> ToolOutput {
    let Some(state) = state else {
        return ToolOutput::default();
    };
    let mut blocks = Vec::new();
    match non_null(state.get("output")).and_then(Value::as_str) {
        Some(output) => blocks.push(ContentBlock::Text(output.to_string())),
        None => {
            let mut text = String::new();
            let mut raw_blocks = Vec::new();
            if let Some(items) = state.get("content").and_then(Value::as_array) {
                for item in items {
                    match content_text(item) {
                        Some(part) => text.push_str(part),
                        None => raw_blocks.push(ContentBlock::Other(item.clone())),
                    }
                }
            }
            if let Some(result) = non_null(state.get("result")).and_then(Value::as_str) {
                if !text.is_empty() {
                    text.push('\n');
                }
                text.push_str(result);
            }
            if text.is_empty()
                && let Some(fallback) = state.pointer("/metadata/output").and_then(Value::as_str)
            {
                text.push_str(fallback);
            }
            if !text.is_empty() {
                blocks.push(ContentBlock::Text(text));
            }
            blocks.extend(raw_blocks);
        }
    }
    ToolOutput {
        // The first output-bearing field the payload actually carries.
        raw: non_null(state.get("output"))
            .or_else(|| non_null(state.get("content")))
            .or_else(|| non_null(state.get("result")))
            .or_else(|| non_null(state.pointer("/metadata/output")))
            .cloned(),
        blocks,
        error: state.get("error").and_then(decode_error),
    }
}

/// A JSON field that was actually reported: absent and explicit `null` both
/// read as "not there", so neither shadows another output source.
fn non_null(value: Option<&Value>) -> Option<&Value> {
    value.filter(|value| !value.is_null())
}

/// The text of a `content` text block, when it carries any.
fn content_text(item: &Value) -> Option<&str> {
    if item.get("type").and_then(Value::as_str) != Some("text") {
        return None;
    }
    item.get("text").and_then(Value::as_str)
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

    /// A string `output` is authoritative: the `content`/`result` sources are
    /// never appended to it (matching the renderer's early return).
    #[test]
    fn a_string_output_is_the_only_text_source() {
        let call = tool(serde_json::json!({
            "status": "completed",
            "output": "only",
            "content": [{"type": "text", "text": "ignored"}],
            "result": "ignored"
        }));

        assert_eq!(call.output.blocks, vec![ContentBlock::Text("only".into())]);
        assert_eq!(call.output.raw, Some(Value::String("only".into())));
    }

    /// An explicit `null` output must not shadow the `content`/`result` text
    /// the state actually carries; non-text blocks stay raw.
    #[test]
    fn null_output_falls_through_to_content_and_result() {
        let call = tool(serde_json::json!({
            "status": "completed",
            "output": null,
            "content": [
                {"type": "text", "text": "line1"},
                {"type": "file", "uri": "file:///a"},
                {"type": "text", "text": "line2"}
            ],
            "result": "done"
        }));

        assert_eq!(
            call.output.blocks,
            vec![
                ContentBlock::Text("line1line2\ndone".into()),
                ContentBlock::Other(serde_json::json!({"type": "file", "uri": "file:///a"})),
            ]
        );
        assert_eq!(
            call.output.raw,
            Some(serde_json::json!([
                {"type": "text", "text": "line1"},
                {"type": "file", "uri": "file:///a"},
                {"type": "text", "text": "line2"}
            ]))
        );
        assert!(call.output.error.is_none());
    }

    /// A non-string `output` is not text and must fall through rather than
    /// suppressing the other sources.
    #[test]
    fn non_string_output_falls_through_to_content_and_result() {
        let call = tool(serde_json::json!({
            "status": "completed",
            "output": {"unexpected": true},
            "content": [{"type": "text", "text": "content"}],
            "result": "result"
        }));

        assert_eq!(
            call.output.blocks,
            vec![ContentBlock::Text("content\nresult".into())]
        );
        assert_eq!(call.output.raw, Some(serde_json::json!({"unexpected": true})));
    }

    /// `metadata.output` is the historical last resort — only when nothing
    /// else produced text — and never overrides real text.
    #[test]
    fn metadata_output_is_the_last_resort_text() {
        let call = tool(serde_json::json!({
            "status": "completed",
            "metadata": {"output": "from metadata"}
        }));
        assert_eq!(
            call.output.blocks,
            vec![ContentBlock::Text("from metadata".into())]
        );
        assert_eq!(call.output.raw, Some(Value::String("from metadata".into())));

        let call = tool(serde_json::json!({
            "status": "completed",
            "content": [{"type": "text", "text": "real"}],
            "metadata": {"output": "ignored"}
        }));
        assert_eq!(call.output.blocks, vec![ContentBlock::Text("real".into())]);
    }

    /// A string output is the whole text; a `content` text block that lost its
    /// text stays raw instead of vanishing.
    #[test]
    fn a_content_text_block_that_lost_its_text_stays_raw() {
        let call = tool(serde_json::json!({
            "status": "completed",
            "output": "legacy text"
        }));
        assert_eq!(call.output.blocks, vec![ContentBlock::Text("legacy text".into())]);
        assert_eq!(call.output.raw, Some(Value::String("legacy text".into())));

        let call = tool(serde_json::json!({
            "status": "completed",
            "content": [{"type": "text"}]
        }));
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
