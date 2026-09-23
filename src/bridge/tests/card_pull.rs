//! `/card` pull-command splits (ADR-0043, 2026-09-22 amendment): the explicit
//! user request that brings the live card back to the newest position after
//! command replies bury it. It reuses the Supplement split machinery — the
//! previous card finalized with the standard split header, a continuation
//! replied to the command message tracking as the new live card — but there is
//! no Backend send, no text acknowledgement, and the continuation's status line
//! says the card moved.

use std::sync::Arc;

use crate::bridge::test_support::*;
use crate::feishu::card::CardState;

/// A Turn that already finished leaves its card session in `cards` with a Done
/// state until the next Turn replaces it — `/card` must not treat that as a
/// live card: it answers the notice and resends nothing.
#[tokio::test]
async fn card_command_after_the_turn_finished_replies_a_notice() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let (app, platform) = build_app(cfg, MockBackend::new(realistic_parts())).await;
    seed_session(&app, "ses_test", "/work").await;
    seed_live_card(&app, "ses_test", "已完成的回合。").await;
    Turn::set_card_state(&app.cards_handle(), "ses_test", CardState::Done).await;

    app.handle_message(incoming(
        "msg_card".into(),
        "chat_1".into(),
        "p2p".into(),
        None,
        "/card".into(),
        None,
    ))
    .await;

    let texts = platform.texts().await;
    assert_eq!(
        texts.len(),
        1,
        "a finished Turn has no live card to pull: {texts:?}"
    );
    assert!(
        texts[0].contains("当前没有正在运行的实时卡片"),
        "the notice must say there is nothing to pull: {texts:?}"
    );
    let calls = platform.calls.lock().await.clone();
    assert!(
        !calls.iter().any(|c| matches!(
            c,
            PlatformCall::UpdateMessage { message_id, .. } if message_id == "om_live"
        )),
        "the finished card must not be finalized again: {calls:?}"
    );
    assert!(platform.replied_cards().await.is_empty(), "no card created");
}

/// `/card` with nothing to pull — no session at all, or a session whose Turn
/// is not rendering — replies one text line and creates no card.
#[tokio::test]
async fn card_command_without_a_live_card_replies_a_notice() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let (app, platform) = build_app(cfg, MockBackend::new(realistic_parts())).await;

    // No session mapped to the conversation yet.
    app.handle_message(incoming(
        "msg_card_1".into(),
        "chat_1".into(),
        "p2p".into(),
        None,
        "/card".into(),
        None,
    ))
    .await;

    // A mapped session exists, but no Turn is rendering a card.
    seed_session(&app, "ses_test", "/work").await;
    app.handle_message(incoming(
        "msg_card_2".into(),
        "chat_1".into(),
        "p2p".into(),
        None,
        "/card".into(),
        None,
    ))
    .await;

    let texts = platform.texts().await;
    assert_eq!(
        texts.len(),
        2,
        "each /card without a live card answers a notice: {texts:?}"
    );
    assert!(
        texts.iter().all(|t| t.contains("当前没有正在运行的实时卡片")),
        "the notice must say there is nothing to pull: {texts:?}"
    );
    assert!(
        platform.replied_cards().await.is_empty(),
        "no card may be created: {:?}",
        platform.calls.lock().await
    );
}

/// A pending permission inlined on the pulled card migrates to the
/// continuation (the Supplement split's path): the previous card's controls are
/// settled, the block stays live on the continuation, and the card handle
/// follows it.
#[tokio::test]
async fn card_pull_migrates_a_pending_permission() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let mut backend = MockBackend::new(realistic_parts());
    backend.permissions = vec![perm_request("per_live", "ses_test", "ls -la")];
    let backend = Arc::new(backend);
    let platform = Arc::new(RecordingPlatform::new());
    let app = Arc::new(App::new(cfg, backend.clone(), platform.clone()).unwrap());
    seed_session(&app, "ses_test", "/work").await;

    // The live card carries the inline permission (the poller path), so the
    // card handle records the block before the pull.
    seed_live_card(&app, "ses_test", "回合的内容。").await;
    let mut seen = std::collections::HashSet::new();
    app.permission.sweep(&app.core, &mut seen).await;
    assert_eq!(
        app.card_handles.lock().await.message_of("per_live"),
        Some("om_live"),
        "precondition: the live card shows the block"
    );

    app.handle_message(incoming(
        "msg_card".into(),
        "chat_1".into(),
        "p2p".into(),
        None,
        "/card".into(),
        None,
    ))
    .await;

    // The previous card keeps its content but loses the controls.
    let finalized = last_update_of(&platform, "om_live").await;
    assert!(
        !finalized.contains("🔐 **权限请求**") && !finalized.contains("允许一次"),
        "the previous card's controls must be settled, not left dead: {finalized}"
    );
    assert!(
        finalized.contains("回合的内容。"),
        "the finalized card keeps its streamed content: {finalized}"
    );

    // The continuation carries the live block and its handle points at it.
    let (reply_to, card) = continuation(&platform).await;
    assert_eq!(reply_to, "msg_card");
    assert!(
        has(&card, "🔐 **权限请求**") && has(&card, "允许一次"),
        "the live block must ride the continuation: {card}"
    );
    assert!(has(&card, "⏬ 实时卡片已移到底部"));
    assert!(
        !has(&card, "回合的内容。"),
        "the continuation carries the delta, not the finalized content: {card}"
    );
    assert_eq!(
        app.card_handles.lock().await.message_of("per_live"),
        Some("msg_reply"),
        "the handle follows the continuation"
    );
}

/// Seed the session's live card (content `text`, id `om_live`), so `/card` has
/// something to pull down.
async fn seed_live_card(app: &Arc<App>, session_id: &str, text: &str) {
    let cards = app.cards_handle();
    Turn::seed_card(&cards, session_id, Some("om_live")).await;
    Turn::set_card_state(&cards, session_id, CardState::Streaming).await;
    Turn::push_text(&cards, session_id, text).await;
    Turn::set_reply_target(&cards, session_id, "msg_1").await;
}

/// The latest continuation card (`ReplyCard`) the platform recorded.
async fn continuation(platform: &RecordingPlatform) -> (String, serde_json::Value) {
    platform
        .calls
        .lock()
        .await
        .iter()
        .rev()
        .find_map(|c| match c {
            PlatformCall::ReplyCard { reply_to, card } => Some((reply_to.clone(), card.clone())),
            _ => None,
        })
        .expect("a continuation card must have been sent")
}

/// The latest in-place update of `message_id`, as a string.
async fn last_update_of(platform: &RecordingPlatform, message_id: &str) -> String {
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

fn has(card: &serde_json::Value, needle: &str) -> bool {
    card.to_string().contains(needle)
}

/// `/card` mid-turn finalizes the live card with the standard split header and
/// sends the continuation replied to the command message, carrying the pull
/// status line — the live card is the newest message again, with no separate
/// acknowledgement.
#[tokio::test]
async fn card_command_splits_the_chain_and_the_continuation_takes_over() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let (app, platform) = build_app(cfg, MockBackend::new(realistic_parts())).await;
    seed_session(&app, "ses_test", "/work").await;
    seed_live_card(&app, "ses_test", "第一段进度。").await;

    app.handle_message(incoming(
        "msg_card".into(),
        "chat_1".into(),
        "p2p".into(),
        None,
        "/card".into(),
        None,
    ))
    .await;

    // No acknowledgement message: the continuation's status line is the reply.
    assert!(
        platform.texts().await.is_empty(),
        "no reply_text acknowledgement may be sent: {:?}",
        platform.calls.lock().await
    );

    // The previous card ends with the standard split header and keeps the
    // content shown before the split (a delta handoff, not a copy).
    let finalized = last_update_of(&platform, "om_live").await;
    assert!(
        finalized.contains("部分完成，继续中"),
        "the previous card must wear the standard split header: {finalized}"
    );
    assert!(
        finalized.contains("第一段进度。"),
        "the previous card keeps everything before the split: {finalized}"
    );
    assert!(
        !finalized.contains("⏬ 实时卡片已移到底部"),
        "the status line records the pull on the continuation only: {finalized}"
    );

    // The continuation replies to the command and carries ONLY the delta.
    let (reply_to, card) = continuation(&platform).await;
    assert_eq!(
        reply_to, "msg_card",
        "the continuation must reply to the /card message"
    );
    assert!(
        !has(&card, "第一段进度。"),
        "the continuation must not re-render the finalized content: {card}"
    );
    assert!(
        has(&card, "⏬ 实时卡片已移到底部"),
        "the pull status line must ride the continuation: {card}"
    );

    // The chain is re-anchored: the continuation is the tracked live card and
    // the split is consumed.
    {
        let cards = app.cards_handle();
        assert_eq!(
            Turn::card_message_id(&cards, "ses_test").await.as_deref(),
            Some("msg_reply")
        );
        assert_eq!(
            Turn::reply_target(&cards, "ses_test").await.as_deref(),
            Some("msg_card")
        );
        assert!(
            !Turn::has_pending_split(&cards, "ses_test").await,
            "the split must be consumed"
        );
        assert!(
            Turn::card_is_live(&cards, "ses_test").await,
            "the continuation is the new live card"
        );
    }

    // Later flushes target the continuation, not the finalized card.
    Turn::push_text(&app.cards_handle(), "ses_test", "后续进度。").await;
    crate::bridge::turn::Turn::flush_card(&app.cards_handle(), "ses_test").await;
    let calls = platform.calls.lock().await.clone();
    let last_update = calls
        .iter()
        .rev()
        .find_map(|c| match c {
            PlatformCall::UpdateMessage { message_id, card } => Some((message_id.clone(), card.clone())),
            _ => None,
        })
        .expect("a later flush");
    assert_eq!(
        last_update.0, "msg_reply",
        "later updates must target the continuation: {calls:?}"
    );
    assert!(has(&last_update.1, "后续进度。"));
}
