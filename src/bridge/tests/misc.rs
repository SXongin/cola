use crate::bridge::command::*;
use crate::bridge::test_support::*;
use crate::bridge::turn::Turn;
use crate::feishu::card::CardState;

/// `/model`, `/agent`, `/stop` and `/compact` used to silently no-op on a
/// thread with no mapped session; they now reply a hint instead.
#[tokio::test]
async fn session_commands_reply_hint_without_mapped_session() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let (app, platform) = build_app(cfg, MockBackend::new(realistic_parts())).await;
    let key = crate::config::ThreadKey::new("chat_1".into(), "chat_1".into());
    let kind = crate::config::ConversationKind::P2p;

    for (text, msg) in [
        ("/model opencode-go/deepseek-v4-flash", "msg_m"),
        ("/agent build", "msg_a"),
        ("/stop", "msg_s"),
        ("/compact", "msg_c"),
    ] {
        send_command_in(&app, text, key.clone(), msg, kind).await;
    }

    let text = platform.texts().await.join("\n");
    assert!(text.contains("还没有会话"), "every command must hint: {text}");
}

/// The `/stop` marker's rollback (#394): a failed interrupt rolls back only a
/// marker THIS call inserted. A stop whose abort failed must not leave a
/// paused card claiming 已停止; a failed second `/stop` must not wipe the
/// marker an earlier successful stop left for the still-draining turn.
#[tokio::test]
async fn a_failed_stop_rolls_back_only_its_own_marker() {
    for (preset, expected) in [(false, false), (true, true)] {
        let _wd = test_work_dir();
        let dir = tempfile::tempdir().unwrap();
        let cfg = test_config(&dir.path().join("sessions.json"));
        let mut backend = MockBackend::new(realistic_parts());
        backend.fail_interrupts(1, "abort endpoint down");
        let (app, _platform) = build_app(cfg, backend).await;
        seed_session(&app, "ses_test", "/work").await;
        if preset {
            app.stopped_sessions.lock().await.insert("ses_test".to_string());
        }

        send_command_in(
            &app,
            "/stop",
            crate::config::ThreadKey::new("chat_1".into(), "chat_1".into()),
            "msg_stop",
            crate::config::ConversationKind::P2p,
        )
        .await;

        assert_eq!(
            app.stopped_sessions.lock().await.contains("ses_test"),
            expected,
            "preset={preset}: a failed stop must roll back only the marker it inserted"
        );
    }
}

/// A `/stop` with a mapped session but no live card (an idle session) has
/// nothing to finalize, so the command itself says what happened. The
/// interrupt still goes out: the run may be live server-side even when cola's
/// card is not.
#[tokio::test]
async fn a_stop_without_a_live_card_replies_that_nothing_ran() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let backend = MockBackend::new(realistic_parts());
    let interrupt_calls = backend.interrupt_calls.clone();
    let (app, platform) = build_app(cfg, backend).await;
    seed_session(&app, "ses_test", "/work").await;

    send_command_in(
        &app,
        "/stop",
        crate::config::ThreadKey::new("chat_1".into(), "chat_1".into()),
        "msg_stop",
        crate::config::ConversationKind::P2p,
    )
    .await;

    assert_eq!(
        interrupt_calls.lock().await.as_slice(),
        &["ses_test".to_string()],
        "the interrupt still goes out with no live card"
    );
    let texts = platform.texts().await.join("\n");
    assert!(
        texts.contains(NO_RUNNING_TASK),
        "an invisible stop must say what happened: {texts}"
    );
}

/// The same reply covers a card that is already terminal: it will never stamp
/// the stop, and no running turn may be implied. The command leaves it alone.
#[tokio::test]
async fn a_stop_over_a_terminal_card_replies_that_nothing_ran() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let backend = MockBackend::new(realistic_parts());
    let interrupt_calls = backend.interrupt_calls.clone();
    let (app, platform) = build_app(cfg, backend).await;
    seed_session(&app, "ses_test", "/work").await;
    Turn::seed_card(&app.cards_handle(), "ses_test", Some("om_card")).await;
    Turn::set_card_state(&app.cards_handle(), "ses_test", CardState::Done).await;

    send_command_in(
        &app,
        "/stop",
        crate::config::ThreadKey::new("chat_1".into(), "chat_1".into()),
        "msg_stop",
        crate::config::ConversationKind::P2p,
    )
    .await;

    assert_eq!(
        interrupt_calls.lock().await.as_slice(),
        &["ses_test".to_string()],
        "the interrupt still goes out over a terminal card"
    );
    let texts = platform.texts().await.join("\n");
    assert!(
        texts.contains(NO_RUNNING_TASK),
        "a terminal card cannot render the stop: {texts}"
    );
    assert_eq!(
        Turn::card_state(&app.cards_handle(), "ses_test").await,
        Some(CardState::Done),
        "a terminal card is never restamped by the command"
    );
}

/// A `/stop` over a card that yielded 「⏳ 等待后台任务」 must ack: the Turn has
/// no render loop, and the card only settles at the quiet true end — after the
/// last Background Task retires (ADR-0059/0060) — so the command names the
/// ending that will land then and leaves the card waiting.
#[tokio::test]
async fn a_stop_over_a_waiting_card_acks_the_deferred_settle() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let backend = MockBackend::new(realistic_parts());
    let interrupt_calls = backend.interrupt_calls.clone();
    let (app, platform) = build_app(cfg, backend).await;
    seed_session(&app, "ses_test", "/work").await;
    Turn::seed_card(&app.cards_handle(), "ses_test", Some("om_card")).await;
    Turn::set_card_state(&app.cards_handle(), "ses_test", CardState::Waiting).await;

    send_command_in(
        &app,
        "/stop",
        crate::config::ThreadKey::new("chat_1".into(), "chat_1".into()),
        "msg_stop",
        crate::config::ConversationKind::P2p,
    )
    .await;

    assert_eq!(
        interrupt_calls.lock().await.as_slice(),
        &["ses_test".to_string()],
        "the interrupt still goes out over a waiting card"
    );
    let texts = platform.texts().await.join("\n");
    assert!(
        texts.contains(STOPPED_WAITING),
        "a deferred settle must be acked: {texts}"
    );
    assert_eq!(
        Turn::card_state(&app.cards_handle(), "ses_test").await,
        Some(CardState::Waiting),
        "the command does not restamp the waiting card — the quiet true end does"
    );
}

/// A reply injects its parent's text as Quoted Context, prefixed ahead of
/// the user's own message so the model sees what the reply answers.
#[tokio::test]
async fn reply_injects_quoted_parent_text() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let backend = MockBackend::new(realistic_parts());
    let prompt_calls = backend.prompt_calls.clone();
    let prompt_files = backend.prompt_files.clone();
    let platform = RecordingPlatform::new();
    platform.quoted_messages.lock().unwrap().insert(
        "om_parent".into(),
        crate::feishu::client::FeishuMessage {
            msg_type: "text".into(),
            content: r#"{"text":"父消息：看看这个报错 @_user_1"}"#.into(),
            mentions: vec![crate::feishu::event::Mention {
                key: Some("@_user_1".into()),
                id: Some(crate::feishu::event::MentionId {
                    open_id: Some("ou_other".into()),
                }),
                name: Some("李明".into()),
            }],
        },
    );
    let app = Arc::new(App::new(cfg, Arc::new(backend), Arc::new(platform)).unwrap());

    app.handle_message(crate::bridge::IncomingMessage {
        message_id: "msg_reply".into(),
        chat_id: "chat_1".into(),
        chat_type: "p2p".into(),
        thread_id: None,
        parent_id: Some("om_parent".into()),
        text: "还是不行".into(),
        images: vec![],
        files: vec![],
        omitted: vec![],
        requester_open_id: Some(TEST_HOST.into()),
    })
    .await;

    let calls = prompt_calls.lock().await.clone();
    assert_eq!(
        calls,
        vec!["[引用消息]:\n父消息：看看这个报错 @李明\n\n还是不行".to_string()]
    );
    // The text parent carries no images.
    assert_eq!(*prompt_files.lock().await, vec![0]);
}

/// A reply to an IMAGE message downloads the quoted image and attaches it as
/// an Image Attachment (prompt_files > 0).
#[tokio::test]
async fn reply_to_image_downloads_quoted_image() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let backend = MockBackend::new(realistic_parts());
    let prompt_files = backend.prompt_files.clone();
    let platform = RecordingPlatform::new();
    platform.quoted_messages.lock().unwrap().insert(
        "om_img_parent".into(),
        crate::feishu::client::FeishuMessage {
            msg_type: "image".into(),
            content: r#"{"image_key":"img_q"}"#.into(),
            mentions: vec![],
        },
    );
    let app = Arc::new(App::new(cfg, Arc::new(backend), Arc::new(platform)).unwrap());

    app.handle_message(crate::bridge::IncomingMessage {
        message_id: "msg_r".into(),
        chat_id: "chat_1".into(),
        chat_type: "p2p".into(),
        thread_id: None,
        parent_id: Some("om_img_parent".into()),
        text: "把这里放大看看".into(),
        images: vec![],
        files: vec![],
        omitted: vec![],
        requester_open_id: Some(TEST_HOST.into()),
    })
    .await;

    assert_eq!(*prompt_files.lock().await, vec![1]);
}

/// An incoming image message attaches its downloaded image; the text is the
/// `[图片]` placeholder (never raw JSON).
#[tokio::test]
async fn image_message_attaches_image_and_placeholder() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let backend = MockBackend::new(realistic_parts());
    let prompt_calls = backend.prompt_calls.clone();
    let prompt_files = backend.prompt_files.clone();
    let app = Arc::new(App::new(cfg, Arc::new(backend), Arc::new(RecordingPlatform::new())).unwrap());

    app.handle_message(crate::bridge::IncomingMessage {
        message_id: "msg_img".into(),
        chat_id: "chat_1".into(),
        chat_type: "p2p".into(),
        thread_id: None,
        parent_id: None,
        text: "[图片]".into(),
        images: vec![crate::feishu::client::ImageAttachment {
            mime: "image/png".into(),
            data: vec![1, 2, 3],
        }],
        files: vec![],
        omitted: vec![],
        requester_open_id: Some(TEST_HOST.into()),
    })
    .await;

    assert_eq!(*prompt_files.lock().await, vec![1]);
    assert_eq!(*prompt_calls.lock().await, vec!["[图片]".to_string()]);
}

/// A `file` message (a PDF) attaches its file and its body names it, so the
/// model receives the content and the user's own record shows the name
/// (ADR-0076's inbound amendment). Nothing is reported: the file attached.
#[tokio::test]
async fn file_message_attaches_file_and_names_it() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let backend = MockBackend::new(realistic_parts());
    let prompt_calls = backend.prompt_calls.clone();
    let prompt_files = backend.prompt_files.clone();
    let prompt_file_parts = backend.prompt_file_parts.clone();
    let (app, platform) = build_app(cfg, backend).await;

    app.handle_message(crate::bridge::IncomingMessage {
        message_id: "msg_file".into(),
        chat_id: "chat_1".into(),
        chat_type: "p2p".into(),
        thread_id: None,
        parent_id: None,
        text: "[文件: P3173R0.pdf]".into(),
        images: vec![],
        files: vec![crate::feishu::client::FileAttachment {
            mime: "application/pdf".into(),
            name: "P3173R0.pdf".into(),
            data: b"%PDF-1.7".to_vec(),
        }],
        omitted: vec![],
        requester_open_id: Some(TEST_HOST.into()),
    })
    .await;

    assert_eq!(*prompt_files.lock().await, vec![1]);
    assert_eq!(
        *prompt_calls.lock().await,
        vec!["[文件: P3173R0.pdf]".to_string()]
    );
    // The payload facts, not just the count: the classified mime, the display
    // name and the bytes.
    assert_eq!(
        *prompt_file_parts.lock().await,
        vec![vec![(
            "application/pdf".to_string(),
            Some("P3173R0.pdf".to_string()),
            {
                use base64::Engine as _;
                base64::engine::general_purpose::STANDARD.encode(b"%PDF-1.7")
            },
        )]]
    );
    assert!(
        platform.texts().await.is_empty(),
        "an attached file needs no warning: {:?}",
        platform.texts().await
    );
}

/// A file the server would not read (a `.docx`) is not attached: the body notes
/// the reason (in place, since a file message has no text of its own) and one
/// `⚠️` reply names it, so the user can tell it was not read.
#[tokio::test]
async fn unsupported_file_is_reported_not_attached() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let backend = MockBackend::new(realistic_parts());
    let prompt_calls = backend.prompt_calls.clone();
    let prompt_files = backend.prompt_files.clone();
    let (app, platform) = build_app(cfg, backend).await;

    app.handle_message(crate::bridge::IncomingMessage {
        message_id: "msg_docx".into(),
        chat_id: "chat_1".into(),
        chat_type: "p2p".into(),
        thread_id: None,
        parent_id: None,
        text: "[文件: x.docx]".into(),
        images: vec![],
        files: vec![crate::feishu::client::FileAttachment {
            mime: "application/octet-stream".into(),
            name: "x.docx".into(),
            data: b"docx".to_vec(),
        }],
        omitted: vec![],
        requester_open_id: Some(TEST_HOST.into()),
    })
    .await;

    assert_eq!(*prompt_files.lock().await, vec![0], "nothing attached");
    assert_eq!(
        *prompt_calls.lock().await,
        vec!["[文件: x.docx · 未附带：类型不支持]".to_string()]
    );
    let texts = platform.texts().await.join("\n");
    assert!(
        texts.contains("⚠️ 文件 x.docx 未能附带（类型不支持）"),
        "the user must be told: {texts}"
    );
}

/// A text-like file served with a generic content type is normalized to
/// `text/plain` (its name decides) and attached.
#[tokio::test]
async fn a_generic_typed_text_file_is_attached_as_text() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let backend = MockBackend::new(realistic_parts());
    let prompt_files = backend.prompt_files.clone();
    let prompt_file_parts = backend.prompt_file_parts.clone();
    let (app, _platform) = build_app(cfg, backend).await;

    app.handle_message(crate::bridge::IncomingMessage {
        message_id: "msg_md".into(),
        chat_id: "chat_1".into(),
        chat_type: "p2p".into(),
        thread_id: None,
        parent_id: None,
        text: "看看这个".into(),
        images: vec![],
        files: vec![crate::feishu::client::FileAttachment {
            mime: "application/octet-stream".into(),
            name: "notes.md".into(),
            data: b"# hi".to_vec(),
        }],
        omitted: vec![],
        requester_open_id: Some(TEST_HOST.into()),
    })
    .await;

    assert_eq!(*prompt_files.lock().await, vec![1]);
    // Normalized to `text/plain` (the extension decided, the header was
    // generic), carrying the name and the bytes.
    assert_eq!(
        *prompt_file_parts.lock().await,
        vec![vec![("text/plain".to_string(), Some("notes.md".to_string()), {
            use base64::Engine as _;
            base64::engine::general_purpose::STANDARD.encode(b"# hi")
        },)]]
    );
}

/// An oversize attachment (past the server's 20MB cap) is not attached — the
/// whole turn would fail — and is reported instead, for a file and (newly) for
/// an image alike.
#[tokio::test]
async fn an_oversize_attachment_is_reported_and_the_turn_continues() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let backend = MockBackend::new(realistic_parts());
    let prompt_calls = backend.prompt_calls.clone();
    let prompt_files = backend.prompt_files.clone();
    let (app, platform) = build_app(cfg, backend).await;

    app.handle_message(crate::bridge::IncomingMessage {
        message_id: "msg_big".into(),
        chat_id: "chat_1".into(),
        chat_type: "p2p".into(),
        thread_id: None,
        parent_id: None,
        text: "[文件: big.pdf]".into(),
        images: vec![],
        files: vec![crate::feishu::client::FileAttachment {
            mime: "application/pdf".into(),
            name: "big.pdf".into(),
            data: vec![0u8; 20 * 1024 * 1024 + 1],
        }],
        omitted: vec![],
        requester_open_id: Some(TEST_HOST.into()),
    })
    .await;

    assert_eq!(*prompt_files.lock().await, vec![0]);
    assert_eq!(
        *prompt_calls.lock().await,
        vec!["[文件: big.pdf · 未附带：超过 20MB]".to_string()]
    );
    let texts = platform.texts().await.join("\n");
    assert!(
        texts.contains("⚠️ 文件 big.pdf 未能附带（超过 20MB）"),
        "the turn must continue and report: {texts}"
    );
}

/// An image outside the four mimes the server forwards (a `bmp`) is not
/// attached and is reported — it would otherwise be silently discarded on V2.
#[tokio::test]
async fn a_non_four_image_is_reported() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let backend = MockBackend::new(realistic_parts());
    let prompt_calls = backend.prompt_calls.clone();
    let prompt_files = backend.prompt_files.clone();
    let (app, platform) = build_app(cfg, backend).await;

    app.handle_message(crate::bridge::IncomingMessage {
        message_id: "msg_bmp".into(),
        chat_id: "chat_1".into(),
        chat_type: "p2p".into(),
        thread_id: None,
        parent_id: None,
        text: "[图片]".into(),
        images: vec![crate::feishu::client::ImageAttachment {
            mime: "image/bmp".into(),
            data: b"BM".to_vec(),
        }],
        files: vec![],
        omitted: vec![],
        requester_open_id: Some(TEST_HOST.into()),
    })
    .await;

    assert_eq!(*prompt_files.lock().await, vec![0]);
    assert_eq!(
        *prompt_calls.lock().await,
        vec!["[图片 · 未附带：类型不支持]".to_string()]
    );
    let texts = platform.texts().await.join("\n");
    assert!(
        texts.contains("⚠️ 图片 未能附带（类型不支持）"),
        "the user must be told: {texts}"
    );
}

/// A ref the platform extracted but could not download (a failed download, a
/// folder entry) is reported as an omission, appended on its own line when the
/// message has its own text.
#[tokio::test]
async fn an_omitted_ref_is_appended_after_the_text() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let backend = MockBackend::new(realistic_parts());
    let prompt_calls = backend.prompt_calls.clone();
    let (app, platform) = build_app(cfg, backend).await;

    app.handle_message(crate::bridge::IncomingMessage {
        message_id: "msg_post".into(),
        chat_id: "chat_1".into(),
        chat_type: "p2p".into(),
        thread_id: None,
        parent_id: None,
        text: "我发这个文件吧".into(),
        images: vec![],
        files: vec![],
        omitted: vec![crate::bridge::OmittedAttachment {
            name: Some("docs".into()),
            reason: crate::bridge::OmitReason::Unsupported,
        }],
        requester_open_id: Some(TEST_HOST.into()),
    })
    .await;

    assert_eq!(
        *prompt_calls.lock().await,
        vec!["我发这个文件吧\n[文件: docs · 未附带：类型不支持]".to_string()]
    );
    let texts = platform.texts().await.join("\n");
    assert!(texts.contains("⚠️ 文件 docs 未能附带（类型不支持）"), "{texts}");
}

/// A failed download is reported as `下载失败` (the mapping the receive path
/// makes), not silently dropped or confused with an unsupported type.
#[tokio::test]
async fn a_failed_download_is_reported_as_download_failed() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let backend = MockBackend::new(realistic_parts());
    let prompt_calls = backend.prompt_calls.clone();
    let (app, platform) = build_app(cfg, backend).await;

    app.handle_message(crate::bridge::IncomingMessage {
        message_id: "msg_fail".into(),
        chat_id: "chat_1".into(),
        chat_type: "p2p".into(),
        thread_id: None,
        parent_id: None,
        text: "[文件: broken.pdf]".into(),
        images: vec![],
        files: vec![],
        omitted: vec![crate::bridge::OmittedAttachment {
            name: Some("broken.pdf".into()),
            reason: crate::bridge::OmitReason::DownloadFailed,
        }],
        requester_open_id: Some(TEST_HOST.into()),
    })
    .await;

    assert_eq!(
        *prompt_calls.lock().await,
        vec!["[文件: broken.pdf · 未附带：下载失败]".to_string()]
    );
    let texts = platform.texts().await.join("\n");
    assert!(
        texts.contains("⚠️ 文件 broken.pdf 未能附带（下载失败）"),
        "{texts}"
    );
}

/// A quoted parent's attachment that downloads slower than the 1.5s text budget
/// still keeps the quoted TEXT: the budgets are split, so the text no longer
/// waits behind the download (ADR-0076's inbound amendment). Under the old
/// single 1500ms budget the whole quoted context — text included — was
/// discarded.
#[tokio::test]
async fn a_slow_quoted_download_keeps_the_quoted_text() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let backend = MockBackend::new(realistic_parts());
    let prompt_calls = backend.prompt_calls.clone();
    let prompt_files = backend.prompt_files.clone();
    let platform = std::sync::Arc::new(RecordingPlatform::new());
    platform.quoted_messages.lock().unwrap().insert(
        "om_slow_parent".into(),
        crate::feishu::client::FeishuMessage {
            msg_type: "file".into(),
            content: r#"{"file_key":"fk_slow","file_name":"slow.pdf"}"#.into(),
            mentions: vec![],
        },
    );
    // The parent's download outlives the text's 1.5s budget.
    *platform.download_file_delay.lock().unwrap() = Some(std::time::Duration::from_millis(2000));
    let app = std::sync::Arc::new(App::new(cfg, std::sync::Arc::new(backend), platform.clone()).unwrap());

    app.handle_message(crate::bridge::IncomingMessage {
        message_id: "msg_slow".into(),
        chat_id: "chat_1".into(),
        chat_type: "p2p".into(),
        thread_id: None,
        parent_id: Some("om_slow_parent".into()),
        text: "看看这个".into(),
        images: vec![],
        files: vec![],
        omitted: vec![],
        requester_open_id: Some(TEST_HOST.into()),
    })
    .await;

    assert_eq!(
        *prompt_calls.lock().await,
        vec!["[引用消息]:\n[文件: slow.pdf]\n\n看看这个".to_string()],
        "the quoted text must survive a slow attachment download"
    );
    assert_eq!(
        *prompt_files.lock().await,
        vec![1],
        "the attachment still lands once the download finishes"
    );
}

/// A reply to a parent carrying a file downloads that file too (Quoted Context
/// carries Prompt Attachments), and records the download call.
#[tokio::test]
async fn reply_to_file_downloads_the_quoted_file() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let backend = MockBackend::new(realistic_parts());
    let prompt_calls = backend.prompt_calls.clone();
    let prompt_files = backend.prompt_files.clone();
    let platform = std::sync::Arc::new(RecordingPlatform::new());
    platform.quoted_messages.lock().unwrap().insert(
        "om_file_parent".into(),
        crate::feishu::client::FeishuMessage {
            msg_type: "file".into(),
            content: r#"{"file_key":"fk_q","file_name":"q.pdf"}"#.into(),
            mentions: vec![],
        },
    );
    let app = std::sync::Arc::new(App::new(cfg, std::sync::Arc::new(backend), platform.clone()).unwrap());

    app.handle_message(crate::bridge::IncomingMessage {
        message_id: "msg_r".into(),
        chat_id: "chat_1".into(),
        chat_type: "p2p".into(),
        thread_id: None,
        parent_id: Some("om_file_parent".into()),
        text: "看看这个".into(),
        images: vec![],
        files: vec![],
        omitted: vec![],
        requester_open_id: Some(TEST_HOST.into()),
    })
    .await;

    assert_eq!(*prompt_files.lock().await, vec![1]);
    assert_eq!(
        *prompt_calls.lock().await,
        vec!["[引用消息]:\n[文件: q.pdf]\n\n看看这个".to_string()]
    );
    assert_eq!(
        platform.download_file_calls.lock().unwrap().clone(),
        vec![(
            "om_file_parent".to_string(),
            "fk_q".to_string(),
            "q.pdf".to_string()
        )]
    );
    // The quoted parent's file attached: nothing to report.
    assert!(platform.texts().await.is_empty(), "{:?}", platform.texts().await);
}

/// A quoted parent's unsupported file is reported too — the quoted text is
/// still injected, but the attachment it could not read is noted and warned, so
/// "explain this PDF" never silently answers about nothing.
#[tokio::test]
async fn reply_reports_an_unsupported_quoted_file() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let backend = MockBackend::new(realistic_parts());
    let prompt_calls = backend.prompt_calls.clone();
    let prompt_files = backend.prompt_files.clone();
    let platform = std::sync::Arc::new(RecordingPlatform::new());
    platform.quoted_messages.lock().unwrap().insert(
        "om_post_parent".into(),
        crate::feishu::client::FeishuMessage {
            msg_type: "post".into(),
            content: r#"{"content":[[{"tag":"text","text":"我发这个文件吧"}]],"files":[{"file_key":"fk_docx","file_name":"x.docx","is_folder":false}]}"#.into(),
            mentions: vec![],
        },
    );
    // The download succeeds but is generic-typed, so the name's extension is
    // what classifies it — a `.docx` is unsupported.
    platform.downloaded_files.lock().unwrap().insert(
        "fk_docx".into(),
        Some(crate::feishu::client::FileAttachment {
            mime: "application/octet-stream".into(),
            name: "x.docx".into(),
            data: b"docx".to_vec(),
        }),
    );
    let app = std::sync::Arc::new(App::new(cfg, std::sync::Arc::new(backend), platform.clone()).unwrap());

    app.handle_message(crate::bridge::IncomingMessage {
        message_id: "msg_r".into(),
        chat_id: "chat_1".into(),
        chat_type: "p2p".into(),
        thread_id: None,
        parent_id: Some("om_post_parent".into()),
        text: "看看这个".into(),
        images: vec![],
        files: vec![],
        omitted: vec![],
        requester_open_id: Some(TEST_HOST.into()),
    })
    .await;

    assert_eq!(*prompt_files.lock().await, vec![0], "nothing attached");
    assert_eq!(
        *prompt_calls.lock().await,
        vec!["[引用消息]:\n我发这个文件吧\n\n看看这个\n[文件: x.docx · 未附带：类型不支持]".to_string()]
    );
    let texts = platform.texts().await.join("\n");
    assert!(
        texts.contains("⚠️ 文件 x.docx 未能附带（类型不支持）"),
        "the quoted parent's file must be reported: {texts}"
    );
}

/// An unrecognized `/command` is forwarded as a prompt, and a file attached to
/// it rides along — the text merely looked like a command, so its attachment
/// must not be lost.
#[tokio::test]
async fn a_forwarded_command_keeps_its_attachment() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let backend = MockBackend::new(realistic_parts());
    let prompt_calls = backend.prompt_calls.clone();
    let prompt_files = backend.prompt_files.clone();
    let (app, _platform) = build_app(cfg, backend).await;

    app.handle_message(crate::bridge::IncomingMessage {
        message_id: "msg_fwd".into(),
        chat_id: "chat_1".into(),
        chat_type: "p2p".into(),
        thread_id: None,
        parent_id: None,
        text: "/please-read".into(),
        images: vec![],
        files: vec![crate::feishu::client::FileAttachment {
            mime: "application/pdf".into(),
            name: "P3173R0.pdf".into(),
            data: b"%PDF-1.7".to_vec(),
        }],
        omitted: vec![],
        requester_open_id: Some(TEST_HOST.into()),
    })
    .await;

    assert_eq!(*prompt_calls.lock().await, vec!["/please-read".to_string()]);
    assert_eq!(*prompt_files.lock().await, vec![1], "the file must ride along");
}

/// Quote-injection failures degrade to text-only (the pre-change behavior).
#[tokio::test]
async fn reply_degrades_when_quote_fetch_fails() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let backend = MockBackend::new(realistic_parts());
    let prompt_calls = backend.prompt_calls.clone();
    let app = Arc::new(App::new(cfg, Arc::new(backend), Arc::new(RecordingPlatform::new())).unwrap());

    // `om_missing` is not in `quoted_messages` → get_message fails → the
    // reply reaches the model without any quote prefix.
    app.handle_message(crate::bridge::IncomingMessage {
        message_id: "msg_d".into(),
        chat_id: "chat_1".into(),
        chat_type: "p2p".into(),
        thread_id: None,
        parent_id: Some("om_missing".into()),
        text: "hi".into(),
        images: vec![],
        files: vec![],
        omitted: vec![],
        requester_open_id: Some(TEST_HOST.into()),
    })
    .await;

    let calls = prompt_calls.lock().await.clone();
    assert_eq!(calls, vec!["hi".to_string()]);
}

/// A `/restart` (or `/update`) issued inside a Topic is announced back
/// INSIDE that topic by replying to the command message; the chat lobby
/// must not receive the card.
#[tokio::test]
async fn restart_announce_replies_inside_the_topic() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let (app, platform) = build_app(cfg, MockBackend::new(realistic_parts())).await;

    app.announce_restart(&RestartNotify {
        chat_id: "oc_1".into(),
        thread_id: Some("omt_1".into()),
        message_id: Some("om_cmd".into()),
        kind: RestartKind::Update,
        version: Some("0.9.0".into()),
    })
    .await;

    let calls = platform.calls.lock().await.clone();
    assert_eq!(calls.len(), 1, "expected exactly one card: {calls:?}");
    match &calls[0] {
        PlatformCall::ReplyCard { reply_to, card } => {
            assert_eq!(reply_to, "om_cmd");
            let text = card.to_string();
            assert!(text.contains("已更新到 0.9.0 并重启完成"), "got {text}");
        }
        other => panic!("expected a reply inside the topic, got {other:?}"),
    }
}

/// A lobby `/restart` keeps the standalone chat card (no topic to reply in).
#[tokio::test]
async fn restart_announce_lobby_sends_to_the_chat() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let (app, platform) = build_app(cfg, MockBackend::new(realistic_parts())).await;

    app.announce_restart(&RestartNotify {
        chat_id: "oc_1".into(),
        thread_id: Some("oc_1".into()),
        message_id: Some("om_cmd".into()),
        kind: RestartKind::Restart,
        version: None,
    })
    .await;

    let calls = platform.calls.lock().await.clone();
    assert_eq!(calls.len(), 1, "expected exactly one card: {calls:?}");
    match &calls[0] {
        PlatformCall::SendCard { receive_id, card } => {
            assert_eq!(receive_id, "oc_1");
            assert!(card_text(card).contains("已重启完成"));
        }
        other => panic!("expected a chat card, got {other:?}"),
    }
}

/// A topic reply that fails degrades to the chat card — the announcement is
/// never silently lost.
#[tokio::test]
async fn restart_announce_topic_failure_falls_back_to_chat() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let platform = Arc::new(RecordingPlatform {
        fail_reply_card: true,
        ..RecordingPlatform::new()
    });
    let app = Arc::new(
        App::new(
            cfg,
            Arc::new(MockBackend::new(realistic_parts())),
            platform.clone(),
        )
        .unwrap(),
    );

    app.announce_restart(&RestartNotify {
        chat_id: "oc_1".into(),
        thread_id: Some("omt_1".into()),
        message_id: Some("om_cmd".into()),
        kind: RestartKind::Restart,
        version: None,
    })
    .await;

    let calls = platform.calls.lock().await.clone();
    assert_eq!(calls.len(), 1, "expected the fallback card: {calls:?}");
    match &calls[0] {
        PlatformCall::SendCard { receive_id, .. } => assert_eq!(receive_id, "oc_1"),
        other => panic!("expected the chat fallback, got {other:?}"),
    }
}
