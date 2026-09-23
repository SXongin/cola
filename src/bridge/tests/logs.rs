//! Session-scoped logs (ADR-0048): the `capture_logs` test seam and the Turn's
//! trace — the `turn{session=… chat=… topic=…}` span, its one INFO start
//! anchor, and the separately-instrumented render poll.

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
            time: Some(MessageTime { created }),
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
fn ctx(thread_key: ThreadKey, text: &str) -> PromptContext {
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

/// The first captured line containing `needle` — the line an assertion is about.
fn line_with<'a>(logs: &'a str, needle: &str) -> &'a str {
    logs.lines()
        .find(|line| line.contains(needle))
        .unwrap_or_else(|| panic!("no captured line contains {needle:?}:\n{logs}"))
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
    let (result, logs) = capture_logs(async { Turn::run(&app, ctx(topic, prompt)).await }).await;
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

    let (result, logs) = capture_logs(async { Turn::run(&app, ctx(lobby(), "hi")).await }).await;
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
    let gate = Arc::new(tokio::sync::Semaphore::new(0));
    let mut backend = MockBackend::new(realistic_parts());
    // Park the prompt so the render poll has to be the first renderer.
    backend.prompt_gate = Some(Arc::clone(&gate));
    backend.message_scripts.lock().await.insert(
        "ses_test".into(),
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

    let (result, logs) = capture_logs(async { Turn::run(&app, ctx(lobby(), "hi")).await }).await;
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
