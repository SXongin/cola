use std::sync::Arc;

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
    let parent = backend.session_id.clone();
    backend.with_session_parent(child, &parent);
    backend.ask_permissions(vec![crate::backend::types::PermissionRequest {
        request_id: "per_child".into(),
        session_id: Some(child.into()),
        permission: Some("bash".into()),
        patterns: vec!["git status".into()],
        metadata: None,
        always: Vec::new(),
        always_supported: true,
    }]);
    let parent_id = backend.session_id.clone();
    let reply_sessions = backend.reply_permission_sessions.clone();
    let (app, _platform) = build_app(cfg, backend).await;

    // Seed the parent session so it maps child → parent → chat, with a LIVE
    // card so the child's permission has an inline host (ADR-0100). The
    // scripted prompt would finish before the poller runs.
    seed_live_card(&app, &parent_id, "om_live").await;

    tokio::spawn({
        let app = app.clone();
        async move {
            app.permission
                .poll_interval_ms
                .store(50, std::sync::atomic::Ordering::Relaxed);
            let _ = app.permission.poll_loop(&app.flow_handles()).await;
        }
    });
    wait_for_first_poll_pass(&app.permission.poll_passes, "the request sweep").await;

    // The parent session has a live streaming card, so the child's
    // permission is INLINED on it (one-card-per-turn), not sent as a
    // separate card — the child itself has no accumulator, so it must be
    // hosted on the parent's card found by walking the parent chain.
    let perm_inline = Turn::live_permissions(&app.cards_handle(), &parent_id).await;
    assert_eq!(
        perm_inline.len(),
        1,
        "subtask permission should be inlined on the parent card"
    );
    assert_eq!(perm_inline[0].0, "per_child");
    assert_eq!(perm_inline[0].1, child);

    // The streaming card renders the inline section with the child's buttons.
    let card = Turn::rendered_card(&app.cards_handle(), &parent_id)
        .await
        .expect("parent card renders");
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
    let result = app.host_action(value).await;
    let result = result.expect("reply should succeed for subtask session");
    let ack = result
        .card
        .as_ref()
        .expect("inline click must carry the parent card in the ack")
        .to_string();
    assert!(
        ack.contains("✅ 已允许一次：⚡ 执行 Shell 命令 `git status`"),
        "receipt missing: {}",
        ack
    );
    assert!(
        Turn::live_permissions(&app.cards_handle(), &parent_id)
            .await
            .is_empty(),
        "inline permission section is resolved after answering"
    );
    // V2's reply is session-scoped: the click must route via the CHILD session
    // id the request belongs to, not the parent card's session.
    assert_eq!(
        reply_sessions.lock().await.as_slice(),
        &[child.to_string()],
        "the permission reply must be routed via the child session id"
    );
}

/// A sub-task child's permission is hosted on its nearest ancestor's live
/// card. If the parent Turn ends between resolving that host and the insert —
/// the window `/card`'s Idle read can land in — the sweep must not insert on
/// the now-dead card: it releases the host's lock and surfaces the request on
/// its own card, so the request is never a dead end on a static read.
#[tokio::test]
async fn subtask_permission_whose_host_goes_idle_before_insert_surfaces_standalone() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let mut backend = MockBackend::new(realistic_parts());
    let child = "ses_child_task";
    let parent = backend.session_id.clone();
    backend.with_session_parent(child, &parent);
    backend.ask_permissions(vec![crate::backend::types::PermissionRequest {
        request_id: "per_child".into(),
        session_id: Some(child.into()),
        permission: Some("bash".into()),
        patterns: vec!["git status".into()],
        metadata: None,
        always: Vec::new(),
        always_supported: true,
    }]);
    let info_calls = backend.session_info_calls.clone();
    let (app, platform) = build_app(cfg, backend).await;
    seed_live_card(&app, &parent, "om_live").await;

    // Hold the host's write lock: the sweep resolves the host and then waits
    // here, exactly the window between host resolution and the inline insert.
    let cards = app.cards_handle();
    let host_lock = cards.write_lock(&parent).await;
    let host_guard = host_lock.lock().await;
    let sweep = {
        let flow = Arc::clone(&app.permission);
        let handles = app.flow_handles();
        tokio::spawn(async move {
            let mut seen = std::collections::HashSet::new();
            flow.sweep(&handles, &mut seen).await;
        })
    };
    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        loop {
            // `prepare`'s auto-accept walk, `resolve_card_target` and then
            // `inline_host_session` each read the child's parent; the THIRD
            // `session_info(child)` means the host walk is evaluating the
            // parent's liveness now.
            let walked = info_calls
                .lock()
                .await
                .iter()
                .filter(|id| id.as_str() == child)
                .count();
            if walked >= 3 {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("the sweep must walk the parent chain to resolve the host");
    tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    assert!(
        !sweep.is_finished(),
        "the sweep must be waiting on the resolved host's lock"
    );

    // The parent Turn ends and `/card` installs its Idle read while the sweep
    // holds the resolved host. The next sweep decision must see the dead card.
    Turn::set_card_state(&cards, &parent, crate::feishu::card::CardState::Done).await;
    assert!(
        Turn::record_idle_card(&cards, &parent, "om_idle", "msg_1").await,
        "the finished card is replaced by the Idle read"
    );
    drop(host_guard);

    tokio::time::timeout(std::time::Duration::from_secs(5), sweep)
        .await
        .expect("the sweep must finish once the host lock is released")
        .expect("the sweep task must not panic");

    // No block was inserted on the dead/idle card, and the request is not lost:
    // it was surfaced as its own card.
    assert_eq!(
        Turn::card_message_id(&cards, &parent).await.as_deref(),
        Some("om_idle"),
        "the Idle read stays the parent's card"
    );
    assert!(
        !Turn::has_interaction_in(&cards, &parent, "per_child").await,
        "an Idle read must never carry the block"
    );
    assert!(
        Turn::live_permissions(&cards, &parent).await.is_empty(),
        "the parent card carries no live permission"
    );
    assert!(
        app.permission.sent_cards.lock().await.contains_key("per_child"),
        "the request must be surfaced standalone, not dropped: {:?}",
        platform.calls.lock().await
    );
    let calls = platform.calls.lock().await.clone();
    assert!(
        calls.iter().any(|call| matches!(
            call,
            PlatformCall::ReplyCard { reply_to, card }
                if reply_to == "msg_1" && card_text(card).contains("git status")
        )),
        "the standalone card is replied to the parent's chat: {calls:?}"
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
    let parent = backend.session_id.clone();
    backend.with_session_parent(child, &parent);
    backend.ask_permissions(vec![crate::backend::types::PermissionRequest {
        request_id: "per_child".into(),
        session_id: Some(child.into()),
        permission: Some("bash".into()),
        patterns: vec!["git status".into()],
        metadata: None,
        always: Vec::new(),
        always_supported: true,
    }]);
    let (app, platform) = build_app(cfg, backend).await;

    // Map the parent session to a chat WITHOUT an active accumulator (no
    // handle_message call — the turn is finished).
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

    tokio::spawn({
        let app = app.clone();
        async move {
            app.permission
                .poll_interval_ms
                .store(50, std::sync::atomic::Ordering::Relaxed);
            let _ = app.permission.poll_loop(&app.flow_handles()).await;
        }
    });
    wait_for_first_poll_pass(&app.permission.poll_passes, "the request sweep").await;

    // No inline host: the permission becomes a separate card delivered into
    // the parent's chat (the child has no card of its own to host it).
    let calls = platform.calls.lock().await.clone();
    let perm_card = calls.iter().find_map(|c| match c {
        PlatformCall::SendCard { receive_id, card }
            if receive_id == "chat_1" && card_text(card).contains("git status") =>
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
