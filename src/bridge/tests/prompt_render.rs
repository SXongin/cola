use crate::bridge::test_support::*;

#[tokio::test]
async fn handle_prompt_renders_reasoning_tools_and_text() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let (app, platform) = build_app(cfg, MockBackend::new(realistic_parts())).await;

    app.handle_message(incoming(
        "msg_1".into(),
        "chat_1".into(),
        "p2p".into(),
        None,
        "分析一下目录".into(),
        None,
    ))
    .await;

    let calls = platform.calls.lock().await.clone();
    // First call must be the Loading reply card.
    assert!(matches!(calls.first(), Some(PlatformCall::ReplyCard { .. })));
    // At least one card update (flush) must follow.
    let updates = platform.updated_cards().await;
    assert!(!updates.is_empty(), "expected card updates, got: {:?}", calls);

    let final_card = updates.last().unwrap().clone();
    let text = final_card.to_string();
    assert!(text.contains("✅"), "final header should be Done: {}", text);
    assert!(text.contains("推理过程"), "reasoning panel missing: {}", text);
    assert!(text.contains("bash"), "tool panel missing: {}", text);
    assert!(
        text.contains("当前目录有 src/ 和 Cargo.toml。"),
        "text missing: {}",
        text
    );
    assert!(text.contains("ls -la"), "tool input missing: {}", text);
}

/// ADR-0019: the Turn Footer's work context is captured at turn start AND
/// refreshed at turn end — the final card shows the branch the AI landed on
/// (here one it created and committed to), not the one it started from.
#[tokio::test]
async fn turn_footer_shows_the_branch_the_ai_landed_on() {
    let dir = tempfile::tempdir().unwrap();
    let repo = git_repo();
    let repo_path = repo.path().to_path_buf();
    let mut cfg = test_config(&dir.path().join("sessions.json"));
    cfg.bridge.work_dir = Some(repo_path.clone());

    let mut mock = MockBackend::new(realistic_parts());
    mock.on_prompt = Some(Box::new(move || {
        // The AI's work: branch off and commit — the tree ends clean.
        git_in(&repo_path, &["switch", "-c", "feat/ai-work"]);
        std::fs::write(repo_path.join("b.txt"), "done").unwrap();
        git_in(&repo_path, &["add", "b.txt"]);
        git_in(&repo_path, &["commit", "-m", "ai work"]);
    }));
    let (app, platform) = build_app(cfg, mock).await;

    app.handle_message(incoming(
        "msg_1".into(),
        "chat_1".into(),
        "p2p".into(),
        None,
        "干个活".into(),
        None,
    ))
    .await;

    let text = final_card(&platform).await.to_string();
    assert!(
        text.contains("· feat/ai-work"),
        "final footer must show the end branch: {text}"
    );
    assert!(
        !text.contains("· main"),
        "the start branch must be gone from the final card: {text}"
    );
    assert!(
        !text.contains("⚠"),
        "a committed tree must not show the dirty marker: {text}"
    );
}

/// ADR-0019: a turn that ends with uncommitted changes lights the ⚠ on the
/// final card even though the tree was clean when the turn started.
#[tokio::test]
async fn turn_footer_shows_dirty_left_by_the_ai() {
    let dir = tempfile::tempdir().unwrap();
    let repo = git_repo();
    let repo_path = repo.path().to_path_buf();
    let mut cfg = test_config(&dir.path().join("sessions.json"));
    cfg.bridge.work_dir = Some(repo_path.clone());

    let mut mock = MockBackend::new(realistic_parts());
    mock.on_prompt = Some(Box::new(move || {
        std::fs::write(repo_path.join("uncommitted.txt"), "left behind").unwrap();
    }));
    let (app, platform) = build_app(cfg, mock).await;

    app.handle_message(incoming(
        "msg_1".into(),
        "chat_1".into(),
        "p2p".into(),
        None,
        "改点东西".into(),
        None,
    ))
    .await;

    let text = final_card(&platform).await.to_string();
    assert!(
        text.contains("· main ⚠"),
        "final footer must show the AI's uncommitted changes: {text}"
    );
}

/// ADR-0019: a PARTIAL turn-end read — the branch still resolves but the
/// `status` command fails (e.g. index lock contention) — must keep the
/// start capture. A failed status read is not a clean tree; clearing the
/// start ⚠ would silently lie about the working tree.
#[tokio::test]
async fn turn_footer_keeps_start_capture_when_the_end_status_read_fails() {
    let dir = tempfile::tempdir().unwrap();
    let repo = git_repo();
    let repo_path = repo.path().to_path_buf();
    // The tree starts dirty (untracked file): the start card shows `main ⚠`.
    std::fs::write(repo_path.join("pre-existing.txt"), "wip").unwrap();
    let mut cfg = test_config(&dir.path().join("sessions.json"));
    cfg.bridge.work_dir = Some(repo_path.clone());

    let mut mock = MockBackend::new(realistic_parts());
    mock.on_prompt = Some(Box::new(move || {
        // Break the index mid-turn: `rev-parse` reads HEAD and still
        // resolves `main`, while `git status --porcelain` fails.
        let index = repo_path.join(".git/index");
        std::fs::remove_file(&index).unwrap();
        std::fs::create_dir(&index).unwrap();
    }));
    let (app, platform) = build_app(cfg, mock).await;

    app.handle_message(incoming(
        "msg_1".into(),
        "chat_1".into(),
        "p2p".into(),
        None,
        "干点活".into(),
        None,
    ))
    .await;

    let text = final_card(&platform).await.to_string();
    assert!(
        text.contains("· main ⚠"),
        "a failed end read must keep the start capture: {text}"
    );
}

#[tokio::test]
async fn prompt_error_renders_error_card() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let mut backend = MockBackend::new(realistic_parts());
    backend.prompt_error = Some("Streaming response failed: [503] The request queue is full.".into());
    let (app, platform) = build_app(cfg, backend).await;

    app.handle_message(incoming(
        "msg_1".into(),
        "chat_1".into(),
        "p2p".into(),
        None,
        "hi".into(),
        None,
    ))
    .await;

    let calls = platform.calls.lock().await.clone();
    let updates: Vec<_> = calls
        .iter()
        .filter_map(|c| match c {
            PlatformCall::UpdateMessage { card, .. } => Some(card.clone()),
            _ => None,
        })
        .collect();
    assert!(
        !updates.is_empty(),
        "expected an error card update, got: {:?}",
        calls
    );
    let card = updates.last().unwrap().to_string();
    assert!(card.contains("❌"), "error card header missing: {}", card);
    assert!(card.contains("503"), "error text missing: {}", card);
}

#[tokio::test]
async fn error_card_retry_reuses_card_and_reruns_prompt() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    // First prompt fails (provider hiccup); the retry must succeed.
    let mock = MockBackend::new(realistic_parts());
    mock.fail_prompt_count
        .store(1, std::sync::atomic::Ordering::SeqCst);
    let prompt_calls = mock.prompt_calls.clone();
    let prompt_ids = mock.prompt_message_ids.clone();
    let backend = Arc::new(mock);
    let platform = Arc::new(RecordingPlatform::new());
    let app = Arc::new(App::new(cfg, backend, platform.clone()).unwrap());

    app.handle_message(incoming(
        "msg_1".into(),
        "chat_1".into(),
        "p2p".into(),
        None,
        "hi".into(),
        None,
    ))
    .await;

    // The error card must carry a retry button.
    let calls = platform.calls.lock().await.clone();
    let updates: Vec<_> = calls
        .iter()
        .filter_map(|c| match c {
            PlatformCall::UpdateMessage { card, .. } => Some(card.clone()),
            _ => None,
        })
        .collect();
    let error_card = updates.last().unwrap().clone();
    let err_text = error_card.to_string();
    assert!(err_text.contains("❌"), "error card missing: {}", err_text);
    assert!(err_text.contains("重试"), "retry button missing: {}", err_text);

    // The card the retry will reuse: the loading reply card id.
    let card_id = match calls.first().unwrap() {
        PlatformCall::ReplyCard { card, .. } if card.to_string().contains("思考中") => "msg_reply",
        _ => panic!("expected a loading reply card first: {:?}", calls),
    };
    assert_eq!(card_id, "msg_reply");

    // User clicks the retry button.
    let retry = app
        .handle_card_action(serde_json::json!({ "action": "retry", "session_id": "ses_test" }))
        .await;
    assert!(retry.is_some(), "retry ack card expected");
    assert_eq!(retry.unwrap().toast.as_deref(), Some("正在重试..."));

    // The spawned retry re-runs the prompt on the SAME card, not a new reply.
    tokio::time::sleep(std::time::Duration::from_millis(2500)).await;

    let calls = platform.calls.lock().await.clone();
    let updates: Vec<_> = calls
        .iter()
        .filter_map(|c| match c {
            PlatformCall::UpdateMessage { message_id, card } => Some((message_id.clone(), card.clone())),
            _ => None,
        })
        .collect();
    assert_eq!(
        updates.last().unwrap().0,
        "msg_reply",
        "retry must update the original card, not send a new one: {:?}",
        calls
    );
    let final_card = updates.last().unwrap().1.to_string();
    assert!(
        final_card.contains("✅"),
        "retry should finish Done: {}",
        final_card
    );
    assert!(
        final_card.contains("当前目录有 src/ 和 Cargo.toml。"),
        "retried answer missing: {}",
        final_card
    );

    let backend_calls = prompt_calls.lock().await.clone();
    assert_eq!(backend_calls, vec!["hi".to_string(), "hi".to_string()]);
    // Both attempts are the SAME logical user message: a fresh `msg_cola_`
    // id on the first send, REUSED by the retry (ADR-0026) so the server
    // deduplicates instead of appending a second user message.
    let message_ids = prompt_ids.lock().await.clone();
    assert_eq!(message_ids.len(), 2, "two prompt attempts expected");
    let first = message_ids[0].clone().expect("prompt must carry a message id");
    assert!(
        first.starts_with("msg_cola_"),
        "cola prompt must self-identify: {}",
        first
    );
    assert_eq!(
        message_ids[1].as_deref(),
        Some(first.as_str()),
        "retry must reuse the failed attempt's message id"
    );
}

#[tokio::test]
async fn group_completion_sends_notice_to_requester() {
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
        Some("ou_requester".into()),
    ))
    .await;

    let calls = platform.calls.lock().await.clone();
    let notices: Vec<_> = calls
        .iter()
        .filter_map(|c| match c {
            PlatformCall::CompletionNotice {
                reply_to,
                open_id,
                text,
                ..
            } => Some((reply_to.clone(), open_id.clone(), text.clone())),
            _ => None,
        })
        .collect();
    assert_eq!(
        notices,
        vec![(
            "msg_1".to_string(),
            "ou_requester".to_string(),
            "✅ 已完成。".to_string()
        )]
    );
}

#[tokio::test]
async fn group_completion_at_mentions_requester_when_name_resolvable() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let mut rp = RecordingPlatform::new();
    rp.user_names
        .insert("ou_requester".to_string(), "李明".to_string());
    let platform = Arc::new(rp);
    let app = Arc::new(
        App::new(
            cfg,
            Arc::new(MockBackend::new(realistic_parts())),
            platform.clone(),
        )
        .unwrap(),
    );

    app.handle_message(incoming(
        "msg_1".into(),
        "oc_group_1".into(),
        "group".into(),
        None,
        "hi".into(),
        Some("ou_requester".into()),
    ))
    .await;

    let calls = platform.calls.lock().await.clone();
    let notices: Vec<_> = calls
        .iter()
        .filter_map(|c| match c {
            PlatformCall::CompletionNotice {
                reply_to,
                open_id,
                name,
                text,
            } => Some((reply_to.clone(), open_id.clone(), name.clone(), text.clone())),
            _ => None,
        })
        .collect();
    assert_eq!(
        notices,
        vec![(
            "msg_1".to_string(),
            "ou_requester".to_string(),
            Some("李明".to_string()),
            "✅ 已完成。".to_string()
        )]
    );
}

#[tokio::test]
async fn p2p_prompt_sends_no_completion_notice() {
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
        Some("ou_user".into()),
    ))
    .await;

    let calls = platform.calls.lock().await.clone();
    assert!(
        !calls
            .iter()
            .any(|c| matches!(c, PlatformCall::CompletionNotice { .. })),
        "p2p must not send a completion notice: {:?}",
        calls
    );
}

#[tokio::test]
async fn subtitle_falls_back_to_id_tail_without_server_title() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let (app, _) = build_app(cfg, MockBackend::new(realistic_parts())).await;
    let key = crate::config::ThreadKey::new("chat_1".into(), "chat_1".into());

    seed_entry(
        &app,
        crate::config::SessionEntry {
            thread_key: key.clone(),
            session_id: "ses_01ba0ed03ffeRvYNWua6mg8d9c".into(),
            directory: "/tmp/x".into(),
            agent: None,
            model: None,
            auto_accept: false,
            topic_anchor: None,
            topic_root: None,
            variant: None,
        },
    )
    .await;

    // No server title → the id-tail alone identifies the session (no cola
    // side name to fall back on; the current prompt is never echoed).
    assert_eq!(
        crate::bridge::render::session_subtitle(&app.core, &key, "另一个问题").await,
        "01ba0ed"
    );
    assert_eq!(
        crate::bridge::render::session_subtitle(&app.core, &key, "你好").await,
        "01ba0ed"
    );
}

/// The session-title fetch must degrade, not hang the turn, when the
/// server swallows the request — a freshly spawned Owned Server's startup
/// window is exactly this: the first request is read but never dispatched
/// (the Lazy Start silent-hang incident).
#[tokio::test]
async fn subtitle_degrades_when_session_info_hangs() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let mock = MockBackend::new(realistic_parts());
    mock.hang_session_info
        .store(usize::MAX, std::sync::atomic::Ordering::SeqCst);
    let (app, _) = build_app(cfg, mock).await;
    let key = crate::config::ThreadKey::new("chat_1".into(), "chat_1".into());
    seed_entry(
        &app,
        crate::config::SessionEntry {
            thread_key: key.clone(),
            session_id: "ses_01ba0ed03ffeRvYNWua6mg8d9c".into(),
            directory: "/tmp/x".into(),
            agent: None,
            model: None,
            auto_accept: false,
            topic_anchor: None,
            topic_root: None,
            variant: None,
        },
    )
    .await;
    // The fetch hangs forever; the subtitle must still return (id-tail
    // only) within the bound instead of hanging the prompt flow.
    let subtitle = tokio::time::timeout(
        std::time::Duration::from_secs(10),
        crate::bridge::render::session_subtitle(&app.core, &key, "问题"),
    )
    .await
    .expect("session_subtitle must not hang when session_info never returns");
    assert_eq!(subtitle, "01ba0ed");
}

/// The Lazy Start readiness probe must keep retrying through wedged
/// attempts (the spawned server's startup window) and only return once
/// the server actually serves a request.
#[tokio::test]
async fn readiness_wait_recovers_after_wedged_attempts() {
    let mock = MockBackend::new(realistic_parts());
    mock.hang_list_sessions
        .store(2, std::sync::atomic::Ordering::SeqCst);
    let backend: Arc<dyn crate::opencode::Backend> = Arc::new(mock);
    let result =
        crate::bridge::pollers::wait_for_server_ready(&backend, std::time::Duration::from_secs(10)).await;
    assert!(
        result.is_ok(),
        "readiness wait must recover after the startup window: {result:?}"
    );
}

/// A server that never serves a request must fail the readiness wait, so
/// the lazy start reports the failure instead of hanging the turn
/// silently.
#[tokio::test]
async fn readiness_wait_fails_when_server_never_serves() {
    let mock = MockBackend::new(realistic_parts());
    mock.hang_list_sessions
        .store(usize::MAX, std::sync::atomic::Ordering::SeqCst);
    let backend: Arc<dyn crate::opencode::Backend> = Arc::new(mock);
    let result =
        crate::bridge::pollers::wait_for_server_ready(&backend, std::time::Duration::from_secs(2)).await;
    assert!(
        result.is_err(),
        "readiness wait must fail when the server never serves"
    );
}

/// A server default title (`New session - ...`) is treated as absent — the
/// id-tail is shown until the server generates a real title.
#[tokio::test]
async fn subtitle_ignores_server_default_title() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let mock = MockBackend::new(realistic_parts());
    mock.session_titles.lock().unwrap().insert(
        "ses_00ea4e77cffez1fo4wrNuJyHF0".into(),
        "New session - 2026-08-28".into(),
    );
    let (app, _) = build_app(cfg, mock).await;
    let key = crate::config::ThreadKey::new("chat_1".into(), "chat_1".into());

    seed_entry(
        &app,
        crate::config::SessionEntry {
            thread_key: key.clone(),
            session_id: "ses_00ea4e77cffez1fo4wrNuJyHF0".into(),
            directory: "/tmp/y".into(),
            agent: None,
            model: None,
            auto_accept: false,
            topic_anchor: None,
            topic_root: None,
            variant: None,
        },
    )
    .await;
    assert_eq!(
        crate::bridge::render::session_subtitle(&app.core, &key, "另一个问题").await,
        "00ea4e7"
    );
}

/// The card subtitle prefers the OpenCode server's own session title (what
/// OpenChamber shows), not cola's `/new`-generated names.
#[tokio::test]
async fn subtitle_prefers_server_title() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let mock = MockBackend::new(realistic_parts());
    mock.session_titles
        .lock()
        .unwrap()
        .insert("ses_test".into(), "OpenChamber 显示的标题".into());
    let (app, _) = build_app(cfg, mock).await;
    let key = crate::config::ThreadKey::new("chat_1".into(), "chat_1".into());

    seed_entry(
        &app,
        crate::config::SessionEntry {
            thread_key: key.clone(),
            session_id: "ses_test".into(),
            directory: "/tmp/x".into(),
            agent: None,
            model: None,
            auto_accept: false,
            topic_anchor: None,
            topic_root: None,
            variant: None,
        },
    )
    .await;
    assert_eq!(
        crate::bridge::render::session_subtitle(&app.core, &key, "问题").await,
        "OpenChamber 显示的标题 · test"
    );
}

/// The card subtitle follows the server's live session title during a turn:
/// OpenCode auto-renames a session after streaming, and `refresh_session_title`
/// (called from the render poll loop) must update the card instead of leaving
/// it on the "new session" default until restart.
#[tokio::test]
async fn refresh_session_title_updates_live_card_on_server_rename() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let mock = MockBackend::new(realistic_parts());
    // The server already has a final auto-generated title.
    mock.session_titles
        .lock()
        .unwrap()
        .insert("ses_test".into(), "修复登录鉴权问题".into());
    let backend = Arc::new(mock);
    let platform = Arc::new(RecordingPlatform::new());
    let app = Arc::new(App::new(cfg, backend.clone(), platform.clone()).unwrap());
    let key = crate::config::ThreadKey::new("chat_1".into(), "chat_1".into());

    seed_entry(
        &app,
        crate::config::SessionEntry {
            thread_key: key.clone(),
            session_id: "ses_test".into(),
            directory: "/tmp/x".into(),
            agent: None,
            model: None,
            auto_accept: false,
            topic_anchor: None,
            topic_root: None,
            variant: None,
        },
    )
    .await;

    // Simulate an in-flight turn whose card was captured with the OLD
    // default subtitle before the server auto-titled the session.
    let mut acc = crate::bridge::streaming::StreamAccumulator::new("test");
    acc.reply_to_message_id = Some("msg_1".into());
    acc.session_id = Some("ses_test".into());
    {
        let mut cards = app.cards.lock().await;
        cards.insert(
            "ses_test".into(),
            crate::bridge::streaming::CardSession::new(acc, None),
        );
    }

    // The server's live title differs → refresh must update the card subtitle.
    let refreshed = crate::bridge::render::refresh_session_title(&app.core, "ses_test").await;
    assert!(refreshed, "a server rename must refresh the card title");
    let title = app.cards.lock().await.get("ses_test").unwrap().acc.title.clone();
    assert_eq!(title, "修复登录鉴权问题 · test");
    // A second refresh with no further change must be a no-op (no churn).
    assert!(
        !crate::bridge::render::refresh_session_title(&app.core, "ses_test").await,
        "no change when the title already matches"
    );
    // No accumulator (a finished turn) → refresh is a no-op.
    app.cards.lock().await.remove("ses_test");
    assert!(!crate::bridge::render::refresh_session_title(&app.core, "ses_test").await);
}

/// ADR-0023: when the server auto-titles a session MID-TURN, the render
/// tick that refreshes the live card's title must ALSO patch the topic
/// cover card — the chat-list entry updates as early as the title agent
/// finishes, not only when the turn completes.
#[tokio::test]
async fn refresh_session_title_syncs_cover_card_mid_turn() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let mock = MockBackend::new(realistic_parts());
    mock.session_titles
        .lock()
        .unwrap()
        .insert("ses_test".into(), "修复登录鉴权问题".into());
    let (app, platform) = build_app(cfg, mock).await;
    let key = crate::config::ThreadKey::new("chat_1".into(), "omt_t_1".into());

    seed_entry(
        &app,
        crate::config::SessionEntry {
            thread_key: key.clone(),
            session_id: "ses_test".into(),
            directory: "/tmp/x".into(),
            agent: None,
            model: None,
            auto_accept: false,
            topic_anchor: Some("om_seed".into()),
            topic_root: Some("om_cover".into()),
            variant: None,
        },
    )
    .await;
    seed_cover_title(&app, "ses_test", "cola").await;
    // An in-flight turn whose card was captured with the OLD subtitle
    // (before the server auto-titled the session).
    let mut acc = crate::bridge::streaming::StreamAccumulator::new("test");
    acc.reply_to_message_id = Some("msg_1".into());
    acc.session_id = Some("ses_test".into());
    {
        let mut cards = app.cards.lock().await;
        cards.insert(
            "ses_test".into(),
            crate::bridge::streaming::CardSession::new(acc, None),
        );
    }

    let refreshed = crate::bridge::render::refresh_session_title(&app.core, "ses_test").await;
    assert!(refreshed, "the mid-turn title change must refresh the card");

    let calls = platform.calls.lock().await.clone();
    let patched = calls
        .iter()
        .filter_map(|c| match c {
            PlatformCall::UpdateMessage { message_id, card } if message_id == "om_cover" => {
                Some(card.to_string())
            }
            _ => None,
        })
        .collect::<Vec<_>>();
    assert!(
        !patched.is_empty(),
        "the cover card must be patched on the same tick, got {calls:?}"
    );
    assert!(
        patched.last().unwrap().contains("修复登录鉴权问题"),
        "cover card must show the auto-title: {:?}",
        patched.last()
    );
    assert_eq!(
        app.core
            .cover_titles
            .lock()
            .await
            .get("ses_test")
            .map(|c| c.title.clone()),
        Some("修复登录鉴权问题".to_string())
    );
}

#[tokio::test]
async fn long_answer_splits_across_cards_no_plain_text() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let (app, platform) = build_app(cfg, MockBackend::new(long_answer_parts())).await;

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
    // Every message cola sent must be a CARD (ReplyCard / UpdateMessage) —
    // no separate plain-text message for long answers.
    assert!(
        calls.iter().all(|c| {
            matches!(
                c,
                PlatformCall::ReplyCard { .. } | PlatformCall::UpdateMessage { .. }
            )
        }),
        "long answer must stay on cards, got: {:?}",
        calls
    );
    // The FULL text must be present across the cards (not truncated).
    let all_cards: String = calls
        .iter()
        .filter_map(|c| match c {
            PlatformCall::ReplyCard { card, .. } => Some(card.to_string()),
            PlatformCall::UpdateMessage { card, .. } => Some(card.to_string()),
            _ => None,
        })
        .collect();
    assert!(
        all_cards.contains("很长的回答。"),
        "full answer must appear on a card: {}",
        all_cards
    );
    let expected = "很长的回答。".repeat(1200);
    assert!(
        all_cards.chars().filter(|c| *c != '"').count() >= expected.chars().count(),
        "all text must be delivered (preview-only would lose the tail)"
    );
}

#[tokio::test]
async fn short_answer_stays_in_card_no_extra_message() {
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
    assert!(
        calls.iter().all(|c| matches!(
            c,
            PlatformCall::ReplyCard { .. } | PlatformCall::UpdateMessage { .. }
        )),
        "answer must stay on cards: {:?}",
        calls
    );
}
