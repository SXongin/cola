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

use crate::bridge::test_support::*;
use crate::bridge::turn::{PromptContext, Turn};
use crate::config::ThreadKey;
use crate::feishu::card::CardState;
use crate::opencode::types::{MessageInfo, MessageTime, SessionMessage, SessionStatus};
use serde_json::json;

/// A Backend message fixture with the given role/id/server time.
fn msg(role: &str, id: &str, created: i64, parts: serde_json::Value) -> SessionMessage {
    SessionMessage {
        info: MessageInfo {
            id: id.into(),
            role: Some(role.into()),
            parent_id: None,
            time: Some(MessageTime { created }),
            model_id: None,
            provider_id: None,
            tokens: None,
        },
        parts,
    }
}

/// A user message cola would have persisted (its `msg_cola_` id identifies it).
fn user(id: &str, created: i64, text: &str) -> SessionMessage {
    msg("user", id, created, json!([{ "type": "text", "text": text }]))
}

/// A finished assistant turn whose only visible content is `text`.
fn assistant(created: i64, text: &str) -> SessionMessage {
    msg(
        "assistant",
        &format!("msg_a_{created}"),
        created,
        json!([
            { "type": "text", "text": text },
            { "type": "step-finish", "reason": "stop" },
        ]),
    )
}

/// An assistant message whose only content is one `bash` tool call in the
/// given state — the #284 fixture: a panel still `running` when the drain
/// bound lands, whose later `completed` update must still reach the card.
fn tool_assistant(created: i64, status: &str, output: &str) -> SessionMessage {
    msg(
        "assistant",
        &format!("msg_tool_{created}"),
        created,
        json!([
            { "type": "tool", "tool": "bash", "callID": "call_1",
              "state": { "status": status, "input": { "command": "sleep 600" }, "output": output } },
            { "type": "step-finish", "reason": "tool-calls" },
        ]),
    )
}

/// The turn under test: the anchor id is fixed so the scripted Backend timeline can name
/// the turn's own user message (`capture_turn_anchor` matches on it).
fn ctx(session_id: &str, text: &str) -> PromptContext {
    PromptContext {
        session_id: session_id.into(),
        thread_key: ThreadKey::new("chat_1".into(), "chat_1".into()),
        text: text.into(),
        message_id: "msg_1".into(),
        subtitle: "p2p".into(),
        existing_card_id: None,
        requester_open_id: None,
        is_group: false,
        cola_message_id: Some("msg_cola_anchor".into()),
        images: Vec::new(),
    }
}

/// An app whose Backend serves the scripted message timeline for `ses_test` (one
/// per `messages` call, the last repeating) and the given session status.
async fn scripted_app(
    scripts: Vec<Vec<SessionMessage>>,
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
    backend.given_timeline("ses_test", scripts);
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
    (dir, app, backend, platform)
}

/// Start the turn on its own task (the test drives the Backend while it drains).
fn spawn_turn(app: &Arc<App>, context: PromptContext) -> tokio::task::JoinHandle<crate::error::Result<()>> {
    let app = Arc::clone(app);
    tokio::spawn(async move { Turn::run(&app.turn_handles(), context).await })
}

/// The #284 scenario app: a Supplement whose `bash` tool is still `running`
/// when the drain bound lands, on a Busy session, with a tiny drain bound and
/// the given follow ceiling.
async fn busy_supplement_app(
    follow_timeout_ms: u64,
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
        tool_assistant(4_000, "running", ""),
    ];
    let (dir, app, backend, platform) = scripted_app(vec![timeline], Some(SessionStatus::Busy)).await;
    app.turn_drain_timeout_ms.store(30, Ordering::Relaxed);
    app.turn_follow_timeout_ms
        .store(follow_timeout_ms, Ordering::Relaxed);
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

/// Settle the scenario's `bash` panel in the scripted timeline the follow
/// reads (`status`/`output` — the server's own part update).
async fn settle_tool(backend: &Arc<MockBackend>, status: &str, output: &str) {
    let mut scripts = backend.message_scripts.lock().await;
    let msgs = &mut scripts.get_mut("ses_test").unwrap()[0];
    let parts = msgs.last_mut().unwrap().parts.as_array_mut().unwrap();
    for part in parts.iter_mut() {
        if part.get("type").and_then(|t| t.as_str()) == Some("tool") {
            part["state"]["status"] = json!(status);
            part["state"]["output"] = json!(output);
        }
    }
}

/// Await a card update carrying `needle`, or panic after 5 s.
async fn wait_for_card_text(platform: &RecordingPlatform, needle: &str) {
    let wait = async {
        loop {
            let seen = platform
                .updated_cards()
                .await
                .iter()
                .any(|c| card_text(c).contains(needle));
            if seen {
                return;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    };
    tokio::time::timeout(Duration::from_secs(5), wait)
        .await
        .unwrap_or_else(|_| panic!("card text {needle:?} never rendered"));
}

/// Await a card update whose header carries `needle`, or panic after 5 s — the
/// follow finalizes out of turn, so tests wait on the card, not on a handle.
async fn wait_for_card_header(platform: &RecordingPlatform, needle: &str) {
    let wait = async {
        loop {
            let seen = platform
                .updated_cards()
                .await
                .iter()
                .any(|c| card_header(c).contains(needle));
            if seen {
                return;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    };
    tokio::time::timeout(Duration::from_secs(5), wait)
        .await
        .unwrap_or_else(|_| panic!("card header {needle:?} never rendered"));
}

/// The drain must stop touching the Backend and the card when the turn ends:
/// no further message reads and no further card PATCHes.
async fn assert_no_further_rendering(backend: &Arc<MockBackend>, platform: &RecordingPlatform) {
    let reads = backend.messages_calls.lock().await.len();
    let patches = platform.updated_cards().await.len();
    tokio::time::sleep(Duration::from_millis(30)).await;
    assert_eq!(
        backend.messages_calls.lock().await.len(),
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
    let (_dir, app, backend, platform) = scripted_app(vec![timeline], Some(SessionStatus::Idle)).await;

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
    backend.message_scripts.lock().await.get_mut("ses_test").unwrap()[0]
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
    let (_dir, app, backend, platform) = scripted_app(vec![quiet, raced], Some(SessionStatus::Idle)).await;

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
    backend.message_scripts.lock().await.get_mut("ses_test").unwrap()[0]
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
    let (_dir, app, backend, platform) = scripted_app(vec![timeline], Some(SessionStatus::Idle)).await;

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
        backend
            .prompt_async_calls
            .lock()
            .await
            .iter()
            .any(|c| c == "ses_test:补充二"),
        "the message must queue as a supplement: {:?}",
        backend.prompt_async_calls.lock().await
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
    backend.message_scripts.lock().await.get_mut("ses_test").unwrap()[0]
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
/// is never reached.
#[tokio::test]
async fn stop_ends_the_drain_promptly() {
    let _wd = test_work_dir();
    let timeline = vec![
        user("msg_cola_anchor", 1_000, "第一条消息"),
        assistant(2_000, "第一轮回答。"),
        user("msg_cola_supp", 3_000, "补充一下"),
    ];
    let (_dir, app, backend, platform) = scripted_app(vec![timeline], Some(SessionStatus::Idle)).await;

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
    assert!(card_header(&final_card).contains("完成"), "final card Done");
    assert!(!app.inflight.lock().await.contains("ses_test"));
    assert_no_further_rendering(&backend, &platform).await;
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
    let (_dir, app, _backend, platform) = scripted_app(vec![timeline], Some(SessionStatus::Idle)).await;

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
/// CARD does not: it is handed to the out-of-turn follow, which keeps it live
/// until its own ceiling and then finalizes Error — never Done under a session
/// that is still running (#284).
#[tokio::test]
async fn the_drain_bound_exits_cleanly_and_finishes_the_turn() {
    let _wd = test_work_dir();
    let timeline = vec![
        user("msg_cola_anchor", 1_000, "第一条消息"),
        assistant(2_000, "第一轮回答。"),
    ];
    let (_dir, app, backend, platform) = scripted_app(vec![timeline], Some(SessionStatus::Busy)).await;
    // Tiny bounds so the long-poll branch and the follow's ceiling run in
    // milliseconds.
    app.turn_drain_timeout_ms.store(30, Ordering::Relaxed);
    app.turn_follow_timeout_ms.store(30, Ordering::Relaxed);
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
        backend.messages_calls.lock().await.len() > 1,
        "the drain must keep polling while the session is busy"
    );

    // The follow kept the card live past the bound and ended it Error at its
    // ceiling: a running session can never finalize Done.
    wait_for_card_header(&platform, "出错").await;
    let final_card = platform.updated_cards().await.last().cloned().unwrap();
    assert!(card_header(&final_card).contains("出错"), "final card Error");
    assert!(card_text(&final_card).contains("第一轮回答。"));
    assert!(
        platform
            .calls
            .lock()
            .await
            .iter()
            .any(|c| matches!(c, PlatformCall::CompletionNotice { text, .. } if text.contains("处理出错"))),
        "the completion notice reports the error: {:?}",
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
    settle_tool(&backend, "completed", "done").await;
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
    settle_tool(&backend, "error", "Tool execution aborted").await;

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

    wait_for_card_header(&platform, "完成").await;
    assert!(
        started.elapsed() < Duration::from_secs(3),
        "the follow must end on the stop, not the 60 s ceiling"
    );
    assert!(
        platform.texts().await.iter().any(|t| t.contains("Interrupted")),
        "the command reply still lands: {:?}",
        platform.calls.lock().await
    );
    assert_no_further_rendering(&backend, &platform).await;
}

/// A follow whose Backend reads hang ends in Error at its own ceiling: the
/// card never sits on an eternal "streaming" state (#284).
#[tokio::test]
async fn a_hung_backend_ends_the_follow_in_error() {
    let _wd = test_work_dir();
    let (_dir, app, backend, platform) = busy_supplement_app(50).await;
    run_to_handoff(&app, &platform).await;

    // Every read the follow makes now hangs (a wedged per-session read); the
    // bounded call must expire at the ceiling and finalize Error.
    backend.hang_message_reads(100);

    let started = std::time::Instant::now();
    wait_for_card_header(&platform, "出错").await;
    assert!(
        started.elapsed() < Duration::from_secs(3),
        "the hung read must end at the follow's ceiling, not DRAIN_REQUEST_TIMEOUT_MS"
    );
    let final_card = platform.updated_cards().await.last().cloned().unwrap();
    assert!(
        !card_header(&final_card).contains("完成"),
        "never Done under a hung run"
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
    assert_eq!(followed_anchor, Some(1_000), "the follow watches the turn anchor");

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
        backend.prompt_async_calls.lock().await.is_empty(),
        "no supplement send after the hand-off: {:?}",
        backend.prompt_async_calls.lock().await
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
    let (_dir, app, backend, platform) = scripted_app(vec![timeline], Some(SessionStatus::Idle)).await;

    Turn::run(&app.turn_handles(), ctx("ses_test", "第一条消息"))
        .await
        .unwrap();
    assert!(
        !app.inflight.lock().await.contains("ses_test"),
        "the guard is released"
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
        backend.prompt_async_calls.lock().await.is_empty(),
        "no supplement send after the release: {:?}",
        backend.prompt_async_calls.lock().await
    );
    assert!(
        !platform.texts().await.iter().any(|t| t.contains("还在处理中")),
        "the new turn is not throttled: {:?}",
        platform.calls.lock().await
    );
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
    let (_dir, app, backend, platform) = scripted_app(vec![timeline], Some(SessionStatus::Busy)).await;
    app.turn_render_poll_ms.store(20, Ordering::Relaxed);
    app.turn_drain_timeout_ms.store(30, Ordering::Relaxed);
    // The first Backend read hangs forever (a half-open connection left by a
    // server restart); later reads serve normally.
    backend.hang_message_reads(1);

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
    backend.message_scripts.lock().await.insert(
        "ses_test".into(),
        vec![vec![
            user(&anchor_id, 1_000, "第一条消息"),
            assistant(2_000, "第一轮回答。"),
            user("msg_cola_supp", 3_000, "补充一下"),
        ]],
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
    backend.message_scripts.lock().await.get_mut("ses_test").unwrap()[0]
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
    let (_dir, app, backend, platform) = scripted_app(vec![timeline], Some(SessionStatus::Busy)).await;
    app.turn_render_poll_ms.store(50, Ordering::Relaxed);
    app.turn_drain_timeout_ms.store(30, Ordering::Relaxed);
    // Both the drain's read and the re-check's read hang; the final reconcile
    // read serves normally.
    backend.hang_message_reads(2);

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
    let (_dir, app, backend, platform) = scripted_app(vec![settled, raced], Some(SessionStatus::Idle)).await;
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
    backend.message_scripts.lock().await.get_mut("ses_test").unwrap()[0]
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

/// Idle with nothing outstanding exits without waiting out the (60 s) bound:
/// the drain's first check is enough, and finalization follows.
#[tokio::test]
async fn an_idle_session_exits_the_drain_without_waiting() {
    let _wd = test_work_dir();
    let timeline = vec![
        user("msg_cola_anchor", 1_000, "第一条消息"),
        assistant(2_000, "第一轮回答。"),
    ];
    let (_dir, app, _backend, platform) = scripted_app(vec![timeline], Some(SessionStatus::Idle)).await;

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
