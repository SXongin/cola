//! Card handles (ADR-0038, rule 2): a card that shows a live interaction block
//! stays repaintable — a click acks the clicked card, a remote resolution
//! repaints a non-current card from its cached JSON, and a split moves the
//! handle onto the continuation that carries the block.

use std::sync::Arc;

use crate::bridge::streaming::{
    CardSession, InteractionBlock, PendingPermission, PendingQuestion, StreamAccumulator,
};
use crate::bridge::test_support::*;
use crate::feishu::card::CardState;
use crate::feishu::card::tool_render::ToolPanel;

/// The permission block the poller would inline on the old turn's card.
fn permission_block(request_id: &str, session_id: &str, directory: &str) -> InteractionBlock {
    let p = perm_request(request_id, session_id, "ls -la");
    InteractionBlock::Permission(PendingPermission {
        session_id: session_id.into(),
        request_id: request_id.into(),
        body: crate::bridge::request::describe_permission(&p),
        target: crate::bridge::request::permission_target(&p),
        directory: directory.into(),
    })
}

/// The question block the poller would inline on the old turn's card.
fn question_block(request_id: &str, session_id: &str, directory: &str) -> InteractionBlock {
    let q = question_request(request_id, session_id);
    InteractionBlock::Question(PendingQuestion {
        request_id: request_id.into(),
        session_id: session_id.into(),
        questions: q.questions,
        directory: directory.into(),
        answers: vec![None; 2],
        done: vec![false; 2],
    })
}

/// A two-question request the QUESTION flow can replay and reply to.
fn question_request(request_id: &str, session_id: &str) -> crate::opencode::types::QuestionRequest {
    use crate::opencode::types::{QuestionInfo, QuestionOption, QuestionRequest};
    let question = |text: &str, header: &str, label: &str| QuestionInfo {
        question: text.into(),
        header: header.into(),
        options: vec![QuestionOption {
            label: label.into(),
            description: String::new(),
        }],
        multiple: None,
        custom: None,
    };
    QuestionRequest {
        id: request_id.into(),
        session_id: session_id.into(),
        questions: vec![
            question("选目录", "目录", "/a"),
            question("选分支", "分支", "main"),
        ],
    }
}

/// Seed the old turn's card and let one sweep inline `per_old` onto it — the
/// poller path, so the card handle records the block. Then replace the
/// accumulator, as a new turn does: the block survives only on the OLD card
/// and in its handle.
async fn seed_old_turn_card_with_permission(app: &Arc<App>) {
    let mut old = StreamAccumulator::new("旧回合");
    old.card_state = CardState::Done;
    old.push_text("旧回合的推理。");
    old.reply_to_message_id = Some("msg_1".into());
    app.cards
        .lock()
        .await
        .insert("ses_old".into(), CardSession::new(old, Some("om_old".into())));

    let mut seen = std::collections::HashSet::new();
    app.permission.sweep(&app.core, &mut seen).await;

    app.cards.lock().await.insert(
        "ses_old".into(),
        CardSession::new(StreamAccumulator::new("新回合"), Some("om_new".into())),
    );
}

/// How many times the platform patched `message_id` in place.
async fn patches_of(platform: &RecordingPlatform, message_id: &str) -> usize {
    platform
        .calls
        .lock()
        .await
        .iter()
        .filter(|c| matches!(c, PlatformCall::UpdateMessage { message_id: mid, .. } if mid == message_id))
        .count()
}

/// The click's ack card as a string.
fn ack_text(result: &crate::bridge::handler::CardActionResult) -> String {
    result
        .card
        .as_ref()
        .expect("the ack must carry a card")
        .to_string()
}

/// A click WITH its `open_message_id` (the card the Host actually clicked)
/// updates THAT card through its handle, atomically in the ack — no PATCH race
/// and no dependence on which card the accumulator currently owns.
#[tokio::test]
async fn click_with_open_message_id_acks_the_cached_card() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let mut backend = MockBackend::new(realistic_parts());
    backend.permissions = vec![perm_request("per_live", "ses_live", "ls -la")];
    let backend = Arc::new(backend);
    let platform = Arc::new(RecordingPlatform::new());
    let app = Arc::new(App::new(cfg, backend.clone(), platform.clone()).unwrap());
    seed_session(&app, "ses_live", "/work").await;

    // The live card carries an inline permission (the poller path).
    let mut acc = StreamAccumulator::new("回合");
    acc.card_state = CardState::Done;
    acc.push_text("回合的内容。");
    acc.reply_to_message_id = Some("msg_1".into());
    app.cards
        .lock()
        .await
        .insert("ses_live".into(), CardSession::new(acc, Some("om_live".into())));
    let mut seen = std::collections::HashSet::new();
    app.permission.sweep(&app.core, &mut seen).await;
    assert_eq!(
        app.card_handles.lock().await.message_of("per_live"),
        Some("om_live"),
        "the handle records the card that renders the block"
    );
    let patches_before = patches_of(&platform, "om_live").await;

    let result = app
        .host_action(serde_json::json!({
            "action": "perm",
            "reply": "once",
            "session_id": "ses_live",
            "directory": "/work",
            "request_id": "per_live",
            "open_message_id": "om_live",
            "perm_label": "✅ 已允许一次",
            "perm_color": "green",
            "perm_body": "bash",
        }))
        .await
        .expect("a card-action result");
    let ack = ack_text(&result);
    assert!(
        ack.contains("✅ 已允许一次：⚡ 执行 Shell 命令 `ls -la` · "),
        "receipt missing: {ack}"
    );
    assert!(
        !ack.contains("🔐 **权限请求**") && !ack.contains("始终允许"),
        "the resolved block (and its controls) must be gone: {ack}"
    );
    assert!(
        ack.contains("回合的内容。"),
        "the ack is the clicked card's own JSON: {ack}"
    );
    assert_eq!(
        patches_of(&platform, "om_live").await,
        patches_before,
        "the ack IS the update; no PATCH may race behind it"
    );
    let handles = app.card_handles.lock().await;
    assert_eq!(handles.live_count(), 0, "no orphaned live block after a resolve");
    assert_eq!(
        handles.cached_count(),
        0,
        "the cache is released with its last block"
    );
    assert_eq!(backend.reply_permission_calls.lock().await.len(), 1);
}

/// A click on a card that is NOT the accumulator's current card (a request
/// that outlived its turn) still updates the CLICKED card: the ack carries the
/// old card's JSON with the receipt, never the new accumulator's card.
#[tokio::test]
async fn click_updates_a_non_current_card_through_its_handle() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let mut backend = MockBackend::new(realistic_parts());
    backend.permissions = vec![perm_request("per_old", "ses_old", "ls -la")];
    let backend = Arc::new(backend);
    let platform = Arc::new(RecordingPlatform::new());
    let app = Arc::new(App::new(cfg, backend.clone(), platform.clone()).unwrap());
    seed_session(&app, "ses_old", "/work").await;
    seed_old_turn_card_with_permission(&app).await;
    let patches_before = patches_of(&platform, "om_old").await;

    let result = app
        .host_action(serde_json::json!({
            "action": "perm",
            "reply": "once",
            "session_id": "ses_old",
            "directory": "/work",
            "request_id": "per_old",
            "open_message_id": "om_old",
            "perm_label": "✅ 已允许一次",
            "perm_color": "green",
            "perm_body": "bash",
        }))
        .await
        .expect("a card-action result");
    let ack = ack_text(&result);
    assert!(
        ack.contains("✅ 已允许一次：⚡ 执行 Shell 命令 `ls -la` · "),
        "receipt missing: {ack}"
    );
    assert!(
        !ack.contains("🔐 **权限请求**") && !ack.contains("始终允许"),
        "the resolved block (and its controls) must be gone: {ack}"
    );
    assert!(ack.contains("旧回合的推理。"), "the ack is the OLD card: {ack}");
    assert!(!ack.contains("新回合"), "never the new accumulator's card: {ack}");
    assert_eq!(
        patches_of(&platform, "om_old").await,
        patches_before,
        "the ack IS the update; the old card must not be patched behind it"
    );
    let handles = app.card_handles.lock().await;
    assert_eq!(handles.live_count(), 0, "no orphaned live block after a resolve");
    assert_eq!(handles.cached_count(), 0);
    assert_eq!(backend.reply_permission_calls.lock().await.len(), 1);
}

/// A remote resolution (another client answered) repaints a non-current card
/// from its cached JSON within one sweep — after the turn ended nothing else
/// would.
#[tokio::test]
async fn sweep_repaints_a_non_current_card_from_its_cache() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let mut backend = MockBackend::new(realistic_parts());
    backend.permissions = vec![perm_request("per_old", "ses_old", "ls -la")];
    let backend = Arc::new(backend);
    let platform = Arc::new(RecordingPlatform::new());
    let app = Arc::new(App::new(cfg, backend.clone(), platform.clone()).unwrap());
    seed_session(&app, "ses_old", "/work").await;
    seed_old_turn_card_with_permission(&app).await;

    // Resolved by another client: the request leaves the pending list.
    backend.replied_permissions.lock().await.insert("per_old".into());
    let mut seen = std::collections::HashSet::new();
    app.permission.sweep(&app.core, &mut seen).await;

    let calls = platform.calls.lock().await.clone();
    let repaint = calls
        .iter()
        .rev()
        .find_map(|c| match c {
            PlatformCall::UpdateMessage { message_id, card } if message_id == "om_old" => {
                Some(card.to_string())
            }
            _ => None,
        })
        .expect("the old card must be repainted from its cache");
    assert!(
        repaint.contains("⏱ 已由其他客户端处理：⚡ 执行 Shell 命令 `ls -la`"),
        "neutral receipt missing: {repaint}"
    );
    assert!(
        !repaint.contains("🔐 **权限请求**"),
        "the resolved block must be gone: {repaint}"
    );
    assert!(
        repaint.contains("旧回合的推理。"),
        "the repaint keeps the card's streamed content: {repaint}"
    );
    let handles = app.card_handles.lock().await;
    assert_eq!(
        handles.live_count(),
        0,
        "no orphaned live block after a sweep resolve"
    );
    assert_eq!(handles.cached_count(), 0);
}

/// After a split the registry points at the continuation card that carries the
/// block; the finalized slice's cache is released, and a vanished block is
/// repainted on the continuation (never on the frozen slice).
#[tokio::test]
async fn a_split_registers_the_continuation_card() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let backend = Arc::new(MockBackend::new(realistic_parts()));
    let platform = Arc::new(RecordingPlatform::new());
    let app = Arc::new(App::new(cfg, backend, platform.clone()).unwrap());

    // A card far over the component budget with a live permission in its tail:
    // the flush finalizes the filled card and sends a continuation carrying the
    // block.
    let mut acc = StreamAccumulator::new("test");
    acc.card_state = CardState::Done;
    for i in 0..50 {
        acc.push_tool(
            &format!("call_{i}"),
            ToolPanel {
                name: format!("tool{i}"),
                status: "completed".into(),
                input: None,
                output: None,
            },
        );
    }
    acc.add_interaction(permission_block("per_split", "ses_split", "/work"));
    acc.reply_to_message_id = Some("msg_1".into());
    app.cards.lock().await.insert(
        "ses_split".into(),
        CardSession::new(acc, Some("om_filled".into())),
    );
    crate::bridge::render::flush_card(&app.core, "ses_split").await;

    {
        let handles = app.card_handles.lock().await;
        assert_eq!(
            handles.message_of("per_split"),
            Some("msg_reply"),
            "the block follows the continuation card"
        );
        assert_eq!(handles.live_count(), 1);
        assert_eq!(
            handles.cached_count(),
            1,
            "only the card that renders the block keeps a cache"
        );
    }
    let calls = platform.calls.lock().await.clone();
    let filled = calls
        .iter()
        .find_map(|c| match c {
            PlatformCall::UpdateMessage { message_id, card } if message_id == "om_filled" => {
                Some(card.to_string())
            }
            _ => None,
        })
        .expect("the filled card was updated");
    assert!(
        !filled.contains("🔐 **权限请求**"),
        "the finalized slice must not carry the tail: {filled}"
    );
    let continuation = calls
        .iter()
        .find_map(|c| match c {
            PlatformCall::ReplyCard { card, .. } => Some(card.to_string()),
            _ => None,
        })
        .expect("a continuation card was sent");
    assert!(
        continuation.contains("🔐 **权限请求**") && continuation.contains("允许一次"),
        "the continuation carries the block: {continuation}"
    );

    // The turn is gone (a replaced/aborted turn): the sweep resolves the block
    // from the continuation's cache, not the frozen slice.
    let filled_patches = patches_of(&platform, "om_filled").await;
    app.cards.lock().await.remove("ses_split");
    let mut seen = std::collections::HashSet::new();
    app.permission.sweep(&app.core, &mut seen).await;

    let calls = platform.calls.lock().await.clone();
    assert!(
        calls.iter().any(|c| matches!(
            c,
            PlatformCall::UpdateMessage { message_id, card }
                if message_id == "msg_reply"
                    && card.to_string().contains("⏱ 已由其他客户端处理")
        )),
        "the continuation is repainted with the receipt: {calls:?}"
    );
    assert_eq!(
        patches_of(&platform, "om_filled").await,
        filled_patches,
        "the frozen slice must not be patched again: {calls:?}"
    );
    let handles = app.card_handles.lock().await;
    assert_eq!(handles.live_count(), 0);
    assert_eq!(handles.cached_count(), 0);
}

/// A partial answer click on a card that is no longer the accumulator's
/// current card refreshes THAT card's cached JSON (the 已选 markers), carried
/// in the ack.
#[tokio::test]
async fn partial_question_answer_refreshes_the_clicked_card() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let backend = Arc::new(MockBackend::new(realistic_parts()));
    let platform = Arc::new(RecordingPlatform::new());
    let app = Arc::new(App::new(cfg, backend.clone(), platform.clone()).unwrap());
    let request = question_request("que_old", "ses_q");

    // The old turn's card carries the question block (the poller path would
    // flush the same state); then a new turn replaces the accumulator.
    let mut old = StreamAccumulator::new("旧问题回合");
    old.card_state = CardState::Done;
    old.push_text("旧问题回合的内容。");
    old.reply_to_message_id = Some("msg_1".into());
    old.add_interaction(question_block("que_old", "ses_q", "/work"));
    app.cards
        .lock()
        .await
        .insert("ses_q".into(), CardSession::new(old, Some("om_q".into())));
    crate::bridge::render::flush_card(&app.core, "ses_q").await;
    app.question.remember_question(&request, "/work").await;
    app.cards.lock().await.insert(
        "ses_q".into(),
        CardSession::new(StreamAccumulator::new("新回合"), Some("om_new".into())),
    );

    let r1 = app
        .host_action(serde_json::json!({
            "action": "question",
            "reply": "answer",
            "request_id": "que_old",
            "session_id": "ses_q",
            "directory": "/work",
            "question_index": 0,
            "answer": "/a",
            "open_message_id": "om_q",
        }))
        .await
        .expect("a card-action result");
    let ack1 = ack_text(&r1);
    assert!(ack1.contains("已选：/a"), "marker missing: {ack1}");
    assert!(
        ack1.contains("旧问题回合的内容。"),
        "the ack is the clicked (old) card, refreshed: {ack1}"
    );
    assert!(
        !ack1.contains("新回合"),
        "never the new accumulator's card: {ack1}"
    );
    assert_eq!(
        app.card_handles.lock().await.live_count(),
        1,
        "the block stays live while a question remains open"
    );

    let r2 = app
        .host_action(serde_json::json!({
            "action": "question",
            "reply": "answer",
            "request_id": "que_old",
            "session_id": "ses_q",
            "directory": "/work",
            "question_index": 1,
            "answer": "main",
            "open_message_id": "om_q",
        }))
        .await
        .expect("a card-action result");
    let ack2 = ack_text(&r2);
    assert!(
        ack2.contains("✅ 已回答：目录 /a、分支 main"),
        "receipt missing: {ack2}"
    );
    assert!(
        !ack2.contains("无法回答") && !ack2.contains("已选："),
        "the resolved block (and its controls) must be gone: {ack2}"
    );
    let handles = app.card_handles.lock().await;
    assert_eq!(handles.live_count(), 0, "no orphaned live block after a resolve");
    assert_eq!(handles.cached_count(), 0);
    assert_eq!(backend.reply_question_calls.lock().await.len(), 1);
}
