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

// ---- ADR-0078: the /dir browse card ----

/// A project directory with `alpha/`, `beta/`, `.hidden/` and a `repo/` that
/// holds a `.git/` entry (a repo, floated first and 📦-marked).
fn browse_project() -> tempfile::TempDir {
    let proj = tempfile::tempdir().unwrap();
    for name in ["alpha", "beta", ".hidden"] {
        std::fs::create_dir(proj.path().join(name)).unwrap();
    }
    std::fs::create_dir_all(proj.path().join("repo").join(".git")).unwrap();
    proj
}

/// A seeded active SessionEntry rooted at `directory`.
fn seeded_entry(key: &crate::config::ThreadKey, directory: &str) -> crate::config::SessionEntry {
    crate::config::SessionEntry {
        thread_key: key.clone(),
        session_id: "ses_seed".into(),
        directory: directory.to_string(),
        agent: None,
        model: None,
        auto_accept: false,
        topic_anchor: None,
        topic_root: None,
        variant: None,
    }
}

fn browse_action(op: &str, dir: &str) -> serde_json::Value {
    serde_json::json!({
        "action": "dir",
        "view": "browse",
        "op": op,
        "chat_id": "chat_1",
        "thread_id": "chat_1",
        "dir": dir,
    })
}

/// The Recent Directories card's 浏览目录 button (op `browse`) opens the browse
/// card in place (ADR-0078).
#[tokio::test]
async fn dir_card_browse_op_opens_the_browse_card() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let (app, _platform) = build_app(cfg, MockBackend::new(realistic_parts())).await;

    let result = app
        .host_action(serde_json::json!({
            "action": "dir",
            "op": "browse",
            "chat_id": "chat_1",
            "thread_id": "chat_1",
        }))
        .await
        .expect("browse op returns a result");
    let card = result.card.expect("a browse card");
    let text = card_text(&card);
    assert!(text.contains("浏览目录"), "browse header: {text}");
    assert!(text.contains("← 最近目录"), "back control: {text}");
    assert!(
        card.to_string().contains("\"name\":\"dir_browse\""),
        "browse form"
    );
}

/// The browse card lists a directory's children: directories only, dot
/// directories hidden by default, and repos floated to the front (ADR-0078).
#[tokio::test]
async fn dir_browse_lists_children_repo_first_and_hides_dot_dirs() {
    let _wd = test_work_dir();
    let proj = browse_project();
    let proj_dir = proj.path().to_string_lossy().to_string();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let (app, _platform) = build_app(cfg, MockBackend::new(realistic_parts())).await;
    let key = crate::config::ThreadKey::new("chat_1".into(), "chat_1".into());
    seed_entry(&app, seeded_entry(&key, &proj_dir)).await;

    let result = app.host_action(browse_action("go", &proj_dir)).await.expect("go");
    let card = result.card.expect("a card");
    let text = card_text(&card);

    assert!(text.contains("📦 repo"), "repo listed + marked: {text}");
    assert!(
        text.contains("📁 alpha") && text.contains("📁 beta"),
        "dirs listed: {text}"
    );
    assert!(!text.contains(".hidden"), "dot dir hidden by default: {text}");
    let repo_at = text.find("📦 repo").expect("repo row");
    let alpha_at = text.find("📁 alpha").expect("alpha row");
    assert!(repo_at < alpha_at, "repo floats first: {text}");
}

/// The hidden toggle shows dot directories; the filter narrows the children
/// (ADR-0078, the stateless payload carrying both).
#[tokio::test]
async fn dir_browse_hidden_toggle_and_filter_narrow_the_children() {
    let _wd = test_work_dir();
    let proj = browse_project();
    let proj_dir = proj.path().to_string_lossy().to_string();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let (app, _platform) = build_app(cfg, MockBackend::new(realistic_parts())).await;
    let key = crate::config::ThreadKey::new("chat_1".into(), "chat_1".into());
    seed_entry(&app, seeded_entry(&key, &proj_dir)).await;

    let mut hidden_op = browse_action("show_hidden", &proj_dir);
    hidden_op["show_hidden"] = serde_json::json!(false);
    let card = app.host_action(hidden_op).await.unwrap().card.expect("card");
    assert!(
        card_text(&card).contains("📁 .hidden"),
        "hidden toggle reveals dot dirs: {}",
        card_text(&card)
    );

    let mut filter_op = browse_action("filter", &proj_dir);
    filter_op["keyword"] = serde_json::json!("alph");
    let card = app.host_action(filter_op).await.unwrap().card.expect("card");
    let text = card_text(&card);
    assert!(text.contains("📁 alpha"), "filter keeps the match: {text}");
    assert!(!text.contains("📁 beta"), "filter drops the rest: {text}");
}

/// 选择 on the browse card declares a Pending Session in the target (the card
/// form of `/dir <path>`), creating no server session (ADR-0041/ADR-0078).
#[tokio::test]
async fn dir_browse_pick_declares_a_pending_in_the_child() {
    let _wd = test_work_dir();
    let proj = browse_project();
    let proj_dir = proj.path().to_string_lossy().to_string();
    let child = proj.path().join("alpha");
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let created = MockBackend::new(realistic_parts());
    let created_dirs = created.created_session_dirs.clone();
    let (app, _platform) = build_app(cfg, created).await;
    let key = crate::config::ThreadKey::new("chat_1".into(), "chat_1".into());
    seed_entry(&app, seeded_entry(&key, &proj_dir)).await;

    let result = app
        .host_action(browse_action("pick", &child.to_string_lossy()))
        .await
        .expect("pick");
    assert!(result.card.is_some(), "pick refreshes the browse card");
    let toast = result.toast.unwrap_or_default();
    assert!(toast.contains("下一条消息"), "pick toasts the pending: {toast}");
    let canonical = std::fs::canonicalize(&child).unwrap();
    assert_eq!(
        app.sessions
            .lock()
            .await
            .pending_for(&key)
            .map(|p| p.directory.clone()),
        Some(canonical.to_string_lossy().to_string()),
        "pending rooted at the canonical child"
    );
    assert!(created_dirs.lock().await.is_empty(), "no server session created");
}

/// The 前往 submit navigates to the typed path; an invalid one clamps to the
/// nearest existing ancestor with a notice (ADR-0078).
#[tokio::test]
async fn dir_browse_go_navigates_and_clamps_a_vanished_path() {
    let _wd = test_work_dir();
    let proj = browse_project();
    let proj_dir = proj.path().to_string_lossy().to_string();
    let child = proj.path().join("alpha");
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let (app, _platform) = build_app(cfg, MockBackend::new(realistic_parts())).await;
    let key = crate::config::ThreadKey::new("chat_1".into(), "chat_1".into());
    seed_entry(&app, seeded_entry(&key, &proj_dir)).await;

    let mut nav = browse_action("go", "");
    nav["path"] = serde_json::json!(child.to_string_lossy());
    let card = app.host_action(nav).await.unwrap().card.expect("card");
    let text = card_text(&card);
    assert!(text.contains("alpha"), "navigated to the typed path: {text}");
    assert!(text.contains("没有子目录"), "alpha is empty: {text}");

    let ghost = proj.path().join("ghost");
    let card = app
        .host_action(browse_action("go", &ghost.to_string_lossy()))
        .await
        .unwrap()
        .card
        .expect("card");
    let text = card_text(&card);
    assert!(text.contains("不存在"), "clamp notice: {text}");
    assert!(text.contains("📁 alpha"), "clamped back into the parent: {text}");
}

/// ← 最近目录 swaps the browse card back for the Recent Directories card in
/// place (ADR-0078).
#[tokio::test]
async fn dir_browse_recent_swaps_back_in_place() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let (app, _platform) = build_app(cfg, MockBackend::new(realistic_parts())).await;

    // The real ← 最近目录 button carries no `view`, so it lands on the recent
    // handler's `recent` op (the browse card's own view routing would send a
    // view-carrying payload to the browse handler instead).
    let card = app
        .host_action(serde_json::json!({
            "action": "dir",
            "op": "recent",
            "chat_id": "chat_1",
            "thread_id": "chat_1",
        }))
        .await
        .unwrap()
        .card
        .expect("card");
    let text = card_text(&card);
    assert!(text.contains("最近目录"), "the recent card: {text}");
    assert!(
        !text.contains("← 最近目录"),
        "no browse back control on the recent card"
    );
    assert!(
        card.to_string().contains("\"op\":\"browse\""),
        "the browse entry is back"
    );
}

/// 建话题 on the browse card reaches the shared topic helper — without a card
/// message reference it degrades to the guidance toast (the same contract as
/// the Recent Directories card).
#[tokio::test]
async fn dir_browse_topic_without_message_reference_degrades() {
    let _wd = test_work_dir();
    let proj = browse_project();
    let proj_dir = proj.path().to_string_lossy().to_string();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let (app, _platform) = build_app(cfg, MockBackend::new(realistic_parts())).await;
    let key = crate::config::ThreadKey::new("chat_1".into(), "chat_1".into());
    seed_entry(&app, seeded_entry(&key, &proj_dir)).await;

    let result = app
        .host_action(browse_action("topic", &proj_dir))
        .await
        .expect("topic");
    assert!(
        result.toast.unwrap_or_default().contains("无法创建话题"),
        "missing message reference degrades"
    );
}

/// 选择 uses the same directory-equality rule as the card's ✅ 当前 mark, so a
/// target spelled differently from the conversation's directory (here a
/// trailing `/.`) is recognised as current and never re-declared (ADR-0078).
#[tokio::test]
async fn dir_browse_pick_recognises_an_equivalent_current_path() {
    let _wd = test_work_dir();
    let proj = tempfile::tempdir().unwrap();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let (app, _platform) = build_app(cfg, MockBackend::new(realistic_parts())).await;
    let key = crate::config::ThreadKey::new("chat_1".into(), "chat_1".into());
    // The store holds the canonical directory; the click names the same
    // directory with a trailing `.`. Append it to the RAW path, not the
    // canonical one: on Windows the canonical form carries a `\\?\` verbatim
    // prefix, which does NOT resolve `.` (the path would not be a directory).
    let canonical = std::fs::canonicalize(proj.path()).unwrap();
    seed_entry(&app, seeded_entry(&key, &canonical.to_string_lossy())).await;
    let equivalent = proj.path().join(".");

    let result = app
        .host_action(browse_action("pick", &equivalent.to_string_lossy()))
        .await
        .expect("pick");
    assert_eq!(
        result.toast.as_deref(),
        Some("已在当前目录"),
        "an equivalent spelling is recognised as current"
    );
    assert!(
        app.sessions.lock().await.pending_for(&key).is_none(),
        "no pending declared for the already-current directory"
    );
}

/// The browse card's pager routes: replaying a 下一页 button's payload flips the
/// page (it must reach the browse handler, not answer 无效操作) — and the flag
/// it carries as a string is read back (ADR-0078).
#[tokio::test]
async fn dir_browse_pager_flips_the_page() {
    let _wd = test_work_dir();
    let proj = tempfile::tempdir().unwrap();
    for i in 1..=13 {
        std::fs::create_dir(proj.path().join(format!("p{i:02}"))).unwrap();
    }
    let proj_dir = proj.path().to_string_lossy().to_string();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let (app, _platform) = build_app(cfg, MockBackend::new(realistic_parts())).await;
    let key = crate::config::ThreadKey::new("chat_1".into(), "chat_1".into());
    seed_entry(&app, seeded_entry(&key, &proj_dir)).await;

    let card = app
        .host_action(browse_action("go", &proj_dir))
        .await
        .unwrap()
        .card
        .expect("card");
    assert_eq!(
        dir_pager_label(&card).as_deref(),
        Some("第 1/3 页 · 共 13 个"),
        "page 1 of 3: {}",
        card_text(&card)
    );

    let next = pager_button(&card, "下一页")["value"].clone();
    let card = app.host_action(next).await.unwrap().card.expect("card");
    assert_eq!(
        dir_pager_label(&card).as_deref(),
        Some("第 2/3 页 · 共 13 个"),
        "the pager click flipped the page: {}",
        card_text(&card)
    );
}

/// The pager carries `show_hidden` as a STRING (`"true"`) because the shared
/// pager only holds string callback fields; replaying it must read back as true,
/// so a dot-directory child stays visible (#670 review).
#[tokio::test]
async fn dir_browse_pager_reads_the_string_flag_back() {
    let _wd = test_work_dir();
    let proj = tempfile::tempdir().unwrap();
    for i in 1..=13 {
        std::fs::create_dir(proj.path().join(format!("p{i:02}"))).unwrap();
    }
    std::fs::create_dir(proj.path().join(".dotdir")).unwrap();
    let proj_dir = proj.path().to_string_lossy().to_string();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let (app, _platform) = build_app(cfg, MockBackend::new(realistic_parts())).await;
    let key = crate::config::ThreadKey::new("chat_1".into(), "chat_1".into());
    seed_entry(&app, seeded_entry(&key, &proj_dir)).await;

    // Page 2 with the toggle on: the pager's 上一页 carries the flag as a string.
    let card = app
        .host_action(serde_json::json!({
            "action": "dir",
            "view": "browse",
            "op": "page",
            "chat_id": "chat_1",
            "thread_id": "chat_1",
            "dir": proj_dir,
            "show_hidden": true,
            "keyword": "",
            "page": 2,
        }))
        .await
        .unwrap()
        .card
        .expect("card");
    let prev = pager_button(&card, "上一页")["value"].clone();
    assert_eq!(prev["show_hidden"], "true", "the pager flags it as a string");

    let card = app.host_action(prev).await.unwrap().card.expect("card");
    assert!(
        card_text(&card).contains(".dotdir"),
        "the string flag was read back as true: {}",
        card_text(&card)
    );
}

/// 选择 on a regular FILE toasts and declares nothing — only directories can be
/// selected (ADR-0078), guarding the non-directory branch (#670 review).
#[tokio::test]
async fn dir_browse_pick_on_a_file_refuses() {
    let _wd = test_work_dir();
    let proj = tempfile::tempdir().unwrap();
    let file = proj.path().join("notes.txt");
    std::fs::write(&file, "x").unwrap();
    let proj_dir = proj.path().to_string_lossy().to_string();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let (app, _platform) = build_app(cfg, MockBackend::new(realistic_parts())).await;
    let key = crate::config::ThreadKey::new("chat_1".into(), "chat_1".into());
    seed_entry(&app, seeded_entry(&key, &proj_dir)).await;

    let result = app
        .host_action(browse_action("pick", &file.to_string_lossy()))
        .await
        .expect("pick");
    assert!(
        result.toast.unwrap_or_default().contains("目录不存在"),
        "a file pick is refused with a toast"
    );
    assert!(
        app.sessions.lock().await.pending_for(&key).is_none(),
        "no Pending Session from a file pick"
    );
}

/// A directory larger than `BROWSE_SCAN_CAP` is scanned only up to the cap and
/// says so — the render cannot outrun the card callback budget (#670 review).
#[tokio::test]
async fn dir_browse_caps_a_huge_directory_scan() {
    let _wd = test_work_dir();
    let proj = tempfile::tempdir().unwrap();
    let cap = crate::bridge::handler::BROWSE_SCAN_CAP;
    for i in 0..=cap {
        std::fs::create_dir(proj.path().join(format!("d{i:05}"))).unwrap();
    }
    let proj_dir = proj.path().to_string_lossy().to_string();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let (app, _platform) = build_app(cfg, MockBackend::new(realistic_parts())).await;
    let key = crate::config::ThreadKey::new("chat_1".into(), "chat_1".into());
    seed_entry(&app, seeded_entry(&key, &proj_dir)).await;

    let card = app
        .host_action(browse_action("go", &proj_dir))
        .await
        .unwrap()
        .card
        .expect("card");
    let text = card_text(&card);
    assert!(text.contains("仅扫描了前"), "truncation is announced: {text}");
    assert!(
        text.contains(&format!("共 {cap} 个")),
        "exactly the cap was scanned, not the extra entry: {text}"
    );
}
