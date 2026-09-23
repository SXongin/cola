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

/// `/new` in a conversation whose current project is rooted elsewhere must
/// inherit that project's directory (ADR-0012) — NOT the configured work_dir.
/// Only a conversation with no session falls back to work_dir.
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

    // First `/dir <proj>` declares a pending rooted in the project (ADR-0041).
    send_command_in(
        &app,
        &format!("/dir {}", proj_dir.clone()),
        thread_key.clone(),
        "msg_dir",
        crate::config::ConversationKind::P2p,
    )
    .await;

    // Then `/new` declares a Pending Session in the project, not back in
    // work_dir (ADR-0012 / ADR-0041).
    send_command_in(
        &app,
        "/new",
        thread_key.clone(),
        "msg_new",
        crate::config::ConversationKind::P2p,
    )
    .await;

    let pending = app
        .sessions
        .lock()
        .await
        .pending_for(&thread_key)
        .cloned()
        .expect("/new declares a pending");
    // normalize_directory canonicalizes the project path (resolving
    // /private/var on macOS and \\?\ / 8.3 short names on Windows), so
    // compare against the canonicalized form — not the raw tempdir path.
    let canonical = std::fs::canonicalize(proj.path()).unwrap();
    assert_eq!(
        pending.directory,
        canonical.to_string_lossy(),
        "/new must inherit the active session's directory, not work_dir"
    );
    assert_ne!(
        pending.directory,
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

    send_command_in(
        &app,
        "/new",
        thread_key.clone(),
        "msg_new",
        crate::config::ConversationKind::P2p,
    )
    .await;

    let pending = app
        .sessions
        .lock()
        .await
        .pending_for(&thread_key)
        .cloned()
        .expect("/new declares a pending");
    assert_eq!(pending.directory, work.to_string_lossy().to_string());
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
    archived.time = Some(opencode::types::SessionTime {
        created: 1,
        updated: 888,
        archived: Some(1),
    });
    backend.given_sessions(vec![
        list_session("ses_a1", "A1", "/work/a", 100),
        list_session("ses_b", "B", "/work/b", 200),
        list_session("ses_a2", "A2", "/work/a", 300),
        child,
        archived,
    ]);
    let (app, _platform) = build_app(cfg, backend).await;
    let key = crate::config::ThreadKey::new("chat_1".into(), "chat_1".into());

    let (dirs, current) = crate::bridge::command::dir_card_data(&app.core, &key).await;
    assert_eq!(dirs, vec!["/work/a".to_string(), "/work/b".to_string()]);
    assert_eq!(current, None);
}

/// Directories cola has mapped are unioned in after the session-derived ones
/// (deduped, most recently mapped first): they survive the server-side deletion
/// or archival that drops a directory's last session — the exact case the
/// shared store stops reporting it.
#[tokio::test]
async fn dir_card_data_unions_store_directories_dropped_by_the_server() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let mut backend = MockBackend::new(realistic_parts());
    let mut archived = list_session("ses_arch", "归档", "/work/arch", 888);
    archived.time = Some(opencode::types::SessionTime {
        created: 1,
        updated: 888,
        archived: Some(1),
    });
    backend.given_sessions(vec![list_session("ses_a", "A", "/work/a", 100), archived]);
    let (app, _platform) = build_app(cfg, backend).await;

    // /work/a is mapped too (dedup), /work/gone's session was deleted
    // server-side, /work/arch's only session is archived (filtered out).
    seed_session(&app, "ses_a", "/work/a").await;
    seed_session(&app, "ses_gone", "/work/gone").await;
    seed_session(&app, "ses_arch", "/work/arch").await;

    let key = crate::config::ThreadKey::new("chat_1".into(), "chat_1".into());
    let (dirs, current) = crate::bridge::command::dir_card_data(&app.core, &key).await;
    assert_eq!(
        dirs,
        vec![
            "/work/a".to_string(),
            "/work/arch".to_string(),
            "/work/gone".to_string()
        ],
        "server-derived dirs first, then cola's own, most recently mapped first"
    );
    assert_eq!(current, Some("/work/arch".to_string()));
}

/// An empty session list (fresh or swapped store, or a failed fetch — the
/// `unwrap_or_default`) still renders the directories cola has mapped instead
/// of the empty-state hint.
#[tokio::test]
async fn dir_card_data_shows_store_directories_with_an_empty_session_list() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let (app, _platform) = build_app(cfg, MockBackend::new(realistic_parts())).await;

    seed_session(&app, "ses_x", "/work/x").await;

    let key = crate::config::ThreadKey::new("chat_1".into(), "chat_1".into());
    let (dirs, current) = crate::bridge::command::dir_card_data(&app.core, &key).await;
    assert_eq!(dirs, vec!["/work/x".to_string()]);
    assert_eq!(current, Some("/work/x".to_string()));
}

/// A Pending Session's directory has no server session and no mapping entry
/// (ADR-0041), so neither source carries it; the card still has to render it
/// (as `当前`) instead of falling back to the empty-state hint.
#[tokio::test]
async fn dir_card_data_shows_a_pending_only_directory() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let (app, _platform) = build_app(cfg, MockBackend::new(realistic_parts())).await;
    let key = crate::config::ThreadKey::new("chat_1".into(), "chat_1".into());

    seed_pending(
        &app,
        crate::bridge::session::PendingEntry::new(key.clone(), "/work/pending"),
    )
    .await;

    let (dirs, current) = crate::bridge::command::dir_card_data(&app.core, &key).await;
    assert_eq!(dirs, vec!["/work/pending".to_string()]);
    assert_eq!(current, Some("/work/pending".to_string()));
}

/// The `/dir` Recent Directories card's `pick` op declares a Pending Session
/// rooted at the picked directory (the card form of `/dir <path>`, ADR-0041):
/// no server session is created, and the refreshed card marks the pending's
/// directory as `当前`.
#[tokio::test]
async fn dir_card_pick_declares_a_pending_and_refreshes_card() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let mut backend = MockBackend::new(realistic_parts());
    backend.given_sessions(vec![
        list_session("ses_a", "项目A", "/work/a", 100),
        list_session("ses_b", "项目B", "/work/b", 200),
    ]);
    let created = backend.created_session_dirs.clone();
    let (app, _platform) = build_app(cfg, backend).await;

    let value = serde_json::json!({
        "action": "dir",
        "op": "pick",
        "chat_id": "chat_1",
        "thread_id": "chat_1",
        "directory": "/work/b",
    });
    let result = app
        .host_action(value)
        .await
        .expect("dir pick should return a result");
    assert!(result.card.is_some(), "dir pick refreshes the card");
    let toast = result.toast.clone().unwrap_or_default();
    assert!(
        toast.contains("下一条消息") && toast.contains("/work/b"),
        "dir pick toasts the pending timing and directory: {toast:?}"
    );
    assert!(
        created.lock().await.is_empty(),
        "dir pick creates NO server session"
    );
    let key = crate::config::ThreadKey::new("chat_1".into(), "chat_1".into());
    assert_eq!(
        app.sessions
            .lock()
            .await
            .pending_for(&key)
            .map(|p| p.directory.clone()),
        Some("/work/b".to_string())
    );
    assert!(
        app.sessions.lock().await.get_active(&key).is_none(),
        "a pending supersedes the active session"
    );
    // The refreshed card marks the pending's directory as current.
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
    backend.given_sessions(vec![list_session("ses_a", "项目A", "/work/a", 100)]);
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
        .host_action(value)
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

/// The `/dir` Recent Directories card's "建话题" op (ADR-0025) opens a
/// brand-new topic around a Pending Session (ADR-0041) — the card equivalent
/// of `/topic <dir>`: cover card at the chat's top level showing the creation
/// timing, thread anchored on it, the pending mapped to the new topic key,
/// the lobby untouched, and NO server session created.
#[tokio::test]
async fn dir_card_topic_opens_a_pending_topic() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let mut backend = MockBackend::new(realistic_parts());
    backend.with_session_id("ses_dir");
    backend.given_sessions(vec![list_session("ses_b", "项目B", "/work/b", 200)]);
    let created = backend.created_session_dirs.clone();
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
        .host_action(value)
        .await
        .expect("dir topic should return a result");
    assert!(result.card.is_some(), "dir topic refreshes the card");
    let toast = result.toast.clone().unwrap_or_default();
    assert!(
        toast.contains("已建话题") && toast.contains("下一条消息"),
        "dir topic toasts the pending timing: {toast:?}"
    );
    assert!(
        card_text(&result.card.unwrap()).contains("建话题"),
        "refreshed card keeps the 建话题 rows"
    );
    assert!(
        created.lock().await.is_empty(),
        "建话题 creates no server session"
    );

    // The topic pipeline is /topic's (ADR-0023): a pending cover card leading
    // with the directory basename goes to the chat's top level, then
    // reply_in_thread on THAT card seeds the topic.
    let calls = platform.calls.lock().await.clone();
    let cover = calls
        .iter()
        .find_map(|c| match c {
            PlatformCall::SendCard { receive_id, card } if receive_id == "chat_1" => Some(card.to_string()),
            _ => None,
        })
        .expect("cover card sent to the chat");
    assert!(
        cover.contains("💬 `b`") && cover.contains("下一条消息创建"),
        "pending cover leads with the directory basename and the creation verb, got: {cover}"
    );
    assert!(
        calls
            .iter()
            .any(|c| matches!(c, PlatformCall::ReplyInThread { message_id, .. } if message_id == "msg_sent")),
        "reply_in_thread anchors on the cover card, got {calls:?}"
    );

    // The new topic's Pending Session carries the picked directory; the lobby
    // stays untouched.
    let topic_key = crate::config::ThreadKey::new("chat_1".into(), "omt_created_topic".into());
    let store = app.sessions.lock().await;
    assert!(store.get_active(&topic_key).is_none());
    let pending = store
        .pending_for(&topic_key)
        .cloned()
        .expect("dir topic records a pending on the new topic");
    drop(store);
    assert_eq!(pending.directory, "/work/b");
    assert_eq!(pending.topic_anchor.as_deref(), Some("msg_topic_reply"));
    assert_eq!(pending.topic_root.as_deref(), Some("msg_sent"));
    assert_eq!(
        app.core
            .cover_titles
            .lock()
            .await
            .values()
            .next()
            .map(|c| (c.title.clone(), c.pending)),
        Some(("b".to_string(), true)),
        "the pending cover is recorded under the pending key"
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
        .host_action(value)
        .await
        .expect("dir topic should return a result");
    assert!(
        result.toast.clone().unwrap_or_default().contains("已建话题"),
        "current-dir 建话题 opens the topic: {:?}",
        result.toast
    );
    let topic_key = crate::config::ThreadKey::new("chat_1".into(), "omt_created_topic".into());
    assert!(
        app.sessions.lock().await.pending_for(&topic_key).is_some(),
        "current-dir 建话题 must record the fresh topic's pending"
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
        .host_action(value)
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
        .host_action(value)
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
        .host_action(value)
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
/// the row's left button on THIS same card: after `pick` declares the
/// never-bound topic's pending, clicking 建话题 on the refreshed card must
/// still reject — the guard is location-based, not state-based.
#[tokio::test]
async fn dir_card_topic_rejects_after_topic_bound_via_pick() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let backend = MockBackend::new(realistic_parts());
    let (app, _platform) = build_app(cfg, backend).await;
    let topic_key = crate::config::ThreadKey::new("chat_1".into(), "omt_t_1".into());

    // Step 1: the never-bound topic declares its pending via `pick`.
    let pick_value = serde_json::json!({
        "action": "dir",
        "op": "pick",
        "chat_id": "chat_1",
        "thread_id": "omt_t_1",
        "directory": "/work/a",
    });
    app.host_action(pick_value)
        .await
        .expect("pick should bind the topic");
    assert_eq!(
        app.sessions
            .lock()
            .await
            .pending_for(&topic_key)
            .map(|p| p.directory.clone()),
        Some("/work/a".to_string()),
        "pick declares the never-bound topic's pending"
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
        .host_action(topic_value)
        .await
        .expect("dir topic should return a result");
    assert_eq!(result.card, None, "no card refresh on rejection");
    assert!(
        result.toast.clone().unwrap_or_default().contains("回主对话操作"),
        "bound topic still rejects nesting: {:?}",
        result.toast
    );
    // Only the original pending remains — no topic, no session was created.
    let store = app.sessions.lock().await;
    assert!(store.all_entries().is_empty());
    assert_eq!(
        store.pending_for(&topic_key).map(|p| p.directory.as_str()),
        Some("/work/a")
    );
}
