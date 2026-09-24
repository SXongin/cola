use crate::bridge::test_support::*;

#[tokio::test]
async fn list_shows_global_sessions_marking_own() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let mut backend = MockBackend::new(realistic_parts());
    backend.given_sessions(vec![
        list_session("ses_alpha01", "外部会话", "/tmp/ext", 100),
        list_session("ses_beta02", "本地会话", "/work/cola", 300),
    ]);
    let (app, platform) = build_app(cfg, backend).await;
    // Our own lobby session, so /list marks it active (ADR-0022: only the
    // active session is marked; the 本会话 ownership marker is gone).
    seed_entry(
        &app,
        crate::config::SessionEntry {
            thread_key: crate::config::ThreadKey::new("chat_1".into(), "chat_1".into()),
            session_id: "ses_beta02".into(),
            directory: "/work/cola".into(),
            agent: None,
            model: None,
            auto_accept: false,
            topic_anchor: None,
            topic_root: None,
            variant: None,
        },
    )
    .await;

    send_command(&app, "/switch list", "msg_list").await;

    let text = platform.texts().await.join("\n");
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
    backend.given_sessions(vec![
        list_session("ses_alpha01", "重写登录模块", "/work/auth", 100),
        list_session("ses_beta02", "修 bug", "/work/cola", 300),
        opencode::types::SessionListInfo {
            parent_id: Some("ses_alpha01".into()),
            ..list_session("ses_child09", "Child session - x", "/work/auth", 400)
        },
    ]);
    let (app, platform) = build_app(cfg, backend).await;

    // Keyword filters by title.
    send_command(&app, "/switch list 登录", "msg_list").await;
    let text = platform.texts().await.join("\n");
    assert!(text.contains("重写登录模块"), "keyword match: {text}");
    assert!(!text.contains("修 bug"), "non-matching title filtered: {text}");

    // Without --all the child is hidden even though it is newest.
    platform.calls.lock().await.clear();
    send_command(&app, "/switch list", "msg_list2").await;
    let text = platform.texts().await.join("\n");
    assert!(!text.contains("Child session"), "child hidden by default: {text}");

    // --all reveals the child.
    platform.calls.lock().await.clear();
    send_command(&app, "/switch list --all", "msg_list3").await;
    let text = platform.texts().await.join("\n");
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
    backend.given_sessions(vec![list_session("ses_alpha01", "标题 A", "/work/a", 100)]);
    let calls_counter = backend.list_sessions_calls.clone();
    let (app, _platform) = build_app(cfg, backend).await;
    let key = crate::config::ThreadKey::new("chat_1".into(), "chat_1".into());
    seed_entry(
        &app,
        crate::config::SessionEntry {
            thread_key: key.clone(),
            session_id: "ses_alpha01".into(),
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

    // Two /list in a row → one server fetch.
    send_command_in(
        &app,
        "/switch list",
        key.clone(),
        "m1",
        crate::config::ConversationKind::P2p,
    )
    .await;
    send_command_in(
        &app,
        "/switch list",
        key.clone(),
        "m2",
        crate::config::ConversationKind::P2p,
    )
    .await;
    assert_eq!(calls_counter.load(std::sync::atomic::Ordering::SeqCst), 1);

    // A rename invalidates the cache → next /list refetches.
    send_command_in(
        &app,
        "/name 新名字",
        key.clone(),
        "m3",
        crate::config::ConversationKind::P2p,
    )
    .await;
    send_command_in(
        &app,
        "/switch list",
        key,
        "m4",
        crate::config::ConversationKind::P2p,
    )
    .await;
    assert_eq!(calls_counter.load(std::sync::atomic::Ordering::SeqCst), 2);
}

#[tokio::test]
async fn attach_adopts_foreign_session_by_id() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let mut backend = MockBackend::new(realistic_parts());
    backend.given_sessions(vec![list_session(
        "ses_foreign123abc",
        "OpenChamber 里的任务",
        "/work/foreign",
        100,
    )]);
    let (app, _platform) = build_app(cfg, backend).await;

    send_command(&app, "/switch ses_foreign123abc", "msg_attach").await;

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
    backend.given_sessions(vec![list_session(
        "ses_foreign123abc",
        "OpenChamber 里的任务",
        "/work/foreign",
        100,
    )]);
    let mut platform = RecordingPlatform::new();
    platform
        .chat_names
        .insert("oc_group_other".into(), "隔壁群".into());
    let platform = Arc::new(platform);
    let app = Arc::new(App::new(cfg, Arc::new(backend), platform.clone()).unwrap());
    // Another thread already owns the session.
    seed_entry(
        &app,
        crate::config::SessionEntry {
            thread_key: crate::config::ThreadKey::new("oc_group_other".into(), "oc_group_other".into()),
            session_id: "ses_foreign123abc".into(),
            directory: "/work/foreign".into(),
            agent: None,
            model: None,
            auto_accept: false,
            topic_anchor: None,
            topic_root: None,
            variant: None,
        },
    )
    .await;

    send_command(&app, "/switch ses_foreign123abc", "msg_attach").await;

    // Rejected: the current thread still has no session.
    let key = crate::config::ThreadKey::new("chat_1".into(), "chat_1".into());
    assert!(app.sessions.lock().await.get_active(&key).is_none());
    let text = platform.texts().await.join("\n");
    assert!(text.contains("隔壁群"), "rejection names the owning chat: {text}");
    assert!(text.contains("--force"), "rejection points at --force: {text}");
}

#[tokio::test]
async fn attach_force_steals_mapping_from_other_thread() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let mut backend = MockBackend::new(realistic_parts());
    backend.given_sessions(vec![list_session(
        "ses_foreign123abc",
        "OpenChamber 里的任务",
        "/work/foreign",
        100,
    )]);
    let (app, _platform) = build_app(cfg, backend).await;
    seed_entry(
        &app,
        crate::config::SessionEntry {
            thread_key: crate::config::ThreadKey::new("oc_group_other".into(), "oc_group_other".into()),
            session_id: "ses_foreign123abc".into(),
            directory: "/work/foreign".into(),
            agent: None,
            model: None,
            auto_accept: false,
            topic_anchor: None,
            topic_root: None,
            variant: None,
        },
    )
    .await;

    send_command(&app, "/switch ses_foreign123abc --force", "msg_attach").await;

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

    send_command(&app, "/switch forget", "msg_forget").await;

    let key = crate::config::ThreadKey::new("chat_1".into(), "chat_1".into());
    assert!(app.sessions.lock().await.get_active(&key).is_none());
}

#[tokio::test]
async fn switch_adopts_unique_foreign_session() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let mut backend = MockBackend::new(realistic_parts());
    backend.given_sessions(vec![list_session(
        "ses_alpha01",
        "唯一外部标题",
        "/work/ext",
        100,
    )]);
    let (app, _platform) = build_app(cfg, backend).await;

    send_command(&app, "/switch 唯一外部标题", "msg_switch").await;

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
    backend.given_sessions(vec![list_session(
        "ses_alpha01",
        "唯一外部标题",
        "/work/ext",
        100,
    )]);
    let (app, platform) = build_app(cfg, backend).await;

    send_command(&app, "/switch 唯一外部标题", "msg_switch").await;

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
    backend.cola_message("ses_own1", "上次的问题");
    let (app, platform, _dir) = build_reeswitch_app(backend).await;

    send_command(&app, "/switch 本项目", "msg_switch").await;

    let calls = platform.calls.lock().await.clone();
    let text = platform.texts().await.join("\n");
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
    backend.external_message_for("ses_own1", "OpenChamber 里的问题");
    let (app, platform, _dir) = build_reeswitch_app(backend).await;

    send_command(&app, "/switch 本项目", "msg_switch").await;

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
    backend.with_session_status("ses_own1", Some(opencode::types::SessionStatus::Busy));
    // Cola-authored newest alone is NOT enough to suppress a busy session.
    backend.cola_message("ses_own1", "上次的问题");
    let (app, platform, _dir) = build_reeswitch_app(backend).await;

    send_command(&app, "/switch 本项目", "msg_switch").await;

    let card = platform
        .replied_cards()
        .await
        .into_iter()
        .next()
        .expect("busy reports a snapshot")
        .to_string();
    assert!(
        card.contains(crate::feishu::snapshot_card::BUSY_CHIP),
        "busy chip: {card}"
    );
}

/// ADR-0028: an adopt-time pending request on a re-switch reports a
/// snapshot with the 等待你的确认 chip — the request is blocked on the
/// operator, so the snapshot must surface it.
#[tokio::test]
async fn switch_reeswitch_snapshots_on_pending_permission() {
    let _wd = test_work_dir();
    let mut backend = MockBackend::new(realistic_parts());
    backend.cola_message("ses_own1", "上次的问题");
    backend.ask_permissions(vec![opencode::types::PermissionRequest {
        request_id: "req_own".into(),
        session_id: Some("ses_own1".into()),
        permission: Some("bash".into()),
        patterns: vec!["ls".into()],
        metadata: None,
        always: vec![],
    }]);
    let (app, platform, _dir) = build_reeswitch_app(backend).await;

    send_command(&app, "/switch 本项目", "msg_switch").await;

    let card = platform
        .replied_cards()
        .await
        .into_iter()
        .next()
        .expect("a pending request reports a snapshot")
        .to_string();
    assert!(
        card.contains(crate::feishu::snapshot_card::WAITING_CHIP),
        "waiting chip: {card}"
    );
}

#[tokio::test]
async fn switch_ambiguous_global_match_lists_candidates() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let mut backend = MockBackend::new(realistic_parts());
    backend.given_sessions(vec![
        list_session("ses_alpha01", "任务 A", "/work/a", 100),
        list_session("ses_beta02", "任务 B", "/work/b", 200),
    ]);
    let (app, platform) = build_app(cfg, backend).await;

    send_command(&app, "/switch 任务", "msg_switch").await;

    // Ambiguous → no adoption, candidates listed.
    let key = crate::config::ThreadKey::new("chat_1".into(), "chat_1".into());
    assert!(app.sessions.lock().await.get_active(&key).is_none());
    let text = platform.texts().await.join("\n");
    assert!(text.contains("/switch"), "points at /switch: {text}");
}

#[tokio::test]
async fn switch_prefers_threads_own_sessions() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let mut backend = MockBackend::new(realistic_parts());
    backend.given_sessions(vec![
        list_session("ses_own1", "本项目会话", "/work/cola", 500),
        list_session("ses_foreign", "本项目会话", "/other/place", 100),
    ]);
    let (app, _platform) = build_app(cfg, backend).await;
    seed_entry(
        &app,
        crate::config::SessionEntry {
            thread_key: crate::config::ThreadKey::new("chat_1".into(), "chat_1".into()),
            session_id: "ses_own1".into(),
            directory: "/work/cola".into(),
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
            thread_key: crate::config::ThreadKey::new("chat_1".into(), "chat_1".into()),
            session_id: "ses_other_own".into(),
            directory: "/work/other".into(),
            agent: None,
            model: None,
            auto_accept: false,
            topic_anchor: None,
            topic_root: None,
            variant: None,
        },
    )
    .await;

    send_command(&app, "/switch 本项目", "msg_switch").await;

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
    backend.given_sessions(vec![
        list_session("ses_alpha01", "重写登录", "/work/auth", 100),
        list_session("ses_beta02", "修 bug", "/work/cola", 300),
    ]);
    let (app, platform) = build_app(cfg, backend).await;

    send_command(&app, "/switch", "msg_switch_card").await;

    let card = platform
        .replied_cards()
        .await
        .into_iter()
        .next()
        .expect("a switch card should be sent");
    let text = card.to_string();
    assert!(text.contains("会话管理"), "header: {text}");
    assert!(text.contains("重写登录"), "session row: {text}");
    assert!(text.contains("修 bug"), "session row: {text}");
    assert!(text.contains("接管"), "adopt button: {text}");
    assert!(text.contains("＋ 新建会话"), "new button: {text}");
    assert!(text.contains("switch_search"), "search form: {text}");
    // The row buttons carry the structured payload (C4 query helper): each
    // listed session has an adopt button.
    let values = platform.button_values().await;
    for id in ["ses_alpha01", "ses_beta02"] {
        assert!(
            values.iter().any(|v| v["session_id"] == id && v["op"] == "adopt"),
            "session {id} needs an adopt button: {values:?}"
        );
    }
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
    backend.given_sessions(vec![
        list_session("ses_alpha01", "重写登录", "/work/auth", 100),
        list_session("ses_beta02", "修 bug", "/work/cola", 300),
    ]);
    let (app, platform) = build_app(cfg, backend).await;
    seed_entry(
        &app,
        crate::config::SessionEntry {
            thread_key: crate::config::ThreadKey::new("chat_1".into(), "chat_1".into()),
            session_id: "ses_beta02".into(),
            directory: "/work/cola".into(),
            agent: None,
            model: None,
            auto_accept: false,
            topic_anchor: None,
            topic_root: None,
            variant: None,
        },
    )
    .await;

    send_command(&app, "/switch", "msg_switch_card").await;

    let calls = platform.calls.lock().await.clone();
    let card = calls
        .iter()
        .filter_map(|c| match c {
            PlatformCall::ReplyCard { card, .. } => Some(card.clone()),
            _ => None,
        })
        .next()
        .expect("a switch card should be sent");
    let text = card_text(&card);
    assert!(text.contains("cola 的会话"), "header names the directory: {text}");
    assert!(text.contains("修 bug"), "own-directory session shown: {text}");
    assert!(
        !text.contains("重写登录"),
        "other-directory session hidden: {text}"
    );
    assert!(text.contains("全部"), "scope-widen toggle present: {text}");
    assert!(
        card_buttons(&card).iter().any(|b| b["value"]["scope"] == "dir"),
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
    backend.given_sessions(vec![
        list_session("ses_alpha01", "重写登录", "/work/auth", 100),
        list_session("ses_beta02", "修 bug", "/work/cola", 300),
    ]);
    let (app, _platform) = build_app(cfg, backend).await;
    seed_entry(
        &app,
        crate::config::SessionEntry {
            thread_key: crate::config::ThreadKey::new("chat_1".into(), "chat_1".into()),
            session_id: "ses_beta02".into(),
            directory: "/work/cola".into(),
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
        "action": "switch",
        "op": "scope",
        "scope": "all",
        "chat_id": "chat_1",
        "thread_id": "chat_1",
    });
    let result = app
        .host_action(value)
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
    backend.given_sessions(vec![
        list_session("ses_alpha01", "重写登录", "/work/auth", 100),
        list_session("ses_beta02", "修 bug", "/work/cola", 300),
    ]);
    let (app, platform) = build_app(cfg, backend).await;

    send_command(&app, "/switch", "msg_switch_card").await;

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
    backend.given_sessions(vec![list_session("ses_alpha01", "重写登录", "/work/auth", 100)]);
    let (app, _platform) = build_app(cfg, backend).await;

    let value = serde_json::json!({
        "action": "switch",
        "op": "adopt",
        "chat_id": "chat_1",
        "thread_id": "chat_1",
        "session_id": "ses_alpha01",
    });
    let result = app
        .host_action(value)
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
    backend.given_sessions(vec![list_session("ses_owned", "被占用的会话", "/work/auth", 100)]);
    let mut platform = RecordingPlatform::new();
    platform
        .chat_names
        .insert("oc_group_other".into(), "隔壁群".into());
    let platform = Arc::new(platform);
    let app = Arc::new(App::new(cfg, Arc::new(backend), platform.clone()).unwrap());
    seed_entry(
        &app,
        crate::config::SessionEntry {
            thread_key: crate::config::ThreadKey::new("oc_group_other".into(), "oc_group_other".into()),
            session_id: "ses_owned".into(),
            directory: "/work/auth".into(),
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
        "action": "switch",
        "op": "adopt",
        "chat_id": "chat_1",
        "thread_id": "chat_1",
        "session_id": "ses_owned",
        "open_message_id": "om_switch_card",
    });
    let result = app.host_action(value).await.expect("adopt result");
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
    backend.given_sessions(vec![list_session("ses_owned", "被占用的会话", "/work/auth", 100)]);
    let mut platform = RecordingPlatform::new();
    platform
        .chat_names
        .insert("oc_group_other".into(), "隔壁群".into());
    let platform = Arc::new(platform);
    let app = Arc::new(App::new(cfg, Arc::new(backend), platform.clone()).unwrap());
    let other = crate::config::ThreadKey::new("oc_group_other".into(), "oc_group_other".into());
    seed_entry(
        &app,
        crate::config::SessionEntry {
            thread_key: other.clone(),
            session_id: "ses_owned".into(),
            directory: "/work/auth".into(),
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
        "action": "switch",
        "op": "force_adopt",
        "chat_id": "chat_1",
        "thread_id": "chat_1",
        "session_id": "ses_owned",
    });
    let result = app.host_action(value).await.expect("force_adopt result");
    let card = result.card.clone().expect("force_adopt returns the snapshot");
    assert!(
        card_text(&card).contains("已接管 被占用的会话"),
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
    backend.given_sessions(vec![list_session("ses_alpha01", "重写登录", "/work/auth", 100)]);
    let (app, _platform) = build_app(cfg, backend).await;

    let value = serde_json::json!({
        "action": "switch",
        "op": "back",
        "chat_id": "chat_1",
        "thread_id": "chat_1",
        "scope": "all",
    });
    let result = app.host_action(value).await.expect("back result");
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
    backend.given_sessions(vec![list_session("ses_own1", "本项目会话", "/work/cola", 500)]);
    // External newness → content to report → full snapshot, not suppressed.
    backend.external_message_for("ses_own1", "OpenChamber 里的问题");
    let (app, _platform) = build_app(cfg, backend).await;
    // The session is mapped to this thread but NOT active (a stacked
    // session) — the row shows 切换.
    seed_entry(
        &app,
        crate::config::SessionEntry {
            thread_key: crate::config::ThreadKey::new("chat_1".into(), "chat_1".into()),
            session_id: "ses_own1".into(),
            directory: "/work/cola".into(),
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
            thread_key: crate::config::ThreadKey::new("chat_1".into(), "chat_1".into()),
            session_id: "ses_active".into(),
            directory: "/work/active".into(),
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
        "action": "switch",
        "op": "adopt",
        "chat_id": "chat_1",
        "thread_id": "chat_1",
        "session_id": "ses_own1",
    });
    let result = app
        .host_action(value)
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
    backend.given_sessions(vec![list_session("ses_own1", "本项目会话", "/work/cola", 500)]);
    backend.cola_message("ses_own1", "上次的问题");
    let (app, _platform) = build_app(cfg, backend).await;
    // ses_own1 is mapped to this thread but NOT active (a stacked
    // session) — the row shows 切换, and this is a re-activation, not the
    // already-active ✅ row.
    seed_entry(
        &app,
        crate::config::SessionEntry {
            thread_key: crate::config::ThreadKey::new("chat_1".into(), "chat_1".into()),
            session_id: "ses_own1".into(),
            directory: "/work/cola".into(),
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
            thread_key: crate::config::ThreadKey::new("chat_1".into(), "chat_1".into()),
            session_id: "ses_active".into(),
            directory: "/work/active".into(),
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
        "action": "switch",
        "op": "adopt",
        "chat_id": "chat_1",
        "thread_id": "chat_1",
        "session_id": "ses_own1",
    });
    let result = app
        .host_action(value)
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
    backend.given_sessions(vec![list_session("ses_alpha01", "重写登录", "/work/auth", 100)]);
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
        .host_action(value)
        .await
        .expect("topic adopt should return a result");
    let card = result.card.expect("adopt patches the card in place");
    assert!(
        card_text(&card).contains("已接管"),
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

/// A `/switch` card "new" action declares a Pending Session in the current
/// project (equivalent to `/new`, ADR-0041) — it creates NO server session.
#[tokio::test]
async fn switch_card_new_action_declares_pending_in_current_project() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let backend = MockBackend::new(realistic_parts());
    let created = backend.created_session_dirs.clone();
    let (app, _platform) = build_app(cfg, backend).await;
    let key = crate::config::ThreadKey::new("chat_1".into(), "chat_1".into());
    // Root an existing session in /work/proj so "current project" is set.
    seed_entry(
        &app,
        crate::config::SessionEntry {
            thread_key: key.clone(),
            session_id: "ses_old".into(),
            directory: "/work/proj".into(),
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
        "action": "switch",
        "op": "new",
        "chat_id": "chat_1",
        "thread_id": "chat_1",
    });
    let result = app
        .host_action(value)
        .await
        .expect("switch new should return a result");
    assert!(result.card.is_some(), "new returns a refreshed card");
    assert_eq!(
        result.toast.as_deref(),
        Some("下一条消息创建会话"),
        "the toast states the creation timing"
    );
    assert!(
        created.lock().await.is_empty(),
        "the card's 新建 must not create a server session"
    );
    let pending = app
        .sessions
        .lock()
        .await
        .pending_for(&key)
        .cloned()
        .expect("a pending is declared");
    assert_eq!(pending.directory, "/work/proj", "inherits the current project");
}

/// Re-adopting an already-mapped session through the switch card must not
/// reset its per-session overrides (ADR-0041 settings belong to the session):
/// auto-accept survived `/new` + a card re-switch in production, then a
/// permission surfaced as a card instead of being auto-accepted.
#[tokio::test]
async fn switch_card_readopt_keeps_per_session_overrides() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let mut backend = MockBackend::new(realistic_parts());
    backend.given_sessions(vec![list_session("ses_own1", "本项目会话", "/work/cola", 500)]);
    backend.external_message_for("ses_own1", "OpenChamber 里的问题");
    let (app, _platform) = build_app(cfg, backend).await;
    let key = crate::config::ThreadKey::new("chat_1".into(), "chat_1".into());
    // The target is mapped but NOT active (a stacked session); its cola-side
    // overrides are set.
    seed_overridden_entry(&app, key.clone(), "ses_own1", "/work/cola").await;
    seed_entry(
        &app,
        crate::config::SessionEntry::new(key.clone(), "ses_active", "/work/active"),
    )
    .await;

    let value = serde_json::json!({
        "action": "switch",
        "op": "adopt",
        "chat_id": "chat_1",
        "thread_id": "chat_1",
        "session_id": "ses_own1",
    });
    app.host_action(value)
        .await
        .expect("switch on a mapped row should return a result");

    let entry = app
        .sessions
        .lock()
        .await
        .entry_for_session("ses_own1")
        .cloned()
        .expect("the re-adopted entry exists");
    assert!(entry.auto_accept, "auto-accept survives the re-adoption");
    assert_eq!(entry.model.as_deref(), Some("provider/model-a"));
    assert_eq!(entry.variant.as_deref(), Some("high"));
    assert_eq!(
        app.sessions.lock().await.get_active(&key).unwrap().session_id,
        "ses_own1",
        "the mapping still moves to the adopted session"
    );
}

/// The text `/attach` path (`adopt_session`) re-adopts through the same
/// rebuild: per-session overrides must survive there too.
#[tokio::test]
async fn attach_readopt_keeps_per_session_overrides() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let mut backend = MockBackend::new(realistic_parts());
    backend.given_sessions(vec![list_session("ses_own1", "本项目会话", "/work/cola", 500)]);
    let (app, _platform) = build_app(cfg, backend).await;
    let key = crate::config::ThreadKey::new("chat_1".into(), "chat_1".into());
    seed_overridden_entry(&app, key.clone(), "ses_own1", "/work/cola").await;
    seed_entry(
        &app,
        crate::config::SessionEntry::new(key.clone(), "ses_active", "/work/active"),
    )
    .await;

    send_command_in(
        &app,
        "/switch ses_own1",
        key.clone(),
        "msg_attach",
        crate::config::ConversationKind::P2p,
    )
    .await;

    let entry = app
        .sessions
        .lock()
        .await
        .entry_for_session("ses_own1")
        .cloned()
        .expect("the re-adopted entry exists");
    assert!(entry.auto_accept, "auto-accept survives the re-adoption");
    assert_eq!(entry.model.as_deref(), Some("provider/model-a"));
    assert_eq!(entry.variant.as_deref(), Some("high"));
    assert_eq!(
        app.sessions.lock().await.get_active(&key).unwrap().session_id,
        "ses_own1"
    );
}

/// ADR-0052: the text `/switch` entry opens on page 1 — the six most recent
/// sessions — with a pager between the rows and the ＋新建 footer, 上一页
/// disabled at the boundary, and every row button carrying keyword/scope/page.
#[tokio::test]
async fn switch_command_starts_on_the_first_page() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let (app, platform) = build_app(cfg, backend_with_sessions(8)).await;

    send_command(&app, "/switch", "msg_switch").await;

    let card = platform
        .replied_cards()
        .await
        .into_iter()
        .next()
        .expect("the /switch command replies with a card");
    let text = card_text(&card);
    assert!(
        text.contains("项目8 ·") && text.contains("项目3 ·"),
        "page 1 holds the most recent six: {text}"
    );
    assert!(
        !text.contains("项目2 ·") && !text.contains("项目1 ·"),
        "page 2's rows are not on the first page: {text}"
    );
    assert_eq!(
        switch_pager_label(&card).as_deref(),
        Some("第 1/2 页 · 共 8 个"),
        "text /switch reports page 1: {text}"
    );
    assert_eq!(
        pager_button(&card, "上一页")["disabled"],
        true,
        "page 1 disables 上一页"
    );
    assert_eq!(
        pager_button(&card, "下一页")["disabled"],
        false,
        "page 1 keeps 下一页 live"
    );
    // The pager renders below the rows and above the ＋新建 footer.
    let texts = card_texts(&card);
    let pager_pos = texts.iter().position(|t| t.contains("页 · 共")).unwrap();
    let new_pos = texts.iter().position(|t| t == "＋ 新建会话").unwrap();
    assert!(pager_pos < new_pos, "pager sits above the footer: {texts:?}");
    let values = platform.button_values().await;
    assert!(
        values.iter().any(|v| v["op"] == "adopt"
            && v["session_id"] == "ses_p8"
            && v["keyword"] == ""
            && v["scope"] == "all"
            && v["page"] == 1),
        "row buttons carry keyword/scope/page: {values:?}"
    );
}

/// ADR-0052: submitting a search always rebuilds at page 1 — a stale page
/// riding the payload is discarded — while the keyword is kept.
#[tokio::test]
async fn switch_card_search_resets_to_the_first_page() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let (app, _platform) = build_app(cfg, backend_with_sessions(8)).await;

    let search = serde_json::json!({
        "action": "switch",
        "op": "search",
        "chat_id": "chat_1",
        "thread_id": "chat_1",
        "keyword": "proj",
        "scope": "all",
        // A stale page from an older, longer result.
        "page": 2,
    });
    let card = app
        .host_action(search)
        .await
        .expect("switch search should return a result")
        .card
        .expect("search refreshes the card");
    let text = card_text(&card);
    assert!(
        text.contains("项目8 ·") && text.contains("项目3 ·"),
        "page 1 holds the most recent six: {text}"
    );
    assert!(
        !text.contains("项目2 ·") && !text.contains("项目1 ·"),
        "page 2's rows are not on the rebuilt page 1: {text}"
    );
    assert_eq!(
        switch_pager_label(&card).as_deref(),
        Some("第 1/2 页 · 共 8 个"),
        "the search landed on page 1: {text}"
    );
    assert!(
        card.to_string().contains("\"default_value\":\"proj\""),
        "keyword echoed into the search box: {card}"
    );
}

/// ADR-0052: the scope toggle also rebuilds at page 1, keeping the keyword.
#[tokio::test]
async fn switch_card_scope_toggle_resets_to_the_first_page() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let (app, _platform) = build_app(cfg, backend_with_sessions(8)).await;
    // A current directory so the 本目录 toggle renders in the all-scope card
    // (a directory with no listed session, so the list stays at eight rows).
    seed_entry(
        &app,
        crate::config::SessionEntry::new(
            crate::config::ThreadKey::new("chat_1".into(), "chat_1".into()),
            "ses_other",
            "/work/other",
        ),
    )
    .await;

    let toggle = serde_json::json!({
        "action": "switch",
        "op": "scope",
        "chat_id": "chat_1",
        "thread_id": "chat_1",
        "keyword": "proj",
        "scope": "all",
        "page": 2,
    });
    let card = app
        .host_action(toggle)
        .await
        .expect("scope toggle should return a result")
        .card
        .expect("the toggle rebuilds the card");
    let text = card_text(&card);
    assert!(
        text.contains("匹配 `proj` 的会话"),
        "the keyword header renders: {text}"
    );
    assert!(text.contains("本目录"), "the directory toggle renders: {text}");
    assert_eq!(
        switch_pager_label(&card).as_deref(),
        Some("第 1/2 页 · 共 8 个"),
        "the toggle landed on page 1: {text}"
    );
    assert!(
        card.to_string().contains("\"default_value\":\"proj\""),
        "the keyword survives the toggle: {card}"
    );
}

/// ADR-0052: 下一页 rebuilds the next window and keeps the keyword in the
/// search box.
#[tokio::test]
async fn switch_card_page_flip_shows_the_next_window_and_keeps_the_keyword() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let (app, _platform) = build_app(cfg, backend_with_sessions(8)).await;

    let flip = serde_json::json!({
        "action": "switch",
        "op": "page",
        "chat_id": "chat_1",
        "thread_id": "chat_1",
        "keyword": "proj",
        "scope": "all",
        "page": 2,
    });
    let card = app
        .host_action(flip)
        .await
        .expect("switch page should return a result")
        .card
        .expect("a page flip refreshes the card");
    let text = card_text(&card);
    assert!(
        text.contains("项目2 ·") && text.contains("项目1 ·"),
        "page 2 shows the seventh and eighth sessions: {text}"
    );
    assert!(
        !text.contains("项目8 ·") && !text.contains("项目3 ·"),
        "page 1's rows are off page 2: {text}"
    );
    assert_eq!(
        switch_pager_label(&card).as_deref(),
        Some("第 2/2 页 · 共 8 个"),
        "the indicator reports the flipped page: {text}"
    );
    assert_eq!(
        pager_button(&card, "下一页")["disabled"],
        true,
        "the last page disables 下一页"
    );
    assert_eq!(
        pager_button(&card, "上一页")["disabled"],
        false,
        "the last page keeps 上一页 live"
    );
    assert!(
        card.to_string().contains("\"default_value\":\"proj\""),
        "the search box echoes the current keyword: {card}"
    );
}

/// ADR-0052: an out-of-range page (data shrank under the user) is clamped to
/// the LAST page, not sprung back to the first.
#[tokio::test]
async fn switch_card_out_of_range_page_clamps_to_the_last_page() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let (app, _platform) = build_app(cfg, backend_with_sessions(8)).await;

    let flip = serde_json::json!({
        "action": "switch",
        "op": "page",
        "chat_id": "chat_1",
        "thread_id": "chat_1",
        "keyword": "",
        "scope": "all",
        "page": 99,
    });
    let card = app
        .host_action(flip)
        .await
        .expect("switch page should return a result")
        .card
        .expect("a page flip refreshes the card");
    let text = card_text(&card);
    assert!(
        text.contains("项目2 ·") && text.contains("项目1 ·"),
        "clamped to the last page's window: {text}"
    );
    assert!(!text.contains("项目8"), "not back on page 1: {text}");
    assert_eq!(
        switch_pager_label(&card).as_deref(),
        Some("第 2/2 页 · 共 8 个"),
        "the clamped page is reported: {text}"
    );
}

/// ADR-0052: the ＋新建 footer does not reset the filter — the refreshed list
/// stays on the same keyword/scope/page.
#[tokio::test]
async fn switch_card_new_preserves_the_filter() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let (app, _platform) = build_app(cfg, backend_with_sessions(8)).await;

    let value = serde_json::json!({
        "action": "switch",
        "op": "new",
        "chat_id": "chat_1",
        "thread_id": "chat_1",
        "keyword": "proj",
        "scope": "all",
        "page": 2,
    });
    let result = app
        .host_action(value)
        .await
        .expect("switch new should return a result");
    assert_eq!(
        result.toast.as_deref(),
        Some("下一条消息创建会话"),
        "new still declares the pending"
    );
    let card = result.card.expect("new returns a refreshed card");
    let text = card_text(&card);
    assert!(
        text.contains("项目2 ·") && text.contains("项目1 ·"),
        "the same page two window is rebuilt: {text}"
    );
    assert!(
        !text.contains("项目8 ·"),
        "the refresh does not fall back to page 1: {text}"
    );
    assert_eq!(
        switch_pager_label(&card).as_deref(),
        Some("第 2/2 页 · 共 8 个"),
        "the page is preserved: {text}"
    );
    assert!(
        card.to_string().contains("\"default_value\":\"proj\""),
        "the keyword is echoed into the search box: {card}"
    );
}

/// ADR-0052: 返回列表 (the force-confirm card's back button) rebuilds the list
/// at the same keyword/scope/page.
#[tokio::test]
async fn switch_card_back_preserves_the_filter() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let (app, _platform) = build_app(cfg, backend_with_sessions(8)).await;

    let value = serde_json::json!({
        "action": "switch",
        "op": "back",
        "chat_id": "chat_1",
        "thread_id": "chat_1",
        "keyword": "proj",
        "scope": "all",
        "page": 2,
    });
    let card = app
        .host_action(value)
        .await
        .expect("back should return a result")
        .card
        .expect("back rebuilds the list card");
    let text = card_text(&card);
    assert!(
        text.contains("项目2 ·") && text.contains("项目1 ·"),
        "the same page two window is rebuilt: {text}"
    );
    assert_eq!(
        switch_pager_label(&card).as_deref(),
        Some("第 2/2 页 · 共 8 个"),
        "the page is preserved: {text}"
    );
    assert!(
        card.to_string().contains("\"default_value\":\"proj\""),
        "the keyword is echoed into the search box: {card}"
    );
}

/// ADR-0052: the force-confirm card carries the filter on both its buttons,
/// and 返回列表 lands back on the same window.
#[tokio::test]
async fn switch_card_force_confirm_round_trip_preserves_the_filter() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let backend = backend_with_sessions(8);
    let mut platform = RecordingPlatform::new();
    platform
        .chat_names
        .insert("oc_group_other".into(), "隔壁群".into());
    let platform = Arc::new(platform);
    let app = Arc::new(App::new(cfg, Arc::new(backend), platform.clone()).unwrap());
    // ses_p1 sits on page 2 and is owned by another chat.
    seed_entry(
        &app,
        crate::config::SessionEntry::new(
            crate::config::ThreadKey::new("oc_group_other".into(), "oc_group_other".into()),
            "ses_p1",
            "/work/proj1",
        ),
    )
    .await;

    let adopt = serde_json::json!({
        "action": "switch",
        "op": "adopt",
        "chat_id": "chat_1",
        "thread_id": "chat_1",
        "session_id": "ses_p1",
        "keyword": "proj",
        "scope": "all",
        "page": 2,
        "open_message_id": "om_switch_card",
    });
    let confirm = app
        .host_action(adopt)
        .await
        .expect("occupied adopt returns the confirm card")
        .card
        .expect("occupied adopt returns the confirm card");
    assert!(
        card_text(&confirm).contains("强制接管"),
        "force button: {confirm}"
    );
    for btn in card_buttons(&confirm) {
        assert_eq!(btn["value"]["keyword"], "proj", "button keeps the keyword: {btn}");
        assert_eq!(btn["value"]["scope"], "all", "button keeps the scope: {btn}");
        assert_eq!(btn["value"]["page"], 2, "button keeps the page: {btn}");
    }

    let back = click_button_card(&app, &confirm, "back").await;
    let text = card_text(&back);
    assert!(
        text.contains("项目2 ·") && text.contains("项目1 ·"),
        "返回列表 rebuilds the same page two window: {text}"
    );
    assert_eq!(
        switch_pager_label(&back).as_deref(),
        Some("第 2/2 页 · 共 8 个"),
        "the page survives the confirm round trip: {text}"
    );
    assert!(
        back.to_string().contains("\"default_value\":\"proj\""),
        "the keyword survives the confirm round trip: {back}"
    );
}

/// ADR-0052: 建话题接管 from a filtered, paged card rebuilds the same window
/// and keyword (it does not reset to page 1).
#[tokio::test]
async fn switch_card_topic_adopt_preserves_the_filter() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let (app, _platform) = build_app(cfg, backend_with_sessions(8)).await;

    let value = serde_json::json!({
        "action": "switch",
        "op": "topic_adopt",
        "chat_id": "chat_1",
        "thread_id": "chat_1",
        "session_id": "ses_p1",
        "keyword": "proj",
        "scope": "all",
        "page": 2,
        "open_message_id": "om_switch_card",
    });
    let result = app
        .host_action(value)
        .await
        .expect("topic_adopt should return a result");
    assert!(
        result.toast.clone().unwrap_or_default().contains("已建话题接管"),
        "建话题接管 still opens the topic: {:?}",
        result.toast
    );
    let card = result.card.expect("topic_adopt refreshes the card");
    let text = card_text(&card);
    assert!(
        text.contains("项目2 ·") && text.contains("项目1 ·"),
        "the same page two window is rebuilt: {text}"
    );
    assert!(
        !text.contains("项目8 ·"),
        "the refresh does not fall back to page 1: {text}"
    );
    assert_eq!(
        switch_pager_label(&card).as_deref(),
        Some("第 2/2 页 · 共 8 个"),
        "the page is preserved: {text}"
    );
    assert!(
        card.to_string().contains("\"default_value\":\"proj\""),
        "the keyword is echoed into the search box: {card}"
    );
}

/// ADR-0052: 强制建话题接管 (the force-confirm card's danger button) also
/// lands back on the same filter and page.
#[tokio::test]
async fn switch_card_force_topic_adopt_preserves_the_filter() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let backend = backend_with_sessions(8);
    let mut platform = RecordingPlatform::new();
    platform
        .chat_names
        .insert("oc_group_other".into(), "隔壁群".into());
    let platform = Arc::new(platform);
    let app = Arc::new(App::new(cfg, Arc::new(backend), platform.clone()).unwrap());
    seed_entry(
        &app,
        crate::config::SessionEntry::new(
            crate::config::ThreadKey::new("oc_group_other".into(), "oc_group_other".into()),
            "ses_p1",
            "/work/proj1",
        ),
    )
    .await;

    let value = serde_json::json!({
        "action": "switch",
        "op": "force_topic_adopt",
        "chat_id": "chat_1",
        "thread_id": "chat_1",
        "session_id": "ses_p1",
        "keyword": "proj",
        "scope": "all",
        "page": 2,
        "open_message_id": "om_switch_card",
    });
    let card = app
        .host_action(value)
        .await
        .expect("force_topic_adopt should return a result")
        .card
        .expect("force_topic_adopt refreshes the card");
    let text = card_text(&card);
    assert!(
        text.contains("项目2 ·") && text.contains("项目1 ·"),
        "the same page two window is rebuilt: {text}"
    );
    assert_eq!(
        switch_pager_label(&card).as_deref(),
        Some("第 2/2 页 · 共 8 个"),
        "the page is preserved: {text}"
    );
    assert!(
        card.to_string().contains("\"default_value\":\"proj\""),
        "the keyword is echoed into the search box: {card}"
    );
}

/// ADR-0052: adopting from a filtered, paged `/switch` list patches the card
/// to the Session Snapshot (ADR-0028), which carries a 「返回列表」 button
/// holding that list's keyword/scope/page; clicking it rebuilds the same
/// filtered window on the same page.
#[tokio::test]
async fn switch_card_adopt_round_trips_back_to_the_filtered_page() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let (app, platform) = build_app(cfg, backend_with_sessions(8)).await;

    send_command(&app, "/switch", "msg_switch").await;
    let page1 = platform
        .replied_cards()
        .await
        .into_iter()
        .next()
        .expect("the /switch command replies with a card");

    // Type `proj` into the search box: the rebuilt card is filtered at page 1.
    let mut search = card_buttons(&page1)
        .into_iter()
        .find(|b| b["value"]["op"] == "search")
        .expect("the switch card has a search submit button")["value"]
        .clone();
    search["keyword"] = serde_json::json!("proj");
    let filtered = app
        .host_action(search)
        .await
        .expect("the search returns a result")
        .card
        .expect("the search rebuilds the card");

    // Flip to page 2; ses_p1's row lives there.
    let flip = pager_button(&filtered, "下一页")["value"].clone();
    let page2 = app
        .host_action(flip)
        .await
        .expect("the page flip returns a result")
        .card
        .expect("the page flip rebuilds the card");
    assert!(
        card_text(&page2).contains("项目1 ·"),
        "ses_p1's row is on page 2: {}",
        card_text(&page2)
    );

    // Click 接管 on ses_p1's row: the list card becomes the snapshot.
    let adopt = card_buttons(&page2)
        .into_iter()
        .find(|b| b["value"]["op"] == "adopt" && b["value"]["session_id"] == "ses_p1")
        .expect("page 2 lists ses_p1's adopt button")["value"]
        .clone();
    let snapshot = app
        .host_action(adopt)
        .await
        .expect("adopt returns a result")
        .card
        .expect("adopt patches the card in place");
    let text = card_text(&snapshot);
    assert!(text.contains("已接管 项目1"), "snapshot header: {text}");
    assert!(!text.contains("会话管理"), "the list is gone: {text}");

    // The snapshot carries the list's exact return target.
    let back = card_buttons(&snapshot)
        .into_iter()
        .find(|b| b["value"]["op"] == "back")
        .expect("the list adoption carries a 返回列表 button");
    assert_eq!(back["text"]["content"], "返回列表");
    assert_eq!(back["value"]["keyword"], "proj");
    assert_eq!(back["value"]["scope"], "all");
    assert_eq!(back["value"]["page"], 2);

    let list = click_button_card(&app, &snapshot, "back").await;
    let text = card_text(&list);
    assert!(
        text.contains("项目2 ·") && text.contains("项目1 ·"),
        "返回列表 rebuilds the same page two window: {text}"
    );
    assert!(
        !text.contains("项目8 ·"),
        "the return does not fall back to page 1: {text}"
    );
    assert_eq!(
        switch_pager_label(&list).as_deref(),
        Some("第 2/2 页 · 共 8 个"),
        "the page survives the adopt round trip: {text}"
    );
    assert!(
        list.to_string().contains("\"default_value\":\"proj\""),
        "the keyword survives the adopt round trip: {list}"
    );
}

/// ADR-0052: the suppressed compact 已切换 state card (a re-switch with
/// nothing to report, ADR-0028) carries the same 「返回列表」 button, and it
/// returns to the same filtered page.
#[tokio::test]
async fn switch_card_suppressed_round_trips_back_to_the_filtered_page() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let mut backend = backend_with_sessions(8);
    backend.cola_message("ses_p1", "上次的问题");
    let (app, platform) = build_app(cfg, backend).await;
    let key = crate::config::ThreadKey::new("chat_1".into(), "chat_1".into());
    // ses_p1 is mapped to this thread but NOT active: its row shows 切换, and
    // the re-activation is eligible for suppression (idle, no pending,
    // newest user message cola-authored).
    seed_entry(
        &app,
        crate::config::SessionEntry::new(key.clone(), "ses_p1", "/work/proj1"),
    )
    .await;
    seed_entry(
        &app,
        crate::config::SessionEntry::new(key, "ses_active", "/work/active"),
    )
    .await;

    send_command(&app, "/switch", "msg_switch").await;
    let page1 = platform
        .replied_cards()
        .await
        .into_iter()
        .next()
        .expect("the /switch command replies with a card");
    // The seeded active session roots the card in /work/active, where no
    // listed session lives: widen to the whole store before searching.
    let toggle = card_buttons(&page1)
        .into_iter()
        .find(|b| b["value"]["op"] == "scope" && b["value"]["scope"] == "all")
        .expect("the directory-scoped card offers 全部")["value"]
        .clone();
    let widened = app
        .host_action(toggle)
        .await
        .expect("the scope toggle returns a result")
        .card
        .expect("the scope toggle rebuilds the card");
    let mut search = card_buttons(&widened)
        .into_iter()
        .find(|b| b["value"]["op"] == "search")
        .expect("the switch card has a search submit button")["value"]
        .clone();
    search["keyword"] = serde_json::json!("proj");
    let filtered = app
        .host_action(search)
        .await
        .expect("the search returns a result")
        .card
        .expect("the search rebuilds the card");
    let flip = pager_button(&filtered, "下一页")["value"].clone();
    let page2 = app
        .host_action(flip)
        .await
        .expect("the page flip returns a result")
        .card
        .expect("the page flip rebuilds the card");

    let adopt = card_buttons(&page2)
        .into_iter()
        .find(|b| b["value"]["op"] == "adopt" && b["value"]["session_id"] == "ses_p1")
        .expect("page 2 lists ses_p1's adopt button")["value"]
        .clone();
    let snapshot = app
        .host_action(adopt)
        .await
        .expect("the re-switch returns a result")
        .card
        .expect("the re-switch patches the card in place");
    let text = card_text(&snapshot);
    assert!(
        text.contains("已切换 项目1"),
        "compact suppressed state header: {text}"
    );
    assert!(
        !text.contains("最近对话"),
        "the suppressed card carries no tail: {text}"
    );

    let back = card_buttons(&snapshot)
        .into_iter()
        .find(|b| b["value"]["op"] == "back")
        .expect("the suppressed state card carries a 返回列表 button");
    assert_eq!(back["value"]["keyword"], "proj");
    assert_eq!(back["value"]["scope"], "all");
    assert_eq!(back["value"]["page"], 2);

    let list = click_button_card(&app, &snapshot, "back").await;
    let text = card_text(&list);
    assert!(
        text.contains("项目2 ·") && text.contains("项目1 ·"),
        "返回列表 rebuilds the same page two window: {text}"
    );
    assert_eq!(
        switch_pager_label(&list).as_deref(),
        Some("第 2/2 页 · 共 8 个"),
        "the page survives the suppressed round trip: {text}"
    );
    assert!(
        list.to_string().contains("\"default_value\":\"proj\""),
        "the keyword survives the suppressed round trip: {list}"
    );
}

/// ADR-0052: a snapshot from a non-list source (the text `/switch <kw>` form,
/// the old `/attach`) carries no 「返回列表」 button — there is no filtered
/// list to return to.
#[tokio::test]
async fn text_switch_snapshot_has_no_back_to_list_button() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let mut backend = MockBackend::new(realistic_parts());
    backend.given_sessions(vec![list_session("ses_alpha01", "重写登录", "/work/auth", 100)]);
    let (app, platform) = build_app(cfg, backend).await;

    send_command(&app, "/switch 重写登录", "msg_switch").await;

    let card = platform
        .replied_cards()
        .await
        .into_iter()
        .next()
        .expect("the text /switch replies with its snapshot card");
    let text = card_text(&card);
    assert!(text.contains("已接管 重写登录"), "snapshot header: {text}");
    assert!(
        !text.contains("返回列表"),
        "a text adoption has no list to return to: {text}"
    );
}

/// `n` sessions 项目N rooted in `/work/projN`, most recently active last (so
/// `switch_card_data` sorts them descending: projN first).
fn backend_with_sessions(n: i64) -> MockBackend {
    let mut backend = MockBackend::new(realistic_parts());
    backend.given_sessions(
        (1..=n)
            .map(|i| {
                list_session(
                    &format!("ses_p{i}"),
                    &format!("项目{i}"),
                    &format!("/work/proj{i}"),
                    i * 100,
                )
            })
            .collect(),
    );
    backend
}

/// The pager's indicator text (`第 x/y 页 · 共 N 个`) on a card.
fn switch_pager_label(card: &serde_json::Value) -> Option<String> {
    card_texts(card)
        .into_iter()
        .find(|t| t.starts_with("第 ") && t.contains(" 页 · 共 "))
}

/// The pager button labelled `label` (上一页 / 下一页).
fn pager_button<'a>(card: &'a serde_json::Value, label: &str) -> &'a serde_json::Value {
    card_buttons(card)
        .into_iter()
        .find(|b| b["value"]["op"] == "page" && b["text"]["content"] == label)
        .unwrap_or_else(|| panic!("pager button `{label}` not found"))
}

/// Click the button whose `value.op` is `op` on `card` as the Host and return
/// the refreshed card it produced.
async fn click_button_card(app: &Arc<App>, card: &serde_json::Value, op: &str) -> serde_json::Value {
    let value = card_buttons(card)
        .into_iter()
        .find(|b| b["value"]["op"] == op)
        .unwrap_or_else(|| panic!("button `{op}` not found"))["value"]
        .clone();
    app.host_action(value)
        .await
        .unwrap_or_else(|| panic!("button `{op}` returned no result"))
        .card
        .unwrap_or_else(|| panic!("button `{op}` returned no card"))
}
