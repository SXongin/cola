//! The live suite's scripted OpenAI-compatible provider (ADR-0057).
//!
//! An in-process HTTP endpoint that answers the OpenCode server's model calls
//! with a fixed script over SSE, so the real agent loop can be driven
//! deterministically and the requests it made can be asserted. Three scripts,
//! chosen from the request body alone:
//!
//! - **title** — the session-title call carries no tools: answer plain text;
//! - **tool call** — a real turn's first model call: stream reasoning, then a
//!   shell tool call the server must gate behind a permission ask;
//! - **final text** — any call whose conversation already carries a tool
//!   result: stream the closing text.
//!
//! The tool the script calls is the generation's own ([`Tool::Bash`] on V1,
//! [`Tool::Shell`] on V2 — the V1→V2 rename); everything else about the script
//! is shared.
//!
//! Every script streams its content as **multiple SSE deltas** (text,
//! reasoning and tool-call arguments alike), so the suite exercises the
//! incremental path — a server that ignored SSE framing and answered in one
//! shot would not produce the assembled parts. The unit tests below pin that
//! the deltas concatenate to the full scripted values; the live tests pin that
//! the transcript the adapter decoded carries exactly those values.
//!
//! Everything the server sent is recorded by the hosting [`TestHttpServer`],
//! so the tests assert both the request side (the tool schema was offered, the
//! tool result came back) and the response side (the transcript the adapter
//! decoded).

use serde_json::{Value, json};

use crate::test_http::{DynamicResponse, TestHttpServer};

/// The shell tool the scripted turn calls, per generation: V1 still calls it
/// `bash`, V2 renamed it `shell` (the permission action follows the rename).
/// [`Tool::Question`] is V2's form-producing tool (V1 has no equivalent
/// scripted here — its question tool asks with positional questions instead).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tool {
    /// V1's `bash` tool.
    Bash,
    /// V2's `shell` tool.
    Shell,
    /// V2's `question` tool: the turn blocks on a form the test answers.
    Question,
}

impl Tool {
    /// The tool name on the model wire and in the transcript.
    pub fn name(self) -> &'static str {
        match self {
            Tool::Bash => "bash",
            Tool::Shell => "shell",
            Tool::Question => "question",
        }
    }
}

/// The command the scripted tool call asks to run. The fast one is enough for
/// the V1 capability chain; the slow one keeps the tool in its running state
/// long enough for a live read to observe (and record) it, so the V2 read test
/// is deterministic rather than racing the echo.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolCommand {
    /// `echo live-harness-tool` — settles immediately.
    Fast,
    /// `sleep 2; echo live-harness-tool` — still running for ~2 s.
    Slow,
}

impl ToolCommand {
    /// The command text the transcript must carry back.
    pub fn text(self) -> &'static str {
        match self {
            ToolCommand::Fast => "echo live-harness-tool",
            ToolCommand::Slow => "sleep 2; echo live-harness-tool",
        }
    }
}

/// The provider id the isolated server config declares.
pub const PROVIDER: &str = "scripted";
/// The model id the isolated server config declares.
pub const MODEL: &str = "scripted-model";
/// The full `provider/model` reference cola prompts with.
pub const MODEL_REF: &str = "scripted/scripted-model";
/// The second model the isolated V2 config declares, with declared variants —
/// what the S7 selection chain switches to and attaches a variant to. The
/// scripted provider answers any requested model, so the switch's effect is
/// read from the session's ref and the transcript, not from the endpoint.
pub const MODEL_ALT: &str = "scripted-model-alt";
/// The full `provider/model` reference for [`MODEL_ALT`].
pub const MODEL_ALT_REF: &str = "scripted/scripted-model-alt";
/// The thinking-level variants [`MODEL_ALT`] declares.
pub const MODEL_ALT_VARIANTS: &[&str] = &["high", "low"];
/// The variant the selection chain attaches to [`MODEL_ALT`]. [`MODEL`] does
/// not declare it, which is what exercises ADR-0020's clear-on-switch rule.
pub const VARIANT: &str = "high";
/// The custom agent the isolated V2 config declares — the S7 agent switch's
/// target (the built-ins alone could not prove a changed selection).
pub const AGENT: &str = "live-agent";

/// The reasoning deltas the tool-call script streams, in order. Assembled they
/// must equal [`REASONING_TEXT`] (pinned by a unit test below).
const REASONING_DELTAS: &[&str] = &["live-harness-", "reasoning"];
/// The assembled reasoning the transcript must carry.
pub const REASONING_TEXT: &str = "live-harness-reasoning";

/// The `echo` command the scripted tool call asks to run by default.
pub const TOOL_COMMAND: &str = "echo live-harness-tool";
/// The correlation id the streamed tool call declares once, on its first delta.
const TOOL_CALL_ID: &str = "call_live_harness_1";

/// The question the scripted `question` tool call asks. Its form round-trip is
/// asserted structurally: one field, keyed `q0`, with these options.
pub const QUESTION_TEXT: &str = "live harness question?";
/// The question's short header.
pub const QUESTION_HEADER: &str = "harness";
/// The option the form chain submits.
pub const QUESTION_OPTION: &str = "继续";

/// The closing-text deltas, streamed in order. Assembled they must equal
/// [`FINAL_TEXT`] (pinned by a unit test below).
const FINAL_TEXT_DELTAS: &[&str] = &["live-harness-final", "-text"];
/// The assembled closing text the transcript must carry.
pub const FINAL_TEXT: &str = "live-harness-final-text";

/// The title-generation deltas; assembled they equal `TITLE_TEXT`.
const TITLE_DELTAS: &[&str] = &["live harness ", "title"];
const TITLE_TEXT: &str = "live harness title";

/// Start a provider whose scripted turn calls V1's `bash` tool with the fast
/// command.
pub async fn start() -> TestHttpServer {
    start_with(Tool::Bash, ToolCommand::Fast).await
}

/// Start a provider whose scripted turn calls `tool` (the generation's shell
/// name) with `command`, answering `POST /v1/chat/completions` from the
/// scripts.
pub async fn start_with(tool: Tool, command: ToolCommand) -> TestHttpServer {
    let server = TestHttpServer::start().await;
    server.route_dynamic("POST", "/v1/chat/completions", move |request| {
        DynamicResponse::new(
            200,
            "text/event-stream",
            completion_body(&request.body, tool, command),
        )
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
fn completion_body(body: &str, tool: Tool, command: ToolCommand) -> String {
    let request: Value = serde_json::from_str(body).unwrap_or(Value::Null);
    match script(&request) {
        Script::Title => sse(&[with_role(content_chunks(TITLE_DELTAS)), finish_chunk("stop")].concat()),
        Script::ToolCall => sse(&[
            with_role(
                [
                    reasoning_chunks(REASONING_DELTAS),
                    tool_call_chunks(tool, command),
                ]
                .concat(),
            ),
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

/// The streamed tool call's arguments: the shell tools carry `command`, the
/// question tool carries V2's `questions` array (header, question text, options
/// and `multiple`). Split across two SSE deltas below — the OpenAI wire shape,
/// and enough fragmentation to prove the client reassembles them.
fn tool_call_arguments(tool: Tool, command: ToolCommand) -> Value {
    match tool {
        Tool::Bash | Tool::Shell => json!({ "command": command.text() }),
        Tool::Question => json!({
            "questions": [{
                "question": QUESTION_TEXT,
                "header": QUESTION_HEADER,
                "options": [
                    { "label": QUESTION_OPTION, "description": "" },
                    { "label": "停止", "description": "" },
                ],
                "multiple": false,
            }],
        }),
    }
}

/// The streamed tool call: its id and name arrive on the first delta, its
/// arguments split across two deltas — the OpenAI wire shape, and enough
/// fragmentation to prove the client reassembles them. The name is the
/// generation's (`bash` / `shell`) or the question tool's.
fn tool_call_chunks(tool: Tool, command: ToolCommand) -> Vec<Value> {
    let arguments = tool_call_arguments(tool, command).to_string();
    let (first, rest) = arguments.split_at(arguments.len() / 2);
    let mut chunks = vec![chunk(
        json!({
            "tool_calls": [{
                "index": 0,
                "id": TOOL_CALL_ID,
                "type": "function",
                "function": { "name": tool.name(), "arguments": first },
            }],
        }),
        None,
        false,
    )];
    chunks.push(chunk(
        json!({
            "tool_calls": [{
                "index": 0,
                "function": { "arguments": rest },
            }],
        }),
        None,
        false,
    ));
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
        let events = sse_events(&completion_body(
            &request(Script::Title).to_string(),
            Tool::Bash,
            ToolCommand::Fast,
        ));
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
        let events = sse_events(&completion_body(
            &request(Script::ToolCall).to_string(),
            Tool::Bash,
            ToolCommand::Fast,
        ));

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

    /// The streamed tool call's name is the generation's own (`bash` on V1,
    /// `shell` on V2).
    #[test]
    fn the_scripted_tool_name_is_the_generations_shell() {
        for tool in [Tool::Bash, Tool::Shell] {
            let events = sse_events(&completion_body(
                &request(Script::ToolCall).to_string(),
                tool,
                ToolCommand::Fast,
            ));
            assert_eq!(
                events.iter().find_map(|event| {
                    event["choices"][0]["delta"]["tool_calls"][0]["function"]["name"].as_str()
                }),
                Some(tool.name()),
                "{tool:?}"
            );
        }
    }

    /// The question script streams V2's `question` tool call: the name and the
    /// fragmented `questions` arguments reassemble into the form the live
    /// chain asserts (one string field with two options).
    #[test]
    fn the_question_script_streams_the_form_arguments() {
        let events = sse_events(&completion_body(
            &request(Script::ToolCall).to_string(),
            Tool::Question,
            ToolCommand::Fast,
        ));
        assert_eq!(
            events.iter().find_map(|event| {
                event["choices"][0]["delta"]["tool_calls"][0]["function"]["name"].as_str()
            }),
            Some("question")
        );
        let fragments = streamed_argument_fragments(&events);
        assert!(fragments.len() >= 2, "arguments must stream as deltas");
        let arguments: Value = serde_json::from_str(&fragments.concat()).expect("valid JSON arguments");
        assert_eq!(arguments["questions"][0]["question"], QUESTION_TEXT);
        assert_eq!(arguments["questions"][0]["header"], QUESTION_HEADER);
        assert_eq!(arguments["questions"][0]["options"][0]["label"], QUESTION_OPTION);
        assert_eq!(arguments["questions"][0]["multiple"], false);
    }

    /// Both scripted commands stream as fragmented argument deltas and
    /// assemble to the exact `{"command": …}` JSON — the slow one is what keeps
    /// the V2 in-flight read deterministic.
    #[test]
    fn the_scripted_command_is_parameterized_and_reassembles() {
        for command in [ToolCommand::Fast, ToolCommand::Slow] {
            let events = sse_events(&completion_body(
                &request(Script::ToolCall).to_string(),
                Tool::Shell,
                command,
            ));
            assert_streamed(
                &streamed_argument_fragments(&events),
                &json!({ "command": command.text() }).to_string(),
                "the tool-call arguments",
            );
        }
        assert!(
            ToolCommand::Slow.text().starts_with("sleep"),
            "the slow command must keep the tool running"
        );
    }

    #[test]
    fn the_final_script_streams_its_text_in_multiple_deltas() {
        let events = sse_events(&completion_body(
            &request(Script::FinalText).to_string(),
            Tool::Bash,
            ToolCommand::Fast,
        ));
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
            let body = completion_body(&request(script).to_string(), Tool::Bash, ToolCommand::Fast);
            assert!(body.ends_with("data: [DONE]\n\n"), "{script:?}: {body}");
        }
    }
}
