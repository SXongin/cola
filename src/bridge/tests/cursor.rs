//! The Rendered Cursor on the Chain Record (spec #561): a confirmed card
//! write advances the record's frontier to the delivered body, a failed or
//! still-owed write advances nothing, and a payload the Pending Card Update
//! drain delivers later advances it exactly once.
//!
//! Every test drives a real Turn (or the real Session Sync pieces) over a
//! scripted Backend and a recording Platform, so the assertions read the
//! cards sent/patched and the sidecar fact — never private internals.

use std::sync::Arc;
use std::time::Duration;

use super::drain::{
    assistant, ctx, scripted_app, settle_tool, spawn_sync, spawn_turn, tool_assistant, user,
    wait_for_card_text,
};
use super::reap::{realistic_card_view, restarted_app_with_backend, wait_for_projection};
use crate::backend::types::SessionStatus;
use crate::backend::{
    Compaction, CompactionReason, CompactionStatus, MessageId, MessageRole, Part, ReasoningPart,
    SessionTranscript, ToolStatus,
};
use crate::bridge::chain::{CursorFrontier, CursorPartKind, RenderedCursor, cursor_prefix_digest};
use crate::bridge::test_support::{
    ALWAYS, MockBackend, PlatformCall, UpdateFailure, card_text, patches_to, test_work_dir, text_part,
    two_parts_at_one_time, typed_message, wait_for_transcript_reads,
};
use crate::bridge::turn::Turn;
use crate::feishu::card::tool_render::ToolPanel;

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
    // The merged unbounded drain keeps the card (and the record) live on the
    // turn's own task; the test inspects the cursor while it runs.
    let _turn = spawn_turn(&app, ctx("ses_test", "第一条消息"));
    wait_for_card_text(&platform, "第一段回答。").await;
    wait_for_card_text(&platform, "⏳ bash").await;

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

/// Two text parts of one assistant message sharing a server time (spec #561,
/// review #569): the render keeps them as separate entries, each with its own
/// source, so the confirmed frontier names the SECOND part with its OWN
/// character extent and digest. Merged into one entry it would carry the first
/// part's digest over the combined text, resolve to a cut at 0, and re-render
/// both parts after a restart.
#[tokio::test]
async fn two_text_parts_at_one_server_time_confirm_their_own_frontier() {
    let _wd = test_work_dir();
    let first = "第一段回答。";
    let second = "第二段回答。";
    let timeline = vec![
        user("msg_cola_anchor", 1_000, "第一条消息"),
        two_parts_at_one_time(first, second),
        // A running tool keeps the card (and its durable record) live without
        // adding a timeline entry of its own.
        tool_assistant(4_000, ToolStatus::Running, ""),
    ];
    let (_dir, app, _backend, platform) =
        scripted_app(vec![SessionTranscript::new(timeline)], Some(SessionStatus::Busy)).await;
    let _turn = spawn_turn(&app, ctx("ses_test", "第一条消息"));
    wait_for_card_text(&platform, second).await;

    let cursor = wait_for_cursor(&app, |cursor| cursor.frontier.is_some()).await;
    assert_eq!(
        cursor.frontier,
        Some(CursorFrontier {
            message_id: MessageId::new("msg_a_2000"),
            part_index: 1,
            kind: CursorPartKind::Text,
            started_at: Some(2_000),
            delivered_chars: second.chars().count(),
            prefix_digest: Some(cursor_prefix_digest(second)),
        }),
        "the second part is its own frontier, with its own extent and digest"
    );
    let card = platform
        .updated_cards()
        .await
        .last()
        .cloned()
        .expect("the parts reached the card");
    let text = card_text(&card);
    assert_eq!(
        text.matches(first).count(),
        1,
        "the first part renders exactly once: {text}"
    );
    assert_eq!(
        text.matches(second).count(),
        1,
        "the second part renders exactly once: {text}"
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
    let _turn = spawn_turn(&app, ctx("ses_test", "第一条消息"));
    wait_for_card_text(&platform, first).await;
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
    let _turn = spawn_turn(&app, ctx("ses_test", "第一条消息"));
    wait_for_card_text(&platform, "第一段回答。").await;
    wait_for_card_text(&platform, "⏳ bash").await;
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
    platform.fail_update(UpdateFailure::Transport, ALWAYS);
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
    platform.clear_fail_update();
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
    // Every write is permanently refused (a semantic refusal, not a content
    // rejection): no later write can ever carry the text.
    platform.fail_update(UpdateFailure::Permanent, ALWAYS);

    // The merged drain keeps the turn running on its own task; the test
    // inspects the refused write's tombstone while it runs.
    let _turn = spawn_turn(&app, ctx("ses_test", "第一条消息"));
    // The loading card is the recorded card (`msg_reply` is the recording
    // platform's create id); its content update is refused.
    wait_for_update_attempts(&platform, "msg_reply", 1).await;
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
}

/// An older confirmation lands LAST (spec #561, review #569): the stage
/// comparison and the durable record write are ONE critical section, so an
/// older stage — whose payload delivered while its confirmation was still
/// pending — can never overwrite a newer cursor the flush already confirmed.
/// Without the guard the older write lands last and a restart replays content
/// the newer card already delivered.
///
/// The interleave is the delivery layer's own: A's payload delivers without a
/// reconcile, a newer body lands and confirms, and only then does A's
/// confirmation run.
///
/// Two confirmations cannot contend, which is why this test orders them: every
/// confirmation writer holds the cards lock through its take-and-write —
/// `confirm_staged_cursor`, the drain's `reconcile_staged_cursors` (re-locking
/// into the same section), and the takeover `attach_and_repoint` — so the guard
/// orders an older stage only against a newer *confirmed* stage: a later
/// confirmation of an older stage leaves the record unchanged. A takeover
/// cannot supply the race instead: its armed successor stays card-less until
/// the create lands, so a concurrent `flush_card_locked` writes nothing and no
/// drain-confirmable stage can be staged in that window. What this test pins is
/// the ACCUMULATOR's half — the in-memory base and its watermark. The durable
/// half is the record's own stage stamp (issue #1186), which orders a
/// confirmation against the live PATCH's direct advance;
/// `the_direct_patch_advance_outranks_an_older_stage_confirmed_later` pins that
/// one. Neither is reachable by a stress test — it fails only
/// probabilistically — and a test-only park is banned by ADR-0110 §2.
#[tokio::test]
async fn an_older_confirmation_never_overwrites_a_newer_durable_cursor() {
    let _wd = test_work_dir();
    let (dir, app, _backend, platform) = scripted_app(
        vec![SessionTranscript::new(vec![user(
            "msg_cola_anchor",
            1_000,
            "你好",
        )])],
        Some(SessionStatus::Idle),
    )
    .await;
    let session_file = dir.path().join("sessions.json");
    let cards = app.cards_handle();
    Turn::seed_card(&cards, "ses_test", Some("om_live")).await;
    Turn::set_reply_target(&cards, "ses_test", "msg_1").await;
    Turn::push_text(&cards, "ses_test", "答复。").await;
    Turn::push_tool(
        &cards,
        "ses_test",
        "call_9",
        ToolPanel::for_test("bash", ToolStatus::Running, None, None),
    )
    .await;
    cards.chains.track(
        "ses_test",
        "om_live",
        MessageId::new("msg_cola_anchor"),
        Some(1_000),
        Some("/work"),
        Some("msg_1"),
    );

    // A's write fails at the transport: its cursor is staged and tied to the
    // Pending Card Update sequence.
    platform.fail_update(UpdateFailure::Transport, 1);
    Turn::flush_card(&cards, "ses_test").await;
    let staged_a = Turn::staged_cursor(&cards, "ses_test")
        .await
        .expect("the failed write staged its cursor");

    // Its payload delivers — but no reconcile runs, so A is still unconfirmed.
    platform.clear_fail_update();
    app.core.feishu.drain_pending_card_updates(true).await;
    assert_eq!(
        cards.chains.cursor("ses_test"),
        None,
        "delivery alone advances nothing"
    );

    // A NEWER body lands: the flush confirms its own stage, so the durable
    // cursor moves past A before A's confirmation ever runs.
    Turn::push_tool(
        &cards,
        "ses_test",
        "call_9",
        ToolPanel::for_test("bash", ToolStatus::Completed, None, Some("done")),
    )
    .await;
    Turn::flush_card(&cards, "ses_test").await;
    // The flush's own confirmation is the newer durable cursor; A's stage is
    // still in the accumulator, unconfirmed and due.
    let staged_b = cards
        .chains
        .cursor("ses_test")
        .expect("the newer confirmation's cursor is the durable one");
    assert_ne!(
        staged_b, staged_a,
        "precondition: the newer body moved the live set"
    );

    // A's confirmation runs now and is discarded: the newer stage already won.
    crate::bridge::turn::reconcile_staged_cursors(&cards).await;
    assert_eq!(
        cards.chains.cursor("ses_test"),
        Some(staged_b.clone()),
        "the older confirmation never overwrites the newer durable cursor"
    );
    let persisted = crate::bridge::chain::ChainRecords::load(crate::state::StateLayout::beside(
        &session_file,
        "chain_records.json",
    ));
    assert_eq!(
        persisted.cursor("ses_test"),
        Some(staged_b),
        "the persisted record keeps the newer cursor"
    );
}

/// The durable cursor can only move forward across two writers (issue #1186):
/// the live PATCH advances the record DIRECTLY, before any await (ADR-0080), so
/// the accumulator's watermark — which only a confirmation advances — can lag
/// it. A stage whose confirmation a cancellation dropped, then, may still be
/// due after a newer body wrote the record, and its confirmation must read as
/// stale. The record's stage stamp is what makes it stale: the direct advance
/// carries the body's stamp, and an older one never moves the cursor or the gap
/// back.
///
/// The interleave is the production one — A's payload delivers with its
/// confirmation still pending, a newer body's PATCH is accepted and writes the
/// record directly, and only then does A's confirmation run — driven step by
/// step here because the cancellation itself has no seam (ADR-0110 §2 bans
/// adding one).
#[tokio::test]
async fn the_direct_patch_advance_outranks_an_older_stage_confirmed_later() {
    let _wd = test_work_dir();
    let (dir, app, _backend, platform) = scripted_app(
        vec![SessionTranscript::new(vec![user(
            "msg_cola_anchor",
            1_000,
            "你好",
        )])],
        Some(SessionStatus::Idle),
    )
    .await;
    let session_file = dir.path().join("sessions.json");
    let cards = app.cards_handle();
    Turn::seed_card(&cards, "ses_test", Some("om_live")).await;
    Turn::set_reply_target(&cards, "ses_test", "msg_1").await;
    Turn::push_text(&cards, "ses_test", "答复。").await;
    Turn::push_tool(
        &cards,
        "ses_test",
        "call_9",
        ToolPanel::for_test("bash", ToolStatus::Running, None, None),
    )
    .await;
    cards.chains.track(
        "ses_test",
        "om_live",
        MessageId::new("msg_cola_anchor"),
        Some(1_000),
        Some("/work"),
        Some("msg_1"),
    );

    // A's write fails at the transport: its cursor is staged and tied to the
    // Pending Card Update sequence.
    platform.fail_update(UpdateFailure::Transport, 1);
    Turn::flush_card(&cards, "ses_test").await;
    let staged_a = Turn::staged_cursor(&cards, "ses_test")
        .await
        .expect("the failed write staged its cursor");

    // Its payload delivers — but no reconcile runs, so A is still due and its
    // confirmation has not happened. This is the state a cancelled flush
    // leaves behind.
    platform.clear_fail_update();
    app.core.feishu.drain_pending_card_updates(true).await;
    assert_eq!(
        cards.chains.cursor("ses_test"),
        None,
        "delivery alone advances nothing"
    );

    // A NEWER body's PATCH is accepted, and its direct advance writes the
    // record in that same synchronous step (ADR-0080), stamped as the newest
    // cursor write there is. Its own confirmation is the one the cancellation
    // ate.
    let newer = RenderedCursor {
        frontier: Some(CursorFrontier {
            message_id: MessageId::new("msg_tool_4000"),
            part_index: 0,
            kind: CursorPartKind::Tool,
            started_at: None,
            delivered_chars: "答复。".chars().count(),
            prefix_digest: Some(cursor_prefix_digest("答复。")),
        }),
        ..RenderedCursor::default()
    };
    cards
        .chains
        .advance_cursor_and_gap("ses_test", "om_live", &newer, None, &[]);
    assert_ne!(
        staged_a, newer,
        "precondition: the two bodies are different render points"
    );
    assert_eq!(
        cards.chains.cursor("ses_test"),
        Some(newer.clone()),
        "precondition: the direct advance wrote the newer body's cursor"
    );

    // A's confirmation now runs. Its stage is the one the record has already
    // overtaken, so it moves nothing — the cursor stays where the delivered
    // newer body put it, and a restart re-renders nothing that body showed.
    crate::bridge::turn::reconcile_staged_cursors(&cards).await;
    assert_eq!(
        cards.chains.cursor("ses_test"),
        Some(newer.clone()),
        "an older stage confirmed after the direct advance never moves the cursor back"
    );
    let persisted = crate::bridge::chain::ChainRecords::load(crate::state::StateLayout::beside(
        &session_file,
        "chain_records.json",
    ));
    assert_eq!(
        persisted.cursor("ses_test"),
        Some(newer),
        "the persisted record keeps the newer cursor too"
    );

    // The guard is not a latch: a write the record has not overtaken still
    // advances it, so a later delivered body moves the frontier on as before.
    let latest = RenderedCursor {
        frontier: Some(CursorFrontier {
            message_id: MessageId::new("msg_a_6000"),
            part_index: 0,
            kind: CursorPartKind::Text,
            started_at: Some(6_000),
            delivered_chars: "补充。".chars().count(),
            prefix_digest: Some(cursor_prefix_digest("补充。")),
        }),
        ..RenderedCursor::default()
    };
    cards
        .chains
        .advance_cursor_and_gap("ses_test", "om_live", &latest, None, &[]);
    assert_eq!(
        cards.chains.cursor("ses_test"),
        Some(latest),
        "a newer stage still advances the record"
    );
}

/// The drain reconcile confirms only the exact write whose delivery it
/// verified, while a delivered-but-unconfirmed write keeps its confirmation
/// even after a newer stage replaces the pending slot (spec #561, review
/// #569) — so the record never skips content the newer, still-owed body
/// carries, and a restart from the confirmed cursor re-renders only the growth
/// past it.
///
/// The interleave is the delivery layer's own: A's payload delivers, a newer
/// body's write fails and takes the pending slot, and only then does the
/// reconcile run.
#[tokio::test]
async fn a_delivered_cursor_survives_a_newer_stage_that_replaces_it() {
    let _wd = test_work_dir();
    let (dir, app, _backend, platform) = scripted_app(
        vec![SessionTranscript::new(vec![
            user("msg_cola_anchor", 1_000, "你好"),
            assistant(2_000, "答复。"),
        ])],
        Some(SessionStatus::Idle),
    )
    .await;
    let session_file = dir.path().join("sessions.json");
    let cards = app.cards_handle();

    // A's write fails at the transport: the ending is owed, its cursor staged
    // and tied to the Pending Card Update sequence.
    platform.fail_update(UpdateFailure::Transport, ALWAYS);
    Turn::run(&app.turn_handles(), ctx("ses_test", "你好"))
        .await
        .unwrap();
    let card_id = cards
        .chains
        .get("ses_test")
        .expect("a failed ending write keeps the record")
        .card_message_id
        .clone();
    assert_eq!(
        cards.chains.cursor("ses_test"),
        None,
        "a failed write advanced nothing"
    );
    assert!(
        app.core.feishu.has_pending_card_update(&card_id),
        "the body is still owed"
    );
    let staged_a = Turn::staged_cursor(&cards, "ses_test")
        .await
        .expect("the failed write staged its cursor");

    // Feishu returns: the owed payload delivers, but no reconcile has run, so
    // nothing is confirmed yet.
    platform.clear_fail_update();
    app.core.feishu.drain_pending_card_updates(true).await;
    assert_eq!(
        cards.chains.cursor("ses_test"),
        None,
        "delivery alone advances nothing"
    );

    // A NEWER body renders — a running tool joins the live set — and its own
    // write fails too, so B holds the pending slot A's payload just vacated.
    Turn::push_tool(
        &cards,
        "ses_test",
        "call_9",
        ToolPanel::for_test("bash", ToolStatus::Running, None, None),
    )
    .await;
    platform.fail_update(UpdateFailure::Transport, 1);
    Turn::flush_card(&cards, "ses_test").await;
    let staged_b = Turn::staged_cursor(&cards, "ses_test")
        .await
        .expect("the newer body staged its cursor");
    assert_ne!(
        staged_b, staged_a,
        "precondition: the newer body moved the live set"
    );

    // The reconcile confirms the stage whose delivery it verified — A — and
    // leaves B for its own (still owed) confirmation.
    crate::bridge::turn::reconcile_staged_cursors(&cards).await;
    assert_eq!(
        cards.chains.cursor("ses_test"),
        Some(staged_a.clone()),
        "the delivered write's cursor is persisted despite the newer stage"
    );
    assert_eq!(
        Turn::staged_cursor(&cards, "ses_test").await,
        Some(staged_b.clone()),
        "the newer stage is left untouched for its own confirmation"
    );
    let persisted = crate::bridge::chain::ChainRecords::load(crate::state::StateLayout::beside(
        &session_file,
        "chain_records.json",
    ));
    assert_eq!(
        persisted.cursor("ses_test"),
        Some(staged_a.clone()),
        "the persisted record carries the delivered write's cursor"
    );

    // A restart over that cursor re-renders only the growth past it: the
    // transcribed answer grew while B was still owed, so the successor shows
    // the growth once and repeats nothing A's delivered write covered. The
    // sidecar is copied so this app can go on and deliver B below.
    let restart_dir = tempfile::tempdir().unwrap();
    let restart_file = restart_dir.path().join("sessions.json");
    std::fs::copy(
        crate::state::StateLayout::beside(&session_file, "chain_records.json"),
        crate::state::StateLayout::beside(&restart_file, "chain_records.json"),
    )
    .expect("the persisted record copies for the restart");
    let transcript = SessionTranscript::new(vec![
        user("msg_cola_anchor", 1_000, "你好"),
        assistant(2_000, "答复。补充。"),
    ]);
    let (app2, platform2, _backend2) =
        restarted_app_with_backend(&restart_file, transcript, Some(SessionStatus::Idle)).await;
    platform2.given_card_view(&card_id, realistic_card_view());
    spawn_sync(&app2);
    let (_successor, successor_text) = wait_for_projection(&platform2, "msg_1").await;
    assert!(
        successor_text.contains("补充。"),
        "the growth past the delivered cursor lands exactly once: {successor_text}"
    );
    assert!(
        !successor_text.contains("答复。"),
        "the delivered content is never repeated on the restart: {successor_text}"
    );

    // B delivers later: its own confirmation now advances the record to it.
    platform.clear_fail_update();
    app.core.feishu.drain_pending_card_updates(true).await;
    crate::bridge::turn::reconcile_staged_cursors(&cards).await;
    assert_eq!(
        cards.chains.cursor("ses_test"),
        Some(staged_b),
        "the newer body's own delivery moves the cursor forward"
    );
}

/// A completed compaction is the newest delivered fact on the card (ADR-0082,
/// ticket #722): the confirmed card write advances the record's frontier ONTO
/// the compaction's own position, so a restart consumes the chain instead of
/// replaying the row. The frontier is a position, not a text: its delivered
/// extent is the empty prefix.
#[tokio::test]
async fn a_confirmed_write_on_a_compaction_advances_the_cursor_onto_it() {
    let _wd = test_work_dir();
    let timeline = vec![
        user("msg_cola_anchor", 1_000, "第一条消息"),
        assistant(2_000, "第一轮回答。"),
        typed_message(
            "msg_comp",
            MessageRole::Other("compaction".into()),
            Some(3_000),
            vec![Part::Compaction(Compaction {
                status: CompactionStatus::Completed,
                reason: CompactionReason::Auto,
                summary: "保留了结论与未决问题。".into(),
                usage: None,
                error: None,
                recent: String::new(),
            })],
        ),
    ];
    let (_dir, app, _backend, platform) =
        scripted_app(vec![SessionTranscript::new(timeline)], Some(SessionStatus::Busy)).await;
    let _turn = spawn_turn(&app, ctx("ses_test", "第一条消息"));
    wait_for_card_text(&platform, "已压缩上下文").await;

    let cursor = wait_for_cursor(&app, |cursor| {
        cursor
            .frontier
            .as_ref()
            .is_some_and(|frontier| frontier.kind == CursorPartKind::Compaction)
    })
    .await;
    assert_eq!(
        cursor.frontier,
        Some(CursorFrontier {
            message_id: MessageId::new("msg_comp"),
            part_index: 0,
            kind: CursorPartKind::Compaction,
            started_at: Some(3_000),
            delivered_chars: 0,
            prefix_digest: Some(cursor_prefix_digest("")),
        }),
        "the frontier is the compaction's own position"
    );
}

/// The issue's cursor acceptance (#1025): a seeded tail replay — a reasoning
/// frontier resolved CUT AT 0, longer than the card's cap — must confirm an
/// extent and a digest over the SAME prefix. A digest that covers more than the
/// extent the frontier records can never verify, and every later render pass
/// replays the part.
#[tokio::test]
async fn a_seeded_reasoning_replay_confirms_one_prefix() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let session_file = dir.path().join("sessions.json");
    let full = format!("{}{}", "隐".repeat(900), "尾巴");
    let cap = crate::feishu::card::REASONING_TEXT_CAP;
    super::reap::seed_cursor_record(
        &session_file,
        "om_frozen",
        "msg_cola_anchor",
        Some(1_000),
        Some("/work"),
        Some(CursorFrontier {
            message_id: MessageId::new("msg_a_2000"),
            part_index: 0,
            kind: CursorPartKind::Reasoning,
            started_at: Some(2_000),
            delivered_chars: 0,
            prefix_digest: Some(cursor_prefix_digest("")),
        }),
        &[],
    );
    // The run is live: the successor is armed at the cut-at-0 frontier and its
    // seeded render pushes the whole part, so the create's confirmation is the
    // cursor this test reads.
    let running = SessionTranscript::new(vec![
        user("msg_cola_anchor", 1_000, "问题"),
        typed_message(
            "msg_a_2000",
            MessageRole::Assistant,
            Some(2_000),
            vec![Part::Reasoning(ReasoningPart {
                text: full.clone(),
                started_at: Some(2_000),
            })],
        ),
    ]);
    let (app, platform, _backend) =
        super::reap::restarted_app_with_backend(&session_file, running, Some(SessionStatus::Busy)).await;
    platform.given_card_view("om_frozen", super::reap::realistic_card_view());
    spawn_sync(&app);
    super::reap::wait_for_projection(&platform, "om_frozen").await;

    let cursor = wait_for_cursor(&app, |cursor| {
        cursor
            .frontier
            .as_ref()
            .is_some_and(|frontier| frontier.delivered_chars == cap)
    })
    .await;
    let frontier = cursor.frontier.as_ref().expect("the reasoning frontier");
    assert_eq!(frontier.kind, CursorPartKind::Reasoning);
    assert_eq!(
        frontier.prefix_digest,
        Some(cursor_prefix_digest(&full.chars().take(cap).collect::<String>())),
        "the confirmed digest covers exactly the extent the frontier records"
    );
}
