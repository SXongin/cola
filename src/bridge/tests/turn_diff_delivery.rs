//! The Turn card's 「查看完整改动」 callback (spec #797, ticket #801).
//!
//! A click is acknowledged immediately and the diff is fetched, rendered and
//! delivered off the callback path as exactly ONE File Message. These tests
//! drive the real card-action seam (`App::handle_card_action`) against the
//! platform fake: one upload, one `msg_type:"file"` message under the clicked
//! card, an empty Turn that says so instead, and a failure that is spoken
//! rather than dropped.

use crate::backend::{
    MessageId, MessageRole, MessageTime, Part, SessionTranscript, ToolCall, ToolIdentity, ToolOutput,
    ToolStatus, TranscriptMessage, TurnDiff, TurnDiffFile,
};
use crate::bridge::test_support::*;
use std::time::Duration;

/// The clicked Turn card's own message id (`open_message_id`).
const CARD_MESSAGE: &str = "msg_turn_card";

/// The user-message anchor the #477 button carries as `from`.
const TURN_FROM: &str = "msg_user_9";

fn click() -> serde_json::Value {
    serde_json::json!({
        "op": "turn_diff",
        "session_id": "ses_test",
        "from": TURN_FROM,
        "directory": "/workspace/cola",
    })
}

fn diff_file(path: &str, additions: usize, deletions: usize) -> TurnDiffFile {
    TurnDiffFile {
        path: path.to_string(),
        patch: format!(
            "Index: {path}\n===================================================================\n--- {path}\n+++ {path}\n@@ -1 +1,2 @@\n-old\n+new\n"
        ),
        additions,
        deletions,
        status: "modified".to_string(),
    }
}

/// Every File Message the platform posted (`msg_type: "file"`), in call order.
async fn file_messages(platform: &RecordingPlatform) -> Vec<PlatformCall> {
    platform
        .calls
        .lock()
        .await
        .iter()
        .filter(|call| matches!(call, PlatformCall::SendMessageReply { msg_type, .. } if msg_type == "file"))
        .cloned()
        .collect()
}

/// Every `upload_file` entry the platform recorded, in call order.
async fn file_uploads(platform: &RecordingPlatform) -> Vec<PlatformCall> {
    platform
        .calls
        .lock()
        .await
        .iter()
        .filter(|call| matches!(call, PlatformCall::UploadFile { .. }))
        .cloned()
        .collect()
}

/// Every text reply the platform recorded, in call order.
async fn text_replies(platform: &RecordingPlatform) -> Vec<String> {
    platform
        .calls
        .lock()
        .await
        .iter()
        .filter_map(|call| match call {
            PlatformCall::ReplyText { text, .. } => Some(text.clone()),
            _ => None,
        })
        .collect()
}

/// Poll until the spawned delivery posts its File Message, or fail the test.
async fn wait_for_file_message(platform: &RecordingPlatform) -> PlatformCall {
    for _ in 0..400 {
        if let Some(message) = file_messages(platform).await.into_iter().next() {
            return message;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    panic!("timed out waiting for the Turn diff File Message");
}

/// Wait for a text reply containing `needle`.
async fn wait_for_reply_containing(platform: &RecordingPlatform, needle: &str) {
    for _ in 0..400 {
        if text_replies(platform)
            .await
            .iter()
            .any(|text| text.contains(needle))
        {
            return;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    panic!("timed out waiting for a reply containing {needle:?}");
}

/// An anchored Turn whose assistant message carries one settled `edit` call —
/// the V1 tool-metadata shape #800 aggregates when `session_diff` answers
/// `None`.
fn anchored_edit_transcript(path: &str) -> SessionTranscript {
    let patch = format!(
        "Index: {path}\n===================================================================\n--- {path}\n+++ {path}\n@@ -1 +1,2 @@\n-old {path}\n+new {path}\n"
    );
    SessionTranscript::new(vec![
        TranscriptMessage {
            id: MessageId::new(TURN_FROM),
            role: MessageRole::User,
            time: Some(MessageTime {
                created: 1_000,
                completed: Some(1_000),
            }),
            model: None,
            tokens: None,
            error: None,
            parts: Vec::new(),
        },
        TranscriptMessage {
            id: MessageId::new("msg_assistant"),
            role: MessageRole::Assistant,
            time: Some(MessageTime {
                created: 1_100,
                completed: Some(1_200),
            }),
            model: None,
            tokens: None,
            error: None,
            parts: vec![Part::Tool(ToolCall {
                identity: ToolIdentity {
                    name: "edit".to_string(),
                    call_id: "call_edit".to_string(),
                },
                status: ToolStatus::Completed,
                started_at: Some(1_100),
                input: None,
                metadata: Some(serde_json::json!({ "diff": patch })),
                output: ToolOutput::default(),
            })],
        },
    ])
}

fn app_config() -> (tempfile::TempDir, crate::config::Config) {
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    (dir, cfg)
}

/// Acceptance: one click is ONE `upload_file` and ONE `msg_type=file` message
/// under the clicked card, carrying the platform's key and an ASCII `.html`
/// name the client previews by extension.
#[tokio::test]
async fn one_click_uploads_once_and_posts_one_html_file_message() {
    let (_dir, cfg) = app_config();
    let mut backend = MockBackend::new(Vec::new());
    backend.given_session_diff(TurnDiff(vec![
        diff_file("src/a.rs", 2, 1),
        diff_file("src/b.rs", 1, 0),
    ]));
    let (app, platform) = build_app(cfg, backend).await;

    let ack = app
        .host_action_on_message(click(), CARD_MESSAGE)
        .await
        .expect("the click is acknowledged");
    assert!(ack.card.is_none(), "the click never rebuilds the card inline");
    assert_eq!(ack.toast.as_deref(), Some("正在生成完整 diff…"));

    let message = wait_for_file_message(&platform).await;
    match message {
        PlatformCall::SendMessageReply {
            message_id,
            msg_type,
            content,
        } => {
            assert_eq!(
                message_id, CARD_MESSAGE,
                "the file replies under the clicked card"
            );
            assert_eq!(msg_type, "file");
            assert_eq!(
                content["file_key"], "file_test",
                "the uploaded key is the payload"
            );
        }
        other => panic!("expected a File Message, got {other:?}"),
    }

    let uploads = file_uploads(&platform).await;
    assert_eq!(uploads.len(), 1, "exactly one upload: {uploads:?}");
    match &uploads[0] {
        PlatformCall::UploadFile { name, mime, size } => {
            assert_eq!(mime, "text/html");
            assert!(*size > 0, "the artifact carries bytes");
            let suffix = name.rsplit_once('.').map(|(_, suffix)| suffix);
            assert_eq!(suffix, Some("html"), "the preview routes by extension: {name}");
            assert_turn_diff_file_name(name);
        }
        other => panic!("expected a file upload, got {other:?}"),
    }
}

/// The filename contract: ASCII, short, and `diff-<short>.html`.
fn assert_turn_diff_file_name(name: &str) {
    assert!(name.is_ascii(), "the filename must be ASCII, got {name:?}");
    assert!(name.len() <= 40, "short enough to render: {name:?}");
    let Some(rest) = name.strip_prefix("diff-") else {
        panic!("filename must start with `diff-`: {name:?}");
    };
    let Some(hash) = rest.strip_suffix(".html") else {
        panic!("filename must end with `.html`: {name:?}");
    };
    assert!(
        !hash.is_empty() && hash.chars().all(|ch| ch.is_ascii_hexdigit()),
        "the short is hex, not a path fragment: {name:?}"
    );
}

/// V1 has no session diff endpoint: the click aggregates the Turn's tool
/// metadata (#800) and still delivers one artifact.
#[tokio::test]
async fn v1_click_falls_back_to_the_tool_metadata_aggregation() {
    let (_dir, cfg) = app_config();
    let mut backend = MockBackend::new(Vec::new());
    backend.given_transcript("ses_test", vec![anchored_edit_transcript("src/v1.rs")]);
    let (app, platform) = build_app(cfg, backend).await;

    app.host_action_on_message(click(), CARD_MESSAGE)
        .await
        .expect("the click is acknowledged");

    let message = wait_for_file_message(&platform).await;
    assert!(
        matches!(
            message,
            PlatformCall::SendMessageReply {
                msg_type: ref ty,
                ..
            } if ty == "file"
        ),
        "the V1 fallback still sends a File Message: {message:?}"
    );
    assert_eq!(file_uploads(&platform).await.len(), 1);
}

/// Repeated clicks are a no-op after the artifact was sent: the in-process
/// `(session, from)` guard answers 「已发送」 and posts nothing again.
#[tokio::test]
async fn a_second_click_after_the_send_is_a_no_op() {
    let (_dir, cfg) = app_config();
    let mut backend = MockBackend::new(Vec::new());
    backend.given_session_diff(TurnDiff(vec![diff_file("src/a.rs", 1, 0)]));
    let (app, platform) = build_app(cfg, backend).await;

    app.host_action_on_message(click(), CARD_MESSAGE)
        .await
        .expect("the first click is acknowledged");
    wait_for_file_message(&platform).await;

    let repeated = app
        .host_action_on_message(click(), CARD_MESSAGE)
        .await
        .expect("a repeated click is acknowledged");
    assert_eq!(repeated.toast.as_deref(), Some("已发送"));
    assert_eq!(file_uploads(&platform).await.len(), 1, "no second upload");
    assert_eq!(file_messages(&platform).await.len(), 1, "no second File Message");
}

/// A Turn that changed no file replies the exact sentence and sends no file.
#[tokio::test]
async fn an_empty_turn_replies_text_and_sends_no_file() {
    let (_dir, cfg) = app_config();
    let mut backend = MockBackend::new(Vec::new());
    backend.given_session_diff(TurnDiff::default());
    let (app, platform) = build_app(cfg, backend).await;

    app.host_action_on_message(click(), CARD_MESSAGE)
        .await
        .expect("the click is acknowledged");
    wait_for_reply_containing(&platform, "本 turn 没有文件改动").await;

    assert!(file_uploads(&platform).await.is_empty(), "no upload");
    assert!(file_messages(&platform).await.is_empty(), "no File Message");
}

/// A failed upload speaks an actionable error, never panics or retries to the
/// timeout; a later click may try again because nothing was sent.
#[tokio::test]
async fn an_upload_failure_replies_an_error_and_stays_retryable() {
    let (_dir, cfg) = app_config();
    let mut backend = MockBackend::new(Vec::new());
    backend.given_session_diff(TurnDiff(vec![diff_file("src/a.rs", 1, 0)]));
    let (app, platform) = build_app(cfg, backend).await;
    platform
        .fail_upload_file_count
        .store(1, std::sync::atomic::Ordering::SeqCst);

    app.host_action_on_message(click(), CARD_MESSAGE)
        .await
        .expect("the click is acknowledged");
    wait_for_reply_containing(&platform, "生成完整 diff 失败").await;

    assert!(file_messages(&platform).await.is_empty(), "nothing was sent");
    assert!(
        file_uploads(&platform).await.is_empty(),
        "the failed upload recorded nothing"
    );

    // A retry is allowed: the failure sent nothing, so it is not a duplicate.
    app.host_action_on_message(click(), CARD_MESSAGE)
        .await
        .expect("the retry is acknowledged");
    wait_for_file_message(&platform).await;
    assert_eq!(file_messages(&platform).await.len(), 1);
}

/// The callback acknowledges before the delivery does any work: with the
/// upload parked, the click still returns, and only the spawned task waits.
#[tokio::test]
async fn the_click_acks_before_the_delivery_runs() {
    let (_dir, cfg) = app_config();
    let mut backend = MockBackend::new(Vec::new());
    backend.given_session_diff(TurnDiff(vec![diff_file("src/a.rs", 1, 0)]));
    let (app, platform) = build_app(cfg, backend).await;

    let (entered, release) = platform.pause("upload_file", "");
    let ack = app
        .host_action_on_message(click(), CARD_MESSAGE)
        .await
        .expect("the click is acknowledged while the upload is parked");
    assert_eq!(ack.toast.as_deref(), Some("正在生成完整 diff…"));
    assert!(
        file_uploads(&platform).await.is_empty(),
        "the click returned before the upload ran"
    );

    // A second click while the first is still in flight is a no-op: the
    // in-flight claim answers without spawning another delivery.
    let repeated = app
        .host_action_on_message(click(), CARD_MESSAGE)
        .await
        .expect("the concurrent second click is acknowledged");
    assert_eq!(repeated.toast.as_deref(), Some("正在生成完整 diff…"));

    tokio::time::timeout(Duration::from_secs(2), entered.notified())
        .await
        .expect("the spawned delivery reaches the upload");
    release.notify_one();
    wait_for_file_message(&platform).await;
    assert_eq!(file_uploads(&platform).await.len(), 1, "one upload");
    assert_eq!(file_messages(&platform).await.len(), 1, "one File Message");
}
