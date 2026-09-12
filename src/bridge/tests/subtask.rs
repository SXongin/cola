use crate::bridge::test_support::*;

#[tokio::test]
async fn subtask_permission_routes_to_mapped_parent_and_reply_carries_directory() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let mut backend = MockBackend::new(realistic_parts());
    // A sub-task session cola never created; its permission must be routed
    // up to the parent session's chat.
    let child = "ses_child_task";
    backend
        .session_parents
        .insert(child.into(), backend.session_id.clone());
    backend.permissions = vec![opencode::client::PermissionRequest {
        request_id: "per_child".into(),
        session_id: Some(child.into()),
        permission: Some("bash".into()),
        patterns: vec!["git status".into()],
        metadata: None,
        always: Vec::new(),
    }];
    let parent_id = backend.session_id.clone();
    let (app, _platform) = build_app(cfg, backend).await;

    // Seed the parent session so it maps child → parent → chat.
    app.handle_message(incoming(
        "msg_1".into(),
        "chat_1".into(),
        "p2p".into(),
        None,
        "hi".into(),
        None,
    ))
    .await;

    tokio::spawn({
        let app = app.clone();
        async move {
            app.permission
                .poll_interval_ms
                .store(50, std::sync::atomic::Ordering::Relaxed);
            let _ = app.permission.poll_loop(&app.core).await;
        }
    });
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;

    // The parent session has a live streaming card, so the child's
    // permission is INLINED on it (one-card-per-turn), not sent as a
    // separate card — the child itself has no accumulator, so it must be
    // hosted on the parent's card found by walking the parent chain.
    let perm_inline = app
        .cards
        .lock()
        .await
        .get(&parent_id)
        .expect("parent accumulator exists")
        .acc
        .pending_permissions
        .clone();
    assert_eq!(
        perm_inline.len(),
        1,
        "subtask permission should be inlined on the parent card"
    );
    assert_eq!(perm_inline[0].request_id, "per_child");
    assert_eq!(perm_inline[0].session_id, child);

    // The streaming card renders the inline section with the child's buttons.
    let card = app.cards.lock().await.get(&parent_id).unwrap().acc.build_card();
    let card_text = card.to_string();
    assert!(card_text.contains("权限请求"), "inline section missing");
    assert!(card_text.contains("git status"), "permission body missing");
    // The button carries the CHILD session id (the request's owner) plus the
    // owning directory so the reply routes to the right instance even though
    // the child session isn't in the store.
    let first_button = card["body"]["elements"]
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["tag"] == "button")
        .expect("permission buttons present");
    let value = first_button["value"].clone();
    assert_eq!(value["session_id"], child);
    assert!(
        value["directory"]
            .as_str()
            .map(|d| !d.is_empty())
            .unwrap_or(false),
        "permission card must carry a directory, got: {}",
        value
    );

    // Clicking Allow routes the reply with that directory and drops the
    // inline section (no replacement card — the streaming card re-renders).
    let mut value = value;
    value["reply"] = serde_json::json!("once");
    value["perm_label"] = serde_json::json!("✅ 已允许一次");
    value["perm_color"] = serde_json::json!("green");
    let result = app.handle_card_action(value).await;
    assert!(result.is_some(), "reply should succeed for subtask session");
    assert!(
        result.unwrap().card.is_none(),
        "inline answer must not replace the streaming card"
    );
    assert!(
        app.cards
            .lock()
            .await
            .get(&parent_id)
            .unwrap()
            .acc
            .pending_permissions
            .is_empty(),
        "inline permission section should be removed after answering"
    );
}

/// Without a live streaming card (e.g. the parent turn finished or cola
/// restarted), a sub-task child's permission falls back to a separate card
/// sent into the parent's chat — still routed up the parent chain, never
/// dropped.
#[tokio::test]
async fn subtask_permission_without_streaming_card_sends_card_to_parent_chat() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let mut backend = MockBackend::new(realistic_parts());
    let child = "ses_child_task";
    backend
        .session_parents
        .insert(child.into(), backend.session_id.clone());
    backend.permissions = vec![opencode::client::PermissionRequest {
        request_id: "per_child".into(),
        session_id: Some(child.into()),
        permission: Some("bash".into()),
        patterns: vec!["git status".into()],
        metadata: None,
        always: Vec::new(),
    }];
    let (app, platform) = build_app(cfg, backend).await;

    // Map the parent session to a chat WITHOUT an active accumulator (no
    // handle_message call — the turn is finished).
    {
        let mut store = app.sessions.lock().await;
        store.set_active(crate::config::SessionEntry {
            thread_key: crate::config::ThreadKey::new("chat_1".into(), "chat_1".into()),
            session_id: "ses_test".into(),
            directory: "/tmp/aa".into(),
            agent: None,
            model: None,
            auto_accept: false,
            topic_anchor: None,
            topic_root: None,
            variant: None,
        });
        store.persist().unwrap();
    }

    tokio::spawn({
        let app = app.clone();
        async move {
            app.permission
                .poll_interval_ms
                .store(50, std::sync::atomic::Ordering::Relaxed);
            let _ = app.permission.poll_loop(&app.core).await;
        }
    });
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;

    // No inline host: the permission becomes a separate card delivered into
    // the parent's chat (the child has no card of its own to host it).
    let calls = platform.calls.lock().await.clone();
    let perm_card = calls.iter().find_map(|c| match c {
        PlatformCall::SendCard { receive_id, card }
            if receive_id == "chat_1" && card.to_string().contains("git status") =>
        {
            Some(card.clone())
        }
        _ => None,
    });
    let perm_card =
        perm_card.expect("subtask permission should fall back to a separate card in the parent chat");
    // The card still carries the child's session id + owning directory.
    let first_button = perm_card["body"]["elements"]
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["tag"] == "button")
        .expect("permission card has buttons");
    let value = first_button["value"].clone();
    assert_eq!(value["session_id"], child);
    assert!(
        value["directory"]
            .as_str()
            .map(|d| !d.is_empty())
            .unwrap_or(false),
        "separate card must carry a directory, got: {}",
        value
    );
}
