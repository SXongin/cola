//! `/sub` — the read-only child-session view (spec #344, ticket #346): the
//! Active Session's direct children, scoped, keyword-filtered, paginated, with
//! one live status read per rendered row.

use crate::bridge::test_support::*;
use crate::feishu::card::session::{CHILD_IDLE, CHILD_RUNNING};
use crate::opencode::types::SessionStatus;

fn key() -> crate::config::ThreadKey {
    crate::config::ThreadKey::new("chat_1".into(), "chat_1".into())
}

/// A child session of `parent` with the given title, directory, agent and last
/// activity (`time.updated`).
fn child_of(
    id: &str,
    title: &str,
    directory: &str,
    updated: i64,
    parent: &str,
    agent: &str,
) -> crate::opencode::types::SessionListInfo {
    let mut s = list_session(id, title, directory, updated);
    s.parent_id = Some(parent.to_string());
    s.agent = Some(agent.to_string());
    s
}

/// The first card the command replied with.
async fn first_card(platform: &RecordingPlatform) -> serde_json::Value {
    platform
        .replied_cards()
        .await
        .into_iter()
        .next()
        .expect("the command replies with a card")
}

/// `/sub` lists ONLY the Active Session's direct children: siblings under other
/// roots and nested descendants never appear, and the live state comes from one
/// status read per rendered row.
#[tokio::test]
async fn sub_lists_only_the_active_sessions_direct_children() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let mut backend = MockBackend::new(realistic_parts());
    backend.given_sessions(vec![
        list_session("ses_root", "根会话", "/work/root", 100),
        child_of("ses_c1", "重写渲染", "/work/root", 300, "ses_root", "build"),
        child_of("ses_c2", "跑测试", "/work/root", 200, "ses_root", "review"),
        child_of(
            "ses_other_c",
            "别人的子",
            "/work/other",
            250,
            "ses_other",
            "build",
        ),
        child_of("ses_nested", "孙会话", "/work/root", 150, "ses_c1", "build"),
        list_session("ses_other", "别的根", "/work/other", 400),
    ]);
    backend.with_session_status("ses_c1", Some(SessionStatus::Busy));
    backend.with_session_status("ses_c2", Some(SessionStatus::Retry));
    // Would be the wrong read: this child belongs to another root.
    backend.with_session_status("ses_other_c", Some(SessionStatus::Idle));
    backend.with_session_status("ses_nested", Some(SessionStatus::Busy));
    let reads = backend.session_status_reads.clone();
    let prompts = backend.prompt_calls.clone();
    let (app, platform) = build_app(cfg, backend).await;
    seed_entry(
        &app,
        crate::config::SessionEntry::new(key(), "ses_root", "/work/root"),
    )
    .await;

    send_command(&app, "/sub", "m1").await;
    let card = first_card(&platform).await;
    let text = card_text(&card);
    assert!(
        prompts.lock().await.is_empty(),
        "/sub is a local command, never a prompt to the backend"
    );
    assert!(text.contains("重写渲染"), "first child listed: {text}");
    assert!(text.contains("跑测试"), "second child listed: {text}");
    assert!(
        !text.contains("别人的子") && !text.contains("别的根"),
        "children of other sessions never appear: {text}"
    );
    assert!(
        !text.contains("孙会话"),
        "nested descendants are out of scope: {text}"
    );
    assert!(text.contains(CHILD_RUNNING), "Busy reads 运行中: {text}");
    assert_eq!(
        text.matches(CHILD_RUNNING).count(),
        2,
        "Busy and Retry both read 运行中, one per rendered row: {text}"
    );
    assert!(!text.contains(CHILD_IDLE), "no row is reported idle: {text}");
    // Newest activity first (c1 at 300, c2 at 200), one read each — the other
    // sessions' rows are never read.
    assert_eq!(
        *reads.lock().await,
        vec!["ses_c1".to_string(), "ses_c2".to_string()],
        "one status read per rendered row, in render order"
    );
    // Observation only: the card's buttons are the search submit (and possibly
    // the pager) — no row action, no takeover.
    for btn in card_buttons(&card) {
        assert!(
            matches!(btn["value"]["op"].as_str(), Some("search" | "page")),
            "read-only card, no row buttons: {btn}"
        );
        assert!(btn.get("value").and_then(|v| v.get("session_id")).is_none());
    }
}

/// `/sub list <keyword>` narrows the child list (title/directory/id,
/// token-AND) and echoes the keyword into the search box — only the matching
/// row is read and rendered.
#[tokio::test]
async fn sub_list_filters_by_keyword_and_echoes_it() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let mut backend = MockBackend::new(realistic_parts());
    backend.given_sessions(vec![
        list_session("ses_root", "根会话", "/work/root", 100),
        child_of("ses_c1", "重写渲染", "/work/root", 300, "ses_root", "build"),
        child_of("ses_c2", "跑测试", "/work/root", 200, "ses_root", "review"),
    ]);
    let reads = backend.session_status_reads.clone();
    let (app, platform) = build_app(cfg, backend).await;
    seed_entry(
        &app,
        crate::config::SessionEntry::new(key(), "ses_root", "/work/root"),
    )
    .await;

    send_command(&app, "/sub list 渲染", "m1").await;
    let card = first_card(&platform).await;
    let text = card_text(&card);
    assert!(text.contains("重写渲染"), "matching row: {text}");
    assert!(!text.contains("跑测试"), "non-matching row filtered out: {text}");
    assert!(
        card.to_string().contains("\"default_value\":\"渲染\""),
        "the keyword is echoed into the search box: {card}"
    );
    assert_eq!(
        *reads.lock().await,
        vec!["ses_c1".to_string()],
        "only the rendered row is read"
    );
}

/// ADR-0052: the child card pages six rows at a time, every page flip carries
/// the active keyword and TARGET page, and each build reads exactly the rows it
/// renders — never the whole child list.
#[tokio::test]
async fn sub_pages_and_reads_one_status_per_rendered_row() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let mut backend = MockBackend::new(realistic_parts());
    let mut sessions = vec![list_session("ses_root", "根会话", "/work/root", 100_000)];
    for i in 1..=13 {
        sessions.push(child_of(
            &format!("ses_c{i}"),
            &format!("子{i}"),
            "/work/root",
            10_000 - i,
            "ses_root",
            "build",
        ));
    }
    backend.given_sessions(sessions);
    let reads = backend.session_status_reads.clone();
    let (app, platform) = build_app(cfg, backend).await;
    seed_entry(
        &app,
        crate::config::SessionEntry::new(key(), "ses_root", "/work/root"),
    )
    .await;

    send_command(&app, "/sub", "m1").await;
    let card = first_card(&platform).await;
    let text = card_text(&card);
    for i in 1..=6 {
        assert!(text.contains(&format!("子{i}")), "page 1 shows 子{i}: {text}");
    }
    assert!(!text.contains("子7"), "page 2's rows are off page 1: {text}");
    assert!(
        text.contains("第 1/3 页 · 共 13 个"),
        "the pager reports the position: {text}"
    );
    assert_eq!(
        reads.lock().await.len(),
        6,
        "page 1 reads only its six rendered rows"
    );

    // Flip to page 2 with the filter riding the payload.
    let result = app
        .host_action(serde_json::json!({
            "action": "sub",
            "op": "page",
            "chat_id": "chat_1",
            "thread_id": "chat_1",
            "keyword": "",
            "page": 2,
        }))
        .await
        .expect("page flip returns a result");
    let card = result.card.expect("page flip rebuilds the card");
    let text = card_text(&card);
    for i in 7..=12 {
        assert!(text.contains(&format!("子{i}")), "page 2 shows 子{i}: {text}");
    }
    assert!(
        text.contains("第 2/3 页 · 共 13 个"),
        "the pager follows the flip: {text}"
    );
    let reads = reads.lock().await;
    assert_eq!(reads.len(), 12, "page 2 read six more rows, not the whole list");
    assert_eq!(
        reads[6..].to_vec(),
        (7..=12).map(|i| format!("ses_c{i}")).collect::<Vec<_>>(),
        "page 2 reads exactly its own rows, in render order"
    );
}

/// A search submit rebuilds at page 1 (a stale page is discarded), keeps the
/// typed keyword, and drops the session-list cache so a fresh child shows up.
#[tokio::test]
async fn sub_search_resets_to_the_first_page_and_keeps_the_keyword() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let mut backend = MockBackend::new(realistic_parts());
    let mut sessions = vec![list_session("ses_root", "根会话", "/work/root", 100_000)];
    for i in 1..=13 {
        sessions.push(child_of(
            &format!("ses_c{i}"),
            &format!("子{i}"),
            "/work/root",
            10_000 - i,
            "ses_root",
            "build",
        ));
    }
    backend.given_sessions(sessions);
    let list_calls = backend.list_sessions_calls.clone();
    let (app, _platform) = build_app(cfg, backend).await;
    seed_entry(
        &app,
        crate::config::SessionEntry::new(key(), "ses_root", "/work/root"),
    )
    .await;

    // Open once (a cached fetch), then search: the submit forces a refetch.
    send_command(&app, "/sub", "m1").await;
    assert_eq!(list_calls.load(std::sync::atomic::Ordering::SeqCst), 1);
    let result = app
        .host_action(serde_json::json!({
            "action": "sub",
            "op": "search",
            "chat_id": "chat_1",
            "thread_id": "chat_1",
            "keyword": "子",
            // A stale page from an older, longer result.
            "page": 3,
        }))
        .await
        .expect("search returns a result");
    let card = result.card.expect("search rebuilds the card");
    let text = card_text(&card);
    assert_eq!(
        list_calls.load(std::sync::atomic::Ordering::SeqCst),
        2,
        "a search is an explicit refresh"
    );
    assert!(
        card.to_string().contains("\"default_value\":\"子\""),
        "the keyword rides the rebuild: {card}"
    );
    assert!(
        text.contains("匹配 `子` 的子会话"),
        "the header names the filter: {text}"
    );
    // All 13 match "子"; the stale page 3 is discarded and the search opens on
    // page 1.
    assert!(
        text.contains("第 1/3 页 · 共 13 个"),
        "the search landed on page 1: {text}"
    );
    assert!(
        text.contains("子1") && !text.contains("子7"),
        "page 1's window is rendered: {text}"
    );
}

/// A fresh chat has no Active Session: the card still opens, as the plain empty
/// state, and no status is read.
#[tokio::test]
async fn sub_without_an_active_session_opens_empty() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let mut backend = MockBackend::new(realistic_parts());
    backend.given_sessions(vec![
        list_session("ses_root", "根会话", "/work/root", 100),
        child_of("ses_c1", "别人的子", "/work/root", 300, "ses_root", "build"),
    ]);
    let reads = backend.session_status_reads.clone();
    let (app, platform) = build_app(cfg, backend).await;

    send_command(&app, "/sub", "m1").await;
    let card = first_card(&platform).await;
    let text = card_text(&card);
    assert!(text.contains("没有子会话"), "plain empty state: {text}");
    assert!(
        !text.contains("别人的子"),
        "nothing is listed without an Active Session: {text}"
    );
    assert!(
        reads.lock().await.is_empty(),
        "no Active Session means no status read"
    );
}

/// A Pending Session supersedes the mapping (ADR-0041): the conversation has
/// no Active Session, so `/sub` opens the same plain empty state.
#[tokio::test]
async fn sub_with_a_pending_session_opens_empty() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let mut backend = MockBackend::new(realistic_parts());
    backend.given_sessions(vec![
        list_session("ses_root", "根会话", "/work/root", 100),
        child_of("ses_c1", "子一", "/work/root", 300, "ses_root", "build"),
    ]);
    let reads = backend.session_status_reads.clone();
    let (app, platform) = build_app(cfg, backend).await;
    seed_entry(
        &app,
        crate::config::SessionEntry::new(key(), "ses_root", "/work/root"),
    )
    .await;
    seed_pending(
        &app,
        crate::bridge::session::PendingEntry::new(key(), "/work/pending"),
    )
    .await;

    send_command(&app, "/sub", "m1").await;
    let card = first_card(&platform).await;
    let text = card_text(&card);
    assert!(text.contains("没有子会话"), "plain empty state: {text}");
    assert!(
        !text.contains("子一"),
        "the superseded session's children hide: {text}"
    );
    assert!(reads.lock().await.is_empty(), "no status read while pending");
}

/// An empty search is distinguishable from an empty child list: the card says
/// "无匹配子会话" instead of "没有子会话".
#[tokio::test]
async fn sub_empty_search_is_distinct_from_an_empty_list() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let mut backend = MockBackend::new(realistic_parts());
    backend.given_sessions(vec![
        list_session("ses_root", "根会话", "/work/root", 100),
        child_of("ses_c1", "重写渲染", "/work/root", 300, "ses_root", "build"),
    ]);
    let (app, platform) = build_app(cfg, backend).await;
    seed_entry(
        &app,
        crate::config::SessionEntry::new(key(), "ses_root", "/work/root"),
    )
    .await;

    send_command(&app, "/sub list 不存在", "m1").await;
    let card = first_card(&platform).await;
    let text = card_text(&card);
    assert!(text.contains("无匹配子会话"), "empty search line: {text}");
    assert!(
        !text.contains("没有子会话"),
        "an empty search is not an empty list: {text}"
    );
}

/// A failed status read drops the row's state label, never the row (and never
/// guesses 空闲) — the same card renders either way.
#[tokio::test]
async fn sub_status_read_failure_keeps_the_rows_without_a_state() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let mut backend = MockBackend::new(realistic_parts());
    backend.given_sessions(vec![
        list_session("ses_root", "根会话", "/work/root", 100),
        child_of("ses_c1", "重写渲染", "/work/root", 300, "ses_root", "build"),
    ]);
    backend.status_read_fails("status endpoint down");
    let (app, platform) = build_app(cfg, backend).await;
    seed_entry(
        &app,
        crate::config::SessionEntry::new(key(), "ses_root", "/work/root"),
    )
    .await;

    send_command(&app, "/sub", "m1").await;
    let card = first_card(&platform).await;
    let text = card_text(&card);
    assert!(text.contains("重写渲染"), "the row still renders: {text}");
    assert!(
        !text.contains(CHILD_RUNNING) && !text.contains(CHILD_IDLE),
        "a failed read is never guessed: {text}"
    );
    assert!(
        !text.contains("没有子会话"),
        "a status failure is not an empty list: {text}"
    );
}
