//! The live Codex suite's scripted Responses provider (ticket #953).
//!
//! Codex speaks OpenAI's Responses API over SSE. An in-process
//! [`TestHttpServer`] answers `/v1/responses` from the request body alone, so
//! the real app-server's model loop runs without a network or a credential:
//!
//! - **text** — a plain assistant message, streamed as one
//!   `response.output_item.done` plus `response.completed`;
//! - **approval** — a `exec_command` function call; after the tool result comes
//!   back, the closing text. The thread's read-only sandbox plus `untrusted`
//!   approval policy turns the call into a real approval request;
//! - **interrupt** — the handler holds the response open until the test
//!   releases it, so `turn/interrupt` lands while the model call is in flight.
//!
//! The prompts select the script; Codex only ever sends the user's own text as
//! the last user message, so the choice is deterministic.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use serde_json::{Value, json};
use tokio::sync::Notify;

use crate::test_http::{DynamicResponse, RecordedRequest, TestHttpServer};

/// The prompt that asks for the plain scripted text.
pub const TEXT_PROMPT: &str = "live: answer with the scripted text";
/// The text the scripted provider returns for [`TEXT_PROMPT`].
pub const TEXT: &str = "SCRIPTED LIVE TEXT";

/// The prompt that asks for the command tool call.
pub const APPROVAL_PROMPT: &str = "live: run the scripted command";
/// The text returned after the approved command completes.
pub const APPROVAL_TEXT: &str = "SCRIPTED APPROVAL DONE";
/// The command the scripted tool call runs. It writes, so the thread's
/// read-only sandbox turns it into a real approval request.
pub const APPROVAL_COMMAND: &str = "echo cola-live-approved > cola-live-approved.txt";

/// The prompt whose model call is held open for the interrupt.
pub const INTERRUPT_PROMPT: &str = "live: wait until interrupted";

/// How long a held response waits before giving up when the test never
/// releases it (a panic in the test must not wedge the HTTP task forever).
const HELD_RESPONSE_BUDGET: Duration = Duration::from_secs(60);

struct Script {
    calls: AtomicUsize,
    /// Fired when the interrupt script's model call has arrived and is waiting.
    /// `notify_one` stores a permit, so a test that starts waiting late still
    /// sees it.
    blocked: Notify,
    /// Released by the test after it sent `turn/interrupt`.
    release: Notify,
}

/// A scripted Responses provider on a real loopback socket.
pub struct ScriptedProvider {
    server: TestHttpServer,
    script: Arc<Script>,
}

impl ScriptedProvider {
    pub async fn start() -> Self {
        let server = TestHttpServer::start().await;
        let script = Arc::new(Script {
            calls: AtomicUsize::new(0),
            blocked: Notify::new(),
            release: Notify::new(),
        });
        let routed = Arc::clone(&script);
        server.route_async_dynamic("POST", "/v1/responses", move |request| {
            let script = Arc::clone(&routed);
            async move { answer_async(&script, request).await }
        });
        Self { server, script }
    }

    /// `http://127.0.0.1:<port>` — the config's `base_url` is this plus `/v1`.
    pub fn base_url(&self) -> String {
        self.server.base_url()
    }

    /// Await the held interrupt call; returns immediately when it already
    /// arrived (the permit is stored).
    pub async fn blocked(&self) {
        self.script.blocked.notified().await;
    }

    /// Let the held interrupt response finish (it is discarded by the
    /// interrupted turn).
    pub fn release(&self) {
        self.script.release.notify_one();
    }

    /// Every request Codex made, in order.
    pub fn requests(&self) -> Vec<RecordedRequest> {
        self.server.requests()
    }
}

/// The scripted handler: the interrupt script parks here.
async fn answer_async(script: &Script, request: RecordedRequest) -> DynamicResponse {
    let body: Value = serde_json::from_str(&request.body).unwrap_or(Value::Null);
    let prompt = prompt_text(&body);
    let call = script.calls.fetch_add(1, Ordering::SeqCst) + 1;

    if prompt.contains("interrupt") {
        script.blocked.notify_one();
        let _ = tokio::time::timeout(HELD_RESPONSE_BUDGET, script.release.notified()).await;
        return DynamicResponse::new(200, "text/event-stream", text_stream("interrupted", call));
    }
    if prompt.contains("command") {
        if has_tool_result(&body) {
            return DynamicResponse::new(200, "text/event-stream", text_stream(APPROVAL_TEXT, call));
        }
        return DynamicResponse::new(200, "text/event-stream", function_call_stream(call));
    }
    DynamicResponse::new(200, "text/event-stream", text_stream(TEXT, call))
}

fn prompt_text(body: &Value) -> String {
    let Some(items) = body.get("input").and_then(Value::as_array) else {
        return String::new();
    };
    for item in items.iter().rev() {
        if item.get("type").and_then(Value::as_str) != Some("message")
            || item.get("role").and_then(Value::as_str) != Some("user")
        {
            continue;
        }
        let Some(content) = item.get("content").and_then(Value::as_array) else {
            continue;
        };
        let text: String = content
            .iter()
            .filter_map(|part| part.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join("");
        if !text.is_empty() {
            return text;
        }
    }
    String::new()
}

fn has_tool_result(body: &Value) -> bool {
    body.get("input").and_then(Value::as_array).is_some_and(|items| {
        items
            .iter()
            .any(|item| item.get("type").and_then(Value::as_str) == Some("function_call_output"))
    })
}

fn text_stream(text: &str, call: usize) -> Vec<u8> {
    let item = json!({
        "id": format!("msg_{call}"),
        "type": "message",
        "role": "assistant",
        "status": "completed",
        "content": [{ "type": "output_text", "text": text, "annotations": [] }],
    });
    completed_stream(call, item)
}

fn function_call_stream(call: usize) -> Vec<u8> {
    let item = json!({
        "id": format!("fc_{call}"),
        "type": "function_call",
        "name": "exec_command",
        "arguments": json!({
            "cmd": APPROVAL_COMMAND,
            "sandbox_permissions": "require_escalated",
            "justification": "write the scripted approval file",
            "yield_time_ms": 10_000,
        })
        .to_string(),
        "call_id": format!("call_{call}"),
        "status": "completed",
    });
    completed_stream(call, item)
}

fn completed_stream(call: usize, item: Value) -> Vec<u8> {
    let created = json!({
        "type": "response.created",
        "response": {
            "id": format!("resp_{call}"),
            "object": "response",
            "status": "in_progress",
            "model": "cola-live-scripted",
            "output": [],
        },
    });
    let item_done = json!({
        "type": "response.output_item.done",
        "output_index": 0,
        "item": item,
    });
    let completed = json!({
        "type": "response.completed",
        "response": {
            "id": format!("resp_{call}"),
            "object": "response",
            "status": "completed",
            "model": "cola-live-scripted",
            "output": [item],
            "usage": { "input_tokens": 1, "output_tokens": 1, "total_tokens": 2 },
        },
    });
    let mut body = Vec::new();
    for (event, data) in [
        ("response.created", created),
        ("response.output_item.done", item_done),
        ("response.completed", completed),
    ] {
        body.extend_from_slice(format!("event: {event}\ndata: {data}\n\n").as_bytes());
    }
    body
}
