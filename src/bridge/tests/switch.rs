use crate::bridge::command::*;
use crate::bridge::test_support::*;

#[tokio::test]
async fn list_shows_global_sessions_marking_own() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let mut backend = MockBackend::new(realistic_parts());
    backend.session_list = vec![
        list_session("ses_alpha01", "外部会话", "/tmp/ext", 100),
        list_session("ses_beta02", "本地会话", "/work/cola", 300),
    ];
    let (app, platform) = build_app(cfg, backend).await;
    // Our own lobby session, so /list marks it active (ADR-0022: only the
    // active session is marked; the 本会话 ownership marker is gone).
    {
        let mut store = app.sessions.lock().await;
        store.set_active(crate::config::SessionEntry {
            thread_key: crate::config::ThreadKey::new("chat_1".into(), "chat_1".into()),
            session_id: "ses_beta02".into(),
            directory: "/work/cola".into(),
            agent: None,
            model: None,
            auto_accept: false,
            topic_anchor: None,
            topic_root: None,
            variant: None,
        });
    }

    crate::bridge::command::handle_command(
        &app.core,
        Command::Switch(SwitchAction::List {
            keyword: None,
            all: false,
        }),
        crate::config::ThreadKey::new("chat_1".into(), "chat_1".into()),
        "msg_list",
        crate::config::ConversationKind::P2p,
    )
    .await
    .unwrap();

    let calls = platform.calls.lock().await.clone();
    let text = calls
        .iter()
        .filter_map(|c| match c {
            PlatformCall::ReplyText { text, .. } => Some(text.clone()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n");
    assert!(text.contains("外部会话"), "external session visible: {text}");
    assert!(text.contains("本地会话"), "own session visible: {text}");
    // The newest (updated 300) sorts first; own session marked active.
    let pos_ext = text.find("外部会话").unwrap();
    let pos_local = text.find("本地会话").unwrap();
    assert!(pos_local < pos_ext, "own (newer) session sorts first: {text}");
    assert!(text.contains("(active)"), "active session marked: {text}");
    assert!(!text.contains("本会话"), "ownership marker dropped: {text}");
}

#[tokio::test]
async fn list_filters_by_keyword_and_hides_children() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let mut backend = MockBackend::new(realistic_parts());
    backend.session_list = vec![
        list_session("ses_alpha01", "重写登录模块", "/work/auth", 100),
        list_session("ses_beta02", "修 bug", "/work/cola", 300),
        opencode::client::SessionListInfo {
            parent_id: Some("ses_alpha01".into()),
            ..list_session("ses_child09", "Child session - x", "/work/auth", 400)
        },
    ];
    let (app, platform) = build_app(cfg, backend).await;

    // Keyword filters by title.
    crate::bridge::command::handle_command(
        &app.core,
        Command::Switch(SwitchAction::List {
            keyword: Some("登录".into()),
            all: false,
        }),
        crate::config::ThreadKey::new("chat_1".into(), "chat_1".into()),
        "msg_list",
        crate::config::ConversationKind::P2p,
    )
    .await
    .unwrap();
    let calls = platform.calls.lock().await.clone();
    let text = calls
        .iter()
        .filter_map(|c| match c {
            PlatformCall::ReplyText { text, .. } => Some(text.clone()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n");
    assert!(text.contains("重写登录模块"), "keyword match: {text}");
    assert!(!text.contains("修 bug"), "non-matching title filtered: {text}");

    // Without --all the child is hidden even though it is newest.
    platform.calls.lock().await.clear();
    crate::bridge::command::handle_command(
        &app.core,
        Command::Switch(SwitchAction::List {
            keyword: None,
            all: false,
        }),
        crate::config::ThreadKey::new("chat_1".into(), "chat_1".into()),
        "msg_list2",
        crate::config::ConversationKind::P2p,
    )
    .await
    .unwrap();
    let calls = platform.calls.lock().await.clone();
    let text = calls
        .iter()
        .filter_map(|c| match c {
            PlatformCall::ReplyText { text, .. } => Some(text.clone()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n");
    assert!(!text.contains("Child session"), "child hidden by default: {text}");

    // --all reveals the child.
    platform.calls.lock().await.clear();
    crate::bridge::command::handle_command(
        &app.core,
        Command::Switch(SwitchAction::List {
            keyword: None,
            all: true,
        }),
        crate::config::ThreadKey::new("chat_1".into(), "chat_1".into()),
        "msg_list3",
        crate::config::ConversationKind::P2p,
    )
    .await
    .unwrap();
    let calls = platform.calls.lock().await.clone();
    let text = calls
        .iter()
        .filter_map(|c| match c {
            PlatformCall::ReplyText { text, .. } => Some(text.clone()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n");
    assert!(text.contains("child09"), "child shown with --all: {text}");
}

/// Repeated `/list` within the 30 s TTL must not re-hit the server; an
/// external rename is only visible after invalidation/expiry.
#[tokio::test]
async fn list_is_cached_within_ttl_and_invalidated_on_rename() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let mut backend = MockBackend::new(realistic_parts());
    backend.session_list = vec![list_session("ses_alpha01", "标题 A", "/work/a", 100)];
    let calls_counter = backend.list_sessions_calls.clone();
    let (app, _platform) = build_app(cfg, backend).await;
    let key = crate::config::ThreadKey::new("chat_1".into(), "chat_1".into());
    {
        let mut store = app.sessions.lock().await;
        store.set_active(crate::config::SessionEntry {
            thread_key: key.clone(),
            session_id: "ses_alpha01".into(),
            directory: "/work/a".into(),
            agent: None,
            model: None,
            auto_accept: false,
            topic_anchor: None,
            topic_root: None,
            variant: None,
        });
    }

    // Two /list in a row → one server fetch.
    crate::bridge::command::handle_command(
        &app.core,
        Command::Switch(SwitchAction::List {
            keyword: None,
            all: false,
        }),
        key.clone(),
        "m1",
        crate::config::ConversationKind::P2p,
    )
    .await
    .unwrap();
    crate::bridge::command::handle_command(
        &app.core,
        Command::Switch(SwitchAction::List {
            keyword: None,
            all: false,
        }),
        key.clone(),
        "m2",
        crate::config::ConversationKind::P2p,
    )
    .await
    .unwrap();
    assert_eq!(calls_counter.load(std::sync::atomic::Ordering::SeqCst), 1);

    // A rename invalidates the cache → next /list refetches.
    crate::bridge::command::handle_command(
        &app.core,
        Command::Name("新名字".into()),
        key.clone(),
        "m3",
        crate::config::ConversationKind::P2p,
    )
    .await
    .unwrap();
    crate::bridge::command::handle_command(
        &app.core,
        Command::Switch(SwitchAction::List {
            keyword: None,
            all: false,
        }),
        key,
        "m4",
        crate::config::ConversationKind::P2p,
    )
    .await
    .unwrap();
    assert_eq!(calls_counter.load(std::sync::atomic::Ordering::SeqCst), 2);
}

#[tokio::test]
async fn attach_adopts_foreign_session_by_id() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let mut backend = MockBackend::new(realistic_parts());
    backend.session_list = vec![list_session(
        "ses_foreign123abc",
        "OpenChamber 里的任务",
        "/work/foreign",
        100,
    )];
    let (app, _platform) = build_app(cfg, backend).await;

    crate::bridge::command::handle_command(
        &app.core,
        Command::Switch(SwitchAction::Attach {
            query: "ses_foreign123abc".into(),
            force: false,
        }),
        crate::config::ThreadKey::new("chat_1".into(), "chat_1".into()),
        "msg_attach",
        crate::config::ConversationKind::P2p,
    )
    .await
    .unwrap();

    // The thread now maps to the foreign session with its directory.
    let key = crate::config::ThreadKey::new("chat_1".into(), "chat_1".into());
    let entry = app.sessions.lock().await.get_active(&key).cloned().unwrap();
    assert_eq!(entry.session_id, "ses_foreign123abc");
    assert_eq!(entry.directory, "/work/foreign");
}

#[tokio::test]
async fn attach_rejects_session_owned_by_another_thread_without_force() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let mut backend = MockBackend::new(realistic_parts());
    backend.session_list = vec![list_session(
        "ses_foreign123abc",
        "OpenChamber 里的任务",
        "/work/foreign",
        100,
    )];
    let mut platform = RecordingPlatform::new();
    platform
        .chat_names
        .insert("oc_group_other".into(), "隔壁群".into());
    let platform = Arc::new(platform);
    let app = Arc::new(App::new(cfg, Arc::new(backend), platform.clone()).unwrap());
    // Another thread already owns the session.
    {
        let mut store = app.sessions.lock().await;
        store.set_active(crate::config::SessionEntry {
            thread_key: crate::config::ThreadKey::new("oc_group_other".into(), "oc_group_other".into()),
            session_id: "ses_foreign123abc".into(),
            directory: "/work/foreign".into(),
            agent: None,
            model: None,
            auto_accept: false,
            topic_anchor: None,
            topic_root: None,
            variant: None,
        });
    }

    crate::bridge::command::handle_command(
        &app.core,
        Command::Switch(SwitchAction::Attach {
            query: "ses_foreign123abc".into(),
            force: false,
        }),
        crate::config::ThreadKey::new("chat_1".into(), "chat_1".into()),
        "msg_attach",
        crate::config::ConversationKind::P2p,
    )
    .await
    .unwrap();

    // Rejected: the current thread still has no session.
    let key = crate::config::ThreadKey::new("chat_1".into(), "chat_1".into());
    assert!(app.sessions.lock().await.get_active(&key).is_none());
    let calls = platform.calls.lock().await.clone();
    let text = calls
        .iter()
        .filter_map(|c| match c {
            PlatformCall::ReplyText { text, .. } => Some(text.clone()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n");
    assert!(text.contains("隔壁群"), "rejection names the owning chat: {text}");
    assert!(text.contains("--force"), "rejection points at --force: {text}");
}

#[tokio::test]
async fn attach_force_steals_mapping_from_other_thread() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let mut backend = MockBackend::new(realistic_parts());
    backend.session_list = vec![list_session(
        "ses_foreign123abc",
        "OpenChamber 里的任务",
        "/work/foreign",
        100,
    )];
    let (app, _platform) = build_app(cfg, backend).await;
    {
        let mut store = app.sessions.lock().await;
        store.set_active(crate::config::SessionEntry {
            thread_key: crate::config::ThreadKey::new("oc_group_other".into(), "oc_group_other".into()),
            session_id: "ses_foreign123abc".into(),
            directory: "/work/foreign".into(),
            agent: None,
            model: None,
            auto_accept: false,
            topic_anchor: None,
            topic_root: None,
            variant: None,
        });
    }

    crate::bridge::command::handle_command(
        &app.core,
        Command::Switch(SwitchAction::Attach {
            query: "ses_foreign123abc".into(),
            force: true,
        }),
        crate::config::ThreadKey::new("chat_1".into(), "chat_1".into()),
        "msg_attach",
        crate::config::ConversationKind::P2p,
    )
    .await
    .unwrap();

    // Stolen: current thread owns it, other thread is sessionless.
    let key = crate::config::ThreadKey::new("chat_1".into(), "chat_1".into());
    assert_eq!(
        app.sessions.lock().await.get_active(&key).unwrap().session_id,
        "ses_foreign123abc"
    );
    let other = crate::config::ThreadKey::new("oc_group_other".into(), "oc_group_other".into());
    assert!(app.sessions.lock().await.get_active(&other).is_none());
}

#[tokio::test]
async fn forget_unmaps_thread_keeping_server_session() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let (app, _platform) = build_app(cfg, MockBackend::new(realistic_parts())).await;
    {
        let mut store = app.sessions.lock().await;
        store.set_active(crate::config::SessionEntry {
            thread_key: crate::config::ThreadKey::new("chat_1".into(), "chat_1".into()),
            session_id: "ses_test".into(),
            directory: "/tmp/aa".into(),
            agent: None,
            model: None,
            auto_accept: false,
            topic_anchor: None,
            topic_root: None,
            variant: None,
        });
    }

    crate::bridge::command::handle_command(
        &app.core,
        Command::Switch(SwitchAction::Forget),
        crate::config::ThreadKey::new("chat_1".into(), "chat_1".into()),
        "msg_forget",
        crate::config::ConversationKind::P2p,
    )
    .await
    .unwrap();

    let key = crate::config::ThreadKey::new("chat_1".into(), "chat_1".into());
    assert!(app.sessions.lock().await.get_active(&key).is_none());
}

#[tokio::test]
async fn switch_adopts_unique_foreign_session() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let mut backend = MockBackend::new(realistic_parts());
    backend.session_list = vec![list_session("ses_alpha01", "唯一外部标题", "/work/ext", 100)];
    let (app, _platform) = build_app(cfg, backend).await;

    crate::bridge::command::handle_command(
        &app.core,
        Command::Switch(SwitchAction::Match("唯一外部标题".into())),
        crate::config::ThreadKey::new("chat_1".into(), "chat_1".into()),
        "msg_switch",
        crate::config::ConversationKind::P2p,
    )
    .await
    .unwrap();

    let key = crate::config::ThreadKey::new("chat_1".into(), "chat_1".into());
    let entry = app.sessions.lock().await.get_active(&key).cloned().unwrap();
    assert_eq!(entry.session_id, "ses_alpha01");
    assert_eq!(entry.directory, "/work/ext");
}

/// ADR-0028: a lobby text adopt ends in EXACTLY ONE Session Snapshot card
/// — never a card plus the old 「已接管…」 text, and never nothing for a
/// first adopt, even when the session is idle and empty. The snapshot is
/// the confirmation.
#[tokio::test]
async fn switch_lobby_adopt_ends_in_one_snapshot_card() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let mut backend = MockBackend::new(realistic_parts());
    backend.session_list = vec![list_session("ses_alpha01", "唯一外部标题", "/work/ext", 100)];
    let (app, platform) = build_app(cfg, backend).await;

    crate::bridge::command::handle_command(
        &app.core,
        Command::Switch(SwitchAction::Match("唯一外部标题".into())),
        crate::config::ThreadKey::new("chat_1".into(), "chat_1".into()),
        "msg_switch",
        crate::config::ConversationKind::P2p,
    )
    .await
    .unwrap();

    let calls = platform.calls.lock().await.clone();
    let cards: Vec<_> = calls
        .iter()
        .filter_map(|c| match c {
            PlatformCall::ReplyCard { card, .. } => Some(card.to_string()),
            _ => None,
        })
        .collect();
    assert_eq!(cards.len(), 1, "exactly one confirmation card: {calls:?}");
    assert!(
        cards[0].contains("已接管 唯一外部标题"),
        "snapshot header carries the adopt verb + title: {}",
        cards[0]
    );
    assert!(
        !cards[0].contains("已接管会话"),
        "the old text confirmation is gone: {}",
        cards[0]
    );
    let texts: Vec<_> = calls
        .iter()
        .filter_map(|c| match c {
            PlatformCall::ReplyText { text, .. } => Some(text.clone()),
            _ => None,
        })
        .collect();
    assert!(
        texts.iter().all(|t| !t.contains("已接管")),
        "no text confirmation alongside the snapshot: {texts:?}"
    );
    let key = crate::config::ThreadKey::new("chat_1".into(), "chat_1".into());
    let entry = app.sessions.lock().await.get_active(&key).cloned().unwrap();
    assert_eq!(entry.session_id, "ses_alpha01");
    assert_eq!(entry.topic_anchor, None, "lobby adopts carry no topic anchor");
}

/// ADR-0028: the re-`/switch` mapped-hit branch applies the suppression
/// predicate. An idle, pending-free session whose newest user message is
/// cola-authored (its recent life is already visible in this thread) keeps
/// today's one-line text ack and sends NO card.
#[tokio::test]
async fn switch_reeswitch_suppressed_when_nothing_to_report() {
    let _wd = test_work_dir();
    let mut backend = MockBackend::new(realistic_parts());
    // Newest user message is cola-authored; status defaults to idle; no
    // pending requests — the one suppressed cell of the matrix.
    backend
        .cola_user_messages
        .insert("ses_own1".into(), "上次的问题".into());
    let (app, platform, _dir) = build_reeswitch_app(backend).await;

    crate::bridge::command::handle_command(
        &app.core,
        Command::Switch(SwitchAction::Match("本项目".into())),
        crate::config::ThreadKey::new("chat_1".into(), "chat_1".into()),
        "msg_switch",
        crate::config::ConversationKind::P2p,
    )
    .await
    .unwrap();

    let calls = platform.calls.lock().await.clone();
    let text = calls
        .iter()
        .filter_map(|c| match c {
            PlatformCall::ReplyText { text, .. } => Some(text.clone()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n");
    assert!(text.contains("Switched to"), "one-line ack kept: {text}");
    assert!(
        calls.iter().all(|c| !matches!(c, PlatformCall::ReplyCard { .. })),
        "suppressed: no snapshot card: {calls:?}"
    );
}

/// ADR-0028: the same re-switch reports a snapshot when the newest user
/// message is EXTERNAL (invisible while the session was inactive, ADR-0017)
/// — the tail is the one vehicle that surfaces it.
#[tokio::test]
async fn switch_reeswitch_snapshots_on_external_newest_message() {
    let _wd = test_work_dir();
    let mut backend = MockBackend::new(realistic_parts());
    backend
        .external_user_messages
        .insert("ses_own1".into(), "OpenChamber 里的问题".into());
    let (app, platform, _dir) = build_reeswitch_app(backend).await;

    crate::bridge::command::handle_command(
        &app.core,
        Command::Switch(SwitchAction::Match("本项目".into())),
        crate::config::ThreadKey::new("chat_1".into(), "chat_1".into()),
        "msg_switch",
        crate::config::ConversationKind::P2p,
    )
    .await
    .unwrap();

    let calls = platform.calls.lock().await.clone();
    let card = calls
        .iter()
        .find_map(|c| match c {
            PlatformCall::ReplyCard { card, .. } => Some(card.to_string()),
            _ => None,
        })
        .expect("external newness reports a snapshot: {calls:?}");
    assert!(card.contains("已切换 本项目会话"), "re-switch verb: {card}");
    assert!(
        card.contains("OpenChamber 里的问题"),
        "the external message surfaces in the tail: {card}"
    );
}

/// ADR-0028: a busy re-switch reports a snapshot (运行中) — the in-flight
/// external turn is invisible until the snapshot surfaces it.
#[tokio::test]
async fn switch_reeswitch_snapshots_on_busy_status() {
    let _wd = test_work_dir();
    let mut backend = MockBackend::new(realistic_parts());
    backend
        .session_statuses
        .insert("ses_own1".into(), Some(opencode::client::SessionStatus::Busy));
    // Cola-authored newest alone is NOT enough to suppress a busy session.
    backend
        .cola_user_messages
        .insert("ses_own1".into(), "上次的问题".into());
    let (app, platform, _dir) = build_reeswitch_app(backend).await;

    crate::bridge::command::handle_command(
        &app.core,
        Command::Switch(SwitchAction::Match("本项目".into())),
        crate::config::ThreadKey::new("chat_1".into(), "chat_1".into()),
        "msg_switch",
        crate::config::ConversationKind::P2p,
    )
    .await
    .unwrap();

    let calls = platform.calls.lock().await.clone();
    let card = calls
        .iter()
        .find_map(|c| match c {
            PlatformCall::ReplyCard { card, .. } => Some(card.to_string()),
            _ => None,
        })
        .expect("busy reports a snapshot: {calls:?}");
    assert!(card.contains("运行中"), "busy chip: {card}");
}

/// ADR-0028: an adopt-time pending request on a re-switch reports a
/// snapshot with the 等待你的确认 chip — the request is blocked on the
/// operator, so the snapshot must surface it.
#[tokio::test]
async fn switch_reeswitch_snapshots_on_pending_permission() {
    let _wd = test_work_dir();
    let mut backend = MockBackend::new(realistic_parts());
    backend
        .cola_user_messages
        .insert("ses_own1".into(), "上次的问题".into());
    backend.permissions = vec![opencode::client::PermissionRequest {
        request_id: "req_own".into(),
        session_id: Some("ses_own1".into()),
        permission: Some("bash".into()),
        patterns: vec!["ls".into()],
        metadata: None,
        always: vec![],
    }];
    let (app, platform, _dir) = build_reeswitch_app(backend).await;

    crate::bridge::command::handle_command(
        &app.core,
        Command::Switch(SwitchAction::Match("本项目".into())),
        crate::config::ThreadKey::new("chat_1".into(), "chat_1".into()),
        "msg_switch",
        crate::config::ConversationKind::P2p,
    )
    .await
    .unwrap();

    let calls = platform.calls.lock().await.clone();
    let card = calls
        .iter()
        .find_map(|c| match c {
            PlatformCall::ReplyCard { card, .. } => Some(card.to_string()),
            _ => None,
        })
        .expect("a pending request reports a snapshot: {calls:?}");
    assert!(card.contains("等待你的确认"), "waiting chip: {card}");
}

#[tokio::test]
async fn switch_ambiguous_global_match_lists_candidates() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let mut backend = MockBackend::new(realistic_parts());
    backend.session_list = vec![
        list_session("ses_alpha01", "任务 A", "/work/a", 100),
        list_session("ses_beta02", "任务 B", "/work/b", 200),
    ];
    let (app, platform) = build_app(cfg, backend).await;

    crate::bridge::command::handle_command(
        &app.core,
        Command::Switch(SwitchAction::Match("任务".into())),
        crate::config::ThreadKey::new("chat_1".into(), "chat_1".into()),
        "msg_switch",
        crate::config::ConversationKind::P2p,
    )
    .await
    .unwrap();

    // Ambiguous → no adoption, candidates listed.
    let key = crate::config::ThreadKey::new("chat_1".into(), "chat_1".into());
    assert!(app.sessions.lock().await.get_active(&key).is_none());
    let calls = platform.calls.lock().await.clone();
    let text = calls
        .iter()
        .filter_map(|c| match c {
            PlatformCall::ReplyText { text, .. } => Some(text.clone()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n");
    assert!(text.contains("/switch"), "points at /switch: {text}");
}

#[tokio::test]
async fn switch_prefers_threads_own_sessions() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let mut backend = MockBackend::new(realistic_parts());
    backend.session_list = vec![
        list_session("ses_own1", "本项目会话", "/work/cola", 500),
        list_session("ses_foreign", "本项目会话", "/other/place", 100),
    ];
    let (app, _platform) = build_app(cfg, backend).await;
    {
        let mut store = app.sessions.lock().await;
        store.set_active(crate::config::SessionEntry {
            thread_key: crate::config::ThreadKey::new("chat_1".into(), "chat_1".into()),
            session_id: "ses_own1".into(),
            directory: "/work/cola".into(),
            agent: None,
            model: None,
            auto_accept: false,
            topic_anchor: None,
            topic_root: None,
            variant: None,
        });
        store.set_active(crate::config::SessionEntry {
            thread_key: crate::config::ThreadKey::new("chat_1".into(), "chat_1".into()),
            session_id: "ses_other_own".into(),
            directory: "/work/other".into(),
            agent: None,
            model: None,
            auto_accept: false,
            topic_anchor: None,
            topic_root: None,
            variant: None,
        });
    }

    crate::bridge::command::handle_command(
        &app.core,
        Command::Switch(SwitchAction::Match("本项目".into())),
        crate::config::ThreadKey::new("chat_1".into(), "chat_1".into()),
        "msg_switch",
        crate::config::ConversationKind::P2p,
    )
    .await
    .unwrap();

    // The thread's own session wins (mapping unchanged, just active).
    let key = crate::config::ThreadKey::new("chat_1".into(), "chat_1".into());
    assert_eq!(
        app.sessions.lock().await.get_active(&key).unwrap().session_id,
        "ses_own1"
    );
}

/// `/switch` (no args) sends the interactive session card: a search form,
/// one row per session with a switch/adopt button, and a "＋new" footer.
#[tokio::test]
async fn switch_no_arg_sends_session_card() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let mut backend = MockBackend::new(realistic_parts());
    backend.session_list = vec![
        list_session("ses_alpha01", "重写登录", "/work/auth", 100),
        list_session("ses_beta02", "修 bug", "/work/cola", 300),
    ];
    let (app, platform) = build_app(cfg, backend).await;

    crate::bridge::command::handle_command(
        &app.core,
        Command::Switch(SwitchAction::Card),
        crate::config::ThreadKey::new("chat_1".into(), "chat_1".into()),
        "msg_switch_card",
        crate::config::ConversationKind::P2p,
    )
    .await
    .unwrap();

    let calls = platform.calls.lock().await.clone();
    let card = calls
        .iter()
        .filter_map(|c| match c {
            PlatformCall::ReplyCard { card, .. } => Some(card.clone()),
            _ => None,
        })
        .next()
        .expect("a switch card should be sent");
    let text = card.to_string();
    assert!(text.contains("会话管理"), "header: {text}");
    assert!(text.contains("重写登录"), "session row: {text}");
    assert!(text.contains("修 bug"), "session row: {text}");
    assert!(text.contains("接管"), "adopt button: {text}");
    assert!(text.contains("＋ 新建会话"), "new button: {text}");
    assert!(text.contains("switch_search"), "search form: {text}");
}

/// `/switch` (no args) defaults to the current directory (ADR-0022): with
/// an active session in /work/cola, the card shows only that directory's
/// sessions, a header naming the directory, and a 全部 toggle to widen.
#[tokio::test]
async fn switch_card_defaults_to_current_directory_scope() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let mut backend = MockBackend::new(realistic_parts());
    backend.session_list = vec![
        list_session("ses_alpha01", "重写登录", "/work/auth", 100),
        list_session("ses_beta02", "修 bug", "/work/cola", 300),
    ];
    let (app, platform) = build_app(cfg, backend).await;
    {
        let mut store = app.sessions.lock().await;
        store.set_active(crate::config::SessionEntry {
            thread_key: crate::config::ThreadKey::new("chat_1".into(), "chat_1".into()),
            session_id: "ses_beta02".into(),
            directory: "/work/cola".into(),
            agent: None,
            model: None,
            auto_accept: false,
            topic_anchor: None,
            topic_root: None,
            variant: None,
        });
    }

    crate::bridge::command::handle_command(
        &app.core,
        Command::Switch(SwitchAction::Card),
        crate::config::ThreadKey::new("chat_1".into(), "chat_1".into()),
        "msg_switch_card",
        crate::config::ConversationKind::P2p,
    )
    .await
    .unwrap();

    let calls = platform.calls.lock().await.clone();
    let card = calls
        .iter()
        .filter_map(|c| match c {
            PlatformCall::ReplyCard { card, .. } => Some(card.clone()),
            _ => None,
        })
        .next()
        .expect("a switch card should be sent");
    let text = card.to_string();
    assert!(text.contains("cola 的会话"), "header names the directory: {text}");
    assert!(text.contains("修 bug"), "own-directory session shown: {text}");
    assert!(
        !text.contains("重写登录"),
        "other-directory session hidden: {text}"
    );
    assert!(text.contains("全部"), "scope-widen toggle present: {text}");
    assert!(
        text.contains("\"scope\":\"dir\""),
        "search carries dir scope: {text}"
    );
}

/// The 全部 toggle widens the switch card from the current directory to the
/// whole store, and the card then offers 本目录 to scope back down.
#[tokio::test]
async fn switch_card_scope_toggle_shows_whole_store() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let mut backend = MockBackend::new(realistic_parts());
    backend.session_list = vec![
        list_session("ses_alpha01", "重写登录", "/work/auth", 100),
        list_session("ses_beta02", "修 bug", "/work/cola", 300),
    ];
    let (app, _platform) = build_app(cfg, backend).await;
    {
        let mut store = app.sessions.lock().await;
        store.set_active(crate::config::SessionEntry {
            thread_key: crate::config::ThreadKey::new("chat_1".into(), "chat_1".into()),
            session_id: "ses_beta02".into(),
            directory: "/work/cola".into(),
            agent: None,
            model: None,
            auto_accept: false,
            topic_anchor: None,
            topic_root: None,
            variant: None,
        });
    }

    let value = serde_json::json!({
        "action": "switch",
        "op": "scope",
        "scope": "all",
        "chat_id": "chat_1",
        "thread_id": "chat_1",
    });
    let result = app
        .handle_card_action(value)
        .await
        .expect("scope toggle should return a card");
    let card = result.card.expect("toggle rebuilds the card");
    let text = card.to_string();
    assert!(text.contains("最近会话"), "all-scope header: {text}");
    assert!(text.contains("修 bug"), "own-directory session: {text}");
    assert!(
        text.contains("重写登录"),
        "other-directory session now visible: {text}"
    );
    assert!(text.contains("本目录"), "scope-back toggle present: {text}");
}

/// Without an active session the switch card has no directory to scope to,
/// so it falls back to the whole store and omits the toggle.
#[tokio::test]
async fn switch_card_falls_back_to_global_without_active_session() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let mut backend = MockBackend::new(realistic_parts());
    backend.session_list = vec![
        list_session("ses_alpha01", "重写登录", "/work/auth", 100),
        list_session("ses_beta02", "修 bug", "/work/cola", 300),
    ];
    let (app, platform) = build_app(cfg, backend).await;

    crate::bridge::command::handle_command(
        &app.core,
        Command::Switch(SwitchAction::Card),
        crate::config::ThreadKey::new("chat_1".into(), "chat_1".into()),
        "msg_switch_card",
        crate::config::ConversationKind::P2p,
    )
    .await
    .unwrap();

    let calls = platform.calls.lock().await.clone();
    let card = calls
        .iter()
        .filter_map(|c| match c {
            PlatformCall::ReplyCard { card, .. } => Some(card.clone()),
            _ => None,
        })
        .next()
        .expect("a switch card should be sent");
    let text = card.to_string();
    assert!(text.contains("最近会话"), "global fallback header: {text}");
    assert!(text.contains("重写登录"), "all sessions shown: {text}");
    assert!(text.contains("修 bug"), "all sessions shown: {text}");
    assert!(
        !text.contains("本目录"),
        "no scope-back toggle without a directory: {text}"
    );
}

/// A `/switch` card "adopt" action maps the session into the thread and
/// patches the list card IN PLACE to the Session Snapshot (ADR-0028): the
/// returned card is the snapshot — the list is no longer visible and no
/// second message is sent.
#[tokio::test]
async fn switch_card_adopt_action_maps_session() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let mut backend = MockBackend::new(realistic_parts());
    backend.session_list = vec![list_session("ses_alpha01", "重写登录", "/work/auth", 100)];
    let (app, _platform) = build_app(cfg, backend).await;

    let value = serde_json::json!({
        "action": "switch",
        "op": "adopt",
        "chat_id": "chat_1",
        "thread_id": "chat_1",
        "session_id": "ses_alpha01",
    });
    let result = app
        .handle_card_action(value)
        .await
        .expect("switch adopt should return a result");
    let card = result.card.clone().expect("adopt returns the patched card");
    let card_str = card.to_string();
    assert!(
        card_str.contains("已接管 重写登录"),
        "patched card is the snapshot: {card_str}"
    );
    assert!(
        !card_str.contains("会话管理"),
        "the list is gone from the patched card: {card_str}"
    );
    assert!(
        !card_str.contains("switch_search"),
        "the search form is gone from the patched card: {card_str}"
    );
    assert!(
        result.toast.clone().unwrap_or_default().contains("接管"),
        "adopt toasts: {:?}",
        result.toast
    );
    let key = crate::config::ThreadKey::new("chat_1".into(), "chat_1".into());
    assert_eq!(
        app.sessions.lock().await.get_active(&key).unwrap().session_id,
        "ses_alpha01"
    );
}

/// A `/switch` card "adopt" on a session owned by ANOTHER chat no longer
/// dead-ends in a "go type a command" toast: it patches the card to a
/// force-confirm card whose 强制接管 button carries the full id (the card's
/// displayed hash alone was never a valid command argument).
#[tokio::test]
async fn switch_card_adopt_occupied_offers_force_confirm() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let mut backend = MockBackend::new(realistic_parts());
    backend.session_list = vec![list_session("ses_owned", "被占用的会话", "/work/auth", 100)];
    let mut platform = RecordingPlatform::new();
    platform
        .chat_names
        .insert("oc_group_other".into(), "隔壁群".into());
    let platform = Arc::new(platform);
    let app = Arc::new(App::new(cfg, Arc::new(backend), platform.clone()).unwrap());
    {
        let mut store = app.sessions.lock().await;
        store.set_active(crate::config::SessionEntry {
            thread_key: crate::config::ThreadKey::new("oc_group_other".into(), "oc_group_other".into()),
            session_id: "ses_owned".into(),
            directory: "/work/auth".into(),
            agent: None,
            model: None,
            auto_accept: false,
            topic_anchor: None,
            topic_root: None,
            variant: None,
        });
    }

    let value = serde_json::json!({
        "action": "switch",
        "op": "adopt",
        "chat_id": "chat_1",
        "thread_id": "chat_1",
        "session_id": "ses_owned",
        "open_message_id": "om_switch_card",
    });
    let result = app.handle_card_action(value).await.expect("adopt result");
    let card = result
        .card
        .clone()
        .expect("occupied adopt returns the confirm card");
    let card_str = card.to_string();
    assert!(card_str.contains("强制接管"), "force button: {card_str}");
    assert!(card_str.contains("隔壁群"), "owner named: {card_str}");
    assert!(card_str.contains("force_adopt"), "force op payload: {card_str}");
    assert!(
        card_str.contains("ses_owned"),
        "full id in the payload: {card_str}"
    );
    assert!(
        result.toast.clone().unwrap_or_default().contains("占用"),
        "toast: {:?}",
        result.toast
    );
    // Not adopted yet: the lobby thread still has no session.
    let lobby = crate::config::ThreadKey::new("chat_1".into(), "chat_1".into());
    assert!(app.sessions.lock().await.get_active(&lobby).is_none());
}

/// Clicking 强制接管 on the confirm card steals the mapping from the other
/// chat and adopts into this one (the card equivalent of
/// `/switch <id> --force`).
#[tokio::test]
async fn switch_card_force_adopt_steals_owned_session() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let mut backend = MockBackend::new(realistic_parts());
    backend.session_list = vec![list_session("ses_owned", "被占用的会话", "/work/auth", 100)];
    let mut platform = RecordingPlatform::new();
    platform
        .chat_names
        .insert("oc_group_other".into(), "隔壁群".into());
    let platform = Arc::new(platform);
    let app = Arc::new(App::new(cfg, Arc::new(backend), platform.clone()).unwrap());
    let other = crate::config::ThreadKey::new("oc_group_other".into(), "oc_group_other".into());
    {
        let mut store = app.sessions.lock().await;
        store.set_active(crate::config::SessionEntry {
            thread_key: other.clone(),
            session_id: "ses_owned".into(),
            directory: "/work/auth".into(),
            agent: None,
            model: None,
            auto_accept: false,
            topic_anchor: None,
            topic_root: None,
            variant: None,
        });
    }

    let value = serde_json::json!({
        "action": "switch",
        "op": "force_adopt",
        "chat_id": "chat_1",
        "thread_id": "chat_1",
        "session_id": "ses_owned",
    });
    let result = app.handle_card_action(value).await.expect("force_adopt result");
    let card = result.card.clone().expect("force_adopt returns the snapshot");
    assert!(
        card.to_string().contains("已接管 被占用的会话"),
        "snapshot header: {card}"
    );
    let lobby = crate::config::ThreadKey::new("chat_1".into(), "chat_1".into());
    assert_eq!(
        app.sessions.lock().await.get_active(&lobby).unwrap().session_id,
        "ses_owned"
    );
    assert!(
        app.sessions.lock().await.get_active(&other).is_none(),
        "the old owner becomes sessionless"
    );
}

/// The confirm card's 返回列表 button rebuilds the session list card.
#[tokio::test]
async fn switch_card_back_rebuilds_list() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let mut backend = MockBackend::new(realistic_parts());
    backend.session_list = vec![list_session("ses_alpha01", "重写登录", "/work/auth", 100)];
    let (app, _platform) = build_app(cfg, backend).await;

    let value = serde_json::json!({
        "action": "switch",
        "op": "back",
        "chat_id": "chat_1",
        "thread_id": "chat_1",
        "scope": "all",
    });
    let result = app.handle_card_action(value).await.expect("back result");
    let card = result.card.expect("back rebuilds the list card");
    let card_str = card.to_string();
    assert!(card_str.contains("会话管理"), "list header: {card_str}");
    assert!(card_str.contains("重写登录"), "session row: {card_str}");
}

/// ADR-0028: a 切换 on a row of a session already mapped to this thread
/// patches the card to a snapshot with the 已切换 verb — a mapped re-switch
/// is an activation and reports content (external newness here).
#[tokio::test]
async fn switch_card_switch_on_mapped_session_patches_to_snapshot() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let mut backend = MockBackend::new(realistic_parts());
    backend.session_list = vec![list_session("ses_own1", "本项目会话", "/work/cola", 500)];
    // External newness → content to report → full snapshot, not suppressed.
    backend
        .external_user_messages
        .insert("ses_own1".into(), "OpenChamber 里的问题".into());
    let (app, _platform) = build_app(cfg, backend).await;
    // The session is mapped to this thread but NOT active (a stacked
    // session) — the row shows 切换.
    {
        let mut store = app.sessions.lock().await;
        store.set_active(crate::config::SessionEntry {
            thread_key: crate::config::ThreadKey::new("chat_1".into(), "chat_1".into()),
            session_id: "ses_own1".into(),
            directory: "/work/cola".into(),
            agent: None,
            model: None,
            auto_accept: false,
            topic_anchor: None,
            topic_root: None,
            variant: None,
        });
        store.set_active(crate::config::SessionEntry {
            thread_key: crate::config::ThreadKey::new("chat_1".into(), "chat_1".into()),
            session_id: "ses_active".into(),
            directory: "/work/active".into(),
            agent: None,
            model: None,
            auto_accept: false,
            topic_anchor: None,
            topic_root: None,
            variant: None,
        });
    }

    let value = serde_json::json!({
        "action": "switch",
        "op": "adopt",
        "chat_id": "chat_1",
        "thread_id": "chat_1",
        "session_id": "ses_own1",
    });
    let result = app
        .handle_card_action(value)
        .await
        .expect("switch on a mapped row should return a result");
    let card = result.card.expect("mapped re-switch patches the card");
    let card_str = card.to_string();
    assert!(
        card_str.contains("已切换 本项目会话"),
        "re-switch verb: {card_str}"
    );
    assert!(
        card_str.contains("OpenChamber 里的问题"),
        "external message surfaces in the tail: {card_str}"
    );
    assert!(
        result.toast.clone().unwrap_or_default().contains("已切换"),
        "re-switch toasts the verb: {:?}",
        result.toast
    );
    // The mapped session is now the thread's ACTIVE one.
    let key = crate::config::ThreadKey::new("chat_1".into(), "chat_1".into());
    assert_eq!(
        app.sessions.lock().await.get_active(&key).unwrap().session_id,
        "ses_own1"
    );
}

/// ADR-0028: a 切换 on a fully-visible mapped session (idle, no pending,
/// newest user message cola-authored) patches to the COMPACT 「已切换」 state
/// card instead of a full snapshot — mirroring the text form's one-line
/// ack under suppression.
#[tokio::test]
async fn switch_card_switch_suppressed_patches_to_compact_state() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let mut backend = MockBackend::new(realistic_parts());
    backend.session_list = vec![list_session("ses_own1", "本项目会话", "/work/cola", 500)];
    backend
        .cola_user_messages
        .insert("ses_own1".into(), "上次的问题".into());
    let (app, _platform) = build_app(cfg, backend).await;
    // ses_own1 is mapped to this thread but NOT active (a stacked
    // session) — the row shows 切换, and this is a re-activation, not the
    // already-active ✅ row.
    {
        let mut store = app.sessions.lock().await;
        store.set_active(crate::config::SessionEntry {
            thread_key: crate::config::ThreadKey::new("chat_1".into(), "chat_1".into()),
            session_id: "ses_own1".into(),
            directory: "/work/cola".into(),
            agent: None,
            model: None,
            auto_accept: false,
            topic_anchor: None,
            topic_root: None,
            variant: None,
        });
        store.set_active(crate::config::SessionEntry {
            thread_key: crate::config::ThreadKey::new("chat_1".into(), "chat_1".into()),
            session_id: "ses_active".into(),
            directory: "/work/active".into(),
            agent: None,
            model: None,
            auto_accept: false,
            topic_anchor: None,
            topic_root: None,
            variant: None,
        });
    }

    let value = serde_json::json!({
        "action": "switch",
        "op": "adopt",
        "chat_id": "chat_1",
        "thread_id": "chat_1",
        "session_id": "ses_own1",
    });
    let result = app
        .handle_card_action(value)
        .await
        .expect("suppressed switch should return a result");
    let card = result.card.expect("suppressed 切换 still patches the card");
    let card_str = card.to_string();
    assert!(
        card_str.contains("已切换 本项目会话"),
        "compact state header: {card_str}"
    );
    assert!(
        !card_str.contains("最近对话"),
        "no full snapshot tail on a suppressed 切换: {card_str}"
    );
    assert!(
        !card_str.contains("已接管"),
        "suppressed re-switch is 已切换, never 已接管: {card_str}"
    );
}

/// ADR-0028: when the switch card lives inside a topic (a never-bound
/// topic adopting its single session), the patched card is the in-topic
/// confirmation — its own message id (`open_message_id`) is persisted as
/// the fallback-card anchor, so later permission/question cards keep
/// routing inside the topic.
#[tokio::test]
async fn switch_card_adopt_in_topic_persists_anchor() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let mut backend = MockBackend::new(realistic_parts());
    backend.session_list = vec![list_session("ses_alpha01", "重写登录", "/work/auth", 100)];
    let (app, _platform) = build_app(cfg, backend).await;

    let value = serde_json::json!({
        "action": "switch",
        "op": "adopt",
        "chat_id": "chat_1",
        "thread_id": "omt_fresh",
        "session_id": "ses_alpha01",
        "open_message_id": "om_switch_card",
    });
    let result = app
        .handle_card_action(value)
        .await
        .expect("topic adopt should return a result");
    let card = result.card.expect("adopt patches the card in place");
    assert!(
        card.to_string().contains("已接管"),
        "snapshot in the topic: {card}"
    );
    let topic_key = crate::config::ThreadKey::new("chat_1".into(), "omt_fresh".into());
    let entry = app
        .sessions
        .lock()
        .await
        .get_active(&topic_key)
        .cloned()
        .expect("the topic maps to the adopted session");
    assert_eq!(entry.session_id, "ses_alpha01");
    assert_eq!(
        entry.topic_anchor.as_deref(),
        Some("om_switch_card"),
        "the patched card is the in-topic fallback anchor"
    );
}

/// A `/switch` card "new" action creates a session in the current project
/// (equivalent to `/new`) and maps it as active.
#[tokio::test]
async fn switch_card_new_action_creates_session_in_current_project() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let (app, _platform) = build_app(cfg, MockBackend::new(realistic_parts())).await;
    let key = crate::config::ThreadKey::new("chat_1".into(), "chat_1".into());
    // Root an existing session in /work/proj so "current project" is set.
    {
        let mut store = app.sessions.lock().await;
        store.set_active(crate::config::SessionEntry {
            thread_key: key.clone(),
            session_id: "ses_old".into(),
            directory: "/work/proj".into(),
            agent: None,
            model: None,
            auto_accept: false,
            topic_anchor: None,
            topic_root: None,
            variant: None,
        });
    }

    let value = serde_json::json!({
        "action": "switch",
        "op": "new",
        "chat_id": "chat_1",
        "thread_id": "chat_1",
    });
    let result = app
        .handle_card_action(value)
        .await
        .expect("switch new should return a result");
    assert!(result.card.is_some(), "new returns a refreshed card");
    let entry = app.sessions.lock().await.get_active(&key).cloned().unwrap();
    assert_ne!(entry.session_id, "ses_old", "a fresh session is created");
    assert_eq!(entry.directory, "/work/proj", "inherits the current project");
}
