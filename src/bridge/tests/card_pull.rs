//! `/card`'s two states (ADR-0043, 2026-09-22 amendment; ADR-0100): a
//! render-owned card is pulled down — the explicit user request that brings the
//! live card back to the newest position after command replies bury it, reusing
//! the Supplement split machinery (the previous card finalized with the standard
//! split header, a continuation replied to the command message tracking as the
//! new live card; no Backend send, no text acknowledgement) — while a Session
//! no renderer owns is read once and answered with one read-only Session Card
//! that joins no Chain Record.

use std::sync::Arc;

use crate::backend::{FinishReason, MessageId, MessageRole, Part, SessionTranscript, StepFinish};
use crate::bridge::snapshot::SnapshotData;
use crate::bridge::test_support::*;
use crate::feishu::card::CardState;
use crate::feishu::card::session::{BUSY_CHIP, FOLLOW_HINT, IDLE_CHIP, LEAD_COMPACTION_PREFIX};

/// A backend whose `ses_test` transcript carries one question and one answer,
/// so an idle read has a 最近对话 tail to render.
fn idle_read_backend() -> MockBackend {
    let mut mock = MockBackend::new(realistic_parts());
    mock.given_transcript(
        "ses_test",
        vec![SessionTranscript::new(vec![
            typed_message(
                "msg_u1",
                MessageRole::User,
                Some(1_000),
                vec![text_part("继续改登录")],
            ),
            typed_message(
                "msg_a1",
                MessageRole::Assistant,
                Some(2_000),
                vec![text_part("好的，已改完")],
            ),
        ])],
    );
    mock
}

/// The one card the platform posted, asserting there is exactly one.
async fn one_posted_card(platform: &RecordingPlatform) -> serde_json::Value {
    let cards = platform.replied_cards().await;
    assert_eq!(
        cards.len(),
        1,
        "exactly one card must be posted: {:?}",
        platform.calls.lock().await
    );
    cards.into_iter().next().unwrap()
}

/// A Turn that already finished leaves its card session in `cards` with a Done
/// state until the next Turn replaces it. `/card` must not treat that as a live
/// card to pull — it reads the Session once and posts a read-only Session Card
/// (ADR-0100), leaves no text notice, and does not touch the finished card.
#[tokio::test]
async fn card_command_after_the_turn_finished_posts_an_idle_read() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let (app, platform) = build_app(cfg, idle_read_backend()).await;
    seed_session(&app, "ses_test", "/work").await;
    seed_live_card(&app, "ses_test", "已完成的回合。").await;
    Turn::set_card_state(&app.cards_handle(), "ses_test", CardState::Done).await;

    app.handle_message(incoming(
        "msg_card".into(),
        "chat_1".into(),
        "p2p".into(),
        None,
        "/card".into(),
        None,
    ))
    .await;

    assert!(
        platform.texts().await.is_empty(),
        "the retired 'no live card' notice must not be sent: {:?}",
        platform.calls.lock().await
    );
    let card = one_posted_card(&platform).await;
    assert_eq!(
        card["header"]["title"]["content"], "📄 会话",
        "the idle read wears the Session Card's static header: {card}"
    );
    let text = card_text(&card);
    assert!(text.contains("✅ 空闲"), "the status line names the read: {text}");
    assert!(text.contains("**最近对话**"), "the tail renders: {text}");
    assert!(
        text.contains("继续改登录"),
        "the tail carries the conversation: {text}"
    );

    // The read is the Session's card in memory, but it is not a live card and
    // joins no Chain Record: a restart's reap, a Wake and the ownership verdict
    // keep ignoring it.
    let cards = app.cards_handle();
    assert_eq!(
        Turn::card_message_id(&cards, "ses_test").await.as_deref(),
        Some("msg_reply")
    );
    assert!(
        !Turn::is_running(&cards, "ses_test").await,
        "a read is never live"
    );
    assert!(
        !Turn::is_chain_card(&cards, "ses_test").await,
        "the idle card is no chain card"
    );
    assert!(
        cards.chains.get("ses_test").is_none(),
        "the idle read must write no Chain Record"
    );

    let calls = platform.calls.lock().await.clone();
    assert!(
        !calls.iter().any(|c| matches!(
            c,
            PlatformCall::UpdateMessage { message_id, .. } if message_id == "om_live"
        )),
        "a finished Turn card must not be repainted: {calls:?}"
    );
}

/// A stopped Turn's card session also stays in `cards` (#394): `/card` reads
/// the Session once exactly like a Done one — the pull probe is not running,
/// so Stopped never causes the card to be pulled or repainted.
#[tokio::test]
async fn card_command_after_a_stopped_turn_posts_an_idle_read() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let (app, platform) = build_app(cfg, idle_read_backend()).await;
    seed_session(&app, "ses_test", "/work").await;
    seed_live_card(&app, "ses_test", "被停止的回合。").await;
    Turn::set_card_state(&app.cards_handle(), "ses_test", CardState::Stopped).await;
    assert!(
        !Turn::is_running(&app.cards_handle(), "ses_test").await,
        "a stopped card must not count as running for the pull probe"
    );

    app.handle_message(incoming(
        "msg_card".into(),
        "chat_1".into(),
        "p2p".into(),
        None,
        "/card".into(),
        None,
    ))
    .await;

    assert!(
        platform.texts().await.is_empty(),
        "no notice may be sent: {:?}",
        platform.calls.lock().await
    );
    let card = one_posted_card(&platform).await;
    assert_eq!(card["header"]["title"]["content"], "📄 会话");
    let calls = platform.calls.lock().await.clone();
    assert!(
        !calls.iter().any(|c| matches!(
            c,
            PlatformCall::UpdateMessage { message_id, .. } if message_id == "om_live"
        )),
        "the stopped card must not be repainted: {calls:?}"
    );
}

/// A `/card` read that finds the Session **busy** and its newest user message
/// external follows the run (ADR-0100, #1005): the posted card is the Session's
/// live card, the existing external renderer is armed on it, and a Chain Record
/// now names it. The card still opens with the read's status and tail.
#[tokio::test]
async fn a_busy_read_follows_the_run_on_the_posted_card() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let mut backend = idle_read_backend();
    backend.with_session_status("ses_test", Some(crate::backend::types::SessionStatus::Busy));
    let (app, platform) = build_app(cfg, backend).await;
    seed_session(&app, "ses_test", "/work").await;
    // A committed follow adopts the new run, so it clears the pre-run marker.
    app.stopped_sessions.lock().await.insert("ses_test".to_string());

    app.handle_message(incoming(
        "msg_card".into(),
        "chat_1".into(),
        "p2p".into(),
        None,
        "/card".into(),
        None,
    ))
    .await;

    let card = one_posted_card(&platform).await;
    let text = card_text(&card);
    assert!(text.contains("⚙️ 运行中"), "the status line names busy: {text}");
    assert!(text.contains("**最近对话**"), "the tail renders: {text}");
    assert!(text.contains("继续改登录"), "the tail carries the read: {text}");
    assert!(
        !text.contains(FOLLOW_HINT),
        "the pre-arm send is the static read, not the follow copy: {text}"
    );
    assert!(
        !text.contains("超出单卡容量"),
        "the read renders no overflow notice: {text}"
    );

    // The posted card became the Session's live card, and the arm wrote the
    // Chain Record that makes a restart reap it.
    let cards = app.cards_handle();
    assert!(
        Turn::is_running(&cards, "ses_test").await,
        "the follow is render-owned"
    );
    assert!(
        Turn::is_chain_card(&cards, "ses_test").await,
        "a follow is a chain card"
    );
    assert!(
        Turn::card_is_live(&cards, "ses_test").await,
        "the follow is the growing live card"
    );
    assert_eq!(
        Turn::card_message_id(&cards, "ses_test").await.as_deref(),
        Some("msg_reply"),
        "the sent id is the card's identity"
    );
    let record = cards
        .chains
        .get("ses_test")
        .expect("the follow takes the chain over");
    assert_eq!(record.card_message_id, "msg_reply");
    assert!(
        !app.stopped_sessions.lock().await.contains("ses_test"),
        "the committed follow clears the pre-run stop marker"
    );
}

/// A `/card` read that finds the Session **waiting on Background Tasks** with
/// an external newest message follows the wait (ADR-0059, ADR-0100): the
/// posted card is the live card and the renderer yields in place.
#[tokio::test]
async fn a_waiting_read_follows_the_run_on_the_posted_card() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let mut backend = MockBackend::new(realistic_parts());
    backend.given_transcript(
        "ses_test",
        vec![
            SessionTranscript::new(vec![
                typed_message(
                    "msg_u1",
                    MessageRole::User,
                    Some(1_000),
                    vec![text_part("跑一下 CI")],
                ),
                typed_message(
                    "msg_a1",
                    MessageRole::Assistant,
                    Some(2_000),
                    vec![Part::StepFinish(StepFinish {
                        reason: FinishReason::Stop,
                    })],
                ),
            ])
            .with_executions(vec![execution(2_500)])
            .with_background_tasks(vec![background_shell(1_100)]),
        ],
    );
    let (app, platform) = build_app(cfg, backend).await;
    seed_session(&app, "ses_test", "/work").await;

    app.handle_message(incoming(
        "msg_card".into(),
        "chat_1".into(),
        "p2p".into(),
        None,
        "/card".into(),
        None,
    ))
    .await;

    let card = one_posted_card(&platform).await;
    let text = card_text(&card);
    assert!(
        text.contains("⏳ 等待后台任务"),
        "the status line names the wait: {text}"
    );
    assert!(text.contains("**最近对话**"), "the tail renders: {text}");
    assert!(text.contains("跑一下 CI"), "the tail carries the read: {text}");
    assert!(
        !text.contains("超出单卡容量"),
        "the follow card renders no overflow notice: {text}"
    );

    let cards = app.cards_handle();
    assert!(
        Turn::is_running(&cards, "ses_test").await,
        "a waiting run is followed"
    );
    assert!(
        Turn::is_chain_card(&cards, "ses_test").await,
        "the follow replaced the read with a chain card"
    );
    assert!(Turn::card_is_live(&cards, "ses_test").await);
    assert_eq!(
        Turn::card_message_id(&cards, "ses_test").await.as_deref(),
        Some("msg_reply")
    );
    let record = cards
        .chains
        .get("ses_test")
        .expect("the wait follow takes the chain over");
    assert_eq!(record.card_message_id, "msg_reply");
}

/// The read's pending request is seeded onto the follow card, and the next
/// sweep — which does not share the follow's `seen` set — must not post a
/// duplicate standalone card for it (ADR-0100, #1005; the #1004 blocker).
#[tokio::test]
async fn a_busy_follow_seeds_its_pending_and_the_sweep_does_not_duplicate_it() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let mut backend = idle_read_backend();
    backend.with_session_status("ses_test", Some(crate::backend::types::SessionStatus::Busy));
    backend.ask_permission(perm_request("per_1", "ses_test", "ls -la"));
    let (app, platform) = build_app(cfg, backend).await;
    seed_session(&app, "ses_test", "/work").await;

    app.handle_message(incoming(
        "msg_card".into(),
        "chat_1".into(),
        "p2p".into(),
        None,
        "/card".into(),
        None,
    ))
    .await;

    let cards = app.cards_handle();
    assert!(
        Turn::has_interaction_in(&cards, "ses_test", "per_1").await,
        "the read's pending rides on the follow card's accumulator"
    );
    let card = one_posted_card(&platform).await;
    let text = card_text(&card);
    assert!(
        !text.contains(FOLLOW_HINT),
        "the pre-arm send is the static read: {text}"
    );
    assert!(
        text.contains("有 1 个请求等待你处理（见原卡片）"),
        "the static read points the request at its own card: {text}"
    );

    // A fresh poller (its own `seen`) must recognise the block already on the
    // card instead of posting a standalone duplicate for it.
    let mut seen = std::collections::HashSet::new();
    app.permission.sweep(&app.flow_handles(), &mut seen).await;
    assert_eq!(
        platform.replied_cards().await.len(),
        1,
        "no second card for a request the follow already carries: {:?}",
        platform.calls.lock().await
    );
    assert!(
        !platform
            .calls
            .lock()
            .await
            .iter()
            .any(|c| matches!(c, PlatformCall::SendCard { .. })),
        "no standalone permission card: {:?}",
        platform.calls.lock().await
    );
}

/// A permission seeded onto the `/card` follow is answerable through the normal
/// inline path: the ack carries the updated follow card with the receipt and no
/// live block, and the resumed external run finishes on that same card.
#[tokio::test]
async fn a_busy_follow_permission_click_resumes_the_run() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let mut backend = MockBackend::new(realistic_parts());
    backend.with_session_status("ses_test", Some(crate::backend::types::SessionStatus::Busy));
    backend.external_message_for("ses_test", "帮我重构这个模块");
    backend.ask_permission(perm_request("per_1", "ses_test", "ls -la"));
    let reply_ready = backend.external_reply(realistic_parts());
    let backend = Arc::new(backend);
    let platform = Arc::new(RecordingPlatform::new());
    let app = Arc::new(App::new(cfg, backend.clone(), platform.clone()).unwrap());
    app.core
        .external
        .render_poll_ms
        .store(50, std::sync::atomic::Ordering::Relaxed);
    seed_session(&app, "ses_test", "/work").await;

    app.handle_message(incoming(
        "msg_card".into(),
        "chat_1".into(),
        "p2p".into(),
        None,
        "/card".into(),
        None,
    ))
    .await;

    let cards = app.cards_handle();
    assert_eq!(
        Turn::live_permissions(&cards, "ses_test").await.len(),
        1,
        "the read's permission is seeded onto the follow card"
    );

    let value = serde_json::json!({
        "action": "perm",
        "reply": "once",
        "session_id": "ses_test",
        "request_id": "per_1",
        "directory": "/work",
        "perm_label": "✅ 已允许一次",
        "perm_color": "green",
        "perm_body": "bash",
    });
    let result = app
        .host_action(value)
        .await
        .expect("permission click returns a result");
    let ack = result
        .card
        .as_ref()
        .expect("an inline click carries the updated follow card in the ack")
        .to_string();
    assert!(
        ack.contains("✅ 已允许一次：⚡ 执行 Shell 命令 `ls -la`"),
        "the receipt lands on the follow card: {ack}"
    );
    assert!(
        !ack.contains("🔐 **权限请求**"),
        "the approved block is gone: {ack}"
    );
    assert_eq!(result.toast.as_deref(), Some("已允许本次执行"));
    assert!(
        Turn::live_permissions(&app.cards_handle(), "ses_test")
            .await
            .is_empty(),
        "the approved section is no longer live"
    );

    reply_ready.store(true, std::sync::atomic::Ordering::SeqCst);
    wait_for_card_update(&platform, "the follow's Done card", CardUpdates::Latest, |card| {
        card_header(card).contains("✅")
    })
    .await;
    let calls = platform.calls.lock().await.clone();
    let final_card = calls
        .iter()
        .filter_map(|c| match c {
            PlatformCall::UpdateMessage { message_id, card } if message_id == "msg_reply" => {
                Some(card.to_string())
            }
            _ => None,
        })
        .next_back()
        .expect("the follow card streams the resume");
    assert!(
        final_card.contains("✅ 完成"),
        "the resumed run finished on the follow card: {final_card}"
    );
}

/// A question seeded onto the `/card` follow is remembered (the poll never saw
/// it): clicking an option records the answer, strips the block, and returns the
/// updated follow card rather than an empty ack.
#[tokio::test]
async fn a_busy_follow_question_click_resolves_the_block() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let mut backend = MockBackend::new(realistic_parts());
    backend.with_session_status("ses_test", Some(crate::backend::types::SessionStatus::Busy));
    backend.external_message_for("ses_test", "帮我重构这个模块");
    backend.ask_questions(vec![crate::backend::types::QuestionRequest {
        id: "q_1".into(),
        session_id: "ses_test".into(),
        questions: vec![crate::backend::types::QuestionInfo {
            question: "选择语言".into(),
            header: "language".into(),
            options: vec![crate::backend::types::QuestionOption {
                label: "rust".into(),
                description: String::new(),
                ..Default::default()
            }],
            kind: crate::backend::types::FormFieldKind::String,
            custom: Some(false),
            ..Default::default()
        }],
        ..Default::default()
    }]);
    let (app, _platform) = build_app(cfg, backend).await;
    app.core
        .external
        .render_poll_ms
        .store(50, std::sync::atomic::Ordering::Relaxed);
    seed_session(&app, "ses_test", "/work").await;

    app.handle_message(incoming(
        "msg_card".into(),
        "chat_1".into(),
        "p2p".into(),
        None,
        "/card".into(),
        None,
    ))
    .await;

    let cards = app.cards_handle();
    assert_eq!(
        Turn::live_questions(&cards, "ses_test").await.len(),
        1,
        "the read's question is seeded onto the follow card"
    );
    assert!(
        app.question.has_question("q_1").await,
        "the follow remembered the full request"
    );

    let value = serde_json::json!({
        "action": "question",
        "reply": "answer",
        "session_id": "ses_test",
        "request_id": "q_1",
        "directory": "/work",
        "question_index": 0,
        "answer": "rust",
    });
    let result = app
        .host_action(value)
        .await
        .expect("question click returns a result");
    assert_eq!(result.toast.as_deref(), Some("已回答"));
    let ack = result
        .card
        .expect("an inline click carries the updated follow card, not an empty ack");
    assert!(
        card_text(&ack).contains("✅ 已回答：选择语言 rust"),
        "the follow card leaves the question's receipt: {}",
        card_text(&ack)
    );
    assert!(
        !ack.to_string().contains("\"action\":\"question\""),
        "the answered block's controls are stripped from the follow card: {ack}"
    );
    assert!(
        Turn::live_questions(&app.cards_handle(), "ses_test")
            .await
            .is_empty(),
        "the answered question block is no longer live"
    );
}

/// The busy→idle race across the arm: the gather reads Busy (so the card is
/// built from a live turn), but the follow's fresh arm-time read sees the turn
/// already finished. `/card` still posts the read; it arms no renderer, writes
/// no Chain Record, and the card stays static.
#[tokio::test]
async fn a_busy_read_that_turns_idle_before_the_arm_stays_static() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let backend = idle_read_backend();
    backend.busy_then_idle_once();
    let (app, platform) = build_app(cfg, backend).await;
    seed_session(&app, "ses_test", "/work").await;
    app.core
        .external
        .render_poll_ms
        .store(20, std::sync::atomic::Ordering::Relaxed);

    app.handle_message(incoming(
        "msg_card".into(),
        "chat_1".into(),
        "p2p".into(),
        None,
        "/card".into(),
        None,
    ))
    .await;

    let cards = app.cards_handle();
    assert!(
        !Turn::is_running(&cards, "ses_test").await,
        "an idle arm-time read must not leave a renderer owning the card"
    );
    assert!(
        !Turn::is_chain_card(&cards, "ses_test").await,
        "the posted read joins no chain"
    );
    assert!(
        cards.chains.get("ses_test").is_none(),
        "the static read writes no Chain Record"
    );

    let card = one_posted_card(&platform).await;
    let text = card_text(&card);
    assert!(
        text.contains("⚙️ 运行中"),
        "the card is still the busy gather's static read: {text}"
    );
    assert!(
        text.contains("继续改登录"),
        "the read's tail rides the static card: {text}"
    );

    // With no renderer armed, even a full poll window leaves the card alone.
    tokio::time::sleep(std::time::Duration::from_millis(80)).await;
    let calls = platform.calls.lock().await.clone();
    assert!(
        calls
            .iter()
            .all(|call| !matches!(call, PlatformCall::UpdateMessage { .. })),
        "the static read must never be re-rendered: {calls:?}"
    );
}

/// A request the sweep had already surfaced on its own card stays there: `/card`
/// runs while the Session is busy, follows the run, and does not seed a second
/// copy or count it on the new card.
#[tokio::test]
async fn a_follow_keeps_a_pending_the_sweep_already_surfaced() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let mut backend = idle_read_backend();
    backend.with_session_status("ses_test", Some(crate::backend::types::SessionStatus::Busy));
    backend.ask_permission(perm_request("per_1", "ses_test", "ls -la"));
    let (app, platform) = build_app(cfg, backend).await;
    seed_session(&app, "ses_test", "/work").await;

    // The sweep surfaces the request on its own card before `/card` begins.
    let mut seen = std::collections::HashSet::new();
    app.permission.sweep(&app.flow_handles(), &mut seen).await;
    assert_eq!(
        platform.sent_cards().await.len(),
        1,
        "the standalone card lands first"
    );

    app.handle_message(incoming(
        "msg_card".into(),
        "chat_1".into(),
        "p2p".into(),
        None,
        "/card".into(),
        None,
    ))
    .await;

    let cards = app.cards_handle();
    assert!(
        Turn::is_running(&cards, "ses_test").await,
        "the busy read follows the run"
    );
    assert!(
        !Turn::has_interaction_in(&cards, "ses_test", "per_1").await,
        "the already-surfaced request keeps its own card"
    );
    let cards_now = platform.replied_cards().await;
    assert_eq!(cards_now.len(), 1, "the read posts its own card");
    let text = card_text(&cards_now[0]);
    assert!(
        !text.contains(FOLLOW_HINT),
        "the pre-arm send is the static read: {text}"
    );
    assert!(
        text.contains("有 1 个请求等待你处理（见原卡片）"),
        "the static read points the pre-surfaced request at its own card: {text}"
    );
    assert!(
        text.contains("⏳ 等待你的确认"),
        "the status still names the wait the read saw: {text}"
    );

    // The poller still owns the standalone surface; its next pass must leave
    // the follow card alone.
    app.permission.sweep(&app.flow_handles(), &mut seen).await;
    assert_eq!(
        platform.sent_cards().await.len(),
        1,
        "no duplicate standalone card after the follow: {:?}",
        platform.calls.lock().await
    );
    assert_eq!(
        platform.replied_cards().await.len(),
        1,
        "the follow card is not reposted either: {:?}",
        platform.calls.lock().await
    );
}

/// A **cola-authored** newest message keeps the busy read static (ADR-0100):
/// that run's hand-off belongs to the Chain Record and Session Sync, so no
/// renderer is armed and no chain record appears.
#[tokio::test]
async fn a_busy_read_of_a_cola_authored_turn_stays_static() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let mut backend = MockBackend::new(realistic_parts());
    backend.with_session_status("ses_test", Some(crate::backend::types::SessionStatus::Busy));
    backend.given_transcript(
        "ses_test",
        vec![SessionTranscript::new(vec![typed_message(
            "msg_cola_turn",
            MessageRole::User,
            Some(1_000),
            vec![text_part("cola 自己发起的回合")],
        )])],
    );
    let (app, platform) = build_app(cfg, backend).await;
    seed_session(&app, "ses_test", "/work").await;

    app.handle_message(incoming(
        "msg_card".into(),
        "chat_1".into(),
        "p2p".into(),
        None,
        "/card".into(),
        None,
    ))
    .await;

    let card = one_posted_card(&platform).await;
    assert!(
        card_text(&card).contains("⚙️ 运行中"),
        "the read still names the busy state: {}",
        card_text(&card)
    );
    let cards = app.cards_handle();
    assert!(
        !Turn::is_running(&cards, "ses_test").await,
        "a cola-authored turn is never followed"
    );
    assert!(
        !Turn::is_chain_card(&cards, "ses_test").await,
        "the card stays a read"
    );
    assert_eq!(
        Turn::card_message_id(&cards, "ses_test").await.as_deref(),
        Some("msg_reply"),
        "the static read is still the Session's current card"
    );
    assert!(
        cards.chains.get("ses_test").is_none(),
        "no renderer armed => no Chain Record"
    );
}

/// A command cancelled before the commit publishes nothing: the read and its
/// follow are prepared off the card map, and the send/insert are one critical
/// section. Dropping the future while it waits for that lock leaves no card in
/// the map and posts nothing.
#[tokio::test]
async fn a_card_read_cancelled_before_the_commit_publishes_nothing() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let mut backend = idle_read_backend();
    backend.with_session_status("ses_test", Some(crate::backend::types::SessionStatus::Busy));
    let (app, platform) = build_app(cfg, backend).await;
    seed_session(&app, "ses_test", "/work").await;

    // Hold the Session's card write lock so the command parks before its send.
    let lock = app.cards_handle().write_lock("ses_test").await;
    let held = lock.lock().await;
    let handles = app.command_handles();
    let thread_key = crate::config::ThreadKey::new("chat_1".into(), "chat_1".into());
    let command = tokio::spawn(async move {
        crate::bridge::command::handle_command(
            &handles,
            crate::bridge::command::Command::Card,
            thread_key,
            "msg_card",
            crate::config::ConversationKind::P2p,
            None,
            false,
        )
        .await
        .unwrap();
    });
    for _ in 0..20 {
        tokio::task::yield_now().await;
    }
    command.abort();
    let _ = command.await;
    drop(held);

    assert!(
        platform.replied_cards().await.is_empty(),
        "a command cancelled before its commit posts nothing: {:?}",
        platform.calls.lock().await
    );
    let cards = app.cards_handle();
    assert!(
        Turn::card_message_id(&cards, "ses_test").await.is_none(),
        "no card may be recorded"
    );
    assert!(
        cards.chains.get("ses_test").is_none(),
        "no Chain Record may be written"
    );
}

/// A sweep that starts BEFORE `/card` commits waits on the same card write
/// lock and then sees the armed card, so it attaches the request there instead
/// of posting a standalone duplicate (ADR-0100).
#[tokio::test]
async fn a_sweep_started_before_the_follow_commit_does_not_duplicate_the_pending() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let mut backend = idle_read_backend();
    backend.with_session_status("ses_test", Some(crate::backend::types::SessionStatus::Busy));
    backend.ask_permission(perm_request("per_1", "ses_test", "ls -la"));
    let (app, platform) = build_app(cfg, backend).await;
    seed_session(&app, "ses_test", "/work").await;

    // Park `/card` inside its send while it already holds the card write lock,
    // then start the sweep: it reaches the same lock and cannot commit first.
    let (entered, release) = platform.pause("reply", "msg_card");
    let app2 = app.clone();
    let send = tokio::spawn(async move {
        app2.handle_message(incoming(
            "msg_card".into(),
            "chat_1".into(),
            "p2p".into(),
            None,
            "/card".into(),
            None,
        ))
        .await;
    });
    entered.notified().await;
    let app3 = app.clone();
    let sweep = tokio::spawn(async move {
        let mut seen = std::collections::HashSet::new();
        app3.permission.sweep(&app3.flow_handles(), &mut seen).await;
    });
    for _ in 0..10 {
        tokio::task::yield_now().await;
    }
    release.notify_one();
    send.await.unwrap();
    sweep.await.unwrap();

    let cards = app.cards_handle();
    assert!(
        Turn::is_running(&cards, "ses_test").await,
        "the read armed the follow before the sweep could surface"
    );
    assert!(
        Turn::has_interaction_in(&cards, "ses_test", "per_1").await,
        "the blocked sweep attaches the request to the follow card"
    );
    assert_eq!(
        platform.replied_cards().await.len(),
        1,
        "only the /card read is posted: {:?}",
        platform.calls.lock().await
    );
    assert!(
        platform.sent_cards().await.is_empty(),
        "no standalone card for the follow's seeded pending: {:?}",
        platform.calls.lock().await
    );
    assert!(
        app.permission.sent_cards.lock().await.is_empty(),
        "no standalone surface may be recorded for the follow's seeded pending"
    );
}

/// A command cancelled AFTER its send has returned and INSIDE the commit
/// publishes no map entry and no second card: the send landed as a static
/// read, but the map insert and the Chain Record never happen. The commit is
/// parked on the cards map so the abort lands in that exact window.
#[tokio::test]
async fn a_card_read_cancelled_inside_the_commit_publishes_no_map_entry_or_second_card() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let mut backend = idle_read_backend();
    backend.with_session_status("ses_test", Some(crate::backend::types::SessionStatus::Busy));
    backend.ask_permission(perm_request("per_1", "ses_test", "ls -la"));
    let (app, platform) = build_app(cfg, backend).await;
    seed_session(&app, "ses_test", "/work").await;

    let (entered, release) = platform.pause("reply", "msg_card");
    let app2 = app.clone();
    let send = tokio::spawn(async move {
        app2.handle_message(incoming(
            "msg_card".into(),
            "chat_1".into(),
            "p2p".into(),
            None,
            "/card".into(),
            None,
        ))
        .await;
    });
    entered.notified().await;
    // The command is parked in its send; its earlier map reads are done. Hold
    // the cards map so the commit's insert cannot run, release the send, then
    // abort the future while it waits for the map.
    let cards_handle = app.cards_handle();
    let cards_guard = cards_handle.cards.lock().await;
    release.notify_one();
    for _ in 0..20 {
        tokio::task::yield_now().await;
    }
    send.abort();
    let _ = send.await;
    drop(cards_guard);

    let posted = platform.replied_cards().await;
    assert_eq!(
        posted.len(),
        1,
        "the send landed as a static read: {:?}",
        platform.calls.lock().await
    );
    let text = card_text(&posted[0]);
    assert!(
        !text.contains(FOLLOW_HINT),
        "a card with no renderer must not promise updates: {text}"
    );
    assert!(
        text.contains("有 1 个请求等待你处理（见原卡片）"),
        "the static read points the pending at its own card: {text}"
    );
    let cards = app.cards_handle();
    assert!(
        Turn::card_message_id(&cards, "ses_test").await.is_none(),
        "an aborted commit records no card"
    );
    assert!(
        cards.chains.get("ses_test").is_none(),
        "an aborted commit writes no Chain Record"
    );
}

/// A busy Session whose newest user message carries no server time cannot be
/// followed (no render anchor): `/card` posts the static read, never a dead end.
#[tokio::test]
async fn a_busy_read_with_an_untimed_newest_message_stays_static() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let mut backend = MockBackend::new(realistic_parts());
    backend.with_session_status("ses_test", Some(crate::backend::types::SessionStatus::Busy));
    backend.given_transcript(
        "ses_test",
        vec![SessionTranscript::new(vec![
            typed_message(
                "msg_older",
                MessageRole::User,
                Some(1_000),
                vec![text_part("有时间的老消息")],
            ),
            typed_message(
                "msg_untimed",
                MessageRole::User,
                None,
                vec![text_part("没有时间的新消息")],
            ),
        ])],
    );
    let (app, platform) = build_app(cfg, backend).await;
    seed_session(&app, "ses_test", "/work").await;

    app.handle_message(incoming(
        "msg_card".into(),
        "chat_1".into(),
        "p2p".into(),
        None,
        "/card".into(),
        None,
    ))
    .await;

    let card = one_posted_card(&platform).await;
    assert!(
        card_text(&card).contains("⚙️ 运行中"),
        "the read still reports busy: {}",
        card_text(&card)
    );
    let cards = app.cards_handle();
    assert!(
        !Turn::is_running(&cards, "ses_test").await,
        "no follow is armed without an anchor"
    );
    assert!(
        !Turn::is_chain_card(&cards, "ses_test").await,
        "the card stays a read"
    );
    assert!(
        cards.chains.get("ses_test").is_none(),
        "an unfollowed read writes no Chain Record"
    );
}

/// An untimed newest user message with live Background Tasks is still a
/// Waiting Session: the idle card and the adoption line say 等待后台任务, not
/// 空闲 (the run can wake by itself; #999 US2).
#[tokio::test]
async fn an_untimed_newest_message_with_live_tasks_reads_waiting() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let mut backend = MockBackend::new(realistic_parts());
    backend.given_transcript(
        "ses_test",
        vec![
            SessionTranscript::new(vec![
                typed_message(
                    "msg_older",
                    MessageRole::User,
                    Some(1_000),
                    vec![text_part("有时间的老消息")],
                ),
                typed_message(
                    "msg_untimed",
                    MessageRole::User,
                    None,
                    vec![text_part("没有时间的新消息")],
                ),
            ])
            .with_background_tasks(vec![background_shell(1_100)]),
        ],
    );
    let (app, platform) = build_app(cfg, backend).await;
    seed_session(&app, "ses_test", "/work").await;

    app.handle_message(incoming(
        "msg_card".into(),
        "chat_1".into(),
        "p2p".into(),
        None,
        "/card".into(),
        None,
    ))
    .await;

    let card = one_posted_card(&platform).await;
    let text = card_text(&card);
    assert!(
        text.contains("⏳ 等待后台任务"),
        "an untimed newest message with live tasks still waits: {text}"
    );
    assert!(!text.contains("✅ 空闲"), "never a false idle: {text}");
    let cards = app.cards_handle();
    assert!(!Turn::is_running(&cards, "ses_test").await);
    assert!(cards.chains.get("ses_test").is_none());

    let line = crate::bridge::snapshot::adoption_line(
        &crate::bridge::handles::SnapshotHandles::from_flow(&app.flow_handles()),
        "接管",
        &list_session("ses_test", "标题", "/work", 1),
    )
    .await;
    assert!(
        line.contains("⏳ 等待后台任务"),
        "the adoption line names the wait: {line}"
    );
    assert!(!line.contains("✅ 空闲"), "never a false idle: {line}");
}

/// A `/card` whose card create FAILS returns the error and records or stamps
/// nothing: there is no card to track and no read to mark.
#[tokio::test]
async fn a_failed_card_create_records_and_stamps_nothing() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let mut platform = RecordingPlatform::new();
    platform.fail_reply_card = true;
    let platform = Arc::new(platform);
    let app = Arc::new(App::new(cfg, Arc::new(idle_read_backend()), platform.clone()).unwrap());
    seed_session(&app, "ses_test", "/work").await;

    let result = crate::bridge::command::handle_command(
        &app.command_handles(),
        crate::bridge::command::Command::Card,
        crate::config::ThreadKey::new("chat_1".into(), "chat_1".into()),
        "msg_card",
        crate::config::ConversationKind::P2p,
        None,
        false,
    )
    .await;

    assert!(result.is_err(), "the create error reaches the command");
    let cards = app.cards_handle();
    assert!(
        Turn::card_message_id(&cards, "ses_test").await.is_none(),
        "a failed create records no card"
    );
    assert!(
        cards.chains.get("ses_test").is_none(),
        "a failed create writes no Chain Record"
    );
    assert!(
        platform.replied_cards().await.is_empty(),
        "the failed create left no card to stamp"
    );
}

/// `/card` always answers: a backend that hangs its transcript read times out
/// into the best-effort static read instead of hanging the command.
#[tokio::test]
async fn a_hanging_card_read_posts_a_best_effort_static_read() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let backend = idle_read_backend();
    backend.hang_transcript_for("ses_test").await;
    let (app, platform) = build_app(cfg, backend).await;
    app.core
        .external
        .request_timeout_ms
        .store(50, std::sync::atomic::Ordering::Relaxed);
    seed_session(&app, "ses_test", "/work").await;

    tokio::time::timeout(
        std::time::Duration::from_millis(500),
        app.handle_message(incoming(
            "msg_card".into(),
            "chat_1".into(),
            "p2p".into(),
            None,
            "/card".into(),
            None,
        )),
    )
    .await
    .expect("/card must answer even when a backend read hangs");

    let card = one_posted_card(&platform).await;
    assert_eq!(card["header"]["title"]["content"], "📄 会话");
    let text = card_text(&card);
    assert!(
        !text.contains("⚙️ 运行中") && !text.contains("**最近对话**"),
        "the timed-out read posts an empty best-effort body: {text}"
    );
}

/// The arm spawns its renderer immediately after the map insert, before the
/// seed/record awaits. Aborting it after the insert therefore leaves a live card
/// with a working render loop, not a rendererless `is_running` entry.
#[tokio::test]
async fn an_abort_after_the_arm_insert_leaves_the_follow_rendering() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let mut backend = idle_read_backend();
    backend.with_session_status("ses_test", Some(crate::backend::types::SessionStatus::Busy));
    let (app, platform) = build_app(cfg, backend).await;
    app.core
        .external
        .render_poll_ms
        .store(20, std::sync::atomic::Ordering::Relaxed);
    seed_session(&app, "ses_test", "/work").await;
    let (entered, release) = (
        Arc::new(tokio::sync::Notify::new()),
        Arc::new(tokio::sync::Notify::new()),
    );
    *app.core.external.arm_after_spawn_gate.lock().await = Some((entered.clone(), release.clone()));

    let app2 = app.clone();
    let send = tokio::spawn(async move {
        app2.handle_message(incoming(
            "msg_card".into(),
            "chat_1".into(),
            "p2p".into(),
            None,
            "/card".into(),
            None,
        ))
        .await;
    });
    entered.notified().await;
    send.abort();
    let _ = send.await;
    release.notify_one();

    wait_for_card_update(
        &platform,
        "the spawned follow renders after the abort",
        CardUpdates::Any,
        |card| card_text(card).contains(FOLLOW_HINT),
    )
    .await;
    assert!(
        Turn::is_running(&app.cards_handle(), "ses_test").await,
        "the card is render-owned, not a rendererless entry"
    );
}

/// The previous card's stamp runs BEFORE the arm, so aborting the command
/// inside that bounded GET/PATCH leaves no armed card: the map stays the old
/// idle read and no rendererless live card is created.
#[tokio::test]
async fn an_abort_during_the_old_card_stamp_never_arms_a_rendererless_follow() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let backend = Arc::new(idle_read_backend());
    let platform = Arc::new(RecordingPlatform::new());
    let app = Arc::new(App::new(cfg, backend.clone(), platform.clone()).unwrap());
    app.core
        .external
        .render_poll_ms
        .store(20, std::sync::atomic::Ordering::Relaxed);
    seed_session(&app, "ses_test", "/work").await;
    platform.given_reply_id("om_idle");
    platform.given_reply_id("om_follow");

    // First `/card`: an idle read becomes the current card.
    app.handle_message(incoming(
        "msg_card_1".into(),
        "chat_1".into(),
        "p2p".into(),
        None,
        "/card".into(),
        None,
    ))
    .await;
    assert_eq!(
        Turn::card_message_id(&app.cards_handle(), "ses_test")
            .await
            .as_deref(),
        Some("om_idle")
    );
    let idle = platform.replied_cards().await[0].clone();
    platform.given_card_view("om_idle", idle);
    backend
        .set_session_status("ses_test", Some(crate::backend::types::SessionStatus::Busy))
        .await;

    // The command parks in the previous card's stamp, before the arm.
    let (entered, release) = platform.pause("update", "om_idle");
    let app2 = app.clone();
    let second = tokio::spawn(async move {
        app2.handle_message(incoming(
            "msg_card_2".into(),
            "chat_1".into(),
            "p2p".into(),
            None,
            "/card".into(),
            None,
        ))
        .await;
    });
    entered.notified().await;
    second.abort();
    let _ = second.await;
    release.notify_one();

    let cards = app.cards_handle();
    assert_eq!(
        Turn::card_message_id(&cards, "ses_test").await.as_deref(),
        Some("om_idle"),
        "the aborted command never switched the map to the follow card"
    );
    assert!(
        !Turn::is_running(&cards, "ses_test").await,
        "no rendererless live card may be left behind"
    );
    assert!(
        cards.chains.get("ses_test").is_none(),
        "the aborted follow wrote no Chain Record"
    );
}

/// A Turn install that is admitted before `/card` arms its follow now waits on
/// the session write lock: it cannot replace the follow card between the arm
/// and the seed. When it does land, the seeded request is re-surfaced on the
/// new Turn's card by the sweep, with a card handle — never a dead
/// 「等待你的确认」.
#[tokio::test]
async fn a_turn_install_after_the_follow_arm_keeps_the_seeded_request_answerable() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let mut backend = idle_read_backend();
    backend.with_session_status("ses_test", Some(crate::backend::types::SessionStatus::Busy));
    backend.ask_permission(perm_request("per_1", "ses_test", "ls -la"));
    let (app, platform) = build_app(cfg, backend).await;
    seed_session(&app, "ses_test", "/work").await;
    let cards = app.cards_handle();

    // `/card` holds the write lock across its send/arm/seed. A Turn install
    // spawned in that window must park instead of replacing the card.
    let (entered, release) = platform.pause("reply", "msg_card");
    let app2 = app.clone();
    let send = tokio::spawn(async move {
        app2.handle_message(incoming(
            "msg_card".into(),
            "chat_1".into(),
            "p2p".into(),
            None,
            "/card".into(),
            None,
        ))
        .await;
    });
    entered.notified().await;
    let app3 = app.clone();
    let install = tokio::spawn(async move {
        Turn::install_fresh_session_for_test(&app3.cards_handle(), "ses_test").await;
    });
    for _ in 0..20 {
        tokio::task::yield_now().await;
    }
    assert!(
        !install.is_finished(),
        "a Turn install must wait behind /card's arm+seed"
    );
    release.notify_one();
    send.await.unwrap();
    install.await.unwrap();

    // The arm ran first: its Chain Record names the follow card.
    assert_eq!(
        cards
            .chains
            .get("ses_test")
            .expect("the follow armed")
            .card_message_id,
        "msg_reply"
    );
    // Give the installed loading card the id a real Turn would send next.
    Turn::set_card_message_id(&cards, "ses_test", "om_turn").await;

    // The seed lived on the replaced follow card; the sweep must put the
    // request on the new Turn card and record its handle.
    let mut seen = std::collections::HashSet::new();
    app.permission.sweep(&app.flow_handles(), &mut seen).await;
    assert!(
        Turn::has_interaction_in(&cards, "ses_test", "per_1").await,
        "the request is answerable on the new Turn card"
    );
    assert_eq!(
        app.card_handles.lock().await.message_of("per_1"),
        Some("om_turn"),
        "the sweep records a handle so the block is not a dead end"
    );
}

/// The sweep's insert and flush are one lock section: a Turn install cannot
/// replace the card between them. While the flush is parked, the install stays
/// un-finished; after release the block has a card handle.
#[tokio::test]
async fn a_turn_install_cannot_split_the_sweeps_insert_and_flush() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let mut backend = idle_read_backend();
    backend.ask_permission(perm_request("per_1", "ses_test", "ls -la"));
    let (app, platform) = build_app(cfg, backend).await;
    seed_session(&app, "ses_test", "/work").await;
    seed_live_card(&app, "ses_test", "回合的内容。").await;

    let (entered, release) = platform.pause("update", "om_live");
    let app2 = app.clone();
    let sweep = tokio::spawn(async move {
        let mut seen = std::collections::HashSet::new();
        app2.permission.sweep(&app2.flow_handles(), &mut seen).await;
    });
    entered.notified().await;
    let app3 = app.clone();
    let install = tokio::spawn(async move {
        Turn::install_fresh_session_for_test(&app3.cards_handle(), "ses_test").await;
    });
    for _ in 0..20 {
        tokio::task::yield_now().await;
    }
    assert!(
        !install.is_finished(),
        "the install must wait through the sweep's insert+flush"
    );
    release.notify_one();
    sweep.await.unwrap();
    install.await.unwrap();

    assert_eq!(
        app.card_handles.lock().await.message_of("per_1"),
        Some("om_live"),
        "the flushed block kept a card handle before the install replaced the map"
    );
}

/// A gather that read Idle followed by an arm-time read of Busy must not leave
/// the lead's identity saying "continuing" over a 空闲 body: the body follows
/// the arm-time status.
#[tokio::test]
async fn a_busy_arm_after_an_idle_gather_renders_a_busy_body() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let backend = idle_read_backend();
    backend.idle_then_busy_once();
    let (app, platform) = build_app(cfg, backend).await;
    app.core
        .external
        .render_poll_ms
        .store(20, std::sync::atomic::Ordering::Relaxed);
    seed_session(&app, "ses_test", "/work").await;

    app.handle_message(incoming(
        "msg_card".into(),
        "chat_1".into(),
        "p2p".into(),
        None,
        "/card".into(),
        None,
    ))
    .await;

    wait_for_card_update(
        &platform,
        "the arm-time Busy status reaches the lead body",
        CardUpdates::Any,
        |card| {
            let text = card_text(card);
            text.contains("正在继续该会话的回合") && text.contains(BUSY_CHIP) && !text.contains(IDLE_CHIP)
        },
    )
    .await;
}

/// A prepare timeout keeps the good gather: only the follow arm's own status
/// read hangs, and the static read is still posted from the gathered data.
#[tokio::test]
async fn a_follow_prepare_timeout_keeps_the_gathered_read() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let backend = idle_read_backend();
    backend.hang_second_session_status();
    let (app, platform) = build_app(cfg, backend).await;
    app.core
        .external
        .request_timeout_ms
        .store(50, std::sync::atomic::Ordering::Relaxed);
    seed_session(&app, "ses_test", "/work").await;

    tokio::time::timeout(
        std::time::Duration::from_millis(500),
        app.handle_message(incoming(
            "msg_card".into(),
            "chat_1".into(),
            "p2p".into(),
            None,
            "/card".into(),
            None,
        )),
    )
    .await
    .expect("/card must answer when the follow prepare hangs");

    let card = one_posted_card(&platform).await;
    let text = card_text(&card);
    assert!(
        text.contains("✅ 空闲") && text.contains("继续改登录"),
        "the gathered status and tail survive the prepare timeout: {text}"
    );
    assert!(
        !Turn::is_running(&app.cards_handle(), "ses_test").await,
        "a prepare timeout posts the static read, not a follow"
    );
}

/// The follow's identity line names the Session by its real title, exactly as
/// the adoption line does — never the bare id tail because `/card` passed no
/// title. The renderer's first flush carries the lead.
#[tokio::test]
async fn a_follow_names_the_read_with_the_session_title() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let mut backend = idle_read_backend();
    backend.with_session_status("ses_test", Some(crate::backend::types::SessionStatus::Busy));
    backend.with_session_title("ses_test", "我的会话");
    backend.ask_permission(perm_request("per_1", "ses_test", "ls -la"));
    let (app, platform) = build_app(cfg, backend).await;
    app.core
        .external
        .render_poll_ms
        .store(20, std::sync::atomic::Ordering::Relaxed);
    seed_session(&app, "ses_test", "/work").await;

    app.handle_message(incoming(
        "msg_card".into(),
        "chat_1".into(),
        "p2p".into(),
        None,
        "/card".into(),
        None,
    ))
    .await;

    // The pre-arm send is the static read: it points at the request's own card
    // and promises no updates.
    let posted = platform.replied_cards().await;
    assert_eq!(posted.len(), 1);
    let posted_text = card_text(&posted[0]);
    assert!(!posted_text.contains(FOLLOW_HINT), "{posted_text}");
    assert!(
        posted_text.contains("有 1 个请求等待你处理（见原卡片）"),
        "the pre-arm card is a read: {posted_text}"
    );

    // The renderer's first flush paints the follow copy: the title, the update
    // promise, and the pointer that counts the seeded block on THIS card.
    wait_for_card_update(
        &platform,
        "the follow lead names the session title",
        CardUpdates::Any,
        |card| {
            let text = card_text(card);
            text.contains("已读取 我的会话")
                && text.contains(FOLLOW_HINT)
                && text.contains("有 1 个请求等待你处理")
                && !text.contains("见原卡片")
        },
    )
    .await;
}

/// A Turn that installs its accumulator while the follow's card is being sent
/// wins the map (ADR-0100): the arm's commit is a compare-and-swap, so the
/// just-sent read is stamped taken over and no follow chain record is written.
#[tokio::test]
async fn a_turn_installed_during_the_follow_send_wins_the_map() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let mut backend = idle_read_backend();
    backend.with_session_status("ses_test", Some(crate::backend::types::SessionStatus::Busy));
    backend.ask_permission(perm_request("per_1", "ses_test", "ls -la"));
    let (app, platform) = build_app(cfg, backend).await;
    seed_session(&app, "ses_test", "/work").await;
    // A refused follow adopts nothing, so it must keep the pre-run `/stop`
    // marker the run that still owns the card needs.
    app.stopped_sessions.lock().await.insert("ses_test".to_string());

    let (entered, release) = platform.pause("reply", "msg_card");
    let app2 = app.clone();
    let send = tokio::spawn(async move {
        app2.handle_message(incoming(
            "msg_card".into(),
            "chat_1".into(),
            "p2p".into(),
            None,
            "/card".into(),
            None,
        ))
        .await;
    });
    entered.notified().await;
    // The user's next prompt starts while the follow's send is in flight.
    seed_live_card(&app, "ses_test", "om_live").await;
    release.notify_one();
    send.await.unwrap();

    let cards = app.cards_handle();
    assert_eq!(
        Turn::card_message_id(&cards, "ses_test").await.as_deref(),
        Some("om_live"),
        "the live Turn keeps the session"
    );
    assert!(
        Turn::is_running(&cards, "ses_test").await,
        "the follow must not replace a running card"
    );
    assert!(
        cards.chains.get("ses_test").is_none(),
        "the refused follow writes no Chain Record"
    );
    assert!(
        app.stopped_sessions.lock().await.contains("ses_test"),
        "a refused arm adopts no run, so it leaves the stop marker for the owner"
    );

    let calls = platform.calls.lock().await.clone();
    let stamped = calls
        .iter()
        .rev()
        .find_map(|c| match c {
            PlatformCall::UpdateMessage { message_id, card } if message_id == "msg_reply" => {
                Some(card.clone())
            }
            _ => None,
        })
        .expect("the posted read is stamped taken over");
    let stamped_text = card_text(&stamped);
    assert!(
        stamped_text.contains("⏳ 已由新卡片接管 · 已停止更新"),
        "the read that lost the race is not left unmarked: {stamped_text}"
    );
    assert!(
        !stamped_text.contains(FOLLOW_HINT),
        "the stamp keeps the static body, never the follow promise: {stamped_text}"
    );
    assert!(
        stamped_text.contains("有 1 个请求等待你处理（见原卡片）"),
        "the static body still points the pending at its own card: {stamped_text}"
    );
}

/// The follow's Chain Record write is guarded on the map still holding the
/// card the arm inserted (ADR-0100): once a Turn's card has replaced it, the
/// follow's takeover must neither re-point that card nor write a record naming
/// the read. This simulates the post-insert window directly.
#[tokio::test]
async fn a_follow_record_never_lands_on_a_replaced_card() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let (app, _platform) = build_app(cfg, idle_read_backend()).await;
    seed_live_card(&app, "ses_test", "一个回合").await;

    let took = Turn::take_over_card_if_current(
        &app.cards_handle(),
        "ses_test",
        "om_follow",
        0, // not the planted Turn card's chain id
        Some("/work"),
    )
    .await;

    assert!(!took, "a replaced card is never taken over by the follow");
    assert_eq!(
        Turn::card_message_id(&app.cards_handle(), "ses_test")
            .await
            .as_deref(),
        Some("om_live"),
        "the Turn's card identity is never re-pointed at the read"
    );
    assert!(
        app.cards_handle().chains.get("ses_test").is_none(),
        "no Chain Record may name the refused follow"
    );
}

/// The positive side of the guarded takeover: while the map still holds the
/// armed card, the Chain Record is re-pointed and the previous card is
/// collected. A stub that always returns false would fail this test.
#[tokio::test]
async fn a_follow_record_lands_on_the_armed_card_and_collects_its_predecessor() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let (app, platform) = build_app(cfg, idle_read_backend()).await;
    let cards = app.cards_handle();
    Turn::seed_card(&cards, "ses_test", Some("om_arm")).await;
    Turn::set_turn_anchor(&cards, "ses_test", &turn_anchor(1_000)).await;
    cards.chains.track(
        "ses_test",
        "om_prev",
        MessageId::new("msg_prev"),
        Some(500),
        Some("/work"),
        Some("msg_prev_reply"),
    );

    let chain_id = Turn::card_chain_id(&cards, "ses_test")
        .await
        .expect("the armed card's chain id");
    assert!(
        Turn::take_over_card_if_current(&cards, "ses_test", "om_arm", chain_id, Some("/work")).await,
        "the current armed card is taken over"
    );

    assert_eq!(
        cards.chains.get("ses_test").expect("the record").card_message_id,
        "om_arm",
        "the Chain Record names the armed card"
    );
    let calls = platform.calls.lock().await.clone();
    assert!(
        calls
            .iter()
            .any(|c| matches!(c, PlatformCall::UpdateMessage { message_id, .. } if message_id == "om_prev")),
        "the predecessor is collected: {calls:?}"
    );
}

/// A `/card` that resolves to the pull/split path must not clear a live
/// `/stop` marker: only the branch that arms a follow adopts a new run and owns
/// that reset. The marker is what lets the drain finalize ⏹ 已停止.
#[tokio::test]
async fn a_card_read_that_resolves_to_a_pull_keeps_the_stop_marker() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let mut backend = idle_read_backend();
    backend.with_session_status("ses_test", Some(crate::backend::types::SessionStatus::Busy));
    let (app, _platform) = build_app(cfg, backend).await;
    seed_session(&app, "ses_test", "/work").await;
    app.stopped_sessions.lock().await.insert("ses_test".to_string());

    // Hold the card lock so `/card` passes its outer pull check, then install a
    // live card before it reaches the in-lock re-check: the command resolves to
    // the pull/split branch.
    let lock = app.cards_handle().write_lock("ses_test").await;
    let held = lock.lock().await;
    let handles = app.command_handles();
    let thread_key = crate::config::ThreadKey::new("chat_1".into(), "chat_1".into());
    let command = tokio::spawn(async move {
        crate::bridge::command::handle_command(
            &handles,
            crate::bridge::command::Command::Card,
            thread_key,
            "msg_card",
            crate::config::ConversationKind::P2p,
            None,
            false,
        )
        .await
        .unwrap();
    });
    for _ in 0..10 {
        tokio::task::yield_now().await;
    }
    seed_live_card(&app, "ses_test", "om_live").await;
    drop(held);
    command.await.unwrap();

    assert!(
        app.stopped_sessions.lock().await.contains("ses_test"),
        "a pull/split never adopts the run, so it must leave the stop marker alone"
    );
}

/// `/card` with no Session mapped to the conversation has nothing to read: one
/// text line says so and no card is created. The 「当前没有正在运行的实时卡片」
/// notice is retired (ADR-0100) — an existing Session always answers with a
/// card.
#[tokio::test]
async fn card_command_without_a_session_answers_a_line() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let (app, platform) = build_app(cfg, MockBackend::new(realistic_parts())).await;

    app.handle_message(incoming(
        "msg_card_1".into(),
        "chat_1".into(),
        "p2p".into(),
        None,
        "/card".into(),
        None,
    ))
    .await;

    assert_eq!(
        platform.texts().await,
        vec!["当前没有可读取的会话。".to_string()],
        "no mapped Session answers one line: {:?}",
        platform.calls.lock().await
    );
    assert!(
        platform.replied_cards().await.is_empty(),
        "nothing may be posted without a Session"
    );
}

/// `/card` with a pending permission posts the read-only pointer, never a live
/// block: the status line waits, the pointer counts, and no control or card
/// handle exists on the read.
#[tokio::test]
async fn an_idle_read_points_at_a_pending_request_without_hosting_it() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let mut backend = idle_read_backend();
    backend.ask_permission(perm_request("per_idle", "ses_test", "git status"));
    let (app, platform) = build_app(cfg, backend).await;
    seed_session(&app, "ses_test", "/work").await;

    app.handle_message(incoming(
        "msg_card".into(),
        "chat_1".into(),
        "p2p".into(),
        None,
        "/card".into(),
        None,
    ))
    .await;

    let card = one_posted_card(&platform).await;
    let text = card_text(&card);
    assert!(
        text.contains("⏳ 等待你的确认"),
        "the read waits on the Host: {text}"
    );
    assert!(
        text.contains("有 1 个请求等待你处理"),
        "the pointer counts the request: {text}"
    );
    assert!(
        !text.contains("允许一次") && !text.contains("🔐 **权限请求**"),
        "the read must not host the block: {text}"
    );
    assert!(
        app.card_handles.lock().await.message_of("per_idle").is_none(),
        "no card handle may point the request at the read"
    );
}

/// A second `/card` with nothing in between stamps the previous read
/// `TakenOver` (ADR-0100), and only that card: the new read is current and
/// carries no stamp.
#[tokio::test]
async fn a_second_card_marks_the_previous_read_taken_over() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let (app, platform) = build_app(cfg, idle_read_backend()).await;
    platform.given_reply_id("om_idle_1");
    platform.given_reply_id("om_idle_2");
    seed_session(&app, "ses_test", "/work").await;

    app.handle_message(incoming(
        "msg_card_1".into(),
        "chat_1".into(),
        "p2p".into(),
        None,
        "/card".into(),
        None,
    ))
    .await;
    assert_eq!(
        Turn::card_message_id(&app.cards_handle(), "ses_test")
            .await
            .as_deref(),
        Some("om_idle_1"),
        "the first read becomes the Session's current card"
    );
    // The second read stamps the first by reading its body back first (the
    // real platform serves the card view).
    let first_card = platform.replied_cards().await[0].clone();
    platform.given_card_view("om_idle_1", first_card);

    app.handle_message(incoming(
        "msg_card_2".into(),
        "chat_1".into(),
        "p2p".into(),
        None,
        "/card".into(),
        None,
    ))
    .await;
    assert_eq!(
        Turn::card_message_id(&app.cards_handle(), "ses_test")
            .await
            .as_deref(),
        Some("om_idle_2"),
        "the second read becomes the Session's current card"
    );

    assert_eq!(platform.replied_cards().await.len(), 2, "two reads in sequence");
    assert_eq!(
        Turn::card_message_id(&app.cards_handle(), "ses_test")
            .await
            .as_deref(),
        Some("om_idle_2")
    );
    assert!(
        !Turn::is_chain_card(&app.cards_handle(), "ses_test").await,
        "the current read is still no chain card"
    );

    let calls = platform.calls.lock().await.clone();
    let stamped: Vec<serde_json::Value> = calls
        .iter()
        .filter_map(|c| match c {
            PlatformCall::UpdateMessage { message_id, card } if message_id == "om_idle_1" => {
                Some(card.clone())
            }
            _ => None,
        })
        .collect();
    assert_eq!(stamped.len(), 1, "the previous read is stamped once: {calls:?}");
    assert!(
        card_text(&stamped[0]).contains("⏳ 已由新卡片接管 · 已停止更新"),
        "the previous read wears the superseded header: {}",
        stamped[0]
    );
    assert!(
        !calls.iter().any(|c| matches!(
            c,
            PlatformCall::UpdateMessage { message_id, .. } if message_id == "om_idle_2"
        )),
        "the newer read must not be stamped: {calls:?}"
    );
    assert!(
        app.cards_handle().chains.get("ses_test").is_none(),
        "the supersede writes no Chain Record either"
    );
}

/// A card view carrying a recovery control, as Feishu would return it.
fn recovery_card_view(title: &str, label: &str, action: &str) -> serde_json::Value {
    serde_json::json!({
        "schema": "2.0",
        "config": { "wide_screen_mode": true },
        "header": {
            "title": { "tag": "plain_text", "content": title },
            "template": "red"
        },
        "body": {
            "elements": [
                { "tag": "markdown", "content": "失败内容" },
                {
                    "tag": "button",
                    "text": { "tag": "plain_text", "content": label },
                    "type": "primary",
                    "value": { "action": action, "session_id": "ses_test" }
                }
            ]
        }
    })
}

/// `/card` must stamp away a terminal card that still owns a recovery action:
/// the old Error/Unreceived card would otherwise keep a Retry/重新发起 button
/// whose click resolves against the new idle card and silently does nothing.
#[tokio::test]
async fn a_card_read_supersedes_an_actionable_terminal_card() {
    let _wd = test_work_dir();
    for (state, title, label, action) in [
        (CardState::Error, "❌ 出错", "🔄 重试", "retry"),
        (CardState::Unreceived, "⚠️ 这条消息未被接收", "重新发起", "resume"),
    ] {
        let dir = tempfile::tempdir().unwrap();
        let cfg = test_config(&dir.path().join("sessions.json"));
        let (app, platform) = build_app(cfg, idle_read_backend()).await;
        seed_session(&app, "ses_test", "/work").await;
        seed_live_card(&app, "ses_test", "失败内容").await;
        let cards = app.cards_handle();
        Turn::set_card_state(&cards, "ses_test", state.clone()).await;
        platform.given_card_view("om_live", recovery_card_view(title, label, action));

        app.handle_message(incoming(
            "msg_card".into(),
            "chat_1".into(),
            "p2p".into(),
            None,
            "/card".into(),
            None,
        ))
        .await;

        assert_eq!(
            Turn::card_message_id(&cards, "ses_test").await.as_deref(),
            Some("msg_reply"),
            "the read is the Session's current card"
        );
        let calls = platform.calls.lock().await.clone();
        let stamped = calls
            .iter()
            .rev()
            .find_map(|c| match c {
                PlatformCall::UpdateMessage { message_id, card } if message_id == "om_live" => {
                    Some(card.clone())
                }
                _ => None,
            })
            .expect("the actionable terminal card is stamped");
        assert!(
            card_header(&stamped).contains("已由新卡片接管"),
            "the old card says it was superseded: {}",
            card_header(&stamped)
        );
        let json = stamped.to_string();
        assert!(
            !json.contains("\"tag\":\"button\""),
            "the dead recovery control is stripped: {json}"
        );
        assert!(!json.contains(label), "the label is gone: {json}");
        assert!(
            !json.contains(&format!("\"action\":\"{action}\"")),
            "the callback value is gone: {json}"
        );
        assert!(
            Turn::claim_recovery(&cards, "ses_test", state).await.is_none(),
            "a click on the superseded card claims nothing (the button is gone)"
        );
    }
}

/// When an actionable card's view cannot be read, the stamp falls back to the
/// BARE `TakenOver` header rather than skipping: the dead Retry/重新发起 control
/// outranks body preservation. The read is still recorded.
#[tokio::test]
async fn an_unreadable_actionable_card_is_stamped_bare() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let (app, platform) = build_app(cfg, idle_read_backend()).await;
    seed_session(&app, "ses_test", "/work").await;
    seed_live_card(&app, "ses_test", "失败内容").await;
    let cards = app.cards_handle();
    Turn::set_card_state(&cards, "ses_test", CardState::Error).await;
    // No card view is scripted: the read fails.

    app.handle_message(incoming(
        "msg_card".into(),
        "chat_1".into(),
        "p2p".into(),
        None,
        "/card".into(),
        None,
    ))
    .await;

    assert_eq!(
        Turn::card_message_id(&cards, "ses_test").await.as_deref(),
        Some("msg_reply"),
        "the read is recorded: the bare stamp landed"
    );
    let calls = platform.calls.lock().await.clone();
    let stamped = calls
        .iter()
        .rev()
        .find_map(|c| match c {
            PlatformCall::UpdateMessage { message_id, card } if message_id == "om_live" => Some(card.clone()),
            _ => None,
        })
        .expect("the unreadable card gets the bare TakenOver PATCH");
    assert!(card_header(&stamped).contains("已由新卡片接管"));
    let json = stamped.to_string();
    assert!(
        !json.contains("\"tag\":\"button\""),
        "the bare stamp carries no control: {json}"
    );
    assert!(
        !json.contains("失败内容"),
        "the bare stamp drops the unreadable body: {json}"
    );
}

/// A failed stamp PATCH must not replace the Error card: keeping it current is
/// what keeps its Retry live.
#[tokio::test]
async fn a_failed_actionable_stamp_keeps_the_error_card_current() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let mut backend = MockBackend::new(realistic_parts());
    backend.fail_prompts(1, "simulated failure");
    let (app, platform) = build_app(cfg, backend).await;
    seed_session(&app, "ses_test", "/work").await;
    platform.given_reply_id("om_error");

    app.handle_message(incoming(
        "msg_user".into(),
        "chat_1".into(),
        "p2p".into(),
        None,
        "hi".into(),
        None,
    ))
    .await;
    wait_for_card_update(&platform, "the Error card", CardUpdates::Latest, |card| {
        card_header(card) == "❌ 出错"
    })
    .await;

    let cards = app.cards_handle();
    assert_eq!(
        Turn::card_message_id(&cards, "ses_test").await.as_deref(),
        Some("om_error")
    );
    let error_card = platform
        .updated_cards()
        .await
        .last()
        .cloned()
        .expect("the Error card");
    platform.given_card_view("om_error", error_card);
    platform.given_reply_id("om_read");
    platform
        .fail_update_count
        .store(1, std::sync::atomic::Ordering::SeqCst);

    app.handle_message(incoming(
        "msg_card".into(),
        "chat_1".into(),
        "p2p".into(),
        None,
        "/card".into(),
        None,
    ))
    .await;

    assert_eq!(
        Turn::card_message_id(&cards, "ses_test").await.as_deref(),
        Some("om_error"),
        "a failed stamp leaves the Error card current"
    );
    assert_eq!(Turn::card_state(&cards, "ses_test").await, Some(CardState::Error));
    assert!(
        Turn::claim_recovery(&cards, "ses_test", CardState::Error)
            .await
            .is_some(),
        "the Retry still claims"
    );
}

/// A second `/card` with an UNREADABLE old read view still marks the old card:
/// the bare TakeOver header is PATCHed, so exactly one read is current and none
/// is left both unmarked and behind.
#[tokio::test]
async fn a_second_card_stamps_an_unreadable_previous_read() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let (app, platform) = build_app(cfg, idle_read_backend()).await;
    platform.given_reply_id("om_idle_1");
    platform.given_reply_id("om_idle_2");
    seed_session(&app, "ses_test", "/work").await;

    app.handle_message(incoming(
        "msg_card_1".into(),
        "chat_1".into(),
        "p2p".into(),
        None,
        "/card".into(),
        None,
    ))
    .await;
    // No view is scripted for om_idle_1: the second stamp's read fails.
    app.handle_message(incoming(
        "msg_card_2".into(),
        "chat_1".into(),
        "p2p".into(),
        None,
        "/card".into(),
        None,
    ))
    .await;

    assert_eq!(
        Turn::card_message_id(&app.cards_handle(), "ses_test")
            .await
            .as_deref(),
        Some("om_idle_2"),
        "the new read is current"
    );
    let calls = platform.calls.lock().await.clone();
    let stamped = calls
        .iter()
        .rev()
        .find_map(|c| match c {
            PlatformCall::UpdateMessage { message_id, card } if message_id == "om_idle_1" => {
                Some(card.clone())
            }
            _ => None,
        })
        .expect("the unreadable old read is still stamped");
    assert_eq!(
        stamped["header"]["title"]["content"],
        "⏳ 已由新卡片接管 · 已停止更新"
    );
    assert!(
        stamped["body"]["elements"]
            .as_array()
            .is_none_or(|elements| elements.is_empty()),
        "the fallback is the bare header: {stamped}"
    );
}

/// If the old read's stamp PATCH fails, the new read must NOT become current:
/// the old map entry stays, the posted read stays untracked, and the next
/// `/card` retries and marks it.
#[tokio::test]
async fn a_failed_previous_read_stamp_keeps_the_old_read_current() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let (app, platform) = build_app(cfg, idle_read_backend()).await;
    platform.given_reply_id("om_idle_1");
    platform.given_reply_id("om_idle_2");
    platform.given_reply_id("om_idle_3");
    seed_session(&app, "ses_test", "/work").await;

    app.handle_message(incoming(
        "msg_card_1".into(),
        "chat_1".into(),
        "p2p".into(),
        None,
        "/card".into(),
        None,
    ))
    .await;
    let first = platform.replied_cards().await[0].clone();
    platform.given_card_view("om_idle_1", first);
    platform
        .fail_update_count
        .store(1, std::sync::atomic::Ordering::SeqCst);

    app.handle_message(incoming(
        "msg_card_2".into(),
        "chat_1".into(),
        "p2p".into(),
        None,
        "/card".into(),
        None,
    ))
    .await;
    assert_eq!(
        Turn::card_message_id(&app.cards_handle(), "ses_test")
            .await
            .as_deref(),
        Some("om_idle_1"),
        "the old read stays current when its stamp PATCH fails"
    );
    let calls = platform.calls.lock().await.clone();
    let untracked = calls
        .iter()
        .rev()
        .find_map(|c| match c {
            PlatformCall::UpdateMessage { message_id, card } if message_id == "om_idle_2" => {
                Some(card.clone())
            }
            _ => None,
        })
        .expect("the untracked read is marked superseded");
    assert_eq!(
        untracked["header"]["title"]["content"], "⏳ 已由新卡片接管 · 已停止更新",
        "the just-sent read cannot be mistaken for current"
    );

    // The next `/card` retries the stamp and becomes current.
    app.handle_message(incoming(
        "msg_card_3".into(),
        "chat_1".into(),
        "p2p".into(),
        None,
        "/card".into(),
        None,
    ))
    .await;
    assert_eq!(
        Turn::card_message_id(&app.cards_handle(), "ses_test")
            .await
            .as_deref(),
        Some("om_idle_3"),
        "the retried stamp lets the next read become current"
    );
    let calls = platform.calls.lock().await.clone();
    let stamps: Vec<&serde_json::Value> = calls
        .iter()
        .filter_map(|c| match c {
            PlatformCall::UpdateMessage { message_id, card } if message_id == "om_idle_1" => Some(card),
            _ => None,
        })
        .collect();
    assert!(
        stamps
            .last()
            .is_some_and(|card| card["header"]["title"]["content"] == "⏳ 已由新卡片接管 · 已停止更新"),
        "the later `/card` marks the old read: {calls:?}"
    );
}

/// A stalled stamp PATCH is bounded: it times out as a failed stamp, so the
/// old read stays current instead of holding the session write lock forever.
#[tokio::test]
async fn a_parked_stamp_patch_times_out_and_keeps_the_old_read_current() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let (app, platform) = build_app(cfg, idle_read_backend()).await;
    platform.given_reply_id("om_idle_1");
    platform.given_reply_id("om_idle_2");
    seed_session(&app, "ses_test", "/work").await;

    app.handle_message(incoming(
        "msg_card_1".into(),
        "chat_1".into(),
        "p2p".into(),
        None,
        "/card".into(),
        None,
    ))
    .await;
    let first = platform.replied_cards().await[0].clone();
    platform.given_card_view("om_idle_1", first);
    app.cards_handle()
        .preserved_view_timeout_ms
        .store(25, std::sync::atomic::Ordering::Relaxed);
    let (_entered, _release) = platform.pause("update", "om_idle_1");

    app.handle_message(incoming(
        "msg_card_2".into(),
        "chat_1".into(),
        "p2p".into(),
        None,
        "/card".into(),
        None,
    ))
    .await;

    assert_eq!(
        Turn::card_message_id(&app.cards_handle(), "ses_test")
            .await
            .as_deref(),
        Some("om_idle_1"),
        "a timed-out stamp leaves the old read current"
    );
}

/// A terminal card can still carry an unanswered Permission/Question (a lost
/// contact or stuck panel ends the Turn in Error). `/card` must not replace it:
/// the old card stays current with its answerable controls, and the posted read
/// is marked TakenOver so it cannot look current.
#[tokio::test]
async fn a_card_read_keeps_a_terminal_card_with_a_live_block_answerable() {
    let _wd = test_work_dir();
    for permission in [true, false] {
        let dir = tempfile::tempdir().unwrap();
        let cfg = test_config(&dir.path().join("sessions.json"));
        let (app, platform) = build_app(cfg, idle_read_backend()).await;
        seed_session(&app, "ses_test", "/work").await;
        seed_live_card(&app, "ses_test", "失败内容").await;
        platform.given_reply_id("om_read");
        let cards = app.cards_handle();
        if permission {
            assert!(
                Turn::add_permission(
                    &cards,
                    "ses_test",
                    &perm_request("per_1", "ses_test", "ls -la"),
                    "/work"
                )
                .await
            );
        } else {
            let question = crate::backend::types::QuestionRequest {
                id: "q_1".into(),
                session_id: "ses_test".into(),
                questions: vec![crate::backend::types::QuestionInfo {
                    question: "继续？".into(),
                    header: "继续".into(),
                    options: vec![crate::backend::types::QuestionOption {
                        label: "是".into(),
                        description: String::new(),
                        ..Default::default()
                    }],
                    ..Default::default()
                }],
                ..Default::default()
            };
            let paint = crate::bridge::question::QuestionPaint::blank(question.questions.len());
            assert!(Turn::add_question(&cards, "ses_test", &question, "/work", &paint).await);
        }
        Turn::set_card_state(&cards, "ses_test", CardState::Error).await;

        app.handle_message(incoming(
            "msg_card".into(),
            "chat_1".into(),
            "p2p".into(),
            None,
            "/card".into(),
            None,
        ))
        .await;

        assert_eq!(
            Turn::card_message_id(&cards, "ses_test").await.as_deref(),
            Some("om_live"),
            "the terminal block card stays current"
        );
        if permission {
            assert!(!Turn::live_permissions(&cards, "ses_test").await.is_empty());
        } else {
            assert!(!Turn::live_questions(&cards, "ses_test").await.is_empty());
        }
        let rendered = Turn::rendered_card(&cards, "ses_test")
            .await
            .expect("the old card renders")
            .to_string();
        assert!(
            rendered.contains("✅ 允许一次") || rendered.contains("继续？"),
            "the answerable controls survive: {rendered}"
        );

        let calls = platform.calls.lock().await.clone();
        assert!(
            !calls.iter().any(|c| matches!(
                c,
                PlatformCall::UpdateMessage { message_id, .. } if message_id == "om_live"
            )),
            "the block card is never stamped: {calls:?}"
        );
        let read = calls
            .iter()
            .rev()
            .find_map(|c| match c {
                PlatformCall::UpdateMessage { message_id, card } if message_id == "om_read" => {
                    Some(card.clone())
                }
                _ => None,
            })
            .expect("the untracked read is marked taken over");
        assert_eq!(
            read["header"]["title"]["content"], "⏳ 已由新卡片接管 · 已停止更新",
            "the posted read cannot look current"
        );
    }
}

/// A Turn that ran between two `/card` commands has already superseded the
/// older read: the map holds the Turn's card when the second read posts, so the
/// older read is left untouched — no stale stamp collects in the chat.
#[tokio::test]
async fn a_turn_between_two_reads_leaves_the_older_read_alone() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let (app, platform) = build_app(cfg, idle_read_backend()).await;
    platform.given_reply_id("om_idle_1");
    platform.given_reply_id("om_idle_2");
    seed_session(&app, "ses_test", "/work").await;

    app.handle_message(incoming(
        "msg_card_1".into(),
        "chat_1".into(),
        "p2p".into(),
        None,
        "/card".into(),
        None,
    ))
    .await;
    // A Turn runs and ends in between: its terminal card is the map's current
    // card, so the older read is no longer the session's current card.
    seed_live_card(&app, "ses_test", "om_turn").await;
    Turn::set_card_state(&app.cards_handle(), "ses_test", CardState::Done).await;

    app.handle_message(incoming(
        "msg_card_2".into(),
        "chat_1".into(),
        "p2p".into(),
        None,
        "/card".into(),
        None,
    ))
    .await;

    assert_eq!(platform.replied_cards().await.len(), 2, "two reads in sequence");
    assert_eq!(
        Turn::card_message_id(&app.cards_handle(), "ses_test")
            .await
            .as_deref(),
        Some("om_idle_2")
    );
    let calls = platform.calls.lock().await.clone();
    for untouched in ["om_idle_1", "om_turn"] {
        assert!(
            !calls.iter().any(|c| matches!(
                c,
                PlatformCall::UpdateMessage { message_id, .. } if message_id == untouched
            )),
            "a card a Turn superseded must not be stamped: {untouched} in {calls:?}"
        );
    }
}

/// Two `/card` commands racing serialize on the Session's card write lock: the
/// later one stamps the earlier, so exactly one card is marked and the other is
/// the current read — never both unmarked.
#[tokio::test]
async fn racing_card_commands_stamp_the_later_over_the_earlier() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let (app, platform) = build_app(cfg, idle_read_backend()).await;
    platform.given_reply_id("om_idle_1");
    platform.given_reply_id("om_idle_2");
    // The later command stamps the earlier by reading its body back first; both
    // possible ids are scripted so the test observes the stamp, not the
    // unreadable-view degradation.
    let read_view = serde_json::json!({
        "schema": "2.0",
        "config": { "wide_screen_mode": true },
        "header": {
            "title": { "tag": "plain_text", "content": "📄 会话" },
            "template": "grey"
        },
        "body": { "elements": [{ "tag": "markdown", "content": "✅ 空闲" }] }
    });
    platform.given_card_view("om_idle_1", read_view.clone());
    platform.given_card_view("om_idle_2", read_view);
    seed_session(&app, "ses_test", "/work").await;

    let thread_key = crate::config::ThreadKey::new("chat_1".into(), "chat_1".into());
    // Hold the lock so both commands are in flight before either may commit.
    let lock = app.cards_handle().write_lock("ses_test").await;
    let held = lock.lock().await;
    let spawn = |message: &'static str| {
        let handles = app.command_handles();
        let key = thread_key.clone();
        tokio::spawn(async move {
            crate::bridge::command::handle_command(
                &handles,
                crate::bridge::command::Command::Card,
                key,
                message,
                crate::config::ConversationKind::P2p,
                None,
                false,
            )
            .await
            .unwrap();
        })
    };
    let first = spawn("msg_card_1");
    let second = spawn("msg_card_2");
    for _ in 0..10 {
        tokio::task::yield_now().await;
    }
    drop(held);
    first.await.unwrap();
    second.await.unwrap();

    assert_eq!(platform.replied_cards().await.len(), 2, "both commands post");
    let current = Turn::card_message_id(&app.cards_handle(), "ses_test")
        .await
        .expect("a read is current");
    let calls = platform.calls.lock().await.clone();
    let stamps: Vec<String> = calls
        .iter()
        .filter_map(|c| match c {
            PlatformCall::UpdateMessage { message_id, card }
                if card_text(card).contains("⏳ 已由新卡片接管 · 已停止更新") =>
            {
                Some(message_id.clone())
            }
            _ => None,
        })
        .collect();
    assert_eq!(stamps.len(), 1, "exactly one card is stamped: {calls:?}");
    assert_ne!(
        stamps[0], current,
        "the stamp names the earlier card, not the current one"
    );
    assert!(
        ["om_idle_1", "om_idle_2"].contains(&stamps[0].as_str()),
        "the stamp targets one of the posted reads: {stamps:?}"
    );
}

/// The map write is a compare-and-swap: a card already installed while
/// `/card`'s send is in flight (the test seam writes it directly; a real
/// `install_fresh_session` now waits on the write lock) wins. The read must not
/// overwrite it: the live Turn stays the Session's current card and the
/// just-sent read is stamped taken over so it never looks current.
#[tokio::test]
async fn a_turn_installed_during_the_read_wins_the_map() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let (app, platform) = build_app(cfg, idle_read_backend()).await;
    seed_session(&app, "ses_test", "/work").await;

    // Park the idle card's send so a Turn can be installed between the
    // admission check and the map write.
    let (entered, release) = platform.pause("reply", "msg_card");
    let app2 = app.clone();
    let send = tokio::spawn(async move {
        app2.handle_message(incoming(
            "msg_card".into(),
            "chat_1".into(),
            "p2p".into(),
            None,
            "/card".into(),
            None,
        ))
        .await;
    });
    entered.notified().await;
    // The user's next prompt starts while the read's send is in flight.
    seed_live_card(&app, "ses_test", "om_live").await;
    release.notify_one();
    send.await.unwrap();

    let cards = app.cards_handle();
    assert_eq!(
        Turn::card_message_id(&cards, "ses_test").await.as_deref(),
        Some("om_live"),
        "the live Turn keeps the session"
    );
    assert!(
        Turn::is_running(&cards, "ses_test").await,
        "the read must not replace a running card"
    );
    assert!(
        cards.chains.get("ses_test").is_none(),
        "the refused read writes no Chain Record"
    );

    let calls = platform.calls.lock().await.clone();
    let stamped = calls
        .iter()
        .rev()
        .find_map(|c| match c {
            PlatformCall::UpdateMessage { message_id, card } if message_id == "msg_reply" => {
                Some(card.clone())
            }
            _ => None,
        })
        .expect("the posted read is stamped taken over");
    let text = card_text(&stamped);
    assert!(
        text.contains("⏳ 已由新卡片接管 · 已停止更新"),
        "the read that lost the race is not left unmarked: {text}"
    );
    assert!(
        text.contains("✅ 空闲") && text.contains("最近对话"),
        "the stamp keeps the read's body: {text}"
    );
}

/// A read-only Session Card is terminal, so `is_render_owned` alone would admit
/// it as the chain's residual card or a Wake continuation. Neither path may
/// repurpose it, and no Chain Record may appear: the read is sent once and only
/// the next `/card` replaces it.
#[tokio::test]
async fn an_idle_read_is_never_admitted_as_a_residual_or_wake_chain() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let (app, platform) = build_app(cfg, idle_read_backend()).await;
    seed_session(&app, "ses_test", "/work").await;
    app.handle_message(incoming(
        "msg_card".into(),
        "chat_1".into(),
        "p2p".into(),
        None,
        "/card".into(),
        None,
    ))
    .await;
    let idle_id = Turn::card_message_id(&app.cards_handle(), "ses_test")
        .await
        .expect("the read is current");

    let handles = app.flow_handles();
    let transcript = SessionTranscript::default();
    let residual = Turn::render_residual_in_place(
        &handles.cards,
        &handles.sessions,
        &handles.backend,
        &handles.requests,
        "ses_test",
        &transcript,
    )
    .await;
    assert!(!residual, "an idle read is no residual card");

    let split = Turn::split_chain_for_continuation(
        &handles.cards,
        &handles.backend,
        "ses_test",
        "msg_card",
        crate::bridge::turn::ContinuationLine {
            at: 1_000,
            wake: None,
        },
        &transcript,
        1_000,
    )
    .await;
    assert_eq!(split, None, "an idle read is no Wake chain");

    assert!(
        handles.cards.chains.get("ses_test").is_none(),
        "no Chain Record may be written for the read"
    );
    assert_eq!(
        platform.replied_cards().await.len(),
        1,
        "neither path posts another card"
    );
    let calls = platform.calls.lock().await.clone();
    assert!(
        !calls.iter().any(|c| matches!(
            c,
            PlatformCall::UpdateMessage { message_id, .. } if message_id == &idle_id
        )),
        "the read is not repainted as a chain card: {calls:?}"
    );
}

/// An existing Chain Record still names the chain's card: the idle arm stops
/// before the chain-record write **and** its predecessor collect (ADR-0100), so
/// a Turn's record is left exactly as it was and its card is never stamped.
#[tokio::test]
async fn an_idle_read_keeps_an_existing_chain_record() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let (app, platform) = build_app(cfg, idle_read_backend()).await;
    seed_live_card(&app, "ses_test", "om_chain").await;
    Turn::set_card_state(&app.cards_handle(), "ses_test", CardState::Done).await;
    {
        let cards = app.cards_handle();
        cards.chains.track(
            "ses_test",
            "om_chain",
            MessageId::new("msg_turn"),
            Some(1_000),
            Some("/work"),
            Some("msg_1"),
        );
    }
    let before = app.cards_handle().chains.get("ses_test").expect("a record");
    platform.given_reply_id("om_idle");

    app.handle_message(incoming(
        "msg_card".into(),
        "chat_1".into(),
        "p2p".into(),
        None,
        "/card".into(),
        None,
    ))
    .await;

    let after = app
        .cards_handle()
        .chains
        .get("ses_test")
        .expect("the record survives the read");
    assert_eq!(
        after.card_message_id, "om_chain",
        "an existing record still names the chain's card"
    );
    assert_eq!(after.message_id, before.message_id);
    assert_eq!(
        after.generation, before.generation,
        "the read bumps no chain generation"
    );
    let calls = platform.calls.lock().await.clone();
    assert!(
        !calls.iter().any(|c| matches!(
            c,
            PlatformCall::UpdateMessage { message_id, .. } if message_id == "om_chain"
        )),
        "the chain's card is never collected by a read: {calls:?}"
    );
}

/// `/card` mid-turn finalizes the live card with the standard split header and
/// continuation (the Supplement split's path): the previous card's controls are
/// settled, the block stays live on the continuation, and the card handle
/// follows it.
#[tokio::test]
async fn card_pull_migrates_a_pending_permission() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let mut backend = MockBackend::new(realistic_parts());
    backend.ask_permission(perm_request("per_live", "ses_test", "ls -la"));
    let backend = Arc::new(backend);
    let platform = Arc::new(RecordingPlatform::new());
    let app = Arc::new(App::new(cfg, backend.clone(), platform.clone()).unwrap());
    seed_session(&app, "ses_test", "/work").await;

    // The live card carries the inline permission (the poller path), so the
    // card handle records the block before the pull.
    seed_live_card(&app, "ses_test", "回合的内容。").await;
    let mut seen = std::collections::HashSet::new();
    app.permission.sweep(&app.flow_handles(), &mut seen).await;
    assert_eq!(
        app.card_handles.lock().await.message_of("per_live"),
        Some("om_live"),
        "precondition: the live card shows the block"
    );

    app.handle_message(incoming(
        "msg_card".into(),
        "chat_1".into(),
        "p2p".into(),
        None,
        "/card".into(),
        None,
    ))
    .await;

    // The previous card keeps its content but loses the controls.
    let finalized = last_update_of(&platform, "om_live").await;
    assert!(
        !finalized.contains("🔐 **权限请求**") && !finalized.contains("允许一次"),
        "the previous card's controls must be settled, not left dead: {finalized}"
    );
    assert!(
        finalized.contains("回合的内容。"),
        "the finalized card keeps its streamed content: {finalized}"
    );

    // The continuation carries the live block and its handle points at it.
    let (reply_to, card) = continuation(&platform).await;
    assert_eq!(reply_to, "msg_card");
    assert!(
        has(&card, "🔐 **权限请求**") && has(&card, "允许一次"),
        "the live block must ride the continuation: {card}"
    );
    assert!(has(&card, "⏬ 实时卡片已移到底部"));
    assert!(
        !has(&card, "回合的内容。"),
        "the continuation carries the delta, not the finalized content: {card}"
    );
    assert_eq!(
        app.card_handles.lock().await.message_of("per_live"),
        Some("msg_reply"),
        "the handle follows the continuation"
    );
}

/// Leave `ses_test` with a `Done` card that still owes a split continuation: a
/// pull queue whose continuation create failed, with the run then settling.
/// The queue and its reply target are the only record of that tail.
async fn seed_done_card_owing_continuation(app: &Arc<App>, platform: &RecordingPlatform) {
    seed_live_card(app, "ses_test", "已完成但欠一段续卡。").await;
    platform
        .fail_reply_card_count
        .store(1, std::sync::atomic::Ordering::SeqCst);
    let cards = app.cards_handle();
    Turn::split_card_chain(
        &cards,
        "ses_test",
        "msg_pull",
        crate::bridge::turn::SplitKind::Pull,
    )
    .await;
    assert!(
        Turn::has_pending_split(&cards, "ses_test").await,
        "the failed continuation leaves its split queued: {:?}",
        Turn::pending_splits(&cards, "ses_test").await
    );
    assert_eq!(
        Turn::card_message_id(&cards, "ses_test").await.as_deref(),
        Some("om_live"),
        "the failed continuation leaves the finalized card current"
    );
    // The run settles while the continuation is still owed: the terminal card
    // remains the chain's only anchor for that queued content/its target.
    Turn::set_card_state(&cards, "ses_test", CardState::Done).await;
}

/// Seed the session's live card (content `text`, id `om_live`), so `/card` has
/// something to pull down.
async fn seed_live_card(app: &Arc<App>, session_id: &str, text: &str) {
    let cards = app.cards_handle();
    Turn::seed_card(&cards, session_id, Some("om_live")).await;
    Turn::set_card_state(&cards, session_id, CardState::Streaming).await;
    Turn::push_text(&cards, session_id, text).await;
    Turn::set_reply_target(&cards, session_id, "msg_1").await;
}

/// The latest continuation card (`ReplyCard`) the platform recorded.
async fn continuation(platform: &RecordingPlatform) -> (String, serde_json::Value) {
    platform
        .calls
        .lock()
        .await
        .iter()
        .rev()
        .find_map(|c| match c {
            PlatformCall::ReplyCard { reply_to, card } => Some((reply_to.clone(), card.clone())),
            _ => None,
        })
        .expect("a continuation card must have been sent")
}

/// The latest in-place update of `message_id`, as a string.
async fn last_update_of(platform: &RecordingPlatform, message_id: &str) -> String {
    platform
        .calls
        .lock()
        .await
        .iter()
        .rev()
        .find_map(|c| match c {
            PlatformCall::UpdateMessage {
                message_id: mid,
                card,
            } if mid == message_id => Some(card.to_string()),
            _ => None,
        })
        .unwrap_or_else(|| panic!("no update for {message_id}"))
}

fn has(card: &serde_json::Value, needle: &str) -> bool {
    card_text(card).contains(needle)
}

/// `/card` mid-turn finalizes the live card with the standard split header and
/// sends the continuation replied to the command message, carrying the pull
/// status line — the live card is the newest message again, with no separate
/// acknowledgement.
#[tokio::test]
async fn card_command_splits_the_chain_and_the_continuation_takes_over() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let (app, platform) = build_app(cfg, MockBackend::new(realistic_parts())).await;
    seed_session(&app, "ses_test", "/work").await;
    seed_live_card(&app, "ses_test", "第一段进度。").await;

    app.handle_message(incoming(
        "msg_card".into(),
        "chat_1".into(),
        "p2p".into(),
        None,
        "/card".into(),
        None,
    ))
    .await;

    // No acknowledgement message: the continuation's status line is the reply.
    assert!(
        platform.texts().await.is_empty(),
        "no reply_text acknowledgement may be sent: {:?}",
        platform.calls.lock().await
    );

    // The previous card ends with the standard split header and keeps the
    // content shown before the split (a delta handoff, not a copy).
    let finalized = last_update_of(&platform, "om_live").await;
    assert!(
        finalized.contains("部分完成，继续中"),
        "the previous card must wear the standard split header: {finalized}"
    );
    assert!(
        finalized.contains("第一段进度。"),
        "the previous card keeps everything before the split: {finalized}"
    );
    assert!(
        !finalized.contains("⏬ 实时卡片已移到底部"),
        "the status line records the pull on the continuation only: {finalized}"
    );

    // The continuation replies to the command and carries ONLY the delta.
    let (reply_to, card) = continuation(&platform).await;
    assert_eq!(
        reply_to, "msg_card",
        "the continuation must reply to the /card message"
    );
    assert!(
        !has(&card, "第一段进度。"),
        "the continuation must not re-render the finalized content: {card}"
    );
    assert!(
        has(&card, "⏬ 实时卡片已移到底部"),
        "the pull status line must ride the continuation: {card}"
    );

    // The chain is re-anchored: the continuation is the tracked live card and
    // the split is consumed.
    {
        let cards = app.cards_handle();
        assert_eq!(
            Turn::card_message_id(&cards, "ses_test").await.as_deref(),
            Some("msg_reply")
        );
        assert_eq!(
            Turn::reply_target(&cards, "ses_test").await.as_deref(),
            Some("msg_card")
        );
        assert!(
            !Turn::has_pending_split(&cards, "ses_test").await,
            "the split must be consumed"
        );
        assert!(
            Turn::card_is_live(&cards, "ses_test").await,
            "the continuation is the new live card"
        );
    }

    // Later flushes target the continuation, not the finalized card.
    Turn::push_text(&app.cards_handle(), "ses_test", "后续进度。").await;
    crate::bridge::turn::Turn::flush_card(&app.cards_handle(), "ses_test").await;
    let calls = platform.calls.lock().await.clone();
    let last_update = calls
        .iter()
        .rev()
        .find_map(|c| match c {
            PlatformCall::UpdateMessage { message_id, card } => Some((message_id.clone(), card.clone())),
            _ => None,
        })
        .expect("a later flush");
    assert_eq!(
        last_update.0, "msg_reply",
        "later updates must target the continuation: {calls:?}"
    );
    assert!(has(&last_update.1, "后续进度。"));
}
// -- ADR-0100: one body composition, two drivers ----------------------------

fn body_tail_message(role: MessageRole, created_ms: i64, text: &str) -> crate::backend::TailEntry {
    crate::backend::TailEntry::Message(crate::backend::TailMessage {
        role,
        created_ms,
        text: text.into(),
        files: Vec::new(),
    })
}

fn body_compaction(created_ms: i64, summary: &str) -> crate::backend::TailEntry {
    crate::backend::TailEntry::Compaction(crate::backend::TailMarker {
        created_ms,
        compaction: crate::backend::Compaction {
            status: crate::backend::CompactionStatus::Completed,
            reason: crate::backend::CompactionReason::Auto,
            summary: summary.into(),
            usage: None,
            error: None,
            recent: String::new(),
        },
    })
}

/// One body as the two presentations carry it, for a real comparison rather
/// than a preview-vs-preview one:
/// - `lead` is the status line (and pointer) above the tail, as flat lines;
/// - `messages` is every tail message with its FULL body — the card side reads
///   the folded panel's content chunks, never its 40-char preview title;
/// - `markers` is every Compaction row the card carries (title + folded
///   summary). Both drivers keep the same rows at their server positions
///   (#1002); a live timeline may additionally render its own row, which
///   ADR-0082's 2026-11-07 amendment records as deliberate.
#[derive(Debug, Default, PartialEq)]
struct RenderedBody {
    lead: Vec<String>,
    messages: Vec<(String, String)>,
    markers: Vec<(String, String)>,
    omitted: Option<String>,
}

fn split_role_line(line: &str) -> Option<(String, String)> {
    for role in ["👤", "🤖", "💬"] {
        if let Some(rest) = line.strip_prefix(role) {
            return Some((role.to_string(), rest.trim_start().to_string()));
        }
    }
    None
}

/// The one-shot card's body. Panels are compared by their CONTENT chunks (the
/// full entry), which is what the reader sees when the fold opens; the title is
/// only a preview and a parity check over it would not see a truncated body.
fn idle_card_body(card: &serde_json::Value) -> RenderedBody {
    let mut body = RenderedBody::default();
    let mut in_tail = false;
    for element in card["body"]["elements"].as_array().expect("card body") {
        match element["tag"].as_str() {
            Some("markdown") => {
                let content = element["content"].as_str().expect("markdown content");
                if content == "**最近对话**" {
                    in_tail = true;
                } else if !in_tail {
                    body.lead.extend(content.lines().map(str::to_string));
                } else if content.starts_with("…更早的") {
                    body.omitted = Some(content.to_string());
                } else {
                    // A compaction row with nothing to fold is one markdown line.
                    body.markers.push((content.to_string(), String::new()));
                }
            }
            Some("collapsible_panel") => {
                let title = element["header"]["title"]["content"]
                    .as_str()
                    .expect("panel title");
                let folded: String = element["elements"]
                    .as_array()
                    .expect("panel elements")
                    .iter()
                    .filter_map(|chunk| chunk["content"].as_str())
                    .collect();
                match split_role_line(title) {
                    Some((role, _preview)) => body.messages.push((role, folded)),
                    None => body.markers.push((title.to_string(), folded)),
                }
            }
            _ => {}
        }
    }
    body
}

/// The busy follow's static lead, with its takeover identity line dropped: the
/// body begins after the blank line that separates the two.
///
/// A message body is MULTI-LINE, so the tail is parsed by its role prefix rather
/// than line-by-line: a line that starts with a role marker opens an entry and
/// every following line (blank ones included) belongs to that entry's body until
/// the next marker. A Compaction row opens with [`LEAD_COMPACTION_PREFIX`] and
/// keeps its folded summary as following lines, so the marker's (title, body)
/// pair can be compared with the card's panel.
fn follow_lead_body(lead: &str) -> RenderedBody {
    let (_, text) = lead.split_once("\n\n").expect("identity line, then the body");
    let mut body = RenderedBody::default();
    let mut in_tail = false;
    let mut open: Option<(String, String)> = None;
    let mut marker: Option<(String, String)> = None;
    for line in text.lines() {
        if line == "**最近对话**" {
            in_tail = true;
            continue;
        }
        if !in_tail {
            if !line.is_empty() {
                body.lead.push(line.to_string());
            }
            continue;
        }
        if let Some(title) = line.strip_prefix(LEAD_COMPACTION_PREFIX) {
            if let Some(entry) = open.take() {
                body.messages.push(entry);
            }
            if let Some(entry) = marker.take() {
                body.markers.push(entry);
            }
            marker = Some((title.to_string(), String::new()));
        } else if let Some((role, first)) = split_role_line(line) {
            if let Some(entry) = open.take() {
                body.messages.push(entry);
            }
            if let Some(entry) = marker.take() {
                body.markers.push(entry);
            }
            open = Some((role, first));
        } else if let Some((_, message)) = open.as_mut() {
            message.push('\n');
            message.push_str(line);
        } else if let Some((_, marker_body)) = marker.as_mut() {
            if !marker_body.is_empty() {
                marker_body.push('\n');
            }
            marker_body.push_str(line);
        } else if line.starts_with("…更早的") {
            body.omitted = Some(line.to_string());
        } else if !line.is_empty() {
            // The lead must not carry an unmatched commit marker: record it so
            // the parity test can see one if it ever comes back.
            body.markers.push((line.to_string(), String::new()));
        }
    }
    if let Some(entry) = open.take() {
        body.messages.push(entry);
    }
    if let Some(entry) = marker.take() {
        body.markers.push(entry);
    }
    body
}

fn parity_read(
    status: crate::backend::types::SessionStatus,
    waiting: bool,
    tail: Vec<crate::backend::TailEntry>,
) -> SnapshotData {
    SnapshotData {
        session_id: "ses_alpha01".into(),
        directory: "/work/ext".into(),
        status: Some(status),
        waiting,
        pending: Vec::new(),
        tail,
        newest_user_anchor: None,
        newest_user_is_cola_authored: false,
    }
}

/// ADR-0100 parity: the idle Session Card and the busy follow's static lead
/// take the SAME read through the ONE body composition, so the same read cannot
/// render two ways. Message bodies are compared in FULL (the card's folded
/// content chunks against the lead's text), including one longer than the
/// panel's 40-char preview — the case a title-only comparison cannot see.
#[test]
fn the_idle_card_and_the_busy_follow_render_the_same_body() {
    let long =
        "这是一条明显超过四十个字符的对话正文，用来证明对比的是完整内容而不是折叠面板的预览标题。".repeat(2);
    let read = parity_read(
        crate::backend::types::SessionStatus::Idle,
        false,
        vec![
            body_tail_message(MessageRole::User, 1_000, "问题一"),
            body_tail_message(MessageRole::Assistant, 2_000, &long),
            body_tail_message(MessageRole::User, 3_000, "问题二"),
            body_tail_message(MessageRole::Assistant, 4_000, "回答二"),
        ],
    );
    // Driver A: the one-shot card the idle Session Card renders.
    let card = crate::feishu::card::session::build_idle_card(&read);
    // Driver B: the busy follow's static lead.
    let lead = crate::bridge::external::snapshot_follow_lead("接管", "唯一外部标题", &read, &[], true);

    // The fixture really is longer than a panel title, so the full-body
    // comparison is doing work the old title comparison could not.
    assert!(
        long.chars().count() > 40,
        "the fixture must exceed the panel preview budget"
    );
    let long_panel = card["body"]["elements"]
        .as_array()
        .expect("card body")
        .iter()
        .find(|element| element["element_id"] == "tail_1")
        .expect("the long message's panel");
    let title = long_panel["header"]["title"]["content"].as_str().expect("title");
    let folded = long_panel["elements"][0]["content"].as_str().expect("content");
    assert_eq!(folded, long, "the panel folds the FULL body");
    assert!(
        title.chars().count() < long.chars().count() && title.ends_with('…'),
        "the panel title is only a preview: {title}"
    );

    let card_body = idle_card_body(&card);
    let lead_body = follow_lead_body(&lead);
    assert_eq!(card_body.lead, lead_body.lead, "same status/pointer: {card}");
    assert_eq!(card_body.messages, lead_body.messages, "same full message bodies");
    assert_eq!(card_body.markers, lead_body.markers, "same marker rows (none)");
    assert_eq!(card_body.omitted, lead_body.omitted);
}

/// The parity holds for the Waiting copy too: with `busy_now = false` the
/// identity line says the wait is in progress, while the shared body is the
/// 等待后台任务 chip on BOTH drivers — the update promise lives in the identity
/// line, never in the body (#1002).
#[test]
fn the_parity_holds_for_the_waiting_copy() {
    let read = parity_read(
        crate::backend::types::SessionStatus::Idle,
        true,
        vec![body_tail_message(MessageRole::User, 1_000, "问题一")],
    );
    let card = crate::feishu::card::session::build_idle_card(&read);

    for busy_now in [false, true] {
        let lead =
            crate::bridge::external::snapshot_follow_lead("接管", "唯一外部标题", &read, &[], busy_now);
        let identity = lead.split_once("\n\n").expect("identity, then body").0;
        // `busy_now = false` names the wait; a run that turned busy after a
        // waiting gather keeps the continuing copy.
        assert_eq!(
            identity.contains("等待后台任务完成"),
            !busy_now,
            "identity copy for busy_now={busy_now}: {identity}"
        );
        let card_body = idle_card_body(&card);
        let lead_body = follow_lead_body(&lead);
        assert_eq!(
            card_body, lead_body,
            "the same read renders the same body, busy_now={busy_now}: {lead}"
        );
        assert_eq!(
            card_body.lead,
            vec![crate::feishu::card::session::WAITING_BACKGROUND_CHIP.to_string()],
            "the body carries the chip without an update promise"
        );
        assert!(
            identity.contains(FOLLOW_HINT),
            "the follow promise is on the identity line: {identity}"
        );
    }
}

/// One Busy read with a Compaction marker renders the SAME body through both
/// drivers (#999/#1002): the marker keeps its server position and the card and
/// the follow lead agree on every field. The follow's promise is in its identity
/// line, not the shared body.
#[test]
fn the_parity_keeps_the_compaction_marker_on_both_drivers() {
    let read = parity_read(
        crate::backend::types::SessionStatus::Busy,
        false,
        vec![
            body_tail_message(MessageRole::User, 1_000, "问题一"),
            body_compaction(1_500, "保留了结论。"),
            body_tail_message(MessageRole::Assistant, 2_000, "回答一"),
        ],
    );
    let card = crate::feishu::card::session::build_idle_card(&read);
    let lead = crate::bridge::external::snapshot_follow_lead("接管", "唯一外部标题", &read, &[], true);

    let card_body = idle_card_body(&card);
    let lead_body = follow_lead_body(&lead);
    assert_eq!(
        card_body.markers,
        vec![("已压缩上下文（自动）".to_string(), "保留了结论。".to_string())],
        "the card keeps the marker at its server position: {card}"
    );
    assert_eq!(
        lead_body.markers, card_body.markers,
        "the lead keeps the same marker at the same position: {lead}"
    );
    assert_eq!(card_body, lead_body, "same body through both drivers: {lead}");
    assert_eq!(
        card_body.lead,
        vec![crate::feishu::card::session::BUSY_CHIP.to_string()],
        "the shared body carries no update promise"
    );
    let identity = lead.split_once("\n\n").expect("identity, then body").0;
    assert!(
        identity.contains(FOLLOW_HINT),
        "the follow promise is on the identity line: {identity}"
    );
}

/// The parity holds for a body that is not one tidy line: a multi-line message
/// (blank interior line included) compared entry-by-entry in FULL, and a
/// whitespace-only body rendered as the same `（空消息）` on both sides — the two
/// presentations share one emptiness predicate.
#[test]
fn the_parity_holds_for_multi_line_and_whitespace_only_bodies() {
    let read = parity_read(
        crate::backend::types::SessionStatus::Idle,
        false,
        vec![
            body_tail_message(MessageRole::User, 1_000, "第一行\n第二行\n\n第四行"),
            body_tail_message(MessageRole::Assistant, 2_000, "   "),
            body_tail_message(MessageRole::User, 3_000, "结尾"),
        ],
    );
    let card = crate::feishu::card::session::build_idle_card(&read);
    let lead = crate::bridge::external::snapshot_follow_lead("接管", "唯一外部标题", &read, &[], true);

    let card_body = idle_card_body(&card);
    let lead_body = follow_lead_body(&lead);
    assert_eq!(card_body.lead, lead_body.lead, "same status line: {card}");
    assert_eq!(
        card_body.messages, lead_body.messages,
        "same full bodies, interior line breaks included: {card}"
    );
    assert_eq!(
        card_body.messages[0].1, "第一行\n第二行\n\n第四行",
        "the multi-line body survives whole"
    );
    assert_eq!(
        card_body.messages[1].1, "（空消息）",
        "a whitespace-only body is the empty-message line, not a blank panel"
    );
}

/// The parity holds for the body's three parts: a pending read renders the same
/// status chip and count on both drivers, but the one-shot read carries no block
/// at all — its pointer is text and names where the request lives (「见原卡片」),
/// while the follow seeds the block and is hint-free.
#[test]
fn a_pending_read_renders_the_same_status_and_pointer_on_both_drivers() {
    let mut read = parity_read(
        crate::backend::types::SessionStatus::Busy,
        false,
        vec![body_tail_message(MessageRole::User, 1_000, "问题一")],
    );
    let pending = crate::bridge::request::kind::PendingRequest::Permission(perm_request(
        "per_1",
        "ses_alpha01",
        "ls -la",
    ));
    read.pending = vec![pending.clone()];
    let card = crate::feishu::card::session::build_idle_card(&read);
    let seeds = [&pending];
    let lead = crate::bridge::external::snapshot_follow_lead("接管", "唯一外部标题", &read, &seeds, true);

    // The read carries the block nowhere, so its pointer names the card that
    // does; the follow seeds the block it counts, so its pointer is hint-free.
    let card_body = idle_card_body(&card);
    let lead_body = follow_lead_body(&lead);
    assert_eq!(card_body.lead[0], crate::feishu::card::session::WAITING_CHIP);
    assert_eq!(lead_body.lead[0], crate::feishu::card::session::WAITING_CHIP);
    assert_eq!(card_body.lead[1], "有 1 个请求等待你处理（见原卡片）");
    assert_eq!(lead_body.lead[1], "有 1 个请求等待你处理");
    assert_eq!(card_body.messages, lead_body.messages);
    assert!(
        !card.to_string().contains("\"tag\":\"button\""),
        "the read hosts no block: {card}"
    );

    // #1005's filter: a request another card already hosts is not seeded here,
    // so this card carries none — but the READ still waits on the Host, so
    // the status line keeps 等待你的确认 and only the pointer goes away.
    let unseeded = crate::bridge::external::snapshot_follow_lead("接管", "唯一外部标题", &read, &[], true);
    let unseeded_body = follow_lead_body(&unseeded);
    assert_eq!(
        unseeded_body.lead,
        vec![crate::feishu::card::session::WAITING_CHIP.to_string()],
        "the status comes from the read, not the seed set: {unseeded}"
    );
}

/// A terminal card that still owes a queued continuation is the chain's live
/// anchor: replacing it would discard the split queue, its reply target and the
/// content it still owes (a Wake split whose continuation create failed). A
/// `/card` read must leave it current and stamp its own read taken over; once
/// the continuation is served the card is supersedable again.
#[tokio::test]
async fn a_card_read_never_drops_an_owed_continuation() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let (app, platform) = build_app(cfg, idle_read_backend()).await;
    seed_session(&app, "ses_test", "/work").await;
    seed_done_card_owing_continuation(&app, &platform).await;

    // `/card` may not replace that card.
    platform.given_reply_id("om_read");
    app.handle_message(incoming(
        "msg_card".into(),
        "chat_1".into(),
        "p2p".into(),
        None,
        "/card".into(),
        None,
    ))
    .await;

    let cards = app.cards_handle();
    assert_eq!(
        Turn::card_message_id(&cards, "ses_test").await.as_deref(),
        Some("om_live"),
        "a card that owes a continuation stays current"
    );
    let calls = platform.calls.lock().await.clone();
    let read = calls
        .iter()
        .rev()
        .find_map(|c| match c {
            PlatformCall::UpdateMessage { message_id, card } if message_id == "om_read" => Some(card.clone()),
            _ => None,
        })
        .expect("the posted read is stamped taken over");
    assert_eq!(
        read["header"]["title"]["content"], "⏳ 已由新卡片接管 · 已停止更新",
        "the posted read cannot look current"
    );

    // The queue and its reply target survived: the next flush serves the owed
    // continuation to the Wake's message.
    platform.given_reply_id("om_cont");
    Turn::flush_card(&cards, "ses_test").await;
    let (reply_to, _) = continuation(&platform).await;
    assert_eq!(
        reply_to, "msg_pull",
        "the owed continuation still replies to the split's own target"
    );
    assert_eq!(
        Turn::card_message_id(&cards, "ses_test").await.as_deref(),
        Some("om_cont"),
        "the continuation is the chain's newest card"
    );

    // Once the continuation is served and the run settles, `/card` replaces
    // the chain's terminal card normally.
    Turn::set_card_state(&cards, "ses_test", CardState::Done).await;
    platform.given_reply_id("om_read_2");
    app.handle_message(incoming(
        "msg_card".into(),
        "chat_1".into(),
        "p2p".into(),
        None,
        "/card".into(),
        None,
    ))
    .await;
    assert_eq!(
        Turn::card_message_id(&cards, "ses_test").await.as_deref(),
        Some("om_read_2"),
        "a served continuation no longer blocks the read"
    );
}

/// The busy-follow arm is also a compare-and-swap (ADR-0100): a chain that
/// still owes a continuation keeps the Session even when the read says busy, so
/// the queued split and its reply target survive. The follow's card is stamped
/// taken over and no Chain Record is written.
#[tokio::test]
async fn a_busy_follow_never_drops_an_owed_continuation() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let mut backend = idle_read_backend();
    backend.with_session_status("ses_test", Some(crate::backend::types::SessionStatus::Busy));
    let (app, platform) = build_app(cfg, backend).await;
    seed_session(&app, "ses_test", "/work").await;
    seed_done_card_owing_continuation(&app, &platform).await;

    platform.given_reply_id("om_read");
    app.handle_message(incoming(
        "msg_card".into(),
        "chat_1".into(),
        "p2p".into(),
        None,
        "/card".into(),
        None,
    ))
    .await;

    let cards = app.cards_handle();
    assert_eq!(
        Turn::card_message_id(&cards, "ses_test").await.as_deref(),
        Some("om_live"),
        "the follow arm refuses to replace a card that owes a continuation"
    );
    assert!(
        Turn::has_pending_split(&cards, "ses_test").await,
        "the queued split and its reply target survive: {:?}",
        Turn::pending_splits(&cards, "ses_test").await
    );
    assert!(
        cards.chains.get("ses_test").is_none(),
        "a refused arm writes no Chain Record"
    );
    let calls = platform.calls.lock().await.clone();
    let read = calls
        .iter()
        .rev()
        .find_map(|c| match c {
            PlatformCall::UpdateMessage { message_id, card } if message_id == "om_read" => Some(card.clone()),
            _ => None,
        })
        .expect("the posted read is stamped taken over");
    assert_eq!(
        read["header"]["title"]["content"], "⏳ 已由新卡片接管 · 已停止更新",
        "the follow's read never looks current"
    );
}
