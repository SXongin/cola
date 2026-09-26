//! The live suite's scripted OpenAI-compatible provider (ADR-0057).
//!
//! An in-process HTTP endpoint that answers the OpenCode server's model calls
//! with a fixed script over SSE, so the real agent loop can be driven
//! deterministically and the requests it made can be asserted. Three scripts,
//! chosen from the request body alone:
//!
//! - **title** — the session-title call carries no tools: answer plain text;
//! - **tool call** — a real turn's first model call: stream reasoning, then a
//!   `bash` tool call the server must gate behind a permission ask;
//! - **final text** — any call whose conversation already carries a tool
//!   result: stream the closing text.
//!
//! Every script streams its content as **multiple SSE deltas** (text,
//! reasoning and tool-call arguments alike), so the suite exercises the
//! incremental path — a server that ignored SSE framing and answered in one
//! shot would not produce the assembled parts. The unit tests below pin that
//! the deltas concatenate to the full scripted values; the live test pins that
//! the transcript the adapter decoded carries exactly those values.
//!
//! Everything the server sent is recorded by the hosting [`TestHttpServer`],
//! so the test asserts both the request side (the tool schema was offered, the
//! tool result came back) and the response side (the transcript the adapter
//! decoded).

use serde_json::{Value, json};

use crate::test_http::{DynamicResponse, TestHttpServer};

/// The provider id the isolated server config declares.
pub const PROVIDER: &str = "scripted";
/// The model id the isolated server config declares.
pub const MODEL: &str = "scripted-model";
/// The full `provider/model` reference cola prompts with.
pub const MODEL_REF: &str = "scripted/scripted-model";

/// The reasoning deltas the tool-call script streams, in order. Assembled they
/// must equal [`REASONING_TEXT`] (pinned by a unit test below).
const REASONING_DELTAS: &[&str] = &["live-harness-", "reasoning"];
/// The assembled reasoning the transcript must carry.
pub const REASONING_TEXT: &str = "live-harness-reasoning";

/// The `bash` command the scripted tool call asks to run.
pub const TOOL_COMMAND: &str = "echo live-harness-tool";
/// The tool-call argument fragments, streamed in order. Assembled they must be
/// `{"command": TOOL_COMMAND}` (pinned by a unit test below).
const TOOL_ARGUMENT_DELTAS: &[&str] = &[r#"{"command":"echo live-"#, r#"harness-tool"}"#];
/// The correlation id the streamed tool call declares once, on its first delta.
const TOOL_CALL_ID: &str = "call_live_harness_1";

/// The closing-text deltas, streamed in order. Assembled they must equal
/// [`FINAL_TEXT`] (pinned by a unit test below).
const FINAL_TEXT_DELTAS: &[&str] = &["live-harness-final", "-text"];
/// The assembled closing text the transcript must carry.
pub const FINAL_TEXT: &str = "live-harness-final-text";

/// The title-generation deltas; assembled they equal `TITLE_TEXT`.
const TITLE_DELTAS: &[&str] = &["live harness ", "title"];
const TITLE_TEXT: &str = "live harness title";

/// Start a provider that answers `POST /v1/chat/completions` from the scripts.
pub async fn start() -> TestHttpServer {
    let server = TestHttpServer::start().await;
    server.route_dynamic("POST", "/v1/chat/completions", |request| {
        DynamicResponse::new(200, "text/event-stream", completion_body(&request.body))
    });
    server
}

/// Which script a request's body selects.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Script {
    /// No tools were offered — the title-generation call.
    Title,
    /// Tools offered, no tool result yet — the turn's first model call.
    ToolCall,
    /// A tool result is already in the conversation — the closing call.
    FinalText,
}

/// The full SSE body for one model call, selected from the request body.
fn completion_body(body: &str) -> String {
    let request: Value = serde_json::from_str(body).unwrap_or(Value::Null);
    match script(&request) {
        Script::Title => sse(&[with_role(content_chunks(TITLE_DELTAS)), finish_chunk("stop")].concat()),
        Script::ToolCall => sse(&[
            with_role([reasoning_chunks(REASONING_DELTAS), tool_call_chunks()].concat()),
            finish_chunk("tool_calls"),
        ]
        .concat()),
        Script::FinalText => {
            sse(&[with_role(content_chunks(FINAL_TEXT_DELTAS)), finish_chunk("stop")].concat())
        }
    }
}

/// Select the script from the request body alone (pure; unit-tested below).
///
/// The discriminators are structural, not counts: the title call offers no
/// tools, and the closing call is the first whose conversation carries a
/// `tool` role message. Both hold no matter how the server interleaves the
/// title and turn calls.
fn script(request: &Value) -> Script {
    if !offers_tools(request) {
        return Script::Title;
    }
    if has_tool_result(request) {
        return Script::FinalText;
    }
    Script::ToolCall
}

/// Whether the request offered at least one tool schema.
pub(super) fn offers_tools(request: &Value) -> bool {
    request
        .get("tools")
        .and_then(Value::as_array)
        .is_some_and(|tools| !tools.is_empty())
}

/// Whether any message in the conversation is a tool result.
pub(super) fn has_tool_result(request: &Value) -> bool {
    request
        .get("messages")
        .and_then(Value::as_array)
        .is_some_and(|messages| {
            messages
                .iter()
                .any(|message| message.get("role").and_then(Value::as_str) == Some("tool"))
        })
}

/// One OpenAI-compatible streaming chunk. `usage` is only present on the
/// final chunk of a script, exactly as an OpenAI-compatible server sends it.
fn chunk(delta: Value, finish_reason: Option<&str>, usage: bool) -> Value {
    let mut value = json!({
        "id": "chatcmpl-live-harness",
        "object": "chat.completion.chunk",
        "created": 0,
        "model": MODEL,
        "choices": [{
            "index": 0,
            "delta": delta,
            "finish_reason": finish_reason,
        }],
    });
    if usage {
        value["usage"] = json!({
            "prompt_tokens": 11,
            "completion_tokens": 7,
            "total_tokens": 18,
        });
    }
    value
}

/// Put `role: "assistant"` on the message's first delta only — the canonical
/// OpenAI SSE shape; later deltas carry just their fields.
fn with_role(mut chunks: Vec<Value>) -> Vec<Value> {
    if let Some(first) = chunks.first_mut() {
        first["choices"][0]["delta"]["role"] = json!("assistant");
    }
    chunks
}

/// One `content` delta per fragment, in order (the role is added once, on the
/// message's first delta, by [`with_role`]).
fn content_chunks(deltas: &[&str]) -> Vec<Value> {
    deltas
        .iter()
        .map(|delta| chunk(json!({ "content": delta }), None, false))
        .collect()
}

/// One `reasoning_content` delta per fragment, in order (the role is added
/// once, on the message's first delta, by [`with_role`]).
fn reasoning_chunks(deltas: &[&str]) -> Vec<Value> {
    deltas
        .iter()
        .map(|delta| chunk(json!({ "reasoning_content": delta }), None, false))
        .collect()
}

/// The streamed `bash` tool call: its id and name arrive on the first delta,
/// its arguments split across one delta each — the OpenAI wire shape.
fn tool_call_chunks() -> Vec<Value> {
    let (first, rest) = TOOL_ARGUMENT_DELTAS
        .split_first()
        .expect("the tool call needs at least one argument delta");
    let mut chunks = vec![chunk(
        json!({
            "tool_calls": [{
                "index": 0,
                "id": TOOL_CALL_ID,
                "type": "function",
                "function": { "name": "bash", "arguments": first },
            }],
        }),
        None,
        false,
    )];
    chunks.extend(rest.iter().map(|arguments| {
        chunk(
            json!({
                "tool_calls": [{
                    "index": 0,
                    "function": { "arguments": arguments },
                }],
            }),
            None,
            false,
        )
    }));
    chunks
}

/// The script's closing chunk: an empty delta, the finish reason, and usage.
fn finish_chunk(finish_reason: &str) -> Vec<Value> {
    vec![chunk(json!({}), Some(finish_reason), true)]
}

/// Join chunks into an SSE body, terminated with the OpenAI `[DONE]` sentinel.
fn sse(chunks: &[Value]) -> String {
    let mut body = String::new();
    for chunk in chunks {
        body.push_str("data: ");
        body.push_str(&chunk.to_string());
        body.push_str("\n\n");
    }
    body.push_str("data: [DONE]\n\n");
    body
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The SSE events of a body, `[DONE]` excluded.
    fn sse_events(body: &str) -> Vec<Value> {
        body.lines()
            .filter_map(|line| line.strip_prefix("data: "))
            .filter(|data| *data != "[DONE]")
            .map(|data| serde_json::from_str(data).expect("an SSE event must be JSON"))
            .collect()
    }

    /// Every value of a streamed delta field, in event order.
    fn streamed_fragments(events: &[Value], field: &str) -> Vec<String> {
        events
            .iter()
            .filter_map(|event| event["choices"][0]["delta"][field].as_str().map(str::to_string))
            .collect()
    }

    /// Every streamed tool-call argument fragment, in event order.
    fn streamed_argument_fragments(events: &[Value]) -> Vec<String> {
        events
            .iter()
            .filter_map(|event| {
                event["choices"][0]["delta"]["tool_calls"][0]["function"]["arguments"]
                    .as_str()
                    .map(str::to_string)
            })
            .collect()
    }

    /// The stream must arrive as 2+ deltas, and assembling them must yield the
    /// full scripted value — the property a one-shot answer would not have.
    fn assert_streamed(fragments: &[String], expected: &str, what: &str) {
        assert!(
            fragments.len() >= 2,
            "{what} must stream as multiple SSE deltas, got {fragments:?}"
        );
        assert_eq!(
            fragments.concat(),
            expected,
            "{what} must assemble to the full scripted value"
        );
    }

    /// The finish reason on the final event — the only event that carries one.
    fn finish_reason(events: &[Value]) -> Option<&str> {
        events
            .last()
            .and_then(|event| event["choices"][0]["finish_reason"].as_str())
    }

    /// The `role` values the stream carries, in event order.
    fn roles(events: &[Value]) -> Vec<&str> {
        events
            .iter()
            .filter_map(|event| event["choices"][0]["delta"]["role"].as_str())
            .collect()
    }

    fn request(script: Script) -> Value {
        match script {
            Script::Title => json!({ "model": MODEL, "messages": [{ "role": "user", "content": "hi" }] }),
            Script::ToolCall => json!({
                "model": MODEL,
                "tools": [{ "type": "function", "function": { "name": "bash" } }],
                "messages": [{ "role": "user", "content": "run something" }],
            }),
            Script::FinalText => json!({
                "model": MODEL,
                "tools": [{ "type": "function", "function": { "name": "bash" } }],
                "messages": [
                    { "role": "user", "content": "run something" },
                    { "role": "assistant", "tool_calls": [] },
                    { "role": "tool", "content": "live-harness-tool" },
                ],
            }),
        }
    }

    #[test]
    fn a_request_without_tools_selects_the_title_script() {
        let request = json!({ "model": MODEL, "messages": [{ "role": "user", "content": "hi" }] });
        assert_eq!(script(&request), Script::Title);
        let request = json!({ "model": MODEL, "tools": [], "messages": [] });
        assert_eq!(script(&request), Script::Title);
    }

    #[test]
    fn a_first_turn_request_selects_the_tool_call_script() {
        let request = json!({
            "model": MODEL,
            "tools": [{ "type": "function", "function": { "name": "bash" } }],
            "messages": [{ "role": "user", "content": "run something" }],
        });
        assert_eq!(script(&request), Script::ToolCall);
    }

    #[test]
    fn a_request_with_a_tool_result_selects_the_final_text_script() {
        let request = json!({
            "model": MODEL,
            "tools": [{ "type": "function", "function": { "name": "bash" } }],
            "messages": [
                { "role": "user", "content": "run something" },
                { "role": "assistant", "tool_calls": [] },
                { "role": "tool", "content": "live-harness-tool" },
            ],
        });
        assert_eq!(script(&request), Script::FinalText);
    }

    #[test]
    fn the_title_script_streams_its_text_in_multiple_deltas() {
        let events = sse_events(&completion_body(&request(Script::Title).to_string()));
        assert_streamed(
            &streamed_fragments(&events, "content"),
            TITLE_TEXT,
            "the title text",
        );
        assert_eq!(finish_reason(&events), Some("stop"), "{events:#?}");
        assert_eq!(roles(&events), vec!["assistant"], "role on the first delta only");
    }

    #[test]
    fn the_tool_call_script_streams_reasoning_and_split_arguments() {
        let events = sse_events(&completion_body(&request(Script::ToolCall).to_string()));

        assert_streamed(
            &streamed_fragments(&events, "reasoning_content"),
            REASONING_TEXT,
            "the reasoning",
        );
        assert_streamed(
            &streamed_argument_fragments(&events),
            &json!({ "command": TOOL_COMMAND }).to_string(),
            "the tool-call arguments",
        );
        assert_eq!(
            finish_reason(&events),
            Some("tool_calls"),
            "the call must finish with tool_calls: {events:#?}"
        );
        assert_eq!(roles(&events), vec!["assistant"], "role on the first delta only");
        assert!(
            events.len() >= 4,
            "reasoning + tool call must stream as separate events"
        );
    }

    #[test]
    fn the_final_script_streams_its_text_in_multiple_deltas() {
        let events = sse_events(&completion_body(&request(Script::FinalText).to_string()));
        assert_streamed(
            &streamed_fragments(&events, "content"),
            FINAL_TEXT,
            "the closing text",
        );
        assert_eq!(finish_reason(&events), Some("stop"), "{events:#?}");
        assert_eq!(roles(&events), vec!["assistant"], "role on the first delta only");
        assert!(
            !events
                .iter()
                .any(|event| event.to_string().contains(TOOL_COMMAND)),
            "no second tool call: {events:#?}"
        );
    }

    #[test]
    fn every_script_terminates_with_the_done_sentinel() {
        for script in [Script::Title, Script::ToolCall, Script::FinalText] {
            let body = completion_body(&request(script).to_string());
            assert!(body.ends_with("data: [DONE]\n\n"), "{script:?}: {body}");
        }
    }
}
