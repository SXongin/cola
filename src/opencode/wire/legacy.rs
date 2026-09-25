//! Decoder for the legacy (unprefixed) route generation.
//!
//! `GET /session/{id}/message` returns `{info, parts}` envelopes whose parts
//! are raw JSON: the generation's field names (`callID`, `state.status`,
//! `state.output`, `metadata.diff`, ...) live in this module alone. Every
//! shape the server has ever served is decoded tolerantly: unknown parts and
//! statuses keep their raw payload, and a missing field never fails the read.
//!
//! The wire envelope types are private to this decoder: outside the adapter a
//! caller reaches a transcript only through [`decode_response`], so no protocol
//! field name can leak (ADR-0053).

use serde::Deserialize;
use serde_json::Value;

use crate::backend::{
    ContentBlock, MessageId, MessageRole, MessageTime, ModelIdentity, OtherPart, Part, Patch, ReasoningPart,
    SessionTranscript, StepFinish, StepStart, TokenUsage, ToolCall, ToolIdentity, ToolOutput,
    TranscriptMessage,
};
use crate::error::Result;

use super::{
    content_text, decode_error, decode_finish_reason, decode_text_part, decode_tool_status, non_null,
    started_at,
};

/// The legacy generation's message envelope (`{info, parts}`).
#[derive(Debug, Clone, Deserialize)]
struct WireSessionMessage {
    info: WireMessageInfo,
    #[serde(default)]
    parts: Value,
}

/// The legacy generation's message `info`.
#[derive(Debug, Clone, Deserialize)]
struct WireMessageInfo {
    id: String,
    #[serde(default)]
    role: Option<String>,
    #[serde(default)]
    time: Option<WireMessageTime>,
    #[serde(rename = "modelID", default)]
    model_id: Option<String>,
    #[serde(rename = "providerID", default)]
    provider_id: Option<String>,
    #[serde(default)]
    tokens: Option<WireMessageTokens>,
}

/// Token usage carried on an assistant message's `info.tokens`.
#[derive(Debug, Default, Clone, Deserialize)]
struct WireMessageTokens {
    #[serde(default)]
    input: i64,
    #[serde(default)]
    output: i64,
    #[serde(default)]
    total: i64,
    #[serde(default)]
    cache: Option<WireMessageTokenCache>,
}

#[derive(Debug, Default, Clone, Deserialize)]
struct WireMessageTokenCache {
    #[serde(default)]
    read: i64,
    #[serde(default)]
    write: i64,
}

/// A message's server time (epoch ms) on the wire. `completed` is absent while
/// the message is still in flight.
#[derive(Debug, Clone, Deserialize)]
struct WireMessageTime {
    created: i64,
    #[serde(default)]
    completed: Option<i64>,
}

/// Decode one session's message read — the JSON array a
/// `GET /session/{id}/message` response carries.
pub(super) fn decode_response(json: &Value) -> Result<SessionTranscript> {
    let messages = Vec::<WireSessionMessage>::deserialize(json)?;
    Ok(decode(&messages))
}

/// Decode every message of one session, preserving the server's order.
fn decode(messages: &[WireSessionMessage]) -> SessionTranscript {
    SessionTranscript::new(messages.iter().map(decode_message).collect())
}

/// Decode one raw parts array — a message's `parts` or a prompt response's —
/// through the same per-part decoder, so both reads cannot drift.
pub(super) fn decode_parts(parts: &Value) -> Vec<Part> {
    parts
        .as_array()
        .map(|parts| parts.iter().map(decode_part).collect())
        .unwrap_or_default()
}

fn decode_message(message: &WireSessionMessage) -> TranscriptMessage {
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
        parts: decode_parts(&message.parts),
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

fn decode_tokens(tokens: &WireMessageTokens) -> TokenUsage {
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
        // A `text` part without string text is malformed: the shared arm
        // keeps it raw rather than manufacturing an empty text the old
        // extractor dropped (a valid+malformed pair must not join an extra
        // line). An explicit empty string is still a valid empty text part.
        Some("text") => decode_text_part(part, started_at(part, "/time/start")),
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
            // `metadata.output` is the historical last resort and never
            // overrides real text. A present string is the text even when it
            // is empty, so an empty `metadata.output` still yields a text
            // block (the renderer returns `Some("")` there).
            if !text.is_empty() {
                blocks.push(ContentBlock::Text(text));
            } else if let Some(fallback) = state.pointer("/metadata/output").and_then(Value::as_str) {
                blocks.push(ContentBlock::Text(fallback.to_string()));
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::{TextPart, ToolStatus};

    /// Decode one tool part's state into its typed call.
    fn tool(state: Value) -> ToolCall {
        let message = WireSessionMessage {
            info: WireMessageInfo {
                id: "msg_a1".into(),
                role: Some("assistant".into()),
                time: Some(WireMessageTime {
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

    /// Any content item with a string `text` contributes — the kind is not
    /// checked (the renderer reads `text` off any item); an item without one
    /// stays raw.
    #[test]
    fn any_content_item_with_string_text_contributes_its_text() {
        let call = tool(serde_json::json!({
            "status": "completed",
            "content": [
                {"type": "file", "text": "file that carries text"},
                {"type": "file", "uri": "file:///a"},
                {"no": "type", "text": "untyped text"}
            ]
        }));

        assert_eq!(
            call.output.blocks,
            vec![
                ContentBlock::Text("file that carries textuntyped text".into()),
                ContentBlock::Other(serde_json::json!({"type": "file", "uri": "file:///a"})),
            ]
        );
    }

    /// An empty-but-present `metadata.output` is still the fallback text: the
    /// renderer returns `Some("")`, not `None`.
    #[test]
    fn an_empty_metadata_output_still_yields_a_text_block() {
        let call = tool(serde_json::json!({
            "status": "completed",
            "metadata": {"output": ""}
        }));

        assert_eq!(call.output.blocks, vec![ContentBlock::Text(String::new())]);
        assert_eq!(call.output.raw, Some(Value::String(String::new())));
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

    /// Decode one message's parts into its transcript message.
    fn decoded_message(parts: Value) -> TranscriptMessage {
        let transcript = decode(&[WireSessionMessage {
            info: WireMessageInfo {
                id: "msg_a1".into(),
                role: Some("assistant".into()),
                time: Some(WireMessageTime {
                    created: 1_000,
                    completed: Some(1_000),
                }),
                model_id: None,
                provider_id: None,
                tokens: None,
            },
            parts,
        }]);
        transcript.messages.into_iter().next().expect("one message")
    }

    /// A `text` part whose `text` field is not a string is malformed: it stays
    /// raw instead of becoming an empty text part, so the message text joins
    /// only the real text (the old snapshot extractor dropped the malformed
    /// part too — a valid+malformed pair must not grow a phantom newline).
    #[test]
    fn a_text_part_without_string_text_stays_raw() {
        let message = decoded_message(serde_json::json!([
            {"type": "text", "text": "a"},
            {"type": "text", "text": null},
            {"type": "text"}
        ]));

        assert_eq!(
            message.parts[0],
            Part::Text(TextPart {
                text: "a".into(),
                started_at: None
            })
        );
        assert_eq!(
            message.parts[1],
            Part::Other(OtherPart {
                kind: "text".into(),
                raw: serde_json::json!({"type": "text", "text": null})
            })
        );
        assert_eq!(
            message.parts[2],
            Part::Other(OtherPart {
                kind: "text".into(),
                raw: serde_json::json!({"type": "text"})
            })
        );
        assert_eq!(message.text(), "a", "only the real text part joins");

        // An explicit empty string is still a valid empty text part: the old
        // extractor kept it, so joining behaves exactly as before.
        let message = decoded_message(serde_json::json!([
            {"type": "text", "text": "a"},
            {"type": "text", "text": ""}
        ]));
        assert_eq!(
            message.parts[1],
            Part::Text(TextPart {
                text: String::new(),
                started_at: None
            })
        );
        assert_eq!(message.text(), "a\n");
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

    /// The adapter-facing entry point parses the raw response array; malformed
    /// input is a decode error, not a silent empty transcript.
    #[test]
    fn decode_response_parses_the_raw_array_and_rejects_garbage() {
        let transcript = decode_response(&serde_json::json!([
            {"info": {"id": "msg_u1", "role": "user", "time": {"created": 1000}},
             "parts": [{"type": "text", "text": "你好"}]}
        ]))
        .expect("a valid payload decodes");
        assert_eq!(transcript.messages.len(), 1);
        assert_eq!(transcript.messages[0].text(), "你好");

        assert!(
            decode_response(&serde_json::json!({"not": "an array"})).is_err(),
            "malformed payloads must surface as errors"
        );
    }
}
