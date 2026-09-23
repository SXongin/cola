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
/// and in its handle. Returns the `seen` set that sweep used, so a caller can
/// run the NEXT sweep with the poll loop's memory intact (a fresh set would
/// surface the request as new instead of re-hosting it).
async fn seed_old_turn_card_with_permission(app: &Arc<App>) -> std::collections::HashSet<String> {
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
    seen
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

/// The latest card the platform sent to `message_id`, as a string.
async fn last_card_of(platform: &RecordingPlatform, message_id: &str) -> String {
    platform
        .calls
        .lock()
        .await
        .iter()
        .rev()
        .find_map(|c| match c {
            PlatformCall::UpdateMessage {
                message_id: mid,
                card,
            } if mid == message_id => Some(card.to_string()),
            _ => None,
        })
        .unwrap_or_else(|| panic!("no update for {message_id}"))
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
        ack.contains("✅ 已允许一次：⚡ 执行 Shell 命令 `ls -la`"),
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
    // Resolving the last live block must clear the waiting header in the ack
    // itself — the cached card's header was captured while the block was
    // still live, and leaving it to the next render-poll flush left the card
    // visibly "still waiting" for ~2 s after the click.
    assert!(
        !ack.contains("等待你的"),
        "the ack must clear the awaiting title: {ack}"
    );
    assert!(
        ack.contains("✅ 完成"),
        "the ack header must show the accumulator's post-resolution state: {ack}"
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
        ack.contains("✅ 已允许一次：⚡ 执行 Shell 命令 `ls -la`"),
        "receipt missing: {ack}"
    );
    assert!(
        !ack.contains("🔐 **权限请求**") && !ack.contains("始终允许"),
        "the resolved block (and its controls) must be gone: {ack}"
    );
    assert!(ack.contains("旧回合的推理。"), "the ack is the OLD card: {ack}");
    assert!(!ack.contains("新回合"), "never the new accumulator's card: {ack}");
    // The restamp is gated on the accumulator actually resolving a block: the
    // new turn never carried this one, so its live header must not leak onto
    // the old card.
    assert!(
        !ack.contains("思考中"),
        "the old card must not wear the NEW turn's live header: {ack}"
    );
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
    crate::bridge::turn::Turn::flush_card(&app.cards_handle(), "ses_split").await;

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
    crate::bridge::turn::Turn::flush_card(&app.cards_handle(), "ses_q").await;
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

/// #177 (ADR-0038, rule 1): a still-pending request that outlived its turn
/// re-hosts onto the session's current card on the next sweep — the new card
/// carries its controls, the old card is repainted without them, and the block
/// stays answerable on the new card. No duplicate: the controls render on
/// exactly one card.
#[tokio::test]
async fn sweep_rehosts_a_pending_block_onto_the_new_turn_card() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let mut backend = MockBackend::new(realistic_parts());
    backend.permissions = vec![perm_request("per_old", "ses_old", "ls -la")];
    let backend = Arc::new(backend);
    let platform = Arc::new(RecordingPlatform::new());
    let app = Arc::new(App::new(cfg, backend.clone(), platform.clone()).unwrap());
    seed_session(&app, "ses_old", "/work").await;

    // The old turn's card carries the block; a new turn then replaced the
    // accumulator. The poll loop has already seen the request.
    let mut seen = seed_old_turn_card_with_permission(&app).await;
    assert_eq!(
        app.card_handles.lock().await.message_of("per_old"),
        Some("om_old"),
        "precondition: the old card carries the block before the re-host"
    );

    app.permission.sweep(&app.core, &mut seen).await;

    assert_eq!(
        app.card_handles.lock().await.message_of("per_old"),
        Some("om_new"),
        "the handle follows the new turn's card"
    );
    let new_card = last_card_of(&platform, "om_new").await;
    assert!(
        new_card.contains("🔐 **权限请求**") && new_card.contains("允许一次"),
        "the new card carries the controls: {new_card}"
    );
    assert!(
        new_card.contains("新回合"),
        "it is the new turn's card: {new_card}"
    );
    let old_card = last_card_of(&platform, "om_old").await;
    assert!(
        !old_card.contains("🔐 **权限请求**"),
        "the old card lost the controls: {old_card}"
    );
    assert!(
        old_card.contains("旧回合的推理。"),
        "the old card keeps its streamed content: {old_card}"
    );
    assert!(
        !old_card.contains("已允许") && !old_card.contains("已由其他客户端处理"),
        "a moved block leaves no receipt: {old_card}"
    );

    // Answerable on the new card: the click replies and settles the handles.
    let result = app
        .host_action(serde_json::json!({
            "action": "perm",
            "reply": "once",
            "session_id": "ses_old",
            "directory": "/work",
            "request_id": "per_old",
            "open_message_id": "om_new",
            "perm_label": "✅ 已允许一次",
            "perm_color": "green",
            "perm_body": "bash",
        }))
        .await
        .expect("a card-action result");
    assert!(
        ack_text(&result).contains("✅ 已允许一次"),
        "the re-hosted block resolves from the new card"
    );
    let handles = app.card_handles.lock().await;
    assert_eq!(handles.live_count(), 0, "no orphaned live block after a resolve");
    assert_eq!(handles.cached_count(), 0);
    assert_eq!(backend.reply_permission_calls.lock().await.len(), 1);
}

/// #177: re-hosting preserves a question's partial answers — the new turn's
/// card shows the same 已选 markers, the old card loses the controls, and the
/// remaining question stays answerable on the new card.
#[tokio::test]
async fn rehost_preserves_a_questions_partial_answers() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let mut backend = MockBackend::new(realistic_parts());
    let request = question_request("que_old", "ses_q");
    backend.questions = vec![request.clone()];
    let backend = Arc::new(backend);
    let platform = Arc::new(RecordingPlatform::new());
    let app = Arc::new(App::new(cfg, backend.clone(), platform.clone()).unwrap());
    seed_session(&app, "ses_q", "/work").await;

    // The old turn's card carries the question block; the Host answers 目录
    // only (the block stays live with its 已选 marker), then a new turn
    // replaces the accumulator.
    let mut old = StreamAccumulator::new("旧问题回合");
    old.card_state = CardState::Done;
    old.push_text("旧问题回合的内容。");
    old.reply_to_message_id = Some("msg_1".into());
    old.add_interaction(question_block("que_old", "ses_q", "/work"));
    app.cards
        .lock()
        .await
        .insert("ses_q".into(), CardSession::new(old, Some("om_q".into())));
    crate::bridge::turn::Turn::flush_card(&app.cards_handle(), "ses_q").await;
    app.question.remember_question(&request, "/work").await;
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
    assert!(ack_text(&r1).contains("已选：/a"), "partial answer recorded");
    app.cards.lock().await.insert(
        "ses_q".into(),
        CardSession::new(StreamAccumulator::new("新回合"), Some("om_new".into())),
    );

    // The poll loop has seen the request: the sweep re-hosts instead of
    // surfacing it anew.
    let mut seen: std::collections::HashSet<String> = ["que_old".to_string()].into_iter().collect();
    app.question.sweep(&app.core, &mut seen).await;

    assert_eq!(
        app.card_handles.lock().await.message_of("que_old"),
        Some("om_new"),
        "the handle follows the new turn's card"
    );
    let new_card = last_card_of(&platform, "om_new").await;
    assert!(
        new_card.contains("已选：/a"),
        "the partial answer survives the move: {new_card}"
    );
    assert!(
        new_card.contains("选分支"),
        "the still-open question moved too: {new_card}"
    );
    let old_card = last_card_of(&platform, "om_q").await;
    assert!(
        !old_card.contains("已选：/a") && !old_card.contains("选目录"),
        "the old card lost the controls: {old_card}"
    );
    assert!(
        old_card.contains("旧问题回合的内容。"),
        "the old card keeps its streamed content: {old_card}"
    );

    // The remaining question stays answerable on the new card.
    let r2 = app
        .host_action(serde_json::json!({
            "action": "question",
            "reply": "answer",
            "request_id": "que_old",
            "session_id": "ses_q",
            "directory": "/work",
            "question_index": 1,
            "answer": "main",
            "open_message_id": "om_new",
        }))
        .await
        .expect("a card-action result");
    assert!(
        ack_text(&r2).contains("✅ 已回答：目录 /a、分支 main"),
        "the receipt names both answers: {}",
        ack_text(&r2)
    );
    let handles = app.card_handles.lock().await;
    assert_eq!(handles.live_count(), 0, "no orphaned live block after a resolve");
    assert_eq!(handles.cached_count(), 0);
    assert_eq!(backend.reply_question_calls.lock().await.len(), 1);
}

/// Two flushes racing a split (`flush_card` is called from the render poll,
/// the request poller and click acks) must not duplicate a card. Freezing the
/// first flush's continuation send leaves the window: the second flush builds
/// the continuation slice (`render_from` is already advanced) while the card
/// id still names the finalized card, and PATCHes that slice onto it — two
/// identical messages with live controls, only one of them tracked.
#[tokio::test]
async fn a_concurrent_flush_leaves_the_tail_on_one_card() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let backend = Arc::new(MockBackend::new(realistic_parts()));
    let platform = Arc::new(RecordingPlatform::new());
    let app = Arc::new(App::new(cfg, backend, platform.clone()).unwrap());

    // A card far over the component budget with a live permission in its tail:
    // a flush finalizes it and sends the continuation that carries the block.
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

    // Freeze the continuation send: the finalized patch has been sent, the
    // continuation is not yet registered as the session's card.
    let (entered, release) = platform.pause("reply", "msg_1");
    let first = {
        let app = app.clone();
        tokio::spawn(
            async move { crate::bridge::turn::Turn::flush_card(&app.cards_handle(), "ses_split").await },
        )
    };
    entered.notified().await;

    // The render poll's flush now races the request poller's. Without a
    // serialized card write it completes against the stale card id; with one
    // it parks until the first flush is done.
    let finished = Arc::new(tokio::sync::Notify::new());
    let second = {
        let app = app.clone();
        let finished = finished.clone();
        tokio::spawn(async move {
            crate::bridge::turn::Turn::flush_card(&app.cards_handle(), "ses_split").await;
            finished.notify_one();
        })
    };
    let parked = tokio::time::timeout(std::time::Duration::from_millis(200), finished.notified()).await;
    release.notify_one();
    first.await.unwrap();
    if parked.is_err() {
        finished.notified().await;
    }
    second.await.unwrap();

    // The finalized card must end WITHOUT the tail: the racing flush PATCHed
    // the continuation's content (the block) onto it.
    let finalized = last_card_of(&platform, "om_filled").await;
    assert!(
        !finalized.contains("🔐 **权限请求**"),
        "the finalized card was overwritten with the continuation's tail: {finalized}"
    );
    // The block renders on exactly one card: the continuation.
    let continuations = platform.replied_cards().await;
    assert_eq!(continuations.len(), 1, "exactly one continuation card");
    assert!(
        continuations[0].to_string().contains("🔐 **权限请求**"),
        "the continuation carries the block: {}",
        continuations[0]
    );
    assert_eq!(
        app.card_handles.lock().await.message_of("per_split"),
        Some("msg_reply"),
        "the handle follows the continuation"
    );
}

/// A resolution racing an in-flight flush must not be resurrected: freezing
/// the flush inside its PATCH, then letting the click resolve, leaves the
/// stale snapshot to record and re-PATCH the just-resolved block — which the
/// sweep reads as "another client handled it". That was the auto-accept
/// sequence the Host saw (a `⏱` line before the mode receipt).
#[tokio::test]
async fn a_resolution_racing_an_in_flight_flush_is_not_resurrected() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let mut backend = MockBackend::new(realistic_parts());
    backend.permissions = vec![perm_request("per_live", "ses_live", "ls -la")];
    let backend = Arc::new(backend);
    let platform = Arc::new(RecordingPlatform::new());
    let app = Arc::new(App::new(cfg, backend.clone(), platform.clone()).unwrap());
    seed_session(&app, "ses_live", "/work").await;

    // The live card carries the inline permission (the poller path).
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

    // A render-poll flush takes its snapshot (the block is live) and parks in
    // the PATCH.
    let (entered, release) = platform.pause("update", "om_live");
    let flush = {
        let app = app.clone();
        tokio::spawn(
            async move { crate::bridge::turn::Turn::flush_card(&app.cards_handle(), "ses_live").await },
        )
    };
    entered.notified().await;

    // The Host clicks 允许一次 while that PATCH is in flight. Without the lock
    // the click resolves immediately; with it the click waits its turn.
    let click = {
        let app = app.clone();
        tokio::spawn(async move {
            app.host_action(serde_json::json!({
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
        })
    };
    release.notify_one();
    flush.await.unwrap();
    let result = click.await.unwrap().expect("a card-action result");
    assert!(
        ack_text(&result).contains("✅ 已允许一次：⚡ 执行 Shell 命令 `ls -la`"),
        "the click ack carries the decision: {}",
        ack_text(&result)
    );

    // The request left the pending list when cola replied: the next sweep must
    // NOT read that as another client's resolution and stamp the neutral line.
    let mut seen = std::collections::HashSet::new();
    app.permission.sweep(&app.core, &mut seen).await;
    let neutral = platform.calls.lock().await.iter().any(|c| {
        matches!(
            c,
            PlatformCall::UpdateMessage { message_id, card }
                if message_id == "om_live" && card.to_string().contains("⏱ 已由其他客户端处理")
        )
    });
    assert!(
        !neutral,
        "cola's own decision must not be reported as handled elsewhere"
    );
    let handles = app.card_handles.lock().await;
    assert_eq!(handles.live_count(), 0, "no live block survives the resolution");
    assert_eq!(handles.cached_count(), 0, "no cache survives the resolution");
}
