use crate::bridge::command::*;
use crate::bridge::test_support::*;

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

    for (cmd, msg) in [
        (Command::Model("opencode-go/deepseek-v4-flash".into()), "msg_m"),
        (Command::Agent("build".into()), "msg_a"),
        (Command::Stop, "msg_s"),
        (Command::Compact, "msg_c"),
    ] {
        crate::bridge::command::handle_command(&app.core, cmd, key.clone(), msg, kind)
            .await
            .unwrap();
    }

    let text = platform
        .calls
        .lock()
        .await
        .clone()
        .into_iter()
        .filter_map(|c| match c {
            PlatformCall::ReplyText { text, .. } => Some(text),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n");
    assert!(text.contains("还没有会话"), "every command must hint: {text}");
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
    let prompt_images = backend.prompt_images.clone();
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
        requester_open_id: None,
    })
    .await;

    let calls = prompt_calls.lock().await.clone();
    assert_eq!(
        calls,
        vec!["[引用消息]:\n父消息：看看这个报错 @李明\n\n还是不行".to_string()]
    );
    // The text parent carries no images.
    assert_eq!(*prompt_images.lock().await, vec![0]);
}

/// A reply to an IMAGE message downloads the quoted image and attaches it as
/// an Image Attachment (prompt_images > 0).
#[tokio::test]
async fn reply_to_image_downloads_quoted_image() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let backend = MockBackend::new(realistic_parts());
    let prompt_images = backend.prompt_images.clone();
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
        requester_open_id: None,
    })
    .await;

    assert_eq!(*prompt_images.lock().await, vec![1]);
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
    let prompt_images = backend.prompt_images.clone();
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
        requester_open_id: None,
    })
    .await;

    assert_eq!(*prompt_images.lock().await, vec![1]);
    assert_eq!(*prompt_calls.lock().await, vec!["[图片]".to_string()]);
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
        requester_open_id: None,
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
            assert!(card.to_string().contains("已重启完成"));
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
