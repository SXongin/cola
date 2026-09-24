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

/// ADR-0051: the `/dir` card's search op rebuilds the card narrowed to the
/// typed keyword — the submit carries no directory — and echoes the keyword
/// back into the search box.
#[tokio::test]
async fn dir_card_search_narrows_rows_and_echoes_keyword() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let mut backend = MockBackend::new(realistic_parts());
    backend.given_sessions(vec![
        list_session("ses_a", "项目A", "/work/a", 100),
        list_session("ses_b", "项目B", "/work/b", 200),
    ]);
    let (app, _platform) = build_app(cfg, backend).await;

    let value = serde_json::json!({
        "action": "dir",
        "op": "search",
        "chat_id": "chat_1",
        "thread_id": "chat_1",
        "keyword": "work a",
    });
    let result = app
        .host_action(value)
        .await
        .expect("dir search should return a result");
    let card = result.card.expect("search refreshes the card");
    let text = card_text(&card);
    assert!(text.contains("/work/a"), "matching row shown: {text}");
    assert!(!text.contains("/work/b"), "non-matching row hidden: {text}");
    assert!(
        card.to_string().contains("\"default_value\":\"work a\""),
        "keyword echoed into the search box: {}",
        card
    );
}

/// ADR-0052: submitting a search always rebuilds at page 1 — a stale page
/// riding the payload is discarded — while the keyword is kept.
#[tokio::test]
async fn dir_card_search_resets_to_the_first_page() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let (app, _platform) = build_app(cfg, backend_with_dirs(8)).await;

    let search = serde_json::json!({
        "action": "dir",
        "op": "search",
        "chat_id": "chat_1",
        "thread_id": "chat_1",
        "keyword": "proj",
        // A stale page from an older, longer result.
        "page": 3,
    });
    let card = app
        .host_action(search)
        .await
        .expect("dir search should return a result")
        .card
        .expect("search refreshes the card");
    let text = card_text(&card);
    assert!(
        text.contains("/work/proj8") && text.contains("/work/proj3"),
        "page 1 holds the most recent six: {text}"
    );
    assert!(
        !text.contains("/work/proj2") && !text.contains("/work/proj1"),
        "page 2's rows are not on the rebuilt page 1: {text}"
    );
    assert_eq!(
        dir_pager_label(&card).as_deref(),
        Some("第 1/2 页 · 共 8 个"),
        "the search landed on page 1: {text}"
    );
    assert!(
        card.to_string().contains("\"default_value\":\"proj\""),
        "keyword echoed into the search box: {card}"
    );
}

/// ADR-0052: 下一页 rebuilds the next window and keeps the keyword in the
/// search box (acceptance: 第 7 条起).
#[tokio::test]
async fn dir_card_page_flip_shows_the_next_window_and_keeps_the_keyword() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let (app, _platform) = build_app(cfg, backend_with_dirs(8)).await;

    let flip = serde_json::json!({
        "action": "dir",
        "op": "page",
        "chat_id": "chat_1",
        "thread_id": "chat_1",
        "keyword": "proj",
        "page": 2,
    });
    let card = app
        .host_action(flip)
        .await
        .expect("dir page should return a result")
        .card
        .expect("a page flip refreshes the card");
    let text = card_text(&card);
    assert!(
        text.contains("/work/proj2") && text.contains("/work/proj1"),
        "page 2 shows the seventh and eighth directories: {text}"
    );
    assert!(
        !text.contains("/work/proj8") && !text.contains("/work/proj3"),
        "page 1's rows are off page 2: {text}"
    );
    assert_eq!(
        dir_pager_label(&card).as_deref(),
        Some("第 2/2 页 · 共 8 个"),
        "the indicator reports the flipped page: {text}"
    );
    assert!(
        card.to_string().contains("\"default_value\":\"proj\""),
        "the search box echoes the current keyword: {card}"
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
}

/// ADR-0052: an out-of-range page (data shrank under the user) is clamped to
/// the LAST page, not sprung back to the first.
#[tokio::test]
async fn dir_card_out_of_range_page_clamps_to_the_last_page() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let (app, _platform) = build_app(cfg, backend_with_dirs(8)).await;

    let flip = serde_json::json!({
        "action": "dir",
        "op": "page",
        "chat_id": "chat_1",
        "thread_id": "chat_1",
        "keyword": "",
        "page": 99,
    });
    let card = app
        .host_action(flip)
        .await
        .expect("dir page should return a result")
        .card
        .expect("a page flip refreshes the card");
    let text = card_text(&card);
    assert!(
        text.contains("/work/proj2") && text.contains("/work/proj1"),
        "clamped to the last page's window: {text}"
    );
    assert!(!text.contains("/work/proj8"), "not back on page 1: {text}");
    assert_eq!(
        dir_pager_label(&card).as_deref(),
        Some("第 2/2 页 · 共 8 个"),
        "the clamped page is reported: {text}"
    );
}

/// ADR-0052: `pick` from a filtered, paged card keeps the same keyword and
/// page in the rebuilt card (reversing ADR-0051's reset).
#[tokio::test]
async fn dir_card_pick_preserves_the_keyword_and_page() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let (app, _platform) = build_app(cfg, backend_with_dirs(8)).await;

    let pick = serde_json::json!({
        "action": "dir",
        "op": "pick",
        "chat_id": "chat_1",
        "thread_id": "chat_1",
        "directory": "/work/proj2",
        "keyword": "proj",
        "page": 2,
    });
    let result = app
        .host_action(pick)
        .await
        .expect("dir pick should return a result");
    assert!(result.card.is_some(), "dir pick refreshes the card");
    let toast = result.toast.clone().unwrap_or_default();
    assert!(
        toast.contains("下一条消息") && toast.contains("/work/proj2"),
        "dir pick toasts the pending timing and directory: {toast:?}"
    );
    let card = result.card.unwrap();
    let text = card_text(&card);
    assert!(
        text.contains("/work/proj1") && text.contains("/work/proj2"),
        "the same page two window is rebuilt: {text}"
    );
    assert!(
        !text.contains("/work/proj8"),
        "the refresh does not fall back to page 1: {text}"
    );
    assert_eq!(
        dir_pager_label(&card).as_deref(),
        Some("第 2/2 页 · 共 8 个"),
        "the page is preserved: {text}"
    );
    assert!(
        card.to_string().contains("\"default_value\":\"proj\""),
        "the keyword is echoed into the search box: {card}"
    );
}

/// ADR-0052: the 「已在当前目录」 no-op branch rebuilds with the same filter
/// too — a Toast, but not a filter reset.
#[tokio::test]
async fn dir_card_pick_current_directory_preserves_the_filter() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let (app, _platform) = build_app(cfg, backend_with_dirs(8)).await;
    let key = crate::config::ThreadKey::new("chat_1".into(), "chat_1".into());
    seed_entry(
        &app,
        crate::config::SessionEntry {
            thread_key: key,
            session_id: "ses_p2".into(),
            directory: "/work/proj2".into(),
            agent: None,
            model: None,
            auto_accept: false,
            topic_anchor: None,
            topic_root: None,
            variant: None,
        },
    )
    .await;

    let pick = serde_json::json!({
        "action": "dir",
        "op": "pick",
        "chat_id": "chat_1",
        "thread_id": "chat_1",
        "directory": "/work/proj2",
        "keyword": "proj",
        "page": 2,
    });
    let result = app
        .host_action(pick)
        .await
        .expect("dir pick should return a result");
    assert_eq!(
        result.toast.as_deref(),
        Some("已在当前目录"),
        "current dir pick still toasts: {:?}",
        result.toast
    );
    let card = result.card.expect("the no-op branch still refreshes the card");
    let text = card_text(&card);
    assert!(
        text.contains("/work/proj1") && text.contains("/work/proj2"),
        "page 2 survives the no-op: {text}"
    );
    assert_eq!(
        dir_pager_label(&card).as_deref(),
        Some("第 2/2 页 · 共 8 个"),
        "the page is preserved: {text}"
    );
    assert!(
        card.to_string().contains("\"default_value\":\"proj\""),
        "the keyword is echoed into the search box: {card}"
    );
}

/// ADR-0052: 建话题 from a filtered, paged card rebuilds the same window and
/// keyword.
#[tokio::test]
async fn dir_card_topic_preserves_the_keyword_and_page() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let (app, _platform) = build_app(cfg, backend_with_dirs(8)).await;

    let value = serde_json::json!({
        "action": "dir",
        "op": "topic",
        "chat_id": "chat_1",
        "thread_id": "chat_1",
        "directory": "/work/proj2",
        "keyword": "proj",
        "page": 2,
        "open_message_id": "om_dir_card",
    });
    let result = app
        .host_action(value)
        .await
        .expect("dir topic should return a result");
    assert!(
        result.toast.clone().unwrap_or_default().contains("已建话题"),
        "建话题 still opens the topic: {:?}",
        result.toast
    );
    let card = result.card.expect("dir topic refreshes the card");
    let text = card_text(&card);
    assert!(
        text.contains("/work/proj1") && text.contains("/work/proj2"),
        "the same page two window is rebuilt: {text}"
    );
    assert!(
        !text.contains("/work/proj8"),
        "the refresh does not fall back to page 1: {text}"
    );
    assert_eq!(
        dir_pager_label(&card).as_deref(),
        Some("第 2/2 页 · 共 8 个"),
        "the page is preserved: {text}"
    );
    assert!(
        card.to_string().contains("\"default_value\":\"proj\""),
        "the keyword is echoed into the search box: {card}"
    );
}

/// The text `/dir` entry opens on page 1 (ADR-0052), not on some remembered
/// page — the cards are stateless.
#[tokio::test]
async fn dir_command_starts_on_the_first_page() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let (app, platform) = build_app(cfg, backend_with_dirs(8)).await;

    send_command(&app, "/dir", "msg_dir").await;

    let card = platform
        .calls
        .lock()
        .await
        .iter()
        .find_map(|c| match c {
            PlatformCall::ReplyCard { card, .. } => Some(card.clone()),
            _ => None,
        })
        .expect("the /dir command replies with a card");
    let text = card_text(&card);
    assert!(
        text.contains("/work/proj8") && text.contains("/work/proj3"),
        "text /dir opens on the most recent six: {text}"
    );
    assert!(
        !text.contains("/work/proj2") && !text.contains("/work/proj1"),
        "page 2's rows are not on the first page: {text}"
    );
    assert_eq!(
        dir_pager_label(&card).as_deref(),
        Some("第 1/2 页 · 共 8 个"),
        "text /dir reports page 1: {text}"
    );
}

/// `n` recent sessions at `/work/proj1`…`/work/projN`, most recently active
/// last (so `dir_card_data` sorts them descending: projN first).
fn backend_with_dirs(n: i64) -> MockBackend {
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
fn dir_pager_label(card: &serde_json::Value) -> Option<String> {
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
