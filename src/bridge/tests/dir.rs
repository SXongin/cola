use crate::bridge::test_support::*;

#[tokio::test]
async fn new_session_uses_configured_work_dir() {
    let dir = tempfile::tempdir().unwrap();
    let mut cfg = test_config(&dir.path().join("sessions.json"));
    let work = dir.path().join("work");
    cfg.bridge.work_dir = Some(work.clone());

    // No chdir: the session directory must come from [bridge] work_dir, not
    // the process cwd.
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

    let thread = crate::config::ThreadKey::new("chat_1".into(), "chat_1".into());
    let entry = app
        .sessions
        .lock()
        .await
        .get_active(&thread)
        .cloned()
        .expect("a session should have been created");
    assert_eq!(entry.directory, work.to_string_lossy().to_string());
}

/// `/new` in a conversation whose active session lives in a project must
/// inherit that project's directory (ADR-0012) — NOT the configured
/// work_dir. Only a conversation with no session falls back to work_dir.
#[tokio::test]
async fn new_command_inherits_active_sessions_directory() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let mut cfg = test_config(&dir.path().join("sessions.json"));
    let work = dir.path().join("work");
    cfg.bridge.work_dir = Some(work.clone());
    let (app, _platform) = build_app(cfg, MockBackend::new(realistic_parts())).await;

    let thread_key = crate::config::ThreadKey::new("chat_1".into(), "chat_1".into());
    let proj = tempfile::tempdir().unwrap();
    let proj_dir = proj.path().to_string_lossy().to_string();

    // First `/dir <proj>` roots a session in the project.
    crate::bridge::command::handle_command(
        &app.core,
        crate::bridge::command::Command::Dir(proj_dir.clone()),
        thread_key.clone(),
        "msg_dir",
        crate::config::ConversationKind::P2p,
    )
    .await
    .unwrap();

    // Then `/new` must stay in the project, not jump back to work_dir.
    crate::bridge::command::handle_command(
        &app.core,
        crate::bridge::command::Command::New(None),
        thread_key.clone(),
        "msg_new",
        crate::config::ConversationKind::P2p,
    )
    .await
    .unwrap();

    let entry = app
        .sessions
        .lock()
        .await
        .get_active(&thread_key)
        .cloned()
        .expect("a session should be active after /new");
    // normalize_directory canonicalizes the project path (resolving
    // /private/var on macOS and \\?\ / 8.3 short names on Windows), so
    // compare against the canonicalized form — not the raw tempdir path.
    let canonical = std::fs::canonicalize(proj.path()).unwrap();
    assert_eq!(
        entry.directory,
        canonical.to_string_lossy(),
        "/new must inherit the active session's directory, not work_dir"
    );
    assert_ne!(
        entry.directory,
        work.to_string_lossy().to_string(),
        "/new must NOT fall back to work_dir when a session is active"
    );
}

/// `/new` in a conversation with NO active session still falls back to the
/// configured work_dir (the fresh-machine / fresh-topic case).
#[tokio::test]
async fn new_command_falls_back_to_work_dir_without_active_session() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let mut cfg = test_config(&dir.path().join("sessions.json"));
    let work = dir.path().join("work");
    cfg.bridge.work_dir = Some(work.clone());
    let (app, _platform) = build_app(cfg, MockBackend::new(realistic_parts())).await;

    let thread_key = crate::config::ThreadKey::new("chat_1".into(), "chat_1".into());

    crate::bridge::command::handle_command(
        &app.core,
        crate::bridge::command::Command::New(None),
        thread_key.clone(),
        "msg_new",
        crate::config::ConversationKind::P2p,
    )
    .await
    .unwrap();

    let entry = app
        .sessions
        .lock()
        .await
        .get_active(&thread_key)
        .cloned()
        .expect("a session should be active after /new");
    assert_eq!(entry.directory, work.to_string_lossy().to_string());
}

/// `dir_card_data` derives Recent Directories from the shared store:
/// children and archived sessions excluded, deduped by directory (keeping
/// the latest activity), sorted most-recent-first.
#[tokio::test]
async fn dir_card_data_dedupes_sorts_and_filters() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let mut backend = MockBackend::new(realistic_parts());
    let mut child = list_session("ses_child", "子任务", "/work/a", 999);
    child.parent_id = Some("ses_root".into());
    let mut archived = list_session("ses_arch", "归档", "/work/arch", 888);
    archived.time = Some(opencode::client::SessionTime {
        created: 1,
        updated: 888,
        archived: Some(1),
    });
    backend.session_list = vec![
        list_session("ses_a1", "A1", "/work/a", 100),
        list_session("ses_b", "B", "/work/b", 200),
        list_session("ses_a2", "A2", "/work/a", 300),
        child,
        archived,
    ];
    let (app, _platform) = build_app(cfg, backend).await;
    let key = crate::config::ThreadKey::new("chat_1".into(), "chat_1".into());

    let (dirs, current) = crate::bridge::command::dir_card_data(&app.core, &key).await;
    assert_eq!(dirs, vec!["/work/a".to_string(), "/work/b".to_string()]);
    assert_eq!(current, None);
}

/// The `/dir` Recent Directories card's `pick` op re-roots the thread into
/// the picked directory: it creates a NEW session there (matching the text
/// `/dir <path>` form), maps it active, and refreshes the card in place.
#[tokio::test]
async fn dir_card_pick_creates_session_and_refreshes_card() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let mut backend = MockBackend::new(realistic_parts());
    backend.session_list = vec![
        list_session("ses_a", "项目A", "/work/a", 100),
        list_session("ses_b", "项目B", "/work/b", 200),
    ];
    let (app, _platform) = build_app(cfg, backend).await;

    let value = serde_json::json!({
        "action": "dir",
        "op": "pick",
        "chat_id": "chat_1",
        "thread_id": "chat_1",
        "directory": "/work/b",
    });
    let result = app
        .handle_card_action(value)
        .await
        .expect("dir pick should return a result");
    assert!(result.card.is_some(), "dir pick refreshes the card");
    assert!(
        result.toast.clone().unwrap_or_default().contains("已切换目录"),
        "dir pick toasts: {:?}",
        result.toast
    );
    let key = crate::config::ThreadKey::new("chat_1".into(), "chat_1".into());
    let entry = app
        .sessions
        .lock()
        .await
        .get_active(&key)
        .cloned()
        .expect("dir pick maps the new session active");
    assert_eq!(entry.directory, "/work/b");
    // The refreshed card marks the new directory as current.
    let card_str = result.card.unwrap().to_string();
    assert!(
        card_str.contains("当前"),
        "refreshed card marks current: {card_str}"
    );
}

/// Picking the directory the thread is ALREADY in is a no-op: a Toast, no
/// new session (mirrors the switch card's "已在当前会话").
#[tokio::test]
async fn dir_card_pick_current_directory_toasts_only() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let mut backend = MockBackend::new(realistic_parts());
    backend.session_list = vec![list_session("ses_a", "项目A", "/work/a", 100)];
    let (app, _platform) = build_app(cfg, backend).await;
    let key = crate::config::ThreadKey::new("chat_1".into(), "chat_1".into());
    seed_entry(
        &app,
        crate::config::SessionEntry {
            thread_key: key.clone(),
            session_id: "ses_a".into(),
            directory: "/work/a".into(),
            agent: None,
            model: None,
            auto_accept: false,
            topic_anchor: None,
            topic_root: None,
            variant: None,
        },
    )
    .await;

    let value = serde_json::json!({
        "action": "dir",
        "op": "pick",
        "chat_id": "chat_1",
        "thread_id": "chat_1",
        "directory": "/work/a",
    });
    let result = app
        .handle_card_action(value)
        .await
        .expect("dir pick should return a result");
    assert!(result.card.is_some(), "card still refreshes");
    assert_eq!(
        result.toast.as_deref(),
        Some("已在当前目录"),
        "current dir pick toasts: {:?}",
        result.toast
    );
    // No new session was created: the active entry is unchanged.
    let entry = app.sessions.lock().await.get_active(&key).cloned().unwrap();
    assert_eq!(entry.session_id, "ses_a");
    assert_eq!(entry.directory, "/work/a");
}

/// The `/dir` Recent Directories card's "建话题" op (ADR-0025) wraps a NEW
/// session in a brand-new topic — the card equivalent of `/topic <dir>`:
/// cover card at the chat's top level, thread anchored on it, the new
/// session mapped to the new topic key, the lobby untouched.
#[tokio::test]
async fn dir_card_topic_creates_topic_with_new_session() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let mut backend = MockBackend::new(realistic_parts());
    backend.session_id = "ses_dir".into();
    backend.session_list = vec![list_session("ses_b", "项目B", "/work/b", 200)];
    let (app, platform) = build_app(cfg, backend).await;

    let value = serde_json::json!({
        "action": "dir",
        "op": "topic",
        "chat_id": "chat_1",
        "thread_id": "chat_1",
        "directory": "/work/b",
        "open_message_id": "om_dir_card",
    });
    let result = app
        .handle_card_action(value)
        .await
        .expect("dir topic should return a result");
    assert!(result.card.is_some(), "dir topic refreshes the card");
    let toast = result.toast.clone().unwrap_or_default();
    assert!(
        toast.contains("已建话题") && toast.contains("b"),
        "dir topic toasts the created topic: {toast:?}"
    );
    assert!(
        result.card.unwrap().to_string().contains("建话题"),
        "refreshed card keeps the 建话题 rows"
    );

    // The topic pipeline is /topic's (ADR-0023): a cover card leading with
    // the directory basename as the display title goes to the chat's top
    // level, then reply_in_thread on THAT card seeds the topic.
    let calls = platform.calls.lock().await.clone();
    let cover = calls
        .iter()
        .find_map(|c| match c {
            PlatformCall::SendCard { receive_id, card } if receive_id == "chat_1" => Some(card.to_string()),
            _ => None,
        })
        .expect("cover card sent to the chat");
    assert!(
        cover.contains("💬 `b`") && cover.contains("`/work/b`"),
        "cover leads with the directory basename, got: {cover}"
    );
    assert!(
        calls
            .iter()
            .any(|c| matches!(c, PlatformCall::ReplyInThread { message_id, .. } if message_id == "msg_sent")),
        "reply_in_thread anchors on the cover card, got {calls:?}"
    );

    // The new topic owns the NEW session; the lobby stays untouched.
    let topic_key = crate::config::ThreadKey::new("chat_1".into(), "omt_created_topic".into());
    let entry = app
        .sessions
        .lock()
        .await
        .get_active(&topic_key)
        .cloned()
        .expect("dir topic maps the new session to the new topic");
    assert_eq!(entry.session_id, "ses_dir");
    assert_eq!(entry.directory, "/work/b");
    assert_eq!(entry.topic_anchor.as_deref(), Some("msg_topic_reply"));
    assert_eq!(entry.topic_root.as_deref(), Some("msg_sent"));
    assert_eq!(
        app.core.cover_titles.lock().await.get("ses_dir").cloned(),
        Some(crate::bridge::core::CoverTitle {
            title: "b".into(),
            model: None
        })
    );
    let lobby_key = crate::config::ThreadKey::new("chat_1".into(), "chat_1".into());
    assert!(
        app.sessions.lock().await.get_active(&lobby_key).is_none(),
        "lobby must not gain a session from 建话题"
    );
}

/// 建话题 on the CURRENT directory's row is allowed — it equals bare
/// `/topic` in the current project (mirroring the switch card, whose ✅ 当前
/// row still carries 建话题接管). Only `pick` is a no-op on the current row.
#[tokio::test]
async fn dir_card_topic_on_current_directory_opens_topic() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let backend = MockBackend::new(realistic_parts());
    let (app, _platform) = build_app(cfg, backend).await;
    let lobby_key = crate::config::ThreadKey::new("chat_1".into(), "chat_1".into());
    seed_entry(
        &app,
        crate::config::SessionEntry {
            thread_key: lobby_key.clone(),
            session_id: "ses_a".into(),
            directory: "/work/a".into(),
            agent: None,
            model: None,
            auto_accept: false,
            topic_anchor: None,
            topic_root: None,
            variant: None,
        },
    )
    .await;

    let value = serde_json::json!({
        "action": "dir",
        "op": "topic",
        "chat_id": "chat_1",
        "thread_id": "chat_1",
        "directory": "/work/a",
        "open_message_id": "om_dir_card",
    });
    let result = app
        .handle_card_action(value)
        .await
        .expect("dir topic should return a result");
    assert!(
        result.toast.clone().unwrap_or_default().contains("已建话题"),
        "current-dir 建话题 opens the topic: {:?}",
        result.toast
    );
    let topic_key = crate::config::ThreadKey::new("chat_1".into(), "omt_created_topic".into());
    assert!(
        app.sessions.lock().await.get_active(&topic_key).is_some(),
        "current-dir 建话题 must map the fresh topic"
    );
    // The lobby's own session is unchanged (no re-rooting happened).
    assert_eq!(
        app.sessions
            .lock()
            .await
            .get_active(&lobby_key)
            .map(|e| e.session_id.clone()),
        Some("ses_a".into())
    );
}

/// A topic cannot be created from inside a topic (ADR-0006, ADR-0025): a
/// `/dir` card opened in a never-bound topic — which may legally bind its
/// session via `pick` — must not nest another topic via 建话题.
#[tokio::test]
async fn dir_card_topic_rejects_inside_topic() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let backend = MockBackend::new(realistic_parts());
    let (app, _platform) = build_app(cfg, backend).await;

    let value = serde_json::json!({
        "action": "dir",
        "op": "topic",
        "chat_id": "chat_1",
        "thread_id": "omt_t_1",
        "directory": "/work/b",
        "open_message_id": "om_dir_card",
    });
    let result = app
        .handle_card_action(value)
        .await
        .expect("dir topic should return a result");
    assert_eq!(result.card, None, "no card refresh on rejection");
    assert!(
        result.toast.clone().unwrap_or_default().contains("回主对话操作"),
        "nested topic creation is rejected with a Toast: {:?}",
        result.toast
    );
    assert!(app.sessions.lock().await.all_entries().is_empty());
}

/// The dir card's 建话题 op fails gracefully when the card action carries
/// no `open_message_id` (the anchor needed to create the topic).
#[tokio::test]
async fn dir_card_topic_missing_open_message_id() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let backend = MockBackend::new(realistic_parts());
    let (app, _platform) = build_app(cfg, backend).await;

    let value = serde_json::json!({
        "action": "dir",
        "op": "topic",
        "chat_id": "chat_1",
        "thread_id": "chat_1",
        "directory": "/work/b",
    });
    let result = app
        .handle_card_action(value)
        .await
        .expect("dir topic should return a result");
    assert_eq!(result.card, None);
    assert!(
        result
            .toast
            .clone()
            .unwrap_or_default()
            .contains("缺少卡片消息引用"),
        "missing open_message_id surfaces a hint: {:?}",
        result.toast
    );
    assert!(app.sessions.lock().await.all_entries().is_empty());
}

/// A chat without topic support returns no thread_id: 建话题 degrades with
/// a toast pointing at `/dir` or a manual topic — nothing is mapped.
#[tokio::test]
async fn dir_card_topic_no_thread_id_degrades_with_guidance() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let backend = MockBackend::new(realistic_parts());
    let mut platform = RecordingPlatform::new();
    platform.reply_in_thread_thread_id = None;
    let platform = Arc::new(platform);
    let app = Arc::new(App::new(cfg, Arc::new(backend), platform.clone()).unwrap());

    let value = serde_json::json!({
        "action": "dir",
        "op": "topic",
        "chat_id": "chat_1",
        "thread_id": "chat_1",
        "directory": "/work/b",
        "open_message_id": "om_dir_card",
    });
    let result = app
        .handle_card_action(value)
        .await
        .expect("dir topic should return a result");
    assert_eq!(result.card, None);
    assert!(
        result
            .toast
            .clone()
            .unwrap_or_default()
            .contains("不支持创建话题"),
        "no-thread_id chat surfaces guidance: {:?}",
        result.toast
    );
    assert!(app.sessions.lock().await.all_entries().is_empty());
}

/// The location-based guard also covers a topic that bound its session via
/// the row's left button on THIS same card: after `pick` binds the
/// never-bound topic, clicking 建话题 on the refreshed card must still
/// reject — the topic now has a session, so it cannot open another topic.
#[tokio::test]
async fn dir_card_topic_rejects_after_topic_bound_via_pick() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let backend = MockBackend::new(realistic_parts());
    let (app, _platform) = build_app(cfg, backend).await;
    let topic_key = crate::config::ThreadKey::new("chat_1".into(), "omt_t_1".into());

    // Step 1: the never-bound topic binds its single session via `pick`.
    let pick_value = serde_json::json!({
        "action": "dir",
        "op": "pick",
        "chat_id": "chat_1",
        "thread_id": "omt_t_1",
        "directory": "/work/a",
    });
    app.handle_card_action(pick_value)
        .await
        .expect("pick should bind the topic");
    assert!(
        app.sessions.lock().await.get_active(&topic_key).is_some(),
        "pick binds the never-bound topic's session"
    );

    // Step 2: 建话题 on the same thread is rejected — the guard is
    // location-based (thread_id != chat_id), independent of session state.
    let topic_value = serde_json::json!({
        "action": "dir",
        "op": "topic",
        "chat_id": "chat_1",
        "thread_id": "omt_t_1",
        "directory": "/work/b",
        "open_message_id": "om_dir_card",
    });
    let result = app
        .handle_card_action(topic_value)
        .await
        .expect("dir topic should return a result");
    assert_eq!(result.card, None, "no card refresh on rejection");
    assert!(
        result.toast.clone().unwrap_or_default().contains("回主对话操作"),
        "bound topic still rejects nesting: {:?}",
        result.toast
    );
    // Only the original binding remains — no second topic was created.
    assert_eq!(app.sessions.lock().await.all_entries().len(), 1);
}
