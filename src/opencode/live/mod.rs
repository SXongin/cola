//! The live contract suite (ADR-0057): the real pinned OpenCode binary runs
//! its actual agent loop against an in-process, scripted OpenAI-compatible
//! provider, in an isolated store.
//!
//! Two generations share the harness: the **V1** capability chain
//! (`live_v1_scripted_capability_chain`: prompt → streamed reasoning → tool
//! call → permission round-trip → final text) and the **V2** chains — the
//! transcript read with the production prompt (`live_v2_scripted_transcript_read`:
//! the admit-then-return submit comes back while the turn streams, and the
//! decoded transcript and its projections are asserted at rest), the write
//! surface (`live_v2_scripted_write_chain`: supplement steering, retry
//! idempotency, interrupt, compact and title rename), the permission round-trip
//! (`live_v2_scripted_permission_chain`: a gated shell tool answered through the
//! session-scoped decision), the form round-trip
//! (`live_v2_scripted_form_chain`: a typed question form answered with a keyed
//! value, plus a cancellation by delete), the session-scoped selection
//! (`live_v2_scripted_selection_chain`: durable model/agent/variant switches,
//! the next turn running them with nothing re-sent, and the coupling to the
//! bridge's ADR-0020 clear rule) and the retry-id contract
//! (`live_v2_scripted_retry_id_chain`: the `msg_cola_` id as the server's
//! admission key — a re-post is a no-op, a new id runs, an aborted message's
//! shape, and an unfinished run the server died in). Each test spawns the
//! binary of its own generation into its own temp store, so neither can touch
//! the machine's default store, credentials or config; each refuses a binary of
//! the other generation.
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
//! order and content are asserted. Every V2 chain drives the production adapter:
//! the prompt goes through the strategy's native admit-then-return submit and
//! completion is observed from the transcript + run state, never a raw protocol
//! read the Bridge would not perform (ADR-0056). The retry-id contract chain is
//! the one deliberate exception: it also reads the raw wire rows, because that
//! chain pins the row SHAPES the retry id policy rests on — an aborted
//! `finish`/`error`, a tool still `running`, the appended idle event, and the
//! byte-identical row a same-id re-post must leave behind — and the neutral
//! transcript folds those fields away. Those reads are observations only; the
//! submits under test still go through the production adapter.
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
    // The V1 submit is the native fire-and-forget prompt: it returns once the
    // message is durable and a run is forked, so the permission poll below runs
    // concurrently with the live turn — exactly the shape the Bridge's Turn is
    // built around (ADR-0056).
    let prompt = backend.prompt(&session.id, PROMPT_TEXT, &[], None, None, None, Some(&message_id));
    let answer = answer_permission(&backend, &work_dir, &session.id, &server);

    let (prompt_result, permission) = tokio::join!(prompt, answer);
    prompt_result.unwrap_or_else(|error| panic!("prompt submit failed: {error}\n{}", server.stderr()));
    assert_eq!(
        permission.session_id.as_deref(),
        Some(session.id.as_str()),
        "the ask must belong to the prompted session"
    );

    // The submitted turn settles through transcript + run-state observation.
    let transcript = wait_for_turn(&backend, &session.id, &message_id, &server).await;
    assert!(
        turn_error(&transcript, &message_id).is_none(),
        "the submitted turn must not record a model error: {:?}",
        turn_error(&transcript, &message_id)
    );

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

    // Retry idempotency: re-submitting the same cola id upserts the one user
    // message and runs no second turn — the settled assistant already carries a
    // terminal finish, so the server's loop exits without another model call.
    // This is the V1 counterpart of the V2 write chain's assertion (ADR-0026).
    let assistants_before = transcript
        .messages
        .iter()
        .filter(|message| message.role == MessageRole::Assistant)
        .count();
    let completions_before = provider_turn_completions(&provider);
    backend
        .prompt(&session.id, PROMPT_TEXT, &[], None, None, None, Some(&message_id))
        .await
        .unwrap_or_else(|error| panic!("retry submit failed: {error}\n{}", server.stderr()));
    wait_for_idle(&backend, &session.id, &work_dir, &server).await;
    let retried = backend
        .transcript(&session.id)
        .await
        .unwrap_or_else(|error| panic!("transcript read failed: {error}\n{}", server.stderr()));
    assert_eq!(
        retried
            .messages
            .iter()
            .filter(|message| message.id.as_str() == message_id)
            .count(),
        1,
        "a retry with the same id must not duplicate the user message: {retried:#?}"
    );
    assert_eq!(
        retried
            .messages
            .iter()
            .filter(|message| message.role == MessageRole::Assistant)
            .count(),
        assistants_before,
        "the retry must not run a second turn: {retried:#?}"
    );
    assert_eq!(
        provider_turn_completions(&provider),
        completions_before,
        "the retry must not call the model again"
    );

    assert_provider_requests(&provider.requests());
}

/// The V1 failure chain against the pinned binary: a scripted provider error
/// must project into the transcript's assistant `error`, and re-submitting the
/// SAME `msg_cola_` id after the failure must retry onto the one user message —
/// the retried step recovers, and its clean newest message is what clears the
/// failure (a recovered earlier step is not a failure). Without the projection
/// the card would silently ship Done on a failed turn.
#[tokio::test]
#[ignore = "live: needs the pinned V1 binary (see the module docs)"]
async fn live_v1_scripted_failure_and_retry_chain() {
    let binary = live_v1_binary();
    let version = server::ensure_v1_binary(&binary).await;
    eprintln!("live V1 binary: {binary} ({version})");

    let provider = provider::start_failing_once(provider::Tool::Bash, provider::ToolCommand::Fast).await;
    let server = LiveServer::start(&binary, &provider.base_url()).await;

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
    backend
        .prompt(&session.id, PROMPT_TEXT, &[], None, None, None, Some(&message_id))
        .await
        .unwrap_or_else(|error| panic!("prompt submit failed: {error}\n{}", server.stderr()));

    // The turn's failure must be OBSERVABLE: the assistant message carries the
    // scripted provider error, and the turn never reads complete (a failed run
    // has no terminal step finish). This is what the Turn's Error card rests on.
    let failed = wait_for_turn(&backend, &session.id, &message_id, &server).await;
    let error = turn_error(&failed, &message_id)
        .unwrap_or_else(|| panic!("the provider failure must project into the transcript: {failed:#?}"));
    assert!(
        error.contains(provider::FAILURE_TEXT),
        "the transcript must carry the provider's failure message ({error}); transcript: {failed:#?}"
    );
    let anchor = failed
        .anchor_of_user(&message_id)
        .expect("the admitted message anchors the failed turn");
    assert!(
        !failed.turn_for_user(&anchor).complete,
        "a failed turn has no terminal finish: {failed:#?}"
    );

    // Retry with the SAME id (the error-card retry shape): the server upserts
    // the one user message and, because the failed assistant has no finish,
    // runs a new step. The retried step calls the scripted tool, so answer its
    // ask; the turn then completes cleanly.
    backend
        .prompt(&session.id, PROMPT_TEXT, &[], None, None, None, Some(&message_id))
        .await
        .unwrap_or_else(|error| panic!("retry submit failed: {error}\n{}", server.stderr()));
    answer_permission(&backend, &work_dir, &session.id, &server).await;

    let retried = poll_until(
        "the retried V1 turn to complete",
        POLL_TIMEOUT,
        || async {
            let transcript = backend.transcript(&session.id).await?;
            let Some(anchor) = transcript.anchor_of_user(&message_id) else {
                return Ok(None);
            };
            Ok(transcript.turn_for_user(&anchor).complete.then_some(transcript))
        },
        || server.stderr(),
    )
    .await;

    let anchor = retried
        .anchor_of_user(&message_id)
        .expect("the retried turn keeps the one user anchor");
    let turn = retried.turn_for_user(&anchor);
    assert_eq!(
        turn.error, None,
        "the recovered retry's newest message is clean: {retried:#?}"
    );
    assert_eq!(
        retried
            .messages
            .iter()
            .filter(|message| message.id.as_str() == message_id)
            .count(),
        1,
        "the retry must upsert the one user message: {retried:#?}"
    );
    assert_final_text(&retried);
    assert_turn_complete(&retried, &message_id);

    capture_fixture(
        "v1/transcript_failure_retry",
        "v1",
        &version,
        &capture_command(
            "COLA_LIVE_OPENCODE_BIN",
            &binary,
            "live_v1_scripted_failure_and_retry_chain",
        ),
        &format!("{}/session/{}/message", server.base_url(), session.id),
    )
    .await;

    wait_for_idle(&backend, &session.id, &work_dir, &server).await;
    assert_provider_requests(&provider.requests());
}

/// The V2 read chain against the pinned V2 server: prompt through the
/// production adapter (the admit-then-return submit comes back while the turn
/// streams), watch the transcript mid-turn and at rest, and assert the neutral
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
    let backend = v2_live_backend(&server);

    wait_for_ready(&backend, &server).await;

    let work_dir = server.work_dir();
    let session = backend
        .create_session(&backend.new_session_input(Some(&work_dir)))
        .await
        .unwrap_or_else(|error| panic!("create session failed: {error}\n{}", server.stderr()));

    let message_id = format!("msg_cola_{}", uuid::Uuid::new_v4().simple());
    // The V2 submit is admit-then-return: it comes back as soon as the message
    // is durable, so the in-flight read below sees the live turn (ADR-0056).
    backend
        .prompt(&session.id, PROMPT_TEXT, &[], None, None, None, Some(&message_id))
        .await
        .unwrap_or_else(|error| panic!("prompt submit failed: {error}\n{}", server.stderr()));

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

    // At rest: the submitted turn settles and its closing text appears exactly
    // once — the same turn the old blocking response carried inline.
    let transcript = wait_for_turn(&backend, &session.id, &message_id, &server).await;
    assert!(
        turn_error(&transcript, &message_id).is_none(),
        "the submitted turn must not record a model error: {:?}",
        turn_error(&transcript, &message_id)
    );

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
/// with a mid-turn Supplement steered into the running turn, the cola-chosen
/// id persisted and retry-idempotent, then interrupt, compact and the title
/// rename.
#[tokio::test]
#[ignore = "live: needs a V2 binary (see the module docs)"]
async fn live_v2_scripted_write_chain() {
    let binary = live_v2_binary();
    let version = server::ensure_v2_binary(&binary).await;
    eprintln!("live V2 binary: {binary} ({version})");

    let provider = provider::start_with(provider::Tool::Shell, provider::ToolCommand::Slow).await;
    let server = LiveServer::start_v2(&binary, &provider.base_url()).await;

    let backend = v2_live_backend(&server);

    wait_for_ready(&backend, &server).await;

    let work_dir = server.work_dir();
    let session = backend
        .create_session(&backend.new_session_input(Some(&work_dir)))
        .await
        .unwrap_or_else(|error| panic!("create session failed: {error}\n{}", server.stderr()));

    // The submit returns as soon as the message is durable; the supplement is
    // steered in while the scripted slow tool runs, so the running turn must
    // merge it (ADR-0056).
    let message_id = format!("msg_cola_{}", uuid::Uuid::new_v4().simple());
    let supplement_id = format!("msg_cola_{}", uuid::Uuid::new_v4().simple());
    backend
        .prompt(&session.id, PROMPT_TEXT, &[], None, None, None, Some(&message_id))
        .await
        .unwrap_or_else(|error| panic!("prompt submit failed: {error}\n{}", server.stderr()));
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
        .prompt(
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

    let transcript = wait_for_turn(&backend, &session.id, &message_id, &server).await;

    // The supplement merged into the same turn: both user messages carry their
    // cola ids, and the turn closed on the scripted final text — with no
    // recorded failure.
    assert!(
        turn_error(&transcript, &message_id).is_none(),
        "the supplemented turn must not record a model error: {:?}",
        turn_error(&transcript, &message_id)
    );
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

    // The steer merged into the running turn's model call: the call carrying
    // the supplement text already has the first turn's tool result in its
    // conversation and no closing text yet (a new turn could only run after
    // the closing text, so this is what separates "merged" from "forked").
    let calls: Vec<Value> = provider
        .requests()
        .iter()
        .filter(|request| request.path == "/v1/chat/completions")
        .map(|request| serde_json::from_str(&request.body).expect("a provider request body must be JSON"))
        .collect();
    let steer_call = calls
        .iter()
        .find(|call| call.to_string().contains(SUPPLEMENT_TEXT))
        .unwrap_or_else(|| panic!("the steered supplement must reach the model turn: {calls:#?}"));
    assert!(
        provider::has_tool_result(steer_call),
        "the steered call must already carry the running turn's tool result: {steer_call:#?}"
    );
    assert!(
        !steer_call.to_string().contains(provider::FINAL_TEXT),
        "the supplement must merge before the closing text, not start a turn after it: {steer_call:#?}"
    );

    // Retry idempotency: the same cola id reconciles onto the durable message
    // instead of admitting a second user message or running a second turn. The
    // baseline is taken before the retry: the assistant count and the provider
    // call count must both be unchanged after it, and the session must still be
    // idle (nothing ran).
    let assistants_before = transcript
        .messages
        .iter()
        .filter(|message| message.role == MessageRole::Assistant)
        .count();
    let completions_before = provider_completions(&provider);
    backend
        .prompt(&session.id, PROMPT_TEXT, &[], None, None, None, Some(&message_id))
        .await
        .unwrap_or_else(|error| panic!("retry failed: {error}\n{}", server.stderr()));
    // Let the reconcile settle before counting, as the retired blocking retry
    // did: a deduplicated id schedules nothing.
    wait_for_idle(&backend, &session.id, &work_dir, &server).await;
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
    assert_eq!(
        transcript
            .messages
            .iter()
            .filter(|message| message.role == MessageRole::Assistant)
            .count(),
        assistants_before,
        "the retry must not run a second turn: {transcript:#?}"
    );
    assert_eq!(
        provider_completions(&provider),
        completions_before,
        "the retry must not call the model again"
    );
    wait_for_idle(&backend, &session.id, &work_dir, &server).await;

    // Interrupt: a fresh slow turn is stopped mid-flight; the interrupt answers
    // as an accepted op, the live tool settles, and the session idles. A fresh
    // session keeps the scripted provider's first call a tool call (the
    // transcript above already carries a tool result).
    let interrupt_session = backend
        .create_session(&backend.new_session_input(Some(&work_dir)))
        .await
        .unwrap_or_else(|error| panic!("create interrupt session failed: {error}\n{}", server.stderr()));
    let interrupt_id = format!("msg_cola_{}", uuid::Uuid::new_v4().simple());
    backend
        .prompt(
            &interrupt_session.id,
            PROMPT_TEXT,
            &[],
            None,
            None,
            None,
            Some(&interrupt_id),
        )
        .await
        .unwrap_or_else(|error| panic!("interrupt submit failed: {error}\n{}", server.stderr()));
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
    wait_for_idle(&backend, &interrupt_session.id, &work_dir, &server).await;
    let interrupted = backend
        .transcript(&interrupt_session.id)
        .await
        .unwrap_or_else(|error| panic!("interrupt transcript read failed: {error}\n{}", server.stderr()));
    assert!(
        !has_live_tool(&interrupted),
        "the interrupt must settle the live tool: {interrupted:#?}"
    );

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

/// The V2 session-scoped selection chain against the pinned V2 server:
/// `switch_session_model` / `switch_session_agent` write durable session state
/// (the variant inside the model ref), the next turn runs the switched
/// selection with NOTHING re-sent per prompt, and the bridge's own ADR-0020
/// clearing rule rewrites the live session's ref when the new model does not
/// declare the variant.
#[tokio::test]
#[ignore = "live: needs a V2 binary (see the module docs)"]
async fn live_v2_scripted_selection_chain() {
    use crate::bridge::handles::{PickOutcome, SessionsHandle};
    use crate::opencode::types::ModelInfo;

    let binary = live_v2_binary();
    let version = server::ensure_v2_binary(&binary).await;
    eprintln!("live V2 binary: {binary} ({version})");

    let provider = provider::start_with(provider::Tool::Shell, provider::ToolCommand::Fast).await;
    let server = LiveServer::start_v2(&binary, &provider.base_url()).await;

    let backend = v2_live_backend(&server);

    wait_for_ready(&backend, &server).await;

    let work_dir = server.work_dir();
    let session = backend
        .create_session(&backend.new_session_input(Some(&work_dir)))
        .await
        .unwrap_or_else(|error| panic!("create session failed: {error}\n{}", server.stderr()));

    // The create recorded cola's configured model as the session's selection.
    let selection = backend
        .session_selection(&session.id, Some(&work_dir))
        .await
        .unwrap_or_else(|error| panic!("selection read failed: {error}\n{}", server.stderr()))
        .expect("a V2 session exposes its durable selection");
    assert_eq!(
        selection.model.as_ref().map(|model| model.id.as_str()),
        Some(provider::MODEL),
        "the create's configured model is the session's selection: {selection:?}"
    );

    // `/model` + `/think`: switch to the variant-declaring model with the
    // variant inside the ref; `/agent`: switch to an agent other than the
    // configured default (so the reset below has somewhere to land).
    backend
        .switch_session_model(
            &session.id,
            &ModelInfo {
                id: provider::MODEL_ALT.into(),
                provider_id: provider::PROVIDER.into(),
                variant: Some(provider::VARIANT.into()),
            },
        )
        .await
        .unwrap_or_else(|error| panic!("model switch failed: {error}\n{}", server.stderr()));
    backend
        .switch_session_agent(&session.id, "build")
        .await
        .unwrap_or_else(|error| panic!("agent switch failed: {error}\n{}", server.stderr()));

    let selection = backend
        .session_selection(&session.id, Some(&work_dir))
        .await
        .unwrap_or_else(|error| panic!("selection read failed: {error}\n{}", server.stderr()))
        .expect("the switched session keeps a selection");
    let model = selection.model.expect("the switched model survives");
    assert_eq!(model.id, provider::MODEL_ALT, "the model switch persisted");
    assert_eq!(
        model.variant.as_deref(),
        Some(provider::VARIANT),
        "the variant rides inside the persisted model ref"
    );
    assert_eq!(
        selection.agent.as_deref(),
        Some("build"),
        "the agent switch persisted"
    );

    // The next turn runs the switched selection with NOTHING re-sent per
    // prompt: every prompt axis beyond the text and the cola message id is
    // `None` (the V2 strategy would drop them anyway — the wire has no such
    // fields — so this is what "the session's selection applies" means).
    let message_id = format!("msg_cola_{}", uuid::Uuid::new_v4().simple());
    backend
        .prompt(&session.id, PROMPT_TEXT, &[], None, None, None, Some(&message_id))
        .await
        .unwrap_or_else(|error| panic!("prompt submit failed: {error}\n{}", server.stderr()));
    let transcript = wait_for_turn(&backend, &session.id, &message_id, &server).await;
    assert!(
        turn_error(&transcript, &message_id).is_none(),
        "the switched selection must run cleanly: {:?}",
        turn_error(&transcript, &message_id)
    );
    assert_user_anchor(&transcript, &message_id);
    assert_turn_complete(&transcript, &message_id);
    let answer = transcript
        .messages
        .iter()
        .rev()
        .find(|message| {
            message.role == MessageRole::Assistant
                && message.time.is_some_and(|time| time.completed.is_some())
        })
        .expect("a completed assistant message");
    let identity = answer
        .model
        .as_ref()
        .expect("the answering message names its model");
    assert_eq!(
        identity.model_id,
        provider::MODEL_ALT,
        "the turn ran the session's switched model: {identity:?}"
    );
    assert_eq!(
        identity.variant.as_deref(),
        Some(provider::VARIANT),
        "the turn ran the switched ref's variant: {identity:?}"
    );
    wait_for_idle(&backend, &session.id, &work_dir, &server).await;

    // The scripted provider was asked for the switched model — the server
    // really resolved the session ref, not just persisted it.
    assert!(
        provider.requests().iter().any(|request| {
            request.path == "/v1/chat/completions" && request.body.contains(provider::MODEL_ALT)
        }),
        "the provider must be asked for the switched model:\n{}",
        server.stderr()
    );

    // The bridge's own pick path against the live server: a model that
    // declares the variant keeps it; a model that does not clears it from the
    // session's ref (ADR-0020), judged against the server's selection.
    let store_dir = tempfile::tempdir().expect("a temp store for the live pick");
    let mut store = crate::bridge::session::SessionStore::new(store_dir.path().join("sessions.json"))
        .expect("a fresh session store");
    let key = crate::config::ThreadKey::new("live_selection".into(), "live_selection".into());
    let mut entry = crate::config::SessionEntry::new(key.clone(), session.id.clone(), work_dir.clone());
    entry.model = Some(provider::MODEL_ALT_REF.into());
    entry.variant = Some(provider::VARIANT.into());
    store.activate(entry).expect("seed the live mapping");
    let sessions = SessionsHandle::new(
        std::sync::Arc::new(tokio::sync::Mutex::new(store)),
        std::sync::Arc::new(tokio::sync::Mutex::new(None)),
    );
    let pick_backend = std::sync::Arc::new(backend.clone()) as std::sync::Arc<dyn crate::backend::Backend>;

    let outcome = sessions
        .pick_model(&pick_backend, &key, provider::MODEL_ALT_REF)
        .await
        .unwrap_or_else(|error| panic!("pick_model failed: {error}\n{}", server.stderr()));
    assert_eq!(
        outcome,
        PickOutcome::Applied {
            cleared_variant: None
        },
        "a model that declares the variant keeps it"
    );
    let selection = backend
        .session_selection(&session.id, Some(&work_dir))
        .await
        .unwrap_or_else(|error| panic!("selection read failed: {error}\n{}", server.stderr()))
        .expect("a selection");
    assert_eq!(
        selection.model.expect("a model").variant.as_deref(),
        Some(provider::VARIANT),
        "the live session kept the variant"
    );

    // The bridge's ladder reads the SESSION's selection live (not a mirror):
    // the effective model and variant come from the server's own selection.
    let settings = sessions
        .session_settings(&key)
        .await
        .expect("the live mapping is the settings target");
    let effective = sessions
        .effective_selection(&pick_backend, &settings)
        .await
        .expect("the live durable selection resolves");
    assert_eq!(effective.id, provider::MODEL_ALT);
    assert_eq!(effective.variant.as_deref(), Some(provider::VARIANT));
    assert_eq!(
        sessions
            .effective_variant(&pick_backend, &session.id, Some(&work_dir))
            .await
            .as_deref(),
        Some(provider::VARIANT),
        "the footer's variant source reads the live selection"
    );

    let outcome = sessions
        .pick_model(&pick_backend, &key, provider::MODEL_REF)
        .await
        .unwrap_or_else(|error| panic!("pick_model failed: {error}\n{}", server.stderr()));
    assert_eq!(
        outcome,
        PickOutcome::Applied {
            cleared_variant: Some(provider::VARIANT.to_string())
        },
        "a model that lacks the variant clears it"
    );
    let selection = backend
        .session_selection(&session.id, Some(&work_dir))
        .await
        .unwrap_or_else(|error| panic!("selection read failed: {error}\n{}", server.stderr()))
        .expect("a selection");
    let model = selection.model.expect("the cleared model survives");
    assert_eq!(model.id, provider::MODEL, "the ref points at the picked model");
    assert!(
        model.variant.is_none(),
        "the undeclared variant is gone from the live session ref"
    );

    // The `/agent --reset` default heuristic validated against the live
    // server: the isolated config pins `default_agent` to the custom agent, so
    // the server sorts it first in `GET /agent` and the heuristic must return
    // exactly that (not the built-in `build` fallback).
    let agents = backend.list_agents().await;
    let default = crate::opencode::types::AgentInfo::default_agent(&agents);
    assert_eq!(
        default.as_deref(),
        Some(provider::AGENT),
        "the catalog's first primary visible agent is the configured default: {agents:?}"
    );
    let outcome = sessions
        .pick_agent(&pick_backend, &key, None)
        .await
        .unwrap_or_else(|error| panic!("pick_agent failed: {error}\n{}", server.stderr()));
    assert_eq!(
        outcome,
        PickOutcome::Applied {
            cleared_variant: None
        },
        "the reset resolves a default and switches to it"
    );
    let selection = backend
        .session_selection(&session.id, Some(&work_dir))
        .await
        .unwrap_or_else(|error| panic!("selection read failed: {error}\n{}", server.stderr()))
        .expect("a selection");
    assert_eq!(
        selection.agent.as_deref(),
        Some(provider::AGENT),
        "the live session's agent is the resolved default"
    );
}

/// The V2 permission round-trip against the pinned V2 server: the production
/// prompt blocks on a pending form/tool gate, the location-scoped pending list
/// surfaces the ask, the session-scoped decision releases it, and the turn
/// finishes with the tool result and closing text.
#[tokio::test]
#[ignore = "live: needs a V2 binary (see the module docs)"]
async fn live_v2_scripted_permission_chain() {
    use server::V2Permissions;

    let binary = live_v2_binary();
    let version = server::ensure_v2_binary(&binary).await;
    eprintln!("live V2 binary: {binary} ({version})");

    let provider = provider::start_with(provider::Tool::Shell, provider::ToolCommand::Fast).await;
    let server = LiveServer::start_v2_with(&binary, &provider.base_url(), V2Permissions::AskShell).await;

    let backend = v2_live_backend(&server);

    wait_for_ready(&backend, &server).await;

    let work_dir = server.work_dir();
    let session = backend
        .create_session(&backend.new_session_input(Some(&work_dir)))
        .await
        .unwrap_or_else(|error| panic!("create session failed: {error}\n{}", server.stderr()));

    let message_id = format!("msg_cola_{}", uuid::Uuid::new_v4().simple());
    // The admit-then-return submit schedules the turn; it blocks on the shell
    // ask, which the location-scoped pending list must surface (one call per
    // directory, never per session).
    backend
        .prompt(&session.id, PROMPT_TEXT, &[], None, None, None, Some(&message_id))
        .await
        .unwrap_or_else(|error| panic!("prompt submit failed: {error}\n{}", server.stderr()));
    let permission = poll_until(
        &format!("a V2 permission ask for session {}", session.id),
        POLL_TIMEOUT,
        || async {
            backend
                .list_permissions(Some(&work_dir))
                .await
                .map(|pending| pending.into_iter().next())
        },
        || server.stderr(),
    )
    .await;
    assert_eq!(
        permission.session_id.as_deref(),
        Some(session.id.as_str()),
        "the ask must belong to the prompted session"
    );
    assert_eq!(
        permission.permission.as_deref(),
        Some("shell"),
        "the scripted shell tool must be gated as a shell ask: {permission:?}"
    );

    // The session-scoped decision releases the run.
    backend
        .reply_permission(&session.id, &permission.request_id, "once", Some(&work_dir))
        .await
        .unwrap_or_else(|error| panic!("permission reply failed: {error}\n{}", server.stderr()));

    assert!(
        backend
            .list_permissions(Some(&work_dir))
            .await
            .unwrap_or_else(|error| panic!("permission list failed: {error}\n{}", server.stderr()))
            .is_empty(),
        "the replied request must leave the pending list"
    );

    let transcript = wait_for_turn(&backend, &session.id, &message_id, &server).await;
    assert!(
        turn_error(&transcript, &message_id).is_none(),
        "the answered turn must not record a model error: {:?}",
        turn_error(&transcript, &message_id)
    );
    assert_user_anchor(&transcript, &message_id);
    assert_tool_call(&transcript, provider::Tool::Shell, provider::ToolCommand::Fast);
    assert_streamed_reasoning(&transcript);
    assert_final_text(&transcript);
    assert_turn_complete(&transcript, &message_id);
    wait_for_idle(&backend, &session.id, &work_dir, &server).await;

    // The other two decisions round-trip too. `reject` runs first: it declines
    // the tool without saving anything, while `always` persists an approval for
    // the same resource — a later ask would then be auto-allowed and never
    // surface. `always`'s own turn completes like the `once` one.
    for decision in ["reject", "always"] {
        let session = backend
            .create_session(&backend.new_session_input(Some(&work_dir)))
            .await
            .unwrap_or_else(|error| {
                panic!("{decision}: create session failed: {error}\n{}", server.stderr())
            });
        let message_id = format!("msg_cola_{}", uuid::Uuid::new_v4().simple());
        backend
            .prompt(&session.id, PROMPT_TEXT, &[], None, None, None, Some(&message_id))
            .await
            .unwrap_or_else(|error| panic!("{decision}: prompt submit failed: {error}\n{}", server.stderr()));
        let permission = poll_until(
            &format!("a V2 permission ask ({decision})"),
            POLL_TIMEOUT,
            || async {
                backend
                    .list_permissions(Some(&work_dir))
                    .await
                    .map(|pending| pending.into_iter().next())
            },
            || server.stderr(),
        )
        .await;
        assert_eq!(permission.permission.as_deref(), Some("shell"), "{decision}");
        backend
            .reply_permission(&session.id, &permission.request_id, decision, Some(&work_dir))
            .await
            .unwrap_or_else(|error| {
                panic!(
                    "{decision}: permission reply failed: {error}\n{}",
                    server.stderr()
                )
            });
        // The replied request leaves the pending list whichever decision it was.
        poll_until(
            &format!("the {decision} reply to leave the pending list"),
            POLL_TIMEOUT,
            || async {
                backend.list_permissions(Some(&work_dir)).await.map(|pending| {
                    pending
                        .iter()
                        .all(|request| request.request_id != permission.request_id)
                        .then_some(())
                })
            },
            || server.stderr(),
        )
        .await;
        if decision == "always" {
            let transcript = wait_for_turn(&backend, &session.id, &message_id, &server).await;
            assert!(
                turn_error(&transcript, &message_id).is_none(),
                "always: the approved turn must settle cleanly: {:?}",
                turn_error(&transcript, &message_id)
            );
        }
        // `reject` deliberately declines the tool (a defect tunnel upstream), so
        // only the run settling is asserted — never a specific turn outcome.
        wait_for_idle(&backend, &session.id, &work_dir, &server).await;
    }
}

/// The V2 form round-trip against the pinned V2 server: the scripted question
/// tool blocks the turn on a typed form, the location-scoped list surfaces it,
/// the keyed answer releases it, and a second form cancels by delete.
#[tokio::test]
#[ignore = "live: needs a V2 binary (see the module docs)"]
async fn live_v2_scripted_form_chain() {
    use crate::opencode::types::{FormAnswer, FormFieldKind, FormValue};
    use server::V2Permissions;

    let binary = live_v2_binary();
    let version = server::ensure_v2_binary(&binary).await;
    eprintln!("live V2 binary: {binary} ({version})");

    let provider = provider::start_with(provider::Tool::Question, provider::ToolCommand::Fast).await;
    let server = LiveServer::start_v2_with(&binary, &provider.base_url(), V2Permissions::AllowQuestion).await;

    let backend = v2_live_backend(&server);

    wait_for_ready(&backend, &server).await;

    let work_dir = server.work_dir();
    let session = backend
        .create_session(&backend.new_session_input(Some(&work_dir)))
        .await
        .unwrap_or_else(|error| panic!("create session failed: {error}\n{}", server.stderr()));

    let message_id = format!("msg_cola_{}", uuid::Uuid::new_v4().simple());
    // The admit-then-return submit schedules the turn; it blocks on the typed
    // form the location-scoped list must surface.
    backend
        .prompt(&session.id, PROMPT_TEXT, &[], None, None, None, Some(&message_id))
        .await
        .unwrap_or_else(|error| panic!("prompt submit failed: {error}\n{}", server.stderr()));

    let form = poll_until(
        &format!("a V2 form for session {}", session.id),
        POLL_TIMEOUT,
        || async {
            backend
                .list_questions(Some(&work_dir))
                .await
                .map(|forms| forms.into_iter().next())
        },
        || server.stderr(),
    )
    .await;
    assert_eq!(form.session_id, session.id, "the form belongs to the session");
    assert_eq!(form.questions.len(), 1, "one scripted field: {form:?}");
    let field = &form.questions[0];
    assert_eq!(field.key, "q0", "the question tool keys fields qN: {field:?}");
    assert_eq!(field.kind, FormFieldKind::String, "a single-select string field");
    assert!(
        field
            .options
            .iter()
            .any(|option| option.answer_value() == provider::QUESTION_OPTION),
        "the scripted option must be offered: {field:?}"
    );

    // The keyed answer releases the question tool; the turn continues to the
    // scripted closing text.
    let answers = vec![FormAnswer {
        key: field.key.clone(),
        value: Some(FormValue::Text(provider::QUESTION_OPTION.to_string())),
    }];
    backend
        .reply_question(&session.id, &form.id, &answers, Some(&work_dir))
        .await
        .unwrap_or_else(|error| panic!("form reply failed: {error}\n{}", server.stderr()));

    assert_eq!(
        backend
            .list_questions(Some(&work_dir))
            .await
            .unwrap_or_else(|error| panic!("form list failed: {error}\n{}", server.stderr()))
            .len(),
        0,
        "the answered form must leave the pending list"
    );

    let transcript = wait_for_turn(&backend, &session.id, &message_id, &server).await;
    assert!(
        turn_error(&transcript, &message_id).is_none(),
        "the answered turn must not record a model error: {:?}",
        turn_error(&transcript, &message_id)
    );
    assert_user_anchor(&transcript, &message_id);
    assert_question_tool_call(&transcript);
    assert_final_text(&transcript);
    assert_turn_complete(&transcript, &message_id);
    wait_for_idle(&backend, &session.id, &work_dir, &server).await;

    // A second, cancelled form on a fresh session: the delete removes it from
    // the pending list and unblocks the run (the tool fails with the
    // cancellation; the prompt outcome itself is not asserted).
    let cancel_session = backend
        .create_session(&backend.new_session_input(Some(&work_dir)))
        .await
        .unwrap_or_else(|error| panic!("create cancel session failed: {error}\n{}", server.stderr()));
    let cancel_message_id = format!("msg_cola_{}", uuid::Uuid::new_v4().simple());
    backend
        .prompt(
            &cancel_session.id,
            PROMPT_TEXT,
            &[],
            None,
            None,
            None,
            Some(&cancel_message_id),
        )
        .await
        .unwrap_or_else(|error| panic!("cancel submit failed: {error}\n{}", server.stderr()));
    let cancelled_form = poll_until(
        "the second V2 form",
        POLL_TIMEOUT,
        || async {
            backend
                .list_questions(Some(&work_dir))
                .await
                .map(|forms| forms.into_iter().next())
        },
        || server.stderr(),
    )
    .await;
    assert_eq!(cancelled_form.session_id, cancel_session.id);
    backend
        .reject_question(&cancel_session.id, &cancelled_form.id, Some(&work_dir))
        .await
        .unwrap_or_else(|error| panic!("form cancel failed: {error}\n{}", server.stderr()));
    poll_until(
        "the cancelled form to leave the pending list",
        POLL_TIMEOUT,
        || async {
            backend
                .list_questions(Some(&work_dir))
                .await
                .map(|forms| forms.is_empty().then_some(()))
        },
        || server.stderr(),
    )
    .await;
    // The cancelled turn may settle with a tool failure; let the run reach its
    // end so the server is not left with a blocked run.
    wait_for_idle(&backend, &cancel_session.id, &work_dir, &server).await;
}

/// The V2 retry-id contract against the pinned V2 server: the server keeps
/// cola's `msg_cola_` id as an ADMISSION key.
///
/// - a re-post with an id the server already admitted is a no-op: the submit
///   is accepted, no assistant message is produced, no model call is made, and
///   every stored row is unchanged — only an idle event is appended;
/// - a re-post with a NEW id runs a fresh turn;
/// - an ABORTED assistant message carries `finish="error"` plus an `aborted`
///   error — exactly the shape the retry matrix reads (a terminal finish, so
///   the settled arm picks a new id);
/// - the matrix's "unfinished + idle + reuse continues the turn" cell is
///   pinned the way the server actually behaves: a run the server died in
///   leaves the turn unfinished and the session idle, and re-posting the SAME
///   id still runs nothing (the id's admission already exists, so there is
///   nothing to drain). The reuse arm is therefore idempotent — it never
///   duplicates — but it does not revive a run that already started.
///
/// Every turn is scripted: the provider answers with a tool call and a closing
/// text, so a "run" is observable as assistant rows and model calls, not as a
/// wall-clock wait. The scratch sessions are deleted at the end; the whole
/// chain runs in the isolated store (ADR-0057).
#[tokio::test]
#[ignore = "live: needs a V2 binary (see the module docs)"]
async fn live_v2_scripted_retry_id_chain() {
    let binary = live_v2_binary();
    let version = server::ensure_v2_binary(&binary).await;
    eprintln!("live V2 binary: {binary} ({version})");

    let provider = provider::start_with(provider::Tool::Shell, provider::ToolCommand::Slow).await;
    let mut server = LiveServer::start_v2(&binary, &provider.base_url()).await;
    let backend = v2_live_backend(&server);
    wait_for_ready(&backend, &server).await;
    let work_dir = server.work_dir();

    // A settled turn. The submit is the production adapter's admit-then-return
    // prompt; the turn settles through the transcript + run state.
    let settled = scratch_session(&backend, &work_dir, &server).await;
    let settled_id = start_turn(&backend, &server, &settled.id).await;
    let transcript = wait_for_turn(&backend, &settled.id, &settled_id, &server).await;
    assert!(
        turn_error(&transcript, &settled_id).is_none(),
        "the submitted turn must not record a model error: {:?}",
        turn_error(&transcript, &settled_id)
    );
    assert_user_anchor(&transcript, &settled_id);
    assert_turn_complete(&transcript, &settled_id);
    wait_for_idle(&backend, &settled.id, &work_dir, &server).await;

    // Same id, settled turn: the server has already admitted that id, so the
    // re-post reconciles onto it and runs nothing — the acceptance contract
    // the retry matrix's settled arm (a fresh id) exists for.
    assert_retry_id_no_op(
        &backend,
        &provider,
        &server,
        &settled.id,
        &settled_id,
        "a settled turn",
    )
    .await;
    wait_for_idle(&backend, &settled.id, &work_dir, &server).await;

    // A NEW id on the same session runs a second turn for real.
    let completions_before = provider_turn_completions(&provider);
    let fresh_id = start_turn(&backend, &server, &settled.id).await;
    let fresh = wait_for_turn(&backend, &settled.id, &fresh_id, &server).await;
    assert_user_anchor(&fresh, &fresh_id);
    assert_turn_complete(&fresh, &fresh_id);
    assert!(
        provider_turn_completions(&provider) > completions_before,
        "a new id must actually run the turn:\n{}",
        server.stderr()
    );
    assert_eq!(
        fresh
            .messages
            .iter()
            .filter(|message| message.role == MessageRole::User)
            .count(),
        2,
        "the retried question appears as asked again: {fresh:#?}"
    );
    wait_for_idle(&backend, &settled.id, &work_dir, &server).await;

    // An aborted turn: interrupt the scripted slow tool mid-flight, then read
    // the aborted message's shape — the data the retry matrix evaluates.
    let aborted = scratch_session(&backend, &work_dir, &server).await;
    let aborted_id = start_turn(&backend, &server, &aborted.id).await;
    wait_for_live_tool(&backend, &aborted.id, &server).await;
    backend
        .interrupt(&aborted.id)
        .await
        .unwrap_or_else(|error| panic!("interrupt failed: {error}\n{}", server.stderr()));
    wait_for_idle(&backend, &aborted.id, &work_dir, &server).await;

    let read = raw_read(&server, &aborted.id).await;
    let message = raw_assistant(&read).unwrap_or_else(|| {
        panic!(
            "the aborted turn is in the transcript:\n{}",
            serde_json::to_string_pretty(&read).unwrap()
        )
    });
    assert_eq!(
        message["finish"], "error",
        "an aborted assistant message reads as an error finish: {message:#?}"
    );
    assert_eq!(
        message["error"]["type"], "aborted",
        "the finish carries the aborted error: {message:#?}"
    );
    let tool =
        raw_tool_part(&read).unwrap_or_else(|| panic!("the aborted turn carries its tool call: {read:#?}"));
    assert_eq!(tool["state"]["status"], "error", "{tool:#?}");
    assert_eq!(
        tool["state"]["error"]["type"], "aborted",
        "the interrupted tool is recorded as aborted: {tool:#?}"
    );
    // The neutral projection the matrix reads: `finish="error"` is a terminal
    // step finish, so the turn reads settled AND carries the abort text.
    let transcript = backend.transcript(&aborted.id).await.unwrap();
    let anchor = transcript
        .anchor_of_user(&aborted_id)
        .expect("the aborted turn keeps its user anchor");
    let turn = transcript.turn_for_user(&anchor);
    assert!(
        turn.complete,
        "an aborted message is a terminal finish, so the turn reads settled: {transcript:#?}"
    );
    assert!(
        turn.error.is_some(),
        "the abort text lands on the turn's error: {transcript:#?}"
    );

    capture_fixture(
        "v2/transcript_aborted",
        "v2",
        &version,
        &capture_command(
            "COLA_LIVE_OPENCODE_V2_BIN",
            &binary,
            "live_v2_scripted_retry_id_chain",
        ),
        &transcript_read_url(&server, &aborted.id),
    )
    .await;

    // A same-id re-post on the aborted turn is the same admission no-op.
    assert_retry_id_no_op(
        &backend,
        &provider,
        &server,
        &aborted.id,
        &aborted_id,
        "an aborted turn",
    )
    .await;
    wait_for_idle(&backend, &aborted.id, &work_dir, &server).await;

    // The "unfinished + idle + reuse" cell. A run the server is killed in
    // never finalizes: the assistant message keeps no finish and its tool
    // stays running, while a fresh process reports the session idle. Re-posting
    // the same id is STILL the admission no-op — the id was already admitted,
    // so there is nothing to drain and the turn is not revived. This is the
    // live evidence for the generation capability the retry matrix reads
    // (`Backend::reuse_continues_an_admitted_turn`): false on V2, and the reuse
    // arm never duplicates a submission but does not continue a run that
    // already started.
    let orphaned = scratch_session(&backend, &work_dir, &server).await;
    let orphaned_id = start_turn(&backend, &server, &orphaned.id).await;
    wait_for_live_tool(&backend, &orphaned.id, &server).await;
    server.restart().await;
    let backend = v2_live_backend(&server);
    wait_for_ready(&backend, &server).await;

    assert_eq!(
        backend
            .session_status(&orphaned.id, Some(&work_dir))
            .await
            .unwrap_or_else(|error| panic!("orphan status read failed: {error}\n{}", server.stderr())),
        Some(SessionStatus::Idle),
        "the dead run leaves the session idle"
    );
    let orphaned_read = raw_read(&server, &orphaned.id).await;
    let message = raw_assistant(&orphaned_read)
        .unwrap_or_else(|| panic!("the orphaned message survives the restart: {orphaned_read:#?}"));
    assert!(
        message["finish"].is_null(),
        "a run the server died in leaves no finish: {message:#?}"
    );
    let tool = raw_tool_part(&orphaned_read)
        .unwrap_or_else(|| panic!("the orphaned turn keeps its tool call: {orphaned_read:#?}"));
    assert_eq!(
        tool["state"]["status"], "running",
        "the dead run's tool stays running: {tool:#?}"
    );
    let transcript = backend.transcript(&orphaned.id).await.unwrap();
    let anchor = transcript
        .anchor_of_user(&orphaned_id)
        .expect("the orphaned turn keeps its user anchor");
    let turn = transcript.turn_for_user(&anchor);
    assert!(
        !turn.complete,
        "a message with no finish leaves the turn unfinished: {transcript:#?}"
    );
    assert!(
        turn.error.is_none(),
        "the dead run records no error on the message: {transcript:#?}"
    );
    assert_retry_id_no_op(
        &backend,
        &provider,
        &server,
        &orphaned.id,
        &orphaned_id,
        "an unfinished (orphaned) turn",
    )
    .await;

    for session in [settled.id, aborted.id, orphaned.id] {
        backend
            .delete_session(&session)
            .await
            .unwrap_or_else(|error| panic!("delete session {session} failed: {error}\n{}", server.stderr()));
    }
}

/// The production adapter pointed at the live child, with the env-proxy
/// workaround — rebuilt after a [`LiveServer::restart`] because the port moved.
fn v2_live_backend(server: &LiveServer) -> OpenCodeBackend {
    let backend = OpenCodeBackend::with_generation(
        Some(provider::MODEL_REF),
        server.base_url(),
        Some("opencode"),
        Some(server::PASSWORD),
        Generation::V2,
        None,
    );
    backend.disable_env_proxy(Some("opencode"), Some(server::PASSWORD));
    backend
}

/// Re-post `message_id` and assert the server's admission contract: the raw
/// answer is exactly `200 OK` echoing the admitted id, the production adapter's
/// own submit path accepts it too, no new assistant message is produced, no
/// model call is made, every existing wire row stays byte-identical, and each
/// re-post appends exactly one idle event. `what` names the turn state in
/// failures.
async fn assert_retry_id_no_op(
    backend: &OpenCodeBackend,
    provider: &crate::test_http::TestHttpServer,
    server: &LiveServer,
    session_id: &str,
    message_id: &str,
    what: &str,
) {
    let baseline = raw_read(server, session_id).await;
    let rows_before = raw_rows(&baseline);
    let completions_before = provider_turn_completions(provider);
    let assistants_before = raw_assistant_count(&baseline);

    // The wire answer: exactly 200, with the admitted message echoed back (the
    // re-post reconciles onto the existing admission, never a new id). The
    // adapter maps any non-2xx to `Err`, so this raw submit is what pins the
    // exact status the contract names.
    let (status, body) = raw_prompt(server, session_id, message_id, PROMPT_TEXT).await;
    assert_eq!(
        status,
        reqwest::StatusCode::OK,
        "{what}: a same-id re-post must answer 200, got {status}: {body:#?}\n{}",
        server.stderr()
    );
    assert_eq!(
        body["data"]["id"].as_str(),
        Some(message_id),
        "{what}: the re-post must reconcile onto the admitted id: {body:#?}"
    );

    // …and the production submit path accepts it too (the adapter's `Ok` is
    // its 2xx mapping).
    submit_prompt(backend, server, session_id, message_id).await;

    let rows_after = poll_until(
        &format!("{what}: the same-id re-posts' idle events"),
        POLL_TIMEOUT,
        || async {
            let read = raw_read(server, session_id).await;
            Ok((raw_rows(&read).len() >= rows_before.len() + 2).then_some(raw_rows(&read)))
        },
        || server.stderr(),
    )
    .await;

    for (id, row) in &rows_before {
        assert_eq!(
            rows_after.get(id),
            Some(row),
            "{what}: the {id} row must be unchanged by a same-id re-post"
        );
    }
    let appended: Vec<&Value> = rows_after
        .iter()
        .filter(|(id, _)| !rows_before.contains_key(*id))
        .map(|(_, row)| row)
        .collect();
    assert_eq!(
        appended.len(),
        2,
        "{what}: each same-id re-post appends only its idle event: {appended:#?}"
    );
    assert!(
        appended.iter().all(|row| row["type"].as_str() == Some("idle")),
        "{what}: the only effect of a same-id re-post is an idle event: {appended:#?}"
    );
    assert_eq!(
        raw_assistant_count(&raw_read(server, session_id).await),
        assistants_before,
        "{what}: a same-id re-post runs no new step:\n{}",
        server.stderr()
    );
    assert_eq!(
        provider_turn_completions(provider),
        completions_before,
        "{what}: a same-id re-post must not call the model"
    );
}

/// A fresh cola-chosen message id — the admission key the retry policy uses.
fn cola_message_id() -> String {
    format!("msg_cola_{}", uuid::Uuid::new_v4().simple())
}

/// Create one scratch session in the isolated store.
async fn scratch_session(
    backend: &OpenCodeBackend,
    work_dir: &str,
    server: &LiveServer,
) -> crate::opencode::types::Session {
    backend
        .create_session(&backend.new_session_input(Some(work_dir)))
        .await
        .unwrap_or_else(|error| panic!("create session failed: {error}\n{}", server.stderr()))
}

/// Submit one fresh `msg_cola_` prompt through the production adapter and
/// return the id it used.
async fn start_turn(backend: &OpenCodeBackend, server: &LiveServer, session_id: &str) -> String {
    let message_id = cola_message_id();
    submit_prompt(backend, server, session_id, &message_id).await;
    message_id
}

/// Submit `message_id` through the production adapter and assert acceptance.
async fn submit_prompt(backend: &OpenCodeBackend, server: &LiveServer, session_id: &str, message_id: &str) {
    backend
        .prompt(session_id, PROMPT_TEXT, &[], None, None, None, Some(message_id))
        .await
        .unwrap_or_else(|error| {
            panic!(
                "prompt submit to session {session_id} failed: {error}\n{}",
                server.stderr()
            )
        });
}

/// Submit `message_id` raw, with the wire body the V2 strategy sends (`text`,
/// `id`, `delivery: "steer"`), returning the status and body — the exact-code
/// answer the adapter's `Ok`/`Err` cannot express.
async fn raw_prompt(
    server: &LiveServer,
    session_id: &str,
    message_id: &str,
    text: &str,
) -> (reqwest::StatusCode, Value) {
    let response = crate::test_http::no_proxy_transport()
        .post(format!("{}/api/session/{}/prompt", server.base_url(), session_id))
        .basic_auth("opencode", Some(server::PASSWORD))
        .json(&serde_json::json!({ "text": text, "id": message_id, "delivery": "steer" }))
        .send()
        .await
        .unwrap_or_else(|error| panic!("raw prompt submit failed: {error}\n{}", server.stderr()));
    let status = response.status();
    let body = response
        .json()
        .await
        .unwrap_or_else(|error| panic!("raw prompt body is not JSON: {error}\n{}", server.stderr()));
    (status, body)
}

/// Poll until the session's transcript shows a still-live tool call.
async fn wait_for_live_tool(backend: &OpenCodeBackend, session_id: &str, server: &LiveServer) {
    poll_until(
        &format!("a live tool in session {session_id}"),
        POLL_TIMEOUT,
        || async {
            let transcript = backend.transcript(session_id).await?;
            Ok(has_live_tool(&transcript).then_some(()))
        },
        || server.stderr(),
    )
    .await;
}

/// The raw per-session message read both the shape assertions and the fixture
/// capture read.
fn transcript_read_url(server: &LiveServer, session_id: &str) -> String {
    format!(
        "{}/api/session/{}/message?order=asc&limit=200",
        server.base_url(),
        session_id
    )
}

/// One raw transcript read: the wire rows the id policy's shape assertions
/// name. The chains otherwise consume the production adapter; the shape of an
/// aborted or orphaned row is a wire fact the neutral projection folds away.
async fn raw_read(server: &LiveServer, session_id: &str) -> Value {
    let response = crate::test_http::no_proxy_transport()
        .get(transcript_read_url(server, session_id))
        .basic_auth("opencode", Some(server::PASSWORD))
        .send()
        .await
        .unwrap_or_else(|error| panic!("raw transcript read failed: {error}\n{}", server.stderr()));
    assert!(
        response.status().is_success(),
        "raw transcript read: HTTP {}\n{}",
        response.status(),
        server.stderr()
    );
    response.json().await.expect("the transcript read body is JSON")
}

/// The read's rows keyed by message id, for the unchanged-row assertion. Every
/// row must carry a string id: an id-less row would silently collapse the map
/// key and weaken the check it exists for.
fn raw_rows(read: &Value) -> std::collections::BTreeMap<String, Value> {
    read["data"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|row| {
            let id = row["id"]
                .as_str()
                .unwrap_or_else(|| panic!("every wire row carries a string id: {row:#?}"));
            (id.to_string(), row.clone())
        })
        .collect()
}

/// The read's first assistant message — the row whose `finish` the matrix reads.
fn raw_assistant(read: &Value) -> Option<&Value> {
    read["data"]
        .as_array()?
        .iter()
        .find(|row| row["type"].as_str() == Some("assistant"))
}

/// How many assistant rows the read carries.
fn raw_assistant_count(read: &Value) -> usize {
    read["data"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|row| row["type"].as_str() == Some("assistant"))
        .count()
}

/// The read's first tool content item, as the wire row spells it.
fn raw_tool_part(read: &Value) -> Option<&Value> {
    read["data"]
        .as_array()?
        .iter()
        .flat_map(|row| row["content"].as_array().into_iter().flatten())
        .find(|part| part["type"].as_str() == Some("tool"))
}

/// Whether any decoded part of the transcript is a still-live tool call.
fn has_live_tool(transcript: &SessionTranscript) -> bool {
    transcript
        .messages
        .iter()
        .flat_map(|message| message.parts.iter())
        .any(|part| matches!(part, Part::Tool(call) if call.status.is_live()))
}

/// How many model calls the scripted provider has received so far — the
/// baseline a retry must not move.
fn provider_completions(provider: &crate::test_http::TestHttpServer) -> usize {
    provider
        .requests()
        .iter()
        .filter(|request| request.path == "/v1/chat/completions")
        .count()
}

/// How many TURN model calls the scripted provider has received — the title
/// calls offer no tools, so they are excluded and a retry's idempotency
/// assertion cannot race the title agent.
fn provider_turn_completions(provider: &crate::test_http::TestHttpServer) -> usize {
    provider
        .requests()
        .iter()
        .filter(|request| {
            request.path == "/v1/chat/completions"
                && serde_json::from_str::<Value>(&request.body)
                    .is_ok_and(|body| provider::offers_tools(&body))
        })
        .count()
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
        .reply_permission(session_id, &permission.request_id, "once", Some(directory))
        .await
        .expect("the permission reply must be accepted");
    permission
}

/// Wait until the server reports the session idle — the run state may clear a
/// beat after the turn's transcript settles.
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

/// Wait for the turn anchored at `message_id` to settle — a terminal step or a
/// recorded failure — and return the transcript that observed it. This is the
/// observation the async-native Turn performs from the transcript + run state
/// (ADR-0056); the retired blocking prompt used to hand the finished turn back
/// inline. Verification is the caller's: a clean turn asserts no error, an
/// aborted one does not.
async fn wait_for_turn(
    backend: &OpenCodeBackend,
    session_id: &str,
    message_id: &str,
    server: &LiveServer,
) -> crate::backend::SessionTranscript {
    poll_until(
        "the submitted turn to settle",
        POLL_TIMEOUT,
        || async {
            let transcript = backend.transcript(session_id).await?;
            let Some(anchor) = transcript.anchor_of_user(message_id) else {
                return Ok(None);
            };
            let turn = transcript.turn_for_user(&anchor);
            Ok((turn.complete || turn.error.is_some()).then_some(transcript))
        },
        || server.stderr(),
    )
    .await
}

/// The failure the turn anchored at `message_id` recorded, if any — the
/// neutral projection the Turn reads (ADR-0056).
fn turn_error(transcript: &crate::backend::SessionTranscript, message_id: &str) -> Option<String> {
    let anchor = transcript.anchor_of_user(message_id)?;
    transcript.turn_for_user(&anchor).error
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
    assert!(
        transcript.anchor_of_user(message_id).is_some(),
        "a live message carries a server time"
    );
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

/// The question tool call ran to completion: the fragmented arguments
/// reassembled into the scripted question and options, and the answered form
/// (not a cancellation) came back.
fn assert_question_tool_call(transcript: &crate::backend::SessionTranscript) {
    let tool = transcript
        .messages
        .iter()
        .flat_map(|message| message.parts.iter())
        .find_map(|part| match part {
            Part::Tool(tool) if tool.identity.name == provider::Tool::Question.name() => Some(tool),
            _ => None,
        })
        .unwrap_or_else(|| panic!("the question tool call must be in the transcript: {transcript:#?}"));
    assert_eq!(
        tool.status,
        ToolStatus::Completed,
        "the answered question tool must have completed: {tool:#?}"
    );
    let input = tool.input.as_ref().expect("the question tool carries its input");
    let first = input
        .get("questions")
        .and_then(Value::as_array)
        .and_then(|questions| questions.first())
        .unwrap_or_else(|| panic!("the reassembled input must carry questions: {tool:#?}"));
    assert_eq!(
        first.get("question").and_then(Value::as_str),
        Some(provider::QUESTION_TEXT),
        "the split argument deltas must reassemble into the scripted question: {tool:#?}"
    );
    assert_eq!(
        first
            .get("options")
            .and_then(Value::as_array)
            .and_then(|options| options.first())
            .and_then(|option| option.get("label"))
            .and_then(Value::as_str),
        Some(provider::QUESTION_OPTION),
        "the scripted options must reassemble: {tool:#?}"
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
    let anchor = transcript
        .anchor_of_user(message_id)
        .expect("the user message carries a server time");
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
