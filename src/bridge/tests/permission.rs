use crate::bridge::command::*;
use crate::bridge::test_support::*;

#[tokio::test]
async fn permission_poller_sends_card_and_card_action_replies() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let mut backend = MockBackend::new(realistic_parts());
    backend.permissions = vec![opencode::types::PermissionRequest {
        request_id: "per_1".into(),
        session_id: Some("ses_test".into()),
        permission: Some("bash".into()),
        patterns: vec!["ls -la".into()],
        metadata: None,
        always: Vec::new(),
    }];
    let (app, _platform) = build_app(cfg, backend).await;

    // Seed a session + accumulator so the poller has a reply target.
    app.handle_message(incoming(
        "msg_1".into(),
        "chat_1".into(),
        "p2p".into(),
        None,
        "hi".into(),
        None,
    ))
    .await;

    // Run the permission poller briefly.
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

    // The session has an active streaming card, so the permission is surfaced
    // INLINE on it (one-card-per-turn) — not as a separate card.
    let perm_inline = app
        .cards
        .lock()
        .await
        .get("ses_test")
        .expect("accumulator exists")
        .acc
        .live_permissions();
    assert_eq!(perm_inline.len(), 1, "permission should be inlined");
    assert_eq!(perm_inline[0].request_id, "per_1");
    // The streaming card itself renders the inline permission section.
    let card = app.cards.lock().await.get("ses_test").unwrap().acc.build_card();
    let card_text = card.to_string();
    assert!(
        card_text.contains("权限请求"),
        "inline section missing: {}",
        card_text
    );
    assert!(
        card_text.contains("ls -la"),
        "permission body missing: {}",
        card_text
    );
    assert!(
        card_text.contains("允许一次"),
        "allow button missing: {}",
        card_text
    );

    // Simulate the user clicking "允许一次" — answered inline, so the ack
    // carries the CLICKED card with the block replaced by an Interaction
    // Receipt (ADR-0038), not a replacement card.
    let value = serde_json::json!({
        "action": "perm",
        "reply": "once",
        "session_id": "ses_test",
        "request_id": "per_1",
        "perm_label": "✅ 已允许一次",
        "perm_color": "green",
        "perm_body": "bash",
    });
    let result = app.host_action(value).await;
    assert!(result.is_some());
    let result = result.unwrap();
    let ack = result
        .card
        .as_ref()
        .expect("inline click must carry the updated card in the ack")
        .to_string();
    assert!(
        ack.contains("✅ 已允许一次：⚡ 执行 Shell 命令 `ls -la`"),
        "receipt missing: {}",
        ack
    );
    assert!(
        !ack.contains("🔐 **权限请求**") && !ack.contains("始终允许"),
        "the resolved block (and its controls) must be gone: {}",
        ack
    );
    // A toast gives the client instant feedback on the button press.
    assert_eq!(result.toast.as_deref(), Some("已允许本次执行"));
    // The block is resolved in the accumulator too: not live, receipt kept.
    assert!(
        app.cards
            .lock()
            .await
            .get("ses_test")
            .unwrap()
            .acc
            .live_permissions()
            .is_empty()
    );
}

/// Resolving an INLINE permission updates the clicked card ATOMICALLY in the
/// callback ack (ADR-0038, rule 3): the block becomes an Interaction Receipt
/// on the returned card, so there is no toast-then-PATCH lag and no
/// dependence on which card is current. No separate PATCH is needed — that
/// disappearance path is gone for inline clicks.
#[tokio::test]
async fn inline_permission_click_carries_the_receipt_in_the_ack() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let mut backend = MockBackend::new(realistic_parts());
    backend.permissions = vec![opencode::types::PermissionRequest {
        request_id: "per_1".into(),
        session_id: Some("ses_test".into()),
        permission: Some("bash".into()),
        patterns: vec!["ls -la".into()],
        metadata: None,
        always: Vec::new(),
    }];
    let (app, platform) = build_app(cfg, backend).await;

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
    assert!(
        !app.cards
            .lock()
            .await
            .get("ses_test")
            .unwrap()
            .acc
            .live_permissions()
            .is_empty(),
        "permission should be inlined on the streaming card"
    );

    let updates_before = platform
        .calls
        .lock()
        .await
        .iter()
        .filter(|c| matches!(c, PlatformCall::UpdateMessage { .. }))
        .count();

    let result = app
        .host_action(serde_json::json!({
            "action": "perm",
            "reply": "once",
            "session_id": "ses_test",
            "request_id": "per_1",
            "perm_label": "✅ 已允许一次",
            "perm_color": "green",
            "perm_body": "bash",
        }))
        .await;
    let ack = result
        .expect("a card-action result")
        .card
        .expect("the ack must carry the clicked card")
        .to_string();
    assert!(
        ack.contains("✅ 已允许一次：⚡ 执行 Shell 命令 `ls -la`"),
        "receipt missing: {}",
        ack
    );
    assert!(
        !ack.contains("🔐 **权限请求**") && !ack.contains("始终允许"),
        "the block (and its controls) must be replaced by the receipt: {}",
        ack
    );

    // The ack IS the update: no PATCH raced behind it.
    let calls = platform.calls.lock().await.clone();
    let new_updates = calls
        .iter()
        .filter(|c| matches!(c, PlatformCall::UpdateMessage { .. }))
        .skip(updates_before)
        .count();
    assert_eq!(
        new_updates, 0,
        "the ack carries the card; no separate PATCH may fire: {:?}",
        calls
    );
}

/// Two near-simultaneous clicks on the same permission card must reach the
/// backend exactly once. The test holds the answered-set lock so both
/// clicks queue on the guard and are released together — reproducing the
/// check-then-act window the atomic guard closes.
#[tokio::test]
async fn concurrent_permission_clicks_reply_once() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let backend = Arc::new(MockBackend::new(realistic_parts()));
    let platform = Arc::new(RecordingPlatform::new());
    let app = Arc::new(App::new(cfg, backend.clone(), platform).unwrap());
    seed_session(&app, "ses_1", "/work").await;

    let value = serde_json::json!({
        "action": "perm",
        "reply": "once",
        "session_id": "ses_1",
        "request_id": "per_1",
        "perm_label": "✅ 已允许一次",
        "perm_color": "green",
        "perm_body": "bash",
    });

    // Hold the guard lock, start both clicks, let them queue on it, then
    // release: with the old check-then-act pair both pass the check before
    // either records, so both reply.
    let guard = app.answered_requests.lock().await;
    let a = {
        let app = app.clone();
        let value = value.clone();
        tokio::spawn(async move { app.host_action(value).await })
    };
    let b = {
        let app = app.clone();
        tokio::spawn(async move { app.host_action(value).await })
    };
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    drop(guard);
    let (ra, rb) = tokio::join!(a, b);
    assert!(ra.is_ok() && rb.is_ok(), "both clicks must be handled");

    let calls = backend.reply_permission_calls.lock().await.clone();
    assert_eq!(calls.len(), 1, "two clicks must reply exactly once: {:?}", calls);
}

/// A 404 from the backend means the permission was already resolved (by
/// another client, or by a click replayed after a cola restart cleared the
/// in-memory guard). That is a benign outcome: a neutral "已处理" card, not
/// the red failure card.
#[tokio::test]
async fn permission_reply_404_renders_neutral_already_handled() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let mut backend = MockBackend::new(realistic_parts());
    backend.reply_permission_not_found = true;
    let backend = Arc::new(backend);
    let platform = Arc::new(RecordingPlatform::new());
    let app = Arc::new(App::new(cfg, backend.clone(), platform).unwrap());
    seed_session(&app, "ses_1", "/work").await;

    let value = serde_json::json!({
        "action": "perm",
        "reply": "once",
        "session_id": "ses_1",
        "request_id": "per_1",
        "perm_label": "✅ 已允许一次",
        "perm_color": "green",
        "perm_body": "bash",
    });
    let result = app.host_action(value).await.expect("a result card");
    let card = result.card.expect("standalone card").to_string();
    assert!(card.contains("已处理"), "neutral card expected: {}", card);
    assert!(
        !card.contains("处理失败"),
        "a 404 must not render as a failure: {}",
        card
    );
    assert_eq!(result.toast.as_deref(), Some("该权限已处理"));
    // The request did reach the backend once (it was this click that got
    // the 404) — the point is only that the outcome is neutral.
    assert_eq!(backend.reply_permission_calls.lock().await.len(), 1);
}

/// A genuine (non-404) failure must roll the answered mark back, so a
/// retry click replies to the backend again instead of replaying a
/// decision that never reached it.
#[tokio::test]
async fn permission_reply_failure_rolls_back_so_retry_replies() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let backend = MockBackend::new(realistic_parts());
    backend
        .fail_reply_permission_count
        .store(1, std::sync::atomic::Ordering::SeqCst);
    let backend = Arc::new(backend);
    let platform = Arc::new(RecordingPlatform::new());
    let app = Arc::new(App::new(cfg, backend.clone(), platform).unwrap());
    seed_session(&app, "ses_1", "/work").await;

    let value = serde_json::json!({
        "action": "perm",
        "reply": "once",
        "session_id": "ses_1",
        "request_id": "per_1",
        "perm_label": "✅ 已允许一次",
        "perm_color": "green",
        "perm_body": "bash",
    });
    let first = app.host_action(value.clone()).await.expect("a result card");
    let first_card = first.card.expect("standalone card").to_string();
    assert!(
        first_card.contains("处理失败"),
        "genuine failure still shows the failure card: {}",
        first_card
    );

    let second = app.host_action(value).await.expect("a result card");
    assert!(
        second.card.is_some(),
        "retry must be handled, not silently dropped"
    );
    let calls = backend.reply_permission_calls.lock().await.clone();
    assert_eq!(
        calls.len(),
        2,
        "retry after a failure must reply again: {:?}",
        calls
    );
}

/// A second click on a DIFFERENT permission button must re-serve the first
/// click's result, not its own. Otherwise the card would show a decision
/// the backend never received (the winner sent "once", the loser picked
/// "always") — the "card state flips to the last click" bug.
#[tokio::test]
async fn second_permission_click_reserves_the_first_result() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let backend = Arc::new(MockBackend::new(realistic_parts()));
    let platform = Arc::new(RecordingPlatform::new());
    let app = Arc::new(App::new(cfg, backend.clone(), platform).unwrap());
    seed_session(&app, "ses_1", "/work").await;

    let value_for = |reply: &str, label: &str| {
        serde_json::json!({
            "action": "perm",
            "reply": reply,
            "session_id": "ses_1",
            "request_id": "per_1",
            "perm_label": label,
            "perm_color": "green",
            "perm_body": "bash",
        })
    };
    let first = app
        .host_action(value_for("once", "✅ 已允许一次"))
        .await
        .expect("a result card");
    let second = app
        .host_action(value_for("always", "✅ 已允许（总是）"))
        .await
        .expect("a result card");

    assert_eq!(
        first.card, second.card,
        "the losing click must re-serve the winning result card"
    );
    assert_eq!(first.toast, second.toast);
    let calls = backend.reply_permission_calls.lock().await.clone();
    assert_eq!(calls, vec![("per_1".to_string(), "once".to_string())]);
}

/// A poll that hangs on the backend list call (a half-open connection left
/// by a server restart) must not freeze the poller forever: the list call
/// is bounded, so a later poll still surfaces pending requests.
#[tokio::test]
async fn permission_poller_recovers_when_a_list_call_hangs() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let mut backend = MockBackend::new(realistic_parts());
    backend.permissions = vec![opencode::types::PermissionRequest {
        request_id: "per_hung".into(),
        session_id: Some("ses_test".into()),
        permission: Some("bash".into()),
        patterns: vec!["ls -la".into()],
        metadata: None,
        always: Vec::new(),
    }];
    // The first list call hangs forever, like a request in flight when the
    // server was SIGTERM'd; later calls serve normally.
    backend
        .hang_list_permissions
        .store(1, std::sync::atomic::Ordering::SeqCst);
    let (app, _platform) = build_app(cfg, backend).await;

    // Seed a session + accumulator so the poller has a reply target.
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
                .store(20, std::sync::atomic::Ordering::Relaxed);
            app.permission
                .list_timeout_ms
                .store(50, std::sync::atomic::Ordering::Relaxed);
            let _ = app.permission.poll_loop(&app.core).await;
        }
    });

    // Without a bound on the list call the poller sits on the first hung
    // call forever and the permission never surfaces.
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(2);
    loop {
        let surfaced = app
            .cards
            .lock()
            .await
            .get("ses_test")
            .map(|c| c.acc.interaction("per_hung").is_some())
            .unwrap_or(false);
        if surfaced {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "poller never recovered from the hung list call"
        );
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
}

/// Clicking "开启自动授权" on a permission card turns on the session's
/// Auto-Accept (cola-side `/autoaccept`) AND approves the current pending
/// permission — all without posting a new message. The backend sees the
/// normal "once" reply, never "autoaccept".
#[tokio::test]
async fn autoaccept_toggle_on_permission_card_flips_flag_and_approves() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let mut backend = MockBackend::new(realistic_parts());
    backend.permissions = vec![
        opencode::types::PermissionRequest {
            request_id: "per_aa_toggle".into(),
            session_id: Some("ses_test".into()),
            permission: Some("bash".into()),
            patterns: vec!["ls -la".into()],
            metadata: None,
            always: Vec::new(),
        },
        // A second, already-pending request for the same session — the
        // toggle approves it too and must drop its inline section in the
        // same interaction, not leave it lingering until the next poll.
        opencode::types::PermissionRequest {
            request_id: "per_aa_other".into(),
            session_id: Some("ses_test".into()),
            permission: Some("edit".into()),
            // An edit is about its file: the receipt must name the filepath,
            // not the looser pattern.
            patterns: vec!["*.rs".into()],
            metadata: Some(serde_json::json!({ "filepath": "src/main.rs" })),
            always: Vec::new(),
        },
    ];
    let perm_calls = backend.reply_permission_calls.clone();
    let (app, platform) = build_app(cfg, backend).await;

    // Seed a session (auto_accept defaults to false) + a live streaming card
    // so the poller surfaces the permission INLINE.
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

    // Permission inlined on the streaming card, auto_accept still off.
    assert_eq!(
        app.cards
            .lock()
            .await
            .get("ses_test")
            .unwrap()
            .acc
            .live_permissions()
            .len(),
        2
    );
    assert!(
        !app.sessions
            .lock()
            .await
            .entry_for_session("ses_test")
            .unwrap()
            .auto_accept
    );

    // Click the toggle.
    let value = serde_json::json!({
        "action": "perm",
        "reply": "autoaccept",
        "session_id": "ses_test",
        "request_id": "per_aa_toggle",
        "perm_label": "✅ 已开启自动授权",
        "perm_color": "blue",
        "perm_body": "bash",
    });
    let result = app.host_action(value).await.expect("toggle result");
    let ack = result
        .card
        .as_ref()
        .expect("inline toggle must carry the updated card in the ack")
        .to_string();
    assert_eq!(result.toast.as_deref(), Some("已开启自动授权"));
    // ONE receipt for the whole mode change, in the same ack — not one per
    // approved request: naming a command made the line read as "only this
    // command is now auto-approved".
    assert_eq!(
        ack.matches("🔄 已开启自动授权").count(),
        1,
        "one receipt for the toggle, not one per block: {}",
        ack
    );
    assert!(
        ack.contains("🔄 已开启自动授权：后续权限请求将自动批准"),
        "the receipt explains the MODE change: {}",
        ack
    );
    assert!(
        !ack.contains("🔄 已开启自动授权：⚡ 执行 Shell 命令 `ls -la`")
            && !ack.contains("🔄 已开启自动授权：✏️ 编辑文件 `src/main.rs`"),
        "the receipt must not name the requests it approved: {}",
        ack
    );
    assert!(
        !ack.contains("🔐 **权限请求**"),
        "no block may survive the toggle: {}",
        ack
    );

    // Both pending permissions approved with "once"; flag flipped.
    let mut calls = perm_calls.lock().await.clone();
    calls.sort();
    assert_eq!(
        calls,
        vec![
            ("per_aa_other".to_string(), "once".to_string()),
            ("per_aa_toggle".to_string(), "once".to_string()),
        ],
        "the toggle should approve the current AND other pending permissions"
    );
    assert!(
        app.sessions
            .lock()
            .await
            .entry_for_session("ses_test")
            .unwrap()
            .auto_accept,
        "auto_accept flag should flip on"
    );
    assert!(
        app.cards
            .lock()
            .await
            .get("ses_test")
            .unwrap()
            .acc
            .live_permissions()
            .is_empty(),
        "ALL inline sections removed after the toggle, not just the clicked one"
    );
    // No NEW message: only the loading card + final card, no text reply.
    let sent = platform.calls.lock().await.clone();
    assert!(
        !sent.iter().any(|c| matches!(c, PlatformCall::ReplyText { .. })),
        "toggle must not send a new message"
    );
}

/// `/autoaccept on` (the command, not the card button) leaves the SAME mode
/// receipt on the live card. Without it the sweep resolves the blocks cola
/// itself just approved as `⏱ 已由其他客户端处理` — a lie (#193 follow-up).
#[tokio::test]
async fn autoaccept_command_leaves_the_mode_receipt_on_the_card() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let mut backend = MockBackend::new(realistic_parts());
    backend.permissions = vec![perm_request("per_1", "ses_1", "ls -la")];
    let (app, platform) = build_app(cfg, backend).await;
    seed_session(&app, "ses_1", "/work").await;
    seed_inline_permission_card(&app, "ses_1", "per_1").await;
    // Render once so the block is on the card AND its handle is registered.
    crate::bridge::render::flush_card(&app.core, "ses_1").await;
    let card_id = app
        .card_handles
        .lock()
        .await
        .message_of("per_1")
        .expect("the flush registers the card that renders the block")
        .to_string();
    // An older copy of the same block (a re-host leaves one behind): the
    // command path must repaint it too — nothing else would.
    {
        let stale_card = serde_json::json!({
            "schema": "2.0",
            "body": { "elements": [
                { "tag": "markdown", "content": "🔐 **权限请求**" },
                { "tag": "button", "text": { "tag": "plain_text", "content": "✅ 允许一次" },
                  "type": "primary", "value": { "action": "perm", "request_id": "per_1" } },
            ] },
        });
        let mut handles = app.card_handles.lock().await;
        handles.record(
            "msg_stale",
            &stale_card,
            vec![crate::bridge::card_handles::RenderedBlock {
                request_id: "per_1".into(),
                start: 0,
                end: 2,
                kind: crate::bridge::snapshot_claims::ClaimKind::Permission,
                session_id: "ses_1".into(),
                directory: "/work".into(),
                target: "⚡ 执行 Shell 命令 `ls -la`".into(),
            }],
        );
    }

    crate::bridge::command::handle_command(
        &app.core,
        Command::AutoAccept(crate::bridge::command::AutoAcceptAction::Set(true)),
        crate::config::ThreadKey::new("chat_1".into(), "chat_1".into()),
        "msg_cmd",
        crate::config::ConversationKind::P2p,
    )
    .await
    .unwrap();

    let patches: Vec<String> = platform
        .calls
        .lock()
        .await
        .iter()
        .filter_map(|c| match c {
            PlatformCall::UpdateMessage { message_id, card } if message_id == &card_id => {
                Some(card.to_string())
            }
            _ => None,
        })
        .collect();
    let repaint = patches.last().expect("the command must patch the card");
    assert_eq!(
        repaint.matches("🔄 已开启自动授权").count(),
        1,
        "one mode receipt: {repaint}"
    );
    assert!(
        repaint.contains("后续权限请求将自动批准"),
        "the receipt explains the MODE change: {repaint}"
    );
    assert!(
        !repaint.contains("⏱ 已由其他客户端处理"),
        "cola approved it itself — never 'handled elsewhere': {repaint}"
    );
    assert!(
        !repaint.contains("🔐 **权限请求**"),
        "the approved block must be gone: {repaint}"
    );
    assert!(
        app.cards
            .lock()
            .await
            .get("ses_1")
            .unwrap()
            .acc
            .live_permissions()
            .is_empty(),
        "the accumulator must not keep the block live"
    );
    // The stale copy gets the mode receipt without a header restamp (it may
    // belong to an older turn; only the registered card is restamped).
    let stale = platform
        .calls
        .lock()
        .await
        .iter()
        .rev()
        .find_map(|c| match c {
            PlatformCall::UpdateMessage { message_id, card } if message_id == "msg_stale" => {
                Some(card.to_string())
            }
            _ => None,
        })
        .expect("the stale copy of the block must be repainted too");
    assert!(
        stale.contains("后续权限请求将自动批准")
            && !stale.contains("🔐 **权限请求**")
            && !stale.contains("✅ 允许一次"),
        "the stale copy gets the mode receipt, not the live block: {stale}"
    );

    // The sweep after the command finds nothing left to resolve: no second
    // receipt may appear on the card.
    let mut seen = std::collections::HashSet::new();
    app.permission.sweep(&app.core, &mut seen).await;
    let after = platform
        .calls
        .lock()
        .await
        .iter()
        .filter(|c| matches!(c, PlatformCall::UpdateMessage { message_id, .. } if message_id == &card_id))
        .count();
    assert_eq!(after, patches.len(), "the sweep must not repaint again");
}

/// The `/autoaccept` toggle CARD's "on" button (not the command, not the
/// permission card's own button) leaves the same mode receipt on a pending
/// permission: the approval settles the block itself and never lets the sweep
/// report it as another client's work.
#[tokio::test]
async fn autoaccept_toggle_card_settles_pending_permissions() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let mut backend = MockBackend::new(realistic_parts());
    backend.permissions = vec![perm_request("per_1", "ses_1", "ls -la")];
    let (app, platform) = build_app(cfg, backend).await;
    seed_session(&app, "ses_1", "/work").await;
    seed_inline_permission_card(&app, "ses_1", "per_1").await;
    crate::bridge::render::flush_card(&app.core, "ses_1").await;
    let card_id = app
        .card_handles
        .lock()
        .await
        .message_of("per_1")
        .expect("the flush registers the card that renders the block")
        .to_string();

    let result = app
        .host_action(serde_json::json!({
            "action": "autoaccept",
            "chat_id": "chat_1",
            "thread_id": "chat_1",
            "value": "on",
        }))
        .await
        .expect("the toggle card acks");
    assert!(result.card.is_some(), "the refreshed toggle card is returned");
    assert!(
        app.sessions
            .lock()
            .await
            .entry_for_session("ses_1")
            .unwrap()
            .auto_accept
    );

    let patches: Vec<String> = platform
        .calls
        .lock()
        .await
        .iter()
        .filter_map(|c| match c {
            PlatformCall::UpdateMessage { message_id, card } if message_id == &card_id => {
                Some(card.to_string())
            }
            _ => None,
        })
        .collect();
    let repaint = patches.last().expect("the toggle must patch the permission card");
    assert!(
        repaint.contains("🔄 已开启自动授权"),
        "the mode receipt must render: {repaint}"
    );
    assert!(
        !repaint.contains("⏱ 已由其他客户端处理") && !repaint.contains("🔐 **权限请求**"),
        "the approved block leaves the mode receipt, not the neutral line: {repaint}"
    );
    assert!(
        app.core.settling_requests.lock().await.is_empty(),
        "the settlement released its in-flight claim"
    );

    // The sweep after the toggle finds nothing left to resolve.
    let mut seen = std::collections::HashSet::new();
    app.permission.sweep(&app.core, &mut seen).await;
    let after = platform
        .calls
        .lock()
        .await
        .iter()
        .filter(|c| matches!(c, PlatformCall::UpdateMessage { message_id, .. } if message_id == &card_id))
        .count();
    assert_eq!(after, patches.len(), "the sweep must not repaint again");
}

/// An auto-accept approval replies to the server BEFORE its block settles: a
/// sweep landing in that window must leave the claimed block to its own
/// settlement instead of reporting "handled elsewhere". That gap is what the
/// Host saw — the pending permission card flipped to
/// `⏱ 已由其他客户端处理` the moment auto-accept was enabled from that card.
#[tokio::test]
async fn sweep_leaves_a_claimed_approval_to_its_settlement() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let mut backend = MockBackend::new(realistic_parts());
    backend.permissions = vec![perm_request("per_1", "ses_1", "ls -la")];
    let backend = Arc::new(backend);
    let platform = Arc::new(RecordingPlatform::new());
    let app = Arc::new(App::new(cfg, backend.clone(), platform.clone()).unwrap());
    seed_session(&app, "ses_1", "/work").await;
    seed_inline_permission_card(&app, "ses_1", "per_1").await;
    crate::bridge::render::flush_card(&app.core, "ses_1").await;
    assert_eq!(
        app.card_handles.lock().await.message_of("per_1"),
        Some("msg_live"),
        "precondition: the block's handle is recorded"
    );

    // cola took the answer claim (a click, or the auto-accept approval) and the
    // reply landed: the request is off the pending list, its settlement has not
    // run yet.
    assert!(app.permission.try_mark_answered(&app.core, "per_1").await);
    backend.replied_permissions.lock().await.insert("per_1".into());

    let mut seen = std::collections::HashSet::new();
    app.permission.sweep(&app.core, &mut seen).await;

    // The sweep left it alone: no neutral receipt reached the card, the handle
    // and the accumulator still carry the block for the in-flight settlement.
    let neutral = platform.calls.lock().await.iter().any(|c| {
        matches!(
            c,
            PlatformCall::UpdateMessage { card, .. }
                if card.to_string().contains("⏱ 已由其他客户端处理")
        )
    });
    assert!(!neutral, "a claimed approval must not be settled by the sweep");
    assert_eq!(
        app.card_handles.lock().await.message_of("per_1"),
        Some("msg_live"),
        "the handle still names the card the settlement will repaint"
    );
    assert!(
        app.cards
            .lock()
            .await
            .get("ses_1")
            .unwrap()
            .acc
            .live_permissions()
            .iter()
            .any(|p| p.request_id == "per_1"),
        "the accumulator keeps the claimed block live for its own settlement"
    );
}

/// `/autoaccept on` (and the picker toggle) marks the requests it approves
/// before they leave the pending list, so the sweep's claimed-approval check
/// has something to honour. Without the mark the sweep races the settlement.
#[tokio::test]
async fn autoaccept_approval_claims_the_requests_it_answers() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let mut backend = MockBackend::new(realistic_parts());
    backend.permissions = vec![perm_request("per_1", "ses_1", "ls -la")];
    let backend = Arc::new(backend);
    let platform = Arc::new(RecordingPlatform::new());
    let app = Arc::new(App::new(cfg, backend.clone(), platform.clone()).unwrap());
    seed_session(&app, "ses_1", "/work").await;

    let approved = app.core.approve_pending_for_session("ses_1", "/work").await;

    assert_eq!(approved, vec!["per_1".to_string()]);
    assert!(backend.replied_permissions.lock().await.contains("per_1"));
    assert!(
        app.core.settling_requests.lock().await.contains_key("per_1"),
        "the approval must take the claim its settlement owns"
    );
}

/// A standalone permission card (no accumulator, no card handle) must keep its
/// `sent_cards` entry when `/autoaccept on` approves it: clearing it would
/// strand the card's live buttons. `mark_stale_cards` owns that lifecycle and
/// repaints it, as ADR-0038 rule 6 says.
#[tokio::test]
async fn autoaccept_command_keeps_the_standalone_card_for_the_sweep() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let mut backend = MockBackend::new(realistic_parts());
    backend.permissions = vec![perm_request("per_1", "ses_1", "ls -la")];
    let (app, platform) = build_app(cfg, backend).await;
    seed_session(&app, "ses_1", "/work").await;
    // The poller surfaced this one as a standalone card (no live turn).
    app.permission.sent_cards.lock().await.insert(
        "per_1".into(),
        crate::bridge::request::SentCard {
            message_id: "om_sent".into(),
            summary: "bash ls -la".into(),
            directory: "/work".into(),
        },
    );

    crate::bridge::command::handle_command(
        &app.core,
        Command::AutoAccept(crate::bridge::command::AutoAcceptAction::Set(true)),
        crate::config::ThreadKey::new("chat_1".into(), "chat_1".into()),
        "msg_cmd",
        crate::config::ConversationKind::P2p,
    )
    .await
    .unwrap();

    // Approved server-side, but this seam has nothing inline to settle: the
    // entry survives so the sweep can repaint the card.
    assert!(
        app.permission.sent_cards.lock().await.contains_key("per_1"),
        "a standalone card's entry must survive the command"
    );

    let mut seen = std::collections::HashSet::new();
    app.permission.sweep(&app.core, &mut seen).await;
    let stale = platform
        .calls
        .lock()
        .await
        .iter()
        .find_map(|c| match c {
            PlatformCall::UpdateMessage { message_id, card } if message_id == "om_sent" => {
                Some(card.to_string())
            }
            _ => None,
        })
        .expect("the sweep marks the standalone card handled");
    assert!(
        stale.contains("已处理"),
        "standalone card shows as handled: {stale}"
    );
    assert!(
        !app.permission.sent_cards.lock().await.contains_key("per_1"),
        "the sweep owns the standalone entry"
    );
}

#[tokio::test]
async fn auto_accept_session_answers_permission_without_card() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let mut mock = MockBackend::new(realistic_parts());
    mock.permissions = vec![opencode::types::PermissionRequest {
        request_id: "per_aa".into(),
        session_id: Some("ses_test".into()),
        permission: Some("bash".into()),
        patterns: vec!["ls".into()],
        metadata: None,
        always: Vec::new(),
    }];
    let perm_calls = mock.reply_permission_calls.clone();
    let (app, platform) = build_app(cfg, mock).await;

    // Enable `/autoaccept` on the session.
    seed_entry(
        &app,
        crate::config::SessionEntry {
            thread_key: crate::config::ThreadKey::new("chat_1".into(), "chat_1".into()),
            session_id: "ses_test".into(),
            directory: "/tmp/aa".into(),
            agent: None,
            model: None,
            auto_accept: true,
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
            let _ = app.permission.poll_loop(&app.core).await;
        }
    });
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;

    // Auto-accepted: reply_permission called with "once", no card sent.
    let calls = perm_calls.lock().await.clone();
    assert_eq!(
        calls,
        vec![("per_aa".to_string(), "once".to_string())],
        "auto-accept should reply once"
    );
    let sent = platform.calls.lock().await.clone();
    assert!(
        !sent.iter().any(|c| {
            if let PlatformCall::ReplyCard { card, .. } = c {
                card.to_string().contains("权限请求")
            } else {
                false
            }
        }),
        "no permission card should be sent for an auto-accept session: {:?}",
        sent
    );
}

/// `/autoaccept on` must also approve requests that were ALREADY pending
/// (seen before the flag existed), not just future ones. Otherwise the
/// poller's `seen` set leaves old permission cards hanging forever.
#[tokio::test]
async fn autoaccept_on_approves_already_pending_permission() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let mut mock = MockBackend::new(realistic_parts());
    mock.permissions = vec![opencode::types::PermissionRequest {
        request_id: "per_pending".into(),
        session_id: Some("ses_test".into()),
        permission: Some("bash".into()),
        patterns: vec!["ls -la".into()],
        metadata: None,
        always: Vec::new(),
    }];
    let perm_calls = mock.reply_permission_calls.clone();
    let (app, _platform) = build_app(cfg, mock).await;

    // Session already mapped but autoaccept OFF — the permission would have
    // been surfaced as a card before the user enabled it.
    seed_entry(
        &app,
        crate::config::SessionEntry {
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

    // Now the user turns autoaccept on via the command.
    crate::bridge::command::handle_command(
        &app.core,
        Command::AutoAccept(crate::bridge::command::AutoAcceptAction::Set(true)),
        crate::config::ThreadKey::new("chat_1".into(), "chat_1".into()),
        "msg_cmd",
        crate::config::ConversationKind::P2p,
    )
    .await
    .unwrap();

    // The already-pending request was approved with "once" immediately.
    let calls = perm_calls.lock().await.clone();
    assert_eq!(
        calls,
        vec![("per_pending".to_string(), "once".to_string())],
        "turning autoaccept on should approve already-pending requests"
    );
    // The flag is persisted for future requests.
    let entry = {
        let store = app.sessions.lock().await;
        store
            .get_active(&crate::config::ThreadKey::new("chat_1".into(), "chat_1".into()))
            .cloned()
    };
    assert!(entry.unwrap().auto_accept, "autoaccept flag should persist");
}

/// Sub-task child sessions carry their own sessionID; `/autoaccept on` on
/// the parent must reach through the parent chain and approve their
/// pending permissions too.
#[tokio::test]
async fn autoaccept_on_approves_child_session_permission() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let mut mock = MockBackend::new(realistic_parts());
    mock.permissions = vec![opencode::types::PermissionRequest {
        request_id: "per_child".into(),
        session_id: Some("ses_child".into()),
        permission: Some("bash".into()),
        patterns: vec!["rm -rf x".into()],
        metadata: None,
        always: Vec::new(),
    }];
    mock.session_parents.insert("ses_child".into(), "ses_test".into());
    let perm_calls = mock.reply_permission_calls.clone();
    let (app, _platform) = build_app(cfg, mock).await;

    seed_entry(
        &app,
        crate::config::SessionEntry {
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

    crate::bridge::command::handle_command(
        &app.core,
        Command::AutoAccept(crate::bridge::command::AutoAcceptAction::Set(true)),
        crate::config::ThreadKey::new("chat_1".into(), "chat_1".into()),
        "msg_cmd",
        crate::config::ConversationKind::P2p,
    )
    .await
    .unwrap();

    let calls = perm_calls.lock().await.clone();
    assert_eq!(
        calls,
        vec![("per_child".to_string(), "once".to_string())],
        "child-session permission should be approved via the parent chain"
    );
}

#[tokio::test]
async fn stale_permission_card_marked_handled_when_resolved_elsewhere() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    // No pending permissions on the server — the card cola sent is stale.
    let (app, platform) = build_app(cfg, MockBackend::new(realistic_parts())).await;
    app.permission.sent_cards.lock().await.insert(
        "per_stale".into(),
        crate::bridge::request::SentCard {
            message_id: "om_sent_card".into(),
            summary: "bash ls -la".into(),
            directory: "/work".into(),
        },
    );

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

    let calls = platform.calls.lock().await.clone();
    let stale = calls.iter().find_map(|c| match c {
        PlatformCall::UpdateMessage { message_id, card } if message_id == "om_sent_card" => {
            Some(card.clone())
        }
        _ => None,
    });
    let stale = stale.expect("stale permission card should be marked");
    assert!(
        stale.to_string().contains("已处理"),
        "stale card should show as handled: {}",
        stale
    );
    assert!(
        stale.to_string().contains("bash ls -la"),
        "stale card should keep the original request text: {}",
        stale
    );
}

/// #144: a directory whose list call fails must not clear its live permission
/// surfaces — the standalone card, the inline section and the snapshot claim
/// all survive until a SUCCESSFUL list proves the request gone.
#[tokio::test]
async fn failed_directory_list_keeps_permission_surfaces() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let backend = Arc::new(MockBackend::new(realistic_parts()));
    let platform = Arc::new(RecordingPlatform::new());
    let app = Arc::new(App::new(cfg, backend.clone(), platform.clone()).unwrap());
    seed_session(&app, "ses_1", "/work").await;

    let mut inline_acc = crate::bridge::streaming::StreamAccumulator::new("test");
    inline_acc.add_interaction(crate::bridge::streaming::InteractionBlock::Permission(
        crate::bridge::streaming::PendingPermission {
            session_id: "ses_1".into(),
            request_id: "per_inline".into(),
            body: "bash ls -la".into(),
            target: "⚡ 执行 Shell 命令 `ls -la`".into(),
            directory: "/work".into(),
        },
    ));
    assert_failed_dir_keeps_surfaces(
        &app,
        &app.permission,
        &backend.hang_list_permissions,
        &platform,
        FailedDirSurfaces {
            card_id: "per_card",
            card_message_id: "om_card",
            inline_id: "per_inline",
            snapshot_message_id: "om_snapshot",
            inline_acc,
            claim: crate::bridge::request::PendingRequest::Permission(perm_request(
                "per_claim",
                "ses_1",
                "ls -la",
            )),
        },
    )
    .await;
}

/// A topic-backed session with no streaming card falls back to a separate
/// permission card. The card replies to the session's topic anchor (a
/// message inside the topic), which keeps it inside the topic — the create
/// API rejects `thread_id` as a receive target, so replying to the anchor is
/// the reliable way to reach the topic.
#[tokio::test]
async fn separate_permission_card_sent_into_topic_for_topic_session() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let mut backend = MockBackend::new(realistic_parts());
    backend.permissions = vec![opencode::types::PermissionRequest {
        request_id: "per_topic".into(),
        session_id: Some("ses_topic".into()),
        permission: Some("bash".into()),
        patterns: vec!["cargo build".into()],
        metadata: None,
        always: Vec::new(),
    }];
    let (app, platform) = build_app(cfg, backend).await;

    // Map the session to a TOPIC (thread_id != chat_id) with an anchor
    // message inside the topic, no accumulator.
    seed_entry(
        &app,
        crate::config::SessionEntry {
            thread_key: crate::config::ThreadKey::new("chat_1".into(), "omt_topic_1".into()),
            session_id: "ses_topic".into(),
            directory: "/tmp/topic".into(),
            agent: None,
            model: None,
            auto_accept: false,
            topic_anchor: Some("msg_in_topic_anchor".into()),
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
            let _ = app.permission.poll_loop(&app.core).await;
        }
    });
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;

    // The separate card must be reply'd to the topic anchor (not sent to
    // the chat top level).
    let calls = platform.calls.lock().await.clone();
    let perm_card = calls.iter().find_map(|c| match c {
        PlatformCall::ReplyCard { reply_to, card }
            if reply_to == "msg_in_topic_anchor" && card.to_string().contains("cargo build") =>
        {
            Some(card.clone())
        }
        _ => None,
    });
    assert!(
        perm_card.is_some(),
        "topic permission card should reply to the topic anchor, got: {calls:?}"
    );
    assert!(
        !calls
            .iter()
            .any(|c| matches!(c, PlatformCall::SendCard { receive_id, .. } if receive_id == "chat_1")),
        "permission card must NOT go to the chat top level: {calls:?}"
    );
}

// ===== Interaction Receipts (ADR-0038) =====

fn perm_req(id: &str, session: &str, pattern: &str) -> opencode::types::PermissionRequest {
    opencode::types::PermissionRequest {
        request_id: id.into(),
        session_id: Some(session.into()),
        permission: Some("bash".into()),
        patterns: vec![pattern.into()],
        metadata: None,
        always: Vec::new(),
    }
}

/// Click a permission button as the Host and return the ack card's text.
async fn click_perm(app: &Arc<App>, reply: &str, req: &str, label: &str) -> String {
    let value = serde_json::json!({
        "action": "perm",
        "reply": reply,
        "session_id": "ses_test",
        "request_id": req,
        "perm_label": label,
        "perm_color": "green",
        "perm_body": "bash",
    });
    app.host_action(value)
        .await
        .expect("a card-action result")
        .card
        .expect("an inline click must carry the updated card in the ack")
        .to_string()
}

/// Every permission decision — allow once, allow always, deny — replaces the
/// clicked block with its own Interaction Receipt and carries the updated card
/// in the ack; the accumulator keeps the receipts (ADR-0038, rules 3+4).
#[tokio::test]
async fn permission_click_variants_leave_their_receipts() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let mut backend = MockBackend::new(realistic_parts());
    backend.permissions = vec![
        perm_req("per_once", "ses_test", "ls -la"),
        perm_req("per_always", "ses_test", "cargo build"),
        perm_req("per_deny", "ses_test", "rm -rf target"),
    ];
    let perm_calls = backend.reply_permission_calls.clone();
    let (app, _platform) = build_app(cfg, backend).await;
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
    assert_eq!(
        app.cards
            .lock()
            .await
            .get("ses_test")
            .unwrap()
            .acc
            .live_permissions()
            .len(),
        3,
        "all three permissions inlined"
    );

    let ack = click_perm(&app, "once", "per_once", "✅ 已允许一次").await;
    assert!(
        ack.contains("✅ 已允许一次：⚡ 执行 Shell 命令 `ls -la`"),
        "once receipt: {}",
        ack
    );
    // The click resolves only its own block; the others stay live.
    assert!(ack.contains("🔐 **权限请求**"), "other blocks stay: {}", ack);

    let ack = click_perm(&app, "always", "per_always", "✅ 已始终允许").await;
    assert!(
        ack.contains("✅ 已始终允许：⚡ 执行 Shell 命令 `cargo build`"),
        "always receipt: {}",
        ack
    );

    let ack = click_perm(&app, "reject", "per_deny", "🚫 已拒绝").await;
    assert!(
        ack.contains("🚫 已拒绝：⚡ 执行 Shell 命令 `rm -rf target`"),
        "deny receipt: {}",
        ack
    );
    assert!(
        !ack.contains("🔐 **权限请求**"),
        "every block is resolved: {}",
        ack
    );

    // The accumulator is the render source of truth: no live block, three
    // receipts.
    let acc = app.cards.lock().await.get("ses_test").unwrap().acc.clone();
    assert!(acc.live_permissions().is_empty());
    let rendered = acc.build_card().to_string();
    assert_eq!(rendered.matches("已允许一次：").count(), 1, "{}", rendered);
    assert!(
        rendered.contains("✅ 已始终允许：⚡ 执行 Shell 命令 `cargo build`"),
        "{}",
        rendered
    );
    assert!(
        rendered.contains("🚫 已拒绝：⚡ 执行 Shell 命令 `rm -rf target`"),
        "{}",
        rendered
    );

    let calls = perm_calls.lock().await.clone();
    assert_eq!(calls.len(), 3, "each click replied once: {:?}", calls);
}

/// A receipt is rendered from the accumulator, so it survives every later
/// flush — a new part arriving, and a split (ADR-0038). It belongs to the card
/// that owns its anchor: exactly one card of the chain carries it.
#[tokio::test]
async fn interaction_receipt_survives_later_flushes() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let backend = Arc::new(MockBackend::new(realistic_parts()));
    let platform = Arc::new(RecordingPlatform::new());
    let app = Arc::new(App::new(cfg, backend, platform.clone()).unwrap());
    seed_session(&app, "ses_test", "/work").await;

    // A live card with two transcript items and an inlined permission (the
    // poll loop is what inlines in production; seeding it keeps this test
    // deterministic — no background render poll splitting underneath us).
    let mut acc = crate::bridge::streaming::StreamAccumulator::new("test");
    acc.reply_to_message_id = Some("msg_1".into());
    acc.push_reasoning("用户想让我分析目录。");
    acc.push_text("当前目录有 src/ 和 Cargo.toml。");
    acc.add_interaction(crate::bridge::streaming::InteractionBlock::Permission(
        crate::bridge::streaming::PendingPermission {
            session_id: "ses_test".into(),
            request_id: "per_1".into(),
            body: "bash ls -la".into(),
            target: "⚡ 执行 Shell 命令 `ls -la`".into(),
            directory: "/work".into(),
        },
    ));
    app.cards.lock().await.insert(
        "ses_test".to_string(),
        crate::bridge::streaming::CardSession::new(acc, Some("msg_live".into())),
    );

    let ack = click_perm(&app, "once", "per_1", "✅ 已允许一次").await;
    assert!(
        ack.contains("✅ 已允许一次：⚡ 执行 Shell 命令 `ls -la`"),
        "receipt missing: {}",
        ack
    );

    // A new part arrives: the flush re-renders the receipt from the
    // accumulator (the render source of truth), not just into the ack.
    app.cards
        .lock()
        .await
        .get_mut("ses_test")
        .unwrap()
        .acc
        .push_text("新的进展。");
    crate::bridge::render::flush_card(&app.core, "ses_test").await;
    let flushed = latest_card(&platform).await.to_string();
    assert!(
        flushed.contains("✅ 已允许一次：⚡ 执行 Shell 命令 `ls -la`"),
        "receipt dropped by a later flush: {}",
        flushed
    );
    assert!(
        !flushed.contains("🔐 **权限请求**"),
        "the block must not come back: {}",
        flushed
    );

    // Force a multi-card split: the receipt is anchored in the transcript, so
    // exactly one card of the chain carries it — and the split loop must not
    // overwrite that card with a later slice.
    app.cards
        .lock()
        .await
        .get_mut("ses_test")
        .unwrap()
        .acc
        .push_text(&"很长的回答。".repeat(2000));
    crate::bridge::render::flush_card(&app.core, "ses_test").await;
    let calls = platform.calls.lock().await.clone();
    assert!(
        calls.iter().any(|c| matches!(
            c,
            PlatformCall::UpdateMessage { card, .. } | PlatformCall::ReplyCard { card, .. }
                if card.to_string().contains("已允许一次")
        )),
        "the receipt must survive the split on the card that owns its anchor: {:?}",
        calls
    );

    // Regression for the split-loop overwrite this test caught: a finalized
    // continuation card carries its own slice (and could carry a receipt) and
    // must never be patched again — the mock's continuation id is "msg_reply".
    let continuation_updates = calls
        .iter()
        .filter(|c| matches!(c, PlatformCall::UpdateMessage { message_id, .. } if message_id == "msg_reply"))
        .count();
    assert_eq!(
        continuation_updates, 0,
        "a finalized continuation must never be overwritten: {:?}",
        calls
    );
}

/// A click that finds the request already resolved by another client still
/// updates the clicked card — with the neutral "handled elsewhere" receipt —
/// delivered in the same ack (ADR-0038, rule 4).
#[tokio::test]
async fn inline_permission_click_after_remote_resolution_gets_receipt() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let mut backend = MockBackend::new(realistic_parts());
    backend.permissions = vec![perm_req("per_1", "ses_test", "ls -la")];
    backend.reply_permission_not_found = true;
    let (app, _platform) = build_app(cfg, backend).await;
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

    let result = app
        .host_action(serde_json::json!({
            "action": "perm",
            "reply": "once",
            "session_id": "ses_test",
            "request_id": "per_1",
            "perm_label": "✅ 已允许一次",
            "perm_color": "green",
            "perm_body": "bash",
        }))
        .await
        .expect("a card-action result");
    assert_eq!(result.toast.as_deref(), Some("该权限已处理"));
    let ack = result
        .card
        .expect("the ack must carry the clicked card")
        .to_string();
    assert!(
        ack.contains("⏱ 已由其他客户端处理：⚡ 执行 Shell 命令 `ls -la`"),
        "neutral receipt missing: {}",
        ack
    );
    assert!(
        !ack.contains("🔐 **权限请求**"),
        "the resolved block must be gone: {}",
        ack
    );
    assert!(
        app.cards
            .lock()
            .await
            .get("ses_test")
            .unwrap()
            .acc
            .live_permissions()
            .is_empty()
    );
}

/// Seed the state the permission poller leaves behind (#175): one session
/// whose live card carries an inline permission block. The tests drive the
/// sweep by hand, so no background poll or render tick can repaint underneath
/// them — the sweep alone must put the receipt on the card.
async fn seed_inline_permission_card(app: &Arc<App>, session_id: &str, request_id: &str) {
    let mut acc = crate::bridge::streaming::StreamAccumulator::new("test");
    acc.reply_to_message_id = Some("msg_trigger".into());
    acc.add_interaction(crate::bridge::streaming::InteractionBlock::Permission(
        crate::bridge::streaming::PendingPermission {
            session_id: session_id.into(),
            request_id: request_id.into(),
            body: "bash ls -la".into(),
            target: "⚡ 执行 Shell 命令 `ls -la`".into(),
            directory: "/work".into(),
        },
    ));
    app.cards.lock().await.insert(
        session_id.to_string(),
        crate::bridge::streaming::CardSession::new(acc, Some("msg_live".into())),
    );
}

/// #175: drive one sweep over a session whose block vanished (resolved by
/// another client) and assert the hosting card was repainted with the neutral
/// receipt — the render poll is never spawned, so the sweep is the only thing
/// that can have done it.
async fn sweep_and_assert_receipt_on_the_card(app: &Arc<App>, platform: &Arc<RecordingPlatform>) {
    let mut seen = std::collections::HashSet::new();
    app.permission.sweep(&app.core, &mut seen).await;

    let calls = platform.calls.lock().await.clone();
    let patched = calls
        .iter()
        .filter_map(|c| match c {
            PlatformCall::UpdateMessage { message_id, card } if message_id == "msg_live" => {
                Some(card.to_string())
            }
            _ => None,
        })
        .next_back()
        .expect("the sweep itself must repaint the hosting card");
    assert!(
        patched.contains("⏱ 已由其他客户端处理：⚡ 执行 Shell 命令 `ls -la`"),
        "the receipt must be on the repainted card: {patched}"
    );
    assert!(
        !patched.contains("🔐 **权限请求**"),
        "the live block must be gone from the card: {patched}"
    );
    assert!(
        app.cards
            .lock()
            .await
            .get("ses_1")
            .unwrap()
            .acc
            .live_permissions()
            .is_empty(),
        "the accumulator must not keep the block live"
    );
}

/// #175: a request resolved by another client (OpenChamber, CLI) becomes its
/// Interaction Receipt on the live turn's card within ONE sweep — no reliance
/// on the ~1.5 s render tick.
#[tokio::test]
async fn sweep_repaints_a_live_turn_card_when_the_request_resolves_elsewhere() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    // The server no longer lists the request: another client resolved it.
    let (app, platform) = build_app(cfg, MockBackend::new(realistic_parts())).await;
    seed_session(&app, "ses_1", "/work").await;
    seed_inline_permission_card(&app, "ses_1", "per_1").await;

    sweep_and_assert_receipt_on_the_card(&app, &platform).await;
}

/// #175: a FINISHED turn's card gets the same receipt within one sweep — the
/// render poll stopped ticking, so only the sweep can repaint it. This is the
/// "the block lingers forever" case.
#[tokio::test]
async fn sweep_repaints_a_finished_turns_card_when_the_request_resolves_elsewhere() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let (app, platform) = build_app(cfg, MockBackend::new(realistic_parts())).await;
    seed_session(&app, "ses_1", "/work").await;
    seed_inline_permission_card(&app, "ses_1", "per_1").await;
    {
        // The turn ended: the poll loop is gone and the card shows its final
        // state. Nothing except the request sweep can still touch it.
        let mut cards = app.cards.lock().await;
        let acc = &mut cards.get_mut("ses_1").unwrap().acc;
        acc.card_state = crate::feishu::card::CardState::Done;
        acc.current_phase = None;
    }

    sweep_and_assert_receipt_on_the_card(&app, &platform).await;
}

/// A click whose card is over the split budget cannot ride the ack (Feishu
/// would reject the oversized card): the fallback flush finalizes the card and
/// sends the continuation, and the receipt rides the continuation — still
/// rendered from the accumulator, never lost.
#[tokio::test]
async fn permission_click_at_the_split_limit_falls_back_to_the_flushed_receipt() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let backend = Arc::new(MockBackend::new(realistic_parts()));
    let platform = Arc::new(RecordingPlatform::new());
    let app = Arc::new(App::new(cfg, backend.clone(), platform.clone()).unwrap());
    seed_session(&app, "ses_1", "/work").await;

    // A live card carrying the block and a timeline already past the split
    // budget (a poll rendered parts, then the click raced the next flush).
    let mut acc = crate::bridge::streaming::StreamAccumulator::new("test");
    acc.reply_to_message_id = Some("msg_1".into());
    acc.add_interaction(crate::bridge::streaming::InteractionBlock::Permission(
        crate::bridge::streaming::PendingPermission {
            session_id: "ses_1".into(),
            request_id: "per_1".into(),
            body: "bash ls -la".into(),
            target: "⚡ 执行 Shell 命令 `ls -la`".into(),
            directory: "/work".into(),
        },
    ));
    acc.push_text(&"很长的回答。".repeat(2000));
    {
        let mut cards = app.cards.lock().await;
        cards.insert(
            "ses_1".to_string(),
            crate::bridge::streaming::CardSession::new(acc, Some("msg_live".into())),
        );
    }

    let result = app
        .host_action(serde_json::json!({
            "action": "perm",
            "reply": "once",
            "session_id": "ses_1",
            "request_id": "per_1",
            "perm_label": "✅ 已允许一次",
            "perm_color": "green",
            "perm_body": "bash",
        }))
        .await
        .expect("a card-action result");
    assert!(
        result.card.is_none(),
        "an over-budget card degrades to the PATCH flush, not an oversized ack"
    );

    // The fallback flushed: the receipt is keyed at the click, which came
    // after the timeline content, so it rides after it — one card carries it,
    // and the resolved block does not come back.
    let continuation = latest_card(&platform).await.to_string();
    assert!(
        continuation.contains("✅ 已允许一次：⚡ 执行 Shell 命令 `ls -la`"),
        "the receipt must survive the split: {}",
        continuation
    );
    assert!(
        !continuation.contains("🔐 **权限请求**"),
        "the resolved block must not come back: {}",
        continuation
    );
    let all_cards: Vec<String> = platform
        .calls
        .lock()
        .await
        .iter()
        .filter_map(|c| match c {
            PlatformCall::UpdateMessage { card, .. } | PlatformCall::ReplyCard { card, .. } => {
                Some(card.to_string())
            }
            _ => None,
        })
        .collect();
    assert_eq!(
        all_cards.iter().filter(|c| c.contains("已允许一次")).count(),
        1,
        "the receipt must render exactly once across the chain: {:?}",
        all_cards
    );
    assert!(
        app.cards
            .lock()
            .await
            .get("ses_1")
            .unwrap()
            .acc
            .live_permissions()
            .is_empty()
    );
}

/// A receipt is keyed at the moment of resolution: it renders below the content
/// that preceded the click and above content streamed after it — not at the
/// bottom of the card with the tail (ADR-0038, rule 4).
#[tokio::test]
async fn interaction_receipt_renders_at_the_interaction_position() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let backend = Arc::new(MockBackend::new(realistic_parts()));
    let platform = Arc::new(RecordingPlatform::new());
    let app = Arc::new(App::new(cfg, backend, platform.clone()).unwrap());
    seed_session(&app, "ses_test", "/work").await;

    // 第一段 → [permission A] → a reasoning panel → [permission B]
    let mut acc = crate::bridge::streaming::StreamAccumulator::new("test");
    acc.reply_to_message_id = Some("msg_1".into());
    acc.push_text("第一段。");
    acc.add_interaction(crate::bridge::streaming::InteractionBlock::Permission(
        crate::bridge::streaming::PendingPermission {
            session_id: "ses_test".into(),
            request_id: "per_1".into(),
            body: "bash ls -la".into(),
            target: "⚡ 执行 Shell 命令 `ls -la`".into(),
            directory: "/work".into(),
        },
    ));
    acc.push_reasoning("正在思考。");
    acc.add_interaction(crate::bridge::streaming::InteractionBlock::Permission(
        crate::bridge::streaming::PendingPermission {
            session_id: "ses_test".into(),
            request_id: "per_2".into(),
            body: "bash cargo build".into(),
            target: "⚡ 执行 Shell 命令 `cargo build`".into(),
            directory: "/work".into(),
        },
    ));
    app.cards.lock().await.insert(
        "ses_test".to_string(),
        crate::bridge::streaming::CardSession::new(acc, Some("msg_live".into())),
    );

    let ack = click_perm(&app, "once", "per_1", "✅ 已允许一次").await;
    assert!(
        ack.contains("✅ 已允许一次：⚡ 执行 Shell 命令 `ls -la`"),
        "receipt missing: {}",
        ack
    );
    let ack = click_perm(&app, "reject", "per_2", "🚫 已拒绝").await;
    assert!(
        ack.contains("🚫 已拒绝：⚡ 执行 Shell 命令 `cargo build`"),
        "receipt missing: {}",
        ack
    );

    // ... then the AI keeps streaming: 第二段 must land BELOW both receipts.
    app.cards
        .lock()
        .await
        .get_mut("ses_test")
        .unwrap()
        .acc
        .push_text("第二段。");
    crate::bridge::render::flush_card(&app.core, "ses_test").await;

    let card = final_card(&platform).await;
    let elements = card["body"]["elements"].as_array().expect("elements");
    let index_of = |needle: &str| {
        elements
            .iter()
            .position(|e| {
                e["content"]
                    .as_str()
                    .is_some_and(|content| content.contains(needle))
            })
            .unwrap_or_else(|| panic!("{} not on the card: {}", needle, card))
    };
    let first = index_of("第一段。");
    let receipt_a = index_of("已允许一次");
    let receipt_b = index_of("已拒绝");
    let second = index_of("第二段。");
    assert!(
        first < receipt_a && receipt_a < receipt_b && receipt_b < second,
        "receipts must anchor in interaction order, between 第一段 and 第二段: {}",
        card
    );
}

/// A part the render poll delivers LATE — the server wrote it before the click,
/// but cola only renders it afterwards — still lands ABOVE the receipt: the
/// timeline is ordered by each part's own start time, not by when cola
/// rendered it. This is the "bash and the thinking ran behind the receipt"
/// report (ADR-0038, rule 4).
#[tokio::test]
async fn late_rendered_command_lands_above_the_receipt() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let backend = Arc::new(MockBackend::new(realistic_parts()));
    let platform = Arc::new(RecordingPlatform::new());
    let app = Arc::new(App::new(cfg, backend, platform.clone()).unwrap());
    seed_session(&app, "ses_test", "/work").await;

    // The request poll surfaces the permission before ANY of the turn's parts
    // have been rendered (the race that put receipts above their command).
    let mut acc = crate::bridge::streaming::StreamAccumulator::new("test");
    acc.reply_to_message_id = Some("msg_1".into());
    acc.add_interaction(crate::bridge::streaming::InteractionBlock::Permission(
        crate::bridge::streaming::PendingPermission {
            session_id: "ses_test".into(),
            request_id: "per_1".into(),
            body: "bash ls -la".into(),
            target: "⚡ 执行 Shell 命令 `ls -la`".into(),
            directory: "/work".into(),
        },
    ));
    app.cards.lock().await.insert(
        "ses_test".to_string(),
        crate::bridge::streaming::CardSession::new(acc, Some("msg_live".into())),
    );

    // The operator clicks Allow; the receipt is keyed at this moment.
    let clicked_at = chrono::Utc::now().timestamp_millis();
    let ack = click_perm(&app, "once", "per_1", "✅ 已允许一次").await;
    assert!(
        ack.contains("✅ 已允许一次：⚡ 执行 Shell 命令 `ls -la`"),
        "receipt missing: {}",
        ack
    );

    // The render poll then catches up with the parts the server had already
    // written BEFORE the click: the reasoning behind the command, and the
    // command's own panel.
    app.cards
        .lock()
        .await
        .get_mut("ses_test")
        .unwrap()
        .acc
        .push_reasoning_at(Some(clicked_at - 300), "先看一下目录里有什么。");
    app.cards
        .lock()
        .await
        .get_mut("ses_test")
        .unwrap()
        .acc
        .push_tool_at(
            Some(clicked_at - 200),
            "call_1",
            crate::feishu::card::tool_render::ToolPanel {
                name: "bash".into(),
                status: "running".into(),
                input: Some(serde_json::json!({ "command": "ls -la" })),
                output: None,
            },
        );
    crate::bridge::render::flush_card(&app.core, "ses_test").await;

    // While the command runs, its panel is live tail content (ADR-0045): it
    // renders on the live card, below the timeline — the receipt included.
    let live = final_card(&platform).await;
    assert!(
        live.to_string().contains("ls -la"),
        "the running panel must ride the live card: {live}"
    );

    // Settling moves it into the timeline at its OWN start time, so it lands
    // above the receipt that resolved its permission despite rendering late.
    app.cards
        .lock()
        .await
        .get_mut("ses_test")
        .unwrap()
        .acc
        .push_tool_at(
            Some(clicked_at - 200),
            "call_1",
            crate::feishu::card::tool_render::ToolPanel {
                name: "bash".into(),
                status: "completed".into(),
                input: Some(serde_json::json!({ "command": "ls -la" })),
                output: Some("src".into()),
            },
        );
    crate::bridge::render::flush_card(&app.core, "ses_test").await;

    let card = final_card(&platform).await;
    let elements = card["body"]["elements"].as_array().expect("elements");
    let index_of = |needle: &str| {
        elements
            .iter()
            .position(|e| {
                e["content"].as_str().is_some_and(|c| c.contains(needle))
                    || e["header"]["title"]["content"]
                        .as_str()
                        .is_some_and(|t| t.contains(needle))
            })
            .unwrap_or_else(|| panic!("{} not on the card: {}", needle, card))
    };
    let reasoning = index_of("推理过程");
    let command = index_of("bash");
    let receipt = index_of("已允许一次");
    assert!(
        reasoning < command && command < receipt,
        "parts written before the click must stay above the receipt even when \
         rendered late: {}",
        card
    );
}
