//! Decoder for the current `/api` (V2) route generation.
//!
//! `GET /api/session/{id}/message` answers with a `{data, cursor}` page of
//! projected messages. The generation's field names and its paging contract
//! live in this module alone (spec #339, ADR-0053).
//!
//! API diff against the reference checkout (opencode 1.18.32, commit
//! 16c56fe5ec; shapes confirmed against a live 1.18.23 server):
//!
//! | | legacy (unprefixed) | current (`/api`) |
//! |---|---|---|
//! | read | `GET /session/{id}/message` | `GET /api/session/{id}/message` |
//! | body | `[{info, parts}]` | `{data: [Message], cursor: {previous?, next?}}` |
//! | paging | none: one response carries every message | `limit` 1..=200 (server default 50), `order` asc/desc (default desc), opaque `cursor` that cannot be combined with `order`; follow `cursor.next` |
//! | message | `info.role` user/assistant | `type` user/assistant/system/synthetic/shell/compaction/agent-switched/model-switched |
//! | shell | none — the legacy generation has no shell message | message kind `shell` `{callID, command, output, time.{created,completed?}}`, decoded opaque (no neutral counterpart) |
//! | time | `info.time.{created,completed?}` | `time.{created,completed?}` |
//! | model | `info.modelID` + `info.providerID` | `model.{id,providerID,variant?}` |
//! | tokens | `input`, `output`, `total`, `cache.{read,write}` | `input`, `output`, `reasoning`, `cache.{read,write}` — no `total` |
//! | text | `{type:"text", text, time.start?}` part; a user's text lives in parts | assistant `content[]` `{type:"text", id, text}` (no time); a user's text is the message's `text` field |
//! | reasoning | `{type:"reasoning", text, time.start?}` | `content[]` `{type:"reasoning", id, text, time?}` |
//! | tool | `{type:"tool", tool, callID, state}` | `content[]` `{type:"tool", id, name, provider?, state, time.{created,ran?,completed?}}` |
//! | tool state | `state.{status,input,output,metadata,content,result,error,time}` | `state.status` + tagged bodies; no `output`/`metadata`, the structured result lives in `state.structured` |
//! | tool provider | none | `provider.{executed, metadata, resultMetadata}` — only `resultMetadata` feeds the neutral `metadata` (behind the diff reassembly) |
//! | step start | `{type:"step-start"}` | none — the assistant message is the step |
//! | step finish | `{type:"step-finish", reason}` | assistant `finish` |
//! | patch | `{type:"patch", hash, files}` | assistant `snapshot.{start?,end?,files?}` |
//!
//! Both generations map onto the same neutral views: `finish` becomes the
//! trailing `step-finish` part a Turn's completion is read from,
//! `snapshot.files` a `patch` part, `time.ran` the tool's start, and the
//! edit-family diff is reassembled from `structured.files[].patch` so the
//! untouched Tool Panel keeps rendering it. Message kinds with no neutral
//! counterpart (`system`, `synthetic`, `shell`, `compaction`, switch markers)
//! decode into tolerant roles and raw parts and never join a Turn. Every
//! unknown kind, status or nested shape keeps its raw payload.
//!
//! The `/api` store receives rows from the V2 execution path, so a Session
//! driven through the legacy prompt route has none here. The adapter reads an
//! empty first page as "this generation cannot serve the read" and falls back
//! to the legacy decoder (see `Client::transcript`), which keeps V1 sessions
//! readable while the generations run side by side.

use serde::Deserialize;
use serde_json::Value;

use crate::backend::{
    ContentBlock, MessageId, MessageRole, MessageTime, ModelIdentity, OtherPart, Part, Patch, ReasoningPart,
    StepFinish, TokenUsage, ToolCall, ToolIdentity, ToolOutput, TranscriptMessage,
};
use crate::error::Result;

use super::{
    assemble_tool_output, decode_error, decode_finish_reason, decode_text_part, decode_tool_status, non_null,
    started_at,
};

/// One drained page of the `/api` read: its messages plus the opaque cursor
/// for the next (newer, since pages are read oldest-first) page. The cursor is
/// the generation's wire value; the adapter follows it without interpreting it.
#[derive(Debug)]
pub(crate) struct Page {
    pub(crate) messages: Vec<TranscriptMessage>,
    pub(crate) next: Option<String>,
}

/// The `/api` page envelope.
#[derive(Debug, Deserialize)]
struct WirePage {
    data: Vec<Value>,
    #[serde(default)]
    cursor: Option<WireCursor>,
}

#[derive(Debug, Deserialize)]
struct WireCursor {
    #[serde(default)]
    next: Option<String>,
}

/// Decode one `/api` response page. `data` must be an array — a malformed
/// envelope is an error, not an empty page — while every message inside it
/// decodes tolerantly.
pub(crate) fn decode_page(json: &Value) -> Result<Page> {
    let page = WirePage::deserialize(json)?;
    Ok(Page {
        messages: page.data.iter().map(decode_message).collect(),
        next: page
            .cursor
            .and_then(|cursor| cursor.next)
            .filter(|next| !next.is_empty()),
    })
}

fn decode_message(value: &Value) -> TranscriptMessage {
    let kind = value.get("type").and_then(Value::as_str).unwrap_or_default();
    let id = MessageId::new(value.get("id").and_then(Value::as_str).unwrap_or_default());
    let time = value.get("time").and_then(decode_time);
    match kind {
        "user" => TranscriptMessage {
            id,
            role: MessageRole::User,
            time,
            model: None,
            tokens: None,
            parts: decode_user_parts(value),
        },
        "assistant" => TranscriptMessage {
            id,
            role: MessageRole::Assistant,
            time,
            model: non_null(value.get("model")).map(decode_model),
            tokens: non_null(value.get("tokens")).map(decode_tokens),
            parts: decode_assistant_parts(value),
        },
        // `system` and `synthetic` are server-authored text messages: they
        // carry their text as a field, not as parts. Only `system` has a
        // neutral role; a synthetic injection stays verbatim.
        "system" => TranscriptMessage {
            id,
            role: MessageRole::System,
            time,
            model: None,
            tokens: None,
            parts: decode_message_text(value),
        },
        "synthetic" => TranscriptMessage {
            id,
            role: MessageRole::Other("synthetic".to_string()),
            time,
            model: None,
            tokens: None,
            parts: decode_message_text(value),
        },
        other => TranscriptMessage {
            id,
            role: if other.is_empty() {
                MessageRole::Unknown
            } else {
                MessageRole::Other(other.to_string())
            },
            time,
            model: None,
            tokens: None,
            // A message kind this build does not model (a shell run, a
            // compaction, a switch marker) stays whole and raw: it never
            // renders, and nothing projects it into a Turn.
            parts: vec![Part::Other(OtherPart {
                kind: other.to_string(),
                raw: value.clone(),
            })],
        },
    }
}

fn decode_time(time: &Value) -> Option<MessageTime> {
    Some(MessageTime {
        created: time.get("created").and_then(Value::as_i64)?,
        completed: time.get("completed").and_then(Value::as_i64),
    })
}

/// A message-level `text` field as parts: absent yields no part at all (a
/// files-only `/api` user message has no text to carry), while a present but
/// non-string value stays raw through the shared malformed-text rule.
fn decode_message_text(value: &Value) -> Vec<Part> {
    if value.get("text").is_none() {
        return Vec::new();
    }
    vec![decode_text_part(value, None)]
}

/// A user message's text field plus its file/agent attachments. The text is
/// the generation's field, not a part, so it becomes the text part the
/// neutral model (and the tail projection) reads.
fn decode_user_parts(value: &Value) -> Vec<Part> {
    let mut parts = decode_message_text(value);
    for (field, kind) in [("files", "file"), ("agents", "agent")] {
        if let Some(items) = value.get(field).and_then(Value::as_array) {
            parts.extend(items.iter().map(|item| {
                Part::Other(OtherPart {
                    kind: kind.to_string(),
                    raw: item.clone(),
                })
            }));
        }
    }
    parts
}

/// An assistant message's `content` items, then the message-level `snapshot`
/// and `finish` that the legacy generation carried as parts.
fn decode_assistant_parts(value: &Value) -> Vec<Part> {
    let mut parts: Vec<Part> = value
        .get("content")
        .and_then(Value::as_array)
        .map(|items| items.iter().map(decode_content).collect())
        .unwrap_or_default();
    if let Some(snapshot) = non_null(value.get("snapshot")) {
        let files: Vec<String> = snapshot
            .get("files")
            .and_then(Value::as_array)
            .map(|files| {
                files
                    .iter()
                    .filter_map(Value::as_str)
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default();
        // The legacy patch part appears only when files changed; the hash is
        // the tree the step left behind (`snapshot.end`).
        if !files.is_empty() {
            parts.push(Part::Patch(Patch {
                hash: snapshot
                    .get("end")
                    .or_else(|| snapshot.get("start"))
                    .and_then(Value::as_str)
                    .map(str::to_string),
                files,
            }));
        }
    }
    if let Some(finish) = value.get("finish") {
        parts.push(Part::StepFinish(StepFinish {
            reason: decode_finish_reason(Some(finish)),
        }));
    }
    parts
}

fn decode_content(item: &Value) -> Part {
    match item.get("type").and_then(Value::as_str) {
        // The shared malformed-text rule: a `text` item without string text
        // stays raw instead of manufacturing an empty text.
        Some("text") => decode_text_part(item, None),
        Some("reasoning") => Part::Reasoning(ReasoningPart {
            text: item
                .get("text")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
            started_at: started_at(item, "/time/created"),
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

fn decode_tool(part: &Value) -> ToolCall {
    let name = part
        .get("name")
        .and_then(Value::as_str)
        .unwrap_or("tool")
        .to_string();
    // The correlation id is the content item's own id, never the tool's name:
    // it is what folds a running call's later updates onto the same panel.
    let call_id = part
        .get("id")
        .and_then(Value::as_str)
        .unwrap_or(name.as_str())
        .to_string();
    let state = part.get("state");
    ToolCall {
        identity: ToolIdentity { name, call_id },
        status: decode_tool_status(state.and_then(|state| state.get("status"))),
        // `time.ran` is the `/api` counterpart of the legacy state time
        // `start`: the moment execution began, absent while the call is only
        // preparing its input.
        started_at: started_at(part, "/time/ran"),
        // While `pending` the input is the partially streamed JSON string;
        // running and settled states carry the decoded object.
        input: state.and_then(|state| state.get("input")).cloned(),
        metadata: decode_tool_metadata(part, state),
        output: decode_tool_output(state),
    }
}

/// The `/api` counterpart of the legacy tool `metadata`: the server-native
/// record for provider-executed tools, else the edit-family diff reassembled
/// from `structured.files[].patch` — the one metadata field the Tool Panel
/// reads, so the edit/apply_patch presentation survives the generation swap.
fn decode_tool_metadata(part: &Value, state: Option<&Value>) -> Option<Value> {
    if let Some(metadata) = non_null(part.pointer("/provider/resultMetadata")) {
        return Some(metadata.clone());
    }
    let files = non_null(state.and_then(|state| state.get("structured")))
        .and_then(|structured| structured.get("files"))
        .and_then(Value::as_array)?;
    let patches: Vec<&str> = files
        .iter()
        .filter_map(|file| file.get("patch").and_then(Value::as_str))
        .collect();
    if patches.is_empty() {
        return None;
    }
    Some(serde_json::json!({ "diff": patches.join("\n") }))
}

/// Decode a tool state's output side. The text sources follow the same
/// precedence the legacy decoder applies to its own: the shared
/// [`assemble_tool_output`] joins the `content` text runs and a string
/// `result`; a non-text block stays raw, and a failure keeps its reason apart
/// as [`ToolOutput::error`].
fn decode_tool_output(state: Option<&Value>) -> ToolOutput {
    let Some(state) = state else {
        return ToolOutput::default();
    };
    let (text, raw_blocks) = assemble_tool_output(state.get("content"), state.get("result"));
    let mut blocks = Vec::new();
    if !text.is_empty() {
        blocks.push(ContentBlock::Text(text));
    }
    blocks.extend(raw_blocks);
    ToolOutput {
        // The first output-bearing field the payload actually carries: the
        // content blocks, else the result value, else the structured result
        // (a tool like `read` keeps its whole output there). An empty array
        // or object is the server saying "no output", not an output.
        raw: non_null(state.get("content"))
            .filter(|value| has_payload(value))
            .or_else(|| non_null(state.get("result")))
            .or_else(|| non_null(state.get("structured")).filter(|value| has_payload(value)))
            .cloned(),
        blocks,
        error: state.get("error").and_then(decode_error),
    }
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

fn decode_model(model: &Value) -> ModelIdentity {
    ModelIdentity {
        provider_id: model
            .get("providerID")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        model_id: model
            .get("id")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        variant: model.get("variant").and_then(Value::as_str).map(str::to_string),
    }
}

fn decode_tokens(tokens: &Value) -> TokenUsage {
    TokenUsage {
        input: tokens.get("input").and_then(Value::as_i64).unwrap_or(0),
        output: tokens.get("output").and_then(Value::as_i64).unwrap_or(0),
        // The `/api` envelope reports no running total; `context_used` falls
        // back to `input + cache.read`, the same fallback a legacy payload
        // whose server omitted `total` gets.
        total: 0,
        cache_read: tokens.pointer("/cache/read").and_then(Value::as_i64).unwrap_or(0),
        cache_write: tokens
            .pointer("/cache/write")
            .and_then(Value::as_i64)
            .unwrap_or(0),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::{FinishReason, TextPart, ToolStatus};

    fn page(json: Value) -> Page {
        decode_page(&json).expect("a valid page decodes")
    }

    fn only_message(json: Value) -> TranscriptMessage {
        let Page { messages, .. } = page(json);
        messages.into_iter().next().expect("one message")
    }

    /// The envelope's `data` array decodes and `cursor.next` is the opaque
    /// follow-up cursor; a null or absent cursor is no next page.
    #[test]
    fn decode_page_reads_the_data_envelope_and_the_next_cursor() {
        let decoded = page(serde_json::json!({
            "data": [
                {"type": "user", "id": "msg_u1", "time": {"created": 1000}, "text": "问题"},
                {"type": "assistant", "id": "msg_a1", "time": {"created": 1100}, "content": []}
            ],
            "cursor": {"previous": "p", "next": "n"}
        }));
        let ids: Vec<_> = decoded.messages.iter().map(|m| m.id.as_str()).collect();
        assert_eq!(ids, vec!["msg_u1", "msg_a1"]);
        assert_eq!(decoded.next.as_deref(), Some("n"));

        let decoded = page(serde_json::json!({
            "data": [],
            "cursor": {"previous": null, "next": null}
        }));
        assert!(decoded.messages.is_empty());
        assert!(decoded.next.is_none(), "an explicit null cursor is no next page");

        let decoded = page(serde_json::json!({"data": []}));
        assert!(decoded.next.is_none(), "a missing cursor is no next page");
    }

    /// A malformed envelope is a decode error, not a silent empty page.
    #[test]
    fn decode_page_rejects_malformed_envelopes() {
        assert!(decode_page(&serde_json::json!({"data": "nope"})).is_err());
        assert!(decode_page(&serde_json::json!({})).is_err());
    }

    /// A user message's text field becomes one text part; attachments stay
    /// raw, in field order after it.
    #[test]
    fn a_user_message_becomes_a_text_part_and_raw_attachments() {
        let message = only_message(serde_json::json!({
            "data": [{
                "type": "user",
                "id": "msg_u1",
                "time": {"created": 1000},
                "text": "看这个",
                "files": [{"uri": "file:///a.png", "mime": "image/png"}],
                "agents": [{"name": "explore"}]
            }]
        }));
        assert_eq!(message.role, MessageRole::User);
        assert_eq!(message.time.unwrap().created, 1000);
        assert_eq!(
            message.parts[0],
            Part::Text(TextPart {
                text: "看这个".into(),
                started_at: None
            })
        );
        assert_eq!(
            message.parts[1],
            Part::Other(OtherPart {
                kind: "file".into(),
                raw: serde_json::json!({"uri": "file:///a.png", "mime": "image/png"})
            })
        );
        assert_eq!(
            message.parts[2],
            Part::Other(OtherPart {
                kind: "agent".into(),
                raw: serde_json::json!({"name": "explore"})
            })
        );
        assert_eq!(message.text(), "看这个");
    }

    /// A user message with no `text` field gains NO text part — a files-only
    /// message must not widen into a whole-message raw part — while a
    /// present but non-string text still stays raw (the malformed-value rule,
    /// pinned for a message-level field in the tolerance test below).
    #[test]
    fn a_files_only_user_message_gains_no_text_part() {
        let message = only_message(serde_json::json!({
            "data": [{
                "type": "user",
                "id": "msg_u1",
                "time": {"created": 1000},
                "files": [{"uri": "file:///a.png", "mime": "image/png"}]
            }]
        }));
        assert_eq!(message.role, MessageRole::User);
        assert_eq!(
            message.parts,
            vec![Part::Other(OtherPart {
                kind: "file".into(),
                raw: serde_json::json!({"uri": "file:///a.png", "mime": "image/png"})
            })]
        );
        assert_eq!(message.text(), "");
    }

    /// Assistant content maps onto the typed parts: text (no time), reasoning
    /// (its created time), and a tool whose identity/status/input/start come
    /// from the content item and its state.
    #[test]
    fn assistant_content_maps_text_reasoning_and_tool() {
        let message = only_message(serde_json::json!({
            "data": [{
                "type": "assistant",
                "id": "msg_a1",
                "time": {"created": 1100, "completed": 1400},
                "model": {"id": "deepseek-v4-flash", "providerID": "opencode-go"},
                "tokens": {"input": 220, "output": 129, "reasoning": 12,
                           "cache": {"read": 161664, "write": 0}},
                "content": [
                    {"type": "reasoning", "id": "prt_r", "text": "想一下",
                     "time": {"created": 1110, "completed": 1120}},
                    {"type": "text", "id": "prt_t", "text": "答案一"},
                    {"type": "tool", "id": "call_1", "name": "bash",
                     "state": {"status": "completed", "input": {"command": "ls"},
                               "content": [{"type": "text", "text": "src"}],
                               "structured": {}},
                     "time": {"created": 1120, "ran": 1130, "completed": 1140}}
                ]
            }]
        }));

        assert_eq!(message.role, MessageRole::Assistant);
        assert_eq!(
            message.model.as_ref().unwrap(),
            &ModelIdentity {
                provider_id: "opencode-go".into(),
                model_id: "deepseek-v4-flash".into(),
                variant: None
            }
        );
        // No `total` on the `/api` envelope: context_used falls back to
        // input + cache.read.
        let tokens = message.tokens.unwrap();
        assert_eq!(tokens.total, 0);
        assert_eq!(tokens.context_used(), 220 + 161_664);

        assert_eq!(
            message.parts[0],
            Part::Reasoning(ReasoningPart {
                text: "想一下".into(),
                started_at: Some(1110)
            })
        );
        assert_eq!(
            message.parts[1],
            Part::Text(TextPart {
                text: "答案一".into(),
                started_at: None
            })
        );
        let Part::Tool(call) = &message.parts[2] else {
            panic!("expected a tool part: {:?}", message.parts[2]);
        };
        assert_eq!(call.identity.name, "bash");
        assert_eq!(call.identity.call_id, "call_1");
        assert_eq!(call.status, ToolStatus::Completed);
        assert_eq!(call.started_at, Some(1130));
        assert_eq!(call.input.as_ref().unwrap()["command"], "ls");
        assert_eq!(call.output.blocks, vec![ContentBlock::Text("src".into())]);
        assert_eq!(
            call.output.raw,
            Some(serde_json::json!([{"type": "text", "text": "src"}]))
        );
        assert!(call.output.error.is_none());
    }

    /// The message-level `finish` becomes the trailing step-finish part the
    /// Turn completion reads, and `snapshot.files` becomes the patch part.
    #[test]
    fn finish_becomes_a_step_finish_and_snapshot_a_patch() {
        let message = only_message(serde_json::json!({
            "data": [{
                "type": "assistant",
                "id": "msg_a1",
                "time": {"created": 1100, "completed": 1400},
                "content": [],
                "snapshot": {"start": "before", "end": "after", "files": ["src/a.rs", "src/b.rs"]},
                "finish": "tool-calls"
            }]
        }));
        assert_eq!(
            message.parts,
            vec![
                Part::Patch(Patch {
                    hash: Some("after".into()),
                    files: vec!["src/a.rs".into(), "src/b.rs".into()]
                }),
                Part::StepFinish(StepFinish {
                    reason: FinishReason::ToolCalls
                }),
            ]
        );
        assert!(
            matches!(&message.parts[1], Part::StepFinish(finish) if !finish.reason.is_terminal()),
            "tool-calls is not terminal"
        );

        // No snapshot → no patch part; a terminal finish still lands.
        let message = only_message(serde_json::json!({
            "data": [{
                "type": "assistant",
                "id": "msg_a2",
                "time": {"created": 1100, "completed": 1200},
                "content": [],
                "finish": "stop"
            }]
        }));
        assert_eq!(
            message.parts,
            vec![Part::StepFinish(StepFinish {
                reason: FinishReason::Stop
            })]
        );
    }

    /// The edit-family diff is reassembled from `structured.files[].patch`
    /// (the one metadata field the Tool Panel reads); a provider-native
    /// metadata record wins when the payload carries one.
    #[test]
    fn tool_metadata_assembles_the_edit_diff() {
        let message = only_message(serde_json::json!({
            "data": [{
                "type": "assistant",
                "id": "msg_a1",
                "time": {"created": 1100},
                "content": [{
                    "type": "tool", "id": "call_1", "name": "edit",
                    "state": {"status": "completed", "input": {"filePath": "a.rs"},
                              "content": [{"type": "text", "text": "Edit applied successfully."}],
                              "structured": {"files": [
                                  {"file": "a.rs", "patch": "@@ -1 +1 @@"},
                                  {"file": "b.rs", "patch": "@@ -2 +2 @@"}
                              ]}}
                }]
            }]
        }));
        let Part::Tool(call) = &message.parts[0] else {
            panic!("expected a tool part: {:?}", message.parts[0]);
        };
        assert_eq!(
            call.metadata,
            Some(serde_json::json!({"diff": "@@ -1 +1 @@\n@@ -2 +2 @@"}))
        );

        let message = only_message(serde_json::json!({
            "data": [{
                "type": "assistant",
                "id": "msg_a1",
                "time": {"created": 1100},
                "content": [{
                    "type": "tool", "id": "call_1", "name": "websearch",
                    "provider": {"executed": true, "resultMetadata": {"source": "search"}},
                    "state": {"status": "completed", "input": {}, "content": [], "structured": {}}
                }]
            }]
        }));
        let Part::Tool(call) = &message.parts[0] else {
            panic!("expected a tool part: {:?}", message.parts[0]);
        };
        assert_eq!(call.metadata, Some(serde_json::json!({"source": "search"})));

        // No files, no native metadata: the field stays absent rather than
        // manufacturing an empty diff.
        let message = only_message(serde_json::json!({
            "data": [{
                "type": "assistant",
                "id": "msg_a1",
                "time": {"created": 1100},
                "content": [{
                    "type": "tool", "id": "call_1", "name": "bash",
                    "state": {"status": "completed", "input": {"command": "ls"},
                              "content": [{"type": "text", "text": "src"}], "structured": {}}
                }]
            }]
        }));
        let Part::Tool(call) = &message.parts[0] else {
            panic!("expected a tool part: {:?}", message.parts[0]);
        };
        assert!(call.metadata.is_none());
    }

    /// A tool's output side: content texts join, a string `result` follows on
    /// a new line, non-text content stays raw, and a failure keeps its
    /// message.
    #[test]
    fn tool_output_joins_content_text_and_result() {
        let message = only_message(serde_json::json!({
            "data": [{
                "type": "assistant",
                "id": "msg_a1",
                "time": {"created": 1100},
                "content": [{
                    "type": "tool", "id": "call_1", "name": "read",
                    "state": {
                        "status": "completed", "input": {"filePath": "a.rs"},
                        "content": [
                            {"type": "text", "text": "line1"},
                            {"type": "file", "uri": "file:///a"},
                            {"type": "text", "text": "line2"}
                        ],
                        "result": "2 lines",
                        "structured": {}
                    }
                }]
            }]
        }));
        let Part::Tool(call) = &message.parts[0] else {
            panic!("expected a tool part: {:?}", message.parts[0]);
        };
        assert_eq!(
            call.output.blocks,
            vec![
                ContentBlock::Text("line1line2\n2 lines".into()),
                ContentBlock::Other(serde_json::json!({"type": "file", "uri": "file:///a"})),
            ]
        );

        let message = only_message(serde_json::json!({
            "data": [{
                "type": "assistant",
                "id": "msg_a1",
                "time": {"created": 1100},
                "content": [{
                    "type": "tool", "id": "call_1", "name": "bash",
                    "state": {"status": "error", "input": {"command": "false"},
                              "content": [], "structured": {},
                              "error": {"type": "unknown", "message": "boom"}}
                }]
            }]
        }));
        let Part::Tool(call) = &message.parts[0] else {
            panic!("expected a tool part: {:?}", message.parts[0]);
        };
        assert_eq!(call.output.error.as_deref(), Some("boom"));
        assert!(call.output.blocks.is_empty());

        // A tool whose output is structured only (a `read` of a directory)
        // keeps the payload even though it renders no text blocks, and an
        // empty content array does not shadow it.
        let message = only_message(serde_json::json!({
            "data": [{
                "type": "assistant",
                "id": "msg_a1",
                "time": {"created": 1100},
                "content": [{
                    "type": "tool", "id": "call_1", "name": "read",
                    "state": {"status": "completed", "input": {"path": "."},
                              "content": [],
                              "structured": {"entries": [{"path": "a.rs", "type": "file"}]}}
                }]
            }]
        }));
        let Part::Tool(call) = &message.parts[0] else {
            panic!("expected a tool part: {:?}", message.parts[0]);
        };
        assert!(call.output.blocks.is_empty());
        assert_eq!(
            call.output.raw,
            Some(serde_json::json!({"entries": [{"path": "a.rs", "type": "file"}]}))
        );
    }

    /// Unknown message kinds, content kinds, statuses, finish reasons and
    /// malformed text keep their raw payload and never fail the page.
    #[test]
    fn unknown_shapes_decode_tolerantly() {
        let Page { messages, .. } = page(serde_json::json!({
            "data": [
                {"type": "compaction", "id": "msg_c1", "time": {"created": 1000},
                 "reason": "auto", "summary": "s", "recent": "r"},
                {"type": "mystery-message", "id": "msg_m1", "time": {"created": 1100}, "payload": 42},
                {"type": "assistant", "id": "msg_a1", "time": {"created": 1200},
                 "content": [
                     {"type": "mystery-part", "payload": 7},
                     {"type": "text", "id": "prt_bad"},
                     {"type": "tool", "id": "call_1", "name": "mystery",
                      "state": {"status": "weird", "input": {"x": 1}}}
                 ],
                 "finish": "new-reason"},
                {"type": "system", "id": "msg_s1", "time": {"created": 1300}, "text": 42}
            ]
        }));

        assert_eq!(messages[0].role, MessageRole::Other("compaction".into()));
        assert_eq!(
            messages[0].parts,
            vec![Part::Other(OtherPart {
                kind: "compaction".into(),
                raw: serde_json::json!({"type": "compaction", "id": "msg_c1", "time": {"created": 1000},
                                        "reason": "auto", "summary": "s", "recent": "r"})
            })]
        );
        assert_eq!(messages[1].role, MessageRole::Other("mystery-message".into()));

        let tool = &messages[2];
        assert_eq!(
            tool.parts[0],
            Part::Other(OtherPart {
                kind: "mystery-part".into(),
                raw: serde_json::json!({"type": "mystery-part", "payload": 7})
            })
        );
        assert_eq!(
            tool.parts[1],
            Part::Other(OtherPart {
                kind: "text".into(),
                raw: serde_json::json!({"type": "text", "id": "prt_bad"})
            })
        );
        let Part::Tool(call) = &tool.parts[2] else {
            panic!("expected a tool part: {:?}", tool.parts[2]);
        };
        assert_eq!(call.status, ToolStatus::Other("weird".into()));
        assert!(call.started_at.is_none());
        assert_eq!(call.input.as_ref().unwrap()["x"], 1);
        assert_eq!(
            tool.parts[3],
            Part::StepFinish(StepFinish {
                reason: FinishReason::Other("new-reason".into())
            })
        );

        // A message-level text field that is not a string keeps the WHOLE
        // message raw (the shared rule's canonical raw).
        let system = &messages[3];
        assert_eq!(system.role, MessageRole::System);
        assert_eq!(
            system.parts[0],
            Part::Other(OtherPart {
                kind: "text".into(),
                raw: serde_json::json!({"type": "system", "id": "msg_s1", "time": {"created": 1300}, "text": 42})
            })
        );
    }

    /// A pending tool's partially streamed input stays the raw string; a
    /// message with no type at all still decodes into the Unknown role.
    #[test]
    fn pending_input_stays_raw_and_a_typeless_message_is_unknown() {
        let message = only_message(serde_json::json!({
            "data": [{
                "type": "assistant",
                "id": "msg_a1",
                "time": {"created": 1100},
                "content": [{
                    "type": "tool", "id": "call_1", "name": "bash",
                    "state": {"status": "pending", "input": "{\"command\":"},
                    "time": {"created": 1105}
                }]
            }]
        }));
        let Part::Tool(call) = &message.parts[0] else {
            panic!("expected a tool part: {:?}", message.parts[0]);
        };
        assert_eq!(call.status, ToolStatus::Pending);
        assert!(call.started_at.is_none(), "a pending call has not run yet");
        assert_eq!(call.input, Some(Value::String("{\"command\":".into())));

        let message = only_message(serde_json::json!({"data": [{"id": "msg_x", "time": {"created": 1}}]}));
        assert_eq!(message.role, MessageRole::Unknown);
    }
}
