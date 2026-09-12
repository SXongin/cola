use crate::bridge::test_support::*;

#[tokio::test]
async fn external_message_from_shared_store_notifies_feishu() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let mut mock = MockBackend::new(realistic_parts());
    mock.external_user_message = Some("OpenChamber 里发的消息".to_string());
    let (app, platform) = build_app(cfg, mock).await;

    // A known session whose chat the notification goes to.
    seed_entry(
        &app,
        crate::config::SessionEntry {
            thread_key: crate::config::ThreadKey::new("oc_group_1".into(), "oc_group_1".into()),
            session_id: "ses_ext".into(),
            directory: "/tmp/ext".into(),
            agent: None,
            model: None,
            auto_accept: false,
            topic_anchor: None,
            topic_root: None,
            variant: None,
        },
    )
    .await;
    // Baseline: a minute ago, so the fresh user message is "new".
    let watermark = chrono::Utc::now().timestamp_millis() - 60_000;
    app.external
        .last_user_msg_epoch
        .lock()
        .await
        .insert("ses_ext".into(), watermark);

    app.external
        .poll_interval_ms
        .store(50, std::sync::atomic::Ordering::Relaxed);
    tokio::spawn({
        let app = app.clone();
        async move {
            let _ = app.external.poll_loop(&app.core).await;
        }
    });
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;

    let calls = platform.calls.lock().await.clone();
    let notify = calls.iter().find_map(|c| match c {
        PlatformCall::SendCard { card, .. } if card.to_string().contains("有新消息") => {
            Some(card.clone())
        }
        _ => None,
    });
    let notify = notify.expect("external message should produce a notification card");
    assert!(
        notify.to_string().contains("OpenChamber 里发的消息"),
        "notification should preview the message: {}",
        notify
    );
}

/// A hung `messages` read (a half-open connection left by a server
/// restart) must not freeze the external poller forever: the read is
/// bounded, so a later poll still notifies about the external message.
#[tokio::test]
async fn external_poller_recovers_when_messages_hangs() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let mut mock = MockBackend::new(realistic_parts());
    mock.external_user_message = Some("OpenChamber 里发的消息".to_string());
    // The first `messages` call hangs forever, like a request in flight
    // when the server was SIGTERM'd; later calls serve normally.
    mock.hang_messages.store(1, std::sync::atomic::Ordering::SeqCst);
    let (app, platform) = build_app(cfg, mock).await;

    seed_entry(
        &app,
        crate::config::SessionEntry {
            thread_key: crate::config::ThreadKey::new("oc_group_1".into(), "oc_group_1".into()),
            session_id: "ses_ext".into(),
            directory: "/tmp/ext".into(),
            agent: None,
            model: None,
            auto_accept: false,
            topic_anchor: None,
            topic_root: None,
            variant: None,
        },
    )
    .await;
    // Baseline: a minute ago, so the fresh user message is "new".
    let watermark = chrono::Utc::now().timestamp_millis() - 60_000;
    app.external
        .last_user_msg_epoch
        .lock()
        .await
        .insert("ses_ext".into(), watermark);

    app.external
        .poll_interval_ms
        .store(20, std::sync::atomic::Ordering::Relaxed);
    app.external
        .request_timeout_ms
        .store(50, std::sync::atomic::Ordering::Relaxed);
    tokio::spawn({
        let app = app.clone();
        async move {
            let _ = app.external.poll_loop(&app.core).await;
        }
    });

    // Without a bound on `messages` the poller sits on the first hung call
    // forever and the notification never arrives.
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(2);
    loop {
        let notified = platform.calls.lock().await.iter().any(|c| {
            matches!(
                c,
                PlatformCall::SendCard { card, .. }
                    if card.to_string().contains("有新消息")
            )
        });
        if notified {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "external poller never recovered from the hung messages call"
        );
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
}

/// ADR-0026 regression (observed 2026-09-09): when a server dies mid-turn and
/// cola heals by starting its own server on the same store, cola's OWN
/// persisted user message (`msg_cola_` id) surfaces as newer than the stale
/// Sync Watermark. It must NOT be re-notified into Feishu as an external
/// message — authorship is carried by the id prefix, not by the watermark.
#[tokio::test]
async fn cola_own_message_after_heal_is_never_notified_external() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let mut mock = MockBackend::new(realistic_parts());
    // cola's own prompt persisted on the store before the crash (a
    // `msg_cola_` id, exactly what the real server echoes back).
    mock.cola_user_messages.insert(
        "ses_ext".into(),
        "可以把我本地的 openchamber serve 杀掉吗？".to_string(),
    );
    let (app, platform) = build_app(cfg, mock).await;

    seed_entry(
        &app,
        crate::config::SessionEntry {
            thread_key: crate::config::ThreadKey::new("oc_group_1".into(), "oc_group_1".into()),
            session_id: "ses_ext".into(),
            directory: "/tmp/ext".into(),
            agent: None,
            model: None,
            auto_accept: false,
            topic_anchor: None,
            topic_root: None,
            variant: None,
        },
    )
    .await;
    // Stale watermark: the old run_prompt-era epoch predates the message the
    // dying server actually persisted — the exact state that caused the bug.
    let stale = chrono::Utc::now().timestamp_millis() - 60_000;
    app.external
        .last_user_msg_epoch
        .lock()
        .await
        .insert("ses_ext".into(), stale);

    app.external
        .poll_interval_ms
        .store(50, std::sync::atomic::Ordering::Relaxed);
    tokio::spawn({
        let app = app.clone();
        async move {
            let _ = app.external.poll_loop(&app.core).await;
        }
    });
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;

    // No external-message card: cola's own round is recognised, not echoed.
    let calls = platform.calls.lock().await.clone();
    assert!(
        !calls.iter().any(|c| match c {
            PlatformCall::SendCard { card, .. } | PlatformCall::ReplyCard { card, .. } => {
                card.to_string().contains("有新消息")
            }
            _ => false,
        }),
        "cola's own message must never be notified as external: {calls:?}"
    );
    // The watermark advanced PAST cola's message (so a later genuine
    // external message is still detected).
    let watermark = app
        .external
        .last_user_msg_epoch
        .lock()
        .await
        .get("ses_ext")
        .copied();
    assert!(
        watermark.is_some_and(|w| w > stale),
        "watermark should advance over cola's own message: {watermark:?}"
    );
}

/// ADR-0026: a GENUINE external message posted AFTER cola's own (the heal
/// scenario) is still notified — recognising cola's authorship must not
/// swallow real OpenChamber traffic.
#[tokio::test]
async fn newer_external_message_after_cola_own_still_notifies() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let mut mock = MockBackend::new(realistic_parts());
    mock.cola_user_messages
        .insert("ses_ext".into(), "cola 自己的一轮".to_string());
    mock.external_user_message = Some("OpenChamber 后来发的消息".to_string());
    let (app, platform) = build_app(cfg, mock).await;

    seed_entry(
        &app,
        crate::config::SessionEntry {
            thread_key: crate::config::ThreadKey::new("oc_group_1".into(), "oc_group_1".into()),
            session_id: "ses_ext".into(),
            directory: "/tmp/ext".into(),
            agent: None,
            model: None,
            auto_accept: false,
            topic_anchor: None,
            topic_root: None,
            variant: None,
        },
    )
    .await;
    let stale = chrono::Utc::now().timestamp_millis() - 60_000;
    app.external
        .last_user_msg_epoch
        .lock()
        .await
        .insert("ses_ext".into(), stale);

    app.external
        .poll_interval_ms
        .store(50, std::sync::atomic::Ordering::Relaxed);
    tokio::spawn({
        let app = app.clone();
        async move {
            let _ = app.external.poll_loop(&app.core).await;
        }
    });
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;

    let calls = platform.calls.lock().await.clone();
    let notify = calls.iter().find_map(|c| match c {
        PlatformCall::SendCard { card, .. } if card.to_string().contains("有新消息") => {
            Some(card.clone())
        }
        _ => None,
    });
    let notify = notify.expect("the genuine external message should be notified");
    assert!(
        notify.to_string().contains("OpenChamber 后来发的消息"),
        "notification should preview the external message: {}",
        notify
    );
}

/// ADR-0017: an external message on a HISTORICAL (non-active) lobby session
/// must NOT be notified into the chat — only the thread's active session is
/// synced. Its watermark is also cleared so a later /switch back re-syncs.
#[tokio::test]
async fn external_message_to_historical_session_is_not_notified() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let mut mock = MockBackend::new(realistic_parts());
    // External message ONLY on the historical session; the active session
    // has none (otherwise BOTH would get an external message and the test
    // couldn't isolate the historical one being suppressed).
    mock.external_user_messages
        .insert("ses_historical".into(), "历史会话的外部消息".to_string());
    let (app, platform) = build_app(cfg, mock).await;
    let key = crate::config::ThreadKey::new("chat_1".into(), "chat_1".into());

    // Active session A; historical session B mapped to the SAME lobby.
    // set_active pushes to the front, so the LAST call is the active one.
    seed_entry(
        &app,
        crate::config::SessionEntry {
            thread_key: key.clone(),
            session_id: "ses_historical".into(),
            directory: "/tmp/hist".into(),
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
            thread_key: key.clone(),
            session_id: "ses_active".into(),
            directory: "/tmp/active".into(),
            agent: None,
            model: None,
            auto_accept: false,
            topic_anchor: None,
            topic_root: None,
            variant: None,
        },
    )
    .await;
    // The ACTIVE session is `ses_active` (last set_active pushes to front).
    assert_eq!(
        app.sessions.lock().await.get_active(&key).unwrap().session_id,
        "ses_active"
    );
    // Both sessions have a watermark from before the external message.
    let watermark = chrono::Utc::now().timestamp_millis() - 60_000;
    app.external
        .last_user_msg_epoch
        .lock()
        .await
        .insert("ses_active".into(), watermark);
    app.external
        .last_user_msg_epoch
        .lock()
        .await
        .insert("ses_historical".into(), watermark);

    app.external
        .poll_interval_ms
        .store(50, std::sync::atomic::Ordering::Relaxed);
    tokio::spawn({
        let app = app.clone();
        async move {
            let _ = app.external.poll_loop(&app.core).await;
        }
    });
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;

    // No notification card at all: the only external message is on the
    // historical session, which must be suppressed.
    let calls = platform.calls.lock().await.clone();
    assert!(
        !calls.iter().any(|c| match c {
            PlatformCall::SendCard { card, .. } | PlatformCall::ReplyCard { card, .. } => {
                card.to_string().contains("有新消息")
            }
            _ => false,
        }),
        "historical session external message must NOT be notified: {calls:?}"
    );
    // The historical session's watermark was cleared (ready to re-sync
    // silently when it becomes active again).
    assert!(
        !app.external
            .last_user_msg_epoch
            .lock()
            .await
            .contains_key("ses_historical"),
        "historical session watermark should be cleared"
    );
}

/// ADR-0017: switching back to a historical session makes it the active one
/// and re-syncs SILENTLY — external messages received while it was
/// inactive are marked read, not replayed as a stale notification.
#[tokio::test]
async fn reactivated_session_resyncs_silently() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let mut mock = MockBackend::new(realistic_parts());
    // The external message is on the session that is being REACTIVATED.
    mock.external_user_messages
        .insert("ses_old".into(), "离开期间的外部消息".to_string());
    let (app, platform) = build_app(cfg, mock).await;
    let key = crate::config::ThreadKey::new("chat_1".into(), "chat_1".into());

    // set_active pushes to the front, so the LAST call is the active one
    // (ses_old, the session being reactivated by /switch).
    seed_entry(
        &app,
        crate::config::SessionEntry {
            thread_key: key.clone(),
            session_id: "ses_new".into(),
            directory: "/tmp/new".into(),
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
            thread_key: key.clone(),
            session_id: "ses_old".into(),
            directory: "/tmp/old".into(),
            agent: None,
            model: None,
            auto_accept: false,
            topic_anchor: None,
            topic_root: None,
            variant: None,
        },
    )
    .await;
    // ses_old is the active session after the /switch back.
    assert_eq!(
        app.sessions.lock().await.get_active(&key).unwrap().session_id,
        "ses_old"
    );
    // While ses_old was historical, the poller cleared its watermark (see the
    // test above). So on the first poll after reactivation the map has NO
    // entry for it → first-observation path → silent re-sync, no notify.
    assert!(
        !app.external
            .last_user_msg_epoch
            .lock()
            .await
            .contains_key("ses_old"),
        "precondition: watermark was cleared while inactive"
    );

    app.external
        .poll_interval_ms
        .store(50, std::sync::atomic::Ordering::Relaxed);
    tokio::spawn({
        let app = app.clone();
        async move {
            let _ = app.external.poll_loop(&app.core).await;
        }
    });
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;

    // No notification: the external message was marked read on first
    // observation (silent re-sync), not replayed as stale.
    let calls = platform.calls.lock().await.clone();
    assert!(
        !calls.iter().any(|c| match c {
            PlatformCall::SendCard { card, .. } | PlatformCall::ReplyCard { card, .. } => {
                card.to_string().contains("有新消息")
            }
            _ => false,
        }),
        "reactivated session must re-sync silently, no notification: {calls:?}"
    );
    // The watermark is now recorded for the reactivated session.
    assert!(
        app.external
            .last_user_msg_epoch
            .lock()
            .await
            .contains_key("ses_old"),
        "reactivated session should have a recorded watermark"
    );
}

/// External messages to a TOPIC-backed session are notified by replying to
/// a message INSIDE the topic, not sent to the chat top level. Covers the
/// no-persisted-anchor case (session created before `/topic` stored the
/// anchor): `resolve_topic_anchor` queries the thread for the newest bot
/// message and replies to it.
#[tokio::test]
async fn external_message_to_topic_session_notifies_into_thread() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let mut mock = MockBackend::new(realistic_parts());
    mock.external_user_message = Some("话题里的外部消息".to_string());
    let (app, platform) = build_app(cfg, mock).await;

    // A TOPIC-backed session (thread_id != chat_id) with NO persisted
    // anchor (like the old /topic sessions) — the anchor must be resolved
    // by querying the thread.
    seed_entry(
        &app,
        crate::config::SessionEntry {
            thread_key: crate::config::ThreadKey::new("chat_1".into(), "omt_topic_ext".into()),
            session_id: "ses_ext".into(),
            directory: "/tmp/ext".into(),
            agent: None,
            model: None,
            auto_accept: false,
            topic_anchor: None,
            topic_root: None,
            variant: None,
        },
    )
    .await;
    let watermark = chrono::Utc::now().timestamp_millis() - 60_000;
    app.external
        .last_user_msg_epoch
        .lock()
        .await
        .insert("ses_ext".into(), watermark);
    app.external
        .poll_interval_ms
        .store(50, std::sync::atomic::Ordering::Relaxed);

    tokio::spawn({
        let app = app.clone();
        async move {
            let _ = app.external.poll_loop(&app.core).await;
        }
    });
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;

    let calls = platform.calls.lock().await.clone();
    assert!(
        calls.iter().any(|c| matches!(
            c,
            PlatformCall::ReplyCard { reply_to, card } if reply_to == "msg_in_topic_anchor" && card.to_string().contains("有新消息")
        )),
        "topic external notification should reply into the topic (resolved anchor): {calls:?}"
    );
    assert!(
        !calls
            .iter()
            .any(|c| matches!(c, PlatformCall::SendCard { receive_id, .. } if receive_id == "chat_1")),
        "topic external notification must NOT go to chat top level: {calls:?}"
    );
}

/// The model's reply to an external (OpenChamber-posted) message must render
/// INTO the notification card — update in place, so the Feishu side sees the
/// answer without a second card. The reply is scripted to arrive on a LATER
/// poll: the renderer must idle (no card update) while the reply is absent,
/// then stream reasoning/tools/text in and finalize Done when the turn
/// completes.
#[tokio::test]
async fn external_message_reply_renders_into_notification_card() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let repo = git_repo();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let mut mock = MockBackend::new(realistic_parts());
    mock.external_user_message = Some("OpenChamber 里发的消息".to_string());
    // OpenCode's reply to that message: reasoning → tool → text → stop.
    mock.external_reply_parts = Some(serde_json::json!([
        { "type": "step-start", "snapshot": "x" },
        { "type": "reasoning", "text": "我来看看目录。" },
        { "type": "tool", "tool": "bash", "callID": "call_1",
          "state": { "status": "completed", "input": { "command": "ls" }, "output": "src" } },
        { "type": "text", "text": "目录里有 src。" },
        { "type": "step-finish", "reason": "stop" },
    ]));
    let reply_ready = mock.external_reply_ready.clone();
    let (app, platform) = build_app(cfg, mock).await;

    // A known session whose chat the notification goes to.
    seed_entry(
        &app,
        crate::config::SessionEntry {
            thread_key: crate::config::ThreadKey::new("oc_group_1".into(), "oc_group_1".into()),
            session_id: "ses_ext".into(),
            directory: repo.path().to_string_lossy().to_string(),
            agent: None,
            model: None,
            auto_accept: false,
            topic_anchor: None,
            topic_root: None,
            variant: None,
        },
    )
    .await;
    let watermark = chrono::Utc::now().timestamp_millis() - 60_000;
    app.external
        .poll_interval_ms
        .store(50, std::sync::atomic::Ordering::Relaxed);
    app.external
        .last_user_msg_epoch
        .lock()
        .await
        .insert("ses_ext".into(), watermark);

    tokio::spawn({
        let app = app.clone();
        async move {
            let _ = app.external.poll_loop(&app.core).await;
        }
    });
    // First poll tick (8s) sends the notification and arms the renderer;
    // a few render-loop ticks then run while the reply is still absent.
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;

    // The renderer must NOT have touched the card yet — the reply hasn't
    // arrived, so the notification card stays as sent.
    let calls_before = platform.calls.lock().await.clone();
    assert!(
        calls_before
            .iter()
            .all(|c| !matches!(c, PlatformCall::UpdateMessage { .. })),
        "no card update while the reply is absent: {calls_before:?}"
    );
    assert!(
        calls_before
            .iter()
            .any(|c| matches!(c, PlatformCall::SendCard { receive_id, .. } if receive_id == "oc_group_1")),
        "notification card should be sent: {calls_before:?}"
    );

    // The turn's work: the AI switched branches before its reply landed —
    // the final card must show where it landed (ADR-0019 end refresh).
    git_in(repo.path(), &["switch", "-c", "feat/ext"]);
    // Now the model replies; the renderer picks it up on the next poll.
    reply_ready.store(true, std::sync::atomic::Ordering::SeqCst);
    tokio::time::sleep(std::time::Duration::from_millis(2500)).await;

    let calls = platform.calls.lock().await.clone();
    let sent_card = calls.iter().find_map(|c| match c {
        PlatformCall::SendCard { receive_id, card } if receive_id == "oc_group_1" => Some(card.clone()),
        _ => None,
    });
    let sent_card = sent_card.expect("notification card should be sent");
    // The reply rendered IN PLACE on the SAME card (msg_sent): it shows the
    // external user's message and the AI's reasoning/tool/text, finalized Done.
    // The LAST update is the finalized card (earlier flushes streamed while
    // the model was still working).
    let final_card = calls.iter().rev().find_map(|c| match c {
        PlatformCall::UpdateMessage { message_id, card } if message_id == "msg_sent" => Some(card.clone()),
        _ => None,
    });
    let final_card = final_card.expect("the notification card must be updated in place");
    let text = final_card.to_string();
    assert!(text.contains("✅"), "reply card should be Done: {}", text);
    assert!(
        text.contains("OpenChamber 里发的消息"),
        "the external user message must stay visible: {}",
        text
    );
    assert!(text.contains("我来看看目录"), "reasoning missing: {}", text);
    assert!(text.contains("bash"), "tool panel missing: {}", text);
    assert!(text.contains("目录里有 src。"), "reply text missing: {}", text);
    assert!(
        text.contains("· feat/ext"),
        "final footer must show the end branch: {}",
        text
    );

    // No SECOND card was sent — the reply lives entirely on the notification.
    let second_cards = calls
        .iter()
        .filter(|c| matches!(c, PlatformCall::ReplyCard { .. } | PlatformCall::SendCard { .. }))
        .count();
    assert_eq!(
        second_cards, 1,
        "only the notification card should be sent, got: {calls:?}"
    );
    // Sanity: the notification card was NOT replaced by a different one.
    assert!(sent_card.to_string().contains("有新消息"));
}

/// The renderer guard: arming a renderer for the SAME external message must
/// be a no-op (no clobbering of the armed card), while a NEWER message must
/// replace the armed renderer so the old one exits on the next poll.
#[tokio::test]
async fn external_reply_render_guard_replaces_only_newer_messages() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let mut mock = MockBackend::new(realistic_parts());
    // No reply parts: the armed loops just idle/exit; we assert state only.
    mock.external_user_message = Some("外部消息".to_string());
    let (app, _platform) = build_app(cfg, mock).await;

    // Arm once for the first external message.
    app.external
        .start_reply_render(&app.core, "ses_ext", 1000, "n1", "第一条")
        .await;
    {
        let cards = app.cards.lock().await;
        let acc = &cards.get("ses_ext").expect("renderer accumulator").acc;
        assert_eq!(acc.submit_epoch_ms, Some(1000));
        assert_eq!(acc.reply_to_message_id.as_deref(), Some("n1"));
    }
    assert_eq!(
        app.cards
            .lock()
            .await
            .get("ses_ext")
            .and_then(|c| c.card_message_id.as_deref()),
        Some("n1")
    );

    // Re-arming for the SAME message (e.g. a duplicate poll) is a no-op:
    // the armed card id and epoch must not be clobbered.
    app.external
        .start_reply_render(&app.core, "ses_ext", 1000, "n1b", "第一条")
        .await;
    {
        let cards = app.cards.lock().await;
        let acc = &cards.get("ses_ext").expect("renderer accumulator").acc;
        assert_eq!(acc.submit_epoch_ms, Some(1000));
        assert_eq!(acc.reply_to_message_id.as_deref(), Some("n1"));
    }
    assert_eq!(
        app.cards
            .lock()
            .await
            .get("ses_ext")
            .and_then(|c| c.card_message_id.as_deref()),
        Some("n1")
    );

    // A NEWER external message replaces the armed renderer (its card id and
    // epoch move to the new notification).
    app.external
        .start_reply_render(&app.core, "ses_ext", 2000, "n2", "第二条")
        .await;
    {
        let cards = app.cards.lock().await;
        let acc = &cards.get("ses_ext").expect("renderer accumulator").acc;
        assert_eq!(acc.submit_epoch_ms, Some(2000));
        assert_eq!(acc.reply_to_message_id.as_deref(), Some("n2"));
    }
    assert_eq!(
        app.cards
            .lock()
            .await
            .get("ses_ext")
            .and_then(|c| c.card_message_id.as_deref()),
        Some("n2")
    );
}

/// The external-reply renderer's hard-timeout branch: a partial reply is
/// rendered but the model never finishes, so the loop finalizes the card as
/// Done when the (injected, tiny) timeout elapses — the card never sits on
/// an eternal spinner. Exercises the timeout with millisecond fields instead
/// of the production 10-minute default.
#[tokio::test]
async fn external_reply_render_times_out_and_finalizes_partial_content() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let mut mock = MockBackend::new(realistic_parts());
    mock.external_user_message = Some("OpenChamber 里发的消息".to_string());
    // A partial reply: reasoning + text, but NO step-finish — the turn never
    // completes, so the loop must be stopped by the timeout.
    mock.external_reply_parts = Some(serde_json::json!([
        { "type": "step-start", "snapshot": "x" },
        { "type": "reasoning", "text": "我在想。" },
        { "type": "text", "text": "部分回答。" },
    ]));
    mock.external_reply_ready
        .store(true, std::sync::atomic::Ordering::SeqCst);
    let (app, platform) = build_app(cfg, mock).await;

    // A known session whose chat the notification goes to.
    seed_entry(
        &app,
        crate::config::SessionEntry {
            thread_key: crate::config::ThreadKey::new("oc_group_1".into(), "oc_group_1".into()),
            session_id: "ses_ext".into(),
            directory: "/tmp/ext".into(),
            agent: None,
            model: None,
            auto_accept: false,
            topic_anchor: None,
            topic_root: None,
            variant: None,
        },
    )
    .await;
    // Tiny cadences + timeout so the whole branch runs in milliseconds.
    app.external
        .poll_interval_ms
        .store(50, std::sync::atomic::Ordering::Relaxed);
    app.external
        .render_poll_ms
        .store(5, std::sync::atomic::Ordering::Relaxed);
    app.external
        .render_timeout_ms
        .store(20, std::sync::atomic::Ordering::Relaxed);
    let watermark = chrono::Utc::now().timestamp_millis() - 60_000;
    app.external
        .last_user_msg_epoch
        .lock()
        .await
        .insert("ses_ext".into(), watermark);

    // The poll loop detects the external message, sends the notification,
    // arms the renderer with the REAL message epoch, and the renderer
    // renders the partial reply then times out and finalizes it Done.
    tokio::spawn({
        let app = app.clone();
        async move {
            let _ = app.external.poll_loop(&app.core).await;
        }
    });
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;

    // The partial content was rendered AND the card finalized as Done (the
    // final update carries the terminal state).
    let calls = platform.calls.lock().await.clone();
    let updates: Vec<serde_json::Value> = calls
        .iter()
        .filter_map(|c| match c {
            PlatformCall::UpdateMessage { card, .. } => Some(card.clone()),
            _ => None,
        })
        .collect();
    let last = updates.last().expect("card was updated at least once");
    assert!(
        last.to_string().contains("部分回答"),
        "partial text must render: {}",
        last
    );
    let done_header = last["header"]["title"]["content"].as_str().unwrap_or("");
    assert!(
        done_header.contains("完成") || done_header.contains("✓"),
        "timeout must finalize the card as Done, header: {}",
        done_header
    );
}
