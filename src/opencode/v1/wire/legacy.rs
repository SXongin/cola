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
//!
//! One event this generation does NOT put on a message of its own is the
//! **Compaction** (ADR-0082): V1 states it as a `compaction` part on a user
//! message plus a paired assistant summary message, and [`decode`] pairs the
//! two into the one generation-neutral fact before any message is classified.

use serde::Deserialize;
use serde_json::Value;

use crate::backend::{
    Compaction, CompactionReason, CompactionStatus, ContentBlock, FileContent, MessageId, MessageRole,
    MessageTime, ModelIdentity, OtherPart, Part, Patch, ReasoningPart, SessionTranscript, StepFinish,
    StepStart, TokenUsage, ToolCall, ToolOutput, ToolStatus, TranscriptMessage,
};
use crate::error::Result;

use super::{
    assemble_tool_output, decode_error, decode_finish_reason, decode_text_part, decode_tool_identity,
    decode_tool_status, error_suppresses_fallback, has_payload, non_null, started_at, string_list,
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
    /// The message an assistant message was produced from (V1's `parentID`).
    /// The compaction pairing links a summary message to its carrier through it.
    #[serde(rename = "parentID", default)]
    parent_id: Option<String>,
    /// The mode an assistant message was produced in; a compaction summary
    /// carries `"compaction"`.
    #[serde(default)]
    mode: Option<String>,
    /// Whether an assistant message is a summary; a compaction summary carries
    /// `true`. Kept raw because V1's USER messages spell `summary` as an object
    /// (`{additions, deletions, files, diffs}`, the user-message format
    /// summary); a bool field would fail the whole read on one.
    #[serde(default)]
    summary: Option<Value>,
    #[serde(rename = "modelID", default)]
    model_id: Option<String>,
    #[serde(rename = "providerID", default)]
    provider_id: Option<String>,
    #[serde(default)]
    tokens: Option<WireMessageTokens>,
    /// The assistant failure the message records (`AssistantError` on the V1
    /// schema): a wrapped `{name, data:{message}}`, a plain `{message}`, or a
    /// string. Kept raw here; [`decode_message_error`] normalizes it.
    #[serde(default)]
    error: Option<Value>,
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

/// The role a paired compaction message carries (`MessageRole::Other`), the
/// verbatim kind exactly as `synthetic` and `skill` keep theirs — so no
/// language-model role gains a member.
const COMPACTION_ROLE: &str = "compaction";

/// Which message states which part of one **Compaction** (ADR-0082). V1 spells
/// a single compaction across TWO messages, so a message's neutral role and
/// parts can only be decoded once its sibling is known.
enum CompactionRole {
    /// The user message carrying a `compaction` part — the event's **carrier**.
    /// It holds the typed fact; `summary` is the paired message's text.
    Carrier {
        reason: CompactionReason,
        summary: String,
    },
    /// The assistant summary message paired with a carrier. It is the
    /// compaction's own text, so it is never an Assistant message: it answers
    /// no Turn, and its tokens never become the Turn Footer's context figure.
    Summary,
    /// A summary message whose carrier this read does not carry. It is still a
    /// compaction's summary, not an answer, so it degrades to a completed,
    /// summary-less fact rather than rendering as conversation.
    OrphanedSummary,
}

/// Decode every message of one session, preserving the server's order.
///
/// The compaction pairing runs over the WHOLE read first: V1 states one
/// **Compaction** as a `compaction` part on a user message plus a paired
/// assistant summary message, and neither message can be classified without
/// the other.
fn decode(messages: &[WireSessionMessage]) -> SessionTranscript {
    let roles = compaction_roles(messages);
    SessionTranscript::new(
        messages
            .iter()
            .zip(roles)
            .map(|(message, role)| decode_message(message, role))
            .collect(),
    )
}

/// A completed compaction fact as V1 can state it. V1 has no `status` field and
/// no failed event (ADR-0082), so every compaction it states is completed; it
/// carries no usage on the compaction, and its own `tail_start_id` is not a
/// decoded fact (`recent` stays V2-only).
fn completed_fact(reason: CompactionReason, summary: String) -> Compaction {
    Compaction {
        status: CompactionStatus::Completed,
        reason,
        summary,
        usage: None,
        error: None,
        recent: String::new(),
    }
}

/// Pair every V1 **Compaction** this read spells out (ADR-0082), in message
/// order: one entry per message, `Some` when it is part of a pair.
///
/// The partner of a carrier is the assistant message that names it as its
/// `parentID` and states itself as a summary — V1 writes both `summary:true`
/// and `mode:"compaction"`, and either signal marks it. Neither orphan
/// vanishes: a carrier the read cannot pair (the summarizer never wrote its
/// summary, or the read is a page that lost it) keeps a summary-less fact, and
/// so does a summary whose carrier the read lost.
fn compaction_roles(messages: &[WireSessionMessage]) -> Vec<Option<CompactionRole>> {
    let mut roles: Vec<Option<CompactionRole>> = Vec::with_capacity(messages.len());
    roles.resize_with(messages.len(), || None);
    let mut paired = vec![false; messages.len()];
    for (carrier_index, carrier) in messages.iter().enumerate() {
        if carrier.info.role.as_deref() != Some("user") {
            continue;
        }
        let Some(part) = compaction_part(&carrier.parts) else {
            continue;
        };
        let paired_index = messages.iter().enumerate().position(|(index, message)| {
            !paired[index]
                && is_summary_message(message)
                && message.info.parent_id.as_deref() == Some(carrier.info.id.as_str())
        });
        let summary = match paired_index {
            Some(index) => {
                paired[index] = true;
                summary_text(&messages[index].parts)
            }
            None => String::new(),
        };
        roles[carrier_index] = Some(CompactionRole::Carrier {
            reason: decode_compaction_reason(part),
            summary,
        });
    }
    for (index, message) in messages.iter().enumerate() {
        if paired[index] {
            roles[index] = Some(CompactionRole::Summary);
        } else if roles[index].is_none() && is_summary_message(message) {
            roles[index] = Some(CompactionRole::OrphanedSummary);
        }
    }
    roles
}

/// The `compaction` part of a message's raw `parts`, when it carries one: the
/// one part kind that is not a body but the statement of a Session-scoped
/// event.
fn compaction_part(parts: &Value) -> Option<&Value> {
    parts
        .as_array()?
        .iter()
        .find(|part| part.get("type").and_then(Value::as_str) == Some("compaction"))
}

/// Whether an assistant message states itself as a compaction summary: V1
/// writes `summary:true` and `mode:"compaction"` together, and either one names
/// it (a tolerant read of a server that writes only one of the two).
fn is_summary_message(message: &WireSessionMessage) -> bool {
    message.info.role.as_deref() == Some("assistant")
        && (message.info.summary.as_ref() == Some(&Value::Bool(true))
            || message.info.mode.as_deref() == Some(COMPACTION_ROLE))
}

/// Why a V1 `compaction` part ran: automatic when the server set `auto` or
/// triggered it on an overflow, manual when the user asked. V2 states an
/// overflow as `reason:"auto"`, so the two generations agree.
fn decode_compaction_reason(part: &Value) -> CompactionReason {
    let flag = |field: &str| part.get(field).and_then(Value::as_bool).unwrap_or(false);
    if flag("auto") || flag("overflow") {
        CompactionReason::Auto
    } else {
        CompactionReason::Manual
    }
}

/// The text a compaction summary message carries: its non-empty `text` parts in
/// order, joined as paragraphs. The join matches V1's own summary composition
/// (and the migration that replaced it), so a multi-part summary reads the way
/// the server wrote it; an empty part contributes nothing.
fn summary_text(parts: &Value) -> String {
    parts
        .as_array()
        .map(|parts| {
            parts
                .iter()
                .filter_map(|part| match part.get("type").and_then(Value::as_str) {
                    Some("text") => part.get("text").and_then(Value::as_str),
                    _ => None,
                })
                .filter(|text| !text.is_empty())
                .collect::<Vec<_>>()
                .join("\n\n")
        })
        .unwrap_or_default()
}

/// Decode one raw `parts` array through the per-part decoder.
pub(super) fn decode_parts(parts: &Value) -> Vec<Part> {
    parts
        .as_array()
        .map(|parts| parts.iter().map(decode_part).collect())
        .unwrap_or_default()
}

fn decode_message(message: &WireSessionMessage, compaction: Option<CompactionRole>) -> TranscriptMessage {
    let info = &message.info;
    let (role, parts) = match compaction {
        Some(CompactionRole::Carrier { reason, summary }) => (
            MessageRole::Other(COMPACTION_ROLE.to_string()),
            vec![Part::Compaction(completed_fact(reason, summary))],
        ),
        // The summary message's body IS the summary, and it now rides the
        // carrier's fact: the message keeps its identity, time and failure, but
        // its text is not decoded a second time (the summary renders once).
        Some(CompactionRole::Summary) => (MessageRole::Other(COMPACTION_ROLE.to_string()), Vec::new()),
        // A summary whose carrier the read lost is still a compaction, never an
        // answer: a completed fact with no summary to attach.
        Some(CompactionRole::OrphanedSummary) => (
            MessageRole::Other(COMPACTION_ROLE.to_string()),
            vec![Part::Compaction(completed_fact(
                CompactionReason::Auto,
                String::new(),
            ))],
        ),
        None => (decode_role(info.role.as_deref()), decode_parts(&message.parts)),
    };
    TranscriptMessage {
        id: MessageId::new(info.id.clone()),
        role,
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
        error: info.error.as_ref().and_then(decode_message_error),
        parts,
    }
}

/// Normalize an assistant message's recorded failure to its message text. V1's
/// `AssistantError` union serializes as `{name, data:{message, …}}` for most
/// arms (`UnknownError`, `APIError`, …); a few shapes carry `message` at the
/// top level. The wrapped form wins when both exist — the same precedence the
/// retired blocking-prompt decoder used, so the error text cannot drift.
fn decode_message_error(error: &Value) -> Option<String> {
    error
        .pointer("/data/message")
        .and_then(Value::as_str)
        .map(str::to_string)
        .or_else(|| decode_error(error))
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
            files: string_list(part.get("files")),
        }),
        Some("file") => match decode_file_part(part) {
            Some(content) => Part::File(content),
            None => Part::Other(OtherPart {
                kind: "file".to_string(),
                raw: part.clone(),
            }),
        },
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

/// A V1 `file` part: its `url` is the payload URI (cola attaches images as
/// inline `data:` URIs) and its `filename` the display name. A part that only
/// references a file inlines no payload, so it is not a File Content and stays
/// raw (the caller's fallthrough).
fn decode_file_part(part: &Value) -> Option<FileContent> {
    FileContent::decode(
        part.get("url").and_then(Value::as_str)?,
        part.get("mime").and_then(Value::as_str),
        part.get("filename").and_then(Value::as_str),
    )
}

fn decode_tool(part: &Value) -> ToolCall {
    // The correlation id is the call's, never the tool's: it is what folds a
    // running call's later updates onto the same panel.
    let identity = decode_tool_identity(part.get("tool"), part.get("callID"));
    let state = part.get("state");
    let status = decode_tool_status(state.and_then(|state| state.get("status")));
    ToolCall {
        identity,
        status: status.clone(),
        started_at: state.and_then(|state| started_at(state, "/time/start")),
        input: state.and_then(|state| state.get("input")).cloned(),
        metadata: state.and_then(|state| state.get("metadata")).cloned(),
        output: decode_tool_output(state, &status),
    }
}

/// Decode a tool state's output side. Text sources follow the precedence the
/// presentation has always rendered: a string `output` is authoritative (the
/// other sources are never appended to it); otherwise the shared
/// [`assemble_tool_output`] joins the `content` text runs and a string
/// `result`; otherwise `metadata.output` is the last resort — unless a decoded
/// failure message suppresses it, so the panel's appended `❌ …` reproduces the
/// historical extractor. A non-string `output` — and a null or absent one —
/// falls through to those sources. Non-text blocks (and a text block that lost
/// its text) stay raw instead of vanishing; the raw payload is preserved so
/// per-tool presentation stays in the Platform (ADR-0042). A failure's reason
/// lives apart from the output as [`ToolOutput::error`].
fn decode_tool_output(state: Option<&Value>, status: &ToolStatus) -> ToolOutput {
    let Some(state) = state else {
        return ToolOutput::default();
    };
    let mut blocks = Vec::new();
    let error = state.get("error").and_then(decode_error);
    match non_null(state.get("output")).and_then(Value::as_str) {
        Some(output) => blocks.push(ContentBlock::Text(output.to_string())),
        None => {
            let (text, raw_blocks) = assemble_tool_output(state.get("content"), state.get("result"));
            // `metadata.output` is the historical last resort and never
            // overrides real text. A present string is the text even when it
            // is empty, so an empty `metadata.output` still yields a text
            // block (the renderer returns `Some("")` there). A decoded failure
            // message suppresses the fallback: the panel appends `❌ …`, which
            // the historical extractor counted as output.
            if !text.is_empty() {
                blocks.push(ContentBlock::Text(text));
            } else if !error_suppresses_fallback(status, error.as_deref())
                && let Some(fallback) = state.pointer("/metadata/output").and_then(Value::as_str)
            {
                blocks.push(ContentBlock::Text(fallback.to_string()));
            }
            blocks.extend(raw_blocks);
        }
    }
    ToolOutput {
        // The first output-bearing field the payload actually carries; an
        // empty container means the server carried no output.
        raw: non_null(state.get("output"))
            .filter(|value| has_payload(value))
            .or_else(|| non_null(state.get("content")).filter(|value| has_payload(value)))
            .or_else(|| non_null(state.get("result")))
            .or_else(|| non_null(state.pointer("/metadata/output")).filter(|value| has_payload(value)))
            .cloned(),
        blocks,
        error,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::{CompactionReason, CompactionStatus, TextPart, ToolStatus};

    /// One raw V1 message envelope — id, role, a server time and raw parts:
    /// the shape every decoder test drives. Its compaction-pairing fields
    /// (`parentID`, `mode`, `summary`) are left unset; the compaction tests
    /// below spell them on the wire instead.
    fn envelope(id: &str, role: &str, created: i64, parts: Value) -> WireSessionMessage {
        WireSessionMessage {
            info: WireMessageInfo {
                id: id.into(),
                role: Some(role.into()),
                time: Some(WireMessageTime {
                    created,
                    completed: Some(created),
                }),
                parent_id: None,
                mode: None,
                summary: None,
                model_id: None,
                provider_id: None,
                tokens: None,
                error: None,
            },
            parts,
        }
    }

    /// Decode one tool part's state into its typed call.
    fn tool(state: Value) -> ToolCall {
        let message = envelope(
            "msg_a1",
            "assistant",
            1_000,
            serde_json::json!([
                {"type": "tool", "tool": "bash", "callID": "call_1", "state": state}
            ]),
        );
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

    /// A decoded failure message suppresses the `metadata.output` last resort:
    /// the historical extractor appended its `❌ …` line before consulting the
    /// fallback and counted that line as output, so an errored panel renders
    /// only the message. A failure whose payload has no decodable message
    /// still falls back.
    #[test]
    fn a_decoded_error_message_suppresses_the_metadata_output_fallback() {
        let call = tool(serde_json::json!({
            "status": "error",
            "error": "boom",
            "metadata": {"output": "from metadata"}
        }));
        assert!(
            call.output.blocks.is_empty(),
            "no metadata text block may render beside the error: {:?}",
            call.output.blocks
        );
        assert_eq!(call.output.error.as_deref(), Some("boom"));
        // The raw payload still records the first output-bearing field.
        assert_eq!(call.output.raw, Some(Value::String("from metadata".into())));

        let call = tool(serde_json::json!({
            "status": "error",
            "error": {"type": "unknown"},
            "metadata": {"output": "from metadata"}
        }));
        assert_eq!(
            call.output.blocks,
            vec![ContentBlock::Text("from metadata".into())]
        );
        assert!(call.output.error.is_none());
    }

    /// The shared payload-existence policy: an empty `content` array is the
    /// server saying "no output", so it neither becomes the raw payload nor
    /// masks a later source.
    #[test]
    fn an_empty_content_array_is_not_an_output_payload() {
        let call = tool(serde_json::json!({
            "status": "completed",
            "content": [],
            "result": "done"
        }));
        assert_eq!(call.output.raw, Some(Value::String("done".into())));
        assert_eq!(call.output.blocks, vec![ContentBlock::Text("done".into())]);

        let call = tool(serde_json::json!({"status": "completed", "content": []}));
        assert!(call.output.raw.is_none());
        assert!(call.output.blocks.is_empty());
    }

    /// A tool part that lost its name or correlation id keeps the historical
    /// fallbacks: the name reads `"tool"` and the id falls back to it, so a
    /// malformed call still folds onto one panel.
    #[test]
    fn a_tool_without_name_or_call_id_keeps_the_historical_fallbacks() {
        let message = decoded_message(serde_json::json!([
            {"type": "tool", "state": {"status": "completed"}}
        ]));
        let Part::Tool(call) = &message.parts[0] else {
            panic!("expected a tool part: {:?}", message.parts[0]);
        };
        assert_eq!(call.identity.name, "tool");
        assert_eq!(call.identity.call_id, "tool");
    }

    /// Any content item with a string `text` contributes — the kind is not
    /// checked (the renderer reads `text` off any item); an item without one
    /// stays raw unless it is an inline `file` block (below).
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

    /// A tool `file` content block that inlines its payload decodes into the
    /// neutral File Content view on V1 exactly as on V2 — the image/PDF `read`
    /// returns — while the output payload is preserved verbatim beside it.
    #[test]
    fn an_inline_tool_file_block_decodes_into_the_file_content_view() {
        let file = serde_json::json!({
            "type": "file",
            "uri": "data:image/png;base64,QUJD",
            "mime": "image/png",
            "name": "shot.png"
        });
        let call = tool(serde_json::json!({
            "status": "completed",
            "content": [
                {"type": "text", "text": "Image read successfully"},
                file.clone()
            ]
        }));

        assert_eq!(
            call.output.blocks,
            vec![
                ContentBlock::Text("Image read successfully".into()),
                ContentBlock::File(FileContent {
                    uri: "data:image/png;base64,QUJD".into(),
                    mime: "image/png".into(),
                    name: "shot.png".into(),
                    size: 3,
                }),
            ]
        );
        assert_eq!(
            call.output.raw,
            Some(serde_json::json!([
                {"type": "text", "text": "Image read successfully"},
                file
            ])),
            "the raw payload is preserved verbatim"
        );
    }

    /// A V1 user message's `file` part decodes into the same typed view: its
    /// `url` is the payload URI and its `filename` the display name (with the
    /// sane default when the part carries none).
    #[test]
    fn an_inline_file_part_decodes_into_the_file_content_view() {
        let message = decoded_message(serde_json::json!([
            {"type": "file", "mime": "image/png", "filename": "shot.png",
             "url": "data:image/png;base64,QUJD"},
            {"type": "file", "mime": "application/pdf", "url": "data:application/pdf;base64,QUJD"}
        ]));

        assert_eq!(
            message.parts[0],
            Part::File(FileContent {
                uri: "data:image/png;base64,QUJD".into(),
                mime: "image/png".into(),
                name: "shot.png".into(),
                size: 3,
            })
        );
        assert_eq!(
            message.parts[1],
            Part::File(FileContent {
                uri: "data:application/pdf;base64,QUJD".into(),
                mime: "application/pdf".into(),
                name: "file".into(),
                size: 3,
            }),
            "a part without a filename reads the sane default"
        );
    }

    /// A `file` part that only references a file (no inline payload) is not a
    /// File Content: it keeps its raw payload, exactly as before.
    #[test]
    fn a_file_part_without_an_inline_payload_stays_raw() {
        let part = serde_json::json!({"type": "file", "mime": "image/png", "url": "file:///a.png"});
        let message = decoded_message(serde_json::json!([part.clone()]));

        assert_eq!(
            message.parts,
            vec![Part::Other(OtherPart {
                kind: "file".into(),
                raw: part
            })]
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
        let transcript = decode(&[envelope("msg_a1", "assistant", 1_000, parts)]);
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

    /// A message's recorded failure decodes onto the neutral message: the
    /// wrapped `AssistantError` arm (`{name, data:{message}}`) and the bare
    /// `message` shape both normalize, and a missing/null error stays `None`.
    #[test]
    fn message_errors_decode_onto_the_neutral_message() {
        let decode_one = |error: Value| {
            let mut info = serde_json::json!({
                "id": "msg_a1",
                "role": "assistant",
                "time": {"created": 1_000, "completed": 1_000},
            });
            if !error.is_null() {
                info["error"] = error;
            }
            decode_response(&serde_json::json!([{"info": info, "parts": []}]))
                .expect("a valid payload decodes")
                .messages
                .remove(0)
        };

        assert_eq!(
            decode_one(serde_json::json!({
                "name": "UnknownError",
                "data": {"message": "Provider is not available", "ref": "x"},
            }))
            .error
            .as_deref(),
            Some("Provider is not available"),
            "the wrapped AssistantError arm normalizes"
        );
        assert_eq!(
            decode_one(serde_json::json!({"message": "bare failure"}))
                .error
                .as_deref(),
            Some("bare failure"),
            "a top-level message normalizes"
        );
        assert_eq!(
            decode_one(serde_json::json!("plain string")).error.as_deref(),
            Some("plain string")
        );
        assert!(
            decode_one(Value::Null).error.is_none(),
            "a message without an error carries none"
        );
    }

    /// Decode one compaction carrier (a user message whose only part is the
    /// `compaction` part) plus, when given, its paired summary message.
    fn decode_pair(part: Value, summary: Option<Value>) -> SessionTranscript {
        let mut messages = vec![serde_json::json!({
            "info": {"id": "msg_u1", "role": "user", "time": {"created": 1_000}},
            "parts": [part],
        })];
        if let Some(summary) = summary {
            messages.push(summary);
        }
        decode_response(&Value::Array(messages)).expect("a valid payload decodes")
    }

    /// A paired summary assistant message: `parentID` names the carrier, and V1
    /// writes `mode:"compaction"` and `summary:true` together.
    fn summary_message(parent: &str, parts: Value) -> Value {
        serde_json::json!({
            "info": {"id": "msg_a1", "role": "assistant", "parentID": parent,
                      "mode": "compaction", "summary": true,
                      "time": {"created": 2_000, "completed": 2_100}},
            "parts": parts,
        })
    }

    /// The typed fact the carrier message of a decoded pair carries.
    fn fact_of(transcript: &SessionTranscript) -> &Compaction {
        let Part::Compaction(fact) = &transcript.messages[0].parts[0] else {
            panic!(
                "expected a compaction fact: {:?}",
                transcript.messages[0].parts[0]
            );
        };
        fact
    }

    /// One V1 compaction (ADR-0082) is TWO messages: a `compaction` part on a
    /// user message (the carrier) and a paired assistant summary message
    /// (`parentID` naming the carrier, `mode:"compaction"`, `summary:true`).
    /// The pair decodes to ONE typed fact on the carrier, the summary attached
    /// — and both messages leave the conversation roles, so the carrier can
    /// never become a Turn anchor and the summary can never answer a Turn.
    #[test]
    fn a_compaction_part_and_its_paired_summary_decode_to_one_fact() {
        let transcript = decode_response(&serde_json::json!([
            {"info": {"id": "msg_cola_anchor", "role": "user", "time": {"created": 500}},
             "parts": [{"type": "text", "text": "第一条消息"}]},
            {"info": {"id": "msg_u1", "role": "user", "time": {"created": 1_000}},
             "parts": [{"type": "compaction", "auto": true}]},
            {"info": {"id": "msg_a1", "role": "assistant", "parentID": "msg_u1",
                       "mode": "compaction", "summary": true,
                       "time": {"created": 2_000, "completed": 2_100}},
             "parts": [{"type": "text", "text": "保留了结论与未决问题。"}]},
        ]))
        .expect("a valid payload decodes");

        assert_eq!(
            transcript.messages[1].role,
            MessageRole::Other("compaction".into()),
            "the carrier is never a User (a Turn anchor), so it stays the verbatim kind"
        );
        let Part::Compaction(fact) = &transcript.messages[1].parts[0] else {
            panic!(
                "expected the typed compaction fact: {:?}",
                transcript.messages[1].parts[0]
            );
        };
        assert_eq!(fact.status, CompactionStatus::Completed);
        assert_eq!(fact.reason, CompactionReason::Auto);
        assert_eq!(
            fact.summary, "保留了结论与未决问题。",
            "the paired message's text rides the fact"
        );
        assert_eq!(
            transcript.messages[1].parts.len(),
            1,
            "the pair is ONE fact, not the raw part beside it"
        );
        assert_eq!(
            transcript.messages[2].role,
            MessageRole::Other("compaction".into()),
            "the summary message is never an Assistant (it answers no Turn)"
        );
    }

    /// V1's `overflow` flag maps onto the automatic reason — the reason V2
    /// itself writes for the same event — and a part that reports neither flag
    /// is the user's own `/compact`.
    #[test]
    fn a_compaction_part_reports_its_reason_from_auto_and_overflow() {
        let auto = decode_pair(serde_json::json!({"type": "compaction", "auto": true}), None);
        assert_eq!(
            fact_of(&auto).reason,
            CompactionReason::Auto,
            "an auto compaction is automatic"
        );
        let overflow = decode_pair(
            serde_json::json!({"type": "compaction", "auto": true, "overflow": true}),
            None,
        );
        assert_eq!(
            fact_of(&overflow).reason,
            CompactionReason::Auto,
            "an overflow maps onto the automatic reason, exactly as V2 states it"
        );
        let manual = decode_pair(serde_json::json!({"type": "compaction", "auto": false}), None);
        assert_eq!(
            fact_of(&manual).reason,
            CompactionReason::Manual,
            "a part the user asked for is manual"
        );
        let bare = decode_pair(serde_json::json!({"type": "compaction"}), None);
        assert_eq!(
            fact_of(&bare).reason,
            CompactionReason::Manual,
            "a part that reports no flag is not guessed into an automatic one"
        );
    }

    /// An orphaned part — a carrier the read cannot pair with a summary
    /// message — still decodes to a completed fact, with no summary rather
    /// than vanishing.
    #[test]
    fn an_orphaned_compaction_part_degrades_to_a_summaryless_completed_fact() {
        let transcript = decode_pair(serde_json::json!({"type": "compaction", "auto": true}), None);

        assert_eq!(
            transcript.messages[0].role,
            MessageRole::Other("compaction".into()),
            "an orphaned carrier is still the compaction's carrier"
        );
        let fact = fact_of(&transcript);
        assert_eq!(fact.status, CompactionStatus::Completed);
        assert_eq!(fact.reason, CompactionReason::Auto);
        assert_eq!(fact.summary, "", "there is no summary to attach");
    }

    /// A summary message whose carrier this read does not carry is not an
    /// ordinary assistant answer either: it degrades to the same completed,
    /// summary-less fact rather than rendering as conversation.
    #[test]
    fn an_orphaned_summary_message_degrades_to_a_summaryless_fact() {
        // The read carries the summary but not the user message that owns the
        // `compaction` part (a page that lost it, or a run the server stopped
        // between the two writes).
        let transcript = decode_response(&serde_json::json!([summary_message(
            "msg_gone",
            serde_json::json!([{"type": "text", "text": "保留了结论。"}]),
        )]))
        .expect("a valid payload decodes");

        assert_eq!(
            transcript.messages[0].role,
            MessageRole::Other("compaction".into()),
            "the orphaned summary is never an Assistant (it answers no Turn)"
        );
        let Part::Compaction(fact) = &transcript.messages[0].parts[0] else {
            panic!(
                "expected a degraded compaction fact: {:?}",
                transcript.messages[0].parts[0]
            );
        };
        assert_eq!(fact.status, CompactionStatus::Completed);
        assert_eq!(fact.summary, "");
    }

    /// V1 spells `summary` differently per role — a bool on an assistant, an
    /// object (`{additions, deletions, files, diffs}`) on a user message. The
    /// decoder reads past the user-message shape instead of failing the whole
    /// read on it, and never treats a user message as a compaction summary.
    #[test]
    fn a_user_messages_summary_object_is_not_a_compaction_summary() {
        let transcript = decode_response(&serde_json::json!([
            {"info": {"id": "msg_u1", "role": "user", "time": {"created": 1_000},
                       "summary": {"additions": 0, "deletions": 0, "files": 0, "diffs": []}},
             "parts": [{"type": "text", "text": "改了一下"}]},
        ]))
        .expect("a user message's format summary must not fail the read");

        assert_eq!(transcript.messages[0].role, MessageRole::User);
        assert_eq!(transcript.messages[0].text(), "改了一下");
    }

    /// The summary is the paired message's non-empty `text` parts, joined as
    /// paragraphs in order — V1 composes a summary that way, and an empty part
    /// contributes nothing.
    #[test]
    fn a_multi_part_summary_joins_its_non_empty_text_parts() {
        let transcript = decode_pair(
            serde_json::json!({"type": "compaction", "auto": true}),
            Some(summary_message(
                "msg_u1",
                serde_json::json!([
                    {"type": "text", "text": "第一段。"},
                    {"type": "text", "text": ""},
                    {"type": "text", "text": "第二段。"},
                ]),
            )),
        );

        assert_eq!(fact_of(&transcript).summary, "第一段。\n\n第二段。");
    }

    /// The carrier is never a User message and the paired summary never an
    /// Assistant one, so the transcript's own projections — the ones that
    /// decide Turn membership — cannot pick either up: a newer carrier is not
    /// the session's newest user, and a summary answers no Turn.
    #[test]
    fn a_carrier_is_never_a_turn_anchor_and_a_summary_never_answers() {
        let transcript = decode_response(&serde_json::json!([
            {"info": {"id": "msg_cola_anchor", "role": "user", "time": {"created": 1_000}},
             "parts": [{"type": "text", "text": "第一条消息"}]},
            {"info": {"id": "msg_u1", "role": "user", "time": {"created": 2_000}},
             "parts": [{"type": "compaction", "auto": true}]},
            {"info": {"id": "msg_a1", "role": "assistant", "parentID": "msg_u1",
                       "mode": "compaction", "summary": true,
                       "time": {"created": 2_100, "completed": 2_200}},
             "parts": [{"type": "text", "text": "保留了结论。"}]},
            {"info": {"id": "msg_a2", "role": "assistant", "parentID": "msg_cola_anchor",
                       "mode": "build", "time": {"created": 3_000, "completed": 3_100}},
             "parts": [{"type": "text", "text": "第一轮回答。"},
                        {"type": "step-finish", "reason": "stop"}]},
        ]))
        .expect("a valid payload decodes");

        let newest = transcript
            .newest_user()
            .expect("the real user message is the session's newest user");
        assert_eq!(
            newest.id.as_str(),
            "msg_cola_anchor",
            "a compaction carrier, though newer, is never the newest user"
        );
        let anchor = newest.anchor().expect("the anchor carries its server time");
        let turn = transcript.turn_for_user(&anchor);
        let ids: Vec<_> = turn.messages.iter().map(|message| message.id.as_str()).collect();
        assert_eq!(
            ids,
            vec!["msg_a2"],
            "the compaction summary neither answers the Turn nor completes it"
        );
        assert!(turn.complete, "the real step-finish completes the Turn");
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
