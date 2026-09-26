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

/// `/sub attach <id>` adopts a direct child: the chat receives exactly one
/// Session Snapshot receipt, the child becomes the Active Session, and the
/// parent stays mapped — `/switch` switches back to it. No prompt is ever sent
/// into the child (spec #344, ADR-0054).
#[tokio::test]
async fn sub_attach_adopts_a_direct_child_and_keeps_the_parent_mapped() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let mut backend = MockBackend::new(realistic_parts());
    backend.given_sessions(vec![
        list_session("ses_root", "根会话", "/work/root", 100),
        child_of("ses_c1", "重写渲染", "/work/root", 300, "ses_root", "build"),
        list_session("ses_other", "别的根", "/work/other", 400),
    ]);
    let prompts = backend.prompt_calls.clone();
    let (app, platform) = build_app(cfg, backend).await;
    seed_entry(
        &app,
        crate::config::SessionEntry::new(key(), "ses_root", "/work/root"),
    )
    .await;

    send_command(&app, "/sub attach ses_c1", "m1").await;

    // Exactly one receipt, and it is the Session Snapshot with the adopt verb.
    let cards = platform.replied_cards().await;
    assert_eq!(cards.len(), 1, "one Session Snapshot receipt: {cards:?}");
    assert!(
        card_text(&cards[0]).contains("已接管 重写渲染"),
        "snapshot header: {}",
        card_text(&cards[0])
    );
    assert!(
        prompts.lock().await.is_empty(),
        "taking over a child never injects a message into it"
    );

    // The child is the Active Session with its own directory.
    let entry = app.sessions.lock().await.get_active(&key()).cloned().unwrap();
    assert_eq!(entry.session_id, "ses_c1");
    assert_eq!(entry.directory, "/work/root");

    // The parent stays mapped alongside the child...
    let mapped: Vec<String> = app
        .sessions
        .lock()
        .await
        .list_thread(&key())
        .into_iter()
        .map(|e| e.session_id.clone())
        .collect();
    assert!(
        mapped.contains(&"ses_c1".to_string()) && mapped.contains(&"ses_root".to_string()),
        "both parent and child stay mapped: {mapped:?}"
    );

    // ...and `/switch` still switches back to it.
    send_command(&app, "/switch ses_root", "m2").await;
    assert_eq!(
        app.sessions.lock().await.get_active(&key()).unwrap().session_id,
        "ses_root",
        "the parent is still a normal switch target"
    );
}

/// The query takes the `/switch` id forms within the child scope: a unique
/// id-prefix (including the bare card hash) and a unique title substring both
/// resolve.
#[tokio::test]
async fn sub_attach_resolves_id_prefix_and_title_like_switch() {
    for query in ["child0001", "ses_child0001abcd", "重写渲染"] {
        let _wd = test_work_dir();
        let dir = tempfile::tempdir().unwrap();
        let cfg = test_config(&dir.path().join("sessions.json"));
        let mut backend = MockBackend::new(realistic_parts());
        backend.given_sessions(vec![
            list_session("ses_root", "根会话", "/work/root", 100),
            child_of(
                "ses_child0001abcd",
                "重写渲染",
                "/work/root",
                300,
                "ses_root",
                "build",
            ),
        ]);
        let (app, _platform) = build_app(cfg, backend).await;
        seed_entry(
            &app,
            crate::config::SessionEntry::new(key(), "ses_root", "/work/root"),
        )
        .await;

        send_command(&app, &format!("/sub attach {query}"), "m1").await;

        assert_eq!(
            app.sessions.lock().await.get_active(&key()).unwrap().session_id,
            "ses_child0001abcd",
            "query {query} adopts the child"
        );
    }
}

/// An ambiguous query lists the candidate children and adopts nothing.
#[tokio::test]
async fn sub_attach_ambiguous_query_lists_candidates() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let mut backend = MockBackend::new(realistic_parts());
    backend.given_sessions(vec![
        list_session("ses_root", "根会话", "/work/root", 100),
        child_of("ses_c1", "任务 A", "/work/root", 300, "ses_root", "build"),
        child_of("ses_c2", "任务 B", "/work/root", 200, "ses_root", "review"),
    ]);
    let (app, platform) = build_app(cfg, backend).await;
    seed_entry(
        &app,
        crate::config::SessionEntry::new(key(), "ses_root", "/work/root"),
    )
    .await;

    send_command(&app, "/sub attach 任务", "m1").await;

    let text = platform.texts().await.join("\n");
    assert!(text.contains("找到多个子会话"), "ambiguity is reported: {text}");
    assert!(
        text.contains("任务 A") && text.contains("任务 B"),
        "both candidates are listed: {text}"
    );
    assert!(
        platform.replied_cards().await.is_empty(),
        "an ambiguous query adopts nothing"
    );
    assert_eq!(
        app.sessions.lock().await.get_active(&key()).unwrap().session_id,
        "ses_root",
        "the Active Session is unchanged"
    );
}

/// An unknown query reports a plain no-match; nothing is adopted.
#[tokio::test]
async fn sub_attach_unknown_query_reports_no_match() {
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

    send_command(&app, "/sub attach 不存在的子会话", "m1").await;

    let text = platform.texts().await.join("\n");
    assert!(text.contains("没有匹配的子会话"), "plain no-match: {text}");
    assert!(platform.replied_cards().await.is_empty(), "nothing adopted");
    assert_eq!(
        app.sessions.lock().await.get_active(&key()).unwrap().session_id,
        "ses_root"
    );
}

/// No Active Session (a fresh chat, or a Pending Session — ADR-0041) reports
/// plainly; there is nothing to attach a child to.
#[tokio::test]
async fn sub_attach_without_an_active_session_reports_plainly() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let mut backend = MockBackend::new(realistic_parts());
    backend.given_sessions(vec![
        list_session("ses_root", "根会话", "/work/root", 100),
        child_of("ses_c1", "重写渲染", "/work/root", 300, "ses_root", "build"),
    ]);
    let (app, platform) = build_app(cfg, backend).await;

    send_command(&app, "/sub attach ses_c1", "m1").await;

    let text = platform.texts().await.join("\n");
    assert!(text.contains("没有活动会话"), "plain no-session report: {text}");
    assert!(
        platform.replied_cards().await.is_empty(),
        "nothing is adopted without an Active Session"
    );
}

/// A query that names a session outside the child scope is refused: neither a
/// sibling root nor a nested descendant (a grandchild) is adoptable here.
#[tokio::test]
async fn sub_attach_refuses_a_session_that_is_not_a_direct_child() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let mut backend = MockBackend::new(realistic_parts());
    backend.given_sessions(vec![
        list_session("ses_root", "根会话", "/work/root", 100),
        child_of("ses_c1", "重写渲染", "/work/root", 300, "ses_root", "build"),
        child_of("ses_nested", "孙会话", "/work/root", 150, "ses_c1", "build"),
        list_session("ses_other", "别的根", "/work/other", 400),
    ]);
    let (app, platform) = build_app(cfg, backend).await;
    seed_entry(
        &app,
        crate::config::SessionEntry::new(key(), "ses_root", "/work/root"),
    )
    .await;

    for query in ["ses_other", "别的根", "ses_nested"] {
        send_command(&app, &format!("/sub attach {query}"), "m1").await;
        let text = platform.texts().await.join("\n");
        assert!(
            text.contains("不是当前会话的直接子会话"),
            "query {query} is refused as out of scope: {text}"
        );
        assert!(
            platform.replied_cards().await.is_empty(),
            "query {query} adopts nothing"
        );
        assert_eq!(
            app.sessions.lock().await.get_active(&key()).unwrap().session_id,
            "ses_root",
            "query {query} leaves the Active Session unchanged"
        );
    }
}

/// The owner check matches `/switch`: a child mapped to another chat is
/// refused with its owner info and a `/sub attach ... --force` pointer, and no
/// mapping is written.
#[tokio::test]
async fn sub_attach_owned_child_is_refused_without_force() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let mut backend = MockBackend::new(realistic_parts());
    backend.given_sessions(vec![
        list_session("ses_root", "根会话", "/work/root", 100),
        child_of("ses_c1", "重写渲染", "/work/root", 300, "ses_root", "build"),
    ]);
    let mut platform = RecordingPlatform::new();
    platform
        .chat_names
        .insert("oc_group_other".into(), "隔壁群".into());
    let platform = Arc::new(platform);
    let app = Arc::new(App::new(cfg, Arc::new(backend), platform.clone()).unwrap());
    seed_entry(
        &app,
        crate::config::SessionEntry::new(key(), "ses_root", "/work/root"),
    )
    .await;
    let other = crate::config::ThreadKey::new("oc_group_other".into(), "oc_group_other".into());
    seed_entry(
        &app,
        crate::config::SessionEntry::new(other.clone(), "ses_c1", "/work/root"),
    )
    .await;

    send_command(&app, "/sub attach ses_c1", "m1").await;

    let text = platform.texts().await.join("\n");
    assert!(text.contains("隔壁群"), "the owner chat is named: {text}");
    assert!(
        text.contains("/sub attach") && text.contains("--force"),
        "the refusal points at the sanctioned force form: {text}"
    );
    assert!(
        platform.replied_cards().await.is_empty(),
        "a refused adoption sends no Session Snapshot"
    );
    assert_eq!(
        app.sessions.lock().await.get_active(&key()).unwrap().session_id,
        "ses_root",
        "the child was not stolen"
    );
    assert_eq!(
        app.sessions.lock().await.get_active(&other).unwrap().session_id,
        "ses_c1",
        "the other chat keeps its mapping"
    );
}

/// `--force` steals a child mapped to another chat: this chat adopts it and the
/// other chat becomes sessionless (the `/switch --force` semantics on the
/// scoped path).
#[tokio::test]
async fn sub_attach_force_steals_the_child_from_another_chat() {
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
    let other = crate::config::ThreadKey::new("oc_group_other".into(), "oc_group_other".into());
    seed_entry(
        &app,
        crate::config::SessionEntry::new(other.clone(), "ses_c1", "/work/root"),
    )
    .await;

    send_command(&app, "/sub attach ses_c1 --force", "m1").await;

    assert_eq!(
        app.sessions.lock().await.get_active(&key()).unwrap().session_id,
        "ses_c1",
        "the child is adopted after the steal"
    );
    assert!(
        app.sessions.lock().await.get_active(&other).is_none(),
        "the old owner becomes sessionless"
    );
    let cards = platform.replied_cards().await;
    assert_eq!(cards.len(), 1, "the steal still ends in one snapshot: {cards:?}");
    assert!(card_text(&cards[0]).contains("已接管 重写渲染"), "{cards:?}");
}

/// Re-running `/sub attach` for the already-active child is idempotent: the
/// reply says so, no Session Snapshot is sent, and the store is not written
/// again.
#[tokio::test]
async fn sub_attach_already_active_child_is_idempotent() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let store_path = dir.path().join("sessions.json");
    let cfg = test_config(&store_path);
    let mut backend = MockBackend::new(realistic_parts());
    backend.given_sessions(vec![
        list_session("ses_root", "根会话", "/work/root", 100),
        child_of("ses_c1", "重写渲染", "/work/root", 300, "ses_root", "build"),
    ]);
    let (app, platform) = build_app(cfg, backend).await;
    // The child was already taken over: it is active, the parent stays mapped.
    seed_entry(
        &app,
        crate::config::SessionEntry::new(key(), "ses_root", "/work/root"),
    )
    .await;
    seed_entry(
        &app,
        crate::config::SessionEntry::new(key(), "ses_c1", "/work/root"),
    )
    .await;
    let before = std::fs::read(&store_path).expect("the seeded store is persisted");

    send_command(&app, "/sub attach ses_c1", "m1").await;

    let text = platform.texts().await.join("\n");
    assert!(text.contains("Already active"), "idempotent reply: {text}");
    assert!(
        platform.replied_cards().await.is_empty(),
        "an already-active child gets no second snapshot"
    );
    let after = std::fs::read(&store_path).expect("the store is still readable");
    assert_eq!(before, after, "no second mapping write");
    assert_eq!(
        app.sessions.lock().await.get_active(&key()).unwrap().session_id,
        "ses_c1"
    );
    let mapped = app.sessions.lock().await.list_thread(&key()).len();
    assert_eq!(mapped, 2, "parent and child stay mapped");
}

/// Taking over a running child succeeds, and the Session Snapshot reflects its
/// live state (运行中) rather than hiding it.
#[tokio::test]
async fn sub_attach_running_child_snapshot_shows_its_live_state() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let mut backend = MockBackend::new(realistic_parts());
    backend.given_sessions(vec![
        list_session("ses_root", "根会话", "/work/root", 100),
        child_of("ses_c1", "重写渲染", "/work/root", 300, "ses_root", "build"),
    ]);
    backend.with_session_status("ses_c1", Some(SessionStatus::Busy));
    let prompts = backend.prompt_calls.clone();
    let (app, platform) = build_app(cfg, backend).await;
    seed_entry(
        &app,
        crate::config::SessionEntry::new(key(), "ses_root", "/work/root"),
    )
    .await;

    send_command(&app, "/sub attach ses_c1", "m1").await;

    let card = first_card(&platform).await;
    let text = card_text(&card);
    assert!(
        text.contains(crate::feishu::snapshot_card::BUSY_CHIP),
        "the snapshot shows the child's live run state: {text}"
    );
    assert_eq!(
        app.sessions.lock().await.get_active(&key()).unwrap().session_id,
        "ses_c1"
    );
    assert!(
        prompts.lock().await.is_empty(),
        "the running child is never messaged"
    );
}
