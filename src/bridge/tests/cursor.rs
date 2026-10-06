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
    assistant, ctx, scripted_app, settle_tool, spawn_sync, spawn_turn, tool_assistant, user,
    wait_for_card_text,
};
use crate::backend::{
    MessageId, MessageRole, MessageTime, Part, SessionTranscript, TextPart, ToolStatus, TranscriptMessage,
};
use crate::bridge::chain::{CursorFrontier, CursorPartKind, RenderedCursor, cursor_prefix_digest};
use crate::bridge::test_support::{
    MockBackend, PlatformCall, card_text, patches_to, realistic_parts, seed_session, test_config,
    test_work_dir, text_part, typed_message, wait_for_transcript_reads,
};
use crate::bridge::turn::Turn;
use crate::opencode::types::SessionStatus;

/// Wait until a reply to `reply_to` carries `needle`, or panic after 5 s: the
/// restart's projection successor (spec #561's adoption).
async fn wait_for_reply_text(
    platform: &Arc<crate::bridge::test_support::RecordingPlatform>,
    reply_to: &str,
    needle: &str,
) -> String {
    let probe = async {
        loop {
            {
                let calls = platform.calls.lock().await;
                if let Some(card) = calls.iter().find_map(|call| match call {
                    PlatformCall::ReplyCard { reply_to: to, card } if to == reply_to => Some(card.clone()),
                    _ => None,
                }) {
                    let text = card_text(&card);
                    if text.contains(needle) {
                        return text;
                    }
                }
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    };
    tokio::time::timeout(Duration::from_secs(5), probe)
        .await
        .unwrap_or_else(|_| panic!("the projection never showed {needle}"))
}

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
            prefix_digest: Some(cursor_prefix_digest("第一段回答。")),
        }),
        "the frontier is the delivered text part and its character extent"
    );
    assert_eq!(
        cursor.live_calls,
        ["call_1".to_string()].into_iter().collect(),
        "the running panel's call is in the delivered live set"
    );

    // The tool settles: the next delivered body renders it as a timeline
    // panel, so the live set drops it. The frontier then moves to the settled
    // panel — it WAS delivered, so a restart must not re-render it — while
    // still carrying the text part's delivered extent (review #569).
    settle_tool(&backend, ToolStatus::Completed, "done").await;
    wait_for_card_text(&platform, "done").await;
    let cursor = wait_for_cursor(&app, |cursor| {
        cursor.live_calls.is_empty()
            && cursor
                .frontier
                .as_ref()
                .is_some_and(|frontier| frontier.kind == CursorPartKind::Tool)
    })
    .await;
    assert_eq!(
        cursor.frontier,
        Some(CursorFrontier {
            message_id: MessageId::new("msg_tool_4000"),
            part_index: 0,
            kind: CursorPartKind::Tool,
            // The fixture's tool part carries no server clock.
            started_at: None,
            delivered_chars: "第一段回答。".chars().count(),
            prefix_digest: Some(cursor_prefix_digest("第一段回答。")),
        }),
        "the settled panel is the frontier, carrying the text's delivered extent"
    );
}

/// Append a still-in-flight assistant answer to the live session's scripted
/// transcript — the server's own growth while the follow renders it (a finished
/// message would settle the run by transcript truth).
async fn grow_scripted_answer(backend: &Arc<MockBackend>, created: i64, text: &str) {
    let mut scripts = backend.transcript_scripts.lock().await;
    let transcript = &mut scripts.get_mut("ses_test").unwrap()[0];
    transcript.messages.push(typed_message(
        &format!("msg_a_{created}"),
        MessageRole::Assistant,
        Some(created),
        vec![text_part(text)],
    ));
}

/// Replace the newest scripted assistant part's text — the server re-sending a
/// GROWN snapshot of the SAME part, not a new message (the in-flight update
/// shape the Rendered Cursor must count exactly once).
async fn grow_scripted_part(backend: &Arc<MockBackend>, text: &str) {
    let mut scripts = backend.transcript_scripts.lock().await;
    let transcript = &mut scripts.get_mut("ses_test").unwrap()[0];
    for message in transcript.messages.iter_mut() {
        if message.id.as_str() != "msg_a_2000" {
            continue;
        }
        for part in message.parts.iter_mut() {
            if let crate::backend::Part::Text(part) = part {
                part.text = text.to_string();
                return;
            }
        }
    }
    panic!("the scripted transcript carries no msg_a_2000 text part to grow");
}

/// A part the server grows in place while the follow streams (spec #561, Codex
/// review on PR #569): every read re-sends the whole snapshot, so the card must
/// show the part exactly once and the record's frontier must be its character
/// extent — never a sum over the snapshots. Before the fix the grown snapshot
/// was appended whole, the card read "第一段回答。第一段回答。第二段回答。" and
/// the frontier recorded one character per snapshot twice over, which no
/// restart could resolve.
#[tokio::test]
async fn a_grown_part_advances_the_cursor_to_its_own_extent() {
    let _wd = test_work_dir();
    let first = "第一段回答。";
    let full = format!("{first}第二段回答。");
    let timeline = vec![
        user("msg_cola_anchor", 1_000, "第一条消息"),
        assistant(2_000, first),
        tool_assistant(4_000, ToolStatus::Running, ""),
    ];
    let (_dir, app, backend, platform) =
        scripted_app(vec![SessionTranscript::new(timeline)], Some(SessionStatus::Busy)).await;
    app.turn_drain_timeout_ms.store(30, Ordering::Relaxed);

    let turn = spawn_turn(&app, ctx("ses_test", "第一条消息"));
    wait_for_card_text(&platform, first).await;
    // The turn hands off at the tiny drain bound; the follow keeps the card
    // (and the record) live.
    let result = tokio::time::timeout(Duration::from_secs(5), turn)
        .await
        .expect("the turn must hand off at the drain bound")
        .unwrap();
    result.unwrap();
    wait_for_cursor(&app, |cursor| {
        cursor
            .frontier
            .as_ref()
            .is_some_and(|frontier| frontier.delivered_chars == first.chars().count())
    })
    .await;

    // The same part is re-sent, grown: the card takes only the tail and the
    // frontier stays the part's own extent.
    grow_scripted_part(&backend, &full).await;
    let cursor = wait_for_cursor(&app, |cursor| {
        cursor
            .frontier
            .as_ref()
            .is_some_and(|frontier| frontier.delivered_chars == full.chars().count())
    })
    .await;
    assert_eq!(
        cursor.frontier.as_ref().map(|f| f.message_id.as_str()),
        Some("msg_a_2000"),
        "the grown part keeps its own message identity"
    );
    wait_for_card_text(&platform, "第二段回答。").await;
    let card = platform
        .updated_cards()
        .await
        .last()
        .cloned()
        .expect("the grown part reached the card");
    let text = card_text(&card);
    assert_eq!(
        text.matches(first).count(),
        1,
        "the delivered prefix is never repeated by a grown snapshot: {text}"
    );
    assert_eq!(
        text.matches("第二段回答。").count(),
        1,
        "the grown tail lands exactly once: {text}"
    );
}

/// The write-cost acceptance (spec #561; reopening ADR-0061's rejected
/// render-frontier watermark): on a live-turn scenario the Chain Record
/// sidecar is persisted once per confirmed card write that moved the frontier
/// — never once per render poll. The seam is exact: the store counts its own
/// persisted writes (test-only), every platform `UpdateMessage` in the window
/// is a confirmed PATCH, and the follow's repeated transcript reads stand for
/// the poll cadence the old rejection weighed.
#[tokio::test]
async fn a_live_turn_persists_the_record_once_per_confirmed_write_not_per_poll() {
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
    let result = tokio::time::timeout(Duration::from_secs(5), turn)
        .await
        .expect("the turn must hand off at the drain bound")
        .unwrap();
    result.unwrap();
    wait_for_cursor(&app, |cursor| cursor.frontier.is_some()).await;
    // The follow reads twice with nothing new before the baseline: any flush
    // the hand-off still owed has landed, so the window starts quiescent.
    let reads = backend.transcript_calls.lock().await.len();
    wait_for_transcript_reads(&backend, "ses_test", reads + 2).await;

    let chains = app.cards_handle().chains.clone();
    let writes0 = chains.writes();
    let patches0 = patches_to(&platform, "msg_reply").await.len();

    // Growth 1: the running tool settles — one new confirmed body (the live
    // set drops). Growth 2: the answer streams on — one more (the frontier
    // moves). Each is persisted exactly once.
    settle_tool(&backend, ToolStatus::Completed, "done").await;
    wait_for_card_text(&platform, "done").await;
    wait_for_cursor(&app, |cursor| cursor.live_calls.is_empty()).await;
    grow_scripted_answer(&backend, 6_000, "第二段回答。").await;
    wait_for_card_text(&platform, "第二段回答。").await;
    let grown = wait_for_cursor(&app, |cursor| {
        cursor
            .frontier
            .as_ref()
            .is_some_and(|frontier| frontier.message_id.as_str() == "msg_a_6000")
    })
    .await;
    assert_eq!(
        grown.frontier.as_ref().map(|frontier| frontier.delivered_chars),
        Some("第二段回答。".chars().count()),
        "the frontier is the newly delivered part's extent"
    );

    let writes1 = chains.writes();
    let patches1 = patches_to(&platform, "msg_reply").await.len();
    assert!(
        patches1 - patches0 >= 2,
        "precondition: both confirmed bodies reached the card"
    );
    assert_eq!(
        writes1 - writes0,
        2,
        "each confirmed body that moved the frontier persists exactly one write"
    );

    // Idle render polls: the follow keeps reading with nothing new to deliver
    // and persists nothing — the per-poll write the reopened decision weighed
    // is not there.
    let reads = backend.transcript_calls.lock().await.len();
    wait_for_transcript_reads(&backend, "ses_test", reads + 3).await;
    assert_eq!(
        chains.writes(),
        writes1,
        "a render poll with nothing confirmed persists nothing"
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
    let chains = app.cards_handle().chains.clone();
    let writes0 = chains.writes();

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
    assert_eq!(
        chains.writes() - writes0,
        1,
        "only the chain record's own track write persisted; no failed write did"
    );

    // Feishu returns: the drain delivers the owed payload, and the reconcile
    // advances the staged cursor exactly once — one persisted write.
    platform.fail_update_transport_count.store(0, Ordering::SeqCst);
    app.core.feishu.drain_pending_card_updates(true).await;
    crate::bridge::turn::reconcile_staged_cursors(&app.cards_handle()).await;

    assert_eq!(
        chains.writes() - writes0,
        2,
        "the drain-delivered payload persisted exactly one write"
    );
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
            prefix_digest: Some(cursor_prefix_digest("答复。")),
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
    assert_eq!(
        chains.writes() - writes0,
        2,
        "a second pass must not persist anything either"
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

/// Both writes confirmed, the older one's durable write attempted LAST (spec
/// #561, review #569): the stage comparison and the record write are ONE
/// critical section, so an older stage — already applied and removed — can
/// never overwrite a newer cursor's durable position. Without the atomic
/// confirmation the older write lands last and a restart replays content the
/// newer card already delivered.
#[tokio::test]
async fn an_older_confirmation_never_overwrites_a_newer_durable_cursor() {
    let _wd = test_work_dir();
    let transcript = SessionTranscript::new(vec![
        user("msg_cola_anchor", 1_000, "你好"),
        assistant(2_000, "答复。"),
    ]);
    let (dir, app, _backend, platform) = scripted_app(vec![transcript], Some(SessionStatus::Idle)).await;
    let session_file = dir.path().join("sessions.json");
    // Every content write fails at the transport: the ending is owed, cursor A
    // staged and tied to its Pending Card Update sequence.
    platform.fail_update_transport_count.store(100, Ordering::SeqCst);
    Turn::run(&app.turn_handles(), ctx("ses_test", "你好"))
        .await
        .unwrap();
    let card_id = app
        .cards_handle()
        .chains
        .get("ses_test")
        .expect("the failed ending keeps the record")
        .card_message_id
        .clone();
    let staged_a = Turn::staged_cursor(&app.cards_handle(), "ses_test")
        .await
        .expect("A is staged");
    let stage_a = Turn::staged_cursor_id(&app.cards_handle(), "ses_test")
        .await
        .expect("A's exact identity");
    // A NEWER stage B is staged NOW — before A's confirmation takes the cards
    // lock — so the test never contends for that lock itself.
    let mut staged_b = staged_a.clone();
    staged_b.frontier = staged_a.frontier.clone().map(|mut frontier| {
        frontier.delivered_chars += 1;
        frontier
    });
    let stage_b = Turn::stage_cursor(&app.cards_handle(), "ses_test", Some(&card_id), &staged_b).await;

    // A's payload delivers: its confirmation takes the stage and parks before
    // the durable write.
    platform.fail_update_transport_count.store(0, Ordering::SeqCst);
    app.core.feishu.drain_pending_card_updates(true).await;
    let entered = Arc::new(tokio::sync::Notify::new());
    let release = Arc::new(tokio::sync::Notify::new());
    let first = {
        let cards = app.cards_handle();
        let card_id = card_id.clone();
        let gate = crate::bridge::turn::ConfirmGate {
            entered: entered.clone(),
            release: release.clone(),
        };
        tokio::spawn(async move {
            crate::bridge::turn::confirm_staged_cursor_for_test(
                &cards,
                "ses_test",
                &card_id,
                stage_a,
                Some(&gate),
            )
            .await;
        })
    };
    entered.notified().await;

    let second = {
        let cards = app.cards_handle();
        let card_id = card_id.clone();
        tokio::spawn(async move {
            crate::bridge::turn::confirm_staged_cursor_for_test(&cards, "ses_test", &card_id, stage_b, None)
                .await;
        })
    };
    // B gets its chance: with the atomic confirmation it waits for A's
    // critical section; without it, it persists B now — and A's late write
    // would then overwrite it.
    tokio::time::sleep(Duration::from_millis(50)).await;
    release.notify_one();
    first.await.unwrap();
    second.await.unwrap();

    assert_eq!(
        app.cards_handle().chains.cursor("ses_test"),
        Some(staged_b.clone()),
        "the newer confirmation's cursor is the durable one"
    );
    let persisted =
        crate::bridge::chain::ChainRecords::load(session_file.with_file_name("chain_records.json"));
    assert_eq!(
        persisted.cursor("ses_test"),
        Some(staged_b),
        "the persisted record keeps the newer cursor"
    );
}

/// The drain reconcile confirms only the exact write whose delivery it
/// verified, while a delivered-but-unconfirmed write keeps its confirmation
/// even after a newer stage replaces the pending slot (spec #561, review
/// #569). The interleaving is deterministic through the drain's test gate, and
/// a restart from the persisted cursor re-renders only what the delivered
/// write did not cover.
#[tokio::test]
async fn a_delivered_cursor_survives_a_newer_stage_that_replaces_it() {
    let _wd = test_work_dir();
    let transcript = SessionTranscript::new(vec![
        user("msg_cola_anchor", 1_000, "你好"),
        assistant(2_000, "答复。"),
    ]);
    let (dir, app, _backend, platform) = scripted_app(vec![transcript], Some(SessionStatus::Idle)).await;
    let session_file = dir.path().join("sessions.json");
    // Every content write fails at the transport: the ending is owed, its
    // cursor staged and tied to the Pending Card Update sequence.
    platform.fail_update_transport_count.store(100, Ordering::SeqCst);
    Turn::run(&app.turn_handles(), ctx("ses_test", "你好"))
        .await
        .unwrap();
    let card_id = app
        .cards_handle()
        .chains
        .get("ses_test")
        .expect("the failed ending keeps the record")
        .card_message_id
        .clone();
    assert_eq!(
        app.cards_handle().chains.cursor("ses_test"),
        None,
        "a failed write advanced nothing"
    );
    let staged_a = Turn::staged_cursor(&app.cards_handle(), "ses_test")
        .await
        .expect("the failed write staged its cursor");

    // Feishu returns: the owed payload delivers, so the reconcile now verifies
    // cursor A's write as delivered.
    platform.fail_update_transport_count.store(0, Ordering::SeqCst);
    app.core.feishu.drain_pending_card_updates(true).await;

    // Park the reconcile between that verification and its confirmation, and
    // stage a NEWER cursor there — the fresh flush's body, its own PATCH still
    // pending.
    let gate = crate::bridge::turn::ReconcileGate {
        entered: std::sync::Arc::new(tokio::sync::Notify::new()),
        release: std::sync::Arc::new(tokio::sync::Notify::new()),
    };
    let reconcile = {
        let cards = app.cards_handle();
        let gate = crate::bridge::turn::ReconcileGate {
            entered: gate.entered.clone(),
            release: gate.release.clone(),
        };
        tokio::spawn(async move {
            crate::bridge::turn::reconcile_staged_cursors_gated(&cards, &gate).await;
        })
    };
    gate.entered.notified().await;
    let mut staged_b = staged_a.clone();
    staged_b.frontier = staged_a.frontier.clone().map(|mut frontier| {
        frontier.delivered_chars += 1;
        frontier
    });
    Turn::stage_cursor(&app.cards_handle(), "ses_test", Some(&card_id), &staged_b).await;
    gate.release.notify_one();
    reconcile.await.unwrap();

    // The delivered write A is persisted even though B now holds the pending
    // slot; B itself is left for its own confirmation — a restart from here
    // re-renders B's undelivered content instead of skipping it.
    assert_eq!(
        app.cards_handle().chains.cursor("ses_test"),
        Some(staged_a.clone()),
        "the delivered write's cursor is persisted despite the newer stage"
    );
    assert_eq!(
        Turn::staged_cursor(&app.cards_handle(), "ses_test").await,
        Some(staged_b.clone()),
        "the newer stage is left untouched for its own confirmation"
    );
    let persisted =
        crate::bridge::chain::ChainRecords::load(session_file.with_file_name("chain_records.json"));
    assert_eq!(
        persisted.cursor("ses_test"),
        Some(staged_a.clone()),
        "the persisted record carries the delivered write's cursor"
    );

    // A restart over that cursor re-renders only the growth past it: the
    // delivered content is never repeated (review #569).
    drop(app);
    let grown = "答复。补充。";
    let mut backend2 = MockBackend::new(realistic_parts());
    backend2.given_transcript(
        "ses_test",
        vec![SessionTranscript::new(vec![
            user("msg_cola_anchor", 1_000, "你好"),
            TranscriptMessage {
                id: MessageId::new("msg_a_2000"),
                role: MessageRole::Assistant,
                time: Some(MessageTime {
                    created: 2_000,
                    completed: None,
                }),
                model: None,
                tokens: None,
                error: None,
                parts: vec![Part::Text(TextPart {
                    text: grown.to_string(),
                    started_at: Some(2_000),
                })],
            },
        ])],
    );
    backend2.with_session_status("ses_test", Some(SessionStatus::Busy));
    let backend2 = Arc::new(backend2);
    let platform2 = Arc::new(crate::bridge::test_support::RecordingPlatform::new());
    let app2 = Arc::new(
        crate::bridge::App::new(test_config(&session_file), backend2.clone(), platform2.clone())
            .expect("the restarted app builds"),
    );
    seed_session(&app2, "ses_test", "/work").await;
    spawn_sync(&app2);
    let successor_text = wait_for_reply_text(&platform2, "msg_cola_anchor", "补充。").await;
    assert!(
        !successor_text.contains("答复。"),
        "the delivered content is never repeated on the restart: {successor_text}"
    );
}
