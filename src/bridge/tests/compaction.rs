//! Compaction on the live card (ADR-0082, spec #719, ticket #721): a V2
//! Session's **Compaction** is a Session-scoped fact, so a row for it rides the
//! card that owns the chain at its own server time — a completed one as a folded
//! panel naming its reason, an in-flight one as a live row that updates in place,
//! a failed one as a visible failure row. These tests drive the real bridge
//! (MockBackend + App/Turn) and read the rows off the recorded card PATCHes.

use std::time::Duration;

use crate::backend::{
    Compaction, CompactionReason, CompactionStatus, MessageRole, Part, SessionTranscript, TokenUsage,
    TranscriptMessage,
};
use crate::bridge::chain::{CursorFrontier, CursorPartKind, RenderedCursor, cursor_prefix_digest};
use crate::bridge::test_support::*;
use crate::bridge::tests::drain::{assistant, ctx, scripted_app, spawn_turn, user};
use crate::opencode::types::SessionStatus;

/// One compaction message: the neutral typed fact on a message of its own, the
/// way V2 persists it (its server time is the MESSAGE's `time.created`).
fn compaction(id: &str, created: i64, fact: Compaction) -> TranscriptMessage {
    typed_message(
        id,
        MessageRole::Other("compaction".into()),
        Some(created),
        vec![Part::Compaction(fact)],
    )
}

/// A completed compaction that summarized the older conversation.
fn completed(reason: CompactionReason, summary: &str) -> Compaction {
    Compaction {
        status: CompactionStatus::Completed,
        reason,
        summary: summary.to_string(),
        usage: None,
        error: None,
        recent: "读者已经读过的最近对话".to_string(),
    }
}

/// An in-flight compaction: the Backend has not written the summary yet.
fn running(reason: CompactionReason) -> Compaction {
    Compaction {
        status: CompactionStatus::Running,
        reason,
        summary: String::new(),
        usage: None,
        error: None,
        recent: "读者已经读过的最近对话".to_string(),
    }
}

/// A failed compaction carrying the Backend's own error.
fn failed(reason: CompactionReason, error: &str) -> Compaction {
    Compaction {
        status: CompactionStatus::Failed,
        reason,
        summary: String::new(),
        usage: None,
        error: Some(error.to_string()),
        recent: String::new(),
    }
}

/// The card's body as it renders, in order: a folded panel contributes its title
/// then its body chunks, any other element its own markdown content.
fn body_lines(card: &serde_json::Value) -> Vec<String> {
    let mut lines = Vec::new();
    for element in card["body"]["elements"].as_array().into_iter().flatten() {
        if let Some(title) = element
            .pointer("/header/title/content")
            .and_then(|value| value.as_str())
        {
            lines.push(title.to_string());
        }
        if let Some(content) = element.get("content").and_then(|value| value.as_str()) {
            lines.push(content.to_string());
        }
        for chunk in element["elements"].as_array().into_iter().flatten() {
            if let Some(content) = chunk.get("content").and_then(|value| value.as_str()) {
                lines.push(content.to_string());
            }
        }
    }
    lines
}

/// Every compaction row the card carries, in body order: `(title, body)` — the
/// body empty for a plain row. Recognised by the copy's own markers, so the test
/// reads the card the way the reader does.
fn compaction_rows(card: &serde_json::Value) -> Vec<(String, String)> {
    let lines = body_lines(card);
    let mut rows = Vec::new();
    for (index, line) in lines.iter().enumerate() {
        if line.starts_with("已压缩上下文")
            || line.starts_with("⏳ 压缩中")
            || line.starts_with("⚠️ 压缩失败")
        {
            let body = lines.get(index + 1).cloned().unwrap_or_default();
            rows.push((line.clone(), body));
        }
    }
    rows
}

/// The line index of the first body line carrying `needle`.
fn line_index(card: &serde_json::Value, needle: &str) -> usize {
    body_lines(card)
        .iter()
        .position(|line| line.contains(needle))
        .unwrap_or_else(|| panic!("no line carrying {needle:?}: {:?}", body_lines(card)))
}

/// The `element_id` of every folded panel whose title names a compaction, in body
/// order: two rows carry two identities, one row one.
fn compaction_panel_ids(card: &serde_json::Value) -> Vec<String> {
    card["body"]["elements"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|element| {
            element
                .pointer("/header/title/content")
                .and_then(|value| value.as_str())
                .is_some_and(|title| title.starts_with("已压缩上下文"))
        })
        .filter_map(|element| element.get("element_id").and_then(|value| value.as_str()))
        .map(str::to_string)
        .collect()
}

/// Run the Turn to its end and return the card it settled on.
async fn finished_card(app: &Arc<App>, platform: &RecordingPlatform) -> serde_json::Value {
    let turn = spawn_turn(app, ctx("ses_test", "第一条消息"));
    tokio::time::timeout(Duration::from_secs(5), turn)
        .await
        .expect("the turn must finish")
        .unwrap()
        .unwrap();
    platform
        .updated_cards()
        .await
        .pop()
        .expect("the final card flush")
}

/// A completed compaction renders a folded row titled with its reason, its body
/// the summary, at its own server time among the surrounding parts — the
/// transcript's compaction fact, not a Turn part.
#[tokio::test]
async fn a_completed_compaction_renders_a_folded_row_at_its_server_time() {
    let _wd = test_work_dir();
    let timeline = vec![
        user("msg_cola_anchor", 1_000, "第一条消息"),
        compaction(
            "msg_comp",
            2_000,
            completed(CompactionReason::Auto, "保留了结论与未决问题。"),
        ),
        assistant(3_000, "第一轮回答。"),
    ];
    let (_dir, app, _backend, platform) =
        scripted_app(vec![SessionTranscript::new(timeline)], Some(SessionStatus::Idle)).await;

    let card = finished_card(&app, &platform).await;
    let rows = compaction_rows(&card);
    assert_eq!(
        rows.len(),
        1,
        "one compaction renders one row: {:?}",
        body_lines(&card)
    );
    assert_eq!(rows[0].0, "已压缩上下文（自动）");
    assert_eq!(
        rows[0].1, "保留了结论与未决问题。",
        "the fold body is the summary"
    );
    assert!(
        line_index(&card, "已压缩上下文（自动）") < line_index(&card, "第一轮回答。"),
        "the row takes its server-time place above the reply, never appended: {:?}",
        body_lines(&card)
    );
}

/// A compaction the Turn's own window cannot reach — its server time precedes
/// the Turn's anchor, because it ran while the session was idle — still renders
/// on that Turn's card: the rows come from the session's compaction facts, never
/// from `turn_for_user`.
#[tokio::test]
async fn a_compaction_before_the_turns_anchor_still_renders_on_that_turns_card() {
    let _wd = test_work_dir();
    let timeline = vec![
        compaction(
            "msg_comp",
            500,
            completed(CompactionReason::Auto, "更早的对话已被压缩。"),
        ),
        user("msg_cola_anchor", 2_000, "第一条消息"),
        assistant(3_000, "第一轮回答。"),
    ];
    let (_dir, app, _backend, platform) =
        scripted_app(vec![SessionTranscript::new(timeline)], Some(SessionStatus::Idle)).await;

    let card = finished_card(&app, &platform).await;
    let rows = compaction_rows(&card);
    assert_eq!(
        rows.len(),
        1,
        "a pre-anchor compaction is still this Turn's card's content: {:?}",
        body_lines(&card)
    );
    assert_eq!(rows[0].0, "已压缩上下文（自动）");
    assert!(
        line_index(&card, "已压缩上下文（自动）") < line_index(&card, "第一轮回答。"),
        "it keeps its server-time place: {:?}",
        body_lines(&card)
    );
}

/// A failed compaction is visible and carries the Backend's own error — the case
/// the reader most needs, because the window did not free up. It offers no
/// action: `/compact` stays the trigger.
#[tokio::test]
async fn a_failed_compaction_renders_a_row_carrying_the_backends_error() {
    let _wd = test_work_dir();
    let timeline = vec![
        user("msg_cola_anchor", 1_000, "第一条消息"),
        compaction(
            "msg_comp",
            2_000,
            failed(CompactionReason::Auto, "context length exceeded"),
        ),
        assistant(3_000, "第一轮回答。"),
    ];
    let (_dir, app, _backend, platform) =
        scripted_app(vec![SessionTranscript::new(timeline)], Some(SessionStatus::Idle)).await;

    let card = finished_card(&app, &platform).await;
    assert!(
        card_text(&card).contains("⚠️ 压缩失败：context length exceeded"),
        "the failure row carries the server's error: {:?}",
        body_lines(&card)
    );
    assert!(
        !card_text(&card).contains("已压缩上下文"),
        "a failure is not a completion: {:?}",
        body_lines(&card)
    );
}

/// An in-flight compaction is a live row, and the read that delivered it does
/// not decide the Turn's ending on the spot: the row updates IN PLACE to its
/// terminal state once the Backend writes the summary. The transcript a settled
/// read serves changes from `running` to `completed`, so a card that settled on
/// the first read could never show the summary at all.
#[tokio::test]
async fn an_in_flight_compaction_holds_the_card_then_updates_in_place() {
    let _wd = test_work_dir();
    let live = SessionTranscript::new(vec![
        user("msg_cola_anchor", 1_000, "第一条消息"),
        compaction("msg_comp", 2_000, running(CompactionReason::Auto)),
        assistant(3_000, "第一轮回答。"),
    ]);
    let ended = SessionTranscript::new(vec![
        user("msg_cola_anchor", 1_000, "第一条消息"),
        compaction(
            "msg_comp",
            2_000,
            completed(CompactionReason::Auto, "保留了结论。"),
        ),
        assistant(3_000, "第一轮回答。"),
    ]);
    // Both reads look settled (an idle session, a terminal step finish): only
    // the newly delivered compaction row keeps the ending off the first one.
    let (_dir, app, _backend, platform) = scripted_app(vec![live, ended], Some(SessionStatus::Idle)).await;

    let turn = spawn_turn(&app, ctx("ses_test", "第一条消息"));
    tokio::time::timeout(Duration::from_secs(5), turn)
        .await
        .expect("the turn must finish once the compaction ended")
        .unwrap()
        .unwrap();

    let card = platform
        .updated_cards()
        .await
        .pop()
        .expect("the final card flush");
    let rows = compaction_rows(&card);
    assert_eq!(
        rows.len(),
        1,
        "the row updates in place, never accumulating: {:?}",
        body_lines(&card)
    );
    assert_eq!(rows[0].0, "已压缩上下文（自动）");
    assert_eq!(rows[0].1, "保留了结论。");
    assert!(
        !card_text(&card).contains("⏳ 压缩中"),
        "the terminal card no longer shows the live row: {:?}",
        body_lines(&card)
    );
    assert!(
        line_index(&card, "已压缩上下文（自动）") < line_index(&card, "第一轮回答。"),
        "the updated row keeps its place: {:?}",
        body_lines(&card)
    );
}

/// A compaction that starts while the chain is live shows its in-flight row on
/// the card, and the row keeps its place when it ends: the same timeline entry
/// becomes the folded row, never a second one.
#[tokio::test]
async fn a_live_turns_in_flight_compaction_renders_a_live_row() {
    let _wd = test_work_dir();
    let live = SessionTranscript::new(vec![
        user("msg_cola_anchor", 1_000, "第一条消息"),
        compaction("msg_comp", 2_000, running(CompactionReason::Manual)),
        assistant(3_000, "第一轮回答。"),
    ]);
    let ended = SessionTranscript::new(vec![
        user("msg_cola_anchor", 1_000, "第一条消息"),
        compaction(
            "msg_comp",
            2_000,
            completed(CompactionReason::Manual, "保留了结论。"),
        ),
        assistant(3_000, "第一轮回答。"),
    ]);
    let (_dir, app, backend, platform) = scripted_app(vec![live, ended], Some(SessionStatus::Busy)).await;

    let turn = spawn_turn(&app, ctx("ses_test", "第一条消息"));
    wait_for_card_update(&platform, "the in-flight row", CardUpdates::Any, |card| {
        card_text(card).contains("⏳ 压缩中（手动）")
    })
    .await;
    let in_flight = platform
        .updated_cards()
        .await
        .into_iter()
        .find(|card| card_text(card).contains("⏳ 压缩中（手动）"))
        .expect("the recorded live row");
    let row_index = line_index(&in_flight, "⏳ 压缩中（手动）");

    // The run reads idle: the drain settles on a fresh read, which flushes the
    // terminal row into the place the live one held.
    backend
        .set_session_status("ses_test", Some(SessionStatus::Idle))
        .await;
    tokio::time::timeout(Duration::from_secs(5), turn)
        .await
        .expect("the turn must finish once the run reads idle")
        .unwrap()
        .unwrap();

    let card = platform
        .updated_cards()
        .await
        .pop()
        .expect("the final card flush");
    let rows = compaction_rows(&card);
    assert_eq!(rows.len(), 1, "{:?}", body_lines(&card));
    assert_eq!(rows[0].0, "已压缩上下文（手动）");
    assert_eq!(
        line_index(&card, "已压缩上下文（手动）"),
        row_index,
        "the terminal row takes the very place the live one held: {:?}",
        body_lines(&card)
    );
}

/// The compaction request's own usage rides its row and never becomes the Turn
/// Footer's context figure: V2's `cost`/`tokens` on a compaction are the summary
/// request's own consumption, not the size of the context it produced. An
/// explicitly reported ZERO is still a reported usage — presence, not magnitude,
/// decides whether the row names one.
#[tokio::test]
async fn the_compactions_own_usage_rides_its_row_and_never_the_turn_footer() {
    let _wd = test_work_dir();
    let mut summarized = completed(CompactionReason::Manual, "保留了结论。");
    summarized.usage = Some(TokenUsage {
        input: 300_000,
        output: 0,
        total: 300_000,
        cache_read: 0,
        cache_write: 0,
    });
    let mut zero_usage = completed(CompactionReason::Auto, "更早的对话已被压缩。");
    zero_usage.usage = Some(TokenUsage::default());
    let mut reply = assistant(4_000, "第一轮回答。");
    reply.tokens = Some(TokenUsage {
        input: 42_000,
        output: 0,
        total: 42_000,
        cache_read: 0,
        cache_write: 0,
    });
    let timeline = vec![
        user("msg_cola_anchor", 1_000, "第一条消息"),
        compaction("msg_comp_zero", 2_000, zero_usage),
        compaction("msg_comp", 3_000, summarized),
        reply,
    ];
    let (_dir, app, _backend, platform) =
        scripted_app(vec![SessionTranscript::new(timeline)], Some(SessionStatus::Idle)).await;

    let card = finished_card(&app, &platform).await;
    let rows = compaction_rows(&card);
    assert_eq!(rows.len(), 2, "{:?}", body_lines(&card));
    assert_eq!(
        rows[1].0, "已压缩上下文（手动） · 300k tokens",
        "the row names its reason and carries the compaction's own usage"
    );
    assert_eq!(
        rows[0].0, "已压缩上下文（自动） · 0 tokens",
        "a reported zero usage is a reported usage: {rows:?}"
    );
    let text = card_text(&card);
    assert!(
        text.contains("📊 上下文 42k"),
        "the footer keeps the conversation's own figure: {text}"
    );
    assert!(
        !text.contains("📊 上下文 300k"),
        "the compaction's usage never becomes the context figure: {text}"
    );
}

/// The fold body carries the summary chunked past the card's per-element budget,
/// and the protocol's `recent` field — the verbatim tail kept beside the summary
/// — never reaches the card.
#[tokio::test]
async fn the_summary_is_chunked_and_the_recent_tail_never_renders() {
    let _wd = test_work_dir();
    let summary = "摘要正文。".repeat(1_200);
    let timeline = vec![
        user("msg_cola_anchor", 1_000, "第一条消息"),
        compaction("msg_comp", 2_000, completed(CompactionReason::Auto, &summary)),
        assistant(3_000, "第一轮回答。"),
    ];
    let (_dir, app, _backend, platform) =
        scripted_app(vec![SessionTranscript::new(timeline)], Some(SessionStatus::Idle)).await;

    let card = finished_card(&app, &platform).await;
    assert!(
        !card_text(&card).contains("读者已经读过的最近对话"),
        "the protocol's recent tail is never rendered"
    );
    let panel = card["body"]["elements"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|element| {
            element
                .pointer("/header/title/content")
                .and_then(|value| value.as_str())
                .is_some_and(|title| title.starts_with("已压缩上下文"))
        })
        .expect("the compaction fold");
    let chunks: Vec<&str> = panel["elements"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|chunk| chunk.get("content").and_then(|value| value.as_str()))
        .collect();
    assert!(
        chunks.len() > 1,
        "a summary past the per-element budget is chunked: {} chunks",
        chunks.len()
    );
    for chunk in &chunks {
        assert!(
            chunk.chars().count() <= crate::feishu::card::MAX_ELEMENT_TEXT_CHARS,
            "no chunk may exceed the card's per-element budget: {}",
            chunk.chars().count()
        );
    }
    assert_eq!(
        chunks.concat(),
        summary,
        "the chunks are the summary, whole and in order"
    );
}

/// A completion that carries `providerContext` is labelled like any other: a
/// provider-native checkpoint is V2's mechanism, not a different domain fact.
#[tokio::test]
async fn a_checkpoint_completion_is_labelled_like_any_other() {
    let _wd = test_work_dir();
    let timeline = vec![
        user("msg_cola_anchor", 1_000, "第一条消息"),
        compaction(
            "msg_comp",
            2_000,
            completed(CompactionReason::Manual, "保留了结论。"),
        ),
        assistant(3_000, "第一轮回答。"),
    ];
    let (_dir, app, _backend, platform) =
        scripted_app(vec![SessionTranscript::new(timeline)], Some(SessionStatus::Idle)).await;

    let card = finished_card(&app, &platform).await;
    let rows = compaction_rows(&card);
    assert_eq!(rows.len(), 1, "{:?}", body_lines(&card));
    assert_eq!(
        rows[0].0, "已压缩上下文（手动）",
        "a checkpoint completion gains no label of its own"
    );
}

/// Two DISTINCT compactions that happen to render the same fields are two rows,
/// not one: the dedup is the position the card holds a row at, never the fields
/// alone — otherwise a second compaction with an identical summary (a repeated
/// `/compact` over the same context, say) would vanish and could never hold the
/// card's settlement. Each row keeps its own server-time place and identity.
#[tokio::test]
async fn two_identical_compactions_at_distinct_positions_both_render() {
    let _wd = test_work_dir();
    let shared = completed(CompactionReason::Auto, "保留了结论。");
    let first = SessionTranscript::new(vec![
        user("msg_cola_anchor", 1_000, "第一条消息"),
        compaction("msg_comp_a", 2_000, shared.clone()),
        assistant(4_000, "第一轮回答。"),
    ]);
    // The second compaction lands later, on a session whose card already shows
    // the first one's row.
    let mut later = first.clone();
    later.messages.push(compaction("msg_comp_b", 5_000, shared));
    let (_dir, app, backend, platform) = scripted_app(vec![first], Some(SessionStatus::Busy)).await;

    let turn = spawn_turn(&app, ctx("ses_test", "第一条消息"));
    wait_for_card_update(&platform, "the first compaction row", CardUpdates::Any, |card| {
        card_text(card).contains("已压缩上下文（自动）")
    })
    .await;
    backend
        .given_transcript_after_build("ses_test", vec![later])
        .await;
    wait_for_card_update(&platform, "both compaction rows", CardUpdates::Any, |card| {
        compaction_panel_ids(card).len() == 2
    })
    .await;
    backend
        .set_session_status("ses_test", Some(SessionStatus::Idle))
        .await;
    tokio::time::timeout(Duration::from_secs(5), turn)
        .await
        .expect("the turn must finish once the run reads idle")
        .unwrap()
        .unwrap();

    let card = platform
        .updated_cards()
        .await
        .pop()
        .expect("the final card flush");
    let rows = compaction_rows(&card);
    assert_eq!(
        rows.len(),
        2,
        "each compaction is a row of its own: {:?}",
        body_lines(&card)
    );
    assert!(
        rows.iter()
            .all(|(title, body)| title == "已压缩上下文（自动）" && body == "保留了结论。"),
        "both rows render their own fields: {rows:?}"
    );
    let ids = compaction_panel_ids(&card);
    assert_ne!(ids[0], ids[1], "the rows are distinct entries: {ids:?}");

    // Each row keeps its own server-time place: the first before the reply, the
    // second after it.
    let lines = body_lines(&card);
    let row_lines: Vec<usize> = lines
        .iter()
        .enumerate()
        .filter(|(_, line)| line.starts_with("已压缩上下文"))
        .map(|(index, _)| index)
        .collect();
    let reply = line_index(&card, "第一轮回答。");
    assert!(
        row_lines[0] < reply && reply < row_lines[1],
        "each row sits at its own server time: {lines:?}"
    );
}

/// A compaction the chain's Rendered Cursor has already passed is not replayed:
/// the cursor is the card's delivered bound, so a successor card renders only the
/// facts past it. The cursor is seeded here the way a confirmed write leaves it on
/// the chain's record.
#[tokio::test]
async fn a_compaction_behind_the_rendered_cursor_is_not_replayed() {
    let _wd = test_work_dir();
    let timeline = vec![
        user("msg_cola_anchor", 1_000, "第一条消息"),
        compaction(
            "msg_comp",
            2_000,
            completed(CompactionReason::Auto, "更早的对话已被压缩。"),
        ),
        assistant(3_000, "第一轮回答。"),
    ];
    let (_dir, app, backend, platform) =
        scripted_app(vec![SessionTranscript::new(timeline)], Some(SessionStatus::Idle)).await;
    // The chain already delivered the reply (the cursor's frontier): the
    // compaction before it is delivered too.
    let cursor = RenderedCursor {
        frontier: Some(CursorFrontier {
            message_id: crate::backend::MessageId::new("msg_a_3000"),
            part_index: 0,
            kind: CursorPartKind::Text,
            started_at: Some(3_000),
            delivered_chars: "第一轮回答。".chars().count(),
            prefix_digest: Some(cursor_prefix_digest("第一轮回答。")),
        }),
        live_calls: Default::default(),
    };
    let chains = app.cards_handle().chains;
    chains.track(
        "ses_test",
        "om_card",
        crate::backend::MessageId::new("msg_cola_anchor"),
        Some(1_000),
        Some("/work"),
        None,
    );
    chains.advance_cursor("ses_test", "om_card", &cursor);
    let _ = &backend;

    let card = finished_card(&app, &platform).await;
    assert!(
        compaction_rows(&card).is_empty(),
        "a compaction past the cursor is not re-rendered: {:?}",
        body_lines(&card)
    );
}

/// `/compact` acknowledges in Chinese like every other command reply. It stays a
/// submit acknowledgement: the compaction itself is the card's to render, so
/// nothing about a result rides this reply.
#[tokio::test]
async fn compact_acknowledges_in_chinese() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let backend = MockBackend::new(realistic_parts());
    let compact_calls = backend.compact_calls.clone();
    let (app, platform) = build_app(cfg, backend).await;
    seed_session(&app, "ses_test", "/work").await;

    send_command_in(
        &app,
        "/compact",
        crate::config::ThreadKey::new("chat_1".into(), "chat_1".into()),
        "msg_compact",
        crate::config::ConversationKind::P2p,
    )
    .await;

    let texts = platform.texts().await.join("\n");
    assert!(
        texts.contains("已提交压缩"),
        "the acknowledgement is Chinese, like every other command reply: {texts}"
    );
    assert!(
        !texts.contains("Compacting..."),
        "the English placeholder is gone: {texts}"
    );
    assert_eq!(
        compact_calls.lock().await.as_slice(),
        ["ses_test".to_string()],
        "the acknowledgement still follows the submit"
    );
}
