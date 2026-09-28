//! The post-prompt render drain (ADR-0043): a Supplement that misses the
//! running run starts a new Turn on the server, whose reply the synchronous
//! prompt's render loop can no longer see. Between the prompt returning and
//! finalization the drain keeps polling under the inflight guard — busy, or an
//! unanswered cola-authored Supplement newer than the turn anchor — and renders
//! into the live (continuation) card, then finalization re-checks once before
//! the card is marked Done and the guard released.

use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use crate::backend::{
    ContentBlock, FinishReason, MessageRole, Part, SessionTranscript, StepFinish, TextPart, ToolOutput,
    ToolStatus, TranscriptMessage,
};
use crate::bridge::test_support::*;
use crate::bridge::turn::{PromptContext, Turn};
use crate::config::ThreadKey;
use crate::feishu::card::CardState;
use crate::opencode::types::SessionStatus;

/// A user message cola would have persisted (its `msg_cola_` id identifies it).
pub(crate) fn user(id: &str, created: i64, text: &str) -> TranscriptMessage {
    typed_message(id, MessageRole::User, Some(created), vec![text_part(text)])
}

/// A finished assistant turn whose only visible content is `text`.
fn assistant(created: i64, text: &str) -> TranscriptMessage {
    typed_message(
        &format!("msg_a_{created}"),
        MessageRole::Assistant,
        Some(created),
        vec![
            Part::Text(TextPart {
                text: text.to_string(),
                started_at: None,
            }),
            Part::StepFinish(StepFinish {
                reason: FinishReason::Stop,
            }),
        ],
    )
}

/// An assistant message whose only content is one `bash` tool call in the
/// given state — the #284 fixture: a panel still `running` when the drain
/// bound lands, whose later `completed` update must still reach the card.
fn tool_assistant(created: i64, status: ToolStatus, output: &str) -> TranscriptMessage {
    typed_message(
        &format!("msg_tool_{created}"),
        MessageRole::Assistant,
        Some(created),
        vec![
            tool_part(
                "bash",
                "call_1",
                status,
                serde_json::json!({ "command": "sleep 600" }),
                output,
            ),
            Part::StepFinish(StepFinish {
                reason: FinishReason::ToolCalls,
            }),
        ],
    )
}

/// The turn under test: the anchor id is fixed so the scripted Backend timeline can name
/// the turn's own user message (`capture_turn_anchor` matches on it).
pub(crate) fn ctx(session_id: &str, text: &str) -> PromptContext {
    PromptContext {
        session_id: session_id.into(),
        thread_key: ThreadKey::new("chat_1".into(), "chat_1".into()),
        text: text.into(),
        message_id: "msg_1".into(),
        subtitle: "p2p".into(),
        is_retry: false,
        requester_open_id: None,
        is_group: false,
        cola_message_id: Some("msg_cola_anchor".into()),
        images: Vec::new(),
    }
}

/// An app whose Backend serves the scripted transcripts for `ses_test` (one
/// per `transcript` call, the last repeating) and the given session status.
pub(crate) async fn scripted_app(
    scripts: Vec<SessionTranscript>,
    status: Option<SessionStatus>,
) -> (
    tempfile::TempDir,
    Arc<App>,
    Arc<MockBackend>,
    Arc<RecordingPlatform>,
) {
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let mut backend = MockBackend::new(realistic_parts());
    backend.given_transcript("ses_test", scripts);
    if let Some(status) = status {
        backend.with_session_status("ses_test", Some(status));
    }
    let backend = Arc::new(backend);
    let platform = Arc::new(RecordingPlatform::new());
    let app = Arc::new(App::new(cfg, backend.clone(), platform.clone()).unwrap());
    seed_session(&app, "ses_test", "/work").await;
    // Tiny cadences so the whole lifecycle runs in milliseconds; the timeouts
    // below assert the exits are state-driven, not the default 10 min bound.
    app.turn_render_poll_ms.store(5, Ordering::Relaxed);
    app.turn_drain_timeout_ms.store(60_000, Ordering::Relaxed);
    // A tiny per-read bound: a scripted hung read must fail fast instead of
    // eating a test's whole grace.
    app.turn_follow_read_timeout_ms.store(20, Ordering::Relaxed);
    (dir, app, backend, platform)
}

/// Start the turn on its own task (the test drives the Backend while it drains).
pub(crate) fn spawn_turn(
    app: &Arc<App>,
    context: PromptContext,
) -> tokio::task::JoinHandle<crate::error::Result<()>> {
    let app = Arc::clone(app);
    tokio::spawn(async move { Turn::run(&app.turn_handles(), context).await })
}

/// The #284 scenario app: a Supplement whose `bash` tool is still `running`
/// when the drain bound lands, on a Busy session, with a tiny drain bound and
/// the given follow grace.
async fn busy_supplement_app(
    follow_grace_ms: u64,
) -> (
    tempfile::TempDir,
    Arc<App>,
    Arc<MockBackend>,
    Arc<RecordingPlatform>,
) {
    let timeline = vec![
        user("msg_cola_anchor", 1_000, "第一条消息"),
        assistant(2_000, "第一轮回答。"),
        user("msg_cola_supp", 3_000, "补充一下"),
        tool_assistant(4_000, ToolStatus::Running, ""),
    ];
    let (dir, app, backend, platform) =
        scripted_app(vec![SessionTranscript::new(timeline)], Some(SessionStatus::Busy)).await;
    app.turn_drain_timeout_ms.store(30, Ordering::Relaxed);
    app.turn_follow_grace_ms.store(follow_grace_ms, Ordering::Relaxed);
    (dir, app, backend, platform)
}

/// Run the #284 scenario turn to its drain-bound hand-off: the running panel
/// is on the card, and `Turn::run` returning IS the hand-off (the bound was
/// reached and the guard released inside it).
async fn run_to_handoff(app: &Arc<App>, platform: &RecordingPlatform) {
    let turn = spawn_turn(app, ctx("ses_test", "第一条消息"));
    wait_for_card_text(platform, "⏳ bash").await;
    let result = tokio::time::timeout(Duration::from_secs(5), turn)
        .await
        .expect("the turn must hand off at the drain bound")
        .unwrap();
    result.unwrap();
    assert!(
        !app.inflight.lock().await.contains("ses_test"),
        "the guard must be released at the bound"
    );
}

/// Settle the scenario's `bash` panel in the scripted transcript the follow
/// reads (`status`/`output` — the server's own part update).
async fn settle_tool(backend: &Arc<MockBackend>, status: ToolStatus, output: &str) {
    let mut scripts = backend.transcript_scripts.lock().await;
    let transcript = &mut scripts.get_mut("ses_test").unwrap()[0];
    let message = transcript.messages.last_mut().unwrap();
    for part in message.parts.iter_mut() {
        if let Part::Tool(call) = part {
            call.status = status.clone();
            call.output = ToolOutput {
                raw: Some(serde_json::json!(output)),
                blocks: vec![ContentBlock::Text(output.to_string())],
                error: None,
            };
        }
    }
}

/// Await a card update carrying `needle`, or panic after 5 s.
pub(crate) async fn wait_for_card_text(platform: &RecordingPlatform, needle: &str) {
    wait_for_card_update(
        platform,
        &format!("card text {needle:?}"),
        CardUpdates::Any,
        |card| card_text(card).contains(needle),
    )
    .await;
}

/// Await a card update whose header carries `needle`, or panic after 5 s — the
/// follow finalizes out of turn, so tests wait on the card, not on a handle.
async fn wait_for_card_header(platform: &RecordingPlatform, needle: &str) {
    wait_for_card_update(
        platform,
        &format!("card header {needle:?}"),
        CardUpdates::Any,
        |card| card_header(card).contains(needle),
    )
    .await;
}

/// The drain must stop touching the Backend and the card when the turn ends:
/// no further transcript reads and no further card PATCHes.
async fn assert_no_further_rendering(backend: &Arc<MockBackend>, platform: &RecordingPlatform) {
    let reads = backend.transcript_calls.lock().await.len();
    let patches = platform.updated_cards().await.len();
    tokio::time::sleep(Duration::from_millis(30)).await;
    assert_eq!(
        backend.transcript_calls.lock().await.len(),
        reads,
        "rendering must stop with the turn"
    );
    assert_eq!(
        platform.updated_cards().await.len(),
        patches,
        "no card PATCH may continue after the turn"
    );
}

/// With a scripted backend where the first run ends and a cola-authored
/// supplement plus its assistant reply follow, updates continue on the live
/// card: while the supplement is unanswered the drain keeps polling and
/// renders; once the answer lands, the drain exits and the turn finishes.
#[tokio::test]
async fn drain_renders_the_new_turns_reply_on_the_live_card() {
    let _wd = test_work_dir();
    let timeline = vec![
        user("msg_cola_anchor", 1_000, "第一条消息"),
        assistant(2_000, "第一轮回答。"),
        user("msg_cola_supp", 3_000, "补充一下"),
    ];
    let (_dir, app, backend, platform) =
        scripted_app(vec![SessionTranscript::new(timeline)], Some(SessionStatus::Idle)).await;

    let turn = spawn_turn(&app, ctx("ses_test", "第一条消息"));

    // The unanswered supplement keeps the drain alive; it renders the first
    // run's reply onto the live card while the guard is still held.
    wait_for_card_text(&platform, "第一轮回答。").await;
    assert!(
        app.inflight.lock().await.contains("ses_test"),
        "the inflight guard must be held during the drain"
    );
    assert_eq!(
        Turn::card_state(&app.cards_handle(), "ses_test").await,
        Some(CardState::Streaming),
        "the drain renders into the live card, not the final state"
    );

    // The supplement's new Turn answers: an assistant message after it.
    backend
        .transcript_scripts
        .lock()
        .await
        .get_mut("ses_test")
        .unwrap()[0]
        .messages
        .push(assistant(4_000, "补充后的回答。"));

    let result = tokio::time::timeout(Duration::from_secs(5), turn)
        .await
        .expect("the turn must finish once the supplement is answered")
        .unwrap();
    result.unwrap();

    let updates = platform.updated_cards().await;
    let final_card = updates.last().expect("the final card flush");
    assert!(
        card_text(final_card).contains("补充后的回答。"),
        "the new Turn's reply must render on the live card: {final_card}"
    );
    assert!(
        card_header(final_card).contains("完成"),
        "the final card must be marked Done: {}",
        card_header(final_card)
    );
    assert!(
        !app.inflight.lock().await.contains("ses_test"),
        "the busy guard must be released"
    );
    assert_no_further_rendering(&backend, &platform).await;
}

/// A supplement racing the drain's exit is not dropped: the pre-finalization
/// re-check sees it and drains it, so the turn keeps rendering (guard held,
/// card streaming) instead of finalizing around it.
#[tokio::test]
async fn finish_rechecks_for_a_supplement_racing_the_drain_exit() {
    let _wd = test_work_dir();
    let quiet = vec![
        user("msg_cola_anchor", 1_000, "第一条消息"),
        assistant(2_000, "第一轮回答。"),
    ];
    let raced = vec![
        user("msg_cola_anchor", 1_000, "第一条消息"),
        assistant(2_000, "第一轮回答。"),
        user("msg_cola_supp", 3_000, "补充一下"),
    ];
    // Snapshot 1 (the drain's exit check): nothing pending. Snapshot 2 (the
    // finish re-check): the supplement landed, still unanswered.
    let (_dir, app, backend, platform) = scripted_app(
        vec![SessionTranscript::new(quiet), SessionTranscript::new(raced)],
        Some(SessionStatus::Idle),
    )
    .await;

    let turn = spawn_turn(&app, ctx("ses_test", "第一条消息"));

    // The re-check drained the supplement: the first run's reply renders on a
    // STREAMING flush, the guard stays held, and the turn is still running —
    // a supplement ignored by finalization would have finished the turn (Done
    // header, guard released) instead.
    wait_for_card_text(&platform, "第一轮回答。").await;
    let rendered = platform.updated_cards().await;
    let first = rendered
        .iter()
        .find(|c| card_text(c).contains("第一轮回答。"))
        .expect("the first run's reply");
    assert!(
        !card_header(first).contains("完成"),
        "the drain must render before finalization: {}",
        card_header(first)
    );
    assert!(
        app.inflight.lock().await.contains("ses_test"),
        "the racing supplement must keep the guard held"
    );

    // The new Turn answers; the drain picks it up and the turn finishes.
    backend
        .transcript_scripts
        .lock()
        .await
        .get_mut("ses_test")
        .unwrap()[0]
        .messages
        .push(assistant(4_000, "补充后的回答。"));
    let result = tokio::time::timeout(Duration::from_secs(5), turn)
        .await
        .expect("the turn must finish once the supplement is answered")
        .unwrap();
    result.unwrap();

    let final_card = platform.updated_cards().await.last().cloned().unwrap();
    assert!(card_text(&final_card).contains("补充后的回答。"));
    assert!(card_header(&final_card).contains("完成"), "final card Done");
    assert!(!app.inflight.lock().await.contains("ses_test"));
}

/// A supplement (or any message) arriving while the drain runs is still
/// handled as a Supplement — no busy notice — because the guard is held.
#[tokio::test]
async fn a_message_during_the_drain_is_handled_as_a_supplement() {
    let _wd = test_work_dir();
    let timeline = vec![
        user("msg_cola_anchor", 1_000, "第一条消息"),
        assistant(2_000, "第一轮回答。"),
        user("msg_cola_supp", 3_000, "补充一下"),
    ];
    let (_dir, app, backend, platform) =
        scripted_app(vec![SessionTranscript::new(timeline)], Some(SessionStatus::Idle)).await;

    let turn = spawn_turn(&app, ctx("ses_test", "第一条消息"));
    wait_for_card_text(&platform, "第一轮回答。").await;
    assert!(
        app.inflight.lock().await.contains("ses_test"),
        "precondition: the drain holds the guard"
    );

    app.handle_message(incoming(
        "msg_sup2".into(),
        "chat_1".into(),
        "p2p".into(),
        None,
        "补充二".into(),
        None,
    ))
    .await;

    assert!(
        backend.prompt_calls.lock().await.iter().any(|c| c == "补充二"),
        "the message must queue as a supplement: {:?}",
        backend.prompt_calls.lock().await
    );
    assert!(
        !platform.texts().await.iter().any(|t| t.contains("还在处理中")),
        "a supplement during the drain must not hit the busy notice: {:?}",
        platform.calls.lock().await
    );
    assert!(
        platform
            .calls
            .lock()
            .await
            .iter()
            .any(|c| matches!(c, PlatformCall::ReplyCard { reply_to, .. } if reply_to == "msg_sup2")),
        "the supplement splits the chain onto a continuation: {:?}",
        platform.calls.lock().await
    );
    assert!(
        app.inflight.lock().await.contains("ses_test"),
        "the drain keeps holding the guard"
    );

    // Answer the second supplement and let the turn finish.
    backend
        .transcript_scripts
        .lock()
        .await
        .get_mut("ses_test")
        .unwrap()[0]
        .messages
        .push(assistant(4_000, "补充二的回答。"));
    let result = tokio::time::timeout(Duration::from_secs(5), turn)
        .await
        .expect("the turn must finish")
        .unwrap();
    result.unwrap();

    let final_card = platform.updated_cards().await.last().cloned().unwrap();
    assert!(card_text(&final_card).contains("补充二的回答。"));
    assert!(card_header(&final_card).contains("完成"));
    assert!(!app.inflight.lock().await.contains("ses_test"));
    assert_no_further_rendering(&backend, &platform).await;
}

/// `/stop` ends the drain promptly: the interrupt stops the session, the stop
/// marker makes the next drain tick finalize even though the aborted new Turn
/// left the supplement unanswered, and no rendering continues — the 60 s bound
/// is never reached. The card finalizes `Stopped` (#394).
#[tokio::test]
async fn stop_ends_the_drain_promptly() {
    let _wd = test_work_dir();
    let timeline = vec![
        user("msg_cola_anchor", 1_000, "第一条消息"),
        assistant(2_000, "第一轮回答。"),
        user("msg_cola_supp", 3_000, "补充一下"),
    ];
    let (_dir, app, backend, platform) =
        scripted_app(vec![SessionTranscript::new(timeline)], Some(SessionStatus::Idle)).await;

    let turn = spawn_turn(&app, ctx("ses_test", "第一条消息"));
    wait_for_card_text(&platform, "第一轮回答。").await;

    app.handle_message(incoming(
        "msg_stop".into(),
        "chat_1".into(),
        "p2p".into(),
        None,
        "/stop".into(),
        None,
    ))
    .await;
    assert_eq!(
        backend.interrupt_calls.lock().await.as_slice(),
        &["ses_test".to_string()],
        "/stop must interrupt the session"
    );

    let started = std::time::Instant::now();
    let result = tokio::time::timeout(Duration::from_secs(5), turn)
        .await
        .expect("the drain must end on the stop, not the 60 s bound")
        .unwrap();
    result.unwrap();
    assert!(
        started.elapsed() < Duration::from_secs(3),
        "the drain must end promptly after /stop"
    );
    assert!(
        platform.texts().await.iter().any(|t| t.contains("Interrupted")),
        "the command reply still lands: {:?}",
        platform.calls.lock().await
    );
    let final_card = platform.updated_cards().await.last().cloned().unwrap();
    assert!(
        card_header(&final_card).contains("已停止"),
        "a deliberate /stop finalizes Stopped, not Done: {final_card}"
    );
    assert!(
        card_buttons(&final_card).is_empty(),
        "a stopped card must not offer a retry: {final_card}"
    );
    assert!(!app.inflight.lock().await.contains("ses_test"));
    assert_no_further_rendering(&backend, &platform).await;
}

/// A deliberate `/stop` is not a failure (#394): the abort the server records
/// on the transcript must not reach the card, and the card finalizes Stopped —
/// not Error — with no retry button.
#[tokio::test]
async fn a_stopped_turn_discards_the_abort_error() {
    let _wd = test_work_dir();
    let mut aborted = assistant(2_000, "");
    aborted.error = Some("Aborted".into());
    let timeline = vec![user("msg_cola_anchor", 1_000, "第一条消息"), aborted];
    // Busy keeps the drain polling until the stop marker ends it, so the stop
    // lands before finalization.
    let (_dir, app, backend, platform) =
        scripted_app(vec![SessionTranscript::new(timeline)], Some(SessionStatus::Busy)).await;

    let turn = spawn_turn(&app, ctx("ses_test", "第一条消息"));
    wait_for_card_header(&platform, "思考中").await;
    app.handle_message(incoming(
        "msg_stop".into(),
        "chat_1".into(),
        "p2p".into(),
        None,
        "/stop".into(),
        None,
    ))
    .await;

    let result = tokio::time::timeout(Duration::from_secs(5), turn)
        .await
        .expect("the drain must end on the stop, not the 60 s bound")
        .unwrap();
    result.unwrap();

    let final_card = platform.updated_cards().await.last().cloned().unwrap();
    assert!(
        card_header(&final_card).contains("已停止"),
        "a stopped turn finalizes Stopped: {final_card}"
    );
    assert!(
        !card_text(&final_card).contains("Aborted") && !card_text(&final_card).contains("出错"),
        "the transcript's abort error is not a failure and must not reach the card: {final_card}"
    );
    assert!(
        card_buttons(&final_card).is_empty(),
        "a stopped card must not offer a retry: {final_card}"
    );
    assert_no_further_rendering(&backend, &platform).await;
}

/// A deliberate stop's completion notice says 已停止 (#394): never 完成, never
/// 出错.
#[tokio::test]
async fn a_stopped_group_turn_notices_stopped() {
    let _wd = test_work_dir();
    let timeline = vec![
        user("msg_cola_anchor", 1_000, "第一条消息"),
        assistant(2_000, "第一轮回答。"),
        user("msg_cola_supp", 3_000, "补充一下"),
    ];
    let (_dir, app, _backend, platform) =
        scripted_app(vec![SessionTranscript::new(timeline)], Some(SessionStatus::Idle)).await;

    let mut context = ctx("ses_test", "第一条消息");
    context.is_group = true;
    context.requester_open_id = Some(TEST_HOST.to_string());
    let turn = spawn_turn(&app, context);
    wait_for_card_text(&platform, "第一轮回答。").await;

    app.handle_message(incoming(
        "msg_stop".into(),
        "chat_1".into(),
        "p2p".into(),
        None,
        "/stop".into(),
        None,
    ))
    .await;
    let result = tokio::time::timeout(Duration::from_secs(5), turn)
        .await
        .expect("the drain must end on the stop, not the 60 s bound")
        .unwrap();
    result.unwrap();

    let calls = platform.calls.lock().await.clone();
    let notices: Vec<&str> = calls
        .iter()
        .filter_map(|c| match c {
            PlatformCall::CompletionNotice { text, .. } => Some(text.as_str()),
            _ => None,
        })
        .collect();
    assert!(
        notices.contains(&"⏹ 已停止。"),
        "the stopped notice must be used: {notices:?}"
    );
    assert!(
        !notices
            .iter()
            .any(|text| text.contains("已完成") || text.contains("出错")),
        "a stop is never announced as 完成 or 出错: {notices:?}"
    );
}

/// ADR-0048: the stopped-finalization is one state transition per Turn. The
/// post-prompt drain and its pre-finalization re-check both observe the same
/// `/stop` marker, so the line must be logged once, not once per phase.
#[tokio::test]
async fn a_stopped_turn_logs_finalizing_once_per_turn() {
    let _wd = test_work_dir();
    let timeline = vec![
        user("msg_cola_anchor", 1_000, "第一条消息"),
        assistant(2_000, "第一轮回答。"),
        user("msg_cola_supp", 3_000, "补充一下"),
    ];
    let (_dir, app, _backend, platform) =
        scripted_app(vec![SessionTranscript::new(timeline)], Some(SessionStatus::Idle)).await;

    let (_, logs) = capture_logs(async {
        let turn = spawn_turn(&app, ctx("ses_test", "第一条消息"));
        wait_for_card_text(&platform, "第一轮回答。").await;
        app.handle_message(incoming(
            "msg_stop".into(),
            "chat_1".into(),
            "p2p".into(),
            None,
            "/stop".into(),
            None,
        ))
        .await;
        let result = tokio::time::timeout(Duration::from_secs(5), turn)
            .await
            .expect("the drain must end on the stop")
            .unwrap();
        result.unwrap();
    })
    .await;

    let finalizing: Vec<&str> = logs
        .lines()
        .filter(|line| line.contains("was stopped; finalizing"))
        .collect();
    assert_eq!(
        finalizing.len(),
        1,
        "finalizing must be logged once per Turn: {finalizing:?}\n{logs}"
    );
}

/// A session that stays busy runs the drain to its bound. The TURN ends there
/// (the guard is released, so the next message is a normal new Turn), but the
/// CARD does not: it is handed to the out-of-turn follow. The follow has no
/// total budget (#386) — a readable run stays live for as long as it runs —
/// so only the session going idle finalizes it, and only then Done (#284).
#[tokio::test]
async fn the_drain_bound_exits_cleanly_and_finishes_the_turn() {
    let _wd = test_work_dir();
    let timeline = vec![
        user("msg_cola_anchor", 1_000, "第一条消息"),
        assistant(2_000, "第一轮回答。"),
    ];
    let (_dir, app, backend, platform) =
        scripted_app(vec![SessionTranscript::new(timeline)], Some(SessionStatus::Busy)).await;
    // Tiny bounds so the long-poll branch and the follow grace run in
    // milliseconds.
    app.turn_drain_timeout_ms.store(30, Ordering::Relaxed);
    app.turn_follow_grace_ms.store(30, Ordering::Relaxed);
    // Group + requester: the follow's finalization sends the completion notice.
    let mut context = ctx("ses_test", "第一条消息");
    context.is_group = true;
    context.requester_open_id = Some(TEST_HOST.to_string());

    Turn::run(&app.turn_handles(), context).await.unwrap();

    // The turn released the guard at the bound: a message arriving now is a
    // normal new Turn, exactly as before the follow existed.
    assert!(
        !app.inflight.lock().await.contains("ses_test"),
        "the guard must be released at the bound"
    );
    assert!(
        backend.transcript_calls.lock().await.len() > 1,
        "the drain must keep polling while the session is busy"
    );

    // The follow keeps the card live well past the grace while the run stays
    // readable: no total budget, no forced Error.
    tokio::time::sleep(Duration::from_millis(150)).await;
    assert!(
        !platform
            .updated_cards()
            .await
            .iter()
            .any(|c| card_header(c).contains("出错")),
        "a readable run must not error on a budget: {:?}",
        platform.updated_cards().await
    );
    assert_eq!(
        Turn::card_state(&app.cards_handle(), "ses_test").await,
        Some(CardState::Streaming),
        "the card stays live while the session is readable and busy"
    );

    // It genuinely ends: the session goes idle and the follow finalizes Done.
    backend
        .set_session_status("ses_test", Some(SessionStatus::Idle))
        .await;
    wait_for_card_header(&platform, "完成").await;
    let final_card = platform.updated_cards().await.last().cloned().unwrap();
    assert!(card_text(&final_card).contains("第一轮回答。"));
    assert!(
        platform
            .calls
            .lock()
            .await
            .iter()
            .any(|c| matches!(c, PlatformCall::CompletionNotice { text, .. } if text.contains("已完成"))),
        "the completion notice reports the real end: {:?}",
        platform.calls.lock().await
    );
    assert_no_further_rendering(&backend, &platform).await;
}

/// #284 regression: a Supplement whose tool part is still `running` at the
/// drain bound must not freeze the card. The follow keeps rendering the SAME
/// card past the bound — not Done, the panel still live — refuses to finalize
/// even when the session reports idle while the panel is still live, and ends
/// Done with the tool's completion once the panel settles.
#[tokio::test]
async fn a_running_supplement_panel_rides_past_the_drain_bound_until_idle() {
    let _wd = test_work_dir();
    // A ceiling the test would never wait out: only the panel settling ends it.
    let (_dir, app, backend, platform) = busy_supplement_app(60_000).await;

    run_to_handoff(&app, &platform).await;
    assert_eq!(
        Turn::card_state(&app.cards_handle(), "ses_test").await,
        Some(CardState::Streaming),
        "the hand-off must leave the card live, not Done"
    );
    let live = platform.updated_cards().await.last().cloned().unwrap();
    assert!(
        !card_header(&live).contains("完成"),
        "the card must not be Done at the bound: {}",
        card_header(&live)
    );
    assert!(
        card_text(&live).contains("⏳ bash"),
        "the running panel stays on the live card: {live}"
    );

    // The session reports idle while the panel is STILL live: the card must
    // not finalize Done over a `⏳` — it keeps watching (the invariant).
    backend
        .set_session_status("ses_test", Some(SessionStatus::Idle))
        .await;
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(
        Turn::card_state(&app.cards_handle(), "ses_test").await,
        Some(CardState::Streaming),
        "an idle session with a running panel must not finalize Done"
    );

    // The tool settles; the next tick renders it and finalizes Done.
    settle_tool(&backend, ToolStatus::Completed, "done").await;
    wait_for_card_header(&platform, "完成").await;
    let final_card = platform.updated_cards().await.last().cloned().unwrap();
    assert!(
        card_text(&final_card).contains("done"),
        "the tool's completion must render after the bound: {final_card}"
    );
    assert!(
        !card_text(&final_card).contains("⏳ bash"),
        "no running panel may remain on the Done card: {final_card}"
    );
    // The invariant, over every card the turn ever sent.
    for card in platform.updated_cards().await.iter() {
        assert!(
            !(card_header(card).contains("完成") && card_text(card).contains("⏳ bash")),
            "a Done card must never show a running panel: {card}"
        );
    }
    assert_no_further_rendering(&backend, &platform).await;
}

/// A followed long turn that ends in a provider failure must finalize Error,
/// not Done: the failure lives on the transcript's newest assistant message,
/// and the out-of-turn follow reads the same shared rule the Turn's own
/// finalization uses (ADR-0056).
#[tokio::test]
async fn a_followed_failure_finalizes_error_not_done() {
    let _wd = test_work_dir();
    // A ceiling the test would never wait out: only the settled failure ends it.
    let (_dir, app, backend, platform) = busy_supplement_app(60_000).await;

    run_to_handoff(&app, &platform).await;

    // The run settles: the tool completes and the server records the provider
    // failure on the newest assistant message, with the session idle.
    settle_tool(&backend, ToolStatus::Completed, "done").await;
    {
        let mut scripts = backend.transcript_scripts.lock().await;
        let transcript = &mut scripts.get_mut("ses_test").unwrap()[0];
        transcript.messages.last_mut().unwrap().error = Some("provider 503".into());
    }
    backend
        .set_session_status("ses_test", Some(SessionStatus::Idle))
        .await;

    wait_for_card_header(&platform, "出错").await;
    let final_card = platform.updated_cards().await.last().cloned().unwrap();
    assert!(card_header(&final_card).contains("出错"), "final card Error");
    assert!(
        card_text(&final_card).contains("provider 503"),
        "the transcript's failure must reach the card: {final_card}"
    );
    assert_no_further_rendering(&backend, &platform).await;
}

/// `/stop` during the follow finalizes the card promptly — the same sticky
/// stopped-session marker the drain observes — instead of waiting out the
/// follow's ceiling.
#[tokio::test]
async fn stop_during_the_follow_finalizes_promptly() {
    let _wd = test_work_dir();
    let (_dir, app, backend, platform) = busy_supplement_app(60_000).await;
    run_to_handoff(&app, &platform).await;

    // The interrupt settles the running tool server-side (OpenCode writes it
    // as `error`): mirror that, so the final render shows the aborted panel.
    settle_tool(&backend, ToolStatus::Error, "Tool execution aborted").await;

    let started = std::time::Instant::now();
    app.handle_message(incoming(
        "msg_stop".into(),
        "chat_1".into(),
        "p2p".into(),
        None,
        "/stop".into(),
        None,
    ))
    .await;
    assert_eq!(
        backend.interrupt_calls.lock().await.as_slice(),
        &["ses_test".to_string()],
        "/stop must interrupt the session"
    );

    wait_for_card_header(&platform, "已停止").await;
    assert!(
        started.elapsed() < Duration::from_secs(3),
        "the follow must end on the stop, not the 60 s grace"
    );
    let final_card = platform.updated_cards().await.last().cloned().unwrap();
    assert!(
        card_buttons(&final_card).is_empty(),
        "a stopped card must not offer a retry: {final_card}"
    );
    assert!(
        platform.texts().await.iter().any(|t| t.contains("Interrupted")),
        "the command reply still lands: {:?}",
        platform.calls.lock().await
    );
    assert_no_further_rendering(&backend, &platform).await;
}

/// The follow's mid-tick stop re-check (#394): a stop landing AFTER a tick's
/// top marker check — while its transcript read is already in flight — must
/// still finalize `Stopped`, not the settled run's Done. The gate parks the
/// follow's read (the only transcript reader after the hand-off), so the
/// `/stop` deterministically lands inside the tick.
#[tokio::test]
async fn a_stop_landing_mid_tick_finalizes_stopped_not_done() {
    let _wd = test_work_dir();
    // A ceiling the test would never wait out: only the stop can end it.
    let (_dir, app, backend, platform) = busy_supplement_app(60_000).await;
    // The parked read must stay parked until the test releases it: the
    // follow's default per-read bound would cancel it first.
    app.turn_follow_read_timeout_ms.store(60_000, Ordering::Relaxed);
    run_to_handoff(&app, &platform).await;

    // Park the follow's next read. It has already passed the tick's top
    // marker check (no marker is set yet), so only the mid-tick re-check can
    // see the stop that lands below.
    let gate = backend.hold_transcripts();
    let entered = backend
        .transcript_gate_entered
        .load(std::sync::atomic::Ordering::SeqCst);
    let wait_parked = async {
        while backend
            .transcript_gate_entered
            .load(std::sync::atomic::Ordering::SeqCst)
            == entered
        {
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
    };
    tokio::time::timeout(Duration::from_secs(3), wait_parked)
        .await
        .expect("the follow's read must reach the gate");

    // The run settles while the tick is parked: without the re-check the
    // tick would classify it Done.
    settle_tool(&backend, ToolStatus::Completed, "done").await;
    backend
        .set_session_status("ses_test", Some(SessionStatus::Idle))
        .await;
    app.handle_message(incoming(
        "msg_stop".into(),
        "chat_1".into(),
        "p2p".into(),
        None,
        "/stop".into(),
        None,
    ))
    .await;
    assert_eq!(
        backend.interrupt_calls.lock().await.as_slice(),
        &["ses_test".to_string()],
        "/stop must interrupt the session"
    );

    gate.add_permits(1);
    wait_for_card_header(&platform, "已停止").await;
    assert_eq!(
        Turn::card_state(&app.cards_handle(), "ses_test").await,
        Some(CardState::Stopped),
        "the mid-tick stop must finalize Stopped, not the settled run's Done"
    );
    let final_card = platform.updated_cards().await.last().cloned().unwrap();
    assert!(
        card_buttons(&final_card).is_empty(),
        "a stopped card must not offer a retry: {final_card}"
    );
    assert_no_further_rendering(&backend, &platform).await;
}

/// A follow whose Backend reads hang ends in the lost-contact Error: the card
/// never sits on an eternal "streaming" state over a run cola cannot see
/// (#284/#386).
#[tokio::test]
async fn a_hung_backend_ends_the_follow_in_error() {
    let _wd = test_work_dir();
    let (_dir, app, backend, platform) = busy_supplement_app(50).await;
    run_to_handoff(&app, &platform).await;

    // Every transcript read the follow makes now hangs (a wedged per-session
    // read); the status read alone is not a full read pair, so the
    // lost-contact grace ends the card.
    backend.hang_transcript_reads(100);

    let started = std::time::Instant::now();
    wait_for_card_header(&platform, "出错").await;
    assert!(
        started.elapsed() < Duration::from_secs(3),
        "the hung read must end at the follow's grace, not the per-read bound"
    );
    let final_card = platform.updated_cards().await.last().cloned().unwrap();
    assert!(
        !card_header(&final_card).contains("完成"),
        "never Done under a hung run"
    );
    assert!(
        card_text(&final_card).contains("失去联系"),
        "the lost-contact copy: {final_card}"
    );
    assert_no_further_rendering(&backend, &platform).await;
}

/// A live `⏳` panel on a readable, idle session that never settles is an
/// unreconcilable panel: the follow gives it the grace to settle, then ends
/// Error — never Done over a `⏳`, never an eternal card (#386).
#[tokio::test]
async fn an_orphaned_panel_ends_the_follow_in_error() {
    let _wd = test_work_dir();
    let (_dir, app, backend, platform) = busy_supplement_app(50).await;
    run_to_handoff(&app, &platform).await;

    // The run is gone (idle) but its last panel stays `running`: a crash
    // orphan nothing will ever settle.
    backend
        .set_session_status("ses_test", Some(SessionStatus::Idle))
        .await;

    wait_for_card_header(&platform, "出错").await;
    let final_card = platform.updated_cards().await.last().cloned().unwrap();
    assert!(
        card_text(&final_card).contains("未收尾"),
        "the orphaned-panel copy: {final_card}"
    );
    assert!(
        card_text(&final_card).contains("⏳ bash"),
        "the panel is still shown under the error: {final_card}"
    );
    // The invariant, over every card the turn ever sent.
    for card in platform.updated_cards().await.iter() {
        assert!(
            !(card_header(card).contains("完成") && card_text(card).contains("⏳ bash")),
            "a Done card must never show a running panel: {card}"
        );
    }
    assert_no_further_rendering(&backend, &platform).await;
}

/// The lost-contact grace is CONTINUOUS: a read outage shorter than the grace
/// must not end the card — the next fully-answered tick resets it (#386).
#[tokio::test]
async fn a_successful_read_resets_the_lost_contact_grace() {
    let _wd = test_work_dir();
    let (_dir, app, backend, platform) = busy_supplement_app(500).await;
    run_to_handoff(&app, &platform).await;

    // Three hung transcript reads: no full read pair for well under 500 ms.
    backend.hang_transcript_reads(3);
    tokio::time::sleep(Duration::from_millis(120)).await;
    // The counter is spent; the next tick answers in full and resets the grace.
    tokio::time::sleep(Duration::from_millis(80)).await;
    assert!(
        !platform
            .updated_cards()
            .await
            .iter()
            .any(|c| card_header(c).contains("出错")),
        "a flap shorter than the grace must not end the card: {:?}",
        platform.updated_cards().await
    );

    // The same outage sustained without a full answer ends it.
    backend.hang_transcript_reads(100);
    wait_for_card_header(&platform, "出错").await;
    let final_card = platform.updated_cards().await.last().cloned().unwrap();
    assert!(
        card_text(&final_card).contains("失去联系"),
        "the lost-contact copy: {final_card}"
    );
    assert_no_further_rendering(&backend, &platform).await;
}

/// The wait scenario (#386): a Busy session with one pending permission
/// inlined on its live card, driven through the drain-bound hand-off.
async fn pending_permission_app() -> (
    tempfile::TempDir,
    Arc<App>,
    Arc<MockBackend>,
    Arc<RecordingPlatform>,
) {
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let mut backend = MockBackend::new(realistic_parts());
    backend.given_transcript(
        "ses_test",
        vec![SessionTranscript::new(vec![
            user("msg_cola_anchor", 1_000, "第一条消息"),
            assistant(2_000, "第一轮回答。"),
        ])],
    );
    backend.with_session_status("ses_test", Some(SessionStatus::Busy));
    backend.ask_permissions(vec![crate::opencode::types::PermissionRequest {
        request_id: "per_1".into(),
        session_id: Some("ses_test".into()),
        permission: Some("bash".into()),
        patterns: vec!["ls -la".into()],
        metadata: None,
        always: Vec::new(),
    }]);
    let backend = Arc::new(backend);
    let platform = Arc::new(RecordingPlatform::new());
    let app = Arc::new(App::new(cfg, backend.clone(), platform.clone()).unwrap());
    seed_session(&app, "ses_test", "/work").await;
    app.turn_render_poll_ms.store(5, Ordering::Relaxed);
    app.turn_drain_timeout_ms.store(30, Ordering::Relaxed);
    app.turn_follow_grace_ms.store(30, Ordering::Relaxed);
    app.turn_follow_read_timeout_ms.store(20, Ordering::Relaxed);

    // Run the turn to its hand-off, then surface the permission the way
    // `App::run` does — after the accumulator arms, so it inlines on the card.
    let turn = spawn_turn(&app, ctx("ses_test", "第一条消息"));
    let armed = async {
        while Turn::armed_turn_anchor(&app.cards_handle(), "ses_test")
            .await
            .is_none()
        {
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
    };
    tokio::time::timeout(Duration::from_secs(5), armed)
        .await
        .expect("the accumulator must arm");
    tokio::spawn({
        let app = Arc::clone(&app);
        async move {
            app.permission.poll_interval_ms.store(20, Ordering::Relaxed);
            let _ = app.permission.poll_loop(&app.flow_handles()).await;
        }
    });
    let result = tokio::time::timeout(Duration::from_secs(5), turn)
        .await
        .expect("the turn must hand off at the drain bound")
        .unwrap();
    result.unwrap();
    wait_for_card_header(&platform, "等待你的授权").await;
    (dir, app, backend, platform)
}

/// A Permission wait is unbounded (#386): the card keeps its 「等待你的授权」
/// header for as long as the operator takes — the wait is never converted
/// into an error and a retry — and answering resumes the run.
#[tokio::test]
async fn a_pending_permission_waits_past_the_grace_without_error() {
    let _wd = test_work_dir();
    let (_dir, app, backend, platform) = pending_permission_app().await;

    // The wait outlives the grace several times over: still waiting, no Error.
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(
        Turn::card_state(&app.cards_handle(), "ses_test").await,
        Some(CardState::Streaming),
        "a wait must leave the card live"
    );
    assert!(
        !platform
            .updated_cards()
            .await
            .iter()
            .any(|c| card_header(c).contains("出错")),
        "a wait must never become an error: {:?}",
        platform.updated_cards().await
    );
    assert!(
        platform
            .updated_cards()
            .await
            .last()
            .is_some_and(|c| card_header(c).contains("等待你的授权")),
        "the header still names the wait: {:?}",
        platform.updated_cards().await
    );

    // The operator answers elsewhere and the run settles: the follow finalizes
    // Done, not Error — the wait was never a failure.
    backend.permission_resolved_by_another("per_1").await;
    backend
        .set_session_status("ses_test", Some(SessionStatus::Idle))
        .await;
    wait_for_card_header(&platform, "完成").await;
    // The permission poller's sweep repaints the card with the
    // 「已由其他客户端处理」 receipt independently of the follow (one more
    // legitimate PATCH); account for it before asserting the drain stopped —
    // otherwise this races the sweep, not the drain.
    wait_for_card_update(
        &platform,
        "the handled-elsewhere receipt",
        CardUpdates::Any,
        |card| card_text(card).contains("已由其他客户端处理"),
    )
    .await;
    assert_no_further_rendering(&backend, &platform).await;
}

/// The wait is not a shield for a lost Backend (#386): a wait whose reads
/// stopped answering still ends in the lost-contact Error — a stale pending
/// record must not suspend the fallback forever.
#[tokio::test]
async fn a_pending_wait_with_lost_contact_still_ends_in_error() {
    let _wd = test_work_dir();
    let (_dir, _app, backend, platform) = pending_permission_app().await;

    backend.hang_transcript_reads(100);
    wait_for_card_header(&platform, "出错").await;
    let final_card = platform.updated_cards().await.last().cloned().unwrap();
    assert!(
        card_text(&final_card).contains("失去联系"),
        "the lost-contact copy under a wait: {final_card}"
    );
    assert_no_further_rendering(&backend, &platform).await;
}

/// The follow never holds the session: the guard was released at the bound, so
/// a message arriving after the hand-off is a normal new Turn (not a
/// supplement), and it replaces the accumulator the follow was watching.
#[tokio::test]
async fn a_message_after_the_bound_handoff_starts_a_normal_new_turn() {
    let _wd = test_work_dir();
    let (_dir, app, backend, platform) = busy_supplement_app(60_000).await;
    run_to_handoff(&app, &platform).await;

    let followed_anchor = Turn::armed_turn_anchor(&app.cards_handle(), "ses_test").await;
    assert_eq!(
        followed_anchor.as_ref().map(|anchor| anchor.created_ms),
        Some(1_000),
        "the follow watches the turn anchor"
    );

    app.handle_message(incoming(
        "msg_next".into(),
        "chat_1".into(),
        "p2p".into(),
        None,
        "接着问".into(),
        None,
    ))
    .await;

    assert!(
        backend.prompt_calls.lock().await.iter().any(|t| t == "接着问"),
        "the released session must take the normal prompt path: {:?}",
        backend.prompt_calls.lock().await
    );
    assert!(
        !platform.texts().await.iter().any(|t| t.contains("还在处理中")),
        "the new turn is not throttled: {:?}",
        platform.calls.lock().await
    );
    assert_ne!(
        Turn::armed_turn_anchor(&app.cards_handle(), "ses_test").await,
        followed_anchor,
        "the new Turn must replace the accumulator the follow watched"
    );
}

/// The other half of the racing-supplement guarantee: once the turn released
/// the guard, a new message is no longer a supplement — the normal prompt path
/// starts a fresh Turn, so it is never dropped either.
#[tokio::test]
async fn a_message_after_the_release_becomes_a_normal_new_turn() {
    let _wd = test_work_dir();
    let timeline = vec![
        user("msg_cola_anchor", 1_000, "第一条消息"),
        assistant(2_000, "第一轮回答。"),
    ];
    let (_dir, app, backend, platform) =
        scripted_app(vec![SessionTranscript::new(timeline)], Some(SessionStatus::Idle)).await;

    Turn::run(&app.turn_handles(), ctx("ses_test", "第一条消息"))
        .await
        .unwrap();
    assert!(
        !app.inflight.lock().await.contains("ses_test"),
        "the guard is released"
    );

    // The released session starts a fresh Turn; run it on its own task so the
    // test can serve the new turn's history once the handler has generated its
    // `msg_cola_` id (the scripted timeline above belongs to the first turn).
    let next_turn = {
        let app = Arc::clone(&app);
        tokio::spawn(async move {
            app.handle_message(incoming(
                "msg_next".into(),
                "chat_1".into(),
                "p2p".into(),
                None,
                "接着问".into(),
                None,
            ))
            .await;
        })
    };
    let next_id = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if let Some(Some(id)) = backend.prompt_message_ids.lock().await.last().cloned()
                && backend
                    .prompt_calls
                    .lock()
                    .await
                    .last()
                    .is_some_and(|text| text == "接着问")
            {
                return id;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("the new turn's prompt must be submitted");

    assert!(
        backend.prompt_calls.lock().await.iter().any(|t| t == "接着问"),
        "the released session must take the normal prompt path: {:?}",
        backend.prompt_calls.lock().await
    );
    assert!(
        !platform.texts().await.iter().any(|t| t.contains("还在处理中")),
        "the new turn is not throttled: {:?}",
        platform.calls.lock().await
    );

    // The new turn's answer closes it, so the turn task returns.
    *backend
        .transcript_scripts
        .lock()
        .await
        .get_mut("ses_test")
        .unwrap() = vec![SessionTranscript::new(vec![
        user(&next_id, 3_000, "接着问"),
        assistant(4_000, "新一轮回答。"),
    ])];
    tokio::time::timeout(Duration::from_secs(5), next_turn)
        .await
        .expect("the new turn must finish")
        .unwrap();

    assert!(!app.inflight.lock().await.contains("ses_test"));
}

/// A single hung Backend read is bounded by the drain budget, not the fixed
/// request timeout: with a tiny injected bound the drain ends at ~the bound
/// (not after 30 s) and finalization proceeds normally.
#[tokio::test]
async fn a_hung_backend_read_ends_the_drain_at_the_bound() {
    let _wd = test_work_dir();
    let timeline = vec![
        user("msg_cola_anchor", 1_000, "第一条消息"),
        assistant(2_000, "第一轮回答。"),
    ];
    let (_dir, app, backend, platform) =
        scripted_app(vec![SessionTranscript::new(timeline)], Some(SessionStatus::Busy)).await;
    app.turn_render_poll_ms.store(20, Ordering::Relaxed);
    app.turn_drain_timeout_ms.store(30, Ordering::Relaxed);
    // The first Backend read hangs forever (a half-open connection left by a
    // server restart); later reads serve normally.
    backend.hang_transcript_reads(1);

    let started = std::time::Instant::now();
    let result = tokio::time::timeout(
        Duration::from_secs(5),
        spawn_turn(&app, ctx("ses_test", "第一条消息")),
    )
    .await
    .expect("the drain must not wait out DRAIN_REQUEST_TIMEOUT_MS")
    .unwrap();
    result.unwrap();

    assert!(
        started.elapsed() < Duration::from_secs(1),
        "a hung read must be bounded by the drain bound: {:?}",
        started.elapsed()
    );
    let final_card = platform.updated_cards().await.last().cloned().unwrap();
    assert!(card_header(&final_card).contains("完成"), "final card Done");
    assert!(!app.inflight.lock().await.contains("ses_test"));
    assert_no_further_rendering(&backend, &platform).await;
}

/// The same scripted sequence started through the real message handler (not
/// `Turn::run`): the turn's generated `msg_cola_` id anchors the Backend
/// timeline, and the drain renders the missed supplement's reply onto the
/// live card before the turn finishes.
#[tokio::test]
async fn a_handler_started_turn_drains_the_new_turns_reply() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    // Park the prompt until the test has scripted the timeline: only the
    // handler knows the `msg_cola_` id this turn will carry.
    let mut backend = MockBackend::new(realistic_parts());
    let gate = backend.hold_prompts();
    let backend = Arc::new(backend);
    let platform = Arc::new(RecordingPlatform::new());
    let app = Arc::new(App::new(cfg, backend.clone(), platform.clone()).unwrap());
    seed_session(&app, "ses_test", "/work").await;
    // The prompt gate is held for a few ms; a cadence well above that keeps
    // the render poll out of the window and the drain the first renderer.
    app.turn_render_poll_ms.store(50, Ordering::Relaxed);
    app.turn_drain_timeout_ms.store(60_000, Ordering::Relaxed);

    let turn = {
        let app = Arc::clone(&app);
        tokio::spawn(async move {
            app.handle_message(incoming(
                "msg_1".into(),
                "chat_1".into(),
                "p2p".into(),
                None,
                "第一条消息".into(),
                None,
            ))
            .await;
        })
    };

    // The prompt records its cola id before parking on the gate.
    let anchor_id = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if let Some(Some(id)) = backend.prompt_message_ids.lock().await.last().cloned() {
                return id;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("the handler's prompt must record its cola id");

    // The first run ended; a cola-authored supplement missed it.
    backend.transcript_scripts.lock().await.insert(
        "ses_test".into(),
        vec![SessionTranscript::new(vec![
            user(&anchor_id, 1_000, "第一条消息"),
            assistant(2_000, "第一轮回答。"),
            user("msg_cola_supp", 3_000, "补充一下"),
        ])],
    );
    gate.add_permits(1);

    // The drain renders the first run's reply and keeps polling under the
    // guard while the supplement is unanswered.
    wait_for_card_text(&platform, "第一轮回答。").await;
    assert!(
        app.inflight.lock().await.contains("ses_test"),
        "the drain must hold the guard while the supplement is unanswered"
    );

    // The new Turn answers; the drain picks it up and the turn finishes.
    backend
        .transcript_scripts
        .lock()
        .await
        .get_mut("ses_test")
        .unwrap()[0]
        .messages
        .push(assistant(4_000, "补充后的回答。"));

    tokio::time::timeout(Duration::from_secs(5), turn)
        .await
        .expect("the turn must finish")
        .unwrap();

    let final_card = platform.updated_cards().await.last().cloned().unwrap();
    assert!(
        card_text(&final_card).contains("补充后的回答。"),
        "the supplement's reply must render on the live card: {final_card}"
    );
    assert!(card_header(&final_card).contains("完成"), "final card Done");
    assert!(!app.inflight.lock().await.contains("ses_test"));
    assert_no_further_rendering(&backend, &platform).await;
}

/// A hung re-check read is bounded by the injected drain budget, not the
/// fixed 30 s request timeout: both the drain's read and the re-check's read
/// hang, and the turn still finalizes at ~2× the tiny bound instead of
/// holding the card (and the guard) for 30 s.
#[tokio::test]
async fn a_hung_recheck_read_does_not_extend_finalization() {
    let _wd = test_work_dir();
    let timeline = vec![
        user("msg_cola_anchor", 1_000, "第一条消息"),
        assistant(2_000, "第一轮回答。"),
    ];
    let (_dir, app, backend, platform) =
        scripted_app(vec![SessionTranscript::new(timeline)], Some(SessionStatus::Busy)).await;
    app.turn_render_poll_ms.store(50, Ordering::Relaxed);
    app.turn_drain_timeout_ms.store(30, Ordering::Relaxed);
    // Both the drain's read and the re-check's read hang; the final reconcile
    // read serves normally.
    backend.hang_transcript_reads(2);

    let started = std::time::Instant::now();
    let result = tokio::time::timeout(
        Duration::from_secs(5),
        spawn_turn(&app, ctx("ses_test", "第一条消息")),
    )
    .await
    .expect("the hung re-check must not wait out DRAIN_REQUEST_TIMEOUT_MS")
    .unwrap();
    result.unwrap();

    assert!(
        started.elapsed() < Duration::from_secs(1),
        "the re-check must be bounded by the injected drain budget: {:?}",
        started.elapsed()
    );
    let final_card = platform.updated_cards().await.last().cloned().unwrap();
    assert!(card_header(&final_card).contains("完成"), "final card Done");
    assert!(!app.inflight.lock().await.contains("ses_test"));
    assert_no_further_rendering(&backend, &platform).await;
}

/// A supplement that first becomes visible only after the drain exited is
/// still drained: the drain's read shows a settled timeline and it stops, the
/// re-check's read is the first to see the unanswered supplement, and the new
/// Turn's reply rides the final card. The test fails if the re-check is
/// removed (finalization would render the reply on the Done flush — or not at
/// all — with the guard already released).
#[tokio::test]
async fn a_supplement_landing_after_the_drain_exit_is_still_drained() {
    let _wd = test_work_dir();
    // Snapshot 1 is what the drain reads: no supplement, so it settles.
    let settled = vec![
        user("msg_cola_anchor", 1_000, "第一条消息"),
        assistant(2_000, "第一轮回答。"),
    ];
    // Snapshot 2 is the re-check's read: the supplement landed meanwhile.
    let raced = vec![
        user("msg_cola_anchor", 1_000, "第一条消息"),
        assistant(2_000, "第一轮回答。"),
        user("msg_cola_supp", 3_000, "补充一下"),
    ];
    let (_dir, app, backend, platform) = scripted_app(
        vec![SessionTranscript::new(settled), SessionTranscript::new(raced)],
        Some(SessionStatus::Idle),
    )
    .await;
    app.turn_render_poll_ms.store(20, Ordering::Relaxed);
    app.turn_drain_timeout_ms.store(200, Ordering::Relaxed);

    let turn = spawn_turn(&app, ctx("ses_test", "第一条消息"));

    // The re-check drained the supplement: the first run's reply renders on a
    // STREAMING flush and the guard stays held — a skipped re-check would have
    // finalized (Done header, guard released) around the supplement.
    wait_for_card_text(&platform, "第一轮回答。").await;
    let rendered = platform.updated_cards().await;
    let first = rendered
        .iter()
        .find(|c| card_text(c).contains("第一轮回答。"))
        .expect("the first run's reply");
    assert!(
        !card_header(first).contains("完成"),
        "the re-check must drain before finalization: {}",
        card_header(first)
    );
    assert!(
        app.inflight.lock().await.contains("ses_test"),
        "the supplement after the drain exit must keep the guard held"
    );

    // The new Turn answers while the re-check's drain is still polling.
    backend
        .transcript_scripts
        .lock()
        .await
        .get_mut("ses_test")
        .unwrap()[0]
        .messages
        .push(assistant(4_000, "补充后的回答。"));

    let result = tokio::time::timeout(Duration::from_secs(5), turn)
        .await
        .expect("the turn must finish")
        .unwrap();
    result.unwrap();

    let final_card = platform.updated_cards().await.last().cloned().unwrap();
    assert!(
        card_text(&final_card).contains("补充后的回答。"),
        "the supplement's reply must ride the final card: {final_card}"
    );
    assert!(card_header(&final_card).contains("完成"), "final card Done");
    assert!(!app.inflight.lock().await.contains("ses_test"));
    assert_no_further_rendering(&backend, &platform).await;
}

/// A submitted run may not have registered yet when the drain's first reads
/// happen (a prompt admit only *schedules* execution, ADR-0056): the first
/// idle reads must NOT settle the turn while the confirmation window is open —
/// the answer that lands inside it must still render and finalize the card.
#[tokio::test]
async fn an_unregistered_run_is_not_settled_by_the_first_idle_read() {
    let _wd = test_work_dir();
    // The first snapshot carries only the admitted user message: the run has
    // produced nothing yet.
    let admitted = vec![user("msg_cola_anchor", 1_000, "第一条消息")];
    let answered = vec![
        user("msg_cola_anchor", 1_000, "第一条消息"),
        assistant(2_000, "第一轮回答。"),
    ];
    let (_dir, app, backend, platform) =
        scripted_app(vec![SessionTranscript::new(admitted)], Some(SessionStatus::Idle)).await;
    // One poll per 20 ms, so the 3-read confirmation window has not closed
    // when the test asserts.
    app.turn_render_poll_ms.store(20, Ordering::Relaxed);

    let turn = spawn_turn(&app, ctx("ses_test", "第一条消息"));

    // The early idle reads alone must not finalize: the guard stays held and
    // the card is not Done while no run step is visible.
    tokio::time::sleep(Duration::from_millis(30)).await;
    assert!(
        app.inflight.lock().await.contains("ses_test"),
        "an unregistered run must keep the turn open through the window"
    );
    let state = Turn::card_state(&app.cards_handle(), "ses_test").await;
    assert!(
        !matches!(state, Some(CardState::Done | CardState::Error)),
        "the card must not be finalized before the run is observed: {state:?}"
    );

    // The run's answer lands; the drain picks it up and finalizes Done.
    *backend
        .transcript_scripts
        .lock()
        .await
        .get_mut("ses_test")
        .unwrap() = vec![SessionTranscript::new(answered)];
    let result = tokio::time::timeout(Duration::from_secs(5), turn)
        .await
        .expect("the turn must finish once the answer lands")
        .unwrap();
    result.unwrap();

    let final_card = platform.updated_cards().await.last().cloned().unwrap();
    assert!(
        card_text(&final_card).contains("第一轮回答。"),
        "the late answer must render: {final_card}"
    );
    assert!(card_header(&final_card).contains("完成"), "final card Done");
    assert!(
        !app.inflight.lock().await.contains("ses_test"),
        "the busy guard must be released"
    );
}

/// A run that never registers (the transcript never carries a step and the
/// status stays idle) is bounded by the confirmation window, not the whole
/// drain budget: the turn finalizes promptly instead of observing for minutes.
#[tokio::test]
async fn a_never_registering_run_settles_after_the_confirmation_window() {
    let _wd = test_work_dir();
    // Only the admitted user message, idle status, forever.
    let admitted = vec![user("msg_cola_anchor", 1_000, "第一条消息")];
    let (_dir, app, _backend, platform) =
        scripted_app(vec![SessionTranscript::new(admitted)], Some(SessionStatus::Idle)).await;
    app.turn_render_poll_ms.store(5, Ordering::Relaxed);
    // A bound the test would never wait out: only the window can end the drain.
    app.turn_drain_timeout_ms.store(60_000, Ordering::Relaxed);

    let started = std::time::Instant::now();
    Turn::run(&app.turn_handles(), ctx("ses_test", "第一条消息"))
        .await
        .unwrap();

    assert!(
        started.elapsed() < Duration::from_secs(2),
        "the confirmation window must bound an unregistered submit: {:?}",
        started.elapsed()
    );
    let final_card = platform.updated_cards().await.last().cloned().unwrap();
    assert!(card_header(&final_card).contains("完成"), "final card Done");
    assert!(!app.inflight.lock().await.contains("ses_test"));
}

/// A final reconcile whose transcript read no longer carries the turn (a
/// failed/empty read) must not stamp Done over a failure the drain observed:
/// the last observed failure is the fallback, so a read hiccup cannot erase
/// the Error card.
#[tokio::test]
async fn a_failed_final_read_keeps_the_drains_observed_failure() {
    let _wd = test_work_dir();
    let mut failed = typed_message(
        "msg_a1",
        MessageRole::Assistant,
        Some(2_000),
        vec![text_part("失败的一步")],
    );
    failed.error = Some("provider 503".into());
    let timeline = vec![user("msg_cola_anchor", 1_000, "第一条消息"), failed];
    // Snapshot 1 is the settled failed turn the drain observes; snapshot 2 (and
    // every read after it) is anchorless — the final reconcile's read hiccup.
    let (_dir, app, _backend, platform) = scripted_app(
        vec![
            SessionTranscript::new(timeline),
            SessionTranscript::new(Vec::new()),
        ],
        Some(SessionStatus::Idle),
    )
    .await;
    app.turn_render_poll_ms.store(20, Ordering::Relaxed);
    app.turn_drain_timeout_ms.store(60_000, Ordering::Relaxed);

    Turn::run(&app.turn_handles(), ctx("ses_test", "第一条消息"))
        .await
        .unwrap();

    let final_card = platform.updated_cards().await.last().cloned().unwrap();
    assert!(
        card_header(&final_card).contains("出错"),
        "an observed failure must survive the final read: {}",
        card_header(&final_card)
    );
    assert!(
        card_text(&final_card).contains("provider 503"),
        "the observed failure must reach the card: {final_card}"
    );
    assert!(!app.inflight.lock().await.contains("ses_test"));
}

/// A rejected submit schedules no run: when the transcript carries no admitted
/// message (a real server's 500), the drain must settle on its first non-busy
/// read — the submit's error is the outcome — instead of observing to its
/// bound for a run that never registered.
#[tokio::test]
async fn a_rejected_submit_does_not_wait_out_the_drain_bound() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let mut backend = MockBackend::new(realistic_parts());
    backend.fail_prompt("provider 503");
    // A rejected submit leaves no message behind: the history stays empty.
    backend.given_transcript("ses_test", vec![SessionTranscript::new(Vec::new())]);
    let backend = Arc::new(backend);
    let platform = Arc::new(RecordingPlatform::new());
    let app = Arc::new(App::new(cfg, backend.clone(), platform.clone()).unwrap());
    seed_session(&app, "ses_test", "/work").await;
    app.turn_render_poll_ms.store(5, Ordering::Relaxed);
    // A bound the test would never wait out: only the submit-failure rule can
    // end the drain promptly.
    app.turn_drain_timeout_ms.store(60_000, Ordering::Relaxed);

    let started = std::time::Instant::now();
    Turn::run(&app.turn_handles(), ctx("ses_test", "第一条消息"))
        .await
        .unwrap();
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "a rejected submit must not be mistaken for a registration race"
    );
    let final_card = platform.updated_cards().await.last().cloned().unwrap();
    assert!(
        card_header(&final_card).contains("出错"),
        "a rejected submit ends Error: {}",
        card_header(&final_card)
    );
    assert!(
        card_text(&final_card).contains("provider 503"),
        "the submit failure must reach the card: {final_card}"
    );
    assert!(!app.inflight.lock().await.contains("ses_test"));
}

/// Idle with nothing outstanding exits without waiting out the (60 s) bound:
/// the drain's first check is enough, and finalization follows.
#[tokio::test]
async fn an_idle_session_exits_the_drain_without_waiting() {
    let _wd = test_work_dir();
    let timeline = vec![
        user("msg_cola_anchor", 1_000, "第一条消息"),
        assistant(2_000, "第一轮回答。"),
    ];
    let (_dir, app, _backend, platform) =
        scripted_app(vec![SessionTranscript::new(timeline)], Some(SessionStatus::Idle)).await;

    let started = std::time::Instant::now();
    Turn::run(&app.turn_handles(), ctx("ses_test", "第一条消息"))
        .await
        .unwrap();

    assert!(
        started.elapsed() < Duration::from_secs(1),
        "an idle session must not wait the drain bound"
    );
    let final_card = platform.updated_cards().await.last().cloned().unwrap();
    assert!(card_header(&final_card).contains("完成"));
    assert!(!app.inflight.lock().await.contains("ses_test"));
}
