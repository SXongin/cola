//! `[bridge] default_auto_accept` (#513): a session cola **creates** starts
//! with Auto-Accept on, while a session it merely **adopts** keeps its own
//! state.

use crate::bridge::test_support::*;

fn chat_key() -> crate::config::ThreadKey {
    crate::config::ThreadKey::new("chat_1".into(), "chat_1".into())
}

async fn active_entry(app: &std::sync::Arc<App>) -> crate::config::SessionEntry {
    app.sessions
        .lock()
        .await
        .get_active(&chat_key())
        .cloned()
        .expect("a session should be mapped")
}

/// The opt-in marks a session created by a fresh chat's first message — and
/// that session answers its pending permissions without a card.
#[tokio::test]
async fn configured_default_marks_a_fresh_session_and_answers_permissions() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let mut cfg = test_config(&dir.path().join("sessions.json"));
    cfg.bridge.default_auto_accept = true;
    let mut mock = MockBackend::new(realistic_parts());
    mock.ask_permissions(vec![opencode::types::PermissionRequest {
        request_id: "per_default".into(),
        session_id: Some("ses_test".into()),
        permission: Some("bash".into()),
        patterns: vec!["ls".into()],
        metadata: None,
        always: Vec::new(),
    }]);
    let perm_calls = mock.reply_permission_calls.clone();
    let (app, platform) = build_app(cfg, mock).await;

    app.handle_message(incoming(
        "msg_1".into(),
        "chat_1".into(),
        "p2p".into(),
        None,
        "hi".into(),
        None,
    ))
    .await;
    assert!(
        active_entry(&app).await.auto_accept,
        "the configured default should mark the created session"
    );

    tokio::spawn({
        let app = app.clone();
        async move {
            app.permission
                .poll_interval_ms
                .store(50, std::sync::atomic::Ordering::Relaxed);
            let _ = app.permission.poll_loop(&app.flow_handles()).await;
        }
    });
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;

    assert_eq!(
        perm_calls.lock().await.clone(),
        vec![("per_default".to_string(), "once".to_string())],
        "an auto-accept session replies to its pending permission"
    );
    let sent = platform.calls.lock().await.clone();
    assert!(
        !sent.iter().any(|c| {
            if let PlatformCall::ReplyCard { card, .. } = c {
                card_text(card).contains("权限请求")
            } else {
                false
            }
        }),
        "no permission card should be sent for an auto-accept session: {sent:?}"
    );
}

/// Absent / explicit `false` keeps today's behavior: created sessions start
/// OFF, so an upgrade never changes permission behavior without consent.
#[tokio::test]
async fn absent_config_keeps_created_sessions_off() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let (app, _platform) = build_app(cfg, MockBackend::new(realistic_parts())).await;

    app.handle_message(incoming(
        "msg_1".into(),
        "chat_1".into(),
        "p2p".into(),
        None,
        "hi".into(),
        None,
    ))
    .await;

    assert!(!active_entry(&app).await.auto_accept);
}

/// `/new` declares a Pending Session that already carries the configured
/// default, and materialisation keeps the flag.
#[tokio::test]
async fn new_pending_carries_the_default_into_the_session() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let mut cfg = test_config(&dir.path().join("sessions.json"));
    cfg.bridge.default_auto_accept = true;
    let (app, _platform) = build_app(cfg, MockBackend::new(realistic_parts())).await;

    send_command(&app, "/new", "msg_new").await;
    {
        let store = app.sessions.lock().await;
        let pending = store
            .pending_for(&chat_key())
            .cloned()
            .expect("the pending is recorded");
        assert!(
            pending.auto_accept,
            "the pending should carry the configured default"
        );
    }

    app.handle_message(incoming(
        "msg_2".into(),
        "chat_1".into(),
        "p2p".into(),
        None,
        "go".into(),
        None,
    ))
    .await;
    assert!(
        active_entry(&app).await.auto_accept,
        "materialisation should keep the pending's flag"
    );
}

/// The pending's own toggle wins over the config default: `/autoaccept off`
/// after `/new` materialises OFF.
#[tokio::test]
async fn autoaccept_off_on_a_pending_wins_over_the_default() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let mut cfg = test_config(&dir.path().join("sessions.json"));
    cfg.bridge.default_auto_accept = true;
    let (app, _platform) = build_app(cfg, MockBackend::new(realistic_parts())).await;

    send_command(&app, "/new", "msg_new").await;
    send_command(&app, "/autoaccept off", "msg_off").await;
    {
        let store = app.sessions.lock().await;
        let pending = store.pending_for(&chat_key()).cloned().expect("pending");
        assert!(!pending.auto_accept, "the explicit off must win");
    }

    app.handle_message(incoming(
        "msg_2".into(),
        "chat_1".into(),
        "p2p".into(),
        None,
        "go".into(),
        None,
    ))
    .await;
    assert!(!active_entry(&app).await.auto_accept);
}

/// `/topic` declares its pending through the same constructor as `/new`, so
/// the configured default rides onto the topic's Pending Session too.
#[tokio::test]
async fn topic_pending_carries_the_default() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let mut cfg = test_config(&dir.path().join("sessions.json"));
    cfg.bridge.default_auto_accept = true;
    let (app, _platform) = build_app(cfg, MockBackend::new(realistic_parts())).await;
    let proj = tempfile::tempdir().unwrap();

    send_command(
        &app,
        &format!("/topic {}", proj.path().to_string_lossy()),
        "msg_topic",
    )
    .await;

    let topic_key = crate::config::ThreadKey::new("chat_1".into(), "omt_created_topic".into());
    let store = app.sessions.lock().await;
    let pending = store
        .pending_for(&topic_key)
        .cloned()
        .expect("the topic pending is recorded");
    assert!(
        pending.auto_accept,
        "the topic pending should carry the configured default"
    );
}

/// Adoption never applies the default: a foreign session cola did not create
/// stays OFF, so the config cannot turn somebody else's session into
/// auto-approved.
#[tokio::test]
async fn adoption_keeps_the_sessions_own_state() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let mut cfg = test_config(&dir.path().join("sessions.json"));
    cfg.bridge.default_auto_accept = true;
    let mut backend = MockBackend::new(realistic_parts());
    backend.given_sessions(vec![list_session(
        "ses_alpha01",
        "唯一外部标题",
        "/work/ext",
        100,
    )]);
    let (app, _platform) = build_app(cfg, backend).await;

    send_command(&app, "/switch 唯一外部标题", "msg_switch").await;

    let entry = active_entry(&app).await;
    assert_eq!(entry.session_id, "ses_alpha01");
    assert!(
        !entry.auto_accept,
        "adoption must not apply the create-time default"
    );
}
