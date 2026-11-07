use crate::backend::{Backend, FinishReason, MessageRole, Part, SessionTranscript, StepFinish};
use crate::bridge::test_support::*;

/// Repeated session-list reads through the `/switch` card within the 30 s
/// TTL must not re-hit the server; an external rename is only visible after
/// invalidation/expiry.
#[tokio::test]
async fn switch_card_is_cached_within_ttl_and_invalidated_on_rename() {
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
            client_id: crate::config::DEFAULT_CLIENT_ID.into(),
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

    // Two `/switch` cards in a row → one server fetch.
    send_command_in(
        &app,
        "/switch",
        key.clone(),
        "m1",
        crate::config::ConversationKind::P2p,
    )
    .await;
    send_command_in(
        &app,
        "/switch",
        key.clone(),
        "m2",
        crate::config::ConversationKind::P2p,
    )
    .await;
    assert_eq!(calls_counter.load(std::sync::atomic::Ordering::SeqCst), 1);

    // A rename invalidates the cache → the next card refetches.
    send_command_in(
        &app,
        "/name 新名字",
        key.clone(),
        "m3",
        crate::config::ConversationKind::P2p,
    )
    .await;
    send_command_in(&app, "/switch", key, "m4", crate::config::ConversationKind::P2p).await;
    assert_eq!(calls_counter.load(std::sync::atomic::Ordering::SeqCst), 2);
}

/// The retired text list gets no dedicated reply: `/switch list` is an
/// ordinary keyword query, so it takes the normal no-match path — the switch
/// card, pre-filtered by the keyword — never a list-shaped text response.
#[tokio::test]
async fn switch_list_is_an_ordinary_keyword_with_no_dedicated_reply() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let mut backend = MockBackend::new(realistic_parts());
    backend.given_sessions(vec![list_session("ses_alpha01", "重写登录", "/work/auth", 100)]);
    let (app, platform) = build_app(cfg, backend).await;

    send_command(&app, "/switch list", "msg_list").await;

    let card = platform
        .replied_cards()
        .await
        .into_iter()
        .next()
        .expect("no match opens the switch card");
    let text = card_text(&card);
    assert!(text.contains("会话管理"), "the switch card is the reply: {text}");
    assert!(
        text.contains("无匹配会话"),
        "the card carries the ordinary empty result: {text}"
    );
    assert!(
        card.to_string().contains("\"default_value\":\"list\""),
        "the card is pre-filtered by the keyword: {card}"
    );
    let replies = platform.texts().await;
    assert!(replies.is_empty(), "no dedicated text reply: {replies:?}");
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
            client_id: crate::config::DEFAULT_CLIENT_ID.into(),
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
            client_id: crate::config::DEFAULT_CLIENT_ID.into(),
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
            client_id: crate::config::DEFAULT_CLIENT_ID.into(),
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

/// ADR-0100: a lobby text adopt confirms in EXACTLY ONE text line carrying the
/// status summary — it posts NO card, so a Session keeps its one Session Card
/// for `/card`. The line is a single line even when the chip would render a
/// follow hint on a card.
#[tokio::test]
async fn switch_lobby_adopt_confirms_in_one_text_line() {
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
    assert!(
        !calls.iter().any(|c| matches!(c, PlatformCall::ReplyCard { .. })),
        "an adoption posts no card: {calls:?}"
    );
    let texts: Vec<_> = calls
        .iter()
        .filter_map(|c| match c {
            PlatformCall::ReplyText { text, .. } => Some(text.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(texts.len(), 1, "exactly one text confirmation: {calls:?}");
    let line = &texts[0];
    assert!(
        line.contains("已接管「唯一外部标题」"),
        "the line names the takeover and title: {line}"
    );
    assert!(
        line.contains(crate::feishu::card::session::IDLE_CHIP),
        "the line carries the status summary: {line}"
    );
    assert_eq!(line.lines().count(), 1, "the confirmation is ONE line: {line}");
    let key = crate::config::ThreadKey::new("chat_1".into(), "chat_1".into());
    let entry = app.sessions.lock().await.get_active(&key).cloned().unwrap();
    assert_eq!(entry.session_id, "ses_alpha01");
    assert_eq!(entry.topic_anchor, None, "lobby adopts carry no topic anchor");
}

/// ADR-0100: a re-`/switch` to an already-mapped Session with nothing to
/// report still answers ONE text line carrying the status summary — the
/// suppression predicate and its card are gone, but the takeover never goes
/// silent.
#[tokio::test]
async fn switch_reeswitch_with_nothing_to_report_answers_one_line() {
    let _wd = test_work_dir();
    let mut backend = MockBackend::new(realistic_parts());
    // Idle, no pending, newest user message cola-authored — the read has no
    // content to report beyond its status.
    backend.cola_message("ses_own1", "上次的问题");
    let (app, platform, _dir) = build_reeswitch_app(backend).await;

    send_command(&app, "/switch 本项目", "msg_switch").await;

    let calls = platform.calls.lock().await.clone();
    assert!(
        !calls.iter().any(|c| matches!(c, PlatformCall::ReplyCard { .. })),
        "a re-switch posts no card: {calls:?}"
    );
    let texts = platform.texts().await;
    assert_eq!(texts.len(), 1, "exactly one line: {texts:?}");
    assert!(
        texts[0].contains("已切换「本项目会话」")
            && texts[0].contains(crate::feishu::card::session::IDLE_CHIP),
        "the line names the switch and its status: {texts:?}"
    );
    assert_eq!(texts[0].lines().count(), 1, "one line only: {:?}", texts[0]);
}

/// ADR-0100: a re-switch whose newest user message is EXTERNAL still confirms
/// in ONE text line and posts NO card — the external message is no longer
/// surfaced by the takeover (a Session Card read is how you see it).
#[tokio::test]
async fn switch_reeswitch_does_not_post_a_card_for_external_newness() {
    let _wd = test_work_dir();
    let mut backend = MockBackend::new(realistic_parts());
    backend.external_message_for("ses_own1", "OpenChamber 里的问题");
    let (app, platform, _dir) = build_reeswitch_app(backend).await;

    send_command(&app, "/switch 本项目", "msg_switch").await;

    let calls = platform.calls.lock().await.clone();
    assert!(
        !calls.iter().any(|c| matches!(c, PlatformCall::ReplyCard { .. })),
        "no snapshot card on a re-switch: {calls:?}"
    );
    let texts = platform.texts().await;
    assert_eq!(texts.len(), 1, "one line: {texts:?}");
    assert!(
        texts[0].contains("已切换「本项目会话」"),
        "re-switch verb: {texts:?}"
    );
    assert_eq!(texts[0].lines().count(), 1, "one line only: {:?}", texts[0]);
}

/// ADR-0100: the one-line confirmation carries the 运行中 status (and only the
/// chip — the card's follow hint does not fit a single line). No card.
#[tokio::test]
async fn switch_reeswitch_line_carries_the_busy_status() {
    let _wd = test_work_dir();
    let mut backend = MockBackend::new(realistic_parts());
    backend.with_session_status("ses_own1", Some(crate::backend::types::SessionStatus::Busy));
    // Cola-authored newest alone is NOT enough to hide the run state.
    backend.cola_message("ses_own1", "上次的问题");
    let (app, platform, _dir) = build_reeswitch_app(backend).await;

    send_command(&app, "/switch 本项目", "msg_switch").await;

    let calls = platform.calls.lock().await.clone();
    assert!(
        !calls.iter().any(|c| matches!(c, PlatformCall::ReplyCard { .. })),
        "no card: {calls:?}"
    );
    let texts = platform.texts().await;
    let line = texts.first().expect("the re-switch answers a line");
    assert!(
        line.contains(crate::feishu::card::session::BUSY_CHIP),
        "busy status: {line}"
    );
    assert!(
        !line.contains(crate::feishu::card::session::FOLLOW_HINT),
        "the card-only follow hint stays off the one-line form: {line}"
    );
}

/// ADR-0100: the one-line confirmation carries the 需要重试 status.
#[tokio::test]
async fn switch_reeswitch_line_carries_the_retry_status() {
    let _wd = test_work_dir();
    let mut backend = MockBackend::new(realistic_parts());
    backend.with_session_status("ses_own1", Some(crate::backend::types::SessionStatus::Retry));
    let (app, platform, _dir) = build_reeswitch_app(backend).await;

    send_command(&app, "/switch 本项目", "msg_switch").await;

    let texts = platform.texts().await;
    let line = texts.first().expect("the re-switch answers a line");
    assert!(
        line.contains(crate::feishu::card::session::RETRY_CHIP),
        "retry status: {line}"
    );
    assert_eq!(line.lines().count(), 1, "one line only: {line}");
}

/// ADR-0100: a Session in a Waiting turn (server-idle with a live Background
/// Task, ADR-0059) confirms with 等待后台任务 on the one line — the card's
/// follow hint is a second line and stays off this form.
#[tokio::test]
async fn switch_reeswitch_line_carries_the_background_wait() {
    let _wd = test_work_dir();
    let mut backend = MockBackend::new(realistic_parts());
    backend.given_transcript(
        "ses_own1",
        vec![
            SessionTranscript::new(vec![
                typed_message(
                    "msg_u1",
                    MessageRole::User,
                    Some(1_000),
                    vec![text_part("跑一下 CI")],
                ),
                typed_message(
                    "msg_a1",
                    MessageRole::Assistant,
                    Some(2_000),
                    vec![Part::StepFinish(StepFinish {
                        reason: FinishReason::Stop,
                    })],
                ),
            ])
            .with_executions(vec![execution(2_500)])
            .with_background_tasks(vec![background_shell(2_100)]),
        ],
    );
    let (app, platform, _dir) = build_reeswitch_app(backend).await;

    send_command(&app, "/switch 本项目", "msg_switch").await;

    let texts = platform.texts().await;
    let line = texts.first().expect("the re-switch answers a line");
    assert!(
        line.contains(crate::feishu::card::session::WAITING_BACKGROUND_CHIP),
        "the wait is named: {line}"
    );
    assert!(
        !line.contains(crate::feishu::card::session::FOLLOW_HINT),
        "the card-only follow hint stays off the one-line form: {line}"
    );
    assert_eq!(line.lines().count(), 1, "one line only: {line}");
}
/// ADR-0100: an adopt-time pending request makes the line say 等待你的确认
/// (the Host's action outranks the run state) — and the adoption posts no
/// card; the request keeps its own card through the ordinary sweep.
#[tokio::test]
async fn switch_reeswitch_line_carries_the_pending_status() {
    let _wd = test_work_dir();
    let mut backend = MockBackend::new(realistic_parts());
    backend.cola_message("ses_own1", "上次的问题");
    backend.ask_permissions(vec![crate::backend::types::PermissionRequest {
        request_id: "req_own".into(),
        session_id: Some("ses_own1".into()),
        permission: Some("bash".into()),
        patterns: vec!["ls".into()],
        metadata: None,
        always: vec![],
        always_supported: true,
    }]);
    let (app, platform, _dir) = build_reeswitch_app(backend).await;

    send_command(&app, "/switch 本项目", "msg_switch").await;

    let calls = platform.calls.lock().await.clone();
    assert!(
        !calls.iter().any(|c| matches!(c, PlatformCall::ReplyCard { .. })),
        "no card on a pending re-switch: {calls:?}"
    );
    let texts = platform.texts().await;
    let line = texts.first().expect("the re-switch answers a line");
    assert!(
        line.contains(crate::feishu::card::session::WAITING_CHIP),
        "waiting status: {line}"
    );
    assert_eq!(line.lines().count(), 1, "one line only: {line}");
}

/// ADR-0100: a pending already hosted by a standalone card still makes the
/// one-line confirmation say 等待你的确认 — accurate, with no elsewhere pointer
/// (「见原卡片」 is a card-only nuance and the claim registry is retiring), and
/// the adoption posts no card, so the block is never duplicated.
#[tokio::test]
async fn switch_reeswitch_line_reports_a_pending_already_on_its_own_card() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let mut backend = MockBackend::new(realistic_parts());
    // Busy proves the pending outranks the server run state.
    backend.with_session_status("ses_own1", Some(crate::backend::types::SessionStatus::Busy));
    backend.cola_message("ses_own1", "上次的问题");
    backend.ask_permissions(vec![crate::backend::types::PermissionRequest {
        request_id: "req_own".into(),
        session_id: Some("ses_own1".into()),
        permission: Some("bash".into()),
        patterns: vec!["ls".into()],
        metadata: None,
        always: vec![],
        always_supported: true,
    }]);
    backend.given_sessions(vec![list_session("ses_own1", "本项目会话", "/work/cola", 500)]);
    let (app, platform) = build_app(cfg, backend).await;
    seed_entry(
        &app,
        crate::config::SessionEntry {
            client_id: crate::config::DEFAULT_CLIENT_ID.into(),
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

    // The request is already on its standalone card (same-directory polling)
    // when the re-switch happens.
    let mut seen = std::collections::HashSet::new();
    app.permission.sweep(&app.flow_handles(), &mut seen).await;
    assert!(
        app.permission.sent_cards.lock().await.contains_key("req_own"),
        "precondition: the permission has a standalone card"
    );
    let cards_before = platform.replied_cards().await.len();

    send_command(&app, "/switch 本项目", "msg_switch").await;

    let texts = platform.texts().await;
    let line = texts.first().expect("the re-switch answers a line");
    assert!(
        line.contains(crate::feishu::card::session::WAITING_CHIP),
        "the Host's pending outranks the run state: {line}"
    );
    assert!(
        !line.contains(crate::feishu::card::session::BUSY_CHIP),
        "the server run state is not shown: {line}"
    );
    assert!(
        !line.contains(crate::feishu::card::session::FOLLOW_HINT),
        "no card-only hint on the line: {line}"
    );
    assert_eq!(
        platform.replied_cards().await.len(),
        cards_before,
        "the adoption posts no card and does not duplicate the surfaced block"
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
            client_id: crate::config::DEFAULT_CLIENT_ID.into(),
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
            client_id: crate::config::DEFAULT_CLIENT_ID.into(),
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
            client_id: crate::config::DEFAULT_CLIENT_ID.into(),
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
            client_id: crate::config::DEFAULT_CLIENT_ID.into(),
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
/// patches the clicked list card IN PLACE to the compact 已切换 state
/// (ADR-0100/ADR-0052, the toast carries the same verb): no second message is
/// sent, the list is gone, the compact body replaces the old snapshot body, and
/// the card keeps the 返回列表 button for the list state it came from.
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
        card_str.contains("已切换 重写登录"),
        "the patched card is the compact 已切换 state: {card_str}"
    );
    assert!(
        card_str.contains("已切换到该会话"),
        "the compact body names the state: {card_str}"
    );
    assert!(
        !card_str.contains("最近对话") && !card_str.contains("权限请求"),
        "the retired full snapshot body is gone: {card_str}"
    );
    assert!(
        card_str.contains("返回列表"),
        "the list state stays reachable from the patch: {card_str}"
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
        result.toast.clone().unwrap_or_default().contains("已切换"),
        "adopt toasts the unified 已切换 verb: {:?}",
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
            client_id: crate::config::DEFAULT_CLIENT_ID.into(),
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
            client_id: crate::config::DEFAULT_CLIENT_ID.into(),
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
    let card = result
        .card
        .clone()
        .expect("force_adopt patches the card in place");
    assert!(
        card_text(&card).contains("已切换 被占用的会话"),
        "compact 已切换 state: {card}"
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

/// ADR-0100: a 切换 on a row of a session already mapped to this thread
/// patches the card in place to the compact 已切换 state — no full snapshot
/// and no new message — and the toast carries the 切换 verb. A re-switch with
/// nothing to report behaves the same (the ticket's "still answers one line"
/// applies to the text form; the card form is always this compact state).
#[tokio::test]
async fn switch_card_switch_on_mapped_session_patches_to_compact_state() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let mut backend = MockBackend::new(realistic_parts());
    backend.given_sessions(vec![list_session("ses_own1", "本项目会话", "/work/cola", 500)]);
    backend.cola_message("ses_own1", "上次的问题");
    let (app, platform) = build_app(cfg, backend).await;
    // The session is mapped to this thread but NOT active (a stacked
    // session) — the row shows 切换.
    seed_entry(
        &app,
        crate::config::SessionEntry {
            client_id: crate::config::DEFAULT_CLIENT_ID.into(),
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
            client_id: crate::config::DEFAULT_CLIENT_ID.into(),
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
    let cards_before = platform.replied_cards().await.len();
    let card = result.card.expect("mapped re-switch patches the card");
    let card_str = card.to_string();
    assert!(
        card_str.contains("已切换 本项目会话"),
        "compact switched state header: {card_str}"
    );
    assert!(
        !card_str.contains("最近对话"),
        "the compact state carries no tail: {card_str}"
    );
    assert!(
        result.toast.clone().unwrap_or_default().contains("已切换"),
        "re-switch toasts the verb: {:?}",
        result.toast
    );
    assert_eq!(
        platform.replied_cards().await.len(),
        cards_before,
        "the card op sends no message; the ack patches the list card in place"
    );
    // The mapped session is now the thread's ACTIVE one.
    let key = crate::config::ThreadKey::new("chat_1".into(), "chat_1".into());
    assert_eq!(
        app.sessions.lock().await.get_active(&key).unwrap().session_id,
        "ses_own1"
    );
}

/// ADR-0100: a mapped re-switch whose Session DOES have content to report
/// (busy, plus a pending permission) still patches the clicked list card in
/// place to the compact 已切换 state — no snapshot card, no new message, and
/// 返回列表 keeps the clicked list's filter. The status summary and the
/// pending live on the text surfaces; the list patch cannot carry them.
#[tokio::test]
async fn switch_card_mapped_adopt_with_content_still_patches_compact() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let mut backend = MockBackend::new(realistic_parts());
    backend.given_sessions(vec![list_session("ses_own1", "本项目会话", "/work/cola", 500)]);
    backend.with_session_status("ses_own1", Some(crate::backend::types::SessionStatus::Busy));
    backend.ask_permissions(vec![crate::backend::types::PermissionRequest {
        request_id: "req_own".into(),
        session_id: Some("ses_own1".into()),
        permission: Some("bash".into()),
        patterns: vec!["ls".into()],
        metadata: None,
        always: vec![],
        always_supported: true,
    }]);
    let (app, platform) = build_app(cfg, backend).await;
    let key = crate::config::ThreadKey::new("chat_1".into(), "chat_1".into());
    seed_entry(
        &app,
        crate::config::SessionEntry::new(key.clone(), "ses_own1", "/work/cola"),
    )
    .await;
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
        "keyword": "proj",
        "scope": "all",
        "page": 2,
    });
    let cards_before = platform.replied_cards().await.len();
    let result = app
        .host_action(value)
        .await
        .expect("mapped re-switch returns a result");
    let card = result.card.expect("the re-switch patches the clicked card");
    let card_str = card.to_string();
    assert!(
        card_str.contains("已切换 本项目会话"),
        "the compact state header keeps the 切换 verb: {card_str}"
    );
    assert!(
        !card_str.contains("最近对话")
            && !card_str.contains("权限请求")
            && !card_str.contains(crate::feishu::card::session::BUSY_CHIP)
            && !card_str.contains(crate::feishu::card::session::WAITING_CHIP),
        "no snapshot body on the patched list card: {card_str}"
    );
    assert_eq!(
        platform.replied_cards().await.len(),
        cards_before,
        "the card op sends no message"
    );
    let back = card_buttons(&card)
        .into_iter()
        .find(|b| b["value"]["op"] == "back")
        .expect("the patched card keeps 返回列表");
    assert_eq!(back["value"]["keyword"], "proj");
    assert_eq!(back["value"]["scope"], "all");
    assert_eq!(back["value"]["page"], 2);
    assert_eq!(
        app.sessions.lock().await.get_active(&key).unwrap().session_id,
        "ses_own1"
    );
}

/// ADR-0100: a 切换 on a fully-visible mapped session (idle, no pending,
/// newest user message cola-authored) — the case the old suppression predicate
/// swallowed — still patches to the same compact 已切换 state, never to a full
/// snapshot and never to silence.
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
            client_id: crate::config::DEFAULT_CLIENT_ID.into(),
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
            client_id: crate::config::DEFAULT_CLIENT_ID.into(),
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

/// ADR-0028 + ADR-0100: when the switch card lives inside a topic (a
/// never-bound topic adopting its single session), the patched card is the
/// compact in-topic confirmation — its own message id (`open_message_id`) is
/// persisted as the fallback-card anchor, so later permission/question cards
/// keep routing inside the topic.
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
        card_text(&card).contains("已切换"),
        "compact 已切换 state in the topic: {card}"
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
    let sessions = dir.path().join("sessions.json");
    let cfg = test_config(&sessions);
    let backend = MockBackend::new(realistic_parts());
    let created = backend.created_session_dirs.clone();
    let (app, _platform) = build_app(cfg, backend).await;
    let key = crate::config::ThreadKey::new("chat_1".into(), "chat_1".into());
    // Root an existing session in /work/proj so "current project" is set.
    seed_entry(
        &app,
        crate::config::SessionEntry {
            client_id: crate::config::DEFAULT_CLIENT_ID.into(),
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
    // ADR-0089: the deferred write settles before the callback answers.
    let text = std::fs::read_to_string(&sessions).unwrap();
    assert!(
        text.contains("/work/proj"),
        "the pending reached the file: {text}"
    );
}

/// The `/switch` card's 新建 with no session and no pending falls back to
/// `bridge.work_dir` — the same resolution `/new` uses (ADR-0041), so the card
/// and the command cannot disagree about the project.
#[tokio::test]
async fn switch_card_new_falls_back_to_work_dir_without_active_session() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let mut cfg = test_config(&dir.path().join("sessions.json"));
    let work = dir.path().join("work");
    cfg.bridge.work_dir = Some(work.clone());
    let (app, _platform) = build_app(cfg, MockBackend::new(realistic_parts())).await;
    let key = crate::config::ThreadKey::new("chat_1".into(), "chat_1".into());

    let result = app
        .host_action(serde_json::json!({
            "action": "switch",
            "op": "new",
            "chat_id": "chat_1",
            "thread_id": "chat_1",
        }))
        .await
        .expect("switch new should return a result");
    assert_eq!(result.toast.as_deref(), Some("下一条消息创建会话"));
    let pending = app
        .sessions
        .lock()
        .await
        .pending_for(&key)
        .cloned()
        .expect("a pending is declared");
    assert_eq!(pending.directory, work.to_string_lossy().to_string());
    assert_eq!(
        saved_pending_dirs(&dir.path().join("sessions.json")),
        vec![work.to_string_lossy().to_string()],
        "the fallback pending reached the file too"
    );
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

/// ADR-0052 + ADR-0100: adopting from a filtered, paged `/switch` list patches
/// the card in place to the compact 已切换 state, which carries a 「返回列表」
/// button holding that list's keyword/scope/page; clicking it rebuilds the same
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
    let switched = app
        .host_action(adopt)
        .await
        .expect("adopt returns a result")
        .card
        .expect("adopt patches the card in place");
    let text = card_text(&switched);
    assert!(text.contains("已切换 项目1"), "compact 已切换 state: {text}");
    assert!(!text.contains("会话管理"), "the list is gone: {text}");

    // The compact state carries the list's exact return target.
    let back = card_buttons(&switched)
        .into_iter()
        .find(|b| b["value"]["op"] == "back")
        .expect("the list adoption carries a 返回列表 button");
    assert_eq!(back["text"]["content"], "返回列表");
    assert_eq!(back["value"]["keyword"], "proj");
    assert_eq!(back["value"]["scope"], "all");
    assert_eq!(back["value"]["page"], 2);

    let list = click_button_card(&app, &switched, "back").await;
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

/// ADR-0052 + ADR-0100: a re-switch with nothing to report (idle, no pending,
/// newest user message cola-authored) still patches to the compact 已切换
/// state, and its 「返回列表」 button returns to the same filtered page.
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

/// ADR-0052 + ADR-0100: the text `/switch <kw>` adoption is a text line, not a
/// card patch — so it carries no 「返回列表」 button (there is no list card to
/// return to) and posts no card at all.
#[tokio::test]
async fn text_switch_adoption_posts_a_line_and_no_back_to_list_button() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let mut backend = MockBackend::new(realistic_parts());
    backend.given_sessions(vec![list_session("ses_alpha01", "重写登录", "/work/auth", 100)]);
    let (app, platform) = build_app(cfg, backend).await;

    send_command(&app, "/switch 重写登录", "msg_switch").await;

    assert!(
        platform.replied_cards().await.is_empty(),
        "the text adoption posts no card"
    );
    let text = platform.texts().await.join("\n");
    assert!(
        text.contains("已接管「重写登录」"),
        "one-line confirmation: {text}"
    );
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

/// A real Codex client over the scripted app-server — the production adapter,
/// so the release assertions read the adapter's own wire traffic instead of a
/// mock's call log.
async fn codex_client(url: &str) -> crate::codex::CodexClient {
    let endpoint = crate::codex::Endpoint::parse(url).expect("endpoint");
    crate::codex::CodexClient::connect(
        &crate::codex::CodexOptions::new(endpoint),
        &crate::codex::PinnedProbe,
    )
    .await
    .expect("attach to the scripted app-server")
}

/// One mapped Codex thread for `/switch forget` / switch tests.
fn codex_entry(thread_id: &str) -> crate::config::SessionEntry {
    crate::config::SessionEntry {
        client_id: crate::config::DEFAULT_CLIENT_ID.into(),
        thread_key: crate::config::ThreadKey::new("chat_1".into(), "chat_1".into()),
        session_id: thread_id.into(),
        directory: "/work/cola".into(),
        agent: None,
        model: None,
        auto_accept: false,
        topic_anchor: None,
        topic_root: None,
        variant: None,
    }
}

/// `/switch forget` on a Codex session releases the adapter's subscription:
/// `thread/unsubscribe` goes out once, a second connection's subscription to
/// the same thread keeps streaming, and the released session re-adopts later.
#[tokio::test]
async fn switch_forget_releases_only_this_chats_codex_subscription() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let (server, url) = crate::codex::test_server::FakeServer::tcp("codex-tui/0.166.0 (test)").await;
    server.seed_thread(crate::codex::test_server::ScriptedThread::adopted(
        "thread-1",
        "session-root",
        "/work/cola",
        "a real thread",
    ));
    let client = codex_client(&url).await;
    let peer = codex_client(&url).await;
    client
        .transcript("thread-1")
        .await
        .expect("cola's connection adopts");
    peer.transcript("thread-1").await.expect("the peer adopts");
    server.wait_for_subscribers("thread-1", 2).await;

    let app = Arc::new(App::new(cfg, Arc::new(client.clone()), Arc::new(RecordingPlatform::new())).unwrap());
    seed_entry(&app, codex_entry("thread-1")).await;

    send_command(&app, "/switch forget", "msg_forget").await;

    assert_eq!(server.unsubscribe_calls(), 1, "one acknowledged unsubscribe");
    server.wait_for_subscribers("thread-1", 1).await;
    // The other subscriber still streams: the server did not unload the thread.
    assert_eq!(
        server.push_item("thread-1", "item-peer", "the peer still streams"),
        1
    );
    assert!(
        client
            .projections
            .transcript("thread-1")
            .messages
            .iter()
            .all(|message| !message.text().contains("the peer still streams")),
        "the released connection no longer folds this thread's items"
    );
    // Re-adopting the released session works (a fresh `thread/resume` and a
    // live stream), which is the promise `/switch forget` makes to the user.
    client
        .transcript("thread-1")
        .await
        .expect("the released session re-adopts");
    assert_eq!(
        server.resume_calls(),
        3,
        "each connection adopted once; the release re-adopts once"
    );
    server.wait_for_subscribers("thread-1", 2).await;
}

/// Switching to another session releases the previous one's subscription: the
/// displaced session stays mapped and switchable, but its stream is freed.
#[tokio::test]
async fn switching_to_another_session_releases_the_previous_codex_subscription() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let (server, url) = crate::codex::test_server::FakeServer::tcp("codex-tui/0.166.0 (test)").await;
    server.seed_thread(crate::codex::test_server::ScriptedThread::adopted(
        "thread-1",
        "session-root",
        "/work/cola",
        "first session",
    ));
    server.seed_thread(crate::codex::test_server::ScriptedThread::adopted(
        "thread-2",
        "session-root-2",
        "/work/cola",
        "second session",
    ));
    let client = codex_client(&url).await;
    client
        .transcript("thread-1")
        .await
        .expect("the mapped session is mirrored");
    server.wait_for_subscribers("thread-1", 1).await;

    let app = Arc::new(App::new(cfg, Arc::new(client), Arc::new(RecordingPlatform::new())).unwrap());
    seed_entry(&app, codex_entry("thread-1")).await;

    send_command(&app, "/switch second session", "msg_switch").await;

    let key = crate::config::ThreadKey::new("chat_1".into(), "chat_1".into());
    assert_eq!(
        app.sessions.lock().await.get_active(&key).unwrap().session_id,
        "thread-2",
        "the switch landed on the new session"
    );
    assert_eq!(server.unsubscribe_calls(), 1, "the displaced session is released");
    server.wait_for_subscribers("thread-1", 0).await;
    // The new session was adopted by the switch (its snapshot read), so the
    // release did not disturb another thread's stream.
    server.wait_for_subscribers("thread-2", 1).await;
}

/// A release the client cannot complete is logged, never surfaced: the local
/// unmap and its user-facing confirmation still happen.
#[tokio::test]
async fn a_failed_release_does_not_fail_switch_forget() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let backend = MockBackend::new(realistic_parts());
    let release_calls = backend.release_calls.clone();
    backend
        .fail_release_count
        .store(1, std::sync::atomic::Ordering::SeqCst);
    let (app, platform) = build_app(cfg, backend).await;
    seed_entry(&app, codex_entry("ses_release_fails")).await;

    send_command(&app, "/switch forget", "msg_forget").await;

    let key = crate::config::ThreadKey::new("chat_1".into(), "chat_1".into());
    assert!(
        app.sessions.lock().await.get_active(&key).is_none(),
        "the unmap still happened"
    );
    assert_eq!(
        release_calls.lock().await.clone(),
        vec!["ses_release_fails".to_string()],
        "the release was attempted"
    );
    let text = platform.texts().await.join("\n");
    assert!(text.contains("已解除"), "the confirmation still lands: {text}");
}

/// Spec #922: a provider pick on a live Codex thread is a toast and **no
/// outbound request at all** — not even a `thread/resume` — and names `/new`
/// (actionable) and `/fork` (a later capability).
#[tokio::test]
async fn a_live_provider_pick_is_refused_with_no_request() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let (server, url) = crate::codex::test_server::FakeServer::tcp("codex-tui/0.166.0 (test)").await;
    server.seed_thread(crate::codex::test_server::ScriptedThread::adopted(
        "thread-1",
        "session-root",
        "/work/cola",
        "a shared thread",
    ));
    server.set_thread_model("thread-1", Some("gpt-5.1-codex"), Some("openai"));
    let client = codex_client(&url).await;
    client.transcript("thread-1").await.expect("adopt the thread");
    server.wait_for_subscribers("thread-1", 1).await;
    assert_eq!(server.resume_calls(), 1, "only the adoption resumed");

    let app = Arc::new(App::new(cfg, Arc::new(client), Arc::new(RecordingPlatform::new())).unwrap());
    seed_entry(&app, codex_entry("thread-1")).await;

    let pick = serde_json::json!({
        "action": "model",
        "level": "model",
        "chat_id": "chat_1",
        "thread_id": "chat_1",
        "value": "deepseek/deepseek-v4-pro",
    });
    server.clear_calls();
    let step = app.host_action(pick).await.expect("provider pick");
    assert!(step.card.is_none(), "a refusal is a toast, not a card");
    let toast = step.toast.expect("the named refusal");
    assert!(
        toast.contains("配置事实") && toast.contains("model_provider") && toast.contains("重启"),
        "the refusal says provider is a config fact: {toast}"
    );
    assert!(
        !toast.contains("/new") && !toast.contains("/fork"),
        "a config fact is not a `/new` remedy: {toast}"
    );
    assert_eq!(server.resume_calls(), 1, "the click sent no resume");
    assert_eq!(
        server.call_log(),
        Vec::<String>::new(),
        "the refusal sent no outbound request at all"
    );
}

/// Spec #930 acceptance 3/4: when the channel is **undeterminable** (a cold
/// projection), a pick is not blocked: it is recorded model-only, sends no
/// request, and the server's own model error is the fallback.
#[tokio::test]
async fn an_unknown_channel_pick_is_recorded_model_only_with_no_request() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let (server, url) = crate::codex::test_server::FakeServer::tcp("codex-tui/0.166.0 (test)").await;
    server.seed_thread(crate::codex::test_server::ScriptedThread::adopted(
        "thread-1",
        "session-root",
        "/work/cola",
        "a shared thread",
    ));
    server.set_thread_model("thread-1", Some("gpt-5.1-codex"), Some("openai"));
    // Nothing adopts the thread, so the adapter's projection holds no provider
    // fact for it; deciding the pick must not probe.
    let client = codex_client(&url).await;
    let pending_client = client.clone();

    let platform = Arc::new(RecordingPlatform::new());
    let app = Arc::new(App::new(cfg, Arc::new(client), platform.clone()).unwrap());
    seed_entry(&app, codex_entry("thread-1")).await;

    server.clear_calls();
    send_command_in(
        &app,
        "/model deepseek/deepseek-v4-pro",
        crate::config::ThreadKey::new("chat_1".into(), "chat_1".into()),
        "msg_model",
        crate::config::ConversationKind::P2p,
    )
    .await;

    let reply = platform
        .calls
        .lock()
        .await
        .iter()
        .filter_map(|call| match call {
            PlatformCall::ReplyText { text, .. } => Some(text.clone()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        reply.contains("Model: deepseek/deepseek-v4-pro"),
        "an unknown channel records the model-only pick: {reply}"
    );
    assert_eq!(
        pending_client.pending_model("thread-1").await.as_deref(),
        Some("deepseek/deepseek-v4-pro"),
        "the pick is the next turn's pending model"
    );
    assert_eq!(
        server.call_log(),
        Vec::<String>::new(),
        "deciding an unknown-channel pick sends no request"
    );
}

/// Spec #922: Codex's `codex` bucket is **neutral** (`codex/<model>` names no
/// thread-level channel), so a pick using it is a model-only pick even when the
/// thread runs a real provider. It must not be compared to the thread's
/// provider and refused — and it needs no request either.
#[tokio::test]
async fn a_neutral_bucket_pick_is_model_only() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let (server, url) = crate::codex::test_server::FakeServer::tcp("codex-tui/0.166.0 (test)").await;
    server.seed_thread(crate::codex::test_server::ScriptedThread::adopted(
        "thread-1",
        "session-root",
        "/work/cola",
        "a shared thread",
    ));
    server.set_thread_model("thread-1", Some("gpt-5.1-codex"), Some("openai"));
    // The default options leave the provider gate shut.
    let client = codex_client(&url).await;
    client.transcript("thread-1").await.expect("adopt the thread");
    server.wait_for_subscribers("thread-1", 1).await;
    let pending_client = client.clone();

    let platform = Arc::new(RecordingPlatform::new());
    let app = Arc::new(App::new(cfg, Arc::new(client), platform.clone()).unwrap());
    let mut entry = codex_entry("thread-1");
    entry.model = Some("openai/gpt-5.1-codex".into());
    seed_entry(&app, entry).await;

    server.clear_calls();
    send_command_in(
        &app,
        "/model codex/gpt-5.1",
        crate::config::ThreadKey::new("chat_1".into(), "chat_1".into()),
        "msg_model",
        crate::config::ConversationKind::P2p,
    )
    .await;

    let reply = platform
        .calls
        .lock()
        .await
        .iter()
        .filter_map(|call| match call {
            PlatformCall::ReplyText { text, .. } => Some(text.clone()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        reply.contains("Model: codex/gpt-5.1"),
        "the neutral bucket is recorded as a model-only pick: {reply}"
    );
    assert_eq!(
        app.sessions
            .lock()
            .await
            .get_active(&crate::config::ThreadKey::new("chat_1".into(), "chat_1".into()))
            .cloned()
            .unwrap()
            .model
            .as_deref(),
        Some("openai/gpt-5.1-codex"),
        "a live pick does not rewrite the thread's current model"
    );
    assert_eq!(
        pending_client.pending_model("thread-1").await.as_deref(),
        Some("openai/gpt-5.1"),
        "the neutral pick is the next turn's pending, shown on the thread's provider"
    );
    assert_eq!(
        server.call_log(),
        Vec::<String>::new(),
        "the neutral pick needs no request"
    );
}

/// Issue #910/#908 (blocking): `codex` is **reserved** for the neutral catalog
/// namespace, whatever the config declares. A `[model_providers.codex]` cannot
/// change that: a catalog pick stays a per-turn model send (the bare id with the
/// thread's provider untouched), and the card lists the configured id only as a
/// reserved, non-selectable row.
#[tokio::test]
async fn a_catalog_pick_stays_neutral_even_with_a_configured_codex_provider() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let (server, url) = crate::codex::test_server::FakeServer::tcp("codex-tui/0.166.0 (test)").await;
    server.seed_thread(crate::codex::test_server::ScriptedThread::adopted(
        "thread-1",
        "session-root",
        "/work/cola",
        "a shared thread",
    ));
    server.set_thread_model("thread-1", Some("gpt-5.1-codex"), Some("openai"));
    server.set_config_read(serde_json::json!({
        "config": {},
        "layers": [{
            "name": { "type": "user" },
            "version": "v1",
            "config": {
                "model_providers": {
                    "codex": { "name": "Real Codex", "base_url": "http://127.0.0.1:9/v1" }
                }
            }
        }]
    }));
    let client = codex_client(&url).await;
    client.transcript("thread-1").await.expect("adopt the thread");
    server.wait_for_subscribers("thread-1", 1).await;
    let pending_client = client.clone();
    let platform = Arc::new(RecordingPlatform::new());
    let app = Arc::new(App::new(cfg, Arc::new(client), platform.clone()).unwrap());
    let mut entry = codex_entry("thread-1");
    entry.model = Some("openai/gpt-5.1-codex".into());
    seed_entry(&app, entry).await;

    server.clear_calls();
    send_command_in(
        &app,
        "/model codex/gpt-5.1",
        crate::config::ThreadKey::new("chat_1".into(), "chat_1".into()),
        "msg_model",
        crate::config::ConversationKind::P2p,
    )
    .await;

    let reply = platform
        .calls
        .lock()
        .await
        .iter()
        .filter_map(|call| match call {
            PlatformCall::ReplyText { text, .. } => Some(text.clone()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        reply.contains("Model: codex/gpt-5.1"),
        "the catalog ref stays neutral: {reply}"
    );
    assert_eq!(
        app.sessions
            .lock()
            .await
            .get_active(&crate::config::ThreadKey::new("chat_1".into(), "chat_1".into()))
            .cloned()
            .unwrap()
            .model
            .as_deref(),
        Some("openai/gpt-5.1-codex"),
        "the live thread's current pair is untouched"
    );
    assert_eq!(
        pending_client.pending_model("thread-1").await.as_deref(),
        Some("openai/gpt-5.1"),
        "the neutral bucket is the next turn's pending"
    );
    assert_eq!(
        server.call_log(),
        Vec::<String>::new(),
        "and it sends no request / touches no thread provider"
    );
}
