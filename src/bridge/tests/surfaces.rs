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
fn question_request(request_id: &str, session_id: &str) -> crate::opencode::types::QuestionRequest {
    use crate::opencode::types::{QuestionInfo, QuestionOption, QuestionRequest};
    let question = |text: &str, header: &str, label: &str| QuestionInfo {
        question: text.into(),
        header: header.into(),
        options: vec![QuestionOption {
            label: label.into(),
            description: String::new(),
        }],
        multiple: None,
        custom: None,
    };
    QuestionRequest {
        id: request_id.into(),
        session_id: session_id.into(),
        questions: vec![
            question("选目录", "目录", "/a"),
            question("选分支", "分支", "main"),
        ],
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
    std::fs::read_to_string(session_file.with_file_name("interactive_surfaces.json")).ok()
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

    // The restarted process finds the request already resolved.
    let (app, platform, _backend) = restart_app(&session_file, |_| {}).await;
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

    let (app, platform, _backend) = restart_app(&session_file, |_| {}).await;
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
