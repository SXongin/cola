use crate::backend::{
    ContentBlock, FinishReason, MessageId, MessageRole, Part, ReasoningPart, SessionTranscript, StepFinish,
    StepStart, ToolCall, ToolIdentity, ToolOutput, ToolStatus, TranscriptMessage, TurnAnchor,
};
use crate::bridge::test_support::*;

/// A user message's typed view; the Session Transcript's newest-user projection
/// decides the external message from it.
fn user(id: &str, created: i64, message_text: &str) -> TranscriptMessage {
    typed_message(
        id,
        MessageRole::User,
        Some(created),
        vec![text_part(message_text)],
    )
}

/// An assistant message whose terminal `step-finish` completes the turn.
fn finished(id: &str, created: i64, reason: FinishReason) -> TranscriptMessage {
    typed_message(
        id,
        MessageRole::Assistant,
        Some(created),
        vec![Part::StepFinish(StepFinish { reason })],
    )
}

/// The Turn anchor a typed user message would carry, for direct
/// `start_reply_render` calls (message identity + server time, one fact).
fn anchor(created_ms: i64) -> TurnAnchor {
    TurnAnchor {
        message_id: MessageId::new(format!("msg_user_{created_ms}")),
        created_ms,
    }
}

/// An assistant turn whose only moving part is a running tool call's output:
/// a panel revision every snapshot, with no new text part.
fn running_tool_reply(created: i64, output: &str) -> TranscriptMessage {
    typed_message(
        "msg_ext_assist",
        MessageRole::Assistant,
        Some(created),
        vec![Part::Tool(ToolCall {
            identity: ToolIdentity {
                name: "shell".into(),
                call_id: "call_ext".into(),
            },
            status: ToolStatus::Running,
            started_at: Some(created),
            input: None,
            metadata: None,
            output: ToolOutput {
                raw: None,
                blocks: vec![ContentBlock::Text(output.into())],
                error: None,
            },
        })],
    )
}

/// An assistant turn carrying a live `subagent` panel for `child`: the panel
/// whose child liveness every render gathers (spec #501).
fn live_subagent_reply(created: i64, child: &str) -> TranscriptMessage {
    subagent_reply(created, child, ToolStatus::Running)
}

/// [`live_subagent_reply`]'s settled shape — the panel the turn ends with.
fn settled_subagent_reply(created: i64, child: &str, text: &str) -> TranscriptMessage {
    let mut message = subagent_reply(created, child, ToolStatus::Completed);
    message.parts.push(text_part(text));
    message.parts.push(Part::StepFinish(StepFinish {
        reason: FinishReason::Stop,
    }));
    message
}

fn subagent_reply(created: i64, child: &str, status: ToolStatus) -> TranscriptMessage {
    typed_message(
        "msg_ext_assist",
        MessageRole::Assistant,
        Some(created),
        vec![Part::Tool(ToolCall {
            identity: ToolIdentity {
                name: "subagent".into(),
                call_id: "call_sub".into(),
            },
            status,
            started_at: Some(created),
            input: None,
            metadata: Some(serde_json::json!({ "sessionId": child })),
            output: ToolOutput {
                raw: None,
                blocks: vec![],
                error: None,
            },
        })],
    )
}

fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

#[tokio::test]
async fn external_message_from_shared_store_notifies_feishu() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let mut mock = MockBackend::new(realistic_parts());
    // The external message is scripted as a typed transcript: the poller's
    // newest-user/preview read must come from its projection (spec #332).
    mock.given_transcript(
        "ses_ext",
        vec![SessionTranscript::new(vec![user(
            "msg_ext_user",
            now_ms(),
            "OpenChamber 里发的消息",
        )])],
    );
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
            let _ = app.external.poll_loop(&app.flow_handles()).await;
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

/// A hung per-session message read (a half-open connection left by a server
/// restart) must not freeze the external poller forever: the read is
/// bounded, so a later poll still notifies about the external message.
#[tokio::test]
async fn external_poller_recovers_when_messages_hangs() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let mut mock = MockBackend::new(realistic_parts());
    mock.given_transcript(
        "ses_ext",
        vec![SessionTranscript::new(vec![user(
            "msg_ext_user",
            now_ms(),
            "OpenChamber 里发的消息",
        )])],
    );
    // The first per-session read hangs forever, like a request in flight
    // when the server was SIGTERM'd; later calls serve normally.
    mock.hang_transcript_reads(1);
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
            let _ = app.external.poll_loop(&app.flow_handles()).await;
        }
    });

    // Without a bound on the per-session read the poller sits on the first
    // hung call forever and the notification never arrives.
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

/// Completion detection reads the Session Transcript's turn projection: the
/// scripted transcript's assistant message finishes the turn, so the card must
/// be finalized as Done.
#[tokio::test]
async fn external_reply_completion_comes_from_the_transcript() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let now = now_ms();
    // The typed transcript answers the external message and finishes the turn.
    let mut mock = MockBackend::new(Vec::new());
    mock.given_transcript(
        "ses_ext",
        vec![SessionTranscript::new(vec![
            user("msg_ext_user", now - 30_000, "OpenChamber 里发的消息"),
            finished("msg_ext_assist", now - 29_000, FinishReason::Stop),
        ])],
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
    // The watermark sits before the external message, so it is "new".
    app.external
        .last_user_msg_epoch
        .lock()
        .await
        .insert("ses_ext".into(), now - 60_000);
    app.external
        .poll_interval_ms
        .store(50, std::sync::atomic::Ordering::Relaxed);
    app.external
        .render_poll_ms
        .store(5, std::sync::atomic::Ordering::Relaxed);
    tokio::spawn({
        let app = app.clone();
        async move {
            let _ = app.external.poll_loop(&app.flow_handles()).await;
        }
    });
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;

    let calls = platform.calls.lock().await.clone();
    let updates: Vec<serde_json::Value> = calls
        .iter()
        .filter_map(|c| match c {
            PlatformCall::UpdateMessage { card, .. } => Some(card.clone()),
            _ => None,
        })
        .collect();
    let last = updates.last().expect("the card must be updated at least once");
    let done_header = last["header"]["title"]["content"].as_str().unwrap_or("");
    assert!(
        done_header.contains("完成") || done_header.contains("✓"),
        "the transcript's terminal finish must finalize the card as Done, header: {}",
        done_header
    );
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
    mock.given_transcript(
        "ses_ext",
        vec![SessionTranscript::new(vec![user(
            "msg_cola_mock_user",
            now_ms(),
            "可以把我本地的 openchamber serve 杀掉吗？",
        )])],
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
            let _ = app.external.poll_loop(&app.flow_handles()).await;
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
    let now = now_ms();
    // cola's own round, then a genuine external message after it.
    mock.given_transcript(
        "ses_ext",
        vec![SessionTranscript::new(vec![
            user("msg_cola_mock_user", now - 20_000, "cola 自己的一轮"),
            user("msg_ext_user", now - 10_000, "OpenChamber 后来发的消息"),
        ])],
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
            let _ = app.external.poll_loop(&app.flow_handles()).await;
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
    mock.given_transcript(
        "ses_historical",
        vec![SessionTranscript::new(vec![user(
            "msg_ext_user",
            now_ms(),
            "历史会话的外部消息",
        )])],
    );
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
            let _ = app.external.poll_loop(&app.flow_handles()).await;
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
    mock.given_transcript(
        "ses_old",
        vec![SessionTranscript::new(vec![user(
            "msg_ext_user",
            now_ms(),
            "离开期间的外部消息",
        )])],
    );
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
            let _ = app.external.poll_loop(&app.flow_handles()).await;
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
    mock.given_transcript(
        "ses_ext",
        vec![SessionTranscript::new(vec![user(
            "msg_ext_user",
            now_ms(),
            "话题里的外部消息",
        )])],
    );
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
            let _ = app.external.poll_loop(&app.flow_handles()).await;
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
    mock.external_message("OpenChamber 里发的消息");
    // OpenCode's reply to that message: reasoning → tool → text → stop.
    let reply_ready = mock.external_reply(vec![
        Part::StepStart(StepStart),
        Part::Reasoning(ReasoningPart {
            text: "我来看看目录。".into(),
            started_at: None,
        }),
        tool_part(
            "bash",
            "call_1",
            ToolStatus::Completed,
            serde_json::json!({ "command": "ls" }),
            "src",
        ),
        text_part("目录里有 src。"),
        Part::StepFinish(StepFinish {
            reason: FinishReason::Stop,
        }),
    ]);
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
            let _ = app.external.poll_loop(&app.flow_handles()).await;
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
    mock.external_message("外部消息");
    let (app, _platform) = build_app(cfg, mock).await;

    // Arm once for the first external message.
    app.external
        .start_reply_render(&app.flow_handles(), "ses_ext", &anchor(1000), "n1", "第一条")
        .await;
    {
        let cards = app.cards_handle();
        assert_eq!(
            Turn::armed_turn_anchor(&cards, "ses_ext").await,
            Some(anchor(1000))
        );
        assert_eq!(Turn::reply_target(&cards, "ses_ext").await.as_deref(), Some("n1"));
        assert_eq!(
            Turn::card_message_id(&cards, "ses_ext").await.as_deref(),
            Some("n1")
        );
    }

    // Re-arming for the SAME message (e.g. a duplicate poll) is a no-op:
    // the armed card id and anchor must not be clobbered.
    app.external
        .start_reply_render(&app.flow_handles(), "ses_ext", &anchor(1000), "n1b", "第一条")
        .await;
    {
        let cards = app.cards_handle();
        assert_eq!(
            Turn::armed_turn_anchor(&cards, "ses_ext").await,
            Some(anchor(1000))
        );
        assert_eq!(Turn::reply_target(&cards, "ses_ext").await.as_deref(), Some("n1"));
        assert_eq!(
            Turn::card_message_id(&cards, "ses_ext").await.as_deref(),
            Some("n1")
        );
    }

    // A NEWER external message replaces the armed renderer (its card id and
    // anchor move to the new notification).
    app.external
        .start_reply_render(&app.flow_handles(), "ses_ext", &anchor(2000), "n2", "第二条")
        .await;
    {
        let cards = app.cards_handle();
        assert_eq!(
            Turn::armed_turn_anchor(&cards, "ses_ext").await,
            Some(anchor(2000))
        );
        assert_eq!(Turn::reply_target(&cards, "ses_ext").await.as_deref(), Some("n2"));
        assert_eq!(
            Turn::card_message_id(&cards, "ses_ext").await.as_deref(),
            Some("n2")
        );
    }
}

/// Two user messages can share a millisecond. The armed-renderer guard compares
/// the FULL anchor (message id + server time), so a DIFFERENT message in the
/// same millisecond is a different turn and must replace the armed renderer —
/// not be suppressed as a duplicate of it (the old time-only comparison did
/// suppress it, silently dropping the new message's reply).
#[tokio::test]
async fn external_reply_render_guard_distinguishes_same_millisecond_messages() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let mut mock = MockBackend::new(realistic_parts());
    // No reply parts: the armed loops just idle/exit; we assert state only.
    mock.external_message("外部消息");
    let (app, _platform) = build_app(cfg, mock).await;

    // Arm once for the first external message at T.
    app.external
        .start_reply_render(&app.flow_handles(), "ses_ext", &anchor(1000), "n1", "第一条")
        .await;
    let cards = app.cards_handle();
    assert_eq!(
        Turn::armed_turn_anchor(&cards, "ses_ext").await,
        Some(anchor(1000))
    );

    // A DIFFERENT user message created in the same millisecond: the guard must
    // re-arm (the identity differs), or the new message's reply renders
    // nowhere.
    let same_ms = TurnAnchor {
        message_id: MessageId::new("msg_user_other"),
        created_ms: 1000,
    };
    app.external
        .start_reply_render(&app.flow_handles(), "ses_ext", &same_ms, "n2", "第二条")
        .await;
    assert_eq!(
        Turn::armed_turn_anchor(&cards, "ses_ext").await,
        Some(same_ms),
        "a same-millisecond message with a different id must replace the armed renderer"
    );
    assert_eq!(Turn::reply_target(&cards, "ses_ext").await.as_deref(), Some("n2"));
    assert_eq!(
        Turn::card_message_id(&cards, "ses_ext").await.as_deref(),
        Some("n2")
    );
}

/// The external-reply renderer's idle-bound branch: a partial reply is
/// rendered but the model never produces anything more, so the loop finalizes
/// the card as Done when the (injected, tiny) idle bound elapses — the card
/// never sits on an eternal spinner. Exercises the bound with millisecond
/// fields instead of the production 10-minute default.
#[tokio::test]
async fn external_reply_render_times_out_and_finalizes_partial_content() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let mut mock = MockBackend::new(realistic_parts());
    mock.external_message("OpenChamber 里发的消息");
    // A partial reply: reasoning + text, but NO step-finish — the turn never
    // completes, so the loop must be stopped by the timeout.
    mock.external_reply(vec![
        Part::StepStart(StepStart),
        Part::Reasoning(ReasoningPart {
            text: "我在想。".into(),
            started_at: None,
        }),
        text_part("部分回答。"),
    ])
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
        .render_idle_timeout_ms
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
            let _ = app.external.poll_loop(&app.flow_handles()).await;
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
        "the idle bound must finalize the card as Done, header: {}",
        done_header
    );
}

/// #457: the external renderer's bound is an IDLE bound, not a total deadline.
/// A run that keeps gaining parts resets the clock on every poll that renders
/// content, so production past the old total bound keeps streaming. Scripted
/// small: the idle bound is 60 ms while the scripted turn keeps producing for
/// ~120 ms (one snapshot per 5 ms poll), so the last part is only reachable
/// with the renewal.
#[tokio::test]
async fn external_reply_render_renews_its_idle_bound_while_parts_arrive() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let mut mock = MockBackend::new(realistic_parts());
    // Every snapshot is the same external turn with one more text part; the
    // loop consumes one snapshot per poll, so production outlives the bound.
    let snapshots: Vec<SessionTranscript> = (1..=24)
        .map(|i| {
            let mut messages = vec![user("msg_ext_user", 2_000_000, "OpenChamber 里发的消息")];
            let parts: Vec<Part> = (1..=i).map(|j| text_part(&format!("第{j}段。"))).collect();
            messages.push(typed_message(
                "msg_ext_assist",
                MessageRole::Assistant,
                Some(2_001_000 + i),
                parts,
            ));
            SessionTranscript::new(messages)
        })
        .collect();
    mock.given_transcript("ses_ext", snapshots);
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
    app.external
        .render_poll_ms
        .store(5, std::sync::atomic::Ordering::Relaxed);
    app.external
        .render_idle_timeout_ms
        .store(60, std::sync::atomic::Ordering::Relaxed);

    app.external
        .start_reply_render(
            &app.flow_handles(),
            "ses_ext",
            &anchor(2_000_000),
            "msg_sent",
            "OpenChamber 里发的消息",
        )
        .await;

    // Only the renewal can carry the renderer past its original 60 ms bound
    // to the scripted last part.
    wait_for_card_update(&platform, "the last produced part", CardUpdates::Latest, |card| {
        card_text(card).contains("第24段")
    })
    .await;
    // The script stops producing after that: the idle bound then closes the
    // card Done under the partial-content rule.
    wait_for_card_update(&platform, "the idle bound's Done", CardUpdates::Latest, |card| {
        card_header(card).contains("完成") || card_header(card).contains("✓")
    })
    .await;
}

/// #457's safety net kept: a message that never produces anything still ends
/// at the idle bound, and the renderer then leaves the 有新消息 notification
/// card exactly as it was — no content means no finalization. The exit is
/// proven directly: output injected after the bound never reaches the card.
#[tokio::test]
async fn external_reply_render_leaves_the_notification_card_when_nothing_produces() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let mut mock = MockBackend::new(realistic_parts());
    // The external message with NO reply scripted: no assistant turn ever
    // appears, so the renderer has nothing to render.
    mock.external_message("OpenChamber 里发的消息");
    let backend = Arc::new(mock);
    let platform = Arc::new(RecordingPlatform::new());
    let app = Arc::new(App::new(cfg, backend.clone(), platform.clone()).unwrap());
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
    app.external
        .poll_interval_ms
        .store(50, std::sync::atomic::Ordering::Relaxed);
    app.external
        .render_poll_ms
        .store(5, std::sync::atomic::Ordering::Relaxed);
    app.external
        .render_idle_timeout_ms
        .store(30, std::sync::atomic::Ordering::Relaxed);
    let watermark = chrono::Utc::now().timestamp_millis() - 60_000;
    app.external
        .last_user_msg_epoch
        .lock()
        .await
        .insert("ses_ext".into(), watermark);

    tokio::spawn({
        let app = app.clone();
        async move {
            let _ = app.external.poll_loop(&app.flow_handles()).await;
        }
    });
    // The renderer only arms after the poller's first pass; wait for the arm
    // (bounded), then leave ample time for the 30 ms idle bound to run out.
    let armed = async {
        loop {
            if Turn::armed_turn_anchor(&app.cards_handle(), "ses_ext")
                .await
                .is_some()
            {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
    };
    tokio::time::timeout(std::time::Duration::from_secs(5), armed)
        .await
        .expect("the poller must arm the renderer for the external message");
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;

    // The renderer must have STOPPED at the bound — proven directly, not by
    // poll cadence: a part arriving now would be rendered by a live loop (the
    // arm guard keeps the poller from re-arming), so the card must never show
    // it.
    let anchor = Turn::armed_turn_anchor(&app.cards_handle(), "ses_ext")
        .await
        .expect("the renderer armed");
    backend
        .given_transcript_after_build(
            "ses_ext",
            vec![SessionTranscript::new(vec![
                typed_message(
                    anchor.message_id.as_str(),
                    MessageRole::User,
                    Some(anchor.created_ms),
                    vec![text_part("OpenChamber 里发的消息")],
                ),
                typed_message(
                    "msg_ext_assist",
                    MessageRole::Assistant,
                    Some(anchor.created_ms + 1_000),
                    vec![text_part("迟到的输出。")],
                ),
            ])],
        )
        .await;
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;

    let calls = platform.calls.lock().await.clone();
    let notify = calls.iter().find_map(|c| match c {
        PlatformCall::SendCard { card, .. } if card_text(card).contains("有新消息") => Some(card.clone()),
        _ => None,
    });
    let notify = notify.expect("the external message produces the notification card");
    assert!(
        card_text(&notify).contains("OpenChamber 里发的消息"),
        "the notification previews the message: {notify}"
    );
    // No update may carry the late part or turn the silent card terminal: the
    // renderer stopped at the bound and the notification stays.
    for card in calls.iter().filter_map(|c| match c {
        PlatformCall::UpdateMessage { card, .. } => Some(card),
        _ => None,
    }) {
        assert!(
            !card_text(card).contains("迟到的输出"),
            "a stopped renderer must not render content arriving after its bound: {card}"
        );
        let header = card["header"]["title"]["content"].as_str().unwrap_or("");
        assert!(
            !header.contains("完成") && !header.contains("✓") && !header.contains("出错"),
            "a run that never produced must not finalize: {card}"
        );
        assert!(
            card_text(card).contains("OpenChamber 里发的消息"),
            "the notification stays intact: {card}"
        );
    }
}

/// #457's tool-revision renewal: a panel whose call gains output is progress
/// even when no new text part arrives. Scripted small: the idle bound is 60 ms
/// while the tool keeps gaining output for ~120 ms, so the last revision is
/// only reachable with the renewal.
#[tokio::test]
async fn external_reply_render_renews_on_tool_panel_revisions() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let mut mock = MockBackend::new(realistic_parts());
    let snapshots: Vec<SessionTranscript> = (1..=24)
        .map(|i| {
            SessionTranscript::new(vec![
                user("msg_ext_user", 2_000_000, "OpenChamber 里发的消息"),
                running_tool_reply(2_001_000, &format!("工具输出第{i}段。")),
            ])
        })
        .collect();
    mock.given_transcript("ses_ext", snapshots);
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
    app.external
        .render_poll_ms
        .store(5, std::sync::atomic::Ordering::Relaxed);
    app.external
        .render_idle_timeout_ms
        .store(60, std::sync::atomic::Ordering::Relaxed);

    app.external
        .start_reply_render(
            &app.flow_handles(),
            "ses_ext",
            &anchor(2_000_000),
            "msg_sent",
            "OpenChamber 里发的消息",
        )
        .await;

    // Only the renewal can carry the renderer to the scripted last revision.
    wait_for_card_update(&platform, "the last tool revision", CardUpdates::Latest, |card| {
        card_text(card).contains("工具输出第24段")
    })
    .await;
    wait_for_card_update(&platform, "the idle bound's Done", CardUpdates::Latest, |card| {
        card_header(card).contains("完成") || card_header(card).contains("✓")
    })
    .await;
}

/// #457: failed transcript reads are no progress either — the renderer must
/// idle out on them. A part arriving after the bound (reads healed) would be
/// rendered by a still-polling loop, so the card never showing it, with no
/// terminal stamp, is the exit.
#[tokio::test]
async fn external_reply_render_idles_out_when_reads_fail() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let mut mock = MockBackend::new(realistic_parts());
    mock.given_transcript(
        "ses_ext",
        vec![SessionTranscript::new(vec![user(
            "msg_ext_user",
            2_000_000,
            "OpenChamber 里发的消息",
        )])],
    );
    let backend = Arc::new(mock);
    let platform = Arc::new(RecordingPlatform::new());
    let app = Arc::new(App::new(cfg, backend.clone(), platform.clone()).unwrap());
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
    app.external
        .render_poll_ms
        .store(5, std::sync::atomic::Ordering::Relaxed);
    app.external
        .render_idle_timeout_ms
        .store(30, std::sync::atomic::Ordering::Relaxed);
    app.external
        .start_reply_render(
            &app.flow_handles(),
            "ses_ext",
            &anchor(2_000_000),
            "msg_sent",
            "OpenChamber 里发的消息",
        )
        .await;
    let anchor = Turn::armed_turn_anchor(&app.cards_handle(), "ses_ext")
        .await
        .expect("the renderer armed");

    backend.fail_transcript_for("ses_ext").await;
    // Well past the bound: every read failed, so the renderer must have given
    // up instead of polling the wedged Backend forever.
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    backend.heal_transcript("ses_ext").await;
    backend
        .given_transcript_after_build(
            "ses_ext",
            vec![SessionTranscript::new(vec![
                typed_message(
                    anchor.message_id.as_str(),
                    MessageRole::User,
                    Some(anchor.created_ms),
                    vec![text_part("OpenChamber 里发的消息")],
                ),
                typed_message(
                    "msg_ext_assist",
                    MessageRole::Assistant,
                    Some(anchor.created_ms + 1_000),
                    vec![text_part("迟到的输出。")],
                ),
            ])],
        )
        .await;
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;

    let calls = platform.calls.lock().await.clone();
    for card in calls.iter().filter_map(|c| match c {
        PlatformCall::UpdateMessage { card, .. } => Some(card),
        _ => None,
    }) {
        assert!(
            !card_text(card).contains("迟到的输出"),
            "a renderer that idled out on failed reads must not render a later part: {card}"
        );
        let header = card["header"]["title"]["content"].as_str().unwrap_or("");
        assert!(
            !header.contains("完成") && !header.contains("✓") && !header.contains("出错"),
            "an unobservable run must not finalize: {card}"
        );
    }
    assert_ne!(
        Turn::card_state(&app.cards_handle(), "ses_ext").await,
        Some(crate::feishu::card::CardState::Done),
        "the silent card must not be stamped Done"
    );
}

/// #457's liveness renewal: a live subagent panel whose child keeps working is
/// progress even while the parent transcript is byte-identical. Scripted
/// small: the child advances for ~500 ms against a 200 ms idle bound, and the
/// parent turn completes only at the end — its final text is reachable only
/// with the renewal, since a total deadline would cut the loop at the bound
/// with the parent unchanged.
#[tokio::test]
async fn external_reply_render_renews_on_child_liveness() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let mut mock = MockBackend::new(realistic_parts());
    let live_parent = || {
        SessionTranscript::new(vec![
            user("msg_ext_user", 2_000_000, "OpenChamber 里发的消息"),
            live_subagent_reply(2_001_000, "ses_child"),
        ])
    };
    let mut parents: Vec<SessionTranscript> = (0..100).map(|_| live_parent()).collect();
    parents.push(SessionTranscript::new(vec![
        user("msg_ext_user", 2_000_000, "OpenChamber 里发的消息"),
        settled_subagent_reply(2_050_000, "ses_child", "父回合的最后输出。"),
    ]));
    mock.given_transcript("ses_ext", parents);
    // The child's transcript advances on every liveness read.
    let children: Vec<SessionTranscript> = (0..200)
        .map(|i| {
            SessionTranscript::new(vec![typed_message(
                "msg_child_assist",
                MessageRole::Assistant,
                Some(2_100_000 + i * 1_000),
                vec![text_part("子任务进行中。")],
            )])
        })
        .collect();
    mock.given_transcript("ses_child", children);
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
    app.external
        .render_poll_ms
        .store(5, std::sync::atomic::Ordering::Relaxed);
    app.external
        .render_idle_timeout_ms
        .store(200, std::sync::atomic::Ordering::Relaxed);

    app.external
        .start_reply_render(
            &app.flow_handles(),
            "ses_ext",
            &anchor(2_000_000),
            "msg_sent",
            "OpenChamber 里发的消息",
        )
        .await;

    wait_for_card_update(
        &platform,
        "the parent turn's true end",
        CardUpdates::Latest,
        |card| card_text(card).contains("父回合的最后输出"),
    )
    .await;
    wait_for_card_update(
        &platform,
        "the finalized liveness-renewed card",
        CardUpdates::Latest,
        |card| card_header(card).contains("完成") || card_header(card).contains("✓"),
    )
    .await;
}

/// #457: a transcript read that never resolves must not park the loop past
/// its idle bound — the read is bounded, so a half-open connection idles out
/// like any failed read. A part injected after the bound (gate released) must
/// never render.
#[tokio::test]
async fn external_reply_render_idles_out_when_reads_hang() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let mut mock = MockBackend::new(realistic_parts());
    mock.given_transcript(
        "ses_ext",
        vec![SessionTranscript::new(vec![user(
            "msg_ext_user",
            2_000_000,
            "OpenChamber 里发的消息",
        )])],
    );
    let backend = Arc::new(mock);
    let platform = Arc::new(RecordingPlatform::new());
    let app = Arc::new(App::new(cfg, backend.clone(), platform.clone()).unwrap());
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
    app.external
        .render_poll_ms
        .store(5, std::sync::atomic::Ordering::Relaxed);
    app.external
        .request_timeout_ms
        .store(10, std::sync::atomic::Ordering::Relaxed);
    app.external
        .render_idle_timeout_ms
        .store(30, std::sync::atomic::Ordering::Relaxed);

    // Park every transcript read on the gate BEFORE the renderer polls.
    let gate = backend.hold_transcripts();
    app.external
        .start_reply_render(
            &app.flow_handles(),
            "ses_ext",
            &anchor(2_000_000),
            "msg_sent",
            "OpenChamber 里发的消息",
        )
        .await;
    let anchor = Turn::armed_turn_anchor(&app.cards_handle(), "ses_ext")
        .await
        .expect("the renderer armed");

    // Well past the bound: every read hit its 10 ms timeout, so the renderer
    // must have given up rather than park on the half-open connection.
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    gate.add_permits(64);
    backend
        .given_transcript_after_build(
            "ses_ext",
            vec![SessionTranscript::new(vec![
                typed_message(
                    anchor.message_id.as_str(),
                    MessageRole::User,
                    Some(anchor.created_ms),
                    vec![text_part("OpenChamber 里发的消息")],
                ),
                typed_message(
                    "msg_ext_assist",
                    MessageRole::Assistant,
                    Some(anchor.created_ms + 1_000),
                    vec![text_part("迟到的输出。")],
                ),
            ])],
        )
        .await;
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;

    let calls = platform.calls.lock().await.clone();
    for card in calls.iter().filter_map(|c| match c {
        PlatformCall::UpdateMessage { card, .. } => Some(card),
        _ => None,
    }) {
        assert!(
            !card_text(card).contains("迟到的输出"),
            "a renderer that idled out on hung reads must not render a later part: {card}"
        );
    }
    assert_ne!(
        Turn::card_state(&app.cards_handle(), "ses_ext").await,
        Some(crate::feishu::card::CardState::Done),
        "the silent card must not be stamped Done"
    );
}

/// #457: a failed read must not let this renderer finalize SOMEONE ELSE'S
/// card. Renderer A's reads fail while a newer arm replaces its accumulator;
/// A's bound firing must exit silently — the replacement card, already
/// carrying content, stays live.
#[tokio::test]
async fn external_reply_render_does_not_finalize_a_replacement_card() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let mut mock = MockBackend::new(realistic_parts());
    mock.given_transcript(
        "ses_ext",
        vec![SessionTranscript::new(vec![user(
            "msg_ext_user",
            2_000_000,
            "OpenChamber 里发的消息",
        )])],
    );
    let backend = Arc::new(mock);
    let platform = Arc::new(RecordingPlatform::new());
    let app = Arc::new(App::new(cfg, backend.clone(), platform.clone()).unwrap());
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
    app.external
        .render_poll_ms
        .store(5, std::sync::atomic::Ordering::Relaxed);
    app.external
        .render_idle_timeout_ms
        .store(30, std::sync::atomic::Ordering::Relaxed);

    app.external
        .start_reply_render(
            &app.flow_handles(),
            "ses_ext",
            &anchor(2_000_000),
            "msg_sent_a",
            "OpenChamber 里发的消息",
        )
        .await;
    backend.fail_transcript_for("ses_ext").await;

    // A newer external message replaces the accumulator while A's reads fail,
    // and its card already carries content.
    let anchor_b = anchor(3_000_000);
    Turn::arm_external_render(
        &app.cards_handle(),
        "ses_ext",
        "msg_sent_b",
        &anchor_b,
        "sub",
        "/tmp/ext",
        None,
        Some("👤 新消息"),
    )
    .await;
    {
        let flow = app.flow_handles();
        let transcript = SessionTranscript::new(vec![
            typed_message(
                anchor_b.message_id.as_str(),
                MessageRole::User,
                Some(anchor_b.created_ms),
                vec![text_part("新消息")],
            ),
            typed_message(
                "msg_ext_assist_b",
                MessageRole::Assistant,
                Some(anchor_b.created_ms + 1_000),
                vec![text_part("B 的进行中内容。")],
            ),
        ]);
        assert!(
            Turn::render_and_flush(
                &flow.cards,
                &flow.sessions,
                &flow.backend,
                &flow.requests,
                "ses_ext",
                &transcript,
            )
            .await
            .is_some(),
            "the replacement card renders content"
        );
    }

    // Let A's bound fire: it must leave the replacement card alone.
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;

    assert_eq!(
        Turn::card_state(&app.cards_handle(), "ses_ext").await,
        Some(crate::feishu::card::CardState::Streaming),
        "the replacement card must stay live — A must not finalize it"
    );
    let calls = platform.calls.lock().await.clone();
    for card in calls.iter().filter_map(|c| match c {
        PlatformCall::UpdateMessage { card, .. } => Some(card),
        _ => None,
    }) {
        let header = card["header"]["title"]["content"].as_str().unwrap_or("");
        assert!(
            !header.contains("完成") && !header.contains("✓") && !header.contains("出错"),
            "A's bound must not stamp a terminal on the replacement card: {card}"
        );
    }
}

/// #451: a Feishu Supplement landing during an external follow is a
/// cola-authored message merged into the SAME run — only a newer EXTERNAL
/// message is a turn boundary (ADR-0028). Before the fix the follow's renderer
/// exited silently on the newest-user check; the Wake step then declined (the
/// chain was still owned) and the reap deferred (the card was still held), so
/// the card stranded while the run kept working. The follow must keep
/// streaming the continuation into the same card until the true end.
#[tokio::test]
async fn external_render_follows_a_supplement_into_the_continuation() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let mut mock = MockBackend::new(realistic_parts());
    // A fixed server time so the supplement is unambiguously newer than the
    // external turn's anchor.
    mock.external_message("OpenChamber 里发的消息");
    mock.external_message_created_at(2_000_000);
    mock.external_reply(vec![text_part("前半回答。")])
        .store(true, std::sync::atomic::Ordering::SeqCst);
    // Keep the concrete backend: the test swaps its scripted transcript
    // mid-life to land the supplement and the continuation.
    let backend = Arc::new(mock);
    let platform = Arc::new(RecordingPlatform::new());
    let app = Arc::new(App::new(cfg, backend.clone(), platform.clone()).unwrap());
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
    app.external
        .render_poll_ms
        .store(5, std::sync::atomic::Ordering::Relaxed);
    // The production 10-minute cap stays out of the way: the terminal
    // step-finish is the true end this test drives.
    app.external
        .render_idle_timeout_ms
        .store(3_600_000, std::sync::atomic::Ordering::Relaxed);

    let anchor = TurnAnchor {
        message_id: MessageId::new("msg_ext_user"),
        created_ms: 2_000_000,
    };
    app.external
        .start_reply_render(
            &app.flow_handles(),
            "ses_ext",
            &anchor,
            "msg_sent",
            "OpenChamber 里发的消息",
        )
        .await;
    wait_for_card_update(
        &platform,
        "the pre-supplement reply",
        CardUpdates::Latest,
        |card| card_text(card).contains("前半回答"),
    )
    .await;

    // The Supplement lands (a cola-authored user message merged into THIS
    // run) and the run keeps working: more output, no terminal finish yet.
    // Consuming THIS read is the render the old boundary check killed.
    let external = || {
        typed_message(
            "msg_ext_user",
            MessageRole::User,
            Some(2_000_000),
            vec![text_part("OpenChamber 里发的消息")],
        )
    };
    let first_reply = || {
        typed_message(
            "msg_ext_assist",
            MessageRole::Assistant,
            Some(2_001_000),
            vec![text_part("前半回答。")],
        )
    };
    let supplement = || {
        typed_message(
            "msg_cola_supplement",
            MessageRole::User,
            Some(2_002_000),
            vec![text_part("补充")],
        )
    };
    let mid = || {
        typed_message(
            "msg_ext_assist_2",
            MessageRole::Assistant,
            Some(2_003_000),
            vec![text_part("补充后的中间内容。")],
        )
    };
    backend
        .given_transcript_after_build(
            "ses_ext",
            vec![SessionTranscript::new(vec![
                external(),
                first_reply(),
                supplement(),
                mid(),
            ])],
        )
        .await;
    wait_for_card_update(
        &platform,
        "the merged continuation",
        CardUpdates::Latest,
        |card| card_text(card).contains("补充后的中间内容"),
    )
    .await;

    // The merged run finishes: the follow must still be attached, render the
    // rest and end the card Done.
    let tail = || {
        typed_message(
            "msg_ext_assist_3",
            MessageRole::Assistant,
            Some(2_004_000),
            vec![
                text_part("补充之后的回答。"),
                Part::StepFinish(StepFinish {
                    reason: FinishReason::Stop,
                }),
            ],
        )
    };
    backend
        .given_transcript_after_build(
            "ses_ext",
            vec![SessionTranscript::new(vec![
                external(),
                first_reply(),
                supplement(),
                mid(),
                tail(),
            ])],
        )
        .await;
    wait_for_card_update(
        &platform,
        "the continuation's true end",
        CardUpdates::Latest,
        |card| card_text(card).contains("补充之后的回答"),
    )
    .await;
    wait_for_card_update(
        &platform,
        "the finalized continuation",
        CardUpdates::Latest,
        |card| card_header(card).contains("完成") || card_header(card).contains("✓"),
    )
    .await;
    assert_eq!(
        Turn::card_state(&app.cards_handle(), "ses_ext").await,
        Some(crate::feishu::card::CardState::Done),
        "the follow rendered the merged run to its true end"
    );
}

/// A deliberate `/stop` on a live EXTERNAL run finalizes the notification card
/// ⏹ 已停止, never ✅ (#394) — the external renderer owns that card, so it must
/// hold up the "a render-owned card stamps the stop" invariant the command
/// relies on. A stale marker from before the run is dropped when the renderer
/// arms (the same rule a fresh Turn applies), or an earlier stop would end a
/// healthy new turn.
#[tokio::test]
async fn external_reply_render_honors_a_stop_and_drops_a_stale_marker() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let mut mock = MockBackend::new(realistic_parts());
    mock.external_message("OpenChamber 里发的消息");
    // A partial reply with NO step-finish: the turn never completes, so only
    // the stop can end the render (the production render timeout stays long).
    mock.external_reply(vec![
        Part::StepStart(StepStart),
        Part::Reasoning(ReasoningPart {
            text: "我在想。".into(),
            started_at: None,
        }),
        text_part("部分回答。"),
    ])
    .store(true, std::sync::atomic::Ordering::SeqCst);
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
    app.external
        .poll_interval_ms
        .store(50, std::sync::atomic::Ordering::Relaxed);
    app.external
        .render_poll_ms
        .store(5, std::sync::atomic::Ordering::Relaxed);
    let watermark = chrono::Utc::now().timestamp_millis() - 60_000;
    app.external
        .last_user_msg_epoch
        .lock()
        .await
        .insert("ses_ext".into(), watermark);

    // A stop from BEFORE this run: arming must drop it, or the renderer would
    // stamp ⏹ over a healthy new turn on its first tick.
    app.stopped_sessions.lock().await.insert("ses_ext".to_string());
    tokio::spawn({
        let app = app.clone();
        async move {
            let _ = app.external.poll_loop(&app.flow_handles()).await;
        }
    });
    wait_for_card_update(
        &platform,
        "the streamed external reply",
        CardUpdates::Latest,
        |card| card_text(card).contains("部分回答"),
    )
    .await;
    assert!(
        !app.stopped_sessions.lock().await.contains("ses_ext"),
        "arming a new external run clears a stale stop marker"
    );
    let running = platform.updated_cards().await.last().cloned().unwrap();
    assert!(
        !card_header(&running).contains("已停止"),
        "a stale marker must not end the new run: {running}"
    );

    // The operator stops THIS run: the renderer stamps the stop terminal.
    app.stopped_sessions.lock().await.insert("ses_ext".to_string());
    wait_for_card_update(
        &platform,
        "the external stop terminal",
        CardUpdates::Latest,
        |card| card_header(card).contains("已停止"),
    )
    .await;
    assert_eq!(
        Turn::card_state(&app.cards_handle(), "ses_ext").await,
        Some(crate::feishu::card::CardState::Stopped),
        "the external renderer's stop must land as Stopped, never Done"
    );
    let stopped = platform.updated_cards().await.last().cloned().unwrap();
    assert!(
        !card_header(&stopped).contains("完成"),
        "a deliberate stop is never ✅: {stopped}"
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
    mock.external_message("OpenChamber 里发的消息");
    // The external message was posted half a minute ago; the reply's parts
    // carry real server times after it but well before cola renders them.
    mock.external_message_created_at(now - 30_000);
    let reply_ready = mock.external_reply(vec![
        Part::StepStart(StepStart),
        Part::Reasoning(ReasoningPart {
            text: "我来看看目录。".into(),
            started_at: Some(now - 20_000),
        }),
        Part::Text(crate::backend::TextPart {
            text: "目录里有 src。".into(),
            started_at: Some(now - 18_000),
        }),
        Part::StepFinish(StepFinish {
            reason: FinishReason::Stop,
        }),
    ]);
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
            let _ = app.external.poll_loop(&app.flow_handles()).await;
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
    mock.given_transcript(
        "ses_old",
        vec![SessionTranscript::new(vec![user(
            "msg_ext_user",
            now_ms(),
            "离开期间的外部消息",
        )])],
    );
    // The /switch back resolves through the shared session list.
    mock.given_sessions(vec![list_session("ses_old", "旧会话", "/work/proj", 100)]);
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
            let _ = app.external.poll_loop(&app.flow_handles()).await;
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

/// A Session can move itself to another directory mid-life (#433): the sync
/// pass follows the server-reported location, so the directory-routed sweeps
/// (and the next turn's work context) target the new directory. The mapping's
/// other fields — the per-session override here — stay untouched.
#[tokio::test]
async fn session_sync_follows_a_moved_session_to_its_new_directory() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let mut mock = MockBackend::new(realistic_parts());
    // The shared store reports the session at its new location: an agent
    // created a git worktree and moved the session into it.
    mock.given_sessions(vec![list_session(
        "ses_moved",
        "移动到 worktree",
        "/work/.worktrees/zh-user-guide",
        100,
    )]);
    let (app, _platform) = build_app(cfg, mock).await;

    let key = crate::config::ThreadKey::new("oc_moved".into(), "oc_moved".into());
    let mut entry = crate::config::SessionEntry::new(key.clone(), "ses_moved", "/work/old");
    entry.model = Some("provider/model-a".into());
    seed_entry(&app, entry).await;
    // Baseline the watermark so the pass stays off the notify path.
    app.external
        .last_user_msg_epoch
        .lock()
        .await
        .insert("ses_moved".into(), now_ms());

    app.external
        .poll_interval_ms
        .store(20, std::sync::atomic::Ordering::Relaxed);
    tokio::spawn({
        let app = app.clone();
        async move {
            let _ = app.external.poll_loop(&app.flow_handles()).await;
        }
    });

    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(2);
    loop {
        let moved = app
            .sessions
            .lock()
            .await
            .directory_for_session("ses_moved")
            .as_deref()
            == Some("/work/.worktrees/zh-user-guide");
        if moved {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the sync pass must follow the server-reported location"
        );
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }

    let entry = app
        .sessions
        .lock()
        .await
        .entry_for_session("ses_moved")
        .cloned()
        .expect("still mapped");
    assert_eq!(
        entry.model.as_deref(),
        Some("provider/model-a"),
        "the move must not reset the per-session override"
    );
    assert_eq!(entry.thread_key, key);
    // The sweep's directory set follows: the old location is gone and the new
    // one is listed, so requests raised there are found.
    let dirs = app.sessions.lock().await.directories();
    assert!(
        dirs.contains(&"/work/.worktrees/zh-user-guide".to_string()),
        "the new directory must be swept: {dirs:?}"
    );
    assert!(
        !dirs.contains(&"/work/old".to_string()),
        "the old directory must be dropped: {dirs:?}"
    );
}
