use crate::bridge::test_support::*;

#[tokio::test]
async fn group_root_message_creates_lobby_session_and_shows_guidance() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let (app, platform) = build_app(cfg, MockBackend::new(realistic_parts())).await;

    // A top-level group message (no thread_id) is the group "lobby".
    app.handle_message(incoming(
        "msg_1".into(),
        "oc_group_1".into(),
        "group".into(),
        None,
        "hi".into(),
        None,
    ))
    .await;

    // Guidance text is replied once.
    let calls = platform.calls.lock().await.clone();
    let guidance: Vec<_> = calls
        .iter()
        .filter_map(|c| match c {
            PlatformCall::ReplyText { text, .. } => Some(text.clone()),
            _ => None,
        })
        .collect();
    assert!(
        guidance.iter().any(|t| t.contains("已创建群会话")),
        "expected lobby guidance, got: {:?}",
        guidance
    );

    // Session lives under the lobby key (chat_id == thread_id).
    let lobby_key = crate::config::ThreadKey::new("oc_group_1".into(), "oc_group_1".into());
    let entry = app
        .sessions
        .lock()
        .await
        .get_active(&lobby_key)
        .cloned()
        .expect("lobby session created");
    assert_eq!(entry.session_id, "ses_test");
}

#[tokio::test]
async fn group_root_guidance_shown_once_per_lobby() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let (app, platform) = build_app(cfg, MockBackend::new(realistic_parts())).await;

    app.handle_message(incoming(
        "msg_1".into(),
        "oc_group_1".into(),
        "group".into(),
        None,
        "hi".into(),
        None,
    ))
    .await;
    app.handle_message(incoming(
        "msg_2".into(),
        "oc_group_1".into(),
        "group".into(),
        None,
        "again".into(),
        None,
    ))
    .await;

    let calls = platform.calls.lock().await.clone();
    let guidance: Vec<_> = calls
        .iter()
        .filter_map(|c| match c {
            PlatformCall::ReplyText { text, .. } => Some(text.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(
        guidance.len(),
        1,
        "guidance must be one-time, got: {:?}",
        guidance
    );
}

#[tokio::test]
async fn p2p_top_level_message_gets_no_guidance() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let (app, platform) = build_app(cfg, MockBackend::new(realistic_parts())).await;

    app.handle_message(incoming(
        "msg_1".into(),
        "oc_p2p_1".into(),
        "p2p".into(),
        None,
        "hi".into(),
        None,
    ))
    .await;

    let calls = platform.calls.lock().await.clone();
    let guidance: Vec<_> = calls
        .iter()
        .filter_map(|c| match c {
            PlatformCall::ReplyText { text, .. } => Some(text.clone()),
            _ => None,
        })
        .collect();
    assert!(
        guidance.is_empty(),
        "p2p must not show lobby guidance, got: {:?}",
        guidance
    );
}

#[tokio::test]
async fn topic_message_isolates_session_from_lobby() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let (app, platform) = build_app(cfg, MockBackend::new(realistic_parts())).await;

    // Seed distinct sessions for the lobby key and the topic key.
    seed_entry(
        &app,
        crate::config::SessionEntry {
            thread_key: crate::config::ThreadKey::new("oc_group_1".into(), "oc_group_1".into()),
            session_id: "ses_lobby".into(),
            directory: "/tmp/lobby".into(),
            agent: None,
            model: None,
            auto_accept: false,
            topic_anchor: None,
            topic_root: None,
            variant: None,
        },
    )
    .await;
    seed_entry(
        &app,
        crate::config::SessionEntry {
            thread_key: crate::config::ThreadKey::new("oc_group_1".into(), "omt_topic_1".into()),
            session_id: "ses_topic".into(),
            directory: "/tmp/topic".into(),
            agent: None,
            model: None,
            auto_accept: false,
            topic_anchor: None,
            topic_root: None,
            variant: None,
        },
    )
    .await;

    // Lobby message routes to the lobby session; topic message routes to
    // the topic session — never creating or switching across.
    app.handle_message(incoming(
        "msg_1".into(),
        "oc_group_1".into(),
        "group".into(),
        None,
        "hi".into(),
        None,
    ))
    .await;
    app.handle_message(incoming(
        "msg_2".into(),
        "oc_group_1".into(),
        "group".into(),
        Some("omt_topic_1".into()),
        "refactor".into(),
        None,
    ))
    .await;

    let store = app.sessions.lock().await;
    let lobby = store
        .get_active(&crate::config::ThreadKey::new(
            "oc_group_1".into(),
            "oc_group_1".into(),
        ))
        .cloned()
        .unwrap();
    let topic = store
        .get_active(&crate::config::ThreadKey::new(
            "oc_group_1".into(),
            "omt_topic_1".into(),
        ))
        .cloned()
        .unwrap();
    assert_eq!(lobby.session_id, "ses_lobby");
    assert_eq!(topic.session_id, "ses_topic");
    assert_ne!(lobby.thread_key, topic.thread_key);
    drop(store);

    // No guidance: the lobby session already existed.
    let calls = platform.calls.lock().await.clone();
    let guidance: Vec<_> = calls
        .iter()
        .filter_map(|c| match c {
            PlatformCall::ReplyText { text, .. } => Some(text.clone()),
            _ => None,
        })
        .collect();
    assert!(
        guidance.is_empty(),
        "no guidance when lobby exists, got: {:?}",
        guidance
    );
}

#[tokio::test]
async fn p2p_topic_isolated_from_p2p_top_level() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let (app, _platform) = build_app(cfg, MockBackend::new(realistic_parts())).await;

    // Seed a p2p top-level session and a p2p topic session.
    seed_entry(
        &app,
        crate::config::SessionEntry {
            thread_key: crate::config::ThreadKey::new("oc_p2p_1".into(), "oc_p2p_1".into()),
            session_id: "ses_top".into(),
            directory: "/tmp/top".into(),
            agent: None,
            model: None,
            auto_accept: false,
            topic_anchor: None,
            topic_root: None,
            variant: None,
        },
    )
    .await;
    seed_entry(
        &app,
        crate::config::SessionEntry {
            thread_key: crate::config::ThreadKey::new("oc_p2p_1".into(), "omt_p2p_1".into()),
            session_id: "ses_p2p_topic".into(),
            directory: "/tmp/ptopic".into(),
            agent: None,
            model: None,
            auto_accept: false,
            topic_anchor: None,
            topic_root: None,
            variant: None,
        },
    )
    .await;

    app.handle_message(incoming(
        "msg_1".into(),
        "oc_p2p_1".into(),
        "p2p".into(),
        None,
        "hi".into(),
        None,
    ))
    .await;
    app.handle_message(incoming(
        "msg_2".into(),
        "oc_p2p_1".into(),
        "p2p".into(),
        Some("omt_p2p_1".into()),
        "topic hi".into(),
        None,
    ))
    .await;

    let store = app.sessions.lock().await;
    let top = store
        .get_active(&crate::config::ThreadKey::new(
            "oc_p2p_1".into(),
            "oc_p2p_1".into(),
        ))
        .cloned()
        .unwrap();
    let topic = store
        .get_active(&crate::config::ThreadKey::new(
            "oc_p2p_1".into(),
            "omt_p2p_1".into(),
        ))
        .cloned()
        .unwrap();
    assert_eq!(top.session_id, "ses_top");
    assert_eq!(topic.session_id, "ses_p2p_topic");
    assert_ne!(top.thread_key, topic.thread_key);
}

#[tokio::test]
async fn stale_session_mapping_is_recreated_on_404() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let mut backend = MockBackend::new(realistic_parts());
    backend.session_id = "ses_new".into();
    backend.stale_session_404 = true;
    let (app, platform) = build_app(cfg, backend).await;

    // Seed stale mappings: thread -> ses_old (active), then ses_old2. When
    // ses_old 404s, cola must create a FRESH session, not fall through to
    // the next stale mapping.
    let thread = crate::config::ThreadKey::new("chat_1".into(), "chat_1".into());
    seed_entry(
        &app,
        crate::config::SessionEntry {
            thread_key: thread.clone(),
            session_id: "ses_old2".into(),
            directory: "/tmp/old2".into(),
            agent: None,
            model: None,
            auto_accept: false,
            topic_anchor: None,
            topic_root: None,
            variant: None,
        },
    )
    .await;
    seed_entry(
        &app,
        crate::config::SessionEntry {
            thread_key: thread.clone(),
            session_id: "ses_old".into(),
            directory: "/tmp/old".into(),
            agent: None,
            model: None,
            auto_accept: false,
            topic_anchor: None,
            topic_root: None,
            variant: None,
        },
    )
    .await;

    app.handle_message(incoming(
        "msg_1".into(),
        "chat_1".into(),
        "p2p".into(),
        None,
        "hi".into(),
        None,
    ))
    .await;

    // The prompt on the stale session 404s; cola must recreate the session
    // and retry, landing on a Done card instead of an error.
    let calls = platform.calls.lock().await.clone();
    let updates: Vec<_> = calls
        .iter()
        .filter_map(|c| match c {
            PlatformCall::UpdateMessage { card, .. } => Some(card.clone()),
            _ => None,
        })
        .collect();
    assert!(!updates.is_empty(), "expected a card update, got: {:?}", calls);
    let card = updates.last().unwrap().to_string();
    assert!(card.contains("✅"), "expected a Done card, got: {}", card);

    // The store must now map the thread to the recreated session.
    let sid = app
        .sessions
        .lock()
        .await
        .get_active(&thread)
        .map(|e| e.session_id.clone());
    assert_eq!(sid.as_deref(), Some("ses_new"));

    // The busy guard must not linger on the dead id or stay held on the fresh
    // one: recreate moved it and finish released it.
    let inflight = app.inflight.lock().await;
    assert!(!inflight.contains("ses_old"), "dead id guard must be released");
    assert!(!inflight.contains("ses_new"), "fresh id guard must be released");
}

/// ADR-0023: when a cola-created topic's session 404s and is recreated, the
/// topic's creation messages (`topic_anchor`/`topic_root`) survive the
/// recreate — they are Feishu message ids, not session state — so the
/// quote-injection guard keeps working for later replies in the topic.
#[tokio::test]
async fn stale_topic_recreate_preserves_creation_messages() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let mut backend = MockBackend::new(realistic_parts());
    backend.session_id = "ses_new".into();
    backend.stale_session_404 = true;
    let prompt_calls = backend.prompt_calls.clone();
    let (app, _platform) = build_app(cfg, backend).await;

    let topic_key = crate::config::ThreadKey::new("chat_1".into(), "omt_t_1".into());
    seed_entry(
        &app,
        crate::config::SessionEntry {
            thread_key: topic_key.clone(),
            session_id: "ses_old".into(),
            directory: "/work/topic".into(),
            agent: None,
            model: None,
            auto_accept: false,
            topic_anchor: Some("om_seed".into()),
            topic_root: Some("om_root_cmd".into()),
            variant: None,
        },
    )
    .await;
    // The cover-title sync state belongs to the topic, not the session —
    // it must move to the recreated session so the hook keeps patching.
    seed_cover_title(&app, "ses_old", "旧标题").await;

    // The first message 404s on the stale session; cola recreates and retries.
    app.handle_message(crate::bridge::IncomingMessage {
        message_id: "msg_1".into(),
        chat_id: "chat_1".into(),
        chat_type: "group".into(),
        thread_id: Some("omt_t_1".into()),
        parent_id: Some("om_root_cmd".into()),
        text: "第一条".into(),
        images: vec![],
        requester_open_id: None,
    })
    .await;

    // The recreated entry keeps the topic's creation messages (ADR-0023).
    let entry = app
        .sessions
        .lock()
        .await
        .get_active(&topic_key)
        .cloned()
        .expect("recreated session mapped to the topic");
    assert_eq!(entry.session_id, "ses_new");
    assert_eq!(entry.topic_anchor.as_deref(), Some("om_seed"));
    assert_eq!(entry.topic_root.as_deref(), Some("om_root_cmd"));
    // The cover-title sync state moved to the recreated session id.
    let covers = app.core.cover_titles.lock().await.clone();
    assert!(!covers.contains_key("ses_old"), "stale cover title must move");
    assert_eq!(covers.get("ses_new").map(|c| c.title.as_str()), Some("旧标题"));

    // A later plain reply pointing at the root must still skip injection.
    prompt_calls.lock().await.clear();
    app.handle_message(crate::bridge::IncomingMessage {
        message_id: "msg_2".into(),
        chat_id: "chat_1".into(),
        chat_type: "group".into(),
        thread_id: Some("omt_t_1".into()),
        parent_id: Some("om_root_cmd".into()),
        text: "第二条".into(),
        images: vec![],
        requester_open_id: None,
    })
    .await;
    assert_eq!(
        *prompt_calls.lock().await,
        vec!["第二条".to_string()],
        "the recreated entry must still exclude the topic root from Quoted Context"
    );
}

/// When a turn is already in flight, a new message must NOT start a
/// competing run_prompt (which would overwrite the running accumulator and
/// race on the same card). It goes through the supplement path: the message
/// is sent fire-and-forget via prompt_async (OpenCode merges it into the
/// current turn) and the user gets a notice — no Loading card, no second
/// accumulator.
#[tokio::test]
async fn message_during_inflight_goes_to_supplement_path() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let backend = MockBackend::new(realistic_parts());
    let sup_calls = backend.prompt_async_calls.clone();
    let sup_ids = backend.prompt_async_message_ids.clone();
    let (app, platform) = build_app(cfg, backend).await;
    seed_entry(
        &app,
        crate::config::SessionEntry {
            thread_key: crate::config::ThreadKey::new("chat_1".into(), "chat_1".into()),
            session_id: "ses_test".into(),
            directory: "/tmp/aa".into(),
            agent: None,
            model: None,
            auto_accept: false,
            topic_anchor: None,
            topic_root: None,
            variant: None,
        },
    )
    .await;
    app.inflight.lock().await.insert("ses_test".to_string());

    app.handle_message(incoming(
        "msg_sup".into(),
        "chat_1".into(),
        "p2p".into(),
        None,
        "补充一下，改用方案 B".into(),
        None,
    ))
    .await;

    // prompt_async was called with the supplement text.
    let calls = sup_calls.lock().await.clone();
    assert!(
        calls.iter().any(|c| c.contains("补充一下，改用方案 B")),
        "supplement text must be sent via prompt_async: {:?}",
        calls
    );
    // The supplement is a cola-authored message: fresh `msg_cola_` id
    // (ADR-0026).
    let ids = sup_ids.lock().await.clone();
    assert!(
        ids.iter()
            .all(|id| id.as_deref().is_some_and(|i| i.starts_with("msg_cola_"))),
        "every supplement must self-identify as cola-authored: {:?}",
        ids
    );

    // NO Loading card / run_prompt was started for the supplement message.
    let sent = platform.calls.lock().await.clone();
    assert!(
        sent.iter().all(|c| matches!(c, PlatformCall::ReplyText { .. })),
        "supplement must only reply text, not start a card: {:?}",
        sent
    );
    // The in-flight marker is preserved (still running).
    assert!(app.inflight.lock().await.contains("ses_test"));
}
