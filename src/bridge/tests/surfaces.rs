//! Restart re-adoption (ADR-0038): a `/restart` while a Permission/Question is
//! pending must not leave the request with two live surfaces. The interactive
//! surface state is persisted beside `sessions.json` and re-adopted by the new
//! process: the first sweep repaints the card the request already lives on
//! instead of posting a second one, and a click on that card resolves the
//! request and repaints it exactly as before the restart.

use std::sync::Arc;

use crate::bridge::test_support::*;
use crate::feishu::card::CardState;

/// A two-question request the question flow can replay and reply to.
fn question_request(request_id: &str, session_id: &str) -> crate::backend::types::QuestionRequest {
    use crate::backend::types::{QuestionInfo, QuestionOption, QuestionRequest};
    let question = |text: &str, header: &str, label: &str| QuestionInfo {
        question: text.into(),
        header: header.into(),
        options: vec![QuestionOption {
            label: label.into(),
            description: String::new(),
            ..Default::default()
        }],
        kind: crate::backend::types::FormFieldKind::String,
        custom: None,
        ..Default::default()
    };
    QuestionRequest {
        id: request_id.into(),
        session_id: session_id.into(),
        questions: vec![
            question("选目录", "目录", "/a"),
            question("选分支", "分支", "main"),
        ],
        ..Default::default()
    }
}

/// Seed the live streaming card a Turn would have: an accumulator with a card
/// message id, finished content and a reply target.
async fn seed_live_card(app: &Arc<App>, session_id: &str, message_id: &str) {
    let cards = app.cards_handle();
    Turn::seed_card(&cards, session_id, Some(message_id)).await;
    Turn::set_card_state(&cards, session_id, CardState::Done).await;
    Turn::push_text(&cards, session_id, "回合的内容。").await;
    Turn::set_reply_target(&cards, session_id, "msg_1").await;
}

/// One deterministic sweep (tests drive the poll loop's pass directly).
async fn sweep(flow: &crate::bridge::request::flow::RequestFlow, app: &Arc<App>) {
    let mut seen = std::collections::HashSet::new();
    flow.sweep(&app.flow_handles(), &mut seen).await;
}

/// Build the restarted process on the same store: fresh flows, fresh
/// registries, the same pending request served.
async fn restart_app(
    session_file: &std::path::Path,
    configure: impl FnOnce(&mut MockBackend),
) -> (Arc<App>, Arc<RecordingPlatform>, Arc<MockBackend>) {
    let mut backend = MockBackend::new(realistic_parts());
    configure(&mut backend);
    let backend = Arc::new(backend);
    let platform = Arc::new(RecordingPlatform::new());
    let app = Arc::new(
        App::new(test_config(session_file), backend.clone(), platform.clone())
            .expect("the restarted app builds"),
    );
    (app, platform, backend)
}

/// Every card posted as a NEW message (send or reply) — the duplicate the
/// restart must not create.
async fn posted_cards(platform: &RecordingPlatform) -> Vec<serde_json::Value> {
    platform
        .calls
        .lock()
        .await
        .iter()
        .filter_map(|c| match c {
            PlatformCall::SendCard { card, .. } | PlatformCall::ReplyCard { card, .. } => Some(card.clone()),
            _ => None,
        })
        .collect()
}

/// The latest card the platform patched onto `message_id`.
async fn last_update_of(platform: &RecordingPlatform, message_id: &str) -> Option<serde_json::Value> {
    platform.calls.lock().await.iter().rev().find_map(|c| match c {
        PlatformCall::UpdateMessage {
            message_id: mid,
            card,
        } if mid == message_id => Some(card.clone()),
        _ => None,
    })
}

/// The persisted surface record's raw JSON, or `None` when the file is gone.
fn persisted(session_file: &std::path::Path) -> Option<String> {
    std::fs::read_to_string(crate::state::StateLayout::beside(
        session_file,
        "interactive_surfaces.json",
    ))
    .ok()
}

/// A pending permission inlined on the live card survives the restart on that
/// card: no second card is posted, the original is repainted, and a click on it
/// resolves the request and leaves the receipt in place.
#[tokio::test]
async fn restart_readopts_a_pending_inline_permission() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let session_file = dir.path().join("sessions.json");

    // The pre-restart process: the permission inlined on the live card.
    let mut backend = MockBackend::new(realistic_parts());
    backend.ask_permission(perm_request("per_1", "ses_1", "ls -la"));
    let backend = Arc::new(backend);
    let platform = Arc::new(RecordingPlatform::new());
    let app = Arc::new(App::new(test_config(&session_file), backend.clone(), platform.clone()).unwrap());
    seed_session(&app, "ses_1", "/work").await;
    seed_live_card(&app, "ses_1", "om_live").await;
    sweep(&app.permission, &app).await;
    assert_eq!(
        app.card_handles.lock().await.message_of("per_1"),
        Some("om_live"),
        "precondition: the live card carries the block"
    );
    let raw = persisted(&session_file).expect("the surface is persisted");
    assert!(
        raw.contains("per_1") && raw.contains("om_live"),
        "the block and its card are in the record: {raw}"
    );

    // The restarted process: same store, same pending request, no accumulators.
    let (app, platform, backend) = restart_app(&session_file, |b| {
        b.ask_permission(perm_request("per_1", "ses_1", "ls -la"));
    })
    .await;
    assert!(
        app.cards.lock().await.is_empty(),
        "no accumulator survives the restart"
    );
    assert_eq!(
        app.card_handles.lock().await.message_of("per_1"),
        Some("om_live"),
        "the block registry is hydrated from the record"
    );

    sweep(&app.permission, &app).await;

    assert!(
        posted_cards(&platform).await.is_empty(),
        "no second card may be posted after the restart"
    );
    let repaint = last_update_of(&platform, "om_live")
        .await
        .expect("the original card is repainted");
    let text = card_text(&repaint);
    assert!(
        text.contains("🔐 **权限请求**") && text.contains("允许一次"),
        "the repaint keeps the live controls: {text}"
    );
    assert!(
        text.contains("回合的内容。"),
        "the repaint keeps the card's streamed content: {text}"
    );

    // A click on the re-adopted card resolves the request and repaints it.
    let result = app
        .host_action(serde_json::json!({
            "action": "perm",
            "reply": "once",
            "session_id": "ses_1",
            "directory": "/work",
            "request_id": "per_1",
            "open_message_id": "om_live",
            "perm_label": "✅ 已允许一次",
            "perm_color": "green",
            "perm_body": "bash",
        }))
        .await
        .expect("a card-action result");
    let ack = card_text(result.card.as_ref().expect("the ack carries the card"));
    assert!(
        ack.contains("✅ 已允许一次：⚡ 执行 Shell 命令 `ls -la`"),
        "the receipt lands on the re-adopted card: {ack}"
    );
    assert!(
        !ack.contains("🔐 **权限请求**"),
        "the resolved block (and its controls) is gone: {ack}"
    );
    assert!(
        ack.contains("回合的内容。"),
        "the ack is the re-adopted card's own JSON: {ack}"
    );
    assert_eq!(backend.reply_permission_calls.lock().await.len(), 1);
    assert_eq!(app.card_handles.lock().await.live_count(), 0);
    assert_eq!(
        persisted(&session_file),
        None,
        "a resolved surface leaves no record behind"
    );

    // A restart after the resolution has nothing to re-adopt or resurface.
    let (app, platform, _backend) = restart_app(&session_file, |_| {}).await;
    assert!(
        app.card_handles.lock().await.message_of("per_1").is_none(),
        "the resolved block is not hydrated back"
    );
    sweep(&app.permission, &app).await;
    assert!(posted_cards(&platform).await.is_empty());
    assert_eq!(app.card_handles.lock().await.live_count(), 0);
}

/// A pending question inlined on the live card re-adopts the same way: the
/// repaint is re-rendered from this process's state, a partial answer refreshes
/// the card in place, and the final answer leaves the receipt on it.
#[tokio::test]
async fn restart_readopts_a_pending_inline_question() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let session_file = dir.path().join("sessions.json");

    let mut backend = MockBackend::new(realistic_parts());
    backend.ask_question(question_request("que_1", "ses_1"));
    let backend = Arc::new(backend);
    let platform = Arc::new(RecordingPlatform::new());
    let app = Arc::new(App::new(test_config(&session_file), backend.clone(), platform.clone()).unwrap());
    seed_session(&app, "ses_1", "/work").await;
    seed_live_card(&app, "ses_1", "om_live").await;
    sweep(&app.question, &app).await;
    assert_eq!(app.card_handles.lock().await.message_of("que_1"), Some("om_live"));
    assert!(persisted(&session_file).is_some());

    let (app, platform, backend) = restart_app(&session_file, |b| {
        b.ask_question(question_request("que_1", "ses_1"));
    })
    .await;

    sweep(&app.question, &app).await;

    assert!(
        posted_cards(&platform).await.is_empty(),
        "no second card may be posted after the restart"
    );
    let repaint = last_update_of(&platform, "om_live")
        .await
        .expect("the original card is repainted");
    let text = card_text(&repaint);
    assert!(
        text.contains("选目录") && text.contains("选分支"),
        "the repaint carries the question's controls: {text}"
    );
    assert!(
        text.contains("回合的内容。"),
        "the repaint keeps the card's streamed content: {text}"
    );

    // A partial answer refreshes the re-adopted card in place (the clicked card
    // is the inline surface, even though its accumulator is gone).
    let r1 = app
        .host_action(serde_json::json!({
            "action": "question",
            "reply": "answer",
            "request_id": "que_1",
            "session_id": "ses_1",
            "directory": "/work",
            "question_index": 0,
            "answer": "/a",
            "open_message_id": "om_live",
        }))
        .await
        .expect("a card-action result");
    let ack1 = card_text(r1.card.as_ref().expect("the ack carries the card"));
    assert!(ack1.contains("已选：/a"), "marker missing: {ack1}");
    assert!(
        ack1.contains("回合的内容。"),
        "the ack is the re-adopted card, refreshed: {ack1}"
    );

    // The final answer submits: the receipt lands on the same card.
    let r2 = app
        .host_action(serde_json::json!({
            "action": "question",
            "reply": "answer",
            "request_id": "que_1",
            "session_id": "ses_1",
            "directory": "/work",
            "question_index": 1,
            "answer": "main",
            "open_message_id": "om_live",
        }))
        .await
        .expect("a card-action result");
    let ack2 = card_text(r2.card.as_ref().expect("the ack carries the card"));
    assert!(
        ack2.contains("✅ 已回答：目录 /a、分支 main"),
        "receipt missing: {ack2}"
    );
    assert!(
        !ack2.contains("已选："),
        "the resolved block (and its controls) is gone: {ack2}"
    );
    assert_eq!(backend.reply_question_calls.lock().await.len(), 1);
    assert_eq!(app.card_handles.lock().await.live_count(), 0);
    assert_eq!(persisted(&session_file), None);
}

/// A pending request with no live card was surfaced as a standalone card
/// before the restart: the new process re-adopts that card — nothing new is
/// posted — and a click on it resolves the request.
#[tokio::test]
async fn restart_readopts_a_pending_standalone_permission() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let session_file = dir.path().join("sessions.json");

    // No accumulator: the pre-restart process posts a standalone card.
    let mut backend = MockBackend::new(realistic_parts());
    backend.ask_permission(perm_request("per_1", "ses_1", "ls -la"));
    let backend = Arc::new(backend);
    let platform = Arc::new(RecordingPlatform::new());
    let app = Arc::new(App::new(test_config(&session_file), backend.clone(), platform.clone()).unwrap());
    seed_session(&app, "ses_1", "/work").await;
    sweep(&app.permission, &app).await;
    assert_eq!(
        posted_cards(&platform).await.len(),
        1,
        "the first process posts the standalone card"
    );
    let sent = app.permission.sent_cards.lock().await.get("per_1").cloned();
    let sent = sent.expect("the standalone card is recorded");
    let raw = persisted(&session_file).expect("the standalone surface is persisted");
    assert!(
        raw.contains("per_1") && raw.contains(&sent.message_id),
        "the standalone record names its card: {raw}"
    );

    let (app, platform, backend) = restart_app(&session_file, |b| {
        b.ask_permission(perm_request("per_1", "ses_1", "ls -la"));
    })
    .await;
    assert!(
        app.permission.sent_cards.lock().await.contains_key("per_1"),
        "the standalone record is hydrated"
    );

    sweep(&app.permission, &app).await;

    assert!(
        posted_cards(&platform).await.is_empty(),
        "no second card may be posted after the restart"
    );
    assert!(
        last_update_of(&platform, &sent.message_id).await.is_none(),
        "a still-pending standalone card is not repainted stale"
    );

    // A click on the re-adopted standalone card resolves the request.
    let result = app
        .host_action(serde_json::json!({
            "action": "perm",
            "reply": "once",
            "session_id": "ses_1",
            "directory": "/work",
            "request_id": "per_1",
            "open_message_id": sent.message_id,
            "perm_label": "✅ 已允许一次",
            "perm_color": "green",
            "perm_body": "bash",
        }))
        .await
        .expect("a card-action result");
    let ack = card_text(result.card.as_ref().expect("the ack carries the card"));
    assert!(ack.contains("✅ 已允许一次"), "receipt missing: {ack}");
    assert!(
        last_update_of(&platform, &sent.message_id).await.is_none(),
        "the ack IS the clicked card's update; no PATCH may race behind it"
    );
    assert_eq!(backend.reply_permission_calls.lock().await.len(), 1);
    assert_eq!(persisted(&session_file), None);
}

/// The same for a standalone question card: the re-adopted card is the live
/// surface (nothing new posted), a partial answer rebuilds it in the ack, and
/// the final answer resolves the request through it.
#[tokio::test]
async fn restart_readopts_a_pending_standalone_question() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let session_file = dir.path().join("sessions.json");

    let mut backend = MockBackend::new(realistic_parts());
    backend.ask_question(question_request("que_1", "ses_1"));
    let backend = Arc::new(backend);
    let platform = Arc::new(RecordingPlatform::new());
    let app = Arc::new(App::new(test_config(&session_file), backend.clone(), platform.clone()).unwrap());
    seed_session(&app, "ses_1", "/work").await;
    sweep(&app.question, &app).await;
    assert_eq!(posted_cards(&platform).await.len(), 1);
    let sent = app
        .question
        .sent_cards
        .lock()
        .await
        .get("que_1")
        .cloned()
        .expect("the standalone card is recorded");

    let (app, platform, backend) = restart_app(&session_file, |b| {
        b.ask_question(question_request("que_1", "ses_1"));
    })
    .await;
    assert!(
        app.question.sent_cards.lock().await.contains_key("que_1"),
        "the standalone record is hydrated"
    );

    sweep(&app.question, &app).await;
    assert!(
        posted_cards(&platform).await.is_empty(),
        "no second card may be posted after the restart"
    );

    // A partial answer rebuilds the standalone card in the ack (no accumulator
    // and no block handle exist — the ack IS the card's update).
    let r1 = app
        .host_action(serde_json::json!({
            "action": "question",
            "reply": "answer",
            "request_id": "que_1",
            "session_id": "ses_1",
            "directory": "/work",
            "question_index": 0,
            "answer": "/a",
            "open_message_id": sent.message_id,
        }))
        .await
        .expect("a card-action result");
    let ack1 = card_text(r1.card.as_ref().expect("the ack carries the card"));
    assert!(ack1.contains("已选：/a"), "marker missing: {ack1}");

    // The final answer resolves the request through the re-adopted card.
    let r2 = app
        .host_action(serde_json::json!({
            "action": "question",
            "reply": "answer",
            "request_id": "que_1",
            "session_id": "ses_1",
            "directory": "/work",
            "question_index": 1,
            "answer": "main",
            "open_message_id": sent.message_id,
        }))
        .await
        .expect("a card-action result");
    let ack2 = card_text(r2.card.as_ref().expect("the ack carries the card"));
    assert!(ack2.contains("✅ 已回答"), "completion card missing: {ack2}");
    let replies = backend.reply_question_calls.lock().await;
    assert_eq!(replies.len(), 1);
    assert_eq!(
        replies[0].1,
        vec![vec!["/a".to_string()], vec!["main".to_string()]]
    );
    drop(replies);
    assert_eq!(persisted(&session_file), None);
}

/// With no persisted state (a fresh machine, or a pruned record) the behavior
/// is unchanged: the pending request is surfaced as a standalone card.
#[tokio::test]
async fn a_fresh_app_surfaces_a_pending_request_standalone() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let session_file = dir.path().join("sessions.json");
    assert!(
        persisted(&session_file).is_none(),
        "precondition: no persisted state"
    );

    let mut backend = MockBackend::new(realistic_parts());
    backend.ask_permission(perm_request("per_1", "ses_1", "ls -la"));
    let backend = Arc::new(backend);
    let platform = Arc::new(RecordingPlatform::new());
    let app = Arc::new(App::new(test_config(&session_file), backend.clone(), platform.clone()).unwrap());
    seed_session(&app, "ses_1", "/work").await;

    sweep(&app.permission, &app).await;

    let posted = posted_cards(&platform).await;
    assert_eq!(
        posted.len(),
        1,
        "with no persisted state the request is surfaced as a standalone card"
    );
    assert!(card_text(&posted[0]).contains("🔐 权限请求"));
    assert_eq!(
        app.card_handles.lock().await.live_count(),
        0,
        "nothing was re-adopted as an inline block"
    );
    let raw = persisted(&session_file).expect("the standalone surface is persisted");
    assert!(raw.contains("per_1"), "{raw}");
}

/// A request resolved while cola was down is reconciled by the first sweep —
/// the persisted inline block becomes its neutral receipt and no card is
/// resurrected.
#[tokio::test]
async fn restart_reconciles_an_inline_block_resolved_while_down() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let session_file = dir.path().join("sessions.json");

    let mut backend = MockBackend::new(realistic_parts());
    backend.ask_permission(perm_request("per_1", "ses_1", "ls -la"));
    let backend = Arc::new(backend);
    let platform = Arc::new(RecordingPlatform::new());
    let app = Arc::new(App::new(test_config(&session_file), backend.clone(), platform.clone()).unwrap());
    seed_session(&app, "ses_1", "/work").await;
    seed_live_card(&app, "ses_1", "om_live").await;
    sweep(&app.permission, &app).await;
    assert!(persisted(&session_file).is_some());

    // The restarted process finds the request already resolved while the run
    // is still live (another client answered it).
    let (app, platform, _backend) = restart_app(&session_file, |backend| {
        backend.with_session_status("ses_1", Some(crate::backend::types::SessionStatus::Busy));
    })
    .await;
    sweep(&app.permission, &app).await;

    assert!(
        posted_cards(&platform).await.is_empty(),
        "a resolved request is never resurrected as a card"
    );
    let repaint = last_update_of(&platform, "om_live")
        .await
        .expect("the stale block is repainted on its card");
    let text = card_text(&repaint);
    assert!(
        text.contains("⏱ 已由其他客户端处理：⚡ 执行 Shell 命令 `ls -la`"),
        "neutral receipt missing: {text}"
    );
    assert!(
        !text.contains("🔐 **权限请求**"),
        "the resolved block is gone: {text}"
    );
    assert_eq!(
        persisted(&session_file),
        None,
        "the reconciled surface leaves no record"
    );
}

/// A standalone card whose request was resolved while cola was down is marked
/// stale by the first sweep, exactly as if the process had been running.
#[tokio::test]
async fn restart_marks_a_standalone_card_stale_when_resolved_while_down() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let session_file = dir.path().join("sessions.json");

    let mut backend = MockBackend::new(realistic_parts());
    backend.ask_permission(perm_request("per_1", "ses_1", "ls -la"));
    let backend = Arc::new(backend);
    let platform = Arc::new(RecordingPlatform::new());
    let app = Arc::new(App::new(test_config(&session_file), backend.clone(), platform.clone()).unwrap());
    seed_session(&app, "ses_1", "/work").await;
    sweep(&app.permission, &app).await;
    let sent = app
        .permission
        .sent_cards
        .lock()
        .await
        .get("per_1")
        .cloned()
        .unwrap();

    // Resolved while cola was down by another client, and the run is still
    // live: the neutral copy.
    let (app, platform, _backend) = restart_app(&session_file, |backend| {
        backend.with_session_status("ses_1", Some(crate::backend::types::SessionStatus::Busy));
    })
    .await;
    sweep(&app.permission, &app).await;

    assert!(posted_cards(&platform).await.is_empty());
    let stale = last_update_of(&platform, &sent.message_id)
        .await
        .expect("the standalone card is marked stale");
    let text = card_text(&stale);
    assert!(
        text.contains("已在其他端处理") && text.contains("ls -la"),
        "stale card missing: {text}"
    );
    assert!(
        !app.permission.sent_cards.lock().await.contains_key("per_1"),
        "the stale record is dropped"
    );
    assert_eq!(persisted(&session_file), None);
}

/// A standalone card whose request died with its session's run (nobody
/// answered it — an interruption, an eviction, a restart) is marked with the
/// interrupted copy: the old "已在其他端处理" line would claim a decision
/// nobody made.
#[tokio::test]
async fn a_vanished_standalone_card_reads_the_interrupted_copy_when_the_run_is_gone() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    // MockBackend's absent status entry reads Idle: the run is over.
    let (app, platform) = build_app(cfg, MockBackend::new(realistic_parts())).await;
    seed_session(&app, "ses_1", "/work").await;
    app.permission.sent_cards.lock().await.insert(
        "per_1".into(),
        crate::bridge::request::flow::SentCard {
            message_id: "om_perm".into(),
            summary: "⚡ 执行 Shell 命令 `ls -la`".into(),
            directory: "/work".into(),
            session_id: "ses_1".into(),
        },
    );

    sweep(&app.permission, &app).await;

    let stale = last_update_of(&platform, "om_perm")
        .await
        .expect("the standalone card is marked stale");
    let text = card_text(&stale);
    assert!(
        text.contains("已随会话中断") && text.contains("ls -la"),
        "interrupted copy missing: {text}"
    );
    assert!(
        !text.contains("已在其他端处理"),
        "an idle run must not claim another client handled it: {text}"
    );
}

/// A standalone record written before the owning session was persisted
/// (empty id) is UNKNOWN: the stale copy keeps the neutral line, never
/// guessing an interruption.
#[tokio::test]
async fn a_legacy_standalone_card_without_an_owner_keeps_the_neutral_copy() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let (app, platform) = build_app(cfg, MockBackend::new(realistic_parts())).await;
    seed_session(&app, "ses_1", "/work").await;
    app.permission.sent_cards.lock().await.insert(
        "per_1".into(),
        crate::bridge::request::flow::SentCard {
            message_id: "om_perm".into(),
            summary: "⚡ 执行 Shell 命令 `ls -la`".into(),
            directory: "/work".into(),
            // The pre-persistence record shape: no owner to classify.
            session_id: String::new(),
        },
    );

    sweep(&app.permission, &app).await;

    let stale = last_update_of(&platform, "om_perm")
        .await
        .expect("the standalone card is marked stale");
    let text = card_text(&stale);
    assert!(
        text.contains("已在其他端处理"),
        "an unknown owner keeps the neutral copy: {text}"
    );
    assert!(
        !text.contains("已随会话中断"),
        "an unknown owner is never read as an interruption: {text}"
    );
}

/// ADR-0054: each sweep records which sessions hold a pending request of its
/// kind, so the task liveness line reads a child's wait without listing pending
/// requests per render poll. A failed list is unknown, never resolved.
#[tokio::test]
async fn sweep_records_pending_sessions_for_liveness() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let session_file = dir.path().join("sessions.json");

    let mut backend = MockBackend::new(realistic_parts());
    backend.ask_permission(perm_request("per_1", "ses_child", "ls -la"));
    backend.ask_question(question_request("q_1", "ses_child"));
    let backend = Arc::new(backend);
    let platform = Arc::new(RecordingPlatform::new());
    let app = Arc::new(App::new(test_config(&session_file), backend.clone(), platform.clone()).unwrap());
    seed_session(&app, "ses_1", "/work").await;

    assert!(
        app.requests_handle().wait_for("ses_child").await.is_none(),
        "nothing is pending before the first sweep"
    );
    sweep(&app.permission, &app).await;
    assert!(
        app.permission.is_pending_for("ses_child").await,
        "the sweep records the waiting child session"
    );
    sweep(&app.question, &app).await;
    assert_eq!(
        app.requests_handle().wait_for("ses_child").await,
        Some(crate::feishu::card::AwaitingAction::Both),
        "both kinds pending combine into the header's awaiting vocabulary"
    );

    // A failed list says nothing: the record must survive it (#130).
    app.permission
        .list_timeout_ms
        .store(10, std::sync::atomic::Ordering::SeqCst);
    backend
        .hang_list_permissions
        .store(1, std::sync::atomic::Ordering::SeqCst);
    sweep(&app.permission, &app).await;
    assert!(
        app.permission.is_pending_for("ses_child").await,
        "a failed sweep leaves the record alone"
    );

    // Resolved elsewhere: the next complete sweep drops the kind's record.
    backend.permission_resolved_by_another("per_1").await;
    sweep(&app.permission, &app).await;
    assert_eq!(
        app.requests_handle().wait_for("ses_child").await,
        Some(crate::feishu::card::AwaitingAction::Question),
        "the resolved permission leaves only the question"
    );
    backend.question_resolved_by_another("q_1").await;
    sweep(&app.question, &app).await;
    assert!(
        app.requests_handle().wait_for("ses_child").await.is_none(),
        "a complete sweep with no pending request clears the record"
    );
}

/// The over-budget request an older build could have surfaced: its command is
/// past the card's display budget, so the current process must decline it
/// rather than re-adopt the persisted card.
fn over_budget_perm(request_id: &str, session_id: &str) -> crate::backend::types::PermissionRequest {
    let command = format!(
        "echo {}",
        "x".repeat(crate::backend::types::PERMISSION_COMMAND_MAX_CHARS)
    );
    perm_request(request_id, session_id, &command)
}

/// The persisted surface an older build left for `over_budget_perm` — the file
/// is written directly because this build refuses such a request before any
/// card exists, so it can no longer produce one. `inline` picks the shape: an
/// inline block on a streaming card, or a standalone request card.
fn seed_stale_surface(session_file: &std::path::Path, request_id: &str, inline: bool, message_id: &str) {
    use crate::bridge::snapshot_claims::ClaimKind;
    use crate::bridge::surfaces::{InlineSurface, StandaloneSurface, Surfaces};

    let surfaces = Surfaces::load(crate::state::StateLayout::beside(
        session_file,
        "interactive_surfaces.json",
    ));
    if inline {
        let card = serde_json::json!({
            "schema": "2.0",
            "body": { "elements": [
                { "tag": "markdown", "content": "🔐 **权限请求**\n⚡ **执行 Shell 命令**\n**对象**:\n- `echo …`" },
                { "tag": "button", "text": { "tag": "plain_text", "content": "✅ 允许一次" }, "value": { "action": "perm", "reply": "once" } },
            ]}
        });
        surfaces.set_card(
            message_id,
            &card,
            &[crate::bridge::card_handles::BlockSpan {
                request_id: request_id.into(),
                start: 0,
                end: 2,
            }],
        );
        surfaces.set_inline(
            request_id,
            InlineSurface {
                client_id: String::new(),
                message_id: message_id.into(),
                kind: ClaimKind::Permission,
                session_id: "ses_1".into(),
                directory: "/work".into(),
                target: "per_long".into(),
            },
        );
    } else {
        surfaces.set_standalone(
            request_id,
            StandaloneSurface {
                client_id: String::new(),
                kind: ClaimKind::Permission,
                message_id: message_id.into(),
                summary: "⚡ **执行 Shell 命令**\n**对象**:\n- `echo …`".into(),
                directory: "/work".into(),
                session_id: "ses_1".into(),
            },
        );
    }
}

/// Whole-branch re-review #883: an over-budget permission whose card an older
/// build persisted is **declined before readoption**, not re-adopted. The
/// persisted card is repainted to a non-interactive receipt in the same sweep,
/// no second card is posted, and the request is settled on the server.
#[tokio::test]
async fn restart_declines_an_over_budget_inline_permission_instead_of_readopting_it() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let session_file = dir.path().join("sessions.json");

    // The session mapping a pre-restart process persisted.
    let (app, _platform, _backend) = restart_app(&session_file, |_| {}).await;
    seed_session(&app, "ses_1", "/work").await;
    drop(app);

    // The card the older build left for the over-budget request.
    seed_stale_surface(&session_file, "per_long", true, "om_card");

    let (app, platform, backend) = restart_app(&session_file, |b| {
        b.ask_permission(over_budget_perm("per_long", "ses_1"));
    })
    .await;
    sweep(&app.permission, &app).await;

    assert_eq!(
        backend.reply_permission_calls.lock().await.as_slice(),
        &[("per_long".to_string(), "reject".to_string())],
        "the recovered request is declined, not offered"
    );
    let withdrawn = last_update_of(&platform, "om_card")
        .await
        .expect("the persisted card is withdrawn in the same sweep");
    let text = card_text(&withdrawn);
    assert!(
        text.contains(crate::feishu::card::question::UNSHOWABLE_PERMISSION_RECEIPT),
        "the block becomes the refusal receipt: {text}"
    );
    assert!(
        !text.contains("允许一次") && !text.contains("始终允许"),
        "no control survives on the withdrawn card: {text}"
    );
    assert!(
        posted_cards(&platform).await.is_empty(),
        "no second card may be posted for the request"
    );
    assert!(
        app.cards_handle()
            .card_handles
            .lock()
            .await
            .message_of("per_long")
            .is_none(),
        "the block handle is forgotten, so nothing re-adopts it later"
    );
    assert!(
        !persisted(&session_file).unwrap_or_default().contains("per_long"),
        "the persisted surface is gone"
    );
    assert!(
        !app.permission.sent_cards.lock().await.contains_key("per_long"),
        "and no standalone record was invented for it"
    );
}

/// The standalone half of the same rule: a request card an older build posted
/// for an over-budget request is repainted as the refusal card, forgotten, and
/// never clicked.
#[tokio::test]
async fn restart_declines_an_over_budget_standalone_permission_instead_of_readopting_it() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let session_file = dir.path().join("sessions.json");

    let (app, _platform, _backend) = restart_app(&session_file, |_| {}).await;
    seed_session(&app, "ses_1", "/work").await;
    drop(app);

    seed_stale_surface(&session_file, "per_long", false, "om_standalone");

    let (app, platform, backend) = restart_app(&session_file, |b| {
        b.ask_permission(over_budget_perm("per_long", "ses_1"));
    })
    .await;
    sweep(&app.permission, &app).await;

    assert_eq!(
        backend.reply_permission_calls.lock().await.as_slice(),
        &[("per_long".to_string(), "reject".to_string())],
        "the recovered request is declined, not offered"
    );
    let withdrawn = last_update_of(&platform, "om_standalone")
        .await
        .expect("the persisted standalone card is repainted");
    let text = card_text(&withdrawn);
    assert!(
        text.contains(crate::feishu::card::question::UNSHOWABLE_PERMISSION_HEADER),
        "the card carries the refusal copy: {text}"
    );
    assert!(
        !text.contains("允许一次"),
        "no control survives on the withdrawn card: {text}"
    );
    assert!(
        posted_cards(&platform).await.is_empty(),
        "no second card may be posted for the request"
    );
    assert!(
        !app.permission.sent_cards.lock().await.contains_key("per_long"),
        "the standalone record is forgotten"
    );
    assert!(
        !persisted(&session_file).unwrap_or_default().contains("per_long"),
        "the persisted surface is gone"
    );
}

/// Whole-branch re-review #883: a refusal that **fails** must still neutralize
/// the stored card — an over-budget card from an older build must never keep
/// its Allow buttons while the request is still pending — but with a "will
/// retry" receipt that claims nothing, and without marking the request seen so
/// the next sweep retries the decline and then explains it.
#[tokio::test]
async fn restart_neutralizes_a_persisted_card_when_the_refusal_fails_then_retries() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let session_file = dir.path().join("sessions.json");

    let (app, _platform, _backend) = restart_app(&session_file, |_| {}).await;
    seed_session(&app, "ses_1", "/work").await;
    drop(app);

    seed_stale_surface(&session_file, "per_long", true, "om_card");

    let (app, platform, backend) = restart_app(&session_file, |b| {
        b.ask_permission(over_budget_perm("per_long", "ses_1"));
        b.fail_reply_permission_count
            .store(1, std::sync::atomic::Ordering::SeqCst);
    })
    .await;

    // One persistent `seen`, like the poll loop: a failure must not retire the
    // request, or the retry below could never happen.
    let mut seen = std::collections::HashSet::new();
    app.permission.sweep(&app.flow_handles(), &mut seen).await;

    assert_eq!(
        backend.reply_permission_calls.lock().await.len(),
        1,
        "the first sweep attempts the decline"
    );
    let neutral = last_update_of(&platform, "om_card")
        .await
        .expect("the persisted card is neutralized even on failure");
    let text = card_text(&neutral);
    assert!(
        text.contains(crate::feishu::card::question::UNSHOWABLE_PERMISSION_RETRY_RECEIPT),
        "the block says it will retry: {text}"
    );
    assert!(
        !text.contains(crate::feishu::card::question::UNSHOWABLE_PERMISSION_RECEIPT),
        "it must not claim cola rejected it: {text}"
    );
    assert!(
        !text.contains("允许一次") && !text.contains("始终允许"),
        "no control survives while the refusal is unlanded: {text}"
    );
    assert!(
        posted_cards(&platform).await.is_empty(),
        "nothing is posted before the refusal lands: {text}"
    );

    // The retry lands; with the handle already forgotten, the reason card is
    // posted to the chat instead of a second withdrawal.
    app.permission.sweep(&app.flow_handles(), &mut seen).await;
    assert_eq!(
        backend.reply_permission_calls.lock().await.len(),
        2,
        "the second sweep retries the decline"
    );
    assert!(
        posted_cards(&platform)
            .await
            .iter()
            .any(|card| card_text(card)
                .contains(crate::feishu::card::question::UNSHOWABLE_PERMISSION_HEADER)),
        "the landed refusal is explained"
    );
}

/// Whole-branch re-review #883: `NotFound` on the decline means the server no
/// longer holds the request — another client settled it. The old card is
/// withdrawn **neutrally** (handled elsewhere), never with the auto-reject
/// receipt, and no reason card is posted.
#[tokio::test]
async fn restart_withdraws_a_persisted_card_neutrally_when_another_client_answered() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let session_file = dir.path().join("sessions.json");

    let (app, _platform, _backend) = restart_app(&session_file, |_| {}).await;
    seed_session(&app, "ses_1", "/work").await;
    drop(app);

    seed_stale_surface(&session_file, "per_long", true, "om_card");

    let (app, platform, backend) = restart_app(&session_file, |b| {
        b.ask_permission(over_budget_perm("per_long", "ses_1"));
        b.reply_permission_not_found = true;
    })
    .await;
    sweep(&app.permission, &app).await;

    assert_eq!(
        backend.reply_permission_calls.lock().await.len(),
        1,
        "cola asked the server to decline"
    );
    let withdrawn = last_update_of(&platform, "om_card")
        .await
        .expect("the persisted card is withdrawn");
    let text = card_text(&withdrawn);
    assert!(
        text.contains("已由其他客户端处理"),
        "the neutral handled-elsewhere receipt: {text}"
    );
    assert!(
        !text.contains("自动拒绝"),
        "cola must not claim it rejected what another client answered: {text}"
    );
    assert!(
        !text.contains("允许一次"),
        "no control survives on the withdrawn card: {text}"
    );
    assert!(
        posted_cards(&platform).await.is_empty(),
        "no reason card for a request cola did not refuse"
    );
    assert!(
        app.cards_handle()
            .card_handles
            .lock()
            .await
            .message_of("per_long")
            .is_none(),
        "the block handle is forgotten"
    );
}

/// Whole-branch re-review #883: a withdrawal whose card PATCH fails must keep
/// the handle and the persisted record — otherwise the old Allow card stays
/// clickable and cola has thrown away the only state that could neutralize it.
/// The next sweep redelivers the withdrawal (the request already left the
/// pending list, so this is the sweep-level retry) without double-posting or
/// leaving two receipts.
#[tokio::test]
async fn restart_retries_a_withdrawal_whose_card_update_failed() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let session_file = dir.path().join("sessions.json");

    let (app, _platform, _backend) = restart_app(&session_file, |_| {}).await;
    seed_session(&app, "ses_1", "/work").await;
    drop(app);

    seed_stale_surface(&session_file, "per_long", true, "om_card");

    let (app, platform, backend) = restart_app(&session_file, |b| {
        b.ask_permission(over_budget_perm("per_long", "ses_1"));
    })
    .await;
    // The withdrawal's PATCH fails once.
    platform
        .fail_update_count
        .store(1, std::sync::atomic::Ordering::SeqCst);

    sweep(&app.permission, &app).await;

    assert_eq!(
        backend.reply_permission_calls.lock().await.as_slice(),
        &[("per_long".to_string(), "reject".to_string())],
        "the decline landed"
    );
    assert!(
        app.cards_handle()
            .card_handles
            .lock()
            .await
            .message_of("per_long")
            .is_some(),
        "a failed PATCH keeps the handle, or the withdrawal could never be retried"
    );
    assert!(
        persisted(&session_file).unwrap_or_default().contains("per_long"),
        "and keeps the persisted record"
    );

    // The request is no longer listed (it was declined), so only the
    // sweep-level retry can finish the job.
    sweep(&app.permission, &app).await;

    let updates: Vec<serde_json::Value> = {
        let calls = platform.calls.lock().await;
        calls
            .iter()
            .filter_map(|call| match call {
                PlatformCall::UpdateMessage { message_id, card } if message_id == "om_card" => {
                    Some(card.clone())
                }
                _ => None,
            })
            .collect()
    };
    assert_eq!(
        updates.len(),
        2,
        "one failed attempt and one successful retry, no duplicate deliveries"
    );
    let final_card = updates.last().expect("the successful retry");
    let text = card_text(final_card);
    assert_eq!(
        text.matches(crate::feishu::card::question::UNSHOWABLE_PERMISSION_RECEIPT)
            .count(),
        1,
        "exactly one receipt survives: {text}"
    );
    assert!(
        !text.contains("允许一次") && !text.contains("始终允许"),
        "the card ends non-interactive: {text}"
    );
    assert!(
        app.cards_handle()
            .card_handles
            .lock()
            .await
            .message_of("per_long")
            .is_none(),
        "the handle is forgotten only after the retry landed"
    );
    assert!(
        !persisted(&session_file).unwrap_or_default().contains("per_long"),
        "and the persisted record is gone"
    );
    assert!(
        posted_cards(&platform).await.is_empty(),
        "no chat card duplicates the receipt on the withdrawn card"
    );
}

/// Whole-branch re-review #883: one withdrawal PATCH may not wedge the sweep.
/// A hung Feishu call is bounded by the flow's timeout, the attempt counts as
/// failed, the handle is kept, and the same sweep completes — otherwise a
/// stalled PATCH would freeze the whole permission poller with an old Allow
/// card still clickable.
#[tokio::test]
async fn a_hung_withdrawal_patch_does_not_wedge_the_sweep() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let session_file = dir.path().join("sessions.json");

    let (app, _platform, _backend) = restart_app(&session_file, |_| {}).await;
    seed_session(&app, "ses_1", "/work").await;
    drop(app);

    seed_stale_surface(&session_file, "per_long", true, "om_card");

    let (app, platform, _backend) = restart_app(&session_file, |b| {
        b.ask_permission(over_budget_perm("per_long", "ses_1"));
    })
    .await;
    app.permission
        .withdrawal_timeout_ms
        .store(50, std::sync::atomic::Ordering::SeqCst);
    let (entered, release) = platform.pause("update", "om_card");

    let sweep = tokio::spawn({
        let app = app.clone();
        async move {
            let mut seen = std::collections::HashSet::new();
            app.permission.sweep(&app.flow_handles(), &mut seen).await;
        }
    });
    tokio::time::timeout(std::time::Duration::from_secs(2), entered.notified())
        .await
        .expect("the withdrawal PATCH is in flight");

    tokio::time::timeout(std::time::Duration::from_secs(2), sweep)
        .await
        .expect("a hung PATCH must not wedge the sweep")
        .expect("the sweep task joins");

    assert_eq!(
        app.permission.pending_withdrawal_count().await,
        1,
        "the timed-out attempt is kept for the next sweep"
    );
    assert!(
        app.cards_handle()
            .card_handles
            .lock()
            .await
            .message_of("per_long")
            .is_some(),
        "and its handle is retained"
    );
    release.notify_waiters();
}

/// Whole-branch re-review #883: a permanent message-gone failure (`230002`,
/// inside a 2xx body) means nothing is clickable, so the entry and the handle
/// are dropped instead of retried forever — and the next sweep does not try
/// again.
#[tokio::test]
async fn a_message_gone_withdrawal_is_terminated_not_retried() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let session_file = dir.path().join("sessions.json");

    let (app, _platform, _backend) = restart_app(&session_file, |_| {}).await;
    seed_session(&app, "ses_1", "/work").await;
    drop(app);

    seed_stale_surface(&session_file, "per_long", true, "om_card");

    let (app, platform, _backend) = restart_app(&session_file, |b| {
        b.ask_permission(over_budget_perm("per_long", "ses_1"));
    })
    .await;
    platform
        .fail_update_message_gone_count
        .store(1, std::sync::atomic::Ordering::SeqCst);
    sweep(&app.permission, &app).await;

    assert_eq!(
        app.permission.pending_withdrawal_count().await,
        0,
        "a gone message has nothing left to retry"
    );
    assert!(
        app.cards_handle()
            .card_handles
            .lock()
            .await
            .message_of("per_long")
            .is_none(),
        "the dead handle is dropped"
    );
    assert!(
        !persisted(&session_file).unwrap_or_default().contains("per_long"),
        "and the persisted record is gone"
    );

    // A second sweep has nothing to deliver: exactly one PATCH attempt.
    sweep(&app.permission, &app).await;
    let updates = {
        let calls = platform.calls.lock().await;
        calls
            .iter()
            .filter(|call| matches!(call, PlatformCall::UpdateMessage { message_id, .. } if message_id == "om_card"))
            .count()
    };
    assert_eq!(updates, 1, "the terminated entry is never retried");
}

/// Whole-branch re-review #883: the retry backlog is bounded. Persistently
/// failing withdrawals cannot retain card payloads without limit; past the cap
/// the new entry is dropped (the vanished-surface pass still neutralizes the
/// card) and the map stops growing.
#[tokio::test]
async fn the_withdrawal_backlog_is_capped() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let session_file = dir.path().join("sessions.json");

    let (app, _platform, _backend) = restart_app(&session_file, |_| {}).await;
    seed_session(&app, "ses_1", "/work").await;
    drop(app);

    const REQUESTS: usize = 40;
    let mut requests = Vec::with_capacity(REQUESTS);
    for index in 0..REQUESTS {
        let request_id = format!("per_{index}");
        seed_stale_surface(&session_file, &request_id, true, &format!("om_card_{index}"));
        requests.push(over_budget_perm(&request_id, "ses_1"));
    }

    let (app, platform, _backend) = restart_app(&session_file, move |b| {
        b.ask_permissions(requests);
    })
    .await;
    platform
        .fail_update_count
        .store(REQUESTS * 2, std::sync::atomic::Ordering::SeqCst);
    sweep(&app.permission, &app).await;

    assert_eq!(
        app.permission.pending_withdrawal_count().await,
        32,
        "the backlog holds the documented cap, not one entry per request"
    );
    // Every request still attempted its PATCH (the cap drops the payload, not
    // the safety check).
    let attempts = {
        let calls = platform.calls.lock().await;
        calls
            .iter()
            .filter(|call| matches!(call, PlatformCall::UpdateMessage { .. }))
            .count()
    };
    assert!(
        attempts >= REQUESTS,
        "every withdrawal was attempted: {attempts} < {REQUESTS}"
    );
}
