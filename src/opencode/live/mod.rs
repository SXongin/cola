//! The live contract suite (ADR-0057): the real pinned OpenCode binary runs
//! its actual agent loop against an in-process, scripted OpenAI-compatible
//! provider, in an isolated store.
//!
//! Two generations share the harness: the **V1** capability chain
//! (`live_v1_scripted_capability_chain`: prompt → streamed reasoning → tool
//! call → permission round-trip → final text) and the **V2** chains — the
//! transcript read with the production prompt (`live_v2_scripted_transcript_read`:
//! the blocking prompt runs while the turn streams, then the decoded transcript
//! and its projections are asserted) and the write surface
//! (`live_v2_scripted_write_chain`: supplement steering, retry idempotency,
//! interrupt, compact and title rename). Each test spawns the binary of its own
//! generation into its own temp store, so neither can touch the machine's
//! default store, credentials or config; each refuses a binary of the other
//! generation.
//!
//! These tests are `#[ignore]`-gated, so `cargo test --workspace --locked`
//! stays hermetic and credential-free. Run one generation with the pinned
//! binary:
//!
//! ```text
//! COLA_LIVE_OPENCODE_BIN=/path/to/opencode cargo test --locked -- --ignored live_v1
//! COLA_LIVE_OPENCODE_V2_BIN=/path/to/opencode cargo test --locked -- --ignored live_v2
//! ```
//!
//! When an env var is unset, `opencode` (`opencode2` for V2) on `PATH` is used.
//! The V2 binary is the 2.x CLI; the runbook drives it through the
//! `opencode-v2` wrapper for a service, but a plain isolated `serve` is what
//! the harness needs (and is what keeps the default store untouched).
//!
//! The assertion is structural: ids and timestamps vary run to run; membership,
//! order and content are asserted. Both V2 chains drive the production adapter:
//! the prompt goes through the strategy's blocking polyfill (`session.wait`
//! plus the poll fallback), never a raw protocol admit.
//!
//! ## Re-recording the fixture corpus
//!
//! With `COLA_LIVE_CAPTURE_DIR` set, the tests write the raw transcript read
//! they performed — stamped with the server version and the reproducing
//! command — under `<dir>/<name>.json`. Sanitize ids/times/cursors and commit
//! the result under `src/opencode/wire/fixtures/` (spec #364, "Testing
//! Decisions"); no cassette or replay engine is involved.

mod provider;
mod server;

use std::time::{Duration, Instant};

use serde_json::Value;
use tokio::time::sleep;

use crate::backend::{ContentBlock, MessageRole, Part, SessionTranscript, ToolStatus};
use crate::opencode::client::OpenCodeBackend;
use crate::opencode::strategy::Generation;
use crate::opencode::types::SessionStatus;

use server::LiveServer;

/// The prompt text; asserted back from the user message.
const PROMPT_TEXT: &str = "run the live harness command";
/// The supplement text steered into the running V2 turn.
const SUPPLEMENT_TEXT: &str = "also note the supplement";
/// The title the V2 write chain renames the session to.
const RENAMED_TITLE: &str = "live v2 renamed title";
/// How long the readiness and permission polls wait before failing the test.
const POLL_TIMEOUT: Duration = Duration::from_secs(30);
/// How long the post-prompt idle wait gets — the run state clears a beat after
/// the synchronous response, so this is deliberately shorter.
const IDLE_TIMEOUT: Duration = Duration::from_secs(10);
/// The gap between polls of a live read.
const POLL_INTERVAL: Duration = Duration::from_millis(150);

/// The V1 binary under test: the pinned path when set, else `opencode` on PATH.
fn live_v1_binary() -> String {
    std::env::var("COLA_LIVE_OPENCODE_BIN").unwrap_or_else(|_| "opencode".to_string())
}

/// The V2 binary under test: the env override, else `opencode2` — the bin name
/// the official `@opencode/cli` 2.x package installs (the wrapper's direct
/// binary is the usual local override).
fn live_v2_binary() -> String {
    std::env::var("COLA_LIVE_OPENCODE_V2_BIN").unwrap_or_else(|_| "opencode2".to_string())
}

/// The whole chain against the pinned V1 server: create a session, prompt
/// through the real adapter, watch the permission ask, answer it, and assert
/// the transcript and the model requests the server actually made.
#[tokio::test]
#[ignore = "live: needs the pinned V1 binary (see the module docs)"]
async fn live_v1_scripted_capability_chain() {
    let binary = live_v1_binary();
    let version = server::ensure_v1_binary(&binary).await;
    eprintln!("live V1 binary: {binary} ({version})");

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
    let answer = answer_permission(&backend, &work_dir, &session.id, &server);

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
    assert_tool_call(&transcript, provider::Tool::Bash, provider::ToolCommand::Fast);
    assert_streamed_reasoning(&transcript);
    assert_final_text(&transcript);
    assert_turn_complete(&transcript, &message_id);

    capture_fixture(
        "v1/transcript_turn",
        "v1",
        &version,
        &capture_command(
            "COLA_LIVE_OPENCODE_BIN",
            &binary,
            "live_v1_scripted_capability_chain",
        ),
        &format!("{}/session/{}/message", server.base_url(), session.id),
    )
    .await;

    // The finished turn reads idle once the run state clears.
    wait_for_idle(&backend, &session.id, &work_dir, &server).await;

    assert_provider_requests(&provider.requests());
}

/// The V2 read chain against the pinned V2 server: prompt through the
/// production adapter (the blocking polyfill holds while the turn streams),
/// watch the transcript mid-turn and at rest, and assert the neutral
/// projections the Session Snapshot tail, the external-message sync and the
/// follow renderers consume — no duplicated text, correct turn anchoring.
#[tokio::test]
#[ignore = "live: needs a V2 binary (see the module docs)"]
async fn live_v2_scripted_transcript_read() {
    let binary = live_v2_binary();
    let version = server::ensure_v2_binary(&binary).await;
    eprintln!("live V2 binary: {binary} ({version})");

    let provider = provider::start_with(provider::Tool::Shell, provider::ToolCommand::Slow).await;
    let server = LiveServer::start_v2(&binary, &provider.base_url()).await;

    // The real adapter, pointed at the live child — its transport's env-proxy
    // workaround keeps the loopback traffic off a developer shell's proxy.
    let backend = OpenCodeBackend::with_generation(
        Some(provider::MODEL_REF),
        server.base_url(),
        Some("opencode"),
        Some(server::PASSWORD),
        Generation::V2,
        None,
    );
    backend.disable_env_proxy(Some("opencode"), Some(server::PASSWORD));

    wait_for_ready(&backend, &server).await;

    let work_dir = server.work_dir();
    let session = backend
        .create_session(&backend.new_session_input(Some(&work_dir)))
        .await
        .unwrap_or_else(|error| panic!("create session failed: {error}\n{}", server.stderr()));

    let message_id = format!("msg_cola_{}", uuid::Uuid::new_v4().simple());
    // The V2 prompt is admit-then-wait, and the polyfill blocks until the run is
    // idle, so the prompt runs concurrently with the in-flight read below —
    // exactly the shape the Bridge's Turn is built around.
    let prompt_backend = backend.clone();
    let prompt_session = session.id.clone();
    let prompt_message_id = message_id.clone();
    let prompt = tokio::spawn(async move {
        prompt_backend
            .prompt(
                &prompt_session,
                PROMPT_TEXT,
                &[],
                None,
                None,
                None,
                Some(&prompt_message_id),
            )
            .await
    });

    // Mid-turn: the projected assistant message is one row that carries its
    // content while it streams, so the first read that sees a live tool captures
    // V2's in-flight shape (no completion stamp, a running tool) for real.
    let url = format!(
        "{}/api/session/{}/message?order=asc&limit=200",
        server.base_url(),
        session.id
    );
    let inflight = poll_until(
        "the live V2 turn's in-flight transcript",
        POLL_TIMEOUT,
        || async {
            let transcript = backend.transcript(&session.id).await?;
            Ok(has_live_tool(&transcript).then_some(transcript))
        },
        || server.stderr(),
    )
    .await;
    assert_user_anchor(&inflight, &message_id);
    let anchor = inflight
        .newest_user()
        .unwrap()
        .anchor()
        .expect("the user message anchors");
    assert!(
        !inflight.turn_for_user(&anchor).complete,
        "a turn with only a live tool is not complete: {inflight:#?}"
    );
    capture_fixture(
        "v2/transcript_inflight",
        "v2",
        &version,
        &capture_command(
            "COLA_LIVE_OPENCODE_V2_BIN",
            &binary,
            "live_v2_scripted_transcript_read",
        ),
        &url,
    )
    .await;

    // The production prompt's blocking wait returns with the turn's reply: the
    // polyfill's decoded parts carry the closing text exactly once.
    let response = prompt
        .await
        .expect("the prompt task must not panic")
        .unwrap_or_else(|error| panic!("prompt failed: {error}\n{}", server.stderr()));
    assert!(
        response.error.is_none(),
        "the prompt must not report a model error: {:?}",
        response.error
    );
    assert_eq!(
        response.parent_id.as_deref(),
        Some(message_id.as_str()),
        "the polyfilled response answers the admitted message"
    );
    assert_eq!(
        response
            .parts
            .iter()
            .filter(|part| matches!(part, Part::Text(text) if text.text == provider::FINAL_TEXT))
            .count(),
        1,
        "the closing deltas assemble into exactly one part on the response: {:#?}",
        response.parts
    );

    // At rest: the terminal finish ends the turn and every marker is present
    // exactly once.
    let transcript = poll_until(
        "the live V2 turn to complete",
        POLL_TIMEOUT,
        || async {
            let transcript = backend.transcript(&session.id).await?;
            let Some(anchor) = transcript.newest_user().and_then(|message| message.anchor()) else {
                return Ok(None);
            };
            let complete = transcript.turn_for_user(&anchor).complete;
            Ok(complete.then_some(transcript))
        },
        || server.stderr(),
    )
    .await;

    assert_user_anchor(&transcript, &message_id);
    assert_tool_call(&transcript, provider::Tool::Shell, provider::ToolCommand::Slow);
    assert_streamed_reasoning(&transcript);
    assert_final_text(&transcript);
    assert_turn_complete(&transcript, &message_id);
    // Authorship survives V2's tighter message-id rule: the external-message
    // sync's `msg_cola_` check still recognises cola's own turn.
    let user = transcript
        .messages
        .iter()
        .find(|message| message.id.as_str() == message_id)
        .expect("the admitted user message is in the transcript");
    assert!(
        crate::opencode::parsing::is_cola_message_id(user.id.as_str()),
        "V2 must persist the cola-chosen message id: {user:#?}"
    );
    // The Session Snapshot tail: the newest text-bearing conversation, with the
    // tool/reasoning-only steps excluded and nothing duplicated.
    let tail = transcript.transcript_tail();
    assert_eq!(
        tail.last().map(|entry| entry.text.as_str()),
        Some(provider::FINAL_TEXT),
        "the tail ends on the final answer: {tail:#?}"
    );
    assert_eq!(
        tail.iter()
            .filter(|entry| entry.text == provider::FINAL_TEXT)
            .count(),
        1,
        "the final text renders once in the tail: {tail:#?}"
    );

    capture_fixture(
        "v2/transcript_turn",
        "v2",
        &version,
        &capture_command(
            "COLA_LIVE_OPENCODE_V2_BIN",
            &binary,
            "live_v2_scripted_transcript_read",
        ),
        &url,
    )
    .await;

    // The finished turn reads idle once the run state clears (S4a's read).
    wait_for_idle(&backend, &session.id, &work_dir, &server).await;

    // The scripted provider saw the shell tool offered and its result returned.
    let calls: Vec<Value> = provider
        .requests()
        .iter()
        .filter(|request| request.path == "/v1/chat/completions")
        .map(|request| serde_json::from_str(&request.body).expect("a provider request body must be JSON"))
        .collect();
    assert!(
        calls
            .iter()
            .any(|call| provider::offers_tools(call) && call.to_string().contains("\"shell\"")),
        "the shell tool schema must be offered: {calls:#?}"
    );
    assert!(
        calls.iter().any(provider::has_tool_result),
        "the executed tool result must come back: {calls:#?}"
    );
}

/// The V2 write surface against the pinned V2 server: the production prompt
/// (blocking polyfill) with a mid-turn Supplement steered into the running
/// turn, the cola-chosen id persisted and retry-idempotent, then interrupt,
/// compact and the title rename.
#[tokio::test]
#[ignore = "live: needs a V2 binary (see the module docs)"]
async fn live_v2_scripted_write_chain() {
    let binary = live_v2_binary();
    let version = server::ensure_v2_binary(&binary).await;
    eprintln!("live V2 binary: {binary} ({version})");

    let provider = provider::start_with(provider::Tool::Shell, provider::ToolCommand::Slow).await;
    let server = LiveServer::start_v2(&binary, &provider.base_url()).await;

    let backend = OpenCodeBackend::with_generation(
        Some(provider::MODEL_REF),
        server.base_url(),
        Some("opencode"),
        Some(server::PASSWORD),
        Generation::V2,
        None,
    );
    backend.disable_env_proxy(Some("opencode"), Some(server::PASSWORD));

    wait_for_ready(&backend, &server).await;

    let work_dir = server.work_dir();
    let session = backend
        .create_session(&backend.new_session_input(Some(&work_dir)))
        .await
        .unwrap_or_else(|error| panic!("create session failed: {error}\n{}", server.stderr()));

    // The prompt blocks inside the polyfill; the supplement is steered in while
    // the scripted slow tool runs, so the running turn must merge it.
    let message_id = format!("msg_cola_{}", uuid::Uuid::new_v4().simple());
    let supplement_id = format!("msg_cola_{}", uuid::Uuid::new_v4().simple());
    let prompt_backend = backend.clone();
    let prompt_session = session.id.clone();
    let prompt_message_id = message_id.clone();
    let prompt = tokio::spawn(async move {
        prompt_backend
            .prompt(
                &prompt_session,
                PROMPT_TEXT,
                &[],
                None,
                None,
                None,
                Some(&prompt_message_id),
            )
            .await
    });
    poll_until(
        "the live tool so the supplement can steer mid-turn",
        POLL_TIMEOUT,
        || async {
            let transcript = backend.transcript(&session.id).await?;
            Ok(has_live_tool(&transcript).then_some(()))
        },
        || server.stderr(),
    )
    .await;
    backend
        .prompt_async(
            &session.id,
            SUPPLEMENT_TEXT,
            &[],
            None,
            None,
            None,
            Some(&supplement_id),
        )
        .await
        .unwrap_or_else(|error| panic!("supplement admit failed: {error}\n{}", server.stderr()));

    let response = prompt
        .await
        .expect("the prompt task must not panic")
        .unwrap_or_else(|error| panic!("prompt failed: {error}\n{}", server.stderr()));
    assert!(
        response.error.is_none(),
        "the supplemented turn must not report a model error: {:?}",
        response.error
    );

    let transcript = poll_until(
        "the supplemented V2 turn to complete",
        POLL_TIMEOUT,
        || async {
            let transcript = backend.transcript(&session.id).await?;
            let Some(anchor) = transcript
                .messages
                .iter()
                .find(|message| message.id.as_str() == message_id)
                .and_then(|message| message.anchor())
            else {
                return Ok(None);
            };
            Ok(transcript.turn_for_user(&anchor).complete.then_some(transcript))
        },
        || server.stderr(),
    )
    .await;

    // The supplement merged into the same turn: both user messages carry their
    // cola ids, and the turn closed on the scripted final text.
    assert_user_anchor(&transcript, &message_id);
    let supplement = transcript
        .messages
        .iter()
        .find(|message| message.id.as_str() == supplement_id)
        .unwrap_or_else(|| panic!("the steered supplement must be in the transcript: {transcript:#?}"));
    assert_eq!(
        supplement.role,
        MessageRole::User,
        "the supplement is a user message"
    );
    assert_eq!(supplement.text(), SUPPLEMENT_TEXT);
    assert!(
        crate::opencode::parsing::is_cola_message_id(supplement.id.as_str()),
        "V2 must persist the supplement's cola-chosen id: {supplement:#?}"
    );
    assert_tool_call(&transcript, provider::Tool::Shell, provider::ToolCommand::Slow);
    assert_streamed_reasoning(&transcript);
    assert_final_text(&transcript);
    assert_turn_complete(&transcript, &message_id);

    // Retry idempotency: the same cola id reconciles onto the durable message
    // instead of admitting a second user message or running a second turn.
    let retry = backend
        .prompt(&session.id, PROMPT_TEXT, &[], None, None, None, Some(&message_id))
        .await
        .unwrap_or_else(|error| panic!("retry failed: {error}\n{}", server.stderr()));
    assert!(retry.error.is_none());
    let transcript = backend
        .transcript(&session.id)
        .await
        .unwrap_or_else(|error| panic!("transcript read failed: {error}\n{}", server.stderr()));
    assert_eq!(
        transcript
            .messages
            .iter()
            .filter(|message| message.id.as_str() == message_id)
            .count(),
        1,
        "a retry with the same id must not duplicate the user message: {transcript:#?}"
    );
    assert_eq!(
        transcript
            .messages
            .iter()
            .filter(|message| message.id.as_str() == supplement_id)
            .count(),
        1,
        "the retry must not disturb the supplement: {transcript:#?}"
    );

    // Interrupt: a fresh slow turn is stopped mid-flight; the blocking prompt
    // unblocks, the interrupt answers as an accepted op, and the session idles.
    // A fresh session keeps the scripted provider's first call a tool call (the
    // transcript above already carries a tool result).
    let interrupt_session = backend
        .create_session(&backend.new_session_input(Some(&work_dir)))
        .await
        .unwrap_or_else(|error| panic!("create interrupt session failed: {error}\n{}", server.stderr()));
    let interrupt_id = format!("msg_cola_{}", uuid::Uuid::new_v4().simple());
    let prompt_backend = backend.clone();
    let prompt_session = interrupt_session.id.clone();
    let interrupt_message_id = interrupt_id.clone();
    let interrupted = tokio::spawn(async move {
        prompt_backend
            .prompt(
                &prompt_session,
                PROMPT_TEXT,
                &[],
                None,
                None,
                None,
                Some(&interrupt_message_id),
            )
            .await
    });
    poll_until(
        "the interrupt turn's live tool",
        POLL_TIMEOUT,
        || async {
            let transcript = backend.transcript(&interrupt_session.id).await?;
            Ok(has_live_tool(&transcript).then_some(()))
        },
        || server.stderr(),
    )
    .await;
    backend
        .interrupt(&interrupt_session.id)
        .await
        .unwrap_or_else(|error| panic!("interrupt failed: {error}\n{}", server.stderr()));
    let interrupted = tokio::time::timeout(POLL_TIMEOUT, interrupted)
        .await
        .unwrap_or_else(|_| panic!("the interrupted prompt must return\n{}", server.stderr()))
        .expect("the prompt task must not panic");
    assert!(
        interrupted.is_ok(),
        "the interrupted prompt must complete, not fail: {interrupted:?}"
    );
    wait_for_idle(&backend, &interrupt_session.id, &work_dir, &server).await;

    // Compact and the title rename: V2's compact needs its empty payload, and
    // the 204 title patch must persist for the session surfaces.
    backend
        .compact(&session.id)
        .await
        .unwrap_or_else(|error| panic!("compact failed: {error}\n{}", server.stderr()));
    backend
        .update_session_title(&session.id, RENAMED_TITLE)
        .await
        .unwrap_or_else(|error| panic!("title rename failed: {error}\n{}", server.stderr()));
    let info = backend
        .session_info(&session.id, Some(&work_dir))
        .await
        .unwrap_or_else(|error| panic!("session info failed: {error}\n{}", server.stderr()));
    assert_eq!(
        info.title.as_deref(),
        Some(RENAMED_TITLE),
        "V2's 204 title patch must persist"
    );
}

/// Whether any decoded part of the transcript is a still-live tool call.
fn has_live_tool(transcript: &SessionTranscript) -> bool {
    transcript
        .messages
        .iter()
        .flat_map(|message| message.parts.iter())
        .any(|part| matches!(part, Part::Tool(call) if call.status.is_live()))
}

/// The reproducing command stamped into a captured fixture.
fn capture_command(binary_env: &str, binary: &str, test: &str) -> String {
    format!("{binary_env}={binary} COLA_LIVE_CAPTURE_DIR=<dir> cargo test --locked -- --ignored {test}")
}

/// Re-record one transcript fixture (see the module docs): with
/// `COLA_LIVE_CAPTURE_DIR` set, write the raw read `url` returns, stamped with
/// the generation, the server version that produced it and the reproducing
/// command. The committed corpus is sanitized by hand afterwards — this helper
/// records reality, it does not sanitize it.
async fn capture_fixture(name: &str, generation: &str, version: &str, command: &str, url: &str) {
    let Ok(dir) = std::env::var("COLA_LIVE_CAPTURE_DIR") else {
        return;
    };
    let response = crate::test_http::no_proxy_transport()
        .get(url)
        .basic_auth("opencode", Some(server::PASSWORD))
        .send()
        .await
        .unwrap_or_else(|error| panic!("capture {name}: request failed: {error}"));
    let status = response.status();
    let body: Value = response
        .json()
        .await
        .unwrap_or_else(|error| panic!("capture {name}: the response body is not JSON: {error}"));
    assert!(status.is_success(), "capture {name}: HTTP {status}");
    let recorded = serde_json::json!({
        "capture": {
            "generation": generation,
            "source": format!("opencode {}", version.trim_start_matches("opencode ")),
            "command": command,
            "captured_at": chrono::Utc::now().format("%Y-%m-%d").to_string(),
        },
        "response": body,
    });
    let path = std::path::Path::new(&dir).join(format!("{name}.json"));
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).expect("create the capture directory");
    }
    std::fs::write(
        &path,
        serde_json::to_string_pretty(&recorded).expect("serialize the recorded fixture") + "\n",
    )
    .expect("write the recorded fixture");
    eprintln!("captured fixture: {}", path.display());
}

/// Poll `read` until it yields a value, failing the test at `timeout`.
/// `diagnostics` is appended to the timeout panic — the child's stderr, so a
/// hung poll is debuggable from the failure alone — and the last read error is
/// always reported, so a persistent failure is never masked by a bare timeout.
async fn poll_until<T, F, Fut>(
    what: &str,
    timeout: Duration,
    mut read: F,
    diagnostics: impl Fn() -> String,
) -> T
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = crate::error::Result<Option<T>>>,
{
    let deadline = Instant::now() + timeout;
    let mut last_error = None;
    loop {
        match read().await {
            Ok(Some(value)) => return value,
            Ok(None) => {}
            Err(error) => last_error = Some(error),
        }
        assert!(
            Instant::now() < deadline,
            "timed out after {}s waiting for {what} (last error: {last_error:?})\n{}",
            timeout.as_secs(),
            diagnostics()
        );
        sleep(POLL_INTERVAL).await;
    }
}

/// Poll `GET /session` until the freshly started child answers — the server
/// can accept TCP before its dispatch is wired (AGENTS.md pitfall #12).
async fn wait_for_ready(backend: &OpenCodeBackend, server: &LiveServer) {
    poll_until(
        "the child V1 server to answer",
        POLL_TIMEOUT,
        || async { backend.list_sessions().await.map(|_| Some(())) },
        || server.stderr(),
    )
    .await;
}

/// Wait for the tool permission ask and answer it with `once`, returning it.
async fn answer_permission(
    backend: &OpenCodeBackend,
    directory: &str,
    session_id: &str,
    server: &LiveServer,
) -> crate::opencode::types::PermissionRequest {
    let what = format!("a permission ask for session {session_id}");
    let permission = poll_until(
        &what,
        POLL_TIMEOUT,
        || async {
            backend
                .list_permissions(Some(directory))
                .await
                .map(|pending| pending.into_iter().next())
        },
        || server.stderr(),
    )
    .await;
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

/// Wait until the server reports the session idle — the run state may clear a
/// beat after the synchronous prompt response.
async fn wait_for_idle(backend: &OpenCodeBackend, session_id: &str, directory: &str, server: &LiveServer) {
    poll_until(
        "the session to read idle",
        IDLE_TIMEOUT,
        || async {
            backend
                .session_status(session_id, Some(directory))
                .await
                .map(|status| status.filter(|status| *status == SessionStatus::Idle).map(|_| ()))
        },
        || server.stderr(),
    )
    .await;
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

/// The generation's shell tool call ran to completion with the scripted
/// command's output.
fn assert_tool_call(
    transcript: &crate::backend::SessionTranscript,
    shell: provider::Tool,
    command: provider::ToolCommand,
) {
    let tool = transcript
        .messages
        .iter()
        .flat_map(|message| message.parts.iter())
        .find_map(|part| match part {
            Part::Tool(tool) if tool.identity.name == shell.name() => Some(tool),
            _ => None,
        })
        .unwrap_or_else(|| {
            panic!(
                "the {} tool call must be in the transcript: {transcript:#?}",
                shell.name()
            )
        });
    assert_eq!(
        tool.status,
        ToolStatus::Completed,
        "the answered tool call must have completed: {tool:#?}"
    );
    assert_eq!(
        tool.input
            .as_ref()
            .and_then(|input| input.get("command"))
            .and_then(Value::as_str),
        Some(command.text()),
        "the split argument deltas must reassemble into the scripted command: {tool:#?}"
    );
    assert!(
        tool.output
            .blocks
            .iter()
            .any(|block| matches!(block, ContentBlock::Text(text) if text.contains("live-harness-tool"))),
        "the command output must be in the tool result: {tool:#?}"
    );
}

/// Reasoning streamed as its own part, before the tool call it announced, and
/// assembled exactly once from the deltas (no duplicate render).
fn assert_streamed_reasoning(transcript: &crate::backend::SessionTranscript) {
    let message = transcript
        .messages
        .iter()
        .find(|message| message.parts.iter().any(|part| matches!(part, Part::Tool(_))))
        .unwrap_or_else(|| panic!("no assistant message carries the tool call: {transcript:#?}"));
    let reasoning: Vec<&str> = message
        .parts
        .iter()
        .filter_map(|part| match part {
            Part::Reasoning(reasoning) => Some(reasoning.text.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(
        reasoning,
        vec![provider::REASONING_TEXT],
        "the reasoning deltas must assemble into exactly one part: {message:#?}"
    );
    let tool_at = message
        .parts
        .iter()
        .position(|part| matches!(part, Part::Tool(_)))
        .expect("the tool part is present");
    assert!(
        message
            .parts
            .iter()
            .position(|part| matches!(part, Part::Reasoning(_)))
            .unwrap()
            < tool_at,
        "streamed reasoning must precede the tool call it announced: {message:#?}"
    );
}

/// The closing text streamed after the tool result: the deltas assemble into
/// exactly one part equal to the scripted text (no double render).
fn assert_final_text(transcript: &crate::backend::SessionTranscript) {
    let texts: Vec<&str> = transcript
        .messages
        .iter()
        .filter(|message| message.role == MessageRole::Assistant)
        .flat_map(|message| message.parts.iter())
        .filter_map(|part| match part {
            Part::Text(text) => Some(text.text.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(
        texts.iter().filter(|text| **text == provider::FINAL_TEXT).count(),
        1,
        "the closing deltas must assemble into exactly one part: {texts:#?}"
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
