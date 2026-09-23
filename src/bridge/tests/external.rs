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
        PlatformCall::SendCard { card, .. } if card_text(card).contains("有新消息") => Some(card.clone()),
        _ => None,
    });
    let notify = notify.expect("external message should produce a notification card");
    assert!(
        card_text(&notify).contains("OpenChamber 里发的消息"),
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
    mock.hang_message_reads(1);
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
                    if card_text(card).contains("有新消息")
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
                card_text(card).contains("有新消息")
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
        PlatformCall::SendCard { card, .. } if card_text(card).contains("有新消息") => Some(card.clone()),
        _ => None,
    });
    let notify = notify.expect("the genuine external message should be notified");
    assert!(
        card_text(&notify).contains("OpenChamber 后来发的消息"),
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
    mock.external_message_for("ses_historical", "历史会话的外部消息");
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
                card_text(card).contains("有新消息")
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
    mock.external_message_for("ses_old", "离开期间的外部消息");
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
                card_text(card).contains("有新消息")
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
            PlatformCall::ReplyCard { reply_to, card } if reply_to == "msg_in_topic_anchor" && card_text(card).contains("有新消息")
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
    assert!(card_text(&sent_card).contains("有新消息"));
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
        let cards = app.cards_handle();
        assert_eq!(Turn::armed_turn_anchor(&cards, "ses_ext").await, Some(1000));
        assert_eq!(Turn::reply_target(&cards, "ses_ext").await.as_deref(), Some("n1"));
        assert_eq!(
            Turn::card_message_id(&cards, "ses_ext").await.as_deref(),
            Some("n1")
        );
    }

    // Re-arming for the SAME message (e.g. a duplicate poll) is a no-op:
    // the armed card id and epoch must not be clobbered.
    app.external
        .start_reply_render(&app.core, "ses_ext", 1000, "n1b", "第一条")
        .await;
    {
        let cards = app.cards_handle();
        assert_eq!(Turn::armed_turn_anchor(&cards, "ses_ext").await, Some(1000));
        assert_eq!(Turn::reply_target(&cards, "ses_ext").await.as_deref(), Some("n1"));
        assert_eq!(
            Turn::card_message_id(&cards, "ses_ext").await.as_deref(),
            Some("n1")
        );
    }

    // A NEWER external message replaces the armed renderer (its card id and
    // epoch move to the new notification).
    app.external
        .start_reply_render(&app.core, "ses_ext", 2000, "n2", "第二条")
        .await;
    {
        let cards = app.cards_handle();
        assert_eq!(Turn::armed_turn_anchor(&cards, "ses_ext").await, Some(2000));
        assert_eq!(Turn::reply_target(&cards, "ses_ext").await.as_deref(), Some("n2"));
        assert_eq!(
            Turn::card_message_id(&cards, "ses_ext").await.as_deref(),
            Some("n2")
        );
    }
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
        card_text(last).contains("部分回答"),
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

/// The user's external message stays ABOVE the streamed reply even though the
/// reply's parts carry server times (the preview is keyed just before the
/// turn's epoch): keying the whole timeline by part time must not reorder the
/// synthetic preview against the in-flight turn (regression guard for the
/// external-reply card composition, ADR-0028).
#[tokio::test]
async fn external_reply_keeps_the_user_message_above_it() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let now = chrono::Utc::now().timestamp_millis();
    let mut mock = MockBackend::new(realistic_parts());
    mock.external_user_message = Some("OpenChamber 里发的消息".to_string());
    // The external message was posted half a minute ago; the reply's parts
    // carry real server times after it but well before cola renders them.
    mock.external_user_created.lock().unwrap().replace(now - 30_000);
    mock.external_reply_parts = Some(serde_json::json!([
        { "type": "step-start", "snapshot": "x" },
        { "type": "reasoning", "text": "我来看看目录。",
          "time": { "start": now - 20_000, "end": now - 19_000 } },
        { "type": "text", "text": "目录里有 src。",
          "time": { "start": now - 18_000, "end": now - 17_000 } },
        { "type": "step-finish", "reason": "stop" },
    ]));
    let reply_ready = mock.external_reply_ready.clone();
    let (app, platform) = build_app(cfg, mock).await;
    seed_entry(
        &app,
        crate::config::SessionEntry {
            thread_key: crate::config::ThreadKey::new("oc_group_1".into(), "oc_group_1".into()),
            session_id: "ses_ext".into(),
            directory: dir.path().to_string_lossy().to_string(),
            agent: None,
            model: None,
            auto_accept: false,
            topic_anchor: None,
            topic_root: None,
            variant: None,
        },
    )
    .await;
    // The watermark sits before the external message, so it is "new".
    app.external
        .last_user_msg_epoch
        .lock()
        .await
        .insert("ses_ext".into(), now - 60_000);
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
    reply_ready.store(true, std::sync::atomic::Ordering::SeqCst);
    tokio::time::sleep(std::time::Duration::from_millis(2500)).await;

    let calls = platform.calls.lock().await.clone();
    let card = calls
        .iter()
        .rev()
        .find_map(|c| match c {
            PlatformCall::UpdateMessage { card, .. } => Some(card.clone()),
            _ => None,
        })
        .expect("the notification card must be updated in place");
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
    let user_message = index_of("OpenChamber 里发的消息");
    let reasoning = index_of("推理过程");
    let reply = index_of("目录里有 src。");
    assert!(
        user_message < reasoning && reasoning < reply,
        "the user's message must stay above the streamed reply: {}",
        card
    );
}

/// ADR-0041 / ADR-0017 parity: while a Pending Session supersedes the active
/// session, the external poller stops following the old session and drops its
/// Sync Watermark. Switching back re-baselines silently — the external message
/// received while the pending was declared is marked read, never replayed.
/// This is exactly the eager `/new` behaviour before Lazy Session Creation.
#[tokio::test]
async fn new_pending_stops_syncing_and_switch_back_resyncs_silently() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let mut mock = MockBackend::new(realistic_parts());
    // An external message on the superseded session, written after /new.
    mock.external_message_for("ses_old", "离开期间的外部消息");
    // The /switch back resolves through the shared session list.
    mock.session_list = vec![list_session("ses_old", "旧会话", "/work/proj", 100)];
    let (app, platform) = build_app(cfg, mock).await;
    let key = crate::config::ThreadKey::new("chat_1".into(), "chat_1".into());
    seed_entry(
        &app,
        crate::config::SessionEntry::new(key.clone(), "ses_old", "/work/proj"),
    )
    .await;
    // A watermark from before /new: the poller must drop it while the pending
    // supersedes the session.
    app.external
        .last_user_msg_epoch
        .lock()
        .await
        .insert("ses_old".into(), chrono::Utc::now().timestamp_millis());

    send_command_in(
        &app,
        "/new",
        key.clone(),
        "msg_new",
        crate::config::ConversationKind::P2p,
    )
    .await;
    assert!(
        app.sessions.lock().await.get_active(&key).is_none(),
        "the pending means no active session"
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

    // No notification for the old session, and its watermark is gone.
    let calls = platform.calls.lock().await.clone();
    assert!(
        !calls.iter().any(|c| match c {
            PlatformCall::SendCard { card, .. } | PlatformCall::ReplyCard { card, .. } => {
                card_text(card).contains("有新消息")
            }
            _ => false,
        }),
        "the superseded session must not be notified: {calls:?}"
    );
    assert!(
        !app.external
            .last_user_msg_epoch
            .lock()
            .await
            .contains_key("ses_old"),
        "the superseded session's watermark is dropped"
    );

    // Switch back: the pending is replaced, and the first poll re-baselines
    // silently instead of replaying the stale external message.
    send_command_in(
        &app,
        "/switch ses_old",
        key.clone(),
        "msg_switch",
        crate::config::ConversationKind::P2p,
    )
    .await;
    assert_eq!(
        app.sessions.lock().await.get_active(&key).unwrap().session_id,
        "ses_old",
        "switching back makes the old session active again"
    );

    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    let calls = platform.calls.lock().await.clone();
    assert!(
        !calls.iter().any(|c| match c {
            PlatformCall::SendCard { card, .. } | PlatformCall::ReplyCard { card, .. } => {
                card_text(card).contains("有新消息")
            }
            _ => false,
        }),
        "the reactivated session must re-sync silently: {calls:?}"
    );
    assert!(
        app.external
            .last_user_msg_epoch
            .lock()
            .await
            .contains_key("ses_old"),
        "the reactivated session re-records its watermark"
    );
}
