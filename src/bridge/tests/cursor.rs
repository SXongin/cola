//! The Rendered Cursor on the Chain Record (spec #561): a confirmed card
//! write advances the record's frontier to the delivered body, a failed or
//! still-owed write advances nothing, and a payload the Pending Card Update
//! drain delivers later advances it exactly once.
//!
//! Every test drives a real Turn (or the real Session Sync pieces) over a
//! scripted Backend and a recording Platform, so the assertions read the
//! cards sent/patched and the sidecar fact — never private internals.

use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use super::drain::{
    assistant, ctx, scripted_app, settle_tool, spawn_turn, tool_assistant, user, wait_for_card_text,
};
use crate::backend::{MessageId, SessionTranscript, ToolStatus};
use crate::bridge::chain::{CursorFrontier, CursorPartKind, RenderedCursor};
use crate::bridge::test_support::{PlatformCall, card_text, test_work_dir};
use crate::bridge::turn::Turn;
use crate::opencode::types::SessionStatus;

/// Wait until the durable record's Rendered Cursor satisfies `ready`, or
/// panic after 5 s.
async fn wait_for_cursor(
    app: &Arc<crate::bridge::App>,
    mut ready: impl FnMut(&RenderedCursor) -> bool,
) -> RenderedCursor {
    let probe = async {
        loop {
            if let Some(cursor) = app.cards_handle().chains.cursor("ses_test")
                && ready(&cursor)
            {
                return cursor;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    };
    tokio::time::timeout(Duration::from_secs(5), probe)
        .await
        .expect("the record's cursor must advance")
}

/// Wait until the platform observed at least `n` card-update attempts for
/// `card_message_id` (failed attempts included), or panic after 5 s.
async fn wait_for_update_attempts(
    platform: &Arc<crate::bridge::test_support::RecordingPlatform>,
    card_message_id: &str,
    n: usize,
) {
    let probe = async {
        loop {
            let attempts = platform
                .calls
                .lock()
                .await
                .iter()
                .filter(|call| {
                    matches!(
                        call,
                        PlatformCall::UpdateMessage { message_id, .. } if message_id == card_message_id
                    )
                })
                .count();
            if attempts >= n {
                return;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    };
    tokio::time::timeout(Duration::from_secs(5), probe)
        .await
        .expect("the platform must see the write attempts");
}

/// A live turn's confirmed card writes advance the record's Rendered Cursor
/// (spec #561): the frontier names the newest delivered text part with its
/// character extent, and the running tool's call id is in the live set — the
/// settled tool then leaves it on the next delivered body.
#[tokio::test]
async fn a_live_turn_advances_the_record_cursor_to_the_delivered_body() {
    let _wd = test_work_dir();
    let timeline = vec![
        user("msg_cola_anchor", 1_000, "第一条消息"),
        assistant(2_000, "第一段回答。"),
        tool_assistant(4_000, ToolStatus::Running, ""),
    ];
    let (_dir, app, backend, platform) =
        scripted_app(vec![SessionTranscript::new(timeline)], Some(SessionStatus::Busy)).await;
    app.turn_drain_timeout_ms.store(30, Ordering::Relaxed);

    let turn = spawn_turn(&app, ctx("ses_test", "第一条消息"));
    wait_for_card_text(&platform, "第一段回答。").await;
    wait_for_card_text(&platform, "⏳ bash").await;
    // The turn hands off at the tiny drain bound; the follow keeps the card
    // (and the record) live.
    let result = tokio::time::timeout(Duration::from_secs(5), turn)
        .await
        .expect("the turn must hand off at the drain bound")
        .unwrap();
    result.unwrap();

    let cursor = wait_for_cursor(&app, |cursor| {
        cursor
            .frontier
            .as_ref()
            .is_some_and(|frontier| frontier.message_id.as_str() == "msg_a_2000")
    })
    .await;
    assert_eq!(
        cursor.frontier,
        Some(CursorFrontier {
            message_id: MessageId::new("msg_a_2000"),
            part_index: 0,
            kind: CursorPartKind::Text,
            started_at: Some(2_000),
            delivered_chars: "第一段回答。".chars().count(),
        }),
        "the frontier is the delivered text part and its character extent"
    );
    assert_eq!(
        cursor.live_calls,
        ["call_1".to_string()].into_iter().collect(),
        "the running panel's call is in the delivered live set"
    );

    // The tool settles: the next delivered body renders it as a timeline
    // panel, so the live set drops it. The frontier stays the text part.
    settle_tool(&backend, ToolStatus::Completed, "done").await;
    wait_for_card_text(&platform, "done").await;
    let cursor = wait_for_cursor(&app, |cursor| cursor.live_calls.is_empty()).await;
    assert_eq!(
        cursor.frontier.as_ref().map(|f| f.message_id.as_str()),
        Some("msg_a_2000"),
        "a settled tool does not move the text frontier"
    );
}

/// A failed write advances nothing (the failure is still owed as a Pending
/// Card Update), and the drain's later delivery of exactly that payload
/// advances the cursor once — to the delivered body's frontier (spec #561).
#[tokio::test]
async fn a_failed_card_write_stages_and_the_drain_advances_the_cursor_once() {
    let _wd = test_work_dir();
    let transcript = SessionTranscript::new(vec![
        user("msg_cola_anchor", 1_000, "你好"),
        assistant(2_000, "答复。"),
    ]);
    let (_dir, app, _backend, platform) = scripted_app(vec![transcript], Some(SessionStatus::Idle)).await;
    // Every content write fails at the transport: nothing lands, and the
    // ending is the newest owed payload.
    platform.fail_update_transport_count.store(100, Ordering::SeqCst);

    Turn::run(&app.turn_handles(), ctx("ses_test", "你好"))
        .await
        .unwrap();

    let record = app
        .cards_handle()
        .chains
        .get("ses_test")
        .expect("a failed ending write keeps the record");
    let card_id = record.card_message_id.clone();
    assert_eq!(
        app.cards_handle().chains.cursor("ses_test"),
        None,
        "a failed write advanced nothing"
    );
    assert!(
        app.core.feishu.has_pending_card_update(&card_id),
        "the ending is still owed"
    );

    // Feishu returns: the drain delivers the owed payload, and the reconcile
    // advances the staged cursor exactly once.
    platform.fail_update_transport_count.store(0, Ordering::SeqCst);
    app.core.feishu.drain_pending_card_updates(true).await;
    crate::bridge::turn::reconcile_staged_cursors(&app.cards_handle()).await;

    let cursor = app
        .cards_handle()
        .chains
        .cursor("ses_test")
        .expect("the delivered body's cursor");
    assert_eq!(
        cursor.frontier,
        Some(CursorFrontier {
            message_id: MessageId::new("msg_a_2000"),
            part_index: 0,
            kind: CursorPartKind::Text,
            started_at: Some(2_000),
            delivered_chars: "答复。".chars().count(),
        }),
        "the cursor matches the delivered body's render point"
    );
    assert!(cursor.live_calls.is_empty());
    let delivered = platform
        .updated_cards()
        .await
        .last()
        .cloned()
        .expect("the drain delivered a card");
    assert!(
        card_text(&delivered).contains("答复。"),
        "the delivered body is the one the cursor names: {delivered}"
    );

    // A second reconcile pass changes nothing: the stage was consumed by the
    // first (exactly once).
    crate::bridge::turn::reconcile_staged_cursors(&app.cards_handle()).await;
    assert_eq!(
        app.cards_handle().chains.cursor("ses_test"),
        Some(cursor),
        "a second pass must not advance anything"
    );
}

/// A permanently refused card write can never land, so it advances nothing —
/// even though its tombstone leaves no Pending Card Update behind, the drain
/// reconcile must not read it as a delivery (spec #561).
#[tokio::test]
async fn a_permanently_rejected_write_advances_nothing() {
    let _wd = test_work_dir();
    let timeline = vec![
        user("msg_cola_anchor", 1_000, "第一条消息"),
        assistant(2_000, "会被拒绝的回答。"),
    ];
    let (_dir, app, _backend, platform) =
        scripted_app(vec![SessionTranscript::new(timeline)], Some(SessionStatus::Busy)).await;
    app.turn_drain_timeout_ms.store(30, Ordering::Relaxed);
    // The plain attempt and its fenced retry are both refused: the card is
    // suspended and no later write will ever carry the text.
    platform
        .fail_update_card_content_count
        .store(100, Ordering::SeqCst);

    let turn = spawn_turn(&app, ctx("ses_test", "第一条消息"));
    // The loading card is the recorded card (`msg_reply` is the recording
    // platform's create id), and its content update gets the plain rejection
    // and the fenced retry: both refused.
    wait_for_update_attempts(&platform, "msg_reply", 2).await;
    assert!(
        !app.core.feishu.has_pending_card_update("msg_reply"),
        "a refused write leaves only a tombstone"
    );

    crate::bridge::turn::reconcile_staged_cursors(&app.cards_handle()).await;
    assert_eq!(
        app.cards_handle().chains.cursor("ses_test"),
        None,
        "a permanently refused write advances nothing"
    );

    let result = tokio::time::timeout(Duration::from_secs(5), turn)
        .await
        .expect("the turn must hand off at the drain bound")
        .unwrap();
    result.unwrap();
}
