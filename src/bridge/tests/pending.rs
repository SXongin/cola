use crate::bridge::session::PendingEntry;
use crate::bridge::test_support::*;

fn key() -> crate::config::ThreadKey {
    crate::config::ThreadKey::new("chat_1".into(), "chat_1".into())
}

/// ADR-0089: a card callback's declaration is bounded. When the store lock is
/// still held at the deadline, the declaration mutates NOTHING and says so —
/// a click must not commit a pending the callback can no longer confirm.
#[tokio::test]
async fn declare_pending_until_gives_up_without_mutating_when_the_lock_is_held() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let (app, _platform) = build_app(cfg, MockBackend::new(realistic_parts())).await;
    let handle = app.sessions_handle();

    // Hold the store's lock the way a concurrent write path does. Holding it
    // is the only deterministic way to miss a deadline: `timeout_at` polls its
    // future first, so a free lock is acquired even past the deadline.
    let guard = app.sessions.lock().await;
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_millis(50);
    let declared = handle
        .declare_pending_until(&key(), "/work/b", None, deadline)
        .await;
    assert!(
        declared.is_ok_and(|pending| pending.is_none()),
        "the lock outlived the deadline, so nothing was declared"
    );
    drop(guard);
    assert!(
        app.sessions.lock().await.pending_for(&key()).is_none(),
        "a timed-out declaration must not mutate the store"
    );
}

/// The deadline is hard at the mutation: a deadline that has already passed is
/// a refusal even when the store lock is FREE — `timeout_at` polls its future
/// first, so without the check an expired click would still acquire the lock and
/// mutate, committing a change Feishu has already stopped waiting for.
#[tokio::test]
async fn declare_pending_until_refuses_an_expired_deadline() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let (app, _platform) = build_app(cfg, MockBackend::new(realistic_parts())).await;
    let handle = app.sessions_handle();

    let deadline = tokio::time::Instant::now() + std::time::Duration::from_millis(10);
    tokio::time::sleep(std::time::Duration::from_millis(30)).await;
    let declared = handle
        .declare_pending_until(&key(), "/work/b", None, deadline)
        .await;
    assert!(
        declared.is_ok_and(|pending| pending.is_none()),
        "an expired deadline declares nothing, free lock or not"
    );
    assert!(
        app.sessions.lock().await.pending_for(&key()).is_none(),
        "a stale click must not mutate the store"
    );
}

/// ADR-0089: a card callback stays inside the ack budget when the store's
/// writer stalls. The click still answers (the mutation is in memory), the file
/// catches up once the writer is released, and the user is never told a pending
/// exists that memory does not have.
#[tokio::test]
async fn a_stalled_writer_does_not_stall_the_dir_pick_callback() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let sessions = dir.path().join("sessions.json");
    let cfg = test_config(&sessions);
    let (app, _platform) = build_app(cfg, MockBackend::new(realistic_parts())).await;
    let pause = app
        .sessions
        .lock()
        .await
        .pause_writes()
        .expect("the app's store has a writer thread");
    // Hold the writer BEFORE the click, so the click's write is in flight for
    // the whole callback.
    pause.hold();

    let started = std::time::Instant::now();
    let result = app
        .host_action(serde_json::json!({
            "action": "dir",
            "op": "pick",
            "chat_id": "chat_1",
            "thread_id": "chat_1",
            "directory": "/work/b",
        }))
        .await
        .expect("the click still answers");
    let elapsed = started.elapsed();

    assert!(
        elapsed < crate::feishu::ws::CARD_ACK_BUDGET,
        "the callback answered inside the ack budget with the write stalled: {elapsed:?}"
    );
    let toast = result.toast.clone().unwrap_or_default();
    assert!(
        toast.contains("/work/b"),
        "the pending took effect, so the toast says so: {toast:?}"
    );
    assert!(
        app.sessions
            .lock()
            .await
            .pending_for(&key())
            .is_some_and(|pending| pending.directory == "/work/b"),
        "the mutation is in memory before the file"
    );
    assert!(!sessions.exists(), "the stalled write has not landed yet");

    // Releasing the writer lands it, in order, after the callback answered.
    pause.release();
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        if std::fs::read_to_string(&sessions).is_ok_and(|text| text.contains("/work/b")) {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the deferred write landed after the callback"
        );
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
}

/// ADR-0089's documented price: a write still in flight at the deadline
/// forfeits its ERROR, never the write — and never the mutation, which is
/// already in memory. Here the held write then fails on disk; the click has
/// already answered "declared", so the failure is only logged.
#[tokio::test]
async fn a_failure_after_the_deadline_forfeits_the_error_not_the_mutation() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let sessions = dir.path().join("sessions.json");
    let cfg = test_config(&sessions);
    let (app, _platform) = build_app(cfg, MockBackend::new(realistic_parts())).await;
    // A directory squatting on the target makes the write fail.
    std::fs::create_dir(&sessions).unwrap();
    let handle = app.sessions_handle();
    let pause = app
        .sessions
        .lock()
        .await
        .pause_writes()
        .expect("the app's store has a writer thread");
    pause.hold();

    let deadline = tokio::time::Instant::now() + std::time::Duration::from_millis(60);
    let declared = handle
        .declare_pending_until(&key(), "/work/b", None, deadline)
        .await;
    assert!(
        declared.is_ok_and(|pending| pending.is_some()),
        "the in-flight write is not an error: the mutation already happened"
    );
    pause.release();
    // Let the writer hit the failure; nothing observes it by design.
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    assert!(
        app.sessions
            .lock()
            .await
            .pending_for(&key())
            .is_some_and(|pending| pending.directory == "/work/b"),
        "the mutation stays in memory for the next blocking write"
    );
}

/// A store lock held across the whole click must not produce a late commit:
/// the deadline starts when the callback arrives, so the pre-dispatch read (the
/// action span's context) spends the same budget, and the declaration past it
/// refuses instead of committing after Feishu dropped the ack.
#[tokio::test]
async fn a_lock_held_across_the_click_refuses_instead_of_committing_late() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let (app, _platform) = build_app(cfg, MockBackend::new(realistic_parts())).await;
    seed_entry(
        &app,
        crate::config::SessionEntry::new(key(), "ses_old", "/work/proj"),
    )
    .await;

    // Hold the store past the click's budget from another task, so the click's
    // own awaits cannot deadlock the test.
    let store = std::sync::Arc::clone(&app.sessions);
    let (held, lock_taken) = tokio::sync::oneshot::channel::<()>();
    let (release, released) = tokio::sync::oneshot::channel::<()>();
    let holder = tokio::spawn(async move {
        let guard = store.lock().await;
        let _ = held.send(());
        let _ = released.await;
        drop(guard);
    });
    lock_taken.await.expect("the holder takes the lock");

    let started = std::time::Instant::now();
    let result = app
        .host_action(serde_json::json!({
            "action": "switch",
            "op": "new",
            "chat_id": "chat_1",
            "thread_id": "chat_1",
        }))
        .await
        .expect("the click still answers");
    let elapsed = started.elapsed();
    let _ = release.send(());
    holder.await.expect("the holder releases the lock");

    assert!(
        elapsed < crate::feishu::ws::CARD_ACK_BUDGET,
        "the click answered inside the ack budget: {elapsed:?}"
    );
    let toast = result.toast.unwrap_or_default();
    assert!(
        toast.contains("处理超时"),
        "a click that could not reach the store in time refuses: {toast:?}"
    );
    assert!(
        app.sessions.lock().await.pending_for(&key()).is_none(),
        "and commits nothing"
    );
}

/// The same bound on the browse card's 选择, the second call site of
/// `declare_pending_dir` — a regression to a blocking write here would not be
/// caught by the Recent Directories test.
#[tokio::test]
async fn a_stalled_writer_does_not_stall_the_browse_pick_callback() {
    let _wd = test_work_dir();
    let proj = tempfile::tempdir().unwrap();
    let dir = tempfile::tempdir().unwrap();
    let sessions = dir.path().join("sessions.json");
    let cfg = test_config(&sessions);
    let (app, _platform) = build_app(cfg, MockBackend::new(realistic_parts())).await;
    let pause = app
        .sessions
        .lock()
        .await
        .pause_writes()
        .expect("the app's store has a writer thread");
    pause.hold();

    let started = std::time::Instant::now();
    let result = app
        .host_action(serde_json::json!({
            "action": "dir",
            "view": "browse",
            "op": "pick",
            "chat_id": "chat_1",
            "thread_id": "chat_1",
            "dir": proj.path().to_string_lossy(),
        }))
        .await
        .expect("the click still answers");
    let elapsed = started.elapsed();

    assert!(
        elapsed < crate::feishu::ws::CARD_ACK_BUDGET,
        "the callback answered inside the ack budget with the write stalled: {elapsed:?}"
    );
    let expected = std::fs::canonicalize(proj.path()).unwrap();
    let toast = result.toast.unwrap_or_default();
    assert!(
        toast.contains(&expected.to_string_lossy().to_string()),
        "the pending took effect, so the toast says so: {toast:?}"
    );
    assert!(
        app.sessions
            .lock()
            .await
            .pending_for(&key())
            .is_some_and(|pending| pending.directory == expected.to_string_lossy()),
        "the mutation is in memory before the file"
    );

    pause.release();
    let expected = expected.to_string_lossy().to_string();
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        if saved_pending_dirs(&sessions) == vec![expected.clone()] {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the deferred write landed after the callback"
        );
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
}

/// The same bound on the `/switch` card's 新建, which declares through
/// `declare_pending_in_current_project_until` rather than `declare_pending_dir`
/// — a regression to a blocking write there would not be caught by the `/dir`
/// test.
#[tokio::test]
async fn a_stalled_writer_does_not_stall_the_switch_new_callback() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let sessions = dir.path().join("sessions.json");
    let cfg = test_config(&sessions);
    let (app, _platform) = build_app(cfg, MockBackend::new(realistic_parts())).await;
    // The card's current project needs a session to inherit.
    seed_entry(
        &app,
        crate::config::SessionEntry::new(key(), "ses_old", "/work/proj"),
    )
    .await;
    let pause = app
        .sessions
        .lock()
        .await
        .pause_writes()
        .expect("the app's store has a writer thread");
    pause.hold();

    let started = std::time::Instant::now();
    let result = app
        .host_action(serde_json::json!({
            "action": "switch",
            "op": "new",
            "chat_id": "chat_1",
            "thread_id": "chat_1",
        }))
        .await
        .expect("the click still answers");
    let elapsed = started.elapsed();

    assert!(
        elapsed < crate::feishu::ws::CARD_ACK_BUDGET,
        "the callback answered inside the ack budget with the write stalled: {elapsed:?}"
    );
    assert_eq!(
        result.toast.as_deref(),
        Some("下一条消息创建会话"),
        "the pending took effect, so the toast says so"
    );
    assert!(
        app.sessions
            .lock()
            .await
            .pending_for(&key())
            .is_some_and(|pending| pending.directory == "/work/proj"),
        "the mutation is in memory before the file"
    );

    pause.release();
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        if std::fs::read_to_string(&sessions).is_ok_and(|text| text.contains("/work/proj")) {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the deferred write landed after the callback"
        );
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
}

/// The click's card refresh is bounded by the same deadline as its store write:
/// a stalled session-list fetch costs the refresh, not the confirmation. The
/// toast (the answer the user actually reads) still arrives inside the ack
/// budget, and the pending is declared.
#[tokio::test]
async fn a_stalled_refresh_costs_the_card_not_the_confirmation() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let backend = MockBackend::new(realistic_parts());
    // The refresh's session list never answers.
    backend.hang_session_lists(1);
    let (app, _platform) = build_app(cfg, backend).await;

    let started = std::time::Instant::now();
    let result = app
        .host_action(serde_json::json!({
            "action": "dir",
            "op": "pick",
            "chat_id": "chat_1",
            "thread_id": "chat_1",
            "directory": "/work/b",
        }))
        .await
        .expect("the click still answers");
    let elapsed = started.elapsed();

    assert!(
        elapsed < crate::feishu::ws::CARD_ACK_BUDGET,
        "the refresh forfeited its card instead of the click's answer: {elapsed:?}"
    );
    let toast = result.toast.unwrap_or_default();
    assert!(
        toast.contains("/work/b"),
        "the confirmation is the toast, and it arrived: {toast:?}"
    );
    assert!(
        result.card.is_none(),
        "a refresh that missed the deadline must not be returned half-built"
    );
    assert!(
        app.sessions
            .lock()
            .await
            .pending_for(&key())
            .is_some_and(|pending| pending.directory == "/work/b"),
        "the pending is declared"
    );
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

    assert_eq!(
        app.command_handles().current_project_directory(&key()).await,
        "/work/pending"
    );
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

    send_command_in(
        &app,
        &format!("/dir {}", proj_dir.clone()),
        key(),
        "msg_dir",
        crate::config::ConversationKind::P2p,
    )
    .await;

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

    for (msg, text) in [
        ("msg_new", "/new wrong".to_string()),
        ("msg_dir", format!("/dir {first_dir}")),
        ("msg_dir2", format!("/dir {second_dir}")),
    ] {
        send_command_in(&app, &text, key(), msg, crate::config::ConversationKind::P2p).await;
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

    send_command_in(
        &app,
        "/new",
        key(),
        "msg_new",
        crate::config::ConversationKind::P2p,
    )
    .await;

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

    send_command_in(
        &app,
        "/new api-refactor",
        key(),
        "msg_new",
        crate::config::ConversationKind::P2p,
    )
    .await;
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

    send_command_in(
        &app,
        "/new",
        key(),
        "msg_new",
        crate::config::ConversationKind::P2p,
    )
    .await;

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
    backend.fail_create_sessions(1);
    let created = backend.created_session_dirs.clone();
    let (app, platform) = build_app(cfg, backend).await;

    send_command_in(
        &app,
        "/new",
        key(),
        "msg_new",
        crate::config::ConversationKind::P2p,
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
    backend.given_sessions(vec![list_session("ses_old", "旧会话", "/work/proj", 100)]);
    let (app, _platform) = build_app(cfg, backend).await;
    seed_entry(
        &app,
        crate::config::SessionEntry::new(key(), "ses_old", "/work/proj"),
    )
    .await;

    send_command_in(
        &app,
        "/new",
        key(),
        "msg_new",
        crate::config::ConversationKind::P2p,
    )
    .await;
    assert!(app.sessions.lock().await.pending_for(&key()).is_some());

    send_command_in(
        &app,
        "/switch ses_old",
        key(),
        "msg_switch",
        crate::config::ConversationKind::P2p,
    )
    .await;

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
    backend.title_patch_fails();
    let created = backend.created_session_dirs.clone();
    let (app, platform) = build_app(cfg, backend).await;

    send_command_in(
        &app,
        "/new api-refactor",
        key(),
        "msg_new",
        crate::config::ConversationKind::P2p,
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
/// cache is dropped and `/switch` sees the just-created session without
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

    send_command_in(
        &app,
        "/new",
        key(),
        "msg_new",
        crate::config::ConversationKind::P2p,
    )
    .await;
    send_command_in(
        &app,
        "/name api-refactor",
        key(),
        "msg_name",
        crate::config::ConversationKind::P2p,
    )
    .await;

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

    send_command_in(
        &app,
        "/name api-refactor",
        key(),
        "msg_name",
        crate::config::ConversationKind::P2p,
    )
    .await;

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

/// The settings commands write the Pending Session and the first message
/// materialises it with those values — the prompt itself already carries the
/// model, agent and variant (ADR-0041 command matrix).
#[tokio::test]
async fn settings_commands_write_the_pending_and_materialise() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let mut backend = MockBackend::new(realistic_parts());
    backend.with_models(vec![crate::backend::types::ProviderModels {
        provider: "p".into(),
        models: vec![model_option("test", &["high"])],
    }]);
    let prompt_models = backend.prompt_models.clone();
    let prompt_agents = backend.prompt_agents.clone();
    let prompt_variants = backend.prompt_variants.clone();
    let (app, _platform) = build_app(cfg, backend).await;
    seed_pending(&app, PendingEntry::new(key(), "/work/proj")).await;

    for text in ["/agent build", "/model p/test", "/think high", "/autoaccept on"] {
        send_command_in(&app, text, key(), "msg_cfg", crate::config::ConversationKind::P2p).await;
    }

    {
        let store = app.sessions.lock().await;
        let pending = store
            .pending_for(&key())
            .expect("the pending survives its configuration");
        assert_eq!(pending.agent.as_deref(), Some("build"));
        assert_eq!(pending.model.as_deref(), Some("p/test"));
        assert_eq!(pending.variant.as_deref(), Some("high"));
        assert!(pending.auto_accept);
        assert!(
            store.get_active(&key()).is_none(),
            "configuring a pending creates no session"
        );
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
        .expect("materialised");
    assert_eq!(entry.agent.as_deref(), Some("build"));
    assert_eq!(entry.model.as_deref(), Some("p/test"));
    assert_eq!(entry.variant.as_deref(), Some("high"));
    assert!(entry.auto_accept);
    assert_eq!(
        *prompt_models.lock().await,
        vec![Some("p/test".to_string())],
        "the first prompt runs with the pending's model"
    );
    assert_eq!(
        *prompt_agents.lock().await,
        vec![Some("build".to_string())],
        "the first prompt runs with the pending's agent"
    );
    assert_eq!(
        *prompt_variants.lock().await,
        vec![Some("high".to_string())],
        "the first prompt runs with the pending's variant"
    );
}

/// `/model` on a Pending Session applies the ADR-0020 variant-clearing rule to
/// the pending exactly as it does to a real entry.
#[tokio::test]
async fn model_switch_on_a_pending_clears_an_undeclared_variant() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let mut backend = MockBackend::new(realistic_parts());
    backend.with_models(vec![
        crate::backend::types::ProviderModels {
            provider: "p".into(),
            models: vec![model_option("test", &["low", "high"])],
        },
        crate::backend::types::ProviderModels {
            provider: "q".into(),
            models: vec![model_option("other", &[])],
        },
    ]);
    let (app, _platform) = build_app(cfg, backend).await;
    let mut pending = PendingEntry::new(key(), "/work/proj");
    pending.model = Some("p/test".into());
    pending.variant = Some("high".into());
    seed_pending(&app, pending).await;

    // A model that still declares `high` keeps the variant.
    send_command_in(
        &app,
        "/model p/test",
        key(),
        "msg_model_1",
        crate::config::ConversationKind::P2p,
    )
    .await;
    assert_eq!(
        app.sessions
            .lock()
            .await
            .pending_for(&key())
            .and_then(|p| p.variant.as_deref()),
        Some("high")
    );

    // A model that lacks it clears the pending's variant.
    send_command_in(
        &app,
        "/model q/other",
        key(),
        "msg_model_2",
        crate::config::ConversationKind::P2p,
    )
    .await;
    let store = app.sessions.lock().await;
    let pending = store.pending_for(&key()).unwrap();
    assert_eq!(pending.model.as_deref(), Some("q/other"));
    assert_eq!(pending.variant, None, "an undeclared variant is cleared");
}

/// `/think` on a Pending Session validates against the pending's effective
/// model (its own override, else the configured default).
#[tokio::test]
async fn think_on_a_pending_validates_against_the_pending_model() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let mut backend = MockBackend::new(realistic_parts());
    backend.with_models(vec![crate::backend::types::ProviderModels {
        provider: "p".into(),
        models: vec![model_option("test", &["low", "high"])],
    }]);
    let (app, platform) = build_app(cfg, backend).await;
    let mut pending = PendingEntry::new(key(), "/work/proj");
    pending.model = Some("p/test".into());
    seed_pending(&app, pending).await;

    send_command_in(
        &app,
        "/think medium",
        key(),
        "msg_think_1",
        crate::config::ConversationKind::P2p,
    )
    .await;
    let texts = platform.texts().await;
    assert!(
        texts.iter().any(|t| t.contains("不支持思考等级")),
        "an undeclared variant is rejected: {texts:?}"
    );
    assert_eq!(
        app.sessions
            .lock()
            .await
            .pending_for(&key())
            .and_then(|p| p.variant.clone()),
        None,
        "a rejected variant writes nothing"
    );

    send_command_in(
        &app,
        "/think high",
        key(),
        "msg_think_2",
        crate::config::ConversationKind::P2p,
    )
    .await;
    assert_eq!(
        app.sessions
            .lock()
            .await
            .pending_for(&key())
            .and_then(|p| p.variant.as_deref()),
        Some("high")
    );
}

/// The interactive cards render a Pending Session's current values instead of
/// the no-session error, and their buttons write the pending.
#[tokio::test]
async fn settings_cards_render_and_configure_a_pending() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let mut backend = MockBackend::new(realistic_parts());
    backend.with_models(vec![crate::backend::types::ProviderModels {
        provider: "p".into(),
        models: vec![model_option("test", &["low", "high"])],
    }]);
    backend.with_agents(vec![crate::backend::types::AgentInfo {
        name: "build".into(),
        description: None,
        mode: Some("primary".into()),
        hidden: Some(false),
    }]);
    let (app, platform) = build_app(cfg, backend).await;
    let mut pending = PendingEntry::new(key(), "/work/proj");
    pending.model = Some("p/test".into());
    pending.variant = Some("high".into());
    seed_pending(&app, pending).await;

    for text in ["/agent", "/model", "/think", "/autoaccept"] {
        send_command_in(
            &app,
            text,
            key(),
            "msg_card",
            crate::config::ConversationKind::P2p,
        )
        .await;
    }

    let calls = platform.calls.lock().await.clone();
    let cards: Vec<String> = calls
        .iter()
        .filter_map(|c| match c {
            PlatformCall::ReplyCard { card, .. } => Some(card.to_string()),
            _ => None,
        })
        .collect();
    assert!(
        cards
            .iter()
            .any(|c| c.contains("当前 Agent") && c.contains("`build`")),
        "agent card shows the pending's effective agent: {cards:?}"
    );
    assert!(
        cards
            .iter()
            .any(|c| c.contains("当前模型") && c.contains("p/test@high")),
        "model card shows the pending's model and variant: {cards:?}"
    );
    assert!(
        cards.iter().any(|c| c.contains("思考等级") && c.contains("high")),
        "think card shows the pending's variant: {cards:?}"
    );
    assert!(
        cards.iter().any(|c| c.contains("自动审批")),
        "autoaccept card renders for a pending: {cards:?}"
    );
    assert!(
        !platform.texts().await.iter().any(|t| t.contains("还没有会话")),
        "no settings card fell back to the no-session reply"
    );

    // The buttons write the pending, not a session.
    let agent = serde_json::json!({
        "action": "agent", "chat_id": "chat_1", "thread_id": "chat_1", "value": "build",
    });
    let result = app.host_action(agent).await.expect("agent card action");
    assert!(result.card.is_some(), "agent card refreshes");
    let model = serde_json::json!({
        "action": "model", "level": "model",
        "chat_id": "chat_1", "thread_id": "chat_1", "value": "p/test",
    });
    app.host_action(model).await.expect("model card action");
    let think = serde_json::json!({
        "action": "think", "chat_id": "chat_1", "thread_id": "chat_1", "value": "low",
    });
    app.host_action(think).await.expect("think card action");
    let autoaccept = serde_json::json!({
        "action": "autoaccept", "chat_id": "chat_1", "thread_id": "chat_1", "value": "on",
    });
    app.host_action(autoaccept).await.expect("autoaccept card action");

    let store = app.sessions.lock().await;
    let pending = store
        .pending_for(&key())
        .expect("the pending survives the card actions");
    assert_eq!(pending.agent.as_deref(), Some("build"));
    assert_eq!(pending.model.as_deref(), Some("p/test"));
    assert_eq!(pending.variant.as_deref(), Some("low"));
    assert!(pending.auto_accept);
    assert!(
        store.get_active(&key()).is_none(),
        "card actions create no session"
    );
}

/// `/compact` and `/stop` on a Pending Session keep today's no-session replies
/// and send nothing to the backend (ADR-0041 command matrix).
#[tokio::test]
async fn compact_and_stop_on_a_pending_do_not_reach_the_backend() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let backend = MockBackend::new(realistic_parts());
    let interrupt_calls = backend.interrupt_calls.clone();
    let compact_calls = backend.compact_calls.clone();
    let created = backend.created_session_dirs.clone();
    let (app, platform) = build_app(cfg, backend).await;
    seed_pending(&app, PendingEntry::new(key(), "/work/proj")).await;

    for text in ["/compact", "/stop"] {
        send_command_in(
            &app,
            text,
            key(),
            "msg_lifecycle",
            crate::config::ConversationKind::P2p,
        )
        .await;
    }

    let texts = platform.texts().await.join("\n");
    assert!(texts.contains("还没有会话"), "compact no-session reply: {texts}");
    assert!(
        texts.contains("当前没有正在执行的会话"),
        "stop no-session reply: {texts}"
    );
    assert!(compact_calls.lock().await.is_empty(), "nothing compacted");
    assert!(interrupt_calls.lock().await.is_empty(), "nothing interrupted");
    assert!(created.lock().await.is_empty(), "no session created");
    assert!(
        app.sessions.lock().await.pending_for(&key()).is_some(),
        "both commands leave the pending alone"
    );
}

/// `/switch forget` clears the Pending Session together with the thread's
/// mappings (ADR-0041).
#[tokio::test]
async fn switch_forget_clears_the_pending_too() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let (app, _platform) = build_app(cfg, MockBackend::new(realistic_parts())).await;
    seed_entry(
        &app,
        crate::config::SessionEntry::new(key(), "ses_old", "/work/a"),
    )
    .await;
    seed_pending(&app, PendingEntry::new(key(), "/work/proj")).await;

    send_command_in(
        &app,
        "/switch forget",
        key(),
        "msg_forget",
        crate::config::ConversationKind::P2p,
    )
    .await;

    let store = app.sessions.lock().await;
    assert!(store.pending_for(&key()).is_none(), "the pending is forgotten");
    assert!(store.list_thread(&key()).is_empty(), "the mappings are forgotten");
}

/// A Pending Session's picks become session switches at materialisation: the
/// session that is created gets the model ref (variant inside it) and the
/// agent written to it, so a durable generation's first prompt needs no
/// per-prompt fields (V1's switches are no-ops; its prompt still carries them).
#[tokio::test]
async fn pending_picks_are_switched_when_the_session_materialises() {
    use crate::backend::types::ModelInfo;

    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let backend = MockBackend::new(realistic_parts());
    let switch_models = backend.switch_model_calls.clone();
    let switch_agents = backend.switch_agent_calls.clone();
    let (app, _platform) = build_app(cfg, backend).await;
    let mut pending = PendingEntry::new(key(), "/work/proj");
    pending.model = Some("p/test".into());
    pending.variant = Some("high".into());
    pending.agent = Some("build".into());
    seed_pending(&app, pending).await;

    app.handle_message(incoming(
        "msg_prompt".into(),
        "chat_1".into(),
        "p2p".into(),
        None,
        "hi".into(),
        None,
    ))
    .await;

    assert_eq!(
        switch_models.lock().await.as_slice(),
        &[(
            "ses_test".to_string(),
            ModelInfo {
                id: "test".into(),
                provider_id: "p".into(),
                variant: Some("high".into()),
            }
        )],
        "the created session gets the pending's model ref"
    );
    assert_eq!(
        switch_agents.lock().await.as_slice(),
        &[("ses_test".to_string(), "build".to_string())],
        "the created session gets the pending's agent"
    );
    // The materialised entry still carries the picks for V1's per-prompt path.
    let entry = app.sessions.lock().await.get_active(&key()).cloned().unwrap();
    assert_eq!(entry.model.as_deref(), Some("p/test"));
    assert_eq!(entry.variant.as_deref(), Some("high"));
    assert_eq!(entry.agent.as_deref(), Some("build"));
}

/// A Pending Session's `/model` pick is applied at materialisation through the
/// same rule as a live pick: a variant the model does not declare is cleared
/// (ADR-0020) instead of being written into the created session.
#[tokio::test]
async fn pending_variant_is_cleared_when_the_model_does_not_declare_it() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let mut backend = MockBackend::new(realistic_parts());
    backend.with_models(vec![crate::backend::types::ProviderModels {
        provider: "p".into(),
        models: vec![model_option("plain", &[])],
    }]);
    let switch_calls = backend.switch_model_calls.clone();
    let (app, _platform) = build_app(cfg, backend).await;
    let mut pending = PendingEntry::new(key(), "/work/proj");
    pending.model = Some("p/plain".into());
    pending.variant = Some("high".into());
    seed_pending(&app, pending).await;

    app.handle_message(incoming(
        "msg_prompt".into(),
        "chat_1".into(),
        "p2p".into(),
        None,
        "hi".into(),
        None,
    ))
    .await;

    let calls = switch_calls.lock().await.clone();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].0, "ses_test");
    assert_eq!(calls[0].1.id, "plain");
    assert_eq!(calls[0].1.variant, None, "the undeclared variant is cleared");
    assert!(
        app.sessions
            .lock()
            .await
            .get_active(&key())
            .and_then(|e| e.variant.clone())
            .is_none(),
        "the created entry must not carry the variant either"
    );
}

/// A variant-only Pending Session resolves the model the create recorded and
/// attaches the variant to that ref (no `/model` pick to carry it).
#[tokio::test]
async fn variant_only_pending_switches_the_creates_recorded_model() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let mut backend = MockBackend::new(realistic_parts());
    backend.with_models(vec![crate::backend::types::ProviderModels {
        provider: "p".into(),
        models: vec![model_option("plain", &["high"])],
    }]);
    backend.with_session_model(crate::backend::types::SessionModel {
        provider_id: "p".into(),
        id: "plain".into(),
    });
    let switch_calls = backend.switch_model_calls.clone();
    let (app, _platform) = build_app(cfg, backend).await;
    let mut pending = PendingEntry::new(key(), "/work/proj");
    pending.variant = Some("high".into());
    seed_pending(&app, pending).await;

    app.handle_message(incoming(
        "msg_prompt".into(),
        "chat_1".into(),
        "p2p".into(),
        None,
        "hi".into(),
        None,
    ))
    .await;

    let calls = switch_calls.lock().await.clone();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].1.id, "plain");
    assert_eq!(calls[0].1.variant.as_deref(), Some("high"));
}
