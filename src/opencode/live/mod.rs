//! The live contract suite (ADR-0057): the real pinned OpenCode **V1** binary
//! runs its actual agent loop against an in-process, scripted OpenAI-compatible
//! provider, in an isolated store.
//!
//! These tests are `#[ignore]`-gated, so `cargo test --workspace --locked`
//! stays hermetic and credential-free. Run the live suite with the pinned
//! 1.18.31 binary — the same fingerprint `.github/actions/install-opencode-v1`
//! installs for the `Live V1` CI job — via:
//!
//! ```text
//! COLA_LIVE_OPENCODE_BIN=/path/to/opencode cargo test --locked -- --ignored live
//! ```
//!
//! When the env var is unset, `opencode` on `PATH` is used. The harness
//! refuses a V2 binary and never touches the machine's default store: the
//! child server gets its own XDG trees under a temp dir and its config points
//! the only provider at the in-process script. No credentials or external
//! services are involved.
//!
//! The assertion is the capability chain, structurally: prompt → streamed
//! reasoning → tool call → permission round-trip → final text. Ids and
//! timestamps vary run to run; membership, order and content are asserted.

mod provider;
mod server;

use std::time::{Duration, Instant};

use serde_json::Value;
use tokio::time::sleep;

use crate::backend::{ContentBlock, MessageRole, Part, ToolStatus};
use crate::opencode::client::OpenCodeBackend;
use crate::opencode::types::SessionStatus;

use server::LiveServer;

/// The prompt text; asserted back from the user message.
const PROMPT_TEXT: &str = "run the live harness command";
/// How long any poll waits before failing the test.
const POLL_TIMEOUT: Duration = Duration::from_secs(30);

/// The binary under test: the pinned V1 path when set, else `opencode` on PATH.
fn live_binary() -> String {
    std::env::var("COLA_LIVE_OPENCODE_BIN").unwrap_or_else(|_| "opencode".to_string())
}

/// The whole chain against the pinned V1 server: create a session, prompt
/// through the real adapter, watch the permission ask, answer it, and assert
/// the transcript and the model requests the server actually made.
#[tokio::test]
#[ignore = "live: needs the pinned V1 binary (see the module docs)"]
async fn live_v1_scripted_capability_chain() {
    let binary = live_binary();
    server::ensure_v1_binary(&binary).await;

    let provider = provider::start().await;
    let server = LiveServer::start(&binary, &provider.base_url()).await;

    // The real adapter, pointed at the live child — its transport's env-proxy
    // workaround keeps the loopback traffic off a developer shell's proxy.
    let backend = OpenCodeBackend::with_base_url(
        Some(provider::MODEL_REF),
        server.base_url(),
        Some("opencode"),
        Some(server::PASSWORD),
    );
    backend.disable_env_proxy(Some("opencode"), Some(server::PASSWORD));

    wait_for_ready(&backend, &server).await;

    let work_dir = server.work_dir();
    let session = backend
        .create_session(&backend.new_session_input(Some(&work_dir)))
        .await
        .unwrap_or_else(|error| panic!("create session failed: {error}\n{}", server.stderr()));

    let message_id = format!("msg_cola_{}", uuid::Uuid::new_v4().simple());
    // The V1 prompt is synchronous and blocks inside the permission ask, so
    // the permission poll and the prompt run concurrently — exactly the shape
    // the Bridge's Turn is built around.
    let prompt = backend.prompt(&session.id, PROMPT_TEXT, &[], None, None, None, Some(&message_id));
    let answer = answer_permission(&backend, &work_dir, &session.id);

    let (prompt_result, permission) = tokio::join!(prompt, answer);
    let response =
        prompt_result.unwrap_or_else(|error| panic!("prompt failed: {error}\n{}", server.stderr()));
    assert!(
        response.error.is_none(),
        "the prompt must not report a model error: {:?}",
        response.error
    );
    assert_eq!(
        permission.session_id.as_deref(),
        Some(session.id.as_str()),
        "the ask must belong to the prompted session"
    );

    let transcript = backend
        .transcript(&session.id)
        .await
        .unwrap_or_else(|error| panic!("transcript read failed: {error}\n{}", server.stderr()));

    assert_user_anchor(&transcript, &message_id);
    assert_tool_call(&transcript);
    assert_streamed_reasoning(&transcript);
    assert_final_text(&transcript);
    assert_turn_complete(&transcript, &message_id);

    let status = wait_for_idle(&backend, &session.id, &work_dir).await;
    assert_eq!(status, SessionStatus::Idle, "a finished turn must read idle");

    assert_provider_requests(&provider.requests());
}

/// Poll `GET /session` until the freshly started child answers — the server
/// can accept TCP before its dispatch is wired (AGENTS.md pitfall #12).
async fn wait_for_ready(backend: &OpenCodeBackend, server: &LiveServer) {
    let deadline = Instant::now() + POLL_TIMEOUT;
    loop {
        match backend.list_sessions().await {
            Ok(_) => return,
            Err(_) if Instant::now() < deadline => sleep(Duration::from_millis(250)).await,
            Err(error) => panic!(
                "the child V1 server never became ready: {error}\n{}",
                server.stderr()
            ),
        }
    }
}

/// Wait for the tool permission ask and answer it with `once`, returning it.
async fn answer_permission(
    backend: &OpenCodeBackend,
    directory: &str,
    session_id: &str,
) -> crate::opencode::types::PermissionRequest {
    let deadline = Instant::now() + POLL_TIMEOUT;
    let mut last_error = None;
    let permission = loop {
        match backend.list_permissions(Some(directory)).await {
            Ok(pending) => {
                if let Some(permission) = pending.into_iter().next() {
                    break permission;
                }
            }
            Err(error) => last_error = Some(error),
        }
        assert!(
            Instant::now() < deadline,
            "no permission ask arrived for session {session_id} within {}s (last error: {last_error:?})",
            POLL_TIMEOUT.as_secs()
        );
        sleep(Duration::from_millis(150)).await;
    };
    assert_eq!(
        permission.permission.as_deref(),
        Some("bash"),
        "the scripted tool call must be gated as a bash ask: {permission:?}"
    );
    backend
        .reply_permission(&permission.request_id, "once", Some(directory))
        .await
        .expect("the permission reply must be accepted");
    permission
}

/// Poll until the server reports the session idle — the run state may clear a
/// beat after the synchronous prompt response.
async fn wait_for_idle(backend: &OpenCodeBackend, session_id: &str, directory: &str) -> SessionStatus {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if let Ok(Some(SessionStatus::Idle)) = backend.session_status(session_id, Some(directory)).await {
            return SessionStatus::Idle;
        }
        assert!(
            Instant::now() < deadline,
            "the session never read idle after the prompt returned"
        );
        sleep(Duration::from_millis(200)).await;
    }
}

/// The user message is present, under the cola-chosen id (ADR-0026), with the
/// prompt text verbatim.
fn assert_user_anchor(transcript: &crate::backend::SessionTranscript, message_id: &str) {
    let user = transcript
        .messages
        .iter()
        .find(|message| message.id.as_str() == message_id)
        .unwrap_or_else(|| panic!("the user message must be in the transcript: {transcript:#?}"));
    assert_eq!(user.role, MessageRole::User);
    assert_eq!(user.text(), PROMPT_TEXT);
    assert!(user.anchor().is_some(), "a live message carries a server time");
}

/// The `bash` tool call ran to completion with the scripted command's output.
fn assert_tool_call(transcript: &crate::backend::SessionTranscript) {
    let tool = transcript
        .messages
        .iter()
        .flat_map(|message| message.parts.iter())
        .find_map(|part| match part {
            Part::Tool(tool) if tool.identity.name == "bash" => Some(tool),
            _ => None,
        })
        .unwrap_or_else(|| panic!("the bash tool call must be in the transcript: {transcript:#?}"));
    assert_eq!(
        tool.status,
        ToolStatus::Completed,
        "the answered tool call must have completed: {tool:#?}"
    );
    assert!(
        tool.output
            .blocks
            .iter()
            .any(|block| matches!(block, ContentBlock::Text(text) if text.contains("live-harness-tool"))),
        "the command output must be in the tool result: {tool:#?}"
    );
}

/// Reasoning streamed as its own part, before the tool call it announced.
fn assert_streamed_reasoning(transcript: &crate::backend::SessionTranscript) {
    let message = transcript
        .messages
        .iter()
        .find(|message| message.parts.iter().any(|part| matches!(part, Part::Tool(_))))
        .unwrap_or_else(|| panic!("no assistant message carries the tool call: {transcript:#?}"));
    let reasoning_at = message
        .parts
        .iter()
        .position(|part| matches!(part, Part::Reasoning(reasoning) if reasoning.text.contains(provider::REASONING_TEXT)));
    let tool_at = message
        .parts
        .iter()
        .position(|part| matches!(part, Part::Tool(_)));
    match (reasoning_at, tool_at) {
        (Some(reasoning_at), Some(tool_at)) => assert!(
            reasoning_at < tool_at,
            "streamed reasoning must precede the tool call it announced: {message:#?}"
        ),
        _ => panic!("the reasoning and tool parts must both be present: {message:#?}"),
    }
}

/// The closing text streamed after the tool result.
fn assert_final_text(transcript: &crate::backend::SessionTranscript) {
    assert!(
        transcript
            .messages
            .iter()
            .any(|message| message.role == MessageRole::Assistant
                && message.text().contains(provider::FINAL_TEXT)),
        "the final text must be in the transcript: {transcript:#?}"
    );
}

/// The neutral Turn view derives completion from the transcript structurally.
fn assert_turn_complete(transcript: &crate::backend::SessionTranscript, message_id: &str) {
    let user = transcript
        .messages
        .iter()
        .find(|message| message.id.as_str() == message_id)
        .unwrap();
    let anchor = user.anchor().expect("the user message carries a server time");
    let turn = transcript.turn_for_user(&anchor);
    assert!(
        turn.complete,
        "the turn anchored on the user message must be complete: {turn:#?}"
    );
    assert!(
        !turn.messages.is_empty(),
        "the turn must contain the assistant messages: {transcript:#?}"
    );
}

/// The scripted provider's recorded requests: the turn offered tools, and the
/// server came back with the executed tool result.
fn assert_provider_requests(requests: &[crate::test_http::RecordedRequest]) {
    let calls: Vec<Value> = requests
        .iter()
        .filter(|request| request.path == "/v1/chat/completions")
        .map(|request| serde_json::from_str(&request.body).expect("a provider request body must be JSON"))
        .collect();
    assert!(
        calls.len() >= 2,
        "the server must call the model at least twice (tool call + close): {calls:#?}"
    );

    let main = calls
        .iter()
        .find(|call| provider::offers_tools(call) && !provider::has_tool_result(call))
        .unwrap_or_else(|| panic!("no first-turn model call with tools: {calls:#?}"));
    assert!(
        main["tools"]
            .as_array()
            .is_some_and(|tools| tools.iter().any(|tool| tool["function"]["name"] == "bash")),
        "the bash tool schema must be offered: {main:#?}"
    );

    let followup = calls
        .iter()
        .find(|call| provider::has_tool_result(call))
        .unwrap_or_else(|| panic!("the server must send the executed tool result back: {calls:#?}"));
    assert!(
        followup.to_string().contains("live-harness-tool"),
        "the command's output must ride the tool result: {followup:#?}"
    );
}
