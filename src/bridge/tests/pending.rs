use crate::bridge::session::PendingEntry;
use crate::bridge::test_support::*;

fn key() -> crate::config::ThreadKey {
    crate::config::ThreadKey::new("chat_1".into(), "chat_1".into())
}

/// ADR-0041: `current_project_directory` reads the Pending Session first, even
/// though `get_active` is `None` for the thread while the pending exists.
#[tokio::test]
async fn current_project_directory_reads_pending_first() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let mut cfg = test_config(&dir.path().join("sessions.json"));
    cfg.bridge.work_dir = Some(dir.path().join("work"));
    let (app, _platform) = build_app(cfg, MockBackend::new(realistic_parts())).await;

    seed_entry(
        &app,
        crate::config::SessionEntry::new(key(), "ses_old", "/work/a"),
    )
    .await;
    seed_pending(&app, PendingEntry::new(key(), "/work/pending")).await;

    assert_eq!(app.core.current_project_directory(&key()).await, "/work/pending");
}

/// The `/dir` card's 当前 directory follows the pending, not the superseded
/// active session.
#[tokio::test]
async fn dir_card_current_reads_pending() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let mut backend = MockBackend::new(realistic_parts());
    backend.session_list = vec![list_session("ses_a", "A", "/work/a", 100)];
    let (app, _platform) = build_app(cfg, backend).await;

    seed_pending(&app, PendingEntry::new(key(), "/work/pending")).await;

    let (dirs, current) = crate::bridge::command::dir_card_data(&app.core, &key()).await;
    assert_eq!(dirs, vec!["/work/a".to_string()]);
    assert_eq!(current.as_deref(), Some("/work/pending"));
}

/// `/dir <path>` declares a Pending Session in the given directory instead of
/// creating a server session; an older eagerly-created active session is
/// superseded (mapped, switchable, not deleted), exactly like `/new`.
#[tokio::test]
async fn dir_declares_a_pending_and_supersedes_the_active_session() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let backend = MockBackend::new(realistic_parts());
    let created = backend.created_session_dirs.clone();
    let (app, platform) = build_app(cfg, backend).await;
    let proj = tempfile::tempdir().unwrap();
    let proj_dir = std::fs::canonicalize(proj.path())
        .unwrap()
        .to_string_lossy()
        .to_string();
    seed_entry(
        &app,
        crate::config::SessionEntry::new(key(), "ses_old", "/work/a"),
    )
    .await;

    crate::bridge::command::handle_command(
        &app.core,
        crate::bridge::command::Command::Dir(proj_dir.clone()),
        key(),
        "msg_dir",
        crate::config::ConversationKind::P2p,
    )
    .await
    .unwrap();

    assert!(created.lock().await.is_empty(), "/dir creates no server session");
    {
        let store = app.sessions.lock().await;
        assert_eq!(
            store.pending_for(&key()).map(|p| p.directory.as_str()),
            Some(proj_dir.as_str()),
            "the pending carries the picked directory"
        );
        assert!(
            store.get_active(&key()).is_none(),
            "the pending supersedes the old session"
        );
        assert_eq!(
            store.list_thread(&key()).len(),
            1,
            "the superseded session stays mapped"
        );
    }
    let texts = platform.texts().await;
    assert!(
        texts
            .iter()
            .any(|t| t.contains("下一条消息") && t.contains(&proj_dir)),
        "the reply states the creation timing and directory: {texts:?}"
    );

    app.handle_message(incoming(
        "msg_1".into(),
        "chat_1".into(),
        "p2p".into(),
        None,
        "hi".into(),
        None,
    ))
    .await;
    let entry = app
        .sessions
        .lock()
        .await
        .get_active(&key())
        .cloned()
        .expect("the first message materialises the /dir pending");
    assert_eq!(entry.directory, proj_dir);
    assert_eq!(
        *created.lock().await,
        vec![Some(proj_dir.clone())],
        "exactly one session, created in the picked directory"
    );
}

/// Picking the pending's own directory is a no-op toast, not a second pending:
/// the pending (title included) is left exactly as declared.
#[tokio::test]
async fn dir_card_pick_pendings_own_directory_is_a_noop() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let backend = MockBackend::new(realistic_parts());
    let (app, _platform) = build_app(cfg, backend).await;
    let mut pending = PendingEntry::new(key(), "/work/pending");
    pending.title = Some("keep-me".into());
    seed_pending(&app, pending).await;

    let value = serde_json::json!({
        "action": "dir",
        "op": "pick",
        "chat_id": "chat_1",
        "thread_id": "chat_1",
        "directory": "/work/pending",
    });
    let result = app
        .host_action(value)
        .await
        .expect("dir pick should return a result");
    assert!(result.card.is_some(), "card still refreshes");
    assert_eq!(
        result.toast.as_deref(),
        Some("已在当前目录"),
        "pending's own directory pick toasts only: {:?}",
        result.toast
    );
    assert_eq!(
        app.sessions
            .lock()
            .await
            .pending_for(&key())
            .and_then(|p| p.title.clone()),
        Some("keep-me".to_string()),
        "the pending was not replaced"
    );
}

/// Correcting a wrong `/dir` before the first prompt replaces the pending —
/// including one declared by `/new` — so only the last directory materialises;
/// nothing was ever created at the abandoned one.
#[tokio::test]
async fn second_dir_before_the_first_prompt_replaces_the_pending() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let backend = MockBackend::new(realistic_parts());
    let created = backend.created_session_dirs.clone();
    let (app, _platform) = build_app(cfg, backend).await;
    let first = tempfile::tempdir().unwrap();
    let second = tempfile::tempdir().unwrap();
    let first_dir = std::fs::canonicalize(first.path())
        .unwrap()
        .to_string_lossy()
        .to_string();
    let second_dir = std::fs::canonicalize(second.path())
        .unwrap()
        .to_string_lossy()
        .to_string();

    for (msg, cmd) in [
        (
            "msg_new",
            crate::bridge::command::Command::New(Some("wrong".into())),
        ),
        ("msg_dir", crate::bridge::command::Command::Dir(first_dir.clone())),
        (
            "msg_dir2",
            crate::bridge::command::Command::Dir(second_dir.clone()),
        ),
    ] {
        crate::bridge::command::handle_command(
            &app.core,
            cmd,
            key(),
            msg,
            crate::config::ConversationKind::P2p,
        )
        .await
        .unwrap();
    }

    assert!(created.lock().await.is_empty(), "corrections create nothing");
    {
        let store = app.sessions.lock().await;
        let pending = store.pending_for(&key()).expect("one pending remains");
        assert_eq!(pending.directory, second_dir);
        assert_eq!(pending.title, None, "a replaced pending drops the old title");
    }

    app.handle_message(incoming(
        "msg_1".into(),
        "chat_1".into(),
        "p2p".into(),
        None,
        "hi".into(),
        None,
    ))
    .await;
    let entry = app
        .sessions
        .lock()
        .await
        .get_active(&key())
        .cloned()
        .expect("the last pending materialises");
    assert_eq!(entry.directory, second_dir);
    assert_eq!(
        *created.lock().await,
        vec![Some(second_dir.clone())],
        "only the corrected directory is ever used"
    );
}

/// The `/switch` card's current-directory scope follows the pending, and no
/// row is marked active — the pending is not a Session (ADR-0041); the
/// superseded session stays mapped and switchable.
#[tokio::test]
async fn switch_card_current_reads_pending_and_marks_no_active_row() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let mut backend = MockBackend::new(realistic_parts());
    backend.session_list = vec![
        list_session("ses_a", "A", "/work/a", 100),
        list_session("ses_p", "P", "/work/pending", 200),
    ];
    let (app, _platform) = build_app(cfg, backend).await;
    seed_entry(
        &app,
        crate::config::SessionEntry::new(key(), "ses_old", "/work/a"),
    )
    .await;
    seed_pending(&app, PendingEntry::new(key(), "/work/pending")).await;

    let (shown, active_id, mapped_ids, scope, current_dir) = crate::bridge::command::switch_card_data(
        &app.core,
        &key(),
        "",
        crate::bridge::command::SwitchScope::All,
    )
    .await;

    assert_eq!(shown.len(), 2);
    assert_eq!(current_dir.as_deref(), Some("/work/pending"));
    assert!(active_id.is_none(), "a pending means no active session");
    assert_eq!(mapped_ids, vec!["ses_old".to_string()]);
    assert_eq!(scope, crate::bridge::command::SwitchScope::All);
}

/// `/new` declares a Pending Session instead of creating one; the first
/// non-command message materialises it in the pending's directory — once.
#[tokio::test]
async fn new_defers_creation_to_the_first_message() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let backend = MockBackend::new(realistic_parts());
    let created = backend.created_session_dirs.clone();
    let (app, platform) = build_app(cfg, backend).await;
    let proj = tempfile::tempdir().unwrap();
    let proj_dir = std::fs::canonicalize(proj.path())
        .unwrap()
        .to_string_lossy()
        .to_string();
    seed_entry(
        &app,
        crate::config::SessionEntry::new(key(), "ses_old", proj_dir.clone()),
    )
    .await;

    crate::bridge::command::handle_command(
        &app.core,
        crate::bridge::command::Command::New(None),
        key(),
        "msg_new",
        crate::config::ConversationKind::P2p,
    )
    .await
    .unwrap();

    assert!(created.lock().await.is_empty(), "/new creates no server session");
    {
        let store = app.sessions.lock().await;
        assert_eq!(
            store.pending_for(&key()).map(|p| p.directory.as_str()),
            Some(proj_dir.as_str()),
            "the pending carries the current project directory"
        );
        assert!(
            store.get_active(&key()).is_none(),
            "the pending supersedes the old session"
        );
    }
    let texts = platform.texts().await;
    assert!(
        texts.iter().any(|t| t.contains("下一条消息将创建会话")),
        "the reply states the creation timing: {texts:?}"
    );

    app.handle_message(incoming(
        "msg_1".into(),
        "chat_1".into(),
        "p2p".into(),
        None,
        "hi".into(),
        None,
    ))
    .await;

    {
        let store = app.sessions.lock().await;
        assert!(
            store.pending_for(&key()).is_none(),
            "the first message materialises the pending"
        );
        let entry = store
            .get_active(&key())
            .cloned()
            .expect("the materialised session is active");
        assert_eq!(entry.directory, proj_dir, "created in the pending's directory");
    }
    assert_eq!(
        created.lock().await.len(),
        1,
        "exactly one server session created"
    );

    // A second message reuses the materialised session.
    app.handle_message(incoming(
        "msg_2".into(),
        "chat_1".into(),
        "p2p".into(),
        None,
        "again".into(),
        None,
    ))
    .await;
    assert_eq!(
        created.lock().await.len(),
        1,
        "a second message does not create a duplicate"
    );
}

/// `/new <name>` applies the name as the created session's server title at
/// materialisation (creation title policy, ADR-0007) — and `created` is not
/// flagged as an auto-create, so the group-lobby guidance stays silent.
#[tokio::test]
async fn new_name_becomes_the_session_title_at_materialisation() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let backend = MockBackend::new(realistic_parts());
    let titles = backend.update_title_calls.clone();
    let (app, _platform) = build_app(cfg, backend).await;

    crate::bridge::command::handle_command(
        &app.core,
        crate::bridge::command::Command::New(Some("api-refactor".into())),
        key(),
        "msg_new",
        crate::config::ConversationKind::P2p,
    )
    .await
    .unwrap();
    assert!(
        titles.lock().await.is_empty(),
        "no title PATCH before materialisation"
    );

    app.handle_message(incoming(
        "msg_1".into(),
        "chat_1".into(),
        "p2p".into(),
        None,
        "hi".into(),
        None,
    ))
    .await;

    assert_eq!(
        *titles.lock().await,
        vec![("ses_test".to_string(), "api-refactor".to_string())],
        "the pending title is PATCHed onto the created session"
    );
    assert!(app.sessions.lock().await.get_active(&key()).is_some());
}

/// A command as the first message does not materialise; a later real message
/// still does.
#[tokio::test]
async fn command_first_message_does_not_materialise() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let backend = MockBackend::new(realistic_parts());
    let created = backend.created_session_dirs.clone();
    let (app, _platform) = build_app(cfg, backend).await;

    crate::bridge::command::handle_command(
        &app.core,
        crate::bridge::command::Command::New(None),
        key(),
        "msg_new",
        crate::config::ConversationKind::P2p,
    )
    .await
    .unwrap();

    app.handle_message(incoming(
        "msg_help".into(),
        "chat_1".into(),
        "p2p".into(),
        None,
        "/help".into(),
        None,
    ))
    .await;
    assert!(
        created.lock().await.is_empty(),
        "a command does not materialise the pending"
    );
    assert!(
        app.sessions.lock().await.pending_for(&key()).is_some(),
        "the pending survives the command"
    );

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
        app.sessions.lock().await.pending_for(&key()).is_none(),
        "a later real message materialises"
    );
    assert_eq!(created.lock().await.len(), 1);
}

/// A failed create at materialisation keeps the pending: the error surfaces on
/// that message and the next message retries successfully.
#[tokio::test]
async fn failed_materialisation_keeps_the_pending_and_retries() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let mut backend = MockBackend::new(realistic_parts());
    backend.fail_create_session_count = std::sync::atomic::AtomicUsize::new(1).into();
    let created = backend.created_session_dirs.clone();
    let (app, platform) = build_app(cfg, backend).await;

    crate::bridge::command::handle_command(
        &app.core,
        crate::bridge::command::Command::New(None),
        key(),
        "msg_new",
        crate::config::ConversationKind::P2p,
    )
    .await
    .unwrap();

    app.handle_message(incoming(
        "msg_1".into(),
        "chat_1".into(),
        "p2p".into(),
        None,
        "hi".into(),
        None,
    ))
    .await;
    let texts = platform.texts().await;
    assert!(
        texts.iter().any(|t| t.contains("创建会话失败")),
        "the create error surfaces on that message: {texts:?}"
    );
    assert!(
        app.sessions.lock().await.pending_for(&key()).is_some(),
        "the pending is kept for the retry"
    );
    assert!(app.sessions.lock().await.get_active(&key()).is_none());

    app.handle_message(incoming(
        "msg_2".into(),
        "chat_1".into(),
        "p2p".into(),
        None,
        "hi again".into(),
        None,
    ))
    .await;
    assert!(
        app.sessions.lock().await.pending_for(&key()).is_none(),
        "the retry materialises"
    );
    assert!(app.sessions.lock().await.get_active(&key()).is_some());
    assert_eq!(
        created.lock().await.len(),
        2,
        "one failed attempt + one successful retry"
    );
}

/// The group-lobby guidance belongs to the silent auto-create only: a session
/// that came from an explicit `/new` already told the user what happened.
#[tokio::test]
async fn group_lobby_guidance_skipped_for_explicit_new() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let (app, platform) = build_app(cfg, MockBackend::new(realistic_parts())).await;
    let group_key = crate::config::ThreadKey::new("oc_group_1".into(), "oc_group_1".into());

    app.handle_message(incoming(
        "msg_new".into(),
        "oc_group_1".into(),
        "group".into(),
        None,
        "/new".into(),
        None,
    ))
    .await;
    app.handle_message(incoming(
        "msg_1".into(),
        "oc_group_1".into(),
        "group".into(),
        None,
        "hi".into(),
        None,
    ))
    .await;

    assert!(
        app.sessions.lock().await.get_active(&group_key).is_some(),
        "the explicit /new materialises on the first message"
    );
    let texts = platform.texts().await;
    assert!(
        !texts.iter().any(|t| t.contains("已创建群会话")),
        "no lobby guidance for an explicit /new: {texts:?}"
    );
}

/// `/switch <id>` back to the superseded session replaces (clears) the
/// pending, exactly like the other selection commands (ADR-0041).
#[tokio::test]
async fn switch_back_after_new_clears_the_pending() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let mut backend = MockBackend::new(realistic_parts());
    backend.session_list = vec![list_session("ses_old", "旧会话", "/work/proj", 100)];
    let (app, _platform) = build_app(cfg, backend).await;
    seed_entry(
        &app,
        crate::config::SessionEntry::new(key(), "ses_old", "/work/proj"),
    )
    .await;

    crate::bridge::command::handle_command(
        &app.core,
        crate::bridge::command::Command::New(None),
        key(),
        "msg_new",
        crate::config::ConversationKind::P2p,
    )
    .await
    .unwrap();
    assert!(app.sessions.lock().await.pending_for(&key()).is_some());

    crate::bridge::command::handle_command(
        &app.core,
        crate::bridge::command::Command::Switch(crate::bridge::command::SwitchAction::Match(
            "ses_old".into(),
        )),
        key(),
        "msg_switch",
        crate::config::ConversationKind::P2p,
    )
    .await
    .unwrap();

    let store = app.sessions.lock().await;
    assert!(
        store.pending_for(&key()).is_none(),
        "switching back replaces the pending"
    );
    assert_eq!(store.get_active(&key()).unwrap().session_id, "ses_old");
}

/// Materialisation carries every pending field onto the created SessionEntry,
/// and the first prompt already runs with them (`/agent` `/model` `/think`
/// `/autoaccept` write the pending in the command-matrix ticket).
#[tokio::test]
async fn materialisation_carries_the_pending_overrides() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let backend = MockBackend::new(realistic_parts());
    let prompt_models = backend.prompt_models.clone();
    let prompt_agents = backend.prompt_agents.clone();
    let (app, _platform) = build_app(cfg, backend).await;

    let mut pending = PendingEntry::new(key(), "/work/proj");
    pending.agent = Some("build".into());
    pending.model = Some("p/test".into());
    pending.variant = Some("high".into());
    pending.auto_accept = true;
    pending.topic_anchor = Some("om_anchor".into());
    pending.topic_root = Some("om_root".into());
    seed_pending(&app, pending).await;

    app.handle_message(incoming(
        "msg_1".into(),
        "chat_1".into(),
        "p2p".into(),
        None,
        "hi".into(),
        None,
    ))
    .await;

    let entry = app
        .sessions
        .lock()
        .await
        .get_active(&key())
        .cloned()
        .expect("materialised");
    assert_eq!(entry.agent.as_deref(), Some("build"));
    assert_eq!(entry.model.as_deref(), Some("p/test"));
    assert_eq!(entry.variant.as_deref(), Some("high"));
    assert!(entry.auto_accept);
    assert_eq!(entry.topic_anchor.as_deref(), Some("om_anchor"));
    assert_eq!(entry.topic_root.as_deref(), Some("om_root"));
    assert_eq!(
        *prompt_models.lock().await,
        vec![Some("p/test".to_string())],
        "the first prompt already runs with the pending's model"
    );
    assert_eq!(
        *prompt_agents.lock().await,
        vec![Some("build".to_string())],
        "the first prompt already runs with the pending's agent"
    );
}

/// A failed title PATCH must not orphan the created session or keep the pending
/// alive: the session materialises with the server title, and the user gets a
/// warning naming the `/name` retry.
#[tokio::test]
async fn failed_title_patch_still_materialises_and_warns() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let mut backend = MockBackend::new(realistic_parts());
    backend.fail_title_patch = true;
    let created = backend.created_session_dirs.clone();
    let (app, platform) = build_app(cfg, backend).await;

    crate::bridge::command::handle_command(
        &app.core,
        crate::bridge::command::Command::New(Some("api-refactor".into())),
        key(),
        "msg_new",
        crate::config::ConversationKind::P2p,
    )
    .await
    .unwrap();

    app.handle_message(incoming(
        "msg_1".into(),
        "chat_1".into(),
        "p2p".into(),
        None,
        "hi".into(),
        None,
    ))
    .await;

    let texts = platform.texts().await;
    assert!(
        texts.iter().any(|t| t.contains("标题") && t.contains("失败")),
        "the title failure is surfaced: {texts:?}"
    );
    assert_eq!(created.lock().await.len(), 1, "the session was still created");
    let store = app.sessions.lock().await;
    assert!(
        store.pending_for(&key()).is_none(),
        "the pending must not survive a created session"
    );
    assert!(store.get_active(&key()).is_some(), "the session is mapped");
}

/// Materialisation activates through the core wrapper, so the session-list
/// cache is dropped and `/list`/`/switch` see the just-created session without
/// waiting out the TTL.
#[tokio::test]
async fn materialisation_drops_the_session_list_cache() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let backend = MockBackend::new(realistic_parts());
    let list_calls = backend.list_sessions_calls.clone();
    let (app, _platform) = build_app(cfg, backend).await;

    app.core.cached_session_list().await.unwrap();
    let warm = list_calls.load(std::sync::atomic::Ordering::SeqCst);
    assert_eq!(warm, 1, "first read fetches");

    seed_pending(&app, PendingEntry::new(key(), "/work/proj")).await;
    app.handle_message(incoming(
        "msg_1".into(),
        "chat_1".into(),
        "p2p".into(),
        None,
        "hi".into(),
        None,
    ))
    .await;
    assert!(app.sessions.lock().await.get_active(&key()).is_some());

    app.core.cached_session_list().await.unwrap();
    assert_eq!(
        list_calls.load(std::sync::atomic::Ordering::SeqCst),
        warm + 1,
        "materialisation drops the session-list cache"
    );
}

/// `/name` on a pending sets the creation title: no server PATCH yet, and the
/// first message creates the session with that title.
#[tokio::test]
async fn name_on_a_pending_sets_the_creation_title() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let backend = MockBackend::new(realistic_parts());
    let titles = backend.update_title_calls.clone();
    let (app, platform) = build_app(cfg, backend).await;

    crate::bridge::command::handle_command(
        &app.core,
        crate::bridge::command::Command::New(None),
        key(),
        "msg_new",
        crate::config::ConversationKind::P2p,
    )
    .await
    .unwrap();
    crate::bridge::command::handle_command(
        &app.core,
        crate::bridge::command::Command::Name("api-refactor".into()),
        key(),
        "msg_name",
        crate::config::ConversationKind::P2p,
    )
    .await
    .unwrap();

    assert!(
        titles.lock().await.is_empty(),
        "nothing is PATCHed before the session exists"
    );
    assert_eq!(
        app.sessions
            .lock()
            .await
            .pending_for(&key())
            .and_then(|p| p.title.clone()),
        Some("api-refactor".to_string())
    );
    let texts = platform.texts().await;
    assert!(
        texts.iter().any(|t| t.contains("已记下标题")),
        "the reply says the title is pending: {texts:?}"
    );

    app.handle_message(incoming(
        "msg_1".into(),
        "chat_1".into(),
        "p2p".into(),
        None,
        "hi".into(),
        None,
    ))
    .await;
    assert_eq!(
        *titles.lock().await,
        vec![("ses_test".to_string(), "api-refactor".to_string())],
        "materialisation PATCHes the title /name recorded"
    );
}

/// `/name` with neither an active session nor a pending no longer lies about a
/// rename: it replies like the other commands that need a session.
#[tokio::test]
async fn name_without_a_session_replies_no_session() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let (app, platform) = build_app(cfg, MockBackend::new(realistic_parts())).await;

    crate::bridge::command::handle_command(
        &app.core,
        crate::bridge::command::Command::Name("api-refactor".into()),
        key(),
        "msg_name",
        crate::config::ConversationKind::P2p,
    )
    .await
    .unwrap();

    let texts = platform.texts().await;
    assert!(
        texts.iter().any(|t| t.contains("还没有会话")),
        "truthful no-session reply: {texts:?}"
    );
    assert!(
        !texts.iter().any(|t| t.contains("Renamed")),
        "no false rename confirmation: {texts:?}"
    );
}
