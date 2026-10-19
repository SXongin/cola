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

/// A transcript whose only message is the Turn's user anchor at `created_ms` —
/// the server time the artifact header must show.
fn anchor_transcript(created_ms: i64) -> SessionTranscript {
    SessionTranscript::new(vec![TranscriptMessage {
        id: MessageId::new(TURN_FROM),
        role: MessageRole::User,
        time: Some(MessageTime {
            created: created_ms,
            completed: Some(created_ms),
        }),
        model: None,
        tokens: None,
        error: None,
        parts: Vec::new(),
    }])
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

/// Build an app while keeping the `MockBackend` handle, so a test can assert
/// the exact arguments a callback passed to a backend read.
fn app_with_backend(
    cfg: crate::config::Config,
    backend: MockBackend,
) -> (
    std::sync::Arc<crate::bridge::App>,
    std::sync::Arc<RecordingPlatform>,
    std::sync::Arc<MockBackend>,
) {
    let backend = std::sync::Arc::new(backend);
    let platform = std::sync::Arc::new(RecordingPlatform::new());
    let app = std::sync::Arc::new(crate::bridge::App::new(cfg, backend.clone(), platform.clone()).unwrap());
    (app, platform, backend)
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
    let anchor_ms = crate::feishu::card::test_local_ms(2026, 10, 19, 14, 30);
    backend.given_transcript("ses_test", vec![anchor_transcript(anchor_ms)]);
    let (app, platform, backend) = app_with_backend(cfg, backend);

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
        PlatformCall::UploadFile {
            name,
            mime,
            size,
            content,
        } => {
            assert_eq!(mime, "text/html");
            assert!(*size > 0, "the artifact carries bytes");
            let suffix = name.rsplit_once('.').map(|(_, suffix)| suffix);
            assert_eq!(suffix, Some("html"), "the preview routes by extension: {name}");
            assert_turn_diff_file_name(name);
            // #798's header wants title, turn time, stats: the delivered
            // artifact must carry the anchor's server time.
            let html = String::from_utf8_lossy(content);
            assert!(
                html.contains("10-19 14:30"),
                "the artifact header carries the turn time: {html}"
            );
        }
        other => panic!("expected a file upload, got {other:?}"),
    }

    // The Turn range is the button's own anchor: the exact `from`, no `to`,
    // and `context=3` (spec #797 decision 1). A wrong argument would open
    // another Turn's diff, so the test pins it at the backend seam.
    let calls = backend.session_diff_calls.lock().await;
    assert_eq!(
        calls.as_slice(),
        &[("ses_test".to_string(), Some(TURN_FROM.to_string()), None, Some(3))]
    );
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

/// The payload's empty `from` compatibility path (#801) works on V1 too: the
/// backend read omits `from`, and the aggregation resolves the newest user
/// message instead of failing on the empty string.
#[tokio::test]
async fn v1_click_with_an_empty_from_uses_the_newest_user_message() {
    let (_dir, cfg) = app_config();
    let mut backend = MockBackend::new(Vec::new());
    backend.given_transcript("ses_test", vec![anchored_edit_transcript("src/v1.rs")]);
    let (app, platform) = build_app(cfg, backend).await;

    let mut payload = click();
    payload["from"] = serde_json::json!("");
    app.host_action_on_message(payload, CARD_MESSAGE)
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
        "an empty from still resolves the Turn and sends a File Message: {message:?}"
    );
    assert_eq!(file_uploads(&platform).await.len(), 1);
}

/// The V1 fallback must fail loudly when it has no working directory to scope
/// the aggregation: without a root it cannot prove a recorded path belongs to
/// the repository, and an unscoped diff would offer the dead entry this fix
/// removes.
#[tokio::test]
async fn v1_click_without_a_working_directory_fails_loudly() {
    let (_dir, cfg) = app_config();
    let mut backend = MockBackend::new(Vec::new());
    backend.given_transcript("ses_test", vec![anchored_edit_transcript("src/v1.rs")]);
    let (app, platform) = build_app(cfg, backend).await;

    let mut payload = click();
    payload["directory"] = serde_json::json!("");
    app.host_action_on_message(payload, CARD_MESSAGE)
        .await
        .expect("the click is acknowledged");
    wait_for_reply_containing(&platform, "生成完整 diff 失败").await;

    assert!(file_uploads(&platform).await.is_empty(), "no upload");
    assert!(file_messages(&platform).await.is_empty(), "no File Message");
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

    // The send is recorded before the delivery marks the Turn `Sent`; retry
    // the duplicate click until that terminal state is observable (each
    // intermediate click is itself a harmless no-op).
    let mut toast = None;
    for _ in 0..200 {
        let repeated = app
            .host_action_on_message(click(), CARD_MESSAGE)
            .await
            .expect("a repeated click is acknowledged");
        toast = repeated.toast;
        if toast.as_deref() == Some("已发送") {
            break;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    assert_eq!(toast.as_deref(), Some("已发送"));
    assert_eq!(file_uploads(&platform).await.len(), 1, "no second upload");
    assert_eq!(file_messages(&platform).await.len(), 1, "no second File Message");
}

/// A Turn that changed no file replies the exact sentence and sends no file;
/// the empty answer is terminal, so a repeated click neither re-reads the diff
/// nor repeats the reply.
#[tokio::test]
async fn an_empty_turn_replies_text_once_and_sends_no_file() {
    let (_dir, cfg) = app_config();
    let mut backend = MockBackend::new(Vec::new());
    backend.given_session_diff(TurnDiff::default());
    let (app, platform, backend) = app_with_backend(cfg, backend);

    app.host_action_on_message(click(), CARD_MESSAGE)
        .await
        .expect("the click is acknowledged");
    wait_for_reply_containing(&platform, "本 turn 没有文件改动").await;

    assert!(file_uploads(&platform).await.is_empty(), "no upload");
    assert!(file_messages(&platform).await.is_empty(), "no File Message");

    let repeated = app
        .host_action_on_message(click(), CARD_MESSAGE)
        .await
        .expect("a repeated click is acknowledged");
    assert_eq!(repeated.toast.as_deref(), Some("本 turn 没有文件改动"));
    assert_eq!(
        backend.session_diff_calls.lock().await.len(),
        1,
        "the empty outcome is terminal: no second diff read"
    );
    assert_eq!(
        text_replies(&platform).await.len(),
        1,
        "the empty answer is not repeated"
    );
}

/// A failed upload speaks an actionable error, never panics or retries to the
/// timeout; a later click may try again because nothing was sent.
#[tokio::test]
async fn an_upload_failure_replies_an_error_and_stays_retryable() {
    let (_dir, cfg) = app_config();
    let mut backend = MockBackend::new(Vec::new());
    backend.given_session_diff(TurnDiff(vec![diff_file("src/a.rs", 1, 0)]));
    let (app, platform, backend) = app_with_backend(cfg, backend);
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
    // The failed delivery's claim is released as its task returns, so retry
    // until one click actually re-enters the read (the reply being recorded
    // does not yet mean the claim was dropped).
    for _ in 0..200 {
        app.host_action_on_message(click(), CARD_MESSAGE)
            .await
            .expect("the retry is acknowledged");
        if backend.session_diff_calls.lock().await.len() > 1 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    wait_for_file_message(&platform).await;
    assert_eq!(file_messages(&platform).await.len(), 1);
    assert_eq!(
        backend.session_diff_calls.lock().await.len(),
        2,
        "a definite failure is retryable: the second click re-reads the diff"
    );
}

/// An ambiguous File Message send (Feishu accepted it, the response timed out)
/// is terminal: the outcome is spoken once and a later click sends nothing —
/// the artifact cannot be retracted, so a retry could duplicate it.
#[tokio::test]
async fn an_ambiguous_send_is_terminal_and_never_duplicated() {
    let (_dir, cfg) = app_config();
    let mut backend = MockBackend::new(Vec::new());
    backend.given_session_diff(TurnDiff(vec![diff_file("src/a.rs", 1, 0)]));
    let (app, platform, backend) = app_with_backend(cfg, backend);
    // A bound short enough that the parked File Message send cannot answer.
    app.viewer_transfer_timeout_ms
        .store(400, std::sync::atomic::Ordering::Relaxed);

    let (entered, _release) = platform.pause_after_accept("send_message", CARD_MESSAGE);
    app.host_action_on_message(click(), CARD_MESSAGE)
        .await
        .expect("the click is acknowledged");
    tokio::time::timeout(Duration::from_secs(2), entered.notified())
        .await
        .expect("the spawned delivery reaches the File Message send");
    wait_for_reply_containing(&platform, "发送结果未知").await;

    assert_eq!(
        file_messages(&platform).await.len(),
        1,
        "the accepted File Message is recorded once"
    );

    let repeated = app
        .host_action_on_message(click(), CARD_MESSAGE)
        .await
        .expect("a repeated click is acknowledged");
    assert_eq!(
        repeated.toast.as_deref(),
        Some("发送结果未知，为避免重复未重试。")
    );
    assert_eq!(file_uploads(&platform).await.len(), 1, "no second upload");
    assert_eq!(file_messages(&platform).await.len(), 1, "no second File Message");
    assert_eq!(
        backend.session_diff_calls.lock().await.len(),
        1,
        "the ambiguous outcome is terminal: no second diff read"
    );
}

/// A send that fails at transport/5xx after the request went out may have
/// landed. It is spoken as unknown and the `(session, from)` guard stays
/// terminal, so a later click cannot duplicate the File Message.
#[tokio::test]
async fn a_post_accept_send_error_is_terminal_and_never_duplicated() {
    let (_dir, cfg) = app_config();
    let mut backend = MockBackend::new(Vec::new());
    backend.given_session_diff(TurnDiff(vec![diff_file("src/a.rs", 1, 0)]));
    let (app, platform, backend) = app_with_backend(cfg, backend);
    platform
        .ambiguous_send_message_count
        .store(1, std::sync::atomic::Ordering::SeqCst);

    app.host_action_on_message(click(), CARD_MESSAGE)
        .await
        .expect("the click is acknowledged");
    wait_for_reply_containing(&platform, "发送结果未知").await;
    assert_eq!(file_uploads(&platform).await.len(), 1, "one upload");

    let repeated = app
        .host_action_on_message(click(), CARD_MESSAGE)
        .await
        .expect("a repeated click is acknowledged");
    assert_eq!(
        repeated.toast.as_deref(),
        Some("发送结果未知，为避免重复未重试。")
    );
    assert_eq!(file_uploads(&platform).await.len(), 1, "no second upload");
    assert_eq!(
        backend.session_diff_calls.lock().await.len(),
        1,
        "the ambiguous outcome is terminal: no second diff read"
    );
}

/// A failed transcript read does not lose the artifact on V2: the header states
/// the time as unknown and the File Message still lands.
#[tokio::test]
async fn a_failed_transcript_still_delivers_with_an_unknown_time() {
    let (_dir, cfg) = app_config();
    let mut backend = MockBackend::new(Vec::new());
    backend.given_session_diff(TurnDiff(vec![diff_file("src/a.rs", 1, 0)]));
    backend
        .fail_transcript_reads
        .store(1, std::sync::atomic::Ordering::SeqCst);
    let (app, platform, _backend) = app_with_backend(cfg, backend);

    app.host_action_on_message(click(), CARD_MESSAGE)
        .await
        .expect("the click is acknowledged");
    wait_for_file_message(&platform).await;

    let uploads = file_uploads(&platform).await;
    match &uploads[0] {
        PlatformCall::UploadFile { content, .. } => {
            let html = String::from_utf8_lossy(content);
            assert!(
                html.contains("时间未知"),
                "a failed transcript states the gap instead of failing: {html}"
            );
            assert!(html.contains("src/a.rs"), "the diff still renders");
        }
        other => panic!("expected a file upload, got {other:?}"),
    }
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
