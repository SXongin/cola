//! Restart re-adoption (ADR-0038): a `/restart` while a Permission/Question is
//! pending must not leave the request with two live surfaces. The interactive
//! surface state is persisted beside `sessions.json` and re-adopted by the new
//! process: the first sweep repaints the card the request already lives on
//! instead of posting a second one, and a click on that card resolves the
//! request and repaints it exactly as before the restart.

use std::sync::Arc;

use crate::bridge::test_support::*;

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
    seed_live_card(&app, "ses_1", "om_live").await;
    Turn::push_text(&app.cards_handle(), "ses_1", "回合的内容。").await;
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

/// ADR-0102: an over-budget permission's companion record survives a restart.
/// The new process re-adopts the persisted card WITHOUT re-sending the full
/// target, and the click guard still approves because the loaded record matches
/// the pending request.
#[tokio::test]
async fn restart_readopts_an_over_budget_permission_without_resending_its_companion() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let session_file = dir.path().join("sessions.json");
    let request = || {
        let command = format!("echo {}", "x".repeat(5_000));
        perm_request("per_long", "ses_1", &command)
    };

    // The pre-restart process: the companion lands, then the summary card.
    let mut backend = MockBackend::new(realistic_parts());
    backend.ask_permission(request());
    let backend = Arc::new(backend);
    let platform = Arc::new(RecordingPlatform::new());
    let app = Arc::new(App::new(test_config(&session_file), backend.clone(), platform.clone()).unwrap());
    seed_live_card(&app, "ses_1", "om_live").await;
    Turn::push_text(&app.cards_handle(), "ses_1", "回合的内容。").await;
    sweep(&app.permission, &app).await;
    assert!(
        platform.calls.lock().await.iter().any(|call| matches!(
            call,
            PlatformCall::SendMessageReply { msg_type, .. } if msg_type == "post"
        )),
        "the companion is delivered before the card"
    );
    let raw = persisted(&session_file).expect("the surface record is persisted");
    assert!(
        raw.contains("companion") && raw.contains("per_long"),
        "the delivery record is persisted: {raw}"
    );

    // The restarted process: same store, same pending request, fresh platform.
    let (app, platform, backend) = restart_app(&session_file, |b| {
        b.ask_permission(request());
    })
    .await;

    sweep(&app.permission, &app).await;

    assert!(
        !platform.calls.lock().await.iter().any(|call| matches!(
            call,
            PlatformCall::SendMessageReply { .. } | PlatformCall::UploadFile { .. }
        )),
        "readoption trusts the record and never re-sends the companion"
    );
    assert!(
        posted_cards(&platform).await.is_empty(),
        "and posts no second card"
    );

    // The re-adopted card's Allow click still approves: the persisted digest
    // matches the pending request it re-read.
    let result = app
        .host_action(serde_json::json!({
            "action": "perm",
            "reply": "once",
            "session_id": "ses_1",
            "directory": "/work",
            "request_id": "per_long",
            "open_message_id": "om_live",
            "perm_label": "✅ 已允许一次",
            "perm_color": "green",
        }))
        .await
        .expect("a card-action result");
    let ack = card_text(result.card.as_ref().expect("the ack carries the card"));
    assert!(
        ack.contains("✅ 已允许一次：⚡ 执行 Shell 命令"),
        "the receipt lands on the re-adopted card: {ack}"
    );
    assert_eq!(
        backend.reply_permission_calls.lock().await.as_slice(),
        &[("per_long".to_string(), "once".to_string())],
        "a click in the restarted process still approves"
    );
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
    seed_live_card(&app, "ses_1", "om_live").await;
    Turn::push_text(&app.cards_handle(), "ses_1", "回合的内容。").await;
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
        ack2.contains("✅ 已回答：选目录 /a；选分支 main"),
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

/// ADR-0100 regression: taking a session over no longer embeds/claims its
/// pending requests, so the sweep is the ONLY surface for them. An unmapped
/// session's request is listed (its directory is known through a mapped
/// sibling) but has no reply target yet; the failed surface must stay
/// UNMARKED, or every later sweep treats it as already surfaced and
/// `rehost_block` has no card to re-host it on — the request would be
/// invisible forever. Sweep → adopt → the same `seen` → the card appears.
#[tokio::test]
async fn an_unmapped_sessions_pending_survives_until_adoption() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let session_file = dir.path().join("sessions.json");
    let mut backend = MockBackend::new(realistic_parts());
    backend.given_sessions(vec![
        list_session("ses_mapped", "本项目会话", "/work/proj", 100),
        list_session("ses_foreign", "外部会话", "/work/proj", 200),
    ]);
    backend.ask_permission(perm_request("per_1", "ses_foreign", "ls -la"));
    let (app, platform) = build_app(test_config(&session_file), backend).await;
    seed_entry(
        &app,
        crate::config::SessionEntry::new(
            crate::config::ThreadKey::new("chat_1".into(), "chat_1".into()),
            "ses_mapped",
            "/work/proj",
        ),
    )
    .await;

    // ONE seen set across both sweeps, exactly as `poll_loop` keeps it.
    let mut seen = std::collections::HashSet::new();
    app.permission.sweep(&app.flow_handles(), &mut seen).await;
    assert!(
        posted_cards(&platform).await.is_empty(),
        "an unmapped session has no reply target yet: nothing can be posted"
    );
    assert!(
        !seen.contains("per_1"),
        "a failed surface must not mark the request as seen"
    );

    // The user takes the foreign session over (ADR-0100: one text line, no
    // card, nothing claimed).
    send_command(&app, "/switch ses_foreign", "msg_switch").await;
    assert!(
        platform
            .texts()
            .await
            .iter()
            .any(|line| line.contains("已接管「外部会话」")
                && line.contains(crate::feishu::card::session::WAITING_CHIP)),
        "the confirmation names the wait: {:?}",
        platform.texts().await
    );

    // The next sweep surfaces the still-pending request on its own card.
    app.permission.sweep(&app.flow_handles(), &mut seen).await;
    let posted = posted_cards(&platform).await;
    assert_eq!(
        posted.len(),
        1,
        "the adopted session's permission now has a card: {posted:?}"
    );
    assert!(
        card_text(&posted[0]).contains("🔐 权限请求"),
        "the standalone permission card: {}",
        card_text(&posted[0])
    );
    assert!(seen.contains("per_1"), "the successful surface is marked");
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
    seed_live_card(&app, "ses_1", "om_live").await;
    Turn::push_text(&app.cards_handle(), "ses_1", "回合的内容。").await;
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
/// past the card's display budget, so the current process must withdraw a
/// persisted card and reprocess rather than re-adopt it (ADR-0102).
fn over_budget_perm(request_id: &str, session_id: &str) -> crate::backend::types::PermissionRequest {
    let command = format!(
        "echo {}",
        "x".repeat(crate::backend::types::PERMISSION_TARGET_MAX_CHARS)
    );
    perm_request(request_id, session_id, &command)
}

/// The persisted surface an older build left for `over_budget_perm` — the file
/// is written directly because the current build no longer produces the
/// pre-companion shape: it surfaces a summary card or its own fallback instead.
/// `inline` picks the shape: an inline block on a streaming card, or a
/// standalone request card.
fn seed_stale_surface(session_file: &std::path::Path, request_id: &str, inline: bool, message_id: &str) {
    use crate::bridge::request::kind::InteractionKind;
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
                kind: InteractionKind::Permission,
                session_id: "ses_1".into(),
                directory: "/work".into(),
                target: "per_long".into(),
                asked_at: None,
            },
        );
    } else {
        surfaces.set_standalone(
            request_id,
            StandaloneSurface {
                client_id: String::new(),
                kind: InteractionKind::Permission,
                message_id: message_id.into(),
                summary: "⚡ **执行 Shell 命令**\n**对象**:\n- `echo …`".into(),
                directory: "/work".into(),
                session_id: "ses_1".into(),
            },
        );
    }
}

/// ADR-0101/0102 (#1013): a recovered over-budget card an older build
/// persisted is **withdrawn, not re-adopted**. The request then takes the
/// normal fresh path; with no reply target the companion goes into the
/// session's chat and the standalone summary card follows — never a silent
/// decline.
#[tokio::test]
async fn restart_withdraws_an_over_budget_inline_permission_then_surfaces_the_summary() {
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

    assert!(
        backend.reply_permission_calls.lock().await.is_empty(),
        "the fresh path must not decline"
    );
    let withdrawn = last_update_of(&platform, "om_card")
        .await
        .expect("the persisted card is withdrawn in the same sweep");
    let text = card_text(&withdrawn);
    assert!(
        text.contains(crate::feishu::card::question::SUPERSEDED_PERMISSION_RECEIPT),
        "the old block says it was reprocessed: {text}"
    );
    assert!(
        !text.contains("允许一次") && !text.contains("始终允许"),
        "no control survives on the withdrawn card: {text}"
    );
    assert!(
        platform.calls.lock().await.iter().any(|call| matches!(
            call,
            PlatformCall::SendMessage { msg_type, .. } if msg_type == "post"
        )),
        "the companion is sent into the session's chat"
    );
    assert!(
        posted_cards(&platform)
            .await
            .iter()
            .any(|card| { card_text(card).contains("富文本消息（Rich Text Message）") }),
        "the fresh path surfaces the companion summary"
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
        app.permission.sent_cards.lock().await.contains_key("per_long"),
        "the fallback card is recorded as the standalone surface"
    );
    assert!(
        persisted(&session_file).unwrap_or_default().contains("per_long"),
        "the fallback surface is persisted"
    );
}

/// The standalone half of the same rule (#1013): a request card an older build
/// posted for an over-budget request is withdrawn as reprocessed, then the
/// fresh path delivers the companion into the session's chat and surfaces the
/// summary card.
#[tokio::test]
async fn restart_withdraws_an_over_budget_standalone_permission_then_surfaces_the_summary() {
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

    assert!(
        backend.reply_permission_calls.lock().await.is_empty(),
        "the fresh path must not decline"
    );
    let withdrawn = last_update_of(&platform, "om_standalone")
        .await
        .expect("the persisted standalone card is repainted");
    let text = card_text(&withdrawn);
    assert!(
        text.contains(crate::feishu::card::question::SUPERSEDED_PERMISSION_HEADER),
        "the old card says it was reprocessed: {text}"
    );
    assert!(
        !text.contains("允许一次"),
        "no control survives on the withdrawn card: {text}"
    );
    assert!(
        platform.calls.lock().await.iter().any(|call| matches!(
            call,
            PlatformCall::SendMessage { msg_type, .. } if msg_type == "post"
        )),
        "the companion is sent into the session's chat"
    );
    assert!(
        posted_cards(&platform)
            .await
            .iter()
            .any(|card| { card_text(card).contains("富文本消息（Rich Text Message）") }),
        "the fresh path surfaces the companion summary"
    );
    assert!(
        app.permission.sent_cards.lock().await.contains_key("per_long"),
        "the summary card is recorded as the standalone surface"
    );
    assert!(
        persisted(&session_file).unwrap_or_default().contains("per_long"),
        "the summary surface is persisted"
    );
}

/// ADR-0102: a persisted over-budget card whose companion record is
/// **confirmed** but whose digest no longer matches the pending request is not
/// a proof. Content may have been delivered for the previous target, but it
/// does not cover the request the card would approve. The new process must
/// withdraw the old card and reprocess the request — never approve or decline
/// in place.
#[tokio::test]
async fn restart_withdraws_a_card_whose_confirmed_companion_digest_moved() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let session_file = dir.path().join("sessions.json");

    let (app, _platform, _backend) = restart_app(&session_file, |_| {}).await;
    seed_session(&app, "ses_1", "/work").await;
    drop(app);

    seed_stale_surface(&session_file, "per_long", false, "om_standalone");
    // A confirmed delivery for SOME content — just not the request this card
    // would approve. `delivered_companion` must reject it on the digest.
    crate::bridge::surfaces::Surfaces::load(crate::state::StateLayout::beside(
        &session_file,
        "interactive_surfaces.json",
    ))
    .set_companion(
        "per_long",
        crate::bridge::surfaces::CompanionDelivery {
            message_id: "om_companion".into(),
            lane: crate::bridge::surfaces::CompanionLane::RichText,
            target_digest: 0,
        },
    );

    let (app, platform, backend) = restart_app(&session_file, |b| {
        b.ask_permission(over_budget_perm("per_long", "ses_1"));
    })
    .await;
    sweep(&app.permission, &app).await;

    assert!(
        backend.reply_permission_calls.lock().await.is_empty(),
        "a mismatched companion never approves or declines in place"
    );
    let withdrawn = last_update_of(&platform, "om_standalone")
        .await
        .expect("the persisted card is withdrawn and reprocessed");
    let text = card_text(&withdrawn);
    assert!(
        text.contains(crate::feishu::card::question::SUPERSEDED_PERMISSION_HEADER),
        "the old card says it was reprocessed: {text}"
    );
    assert!(
        !text.contains("允许一次"),
        "no control survives on the withdrawn card: {text}"
    );
    let record = app
        .permission
        .surfaces
        .companion("per_long")
        .expect("the fresh path delivers a companion for the current target");
    assert!(
        record.target_digest != 0 && !record.is_uncertain(),
        "the mismatched record is replaced, not trusted: {record:?}"
    );
    assert!(
        platform.calls.lock().await.iter().any(|call| matches!(
            call,
            PlatformCall::SendMessage { msg_type, .. } if msg_type == "post"
        )),
        "the replacement companion is sent into the session's chat"
    );
    assert!(
        posted_cards(&platform)
            .await
            .iter()
            .any(|card| { card_text(card).contains("富文本消息（Rich Text Message）") }),
        "the fresh path surfaces the summary card"
    );
}

/// ADR-0102: a Superseded withdrawal is **not** a settlement, so it must not
/// drop the uncertain companion marker. The fresh path then sees the marker,
/// treats the outcome as unknown, and surfaces the fallback card — it must not
/// re-send a companion the ambiguous-send rule says is terminal.
#[tokio::test]
async fn a_superseded_withdrawal_keeps_the_uncertain_companion_marker() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let session_file = dir.path().join("sessions.json");

    let (app, _platform, _backend) = restart_app(&session_file, |_| {}).await;
    seed_session(&app, "ses_1", "/work").await;
    drop(app);

    seed_stale_surface(&session_file, "per_long", false, "om_standalone");
    // A confirmed *marker* is not a delivery: the companion may have landed, so
    // it is never re-sent. The digest matches the request, so the fresh path
    // must read the marker rather than discard it as stale.
    let request = over_budget_perm("per_long", "ses_1");
    crate::bridge::surfaces::Surfaces::load(crate::state::StateLayout::beside(
        &session_file,
        "interactive_surfaces.json",
    ))
    .set_companion(
        "per_long",
        crate::bridge::surfaces::CompanionDelivery {
            message_id: String::new(),
            lane: crate::bridge::surfaces::CompanionLane::RichText,
            target_digest: crate::bridge::request::companion::companion_target_digest(&request),
        },
    );

    let (app, platform, backend) = restart_app(&session_file, |b| {
        b.ask_permission(request);
    })
    .await;
    sweep(&app.permission, &app).await;

    let marker = app
        .permission
        .surfaces
        .companion("per_long")
        .expect("a Superseded withdrawal keeps the uncertain marker");
    assert!(
        marker.is_uncertain(),
        "the marker is still the no-duplicate record, not a delivery"
    );
    assert!(
        !platform.calls.lock().await.iter().any(|call| matches!(
            call,
            PlatformCall::SendMessageReply { .. } | PlatformCall::UploadFile { .. }
        )),
        "an uncertain marker is terminal: the fresh path must not re-send"
    );
    assert!(
        backend.reply_permission_calls.lock().await.is_empty(),
        "the uncertain outcome is not a decline"
    );
    assert!(
        posted_cards(&platform).await.iter().any(|card| {
            card_text(card).contains(crate::feishu::card::question::FALLBACK_PERMISSION_HEADER)
        }),
        "the fresh path surfaces the fallback card"
    );
}

/// ADR-0101: a recovered over-budget card is never approved or declined in
/// place. The old surface is withdrawn neutrally, then the request takes the
/// normal fresh path — here Auto-Accept approves it unseen with its usual INFO
/// line and no second card.
#[tokio::test]
async fn restart_withdraws_an_unbacked_card_then_auto_accepts_through_the_fresh_path() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let session_file = dir.path().join("sessions.json");

    let (app, _platform, _backend) = restart_app(&session_file, |_| {}).await;
    seed_entry(
        &app,
        crate::config::SessionEntry {
            client_id: crate::config::DEFAULT_CLIENT_ID.into(),
            thread_key: crate::config::ThreadKey::new("chat_1".into(), "chat_1".into()),
            session_id: "ses_1".into(),
            directory: "/work".into(),
            agent: None,
            model: None,
            auto_accept: true,
            topic_anchor: None,
            topic_root: None,
            variant: None,
        },
    )
    .await;
    drop(app);

    seed_stale_surface(&session_file, "per_long", true, "om_card");

    let (app, platform, backend) = restart_app(&session_file, |b| {
        b.ask_permission(over_budget_perm("per_long", "ses_1"));
    })
    .await;
    let mut seen = std::collections::HashSet::new();
    let (_, logs) = capture_logs(async {
        app.permission.sweep(&app.flow_handles(), &mut seen).await;
    })
    .await;

    // The old card is withdrawn, not taken over.
    let withdrawn = last_update_of(&platform, "om_card")
        .await
        .expect("the recovered card is withdrawn");
    let text = card_text(&withdrawn);
    assert!(
        text.contains(crate::feishu::card::question::SUPERSEDED_PERMISSION_RECEIPT),
        "the old card says it was reprocessed: {text}"
    );
    assert!(
        !text.contains("允许一次") && !text.contains("始终允许"),
        "no control survives: {text}"
    );

    // The normal prepare path approves it unseen and audits it.
    assert_eq!(
        backend.reply_permission_calls.lock().await.as_slice(),
        &[("per_long".to_string(), "once".to_string())],
        "Auto-Accept approves through the fresh prepare path"
    );
    let line = assert_line_level(&logs, "approved permission per_long", "INFO");
    assert!(
        line.contains("(targets total"),
        "the ADR-0101 audit line names the length it approved: {line}"
    );
    assert!(
        posted_cards(&platform).await.is_empty(),
        "Auto-Accept posts no second card"
    );
    assert!(
        !platform.calls.lock().await.iter().any(|call| matches!(
            call,
            PlatformCall::SendMessageReply { .. } | PlatformCall::UploadFile { .. }
        )),
        "Auto-Accept sends no companion"
    );
    assert!(
        !persisted(&session_file).unwrap_or_default().contains("per_long"),
        "the recovered surface is gone"
    );
}

/// ADR-0101/0102: a recovered over-budget card with no usable companion is
/// withdrawn, then the normal fresh path delivers the companion and surfaces
/// the summary — never approving or declining the old decision in place.
#[tokio::test]
async fn restart_withdraws_an_unbacked_card_then_delivers_a_fresh_companion() {
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
    // A live card is the reply target the companion rides under.
    seed_live_card(&app, "ses_1", "om_live").await;

    let mut seen = std::collections::HashSet::new();
    app.permission.sweep(&app.flow_handles(), &mut seen).await;

    let withdrawn = last_update_of(&platform, "om_card")
        .await
        .expect("the recovered card is withdrawn");
    assert!(
        card_text(&withdrawn).contains(crate::feishu::card::question::SUPERSEDED_PERMISSION_RECEIPT),
        "the old card says it was reprocessed: {}",
        card_text(&withdrawn)
    );
    assert!(
        backend.reply_permission_calls.lock().await.is_empty(),
        "the old decision is not declined in place"
    );
    let posts = platform
        .calls
        .lock()
        .await
        .iter()
        .filter(|call| {
            matches!(
                call,
                PlatformCall::SendMessageReply { msg_type, .. } if msg_type == "post"
            )
        })
        .count();
    assert_eq!(posts, 1, "the fresh path delivers one companion");
    let card = Turn::rendered_card(&app.cards_handle(), "ses_1")
        .await
        .expect("the new summary card renders");
    let text = card_text(&card);
    assert!(
        text.contains("富文本消息（Rich Text Message）"),
        "the new card carries the summary: {text}"
    );
    assert!(text.contains("3005 字符"), "and the total content length: {text}");
    assert!(
        posted_cards(&platform).await.is_empty(),
        "the old card is patched, not duplicated"
    );
}

/// An uncertain companion marker survives a restart: it still prevents a
/// duplicate send and it never becomes a delivery proof. An Allow click is
/// refused with the uncertain copy, and no send is retried.
#[tokio::test]
async fn restart_keeps_an_uncertain_companion_marker_non_approvable() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let session_file = dir.path().join("sessions.json");

    // Pre-restart: the companion send is ambiguous and the decline fails, so
    // the request stays pending with only the marker.
    let mut backend = MockBackend::new(realistic_parts());
    backend.ask_permission(over_budget_perm("per_long", "ses_1"));
    backend
        .fail_reply_permission_count
        .store(1, std::sync::atomic::Ordering::SeqCst);
    let backend = Arc::new(backend);
    let platform = Arc::new(RecordingPlatform::new());
    platform
        .ambiguous_send_message_count
        .store(1, std::sync::atomic::Ordering::SeqCst);
    let app = Arc::new(App::new(test_config(&session_file), backend.clone(), platform.clone()).unwrap());
    seed_live_card(&app, "ses_1", "om_live").await;
    sweep(&app.permission, &app).await;
    assert!(
        persisted(&session_file).unwrap_or_default().contains("per_long"),
        "the marker is persisted"
    );

    let (app, platform, backend) = restart_app(&session_file, |b| {
        b.ask_permission(over_budget_perm("per_long", "ses_1"));
    })
    .await;
    let marker = app
        .permission
        .surfaces
        .companion("per_long")
        .expect("the uncertain marker hydrates at startup");
    assert!(
        marker.message_id.is_empty(),
        "the marker is not a confirmed delivery"
    );

    // The Allow click is refused before any sweep can re-send.
    let result = app
        .host_action(serde_json::json!({
            "action": "perm",
            "reply": "once",
            "session_id": "ses_1",
            "directory": "/work",
            "request_id": "per_long",
            "perm_label": "✅ 已允许一次",
            "perm_color": "green",
        }))
        .await
        .expect("a card-action result");
    assert_eq!(
        backend.reply_permission_calls.lock().await.as_slice(),
        &[("per_long".to_string(), "reject".to_string())],
        "the restarted marker never approves"
    );
    assert_eq!(
        result.toast.as_deref(),
        Some("已拒绝：完整内容送达结果未知"),
        "the toast names the unknown outcome"
    );
    assert!(
        !platform.calls.lock().await.iter().any(|call| matches!(
            call,
            PlatformCall::SendMessageReply { .. } | PlatformCall::UploadFile { .. }
        )),
        "the marker prevents a duplicate companion after the restart"
    );

    // A sweep also sends nothing: the marker is terminal for delivery.
    sweep(&app.permission, &app).await;
    assert!(
        !platform.calls.lock().await.iter().any(|call| matches!(
            call,
            PlatformCall::SendMessageReply { .. } | PlatformCall::UploadFile { .. }
        )),
        "no companion is re-sent after the restart"
    );
}

/// A recovered unbacked card whose Auto-Accept flag read fails is withdrawn,
/// and the request is left retryable — the fresh `prepare` path retries it and
/// then auto-accepts once the hop heals. No approval or decline is made on the
/// old decision.
#[tokio::test]
async fn restart_withdraws_a_recovered_card_then_retries_an_unknown_auto_accept() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let session_file = dir.path().join("sessions.json");

    let (app, _platform, _backend) = restart_app(&session_file, |_| {}).await;
    seed_entry(
        &app,
        crate::config::SessionEntry {
            client_id: crate::config::DEFAULT_CLIENT_ID.into(),
            thread_key: crate::config::ThreadKey::new("chat_1".into(), "chat_1".into()),
            session_id: "ses_parent".into(),
            directory: "/work".into(),
            agent: None,
            model: None,
            auto_accept: true,
            topic_anchor: None,
            topic_root: None,
            variant: None,
        },
    )
    .await;
    drop(app);

    seed_stale_surface(&session_file, "per_long", true, "om_card");

    let (app, platform, backend) = restart_app(&session_file, |b| {
        b.with_session_parent("ses_child", "ses_parent");
        b.ask_permission(over_budget_perm("per_long", "ses_child"));
        b.fail_session_infos(1);
    })
    .await;

    let mut seen = std::collections::HashSet::new();
    app.permission.sweep(&app.flow_handles(), &mut seen).await;
    assert!(
        backend.reply_permission_calls.lock().await.is_empty(),
        "an unknown read neither approves nor declines"
    );
    let withdrawn = last_update_of(&platform, "om_card")
        .await
        .expect("the unbacked card is withdrawn");
    assert!(
        card_text(&withdrawn).contains(crate::feishu::card::question::SUPERSEDED_PERMISSION_RECEIPT),
        "the old card is neutral: {}",
        card_text(&withdrawn)
    );
    assert!(
        Turn::live_permissions(&app.cards_handle(), "ses_child")
            .await
            .is_empty(),
        "no interactive approval is offered while the flag is unknown"
    );

    // The hop heals: the fresh prepare path auto-accepts unseen.
    let mut seen = std::collections::HashSet::new();
    app.permission.sweep(&app.flow_handles(), &mut seen).await;
    assert_eq!(
        backend.reply_permission_calls.lock().await.as_slice(),
        &[("per_long".to_string(), "once".to_string())],
        "the fresh prepare path approves once the flag resolves"
    );
    assert!(
        posted_cards(&platform).await.is_empty(),
        "Auto-Accept still posts no second card"
    );
}

/// A recovered unbacked card on a chain too deep to read (Capped) is withdrawn,
/// then the fresh path delivers the companion into the session's chat and
/// surfaces the summary card rather than approving or declining the old
/// decision in place.
#[tokio::test]
async fn restart_withdraws_a_recovered_card_then_handles_a_capped_chain() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let session_file = dir.path().join("sessions.json");

    let (app, _platform, _backend) = restart_app(&session_file, |_| {}).await;
    seed_session(&app, "ses_0", "/work").await;
    drop(app);

    seed_stale_surface(&session_file, "per_long", true, "om_card");

    let (app, platform, backend) = restart_app(&session_file, |b| {
        for hop in 0..8 {
            b.with_session_parent(&format!("ses_{hop}"), &format!("ses_{}", hop + 1));
        }
        b.ask_permission(over_budget_perm("per_long", "ses_0"));
    })
    .await;

    let mut seen = std::collections::HashSet::new();
    app.permission.sweep(&app.flow_handles(), &mut seen).await;

    let withdrawn = last_update_of(&platform, "om_card")
        .await
        .expect("the unbacked card is withdrawn");
    assert!(
        card_text(&withdrawn).contains(crate::feishu::card::question::SUPERSEDED_PERMISSION_RECEIPT),
        "the old card is neutral: {}",
        card_text(&withdrawn)
    );
    assert!(
        backend.reply_permission_calls.lock().await.is_empty(),
        "the fresh path must not decline"
    );
    assert!(
        platform.calls.lock().await.iter().any(|call| matches!(
            call,
            PlatformCall::SendMessage { msg_type, .. } if msg_type == "post"
        )),
        "the companion is sent into the session's chat"
    );
    assert!(
        posted_cards(&platform)
            .await
            .iter()
            .any(|card| { card_text(card).contains("富文本消息（Rich Text Message）") }),
        "the fresh path surfaces the companion summary"
    );
}

/// A failed superseded withdrawal must keep the companion marker: an uncertain
/// record is still the only proof state once the card is neutralized, and the
/// next sweep must neither re-send nor approve it.
#[tokio::test]
async fn a_failed_withdrawal_keeps_the_uncertain_companion_marker() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let session_file = dir.path().join("sessions.json");

    let (app, _platform, _backend) = restart_app(&session_file, |_| {}).await;
    seed_session(&app, "ses_1", "/work").await;
    drop(app);

    let request = over_budget_perm("per_long", "ses_1");
    seed_stale_surface(&session_file, "per_long", true, "om_card");
    {
        use crate::bridge::surfaces::{CompanionDelivery, CompanionLane, Surfaces};
        let surfaces = Surfaces::load(crate::state::StateLayout::beside(
            &session_file,
            "interactive_surfaces.json",
        ));
        surfaces.set_companion(
            "per_long",
            CompanionDelivery {
                message_id: String::new(),
                lane: CompanionLane::RichText,
                target_digest: crate::bridge::request::companion::companion_target_digest(&request),
            },
        );
    }

    let (app, platform, backend) = restart_app(&session_file, move |b| {
        b.ask_permission(request);
    })
    .await;
    // Both the superseded withdrawal and its immediate neutralization fail.
    platform
        .fail_update_count
        .store(2, std::sync::atomic::Ordering::SeqCst);
    sweep(&app.permission, &app).await;

    assert!(
        app.permission.surfaces.companion("per_long").is_some(),
        "a failed PATCH keeps the marker"
    );
    assert!(
        app.cards_handle()
            .card_handles
            .lock()
            .await
            .message_of("per_long")
            .is_some(),
        "and the old card is still live until a retry lands"
    );
    assert!(
        backend.reply_permission_calls.lock().await.is_empty(),
        "nothing is approved or declined on the failed withdrawal"
    );

    // The retry lands; the request then takes the fresh path and is surfaced
    // as the fallback card. The marker is deliberately kept (a duplicate
    // companion must never be sent) and nothing is approved or declined.
    let mut seen = std::collections::HashSet::new();
    app.permission.sweep(&app.flow_handles(), &mut seen).await;
    assert!(
        app.permission.surfaces.companion("per_long").is_some(),
        "the uncertain marker remains while the request is pending"
    );
    assert!(
        backend.reply_permission_calls.lock().await.is_empty(),
        "the fallback card never decides for the user"
    );
    assert!(
        posted_cards(&platform).await.iter().any(|card| {
            card_text(card).contains(crate::feishu::card::question::FALLBACK_PERMISSION_HEADER)
        }),
        "the fresh path surfaces the fallback card"
    );
}

/// An unbacked recovered card whose superseded-withdrawal PATCH fails must
/// still be neutralized immediately with a "will retry" receipt, and the
/// request stays unmarked so the next sweep finishes the withdrawal and then
/// takes the fresh path (here: the reason card).
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
    })
    .await;
    // The superseded withdrawal's PATCH fails once; the immediate
    // neutralization then lands with the "will retry" receipt.
    platform
        .fail_update_count
        .store(1, std::sync::atomic::Ordering::SeqCst);

    // One persistent `seen`, like the poll loop: a failure must not retire the
    // request, or the retry below could never happen.
    let mut seen = std::collections::HashSet::new();
    app.permission.sweep(&app.flow_handles(), &mut seen).await;

    assert!(
        backend.reply_permission_calls.lock().await.is_empty(),
        "the first sweep neither approves nor declines the old decision"
    );
    let neutral = last_update_of(&platform, "om_card")
        .await
        .expect("the persisted card is neutralized even on failure");
    let text = card_text(&neutral);
    assert!(
        text.contains(crate::feishu::card::question::SUPERSEDED_PERMISSION_PREVIEW_RECEIPT),
        "the block says the old card is being reprocessed: {text}"
    );
    assert!(
        !text.contains("自动拒绝") && !text.contains("目标过长"),
        "a reprocess preview must not borrow the refusal copy: {text}"
    );
    assert!(
        !text.contains("允许一次") && !text.contains("始终允许"),
        "no control survives while the withdrawal is unlanded: {text}"
    );
    assert!(
        posted_cards(&platform).await.is_empty(),
        "nothing is posted before the withdrawal lands: {text}"
    );

    // The retry lands; the request then takes the fresh path, delivers the
    // companion into the session's chat and surfaces the summary card.
    app.permission.sweep(&app.flow_handles(), &mut seen).await;
    assert!(
        backend.reply_permission_calls.lock().await.is_empty(),
        "the second sweep must not decline through the fresh path"
    );
    let final_card = last_update_of(&platform, "om_card")
        .await
        .expect("the retry paints the final receipt");
    assert!(
        card_text(&final_card).contains(crate::feishu::card::question::SUPERSEDED_PERMISSION_RECEIPT),
        "the retry paints the reprocessed receipt, not the retry placeholder: {}",
        card_text(&final_card)
    );
    assert!(
        posted_cards(&platform)
            .await
            .iter()
            .any(|card| { card_text(card).contains("富文本消息（Rich Text Message）") }),
        "the landed withdrawal is followed by the companion summary"
    );
}

/// A standalone unbacked card is previewed with the reprocess copy while its
/// superseded PATCH is retried, then painted with the final SUPERSEDED card —
/// never left on the refusal-copy placeholder.
#[tokio::test]
async fn restart_previews_a_standalone_card_before_its_superseded_retry() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let session_file = dir.path().join("sessions.json");

    let (app, _platform, _backend) = restart_app(&session_file, |_| {}).await;
    seed_session(&app, "ses_1", "/work").await;
    drop(app);

    seed_stale_surface(&session_file, "per_long", false, "om_standalone");

    let (app, platform, _backend) = restart_app(&session_file, |b| {
        b.ask_permission(over_budget_perm("per_long", "ses_1"));
    })
    .await;
    platform
        .fail_update_count
        .store(1, std::sync::atomic::Ordering::SeqCst);

    sweep(&app.permission, &app).await;

    let preview = last_update_of(&platform, "om_standalone")
        .await
        .expect("the standalone card is previewed");
    let text = card_text(&preview);
    assert!(
        text.contains(crate::feishu::card::question::SUPERSEDED_PERMISSION_PREVIEW_HEADER),
        "the card says it is being reprocessed: {text}"
    );
    assert!(
        !text.contains("自动拒绝") && !text.contains("目标过长"),
        "the preview must not borrow the refusal copy: {text}"
    );
    assert!(
        app.permission.sent_cards.lock().await.contains_key("per_long"),
        "the standalone record is kept for the retry"
    );

    // The retry paints the final SUPERSEDED card, then the fresh path delivers
    // the companion into the session's chat and surfaces the summary card as a
    // new standalone surface.
    sweep(&app.permission, &app).await;
    let final_card = last_update_of(&platform, "om_standalone")
        .await
        .expect("the retry paints the final card");
    assert!(
        card_text(&final_card).contains(crate::feishu::card::question::SUPERSEDED_PERMISSION_HEADER),
        "the final card is the reprocessed one: {}",
        card_text(&final_card)
    );
    assert!(
        app.permission.sent_cards.lock().await.contains_key("per_long"),
        "the fresh summary card is recorded as a new standalone surface"
    );
    assert!(
        posted_cards(&platform)
            .await
            .iter()
            .any(|card| { card_text(card).contains("富文本消息（Rich Text Message）") }),
        "the fresh path surfaces the companion summary"
    );
}

/// If the standalone preview PATCH also fails, the record and card stay for the
/// next sweep, which then paints the final SUPERSEDED card.
#[tokio::test]
async fn a_failed_standalone_preview_keeps_the_record_for_the_retry() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let session_file = dir.path().join("sessions.json");

    let (app, _platform, _backend) = restart_app(&session_file, |_| {}).await;
    seed_session(&app, "ses_1", "/work").await;
    drop(app);

    seed_stale_surface(&session_file, "per_long", false, "om_standalone");

    let (app, platform, _backend) = restart_app(&session_file, |b| {
        b.ask_permission(over_budget_perm("per_long", "ses_1"));
    })
    .await;
    // The superseded PATCH and its preview both fail.
    platform
        .fail_update_count
        .store(2, std::sync::atomic::Ordering::SeqCst);

    sweep(&app.permission, &app).await;

    assert!(
        app.permission.sent_cards.lock().await.contains_key("per_long"),
        "a failed preview keeps the standalone record"
    );
    assert_eq!(
        app.permission.pending_withdrawal_count().await,
        1,
        "the superseded withdrawal stays queued"
    );

    sweep(&app.permission, &app).await;
    let final_card = last_update_of(&platform, "om_standalone")
        .await
        .expect("the retry paints the final card");
    assert!(
        card_text(&final_card).contains(crate::feishu::card::question::SUPERSEDED_PERMISSION_HEADER),
        "the final card is the reprocessed one: {}",
        card_text(&final_card)
    );
    assert!(
        app.permission.sent_cards.lock().await.contains_key("per_long"),
        "the fresh summary card is recorded as a new standalone surface"
    );
    assert!(
        posted_cards(&platform)
            .await
            .iter()
            .any(|card| { card_text(card).contains("富文本消息（Rich Text Message）") }),
        "the retry surfaces the companion summary"
    );
}

/// `NotFound` on the fresh decline means the server no longer holds the
/// request — another client settled it. The old card was already withdrawn as
/// reprocessed, no reason card is posted, and cola claims no decision.
#[tokio::test]
async fn restart_withdraws_a_persisted_card_neutrally_when_another_client_answered() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let session_file = dir.path().join("sessions.json");

    let (app, _platform, _backend) = restart_app(&session_file, |_| {}).await;
    // The request's session is NOT mapped (so no fallback card can be
    // rendered); another session maps the directory so the sweep lists it.
    seed_session(&app, "ses_other", "/work").await;
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
        text.contains(crate::feishu::card::question::SUPERSEDED_PERMISSION_RECEIPT),
        "the old card says it was reprocessed: {text}"
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

/// A superseded-card withdrawal whose PATCH fails is retried from the pending
/// backlog, while the still-live old card is neutralized immediately with the
/// "will retry" receipt. The next sweep finishes the withdrawal and then takes
/// the fresh path (here: the fallback card), without duplicating the old card.
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

    assert!(
        backend.reply_permission_calls.lock().await.is_empty(),
        "the first sweep neither approves nor declines the old decision"
    );
    assert!(
        app.cards_handle()
            .card_handles
            .lock()
            .await
            .message_of("per_long")
            .is_some(),
        "the handle is kept so the pending SUPERSEDED retry can paint it"
    );

    // The retry lands, then the request takes the fresh path, delivers the
    // companion into the session's chat and surfaces the summary card.
    sweep(&app.permission, &app).await;
    assert!(
        backend.reply_permission_calls.lock().await.is_empty(),
        "the fresh path must not decline"
    );

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
        3,
        "failed withdrawal, immediate neutralization, successful retry"
    );
    let final_card = updates.last().expect("the successful retry");
    let text = card_text(final_card);
    assert!(
        text.contains(crate::feishu::card::question::SUPERSEDED_PERMISSION_RECEIPT),
        "the final receipt is the reprocessed one: {text}"
    );
    assert!(
        !text.contains("允许一次") && !text.contains("始终允许"),
        "the card ends non-interactive: {text}"
    );
    assert!(
        persisted(&session_file).unwrap_or_default().contains("per_long"),
        "the new summary surface is persisted"
    );
    assert!(
        posted_cards(&platform)
            .await
            .iter()
            .any(|card| { card_text(card).contains("富文本消息（Rich Text Message）") }),
        "the fresh path posts the companion summary once"
    );
}

/// Whole-branch re-review #883: one withdrawal PATCH may not wedge the sweep.
/// A hung Feishu call is bounded by the flow's timeout, the attempt counts as
/// failed, the handle is kept for the pending retry, and the same sweep
/// completes — otherwise a stalled PATCH would freeze the whole permission
/// poller with an old Allow card still clickable.
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
        "the handle is kept so the pending retry can paint the final receipt"
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
    let raw = persisted(&session_file).unwrap_or_default();
    let record: serde_json::Value = serde_json::from_str(&raw).unwrap_or(serde_json::Value::Null);
    assert!(
        record["inline"]["default"].get("per_long").is_none(),
        "the persisted inline record is gone: {raw}"
    );
    assert!(
        record["cards"]["default"].get("om_card").is_none(),
        "and so is the dead card's cached JSON: {raw}"
    );
    assert!(
        record["standalone"]["default"].get("per_long").is_some(),
        "the fresh fallback card is the remaining surface: {raw}"
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

/// Whole-branch re-review #883: the retry phase is bounded per sweep. A backlog
/// larger than the per-sweep attempt budget is finished across later sweeps
/// instead of delaying the listing with every entry at once.
#[tokio::test]
async fn the_withdrawal_retry_attempts_are_budgeted_per_sweep() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let session_file = dir.path().join("sessions.json");

    let (app, _platform, _backend) = restart_app(&session_file, |_| {}).await;
    seed_session(&app, "ses_1", "/work").await;
    drop(app);

    // Six requests whose first withdrawal PATCH fails: six backlog entries.
    const REQUESTS: usize = 6;
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
    // Each request tries two PATCHes: the superseded withdrawal, then the
    // immediate "will retry" neutralization.
    platform
        .fail_update_count
        .store(REQUESTS * 2, std::sync::atomic::Ordering::SeqCst);
    sweep(&app.permission, &app).await;
    assert_eq!(app.permission.pending_withdrawal_count().await, REQUESTS);

    // One sweep retries at most the documented per-sweep budget (4).
    sweep(&app.permission, &app).await;
    assert_eq!(
        app.permission.pending_withdrawal_count().await,
        REQUESTS - 4,
        "the first retry sweep attempts four entries, not the whole backlog"
    );

    sweep(&app.permission, &app).await;
    assert_eq!(
        app.permission.pending_withdrawal_count().await,
        0,
        "the remaining entries finish on the next sweep"
    );
}

/// Whole-branch re-review #883: one sweep's retry phase is also cut off by a
/// total time budget, so a single hung PATCH cannot stretch the sweep to the
/// per-PATCH timeout times the backlog.
#[tokio::test]
async fn the_withdrawal_retry_phase_is_time_bounded() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let session_file = dir.path().join("sessions.json");

    let (app, _platform, _backend) = restart_app(&session_file, |_| {}).await;
    seed_session(&app, "ses_1", "/work").await;
    drop(app);

    seed_stale_surface(&session_file, "per_long", true, "om_card");

    // First sweep: the decline lands, the PATCH fails, one entry is stored.
    let (app, platform, _backend) = restart_app(&session_file, |b| {
        b.ask_permission(over_budget_perm("per_long", "ses_1"));
    })
    .await;
    // Both the superseded withdrawal and its immediate neutralization fail, so
    // the old card stays live and the pending entry is retried.
    platform
        .fail_update_count
        .store(2, std::sync::atomic::Ordering::SeqCst);
    sweep(&app.permission, &app).await;
    assert_eq!(app.permission.pending_withdrawal_count().await, 1);

    // Second sweep: the retry hangs on the gate, and the phase budget cuts it
    // off long before the per-PATCH timeout would.
    app.permission
        .withdrawal_retry_budget_ms
        .store(50, std::sync::atomic::Ordering::SeqCst);
    app.permission
        .withdrawal_timeout_ms
        .store(10_000, std::sync::atomic::Ordering::SeqCst);
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
        .expect("the retry PATCH is in flight");
    tokio::time::timeout(std::time::Duration::from_secs(2), sweep)
        .await
        .expect("the retry phase's budget must end the sweep")
        .expect("the sweep task joins");
    assert_eq!(
        app.permission.pending_withdrawal_count().await,
        1,
        "the unreached entry stays for the next sweep"
    );
    release.notify_waiters();
}

/// Whole-branch re-review #883: a bare HTTP 404 is not proof the message is
/// gone (a proxy or endpoint outage can answer it), so the withdrawal state is
/// kept and retried rather than discarded alongside a still-clickable card.
#[tokio::test]
async fn a_bare_404_keeps_the_withdrawal_retryable() {
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
        .fail_update_http_404_count
        .store(1, std::sync::atomic::Ordering::SeqCst);
    sweep(&app.permission, &app).await;

    assert_eq!(
        app.permission.pending_withdrawal_count().await,
        1,
        "a bare 404 stays retryable"
    );
    // The immediate neutralization (a separate PATCH) lands, so the old card
    // stops being interactive even though the superseded withdrawal waits.
    assert!(
        app.cards_handle()
            .card_handles
            .lock()
            .await
            .message_of("per_long")
            .is_some(),
        "the handle is kept so the pending retry can paint the final receipt"
    );

    // The next sweep (no injected 404) finishes the withdrawal and takes the
    // fresh path.
    sweep(&app.permission, &app).await;
    assert_eq!(app.permission.pending_withdrawal_count().await, 0);
    let final_card = last_update_of(&platform, "om_card")
        .await
        .expect("the retry patched the card");
    let text = card_text(&final_card);
    assert!(
        text.contains(crate::feishu::card::question::SUPERSEDED_PERMISSION_RECEIPT),
        "and the card ends on the reprocessed receipt: {text}"
    );
    assert!(!text.contains("允许一次"), "no control survives: {text}");
}

/// The completeness guard also runs at the **click** path. A card can stay
/// live with its Allow buttons while the superseded-withdrawal PATCH is
/// retried; a click must re-read the request and refuse because no confirmed
/// companion exists, never approve content the card cannot show.
#[tokio::test]
async fn a_click_on_a_stale_allow_card_without_a_companion_is_refused() {
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
    // Worst window: both the superseded-card PATCH and its immediate
    // neutralization fail, so the old controls stay live and the withdrawal
    // waits for a retry.
    platform
        .fail_update_count
        .store(2, std::sync::atomic::Ordering::SeqCst);
    sweep(&app.permission, &app).await;
    assert!(
        backend.reply_permission_calls.lock().await.is_empty(),
        "the sweep neither approves nor declines the old decision"
    );
    assert_eq!(
        app.permission.pending_withdrawal_count().await,
        1,
        "the unsafe card is kept for a withdrawal retry"
    );

    // The user now clicks the old card's Allow button (the card is still live).
    let result = app
        .host_action(serde_json::json!({
            "action": "perm",
            "reply": "once",
            "session_id": "ses_1",
            "directory": "/work",
            "request_id": "per_long",
            "perm_label": "✅ 已允许一次",
            "perm_color": "green",
        }))
        .await
        .expect("the click produces a result");

    let calls = backend.reply_permission_calls.lock().await.clone();
    assert_eq!(
        calls,
        vec![("per_long".to_string(), "reject".to_string())],
        "the click is declined, never approved: {calls:?}"
    );
    assert_eq!(
        result.toast.as_deref(),
        Some("已拒绝：没有匹配的完整内容"),
        "the toast names the refusal"
    );
    let text = card_text(result.card.as_ref().expect("the result card"));
    assert!(
        text.contains(crate::feishu::card::question::MISSING_PERMISSION_CLICK_LABEL),
        "the result explains the forced refusal: {text}"
    );
    assert!(
        text.contains("没有找到已送达的伴随消息"),
        "the result names the missing companion as the reason: {text}"
    );
    assert!(
        !text.contains("已允许"),
        "nothing may claim the approval happened: {text}"
    );
}

/// The sweep resolves the inline host by walking the parent chain, which calls
/// `session_info`; that read must not happen under the owning Session's card
/// write lock. A stalled read otherwise holds the lock indefinitely, blocking
/// flushes, `/card` and install for that Session. The walk is resolved and
/// bounded before the lock, so the lock stays free and the request falls
/// through to its standalone card after the read times out.
#[tokio::test]
async fn a_stalled_parent_walk_never_holds_the_sweep_card_lock() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let mut backend = MockBackend::new(realistic_parts());
    backend.ask_permission(perm_request("per_1", "ses_test", "ls -la"));
    backend.hang_session_info_reads(usize::MAX);
    let backend = Arc::new(backend);
    let platform = Arc::new(RecordingPlatform::new());
    let app = Arc::new(App::new(cfg, backend.clone(), platform.clone()).unwrap());
    seed_session(&app, "ses_test", "/work").await;
    app.permission
        .list_timeout_ms
        .store(500, std::sync::atomic::Ordering::SeqCst);

    let handles = app.flow_handles();
    let sweep_app = app.clone();
    let sweep = tokio::spawn(async move {
        let mut seen = std::collections::HashSet::new();
        sweep_app
            .permission
            .sweep(&sweep_app.flow_handles(), &mut seen)
            .await;
        seen
    });
    // Wait until the sweep has actually reached the stalled read.
    for _ in 0..200 {
        if !backend.session_info_calls.lock().await.is_empty() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
    assert!(
        !backend.session_info_calls.lock().await.is_empty(),
        "the sweep reached the parent-chain walk"
    );

    // The Session's card write lock must be free while that read is stalled.
    let lock = handles.cards.write_lock("ses_test").await;
    let guard = tokio::time::timeout(std::time::Duration::from_millis(250), lock.lock())
        .await
        .expect("the parent walk must not hold the Session's card write lock");
    drop(guard);

    let seen = tokio::time::timeout(std::time::Duration::from_secs(2), sweep)
        .await
        .expect("the bounded walk must let the sweep finish")
        .unwrap();
    assert!(
        seen.contains("per_1"),
        "the request is surfaced on its standalone card after the walk times out"
    );
    assert_eq!(
        platform.sent_cards().await.len(),
        1,
        "the timed-out walk falls through to the standalone path: {:?}",
        platform.calls.lock().await
    );
}
