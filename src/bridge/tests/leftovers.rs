//! #187 (ADR-0038): when a cola turn ends without completing (abort via
//! `/stop`, interrupt, prompt error), the pending Permission/Question requests
//! it left behind must be rejected at the source — the tool fiber is dead, so
//! an answer would reach nobody, and a still-pending request only lets the next
//! turn re-host a ghost block (#177). Their blocks settle into `🚫 已拒绝`
//! Interaction Receipts (not the neutral `⏱ 已由其他客户端处理` line: cola
//! itself decided).
//!
//! The tests hold a turn in flight with `MockBackend::prompt_gate`, inline the
//! request through the real permission/question sweep (the mid-turn ordering of
//! production), then release the prompt into its scripted abort.

use crate::bridge::test_support::*;

/// A minimal one-question request for `session_id`.
fn question_request(id: &str, session_id: &str) -> opencode::types::QuestionRequest {
    opencode::types::QuestionRequest {
        id: id.into(),
        session_id: session_id.into(),
        questions: vec![opencode::types::QuestionInfo {
            question: "选择目录".into(),
            header: "目录".into(),
            options: vec![opencode::types::QuestionOption {
                label: "/a".into(),
                description: String::new(),
            }],
            multiple: None,
            custom: None,
        }],
    }
}

/// The card accumulator the in-flight turn streams into, or a panic when the
/// turn never got that far.
async fn wait_for_turn_card(app: &Arc<App>, session_id: &str) {
    for _ in 0..200 {
        if app.cards.lock().await.contains_key(session_id) {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    panic!("the turn's card never appeared");
}

/// One turn whose prompt is held at the backend's gate until the test releases
/// it; the scripted outcome is the caller's `prompt_error` (an abort, in
/// production's `/stop` shape).
fn gated_abort(app: &Arc<App>) -> tokio::task::JoinHandle<()> {
    let app = Arc::clone(app);
    tokio::spawn(async move {
        app.handle_message(incoming(
            "msg_1".into(),
            "chat_1".into(),
            "p2p".into(),
            None,
            "跑个命令".into(),
            None,
        ))
        .await;
    })
}

/// `/stop` with an inline permission pending: the request is rejected at the
/// source, the card carries the denial receipt, and the next turn does not
/// re-host the block.
#[tokio::test]
async fn aborted_turn_rejects_its_pending_permission() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let gate = Arc::new(tokio::sync::Semaphore::new(0));
    let mut backend = MockBackend::new(realistic_parts());
    // The first turn aborts (the mock's scripted stand-in for the server
    // returning the AbortedError), the second one runs normally.
    backend
        .fail_prompt_count
        .store(1, std::sync::atomic::Ordering::SeqCst);
    backend.prompt_gate = Some(Arc::clone(&gate));
    backend.permissions = vec![perm_request("per_1", "ses_test", "ls -la")];
    let replies = backend.reply_permission_calls.clone();
    let replied = backend.replied_permissions.clone();
    let interrupts = backend.interrupt_calls.clone();
    let (app, platform) = build_app(cfg, backend).await;

    let turn = gated_abort(&app);
    wait_for_turn_card(&app, "ses_test").await;
    // The poller inlines the permission on the live card, exactly as it does
    // while the server blocks the prompt.
    let mut seen = std::collections::HashSet::new();
    app.permission.sweep(&app.core, &mut seen).await;
    assert!(
        !app.cards
            .lock()
            .await
            .get("ses_test")
            .unwrap()
            .acc
            .live_permissions()
            .is_empty(),
        "the permission must be live on the card before the abort"
    );

    // /stop through the real command path interrupts the in-flight prompt.
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
        interrupts.lock().await.as_slice(),
        &["ses_test".to_string()],
        "/stop must interrupt the running session"
    );
    // The aborted prompt returns: the turn finishes and settles its leftovers.
    gate.add_permits(1);
    turn.await.unwrap();

    assert_eq!(
        replies.lock().await.as_slice(),
        &[("per_1".to_string(), "reject".to_string())],
        "the aborted turn must reject its pending permission"
    );
    assert!(
        replied.lock().await.contains("per_1"),
        "the server must no longer list the rejected request"
    );
    let card = final_card(&platform).await.to_string();
    assert!(
        card.contains("🚫 已拒绝：⚡ 执行 Shell 命令 `ls -la`"),
        "the block must settle into a denial receipt: {card}"
    );
    assert!(
        !card.contains("🔐 **权限请求**") && !card.contains("允许一次"),
        "no live controls may survive on the card: {card}"
    );

    // The next turn must not re-host the block: the server dropped it, so the
    // sweep finds nothing to move onto the fresh card.
    gate.add_permits(1);
    app.handle_message(incoming(
        "msg_2".into(),
        "chat_1".into(),
        "p2p".into(),
        None,
        "再来一次".into(),
        None,
    ))
    .await;
    let mut seen = std::collections::HashSet::new();
    app.permission.sweep(&app.core, &mut seen).await;
    assert!(
        app.cards
            .lock()
            .await
            .get("ses_test")
            .unwrap()
            .acc
            .live_permissions()
            .is_empty(),
        "the rejected request must not come back on the next turn"
    );
    assert!(
        !final_card(&platform)
            .await
            .to_string()
            .contains("🔐 **权限请求**"),
        "the next turn's card must not host the ghost block"
    );
}

/// A sub-task child's pending request is the aborted turn's to reject: the
/// ownership filter walks the parent chain, like `/autoaccept`'s approval.
#[tokio::test]
async fn aborted_turn_rejects_a_child_sessions_request() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let mut backend = MockBackend::new(realistic_parts());
    backend.prompt_error = Some("Aborted".into());
    backend.permissions = vec![perm_request("per_child", "ses_child", "ls -la")];
    backend
        .session_parents
        .insert("ses_child".into(), "ses_test".into());
    let replies = backend.reply_permission_calls.clone();
    let (app, _platform) = build_app(cfg, backend).await;

    app.handle_message(incoming(
        "msg_1".into(),
        "chat_1".into(),
        "p2p".into(),
        None,
        "跑个命令".into(),
        None,
    ))
    .await;

    assert_eq!(
        replies.lock().await.as_slice(),
        &[("per_child".to_string(), "reject".to_string())],
        "a sub-task descendant's request belongs to the aborted turn"
    );
}

/// The same abort with a pending question: rejected at the source, the card
/// carries `🚫 已拒绝`, and the in-flight question state is dropped.
#[tokio::test]
async fn aborted_turn_rejects_its_pending_question() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let gate = Arc::new(tokio::sync::Semaphore::new(0));
    let mut backend = MockBackend::new(realistic_parts());
    backend.prompt_error = Some("Aborted".into());
    backend.prompt_gate = Some(Arc::clone(&gate));
    backend.questions = vec![question_request("que_1", "ses_test")];
    let replies = backend.reply_question_calls.clone();
    let replied = backend.replied_questions.clone();
    let (app, platform) = build_app(cfg, backend).await;

    let turn = gated_abort(&app);
    wait_for_turn_card(&app, "ses_test").await;
    let mut seen = std::collections::HashSet::new();
    app.question.sweep(&app.core, &mut seen).await;

    gate.add_permits(1);
    turn.await.unwrap();

    assert_eq!(
        replies.lock().await.as_slice(),
        &[("que_1".to_string(), vec![vec!["__reject__".to_string()]])],
        "the aborted turn must reject its pending question"
    );
    assert!(replied.lock().await.contains("que_1"));
    assert!(
        !app.question.has_question("que_1").await,
        "the rejected question's in-flight state must be dropped"
    );
    let card = final_card(&platform).await.to_string();
    assert!(
        card.contains("🚫 已拒绝：目录"),
        "the question block must settle into a denial receipt: {card}"
    );
    assert!(
        !card.contains("选择目录") && !card.contains("提交"),
        "no live controls may survive on the card: {card}"
    );
}

/// Only the aborted turn's own requests are rejected: a request of another
/// session in the same project stays untouched.
#[tokio::test]
async fn aborted_turn_leaves_another_sessions_request() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let mut backend = MockBackend::new(realistic_parts());
    backend.prompt_error = Some("Aborted".into());
    backend.permissions = vec![
        perm_request("per_mine", "ses_test", "ls -la"),
        perm_request("per_other", "ses_other", "rm -rf /"),
    ];
    let replies = backend.reply_permission_calls.clone();
    let (app, _platform) = build_app(cfg, backend).await;

    app.handle_message(incoming(
        "msg_1".into(),
        "chat_1".into(),
        "p2p".into(),
        None,
        "跑个命令".into(),
        None,
    ))
    .await;

    assert_eq!(
        replies.lock().await.as_slice(),
        &[("per_mine".to_string(), "reject".to_string())],
        "another session's request must never be answered by this turn"
    );
}

/// A snapshot-claimed request belongs to the snapshot card's lifecycle
/// (ADR-0038, rule 6): the aborted turn must not reject it.
#[tokio::test]
async fn aborted_turn_leaves_a_claimed_request() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let mut backend = MockBackend::new(realistic_parts());
    backend.prompt_error = Some("Aborted".into());
    backend.permissions = vec![perm_request("per_claimed", "ses_test", "ls -la")];
    let replies = backend.reply_permission_calls.clone();
    let (app, _platform) = build_app(cfg, backend).await;
    app.core.snapshot_claims.lock().await.claim(
        "mid_snap",
        "已接管",
        "接管",
        &crate::bridge::snapshot::SnapshotData {
            session_id: "ses_test".into(),
            directory: "/work".into(),
            status: None,
            pending: vec![crate::bridge::request::PendingRequest::Permission(perm_request(
                "per_claimed",
                "ses_test",
                "ls -la",
            ))],
            tail: Vec::new(),
            newest_user_epoch: None,
            newest_user_is_cola_authored: false,
        },
    );

    app.handle_message(incoming(
        "msg_1".into(),
        "chat_1".into(),
        "p2p".into(),
        None,
        "跑个命令".into(),
        None,
    ))
    .await;

    assert!(
        replies.lock().await.is_empty(),
        "a claimed request is the snapshot's to resolve"
    );
}

/// A list call that fails or times out says nothing: the turn must not reject
/// what it cannot see (#130/#144 — unknown is never read as resolved).
#[tokio::test]
async fn aborted_turn_keeps_requests_when_the_list_fails() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let mut backend = MockBackend::new(realistic_parts());
    backend.prompt_error = Some("Aborted".into());
    backend.permissions = vec![perm_request("per_1", "ses_test", "ls -la")];
    backend
        .hang_list_permissions
        .store(1, std::sync::atomic::Ordering::SeqCst);
    let replies = backend.reply_permission_calls.clone();
    let (app, _platform) = build_app(cfg, backend).await;
    app.permission
        .list_timeout_ms
        .store(50, std::sync::atomic::Ordering::Relaxed);

    app.handle_message(incoming(
        "msg_1".into(),
        "chat_1".into(),
        "p2p".into(),
        None,
        "跑个命令".into(),
        None,
    ))
    .await;

    assert!(
        replies.lock().await.is_empty(),
        "an unanswered list must leave the request pending"
    );
}

/// A turn that completes leaves a pending request alone: only an unfinished
/// turn owns the rejections (the request may belong to concurrent work).
#[tokio::test]
async fn completed_turn_keeps_a_pending_request() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let mut backend = MockBackend::new(realistic_parts());
    backend.permissions = vec![perm_request("per_1", "ses_test", "ls -la")];
    let replies = backend.reply_permission_calls.clone();
    let (app, _platform) = build_app(cfg, backend).await;

    app.handle_message(incoming(
        "msg_1".into(),
        "chat_1".into(),
        "p2p".into(),
        None,
        "跑个命令".into(),
        None,
    ))
    .await;

    assert!(
        replies.lock().await.is_empty(),
        "a completed turn must not reject pending requests"
    );
}
