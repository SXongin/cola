use crate::backend::ToolStatus;
use crate::bridge::test_support::*;

#[tokio::test]
async fn permission_poller_sends_card_and_card_action_replies() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let mut backend = MockBackend::new(realistic_parts());
    backend.ask_permissions(vec![crate::backend::types::PermissionRequest {
        request_id: "per_1".into(),
        session_id: Some("ses_test".into()),
        permission: Some("bash".into()),
        patterns: vec!["ls -la".into()],
        metadata: None,
        always: Vec::new(),
        always_supported: true,
    }]);
    let (app, _platform) = build_app(cfg, backend).await;

    // Seed a session + accumulator so the poller has a reply target.
    seed_live_card(&app, "ses_test", "om_live").await;

    // Run the permission poller briefly.
    tokio::spawn({
        let app = app.clone();
        async move {
            app.permission
                .poll_interval_ms
                .store(50, std::sync::atomic::Ordering::Relaxed);
            let _ = app.permission.poll_loop(&app.flow_handles()).await;
        }
    });
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;

    // The session has an active streaming card, so the permission is surfaced
    // INLINE on it (one-card-per-turn) — not as a separate card.
    let perm_inline = Turn::live_permissions(&app.cards_handle(), "ses_test").await;
    assert_eq!(perm_inline.len(), 1, "permission should be inlined");
    assert_eq!(perm_inline[0].0, "per_1");
    // The streaming card itself renders the inline permission section.
    let card = Turn::rendered_card(&app.cards_handle(), "ses_test")
        .await
        .expect("card renders");
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
        Turn::live_permissions(&app.cards_handle(), "ses_test")
            .await
            .is_empty()
    );
}

/// A click on a STANDALONE request card stays standalone even when the
/// Session's card map still holds a finished card (ADR-0100): the map entry is
/// not a live card, so the click must return its replacement result card
/// rather than being routed down the inline path — whose ack carries no card
/// because no live accumulator renders the block.
#[tokio::test]
async fn a_standalone_click_ignores_a_finished_card_in_the_map() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let mut backend = MockBackend::new(realistic_parts());
    backend.ask_permission(perm_request("per_1", "ses_1", "ls -la"));
    let (app, platform) = build_app(cfg, backend).await;
    seed_session(&app, "ses_1", "/work").await;

    // No live card: the request is surfaced on its own card.
    let mut seen = std::collections::HashSet::new();
    app.permission.sweep(&app.flow_handles(), &mut seen).await;
    assert_eq!(platform.sent_cards().await.len(), 1, "one standalone card");
    let standalone = app
        .permission
        .sent_cards
        .lock()
        .await
        .get("per_1")
        .expect("the standalone card is recorded")
        .message_id
        .clone();

    // The Turn finished, but its card stays in the map and still names a reply
    // target: neither fact makes it a live host.
    let cards = app.cards_handle();
    Turn::seed_card(&cards, "ses_1", Some("om_finished")).await;
    Turn::set_reply_target(&cards, "ses_1", "msg_1").await;
    Turn::set_card_state(&cards, "ses_1", crate::feishu::card::CardState::Done).await;

    let result = app
        .host_action(serde_json::json!({
            "action": "perm",
            "reply": "once",
            "session_id": "ses_1",
            "directory": "/work",
            "request_id": "per_1",
            "open_message_id": standalone,
            "perm_label": "✅ 已允许一次",
            "perm_color": "green",
        }))
        .await
        .expect("a card-action result");
    let ack = card_text(
        result
            .card
            .as_ref()
            .expect("a standalone click returns its replacement result card"),
    );
    assert!(ack.contains("✅ 已允许一次"), "receipt missing: {ack}");
    assert!(
        !ack.contains("🔐 **权限请求**"),
        "the result card carries no live block: {ack}"
    );
}

/// The result body is rebuilt at click time, never read from the callback. An
/// allow click re-reads the pending request to run the completeness proof, so
/// that same request is what the result card describes.
#[tokio::test]
async fn a_permission_result_card_rebuilds_its_body_from_the_verified_request() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let mut backend = MockBackend::new(realistic_parts());
    backend.ask_permission(perm_request("per_1", "ses_1", "ls -la"));
    let (app, platform) = build_app(cfg, backend).await;
    seed_session(&app, "ses_1", "/work").await;
    let mut seen = std::collections::HashSet::new();
    app.permission.sweep(&app.flow_handles(), &mut seen).await;
    assert_eq!(platform.sent_cards().await.len(), 1, "one standalone card");
    let standalone = app
        .permission
        .sent_cards
        .lock()
        .await
        .get("per_1")
        .expect("the standalone card is recorded")
        .message_id
        .clone();

    let result = app
        .host_action(serde_json::json!({
            "action": "perm",
            "reply": "once",
            "session_id": "ses_1",
            "directory": "/work",
            "request_id": "per_1",
            "open_message_id": standalone,
            "perm_label": "✅ 已允许一次",
            "perm_color": "green",
            // An older build's persisted card still carries the body; it must
            // be ignored, not echoed into the result.
            "perm_body": "STALE CALLBACK BODY",
        }))
        .await
        .expect("a card-action result");
    let text = card_text(result.card.as_ref().expect("the result card"));
    assert!(
        text.contains("执行 Shell 命令") && text.contains("ls -la"),
        "the result names the request re-read from the server: {text}"
    );
    assert!(
        !text.contains("STALE CALLBACK BODY"),
        "a callback body from an older build is ignored: {text}"
    );
}

/// A rejection runs no verification read, so its result card names what the
/// click showed: the callback's display label (plus the block's recorded target
/// when one is registered) — never a body echoed through the callback.
#[tokio::test]
async fn a_reject_result_card_names_the_callback_label_and_ignores_a_stale_body() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let mut backend = MockBackend::new(realistic_parts());
    backend.ask_permission(perm_request("per_1", "ses_1", "ls -la"));
    let (app, _platform) = build_app(cfg, backend).await;
    seed_session(&app, "ses_1", "/work").await;
    let mut seen = std::collections::HashSet::new();
    app.permission.sweep(&app.flow_handles(), &mut seen).await;
    let standalone = app
        .permission
        .sent_cards
        .lock()
        .await
        .get("per_1")
        .expect("the standalone card is recorded")
        .message_id
        .clone();

    let result = app
        .host_action(serde_json::json!({
            "action": "perm",
            "reply": "reject",
            "session_id": "ses_1",
            "directory": "/work",
            "request_id": "per_1",
            "open_message_id": standalone,
            "perm_label": "🚫 已拒绝",
            "perm_color": "red",
            "perm_body": "STALE CALLBACK BODY",
        }))
        .await
        .expect("a card-action result");
    let text = card_text(result.card.as_ref().expect("the result card"));
    assert!(
        text.contains("🚫 已拒绝"),
        "the result names the callback label: {text}"
    );
    assert!(
        text.contains("ls -la") && text.contains("执行 Shell 命令"),
        "the result names the standalone card's summary: {text}"
    );
    assert!(
        !text.contains("STALE CALLBACK BODY"),
        "a callback body from an older build is ignored: {text}"
    );
}

/// An allow click whose request is no longer in the pending list (the server
/// raced past it) still replies, and the result body comes from the card's
/// recorded summary rather than a `permission:` placeholder.
#[tokio::test]
async fn an_allow_click_without_a_pending_request_uses_the_recorded_summary() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    // Nothing pending on the server: the verification read finds no request.
    let backend = MockBackend::new(realistic_parts());
    let (app, _platform) = build_app(cfg, backend).await;
    seed_session(&app, "ses_1", "/work").await;
    app.permission.sent_cards.lock().await.insert(
        "per_1".into(),
        crate::bridge::request::flow::SentCard {
            message_id: "om_card".into(),
            summary: "⚡ **执行 Shell 命令**\n- `ls -la`".into(),
            directory: "/work".into(),
            session_id: "ses_1".into(),
        },
    );

    let result = app
        .host_action(serde_json::json!({
            "action": "perm",
            "reply": "once",
            "session_id": "ses_1",
            "directory": "/work",
            "request_id": "per_1",
            "open_message_id": "om_card",
            "perm_label": "✅ 已允许一次",
            "perm_color": "green",
        }))
        .await
        .expect("a card-action result");
    let text = card_text(result.card.as_ref().expect("the result card"));
    assert!(
        text.contains("ls -la") && text.contains("执行 Shell 命令"),
        "the result names the summary the card showed: {text}"
    );
    assert!(
        !text.contains("Permission: once"),
        "the placeholder never appears: {text}"
    );
}

/// The idle Session Card is equally not an inline host (ADR-0100): a standalone
/// click must still return its own replacement result card, never the empty ack
/// the inline path would imply.
#[tokio::test]
async fn a_standalone_click_ignores_an_idle_card_in_the_map() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let mut backend = MockBackend::new(realistic_parts());
    backend.ask_permission(perm_request("per_1", "ses_1", "ls -la"));
    let (app, platform) = build_app(cfg, backend).await;
    seed_session(&app, "ses_1", "/work").await;

    // No live card: the request is surfaced on its own card.
    let mut seen = std::collections::HashSet::new();
    app.permission.sweep(&app.flow_handles(), &mut seen).await;
    assert_eq!(platform.sent_cards().await.len(), 1, "one standalone card");
    let standalone = app
        .permission
        .sent_cards
        .lock()
        .await
        .get("per_1")
        .expect("the standalone card is recorded")
        .message_id
        .clone();

    // The idle read is terminal and carries no live block; a click must not be
    // routed down its (non-existent) inline path.
    let cards = app.cards_handle();
    Turn::seed_card(&cards, "ses_1", Some("om_idle")).await;
    Turn::set_reply_target(&cards, "ses_1", "msg_1").await;
    Turn::set_card_state(&cards, "ses_1", crate::feishu::card::CardState::Idle).await;

    let result = app
        .host_action(serde_json::json!({
            "action": "perm",
            "reply": "once",
            "session_id": "ses_1",
            "directory": "/work",
            "request_id": "per_1",
            "open_message_id": standalone,
            "perm_label": "✅ 已允许一次",
            "perm_color": "green",
        }))
        .await
        .expect("a card-action result");
    let ack = card_text(
        result
            .card
            .as_ref()
            .expect("a standalone click returns its replacement result card"),
    );
    assert!(ack.contains("✅ 已允许一次"), "receipt missing: {ack}");
    assert!(
        !ack.contains("🔐 **权限请求**"),
        "the result card carries no live block: {ack}"
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
    backend.ask_permissions(vec![crate::backend::types::PermissionRequest {
        request_id: "per_1".into(),
        session_id: Some("ses_test".into()),
        permission: Some("bash".into()),
        patterns: vec!["ls -la".into()],
        metadata: None,
        always: Vec::new(),
        always_supported: true,
    }]);
    let (app, platform) = build_app(cfg, backend).await;

    seed_live_card(&app, "ses_test", "om_live").await;

    tokio::spawn({
        let app = app.clone();
        async move {
            app.permission
                .poll_interval_ms
                .store(50, std::sync::atomic::Ordering::Relaxed);
            let _ = app.permission.poll_loop(&app.flow_handles()).await;
        }
    });
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    assert!(
        !Turn::live_permissions(&app.cards_handle(), "ses_test")
            .await
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
    backend.permission_resolved_elsewhere();
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
    backend.fail_permission_replies(1);
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
    backend.ask_permissions(vec![crate::backend::types::PermissionRequest {
        request_id: "per_hung".into(),
        session_id: Some("ses_test".into()),
        permission: Some("bash".into()),
        patterns: vec!["ls -la".into()],
        metadata: None,
        always: Vec::new(),
        always_supported: true,
    }]);
    // The first list call hangs forever, like a request in flight when the
    // server was SIGTERM'd; later calls serve normally.
    backend.hang_permission_lists(1);
    let (app, _platform) = build_app(cfg, backend).await;

    // Seed a session + accumulator so the poller has a reply target.
    seed_live_card(&app, "ses_test", "om_live").await;

    tokio::spawn({
        let app = app.clone();
        async move {
            app.permission
                .poll_interval_ms
                .store(20, std::sync::atomic::Ordering::Relaxed);
            app.permission
                .list_timeout_ms
                .store(50, std::sync::atomic::Ordering::Relaxed);
            let _ = app.permission.poll_loop(&app.flow_handles()).await;
        }
    });

    // Without a bound on the list call the poller sits on the first hung
    // call forever and the permission never surfaces.
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(2);
    loop {
        let surfaced = Turn::has_interaction_in(&app.cards_handle(), "ses_test", "per_hung").await;
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
    backend.ask_permissions(vec![
        crate::backend::types::PermissionRequest {
            request_id: "per_aa_toggle".into(),
            session_id: Some("ses_test".into()),
            permission: Some("bash".into()),
            patterns: vec!["ls -la".into()],
            metadata: None,
            always: Vec::new(),
            always_supported: true,
        },
        // A second, already-pending request for the same session — the
        // toggle approves it too and must drop its inline section in the
        // same interaction, not leave it lingering until the next poll.
        crate::backend::types::PermissionRequest {
            request_id: "per_aa_other".into(),
            session_id: Some("ses_test".into()),
            permission: Some("edit".into()),
            // An edit is about its file: the receipt must name the filepath,
            // not the looser pattern.
            patterns: vec!["*.rs".into()],
            metadata: Some(serde_json::json!({ "filepath": "src/main.rs" })),
            always: Vec::new(),
            always_supported: true,
        },
    ]);
    let perm_calls = backend.reply_permission_calls.clone();
    let (app, platform) = build_app(cfg, backend).await;

    // Seed a session (auto_accept defaults to false) + a live streaming card
    // so the poller surfaces the permission INLINE.
    seed_live_card(&app, "ses_test", "om_live").await;

    tokio::spawn({
        let app = app.clone();
        async move {
            app.permission
                .poll_interval_ms
                .store(50, std::sync::atomic::Ordering::Relaxed);
            let _ = app.permission.poll_loop(&app.flow_handles()).await;
        }
    });
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;

    // Permission inlined on the streaming card, auto_accept still off.
    assert_eq!(
        Turn::live_permissions(&app.cards_handle(), "ses_test")
            .await
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
        Turn::live_permissions(&app.cards_handle(), "ses_test")
            .await
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
    backend.ask_permission(perm_request("per_1", "ses_1", "ls -la"));
    let (app, platform) = build_app(cfg, backend).await;
    seed_session(&app, "ses_1", "/work").await;
    seed_inline_permission_card(&app, "ses_1", "per_1").await;
    // Render once so the block is on the card AND its handle is registered.
    crate::bridge::turn::Turn::flush_card(&app.cards_handle(), "ses_1").await;
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
                kind: crate::bridge::request::kind::InteractionKind::Permission,
                session_id: "ses_1".into(),
                directory: "/work".into(),
                target: "⚡ 执行 Shell 命令 `ls -la`".into(),
            }],
        );
    }

    send_command(&app, "/autoaccept on", "msg_cmd").await;

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
        Turn::live_permissions(&app.cards_handle(), "ses_1")
            .await
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
    app.permission.sweep(&app.flow_handles(), &mut seen).await;
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
    backend.ask_permission(perm_request("per_1", "ses_1", "ls -la"));
    let (app, platform) = build_app(cfg, backend).await;
    seed_session(&app, "ses_1", "/work").await;
    seed_inline_permission_card(&app, "ses_1", "per_1").await;
    crate::bridge::turn::Turn::flush_card(&app.cards_handle(), "ses_1").await;
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
    app.permission.sweep(&app.flow_handles(), &mut seen).await;
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
    backend.ask_permission(perm_request("per_1", "ses_1", "ls -la"));
    let backend = Arc::new(backend);
    let platform = Arc::new(RecordingPlatform::new());
    let app = Arc::new(App::new(cfg, backend.clone(), platform.clone()).unwrap());
    seed_session(&app, "ses_1", "/work").await;
    seed_inline_permission_card(&app, "ses_1", "per_1").await;
    crate::bridge::turn::Turn::flush_card(&app.cards_handle(), "ses_1").await;
    assert_eq!(
        app.card_handles.lock().await.message_of("per_1"),
        Some("msg_live"),
        "precondition: the block's handle is recorded"
    );

    // cola took the answer claim (a click, or the auto-accept approval) and the
    // reply landed: the request is off the pending list, its settlement has not
    // run yet.
    assert!(
        app.permission
            .try_mark_answered(&app.requests_handle(), "per_1")
            .await
    );
    backend.permission_resolved_by_another("per_1").await;

    let mut seen = std::collections::HashSet::new();
    app.permission.sweep(&app.flow_handles(), &mut seen).await;

    // The sweep left it alone: no neutral receipt reached the card, the handle
    // and the accumulator still carry the block for the in-flight settlement.
    let neutral = platform.calls.lock().await.iter().any(|c| {
        matches!(
            c,
            PlatformCall::UpdateMessage { card, .. }
                if card_text(card).contains("⏱ 已由其他客户端处理")
        )
    });
    assert!(!neutral, "a claimed approval must not be settled by the sweep");
    assert_eq!(
        app.card_handles.lock().await.message_of("per_1"),
        Some("msg_live"),
        "the handle still names the card the settlement will repaint"
    );
    assert!(
        Turn::live_permissions(&app.cards_handle(), "ses_1")
            .await
            .iter()
            .any(|(id, _)| id == "per_1"),
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
    backend.ask_permission(perm_request("per_1", "ses_1", "ls -la"));
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
    backend.ask_permission(perm_request("per_1", "ses_1", "ls -la"));
    // The session is still live (an external/other-client run): the stale copy
    // keeps the neutral vocabulary.
    backend.with_session_status("ses_1", Some(crate::backend::types::SessionStatus::Busy));
    let (app, platform) = build_app(cfg, backend).await;
    seed_session(&app, "ses_1", "/work").await;
    // The poller surfaced this one as a standalone card (no live turn card).
    app.permission.sent_cards.lock().await.insert(
        "per_1".into(),
        crate::bridge::request::flow::SentCard {
            message_id: "om_sent".into(),
            summary: "bash ls -la".into(),
            directory: "/work".into(),
            session_id: "ses_1".into(),
        },
    );

    send_command(&app, "/autoaccept on", "msg_cmd").await;

    // Approved server-side, but this seam has nothing inline to settle: the
    // entry survives so the sweep can repaint the card.
    assert!(
        app.permission.sent_cards.lock().await.contains_key("per_1"),
        "a standalone card's entry must survive the command"
    );

    let mut seen = std::collections::HashSet::new();
    app.permission.sweep(&app.flow_handles(), &mut seen).await;
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
    mock.ask_permissions(vec![crate::backend::types::PermissionRequest {
        request_id: "per_aa".into(),
        session_id: Some("ses_test".into()),
        permission: Some("bash".into()),
        patterns: vec!["ls".into()],
        metadata: None,
        always: Vec::new(),
        always_supported: true,
    }]);
    let perm_calls = mock.reply_permission_calls.clone();
    let reply_sessions = mock.reply_permission_sessions.clone();
    let (app, platform) = build_app(cfg, mock).await;

    // Enable `/autoaccept` on the session.
    seed_entry(
        &app,
        crate::config::SessionEntry {
            client_id: crate::config::DEFAULT_CLIENT_ID.into(),
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
            let _ = app.permission.poll_loop(&app.flow_handles()).await;
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
    // The auto-accept reply is session-scoped on V2: it must carry the
    // request's own session id.
    assert_eq!(
        reply_sessions.lock().await.as_slice(),
        &["ses_test".to_string()],
        "auto-accept must route via the request's session id"
    );
    let sent = platform.calls.lock().await.clone();
    assert!(
        !sent.iter().any(|c| {
            if let PlatformCall::ReplyCard { card, .. } = c {
                card_text(card).contains("权限请求")
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
    mock.ask_permissions(vec![crate::backend::types::PermissionRequest {
        request_id: "per_pending".into(),
        session_id: Some("ses_test".into()),
        permission: Some("bash".into()),
        patterns: vec!["ls -la".into()],
        metadata: None,
        always: Vec::new(),
        always_supported: true,
    }]);
    let perm_calls = mock.reply_permission_calls.clone();
    let (app, _platform) = build_app(cfg, mock).await;

    // Session already mapped but autoaccept OFF — the permission would have
    // been surfaced as a card before the user enabled it.
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

    // Now the user turns autoaccept on via the command.
    send_command(&app, "/autoaccept on", "msg_cmd").await;

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
    mock.ask_permissions(vec![crate::backend::types::PermissionRequest {
        request_id: "per_child".into(),
        session_id: Some("ses_child".into()),
        permission: Some("bash".into()),
        patterns: vec!["rm -rf x".into()],
        metadata: None,
        always: Vec::new(),
        always_supported: true,
    }]);
    mock.with_session_parent("ses_child", "ses_test");
    let perm_calls = mock.reply_permission_calls.clone();
    let (app, _platform) = build_app(cfg, mock).await;

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

    send_command(&app, "/autoaccept on", "msg_cmd").await;

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
    // No pending permissions on the server — the card cola sent is stale —
    // and the run is still live: another client answered it.
    let mut backend = MockBackend::new(realistic_parts());
    backend.with_session_status("ses_1", Some(crate::backend::types::SessionStatus::Busy));
    let (app, platform) = build_app(cfg, backend).await;
    app.permission.sent_cards.lock().await.insert(
        "per_stale".into(),
        crate::bridge::request::flow::SentCard {
            message_id: "om_sent_card".into(),
            summary: "bash ls -la".into(),
            directory: "/work".into(),
            session_id: "ses_1".into(),
        },
    );

    tokio::spawn({
        let app = app.clone();
        async move {
            app.permission
                .poll_interval_ms
                .store(50, std::sync::atomic::Ordering::Relaxed);
            let _ = app.permission.poll_loop(&app.flow_handles()).await;
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
        card_text(&stale).contains("已处理"),
        "stale card should show as handled: {}",
        stale
    );
    assert!(
        card_text(&stale).contains("bash ls -la"),
        "stale card should keep the original request text: {}",
        stale
    );
}

/// #144: a directory whose list call fails must not clear its live permission
/// surfaces — the standalone card and the inline section both survive until a
/// SUCCESSFUL list proves the request gone.
#[tokio::test]
async fn failed_directory_list_keeps_permission_surfaces() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    // The requests are resolved elsewhere while the run stays live, so the
    // healed directory's cleanup leaves the neutral receipt.
    let mut backend = MockBackend::new(realistic_parts());
    backend.with_session_status("ses_1", Some(crate::backend::types::SessionStatus::Busy));
    let backend = Arc::new(backend);
    let platform = Arc::new(RecordingPlatform::new());
    let app = Arc::new(App::new(cfg, backend.clone(), platform.clone()).unwrap());
    seed_session(&app, "ses_1", "/work").await;

    assert_failed_dir_keeps_surfaces(
        &app,
        &app.permission,
        &backend.hang_list_permissions,
        &platform,
        FailedDirSurfaces {
            card_id: "per_card",
            card_message_id: "om_card",
            inline_id: "per_inline",
            inline_request: crate::bridge::request::kind::PendingRequest::Permission(perm_request(
                "per_inline",
                "ses_1",
                "ls -la",
            )),
        },
    )
    .await;
}

/// #421 × #130: a directory whose list failed while it was already gone is
/// skipped after one WARN — but a skipped directory still counts as a failed
/// one, so its live permission surfaces are never read as resolved.
#[tokio::test]
async fn skipped_dead_directory_keeps_live_permission_surfaces() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let backend = Arc::new(MockBackend::new(realistic_parts()));
    let platform = Arc::new(RecordingPlatform::new());
    let app = Arc::new(App::new(cfg, backend.clone(), platform.clone()).unwrap());

    // Mapped while it existed, then gone from disk; every list fails.
    let work = dir.path().join("work");
    std::fs::create_dir(&work).unwrap();
    let work = work.to_str().unwrap().to_string();
    seed_session(&app, "ses_1", &work).await;
    app.permission.sent_cards.lock().await.insert(
        "per_card".into(),
        crate::bridge::request::flow::SentCard {
            message_id: "om_card".into(),
            summary: "待处理的请求".into(),
            directory: work.clone(),
            session_id: "ses_1".into(),
        },
    );
    std::fs::remove_dir(&work).unwrap();
    backend
        .fail_list_permissions
        .store(true, std::sync::atomic::Ordering::SeqCst);

    let mut seen = std::collections::HashSet::new();
    // The failing sweep marks the directory dead; the skipped sweep must
    // still read it as failed (#130), not as "the request left the list".
    app.permission.sweep(&app.flow_handles(), &mut seen).await;
    app.permission.sweep(&app.flow_handles(), &mut seen).await;

    assert!(
        app.permission.sent_cards.lock().await.contains_key("per_card"),
        "a skipped dead directory must not mark its card stale"
    );
    let calls = platform.calls.lock().await.clone();
    assert!(
        calls.iter().all(|c| !matches!(
            c,
            PlatformCall::UpdateMessage { message_id, .. } if message_id == "om_card"
        )),
        "a skipped dead directory must not re-render its surfaces: {calls:?}"
    );
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
    backend.ask_permissions(vec![crate::backend::types::PermissionRequest {
        request_id: "per_topic".into(),
        session_id: Some("ses_topic".into()),
        permission: Some("bash".into()),
        patterns: vec!["cargo build".into()],
        metadata: None,
        always: Vec::new(),
        always_supported: true,
    }]);
    let (app, platform) = build_app(cfg, backend).await;

    // Map the session to a TOPIC (thread_id != chat_id) with an anchor
    // message inside the topic, no accumulator.
    seed_entry(
        &app,
        crate::config::SessionEntry {
            client_id: crate::config::DEFAULT_CLIENT_ID.into(),
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
            let _ = app.permission.poll_loop(&app.flow_handles()).await;
        }
    });
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;

    // The separate card must be reply'd to the topic anchor (not sent to
    // the chat top level).
    let calls = platform.calls.lock().await.clone();
    let perm_card = calls.iter().find_map(|c| match c {
        PlatformCall::ReplyCard { reply_to, card }
            if reply_to == "msg_in_topic_anchor" && card_text(card).contains("cargo build") =>
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

fn perm_req(id: &str, session: &str, pattern: &str) -> crate::backend::types::PermissionRequest {
    crate::backend::types::PermissionRequest {
        request_id: id.into(),
        session_id: Some(session.into()),
        permission: Some("bash".into()),
        patterns: vec![pattern.into()],
        metadata: None,
        always: Vec::new(),
        always_supported: true,
    }
}

/// Every Rich Text companion the platform recorded, in call order. The
/// companion lane is the one `post` reply the permission flow sends.
async fn posted_companions(platform: &RecordingPlatform) -> Vec<serde_json::Value> {
    platform
        .calls
        .lock()
        .await
        .iter()
        .filter_map(|call| match call {
            PlatformCall::SendMessageReply {
                msg_type, content, ..
            } if msg_type == "post" => Some(content.clone()),
            _ => None,
        })
        .collect()
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
    backend.ask_permissions(vec![
        perm_req("per_once", "ses_test", "ls -la"),
        perm_req("per_always", "ses_test", "cargo build"),
        perm_req("per_deny", "ses_test", "rm -rf target"),
    ]);
    let perm_calls = backend.reply_permission_calls.clone();
    let (app, _platform) = build_app(cfg, backend).await;
    seed_live_card(&app, "ses_test", "om_live").await;
    tokio::spawn({
        let app = app.clone();
        async move {
            app.permission
                .poll_interval_ms
                .store(50, std::sync::atomic::Ordering::Relaxed);
            let _ = app.permission.poll_loop(&app.flow_handles()).await;
        }
    });
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    assert_eq!(
        Turn::live_permissions(&app.cards_handle(), "ses_test")
            .await
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
    let cards = app.cards_handle();
    assert!(Turn::live_permissions(&cards, "ses_test").await.is_empty());
    let rendered = Turn::rendered_card(&cards, "ses_test").await.unwrap().to_string();
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
    let cards = app.cards_handle();
    Turn::seed_card(&cards, "ses_test", Some("msg_live")).await;
    Turn::set_reply_target(&cards, "ses_test", "msg_1").await;
    Turn::push_reasoning(&cards, "ses_test", "用户想让我分析目录。").await;
    Turn::push_text(&cards, "ses_test", "当前目录有 src/ 和 Cargo.toml。").await;
    Turn::add_permission(
        &cards,
        "ses_test",
        &perm_request("per_1", "ses_test", "ls -la"),
        "/work",
    )
    .await;

    let ack = click_perm(&app, "once", "per_1", "✅ 已允许一次").await;
    assert!(
        ack.contains("✅ 已允许一次：⚡ 执行 Shell 命令 `ls -la`"),
        "receipt missing: {}",
        ack
    );

    // A new part arrives: the flush re-renders the receipt from the
    // accumulator (the render source of truth), not just into the ack.
    Turn::push_text(&app.cards_handle(), "ses_test", "新的进展。").await;
    crate::bridge::turn::Turn::flush_card(&app.cards_handle(), "ses_test").await;
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
    Turn::push_text(&app.cards_handle(), "ses_test", &"很长的回答。".repeat(2000)).await;
    crate::bridge::turn::Turn::flush_card(&app.cards_handle(), "ses_test").await;
    let calls = platform.calls.lock().await.clone();
    assert!(
        calls.iter().any(|c| matches!(
            c,
            PlatformCall::UpdateMessage { card, .. } | PlatformCall::ReplyCard { card, .. }
                if card_text(card).contains("已允许一次")
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
    backend.ask_permission(perm_req("per_1", "ses_test", "ls -la"));
    backend.permission_resolved_elsewhere();
    let (app, _platform) = build_app(cfg, backend).await;
    seed_live_card(&app, "ses_test", "om_live").await;
    tokio::spawn({
        let app = app.clone();
        async move {
            app.permission
                .poll_interval_ms
                .store(50, std::sync::atomic::Ordering::Relaxed);
            let _ = app.permission.poll_loop(&app.flow_handles()).await;
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
        Turn::live_permissions(&app.cards_handle(), "ses_test")
            .await
            .is_empty()
    );
}

/// Seed the state the permission poller leaves behind (#175): one session
/// whose live card carries an inline permission block. The tests drive the
/// sweep by hand, so no background poll or render tick can repaint underneath
/// them — the sweep alone must put the receipt on the card.
async fn seed_inline_permission_card(app: &Arc<App>, session_id: &str, request_id: &str) {
    let cards = app.cards_handle();
    Turn::seed_card(&cards, session_id, Some("msg_live")).await;
    Turn::set_reply_target(&cards, session_id, "msg_trigger").await;
    Turn::add_permission(
        &cards,
        session_id,
        &perm_request(request_id, session_id, "ls -la"),
        "/work",
    )
    .await;
}

/// Drive one sweep over a session whose block vanished and assert the hosting
/// card was repainted with `receipt` — the render poll is never spawned, so
/// the sweep is the only thing that can have done it.
async fn sweep_and_assert_receipt_on_the_card(
    app: &Arc<App>,
    platform: &Arc<RecordingPlatform>,
    receipt: &str,
) {
    let mut seen = std::collections::HashSet::new();
    app.permission.sweep(&app.flow_handles(), &mut seen).await;

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
        patched.contains(receipt),
        "the receipt must be on the repainted card: {patched}"
    );
    assert!(
        !patched.contains("🔐 **权限请求**"),
        "the live block must be gone from the card: {patched}"
    );
    assert!(
        Turn::live_permissions(&app.cards_handle(), "ses_1")
            .await
            .is_empty(),
        "the accumulator must not keep the block live"
    );
}

/// #175: a request resolved by another client (OpenChamber, CLI) becomes its
/// neutral Interaction Receipt on the live turn's card within ONE sweep — no
/// reliance on the ~1.5 s render tick. The session is still running, which is
/// what makes the neutral line the truth.
#[tokio::test]
async fn sweep_repaints_a_live_turn_card_when_the_request_resolves_elsewhere() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    // The server no longer lists the request AND the run is still live:
    // another client resolved it while the turn continued.
    let mut backend = MockBackend::new(realistic_parts());
    backend.with_session_status("ses_1", Some(crate::backend::types::SessionStatus::Busy));
    let (app, platform) = build_app(cfg, backend).await;
    seed_session(&app, "ses_1", "/work").await;
    seed_inline_permission_card(&app, "ses_1", "per_1").await;

    sweep_and_assert_receipt_on_the_card(
        &app,
        &platform,
        "⏱ 已由其他客户端处理：⚡ 执行 Shell 命令 `ls -la`",
    )
    .await;
}

/// A request that vanished while its session no longer runs (an interrupted
/// execution, a location eviction, a server restart) died with its run:
/// nobody decided it, so the receipt must say so — never the neutral
/// "another client handled it" line. The session is idle, the render poll is
/// gone, and only the sweep can repaint the card.
#[tokio::test]
async fn sweep_stamps_the_interrupted_receipt_when_the_run_is_gone() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    // MockBackend's absent status entry reads Idle: the run is over.
    let (app, platform) = build_app(cfg, MockBackend::new(realistic_parts())).await;
    seed_session(&app, "ses_1", "/work").await;
    seed_inline_permission_card(&app, "ses_1", "per_1").await;
    // The turn ended: the poll loop is gone and the card shows its final
    // state. Nothing except the request sweep can still touch it.
    let cards = app.cards_handle();
    Turn::set_card_state(&cards, "ses_1", crate::feishu::card::CardState::Done).await;
    Turn::clear_phase(&cards, "ses_1").await;

    sweep_and_assert_receipt_on_the_card(&app, &platform, "⏱ 已随会话中断：⚡ 执行 Shell 命令 `ls -la`")
        .await;
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
        .expect("the repaint is recorded");
    assert!(
        !patched.contains("已由其他客户端处理"),
        "an idle run must not claim another client handled the request: {patched}"
    );
}

/// A failed status read is UNKNOWN, never read as an interruption: the
/// receipt stays the neutral one, exactly as before the classifier existed.
#[tokio::test]
async fn sweep_keeps_the_neutral_receipt_when_the_status_read_fails() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let mut backend = MockBackend::new(realistic_parts());
    backend.status_read_fails("simulated session_status failure");
    let (app, platform) = build_app(cfg, backend).await;
    seed_session(&app, "ses_1", "/work").await;
    seed_inline_permission_card(&app, "ses_1", "per_1").await;

    sweep_and_assert_receipt_on_the_card(
        &app,
        &platform,
        "⏱ 已由其他客户端处理：⚡ 执行 Shell 命令 `ls -la`",
    )
    .await;
}

/// One sweep reads a vanished session's status at most once, even when the
/// session carries candidates on several surfaces (an inline block and a
/// standalone card): the classification is per session, not per block.
#[tokio::test]
async fn a_sweep_reads_each_vanished_sessions_status_once() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let backend = Arc::new(MockBackend::new(realistic_parts()));
    let platform = Arc::new(RecordingPlatform::new());
    let app = Arc::new(App::new(cfg, backend.clone(), platform.clone()).unwrap());
    seed_session(&app, "ses_1", "/work").await;
    seed_inline_permission_card(&app, "ses_1", "per_1").await;
    // A second vanished request of the SAME session, on the standalone surface.
    app.permission.sent_cards.lock().await.insert(
        "per_2".into(),
        crate::bridge::request::flow::SentCard {
            message_id: "om_sent".into(),
            summary: "bash ls".into(),
            directory: "/work".into(),
            session_id: "ses_1".into(),
        },
    );

    let mut seen = std::collections::HashSet::new();
    app.permission.sweep(&app.flow_handles(), &mut seen).await;

    assert_eq!(
        backend.session_status_reads.lock().await.as_slice(),
        ["ses_1".to_string()],
        "one read per distinct session per pass"
    );
}

/// The classifier reads a session's status ONLY for requests that actually
/// vanished: a sweep where every request is still pending makes no status
/// read at all.
#[tokio::test]
async fn a_sweep_without_a_vanished_request_reads_no_session_status() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let mut backend = MockBackend::new(realistic_parts());
    backend.ask_permission(perm_request("per_1", "ses_1", "ls -la"));
    let backend = Arc::new(backend);
    let platform = Arc::new(RecordingPlatform::new());
    let app = Arc::new(App::new(cfg, backend.clone(), platform.clone()).unwrap());
    seed_session(&app, "ses_1", "/work").await;

    let mut seen = std::collections::HashSet::new();
    app.permission.sweep(&app.flow_handles(), &mut seen).await;

    assert!(
        backend.session_status_reads.lock().await.is_empty(),
        "a pending request is not a vanished one: {:?}",
        backend.session_status_reads.lock().await
    );
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
    let cards = app.cards_handle();
    Turn::seed_card(&cards, "ses_1", Some("msg_live")).await;
    Turn::set_reply_target(&cards, "ses_1", "msg_1").await;
    Turn::add_permission(
        &cards,
        "ses_1",
        &perm_request("per_1", "ses_1", "ls -la"),
        "/work",
    )
    .await;
    Turn::push_text(&cards, "ses_1", &"很长的回答。".repeat(2000)).await;

    let result = app
        .host_action(serde_json::json!({
            "action": "perm",
            "reply": "once",
            "session_id": "ses_1",
            "request_id": "per_1",
            "perm_label": "✅ 已允许一次",
            "perm_color": "green",
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
        Turn::live_permissions(&app.cards_handle(), "ses_1")
            .await
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
    let cards = app.cards_handle();
    Turn::seed_card(&cards, "ses_test", Some("msg_live")).await;
    Turn::set_reply_target(&cards, "ses_test", "msg_1").await;
    Turn::push_text(&cards, "ses_test", "第一段。").await;
    Turn::add_permission(
        &cards,
        "ses_test",
        &perm_request("per_1", "ses_test", "ls -la"),
        "/work",
    )
    .await;
    Turn::push_reasoning(&cards, "ses_test", "正在思考。").await;
    Turn::add_permission(
        &cards,
        "ses_test",
        &perm_request("per_2", "ses_test", "cargo build"),
        "/work",
    )
    .await;

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
    Turn::push_text(&app.cards_handle(), "ses_test", "第二段。").await;
    crate::bridge::turn::Turn::flush_card(&app.cards_handle(), "ses_test").await;

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
    let cards = app.cards_handle();
    Turn::seed_card(&cards, "ses_test", Some("msg_live")).await;
    Turn::set_reply_target(&cards, "ses_test", "msg_1").await;
    Turn::add_permission(
        &cards,
        "ses_test",
        &perm_request("per_1", "ses_test", "ls -la"),
        "/work",
    )
    .await;

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
    Turn::push_reasoning_at(
        &app.cards_handle(),
        "ses_test",
        clicked_at - 300,
        "先看一下目录里有什么。",
    )
    .await;
    Turn::push_tool_at(
        &app.cards_handle(),
        "ses_test",
        clicked_at - 200,
        "call_1",
        crate::feishu::card::tool_render::ToolPanel::for_test(
            "bash",
            ToolStatus::Running,
            Some(serde_json::json!({ "command": "ls -la" })),
            None,
        ),
    )
    .await;
    crate::bridge::turn::Turn::flush_card(&app.cards_handle(), "ses_test").await;

    // While the command runs, its panel is live tail content (ADR-0045): it
    // renders on the live card, below the timeline — the receipt included.
    let live = final_card(&platform).await;
    assert!(
        card_text(&live).contains("ls -la"),
        "the running panel must ride the live card: {live}"
    );

    // Settling moves it into the timeline at its OWN start time, so it lands
    // above the receipt that resolved its permission despite rendering late.
    Turn::push_tool_at(
        &app.cards_handle(),
        "ses_test",
        clicked_at - 200,
        "call_1",
        crate::feishu::card::tool_render::ToolPanel::for_test(
            "bash",
            ToolStatus::Completed,
            Some(serde_json::json!({ "command": "ls -la" })),
            Some("src"),
        ),
    )
    .await;
    crate::bridge::turn::Turn::flush_card(&app.cards_handle(), "ses_test").await;

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

/// The sweep lists pending requests once per known DIRECTORY, never once per
/// session: V2's location-scoped list covers every session in a location, so a
/// store with several sessions sharing one directory still makes exactly one
/// pending-list call per kind per sweep (the no-iteration-storm contract).
#[tokio::test]
async fn sweep_lists_once_per_directory_not_per_session() {
    use std::sync::atomic::Ordering;

    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let backend = MockBackend::new(realistic_parts());
    let permission_lists = backend.list_permission_calls.clone();
    let question_lists = backend.list_question_calls.clone();
    let (app, _platform) = build_app(cfg, backend).await;

    // Three sessions mapped into the same directory: `directories()` dedupes
    // them to one location.
    for (thread, session) in [("chat_1", "ses_a"), ("chat_2", "ses_b"), ("chat_3", "ses_c")] {
        seed_entry(
            &app,
            crate::config::SessionEntry::new(
                crate::config::ThreadKey::new(thread.into(), thread.into()),
                session,
                "/work/shared",
            ),
        )
        .await;
    }

    let mut seen = std::collections::HashSet::new();
    app.permission.sweep(&app.flow_handles(), &mut seen).await;
    assert_eq!(
        permission_lists.load(Ordering::SeqCst),
        1,
        "one permission list per directory, not one per session"
    );
    app.question.sweep(&app.flow_handles(), &mut seen).await;
    assert_eq!(
        question_lists.load(Ordering::SeqCst),
        1,
        "one form list per directory, not one per session"
    );
}

/// ADR-0102 (parent spec #1010): an over-budget permission is no longer
/// refused. The sweep delivers the full target as a Rich Text Message
/// companion FIRST, then surfaces the request with a summary; the click guard
/// trusts the delivered record. The companion lands before the card, so there
/// is no window where a button approves text the user never saw.
#[tokio::test]
async fn an_over_budget_permission_surfaces_with_a_post_companion_and_approves() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let session_file = dir.path().join("sessions.json");
    let cfg = test_config(&session_file);
    let command = format!("echo {}", "x".repeat(5_000));
    let mut backend = MockBackend::new(realistic_parts());
    backend.ask_permissions(vec![crate::backend::types::PermissionRequest {
        request_id: "per_long".into(),
        session_id: Some("ses_test".into()),
        permission: Some("bash".into()),
        patterns: vec![command.clone()],
        metadata: None,
        always: Vec::new(),
        always_supported: true,
    }]);
    let replies = std::sync::Arc::clone(&backend.reply_permission_calls);
    let (app, platform) = build_app(cfg, backend).await;

    // A live card is the reply target the companion rides under.
    seed_live_card(&app, "ses_test", "om_live").await;

    let mut seen = std::collections::HashSet::new();
    app.permission.sweep(&app.flow_handles(), &mut seen).await;

    // One companion, delivered as a `post` with an `md` tag carrying the full
    // target — not one per surface.
    let posts = posted_companions(&platform).await;
    assert_eq!(posts.len(), 1, "the companion is sent once per request");
    let md = posts[0]["zh_cn"]["content"][0][0]["text"]
        .as_str()
        .expect("the companion carries a markdown element");
    assert!(
        md.contains(&command),
        "the companion carries the full target: {md}"
    );
    assert!(
        md.contains("执行 Shell 命令"),
        "the companion names the action: {md}"
    );

    // The card is a summary: action, total count, the first 120 characters and
    // where the full target went — never the full command.
    assert_eq!(
        Turn::live_permissions(&app.cards_handle(), "ses_test")
            .await
            .len(),
        1,
        "the request is offered for approval"
    );
    let card = Turn::rendered_card(&app.cards_handle(), "ses_test")
        .await
        .expect("the streaming card renders");
    let text = card_text(&card);
    assert!(text.contains("执行 Shell 命令"), "action missing: {text}");
    assert!(
        text.contains(&format!("{} 字符", command.chars().count())),
        "the summary names the total length: {text}"
    );
    let excerpt: String = command.chars().take(120).collect();
    assert!(
        text.contains(&excerpt),
        "the summary carries the first 120 characters: {text}"
    );
    assert!(
        text.contains("富文本消息（Rich Text Message）"),
        "the summary names the Rich Text Message lane: {text}"
    );
    assert!(
        !text.contains(&command),
        "the card must not carry the full target: {text}"
    );

    // The delivery record is persisted beside the surface records.
    let raw = std::fs::read_to_string(crate::state::StateLayout::beside(
        &session_file,
        "interactive_surfaces.json",
    ))
    .expect("the delivery is persisted");
    assert!(
        raw.contains("companion") && raw.contains("per_long") && raw.contains("RichText"),
        "the companion record is persisted: {raw}"
    );

    // Clicking 允许一次 approves exactly the request the companion carried; the
    // Interaction Receipt still names the target.
    let ack = click_perm(&app, "once", "per_long", "✅ 已允许一次").await;
    assert!(
        ack.contains("✅ 已允许一次：⚡ 执行 Shell 命令"),
        "the receipt names the decision: {ack}"
    );
    assert!(ack.contains("echo"), "and the target it approved: {ack}");
    assert_eq!(
        replies.lock().await.as_slice(),
        &[("per_long".to_string(), "once".to_string())],
        "the click replies once, not a forced rejection"
    );

    // Resolution removes the companion record (the cached card JSON may remain
    // until its own lifecycle releases it, but the delivery proof is gone).
    let record: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(crate::state::StateLayout::beside(
            &session_file,
            "interactive_surfaces.json",
        ))
        .unwrap_or_default(),
    )
    .unwrap_or(serde_json::Value::Null);
    assert!(
        record["companion"]["default"].get("per_long").is_none(),
        "a resolved request leaves no companion record: {record}"
    );
}

/// A companion record whose digest does not match the pending request it is
/// meant to prove — a target that changed after the delivery — is no proof. The
/// click guard refuses the approval and the result names the missing delivery.
#[tokio::test]
async fn a_stale_companion_digest_is_refused_at_the_click() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let command = format!("echo {}", "x".repeat(5_000));
    let mut backend = MockBackend::new(realistic_parts());
    backend.ask_permissions(vec![crate::backend::types::PermissionRequest {
        request_id: "per_long".into(),
        session_id: Some("ses_test".into()),
        permission: Some("bash".into()),
        patterns: vec![command],
        metadata: None,
        always: Vec::new(),
        always_supported: true,
    }]);
    let replies = std::sync::Arc::clone(&backend.reply_permission_calls);
    let (app, _platform) = build_app(cfg, backend).await;
    seed_live_card(&app, "ses_test", "om_live").await;
    let mut seen = std::collections::HashSet::new();
    app.permission.sweep(&app.flow_handles(), &mut seen).await;

    // The turn has ended: the click has no live accumulator to ack, so the
    // result card itself is returned.
    Turn::set_card_state(
        &app.cards_handle(),
        "ses_test",
        crate::feishu::card::CardState::Idle,
    )
    .await;

    // Simulate a record left by a delivery of a DIFFERENT target.
    app.permission.surfaces.set_companion(
        "per_long",
        crate::bridge::surfaces::CompanionDelivery {
            message_id: "om_post".into(),
            lane: crate::bridge::surfaces::CompanionLane::RichText,
            target_digest: 0,
        },
    );

    let result = app
        .host_action(serde_json::json!({
            "action": "perm",
            "reply": "once",
            "session_id": "ses_test",
            "directory": "/work",
            "request_id": "per_long",
            "perm_label": "✅ 已允许一次",
            "perm_color": "green",
        }))
        .await
        .expect("a card-action result");
    assert_eq!(
        replies.lock().await.as_slice(),
        &[("per_long".to_string(), "reject".to_string())],
        "a stale digest never approves"
    );
    assert_eq!(
        result.toast.as_deref(),
        Some("已拒绝：完整内容未送达"),
        "the toast names the missing delivery"
    );
    let text = card_text(result.card.as_ref().expect("the result card"));
    assert!(
        text.contains("没有找到已送达的伴随消息"),
        "the result card says why: {text}"
    );
}

/// A stale companion record left for a request whose content changed is
/// replaced at surfacing time, not trusted into a summary card whose Allow
/// button could only decline. This is the sweep-meets-stale-record path; the
/// click-time test above seeds the record AFTER the sweep.
#[tokio::test]
async fn a_sweep_replaces_a_stale_companion_record_before_surfacing() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let command = format!("echo {}", "x".repeat(5_000));
    let request = crate::backend::types::PermissionRequest {
        request_id: "per_long".into(),
        session_id: Some("ses_test".into()),
        permission: Some("bash".into()),
        patterns: vec![command.clone()],
        metadata: None,
        always: Vec::new(),
        always_supported: true,
    };
    let mut backend = MockBackend::new(realistic_parts());
    backend.ask_permissions(vec![request.clone()]);
    let replies = std::sync::Arc::clone(&backend.reply_permission_calls);
    let (app, platform) = build_app(cfg, backend).await;
    seed_live_card(&app, "ses_test", "om_live").await;

    // A record for a DIFFERENT content is already persisted.
    app.permission.surfaces.set_companion(
        "per_long",
        crate::bridge::surfaces::CompanionDelivery {
            message_id: "om_stale".into(),
            lane: crate::bridge::surfaces::CompanionLane::File,
            target_digest: 0,
        },
    );

    let mut seen = std::collections::HashSet::new();
    app.permission.sweep(&app.flow_handles(), &mut seen).await;

    let record = app
        .permission
        .surfaces
        .companion("per_long")
        .expect("the stale record is replaced");
    assert_ne!(record.message_id, "om_stale", "the stale record is not kept");
    assert_eq!(
        record.target_digest,
        crate::bridge::request::kind::companion_target_digest(&request),
        "the replacement proves the current content"
    );
    assert_eq!(
        posted_companions(&platform).await.len(),
        1,
        "exactly one fresh companion is sent"
    );

    // The card points at the fresh lane, and the click approves.
    let card = Turn::rendered_card(&app.cards_handle(), "ses_test")
        .await
        .expect("the streaming card renders");
    assert!(
        card_text(&card).contains("富文本消息（Rich Text Message）"),
        "the summary names the fresh delivery: {}",
        card_text(&card)
    );
    click_perm(&app, "once", "per_long", "✅ 已允许一次").await;
    assert_eq!(
        replies.lock().await.as_slice(),
        &[("per_long".to_string(), "once".to_string())],
        "the replacement permits the approval"
    );
}

/// A complete permission sweep forgets a companion whose request is no longer
/// pending. If the sweep-level prune is deleted, the record outlives its
/// request and a later click could borrow it.
#[tokio::test]
async fn a_complete_sweep_drops_a_companion_record_for_a_resolved_request() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let (app, _platform) = build_app(cfg, MockBackend::new(Vec::new())).await;
    seed_session(&app, "ses_test", "/work").await;
    app.permission.surfaces.set_companion(
        "per_gone",
        crate::bridge::surfaces::CompanionDelivery {
            message_id: "om_gone".into(),
            lane: crate::bridge::surfaces::CompanionLane::RichText,
            target_digest: 7,
        },
    );

    let mut seen = std::collections::HashSet::new();
    app.permission.sweep(&app.flow_handles(), &mut seen).await;

    assert!(
        app.permission.surfaces.companion("per_gone").is_none(),
        "a request absent from a complete sweep has resolved"
    );
}

/// A target with no reply message (only a chat target) cannot take either
/// companion lane, so the request follows the refusal path rather than offering
/// an approval for content that was not delivered.
#[tokio::test]
async fn an_over_budget_permission_without_a_reply_target_follows_the_refusal_path() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let command = format!("echo {}", "x".repeat(5_000));
    let mut backend = MockBackend::new(realistic_parts());
    backend.ask_permissions(vec![crate::backend::types::PermissionRequest {
        request_id: "per_long".into(),
        session_id: Some("ses_test".into()),
        permission: Some("bash".into()),
        patterns: vec![command],
        metadata: None,
        always: Vec::new(),
        always_supported: true,
    }]);
    let replies = std::sync::Arc::clone(&backend.reply_permission_calls);
    let (app, platform) = build_app(cfg, backend).await;
    // No live card and no reply target: `resolve_card_target` yields a chat
    // target, which neither companion lane can reply under.
    seed_session(&app, "ses_test", "/work").await;

    let mut seen = std::collections::HashSet::new();
    app.permission.sweep(&app.flow_handles(), &mut seen).await;

    assert_eq!(
        replies.lock().await.as_slice(),
        &[("per_long".to_string(), "reject".to_string())],
        "an undeliverable companion declines"
    );
    let calls = platform.calls.lock().await.clone();
    assert!(
        !calls.iter().any(|call| matches!(
            call,
            PlatformCall::SendMessageReply { .. } | PlatformCall::UploadFile { .. }
        )),
        "neither companion lane was attempted"
    );
    let mut cards = platform.sent_cards().await;
    cards.extend(platform.replied_cards().await);
    assert!(
        cards.iter().any(|card| card_text(card)
            .contains(crate::feishu::card::question::UNSHOWABLE_PERMISSION_HEADER)),
        "the chat gets the reason card"
    );
}

/// A Rich Text send that DEFINITELY did not land falls to the File Message
/// lane, as ADR-0083 §5's ladder dictates.
#[tokio::test]
async fn a_definitely_failed_post_falls_to_a_file_companion() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let command = format!("echo {}", "x".repeat(5_000));
    let mut backend = MockBackend::new(realistic_parts());
    backend.ask_permissions(vec![crate::backend::types::PermissionRequest {
        request_id: "per_long".into(),
        session_id: Some("ses_test".into()),
        permission: Some("bash".into()),
        patterns: vec![command.clone()],
        metadata: None,
        always: Vec::new(),
        always_supported: true,
    }]);
    let replies = std::sync::Arc::clone(&backend.reply_permission_calls);
    let (app, platform) = build_app(cfg, backend).await;
    platform
        .fail_send_message_definite_count
        .store(1, std::sync::atomic::Ordering::SeqCst);
    seed_live_card(&app, "ses_test", "om_live").await;

    let mut seen = std::collections::HashSet::new();
    app.permission.sweep(&app.flow_handles(), &mut seen).await;

    assert!(replies.lock().await.is_empty(), "never declined");
    assert!(
        posted_companions(&platform).await.is_empty(),
        "the definite post failure did not land a Rich Text message"
    );
    let calls = platform.calls.lock().await.clone();
    let upload = calls
        .iter()
        .find_map(|call| match call {
            PlatformCall::UploadFile { name, content, .. } => Some((name.clone(), content.clone())),
            _ => None,
        })
        .expect("the File Message lane uploads the full content");
    assert!(upload.0.contains("per_long"));
    assert_eq!(upload.1, command.as_bytes());
    assert!(
        calls.iter().any(|call| matches!(
            call,
            PlatformCall::SendMessageReply { msg_type, .. } if msg_type == "file"
        )),
        "the File Message is sent"
    );
    let card = Turn::rendered_card(&app.cards_handle(), "ses_test")
        .await
        .expect("the streaming card renders");
    assert!(
        card_text(&card).contains("文件消息（File Message）"),
        "the summary names the lane that landed: {}",
        card_text(&card)
    );
}

/// An ambiguous companion send is NOT a delivery: it leaves only a no-duplicate
/// marker, the request follows the uncertain refusal path, and no interactive
/// approval is offered. A later sweep must not re-send the companion.
#[tokio::test]
async fn an_ambiguous_companion_send_is_not_retried_and_follows_the_uncertain_refusal_path() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let command = format!("echo {}", "x".repeat(5_000));
    let mut backend = MockBackend::new(realistic_parts());
    backend.ask_permissions(vec![crate::backend::types::PermissionRequest {
        request_id: "per_long".into(),
        session_id: Some("ses_test".into()),
        permission: Some("bash".into()),
        patterns: vec![command],
        metadata: None,
        always: Vec::new(),
        always_supported: true,
    }]);
    // The first decline fails, so the pending request survives to prove the
    // marker blocks a re-send on the next sweep.
    backend
        .fail_reply_permission_count
        .store(1, std::sync::atomic::Ordering::SeqCst);
    let replies = std::sync::Arc::clone(&backend.reply_permission_calls);
    let (app, platform) = build_app(cfg, backend).await;
    platform
        .ambiguous_send_message_count
        .store(1, std::sync::atomic::Ordering::SeqCst);
    seed_live_card(&app, "ses_test", "om_live").await;

    let mut seen = std::collections::HashSet::new();
    app.permission.sweep(&app.flow_handles(), &mut seen).await;

    assert_eq!(
        replies.lock().await.as_slice(),
        &[("per_long".to_string(), "reject".to_string())],
        "the ambiguous companion declines instead of offering approval"
    );
    let record = app
        .permission
        .surfaces
        .companion("per_long")
        .expect("the no-duplicate marker is recorded");
    assert!(
        record.message_id.is_empty(),
        "the marker is explicitly not a confirmed delivery"
    );
    assert!(
        Turn::live_permissions(&app.cards_handle(), "ses_test")
            .await
            .is_empty(),
        "no interactive approval is offered"
    );
    assert!(
        !platform.calls.lock().await.iter().any(|call| matches!(
            call,
            PlatformCall::SendMessageReply { .. } | PlatformCall::UploadFile { .. }
        )),
        "the ambiguous send does not fall to the File lane"
    );

    // The next sweep meets the marker: no re-send, and the decline lands with
    // the uncertain copy.
    let mut seen = std::collections::HashSet::new();
    app.permission.sweep(&app.flow_handles(), &mut seen).await;

    assert_eq!(
        replies.lock().await.as_slice(),
        &[
            ("per_long".to_string(), "reject".to_string()),
            ("per_long".to_string(), "reject".to_string()),
        ],
        "the retry declines, it does not re-send or approve"
    );
    assert!(
        !platform.calls.lock().await.iter().any(|call| matches!(
            call,
            PlatformCall::SendMessageReply { .. } | PlatformCall::UploadFile { .. }
        )),
        "the marker prevents a duplicate companion"
    );
    assert!(
        Turn::live_permissions(&app.cards_handle(), "ses_test")
            .await
            .is_empty(),
        "still no interactive approval"
    );
    let mut cards = platform.sent_cards().await;
    cards.extend(platform.replied_cards().await);
    assert!(
        cards.iter().any(|card| {
            card_text(card).contains(crate::feishu::card::question::UNCERTAIN_PERMISSION_HEADER)
                && card_text(card).contains("送达结果未知")
        }),
        "the refusal copy names the unknown outcome: {cards:?}"
    );
    assert!(
        app.permission.surfaces.companion("per_long").is_none(),
        "the settled request forgets the marker"
    );
}

/// An Allow click is refused when the only record is an uncertain marker: the
/// click guard treats it like a missing delivery (no completeness proof).
#[tokio::test]
async fn an_allow_click_on_an_uncertain_companion_is_refused() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let command = format!("echo {}", "x".repeat(5_000));
    let mut backend = MockBackend::new(realistic_parts());
    backend.ask_permissions(vec![crate::backend::types::PermissionRequest {
        request_id: "per_long".into(),
        session_id: Some("ses_test".into()),
        permission: Some("bash".into()),
        patterns: vec![command],
        metadata: None,
        always: Vec::new(),
        always_supported: true,
    }]);
    backend
        .fail_reply_permission_count
        .store(1, std::sync::atomic::Ordering::SeqCst);
    let replies = std::sync::Arc::clone(&backend.reply_permission_calls);
    let (app, platform) = build_app(cfg, backend).await;
    platform
        .ambiguous_send_message_count
        .store(1, std::sync::atomic::Ordering::SeqCst);
    seed_live_card(&app, "ses_test", "om_live").await;

    let mut seen = std::collections::HashSet::new();
    app.permission.sweep(&app.flow_handles(), &mut seen).await;

    // End the turn so the click returns a result card rather than an ack.
    Turn::set_card_state(
        &app.cards_handle(),
        "ses_test",
        crate::feishu::card::CardState::Idle,
    )
    .await;
    let result = app
        .host_action(serde_json::json!({
            "action": "perm",
            "reply": "once",
            "session_id": "ses_test",
            "directory": "/work",
            "request_id": "per_long",
            "perm_label": "✅ 已允许一次",
            "perm_color": "green",
        }))
        .await
        .expect("a card-action result");

    assert_eq!(
        replies.lock().await.as_slice(),
        &[
            ("per_long".to_string(), "reject".to_string()),
            ("per_long".to_string(), "reject".to_string()),
        ],
        "an uncertain marker never approves"
    );
    assert_eq!(
        result.toast.as_deref(),
        Some("已拒绝：完整内容送达结果未知"),
        "the toast names the unknown outcome"
    );
    let text = card_text(result.card.as_ref().expect("the result card"));
    assert!(
        text.contains(crate::feishu::card::question::UNCERTAIN_PERMISSION_CLICK_LABEL),
        "the result label names the unknown outcome: {text}"
    );
    assert!(
        text.contains("送达结果未知"),
        "and the result body says why approval is unavailable: {text}"
    );
    assert!(
        Turn::live_permissions(&app.cards_handle(), "ses_test")
            .await
            .is_empty(),
        "no interactive approval is offered"
    );
}

/// A definite Rich Text failure falls to the File lane; if the FILE send is
/// then ambiguous, the companion outcome is unknown. No caption is sent (it
/// would point at a file that may not exist), no retry happens, and the request
/// follows the uncertain refusal path.
#[tokio::test]
async fn an_ambiguous_file_after_a_definite_post_failure_is_uncertain() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let command = format!("echo {}", "x".repeat(5_000));
    let mut backend = MockBackend::new(realistic_parts());
    backend.ask_permissions(vec![crate::backend::types::PermissionRequest {
        request_id: "per_long".into(),
        session_id: Some("ses_test".into()),
        permission: Some("bash".into()),
        patterns: vec![command.clone()],
        metadata: None,
        always: Vec::new(),
        always_supported: true,
    }]);
    let replies = std::sync::Arc::clone(&backend.reply_permission_calls);
    // Keep the decline from landing, so the uncertain marker survives to be
    // asserted after the sweep.
    backend
        .fail_reply_permission_count
        .store(1, std::sync::atomic::Ordering::SeqCst);
    let (app, platform) = build_app(cfg, backend).await;
    platform
        .fail_send_message_definite_count
        .store(1, std::sync::atomic::Ordering::SeqCst);
    platform
        .ambiguous_send_message_count
        .store(1, std::sync::atomic::Ordering::SeqCst);
    seed_live_card(&app, "ses_test", "om_live").await;

    let mut seen = std::collections::HashSet::new();
    app.permission.sweep(&app.flow_handles(), &mut seen).await;

    let calls = platform.calls.lock().await.clone();
    assert!(
        calls
            .iter()
            .any(|call| matches!(call, PlatformCall::UploadFile { .. })),
        "the definite post failure falls to the File lane"
    );
    assert!(
        !calls.iter().any(|call| matches!(
            call,
            PlatformCall::SendMessageReply { msg_type, .. } if msg_type == "file"
        )),
        "the ambiguous file send did not land"
    );
    assert!(
        !calls
            .iter()
            .any(|call| matches!(call, PlatformCall::ReplyText { .. })),
        "no caption points at a file that may not exist"
    );
    let record = app
        .permission
        .surfaces
        .companion("per_long")
        .expect("the uncertain marker is recorded");
    assert!(record.message_id.is_empty());
    assert_eq!(
        replies.lock().await.as_slice(),
        &[("per_long".to_string(), "reject".to_string())],
        "the uncertain file declines"
    );
    assert!(
        Turn::live_permissions(&app.cards_handle(), "ses_test")
            .await
            .is_empty(),
        "no interactive approval is offered"
    );
}

/// A "successful" send that returns no message id is treated exactly like an
/// ambiguous send: an uncertain marker, not a delivery. The card/guard halves
/// must agree that `is_uncertain` means no proof.
#[tokio::test]
async fn an_empty_companion_message_id_is_not_a_delivery() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let command = format!("echo {}", "x".repeat(5_000));
    let mut backend = MockBackend::new(realistic_parts());
    backend.ask_permissions(vec![crate::backend::types::PermissionRequest {
        request_id: "per_long".into(),
        session_id: Some("ses_test".into()),
        permission: Some("bash".into()),
        patterns: vec![command],
        metadata: None,
        always: Vec::new(),
        always_supported: true,
    }]);
    backend
        .fail_reply_permission_count
        .store(1, std::sync::atomic::Ordering::SeqCst);
    let (app, platform) = build_app(cfg, backend).await;
    platform.given_message_reply_id("");
    seed_live_card(&app, "ses_test", "om_live").await;

    let mut seen = std::collections::HashSet::new();
    app.permission.sweep(&app.flow_handles(), &mut seen).await;

    let record = app
        .permission
        .surfaces
        .companion("per_long")
        .expect("the send is recorded");
    assert!(
        record.message_id.is_empty() && record.is_uncertain(),
        "an empty id is an uncertain marker, not a confirmed delivery"
    );
    assert!(
        Turn::live_permissions(&app.cards_handle(), "ses_test")
            .await
            .is_empty(),
        "no interactive approval is offered"
    );
}

/// The result card an over-budget approval returns is the same summary form:
/// it names the action, the total length and the companion lane, and never
/// renders the full target on the card.
#[tokio::test]
async fn an_over_budget_allow_result_card_names_the_target() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let command = format!("echo {}", "x".repeat(5_000));
    let mut backend = MockBackend::new(realistic_parts());
    backend.ask_permissions(vec![crate::backend::types::PermissionRequest {
        request_id: "per_long".into(),
        session_id: Some("ses_test".into()),
        permission: Some("bash".into()),
        patterns: vec![command.clone()],
        metadata: None,
        always: Vec::new(),
        always_supported: true,
    }]);
    let (app, _platform) = build_app(cfg, backend).await;
    seed_live_card(&app, "ses_test", "om_live").await;
    let mut seen = std::collections::HashSet::new();
    app.permission.sweep(&app.flow_handles(), &mut seen).await;

    // The turn has ended: the click has no live accumulator to ack, so the
    // result card itself is returned.
    Turn::set_card_state(
        &app.cards_handle(),
        "ses_test",
        crate::feishu::card::CardState::Idle,
    )
    .await;
    let result = app
        .host_action(serde_json::json!({
            "action": "perm",
            "reply": "once",
            "session_id": "ses_test",
            "directory": "/work",
            "request_id": "per_long",
            "perm_label": "✅ 已允许一次",
            "perm_color": "green",
        }))
        .await
        .expect("a card-action result");
    let text = card_text(result.card.as_ref().expect("the result card"));
    assert!(text.contains("✅ 已允许一次"), "decision missing: {text}");
    assert!(
        text.contains("执行 Shell 命令"),
        "the result names the action: {text}"
    );
    assert!(
        text.contains("5005 字符"),
        "and the target's total length: {text}"
    );
    assert!(
        text.contains("富文本消息（Rich Text Message）"),
        "and where the full target went: {text}"
    );
    assert!(
        !text.contains(&command),
        "the result never renders the full target: {text}"
    );
}

/// ADR-0083 §5's ladder: a target whose Rich Text `post` body would exceed the
/// 30 KB cap does NOT take that lane — the File Message carries the full bytes
/// under a name that names the request, and the card summary says File Message
/// while still naming the target excerpt.
#[tokio::test]
async fn an_over_budget_post_body_falls_to_a_file_companion() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let command = "x".repeat(31_000);
    let mut backend = MockBackend::new(realistic_parts());
    backend.ask_permissions(vec![crate::backend::types::PermissionRequest {
        request_id: "per_huge".into(),
        session_id: Some("ses_test".into()),
        permission: Some("bash".into()),
        patterns: vec![command.clone()],
        metadata: None,
        always: Vec::new(),
        always_supported: true,
    }]);
    let replies = std::sync::Arc::clone(&backend.reply_permission_calls);
    let (app, platform) = build_app(cfg, backend).await;
    seed_live_card(&app, "ses_test", "om_live").await;

    let mut seen = std::collections::HashSet::new();
    app.permission.sweep(&app.flow_handles(), &mut seen).await;

    assert!(replies.lock().await.is_empty(), "never declined");
    let calls = platform.calls.lock().await.clone();
    assert!(
        !calls.iter().any(|call| matches!(
            call,
            PlatformCall::SendMessageReply { msg_type, .. } if msg_type == "post"
        )),
        "a body past the post cap takes no Rich Text lane"
    );
    let upload = calls
        .iter()
        .find_map(|call| match call {
            PlatformCall::UploadFile { name, content, .. } => Some((name.clone(), content.clone())),
            _ => None,
        })
        .expect("the File Message lane uploads the full target");
    assert!(
        upload.0.contains("per_huge") && upload.0.ends_with(".txt"),
        "the file name names the request: {}",
        upload.0
    );
    assert!(
        calls.iter().any(|call| matches!(
            call,
            PlatformCall::ReplyText { text, .. } if text.contains("per_huge")
        )),
        "a caption names the request before the file: {calls:?}"
    );
    assert_eq!(
        upload.1,
        command.as_bytes(),
        "the uploaded bytes are the full target"
    );
    assert!(
        calls.iter().any(|call| matches!(
            call,
            PlatformCall::SendMessageReply { msg_type, content, .. }
                if msg_type == "file" && content["file_key"] == "file_test"
        )),
        "the File Message carries the uploaded key: {calls:?}"
    );

    let card = Turn::rendered_card(&app.cards_handle(), "ses_test")
        .await
        .expect("the streaming card renders");
    let text = card_text(&card);
    assert!(
        text.contains("文件消息（File Message）"),
        "the summary names the File Message lane: {text}"
    );
    let excerpt: String = command.chars().take(120).collect();
    assert!(
        text.contains(&excerpt),
        "the summary still names the target excerpt: {text}"
    );
    assert!(
        !text.contains(&command),
        "the card never carries the full target: {text}"
    );
}

/// The companion proof covers the **summed** target budget, not the longest
/// single target: two patterns that each fit still exceed the one display
/// element, so the request takes the companion lane and its card carries the
/// summed count.
#[tokio::test]
async fn an_over_summed_permission_surfaces_with_a_companion() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let pattern = "x".repeat(1_600);
    let mut backend = MockBackend::new(realistic_parts());
    backend.ask_permissions(vec![crate::backend::types::PermissionRequest {
        request_id: "per_long".into(),
        session_id: Some("ses_test".into()),
        permission: Some("bash".into()),
        patterns: vec![pattern.clone(), pattern.clone()],
        metadata: None,
        always: Vec::new(),
        always_supported: true,
    }]);
    let replies = std::sync::Arc::clone(&backend.reply_permission_calls);
    let (app, platform) = build_app(cfg, backend).await;
    seed_live_card(&app, "ses_test", "om_live").await;

    let mut seen = std::collections::HashSet::new();
    app.permission.sweep(&app.flow_handles(), &mut seen).await;

    assert!(
        replies.lock().await.is_empty(),
        "the summed target is not declined"
    );
    let posts = posted_companions(&platform).await;
    assert_eq!(posts.len(), 1, "one companion carries both targets");
    let md = posts[0]["zh_cn"]["content"][0][0]["text"]
        .as_str()
        .expect("the companion carries a markdown element");
    assert_eq!(
        md.matches(&pattern).count(),
        2,
        "the companion carries both full targets"
    );

    let card = Turn::rendered_card(&app.cards_handle(), "ses_test")
        .await
        .expect("the streaming card renders");
    let text = card_text(&card);
    assert!(
        text.contains("3200 字符"),
        "the summary names the summed length: {text}"
    );
    assert!(
        text.contains("富文本消息（Rich Text Message）"),
        "and where the full targets went: {text}"
    );
}

/// Carry-over from #1011's review (binding for #1012): the companion proof is
/// the ONE `PermissionOverflow`, not the target sum alone. A request whose
/// targets fit the 3,000 budget but whose rendered body crosses the measured
/// single-element ceiling also takes the companion lane and is surfaced with a
/// summary. A `Body` overflow must never be the one overflow still refused.
#[tokio::test]
async fn a_body_overflow_also_takes_the_companion_lane() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    // 2,900 target characters fit the target budget; the 200-char metadata
    // line pushes the rendered body past 3,060.
    let command = "x".repeat(2_900);
    let mut backend = MockBackend::new(realistic_parts());
    backend.ask_permissions(vec![crate::backend::types::PermissionRequest {
        request_id: "per_body".into(),
        session_id: Some("ses_test".into()),
        permission: Some("bash".into()),
        patterns: vec![command.clone()],
        metadata: Some(serde_json::json!({ "command": "y".repeat(200) })),
        always: Vec::new(),
        always_supported: true,
    }]);
    let replies = std::sync::Arc::clone(&backend.reply_permission_calls);
    let (app, platform) = build_app(cfg, backend).await;
    seed_live_card(&app, "ses_test", "om_live").await;

    let mut seen = std::collections::HashSet::new();
    app.permission.sweep(&app.flow_handles(), &mut seen).await;

    assert!(
        replies.lock().await.is_empty(),
        "the rendered-body overflow is not declined"
    );
    let posts = posted_companions(&platform).await;
    assert_eq!(posts.len(), 1, "the body overflow gets a companion");
    let md = posts[0]["zh_cn"]["content"][0][0]["text"]
        .as_str()
        .expect("the companion carries a markdown element");
    assert!(
        md.contains(&command),
        "the companion carries the full target: {md}"
    );
    assert!(
        md.contains(&"y".repeat(200)),
        "and the metadata the rendered-body proof charged: {md}"
    );
    assert_eq!(
        md.matches("执行 Shell 命令").count(),
        1,
        "the action is not duplicated by a second header: {md}"
    );
    assert!(
        !md.starts_with("```"),
        "a rendered-body companion is markdown, not a literal code fence: {md}"
    );

    let card = Turn::rendered_card(&app.cards_handle(), "ses_test")
        .await
        .expect("the streaming card renders");
    let text = card_text(&card);
    assert!(
        text.contains("富文本消息（Rich Text Message）"),
        "the summary names the companion lane: {text}"
    );
    assert!(text.contains("目标长度"), "and the summary form: {text}");
}

/// If both companion lanes fail, no interactive approval is offered: the
/// request follows the existing refusal path. A decline that also fails must
/// stay pending and unmarked, so the next sweep retries — a refusal that never
/// landed would otherwise leave the server waiting with no actionable card.
#[tokio::test]
async fn a_failed_companion_and_decline_keep_the_request_retryable() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let command = format!(
        "echo {}",
        "x".repeat(crate::backend::types::PERMISSION_TARGET_MAX_CHARS)
    );
    let mut backend = MockBackend::new(realistic_parts());
    backend.ask_permissions(vec![crate::backend::types::PermissionRequest {
        request_id: "per_long".into(),
        session_id: Some("ses_test".into()),
        permission: Some("bash".into()),
        patterns: vec![command.clone()],
        metadata: None,
        always: Vec::new(),
        always_supported: true,
    }]);
    // The decline fails once; the platform refuses every companion send across
    // both sweeps (post + file on each).
    backend
        .fail_reply_permission_count
        .store(1, std::sync::atomic::Ordering::SeqCst);
    let replies = std::sync::Arc::clone(&backend.reply_permission_calls);
    let (app, platform) = build_app(cfg, backend).await;
    platform
        .fail_send_message_definite_count
        .store(4, std::sync::atomic::Ordering::SeqCst);
    seed_live_card(&app, "ses_test", "om_live").await;

    // The same `seen` set across both sweeps: a failed refusal that marked the
    // request seen would never be retried.
    let mut seen = std::collections::HashSet::new();
    app.permission.sweep(&app.flow_handles(), &mut seen).await;

    assert_eq!(
        replies.lock().await.as_slice(),
        &[("per_long".to_string(), "reject".to_string())],
        "the first sweep tries the decline after both lanes fail"
    );
    let mut cards = platform.sent_cards().await;
    cards.extend(platform.replied_cards().await);
    assert!(
        cards.is_empty(),
        "a failed decline posts no card: it did not happen"
    );
    assert!(
        Turn::live_permissions(&app.cards_handle(), "ses_test")
            .await
            .is_empty(),
        "and no interactive approval is ever offered"
    );

    // The retry succeeds: the declination lands and the reason card is posted.
    app.permission.sweep(&app.flow_handles(), &mut seen).await;
    assert_eq!(
        replies.lock().await.len(),
        2,
        "the second sweep retries the decline"
    );
    let mut cards = platform.sent_cards().await;
    cards.extend(platform.replied_cards().await);
    assert!(
        cards.iter().any(|card| card_text(card)
            .contains(crate::feishu::card::question::UNSHOWABLE_PERMISSION_HEADER)),
        "the successful retry explains the refusal"
    );
    assert!(
        Turn::live_permissions(&app.cards_handle(), "ses_test")
            .await
            .is_empty(),
        "the request is never offered for approval"
    );
}

/// #892: Auto-Accept is a delegation of the decision, not a surface — it
/// renders no card, so the card's completeness proof has nothing to guard. An
/// over-budget target is approved unseen, and the approval line names its
/// length. Declining here settled the request with no path to approve it from
/// any client (it leaves the server's pending list), so a model re-emitting the
/// same long command looped on denials.
#[tokio::test]
async fn auto_accept_approves_an_unshowable_permission() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let command = format!(
        "echo {}",
        "x".repeat(crate::backend::types::PERMISSION_TARGET_MAX_CHARS)
    );
    let mut backend = MockBackend::new(realistic_parts());
    backend.ask_permissions(vec![crate::backend::types::PermissionRequest {
        request_id: "per_long".into(),
        session_id: Some("ses_test".into()),
        permission: Some("bash".into()),
        patterns: vec![command],
        metadata: None,
        always: Vec::new(),
        always_supported: true,
    }]);
    let replies = std::sync::Arc::clone(&backend.reply_permission_calls);
    let (app, platform) = build_app(cfg, backend).await;
    seed_entry(
        &app,
        crate::config::SessionEntry {
            client_id: crate::config::DEFAULT_CLIENT_ID.into(),
            thread_key: crate::config::ThreadKey::new("chat_1".into(), "chat_1".into()),
            session_id: "ses_test".into(),
            directory: "/work".into(),
            agent: None,
            model: None,
            auto_accept: true,
            topic_anchor: None,
            topic_root: None,
            variant: None,
        },
    )
    .await;

    let mut seen = std::collections::HashSet::new();
    let (_, logs) = capture_logs(async {
        app.permission.sweep(&app.flow_handles(), &mut seen).await;
    })
    .await;

    assert_eq!(
        replies.lock().await.as_slice(),
        &[("per_long".to_string(), "once".to_string())],
        "auto-accept approves even a target the card cannot show"
    );
    let mut cards = platform.sent_cards().await;
    cards.extend(platform.replied_cards().await);
    assert!(
        cards.iter().all(
            |card| !card_text(card).contains(crate::feishu::card::question::UNSHOWABLE_PERMISSION_HEADER)
        ),
        "nothing was surfaced, so there is no refusal card to explain"
    );
    assert!(
        Turn::live_permissions(&app.cards_handle(), "ses_test")
            .await
            .is_empty(),
        "and no permission block is offered"
    );
    // Auto-Accept renders no surface at all, so it sends no companion either
    // (ADR-0101 is untouched by the companion lane, ADR-0102).
    assert!(
        !platform.calls.lock().await.iter().any(|call| matches!(
            call,
            PlatformCall::SendMessageReply { .. } | PlatformCall::UploadFile { .. }
        )),
        "auto-accept sends no companion message"
    );
    // The approval happened without a card, so the log is the only record of
    // what the session delegated: it must name the request, the target's exact
    // length, and the budget it crossed (ADR-0048: auto-accepted is INFO).
    let line = assert_line_level(&logs, "approved permission per_long", "INFO");
    assert!(
        line.contains("(targets total 3005), past the card's 3000 character budget"),
        "the unseen approval names the numbers it crossed: {line}"
    );
    let expected_first_line = format!("first line: echo {}…", "x".repeat(115));
    assert!(
        line.contains(&expected_first_line),
        "the first line is bounded at 120 characters with its cut marker: {line}"
    );
}

/// #892: turning Auto-Accept on approves the requests that are ALREADY pending,
/// including an over-budget one. This path renders no card either, so the
/// completeness proof must not reappear here (it used to decline them).
#[tokio::test]
async fn autoaccept_on_approves_an_already_pending_unshowable_permission() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let command = format!(
        "echo {}",
        "x".repeat(crate::backend::types::PERMISSION_TARGET_MAX_CHARS)
    );
    let mut backend = MockBackend::new(realistic_parts());
    backend.ask_permissions(vec![crate::backend::types::PermissionRequest {
        request_id: "per_long".into(),
        session_id: Some("ses_test".into()),
        permission: Some("bash".into()),
        patterns: vec![command],
        metadata: None,
        always: Vec::new(),
        always_supported: true,
    }]);
    let replies = std::sync::Arc::clone(&backend.reply_permission_calls);
    let (app, _platform) = build_app(cfg, backend).await;
    seed_session(&app, "ses_test", "/work").await;

    let (_, logs) = capture_logs(async {
        send_command(&app, "/autoaccept on", "msg_cmd").await;
    })
    .await;

    assert_eq!(
        replies.lock().await.as_slice(),
        &[("per_long".to_string(), "once".to_string())],
        "enabling auto-accept approves the pending over-budget request too"
    );
    let line = assert_line_level(&logs, "approved permission per_long", "INFO");
    assert!(
        line.contains("(targets total 3005), past the card's 3000 character budget"),
        "the toggle path leaves the same audit record: {line}"
    );
}

/// The `/autoaccept on` command persists before approving too: an unwritable
/// store must approve nothing and leave the flag (and memory) off.
#[tokio::test]
async fn autoaccept_on_with_an_unwritable_store_approves_nothing() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let command = format!(
        "echo {}",
        "x".repeat(crate::backend::types::PERMISSION_TARGET_MAX_CHARS)
    );
    let mut backend = MockBackend::new(realistic_parts());
    backend.ask_permissions(vec![crate::backend::types::PermissionRequest {
        request_id: "per_long".into(),
        session_id: Some("ses_test".into()),
        permission: Some("bash".into()),
        patterns: vec![command],
        metadata: None,
        always: Vec::new(),
        always_supported: true,
    }]);
    let replies = std::sync::Arc::clone(&backend.reply_permission_calls);
    let (app, _platform) = build_app(cfg, backend).await;
    seed_session(&app, "ses_test", "/work").await;

    app.sessions.lock().await.fail_writes_after(0);
    send_command(&app, "/autoaccept on", "msg_cmd").await;

    assert!(
        replies.lock().await.is_empty(),
        "an unwritable flag approves nothing"
    );
    assert!(
        !app.sessions
            .lock()
            .await
            .entry_for_session("ses_test")
            .unwrap()
            .auto_accept,
        "the failed write leaves memory off"
    );
}

/// #892: an Auto-Accept whose reply FAILS must stay retryable. `prepare` runs
/// after the sweep inserted the id into `seen`, so returning "handled" on the
/// error retired a still-pending request with no card: invisible and
/// unapprovable. `Prepared::Retry` removes the mark and the next sweep answers.
#[tokio::test]
async fn a_failed_auto_accept_of_an_unshowable_permission_is_retried() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let command = format!(
        "echo {}",
        "x".repeat(crate::backend::types::PERMISSION_TARGET_MAX_CHARS)
    );
    let mut backend = MockBackend::new(realistic_parts());
    backend.ask_permissions(vec![crate::backend::types::PermissionRequest {
        request_id: "per_long".into(),
        session_id: Some("ses_test".into()),
        permission: Some("bash".into()),
        patterns: vec![command],
        metadata: None,
        always: Vec::new(),
        always_supported: true,
    }]);
    backend
        .fail_reply_permission_count
        .store(1, std::sync::atomic::Ordering::SeqCst);
    let replies = std::sync::Arc::clone(&backend.reply_permission_calls);
    let (app, platform) = build_app(cfg, backend).await;
    seed_entry(
        &app,
        crate::config::SessionEntry {
            client_id: crate::config::DEFAULT_CLIENT_ID.into(),
            thread_key: crate::config::ThreadKey::new("chat_1".into(), "chat_1".into()),
            session_id: "ses_test".into(),
            directory: "/work".into(),
            agent: None,
            model: None,
            auto_accept: true,
            topic_anchor: None,
            topic_root: None,
            variant: None,
        },
    )
    .await;

    // The same `seen` set across both sweeps: a failed attempt that left its
    // mark would never be retried.
    let mut seen = std::collections::HashSet::new();
    app.permission.sweep(&app.flow_handles(), &mut seen).await;

    assert_eq!(
        replies.lock().await.as_slice(),
        &[("per_long".to_string(), "once".to_string())],
        "the first sweep tries the auto-approval"
    );
    assert!(
        !seen.contains("per_long"),
        "a failed approval must not retire the request from the sweep"
    );
    let mut cards = platform.sent_cards().await;
    cards.extend(platform.replied_cards().await);
    assert!(
        cards.is_empty(),
        "an auto-accept session posts no card, failed attempt included"
    );

    app.permission.sweep(&app.flow_handles(), &mut seen).await;
    assert_eq!(
        replies.lock().await.as_slice(),
        &[
            ("per_long".to_string(), "once".to_string()),
            ("per_long".to_string(), "once".to_string()),
        ],
        "the retry answers the still-pending request"
    );
}

/// #892: an Auto-Accept flag read that FAILS ("unknown", not "off") must retry,
/// never decline. A sub-task child is not in the store, so resolving its
/// ancestor costs a `session_info` hop; a transient failure there used to read
/// as "off" and reject the over-budget request for good.
#[tokio::test]
async fn an_unknown_auto_accept_flag_never_declines_an_unshowable_permission() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let command = format!(
        "echo {}",
        "x".repeat(crate::backend::types::PERMISSION_TARGET_MAX_CHARS)
    );
    let mut backend = MockBackend::new(realistic_parts());
    backend.with_session_parent("ses_child", "ses_parent");
    backend.ask_permissions(vec![crate::backend::types::PermissionRequest {
        request_id: "per_long".into(),
        session_id: Some("ses_child".into()),
        permission: Some("bash".into()),
        patterns: vec![command],
        metadata: None,
        always: Vec::new(),
        always_supported: true,
    }]);
    backend.fail_session_infos(1);
    let replies = std::sync::Arc::clone(&backend.reply_permission_calls);
    let (app, _platform) = build_app(cfg, backend).await;
    // The parent carries the flag; the child inherits it up the chain.
    seed_entry(
        &app,
        crate::config::SessionEntry {
            client_id: crate::config::DEFAULT_CLIENT_ID.into(),
            thread_key: crate::config::ThreadKey::new("chat_1".into(), "chat_1".into()),
            session_id: "ses_parent".into(),
            directory: "/work".into(),
            agent: None,
            model: None,
            auto_accept: true,
            topic_anchor: None,
            topic_root: None,
            variant: None,
        },
    )
    .await;

    let mut seen = std::collections::HashSet::new();
    app.permission.sweep(&app.flow_handles(), &mut seen).await;

    assert!(
        replies.lock().await.is_empty(),
        "an unknown flag must neither approve nor decline"
    );
    assert!(!seen.contains("per_long"), "and the request must stay retryable");

    // The hop heals: the child resolves to its auto-accept parent and the
    // approval lands.
    app.permission.sweep(&app.flow_handles(), &mut seen).await;
    assert_eq!(
        replies.lock().await.as_slice(),
        &[("per_long".to_string(), "once".to_string())],
        "the retry reads the inherited flag and approves"
    );
}

/// The unknown flag must also retry on a SHOWABLE target, where `refuse` does
/// not consult the flag at all: `prepare` is the only reader, and it must not
/// surface a card the session's Auto-Accept may have replaced.
#[tokio::test]
async fn an_unknown_auto_accept_flag_retries_a_showable_permission() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let mut backend = MockBackend::new(realistic_parts());
    backend.with_session_parent("ses_child", "ses_parent");
    backend.ask_permissions(vec![crate::backend::types::PermissionRequest {
        request_id: "per_short".into(),
        session_id: Some("ses_child".into()),
        permission: Some("bash".into()),
        patterns: vec!["ls".into()],
        metadata: None,
        always: Vec::new(),
        always_supported: true,
    }]);
    backend.fail_session_infos(1);
    let replies = std::sync::Arc::clone(&backend.reply_permission_calls);
    let (app, platform) = build_app(cfg, backend).await;
    seed_entry(
        &app,
        crate::config::SessionEntry {
            client_id: crate::config::DEFAULT_CLIENT_ID.into(),
            thread_key: crate::config::ThreadKey::new("chat_1".into(), "chat_1".into()),
            session_id: "ses_parent".into(),
            directory: "/work".into(),
            agent: None,
            model: None,
            auto_accept: true,
            topic_anchor: None,
            topic_root: None,
            variant: None,
        },
    )
    .await;

    let mut seen = std::collections::HashSet::new();
    let (_, first) = capture_logs(async {
        app.permission.sweep(&app.flow_handles(), &mut seen).await;
    })
    .await;

    assert!(replies.lock().await.is_empty(), "nothing answered while unknown");
    assert!(!seen.contains("per_short"), "the request must stay retryable");
    let mut cards = platform.sent_cards().await;
    cards.extend(platform.replied_cards().await);
    assert!(
        cards.is_empty(),
        "a possibly-auto-accepted session must not get a card during the unknown"
    );
    // ADR-0048: the first failed read WARNs with its cause.
    assert_line_level(&first, "auto-accept flag read ses_child failed", "WARN");

    app.permission.sweep(&app.flow_handles(), &mut seen).await;
    assert_eq!(
        replies.lock().await.as_slice(),
        &[("per_short".to_string(), "once".to_string())],
        "the healed read approves through the inherited flag"
    );
}

/// A toggle click that cannot persist the flag must claim nothing: no approval,
/// no mode receipt, and the click's claim released so the request is offered
/// again. The old code returned an empty approval list the caller could not tell
/// from "nothing pending", then reported success.
#[tokio::test]
async fn a_permission_toggle_that_cannot_persist_claims_nothing() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let backend = MockBackend::new(realistic_parts());
    let perm_calls = backend.reply_permission_calls.clone();
    let (app, _platform) = build_app(cfg, backend).await;

    // The session is not in the store, so the flag has nowhere to persist.
    let value = serde_json::json!({
        "action": "perm",
        "reply": "autoaccept",
        "session_id": "ses_ghost",
        "request_id": "per_ghost",
        "directory": "/work",
        "perm_label": "✅ 已开启自动授权",
        "perm_color": "blue",
    });
    let result = app.host_action(value).await.expect("failure result card");

    assert_eq!(
        result.toast.as_deref(),
        Some("自动授权状态保存失败，请重试。"),
        "the failure is reported, not sold as success"
    );
    assert!(
        result.card.is_none(),
        "the original card must survive: a buttonless result would strand the request"
    );
    assert!(
        perm_calls.lock().await.is_empty(),
        "nothing was approved without a persisted flag"
    );
    assert!(
        !app.requests_handle()
            .answered_requests
            .lock()
            .await
            .contains("per_ghost"),
        "the click's claim must be released so the request can be offered again"
    );
}

/// A flag WRITE failure (not just a missing entry) must also leave the store's
/// memory off: otherwise the very next sweep reads Auto-Accept as on and
/// approves the over-budget request the user was told was not enabled (#892).
#[tokio::test]
async fn a_permission_toggle_whose_flag_write_fails_approves_nothing() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let command = format!(
        "echo {}",
        "x".repeat(crate::backend::types::PERMISSION_TARGET_MAX_CHARS)
    );
    let mut backend = MockBackend::new(realistic_parts());
    backend.ask_permissions(vec![crate::backend::types::PermissionRequest {
        request_id: "per_long".into(),
        session_id: Some("ses_test".into()),
        permission: Some("bash".into()),
        patterns: vec![command],
        metadata: None,
        always: Vec::new(),
        always_supported: true,
    }]);
    let perm_calls = backend.reply_permission_calls.clone();
    let (app, _platform) = build_app(cfg, backend).await;
    seed_entry(
        &app,
        crate::config::SessionEntry {
            client_id: crate::config::DEFAULT_CLIENT_ID.into(),
            thread_key: crate::config::ThreadKey::new("chat_1".into(), "chat_1".into()),
            session_id: "ses_test".into(),
            directory: "/work".into(),
            agent: None,
            model: None,
            auto_accept: false,
            topic_anchor: None,
            topic_root: None,
            variant: None,
        },
    )
    .await;

    // Refuse the toggle's persist write; the in-memory flag must roll back.
    app.sessions.lock().await.fail_writes_after(0);
    let value = serde_json::json!({
        "action": "perm",
        "reply": "autoaccept",
        "session_id": "ses_test",
        "request_id": "per_long",
        "directory": "/work",
        "perm_label": "✅ 已开启自动授权",
        "perm_color": "blue",
    });
    let result = app.host_action(value).await.expect("failure result card");

    assert_eq!(result.toast.as_deref(), Some("自动授权状态保存失败，请重试。"));
    assert!(
        perm_calls.lock().await.is_empty(),
        "an unwritable flag approves nothing"
    );
    assert!(
        !app.sessions
            .lock()
            .await
            .entry_for_session("ses_test")
            .unwrap()
            .auto_accept,
        "the failed write leaves memory off, so the next sweep cannot approve unseen"
    );
}

/// ADR-0048: the Auto-Accept reply retries every sweep, so its failure WARNs
/// once, DEBUGs identical repeats, and INFOs the recovery. A permanently
/// broken reply path must not flood the log.
#[tokio::test]
async fn a_failing_auto_accept_warns_once_then_debugs_and_reports_recovery() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let command = format!(
        "echo {}",
        "x".repeat(crate::backend::types::PERMISSION_TARGET_MAX_CHARS)
    );
    let mut backend = MockBackend::new(realistic_parts());
    backend.ask_permissions(vec![crate::backend::types::PermissionRequest {
        request_id: "per_long".into(),
        session_id: Some("ses_test".into()),
        permission: Some("bash".into()),
        patterns: vec![command],
        metadata: None,
        always: Vec::new(),
        always_supported: true,
    }]);
    backend.fail_permission_replies(2);
    let replies = std::sync::Arc::clone(&backend.reply_permission_calls);
    let (app, _platform) = build_app(cfg, backend).await;
    seed_entry(
        &app,
        crate::config::SessionEntry {
            client_id: crate::config::DEFAULT_CLIENT_ID.into(),
            thread_key: crate::config::ThreadKey::new("chat_1".into(), "chat_1".into()),
            session_id: "ses_test".into(),
            directory: "/work".into(),
            agent: None,
            model: None,
            auto_accept: true,
            topic_anchor: None,
            topic_root: None,
            variant: None,
        },
    )
    .await;

    let mut seen = std::collections::HashSet::new();
    let (_, first) = capture_logs(async {
        app.permission.sweep(&app.flow_handles(), &mut seen).await;
    })
    .await;
    assert_line_level(&first, "auto-accept per_long failed", "WARN");

    let (_, second) = capture_logs(async {
        app.permission.sweep(&app.flow_handles(), &mut seen).await;
    })
    .await;
    assert_line_level(&second, "auto-accept per_long still failing", "DEBUG");

    let (_, third) = capture_logs(async {
        app.permission.sweep(&app.flow_handles(), &mut seen).await;
    })
    .await;
    assert_line_level(&third, "auto-accept per_long recovered", "INFO");
    assert_eq!(
        replies.lock().await.len(),
        3,
        "each sweep retried the reply until it landed"
    );
}
