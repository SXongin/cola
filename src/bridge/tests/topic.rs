use crate::bridge::command::*;
use crate::bridge::test_support::*;

/// by a new session rooted at <dir>, maps the returned thread_id to that
/// session, and leaves the lobby conversation untouched.
#[tokio::test]
async fn topic_command_creates_topic_mapped_to_new_session() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let mut backend = MockBackend::new(realistic_parts());
    backend.session_id = "ses_topic".into();
    let title_calls = backend.update_title_calls.clone();
    let (app, platform) = build_app(cfg, backend).await;
    let proj = tempfile::tempdir().unwrap();
    let proj_dir = proj.path().to_string_lossy().to_string();

    crate::bridge::command::handle_command(
        &app.core,
        Command::Topic {
            directory: Some(proj_dir.clone()),
            name: Some("api-refactor".into()),
        },
        crate::config::ThreadKey::new("chat_1".into(), "chat_1".into()),
        "msg_topic",
        crate::config::ConversationKind::P2p,
    )
    .await
    .unwrap();

    // The topic is created via a cover card sent to the chat's top level,
    // then reply_in_thread on THAT card: the cover becomes the thread root,
    // so the chat-list topic entry shows the session brief permanently
    // (ADR-0023). On the mock the cover send returns "msg_sent".
    let calls = platform.calls.lock().await.clone();
    assert!(
        calls
            .iter()
            .any(|c| matches!(c, PlatformCall::SendCard { receive_id, .. } if receive_id == "chat_1")),
        "expected a cover card sent to the chat, got {calls:?}"
    );
    assert!(
        calls
            .iter()
            .any(|c| matches!(c, PlatformCall::ReplyInThread { message_id, .. } if message_id == "msg_sent")),
        "expected reply_in_thread on the cover card, got {calls:?}"
    );
    // The cover card carries the session brief; the in-topic seed is only a
    // short hint (the brief lives on the root card at the top of the thread).
    let cover = platform
        .sent_cards()
        .await
        .into_iter()
        .next()
        .map(|c| c.to_string())
        .expect("cover card JSON");
    assert!(
        cover.contains("💬 `api-refactor`") && cover.contains("会话 `topic`"),
        "cover card should lead with the title and session, got: {cover}"
    );
    let seed = calls
        .iter()
        .find_map(|c| match c {
            PlatformCall::ReplyInThread { text, .. } => Some(text.clone()),
            _ => None,
        })
        .expect("reply_in_thread seed text");
    assert_eq!(seed, "请在本话题内回复，即可和这个会话对话。");

    // The created topic's thread_id is mapped to the new session.
    let topic_key = crate::config::ThreadKey::new("chat_1".into(), "omt_created_topic".into());
    let entry = app
        .sessions
        .lock()
        .await
        .get_active(&topic_key)
        .cloned()
        .expect("topic thread_id should map to the new session");
    assert_eq!(entry.session_id, "ses_topic");
    // normalize_directory canonicalizes the project path (resolving
    // /private/var on macOS and \\?\ / 8.3 short names on Windows), so
    // compare against the canonicalized form — not the raw tempdir path.
    assert_eq!(
        entry.directory,
        std::fs::canonicalize(proj.path()).unwrap().to_string_lossy()
    );
    // The named `/topic` PATCHed the server title (ADR-0007).
    assert_eq!(
        title_calls.lock().await.as_slice(),
        &[("ses_topic".to_string(), "api-refactor".to_string())]
    );
    // The topic anchor is the confirmation message INSIDE the topic; future
    // sent cards reply to it so they stay in the topic.
    assert_eq!(entry.topic_anchor.as_deref(), Some("msg_topic_reply"));
    // The thread root is the cover card (ADR-0023): the injection guard
    // excludes it from Quoted Context, and the post-turn hook patches it
    // when the server auto-generates a title.
    assert_eq!(entry.topic_root.as_deref(), Some("msg_sent"));
    // The cover title is recorded so the post-turn hook can sync it.
    assert_eq!(
        app.core.cover_titles.lock().await.get("ses_topic").cloned(),
        Some(crate::bridge::core::CoverTitle {
            title: "api-refactor".to_string(),
            model: None
        })
    );

    // The lobby conversation still maps to nothing new (no session was
    // created for the lobby itself).
    let lobby_key = crate::config::ThreadKey::new("chat_1".into(), "chat_1".into());
    assert!(app.sessions.lock().await.get_active(&lobby_key).is_none());
}

/// ADR-0023: when the cover card cannot be sent, the thread anchors on the
/// user's command message instead — the old behavior — and no cover title
/// is recorded (so the post-turn hook never tries to patch the user's
/// message, which Feishu would reject).
#[tokio::test]
async fn topic_cover_send_failure_falls_back_to_command_root() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let backend = MockBackend::new(realistic_parts());
    let mut platform = Arc::new(RecordingPlatform::new());
    Arc::get_mut(&mut platform).unwrap().fail_send_card = true;
    let app = Arc::new(App::new(cfg, Arc::new(backend), platform.clone()).unwrap());
    let proj = tempfile::tempdir().unwrap();
    let proj_dir = proj.path().to_string_lossy().to_string();

    // A stale cover-title entry may already exist (e.g. the session was
    // previously adopted into a cover-rooted topic); the fallback must drop
    // it so the post-turn hook never patches the user's command message.
    seed_cover_title(&app, "ses_test", "旧封面").await;

    crate::bridge::command::handle_command(
        &app.core,
        Command::Topic {
            directory: Some(proj_dir.clone()),
            name: None,
        },
        crate::config::ThreadKey::new("chat_1".into(), "chat_1".into()),
        "msg_topic",
        crate::config::ConversationKind::P2p,
    )
    .await
    .unwrap();

    let calls = platform.calls.lock().await.clone();
    assert!(
        calls.iter().any(
            |c| matches!(c, PlatformCall::ReplyInThread { message_id, .. } if message_id == "msg_topic")
        ),
        "expected reply_in_thread on the command message after cover failure, got {calls:?}"
    );

    let topic_key = crate::config::ThreadKey::new("chat_1".into(), "omt_created_topic".into());
    let entry = app
        .sessions
        .lock()
        .await
        .get_active(&topic_key)
        .cloned()
        .expect("topic thread_id should map to the new session");
    assert_eq!(entry.topic_root.as_deref(), Some("msg_topic"));
    assert!(
        app.core.cover_titles.lock().await.is_empty(),
        "no cover title may survive without a cover card"
    );
}

/// ADR-0023: after a completed turn, when the server holds a different
/// title for the session (auto-generated after the first exchange), cola
/// patches the cover card in place — the thread root — so the chat-list
/// topic entry shows the real title.
#[tokio::test]
async fn topic_cover_card_updated_with_auto_title_after_turn() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let backend = MockBackend::new(realistic_parts());
    backend
        .session_titles
        .lock()
        .unwrap()
        .insert("ses_t1".into(), "修复登录 bug".into());
    let (app, platform) = build_app(cfg, backend).await;

    let topic_key = crate::config::ThreadKey::new("chat_1".into(), "omt_t_1".into());
    seed_entry(
        &app,
        crate::config::SessionEntry {
            thread_key: topic_key.clone(),
            session_id: "ses_t1".into(),
            directory: "/work/t".into(),
            agent: None,
            model: None,
            auto_accept: false,
            topic_anchor: Some("om_seed".into()),
            topic_root: Some("om_cover".into()),
            variant: None,
        },
    )
    .await;
    seed_cover_title(&app, "ses_t1", "旧标题").await;

    app.handle_message(crate::bridge::IncomingMessage {
        message_id: "msg_1".into(),
        chat_id: "chat_1".into(),
        chat_type: "group".into(),
        thread_id: Some("omt_t_1".into()),
        parent_id: None,
        text: "继续".into(),
        images: vec![],
        requester_open_id: None,
    })
    .await;

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
        "the cover card must be patched in place, got {calls:?}"
    );
    assert!(
        patched.last().unwrap().contains("修复登录 bug"),
        "cover card must show the server title: {:?}",
        patched.last()
    );
    assert_eq!(
        app.core.cover_titles.lock().await.get("ses_t1").cloned(),
        Some(crate::bridge::core::CoverTitle {
            title: "修复登录 bug".to_string(),
            model: None
        })
    );
}

/// Bare `/topic` (no args) creates the topic session in the conversation's
/// CURRENT PROJECT — the active session's directory — instead of demanding
/// an explicit `<dir>`, exactly like `/new` (ADR-0012 project model).
#[tokio::test]
async fn topic_command_bare_inherits_current_project_directory() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let mut backend = MockBackend::new(realistic_parts());
    backend.session_id = "ses_topic".into();
    let (app, platform) = build_app(cfg, backend).await;
    let proj = tempfile::tempdir().unwrap();
    let proj_dir = proj.path().to_string_lossy().to_string();

    let thread_key = crate::config::ThreadKey::new("chat_1".into(), "chat_1".into());

    // Root a session in the project with `/dir`; it becomes the active
    // session whose directory bare `/topic` must inherit.
    crate::bridge::command::handle_command(
        &app.core,
        crate::bridge::command::Command::Dir(proj_dir.clone()),
        thread_key.clone(),
        "msg_dir",
        crate::config::ConversationKind::P2p,
    )
    .await
    .unwrap();

    // Bare `/topic` — no directory given.
    crate::bridge::command::handle_command(
        &app.core,
        Command::Topic {
            directory: None,
            name: None,
        },
        thread_key.clone(),
        "msg_topic",
        crate::config::ConversationKind::P2p,
    )
    .await
    .unwrap();

    // The topic is created on the cover card sent to the chat, like the
    // explicit form (ADR-0023); the mock cover send returns "msg_sent".
    let calls = platform.calls.lock().await.clone();
    assert!(
        calls
            .iter()
            .any(|c| matches!(c, PlatformCall::ReplyInThread { message_id, .. } if message_id == "msg_sent")),
        "expected a reply_in_thread on the cover card, got {calls:?}"
    );

    // The topic session lives in the inherited project directory.
    let topic_key = crate::config::ThreadKey::new("chat_1".into(), "omt_created_topic".into());
    let entry = app
        .sessions
        .lock()
        .await
        .get_active(&topic_key)
        .cloned()
        .expect("bare /topic should map the created topic to its session");
    assert_eq!(entry.session_id, "ses_topic");
    assert_eq!(
        entry.directory,
        std::fs::canonicalize(proj.path()).unwrap().to_string_lossy(),
        "bare /topic must inherit the active session's directory"
    );
    assert_eq!(entry.topic_anchor.as_deref(), Some("msg_topic_reply"));
}

/// A message sent INSIDE the created topic routes to the topic's session,
/// not to a fresh lobby session.
#[tokio::test]
async fn topic_command_created_topic_routes_messages_to_its_session() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let mut backend = MockBackend::new(realistic_parts());
    backend.session_id = "ses_topic".into();
    let prompt_calls = backend.prompt_calls.clone();
    let (app, _platform) = build_app(cfg, backend).await;

    // Create the topic first.
    let proj = tempfile::tempdir().unwrap();
    let proj_dir = proj.path().to_string_lossy().to_string();
    crate::bridge::command::handle_command(
        &app.core,
        Command::Topic {
            directory: Some(proj_dir),
            name: None,
        },
        crate::config::ThreadKey::new("chat_1".into(), "chat_1".into()),
        "msg_topic",
        crate::config::ConversationKind::P2p,
    )
    .await
    .unwrap();

    // Now a message arrives inside that topic (thread_id = the mapped one).
    app.handle_message(incoming(
        "msg_in_topic".into(),
        "chat_1".into(),
        "p2p".into(),
        Some("omt_created_topic".into()),
        "帮我看看这个目录".into(),
        None,
    ))
    .await;

    // It must reuse the topic session (ses_topic), not create a new one.
    let calls = prompt_calls.lock().await.clone();
    assert_eq!(calls, vec!["帮我看看这个目录".to_string()]);
    let store = app.sessions.lock().await;
    let topic_key = crate::config::ThreadKey::new("chat_1".into(), "omt_created_topic".into());
    assert_eq!(
        store.get_active(&topic_key).map(|e| e.session_id.as_str()),
        Some("ses_topic")
    );
    // The lobby got NO session of its own.
    let lobby_key = crate::config::ThreadKey::new("chat_1".into(), "chat_1".into());
    assert!(store.get_active(&lobby_key).is_none());
}

/// The topic command gate (ADR-0007, ADR-0023) is one rule table
/// (`Command::topic_rejection`, called from `handle_command`): `/topic` and
/// its adopt forms are rejected in ANY topic; `/dir`, `/switch` and `/new`
/// only once the topic is bound. This table drives every class through the
/// real dispatcher — a banned command gets exactly one rejection reply and
/// nothing else, an allowed command is never intercepted.
#[tokio::test]
async fn topic_command_gate_rejects_banned_commands_and_lets_others_through() {
    use crate::bridge::command::{
        TOPIC_ADOPT_NEST_REJECTION, TOPIC_NEST_REJECTION, TOPIC_SELECTION_REJECTION,
    };

    struct Case {
        cmd: Command,
        has_session: bool,
        rejection: Option<&'static str>,
    }

    let work = tempfile::tempdir().unwrap();
    let dir = work.path().to_string_lossy().to_string();
    let cases = [
        // `/topic` family: banned in ANY topic, bound or not.
        Case {
            cmd: Command::Topic {
                directory: Some(dir.clone()),
                name: None,
            },
            has_session: false,
            rejection: Some(TOPIC_NEST_REJECTION),
        },
        Case {
            cmd: Command::Topic {
                directory: None,
                name: Some("n".into()),
            },
            has_session: true,
            rejection: Some(TOPIC_NEST_REJECTION),
        },
        Case {
            cmd: Command::TopicAdopt {
                keyword: "kw".into(),
                force: false,
            },
            has_session: false,
            rejection: Some(TOPIC_ADOPT_NEST_REJECTION),
        },
        Case {
            cmd: Command::TopicAdoptCard,
            has_session: true,
            rejection: Some(TOPIC_ADOPT_NEST_REJECTION),
        },
        // Selection commands: banned only once the topic is bound.
        Case {
            cmd: Command::Dir(dir.clone()),
            has_session: true,
            rejection: Some(TOPIC_SELECTION_REJECTION),
        },
        Case {
            cmd: Command::DirCard,
            has_session: true,
            rejection: Some(TOPIC_SELECTION_REJECTION),
        },
        Case {
            cmd: Command::Switch(SwitchAction::Match("kw".into())),
            has_session: true,
            rejection: Some(TOPIC_SELECTION_REJECTION),
        },
        Case {
            cmd: Command::Switch(SwitchAction::Card),
            has_session: true,
            rejection: Some(TOPIC_SELECTION_REJECTION),
        },
        Case {
            cmd: Command::Switch(SwitchAction::List {
                keyword: None,
                all: false,
            }),
            has_session: true,
            rejection: Some(TOPIC_SELECTION_REJECTION),
        },
        Case {
            cmd: Command::Switch(SwitchAction::Forget),
            has_session: true,
            rejection: Some(TOPIC_SELECTION_REJECTION),
        },
        Case {
            cmd: Command::Switch(SwitchAction::Attach {
                query: "ses_x".into(),
                force: false,
            }),
            has_session: true,
            rejection: Some(TOPIC_SELECTION_REJECTION),
        },
        Case {
            cmd: Command::New(None),
            has_session: true,
            rejection: Some(TOPIC_SELECTION_REJECTION),
        },
        // Selection commands in an UNBOUND topic pass...
        Case {
            cmd: Command::Dir(dir.clone()),
            has_session: false,
            rejection: None,
        },
        Case {
            cmd: Command::DirCard,
            has_session: false,
            rejection: None,
        },
        Case {
            cmd: Command::Switch(SwitchAction::Match("kw".into())),
            has_session: false,
            rejection: None,
        },
        Case {
            cmd: Command::Switch(SwitchAction::List {
                keyword: None,
                all: false,
            }),
            has_session: false,
            rejection: None,
        },
        Case {
            cmd: Command::Switch(SwitchAction::Forget),
            has_session: false,
            rejection: None,
        },
        Case {
            cmd: Command::Switch(SwitchAction::Attach {
                query: "ses_x".into(),
                force: false,
            }),
            has_session: false,
            rejection: None,
        },
        Case {
            cmd: Command::New(None),
            has_session: false,
            rejection: None,
        },
        Case {
            cmd: Command::New(Some("n".into())),
            has_session: false,
            rejection: None,
        },
        // ...and control commands even in a bound topic.
        Case {
            cmd: Command::Name("n".into()),
            has_session: true,
            rejection: None,
        },
        Case {
            cmd: Command::Stop,
            has_session: true,
            rejection: None,
        },
        Case {
            cmd: Command::Compact,
            has_session: true,
            rejection: None,
        },
        Case {
            cmd: Command::Agent("--reset".into()),
            has_session: true,
            rejection: None,
        },
        Case {
            cmd: Command::Think("--reset".into()),
            has_session: true,
            rejection: None,
        },
        Case {
            cmd: Command::AutoAccept(AutoAcceptAction::Status),
            has_session: true,
            rejection: None,
        },
        Case {
            cmd: Command::Help(None),
            has_session: true,
            rejection: None,
        },
        Case {
            cmd: Command::Version,
            has_session: true,
            rejection: None,
        },
    ];

    let topic_key = crate::config::ThreadKey::new("chat_1".into(), "omt_gate".into());
    for (i, case) in cases.iter().enumerate() {
        let _wd = test_work_dir();
        let state_dir = tempfile::tempdir().unwrap();
        let cfg = test_config(&state_dir.path().join("sessions.json"));
        let (app, platform) = build_app(cfg, MockBackend::new(realistic_parts())).await;
        if case.has_session {
            seed_entry(
                &app,
                crate::config::SessionEntry {
                    thread_key: topic_key.clone(),
                    session_id: "ses_owned".into(),
                    directory: "/work/topic".into(),
                    agent: None,
                    model: None,
                    auto_accept: false,
                    topic_anchor: None,
                    topic_root: None,
                    variant: None,
                },
            )
            .await;
        }

        crate::bridge::command::handle_command(
            &app.core,
            case.cmd.clone(),
            topic_key.clone(),
            "msg_gate",
            crate::config::ConversationKind::Topic,
        )
        .await
        .unwrap();

        let calls = platform.calls.lock().await.clone();
        let replies = platform.texts().await;
        let label = format!("case {i} ({:?}, has_session={})", case.cmd, case.has_session);
        if let Some(reason) = case.rejection {
            assert_eq!(
                calls.len(),
                1,
                "{label}: a banned command must reply once and do nothing else, got {calls:?}"
            );
            assert_eq!(replies, vec![reason.to_string()], "{label}: rejection text");
            assert_eq!(
                app.sessions.lock().await.all_entries().len(),
                usize::from(case.has_session),
                "{label}: a banned command must not create a session mapping"
            );
        } else {
            for reason in [
                TOPIC_SELECTION_REJECTION,
                TOPIC_NEST_REJECTION,
                TOPIC_ADOPT_NEST_REJECTION,
            ] {
                assert!(
                    !replies.iter().any(|t| t == reason),
                    "{label}: an allowed command must not be intercepted, got {replies:?}"
                );
            }
        }
    }
}

// ===== /topic --adopt (ADR-0016) =====

/// `/topic --adopt <kw>` resolves an existing session and opens a NEW topic
/// around it, mapping the adopted session to the new topic's ThreadKey.
#[tokio::test]
async fn topic_adopt_opens_topic_around_existing_session() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let mut backend = MockBackend::new(realistic_parts());
    backend.session_list = vec![list_session(
        "ses_foreign123abc",
        "重写登录模块",
        "/work/auth",
        100,
    )];
    let (app, platform) = build_app(cfg, backend).await;

    crate::bridge::command::handle_command(
        &app.core,
        Command::TopicAdopt {
            keyword: "重写登录".into(),
            force: false,
        },
        crate::config::ThreadKey::new("chat_1".into(), "chat_1".into()),
        "msg_topic_adopt",
        crate::config::ConversationKind::P2p,
    )
    .await
    .unwrap();

    // The topic is created via a cover card sent to the chat, then the
    // Session Snapshot card replied in-thread on that card (ADR-0023 +
    // ADR-0028): the cover stays the chat-list root, the snapshot is the
    // topic's first in-topic message and its anchor. The mock cover
    // returns "msg_sent".
    let calls = platform.calls.lock().await.clone();
    let seed_card = calls
        .iter()
        .find_map(|c| match c {
            PlatformCall::ReplyCardInThread { message_id, card, .. } if message_id == "msg_sent" => {
                Some(card.to_string())
            }
            _ => None,
        })
        .expect("expected the snapshot card replied in-thread on the cover card, got {calls:?}");
    assert!(
        seed_card.contains("已接管 重写登录模块"),
        "snapshot seed carries the adopt verb and title: {seed_card}"
    );
    assert!(
        !calls
            .iter()
            .any(|c| matches!(c, PlatformCall::ReplyInThread { .. })),
        "an adopted topic seeds with the snapshot card, not the reply hint: {calls:?}"
    );

    // The new topic's thread_id maps to the ADOPTED session (not a new one),
    // with the snapshot card (the topic's first in-topic message) as the
    // fallback-card anchor.
    let topic_key = crate::config::ThreadKey::new("chat_1".into(), "omt_created_topic".into());
    let entry = app
        .sessions
        .lock()
        .await
        .get_active(&topic_key)
        .cloned()
        .expect("topic thread_id should map to the adopted session");
    assert_eq!(entry.session_id, "ses_foreign123abc");
    assert_eq!(entry.directory, "/work/auth");
    assert_eq!(entry.topic_anchor.as_deref(), Some("msg_topic_reply"));
    // The lobby is untouched (no session created for it).
    let lobby_key = crate::config::ThreadKey::new("chat_1".into(), "chat_1".into());
    assert!(app.sessions.lock().await.get_active(&lobby_key).is_none());
}

/// `/topic --adopt` rejects a child (sub-task) session.
#[tokio::test]
async fn topic_adopt_rejects_child_session() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let mut backend = MockBackend::new(realistic_parts());
    backend.session_list = vec![opencode::client::SessionListInfo {
        parent_id: Some("ses_parent".into()),
        ..list_session("ses_child09", "Child session - x", "/work/auth", 100)
    }];
    let (app, platform) = build_app(cfg, backend).await;

    crate::bridge::command::handle_command(
        &app.core,
        Command::TopicAdopt {
            keyword: "ses_child09".into(),
            force: false,
        },
        crate::config::ThreadKey::new("chat_1".into(), "chat_1".into()),
        "msg_topic_adopt",
        crate::config::ConversationKind::P2p,
    )
    .await
    .unwrap();

    let calls = platform.calls.lock().await.clone();
    let text = platform.texts().await.join("\n");
    assert!(text.contains("子任务"), "rejection mentions sub-task: {text}");
    assert!(
        calls
            .iter()
            .all(|c| !matches!(c, PlatformCall::ReplyInThread { .. })),
        "no topic created for a child session: {calls:?}"
    );
    assert!(app.sessions.lock().await.all_entries().is_empty());
}

/// `/topic --adopt` of a session mapped to another thread rejects with an
/// actionable note pointing at the text `--force`, unless `--force` is given.
#[tokio::test]
async fn topic_adopt_rejects_owned_session_without_force() {
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

    crate::bridge::command::handle_command(
        &app.core,
        Command::TopicAdopt {
            keyword: "ses_owned".into(),
            force: false,
        },
        crate::config::ThreadKey::new("chat_1".into(), "chat_1".into()),
        "msg_topic_adopt",
        crate::config::ConversationKind::P2p,
    )
    .await
    .unwrap();

    let calls = platform.calls.lock().await.clone();
    let text = platform.texts().await.join("\n");
    assert!(text.contains("隔壁群"), "rejection names the owning chat: {text}");
    assert!(
        text.contains("--force"),
        "rejection points at /topic --adopt ... --force: {text}"
    );
    assert!(
        calls
            .iter()
            .all(|c| !matches!(c, PlatformCall::ReplyInThread { .. })),
        "no topic created for an owned session: {calls:?}"
    );
}

/// `/topic --adopt ... --force` steals a session mapped to another thread:
/// the other thread becomes sessionless, the new topic owns it.
#[tokio::test]
async fn topic_adopt_force_steals_mapping() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let mut backend = MockBackend::new(realistic_parts());
    backend.session_list = vec![list_session("ses_owned", "被占用的会话", "/work/auth", 100)];
    let (app, _platform) = build_app(cfg, backend).await;
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

    crate::bridge::command::handle_command(
        &app.core,
        Command::TopicAdopt {
            keyword: "ses_owned".into(),
            force: true,
        },
        crate::config::ThreadKey::new("chat_1".into(), "chat_1".into()),
        "msg_topic_adopt",
        crate::config::ConversationKind::P2p,
    )
    .await
    .unwrap();

    let topic_key = crate::config::ThreadKey::new("chat_1".into(), "omt_created_topic".into());
    assert_eq!(
        app.sessions
            .lock()
            .await
            .get_active(&topic_key)
            .map(|e| e.session_id.as_str()),
        Some("ses_owned"),
        "new topic owns the stolen session"
    );
    let other_key = crate::config::ThreadKey::new("oc_group_other".into(), "oc_group_other".into());
    assert!(
        app.sessions.lock().await.get_active(&other_key).is_none(),
        "the old owner thread becomes sessionless"
    );
}

/// `/topic --adopt` (no arg) pops the session-picker card (the `/switch`
/// card), whose rows carry the "建话题接管" button.
#[tokio::test]
async fn topic_adopt_no_arg_sends_switch_card() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let mut backend = MockBackend::new(realistic_parts());
    backend.session_list = vec![list_session("ses_alpha01", "重写登录", "/work/auth", 100)];
    let (app, platform) = build_app(cfg, backend).await;

    crate::bridge::command::handle_command(
        &app.core,
        Command::TopicAdoptCard,
        crate::config::ThreadKey::new("chat_1".into(), "chat_1".into()),
        "msg_card",
        crate::config::ConversationKind::P2p,
    )
    .await
    .unwrap();

    let card = platform
        .replied_cards()
        .await
        .into_iter()
        .next()
        .expect("a session card is sent");
    let card_str = card.to_string();
    assert!(
        card_str.contains("建话题接管"),
        "card rows offer 建话题接管: {card_str}"
    );
    assert!(
        card_str.contains("ses_alpha01"),
        "card lists the session: {card_str}"
    );
    // The "建话题接管" row button carries the structured op payload.
    let values = platform.button_values().await;
    assert!(
        values
            .iter()
            .any(|v| v["op"] == "topic_adopt" && v["session_id"] == "ses_alpha01"),
        "the picker rows need a topic_adopt button: {values:?}"
    );
}

/// The switch card's "建话题接管" op opens a topic anchored on the card's own
/// message (`open_message_id`) and maps the session to the new topic key.
#[tokio::test]
async fn switch_card_topic_adopt_action_creates_topic() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let mut backend = MockBackend::new(realistic_parts());
    backend.session_list = vec![list_session("ses_alpha01", "重写登录", "/work/auth", 100)];
    let (app, _platform) = build_app(cfg, backend).await;

    let value = serde_json::json!({
        "action": "switch",
        "op": "topic_adopt",
        "chat_id": "chat_1",
        "thread_id": "chat_1",
        "session_id": "ses_alpha01",
        "open_message_id": "om_switch_card",
    });
    let result = app
        .handle_card_action(value)
        .await
        .expect("topic_adopt should return a result");
    assert!(result.card.is_some(), "topic_adopt refreshes the card");
    assert!(
        result.toast.clone().unwrap_or_default().contains("话题"),
        "topic_adopt toasts: {:?}",
        result.toast
    );

    // The new topic (thread_id from the mock reply_in_thread) owns the session.
    let topic_key = crate::config::ThreadKey::new("chat_1".into(), "omt_created_topic".into());
    let entry = app
        .sessions
        .lock()
        .await
        .get_active(&topic_key)
        .cloned()
        .expect("topic_adopt maps the session to the new topic");
    assert_eq!(entry.session_id, "ses_alpha01");
    assert_eq!(entry.directory, "/work/auth");
    assert_eq!(entry.topic_anchor.as_deref(), Some("msg_topic_reply"));
    // The lobby thread itself got no session.
    let lobby_key = crate::config::ThreadKey::new("chat_1".into(), "chat_1".into());
    assert!(app.sessions.lock().await.get_active(&lobby_key).is_none());
}

/// The switch card's "建话题接管" op on a session mapped to another thread
/// patches the card to the force-confirm card (强制建话题接管) instead of
/// dead-ending in a "go type /topic --adopt <ID> --force" Toast.
#[tokio::test]
async fn switch_card_topic_adopt_occupied_offers_force_confirm() {
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
        "op": "topic_adopt",
        "chat_id": "chat_1",
        "thread_id": "chat_1",
        "session_id": "ses_owned",
        "open_message_id": "om_switch_card",
    });
    let result = app
        .handle_card_action(value)
        .await
        .expect("topic_adopt should return a result");
    let card = result
        .card
        .clone()
        .expect("occupied topic_adopt returns the confirm card");
    let card_str = card.to_string();
    assert!(card_str.contains("强制建话题接管"), "force button: {card_str}");
    assert!(
        card_str.contains("force_topic_adopt"),
        "force op payload: {card_str}"
    );
    assert!(card_str.contains("隔壁群"), "owner named: {card_str}");
    assert!(
        result.toast.clone().unwrap_or_default().contains("占用"),
        "occupied session toasts: {:?}",
        result.toast
    );
    // The lobby thread got no session and no new topic was created for it.
    let lobby_key = crate::config::ThreadKey::new("chat_1".into(), "chat_1".into());
    assert!(app.sessions.lock().await.get_active(&lobby_key).is_none());
    let topic_key = crate::config::ThreadKey::new("chat_1".into(), "omt_created_topic".into());
    assert!(
        app.sessions.lock().await.get_active(&topic_key).is_none(),
        "no new topic mapping for an owned session"
    );
}

/// Clicking 强制建话题接管 steals the mapping and opens the topic around the
/// stolen session (the card equivalent of `/topic --adopt <id> --force`).
#[tokio::test]
async fn switch_card_force_topic_adopt_steals_owned_session() {
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
        "op": "force_topic_adopt",
        "chat_id": "chat_1",
        "thread_id": "chat_1",
        "session_id": "ses_owned",
        "open_message_id": "om_switch_card",
    });
    let result = app
        .handle_card_action(value)
        .await
        .expect("force_topic_adopt should return a result");
    assert!(result.card.is_some(), "force_topic_adopt refreshes the card");
    let topic_key = crate::config::ThreadKey::new("chat_1".into(), "omt_created_topic".into());
    assert_eq!(
        app.sessions
            .lock()
            .await
            .get_active(&topic_key)
            .expect("the stolen session maps to the new topic")
            .session_id,
        "ses_owned"
    );
    assert!(
        app.sessions.lock().await.get_active(&other).is_none(),
        "the old owner becomes sessionless"
    );
}

/// A failed topic creation during a forced topic-adopt must NOT strand the
/// session: the steal is the mapping write itself, so when the platform
/// returns no thread_id the old owner keeps the session.
#[tokio::test]
async fn switch_card_force_topic_adopt_failure_keeps_old_owner() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let mut backend = MockBackend::new(realistic_parts());
    backend.session_list = vec![list_session("ses_owned", "被占用的会话", "/work/auth", 100)];
    let mut platform = RecordingPlatform::new();
    // No topic support: `reply_in_thread` returns no thread_id, so the topic
    // creation fails before any mapping write.
    platform.reply_in_thread_thread_id = None;
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
        "op": "force_topic_adopt",
        "chat_id": "chat_1",
        "thread_id": "chat_1",
        "session_id": "ses_owned",
        "open_message_id": "om_switch_card",
    });
    let result = app
        .handle_card_action(value)
        .await
        .expect("force_topic_adopt should return a result");
    assert!(
        result.toast.clone().unwrap_or_default().contains("创建话题失败"),
        "failure surfaces a toast: {:?}",
        result.toast
    );
    assert_eq!(
        app.sessions
            .lock()
            .await
            .get_active(&other)
            .expect("the old owner keeps the session when the topic cannot open")
            .session_id,
        "ses_owned"
    );
}

/// The switch card's "建话题接管" op fails gracefully when the card action
/// carries no `open_message_id` (the anchor needed to create the topic).
#[tokio::test]
async fn switch_card_topic_adopt_missing_open_message_id() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let mut backend = MockBackend::new(realistic_parts());
    backend.session_list = vec![list_session("ses_alpha01", "重写登录", "/work/auth", 100)];
    let (app, _platform) = build_app(cfg, backend).await;

    let value = serde_json::json!({
        "action": "switch",
        "op": "topic_adopt",
        "chat_id": "chat_1",
        "thread_id": "chat_1",
        "session_id": "ses_alpha01",
    });
    let result = app
        .handle_card_action(value)
        .await
        .expect("topic_adopt should return a result");
    assert_eq!(result.card, None, "no card refresh on failure");
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

/// The switch card's "建话题接管" op is rejected inside a topic thread too
/// (ADR-0025 retrofit): the session card may legally open in a never-bound
/// topic, but its topic op must not nest — same rule as the text forms.
#[tokio::test]
async fn switch_card_topic_adopt_rejects_inside_topic() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let mut backend = MockBackend::new(realistic_parts());
    backend.session_list = vec![list_session("ses_alpha01", "重写登录", "/work/auth", 100)];
    let (app, _platform) = build_app(cfg, backend).await;

    let value = serde_json::json!({
        "action": "switch",
        "op": "topic_adopt",
        "chat_id": "chat_1",
        "thread_id": "omt_t_1",
        "session_id": "ses_alpha01",
        "open_message_id": "om_switch_card",
    });
    let result = app
        .handle_card_action(value)
        .await
        .expect("topic_adopt should return a result");
    assert_eq!(result.card, None, "no card refresh on rejection");
    assert!(
        result.toast.clone().unwrap_or_default().contains("回主对话操作"),
        "nested topic_adopt is rejected with a Toast: {:?}",
        result.toast
    );
    let topic_key = crate::config::ThreadKey::new("chat_1".into(), "omt_created_topic".into());
    assert!(
        app.sessions.lock().await.get_active(&topic_key).is_none(),
        "no new topic mapping inside a topic"
    );
}

/// `/topic` with a nonexistent directory replies a clear error and creates
/// neither a topic nor a session (spec: validate before creating).
#[tokio::test]
async fn topic_command_rejects_nonexistent_directory() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let (app, platform) = build_app(cfg, MockBackend::new(realistic_parts())).await;

    crate::bridge::command::handle_command(
        &app.core,
        Command::Topic {
            directory: Some("/nonexistent/dir/xyz".into()),
            name: None,
        },
        crate::config::ThreadKey::new("chat_1".into(), "chat_1".into()),
        "msg_topic",
        crate::config::ConversationKind::P2p,
    )
    .await
    .unwrap();

    let calls = platform.calls.lock().await.clone();
    let text = platform.texts().await.join("\n");
    assert!(
        text.contains("目录不存在") && text.contains("/nonexistent/dir/xyz"),
        "/topic with a bad dir must reply a clear error: {text}"
    );
    // No topic was created and nothing was mapped.
    assert!(
        !calls
            .iter()
            .any(|c| matches!(c, PlatformCall::ReplyInThread { .. })),
        "a topic must not be created for a bad directory: {calls:?}"
    );
    assert!(
        app.sessions.lock().await.all_entries().is_empty(),
        "no session mapping should be created: {:?}",
        app.sessions.lock().await.all_entries()
    );
}

#[tokio::test]
async fn fresh_topic_attach_adopts_with_in_topic_anchor() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let mut backend = MockBackend::new(realistic_parts());
    backend.session_list = vec![list_session("ses_foreign123abc", "外部会话", "/work/ext", 100)];
    let (app, platform) = build_app(cfg, backend).await;
    let topic_key = crate::config::ThreadKey::new("chat_1".into(), "omt_fresh".into());

    crate::bridge::command::handle_command(
        &app.core,
        Command::Switch(SwitchAction::Attach {
            query: "ses_foreign123abc".into(),
            force: false,
        }),
        topic_key.clone(),
        "msg_topic_cmd",
        crate::config::ConversationKind::Topic,
    )
    .await
    .unwrap();

    // ADR-0028: the in-topic adoption confirmation is the Session Snapshot
    // card, sent inside the topic — no 「📎 已接管…」 text anchor anymore.
    let calls = platform.calls.lock().await.clone();
    let seed = calls
        .iter()
        .find_map(|c| match c {
            PlatformCall::ReplyCardInThread { message_id, card, .. } if message_id == "msg_topic_cmd" => {
                Some(card.to_string())
            }
            _ => None,
        })
        .expect("snapshot replied in-thread on the command: {calls:?}");
    assert!(seed.contains("已接管 外部会话"), "snapshot header: {seed}");
    assert!(
        !calls
            .iter()
            .any(|c| matches!(c, PlatformCall::ReplyInThread { .. })),
        "no text anchor alongside the snapshot: {calls:?}"
    );

    // Adopted as the topic's single session, anchored to the snapshot card
    // (the mock's created topic-reply message id).
    let entry = app.sessions.lock().await.get_active(&topic_key).cloned().unwrap();
    assert_eq!(entry.session_id, "ses_foreign123abc");
    assert_eq!(entry.topic_anchor.as_deref(), Some("msg_topic_reply"));
}

/// ADR-0023: a plain message in a cola-created topic carries `parent_id`
/// pointing at the topic's own creation messages — the thread root (the
/// user's `/topic` command) or the seed card (`topic_anchor`). Neither must
/// be injected as Quoted Context: both are boilerplate, and injecting them
/// pollutes every prompt in the topic.
#[tokio::test]
async fn topic_plain_reply_skips_own_root_and_seed_injection() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let backend = MockBackend::new(realistic_parts());
    let prompt_calls = backend.prompt_calls.clone();
    let platform = RecordingPlatform::new();
    for (id, text) in [
        ("om_root_cmd", "📌 已创建会话 `api-refactor`"),
        ("om_seed", "📌 已创建会话 `api-refactor`\n请在本话题内回复"),
    ] {
        platform.quoted_messages.lock().unwrap().insert(
            id.into(),
            crate::feishu::client::FeishuMessage {
                msg_type: "text".into(),
                content: format!(r#"{{"text":"{text}"}}"#),
                mentions: vec![],
            },
        );
    }
    let app = Arc::new(App::new(cfg, Arc::new(backend), Arc::new(platform)).unwrap());
    // The cola-created topic already owns a session; its creation messages
    // are the thread root (the `/topic` command) and the seed card.
    seed_entry(
        &app,
        crate::config::SessionEntry {
            thread_key: crate::config::ThreadKey::new("chat_1".into(), "omt_t_1".into()),
            session_id: "ses_topic".into(),
            directory: "/work/topic".into(),
            agent: None,
            model: None,
            auto_accept: false,
            topic_anchor: Some("om_seed".into()),
            topic_root: Some("om_root_cmd".into()),
            variant: None,
        },
    )
    .await;

    for pid in ["om_root_cmd", "om_seed"] {
        prompt_calls.lock().await.clear();
        app.handle_message(crate::bridge::IncomingMessage {
            message_id: format!("msg_{pid}"),
            chat_id: "chat_1".into(),
            chat_type: "group".into(),
            thread_id: Some("omt_t_1".into()),
            parent_id: Some(pid.into()),
            text: "普通回复".into(),
            images: vec![],
            requester_open_id: None,
        })
        .await;
        assert_eq!(
            *prompt_calls.lock().await,
            vec!["普通回复".to_string()],
            "parent {pid} is the topic's own creation message — must not be injected"
        );
    }
}

/// ADR-0023: a genuine quote of a real message inside a topic still injects
/// — only the topic's own creation messages are excluded.
#[tokio::test]
async fn topic_explicit_quote_of_real_message_still_injects() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let backend = MockBackend::new(realistic_parts());
    let prompt_calls = backend.prompt_calls.clone();
    let platform = RecordingPlatform::new();
    platform.quoted_messages.lock().unwrap().insert(
        "om_real_quote".into(),
        crate::feishu::client::FeishuMessage {
            msg_type: "text".into(),
            content: r#"{"text":"真正的上下文"}"#.into(),
            mentions: vec![],
        },
    );
    let app = Arc::new(App::new(cfg, Arc::new(backend), Arc::new(platform)).unwrap());
    seed_entry(
        &app,
        crate::config::SessionEntry {
            thread_key: crate::config::ThreadKey::new("chat_1".into(), "omt_t_1".into()),
            session_id: "ses_topic".into(),
            directory: "/work/topic".into(),
            agent: None,
            model: None,
            auto_accept: false,
            topic_anchor: Some("om_seed".into()),
            topic_root: Some("om_root_cmd".into()),
            variant: None,
        },
    )
    .await;

    app.handle_message(crate::bridge::IncomingMessage {
        message_id: "msg_q".into(),
        chat_id: "chat_1".into(),
        chat_type: "group".into(),
        thread_id: Some("omt_t_1".into()),
        parent_id: Some("om_real_quote".into()),
        text: "继续".into(),
        images: vec![],
        requester_open_id: None,
    })
    .await;

    assert_eq!(
        *prompt_calls.lock().await,
        vec!["[引用消息]:\n真正的上下文\n\n继续".to_string()]
    );
}

/// ADR-0023: a manually-created topic has neither `topic_root` nor
/// `topic_anchor`, so the guard is silent and the user's own subject
/// message (the topic's root) still injects — it is context, not
/// boilerplate.
#[tokio::test]
async fn manual_topic_plain_reply_injects_user_root() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let backend = MockBackend::new(realistic_parts());
    let prompt_calls = backend.prompt_calls.clone();
    let platform = RecordingPlatform::new();
    platform.quoted_messages.lock().unwrap().insert(
        "om_user_root".into(),
        crate::feishu::client::FeishuMessage {
            msg_type: "text".into(),
            content: r#"{"text":"用户的主题消息"}"#.into(),
            mentions: vec![],
        },
    );
    let app = Arc::new(App::new(cfg, Arc::new(backend), Arc::new(platform)).unwrap());
    seed_entry(
        &app,
        crate::config::SessionEntry {
            thread_key: crate::config::ThreadKey::new("chat_1".into(), "omt_t_1".into()),
            session_id: "ses_topic".into(),
            directory: "/work/topic".into(),
            agent: None,
            model: None,
            auto_accept: false,
            topic_anchor: None,
            topic_root: None,
            variant: None,
        },
    )
    .await;

    app.handle_message(crate::bridge::IncomingMessage {
        message_id: "msg_manual".into(),
        chat_id: "chat_1".into(),
        chat_type: "group".into(),
        thread_id: Some("omt_t_1".into()),
        parent_id: Some("om_user_root".into()),
        text: "继续".into(),
        images: vec![],
        requester_open_id: None,
    })
    .await;

    assert_eq!(
        *prompt_calls.lock().await,
        vec!["[引用消息]:\n用户的主题消息\n\n继续".to_string()]
    );
}
