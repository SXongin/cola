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
    BackgroundTask, ChildRuntime, ContentBlock, FinishReason, MessageRole, Part, SessionTranscript, ShellEnd,
    ShellRuntime, StepFinish, TextPart, ToolIdentity, ToolOutput, ToolStatus, TranscriptMessage,
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

/// A finished assistant turn whose only visible content is `text`. The text
/// carries the message's own server time, like a real part: the timeline keys
/// off it, so a fixture cannot make a later server-keyed part sort behind a
/// "now"-keyed one.
pub(crate) fn assistant(created: i64, text: &str) -> TranscriptMessage {
    typed_message(
        &format!("msg_a_{created}"),
        MessageRole::Assistant,
        Some(created),
        vec![
            Part::Text(TextPart {
                text: text.to_string(),
                started_at: Some(created),
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
pub(crate) fn tool_assistant(created: i64, status: ToolStatus, output: &str) -> TranscriptMessage {
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
        advisory_live: false,
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
    scripted_app_with(scripts, status, |_| {}).await
}

/// [`scripted_app`] with the config adjusted before the App is built: the
/// completion-notice opt-ins are fixed at construction, so a test that needs
/// them (the p2p long-task notice) must set them here.
pub(crate) async fn scripted_app_with(
    scripts: Vec<SessionTranscript>,
    status: Option<SessionStatus>,
    configure: impl FnOnce(&mut crate::config::Config),
) -> (
    tempfile::TempDir,
    Arc<App>,
    Arc<MockBackend>,
    Arc<RecordingPlatform>,
) {
    let dir = tempfile::tempdir().unwrap();
    let mut cfg = test_config(&dir.path().join("sessions.json"));
    configure(&mut cfg);
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
    // A tiny per-read bound: a scripted hung read must fail fast instead of
    // eating a test's whole grace.
    app.turn_follow_read_timeout_ms.store(20, Ordering::Relaxed);
    // The shared runtime-reconcile cadence (#589) is test time too: every
    // tick may observe unless a test pins the throttle with a large interval.
    app.runtime_reconcile.interval_ms.store(0, Ordering::Relaxed);
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
/// on a Busy session, with the given single grace.
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
    app.turn_follow_grace_ms.store(follow_grace_ms, Ordering::Relaxed);
    (dir, app, backend, platform)
}

/// Run the #284 scenario turn until its running `bash` panel is on the live
/// card. The turn's own task now owns the merged unbounded drain (#603) — there
/// is no hand-off and `Turn::run` does not return at a bound — so this detaches
/// the task (dropping its handle) and the caller drives the Backend, then awaits
/// [`wait_for_guard_release`] at the true end.
async fn run_to_panel(app: &Arc<App>, platform: &RecordingPlatform) {
    let _turn = spawn_turn(app, ctx("ses_test", "第一条消息"));
    wait_for_card_text(platform, "⏳ bash").await;
    assert!(
        app.inflight.lock().await.contains("ses_test"),
        "the merged drain holds the guard while it renders"
    );
}

/// Await the merged drain's guard release (the turn's true end), or panic: the
/// guard is held for the WHOLE path and released only at the ending.
pub(crate) async fn wait_for_guard_release(app: &Arc<App>) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while app.inflight.lock().await.contains("ses_test") {
        assert!(
            tokio::time::Instant::now() < deadline,
            "the turn never released the guard"
        );
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

/// Settle the scenario's `bash` panel in the scripted transcript the follow
/// reads (`status`/`output` — the server's own part update).
pub(crate) async fn settle_tool(backend: &Arc<MockBackend>, status: ToolStatus, output: &str) {
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
pub(crate) async fn wait_for_card_header(platform: &RecordingPlatform, needle: &str) {
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
        platform.texts().await.is_empty(),
        "a stop with a live card sends no text reply — the card's 已停止 is the acknowledgement: {:?}",
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

/// A long, healthy run renders to its true end on ONE card: the merged
/// unbounded drain (#603) has no total budget, so a readable, busy session
/// simply keeps updating its Card — across as many drain ticks as it takes —
/// and only the session going idle finalizes it, and only then Done. No
/// hand-off, no 「部分完成」/continuation pair.
#[tokio::test]
async fn a_long_healthy_run_finishes_on_one_card() {
    let _wd = test_work_dir();
    let timeline = vec![
        user("msg_cola_anchor", 1_000, "第一条消息"),
        assistant(2_000, "第一轮回答。"),
    ];
    let (_dir, app, backend, platform) =
        scripted_app(vec![SessionTranscript::new(timeline)], Some(SessionStatus::Busy)).await;
    // A fast poll so MANY drain ticks elapse inside the readable stretch, and a
    // grace the readable run never trips: only the session going idle ends it.
    app.turn_render_poll_ms.store(5, Ordering::Relaxed);
    app.turn_follow_grace_ms.store(30, Ordering::Relaxed);
    // Group + requester: the merged path's finalization sends the completion notice.
    let mut context = ctx("ses_test", "第一条消息");
    context.is_group = true;
    context.requester_open_id = Some(TEST_HOST.to_string());

    let turn = spawn_turn(&app, context);
    wait_for_card_text(&platform, "第一轮回答。").await;

    // Stay readable across MANY ticks (well past the grace): the run must keep
    // rendering the SAME card. A fixed total budget — the retired 10-minute
    // hand-off — would eventually split or 「出错」; the unbounded path never
    // does. The tiny cadence makes the tick count observable.
    tokio::time::sleep(Duration::from_millis(300)).await;
    let reads = backend.transcript_calls.lock().await.len();
    assert!(
        reads >= 20,
        "the drain must keep ticking while the session is readable (saw {reads} reads)"
    );
    assert_eq!(
        Turn::card_state(&app.cards_handle(), "ses_test").await,
        Some(CardState::Streaming),
        "the card stays live while the session is readable and busy"
    );
    assert!(
        app.inflight.lock().await.contains("ses_test"),
        "the guard is held for the whole path"
    );
    // ONE card so far: the run was never handed off to a new card.
    assert_eq!(
        card_posts(&platform).await,
        1,
        "a long healthy run must stay on its one card: {:?}",
        platform.calls.lock().await
    );
    assert!(
        continuation_sends(&platform).await.is_empty(),
        "no continuation card may open: {:?}",
        platform.calls.lock().await
    );
    assert!(
        platform
            .updated_cards()
            .await
            .iter()
            .all(|c| !card_header(c).contains("部分完成") && !card_header(c).contains("出错")),
        "a long healthy run must show no hand-off artifact: {:?}",
        platform.updated_cards().await
    );

    // It genuinely ends: the session goes idle and the merged drain finalizes Done.
    backend
        .set_session_status("ses_test", Some(SessionStatus::Idle))
        .await;
    let result = tokio::time::timeout(Duration::from_secs(5), turn)
        .await
        .expect("the idle session must end the drain")
        .unwrap();
    result.unwrap();
    let final_card = platform.updated_cards().await.last().cloned().unwrap();
    assert!(card_header(&final_card).contains("完成"));
    assert!(card_text(&final_card).contains("第一轮回答。"));
    // The whole run lived on the one card: no post beyond the initial reply.
    assert_eq!(
        card_posts(&platform).await,
        1,
        "the whole run lives on one card: {:?}",
        platform.calls.lock().await
    );
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

/// #284 regression: a Supplement whose tool part is still `running` must not
/// freeze the card. The merged drain keeps rendering the SAME card — not Done,
/// the panel still live — refuses to finalize even when the session reports
/// idle while the panel is still live, and ends Done with the tool's completion
/// once the panel settles.
#[tokio::test]
async fn a_running_supplement_panel_rides_past_the_grace_until_idle() {
    let _wd = test_work_dir();
    // A grace the test would never wait out: only the panel settling ends it.
    let (_dir, app, backend, platform) = busy_supplement_app(60_000).await;

    run_to_panel(&app, &platform).await;
    assert_eq!(
        Turn::card_state(&app.cards_handle(), "ses_test").await,
        Some(CardState::Streaming),
        "the merged drain leaves the card live, not Done"
    );
    let live = platform.updated_cards().await.last().cloned().unwrap();
    assert!(
        !card_header(&live).contains("完成"),
        "the card must not be Done over a running panel: {}",
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
    wait_for_guard_release(&app).await;
    let final_card = platform.updated_cards().await.last().cloned().unwrap();
    assert!(
        card_text(&final_card).contains("done"),
        "the tool's completion must render: {final_card}"
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

/// A long turn that ends in a provider failure must finalize Error, not Done:
/// the failure lives on the transcript's newest assistant message, and the
/// merged drain reads the same shared rule the finalization uses (ADR-0056).
#[tokio::test]
async fn a_late_failure_finalizes_error_not_done() {
    let _wd = test_work_dir();
    // A grace the test would never wait out: only the settled failure ends it.
    let (_dir, app, backend, platform) = busy_supplement_app(60_000).await;

    run_to_panel(&app, &platform).await;

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
    wait_for_guard_release(&app).await;
    let final_card = platform.updated_cards().await.last().cloned().unwrap();
    assert!(card_header(&final_card).contains("出错"), "final card Error");
    assert!(
        card_text(&final_card).contains("provider 503"),
        "the transcript's failure must reach the card: {final_card}"
    );
    assert_no_further_rendering(&backend, &platform).await;
}

/// `/stop` during the drain finalizes the card promptly — the same sticky
/// stopped-session marker the drain observes — instead of waiting out the
/// grace.
#[tokio::test]
async fn stop_during_the_drain_finalizes_promptly() {
    let _wd = test_work_dir();
    let (_dir, app, backend, platform) = busy_supplement_app(60_000).await;
    run_to_panel(&app, &platform).await;

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
        "the drain must end on the stop, not the 60 s grace"
    );
    wait_for_guard_release(&app).await;
    let final_card = platform.updated_cards().await.last().cloned().unwrap();
    assert!(
        card_buttons(&final_card).is_empty(),
        "a stopped card must not offer a retry: {final_card}"
    );
    assert!(
        platform.texts().await.is_empty(),
        "a stopped drain sends no text reply either — the finalized card carries 已停止: {:?}",
        platform.calls.lock().await
    );
    assert_no_further_rendering(&backend, &platform).await;
}

/// The drain's mid-tick stop re-check (#394): a stop landing AFTER a tick's
/// top marker check — while its transcript read is already in flight — must
/// still finalize `Stopped`, not the settled run's Done. The gate parks the
/// drain's read, so the `/stop` deterministically lands inside the tick.
#[tokio::test]
async fn a_stop_landing_mid_tick_finalizes_stopped_not_done() {
    let _wd = test_work_dir();
    // A ceiling the test would never wait out: only the stop can end it.
    let (_dir, app, backend, platform) = busy_supplement_app(60_000).await;
    // The parked read must stay parked until the test releases it: the
    // follow's default per-read bound would cancel it first.
    app.turn_follow_read_timeout_ms.store(60_000, Ordering::Relaxed);
    run_to_panel(&app, &platform).await;

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
    wait_for_guard_release(&app).await;
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
async fn a_hung_backend_ends_the_drain_in_error() {
    let _wd = test_work_dir();
    let (_dir, app, backend, platform) = busy_supplement_app(50).await;
    run_to_panel(&app, &platform).await;

    // Every transcript read the drain makes now hangs (a wedged per-session
    // read); the status read alone is not a full read pair, so the
    // lost-contact grace ends the card.
    backend.hang_transcript_reads(100);

    let started = std::time::Instant::now();
    wait_for_card_header(&platform, "出错").await;
    assert!(
        started.elapsed() < Duration::from_secs(3),
        "the hung read must end at the grace, not the per-read bound"
    );
    wait_for_guard_release(&app).await;
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

/// Finding C (spec #602 review): a `Settled` ending derived from an unanswered
/// status must not finalize. Here the TRANSCRIPT reads keep succeeding and carry
/// a live `⏳` panel, but `session_status` never answers — so the tick is not a
/// full read pair (`contact == false`). The Complete state comes only from the
/// readable transcript, and the unreadable status must not be read as "the run
/// ended": the card must NOT be stamped ✅ over the live panel; the lost-contact
/// grace ends it in error instead. (The hung-backend test hangs the transcript
/// too, so it never exercises this half.)
#[tokio::test]
async fn a_settled_read_over_an_unanswered_status_does_not_finalize_done() {
    let _wd = test_work_dir();
    let (_dir, app, backend, platform) = busy_supplement_app(50).await;
    run_to_panel(&app, &platform).await;

    // The transcript keeps reading (the live `⏳ bash` panel is still there) and
    // the anchor turn reads Complete — a Settled state — but every status read
    // now fails: the tick is not a full read pair, so the ending is not real.
    backend.session_status_fails.store(true, Ordering::SeqCst);

    wait_for_card_header(&platform, "出错").await;
    wait_for_guard_release(&app).await;
    let final_card = platform.updated_cards().await.last().cloned().unwrap();
    assert!(
        !card_header(&final_card).contains("完成"),
        "a Settled ending over an unanswered status must never stamp Done: {final_card}"
    );
    assert!(
        card_text(&final_card).contains("失去联系"),
        "the lost-contact grace ends it: {final_card}"
    );
    assert_no_further_rendering(&backend, &platform).await;
}

/// Finding (spec #602 review): an unreadable status must not yield a Waiting
/// card either. Here the transcript is readable and settles to `Waiting` (an
/// idle boundary with a live Background Task), but `session_status` never
/// answers — so no tick is a full read pair. The `Waiting` decision comes only
/// from the transcript; yielding it would end the run *before* the lost-contact
/// grace, contradicting #603's "a lost-contact run still ends in error at the
/// grace". The grace owns the state: the card stays live (never 「等待后台任务」)
/// and ends Error with the lost-contact copy.
#[tokio::test]
async fn an_unreadable_status_never_yields_a_waiting_card() {
    let _wd = test_work_dir();
    let live = SessionTranscript::new(vec![
        user("msg_cola_anchor", 1_000, "跑一下 CI"),
        assistant(2_000, "已经交给后台了。"),
    ])
    .with_executions(vec![execution(2_500)])
    .with_background_tasks(vec![background_shell(2_100)]);
    let (_dir, app, backend, platform) = scripted_app(vec![live], Some(SessionStatus::Idle)).await;
    // The status read never answers: every tick is a partial read pair, so the
    // settle decision's `Waiting` may not be acted on.
    backend.session_status_fails.store(true, Ordering::SeqCst);
    // A grace the test can observe: short enough to wait out, long enough that
    // an immediate yield would have shown itself.
    app.turn_follow_grace_ms.store(300, Ordering::Relaxed);

    let turn = spawn_turn(&app, ctx("ses_test", "跑一下 CI"));
    // The still-readable transcript renders the turn's content...
    wait_for_card_text(&platform, "已经交给后台了。").await;
    // ...but well before the grace the card must NOT carry the waiting yield:
    // no ending may be claimed from a read that never answered.
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(
        !platform
            .updated_cards()
            .await
            .iter()
            .any(|card| card_header(card).contains("等待后台任务")),
        "an unreadable status must not yield Waiting before the grace: {:?}",
        platform.updated_cards().await
    );
    assert!(
        app.inflight.lock().await.contains("ses_test"),
        "the drain keeps observing instead of yielding"
    );

    // The grace then ends the run in error with the lost-contact copy.
    wait_for_card_header(&platform, "出错").await;
    let result = tokio::time::timeout(Duration::from_secs(5), turn)
        .await
        .expect("the lost-contact grace must end the turn")
        .unwrap();
    result.unwrap();
    let final_card = platform.updated_cards().await.last().cloned().unwrap();
    assert!(
        card_text(&final_card).contains("失去联系"),
        "the lost-contact copy: {final_card}"
    );
    assert!(
        !card_header(&final_card).contains("等待后台任务"),
        "the grace's ending is never the waiting yield: {final_card}"
    );
    assert_no_further_rendering(&backend, &platform).await;
}

/// A live `⏳` panel on a readable, idle session that never settles is an
/// unreconcilable panel: the drain gives it the grace to settle, then ends
/// Error — never Done over a `⏳`, never an eternal card (#386).
#[tokio::test]
async fn an_orphaned_panel_ends_the_drain_in_error() {
    let _wd = test_work_dir();
    let (_dir, app, backend, platform) = busy_supplement_app(50).await;
    run_to_panel(&app, &platform).await;

    // The run is gone (idle) but its last panel stays `running`: a crash
    // orphan nothing will ever settle.
    backend
        .set_session_status("ses_test", Some(SessionStatus::Idle))
        .await;

    wait_for_card_header(&platform, "出错").await;
    wait_for_guard_release(&app).await;
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
    run_to_panel(&app, &platform).await;

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
    wait_for_guard_release(&app).await;
    let final_card = platform.updated_cards().await.last().cloned().unwrap();
    assert!(
        card_text(&final_card).contains("失去联系"),
        "the lost-contact copy: {final_card}"
    );
    assert_no_further_rendering(&backend, &platform).await;
}

/// The wait scenario (#386): a Busy session with one pending permission
/// inlined on its live card, drip-fed by the merged unbounded drain.
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
    app.turn_follow_grace_ms.store(30, Ordering::Relaxed);
    app.turn_follow_read_timeout_ms.store(20, Ordering::Relaxed);

    // The turn runs on its own task (the merged drain owns it to the true
    // end), then surface the permission the way `App::run` does — after the
    // accumulator arms, so it inlines on the card.
    let _turn = spawn_turn(&app, ctx("ses_test", "第一条消息"));
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

    // The operator answers elsewhere while the run is still live: the
    // permission poller's sweep repaints the card with the neutral
    // 「已由其他客户端处理」 receipt independently of the drain (one more
    // legitimate PATCH); account for it before asserting the drain stopped —
    // otherwise this races the sweep, not the drain.
    backend.permission_resolved_by_another("per_1").await;
    wait_for_card_update(
        &platform,
        "the handled-elsewhere receipt",
        CardUpdates::Any,
        |card| card_text(card).contains("已由其他客户端处理"),
    )
    .await;
    // Then the run settles: the merged drain finalizes Done, not Error — the
    // wait was never a failure.
    backend
        .set_session_status("ses_test", Some(SessionStatus::Idle))
        .await;
    wait_for_card_header(&platform, "完成").await;
    wait_for_guard_release(&app).await;
    assert_no_further_rendering(&backend, &platform).await;
}

/// The wait is not a shield for a lost Backend (#386): a wait whose reads
/// stopped answering still ends in the lost-contact Error — a stale pending
/// record must not suspend the fallback forever.
#[tokio::test]
async fn a_pending_wait_with_lost_contact_still_ends_in_error() {
    let _wd = test_work_dir();
    let (_dir, app, backend, platform) = pending_permission_app().await;

    backend.hang_transcript_reads(100);
    wait_for_card_header(&platform, "出错").await;
    wait_for_guard_release(&app).await;
    let final_card = platform.updated_cards().await.last().cloned().unwrap();
    assert!(
        card_text(&final_card).contains("失去联系"),
        "the lost-contact copy under a wait: {final_card}"
    );
    assert_no_further_rendering(&backend, &platform).await;
}

/// ADR-0059's routing key, drain-window half: a run that keeps its guard
/// through the merged drain receives a message as a Supplement. It merges into
/// the still-live chain — the Card Chain splits at the message and the drain
/// keeps rendering on the continuation — instead of freezing the old card and
/// starting an overlapping Turn. The accumulator is never replaced and the
/// drain's guard is never read as idle.
#[tokio::test]
async fn a_message_during_the_drain_window_splits_the_live_chain() {
    let _wd = test_work_dir();
    let (_dir, app, backend, platform) = busy_supplement_app(60_000).await;
    run_to_panel(&app, &platform).await;

    let drained_anchor = Turn::armed_turn_anchor(&app.cards_handle(), "ses_test").await;
    assert_eq!(
        drained_anchor.as_ref().map(|anchor| anchor.created_ms),
        Some(1_000),
        "the drain watches the turn anchor"
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

    // The message was submitted to merge into the live run — the drain's
    // guard made it a Supplement, not a busy answer and not a new Turn.
    assert!(
        backend.prompt_calls.lock().await.iter().any(|t| t == "接着问"),
        "the supplement must reach the Backend: {:?}",
        backend.prompt_calls.lock().await
    );
    assert!(
        !platform.texts().await.iter().any(|t| t.contains("还在处理中")),
        "a drain-window message is never answered busy: {:?}",
        platform.calls.lock().await
    );
    assert_eq!(
        Turn::armed_turn_anchor(&app.cards_handle(), "ses_test").await,
        drained_anchor,
        "no competing Turn: the drain's accumulator survives"
    );
    assert!(
        app.inflight.lock().await.contains("ses_test"),
        "the drain still owns the session's guard"
    );

    // Exactly ONE card replies to the message — the split continuation, with
    // the receipt. A new Turn would have replied a Loading card to it. The old
    // card is NOT frozen: the split finalizes it with everything before it,
    // the live running panel rides the continuation (ADR-0045), and the receipt
    // belongs to the continuation only.
    let continuation = supplement_continuation(&platform, "msg_next").await;
    let finalized = {
        let calls = platform.calls.lock().await;
        let reply_idx = calls
            .iter()
            .position(|c| matches!(c, PlatformCall::ReplyCard { reply_to, .. } if reply_to == "msg_next"))
            .expect("the continuation reply");
        calls[..reply_idx]
            .iter()
            .rev()
            .find_map(|c| match c {
                PlatformCall::UpdateMessage { card, .. } => Some(card.clone()),
                _ => None,
            })
            .expect("the split must finalize the old card before replying")
    };
    assert!(
        card_header(&finalized).contains("部分完成"),
        "the old card takes the standard split header: {finalized}"
    );
    assert!(
        card_text(&finalized).contains("第一轮回答。"),
        "the finalized card keeps everything before the split: {finalized}"
    );
    assert!(
        !card_text(&finalized).contains("📨 已收到补充"),
        "the receipt records the Supplement on the continuation only: {finalized}"
    );
    assert!(
        card_text(&continuation).contains("⏳ bash"),
        "the running panel rides the live continuation (ADR-0045): {continuation}"
    );

    // The run goes on: the drain keeps rendering into the continuation. The
    // tool settles and the answer lands — on the split continuation.
    settle_tool(&backend, ToolStatus::Completed, "构建完成").await;
    backend
        .transcript_scripts
        .lock()
        .await
        .get_mut("ses_test")
        .unwrap()[0]
        .messages
        .push(assistant(5_000, "接着问的回答。"));
    backend
        .set_session_status("ses_test", Some(SessionStatus::Idle))
        .await;

    wait_for_card_update(
        &platform,
        "the continuation's Done card",
        CardUpdates::Latest,
        |card| card_header(card).contains("完成") && card_text(card).contains("接着问的回答。"),
    )
    .await;
    let final_card = platform.updated_cards().await.last().cloned().unwrap();
    assert!(
        card_text(&final_card).contains("📨 已收到补充"),
        "the continuation keeps the receipt: {final_card}"
    );
    assert!(
        !card_text(&final_card).contains("第一轮回答。"),
        "the continuation is a delta, never a replay: {final_card}"
    );

    // The drain window closed: the guard went back with the ending.
    wait_for_guard_release(&app).await;
    assert_no_further_rendering(&backend, &platform).await;
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

/// The unified per-read bound (#603): a hung transcript read is abandoned at
/// `follow_read_timeout_ms`, not the fixed 30 s request bound — the drain keeps
/// ticking (so `/stop` stays observable) while a wedged read would otherwise
/// freeze the tick.
#[tokio::test]
async fn a_hung_read_is_abandoned_at_the_per_read_bound() {
    let _wd = test_work_dir();
    // A grace the test would never wait out: only the read bound is under test.
    let (_dir, app, backend, platform) = busy_supplement_app(60_000).await;
    app.turn_follow_read_timeout_ms.store(30, Ordering::Relaxed);
    run_to_panel(&app, &platform).await;

    backend.transcript_calls.lock().await.clear();
    // Three hung reads: each is abandoned at the 30 ms per-read bound, then the
    // reads answer again. Blocking on the fixed 30 s request bound instead would
    // make this take ~90 s.
    backend.hang_transcript_reads(3);
    let started = std::time::Instant::now();
    wait_for_transcript_reads(&backend, "ses_test", 1).await;
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "a hung read must be abandoned at the per-read bound: {:?}",
        started.elapsed()
    );
    assert!(
        app.inflight.lock().await.contains("ses_test"),
        "the drain still holds the guard (the grace is 60 s)"
    );
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
/// The submitted message IS in the transcript — it was received, just never
/// answered — so this is not the Unreceived case (ADR-0062): an idle read with
/// the message present settles Complete.
#[tokio::test]
async fn a_never_registering_run_settles_after_the_confirmation_window() {
    let _wd = test_work_dir();
    // Only the admitted user message, idle status, forever.
    let admitted = vec![user("msg_cola_anchor", 1_000, "第一条消息")];
    let (_dir, app, _backend, platform) =
        scripted_app(vec![SessionTranscript::new(admitted)], Some(SessionStatus::Idle)).await;
    app.turn_render_poll_ms.store(5, Ordering::Relaxed);
    // A bound the test would never wait out: only the window can end the drain.
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

/// ADR-0062 (spec #434, ticket #436): a submit whose message never reaches
/// the transcript and whose session is idle ends Unreceived — the card says
/// 「⚠️ 这条消息未被接收」 and never ✅ — bounded by the same confirmation
/// window an unregistered run gets, not the whole drain budget. The sibling
/// test above carries the message in its transcript: that one was received
/// and is simply unanswered, so it keeps today's Done.
#[tokio::test]
async fn a_never_promoted_submit_at_idle_ends_unreceived() {
    let _wd = test_work_dir();
    // The transcript never carries the submitted message: the steer sat in
    // the session inbox and no runner promoted it.
    let (_dir, app, backend, platform) = scripted_app(
        vec![SessionTranscript::new(Vec::new())],
        Some(SessionStatus::Idle),
    )
    .await;
    app.turn_render_poll_ms.store(5, Ordering::Relaxed);
    // A bound the test would never wait out: only the window can end the drain.
    let started = std::time::Instant::now();
    Turn::run(&app.turn_handles(), ctx("ses_test", "第一条消息"))
        .await
        .unwrap();

    assert!(
        started.elapsed() < Duration::from_secs(2),
        "the confirmation window must bound a never-landed submit: {:?}",
        started.elapsed()
    );
    assert_eq!(
        Turn::card_state(&app.cards_handle(), "ses_test").await,
        Some(CardState::Unreceived),
        "a never-landed submit at idle ends Unreceived"
    );
    let final_card = platform.updated_cards().await.last().cloned().unwrap();
    assert!(
        card_header(&final_card).contains("⚠️ 这条消息未被接收"),
        "the card names the unreceived message: {final_card}"
    );
    assert!(
        !card_header(&final_card).contains("完成"),
        "an Unreceived card is never ✅: {final_card}"
    );
    assert!(
        !card_text(&final_card).contains("等待当前运行接收"),
        "an idle session never shows the waiting hint: {final_card}"
    );
    assert!(
        !app.inflight.lock().await.contains("ses_test"),
        "the guard is released at the Unreceived ending"
    );
    assert_no_further_rendering(&backend, &platform).await;
}

/// ADR-0062 (spec #434, ticket #437): the Unreceived card offers 「重新发起」.
/// Pressing it records interrupt then resume; the queued steer is promoted at
/// the new run's start and renders on the SAME card, which settles through the
/// normal decision — and no second prompt is ever submitted, so the transcript
/// message is never duplicated. A second click while the first is in flight
/// claims nothing.
#[tokio::test]
async fn pressing_resume_interrupts_then_resumes_and_renders_the_promoted_message() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let mut backend = MockBackend::new(realistic_parts());
    // V2: the generation serves the durable resume write.
    backend.with_resume_supported(true);
    // The message never lands: no read carries cola's prompt (the inbox steer
    // no runner promoted).
    backend.given_transcript("ses_test", vec![SessionTranscript::new(Vec::new())]);
    let backend = Arc::new(backend);
    let platform = Arc::new(RecordingPlatform::new());
    let app = Arc::new(App::new(cfg, backend.clone(), platform.clone()).unwrap());
    seed_session(&app, "ses_test", "/work").await;
    app.turn_render_poll_ms.store(5, Ordering::Relaxed);
    Turn::run(&app.turn_handles(), ctx("ses_test", "第一条消息"))
        .await
        .unwrap();
    let unreceived = platform.updated_cards().await.last().cloned().unwrap();
    assert!(
        card_header(&unreceived).contains("未被接收"),
        "the stalled submit must end Unreceived first: {unreceived}"
    );
    assert_eq!(
        Turn::card_state(&app.cards_handle(), "ses_test").await,
        Some(CardState::Unreceived)
    );
    let button = card_buttons(&unreceived)
        .into_iter()
        .find(|button| button["value"]["action"] == "resume")
        .expect("the Unreceived card must offer 重新发起");
    assert_eq!(button["text"]["content"].as_str().unwrap(), "重新发起");
    assert_eq!(button["value"]["session_id"].as_str().unwrap(), "ses_test");
    assert!(!app.inflight.lock().await.contains("ses_test"));

    // The resumed run promotes the queued message: the transcript now carries
    // it, with its answer.
    script_transcript(
        &backend,
        vec![SessionTranscript::new(vec![
            user("msg_cola_anchor", 1_000, "第一条消息"),
            assistant(2_000, "第一轮回答。"),
        ])],
    )
    .await;

    let first = app
        .host_action(serde_json::json!({ "action": "resume", "session_id": "ses_test" }))
        .await
        .expect("the first click must claim the resume");
    assert_eq!(first.toast.as_deref(), Some("正在重新发起..."));
    let second = app
        .host_action(serde_json::json!({ "action": "resume", "session_id": "ses_test" }))
        .await;
    assert!(second.is_none(), "a second click must claim nothing");

    // The click interrupted then resumed, the message rendered on the SAME
    // card, and the settle decision finished it (the session reads idle, the
    // turn's reply is complete).
    wait_for_card_header(&platform, "完成").await;
    assert_eq!(
        backend.recovery_ops.lock().await.clone(),
        vec!["interrupt:ses_test".to_string(), "resume:ses_test".to_string()],
        "the click must interrupt first, then resume — exactly once each"
    );
    let final_card = platform.updated_cards().await.last().cloned().unwrap();
    assert!(
        card_text(&final_card).contains("第一轮回答。"),
        "the promoted message's answer renders on the card: {final_card}"
    );
    assert!(
        !card_header(&final_card).contains("未被接收"),
        "the resumed card is no longer Unreceived: {final_card}"
    );
    assert_eq!(
        backend
            .prompt_calls
            .lock()
            .await
            .iter()
            .filter(|text| text.as_str() == "第一条消息")
            .count(),
        1,
        "resume must not submit a second prompt: {:?}",
        backend.prompt_calls.lock().await
    );
    assert!(!app.inflight.lock().await.contains("ses_test"));
    assert_no_further_rendering(&backend, &platform).await;
}

/// Ticket #437 (ADR-0062): the genuine Unreceived shape — the message never
/// landed and V1 has no resume endpoint, so 重新发起 resubmits it as a new
/// Turn whose own prompt is what creates the message; the new Turn's drain
/// then reads it back and runs the attempt to its true end. No interrupt, no
/// resume, no second id.
#[tokio::test]
async fn resume_on_v1_resubmits_a_never_landed_message_into_a_new_turn() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let mut backend = MockBackend::new(realistic_parts());
    // V1: no resume endpoint (the mock's default, stated explicitly).
    backend.with_resume_supported(false);
    // The message never lands: the scripted transcript stays empty, so the
    // first Turn ends Unreceived.
    backend.given_transcript("ses_test", vec![SessionTranscript::new(Vec::new())]);
    // The resubmit is what creates the message: on the SECOND submit (the
    // click's), the transcript starts carrying the user message and its answer
    // — what a V1 `prompt_async` upsert leaves behind.
    let scripts = backend.transcript_scripts.clone();
    let submits = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let landed = SessionTranscript::new(vec![
        user("msg_cola_anchor", 1_000, "第一条消息"),
        assistant(2_000, "第一轮回答。"),
    ]);
    backend.on_prompt(move || {
        if submits.fetch_add(1, Ordering::SeqCst) == 1 {
            scripts
                .try_lock()
                .expect("the test never holds the transcript script while a prompt runs")
                .insert("ses_test".into(), vec![landed.clone()]);
        }
    });
    let backend = Arc::new(backend);
    let platform = Arc::new(RecordingPlatform::new());
    let app = Arc::new(App::new(cfg, backend.clone(), platform.clone()).unwrap());
    seed_session(&app, "ses_test", "/work").await;
    app.turn_render_poll_ms.store(5, Ordering::Relaxed);
    Turn::run(&app.turn_handles(), ctx("ses_test", "第一条消息"))
        .await
        .unwrap();
    assert_eq!(
        Turn::card_state(&app.cards_handle(), "ses_test").await,
        Some(CardState::Unreceived)
    );

    let result = app
        .host_action(serde_json::json!({ "action": "resume", "session_id": "ses_test" }))
        .await
        .expect("the V1 degradation must still be claimable");
    assert_eq!(result.toast.as_deref(), Some("正在重新发起..."));

    // The resubmitted Turn runs to its true end and renders the answer its own
    // prompt created.
    wait_for_card_update(&platform, "the resubmitted Done card", CardUpdates::Any, |card| {
        card_header(card).contains("完成") && card_text(card).contains("第一轮回答。")
    })
    .await;

    // No resume write on this generation; the message was created by a new
    // Turn under its own id.
    assert!(
        backend.recovery_ops.lock().await.is_empty(),
        "V1 has no resume endpoint: no interrupt, no resume may reach the wire"
    );
    assert_eq!(
        backend.prompt_calls.lock().await.clone(),
        vec!["第一条消息".to_string(), "第一条消息".to_string()],
        "the degradation submits exactly one new prompt"
    );
    assert_eq!(
        backend.prompt_message_ids.lock().await.clone(),
        vec![
            Some("msg_cola_anchor".to_string()),
            Some("msg_cola_anchor".to_string())
        ],
        "the resubmit reuses the message's own id"
    );
    assert!(!app.inflight.lock().await.contains("ses_test"));
}

/// Ticket #437 (ADR-0062): the other V1 shape — the message landed between
/// the Unreceived ending and the click (a delayed promotion). The degradation
/// still resubmits it as a new Turn under its own `msg_cola_` id; V1's prompt
/// upserts by id, so the re-post neither duplicates the transcript message nor
/// mints a second identity. The old card is collected as 「↩️ 已重试」 without
/// keeping the button.
#[tokio::test]
async fn resume_on_v1_reuses_the_message_id_when_it_landed_in_between() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let mut backend = MockBackend::new(realistic_parts());
    // V1: no resume endpoint (the mock's default, stated explicitly).
    backend.with_resume_supported(false);
    backend.given_transcript("ses_test", vec![SessionTranscript::new(Vec::new())]);
    let backend = Arc::new(backend);
    let platform = Arc::new(RecordingPlatform::new());
    let app = Arc::new(App::new(cfg, backend.clone(), platform.clone()).unwrap());
    seed_session(&app, "ses_test", "/work").await;
    app.turn_render_poll_ms.store(5, Ordering::Relaxed);
    Turn::run(&app.turn_handles(), ctx("ses_test", "第一条消息"))
        .await
        .unwrap();
    assert_eq!(
        Turn::card_state(&app.cards_handle(), "ses_test").await,
        Some(CardState::Unreceived)
    );

    script_transcript(
        &backend,
        vec![SessionTranscript::new(vec![
            user("msg_cola_anchor", 1_000, "第一条消息"),
            assistant(2_000, "第一轮回答。"),
        ])],
    )
    .await;

    let result = app
        .host_action(serde_json::json!({ "action": "resume", "session_id": "ses_test" }))
        .await
        .expect("the V1 degradation must still be claimable");
    assert_eq!(result.toast.as_deref(), Some("正在重新发起..."));

    // The resubmitted Turn runs to its true end...
    wait_for_card_header(&platform, "完成").await;
    let wait_finished = async {
        while app.inflight.lock().await.contains("ses_test") {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    };
    tokio::time::timeout(Duration::from_secs(5), wait_finished)
        .await
        .expect("the resubmitted turn must finish");

    // ...while the old Unreceived card is collected as retried: a new attempt
    // exists below, and the collected card keeps no 重新发起 button.
    wait_for_card_header(&platform, "已重试").await;
    let retried = platform
        .updated_cards()
        .await
        .into_iter()
        .find(|card| card_header(card).contains("已重试"))
        .expect("the Unreceived card must be marked retried");
    assert!(
        card_buttons(&retried)
            .iter()
            .all(|button| button["value"]["action"] != "resume"),
        "a collected card must not keep 重新发起: {retried}"
    );

    // No resume write on this generation; the message was resubmitted as a new
    // Turn under its own id (V1 upserts by id, so nothing is duplicated).
    assert!(
        backend.recovery_ops.lock().await.is_empty(),
        "V1 has no resume endpoint: no interrupt, no resume may reach the wire"
    );
    assert!(backend.interrupt_calls.lock().await.is_empty());
    assert!(backend.resume_calls.lock().await.is_empty());
    assert_eq!(
        backend.prompt_calls.lock().await.clone(),
        vec!["第一条消息".to_string(), "第一条消息".to_string()],
        "the degradation submits exactly one new prompt"
    );
    assert_eq!(
        backend.prompt_message_ids.lock().await.clone(),
        vec![
            Some("msg_cola_anchor".to_string()),
            Some("msg_cola_anchor".to_string())
        ],
        "the resubmit must reuse the message's own id"
    );
}

/// Ticket #437: a failed resume write releases the claim and leaves the card
/// Unreceived — the button keeps working for a later press instead of turning
/// into a dead end.
#[tokio::test]
async fn a_failed_resume_leaves_the_unreceived_card_actionable() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let mut backend = MockBackend::new(realistic_parts());
    backend.with_resume_supported(true);
    backend.fail_resumes(1, "Simulated resume failure");
    backend.given_transcript("ses_test", vec![SessionTranscript::new(Vec::new())]);
    let backend = Arc::new(backend);
    let platform = Arc::new(RecordingPlatform::new());
    let app = Arc::new(App::new(cfg, backend.clone(), platform.clone()).unwrap());
    seed_session(&app, "ses_test", "/work").await;
    app.turn_render_poll_ms.store(5, Ordering::Relaxed);
    Turn::run(&app.turn_handles(), ctx("ses_test", "第一条消息"))
        .await
        .unwrap();

    let first = app
        .host_action(serde_json::json!({ "action": "resume", "session_id": "ses_test" }))
        .await;
    assert!(first.is_some(), "the first click claims the resume");

    // The failed write must give the claim back, so a later press can try
    // again; the scripted failure is consumed, so the retry succeeds.
    let second = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if let Some(result) = app
                .host_action(serde_json::json!({ "action": "resume", "session_id": "ses_test" }))
                .await
            {
                return result;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("a failed resume must leave the button actionable");
    assert_eq!(second.toast.as_deref(), Some("正在重新发起..."));
    let wait_recorded = async {
        while backend.resume_calls.lock().await.len() < 2 {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    };
    tokio::time::timeout(Duration::from_secs(5), wait_recorded)
        .await
        .expect("the retry's resume write must reach the backend");
    assert_eq!(
        backend.resume_calls.lock().await.clone(),
        vec!["ses_test".to_string(), "ses_test".to_string()],
        "the first press failed once; the second press tried again"
    );
}

/// ADR-0062 (spec #434, ticket #436): the whole #428 shape through the real
/// message path — the advisory status read says live (a stale active entry),
/// so the new Turn's card opens with the merge line, but no runner ever
/// promotes the steered message and the session idles. The card that owns the
/// message ends Unreceived, never ✅.
#[tokio::test]
async fn a_stale_live_submit_that_never_lands_ends_unreceived() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let mut backend = MockBackend::new(realistic_parts());
    // The advisory read (the routing decision's) says live; every read after
    // it serves the map — idle. The scenario is a stale status, not a run.
    backend.busy_then_idle_once();
    // The message never lands: no read carries cola's prompt (the inbox steer
    // no runner promoted).
    backend.given_transcript("ses_test", vec![SessionTranscript::new(Vec::new())]);
    let backend = Arc::new(backend);
    let platform = Arc::new(RecordingPlatform::new());
    let app = Arc::new(App::new(cfg, backend.clone(), platform.clone()).unwrap());
    seed_session(&app, "ses_test", "/work").await;
    app.turn_render_poll_ms.store(5, Ordering::Relaxed);
    app.handle_message(incoming(
        "msg_1".into(),
        "chat_1".into(),
        "p2p".into(),
        None,
        "补充一下，改用方案 B".into(),
        None,
    ))
    .await;

    // The routing read live and still started a Turn (ADR-0062's advisory
    // rule): the message rode its own prompt.
    assert!(
        backend
            .prompt_calls
            .lock()
            .await
            .iter()
            .any(|text| text == "补充一下，改用方案 B"),
        "the message must ride its own Turn's prompt: {:?}",
        backend.prompt_calls.lock().await
    );
    // The card opened with the merge line and, the message never landing,
    // ended Unreceived — never ✅.
    let final_card = platform.updated_cards().await.last().cloned().unwrap();
    assert!(
        card_header(&final_card).contains("⚠️ 这条消息未被接收"),
        "a never-landed stale-live submit ends Unreceived: {final_card}"
    );
    assert!(
        card_text(&final_card).contains("📨 已收到，将并入当前运行"),
        "the advisory opening line stays on the card: {final_card}"
    );
    assert!(
        !card_header(&final_card).contains("完成"),
        "an Unreceived card is never ✅: {final_card}"
    );
    assert!(
        !app.inflight.lock().await.contains("ses_test"),
        "the guard is released at the Unreceived ending"
    );
}

/// ADR-0062 (spec #434, ticket #436): while the session reads live but the
/// submitted message has not landed, the merged drain keeps the card and the
/// guard; it gains the neutral 「⏳ 等待当前运行接收…」 line only once the grace
/// has passed since the turn's start — a genuine long tool call and a dead run
/// look identical from outside, so cola does not nag early. When the session
/// then idles with the message still absent, the drain ends it Unreceived,
/// never ✅, and the hint stays on the settled card.
#[tokio::test]
async fn a_never_promoted_submit_on_a_live_run_waits_out_the_grace_then_ends_unreceived() {
    let _wd = test_work_dir();
    // The stale-live read of #428: the session reports Busy while the message
    // never lands in the transcript.
    let (_dir, app, backend, platform) = scripted_app(
        vec![SessionTranscript::new(Vec::new())],
        Some(SessionStatus::Busy),
    )
    .await;
    app.turn_render_poll_ms.store(5, Ordering::Relaxed);
    // The grace is both the hint's delay and the failure ceiling; measured
    // from the turn's own start.
    app.turn_follow_grace_ms.store(800, Ordering::Relaxed);
    app.turn_follow_read_timeout_ms.store(20, Ordering::Relaxed);

    let _turn = spawn_turn(&app, ctx("ses_test", "第一条消息"));
    tokio::time::timeout(Duration::from_secs(5), async {
        while !app.inflight.lock().await.contains("ses_test") {
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
    })
    .await
    .expect("the drain holds the guard while the message has not landed");

    // Before the grace passes the card must not nag: the live session may
    // still be a genuine long tool call that merges the message.
    tokio::time::sleep(Duration::from_millis(100)).await;
    let early = platform.updated_cards().await;
    assert!(
        early
            .iter()
            .all(|card| !card_text(card).contains("等待当前运行接收")),
        "the waiting hint must not appear before the grace: {:?}",
        early.last()
    );

    // After the grace the neutral line arrives, exactly once, on a card that
    // is still not ✅.
    wait_for_card_text(&platform, "⏳ 等待当前运行接收…").await;
    let hinted = platform.updated_cards().await.last().cloned().unwrap();
    assert!(
        !card_header(&hinted).contains("完成"),
        "the waiting card is never ✅: {hinted}"
    );
    assert!(
        app.inflight.lock().await.contains("ses_test"),
        "the watch keeps the card while the session reads live"
    );

    // The session idles with the message still absent: Unreceived, never ✅.
    backend
        .set_session_status("ses_test", Some(SessionStatus::Idle))
        .await;
    wait_for_card_header(&platform, "未被接收").await;
    let final_card = platform.updated_cards().await.last().cloned().unwrap();
    assert_eq!(
        card_text(&final_card).matches("等待当前运行接收").count(),
        1,
        "the hint is pushed exactly once: {final_card}"
    );
    assert!(
        !card_header(&final_card).contains("完成"),
        "an Unreceived card is never ✅: {final_card}"
    );
    wait_for_guard_release(&app).await;
    assert_no_further_rendering(&backend, &platform).await;
}

/// ADR-0062 (spec #434, ticket #436): the unreceived wait is a wait, not a
/// verdict — the submitted message may still land. A genuinely live run that
/// merges the steer must be captured by the drain, its reply rendered, and the
/// card settled normally (Done), never Unreceived.
#[tokio::test]
async fn the_unreceived_wait_captures_a_message_that_lands_after_the_grace() {
    let _wd = test_work_dir();
    // The stale-looking start: Busy, with no transcript carrying the message.
    let (_dir, app, backend, platform) = scripted_app(
        vec![SessionTranscript::new(Vec::new())],
        Some(SessionStatus::Busy),
    )
    .await;
    app.turn_render_poll_ms.store(5, Ordering::Relaxed);
    // No hint in this test: the message lands well inside the grace.
    app.turn_follow_grace_ms.store(60_000, Ordering::Relaxed);
    app.turn_follow_read_timeout_ms.store(20, Ordering::Relaxed);

    let _turn = spawn_turn(&app, ctx("ses_test", "第一条消息"));
    tokio::time::timeout(Duration::from_secs(5), async {
        while !app.inflight.lock().await.contains("ses_test") {
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
    })
    .await
    .expect("the drain holds the guard while the message has not landed");

    // The run merges the steered message: it lands, with its answer.
    script_transcript(
        &backend,
        vec![SessionTranscript::new(vec![
            user("msg_cola_anchor", 1_000, "第一条消息"),
            assistant(2_000, "第一轮回答。"),
        ])],
    )
    .await;
    // The drain captures the anchor and renders the answer while the session
    // still reads live.
    wait_for_card_text(&platform, "第一轮回答。").await;
    assert!(
        app.inflight.lock().await.contains("ses_test"),
        "the drain keeps observing while the session reads live"
    );

    // The run ends: the settle decision judges the captured Turn and Done.
    backend
        .set_session_status("ses_test", Some(SessionStatus::Idle))
        .await;
    wait_for_card_header(&platform, "完成").await;
    wait_for_guard_release(&app).await;
    let final_card = platform.updated_cards().await.last().cloned().unwrap();
    assert!(
        !card_header(&final_card).contains("未被接收"),
        "a message that landed is never Unreceived: {final_card}"
    );
    assert_no_further_rendering(&backend, &platform).await;
}

/// An undecided read keeps the Turn observing (ADR-0062's "no divergent
/// ending path"): the drain settles on one read, but the re-check finds a read
/// carrying an unanswered Wake — no ending may be claimed from it, so the Turn
/// keeps observing rather than finalizing from the drain's last disposition
/// (the pre-#436 path stamped Done here). It settles once the Wake's Execution
/// boundary arrives.
#[tokio::test]
async fn an_undecided_read_keeps_the_turn_observing_not_done() {
    let _wd = test_work_dir();
    let settled = SessionTranscript::new(vec![
        user("msg_cola_anchor", 1_000, "跑一下 CI"),
        assistant(2_000, "已经交给后台了。"),
    ]);
    // The re-check read: a Wake resumed the run and its Execution has not
    // reached a boundary yet — the read cannot judge the Turn.
    let waked = SessionTranscript::new(vec![
        user("msg_cola_anchor", 1_000, "跑一下 CI"),
        assistant(2_000, "已经交给后台了。"),
    ])
    .with_wakes(vec![shell_wake(2_900)]);
    let answered = SessionTranscript::new(vec![
        user("msg_cola_anchor", 1_000, "跑一下 CI"),
        assistant(2_000, "已经交给后台了。"),
        assistant(3_100, "CI 通过了。"),
    ])
    .with_executions(vec![execution(3_500)])
    .with_wakes(vec![shell_wake(2_900)]);
    let (_dir, app, backend, platform) = scripted_app(vec![settled, waked], Some(SessionStatus::Idle)).await;
    app.turn_render_poll_ms.store(5, Ordering::Relaxed);

    let _turn = spawn_turn(&app, ctx("ses_test", "跑一下 CI"));
    // The undecided read keeps the Turn observing: never Done/Unreceived, the
    // guard still held.
    tokio::time::sleep(Duration::from_millis(40)).await;
    let state = Turn::card_state(&app.cards_handle(), "ses_test").await;
    assert!(
        !matches!(state, Some(CardState::Done | CardState::Unreceived)),
        "an undecided read must not finalize the card: {state:?}"
    );
    assert!(
        app.inflight.lock().await.contains("ses_test"),
        "the Turn keeps observing instead of finalizing"
    );

    // The Wake's Execution boundary arrives: the true end settles on the card.
    script_transcript(&backend, vec![answered]).await;
    wait_for_card_header(&platform, "完成").await;
    wait_for_guard_release(&app).await;
    let final_card = platform.updated_cards().await.last().cloned().unwrap();
    assert!(
        card_text(&final_card).contains("CI 通过了。"),
        "the Wake's resumed work lands at the true end: {final_card}"
    );
    assert_no_further_rendering(&backend, &platform).await;
}

/// The settle mapping is singular (AC4): the confirmation window still bounds
/// an anchored read whose run was never observed, but the drain's own ending
/// for it now comes from the settle decision, not the retired window-only
/// rule — a read carrying live Background Tasks yields 「⏳ 等待后台任务」
/// rather than reporting the turn settled. Finalization reaches the same
/// yield, so the card can never show two readings of one decision.
#[tokio::test]
async fn an_unobserved_run_with_live_background_tasks_yields_waiting() {
    let _wd = test_work_dir();
    let transcript = SessionTranscript::new(vec![user("msg_cola_anchor", 1_000, "跑一下 CI")])
        .with_executions(vec![execution(1_500)])
        .with_background_tasks(vec![background_shell(1_100)]);
    let (_dir, app, _backend, platform) = scripted_app(vec![transcript], Some(SessionStatus::Idle)).await;
    app.turn_render_poll_ms.store(5, Ordering::Relaxed);
    // A bound the test would never wait out: only the window can end the drain.
    let started = std::time::Instant::now();
    Turn::run(&app.turn_handles(), ctx("ses_test", "跑一下 CI"))
        .await
        .unwrap();
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "the confirmation window must bound the wait: {:?}",
        started.elapsed()
    );

    assert_eq!(
        Turn::card_state(&app.cards_handle(), "ses_test").await,
        Some(CardState::Waiting),
        "the settle decision owns the unobserved ending too"
    );
    let card = platform.updated_cards().await.last().cloned().unwrap();
    assert!(
        card_header(&card).contains("等待后台任务"),
        "the card yields for the live task: {card}"
    );
    assert!(
        !card_header(&card).contains("完成"),
        "a waiting card is never ✅: {card}"
    );
    assert!(!noticed(&platform).await, "no notice before the true end");
    assert!(
        !app.inflight.lock().await.contains("ses_test"),
        "the yield releases the guard"
    );
}

/// ADR-0062 (ticket #436, review fix): the Turn's anchor is sticky. A final
/// read that no longer carries the submitted message (a compaction, a partial
/// read) must not flip a completed card to Unreceived: the captured anchor
/// already proved the message landed, so the read's own missing anchor is not
/// "never landed".
#[tokio::test]
async fn a_final_read_dropping_the_message_does_not_become_unreceived() {
    let _wd = test_work_dir();
    let timeline = vec![
        user("msg_cola_anchor", 1_000, "第一条消息"),
        assistant(2_000, "第一轮回答。"),
    ];
    // Snapshot 1 is the settled turn the drain anchors and renders under the
    // busy read; every read after it is anchorless — the compaction /
    // partial-read hiccup.
    let (_dir, app, backend, platform) = scripted_app(
        vec![
            SessionTranscript::new(timeline),
            SessionTranscript::new(Vec::new()),
        ],
        Some(SessionStatus::Busy),
    )
    .await;
    app.turn_render_poll_ms.store(5, Ordering::Relaxed);
    let turn = spawn_turn(&app, ctx("ses_test", "第一条消息"));
    // The busy drain renders and anchors the turn...
    wait_for_card_text(&platform, "第一轮回答。").await;
    // ...then the session idles and every later read is anchorless.
    backend
        .set_session_status("ses_test", Some(SessionStatus::Idle))
        .await;
    let result = tokio::time::timeout(Duration::from_secs(5), turn)
        .await
        .expect("the turn must finalize once the session idles")
        .unwrap();
    result.unwrap();

    assert_eq!(
        Turn::card_state(&app.cards_handle(), "ses_test").await,
        Some(CardState::Done),
        "a landed message stays Done after an anchorless final read"
    );
    let final_card = platform.updated_cards().await.last().cloned().unwrap();
    assert!(
        card_header(&final_card).contains("完成"),
        "a landed message stays Done after an anchorless final read: {final_card}"
    );
    assert!(
        !card_header(&final_card).contains("未被接收"),
        "the sticky anchor must not be read as never-landed: {final_card}"
    );
    assert!(
        card_text(&final_card).contains("第一轮回答。"),
        "the drained content stays: {final_card}"
    );
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

// ---------------------------------------------------------------------------
// The settle decision (ADR-0059, spec #405 ticket 2): a Turn whose Execution
// idled with live Background Tasks yields 「⏳ 等待后台任务」 — never ✅, no
// Completion Notice — and completes only at the true end.
// ---------------------------------------------------------------------------

/// Replace the session's scripted transcript snapshots (what the next Backend
/// reads serve).
pub(crate) async fn script_transcript(backend: &Arc<MockBackend>, snapshots: Vec<SessionTranscript>) {
    *backend
        .transcript_scripts
        .lock()
        .await
        .get_mut("ses_test")
        .expect("a scripted session") = snapshots;
}

/// Whether the platform sent any Completion Notice — shared by the drain's
/// settle tests and the collect tests (spec #405).
pub(crate) async fn noticed(platform: &RecordingPlatform) -> bool {
    platform
        .calls
        .lock()
        .await
        .iter()
        .any(|call| matches!(call, PlatformCall::CompletionNotice { .. }))
}

/// Start Session Sync's poll loop with tiny injected cadences (the sync tick,
/// the continuation render tick and every read bound) — shared by the Wake
/// continuation tests and the collect tests.
pub(crate) fn spawn_sync(app: &Arc<App>) {
    spawn_sync_with_timeout(app, 50);
}

/// [`spawn_sync`] with an explicit request bound: a test that parks a bounded
/// platform call (a `RecordingPlatform` gate) must not race the bound.
pub(crate) fn spawn_sync_with_timeout(app: &Arc<App>, timeout_ms: u64) {
    app.external.poll_interval_ms.store(20, Ordering::Relaxed);
    app.external.render_poll_ms.store(5, Ordering::Relaxed);
    app.external
        .request_timeout_ms
        .store(timeout_ms, Ordering::Relaxed);
    let app = app.clone();
    tokio::spawn(async move {
        let _ = app.external.poll_loop(&app.flow_handles()).await;
    });
}

/// A V2 group turn whose Execution idles with a live Background Task yields
/// 「⏳ 等待后台任务」: not ✅, not a terminal, and no Completion Notice — the
/// notice belongs to the true end (spec #405 ticket 2).
#[tokio::test]
async fn a_turn_idling_with_a_live_background_task_yields_waiting() {
    let _wd = test_work_dir();
    let transcript = SessionTranscript::new(vec![
        user("msg_cola_anchor", 1_000, "跑一下 CI"),
        assistant(2_000, "已经交给后台了。"),
    ])
    .with_executions(vec![execution(2_500)])
    .with_background_tasks(vec![background_shell(2_100)]);
    let (_dir, app, backend, platform) = scripted_app(vec![transcript], Some(SessionStatus::Idle)).await;
    // A group turn would notify at a true end, so a missing notice proves the
    // yield deferred it.
    let mut context = ctx("ses_test", "跑一下 CI");
    context.is_group = true;
    context.requester_open_id = Some(TEST_HOST.to_string());

    Turn::run(&app.turn_handles(), context).await.unwrap();

    assert_eq!(
        Turn::card_state(&app.cards_handle(), "ses_test").await,
        Some(CardState::Waiting),
        "the idle-with-live-task ending yields the waiting disposition"
    );
    let card = platform.updated_cards().await.last().cloned().unwrap();
    assert!(
        card_header(&card).contains("等待后台任务"),
        "the yield names the wait: {card}"
    );
    assert!(
        !card_header(&card).contains("完成"),
        "a waiting card is never ✅: {card}"
    );
    assert!(
        !noticed(&platform).await,
        "no Completion Notice before the true end: {:?}",
        platform.calls.lock().await
    );
    assert!(
        !app.inflight.lock().await.contains("ses_test"),
        "the guard is released at the yield"
    );
    assert_no_further_rendering(&backend, &platform).await;
}

/// The yield's true end (ADR-0060, ticket #420): the last Background Task
/// retires with nothing to render, so Session Sync's own read settles the
/// WAITING card in place — ✅, the fixed completion entry on the card that
/// hosted the task, no new card. This supersedes the old "a retirement leaves
/// the waiting card alone" expectation: a retirement that still leaves a task
/// live (or one whose resumed run renders work) does not settle, but the last
/// quiet one does. The p2p turn carries no notice opt-in and no requester, so
/// nothing notifies; the notice's own cases live in `ledger.rs`.
#[tokio::test]
async fn a_quiet_retirement_after_the_yield_settles_the_waiting_card() {
    let _wd = test_work_dir();
    let live = SessionTranscript::new(vec![
        user("msg_cola_anchor", 1_000, "跑一下 CI"),
        assistant(2_000, "已经交给后台了。"),
    ])
    .with_executions(vec![execution(2_500)])
    .with_background_tasks(vec![background_shell(2_100)]);
    let (_dir, app, backend, platform) = scripted_app(vec![live], Some(SessionStatus::Idle)).await;

    Turn::run(&app.turn_handles(), ctx("ses_test", "跑一下 CI"))
        .await
        .unwrap();
    assert_eq!(
        Turn::card_state(&app.cards_handle(), "ses_test").await,
        Some(CardState::Waiting)
    );
    Turn::set_card_message_id(&app.cards_handle(), "ses_test", "om_waiting").await;

    // The task retires later and its Execution ends — and its run resumes
    // nothing, so the read is the chain's true end.
    script_transcript(
        &backend,
        vec![
            SessionTranscript::new(vec![
                user("msg_cola_anchor", 1_000, "跑一下 CI"),
                assistant(2_000, "已经交给后台了。"),
            ])
            .with_executions(vec![execution(2_500), execution(4_000)])
            .with_wakes(vec![shell_wake(2_900)]),
        ],
    )
    .await;
    spawn_sync(&app);
    wait_for_card_update(&platform, "the settled card", CardUpdates::Latest, |card| {
        card_header(card).contains("✅") && card_text(card).contains("🔔 shell 完成：gh run watch")
    })
    .await;

    assert_eq!(
        Turn::card_state(&app.cards_handle(), "ses_test").await,
        Some(CardState::Done),
        "the quiet true end is the card's terminal"
    );
    let settled = platform.updated_cards().await.last().cloned().unwrap();
    let text = card_text(&settled);
    assert!(
        !text.contains("后台任务（"),
        "the retired task's live list is gone: {settled}"
    );
    assert!(
        text.contains("🔔 shell 完成：gh run watch") && text.contains("shell sh_bg · "),
        "the fixed entry stays on the card that hosted the task: {settled}"
    );
    assert!(
        continuation_sends(&platform).await.is_empty(),
        "a quiet true end owes no continuation: {:?}",
        platform.calls.lock().await
    );
    assert!(
        !noticed(&platform).await,
        "no notice opt-in and no requester: nothing notifies"
    );
}

/// The retirement can land inside the finalization window: the drain's read
/// shows the live task (a waiting yield), and the final reconcile read shows
/// the Wake and its Execution boundary — the true end, so the turn ends ✅
/// with its notice instead of yielding.
#[tokio::test]
async fn a_retirement_inside_the_finalization_window_ends_the_turn_normally() {
    let _wd = test_work_dir();
    let live = SessionTranscript::new(vec![
        user("msg_cola_anchor", 1_000, "跑一下 CI"),
        assistant(2_000, "已经交给后台了。"),
    ])
    .with_executions(vec![execution(2_500)])
    .with_background_tasks(vec![background_shell(2_100)]);
    let retired = SessionTranscript::new(vec![
        user("msg_cola_anchor", 1_000, "跑一下 CI"),
        assistant(2_000, "已经交给后台了。"),
        assistant(3_100, "CI 通过了。"),
    ])
    .with_executions(vec![execution(2_500), execution(4_000)])
    .with_wakes(vec![shell_wake(2_900)]);
    let (_dir, app, _backend, platform) = scripted_app(vec![live, retired], Some(SessionStatus::Idle)).await;
    let mut context = ctx("ses_test", "跑一下 CI");
    context.is_group = true;
    context.requester_open_id = Some(TEST_HOST.to_string());

    Turn::run(&app.turn_handles(), context).await.unwrap();

    let card = platform.updated_cards().await.last().cloned().unwrap();
    assert!(
        card_header(&card).contains("完成"),
        "a retirement inside the window is the true end: {card}"
    );
    assert!(
        card_text(&card).contains("CI 通过了。"),
        "the true end's content lands: {card}"
    );
    assert!(
        noticed(&platform).await,
        "the notice fires at the true end: {:?}",
        platform.calls.lock().await
    );
}

/// Wait until at least `target` transcript reads have reached the mock's hold
/// gate, or panic after 5 s.
async fn wait_for_parked_reads(backend: &Arc<MockBackend>, target: usize) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while backend.transcript_gate_entered.load(Ordering::SeqCst) < target {
        assert!(
            tokio::time::Instant::now() < deadline,
            "the drain never reached parked read #{target}"
        );
        tokio::time::sleep(Duration::from_millis(2)).await;
    }
}

/// Spec #604: the final part lands BETWEEN the drain's render read and the
/// later idle-status read, and still lands on the Card — the turn ends on that
/// same rendered, quiescent snapshot, never finalizing the stale read into a
/// Wake-less continuation Card (spec evidence
/// `ses_ee3f4531fffeBcOYDY3vHUQDPw`).
///
/// The render read genuinely PREDATES the tail: it is served the tail-less
/// snapshot (the session still busy) and returns before the tail exists. The
/// tail then lands while the next read is parked, and the session idles with
/// it; that LATER read is the one that must carry the tail and render it, so
/// the ending is never stamped over the stale read and no second Card appears.
/// (Contrast the short-cut this test used to take: it served the tail to the
/// parked read itself, so the render read already carried it.)
#[tokio::test]
async fn a_final_part_landing_between_the_render_and_idle_read_still_lands() {
    let _wd = test_work_dir();
    let rendered = vec![
        user("msg_cola_anchor", 1_000, "第一条消息"),
        assistant(2_000, "第一段。"),
    ];
    let with_tail = vec![
        user("msg_cola_anchor", 1_000, "第一条消息"),
        assistant(2_000, "第一段。"),
        assistant(3_000, "第二段。"),
    ];
    let (_dir, app, backend, platform) = scripted_app(
        vec![SessionTranscript::new(rendered.clone())],
        Some(SessionStatus::Busy),
    )
    .await;
    // A grace and a per-read bound the test would never wait out: only the
    // state transitions below may end the drain.
    app.turn_follow_grace_ms.store(60_000, Ordering::Relaxed);
    app.turn_follow_read_timeout_ms.store(60_000, Ordering::Relaxed);

    let _turn = spawn_turn(&app, ctx("ses_test", "第一条消息"));
    wait_for_card_text(&platform, "第一段。").await;
    // Let the in-flight render poll (the submit window) stop: from here the
    // drain is the only transcript reader, so the read parked below is its own.
    tokio::time::sleep(Duration::from_millis(30)).await;

    // Park the drain's RENDER read. The session is still busy and this read
    // serves the tail-less snapshot: the render read genuinely predates the
    // tail.
    let gate = backend.hold_transcripts();
    wait_for_parked_reads(&backend, 1).await;
    script_transcript(&backend, vec![SessionTranscript::new(rendered.clone())]).await;
    gate.add_permits(1);

    // Park the LATER read — the settle re-check. The tail lands now, AFTER the
    // render read, and the session idles: this later read carries the tail.
    wait_for_parked_reads(&backend, 2).await;
    script_transcript(&backend, vec![SessionTranscript::new(with_tail.clone())]).await;
    backend
        .set_session_status("ses_test", Some(SessionStatus::Idle))
        .await;
    // Disarm first: only the two parked reads are gated, so the reads that
    // follow serve the tail (now on the Card) without parking.
    *backend.transcript_gate.lock().unwrap() = None;
    gate.add_permits(1);

    wait_for_guard_release(&app).await;
    let final_card = platform.updated_cards().await.last().cloned().unwrap();
    assert!(
        card_header(&final_card).contains("完成"),
        "the turn ends on the Card: {final_card}"
    );
    assert!(
        card_text(&final_card).contains("第二段。"),
        "the tail that landed after the render read must reach the Card: {final_card}"
    );

    // The durable transcript still carries the tail: Session Sync must find
    // nothing unrendered, so no second (Wake-less continuation) Card opens.
    let posts = card_posts(&platform).await;
    script_transcript(&backend, vec![SessionTranscript::new(with_tail)]).await;
    spawn_sync(&app);
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(
        card_posts(&platform).await,
        posts,
        "a rendered tail must not orphan into a second Card: {:?}",
        platform.calls.lock().await
    );
    assert!(
        continuation_sends(&platform).await.is_empty(),
        "no Wake-less continuation Card opens: {:?}",
        platform.calls.lock().await
    );
}

/// Finding 3 (spec #602 pre-push review, #604): a tail that lands AFTER the
/// drain has settled — after its last transcript read — must still reach the
/// Card. Finalization may not trust the drain's cached `last_transcript`
/// blindly; it must re-read and re-render until it observes a rendered,
/// quiescent snapshot. The old short-circuit stamped the ending on the stale,
/// tail-less snapshot, and the tail later orphaned into a second (residual)
/// Wake-less Card — the very #604 race.
#[tokio::test]
async fn a_tail_landing_after_the_drain_settled_still_lands_on_the_card() {
    let _wd = test_work_dir();
    let tail_less = vec![
        user("msg_cola_anchor", 1_000, "第一条消息"),
        assistant(2_000, "第一段。"),
    ];
    let with_tail = vec![
        user("msg_cola_anchor", 1_000, "第一条消息"),
        assistant(2_000, "第一段。"),
        assistant(3_000, "第二段。"),
    ];
    let (_dir, app, backend, platform) = scripted_app(
        vec![SessionTranscript::new(tail_less.clone())],
        Some(SessionStatus::Busy),
    )
    .await;
    app.turn_follow_grace_ms.store(60_000, Ordering::Relaxed);
    app.turn_follow_read_timeout_ms.store(60_000, Ordering::Relaxed);

    let _turn = spawn_turn(&app, ctx("ses_test", "第一条消息"));
    wait_for_card_text(&platform, "第一段。").await;
    // Let the in-flight submit-window render poll stop: from here the drain is
    // the only transcript reader, so the read frozen below is its own.
    tokio::time::sleep(Duration::from_millis(30)).await;

    // Freeze the drain at its next transcript read: with the session still
    // reading Busy this is a settle candidate, and no permit is added until the
    // state below is staged.
    let gate = backend.hold_transcripts();
    wait_for_parked_reads(&backend, 1).await;
    // The session goes Idle: the frozen read is the settle tick. Both it and the
    // drain's corroborating re-check are served the tail-less snapshot; the tail
    // is scripted only for the read AFTER them — finalization's own fresh read.
    backend
        .set_session_status("ses_test", Some(SessionStatus::Idle))
        .await;
    script_transcript(
        &backend,
        vec![
            SessionTranscript::new(tail_less.clone()),
            SessionTranscript::new(tail_less.clone()),
            SessionTranscript::new(with_tail.clone()),
        ],
    )
    .await;
    *backend.transcript_gate.lock().unwrap() = None;
    gate.add_permits(1);

    wait_for_guard_release(&app).await;
    let final_card = platform.updated_cards().await.last().cloned().unwrap();
    assert!(
        card_header(&final_card).contains("完成"),
        "the turn ends on the Card: {final_card}"
    );
    assert!(
        card_text(&final_card).contains("第二段。"),
        "a tail that landed after the drain settled must reach the Card: {final_card}"
    );
    assert!(
        continuation_sends(&platform).await.is_empty(),
        "no second/residual Card may open for the tail: {:?}",
        platform.calls.lock().await
    );
}

/// Finding 1 (spec #602 review, round 4): finalization must never hold the
/// inflight guard on a CONTINUING stream. A run whose `session_status` never
/// answers but whose every transcript read succeeds WITH new content ends the
/// drain in the lost-contact grace; the decided ending must be stamped at that
/// grace instead of finalization re-reading the live stream forever. The mock's
/// growing transcript makes every read carry a fresh body, so a "continuing
/// stream" is available deterministically; the turn still has to finalize in
/// error and release the guard within the bound (never hang).
#[tokio::test]
async fn a_continuing_stream_on_a_failed_status_read_still_finalizes_at_the_grace() {
    let _wd = test_work_dir();
    let timeline = vec![
        user("msg_cola_anchor", 1_000, "第一条消息"),
        assistant(2_000, "第一段。"),
    ];
    let (_dir, app, backend, platform) = scripted_app(vec![SessionTranscript::new(timeline)], None).await;
    // The status leg never answers: the drain can only end through the
    // lost-contact grace. The transcript leg keeps answering — with content the
    // card has not seen, on every read (a deterministic continuing stream).
    backend.session_status_fails.store(true, Ordering::SeqCst);
    backend.growing_transcript.store(true, Ordering::SeqCst);
    app.turn_follow_grace_ms.store(80, Ordering::Relaxed);
    app.turn_follow_read_timeout_ms.store(200, Ordering::Relaxed);

    let turn = spawn_turn(&app, ctx("ses_test", "第一条消息"));

    // The grace decides the run's ending even while the stream keeps going: the
    // turn must finalize and release the guard within the bound, never chase the
    // stream forever.
    let finalized = tokio::time::timeout(Duration::from_secs(2), async {
        wait_for_card_header(&platform, "出错").await;
        wait_for_guard_release(&app).await;
    })
    .await;
    assert!(
        finalized.is_ok(),
        "the turn must finalize at the grace, not chase the continuing stream"
    );
    tokio::time::timeout(Duration::from_secs(2), turn)
        .await
        .expect("the turn task must end with the finalized card")
        .unwrap()
        .unwrap();
    let final_card = platform.updated_cards().await.last().cloned().unwrap();
    assert!(
        card_text(&final_card).contains("失去联系"),
        "a lost-contact run still ends in error at the grace: {final_card}"
    );
}

/// Spec #602 review, round 5: finalization's reads are bounded by the REMAINING
/// grace budget, not the full per-read timeout. A read begun when the budget is
/// nearly spent must not hold the inflight guard for a whole
/// `follow_read_timeout_ms` past the grace. Here the drain's transcript reads
/// never answer (all parked), so the drain ends in the lost-contact grace and
/// finalization's own read is the last bounded call before the guard releases.
/// With the read timeout far above the grace, an unbounded finalization read
/// spends the full timeout; a bounded one spends only the remaining budget.
#[tokio::test]
async fn finalization_reads_are_bounded_by_the_remaining_grace_budget() {
    let _wd = test_work_dir();
    let timeline = vec![
        user("msg_cola_anchor", 1_000, "第一条消息"),
        assistant(2_000, "第一段。"),
    ];
    let (_dir, app, backend, _platform) =
        scripted_app(vec![SessionTranscript::new(timeline)], Some(SessionStatus::Busy)).await;
    // A tiny grace; the per-read timeout is far larger, so an unbounded
    // finalization read overshoots the grace by a full read timeout.
    app.turn_follow_grace_ms.store(60, Ordering::Relaxed);
    app.turn_follow_read_timeout_ms.store(600, Ordering::Relaxed);
    // The status leg never answers either: the drain can only end through the
    // lost-contact grace, and finalization reads exactly once.
    backend.session_status_fails.store(true, Ordering::SeqCst);

    // Park EVERY transcript read from the start (no permit is ever added), so
    // each read runs to its own bound before the caller gives up.
    let _gate = backend.hold_transcripts();
    let turn = spawn_turn(&app, ctx("ses_test", "第一条消息"));

    // Time from the first hung transcript read: the drain's own read bound is
    // fixed, so the extra time to the guard release is what finalization spends.
    wait_for_parked_reads(&backend, 1).await;
    let start = tokio::time::Instant::now();
    wait_for_guard_release(&app).await;
    let held = start.elapsed();

    // `held` spans the drain's own (fixed 600 ms) read PLUS finalization. Fixed:
    // ~600 ms + the ~60 ms remaining grace ≈ 660 ms. A regression that spent the
    // FULL 600 ms read timeout in finalization would instead be ~1200 ms, so a
    // threshold well under that (800 ms) distinguishes the two and fails a
    // full-timeout read.
    assert!(
        held < Duration::from_millis(800),
        "finalization held the guard {held:?} after the drain — a read must be bound by the remaining budget, not the full read timeout"
    );

    tokio::time::timeout(Duration::from_secs(3), turn)
        .await
        .expect("the turn task must end with the finalized card")
        .unwrap()
        .unwrap();
}

/// Finding 1 (spec #602 review, round 5, #604): a settled drain whose transcript
/// keeps GROWING after the settle must never give up and stamp an ending on a
/// non-quiescent read. When finalization's fresh read still carries content the
/// card lacks the run went live again, so the drain is RE-OPENED and the run is
/// followed to its true end by the drain's own settle logic — no total budget,
/// no give-up. Only when the growth stops does the tail land, once, on the one
/// Card.
///
/// The old bounded finalization re-read spent the grace and then returned its
/// last (contentful) snapshot, stamping ✅ over a read that still showed new
/// content — exactly the #604 race. The ever-changing stream makes every read
/// carry a fresh body, so "still growing" is deterministic.
#[tokio::test]
async fn a_settled_drain_whose_stream_keeps_growing_reopens_instead_of_stamping() {
    let _wd = test_work_dir();
    let tail_less = vec![
        user("msg_cola_anchor", 1_000, "第一条消息"),
        assistant(2_000, "第一段。"),
    ];
    let (_dir, app, backend, platform) = scripted_app(
        vec![SessionTranscript::new(tail_less.clone())],
        Some(SessionStatus::Busy),
    )
    .await;
    // A grace the old bounded re-read would spend and then stamp on: it is the
    // distinguishing budget. The per-read bound stays large so a parked read
    // never times out (the drain's lost-contact grace must not fire while the
    // test holds a read).
    app.turn_follow_grace_ms.store(60, Ordering::Relaxed);
    app.turn_follow_read_timeout_ms.store(60_000, Ordering::Relaxed);

    let _turn = spawn_turn(&app, ctx("ses_test", "第一条消息"));
    wait_for_card_text(&platform, "第一段。").await;
    // Let the submit-window render poll stop: from here the drain is the only
    // transcript reader, so the reads parked below are its own.
    tokio::time::sleep(Duration::from_millis(30)).await;

    // Park the drain's settle read (gate-entered read #1). The hook turns the
    // ever-changing stream ON for gate-entered read #3 — finalization's fresh
    // confirmation read — so the drain settles on a quiescent read and the
    // growth starts exactly on the read that must not be stamped on.
    let gate = backend.hold_transcripts();
    wait_for_parked_reads(&backend, 1).await;
    {
        let stream = Arc::clone(&backend);
        let fired = std::sync::atomic::AtomicBool::new(false);
        *backend.on_transcript_read.lock().unwrap() = Some(Box::new(move |entered| {
            // Fire exactly once, at finalization's read: a later `false` from
            // the test (growth stopped) must stick.
            if entered >= 3 && !fired.swap(true, Ordering::SeqCst) {
                stream.growing_transcript.store(true, Ordering::SeqCst);
            }
        }));
    }
    // Read #1 (the parked settle read) is served the quiescent, tail-less
    // snapshot: the session is Idle, so the drain settles. Read #2 corroborates
    // it; read #3 is finalization's own.
    backend
        .set_session_status("ses_test", Some(SessionStatus::Idle))
        .await;
    gate.add_permits(1);

    // Well past the grace, with content still arriving, the turn must still be
    // observing — an ending is never stamped on a read that keeps showing new
    // content.
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(
        app.inflight.lock().await.contains("ses_test"),
        "a settled drain whose stream keeps growing must re-open, not stamp an ending on a live read"
    );
    assert_ne!(
        Turn::card_state(&app.cards_handle(), "ses_test").await,
        Some(CardState::Done),
        "no ending is stamped while content still arrives"
    );

    // The growth stops: the run reaches its true end and lands on the one Card.
    backend.growing_transcript.store(false, Ordering::SeqCst);
    wait_for_guard_release(&app).await;
    let final_card = platform.updated_cards().await.last().cloned().unwrap();
    assert!(
        card_header(&final_card).contains("完成"),
        "the turn ends on the Card once the growth stops: {final_card}"
    );
    assert!(
        card_text(&final_card).contains("段。"),
        "the growing stream's content kept landing on the Card: {final_card}"
    );
    assert!(
        continuation_sends(&platform).await.is_empty(),
        "one request, one Card — no continuation: {:?}",
        platform.calls.lock().await
    );
}

/// A Wake's content landing in the finalization window must not satisfy the
/// Turn's completion check: its Execution has not reached a boundary yet, so
/// the Turn keeps observing (the old "any terminal step since the anchor" rule
/// is gone) and only the later boundary ends it.
#[tokio::test]
async fn a_wake_in_the_finalization_window_cannot_complete_the_turn_early() {
    let _wd = test_work_dir();
    let running = SessionTranscript::new(vec![
        user("msg_cola_anchor", 1_000, "跑一下 CI"),
        assistant(2_000, "已经交给后台了。"),
    ])
    .with_executions(vec![execution(2_500)])
    .with_background_tasks(vec![background_shell(2_100)]);
    let (_dir, app, backend, platform) = scripted_app(vec![running], Some(SessionStatus::Busy)).await;
    let mut context = ctx("ses_test", "跑一下 CI");
    context.is_group = true;
    context.requester_open_id = Some(TEST_HOST.to_string());

    let turn = spawn_turn(&app, context);
    // The drain is polling the busy session.
    wait_for_card_text(&platform, "已经交给后台了。").await;

    // The Wake retires the task and its content streams, but its Execution has
    // not idled: the read is idle-looking and the task is gone. The old rule
    // would stamp ✅ here from the Wake's terminal step.
    script_transcript(
        &backend,
        vec![
            SessionTranscript::new(vec![
                user("msg_cola_anchor", 1_000, "跑一下 CI"),
                assistant(2_000, "已经交给后台了。"),
                assistant(3_100, "CI 通过了，正在合并。"),
            ])
            .with_executions(vec![execution(2_500)])
            .with_wakes(vec![shell_wake(2_900)]),
        ],
    )
    .await;
    backend
        .set_session_status("ses_test", Some(SessionStatus::Idle))
        .await;

    tokio::time::sleep(Duration::from_millis(40)).await;
    let state = Turn::card_state(&app.cards_handle(), "ses_test").await;
    assert!(
        !matches!(state, Some(CardState::Done | CardState::Waiting)),
        "the Wake's content must not declare the ending: {state:?}"
    );
    assert!(
        app.inflight.lock().await.contains("ses_test"),
        "the Turn keeps observing until the Wake's Execution boundary"
    );
    assert!(
        !noticed(&platform).await,
        "no notice while the Wake's Execution is unbounded"
    );

    // The Wake's Execution idles: the boundary answers the Wake and the true
    // end lands, content included.
    script_transcript(
        &backend,
        vec![
            SessionTranscript::new(vec![
                user("msg_cola_anchor", 1_000, "跑一下 CI"),
                assistant(2_000, "已经交给后台了。"),
                assistant(3_100, "CI 通过了，正在合并。"),
                assistant(4_100, "合并完成。"),
            ])
            .with_executions(vec![execution(2_500), execution(5_000)])
            .with_wakes(vec![shell_wake(2_900)]),
        ],
    )
    .await;

    let result = tokio::time::timeout(Duration::from_secs(5), turn)
        .await
        .expect("the boundary must end the turn")
        .unwrap();
    result.unwrap();

    let card = platform.updated_cards().await.last().cloned().unwrap();
    assert!(card_header(&card).contains("完成"), "{card}");
    assert!(
        card_text(&card).contains("合并完成。"),
        "the true end's content lands: {card}"
    );
    assert!(noticed(&platform).await, "the notice fires at the true end");
}

/// A retirement observed while the drain is still rendering ends the turn
/// normally: the settle decision sees the Wake's answered Execution and no live
/// task, so the card finalizes ✅ with its notice.
#[tokio::test]
async fn a_task_retiring_while_the_drain_renders_ends_the_turn_normally() {
    let _wd = test_work_dir();
    let running = SessionTranscript::new(vec![
        user("msg_cola_anchor", 1_000, "跑一下 CI"),
        assistant(2_000, "已经交给后台了。"),
    ])
    .with_executions(vec![execution(2_500)])
    .with_background_tasks(vec![background_shell(2_100)]);
    let (_dir, app, backend, platform) = scripted_app(vec![running], Some(SessionStatus::Busy)).await;
    app.turn_follow_grace_ms.store(30, Ordering::Relaxed);
    let mut context = ctx("ses_test", "跑一下 CI");
    context.is_group = true;
    context.requester_open_id = Some(TEST_HOST.to_string());

    // The still-busy session keeps the merged drain rendering on its own task.
    let _turn = spawn_turn(&app, context);
    wait_for_card_text(&platform, "已经交给后台了。").await;
    assert!(
        app.inflight.lock().await.contains("ses_test"),
        "the drain holds the guard while it renders"
    );

    // The task retires while the drain renders, and its Execution ends: the
    // true end, on the same card.
    script_transcript(
        &backend,
        vec![
            SessionTranscript::new(vec![
                user("msg_cola_anchor", 1_000, "跑一下 CI"),
                assistant(2_000, "已经交给后台了。"),
                assistant(3_100, "CI 通过了。"),
            ])
            .with_executions(vec![execution(2_500), execution(4_000)])
            .with_wakes(vec![shell_wake(2_900)]),
        ],
    )
    .await;
    backend
        .set_session_status("ses_test", Some(SessionStatus::Idle))
        .await;

    wait_for_card_header(&platform, "完成").await;
    wait_for_guard_release(&app).await;
    let final_card = platform.updated_cards().await.last().cloned().unwrap();
    assert!(
        card_text(&final_card).contains("CI 通过了。"),
        "the true end's content lands: {final_card}"
    );
    assert!(noticed(&platform).await, "the notice fires at the true end");
    assert_no_further_rendering(&backend, &platform).await;
}

/// A settled failure dominates waiting: a failed Turn with live Background
/// Tasks ends ❌ — the later Wake continues on a new card (ticket 3) — never
/// 「⏳ 等待后台任务」.
#[tokio::test]
async fn a_settled_failure_dominates_a_live_background_task() {
    let _wd = test_work_dir();
    let mut failed = assistant(2_000, "失败的一步");
    failed.error = Some("provider 503".into());
    let transcript = SessionTranscript::new(vec![user("msg_cola_anchor", 1_000, "跑一下 CI"), failed])
        .with_executions(vec![execution(2_500)])
        .with_background_tasks(vec![background_shell(2_100)]);
    let (_dir, app, _backend, platform) = scripted_app(vec![transcript], Some(SessionStatus::Idle)).await;

    Turn::run(&app.turn_handles(), ctx("ses_test", "跑一下 CI"))
        .await
        .unwrap();

    let card = platform.updated_cards().await.last().cloned().unwrap();
    assert!(
        card_header(&card).contains("出错"),
        "a settled failure dominates the live task: {card}"
    );
    assert!(
        !card_header(&card).contains("等待后台任务"),
        "a failed Turn never yields waiting: {card}"
    );
    assert!(
        card_text(&card).contains("provider 503"),
        "the failure reaches the card: {card}"
    );
}

/// A deliberate `/stop` dominates waiting too: a stopped Turn with live
/// Background Tasks finalizes 「⏹ 已停止」, never the waiting yield.
#[tokio::test]
async fn a_stop_dominates_a_live_background_task() {
    let _wd = test_work_dir();
    let transcript = SessionTranscript::new(vec![
        user("msg_cola_anchor", 1_000, "跑一下 CI"),
        assistant(2_000, "已经交给后台了。"),
    ])
    .with_executions(vec![execution(2_500)])
    .with_background_tasks(vec![background_shell(2_100)]);
    // Busy keeps the drain polling until the stop marker ends it.
    let (_dir, app, _backend, platform) = scripted_app(vec![transcript], Some(SessionStatus::Busy)).await;

    let turn = spawn_turn(&app, ctx("ses_test", "跑一下 CI"));
    wait_for_card_text(&platform, "已经交给后台了。").await;
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
        .expect("the stop must end the drain")
        .unwrap();
    result.unwrap();

    let card = platform.updated_cards().await.last().cloned().unwrap();
    assert!(
        card_header(&card).contains("已停止"),
        "a deliberate stop dominates: {card}"
    );
    assert!(
        !card_header(&card).contains("等待后台任务"),
        "a stopped Turn never yields waiting: {card}"
    );
}

/// The settle decision's ordinary ending is unchanged: a V2 read at an idle
/// Execution boundary with no live Background Task finalizes ✅ and notifies,
/// exactly as the V1-shaped read always did.
#[tokio::test]
async fn a_v2_idle_read_with_no_live_background_task_completes_as_before() {
    let _wd = test_work_dir();
    let transcript = SessionTranscript::new(vec![
        user("msg_cola_anchor", 1_000, "第一条消息"),
        assistant(2_000, "第一轮回答。"),
    ])
    .with_executions(vec![execution(2_500)])
    .with_wakes(vec![shell_wake(1_500)]);
    let (_dir, app, _backend, platform) = scripted_app(vec![transcript], Some(SessionStatus::Idle)).await;
    let mut context = ctx("ses_test", "第一条消息");
    context.is_group = true;
    context.requester_open_id = Some(TEST_HOST.to_string());

    Turn::run(&app.turn_handles(), context).await.unwrap();

    let card = platform.updated_cards().await.last().cloned().unwrap();
    assert!(card_header(&card).contains("完成"), "{card}");
    assert!(
        noticed(&platform).await,
        "the notice fires at the true end: {:?}",
        platform.calls.lock().await
    );
}

/// The merged drain takes the waiting yield when the run it watches idles with
/// live Background Tasks: the card yields 「⏳ 等待后台任务」, no Completion
/// Notice fires, and the drain stops.
#[tokio::test]
async fn a_drain_idling_with_a_live_background_task_yields_waiting() {
    let _wd = test_work_dir();
    let live = SessionTranscript::new(vec![
        user("msg_cola_anchor", 1_000, "跑一下 CI"),
        assistant(2_000, "已经交给后台了。"),
    ])
    .with_executions(vec![execution(2_500)])
    .with_background_tasks(vec![background_shell(2_100)]);
    let (_dir, app, backend, platform) = scripted_app(vec![live], Some(SessionStatus::Busy)).await;
    app.turn_follow_grace_ms.store(30, Ordering::Relaxed);
    let mut context = ctx("ses_test", "跑一下 CI");
    context.is_group = true;
    context.requester_open_id = Some(TEST_HOST.to_string());

    // The still-busy session keeps the merged drain rendering on its own task.
    let _turn = spawn_turn(&app, context);
    wait_for_card_text(&platform, "已经交给后台了。").await;

    // The Execution then idles with the task still live: the drain yields.
    backend
        .set_session_status("ses_test", Some(SessionStatus::Idle))
        .await;

    wait_for_card_header(&platform, "等待后台任务").await;
    wait_for_guard_release(&app).await;
    assert_eq!(
        Turn::card_state(&app.cards_handle(), "ses_test").await,
        Some(CardState::Waiting)
    );
    assert!(
        !noticed(&platform).await,
        "the yield sends no notice: {:?}",
        platform.calls.lock().await
    );
    assert_no_further_rendering(&backend, &platform).await;
}

// ---------------------------------------------------------------------------
// The shared runtime reconcile on the live reads (#589): the merged drain and
// the out-of-turn follow (the recovery re-attach, the Wake continuation)
// observe the runtime registries on the transcript read each already performs
// — one process-wide verdict per Session per interval, shared with Session
// Sync — so a task that died mid-turn ends the turn directly. A read that
// lists no live task spends nothing; a failed or timed-out one changes nothing
// at all.
// ---------------------------------------------------------------------------

/// Acceptance (#589): a read that lists no live Background Task spends no
/// runtime request on the live path either — the shared step's own guard, not
/// the throttle (the harness admits every attempt).
#[tokio::test]
async fn an_empty_ledger_spends_no_runtime_read() {
    let _wd = test_work_dir();
    let transcript = SessionTranscript::new(vec![
        user("msg_cola_anchor", 1_000, "第一条消息"),
        assistant(2_000, "第一轮回答。"),
    ])
    .with_executions(vec![execution(2_500)]);
    let (_dir, app, backend, platform) = scripted_app(vec![transcript], Some(SessionStatus::Idle)).await;

    Turn::run(&app.turn_handles(), ctx("ses_test", "第一条消息"))
        .await
        .unwrap();

    assert!(
        backend.task_runtime_calls.lock().await.is_empty(),
        "no live task means no runtime read"
    );
    let card = platform.updated_cards().await.last().cloned().unwrap();
    assert!(
        card_header(&card).contains("完成"),
        "the turn is otherwise unchanged: {card}"
    );
}

/// Acceptance (#589): a failed runtime read changes nothing on the live path —
/// the task keeps its row, no entry renders, and the settle still yields
/// waiting because the read could not end anything.
#[tokio::test]
async fn a_failed_runtime_read_leaves_the_turn_waiting() {
    let _wd = test_work_dir();
    let live = SessionTranscript::new(vec![
        user("msg_cola_anchor", 1_000, "跑一下 CI"),
        assistant(2_000, "已经交给后台了。"),
    ])
    .with_executions(vec![execution(2_500)])
    .with_background_tasks(vec![background_shell(2_100)]);
    let (_dir, app, backend, platform) = scripted_app(vec![live], Some(SessionStatus::Idle)).await;
    backend.fail_task_runtime_reads(usize::MAX);

    Turn::run(&app.turn_handles(), ctx("ses_test", "跑一下 CI"))
        .await
        .unwrap();

    assert_eq!(
        Turn::card_state(&app.cards_handle(), "ses_test").await,
        Some(CardState::Waiting),
        "a failed read cannot end the wait"
    );
    let card = platform.updated_cards().await.last().cloned().unwrap();
    let text = card_text(&card);
    assert!(text.contains("⏳ 后台任务（1）"), "the row stays: {card}");
    assert!(
        !text.contains("🔔 shell"),
        "no entry from a read that failed: {card}"
    );
    assert!(
        !backend.task_runtime_calls.lock().await.is_empty(),
        "the attempt was made (and failed)"
    );
}

/// Acceptance (#589): a runtime read that times out changes nothing either —
/// the caller's bound abandons the wedged read and the drain moves on with the
/// transcript exactly as read.
#[tokio::test]
async fn a_timed_out_runtime_read_leaves_the_turn_waiting() {
    let _wd = test_work_dir();
    let live = SessionTranscript::new(vec![
        user("msg_cola_anchor", 1_000, "跑一下 CI"),
        assistant(2_000, "已经交给后台了。"),
    ])
    .with_executions(vec![execution(2_500)])
    .with_background_tasks(vec![background_shell(2_100)]);
    let (_dir, app, backend, platform) = scripted_app(vec![live], Some(SessionStatus::Idle)).await;
    // A tiny drain budget so the wedged runtime read is abandoned fast.
    backend.hang_task_runtime_reads(usize::MAX);

    Turn::run(&app.turn_handles(), ctx("ses_test", "跑一下 CI"))
        .await
        .unwrap();

    assert_eq!(
        Turn::card_state(&app.cards_handle(), "ses_test").await,
        Some(CardState::Waiting),
        "a timed-out read cannot end the wait"
    );
    let card = platform.updated_cards().await.last().cloned().unwrap();
    let text = card_text(&card);
    assert!(text.contains("⏳ 后台任务（1）"), "the row stays: {card}");
    assert!(
        !text.contains("🔔 shell"),
        "no entry from a read that timed out: {card}"
    );
}

/// Acceptance (#589): the merged drain reconciles on its own read — a task the
/// runtime reports ended while the drain renders retires there, the entry lands
/// on the same card, and the settle is the true end instead of a stranded wait.
#[tokio::test]
async fn a_runtime_retirement_during_the_drain_ends_the_turn() {
    let _wd = test_work_dir();
    let running = SessionTranscript::new(vec![
        user("msg_cola_anchor", 1_000, "跑一下 CI"),
        assistant(2_000, "已经交给后台了。"),
    ])
    .with_executions(vec![execution(2_500)])
    .with_background_tasks(vec![background_shell(2_100)]);
    let (_dir, app, backend, platform) = scripted_app(vec![running], Some(SessionStatus::Busy)).await;
    app.turn_follow_grace_ms.store(30, Ordering::Relaxed);

    // The still-busy session keeps the merged drain rendering on its own task.
    let _turn = spawn_turn(&app, ctx("ses_test", "跑一下 CI"));
    // The live row first (the still-busy session keeps the drain rendering):
    // the retirement below must remove it IN PLACE.
    wait_for_card_text(&platform, "⏳ 后台任务（1）").await;
    assert!(
        app.inflight.lock().await.contains("ses_test"),
        "the drain holds the guard while it renders"
    );

    // The runtime confirms the shell ended while the drain is still
    // rendering: the drain's own read retires it and renders the entry.
    backend.task_runtime.lock().unwrap().shells = vec![(
        "sh_bg".into(),
        ShellRuntime::Ended {
            end: ShellEnd::Exited,
            completed_at: Some(2_900),
        },
    )];
    wait_for_card_update(&platform, "the drain's entry", CardUpdates::Latest, |card| {
        card_text(card).contains("🔔 shell 结束")
    })
    .await;
    let retired_card = platform.updated_cards().await.last().cloned().unwrap();
    assert!(
        !card_text(&retired_card).contains("后台任务（"),
        "the row left the live card the moment the runtime retired it: {retired_card}"
    );

    // The Execution then idles: the settle is the true end.
    backend
        .set_session_status("ses_test", Some(SessionStatus::Idle))
        .await;
    wait_for_card_header(&platform, "完成").await;
    wait_for_guard_release(&app).await;
    let final_card = platform.updated_cards().await.last().cloned().unwrap();
    let text = card_text(&final_card);
    assert!(
        text.contains("🔔 shell 结束") && !text.contains("后台任务（"),
        "the drain's read retired the task and kept its entry: {final_card}"
    );
    // Review (spec #588 / #589, PR #595): the retirement reaches the overlay
    // only once the drain's render carried its entry — the same
    // record-after-flush invariant, pinned on this path.
    assert_eq!(
        backend.overlay.retired_call_ids("ses_test"),
        vec!["call_bg".to_string()],
        "the drain's accepted render records the retirement"
    );
}

/// Review (spec #588 / #589, PR #595): the drain's reconcile commits its
/// overlay record only when the render that carried its entries completed —
/// the shared invariant, enforced by construction for every caller. If the
/// accumulator vanishes between the tick's read and its render (a replacement
/// collected the card), the retirement has no card to render on; recording it
/// anyway would hide the task from every later read without an entry ever
/// landing. The read was still spent — only the record is gated, and the drain
/// ends silently.
///
/// Deterministic, no wall-clock margin decides: the drain's next transcript
/// read is parked on the mock's gate, both facts it will read (the missing
/// shell and the vanished card) are moved while it is parked, then the read is
/// released; the drain's exit releases the guard, which is awaited as a
/// condition.
#[tokio::test]
async fn a_drain_whose_render_never_lands_records_nothing() {
    let _wd = test_work_dir();
    let running = SessionTranscript::new(vec![
        user("msg_cola_anchor", 1_000, "跑一下 CI"),
        assistant(2_000, "已经交给后台了。"),
    ])
    .with_executions(vec![execution(2_500)])
    .with_background_tasks(vec![background_shell(2_100)]);
    let (_dir, app, backend, platform) = scripted_app(vec![running], Some(SessionStatus::Busy)).await;
    // The parked tick must outlive the test's own work: no grace and no
    // per-read bound may end the loop while the read is held.
    app.turn_follow_grace_ms.store(60_000, Ordering::Relaxed);
    app.turn_follow_read_timeout_ms.store(60_000, Ordering::Relaxed);

    // The still-busy session keeps the merged drain rendering on its own task.
    let _turn = spawn_turn(&app, ctx("ses_test", "跑一下 CI"));
    wait_for_card_text(&platform, "⏳ 后台任务（1）").await;
    assert!(
        app.inflight.lock().await.contains("ses_test"),
        "the drain holds the guard while it renders"
    );

    // Park the drain's next read.
    let gate = backend.hold_transcripts();
    let entered = backend.transcript_gate_entered.load(Ordering::SeqCst);
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    while backend.transcript_gate_entered.load(Ordering::SeqCst) == entered {
        assert!(
            tokio::time::Instant::now() < deadline,
            "the drain never reached the parked read"
        );
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    // While it is parked, the runtime misses the shell and the accumulator is
    // collected from under it.
    backend.task_runtime.lock().unwrap().shells = vec![("sh_bg".into(), ShellRuntime::Missing)];
    app.cards_handle().cards.lock().await.remove("ses_test");
    // Disarm so no later read parks, then release the tick: its reconcile
    // retires the shell, and its render finds no accumulator.
    *backend.transcript_gate.lock().unwrap() = None;
    gate.add_permits(1);

    // The drain ended without a card to carry the retirement: nothing was
    // recorded, and the task stays live on every later read.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    while app.inflight.lock().await.contains("ses_test") {
        assert!(
            tokio::time::Instant::now() < deadline,
            "the drain never ended on its vanished accumulator"
        );
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    assert!(
        backend.overlay.retired_call_ids("ses_test").is_empty(),
        "a render that never landed records nothing: {:?}",
        backend.overlay.retired_call_ids("ses_test")
    );
    let later = crate::backend::Backend::transcript(backend.as_ref(), "ses_test")
        .await
        .unwrap();
    assert_eq!(
        later
            .background_tasks
            .iter()
            .map(|task| task.tool.call_id.as_str())
            .collect::<Vec<_>>(),
        vec!["call_bg"],
        "the task stays live until a render can carry its entry"
    );
    // The parked tick DID observe — the read was spent; only the record is
    // gated on the render it never got.
    assert!(
        !backend.task_runtime_calls.lock().await.is_empty(),
        "the parked tick's reconcile spent its read"
    );
    // No card ever received an entry.
    let mut cards = platform.updated_cards().await;
    cards.extend(platform.sent_cards().await);
    cards.extend(platform.replied_cards().await);
    assert!(
        cards.iter().all(|card| !card_text(card).contains("🔔")),
        "no card carries a half-rendered entry: {cards:?}"
    );
}

/// Review (spec #588): the unconfirmed marker survives the live-turn reads
/// too. A drain tick within the shared throttle renders the transcript it just
/// read — a read the runtime was not asked about — so without the carried
/// marker the live card would drop the row's `⚠️ 状态待确认` and its title
/// count mid-turn, with no evidence the child is running. The Codex sequence's
/// drain-path twin.
#[tokio::test]
async fn a_throttled_drain_read_keeps_the_unconfirmed_marker() {
    let _wd = test_work_dir();
    let live = SessionTranscript::new(vec![
        user("msg_cola_anchor", 1_000, "跑一下 CI"),
        assistant(2_000, "已经交给后台了。"),
    ])
    .with_executions(vec![execution(2_500)])
    .with_background_tasks(vec![BackgroundTask {
        tool: ToolIdentity {
            name: "subagent".into(),
            call_id: "call_sub".into(),
        },
        shell_id: None,
        child_id: Some("ses_call_sub".into()),
        started_at: Some(2_100),
    }]);
    let (_dir, app, backend, platform) = scripted_app(vec![live], Some(SessionStatus::Busy)).await;
    // The child's own read finds no activity and no concluding evidence: the
    // row stays unconfirmed and nothing ticks (the title count is pinned).
    backend
        .given_transcript_after_build("ses_call_sub", vec![SessionTranscript::new(vec![])])
        .await;
    backend.task_runtime.lock().unwrap().children = vec![("ses_call_sub".into(), ChildRuntime::Inactive)];

    // The still-busy session keeps the drain rendering; its admitted read marks
    // the child on the live card.
    let turn = spawn_turn(&app, ctx("ses_test", "跑一下 CI"));
    wait_for_card_update(&platform, "the unconfirmed marker", CardUpdates::Latest, |card| {
        card_text(card).contains("⚠️ 状态待确认")
    })
    .await;

    // Pin the shared throttle wide: the next drain reads get no verdict, and
    // nothing positive said the child is running.
    app.runtime_reconcile.interval_ms.store(60_000, Ordering::Relaxed);
    backend.transcript_calls.lock().await.clear();
    wait_for_transcript_reads(&backend, "ses_test", 1).await;
    // Let the throttled tick's render land: it must owe none.
    tokio::time::sleep(Duration::from_millis(50)).await;

    let latest = platform.updated_cards().await.last().cloned().unwrap();
    let text = card_text(&latest);
    assert!(
        text.contains("⏳ 后台任务（1 · 1 待确认）"),
        "the live card keeps counting the unconfirmed row: {latest}"
    );
    assert!(
        text.contains("⚠️ 状态待确认"),
        "the marker survives a drain read the runtime was not asked about: {latest}"
    );

    turn.abort();
    let _ = turn.await;
}

// ---------------------------------------------------------------------------
// The Completion Notice gate (spec #602, ticket #607): the notice follows the
// terminal write's carrier — delivered now, or owed by a Pending Card Update
// that must drain first. One rule for every carrier (the direct PATCH, the
// size split, the queued retry); the quiet in-place settle is gated identically
// in `refreshed_yielded_card`.
// ---------------------------------------------------------------------------

/// Await the Completion Notice or panic after 5 s: a gated notice is fired from
/// the delivery layer's drain on its own spawned task, so it trails the drain
/// call that delivered the write.
pub(crate) async fn wait_for_notice(platform: &RecordingPlatform) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while !noticed(platform).await {
        assert!(
            tokio::time::Instant::now() < deadline,
            "no Completion Notice arrived: {:?}",
            platform.calls.lock().await
        );
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

/// Acceptance (ticket #607): a terminal PATCH that lands directly announces at
/// once — the gate must never defer a delivered write.
#[tokio::test]
async fn a_direct_terminal_patch_notifies_at_once() {
    let _wd = test_work_dir();
    let transcript = SessionTranscript::new(vec![
        user("msg_cola_anchor", 1_000, "你好"),
        assistant(2_000, "最终答复。"),
    ]);
    let (_dir, app, _backend, platform) = scripted_app(vec![transcript], Some(SessionStatus::Idle)).await;
    let mut context = ctx("ses_test", "你好");
    context.is_group = true;
    context.requester_open_id = Some(TEST_HOST.to_string());

    Turn::run(&app.turn_handles(), context).await.unwrap();

    assert!(
        noticed(&platform).await,
        "a delivered terminal PATCH notifies without waiting for a drain: {:?}",
        platform.calls.lock().await
    );
}

/// Acceptance (ticket #607): a terminal PATCH that failed recoverably is queued
/// as a Pending Card Update; the notice must NOT fire while it is owed, and
/// must fire once the drain delivers it. Group turn, so the notice is on and a
/// missing one is the gate's doing, not the rules'.
#[tokio::test]
async fn a_queued_ending_patch_notifies_only_after_it_drains() {
    let _wd = test_work_dir();
    let transcript = SessionTranscript::new(vec![
        user("msg_cola_anchor", 1_000, "你好"),
        assistant(2_000, "最终答复。"),
    ]);
    let (_dir, app, _backend, platform) = scripted_app(vec![transcript], Some(SessionStatus::Idle)).await;
    // Every content write fails at the transport: the ending PATCH is owed.
    platform.fail_update_transport_count.store(100, Ordering::SeqCst);
    let mut context = ctx("ses_test", "你好");
    context.is_group = true;
    context.requester_open_id = Some(TEST_HOST.to_string());

    Turn::run(&app.turn_handles(), context).await.unwrap();

    assert!(
        !noticed(&platform).await,
        "an owed terminal write must not announce before it drains: {:?}",
        platform.calls.lock().await
    );

    // Feishu returns: the drain delivers the owed update, and the notice fires.
    platform.fail_update_transport_count.store(0, Ordering::SeqCst);
    app.core.feishu.drain_pending_card_updates(true).await;
    wait_for_notice(&platform).await;

    // The delivered card is the terminal content the notice announces.
    let delivered = platform.updated_cards().await.last().cloned().unwrap();
    assert!(
        card_text(&delivered).contains("最终答复。"),
        "the notice trails the terminal content: {delivered}"
    );
}

/// Acceptance (ticket #607), at the mandated lifecycle seam: a scripted run
/// whose terminal answer outgrows one Card size-splits on its own flush, and
/// the Completion Notice must follow the new Card's send — never precede it.
/// Unlike the hand-seeded companion in `card_handles.rs`, this drives the real
/// `Turn::run`, so the gate is exercised end to end.
#[tokio::test]
async fn a_size_split_terminal_flush_notifies_after_the_new_card() {
    let _wd = test_work_dir();
    // A terminal answer far past one Card's text budget, ending on a marker
    // only the continuation can carry (the first Card cannot hold the tail).
    let long = format!("{}【尾部标记】", "很长的回答。".repeat(1200));
    let transcript = SessionTranscript::new(vec![
        user("msg_cola_anchor", 1_000, "你好"),
        assistant(2_000, &long),
    ]);
    let (_dir, app, _backend, platform) = scripted_app(vec![transcript], Some(SessionStatus::Idle)).await;
    // Keep the submit-window render poll from flushing the long answer first:
    // its first read is gated a full poll cycle away, so `attempt` sets the stop
    // flag before the poll ever reads. The split then happens in finalization's
    // OWN ending flush — the flush the notice gate is bound to — instead of
    // earlier, which is what the bug is about (#607). The drain shares this
    // cadence, so the turn still finishes in well under the notice's deadline.
    app.turn_render_poll_ms.store(200, Ordering::Relaxed);
    // Distinct create ids: the loading card and the split's continuation must be
    // DIFFERENT card ids. The mock's constant `msg_reply` would hide the bug —
    // the size split would move the terminal slice to a continuation that
    // reports the same id as the prefix it replaced, so the notice's identity
    // guard would not notice the swap.
    platform.given_reply_id("om_loading");
    platform.given_reply_id("om_continuation");
    let mut context = ctx("ses_test", "你好");
    context.is_group = true;
    context.requester_open_id = Some(TEST_HOST.to_string());

    Turn::run(&app.turn_handles(), context).await.unwrap();
    wait_for_notice(&platform).await;

    let calls = platform.calls.lock().await.clone();
    // The continuation is the card POST carrying the terminal tail.
    let continuation_at = calls
        .iter()
        .position(|c| match c {
            PlatformCall::ReplyCard { card, .. } | PlatformCall::SendCard { card, .. } => {
                card_text(card).contains("【尾部标记】")
            }
            _ => false,
        })
        .expect("a continuation card carrying the terminal tail was sent");
    let notice_at = calls
        .iter()
        .position(|c| matches!(c, PlatformCall::CompletionNotice { .. }))
        .expect("the notice fired");
    assert!(
        continuation_at < notice_at,
        "the notice must follow the new card's send: {calls:?}"
    );
}

/// Finding D (spec #602 review): a size-split whose continuation create FAILS
/// leaves the tracked card id on the OLD card, whose finalize PATCH delivered —
/// so the delivery layer would answer "Delivered" for the wrong card and the
/// notice would fire although the terminal tail never reached Feishu. The
/// terminal write's own acceptance must suppress it.
#[tokio::test]
async fn a_failed_size_split_continuation_create_suppresses_the_notice() {
    let _wd = test_work_dir();
    let long = format!("{}【尾部标记】", "很长的回答。".repeat(1200));
    let transcript = SessionTranscript::new(vec![
        user("msg_cola_anchor", 1_000, "你好"),
        assistant(2_000, &long),
    ]);
    let (_dir, app, _backend, platform) = scripted_app(vec![transcript], Some(SessionStatus::Idle)).await;
    // The loading card (the first create) lands; every continuation create
    // fails with an ambiguous transport error, so the terminal tail never
    // reaches Feishu. One outcome per card the chain could attempt (MAX_CARD_CHAIN).
    {
        let mut outcomes = platform.reply_card_outcomes.lock().unwrap();
        outcomes.push_back(ReplyOutcome::Lands);
        for _ in 0..(crate::bridge::turn::MAX_CARD_CHAIN + 1) {
            outcomes.push_back(ReplyOutcome::Ambiguous);
        }
    }
    let mut context = ctx("ses_test", "你好");
    context.is_group = true;
    context.requester_open_id = Some(TEST_HOST.to_string());

    Turn::run(&app.turn_handles(), context).await.unwrap();
    // Give a suppressed notice no window to land late.
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(
        !noticed(&platform).await,
        "a continuation create that never carried the tail must suppress the notice: {:?}",
        platform.calls.lock().await
    );
}

/// Finding 1 (spec #602 pre-push review): a deferred Completion Notice belongs
/// to the request that ENDED — the card that carried its terminal write — not
/// to whatever card the session has when the owed write finally drains. A NEW
/// Turn replaces the session's card (a fresh card id, a fresh requester) while
/// the terminal write is still owed; when the delivery layer then drains that
/// write, the notice must be SUPPRESSED, never redirected to the replacement
/// request's requester.
#[tokio::test]
async fn a_deferred_notice_is_suppressed_when_a_new_turn_replaced_the_card() {
    let _wd = test_work_dir();
    let transcript = SessionTranscript::new(vec![
        user("msg_cola_anchor", 1_000, "你好"),
        assistant(2_000, "最终答复。"),
    ]);
    let (_dir, app, _backend, platform) = scripted_app(vec![transcript], Some(SessionStatus::Idle)).await;
    // Every content write fails at the transport: the terminal PATCH is owed
    // and the notice is deferred until that exact write drains.
    platform.fail_update_transport_count.store(100, Ordering::SeqCst);
    let mut context = ctx("ses_test", "你好");
    context.is_group = true;
    context.requester_open_id = Some(TEST_HOST.to_string());

    Turn::run(&app.turn_handles(), context).await.unwrap();
    assert!(
        !noticed(&platform).await,
        "an owed terminal write must defer the notice: {:?}",
        platform.calls.lock().await
    );

    // A NEW Turn replaces the session's card while the first write is still
    // owed: the session now carries a different card and a different requester.
    let cards = app.cards_handle();
    Turn::seed_card(&cards, "ses_test", Some("om_new")).await;
    Turn::set_reply_target(&cards, "ses_test", "msg_new").await;
    Turn::set_turn_identity(&cards, "ses_test", "ou_new_requester", true, 2).await;

    // The owed write drains — to the ORIGINAL card. The deferred notice must
    // not follow it to the replacement card's request.
    platform.fail_update_transport_count.store(0, Ordering::SeqCst);
    app.core.feishu.drain_pending_card_updates(true).await;
    tokio::time::sleep(Duration::from_millis(50)).await;

    let notices = platform.completion_notices().await;
    assert!(
        notices
            .iter()
            .all(|(reply_to, open_id, _, _)| reply_to != "msg_new" && open_id != "ou_new_requester"),
        "the deferred notice must never target the replacement request: {notices:?}"
    );
    assert!(
        notices.is_empty(),
        "the original request's card is gone, so the notice is suppressed: {notices:?}"
    );
}
