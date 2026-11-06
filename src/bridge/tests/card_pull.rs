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
use crate::bridge::test_support::*;
use crate::feishu::card::CardState;

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
        !text.contains("超出单卡容量"),
        "the follow card renders no overflow notice: {text}"
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
    assert!(
        card_text(&card).contains("有 1 个请求等待你处理"),
        "the pointer names the seeded request: {}",
        card_text(&card)
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
    assert!(
        !card_text(&cards_now[0]).contains("有 1 个请求等待你处理"),
        "the follow card does not count a request another card hosts: {}",
        card_text(&cards_now[0])
    );
    assert!(
        card_text(&cards_now[0]).contains("⏳ 等待你的确认"),
        "the status still names the wait the read saw: {}",
        card_text(&cards_now[0])
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
/// publishes no card: the send landed as a static read, but the map insert and
/// the Chain Record never happen. The commit is parked on the cards map so the
/// abort lands in that exact window.
#[tokio::test]
async fn a_card_read_cancelled_inside_the_commit_publishes_no_card() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let mut backend = idle_read_backend();
    backend.with_session_status("ses_test", Some(crate::backend::types::SessionStatus::Busy));
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

    assert_eq!(
        platform.replied_cards().await.len(),
        1,
        "the send landed as a static read: {:?}",
        platform.calls.lock().await
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
        "the follow lead names the session title",
        CardUpdates::Any,
        |card| card_text(card).contains("已读取 我的会话"),
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
    let (app, platform) = build_app(cfg, backend).await;
    seed_session(&app, "ses_test", "/work").await;
    // A follow clears a pre-run `/stop` marker; the idle read path does not.
    // Its absence proves this command took the follow branch, not just that a
    // running card kept the map.
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
        !app.stopped_sessions.lock().await.contains("ses_test"),
        "the follow branch cleared the pre-run stop marker, even though its arm lost"
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
        .expect("the posted follow read is stamped taken over");
    assert!(
        card_text(&stamped).contains("⏳ 已由新卡片接管 · 已停止更新"),
        "the read that lost the race is not left unmarked: {}",
        card_text(&stamped)
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
        "the read waits on the operator: {text}"
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

/// A Turn installs its accumulator through the card map only
/// (`install_fresh_session` does not take the session write lock), so it can
/// win while `/card`'s send is in flight. The read must not overwrite it: the
/// live Turn stays the Session's current card and the just-sent read is stamped
/// taken over so it never looks current.
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
