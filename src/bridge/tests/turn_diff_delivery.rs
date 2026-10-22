//! The Turn card's 「查看完整变化」 callback (spec #797, ticket #801).
//!
//! A click is acknowledged immediately and the diff is fetched, rendered and
//! delivered off the callback path as exactly ONE File Message. These tests
//! drive the real card-action seam (`App::handle_card_action`) against the
//! platform fake: one upload, one `msg_type:"file"` message under the clicked
//! card, an empty Turn that says so instead, and a failure that is spoken
//! rather than dropped.

use crate::backend::{
    MessageId, MessageRole, MessageTime, SessionTranscript, TranscriptMessage, TurnDiff, TurnDiffFile,
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

/// Does the card carry any part of the Turn's change index — the settled line
/// (either spelling) or its complete-diff button? The negative cases assert
/// the whole index is absent, so a regression to the retired `📝 本次改动`
/// copy cannot slip through by matching only the current one.
fn has_turn_diff_index(card: &serde_json::Value) -> bool {
    let text = card_text(card);
    let button = card["body"]["elements"]
        .as_array()
        .map(|elements| {
            elements
                .iter()
                .any(|element| element["value"]["op"] == "turn_diff")
        })
        .unwrap_or(false);
    text.contains("📝 本轮期间工作区变化") || text.contains("📝 本次改动") || button
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
    wait_for_reply_containing(&platform, "本轮期间工作区没有留下净变化。").await;

    assert!(file_uploads(&platform).await.is_empty(), "no upload");
    assert!(file_messages(&platform).await.is_empty(), "no File Message");

    let repeated = app
        .host_action_on_message(click(), CARD_MESSAGE)
        .await
        .expect("a repeated click is acknowledged");
    assert_eq!(repeated.toast.as_deref(), Some("本轮期间工作区没有留下净变化。"));
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

/// ADR-0087's settle seam: a normal Turn's terminal card reads
/// `session.diff(from=<anchor>, context=0)` once — the SAME source the click
/// reads — and renders its counts and button. Nothing is estimated from the
/// transcript while the Turn runs.
#[tokio::test]
async fn the_terminal_card_index_is_the_settle_session_diff_read() {
    let _wd = test_work_dir();
    let timeline = vec![
        super::drain::user("msg_cola_anchor", 1_000, "第一条消息"),
        super::drain::assistant(2_000, "完成。"),
    ];
    let (_dir, app, backend, platform) = super::drain::scripted_app(
        vec![SessionTranscript::new(timeline)],
        Some(crate::backend::types::SessionStatus::Idle),
    )
    .await;
    backend
        .session_diff_available
        .store(true, std::sync::atomic::Ordering::SeqCst);
    *backend
        .session_diff
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) =
        Some(TurnDiff(vec![diff_file("src/a.rs", 2, 1)]));

    let turn = super::drain::spawn_turn(&app, super::drain::ctx("ses_test", "第一条消息"));
    tokio::time::timeout(Duration::from_secs(5), turn)
        .await
        .expect("the turn ends")
        .expect("the turn task does not panic")
        .expect("the turn succeeded");

    let mut cards = platform.sent_cards().await;
    cards.extend(platform.updated_cards().await);
    let card = cards
        .iter()
        .rev()
        .find(|card| card_text(card).contains("📝 本轮期间工作区变化"))
        .unwrap_or_else(|| panic!("the terminal card carries the settled index: {cards:?}"));
    assert!(
        card_text(card).contains("📝 本轮期间工作区变化 · 1 files · +2 −1"),
        "the index is the server's counts: {card}"
    );
    let button = card["body"]["elements"]
        .as_array()
        .unwrap()
        .iter()
        .find(|element| element["value"]["op"] == "turn_diff")
        .unwrap_or_else(|| panic!("the terminal card carries the button: {card}"));
    assert_eq!(button["value"]["from"], "msg_cola_anchor");

    assert_eq!(
        backend.session_diff_calls.lock().await.as_slice(),
        &[(
            "ses_test".to_string(),
            Some("msg_cola_anchor".to_string()),
            None,
            Some(0)
        )],
        "one settle read, scoped to the Turn, at zero context"
    );
}

/// Issue #830's shape, inverted: an EMPTY authoritative answer means no entry.
/// The transcript may record edits, but the card must not claim a change the
/// artifact cannot show — and it still spends the one settle read.
#[tokio::test]
async fn an_empty_settle_read_leaves_the_terminal_card_without_an_index() {
    let _wd = test_work_dir();
    // The transcript records a settled edit, but the authority says empty:
    // #830's "tools recorded something, the net is zero" shape. The card must
    // follow the authority (no index), never the transcript.
    let patch = "Index: src/a.rs\n===\n--- src/a.rs\n+++ src/a.rs\n@@ -1 +1 @@\n-old\n+new\n";
    let mut edit = crate::bridge::test_support::tool_part(
        "edit",
        "call_edit",
        crate::backend::ToolStatus::Completed,
        serde_json::json!({ "filePath": "src/a.rs" }),
        "ok",
    );
    if let crate::backend::Part::Tool(call) = &mut edit {
        call.metadata = Some(serde_json::json!({ "diff": patch }));
    }
    let assistant = crate::bridge::test_support::typed_message(
        "msg_assistant",
        crate::backend::MessageRole::Assistant,
        Some(2_000),
        vec![
            edit,
            crate::bridge::test_support::text_part("完成。"),
            crate::backend::Part::StepFinish(crate::backend::StepFinish {
                reason: crate::backend::FinishReason::Stop,
            }),
        ],
    );
    let timeline = vec![
        super::drain::user("msg_cola_anchor", 1_000, "第一条消息"),
        assistant,
    ];
    let (_dir, app, backend, platform) = super::drain::scripted_app(
        vec![SessionTranscript::new(timeline)],
        Some(crate::backend::types::SessionStatus::Idle),
    )
    .await;
    backend
        .session_diff_available
        .store(true, std::sync::atomic::Ordering::SeqCst);
    *backend
        .session_diff
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(TurnDiff::default());

    let turn = super::drain::spawn_turn(&app, super::drain::ctx("ses_test", "第一条消息"));
    tokio::time::timeout(Duration::from_secs(5), turn)
        .await
        .expect("the turn ends")
        .expect("the turn task does not panic")
        .expect("the turn succeeded");

    let mut cards = platform.sent_cards().await;
    cards.extend(platform.updated_cards().await);
    assert!(
        cards.iter().all(|card| !has_turn_diff_index(card)),
        "an empty authoritative answer renders no index: {cards:?}"
    );
    assert_eq!(
        backend.session_diff_calls.lock().await.len(),
        1,
        "the empty answer is still the one settle read"
    );
}

/// A failed click read speaks the dedicated copy and stays retryable: the
/// claim is released on the failure, and a later successful read sends the
/// artifact. Distinct from the render/upload failure copy (ADR-0087).
#[tokio::test]
async fn a_failed_read_speaks_the_read_failure_and_stays_retryable() {
    let (_dir, cfg) = app_config();
    let mut backend = MockBackend::new(Vec::new());
    backend.given_session_diff(TurnDiff(vec![diff_file("src/a.rs", 1, 0)]));
    backend
        .fail_session_diff
        .store(true, std::sync::atomic::Ordering::SeqCst);
    let (app, platform, backend) = app_with_backend(cfg, backend);

    app.host_action_on_message(click(), CARD_MESSAGE)
        .await
        .expect("the click is acknowledged");
    wait_for_reply_containing(&platform, "暂时读不到终态改动，请稍后再试。").await;
    assert!(file_messages(&platform).await.is_empty(), "nothing was sent");

    backend
        .fail_session_diff
        .store(false, std::sync::atomic::Ordering::SeqCst);
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
    assert_eq!(file_uploads(&platform).await.len(), 1, "one retried upload");
}

/// V1 has no `session.diff`: the unreleased feature's legacy button lands on
/// the same read-failure copy, never on a tool-metadata fallback (ADR-0087).
#[tokio::test]
async fn a_generation_without_the_endpoint_answers_the_read_failure() {
    let (_dir, cfg) = app_config();
    let backend = MockBackend::new(Vec::new());
    let (app, platform) = build_app(cfg, backend).await;

    app.host_action_on_message(click(), CARD_MESSAGE)
        .await
        .expect("the click is acknowledged");
    wait_for_reply_containing(&platform, "暂时读不到终态改动，请稍后再试。").await;

    assert!(file_uploads(&platform).await.is_empty(), "no upload");
    assert!(file_messages(&platform).await.is_empty(), "no File Message");
}

/// A failed SETTLE read is not an error card: the Turn still ends, it simply
/// carries no index (ADR-0087). The read happened and was abandoned.
#[tokio::test]
async fn a_failed_settle_read_leaves_the_terminal_card_without_an_index() {
    let _wd = test_work_dir();
    let timeline = vec![
        super::drain::user("msg_cola_anchor", 1_000, "第一条消息"),
        super::drain::assistant(2_000, "完成。"),
    ];
    let (_dir, app, backend, platform) = super::drain::scripted_app(
        vec![SessionTranscript::new(timeline)],
        Some(crate::backend::types::SessionStatus::Idle),
    )
    .await;
    backend
        .session_diff_available
        .store(true, std::sync::atomic::Ordering::SeqCst);
    backend
        .fail_session_diff
        .store(true, std::sync::atomic::Ordering::SeqCst);

    let turn = super::drain::spawn_turn(&app, super::drain::ctx("ses_test", "第一条消息"));
    tokio::time::timeout(Duration::from_secs(5), turn)
        .await
        .expect("the turn ends despite the failed index read")
        .expect("the turn task does not panic")
        .expect("the turn succeeded");

    let mut cards = platform.sent_cards().await;
    cards.extend(platform.updated_cards().await);
    assert!(
        cards.iter().all(|card| !has_turn_diff_index(card)),
        "a failed read renders no index: {cards:?}"
    );
    assert_eq!(
        backend.session_diff_calls.lock().await.len(),
        1,
        "the settle read was attempted once"
    );
}

/// V1 has no `session.diff` endpoint, so its terminal card renders no index
/// and no button (ADR-0087). The settle read is still attempted once (the mock
/// records it) and answers `Ok(None)`.
#[tokio::test]
async fn a_v1_turn_terminates_without_an_index() {
    let _wd = test_work_dir();
    let timeline = vec![
        super::drain::user("msg_cola_anchor", 1_000, "第一条消息"),
        super::drain::assistant(2_000, "完成。"),
    ];
    let (_dir, app, backend, platform) = super::drain::scripted_app(
        vec![SessionTranscript::new(timeline)],
        Some(crate::backend::types::SessionStatus::Idle),
    )
    .await;

    let turn = super::drain::spawn_turn(&app, super::drain::ctx("ses_test", "第一条消息"));
    tokio::time::timeout(Duration::from_secs(5), turn)
        .await
        .expect("the turn ends")
        .expect("the turn task does not panic")
        .expect("the turn succeeded");

    let mut cards = platform.sent_cards().await;
    cards.extend(platform.updated_cards().await);
    assert!(
        cards.iter().all(|card| !has_turn_diff_index(card)),
        "a V1 terminal card renders no index: {cards:?}"
    );
    assert_eq!(
        backend.session_diff_calls.lock().await.len(),
        1,
        "the settle read was attempted; V1 answers Ok(None)"
    );
}
