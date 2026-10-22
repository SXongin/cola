//! Compaction on the live card (ADR-0082, spec #719, ticket #721): a V2
//! Session's **Compaction** is a Session-scoped fact, so a row for it rides the
//! card that owns the chain at its own server time — a completed one as a folded
//! panel naming its reason, an in-flight one as a live row that updates in place,
//! a failed one as a visible failure row. These tests drive the real bridge
//! (MockBackend + App/Turn) and read the rows off the recorded card PATCHes.

use std::time::Duration;

use crate::backend::types::SessionStatus;
use crate::backend::{
    Compaction, CompactionReason, CompactionStatus, MessageRole, Part, SessionTranscript, TokenUsage,
    TranscriptMessage,
};
use crate::bridge::chain::{CursorFrontier, CursorPartKind, RenderedCursor, cursor_prefix_digest};
use crate::bridge::test_support::*;
use crate::bridge::tests::drain::{assistant, ctx, scripted_app, scripted_app_with, spawn_turn, user};
use crate::bridge::turn::PromptContext;

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

/// A compaction message with no server time of its own: the decoder tolerates a
/// payload without `time.created`, and the row is then keyed by arrival order
/// rather than a clock.
fn timeless_compaction(id: &str, fact: Compaction) -> TranscriptMessage {
    typed_message(
        id,
        MessageRole::Other("compaction".into()),
        None,
        vec![Part::Compaction(fact)],
    )
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

/// A later Turn's own prompt: its own cola message id, so the read's Turn
/// window (and the anchor capture) is that turn's, not the first's.
fn turn_ctx(cola_message_id: &str, text: &str) -> PromptContext {
    let mut context = ctx("ses_test", text);
    context.cola_message_id = Some(cola_message_id.to_string());
    context
}

/// The fold body of the card's compaction row, in element order: the panel's own
/// markdown chunks (the truncation marker included, when there is one).
fn compaction_fold_chunks(card: &serde_json::Value) -> Vec<String> {
    card["body"]["elements"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|element| {
            element
                .pointer("/header/title/content")
                .and_then(|value| value.as_str())
                .is_some_and(|title| title.starts_with("已压缩上下文"))
        })
        .expect("the compaction fold")["elements"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|chunk| chunk.get("content").and_then(|value| value.as_str()))
        .map(str::to_string)
        .collect()
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

/// An adopted session's Session Snapshot carries the compaction in its 最近对话
/// tail, at its server position among the kept messages (ADR-0082, ticket #723):
/// a session whose visible history looks oddly short has its explanation
/// sitting right where it happened, instead of a reader wondering why the
/// conversation starts mid-thread.
#[tokio::test]
async fn the_snapshot_tail_carries_a_compaction_at_its_server_position() {
    let _wd = test_work_dir();
    let timeline = vec![
        user("msg_u1", 1_000, "第一个问题"),
        assistant(2_000, "第一个回答"),
        compaction(
            "msg_comp",
            3_000,
            completed(CompactionReason::Auto, "保留了结论。"),
        ),
        user("msg_u2", 4_000, "第二个问题"),
        assistant(5_000, "第二个回答"),
    ];
    let mut backend = MockBackend::new(realistic_parts());
    // Busy, so the re-switch reports the full snapshot instead of the compact
    // 已切换 state (ADR-0028 suppression).
    backend.with_session_status("ses_own1", Some(SessionStatus::Busy));
    backend.given_transcript("ses_own1", vec![SessionTranscript::new(timeline)]);
    let (app, platform, _dir) = build_reeswitch_app(backend).await;

    send_command(&app, "/switch 本项目", "msg_switch").await;
    let card = platform
        .replied_cards()
        .await
        .into_iter()
        .next()
        .expect("the busy re-switch reports a snapshot");

    let rows = compaction_rows(&card);
    assert_eq!(
        rows.len(),
        1,
        "the tail carries one marker: {:?}",
        body_lines(&card)
    );
    assert_eq!(rows[0].0, "已压缩上下文（自动）");
    assert_eq!(rows[0].1, "保留了结论。", "the marker's fold is the summary");
    assert!(
        line_index(&card, "第一个回答") < line_index(&card, "已压缩上下文（自动）"),
        "the marker sits below the message it followed: {:?}",
        body_lines(&card)
    );
    assert!(
        line_index(&card, "已压缩上下文（自动）") < line_index(&card, "第二个问题"),
        "and above the one that followed it: {:?}",
        body_lines(&card)
    );
}

/// A transcript that kept no conversation message at all — a `/compact` on a
/// session whose only readable fact is the compaction itself — still carries its
/// marker: the snapshot's recent tail shows the compaction "either way"
/// (ADR-0082, ticket #723). A reader who sees nothing else at least learns why.
#[tokio::test]
async fn the_snapshot_tail_carries_a_compaction_with_no_conversation_message() {
    let _wd = test_work_dir();
    let timeline = vec![compaction(
        "msg_comp",
        1_000,
        completed(CompactionReason::Manual, "保留了结论。"),
    )];
    let mut backend = MockBackend::new(realistic_parts());
    backend.with_session_status("ses_own1", Some(SessionStatus::Busy));
    backend.given_transcript("ses_own1", vec![SessionTranscript::new(timeline)]);
    let (app, platform, _dir) = build_reeswitch_app(backend).await;

    send_command(&app, "/switch 本项目", "msg_switch").await;
    let card = platform
        .replied_cards()
        .await
        .into_iter()
        .next()
        .expect("the busy re-switch reports a snapshot");

    let rows = compaction_rows(&card);
    assert_eq!(
        rows.len(),
        1,
        "a compaction-only session still carries its marker: {:?}",
        body_lines(&card)
    );
    assert_eq!(rows[0].0, "已压缩上下文（手动）");
    assert_eq!(rows[0].1, "保留了结论。", "the marker's fold is the summary");
}

/// A compaction inside the tail's window never displaces a message: a marker is
/// not a message, so the snapshot's 最近对话 keeps its four messages and the
/// marker rides between them (ADR-0082, ticket #723). A compaction that
/// PRECEDED the window's oldest message stays out of it entirely.
#[tokio::test]
async fn a_compaction_in_the_snapshot_tail_never_displaces_a_message() {
    let _wd = test_work_dir();
    let timeline = vec![
        user("msg_u0", 500, "窗口外的问题"),
        user("msg_u1", 1_000, "消息一"),
        assistant(2_000, "回答一"),
        user("msg_u2", 3_000, "消息二"),
        compaction(
            "msg_comp",
            3_500,
            completed(CompactionReason::Auto, "保留了结论。"),
        ),
        assistant(4_000, "回答二"),
        user("msg_u3", 5_000, "消息三"),
        assistant(6_000, "回答三"),
    ];
    let mut backend = MockBackend::new(realistic_parts());
    backend.with_session_status("ses_own1", Some(SessionStatus::Busy));
    backend.given_transcript("ses_own1", vec![SessionTranscript::new(timeline)]);
    let (app, platform, _dir) = build_reeswitch_app(backend).await;

    send_command(&app, "/switch 本项目", "msg_switch").await;
    let card = platform
        .replied_cards()
        .await
        .into_iter()
        .next()
        .expect("the busy re-switch reports a snapshot");

    let lines = body_lines(&card);
    let previews: Vec<&str> = lines.iter().map(|line| line.as_str()).collect();
    for message in ["消息二", "回答二", "消息三", "回答三"] {
        assert!(
            previews
                .iter()
                .any(|line| line.starts_with(&format!("👤 {message}"))
                    || line.starts_with(&format!("🤖 {message}"))),
            "the tail keeps its four messages, {message} among them: {lines:?}"
        );
    }
    assert!(
        !card_text(&card).contains("消息一") && !card_text(&card).contains("回答一"),
        "the fifth-newest message is past the four-message budget: {lines:?}"
    );
    assert_eq!(
        compaction_rows(&card).len(),
        1,
        "the marker rides between the messages: {lines:?}"
    );
    assert!(
        line_index(&card, "消息二") < line_index(&card, "已压缩上下文（自动）")
            && line_index(&card, "已压缩上下文（自动）") < line_index(&card, "回答二"),
        "at its own server position: {lines:?}"
    );
    assert!(
        !card_text(&card).contains("窗口外的问题"),
        "a compaction's message is not a message, but the window's cut is still \
         the oldest KEPT message: nothing before it appears: {lines:?}"
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

/// A failure whose error is far past one element's budget is clipped on the card
/// — the row keeps the head of the error and names the truncation — and the whole
/// card stays inside the JSON budget Feishu accepts: a row that blew it would
/// vanish with the turn's update, and a failure is what the reader most needs.
#[tokio::test]
async fn a_long_failed_error_is_clipped_on_the_card() {
    use crate::feishu::card::MAX_ELEMENT_TEXT_CHARS;
    use crate::feishu::card::compaction::ERROR_TRUNCATED_MARKER;

    let _wd = test_work_dir();
    let error = "字".repeat(MAX_ELEMENT_TEXT_CHARS * 100);
    let timeline = vec![
        user("msg_cola_anchor", 1_000, "第一条消息"),
        compaction("msg_comp", 2_000, failed(CompactionReason::Auto, &error)),
        assistant(3_000, "第一轮回答。"),
    ];
    let (_dir, app, _backend, platform) =
        scripted_app(vec![SessionTranscript::new(timeline)], Some(SessionStatus::Idle)).await;

    let card = finished_card(&app, &platform).await;
    let text = card_text(&card);
    assert!(
        text.contains(ERROR_TRUNCATED_MARKER),
        "the clip is named on the card"
    );
    let rows = compaction_rows(&card);
    assert!(
        rows[0].0.chars().count() <= MAX_ELEMENT_TEXT_CHARS,
        "the clipped title stays one budgeted element: {} chars",
        rows[0].0.chars().count()
    );
    assert!(
        rows[0].0.contains(&"字".repeat(100)),
        "the head of the error is kept"
    );
    assert!(
        card.to_string().len() < crate::feishu::card::MAX_CARD_JSON_CHARS,
        "the whole card stays inside the JSON budget: {} bytes",
        card.to_string().len()
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

/// A compaction whose message reports NO server time is keyed by arrival order,
/// and its row updates in place just the same: a revision that arrived after the
/// running row must replace it, not sit beside it. A fresh synthetic key per
/// revision would leave the live row behind, and the settlement gate would keep
/// reading it.
#[tokio::test]
async fn a_timeless_compaction_updates_its_row_in_place() {
    let _wd = test_work_dir();
    let live = SessionTranscript::new(vec![
        user("msg_cola_anchor", 1_000, "第一条消息"),
        timeless_compaction("msg_comp", running(CompactionReason::Auto)),
        assistant(3_000, "第一轮回答。"),
    ]);
    let ended = SessionTranscript::new(vec![
        user("msg_cola_anchor", 1_000, "第一条消息"),
        timeless_compaction("msg_comp", completed(CompactionReason::Auto, "保留了结论。")),
        assistant(3_000, "第一轮回答。"),
    ]);
    let (_dir, app, backend, platform) = scripted_app(vec![live, ended], Some(SessionStatus::Busy)).await;

    let turn = spawn_turn(&app, ctx("ses_test", "第一条消息"));
    wait_for_card_update(&platform, "the timeless live row", CardUpdates::Any, |card| {
        card_text(card).contains("⏳ 压缩中（自动）")
    })
    .await;
    let in_flight = platform
        .updated_cards()
        .await
        .into_iter()
        .find(|card| card_text(card).contains("⏳ 压缩中（自动）"))
        .expect("the recorded live row");
    let row_index = line_index(&in_flight, "⏳ 压缩中（自动）");

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
        1,
        "the terminal revision replaces the live row: {:?}",
        body_lines(&card)
    );
    assert_eq!(rows[0].0, "已压缩上下文（自动）");
    assert_eq!(rows[0].1, "保留了结论。");
    assert!(
        !card_text(&card).contains("⏳ 压缩中"),
        "no live row is left behind: {:?}",
        body_lines(&card)
    );
    assert_eq!(
        line_index(&card, "已压缩上下文（自动）"),
        row_index,
        "the updated row keeps its arrival place: {:?}",
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
/// — never reaches the card. The summary sits exactly at the row's cap, so it is
/// rendered whole (the cap's own clipping is pinned separately).
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
    let chunks = compaction_fold_chunks(&card);
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

/// A body whose CEILING exceeds a whole card is placed on a card of its own: one
/// timeline row cannot be split, so the splitter finalizes the card carrying the
/// row as a split prefix and the turn's other content rides a continuation. That
/// placement is what the ceiling buys — the row's real payload is bounded by its
/// caps, but the splitter must not be told it shares a card with content the card
/// layer may yet enlarge past Feishu's limit.
#[tokio::test]
async fn a_large_compaction_row_takes_a_card_of_its_own() {
    use crate::feishu::card::compaction::MAX_COMPACTION_SUMMARY_CHARS;

    let _wd = test_work_dir();
    let summary = "摘".repeat(MAX_COMPACTION_SUMMARY_CHARS);
    let timeline = vec![
        user("msg_cola_anchor", 1_000, "第一条消息"),
        compaction("msg_comp", 2_000, completed(CompactionReason::Auto, &summary)),
        assistant(3_000, "第一轮回答。"),
    ];
    let (_dir, app, _backend, platform) =
        scripted_app(vec![SessionTranscript::new(timeline)], Some(SessionStatus::Idle)).await;

    let finalized = finished_card(&app, &platform).await;
    assert!(
        compaction_fold_chunks(&finalized).len() > 1,
        "the row renders whole on its own card: {:?}",
        body_lines(&finalized)
    );
    assert!(
        card_header(&finalized).contains("部分完成"),
        "the row's card finalizes as a split prefix: {}",
        card_header(&finalized)
    );
    assert!(
        !card_text(&finalized).contains("第一轮回答。"),
        "the reply moved to the continuation: {:?}",
        body_lines(&finalized)
    );

    // The continuation is the newest replied card, and the row is not repeated on
    // it: one row, one card.
    let (_, continuation) = platform
        .replied_cards_with_targets()
        .await
        .pop()
        .expect("the continuation reply");
    assert!(
        card_text(&continuation).contains("第一轮回答。"),
        "the reply rides the continuation: {:?}",
        body_lines(&continuation)
    );
    assert!(
        !card_text(&continuation).contains("已压缩上下文"),
        "the row is not repeated on the continuation: {:?}",
        body_lines(&continuation)
    );
}

/// A summary past the row's cap is CLIPPED, not fatal: the row keeps the head of
/// the summary and says it was cut, every element stays inside the per-element
/// budget, and the whole card — one timeline row cannot be split — stays inside
/// the JSON budget Feishu accepts. The stored fact keeps its summary whole; only
/// the card is capped.
#[tokio::test]
async fn a_summary_past_the_cap_is_clipped_with_a_marker() {
    use crate::feishu::card::compaction::{MAX_COMPACTION_SUMMARY_CHARS, SUMMARY_TRUNCATED_MARKER};

    let _wd = test_work_dir();
    let summary = "a".repeat(MAX_COMPACTION_SUMMARY_CHARS + 5_000);
    let timeline = vec![
        user("msg_cola_anchor", 1_000, "第一条消息"),
        compaction("msg_comp", 2_000, completed(CompactionReason::Auto, &summary)),
        assistant(3_000, "第一轮回答。"),
    ];
    let (_dir, app, _backend, platform) =
        scripted_app(vec![SessionTranscript::new(timeline)], Some(SessionStatus::Idle)).await;

    let card = finished_card(&app, &platform).await;
    let chunks = compaction_fold_chunks(&card);
    assert_eq!(
        chunks.last().map(String::as_str),
        Some(SUMMARY_TRUNCATED_MARKER),
        "the clip is named on the card, never silent"
    );
    for chunk in &chunks {
        assert!(
            chunk.chars().count() <= crate::feishu::card::MAX_ELEMENT_TEXT_CHARS,
            "no chunk may exceed the card's per-element budget: {}",
            chunk.chars().count()
        );
    }
    let visible: String = chunks[..chunks.len() - 1].concat();
    assert_eq!(
        visible,
        "a".repeat(MAX_COMPACTION_SUMMARY_CHARS),
        "the head of the summary is kept, exactly up to the cap"
    );
    assert!(
        card.to_string().len() < crate::feishu::card::MAX_CARD_JSON_CHARS,
        "the whole card stays inside the JSON budget: {} bytes",
        card.to_string().len()
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

/// A completed compaction is a SESSION-scoped fact delivered **exactly once**
/// (ADR-0082 §3): the card that carried its row is the one it stays on. A later
/// Turn opens a NEW card on a new chain, so its walk must not surface the row
/// again — the row belongs to the session's delivered past, not to every
/// subsequent card. Driven over two consecutive Turns through the real path: the
/// first Turn renders the row once, the second Turn's card must not carry it at
/// all.
#[tokio::test]
async fn a_completed_compaction_is_never_re_rendered_on_a_later_turns_card() {
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

    // The first Turn renders the row on its own card; its confirmed write is
    // what carries the delivered position past the compaction — and that fact
    // has to outlive the chain the terminal card releases.
    let first = spawn_turn(&app, ctx("ses_test", "第一条消息"));
    tokio::time::timeout(Duration::from_secs(5), first)
        .await
        .expect("the first Turn must finish")
        .unwrap()
        .unwrap();
    let first_card = platform
        .updated_cards()
        .await
        .pop()
        .expect("the first Turn's final card flush");
    let rows = compaction_rows(&first_card);
    assert_eq!(
        rows.len(),
        1,
        "the compaction renders once on the Turn that delivered it: {:?}",
        body_lines(&first_card)
    );
    assert_eq!(rows[0].0, "已压缩上下文（自动）");
    assert_eq!(rows[0].1, "更早的对话已被压缩。");

    // The second Turn: the session's transcript grows by that turn's own
    // exchange, and a new card opens on the same chain.
    {
        let mut scripts = backend.transcript_scripts.lock().await;
        let transcript = &mut scripts.get_mut("ses_test").expect("the scripted read")[0];
        transcript
            .messages
            .push(user("msg_cola_second", 4_000, "第二条消息"));
        transcript.messages.push(assistant(5_000, "第二轮回答。"));
    }
    let updates_before = platform.updated_cards().await.len();
    let second = spawn_turn(&app, turn_ctx("msg_cola_second", "第二条消息"));
    tokio::time::timeout(Duration::from_secs(5), second)
        .await
        .expect("the second Turn must finish")
        .unwrap()
        .unwrap();

    let updates = platform.updated_cards().await;
    let second_turn_cards = &updates[updates_before..];
    assert!(
        !second_turn_cards.is_empty(),
        "the second Turn must render its own card"
    );
    let second_turn_bodies: Vec<Vec<String>> = second_turn_cards.iter().map(body_lines).collect();
    let carrying: Vec<&Vec<String>> = second_turn_bodies
        .iter()
        .filter(|lines| {
            lines
                .iter()
                .any(|line| line.starts_with("已压缩上下文") || line.starts_with("⏳ 压缩中"))
        })
        .collect();
    assert!(
        carrying.is_empty(),
        "the row was already delivered by the first Turn's card — exactly once — \
         so the second Turn's card must not carry it: {carrying:?}"
    );
    assert!(
        card_text(second_turn_cards.last().unwrap()).contains("第二轮回答。"),
        "the second Turn's card carries its own reply: {:?}",
        body_lines(second_turn_cards.last().unwrap())
    );
    assert!(
        !card_text(second_turn_cards.last().unwrap()).contains("第一轮回答。"),
        "and not the previous Turn's: {:?}",
        body_lines(second_turn_cards.last().unwrap())
    );
}

/// The one-time migration replay (ADR-0082 amendment): a Session compacted
/// while an earlier build was live already had its row surfaced, but that build
/// wrote no `delivered` section, so the sidecar cannot say the compaction was
/// delivered. The upgrade therefore replays it **once**, on the first card after
/// it — and that card's own confirmed write hands the position over, so the
/// replay cannot repeat.
#[tokio::test]
async fn a_sidecar_without_the_delivered_section_replays_a_compaction_once() {
    let _wd = test_work_dir();
    let timeline = vec![
        user("msg_cola_anchor", 1_000, "第一条消息"),
        compaction(
            "msg_comp",
            2_000,
            completed(CompactionReason::Auto, "升级前的压缩。"),
        ),
        assistant(3_000, "第一轮回答。"),
    ];
    let (dir, app, backend, platform) = scripted_app_with(
        vec![SessionTranscript::new(timeline)],
        Some(SessionStatus::Idle),
        |cfg| {
            // The shape a pre-fix build leaves behind: the terminal card
            // released the chain's record, and the file has no `delivered`
            // section at all.
            std::fs::write(
                cfg.bridge.session_file.with_file_name("chain_records.json"),
                r#"{"records":{},"announcements":{}}"#,
            )
            .unwrap();
        },
    )
    .await;
    let sidecar_path = dir.path().join("chain_records.json");

    // The first card after the upgrade replays the row: the sidecar carries no
    // position saying this compaction was ever surfaced.
    let first = spawn_turn(&app, ctx("ses_test", "第一条消息"));
    tokio::time::timeout(Duration::from_secs(5), first)
        .await
        .expect("the first Turn must finish")
        .unwrap()
        .unwrap();
    let first_card = platform
        .updated_cards()
        .await
        .pop()
        .expect("the first Turn's final card flush");
    let rows = compaction_rows(&first_card);
    assert_eq!(
        rows.len(),
        1,
        "a pre-fix sidecar cannot say the row was surfaced, so it replays: {:?}",
        body_lines(&first_card)
    );
    assert_eq!(rows[0].1, "升级前的压缩。");

    // That write is the handover: the released record's confirmed cursor becomes
    // the Session's delivered position.
    let sidecar: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&sidecar_path).unwrap()).unwrap();
    assert_eq!(
        sidecar["delivered"]["ses_test"], "msg_a_3000",
        "the first card's confirmed write gives the Session its position: {sidecar}"
    );

    // And so the replay cannot repeat: the next Turn's card must not carry it.
    {
        let mut scripts = backend.transcript_scripts.lock().await;
        let transcript = &mut scripts.get_mut("ses_test").expect("the scripted read")[0];
        transcript
            .messages
            .push(user("msg_cola_second", 4_000, "第二条消息"));
        transcript.messages.push(assistant(5_000, "第二轮回答。"));
    }
    let second = spawn_turn(&app, turn_ctx("msg_cola_second", "第二条消息"));
    tokio::time::timeout(Duration::from_secs(5), second)
        .await
        .expect("the second Turn must finish")
        .unwrap()
        .unwrap();
    let card = platform
        .updated_cards()
        .await
        .pop()
        .expect("the second Turn's final card flush");
    assert!(
        compaction_rows(&card).is_empty(),
        "the replay was pinned by the first card's write: {:?}",
        body_lines(&card)
    );
    assert!(
        card_text(&card).contains("第二轮回答。"),
        "the second Turn's card carries its own reply: {:?}",
        body_lines(&card)
    );
}

/// The terminal card's OWN read can be the first to carry a compaction: one
/// that lands after the turn's last live read and before finalization settles
/// renders its row in the settling pass, which deliberately writes no card — so
/// the row's FIRST PATCH is the terminal card's own ending write. That write is
/// then the one confirming the frontier that names the row, and the SAME flush
/// releases the chain's record (the card just reached its ending), handing the
/// frontier to the session's delivered position. Exactly-once (ADR-0082 §3)
/// hangs on that handover: the next Turn opens a new chain, so a released
/// position that lost the row's frontier would replay it on the new card. The
/// compaction is the read's NEWEST fact here, so the frontier names the row
/// itself rather than the reply before it.
#[tokio::test]
async fn a_compaction_first_surfaced_by_the_terminal_read_is_not_re_rendered() {
    let _wd = test_work_dir();
    let timeline = vec![
        user("msg_cola_anchor", 1_000, "第一条消息"),
        assistant(2_000, "第一轮回答。"),
        compaction(
            "msg_comp",
            3_000,
            completed(CompactionReason::Auto, "更早的对话已被压缩。"),
        ),
    ];
    let (_dir, app, backend, platform) =
        scripted_app(vec![SessionTranscript::new(timeline)], Some(SessionStatus::Idle)).await;

    let first = spawn_turn(&app, ctx("ses_test", "第一条消息"));
    tokio::time::timeout(Duration::from_secs(5), first)
        .await
        .expect("the first Turn must finish")
        .unwrap()
        .unwrap();

    // The row's first card write IS the terminal one: no earlier write carried
    // it, and the terminal write is the settled card's own.
    let turn_cards = platform.updated_cards().await;
    let (terminal, earlier) = turn_cards
        .split_last()
        .expect("the first Turn must write its card");
    let rows = compaction_rows(terminal);
    assert_eq!(
        rows.len(),
        1,
        "the terminal card carries the row once: {:?}",
        body_lines(terminal)
    );
    assert_eq!(rows[0].0, "已压缩上下文（自动）");
    assert_eq!(rows[0].1, "更早的对话已被压缩。");
    assert!(
        earlier.iter().all(|card| compaction_rows(card).is_empty()),
        "the row first appears on the terminal write, not an earlier one: {:?}",
        earlier.iter().map(body_lines).collect::<Vec<_>>()
    );

    // The second Turn: the session's transcript grows by that turn's own
    // exchange, and a new card opens on a new chain.
    {
        let mut scripts = backend.transcript_scripts.lock().await;
        let transcript = &mut scripts.get_mut("ses_test").expect("the scripted read")[0];
        transcript
            .messages
            .push(user("msg_cola_second", 4_000, "第二条消息"));
        transcript.messages.push(assistant(5_000, "第二轮回答。"));
    }
    let updates_before = platform.updated_cards().await.len();
    let second = spawn_turn(&app, turn_ctx("msg_cola_second", "第二条消息"));
    tokio::time::timeout(Duration::from_secs(5), second)
        .await
        .expect("the second Turn must finish")
        .unwrap()
        .unwrap();

    let updates = platform.updated_cards().await;
    let second_turn_cards = &updates[updates_before..];
    assert!(
        !second_turn_cards.is_empty(),
        "the second Turn must render its own card"
    );
    let carrying: Vec<Vec<String>> = second_turn_cards
        .iter()
        .filter(|card| !compaction_rows(card).is_empty())
        .map(body_lines)
        .collect();
    assert!(
        carrying.is_empty(),
        "the terminal write that carried the row released the chain's record in \
         the same flush — the row is the session's delivered past now, exactly \
         once — so the second Turn's card must not carry it: {carrying:?}"
    );
    let second_card = second_turn_cards.last().unwrap();
    assert!(
        card_text(second_card).contains("第二轮回答。"),
        "the second Turn's card carries its own reply: {:?}",
        body_lines(second_card)
    );
    assert!(
        !card_text(second_card).contains("第一轮回答。"),
        "and not the previous Turn's: {:?}",
        body_lines(second_card)
    );
}

/// The other half of exactly-once (ADR-0082 §3/§4): a compaction NO confirmed
/// card has surfaced still renders — on the newest live card if there is one,
/// otherwise on the next Turn's card. The previous Turn's delivered position
/// bounds the walk, so a compaction that lands AFTER it is owed, not swallowed by
/// a bound that has outlived its chain.
#[tokio::test]
async fn a_compaction_no_card_has_surfaced_yet_renders_on_the_next_turns_card() {
    let _wd = test_work_dir();
    let first = vec![
        user("msg_cola_anchor", 1_000, "第一条消息"),
        assistant(2_000, "第一轮回答。"),
    ];
    let (_dir, app, backend, platform) =
        scripted_app(vec![SessionTranscript::new(first)], Some(SessionStatus::Idle)).await;

    // The first Turn has no compaction at all: its confirmed write leaves the
    // chain's record — and the Session's delivered position — at its own reply.
    let turn = spawn_turn(&app, ctx("ses_test", "第一条消息"));
    tokio::time::timeout(Duration::from_secs(5), turn)
        .await
        .expect("the first Turn must finish")
        .unwrap()
        .unwrap();
    let first_card = platform
        .updated_cards()
        .await
        .pop()
        .expect("the first Turn's final card flush");
    assert!(
        compaction_rows(&first_card).is_empty(),
        "nothing compacted yet: {:?}",
        body_lines(&first_card)
    );

    // The session is compacted while idle — the compaction lands BEFORE the
    // next user message, so no card exists to carry it and it waits (ADR-0082
    // §4: "it rides the newest live card if there is one, otherwise it waits for
    // the next Turn's card"). The next Turn then opens: the row has never been
    // on a card, so it must ride this one.
    {
        let mut scripts = backend.transcript_scripts.lock().await;
        let transcript = &mut scripts.get_mut("ses_test").expect("the scripted read")[0];
        transcript.messages.push(compaction(
            "msg_comp",
            2_500,
            completed(CompactionReason::Auto, "保留了结论与未决问题。"),
        ));
        transcript
            .messages
            .push(user("msg_cola_second", 3_000, "第二条消息"));
        transcript.messages.push(assistant(4_000, "第二轮回答。"));
    }
    let second = spawn_turn(&app, {
        let mut context = ctx("ses_test", "第二条消息");
        context.cola_message_id = Some("msg_cola_second".into());
        context
    });
    tokio::time::timeout(Duration::from_secs(5), second)
        .await
        .expect("the second Turn must finish")
        .unwrap()
        .unwrap();

    let card = platform
        .updated_cards()
        .await
        .pop()
        .expect("the second Turn's final card flush");
    let rows = compaction_rows(&card);
    assert_eq!(
        rows.len(),
        1,
        "an unsurfaced compaction renders exactly once, on this Turn's card: {:?}",
        body_lines(&card)
    );
    assert_eq!(rows[0].0, "已压缩上下文（自动）");
    assert_eq!(rows[0].1, "保留了结论与未决问题。");
    assert!(
        line_index(&card, "已压缩上下文（自动）") < line_index(&card, "第二轮回答。"),
        "at its own server time, above the reply: {:?}",
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

/// A V1 pair (ADR-0082, ticket #724) as the V1 decoder classifies it: the
/// carrier user message holds the fact, and the paired summary message is a
/// `compaction`-role tombstone beside it. Neither may touch the conversation —
/// the summary answers no Turn, and its tokens never become the Turn Footer's
/// context figure (V2 gets that for free, because its compaction usage is not
/// carried on an assistant message at all).
#[tokio::test]
async fn a_v1_summary_message_never_answers_the_turn_nor_feeds_the_footer() {
    let _wd = test_work_dir();
    let carrier = typed_message(
        "msg_comp_carrier",
        MessageRole::Other("compaction".into()),
        Some(2_000),
        vec![Part::Compaction(completed(
            CompactionReason::Auto,
            "保留了结论与未决问题。",
        ))],
    );
    let mut summary = typed_message(
        "msg_comp_summary",
        MessageRole::Other("compaction".into()),
        Some(2_100),
        Vec::new(),
    );
    summary.tokens = Some(TokenUsage {
        input: 300_000,
        output: 0,
        total: 300_000,
        cache_read: 0,
        cache_write: 0,
    });
    let mut reply = assistant(3_000, "第一轮回答。");
    reply.tokens = Some(TokenUsage {
        input: 42_000,
        output: 0,
        total: 42_000,
        cache_read: 0,
        cache_write: 0,
    });
    let timeline = vec![
        user("msg_cola_anchor", 1_000, "第一条消息"),
        carrier,
        summary,
        reply,
    ];
    let (_dir, app, _backend, platform) =
        scripted_app(vec![SessionTranscript::new(timeline)], Some(SessionStatus::Idle)).await;

    let card = finished_card(&app, &platform).await;
    let rows = compaction_rows(&card);
    assert_eq!(rows.len(), 1, "{:?}", body_lines(&card));
    assert_eq!(rows[0].0, "已压缩上下文（自动）");
    assert_eq!(rows[0].1, "保留了结论与未决问题。");
    let text = card_text(&card);
    assert_eq!(
        text.matches("保留了结论与未决问题。").count(),
        1,
        "the summary renders exactly once: {text}"
    );
    assert!(
        text.contains("📊 上下文 42k"),
        "the footer keeps the answering step's figure: {text}"
    );
    assert!(
        !text.contains("300k"),
        "a V1 summary message's own tokens never become the context figure: {text}"
    );
}
