//! Session-scoped logs (ADR-0048): the `capture_logs` test seam, the Turn's
//! trace — the `turn{session=… chat=… topic=…}` span, its one INFO start
//! anchor, and the separately-instrumented render poll — the waiting path's
//! `request`/`action` spans, and the background flows: the external poll and
//! its reply-render loop.

use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use serde_json::json;

use crate::bridge::test_support::*;
use crate::bridge::turn::{PromptContext, Turn};
use crate::config::{SessionEntry, ThreadKey};
use crate::opencode::types::{MessageInfo, MessageTime, SessionMessage};

/// A Backend message fixture for a scripted timeline.
fn msg(role: &str, id: &str, created: i64, parts: serde_json::Value) -> SessionMessage {
    SessionMessage {
        info: MessageInfo {
            id: id.into(),
            role: Some(role.into()),
            parent_id: None,
            time: Some(MessageTime {
                created,
                completed: Some(created),
            }),
            model_id: None,
            provider_id: None,
            tokens: None,
        },
        parts,
    }
}

/// The lobby ThreadKey: the thread id IS the chat id, i.e. not a topic.
fn lobby() -> ThreadKey {
    ThreadKey::new("oc_chat".into(), "oc_chat".into())
}

/// A Turn context for `ses_test` carrying the fixed `msg_cola_anchor` id, so a
/// scripted Backend timeline can name this turn's own user message.
fn prompt_context(thread_key: ThreadKey, text: &str) -> PromptContext {
    PromptContext {
        session_id: "ses_test".into(),
        thread_key,
        text: text.into(),
        message_id: "msg_1".into(),
        subtitle: "p2p".into(),
        existing_card_id: None,
        requester_open_id: None,
        is_group: false,
        cola_message_id: Some("msg_cola_anchor".into()),
        images: Vec::new(),
    }
}

/// The one INFO anchor per Turn carries the span's session/chat/topic fields,
/// the session's directory and the prompt's character count — never its body.
#[tokio::test]
async fn turn_start_anchor_carries_session_chat_topic_directory_and_prompt_chars() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let (app, _platform) = build_app(cfg, MockBackend::new(realistic_parts())).await;
    let topic = ThreadKey::new("oc_chat".into(), "omt_topic".into());
    seed_entry(
        &app,
        SessionEntry::new(topic.clone(), "ses_test", "/work/project"),
    )
    .await;

    // Four CJK characters: a byte length would read 12, so this pins `chars()`.
    let prompt = "分析目录";
    let (result, logs) =
        capture_logs(async { Turn::run(&app.turn_handles(), prompt_context(topic, prompt)).await }).await;
    result.unwrap();

    let anchor = line_with(&logs, "turn start:");
    assert!(
        anchor.contains("session=ses_test"),
        "the anchor carries the session: {anchor}"
    );
    assert!(
        anchor.contains("chat=oc_chat"),
        "the anchor carries the chat: {anchor}"
    );
    assert!(
        anchor.contains("topic=omt_topic"),
        "the anchor carries the topic: {anchor}"
    );
    assert!(
        anchor.contains("dir=/work/project"),
        "the anchor carries the directory: {anchor}"
    );
    assert!(
        anchor.contains("prompt_chars=4"),
        "the anchor carries the prompt's char count: {anchor}"
    );
    assert!(
        !logs.contains(prompt),
        "the prompt body must never be logged:\n{logs}"
    );
}

/// Outside a Topic the span omits `topic` rather than rendering an empty field.
#[tokio::test]
async fn a_lobby_turn_omits_the_topic_field() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let (app, _platform) = build_app(cfg, MockBackend::new(realistic_parts())).await;
    seed_session(&app, "ses_test", "/work").await;

    let (result, logs) =
        capture_logs(async { Turn::run(&app.turn_handles(), prompt_context(lobby(), "hi")).await }).await;
    result.unwrap();

    let anchor = line_with(&logs, "turn start:");
    assert!(
        anchor.contains("session=ses_test"),
        "the anchor carries the session: {anchor}"
    );
    assert!(
        anchor.contains("chat=oc_chat"),
        "the anchor carries the chat: {anchor}"
    );
    assert!(
        !anchor.contains("topic="),
        "a lobby turn has no topic to carry: {anchor}"
    );
}

/// The render poll runs on its own task (a spawn inherits no span), so it is
/// instrumented explicitly: both its line and the final render's carry the
/// session.
#[tokio::test]
async fn render_poll_and_final_render_lines_carry_the_session() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let mut backend = MockBackend::new(realistic_parts());
    // Park the prompt so the render poll has to be the first renderer.
    let gate = backend.hold_prompts();
    backend.given_timeline(
        "ses_test",
        vec![vec![
            msg(
                "user",
                "msg_cola_anchor",
                1_000,
                json!([{ "type": "text", "text": "hi" }]),
            ),
            msg("assistant", "msg_assist", 2_000, realistic_parts()),
        ]],
    );
    let messages_calls = Arc::clone(&backend.messages_calls);
    let (app, _platform) = build_app(cfg, backend).await;
    seed_session(&app, "ses_test", "/work").await;
    app.turn_render_poll_ms.store(5, Ordering::Relaxed);
    app.turn_drain_timeout_ms.store(60_000, Ordering::Relaxed);
    // Release the prompt only after the poll has read the Backend: the tick
    // that read is the tick that renders and logs, and `RenderPoll::stop`
    // always awaits that tick, so the line cannot be lost to a race.
    let releaser = tokio::spawn(async move {
        while messages_calls.lock().await.is_empty() {
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
        gate.add_permits(1);
    });

    let (result, logs) =
        capture_logs(async { Turn::run(&app.turn_handles(), prompt_context(lobby(), "hi")).await }).await;
    result.unwrap();
    releaser.await.unwrap();

    let poll = line_with(&logs, "render poll:");
    assert!(
        poll.contains("session=ses_test"),
        "the render-poll line carries the session: {poll}"
    );
    let final_line = line_with(&logs, "final render:");
    assert!(
        final_line.contains("session=ses_test"),
        "the final-render line carries the session: {final_line}"
    );
}

/// ADR-0048 level policy: the render poll is the per-Session liveness
/// heartbeat, so it stays at INFO — but only the tick that rendered new
/// content may log. Several ticks over one unchanged snapshot are silent.
#[tokio::test]
async fn the_render_poll_logs_at_info_only_on_progress() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let mut backend = MockBackend::new(realistic_parts());
    // Park the prompt so the poll ticks several times over one snapshot.
    let gate = backend.hold_prompts();
    backend.given_timeline(
        "ses_test",
        vec![vec![
            msg(
                "user",
                "msg_cola_anchor",
                1_000,
                json!([{ "type": "text", "text": "hi" }]),
            ),
            msg("assistant", "msg_assist", 2_000, realistic_parts()),
        ]],
    );
    let messages_calls = Arc::clone(&backend.messages_calls);
    let (app, _platform) = build_app(cfg, backend).await;
    seed_session(&app, "ses_test", "/work").await;
    app.turn_render_poll_ms.store(5, Ordering::Relaxed);
    app.turn_drain_timeout_ms.store(60_000, Ordering::Relaxed);
    // Release the prompt only after several poll ticks have read the same
    // snapshot: the first renders (and logs), the rest dedupe to nothing.
    let releaser = tokio::spawn(async move {
        while messages_calls.lock().await.len() < 4 {
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
        gate.add_permits(1);
    });

    let (result, logs) =
        capture_logs(async { Turn::run(&app.turn_handles(), prompt_context(lobby(), "hi")).await }).await;
    result.unwrap();
    releaser.await.unwrap();

    let polls: Vec<&str> = logs
        .lines()
        .filter(|line| line.contains("render poll:"))
        .collect();
    assert_eq!(
        polls.len(),
        1,
        "only the tick that rendered new content may log: {polls:?}\n{logs}"
    );
    assert_eq!(
        line_level(polls[0]),
        "INFO",
        "the render poll stays INFO: {}",
        polls[0]
    );
}

/// A 404 recreate moves the Turn onto a fresh session: the retry and the
/// finalization must be retrievable by the NEW id, while the warning that names
/// what was missing stays on the stale one.
#[tokio::test]
async fn a_recreated_turn_traces_under_the_fresh_session() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let mut backend = MockBackend::new(realistic_parts());
    // The mapped session is gone from the server: its prompt 404s, and the
    // recreate's `create_session` serves `ses_fresh`.
    backend.with_session_id("ses_fresh");
    backend.stale_session_mapping();
    let (app, _platform) = build_app(cfg, backend).await;
    seed_session(&app, "ses_test", "/work").await;
    // Both attempts await their poll's stop, which waits out one cadence.
    app.turn_render_poll_ms.store(5, Ordering::Relaxed);

    let (result, logs) =
        capture_logs(async { Turn::run(&app.turn_handles(), prompt_context(lobby(), "hi")).await }).await;
    result.unwrap();

    let anchor = line_with(&logs, "turn start:");
    assert!(
        anchor.contains("session=ses_test"),
        "the pre-recreate anchor names the session that was mapped: {anchor}"
    );
    let warn = line_with(&logs, "not found on the server; recreating");
    assert!(
        warn.contains("session=ses_test"),
        "the warning names the session that was missing: {warn}"
    );
    let final_line = line_with(&logs, "final render:");
    assert!(
        final_line.contains("session=ses_fresh"),
        "post-recreate lines carry the fresh session: {final_line}"
    );
    assert!(
        !final_line.contains("session=ses_test"),
        "post-recreate lines must not carry the stale session: {final_line}"
    );
}

/// The capture installs a THREAD-LOCAL subscriber only: a line emitted on
/// another thread while a capture is live must not land in its buffer, while
/// the capturing thread's own line does. That is what lets the harness run
/// these tests in parallel without a global subscriber.
#[tokio::test]
async fn capture_logs_never_installs_a_global_subscriber() {
    let barrier = Arc::new(std::sync::Barrier::new(2));
    let other = std::thread::spawn({
        let barrier = Arc::clone(&barrier);
        move || {
            barrier.wait(); // the capture is installed and its body is running
            tracing::info!("a line from an unrelated thread");
            barrier.wait(); // the body has emitted its own line
        }
    });

    let (_, logs) = capture_logs(async {
        barrier.wait();
        barrier.wait();
        tracing::info!("a line from the captured thread");
    })
    .await;
    other.join().unwrap();

    assert!(
        logs.contains("a line from the captured thread"),
        "the capturing thread's line lands in the buffer: {logs}"
    );
    assert!(
        !logs.contains("a line from an unrelated thread"),
        "capture_logs must never install a global subscriber: {logs}"
    );
}

/// A permission surfaced by a sweep enters its own `request` span (ADR-0048):
/// the surfacing line is retrievable by the Session that waits, and by its
/// Chat/Topic when the store maps it.
#[tokio::test]
async fn a_surfaced_permission_carries_its_session_chat_and_topic() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let mut backend = MockBackend::new(realistic_parts());
    backend.ask_permissions(vec![opencode::types::PermissionRequest {
        request_id: "per_1".into(),
        session_id: Some("ses_test".into()),
        permission: Some("bash".into()),
        patterns: vec!["ls -la".into()],
        metadata: None,
        always: Vec::new(),
    }]);
    let (app, _platform) = build_app(cfg, backend).await;
    let topic = ThreadKey::new("oc_chat".into(), "omt_topic".into());
    seed_entry(&app, SessionEntry::new(topic, "ses_test", "/work/project")).await;

    let mut seen = std::collections::HashSet::new();
    let (_, logs) = capture_logs(async { app.permission.sweep(&app.flow_handles(), &mut seen).await }).await;

    let surfaced = line_with(&logs, "权限");
    assert!(
        surfaced.contains("session=ses_test"),
        "the surfacing line carries the session: {surfaced}"
    );
    assert!(
        surfaced.contains("chat=oc_chat"),
        "the surfacing line carries the chat: {surfaced}"
    );
    assert!(
        surfaced.contains("topic=omt_topic"),
        "the surfacing line carries the topic: {surfaced}"
    );
}

/// A card click is session-scoped once its Session is resolved (ADR-0048): a
/// permission reply carries the session even though the card payload has no
/// chat — the span completes chat/topic from the store.
#[tokio::test]
async fn a_permission_card_action_carries_the_session() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let (app, _platform) = build_app(cfg, MockBackend::new(realistic_parts())).await;
    let topic = ThreadKey::new("oc_chat".into(), "omt_topic".into());
    seed_entry(&app, SessionEntry::new(topic, "ses_test", "/work/project")).await;

    let value = json!({
        "action": "perm",
        "reply": "once",
        "session_id": "ses_test",
        "request_id": "per_1",
        "perm_label": "✅ 已允许一次",
        "perm_color": "green",
        "perm_body": "bash",
    });
    let (result, logs) = capture_logs(async { app.host_action(value).await }).await;
    assert!(result.is_some(), "the click settles with a result card");

    let reply = line_with(&logs, "Permission reply sent");
    assert!(
        reply.contains("session=ses_test"),
        "the reply line carries the session: {reply}"
    );
    assert!(
        reply.contains("chat=oc_chat"),
        "the reply line carries the chat: {reply}"
    );
    assert!(
        !reply.contains("session=per_1"),
        "the request id must not be labelled a session: {reply}"
    );
}

/// Bounded wait for a card carrying `needle` to reach the fake Feishu, so a
/// captured body can advance on an observed side effect instead of a sleep.
async fn wait_for_card(platform: &Arc<RecordingPlatform>, needle: &str) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        let seen = platform.calls.lock().await.iter().any(|call| match call {
            PlatformCall::SendCard { card, .. }
            | PlatformCall::UpdateMessage { card, .. }
            | PlatformCall::ReplyCard { card, .. } => card_text(card).contains(needle),
            _ => false,
        });
        if seen {
            return;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "no card carrying {needle:?} reached Feishu"
        );
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

/// The external-message poll runs inside that Session's `external` span
/// (ADR-0048): the observation that notifies Feishu is retrievable by the
/// Session, and by its Chat/Topic.
#[tokio::test]
async fn an_external_message_observation_carries_its_session_chat_and_topic() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let mut backend = MockBackend::new(realistic_parts());
    backend.external_message("OpenChamber 里发的消息");
    let (app, _platform) = build_app(cfg, backend).await;
    let topic = ThreadKey::new("oc_group_1".into(), "omt_topic".into());
    seed_entry(&app, SessionEntry::new(topic, "ses_ext", "/tmp/ext")).await;
    // Baseline: a minute ago, so the fresh user message is "new".
    let watermark = chrono::Utc::now().timestamp_millis() - 60_000;
    app.external
        .last_user_msg_epoch
        .lock()
        .await
        .insert("ses_ext".into(), watermark);
    app.external.poll_interval_ms.store(20, Ordering::Relaxed);

    let (_, logs) = capture_logs(async {
        let poller = Arc::clone(&app);
        tokio::spawn(async move {
            let _ = poller.external.poll_loop(&poller.flow_handles()).await;
        });
        tokio::time::sleep(Duration::from_millis(200)).await;
    })
    .await;

    let observed = line_with(&logs, "External message on session");
    assert!(
        observed.contains("session=ses_ext"),
        "the observation carries the session: {observed}"
    );
    assert!(
        observed.contains("chat=oc_group_1"),
        "the observation carries the chat: {observed}"
    );
    assert!(
        observed.contains("topic=omt_topic"),
        "the observation carries the topic: {observed}"
    );
}

/// The reply-render loop is its own task, so it is instrumented where it is
/// spawned (ADR-0048): the line that finalizes the external reply carries the
/// Session it streamed.
#[tokio::test]
async fn an_external_reply_render_carries_its_session() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let mut backend = MockBackend::new(realistic_parts());
    backend.external_message("OpenChamber 里发的消息");
    // A reply that finishes in one step, so the renderer reaches Done.
    let reply_ready = backend.external_reply(json!([
        { "type": "step-start", "snapshot": "x" },
        { "type": "text", "text": "目录里有 src。" },
        { "type": "step-finish", "reason": "stop" },
    ]));
    let (app, platform) = build_app(cfg, backend).await;
    let topic = ThreadKey::new("oc_group_1".into(), "omt_topic".into());
    seed_entry(&app, SessionEntry::new(topic, "ses_ext", "/tmp/ext")).await;
    let watermark = chrono::Utc::now().timestamp_millis() - 60_000;
    app.external
        .last_user_msg_epoch
        .lock()
        .await
        .insert("ses_ext".into(), watermark);
    app.external.poll_interval_ms.store(20, Ordering::Relaxed);
    app.external.render_poll_ms.store(5, Ordering::Relaxed);

    let (_, logs) = capture_logs(async {
        let poller = Arc::clone(&app);
        tokio::spawn(async move {
            let _ = poller.external.poll_loop(&poller.flow_handles()).await;
        });
        // The notification arms the renderer; only then does the reply exist.
        wait_for_card(&platform, "有新消息").await;
        reply_ready.store(true, Ordering::SeqCst);
        // The Done card is flushed just before the renderer logs its exit.
        wait_for_card(&platform, "✅").await;
        // Let the render task finish its last line before the capture closes.
        tokio::time::sleep(Duration::from_millis(20)).await;
    })
    .await;

    let rendered = line_with(&logs, "external reply rendered");
    assert!(
        rendered.contains("session=ses_ext"),
        "the render line carries the session: {rendered}"
    );
}

/// A snapshot adoption's settle runs inside the adopted Session's `snapshot`
/// span (ADR-0048): the follow decision is retrievable by the Session it
/// looked at, even though the first adoption is not mapped to a thread yet.
#[tokio::test]
async fn a_snapshot_settle_carries_the_session() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let (app, _platform) = build_app(cfg, MockBackend::new(realistic_parts())).await;
    seed_session(&app, "ses_test", "/work/project").await;

    // Gathered Busy, but the server reports the turn finished by arm time —
    // the follow declines and keeps the static snapshot (ADR-0028).
    let data = crate::bridge::snapshot::SnapshotData {
        session_id: "ses_test".into(),
        directory: "/work/project".into(),
        status: Some(crate::opencode::types::SessionStatus::Busy),
        pending: Vec::new(),
        pending_elsewhere: None,
        tail: Vec::new(),
        newest_user_anchor: Some(crate::backend::TurnAnchor {
            message_id: crate::backend::MessageId::new("msg_user"),
            created_ms: 1_000,
        }),
        newest_user_is_cola_authored: false,
    };
    let (_, logs) = capture_logs(async {
        crate::bridge::external::settle_snapshot_after_send(
            &app.external,
            &app.flow_handles(),
            "om_snap",
            "接管",
            "标题",
            &data,
            None,
        )
        .await
    })
    .await;

    let declined = line_with(&logs, "snapshot follow");
    assert!(
        declined.contains("session=ses_test"),
        "the follow decision carries the session: {declined}"
    );
}

/// The topic cover sync runs inside the Session's `topic` span (ADR-0048): the
/// line that records the retitle is retrievable by the Session whose card was
/// updated, and by its Chat/Topic.
#[tokio::test]
async fn a_topic_cover_retitle_carries_the_session_chat_and_topic() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let mut backend = MockBackend::new(realistic_parts());
    backend.with_session_title("ses_test", "新名字");
    let (app, _platform) = build_app(cfg, backend).await;
    seed_entry(
        &app,
        SessionEntry {
            thread_key: ThreadKey::new("oc_chat".into(), "omt_topic".into()),
            session_id: "ses_test".into(),
            directory: "/work/project".into(),
            agent: None,
            model: None,
            auto_accept: false,
            topic_anchor: Some("om_seed".into()),
            topic_root: Some("om_cover".into()),
            variant: None,
        },
    )
    .await;
    seed_cover_title(&app, "ses_test", "旧名字").await;

    let (settled, logs) = capture_logs(async {
        crate::bridge::topic::sync_topic_cover_title(
            &app.cards_handle(),
            &app.sessions_handle(),
            &app.opencode,
            "ses_test",
        )
        .await
    })
    .await;
    assert!(settled, "the retitle must settle");

    let retitled = line_with(&logs, "topic cover card updated");
    assert!(
        retitled.contains("session=ses_test"),
        "the retitle carries the session: {retitled}"
    );
    assert!(
        retitled.contains("chat=oc_chat"),
        "the retitle carries the chat: {retitled}"
    );
    assert!(
        retitled.contains("topic=omt_topic"),
        "the retitle carries the topic: {retitled}"
    );
}

/// A re-switch — a session already mapped to this thread — runs its snapshot
/// gather inside the Session's `snapshot` span (ADR-0048), so the gather's
/// best-effort read warning is retrievable by the Session being re-activated.
#[tokio::test]
async fn a_re_switch_snapshot_gather_carries_the_session() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let mut backend = MockBackend::new(realistic_parts());
    backend.given_sessions(vec![list_session(
        "ses_alpha01",
        "唯一外部标题",
        "/work/ext",
        100,
    )]);
    // The gather's status read fails: its warning is the line under test.
    backend.status_read_fails("boom");
    let (app, _platform) = build_app(cfg, backend).await;
    let lobby = ThreadKey::new("chat_1".into(), "chat_1".into());
    seed_entry(&app, SessionEntry::new(lobby.clone(), "ses_alpha01", "/work/ext")).await;

    let (_, logs) = capture_logs(async {
        send_command_in(
            &app,
            "/switch 唯一外部标题",
            lobby,
            "msg_switch",
            crate::config::ConversationKind::P2p,
        )
        .await;
    })
    .await;

    let failed = line_with(&logs, "snapshot: session status for");
    assert!(
        failed.contains("session=ses_alpha01"),
        "the gather warning carries the session: {failed}"
    );
    assert!(
        failed.contains("chat=chat_1"),
        "the gather warning carries the chat: {failed}"
    );
}
