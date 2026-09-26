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
/// Reasoning the scripted model streams on the tool-call script.
pub const REASONING_TEXT: &str = "live-harness-reasoning";
/// The `bash` command the scripted tool call asks to run.
pub const TOOL_COMMAND: &str = "echo live-harness-tool";
/// The closing text the scripted model streams after the tool result.
pub const FINAL_TEXT: &str = "live-harness-final-text";
/// Plain text answering the title-generation call.
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
        Script::Title => sse(&[
            chunk(json!({ "role": "assistant", "content": TITLE_TEXT }), None, false),
            chunk(json!({}), Some("stop"), true),
        ]),
        Script::ToolCall => sse(&[
            chunk(
                json!({ "role": "assistant", "reasoning_content": REASONING_TEXT }),
                None,
                false,
            ),
            chunk(tool_call_delta(), None, false),
            chunk(json!({}), Some("tool_calls"), true),
        ]),
        Script::FinalText => sse(&[
            chunk(json!({ "role": "assistant", "content": FINAL_TEXT }), None, false),
            chunk(json!({}), Some("stop"), true),
        ]),
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

/// The streamed `bash` tool call, with its arguments as the JSON string the
/// OpenAI wire carries.
fn tool_call_delta() -> Value {
    json!({
        "tool_calls": [{
            "index": 0,
            "id": "call_live_harness_1",
            "type": "function",
            "function": {
                "name": "bash",
                "arguments": json!({ "command": TOOL_COMMAND }).to_string(),
            },
        }],
    })
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
    fn the_tool_call_script_streams_reasoning_then_the_bash_call() {
        let request = json!({
            "model": MODEL,
            "tools": [{ "type": "function", "function": { "name": "bash" } }],
            "messages": [],
        });
        let body = completion_body(&request.to_string());
        assert!(body.contains(REASONING_TEXT), "reasoning must stream: {body}");
        assert!(
            body.contains("call_live_harness_1"),
            "tool call must stream: {body}"
        );
        assert!(
            body.contains("tool_calls"),
            "the call must finish with tool_calls: {body}"
        );
        assert!(body.contains("[DONE]"), "the stream must terminate: {body}");
    }

    #[test]
    fn the_final_script_streams_the_closing_text() {
        let request = json!({
            "model": MODEL,
            "tools": [{ "type": "function", "function": { "name": "bash" } }],
            "messages": [{ "role": "tool", "content": "done" }],
        });
        let body = completion_body(&request.to_string());
        assert!(body.contains(FINAL_TEXT), "final text must stream: {body}");
        assert!(!body.contains(TOOL_COMMAND), "no second tool call: {body}");
    }
}
