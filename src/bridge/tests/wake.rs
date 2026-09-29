//! The Wake continuation (ADR-0059, spec #405 ticket 3): Session Sync renders
//! work the Backend resumed with no user message — a finished Background Task,
//! a restart, an interruption — as a Card Chain continuation. The new card is
//! replied to the user's message, carries the 承接 line and only the work the
//! chain had not rendered, and ends through the single settle decision: ✅ at
//! the true end, ❌ without Retry for a settled failure, ⏹ 已停止 for `/stop`,
//! or the waiting yield when the Wake backgrounded work of its own.
//!
//! Every test scripts the Backend's reads and injects tiny poll cadences, so no
//! test waits on a production interval.

use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use super::drain::{
    assistant, ctx, script_transcript, scripted_app, spawn_sync, spawn_turn, user, wait_for_card_header,
    wait_for_card_text,
};
use crate::backend::{
    BackgroundTask, ContentBlock, FinishReason, MessageRole, Part, ReasoningPart, SessionTranscript,
    StepFinish, TextPart, ToolCall, ToolIdentity, ToolOutput, ToolStatus, TranscriptMessage,
};
use crate::bridge::test_support::*;
use crate::bridge::turn::Turn;
use crate::feishu::card::CardState;
use crate::opencode::types::SessionStatus;

/// A finished assistant turn whose newest message recorded a failure.
fn failed_assistant(created: i64, text: &str, error: &str) -> TranscriptMessage {
    let mut message = assistant(created, text);
    message.error = Some(error.to_string());
    message
}

/// An assistant message whose only content is a `bash` call that never
/// settles: a live `⏳` panel on the card (the stuck-panel grace's fixture).
fn assistant_with_live_tool(created: i64) -> TranscriptMessage {
    typed_message(
        &format!("msg_a_{created}"),
        MessageRole::Assistant,
        Some(created),
        vec![tool_part(
            "bash",
            "call_live",
            ToolStatus::Running,
            serde_json::json!({ "command": "sleep 600" }),
            "",
        )],
    )
}

/// A live Background Task with its own correlation ids: the shared fixture
/// names one shell, so a scenario with a SECOND live task needs its own.
fn another_background_shell(started_at: i64) -> BackgroundTask {
    BackgroundTask {
        tool: ToolIdentity {
            name: "shell".into(),
            call_id: "call_bg2".into(),
        },
        shell_id: Some("sh_bg2".into()),
        child_id: None,
        started_at: Some(started_at),
    }
}

/// The resumed run's own work: reasoning, a settled tool and the closing text,
/// each carrying the SERVER start time it really has. This is what a Wake
/// continuation renders after the 承接 receipt — and the element kinds the
/// live order bug put first (a receipt keyed at cola's poll moment sorts after
/// work whose server times are already in the past).
fn resumed_work(created: i64) -> TranscriptMessage {
    typed_message(
        &format!("msg_a_{created}"),
        MessageRole::Assistant,
        Some(created),
        vec![
            Part::Reasoning(ReasoningPart {
                text: "正在验证 CI 结果。".into(),
                started_at: Some(created),
            }),
            Part::Tool(ToolCall {
                identity: ToolIdentity {
                    name: "bash".into(),
                    call_id: "call_resumed".into(),
                },
                status: ToolStatus::Completed,
                started_at: Some(created),
                input: Some(serde_json::json!({ "command": "gh run watch" })),
                metadata: None,
                output: ToolOutput {
                    raw: None,
                    blocks: vec![ContentBlock::Text("workflow run 123 成功".into())],
                    error: None,
                },
            }),
            Part::Text(TextPart {
                text: "CI 通过了。".into(),
                started_at: Some(created + 1),
            }),
            Part::StepFinish(StepFinish {
                reason: FinishReason::Stop,
            }),
        ],
    )
}

/// The index of the first card BODY element whose JSON contains `needle` —
/// nested panel content included, so one reasoning/tool panel counts as the
/// one element it renders as. The continuation's element ORDER is the live
/// bug: the 承接 receipt is the card's first visible block.
fn body_index(card: &serde_json::Value, needle: &str) -> Option<usize> {
    card["body"]["elements"]
        .as_array()?
        .iter()
        .position(|element| element.to_string().contains(needle))
}

/// The 承接 receipt line a Wake continuation opens with — the one user-facing
/// marker that tells a continuation card apart from a fresh turn's card.
const LEAD: &str = "已恢复执行";

/// The card a platform call carried, whatever kind of call it was (a sent,
/// replied or updated card) — the one match the helpers below share.
fn call_card(call: &PlatformCall) -> Option<&serde_json::Value> {
    match call {
        PlatformCall::ReplyCard { card, .. }
        | PlatformCall::SendCard { card, .. }
        | PlatformCall::UpdateMessage { card, .. } => Some(card),
        _ => None,
    }
}

/// Whether `card` is a Wake continuation card (it carries the 承接 line).
fn is_continuation(card: &serde_json::Value) -> bool {
    card_text(card).contains(LEAD)
}

/// Every card the platform saw carrying the continuation's 承接 line.
async fn continuation_cards(platform: &RecordingPlatform) -> Vec<serde_json::Value> {
    platform
        .calls
        .lock()
        .await
        .iter()
        .filter_map(call_card)
        .filter(|card| is_continuation(card))
        .cloned()
        .collect()
}

/// Await any card — sent, replied or updated — carrying `needle`, or panic
/// after 5 s. The external-message path sends its notification, then renders
/// into it in place, so a test must accept either call.
async fn wait_for_any_card(platform: &RecordingPlatform, needle: &str) {
    let probe = async {
        loop {
            let found = {
                let calls = platform.calls.lock().await;
                calls
                    .iter()
                    .filter_map(call_card)
                    .any(|card| card_text(card).contains(needle))
            };
            if found {
                return;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    };
    tokio::time::timeout(Duration::from_secs(5), probe)
        .await
        .unwrap_or_else(|_| panic!("no card ever carried {needle:?}"));
}

/// The #403 shape (acceptance 1): a Turn idles with a live Background Task and
/// yields 「⏳ 等待后台任务」; the Wake's later content becomes a continuation
/// card replying to the user's message, streams the new work and ends ✅ under
/// the settle rule — never replaying the previous card's content (acceptance
/// 2), with the Turn Footer current.
#[tokio::test]
async fn a_wake_after_a_waiting_yield_continues_on_a_new_card() {
    let _wd = test_work_dir();
    let waiting = SessionTranscript::new(vec![
        user("msg_cola_anchor", 1_000, "跑一下 CI"),
        assistant(2_000, "已经交给后台了。"),
    ])
    .with_executions(vec![execution(2_500)])
    .with_background_tasks(vec![background_shell(2_100)]);
    let (_dir, app, backend, platform) = scripted_app(vec![waiting], Some(SessionStatus::Idle)).await;

    Turn::run(&app.turn_handles(), ctx("ses_test", "跑一下 CI"))
        .await
        .unwrap();
    assert_eq!(
        Turn::card_state(&app.cards_handle(), "ses_test").await,
        Some(CardState::Waiting)
    );

    // The Wake resumes the Turn with real work: reasoning, a settled tool and
    // the answer, all carrying server times from the wake moment.
    script_transcript(
        &backend,
        vec![
            SessionTranscript::new(vec![
                user("msg_cola_anchor", 1_000, "跑一下 CI"),
                assistant(2_000, "已经交给后台了。"),
                resumed_work(3_100),
            ])
            .with_executions(vec![execution(2_500), execution(4_000)])
            .with_wakes(vec![shell_wake(2_900)]),
        ],
    )
    .await;

    spawn_sync(&app);
    wait_for_card_text(&platform, "CI 通过了。").await;
    wait_for_card_update(
        &platform,
        "the continuation's done card",
        CardUpdates::Latest,
        |card| card_header(card).contains("✅"),
    )
    .await;

    let last = platform.updated_cards().await.last().cloned().unwrap();
    assert!(
        card_text(&last).contains("CI 通过了。"),
        "the resumed work lands on the continuation: {last}"
    );
    assert!(
        !card_text(&last).contains("已经交给后台了。"),
        "the continuation must not replay the previous card's content: {last}"
    );
    assert!(
        platform
            .updated_cards()
            .await
            .iter()
            .any(|card| card_text(card).contains("CI 通过了。") && !card_header(card).contains("✅")),
        "the continuation streams the resumed work before it ends: {:?}",
        platform.updated_cards().await
    );
    // The 承接 receipt is the continuation's FIRST visible block, ahead of
    // every part of the resumed run: the work carries server times from the
    // wake (already in the past at poll time), so a receipt keyed at cola's
    // "now" would sort after it — the live order bug.
    let receipt = body_index(&last, LEAD).expect("the 承接 line rides the continuation");
    assert_eq!(
        receipt, 0,
        "the 承接 line must open the continuation card: {last}"
    );
    for work in ["正在验证 CI 结果。", "workflow run 123 成功", "CI 通过了。"] {
        let at = body_index(&last, work)
            .unwrap_or_else(|| panic!("the resumed work must render ({work}): {last}"));
        assert!(
            receipt < at,
            "the 承接 receipt must precede the resumed work {work:?} (receipt@{receipt}, work@{at}): {last}"
        );
    }
    assert!(
        card_text(&last).contains("📁"),
        "the continuation keeps the Turn Footer: {last}"
    );

    // The continuation is a reply to the user's message (the Turn's own reply
    // target) and is itself the notification.
    let replied = platform.replied_cards().await;
    assert!(
        replied.iter().any(is_continuation),
        "the continuation card must be replied to the user's message: {replied:?}"
    );
    assert!(
        platform.completion_notices().await.is_empty(),
        "the continuation card is the notification; no notice is sent: {:?}",
        platform.calls.lock().await
    );
    // The waiting card hands over with the standard split header — its wait is
    // over and the chain moved on.
    assert!(
        platform
            .updated_cards()
            .await
            .iter()
            .any(|card| card_header(card).contains("继续中")),
        "the waiting card takes the handoff header: {:?}",
        platform.updated_cards().await
    );
}

/// Acceptance 3, first half: a Wake arriving after the Turn's card already
/// ended ✅ still posts a continuation card — the ✅ card keeps its ending.
#[tokio::test]
async fn a_wake_after_a_done_card_continues_on_a_new_card() {
    let _wd = test_work_dir();
    let done = SessionTranscript::new(vec![
        user("msg_cola_anchor", 1_000, "第一条消息"),
        assistant(2_000, "第一轮回答。"),
    ])
    .with_executions(vec![execution(2_500)]);
    let (_dir, app, backend, platform) = scripted_app(vec![done], Some(SessionStatus::Idle)).await;

    Turn::run(&app.turn_handles(), ctx("ses_test", "第一条消息"))
        .await
        .unwrap();
    assert_eq!(
        Turn::card_state(&app.cards_handle(), "ses_test").await,
        Some(CardState::Done)
    );

    // A later Wake resumes the Session with new work.
    script_transcript(
        &backend,
        vec![
            SessionTranscript::new(vec![
                user("msg_cola_anchor", 1_000, "第一条消息"),
                assistant(2_000, "第一轮回答。"),
                assistant(3_100, "合并完成。"),
            ])
            .with_executions(vec![execution(2_500), execution(4_000)])
            .with_wakes(vec![shell_wake(2_900)]),
        ],
    )
    .await;

    spawn_sync(&app);
    wait_for_card_update(
        &platform,
        "the continuation's done card",
        CardUpdates::Latest,
        |card| card_header(card).contains("✅") && card_text(card).contains("合并完成。"),
    )
    .await;

    let last = platform.updated_cards().await.last().cloned().unwrap();
    assert!(card_header(&last).contains("✅"), "{last}");
    assert!(card_text(&last).contains("合并完成。"), "{last}");
    assert!(
        !card_text(&last).contains("第一轮回答。"),
        "the continuation renders only the new work: {last}"
    );
    assert!(
        !platform
            .updated_cards()
            .await
            .iter()
            .any(|card| card_header(card).contains("继续中")),
        "a ✅ card keeps its ending; only a waiting card is restamped: {:?}",
        platform.updated_cards().await
    );
    assert!(
        platform.replied_cards().await.iter().any(is_continuation),
        "the continuation replies to the user's message"
    );
}

/// Acceptance 3, second half: after a simulated cola restart (no card chain in
/// memory at all) a Wake still posts a continuation card — the path is scoped
/// by the Wake's own server time, so the lost card's content is never replayed.
#[tokio::test]
async fn a_wake_after_a_restart_posts_a_continuation_card() {
    let _wd = test_work_dir();
    let resumed = SessionTranscript::new(vec![
        user("msg_cola_anchor", 1_000, "跑一下 CI"),
        assistant(2_000, "已经交给后台了。"),
        assistant(3_100, "CI 通过了。"),
    ])
    .with_executions(vec![execution(2_500), execution(4_000)])
    .with_wakes(vec![shell_wake(2_900)]);
    let (_dir, app, _backend, platform) = scripted_app(vec![resumed], Some(SessionStatus::Idle)).await;
    assert!(
        !Turn::has_card(&app.cards_handle(), "ses_test").await,
        "the restart fixture has no card chain"
    );

    spawn_sync(&app);
    wait_for_card_update(
        &platform,
        "the restart continuation's done card",
        CardUpdates::Latest,
        |card| card_header(card).contains("✅"),
    )
    .await;

    let last = platform.updated_cards().await.last().cloned().unwrap();
    // The fresh arm keys the 承接 line before the Wake's own work, so it opens
    // the card here too — not only in the split path.
    assert_eq!(
        body_index(&last, LEAD),
        Some(0),
        "the 承接 line opens the restart continuation: {last}"
    );
    assert!(card_text(&last).contains("CI 通过了。"), "{last}");
    assert!(
        !card_text(&last).contains("已经交给后台了。"),
        "the lost card's content is never replayed: {last}"
    );
    // No durable Feishu reply target survives a restart for a lobby session:
    // the continuation is sent top-level into the chat.
    assert!(
        platform.sent_cards().await.iter().any(is_continuation),
        "the restart continuation reaches the chat: {:?}",
        platform.calls.lock().await
    );
}

/// Acceptance 4a: `/stop` during a resumed run finalizes ⏹ 已停止 — never ✅ or
/// ❌ — and sends no Completion Notice.
#[tokio::test]
async fn a_stop_during_a_resumed_run_finalizes_stopped() {
    let _wd = test_work_dir();
    let waiting = SessionTranscript::new(vec![
        user("msg_cola_anchor", 1_000, "跑一下 CI"),
        assistant(2_000, "已经交给后台了。"),
    ])
    .with_executions(vec![execution(2_500)])
    .with_background_tasks(vec![background_shell(2_100)]);
    let (_dir, app, backend, platform) = scripted_app(vec![waiting], Some(SessionStatus::Idle)).await;
    Turn::run(&app.turn_handles(), ctx("ses_test", "跑一下 CI"))
        .await
        .unwrap();

    // The Wake's Execution has no boundary yet: the resumed run is still going,
    // so the continuation keeps observing.
    script_transcript(
        &backend,
        vec![
            SessionTranscript::new(vec![
                user("msg_cola_anchor", 1_000, "跑一下 CI"),
                assistant(2_000, "已经交给后台了。"),
                assistant(3_100, "正在合并。"),
            ])
            .with_executions(vec![execution(2_500)])
            .with_wakes(vec![shell_wake(2_900)]),
        ],
    )
    .await;
    spawn_sync(&app);
    wait_for_card_text(&platform, "正在合并。").await;
    assert_eq!(
        Turn::card_state(&app.cards_handle(), "ses_test").await,
        Some(CardState::Streaming),
        "the resumed run is live on the continuation"
    );

    send_command(&app, "/stop", "msg_stop").await;
    wait_for_card_header(&platform, "已停止").await;

    let last = platform.updated_cards().await.last().cloned().unwrap();
    assert!(
        !card_header(&last).contains("✅") && !card_header(&last).contains("出错"),
        "a deliberate stop is its own terminal: {last}"
    );
    assert!(
        platform.completion_notices().await.is_empty(),
        "the continuation card is the notification; no notice is sent"
    );
}

/// Acceptance 4b: a resumed run that fails finalizes ❌ with the failure and
/// NO Retry button — the continuation carries no question to re-ask.
#[tokio::test]
async fn a_failed_resumed_run_finalizes_error_without_retry() {
    let _wd = test_work_dir();
    let waiting = SessionTranscript::new(vec![
        user("msg_cola_anchor", 1_000, "跑一下 CI"),
        assistant(2_000, "已经交给后台了。"),
    ])
    .with_executions(vec![execution(2_500)])
    .with_background_tasks(vec![background_shell(2_100)]);
    let (_dir, app, backend, platform) = scripted_app(vec![waiting], Some(SessionStatus::Idle)).await;
    Turn::run(&app.turn_handles(), ctx("ses_test", "跑一下 CI"))
        .await
        .unwrap();

    script_transcript(
        &backend,
        vec![
            SessionTranscript::new(vec![
                user("msg_cola_anchor", 1_000, "跑一下 CI"),
                assistant(2_000, "已经交给后台了。"),
                failed_assistant(3_100, "失败的一步", "provider 503"),
            ])
            .with_executions(vec![execution(2_500), execution(4_000)])
            .with_wakes(vec![shell_wake(2_900)]),
        ],
    )
    .await;

    spawn_sync(&app);
    wait_for_card_update(
        &platform,
        "the continuation's error card",
        CardUpdates::Latest,
        |card| card_header(card).contains("出错"),
    )
    .await;

    let last = platform.updated_cards().await.last().cloned().unwrap();
    assert!(
        card_text(&last).contains("provider 503"),
        "the settled failure reaches the continuation: {last}"
    );
    assert!(
        !card_buttons(&last)
            .iter()
            .any(|button| button["value"]["action"] == "retry"),
        "a Wake continuation offers no Retry: {last}"
    );
}

/// The content-diff fallback: content the finalized card missed — here with no
/// Wake recorded at all — still continues the chain, rendering only the missed
/// part.
#[tokio::test]
async fn content_a_finalized_card_missed_still_continues() {
    let _wd = test_work_dir();
    let done = SessionTranscript::new(vec![
        user("msg_cola_anchor", 1_000, "第一条消息"),
        assistant(2_000, "第一轮回答。"),
    ])
    .with_executions(vec![execution(2_500)]);
    let (_dir, app, backend, platform) = scripted_app(vec![done], Some(SessionStatus::Idle)).await;
    Turn::run(&app.turn_handles(), ctx("ses_test", "第一条消息"))
        .await
        .unwrap();

    script_transcript(
        &backend,
        vec![
            SessionTranscript::new(vec![
                user("msg_cola_anchor", 1_000, "第一条消息"),
                assistant(2_000, "第一轮回答。"),
                assistant(3_100, "收尾时补上的一段。"),
            ])
            .with_executions(vec![execution(2_500), execution(4_000)]),
        ],
    )
    .await;

    spawn_sync(&app);
    wait_for_card_update(
        &platform,
        "the content-diff continuation",
        CardUpdates::Latest,
        |card| card_header(card).contains("✅") && card_text(card).contains("收尾时补上的一段。"),
    )
    .await;

    let last = platform.updated_cards().await.last().cloned().unwrap();
    assert!(
        !card_text(&last).contains("第一轮回答。"),
        "only the missed content renders: {last}"
    );
    assert_eq!(
        body_index(&last, LEAD),
        Some(0),
        "the 承接 line opens the content-diff continuation too: {last}"
    );
    assert!(
        platform.replied_cards().await.iter().any(is_continuation),
        "the content-diff continuation replies to the user's message"
    );
}

/// Acceptance 5a: a Wake on a Session whose newest user message is EXTERNAL
/// stays unrendered — the external-message path is unchanged and the Wake
/// never moves the Sync Watermark (which accounts user messages only).
#[tokio::test]
async fn a_wake_when_the_newest_user_message_is_external_stays_unrendered() {
    let _wd = test_work_dir();
    // cola's own turn with a Wake that resumed it — and no external message
    // yet, so the Wake first renders (and must leave the watermark alone).
    let wake_only = SessionTranscript::new(vec![
        user("msg_cola_anchor", 1_000, "跑一下 CI"),
        assistant(2_000, "已经交给后台了。"),
        assistant(3_100, "CI 通过了。"),
    ])
    .with_executions(vec![execution(2_500), execution(4_000)])
    .with_wakes(vec![shell_wake(2_900)]);
    let (_dir, app, backend, platform) = scripted_app(vec![wake_only], Some(SessionStatus::Idle)).await;
    app.external
        .last_user_msg_epoch
        .lock()
        .await
        .insert("ses_test".into(), 1_000);

    spawn_sync(&app);
    wait_for_any_card(&platform, LEAD).await;
    // The Wake never moved the Sync Watermark: it still accounts user messages
    // only (the cola-authored message's own time).
    assert_eq!(
        app.external
            .last_user_msg_epoch
            .lock()
            .await
            .get("ses_test")
            .copied(),
        Some(1_000),
        "a Wake never moves the watermark"
    );

    // A NEWER external message then arrives: it notifies normally (the Wake
    // neither suppressed nor replayed anything), and — being the newest user
    // message — suppresses any further Wake rendering.
    script_transcript(
        &backend,
        vec![
            SessionTranscript::new(vec![
                user("msg_cola_anchor", 1_000, "跑一下 CI"),
                assistant(2_000, "已经交给后台了。"),
                assistant(3_100, "CI 通过了。"),
                user("msg_ext_user", 5_000, "OpenChamber 的消息"),
            ])
            .with_executions(vec![execution(2_500), execution(4_000)])
            .with_wakes(vec![shell_wake(2_900)]),
        ],
    )
    .await;
    wait_for_any_card(&platform, "OpenChamber 的消息").await;

    let continuations = continuation_cards(&platform).await;
    let before = continuations.len();
    assert!(before >= 1, "the wake-only pass rendered its continuation");
    // Many more passes with the external message newest: no Wake rendering.
    tokio::time::sleep(Duration::from_millis(150)).await;
    assert_eq!(
        continuation_cards(&platform).await.len(),
        before,
        "a Wake must not render when the newest user message is external: {:?}",
        platform.calls.lock().await
    );
}

/// Acceptance 5b: a Wake on a non-active Session is never rendered — ADR-0017's
/// no-interleaving scope is unchanged.
#[tokio::test]
async fn a_wake_on_a_non_active_session_is_not_rendered() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let mut mock = MockBackend::new(realistic_parts());
    // The Wake lives on the historical session; the active session has no user
    // message at all.
    mock.given_transcript(
        "ses_hist",
        vec![
            SessionTranscript::new(vec![
                user("msg_cola_anchor", 1_000, "跑一下 CI"),
                assistant(2_000, "已经交给后台了。"),
                assistant(3_100, "CI 通过了。"),
            ])
            .with_executions(vec![execution(2_500), execution(4_000)])
            .with_wakes(vec![shell_wake(2_900)]),
        ],
    );
    let (app, platform) = build_app(cfg, mock).await;
    let key = crate::config::ThreadKey::new("chat_1".into(), "chat_1".into());
    // set_active pushes to the front, so the LAST seeded entry is the active
    // one.
    seed_entry(
        &app,
        crate::config::SessionEntry {
            thread_key: key.clone(),
            session_id: "ses_hist".into(),
            directory: "/tmp/hist".into(),
            agent: None,
            model: None,
            auto_accept: false,
            topic_anchor: None,
            topic_root: None,
            variant: None,
        },
    )
    .await;
    seed_entry(
        &app,
        crate::config::SessionEntry {
            thread_key: key.clone(),
            session_id: "ses_active".into(),
            directory: "/tmp/active".into(),
            agent: None,
            model: None,
            auto_accept: false,
            topic_anchor: None,
            topic_root: None,
            variant: None,
        },
    )
    .await;

    spawn_sync(&app);
    tokio::time::sleep(Duration::from_millis(200)).await;

    assert!(
        continuation_cards(&platform).await.is_empty(),
        "a Wake on a non-active session must stay unrendered: {:?}",
        platform.calls.lock().await
    );
    assert_eq!(
        Turn::chain_id(&app.cards_handle(), "ses_hist").await,
        None,
        "no card chain may be armed for the historical session"
    );
}

/// A Wake never double-renders into a card a live Turn/renderer owns.
#[tokio::test]
async fn a_live_card_is_never_split_by_a_wake() {
    let _wd = test_work_dir();
    let resumed = SessionTranscript::new(vec![
        user("msg_cola_anchor", 1_000, "跑一下 CI"),
        assistant(2_000, "已经交给后台了。"),
        assistant(3_100, "CI 通过了。"),
    ])
    .with_executions(vec![execution(2_500), execution(4_000)])
    .with_wakes(vec![shell_wake(2_900)]);
    let (_dir, app, _backend, platform) = scripted_app(vec![resumed], Some(SessionStatus::Idle)).await;
    // A live card owns the session (as a running Turn or renderer would).
    Turn::seed_card(&app.cards_handle(), "ses_test", Some("om_live")).await;
    Turn::set_card_state(&app.cards_handle(), "ses_test", CardState::Streaming).await;
    Turn::set_reply_target(&app.cards_handle(), "ses_test", "msg_1").await;
    let chain = Turn::chain_id(&app.cards_handle(), "ses_test").await;

    spawn_sync(&app);
    tokio::time::sleep(Duration::from_millis(150)).await;

    assert!(
        continuation_cards(&platform).await.is_empty(),
        "a live card must not be split by a Wake: {:?}",
        platform.calls.lock().await
    );
    assert_eq!(
        Turn::chain_id(&app.cards_handle(), "ses_test").await,
        chain,
        "the live chain must be left untouched"
    );
}

/// The Wake step is card-independent: it holds no accumulator across passes, so
/// a second Session Sync pass over the same read is a no-op — one Wake, one
/// continuation card, no replay on every poll.
#[tokio::test]
async fn a_rendered_wake_is_never_re_posted() {
    let _wd = test_work_dir();
    let resumed = SessionTranscript::new(vec![
        user("msg_cola_anchor", 1_000, "跑一下 CI"),
        assistant(2_000, "已经交给后台了。"),
        assistant(3_100, "CI 通过了。"),
    ])
    .with_executions(vec![execution(2_500), execution(4_000)])
    .with_wakes(vec![shell_wake(2_900)]);
    let (_dir, app, _backend, platform) = scripted_app(vec![resumed], Some(SessionStatus::Idle)).await;

    spawn_sync(&app);
    wait_for_card_update(
        &platform,
        "the continuation's done card",
        CardUpdates::Latest,
        |card| card_header(card).contains("✅"),
    )
    .await;
    let posted = continuation_cards(&platform).await.len();
    assert!(posted >= 1, "the Wake must post its continuation first");

    // Many more sync passes over the same read: the card chain already holds
    // the Wake's work, so nothing new is owed.
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(
        continuation_cards(&platform).await.len(),
        posted,
        "a rendered Wake must not be re-posted on every poll: {:?}",
        platform.calls.lock().await
    );
}

/// A chained Wake: after the first continuation ended ✅, a second Wake
/// resumes the Turn again — a new continuation card below, again replying to
/// the user's message and carrying only the second run's work.
#[tokio::test]
async fn a_second_wake_continues_the_chain_again() {
    let _wd = test_work_dir();
    let waiting = SessionTranscript::new(vec![
        user("msg_cola_anchor", 1_000, "跑一下 CI"),
        assistant(2_000, "已经交给后台了。"),
    ])
    .with_executions(vec![execution(2_500)])
    .with_background_tasks(vec![background_shell(2_100)]);
    let (_dir, app, backend, platform) = scripted_app(vec![waiting], Some(SessionStatus::Idle)).await;
    Turn::run(&app.turn_handles(), ctx("ses_test", "跑一下 CI"))
        .await
        .unwrap();

    script_transcript(
        &backend,
        vec![
            SessionTranscript::new(vec![
                user("msg_cola_anchor", 1_000, "跑一下 CI"),
                assistant(2_000, "已经交给后台了。"),
                assistant(3_100, "CI 通过了。"),
            ])
            .with_executions(vec![execution(2_500), execution(4_000)])
            .with_wakes(vec![shell_wake(2_900)]),
        ],
    )
    .await;
    spawn_sync(&app);
    wait_for_card_update(
        &platform,
        "the first continuation's done card",
        CardUpdates::Latest,
        |card| card_header(card).contains("✅") && card_text(card).contains("CI 通过了。"),
    )
    .await;

    // A second Background Task's Wake: new work, a new Execution boundary.
    script_transcript(
        &backend,
        vec![
            SessionTranscript::new(vec![
                user("msg_cola_anchor", 1_000, "跑一下 CI"),
                assistant(2_000, "已经交给后台了。"),
                assistant(3_100, "CI 通过了。"),
                assistant(5_100, "合并完成。"),
            ])
            .with_executions(vec![execution(2_500), execution(4_000), execution(6_000)])
            .with_wakes(vec![shell_wake(2_900), shell_wake(4_900)]),
        ],
    )
    .await;
    wait_for_card_update(
        &platform,
        "the second continuation's done card",
        CardUpdates::Latest,
        |card| card_header(card).contains("✅") && card_text(card).contains("合并完成。"),
    )
    .await;

    let last = platform.updated_cards().await.last().cloned().unwrap();
    assert!(
        !card_text(&last).contains("CI 通过了。"),
        "the second continuation renders only its own run: {last}"
    );
    let continuations = continuation_cards(&platform).await;
    assert!(
        continuations
            .iter()
            .filter(|card| card_header(card).contains("✅"))
            .count()
            >= 2,
        "both Wakes end on their own continuation card: {continuations:?}"
    );
    assert!(
        platform
            .replied_cards()
            .await
            .iter()
            .filter(|card| is_continuation(card))
            .count()
            >= 2,
        "each continuation replies to the user's message: {:?}",
        platform.calls.lock().await
    );
}

/// A `/stop` from BEFORE the Wake is not the resumed run's ending — the marker
/// is cleared when the continuation arms (the same rule a fresh Turn applies,
/// ADR-0043), so "a later Wake continues on a new card" holds after a stop. A
/// stop landing after the arm still finalizes ⏹ (the test above).
#[tokio::test]
async fn a_wake_clears_a_stale_stop_marker() {
    let _wd = test_work_dir();
    let waiting = SessionTranscript::new(vec![
        user("msg_cola_anchor", 1_000, "跑一下 CI"),
        assistant(2_000, "已经交给后台了。"),
    ])
    .with_executions(vec![execution(2_500)])
    .with_background_tasks(vec![background_shell(2_100)]);
    let (_dir, app, backend, platform) = scripted_app(vec![waiting], Some(SessionStatus::Idle)).await;
    Turn::run(&app.turn_handles(), ctx("ses_test", "跑一下 CI"))
        .await
        .unwrap();
    // The operator stopped the (already idle) session before the Wake arrived:
    // the marker is sticky until a Turn — or this Wake — starts new work.
    app.stopped_sessions.lock().await.insert("ses_test".to_string());

    script_transcript(
        &backend,
        vec![
            SessionTranscript::new(vec![
                user("msg_cola_anchor", 1_000, "跑一下 CI"),
                assistant(2_000, "已经交给后台了。"),
                assistant(3_100, "CI 通过了。"),
            ])
            .with_executions(vec![execution(2_500), execution(4_000)])
            .with_wakes(vec![shell_wake(2_900)]),
        ],
    )
    .await;

    spawn_sync(&app);
    wait_for_card_update(
        &platform,
        "the continuation's done card",
        CardUpdates::Latest,
        |card| card_header(card).contains("✅"),
    )
    .await;

    let last = platform.updated_cards().await.last().cloned().unwrap();
    assert!(
        !card_header(&last).contains("已停止"),
        "a stop from before the Wake must not abort the resumed run: {last}"
    );
    assert!(card_text(&last).contains("CI 通过了。"), "{last}");
}

/// The panel-grace rule the shared loop adopted from the follow (#386): a Wake
/// continuation whose settle would be `Waiting` (a live Background Task) but
/// whose card still carries a live `⏳` panel must NOT settle — the panel gets
/// the grace, and the card then ends Error with the stuck-panel copy. The old
/// Wake loop stamped the waiting yield immediately, hiding the orphaned tool.
#[tokio::test]
async fn a_live_panel_outranks_the_wake_settle() {
    let _wd = test_work_dir();
    let waiting = SessionTranscript::new(vec![
        user("msg_cola_anchor", 1_000, "跑一下 CI"),
        assistant(2_000, "已经交给后台了。"),
    ])
    .with_executions(vec![execution(2_500)])
    .with_background_tasks(vec![background_shell(2_100)]);
    let (_dir, app, backend, platform) = scripted_app(vec![waiting], Some(SessionStatus::Idle)).await;
    Turn::run(&app.turn_handles(), ctx("ses_test", "跑一下 CI"))
        .await
        .unwrap();

    // The resumed run starts a foreground tool that never settles, retires the
    // first task and backgrounds a second one: the settle says Waiting, the
    // card's panel says live.
    script_transcript(
        &backend,
        vec![
            SessionTranscript::new(vec![
                user("msg_cola_anchor", 1_000, "跑一下 CI"),
                assistant(2_000, "已经交给后台了。"),
                assistant_with_live_tool(3_100),
            ])
            .with_executions(vec![execution(2_500), execution(4_000)])
            .with_wakes(vec![shell_wake(2_900)])
            .with_background_tasks(vec![another_background_shell(3_200)]),
        ],
    )
    .await;
    // The panel grace, injected tiny (the follow's own rule).
    app.external.render_timeout_ms.store(40, Ordering::Relaxed);

    spawn_sync(&app);
    wait_for_card_header(&platform, "出错").await;

    let last = platform.updated_cards().await.last().cloned().unwrap();
    assert!(
        card_text(&last).contains("未收尾"),
        "the live panel takes the grace and ends with its copy: {last}"
    );
    assert!(
        !card_header(&last).contains("等待后台任务"),
        "a live panel must not take the waiting yield: {last}"
    );
    assert!(
        card_text(&last).contains("⏳ bash"),
        "the orphaned panel stays visible under the error: {last}"
    );
}

/// The status-read rule the shared loop adopted from the follow: a Wake
/// continuation whose status read fails never claims an ending from the
/// transcript — it reads settleable here — and the lost-contact grace owns the
/// state instead. The old Wake loop settled from the last known transcript.
#[tokio::test]
async fn an_unreadable_status_never_settles_a_wake_before_the_grace() {
    let _wd = test_work_dir();
    let waiting = SessionTranscript::new(vec![
        user("msg_cola_anchor", 1_000, "跑一下 CI"),
        assistant(2_000, "已经交给后台了。"),
    ])
    .with_executions(vec![execution(2_500)])
    .with_background_tasks(vec![background_shell(2_100)]);
    let (_dir, app, backend, platform) = scripted_app(vec![waiting], Some(SessionStatus::Idle)).await;
    Turn::run(&app.turn_handles(), ctx("ses_test", "跑一下 CI"))
        .await
        .unwrap();

    // The resumed run reads settleable (its boundary landed, nothing live) ...
    script_transcript(
        &backend,
        vec![
            SessionTranscript::new(vec![
                user("msg_cola_anchor", 1_000, "跑一下 CI"),
                assistant(2_000, "已经交给后台了。"),
                assistant(3_100, "CI 通过了。"),
            ])
            .with_executions(vec![execution(2_500), execution(4_000)])
            .with_wakes(vec![shell_wake(2_900)]),
        ],
    )
    .await;
    // ... but the status read never answers, so no ending may be claimed.
    backend.session_status_fails.store(true, Ordering::SeqCst);
    app.external.render_timeout_ms.store(400, Ordering::Relaxed);

    spawn_sync(&app);
    // The resumed work still streams (the transcript is readable) ...
    wait_for_card_text(&platform, "CI 通过了。").await;
    // ... and nothing settles before the grace: the card stays live.
    tokio::time::sleep(Duration::from_millis(100)).await;
    let live = platform.updated_cards().await.last().cloned().unwrap();
    assert!(
        !card_header(&live).contains("✅"),
        "no ending may be claimed from a broken status read: {live}"
    );
    assert!(!card_header(&live).contains("出错"), "{live}");

    // The grace then ends it Error with the lost-contact copy, not Done from
    // the transcript.
    wait_for_card_header(&platform, "出错").await;
    let last = platform.updated_cards().await.last().cloned().unwrap();
    assert!(
        card_text(&last).contains("失去联系"),
        "the lost-contact copy: {last}"
    );
}

/// ADR-0059's routing key, waiting-window half: the waiting yield left the
/// Session idle and handed the guard back, so a message arriving now starts a
/// NEW Turn on a new card — never a Supplement merged into the ended chain.
/// The held prompt keeps the new Turn observable at its card.
#[tokio::test]
async fn a_message_in_the_waiting_window_starts_a_new_turn() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let waiting = SessionTranscript::new(vec![
        user("msg_cola_anchor", 1_000, "跑一下 CI"),
        assistant(2_000, "已经交给后台了。"),
    ])
    .with_executions(vec![execution(2_500)])
    .with_background_tasks(vec![background_shell(2_100)]);
    let mut backend = MockBackend::new(realistic_parts());
    backend.given_transcript("ses_test", vec![waiting]);
    backend.with_session_status("ses_test", Some(SessionStatus::Idle));
    // Hold every prompt: the single permit lets the first Turn reach its
    // waiting yield, and the second message's held prompt keeps the new
    // Turn's in-flight card observable.
    let gate = backend.hold_prompts();
    let backend = Arc::new(backend);
    let platform = Arc::new(RecordingPlatform::new());
    let app = Arc::new(App::new(cfg, backend.clone(), platform.clone()).unwrap());
    seed_session(&app, "ses_test", "/work").await;
    app.turn_render_poll_ms.store(5, Ordering::Relaxed);
    app.turn_drain_timeout_ms.store(5_000, Ordering::Relaxed);

    // The first Turn: idle + a live Background Task yields 「等待后台任务」 and
    // hands the guard back.
    let first = spawn_turn(&app, ctx("ses_test", "跑一下 CI"));
    gate.add_permits(1);
    let result = tokio::time::timeout(Duration::from_secs(5), first)
        .await
        .expect("the first turn must reach the waiting yield")
        .unwrap();
    result.unwrap();
    wait_for_card_header(&platform, "等待后台任务").await;
    assert_eq!(
        Turn::card_state(&app.cards_handle(), "ses_test").await,
        Some(CardState::Waiting)
    );
    assert!(
        !app.inflight.lock().await.contains("ses_test"),
        "the waiting yield must release the guard"
    );
    let waiting_anchor = Turn::armed_turn_anchor(&app.cards_handle(), "ses_test").await;
    assert!(waiting_anchor.is_some(), "the waiting card keeps its accumulator");

    // The next message arrives in the waiting window: idle, so a new Turn.
    // Hold its prompt: the first prompt's permit was returned when it
    // completed, so take it here — the new Turn's submit stays in flight
    // (holding the guard) while the assertions below observe it.
    let _held = Arc::clone(&gate)
        .acquire_owned()
        .await
        .expect("the prompt gate is open");
    let second = {
        let app = Arc::clone(&app);
        tokio::spawn(async move {
            app.handle_message(incoming(
                "msg_next".into(),
                "chat_1".into(),
                "p2p".into(),
                None,
                "新问题".into(),
                None,
            ))
            .await;
        })
    };
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let replied = {
                let calls = platform.calls.lock().await;
                calls
                    .iter()
                    .any(|c| matches!(c, PlatformCall::ReplyCard { reply_to, .. } if reply_to == "msg_next"))
            };
            if replied {
                return;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("the new Turn must reply its own card to the message");

    // The submit is recorded before the prompt gate, so wait for it instead of
    // racing the Turn's work-context read that sits between the card and the
    // prompt (the held permit keeps the Turn in flight from here on).
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if backend.prompt_calls.lock().await.iter().any(|t| t == "新问题") {
                return;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("the new Turn's submit must be recorded");
    assert!(
        app.inflight.lock().await.contains("ses_test"),
        "the new Turn holds the guard while its prompt is in flight"
    );
    assert_ne!(
        Turn::armed_turn_anchor(&app.cards_handle(), "ses_test").await,
        waiting_anchor,
        "the new Turn replaces the waiting accumulator (a new card)"
    );
    assert!(
        !platform.texts().await.iter().any(|t| t.contains("还在处理中")),
        "the waiting session is not answered busy: {:?}",
        platform.calls.lock().await
    );
    assert!(
        platform
            .calls
            .lock()
            .await
            .iter()
            .filter_map(call_card)
            .all(|card| !card_text(card).contains("📨 已收到补充")),
        "an idle waiting session takes a normal Turn, never a Supplement split: {:?}",
        platform.calls.lock().await
    );

    // Release the held prompt so the new Turn can finish.
    gate.add_permits(1);
    tokio::time::timeout(Duration::from_secs(5), second)
        .await
        .expect("the new Turn must finish")
        .unwrap();
    assert!(!app.inflight.lock().await.contains("ses_test"));
}
