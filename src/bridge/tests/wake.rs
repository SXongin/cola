//! The Wake continuation (ADR-0059, spec #405 ticket 3): Session Sync renders
//! work the Backend resumed with no user message — a finished Background Task,
//! a restart, an interruption — as a Card Chain continuation. The new card is
//! replied to the user's message, carries the 承接 line and only the work the
//! chain had not rendered, and ends through the single settle decision: ✅ at
//! the true end, ❌ without Retry for a settled failure, ⏹ 已停止 for `/stop`,
//! or the waiting yield when the Wake backgrounded work of its own.
//!
//! A shell/subagent completion that lands in an already-live card leaves the
//! Background Task Ledger's fixed completion entry there instead of a receipt
//! line (ADR-0060, ticket #417); each Wake marks exactly once and a
//! restart/interrupt Wake leaves no entry at all.
//!
//! Every test scripts the Backend's reads and injects tiny poll cadences, so no
//! test waits on a production interval.

use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use super::drain::{
    assistant, ctx, noticed, script_transcript, scripted_app, scripted_app_with, spawn_sync, spawn_turn,
    user, wait_for_card_header, wait_for_card_text,
};
use crate::backend::{
    BackgroundTask, ContentBlock, FinishReason, MessageRole, Part, ReasoningPart, SessionTranscript,
    StepFinish, TextPart, ToolCall, ToolIdentity, ToolOutput, ToolStatus, TranscriptMessage, WakeSource,
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

/// The settled `shell` call that moved a run to the background: the panel the
/// hosting card shows, and the launch a completion entry joins its identity
/// and duration from (by the task's own shell id).
fn background_shell_launch(created: i64, call_id: &str, shell_id: &str, command: &str) -> TranscriptMessage {
    typed_message(
        &format!("msg_launch_{call_id}"),
        MessageRole::Assistant,
        Some(created),
        vec![Part::Tool(ToolCall {
            identity: ToolIdentity {
                name: "shell".into(),
                call_id: call_id.into(),
            },
            status: ToolStatus::Completed,
            started_at: Some(created),
            input: Some(serde_json::json!({ "command": command })),
            // The background marker the run's own plugin writes: settled call,
            // run still going — what the read derives a Background Task from.
            metadata: Some(serde_json::json!({ "status": "running", "shellID": shell_id })),
            output: ToolOutput {
                raw: None,
                blocks: vec![ContentBlock::Text("moved to background".into())],
                error: None,
            },
        })],
    )
}

/// [`background_shell_launch`] for a backgrounded subagent, identified by its
/// child session.
fn background_subagent_launch(
    created: i64,
    call_id: &str,
    child_id: &str,
    description: &str,
) -> TranscriptMessage {
    typed_message(
        &format!("msg_launch_{call_id}"),
        MessageRole::Assistant,
        Some(created),
        vec![Part::Tool(ToolCall {
            identity: ToolIdentity {
                name: "subagent".into(),
                call_id: call_id.into(),
            },
            status: ToolStatus::Completed,
            started_at: Some(created),
            input: Some(serde_json::json!({ "description": description })),
            metadata: Some(serde_json::json!({ "status": "running", "sessionID": child_id })),
            output: ToolOutput {
                raw: None,
                blocks: vec![ContentBlock::Text("moved to background".into())],
                error: None,
            },
        })],
    )
}

/// The preamble every merged-entry scenario's card hosted: the anchor prompt,
/// the assistant's hand-off line, and each settled launch panel the card
/// showed. `resumed` appends the Wake's own work, when the scenario has it; the
/// two side offsets are stated once, here.
fn merged_entry_timeline(
    started: i64,
    launches: Vec<TranscriptMessage>,
    resumed: Vec<TranscriptMessage>,
) -> Vec<TranscriptMessage> {
    let mut messages = vec![
        user("msg_cola_anchor", started - 60_000, "跑一下 CI"),
        assistant(started - 30_000, "已经交给后台了。"),
    ];
    messages.extend(launches);
    messages.extend(resumed);
    messages
}

/// [`merged_entry_timeline`] with the standard backgrounded `shell` launch —
/// the shared `call_bg`/`sh_bg`/`gh run watch` fixture most merged-entry
/// scenarios host.
fn merged_shell_timeline(started: i64, resumed: Vec<TranscriptMessage>) -> Vec<TranscriptMessage> {
    merged_entry_timeline(
        started,
        vec![background_shell_launch(
            started,
            "call_bg",
            "sh_bg",
            "gh run watch",
        )],
        resumed,
    )
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

/// A resumed run long enough to overflow ONE card (7200 chars > the 6000-char
/// card text budget), so its chain must split and hand over — the Fresh
/// path's top-level continuation fixture.
fn long_resumed_work(created: i64) -> TranscriptMessage {
    typed_message(
        &format!("msg_a_{created}"),
        MessageRole::Assistant,
        Some(created),
        vec![
            Part::Text(TextPart {
                text: "很长的回答。".repeat(1_200),
                started_at: Some(created),
            }),
            Part::StepFinish(StepFinish {
                reason: FinishReason::Stop,
            }),
        ],
    )
}

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
    card_text(card).contains(WAKE_LEAD)
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

/// Every card POST (a reply or a top-level send) the platform recorded — the
/// counter for "was a new card posted?" (an in-place PATCH updates one).
async fn posted_cards(platform: &RecordingPlatform) -> usize {
    platform
        .calls
        .lock()
        .await
        .iter()
        .filter(|call| {
            matches!(
                call,
                PlatformCall::ReplyCard { .. } | PlatformCall::SendCard { .. }
            )
        })
        .count()
}

/// The one-card-per-request contract (ADR-0066, #485): a Turn idles with a
/// live Background Task and yields 「⏳ 等待后台任务」; the task's completing
/// shell Wake resumes THAT card in place — **no card is posted and nothing is
/// replied to**. The retiring task's fixed entry lands on the card that hosted
/// it, the resumed work streams into the same card, and it ends ✅ there: the
/// header cycles 等待后台任务 → 🔄 继续中 → ✅, and the delivering PATCH
/// advances the durable Wake Watermark (ADR-0061) so a restart cannot re-post
/// the Wake.
#[tokio::test]
async fn a_completion_wake_resumes_the_yielded_card_in_place() {
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
    let posts_before = posted_cards(&platform).await;
    let replies_before = platform.replied_cards().await.len();

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
    wait_for_card_update(
        &platform,
        "the resumed card's done state",
        CardUpdates::Latest,
        |card| card_header(card).contains("✅") && card_text(card).contains("CI 通过了。"),
    )
    .await;

    // No card was posted and nothing was replied to: the SAME card carried the
    // completion entry, the resumed work and the ending.
    assert_eq!(
        posted_cards(&platform).await,
        posts_before,
        "an in-place resume posts no card: {:?}",
        platform.calls.lock().await
    );
    assert_eq!(
        platform.replied_cards().await.len(),
        replies_before,
        "an in-place resume replies to nothing: {:?}",
        platform.calls.lock().await
    );

    let last = platform.updated_cards().await.last().cloned().unwrap();
    assert!(
        card_text(&last).contains("CI 通过了。"),
        "the resumed work lands on the card the request opened: {last}"
    );
    assert!(
        card_text(&last).contains("已经交给后台了。"),
        "the resumed card keeps the request's own timeline: {last}"
    );
    // No 承接 line: the card has its context — one card per request.
    assert!(
        !card_text(&last).contains(WAKE_LEAD),
        "an in-place resume writes no 承接 line: {last}"
    );
    // The retiring task's own entry, exactly once, ahead of the work it
    // announces, and its live row gone.
    assert_eq!(
        card_text(&last).matches("后台任务完成").count(),
        1,
        "exactly one completion entry: {last}"
    );
    let entry = body_index(&last, "后台任务完成").expect("the retired task's entry renders");
    let work = body_index(&last, "CI 通过了。").expect("the resumed work renders");
    assert!(
        entry < work,
        "the entry precedes the work it announces (entry@{entry}, work@{work}): {last}"
    );
    assert!(
        !card_text(&last).contains("⏳ 后台任务（"),
        "the retired task's live row is gone: {last}"
    );
    assert!(
        card_text(&last).contains("📁"),
        "the resumed card keeps the Turn Footer: {last}"
    );

    // The header cycled waiting → 🔄 resuming → ✅ on that one card: the
    // resume PATCH says the task is done and the run works on.
    assert!(
        platform
            .updated_cards()
            .await
            .iter()
            .any(|card| card_header(card).contains("后台任务完成，继续处理中")),
        "the card takes the resuming header before the work streams: {:?}",
        platform.updated_cards().await
    );
    // The resumed work streamed before the ending (the ✅ was not the first
    // thing the card showed).
    assert!(
        platform
            .updated_cards()
            .await
            .iter()
            .any(|card| card_text(card).contains("CI 通过了。") && !card_header(card).contains("✅")),
        "the resumed work streams into the card before it ends: {:?}",
        platform.updated_cards().await
    );
    // A p2p turn under the long-task threshold (the default rules): the
    // in-place resume's ending is not announced — it is the same card, and the
    // card's own updates are what the operator watches.
    assert!(
        platform.completion_notices().await.is_empty(),
        "a short p2p resume sends no notice: {:?}",
        platform.calls.lock().await
    );
    // The delivering PATCH carried the entry, so the durable Wake Watermark
    // advanced (ADR-0061): a restart cannot re-post this Wake.
    assert_eq!(
        app.cards_handle()
            .wake_watermarks
            .announced("ses_test")
            .map(|mark| mark.created_ms),
        Some(2_900),
        "a delivered in-place entry advances the durable Wake Watermark"
    );

    // A restart after the resume (the in-memory chain is gone) finds the Wake
    // covered by the durable mark: no fresh continuation re-posts it.
    Turn::drop_card(&app.cards_handle(), "ses_test").await;
    let posts = posted_cards(&platform).await;
    tokio::time::sleep(Duration::from_millis(150)).await;
    assert_eq!(
        posted_cards(&platform).await,
        posts,
        "an in-place resume's Wake must not be re-posted after a restart: {:?}",
        platform.calls.lock().await
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
    // A fresh app has no earlier card: the ✅ can only be the restart
    // continuation's, and the resumed text ties the wait to it.
    wait_for_card_update(
        &platform,
        "the restart continuation's done card",
        CardUpdates::Latest,
        |card| card_header(card).contains("✅") && card_text(card).contains("CI 通过了。"),
    )
    .await;

    let last = platform.updated_cards().await.last().cloned().unwrap();
    // The fresh arm keys the 承接 line before the Wake's own work, so it opens
    // the card here too — not only in the split path.
    assert_eq!(
        body_index(&last, WAKE_LEAD),
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
    // The Fresh card carried the 承接 line: the durable Wake Watermark
    // advanced (ADR-0061), so a later restart cannot re-post this Wake.
    assert_eq!(
        app.cards_handle()
            .wake_watermarks
            .announced("ses_test")
            .map(|mark| mark.created_ms),
        Some(2_900),
        "the Fresh send advances the durable Wake Watermark"
    );
}

/// A resumed run that outgrows one card still splits (ADR-0066 keeps the size
/// split): the in-place resume renders the whole live slice on the request's
/// card, so an overflow finalizes it with the standard 「部分完成，继续中…」
/// handoff — the retiring task's entry stays on the card that hosted it — and
/// the remainder continues on a new card, which takes the ending.
#[tokio::test]
async fn an_in_place_resume_that_overflows_still_splits() {
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
    Turn::set_card_message_id(&app.cards_handle(), "ses_test", "om_waiting").await;
    let posts_before = posted_cards(&platform).await;

    // The resumed run's answer is longer than one card.
    script_transcript(
        &backend,
        vec![
            SessionTranscript::new(vec![
                user("msg_cola_anchor", 1_000, "跑一下 CI"),
                assistant(2_000, "已经交给后台了。"),
                long_resumed_work(3_100),
            ])
            .with_executions(vec![execution(2_500), execution(4_000)])
            .with_wakes(vec![shell_wake(2_900)]),
        ],
    )
    .await;
    spawn_sync(&app);
    // The overflowing card hands over with the standard split header, carrying
    // the retirement's entry ...
    wait_for_card_update(
        &platform,
        "the overflowing resume's handoff",
        CardUpdates::Latest,
        |card| {
            card_header(card).contains("继续中") && card_text(card).contains("🔔 后台任务完成：gh run watch")
        },
    )
    .await;

    // ... and the remainder continues on a new card, which takes the ending.
    wait_for_card_update(
        &platform,
        "the continuation's done state",
        CardUpdates::Latest,
        |card| card_header(card).contains("✅") && card_text(card).contains("很长的回答。"),
    )
    .await;
    assert!(
        posted_cards(&platform).await > posts_before,
        "an overflowing resume continues on a new card: {:?}",
        platform.calls.lock().await
    );
    // The entry stayed on the card the task lived on: no continuation carries
    // it, and the request's card is the one that was finalized.
    let handover = patches_to(&platform, "om_waiting")
        .await
        .pop()
        .expect("the request's card was PATCHed");
    assert!(
        card_text(&handover).contains("🔔 后台任务完成：gh run watch")
            && card_header(&handover).contains("继续中"),
        "the entry rides the finalized card: {handover}"
    );
}

/// The mid-resume user message (ADR-0066, ADR-0043): a resumed card is LIVE
/// again — render-owned — so a message landing below it is a Supplement. The
/// chain splits at the message, the continuation replies to it with the
/// receipt, and the resumed run keeps streaming into that continuation (the
/// same chain, so the same settle loop) — never a new Turn that would strand
/// the resumed work.
#[tokio::test]
async fn a_message_during_a_resumed_run_supplements_it() {
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

    // The resume: the Wake's own work, its Execution not yet bounded, so the
    // resumed run stays live.
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
        "the resumed run is live on the request's card"
    );

    // A message lands below it: the owned chain routes it as a Supplement.
    app.handle_message(incoming(
        "msg_sup".into(),
        "chat_1".into(),
        "p2p".into(),
        None,
        "补充一下，改用方案 B".into(),
        None,
    ))
    .await;

    // The continuation replies to the message, carries the receipt, and the
    // resumed run keeps rendering into it — the same chain, no new Turn.
    let resumed = {
        let calls = platform.calls.lock().await;
        calls
            .iter()
            .find_map(|call| match call {
                PlatformCall::ReplyCard { reply_to, card } if reply_to == "msg_sup" => Some(card.clone()),
                _ => None,
            })
            .unwrap_or_else(|| panic!("the supplement must split the chain at the message: {calls:?}"))
    };
    assert!(
        card_text(&resumed).contains("📨 已收到补充"),
        "the continuation carries the supplement receipt: {resumed}"
    );
    assert_eq!(
        Turn::card_state(&app.cards_handle(), "ses_test").await,
        Some(CardState::Streaming),
        "the split leaves the run live on the continuation"
    );
    assert!(
        !Turn::has_pending_split(&app.cards_handle(), "ses_test").await,
        "the split is consumed"
    );

    // The run keeps streaming into the continuation: a later read's part lands
    // on it (the harness serves one id for every card, so the receipts and the
    // new content riding ONE payload is what names it).
    script_transcript(
        &backend,
        vec![
            SessionTranscript::new(vec![
                user("msg_cola_anchor", 1_000, "跑一下 CI"),
                assistant(2_000, "已经交给后台了。"),
                assistant(3_100, "正在合并。"),
                assistant(3_400, "合并完成，改用方案 B 后重跑。"),
            ])
            .with_executions(vec![execution(2_500)])
            .with_wakes(vec![shell_wake(2_900)]),
        ],
    )
    .await;
    wait_for_card_update(
        &platform,
        "the continuation's later content",
        CardUpdates::Latest,
        |card| {
            card_text(card).contains("📨 已收到补充")
                && card_text(card).contains("合并完成，改用方案 B 后重跑。")
        },
    )
    .await;
}

/// One notice (ADR-0066): the in-place resume itself never notifies — a PATCH
/// pushes nothing — and its TRUE END notifies once under the ordinary rules,
/// with the clock the request's ORIGINAL Turn start reads: here a p2p run past
/// the long-task threshold fires exactly one notice, and a resume clock
/// ("now") could not have passed it.
#[tokio::test]
async fn an_in_place_resumes_true_end_notifies_once() {
    let _wd = test_work_dir();
    let waiting = SessionTranscript::new(vec![
        user("msg_cola_anchor", 1_000, "跑一下 CI"),
        assistant(2_000, "已经交给后台了。"),
    ])
    .with_executions(vec![execution(2_500)])
    .with_background_tasks(vec![background_shell(2_100)]);
    let (_dir, app, backend, platform) = scripted_app_with(vec![waiting], Some(SessionStatus::Idle), |cfg| {
        cfg.bridge.long_task_notice = true;
    })
    .await;
    app.long_task_notice_ms.store(50, Ordering::Relaxed);
    let mut context = ctx("ses_test", "跑一下 CI");
    context.requester_open_id = Some(TEST_HOST.to_string());
    Turn::run(&app.turn_handles(), context).await.unwrap();
    assert!(!noticed(&platform).await, "the waiting yield never notifies");
    // The wait outlives the threshold: the notice's clock has to be the
    // request's original Turn start (a resume-time clock would measure ~0).
    tokio::time::sleep(Duration::from_millis(120)).await;

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
        "the resumed card's done state",
        CardUpdates::Latest,
        |card| card_header(card).contains("✅") && card_text(card).contains("CI 通过了。"),
    )
    .await;

    // The notice trails the settle PATCH in the same pass: give it the moment.
    tokio::time::sleep(Duration::from_millis(50)).await;
    let notices = platform.completion_notices().await;
    assert_eq!(notices.len(), 1, "exactly one notice: {notices:?}");
    assert_eq!(notices[0].0, "msg_1", "it replies to the request's message");
    assert!(
        notices[0].3.contains("已完成"),
        "the card's ✅ is what it announces: {:?}",
        notices[0].3
    );
}

/// The ADR-0059 boundary ADR-0066 deliberately keeps: a Wake that is NOT a
/// shell/subagent completion — a server restart, an interruption continuation —
/// does not resume a yielded card in place. The chain continues on a 承接 card
/// below the user's message, exactly as before.
#[tokio::test]
async fn a_restart_wake_on_a_waiting_card_still_splits() {
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
    let posts_before = posted_cards(&platform).await;

    // The server restarted: the Wake records that, and the run resumed with
    // real work.
    let mut restart = shell_wake(2_900);
    restart.source = WakeSource::Restart;
    restart.label = None;
    script_transcript(
        &backend,
        vec![
            SessionTranscript::new(vec![
                user("msg_cola_anchor", 1_000, "跑一下 CI"),
                assistant(2_000, "已经交给后台了。"),
                assistant(3_100, "重启后继续。"),
            ])
            .with_executions(vec![execution(2_500), execution(4_000)])
            .with_wakes(vec![restart]),
        ],
    )
    .await;

    spawn_sync(&app);
    wait_for_card_update(
        &platform,
        "the restart continuation's done card",
        CardUpdates::Latest,
        |card| {
            card_header(card).contains("✅")
                && card_text(card).contains(WAKE_LEAD)
                && card_text(card).contains("重启后继续。")
        },
    )
    .await;

    // A NEW card carried the 承接 line and the resumed work: the yielded card
    // was not resumed in place.
    assert!(
        posted_cards(&platform).await > posts_before,
        "a restart Wake still continues on a new card: {:?}",
        platform.calls.lock().await
    );
    assert!(
        platform
            .updated_cards()
            .await
            .iter()
            .any(|card| card_header(card).contains("继续中")),
        "the yielded card hands over with the standard split header: {:?}",
        platform.updated_cards().await
    );
}

/// The other kept boundary: the Wake-less content-diff fallback. New content
/// with no placeable Wake has no completion to name, so 「后台任务完成」 would
/// lie — the yielded card keeps the ADR-0059 split.
#[tokio::test]
async fn a_wake_less_tail_on_a_waiting_card_still_splits() {
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
    let posts_before = posted_cards(&platform).await;

    // Content landed with no Wake recorded at all: the tail the finalized
    // read missed.
    script_transcript(
        &backend,
        vec![
            SessionTranscript::new(vec![
                user("msg_cola_anchor", 1_000, "跑一下 CI"),
                assistant(2_000, "已经交给后台了。"),
                assistant(3_100, "收尾时补上的一段。"),
            ])
            .with_executions(vec![execution(2_500), execution(4_000)])
            .with_background_tasks(vec![background_shell(2_100)]),
        ],
    )
    .await;

    spawn_sync(&app);
    wait_for_card_update(
        &platform,
        "the content-diff continuation",
        CardUpdates::Latest,
        |card| card_text(card).contains(WAKE_LEAD) && card_text(card).contains("收尾时补上的一段。"),
    )
    .await;

    assert!(
        posted_cards(&platform).await > posts_before,
        "the Wake-less fallback still continues on a new card: {:?}",
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

/// Failure keeps Retry on an in-place resume (ADR-0066): the resumed card is
/// the REQUEST's own — it still carries its prompt and its reply target — so a
/// failed resumed run ends ❌ on it with the ordinary #391 重试, which is
/// exactly what the ❌ notice's 「可点击卡片上的「重试」」 copy promises. The ❌
/// notice fires once, on the request's own card, per the ordinary rules.
#[tokio::test]
async fn a_failed_in_place_resume_keeps_retry() {
    let _wd = test_work_dir();
    let waiting = SessionTranscript::new(vec![
        user("msg_cola_anchor", 1_000, "跑一下 CI"),
        assistant(2_000, "已经交给后台了。"),
    ])
    .with_executions(vec![execution(2_500)])
    .with_background_tasks(vec![background_shell(2_100)]);
    let (_dir, app, backend, platform) = scripted_app(vec![waiting], Some(SessionStatus::Idle)).await;
    // A group turn with a requester: a settle notifies, so the notice's copy
    // can be read back with the card's own terminal.
    let mut context = ctx("ses_test", "跑一下 CI");
    context.is_group = true;
    context.requester_open_id = Some(TEST_HOST.to_string());
    Turn::run(&app.turn_handles(), context).await.unwrap();
    let posts_before = posted_cards(&platform).await;

    // The resumed run fails.
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
        "the resumed card's error",
        CardUpdates::Latest,
        |card| card_header(card).contains("出错"),
    )
    .await;

    let last = platform.updated_cards().await.last().cloned().unwrap();
    assert!(
        card_text(&last).contains("provider 503"),
        "the settled failure reaches the resumed card: {last}"
    );
    assert!(
        card_buttons(&last)
            .iter()
            .any(|button| button["value"]["action"] == "retry"),
        "the request's own card keeps its Retry: {last}"
    );
    assert_eq!(
        posted_cards(&platform).await,
        posts_before,
        "the failed resume posted no card: {:?}",
        platform.calls.lock().await
    );
    let notices = platform.completion_notices().await;
    assert_eq!(notices.len(), 1, "exactly one notice: {notices:?}");
    assert_eq!(notices[0].0, "msg_1", "it replies to the request's message");
    assert!(
        notices[0].3.contains("出错") && notices[0].3.contains("重试"),
        "the notice follows the card's terminal and names its Retry: {:?}",
        notices[0].3
    );
}

/// The other half of the Retry rule (ADR-0059, narrowed by ADR-0066): a SPLIT
/// continuation is armed without a user prompt, so a failed resumed run on one
/// ends ❌ with NO Retry — there is no question to re-ask. A late Wake after
/// the card already ended ✅ is exactly the case that still splits.
#[tokio::test]
async fn a_failed_split_continuation_offers_no_retry() {
    let _wd = test_work_dir();
    let done = SessionTranscript::new(vec![
        user("msg_cola_anchor", 1_000, "跑一下 CI"),
        assistant(2_000, "第一轮回答。"),
    ])
    .with_executions(vec![execution(2_500)]);
    let (_dir, app, backend, platform) = scripted_app(vec![done], Some(SessionStatus::Idle)).await;
    Turn::run(&app.turn_handles(), ctx("ses_test", "跑一下 CI"))
        .await
        .unwrap();
    assert_eq!(
        Turn::card_state(&app.cards_handle(), "ses_test").await,
        Some(CardState::Done)
    );

    script_transcript(
        &backend,
        vec![
            SessionTranscript::new(vec![
                user("msg_cola_anchor", 1_000, "跑一下 CI"),
                assistant(2_000, "第一轮回答。"),
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
        |card| card_header(card).contains("出错") && card_text(card).contains(WAKE_LEAD),
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
    assert!(
        platform.completion_notices().await.is_empty(),
        "the split continuation's own send was the notification: {:?}",
        platform.calls.lock().await
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
        body_index(&last, WAKE_LEAD),
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
    wait_for_any_card(&platform, WAKE_LEAD).await;
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

    let posted = continuation_sends(&platform).await.len();
    assert!(posted >= 1, "the wake-only pass rendered its continuation");
    // Many more passes with the external message newest: no NEW continuation
    // is posted (the render may PATCH the one card in place as often as it
    // likes — a send is what a re-post would add).
    tokio::time::sleep(Duration::from_millis(150)).await;
    assert_eq!(
        continuation_sends(&platform).await.len(),
        posted,
        "a Wake must not render when the newest user message is external: {:?}",
        platform.calls.lock().await
    );
}

/// Acceptance 5b: a Wake on a non-active Session is not rendered while it is
/// away — ADR-0017's no-interleaving scope is unchanged. The missed Wake
/// renders when the Session becomes active again (collect.rs).
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
        continuation_sends(&platform).await.is_empty(),
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
        continuation_sends(&platform).await.is_empty(),
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
    // The ✅ can only be this continuation's (a fresh app, one card), and the
    // resumed text ties the wait to it.
    wait_for_card_update(
        &platform,
        "the continuation's done card",
        CardUpdates::Latest,
        |card| card_header(card).contains("✅") && card_text(card).contains("CI 通过了。"),
    )
    .await;
    let posted = continuation_sends(&platform).await.len();
    assert!(posted >= 1, "the Wake must post its continuation first");

    // Many more sync passes over the same read: the card chain already holds
    // the Wake's work, so nothing new is owed. Only SENDS count: the render
    // PATCHes the one card as it streams.
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(
        continuation_sends(&platform).await.len(),
        posted,
        "a rendered Wake must not be re-posted on every poll: {:?}",
        platform.calls.lock().await
    );
}

/// A second completion resumes the SAME card again — no chain of continuation
/// cards (ADR-0066 acceptance 3). The resumed run leaves a new Background Task
/// live, so the card yields back to 「⏳ 等待后台任务」 with its remaining list;
/// the second task's Wake then resumes that same card in place: its own entry,
/// its own work, and the final ✅ all land there. Nothing is ever posted.
#[tokio::test]
async fn a_second_wake_resumes_the_same_card_again() {
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
    // The harness replies every card with one id; name the request's card so
    // every PATCH below can be told apart from any other card's.
    Turn::set_card_message_id(&app.cards_handle(), "ses_test", "om_waiting").await;
    let posts_before = posted_cards(&platform).await;

    // The first completion resumes the card, and its run backgrounds a SECOND
    // task: the card yields back to 「⏳ 等待后台任务」 with the remaining list.
    script_transcript(
        &backend,
        vec![
            SessionTranscript::new(vec![
                user("msg_cola_anchor", 1_000, "跑一下 CI"),
                assistant(2_000, "已经交给后台了。"),
                assistant(3_100, "CI 通过了。"),
                background_shell_launch(3_200, "call_bg2", "sh_bg2", "再次运行"),
            ])
            .with_executions(vec![execution(2_500), execution(4_000)])
            .with_wakes(vec![shell_wake(2_900)])
            .with_background_tasks(vec![another_background_shell(3_200)]),
        ],
    )
    .await;
    spawn_sync(&app);
    wait_for_card_update(
        &platform,
        "the card waiting on the second task",
        CardUpdates::Latest,
        |card| card_text(card).contains("CI 通过了。") && card_text(card).contains("· shell：**再次运行**"),
    )
    .await;
    assert_eq!(
        Turn::card_state(&app.cards_handle(), "ses_test").await,
        Some(CardState::Waiting),
        "a live task yields the resumed card back to ⏳"
    );

    // The second task's completion: its own Wake, its own work, a new boundary.
    let mut second_wake = shell_wake(4_900);
    second_wake.shell_id = Some("sh_bg2".into());
    second_wake.job_id = Some("sh_bg2".into());
    second_wake.label = Some("再次运行".into());
    script_transcript(
        &backend,
        vec![
            SessionTranscript::new(vec![
                user("msg_cola_anchor", 1_000, "跑一下 CI"),
                assistant(2_000, "已经交给后台了。"),
                assistant(3_100, "CI 通过了。"),
                background_shell_launch(3_200, "call_bg2", "sh_bg2", "再次运行"),
                assistant(5_100, "合并完成。"),
            ])
            .with_executions(vec![execution(2_500), execution(4_000), execution(6_000)])
            .with_wakes(vec![shell_wake(2_900), second_wake]),
        ],
    )
    .await;
    wait_for_card_update(
        &platform,
        "the resumed card's second ending",
        CardUpdates::Latest,
        |card| card_header(card).contains("✅") && card_text(card).contains("合并完成。"),
    )
    .await;

    // Both runs live on the ONE request card: both entries, both runs' work.
    let last = platform.updated_cards().await.last().cloned().unwrap();
    let text = card_text(&last);
    assert!(
        text.contains("CI 通过了。") && text.contains("合并完成。"),
        "the same card carries both resumed runs: {last}"
    );
    assert_eq!(
        text.matches("后台任务完成").count(),
        2,
        "each completion leaves its own entry, once: {last}"
    );
    assert!(
        !text.contains("⏳ 后台任务（"),
        "the second retirement leaves no live list: {last}"
    );
    assert!(
        !text.contains(WAKE_LEAD),
        "no continuation was ever posted for either Wake: {last}"
    );
    assert_eq!(
        posted_cards(&platform).await,
        posts_before,
        "two completions, still one card: {:?}",
        platform.calls.lock().await
    );
    // Both completions' PATCHes went to the request's own card.
    assert!(
        patches_to(&platform, "om_waiting").await.len() >= 2,
        "the request's card is the one that flowed: {:?}",
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
    // The waiting card is not ✅; the resumed text ties the wait to the
    // continuation that answers the stale-marker rule under test.
    wait_for_card_update(
        &platform,
        "the continuation's done card",
        CardUpdates::Latest,
        |card| card_header(card).contains("✅") && card_text(card).contains("CI 通过了。"),
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

/// Fix 1 (live defect): the Fresh path must not resurrect a Wake the
/// conversation has moved past. A user message NEWER than the newest Wake
/// proves a later cola life already saw or superseded it, and re-posting would
/// replay every turn after the Wake; with no newer user message the genuine
/// restart case still posts.
#[tokio::test]
async fn a_stale_wake_is_not_reposted_after_a_restart() {
    let _wd = test_work_dir();
    // The Wake resumed a run, and a LATER cola turn followed it: the
    // conversation has moved past the Wake.
    let stale = SessionTranscript::new(vec![
        user("msg_cola_anchor", 1_000, "跑一下 CI"),
        assistant(2_000, "已经交给后台了。"),
        assistant(3_100, "CI 通过了。"),
        user("msg_cola_later", 5_000, "后来我又说了一句"),
        assistant(5_100, "好的。"),
    ])
    .with_executions(vec![execution(2_500), execution(4_000), execution(6_000)])
    .with_wakes(vec![shell_wake(2_900)]);
    let (_dir, app, backend, platform) = scripted_app(vec![stale], Some(SessionStatus::Idle)).await;

    spawn_sync(&app);
    // Several sync passes over the stale read: no Fresh card may be posted.
    tokio::time::sleep(Duration::from_millis(150)).await;
    assert!(
        continuation_sends(&platform).await.is_empty(),
        "a Wake the conversation moved past must not be re-posted: {:?}",
        platform.calls.lock().await
    );

    // The same restart read WITHOUT the later user message is the genuine
    // case: the Wake's run is still pending (or it finished while cola was
    // down), so the continuation must post.
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
    wait_for_any_card(&platform, WAKE_LEAD).await;
    assert!(
        platform.sent_cards().await.iter().any(is_continuation),
        "the genuine restart Wake still posts: {:?}",
        platform.calls.lock().await
    );
}

/// Acceptance 1 (ADR-0061): a Wake a previous cola life already announced is
/// never re-posted after a restart — the durable Wake Watermark is the fact
/// the lost card left behind, and without it the Fresh path re-announces
/// (#424).
#[tokio::test]
async fn an_announced_wake_is_not_reposted_after_a_restart() {
    let _wd = test_work_dir();
    let resumed = SessionTranscript::new(vec![
        user("msg_cola_anchor", 1_000, "跑一下 CI"),
        assistant(2_000, "已经交给后台了。"),
        assistant(3_100, "CI 通过了。"),
    ])
    .with_executions(vec![execution(2_500), execution(4_000)])
    .with_wakes(vec![shell_wake(2_900)]);
    let (_dir, app, _backend, platform) = scripted_app(vec![resumed], Some(SessionStatus::Idle)).await;
    // The previous life's card announced this Wake before the restart.
    app.cards_handle()
        .wake_watermarks
        .advance("ses_test", "msg_wake_2900", 2_900);

    spawn_sync(&app);
    // Several sync passes over the read: no Fresh card may be posted.
    tokio::time::sleep(Duration::from_millis(150)).await;
    assert!(
        continuation_sends(&platform).await.is_empty(),
        "an announced Wake must not be re-posted after a restart: {:?}",
        platform.calls.lock().await
    );
}

/// Acceptance 2 (ADR-0061): a Wake NEWER than the watermark still continues —
/// the mark covers only what a previous life has shown, so work that finished
/// while cola was down is never lost.
#[tokio::test]
async fn a_wake_newer_than_the_watermark_still_continues() {
    let _wd = test_work_dir();
    let resumed = SessionTranscript::new(vec![
        user("msg_cola_anchor", 1_000, "跑一下 CI"),
        assistant(2_000, "已经交给后台了。"),
        assistant(3_100, "CI 通过了。"),
    ])
    .with_executions(vec![execution(2_500), execution(4_000)])
    .with_wakes(vec![shell_wake(2_900)]);
    let (_dir, app, _backend, platform) = scripted_app(vec![resumed], Some(SessionStatus::Idle)).await;
    // An older Wake was announced before the restart; this one was not.
    app.cards_handle()
        .wake_watermarks
        .advance("ses_test", "msg_wake_1900", 1_900);

    spawn_sync(&app);
    wait_for_any_card(&platform, WAKE_LEAD).await;
    assert!(
        platform.sent_cards().await.iter().any(is_continuation),
        "a Wake newer than the watermark must still continue: {:?}",
        platform.calls.lock().await
    );
}

/// #424 follow-up (live 2026-09-30): once the restart continuation has
/// rendered the Wake's work and finalized, later Sync passes must post
/// nothing. The chain probe judged the whole newest-user Turn against the
/// Fresh card's state — whose scope starts at the Wake — so the pre-Wake
/// content the lost card had shown read as unrendered on every pass and a
/// fresh 承接 card looped out each Sync tick.
#[tokio::test]
async fn a_settled_restart_continuation_is_not_re_split() {
    let _wd = test_work_dir();
    let resumed = SessionTranscript::new(vec![
        user("msg_cola_anchor", 1_000, "跑一下 CI"),
        assistant(2_000, "已经交给后台了。"),
        assistant(3_100, "CI 通过了。"),
    ])
    .with_executions(vec![execution(2_500), execution(4_000)])
    .with_wakes(vec![shell_wake(2_900)]);
    let (_dir, app, _backend, platform) = scripted_app(vec![resumed], Some(SessionStatus::Idle)).await;
    // A topic session whose anchor a continuation can reply to: a lobby has
    // no reply target, so the loop's re-split aborts with a warning instead
    // of posting the cards the live defect showed.
    seed_entry(
        &app,
        crate::config::SessionEntry {
            thread_key: crate::config::ThreadKey::new("chat_1".into(), "om_wake_topic".into()),
            session_id: "ses_test".into(),
            directory: "/work".into(),
            agent: None,
            model: None,
            variant: None,
            auto_accept: false,
            topic_anchor: Some("om_anchor".into()),
            topic_root: None,
        },
    )
    .await;

    spawn_sync(&app);
    // The restart continuation posts once and settles ✅.
    wait_for_card_update(
        &platform,
        "the restart continuation's done card",
        CardUpdates::Latest,
        |card| card_header(card).contains("✅") && card_text(card).contains("CI 通过了。"),
    )
    .await;
    let posted = continuation_sends(&platform).await.len();

    // Several more sync passes over the same read: no further card may post.
    tokio::time::sleep(Duration::from_millis(150)).await;
    assert_eq!(
        continuation_sends(&platform).await.len(),
        posted,
        "a settled restart continuation must not be re-split: {:?}",
        platform.calls.lock().await
    );
}

/// Acceptance 3 (ADR-0061): a message being admitted blocks the Fresh post —
/// the claim is set before the Turn writes the prompt, so the Sync's stale
/// read cannot race it — and releasing the claim lets the continuation post.
#[tokio::test]
async fn an_inbound_claim_blocks_then_releases_the_fresh_post() {
    let _wd = test_work_dir();
    let resumed = SessionTranscript::new(vec![
        user("msg_cola_anchor", 1_000, "跑一下 CI"),
        assistant(2_000, "已经交给后台了。"),
        assistant(3_100, "CI 通过了。"),
    ])
    .with_executions(vec![execution(2_500), execution(4_000)])
    .with_wakes(vec![shell_wake(2_900)]);
    let (_dir, app, _backend, platform) = scripted_app(vec![resumed], Some(SessionStatus::Idle)).await;
    app.waits_handle().note_inbound("ses_test").await;

    spawn_sync(&app);
    tokio::time::sleep(Duration::from_millis(120)).await;
    assert!(
        continuation_sends(&platform).await.is_empty(),
        "an inbound claim must block the Fresh post: {:?}",
        platform.calls.lock().await
    );

    app.waits_handle().clear_inbound("ses_test").await;
    wait_for_any_card(&platform, WAKE_LEAD).await;
    assert!(
        platform.sent_cards().await.iter().any(is_continuation),
        "the claim's release lets the continuation post: {:?}",
        platform.calls.lock().await
    );
}

/// Acceptance 3 (ADR-0061), the corroborating read: a message that IS in the
/// store — another shared-store client's — is invisible to the Sync's stale
/// read; the pre-send re-read sees the newest user anchor moved and bails.
#[tokio::test]
async fn a_changed_anchor_in_the_recheck_blocks_the_fresh_post() {
    let _wd = test_work_dir();
    let wake_read = SessionTranscript::new(vec![
        user("msg_cola_anchor", 1_000, "跑一下 CI"),
        assistant(2_000, "已经交给后台了。"),
        assistant(3_100, "CI 通过了。"),
    ])
    .with_executions(vec![execution(2_500), execution(4_000)])
    .with_wakes(vec![shell_wake(2_900)]);
    let moved_on = SessionTranscript::new(vec![
        user("msg_cola_anchor", 1_000, "跑一下 CI"),
        assistant(2_000, "已经交给后台了。"),
        assistant(3_100, "CI 通过了。"),
        user("msg_ext", 5_000, "等一下，先别继续"),
        assistant(5_100, "好。"),
    ])
    .with_executions(vec![execution(2_500), execution(4_000), execution(6_000)])
    .with_wakes(vec![shell_wake(2_900)]);
    let (_dir, app, _backend, platform) =
        scripted_app(vec![wake_read, moved_on], Some(SessionStatus::Idle)).await;

    spawn_sync(&app);
    tokio::time::sleep(Duration::from_millis(150)).await;
    assert!(
        continuation_sends(&platform).await.is_empty(),
        "a moved-on conversation must not get a Fresh continuation: {:?}",
        platform.calls.lock().await
    );
}

/// Fix 2 (live defect): a Fresh card is sent top-level (a restart leaves no
/// reply target), so when its run overflows one card the flush must continue
/// top-level too. Before the fix the continuation was never sent and the card
/// stayed at 「部分完成，继续中」 with the ending unstamped.
#[tokio::test]
async fn a_top_level_continuation_keeps_its_chain() {
    let _wd = test_work_dir();
    let resumed = SessionTranscript::new(vec![
        user("msg_cola_anchor", 1_000, "跑一下 CI"),
        long_resumed_work(3_100),
    ])
    .with_executions(vec![execution(2_500), execution(4_000)])
    .with_wakes(vec![shell_wake(2_900)]);
    let (_dir, app, _backend, platform) = scripted_app(vec![resumed], Some(SessionStatus::Idle)).await;

    spawn_sync(&app);
    // The first card hands over at 继续中 (never ✅); the long resumed text ties
    // the wait to the top-level continuation that takes the ending.
    wait_for_card_update(
        &platform,
        "the continuation's done card",
        CardUpdates::Latest,
        |card| card_header(card).contains("✅") && card_text(card).contains("很长的回答。"),
    )
    .await;

    // The first card hands over with the standard split header ...
    assert!(
        platform
            .updated_cards()
            .await
            .iter()
            .any(|card| card_header(card).contains("继续中")),
        "the overflowing card hands over: {:?}",
        platform.updated_cards().await
    );
    // ... and the remainder continues in a NEW top-level message that takes
    // the ending — nothing is left at 继续中.
    let sent = platform.sent_cards().await;
    assert!(sent.len() >= 2, "the overflow must continue top-level: {sent:?}");
    let last = platform.updated_cards().await.last().cloned().unwrap();
    assert!(card_header(&last).contains("✅"), "{last}");
    assert!(
        card_text(&last).contains("很长的回答。"),
        "the remainder lands on the continuation: {last}"
    );
}

/// The pinned entry body's two server times: a launch at 13:50 and its Wake at
/// 14:02 — 12 minutes later — so the copy pins as
/// `shell sh_bg · 14:02 · 12m` in any machine timezone.
fn entry_span() -> (i64, i64) {
    let finished = crate::feishu::card::test_local_ms(2026, 9, 29, 14, 2);
    (finished - 12 * 60_000, finished)
}

/// The merged-path entry (ADR-0060, ticket #417): a shell Wake whose work
/// resumes a CARD THAT IS ALREADY LIVE leaves one folded ledger entry at the
/// Wake's moment, before the resumed work — the live row leaves the list and
/// the mechanical completion line becomes the entry's collapsed title, its
/// identity and timing the fold body — so "never silently dropped" does not
/// depend on the model narrating it. A repeated poll must not duplicate it.
#[tokio::test]
async fn a_merged_shell_wake_leaves_one_entry_on_the_live_turn_card() {
    let _wd = test_work_dir();
    let (started, finished) = entry_span();
    let live = SessionTranscript::new(merged_shell_timeline(started, vec![]))
        .with_executions(vec![execution(started + 30_000)])
        .with_background_tasks(vec![background_shell(started)]);
    let (_dir, app, backend, platform) = scripted_app(vec![live], Some(SessionStatus::Busy)).await;

    // The Turn is live: its drain keeps rendering onto the same card, whose
    // ledger lists the run.
    let turn = spawn_turn(&app, ctx("ses_test", "跑一下 CI"));
    wait_for_card_update(&platform, "the live ledger row", CardUpdates::Any, |card| {
        let text = card_text(card);
        text.contains("⏳ 后台任务（1）") && text.contains("· shell：**gh run watch**")
    })
    .await;

    // The shell Wake retires the task and its run resumes on the SAME card
    // (its Execution has not reached a boundary yet).
    script_transcript(
        &backend,
        vec![
            SessionTranscript::new(merged_shell_timeline(
                started,
                vec![assistant(finished + 30_000, "CI 通过了。")],
            ))
            .with_executions(vec![execution(started + 30_000)])
            .with_wakes(vec![shell_wake(finished)]),
        ],
    )
    .await;
    wait_for_card_text(&platform, "CI 通过了。").await;
    wait_for_card_update(
        &platform,
        "the merged completion entry",
        CardUpdates::Any,
        |card| card_text(card).contains("后台任务完成"),
    )
    .await;

    let last = platform.updated_cards().await.last().cloned().unwrap();
    let text = card_text(&last);
    assert!(
        text.contains("🔔 后台任务完成：gh run watch"),
        "the Wake's own command is the entry's collapsed title: {last}"
    );
    assert!(
        text.contains("shell sh_bg · 14:02 · 12m"),
        "the fold body carries identity and timing (ADR-0060): {last}"
    );
    assert_eq!(
        text.matches("后台任务完成").count(),
        1,
        "exactly one entry: {last}"
    );
    // The entry is one folded collapsible panel, and the live row it retired
    // is gone: one mechanism, not two renderings.
    let entry = body_index(&last, "后台任务完成").expect("the entry renders");
    let element = &last["body"]["elements"][entry];
    assert_eq!(
        element["tag"], "collapsible_panel",
        "the entry is folded: {element}"
    );
    assert_eq!(
        element["expanded"], false,
        "the entry starts collapsed: {element}"
    );
    assert!(
        !text.contains("⏳ 后台任务（"),
        "the retired run leaves the live list: {last}"
    );
    let work = body_index(&last, "CI 通过了。").expect("the resumed work renders");
    assert!(
        entry < work,
        "the entry precedes the work it announces (entry@{entry}, work@{work}): {last}"
    );

    // Later polls over the same read must not duplicate it.
    tokio::time::sleep(Duration::from_millis(80)).await;
    let later = platform.updated_cards().await.last().cloned().unwrap();
    assert_eq!(
        card_text(&later).matches("后台任务完成").count(),
        1,
        "a repeated poll must not duplicate the entry: {later}"
    );

    // The PATCH that carried the entry advanced the durable Wake Watermark
    // (ADR-0061): a restart must not re-announce this Wake.
    assert_eq!(
        app.cards_handle()
            .wake_watermarks
            .announced("ses_test")
            .map(|mark| mark.created_ms),
        Some(finished),
        "a delivered merged entry advances the durable Wake Watermark"
    );

    // Let the run end so the Turn finishes cleanly.
    script_transcript(
        &backend,
        vec![
            SessionTranscript::new(merged_shell_timeline(
                started,
                vec![assistant(finished + 30_000, "CI 通过了。")],
            ))
            .with_executions(vec![execution(started + 30_000), execution(finished + 60_000)])
            .with_wakes(vec![shell_wake(finished)]),
        ],
    )
    .await;
    backend
        .set_session_status("ses_test", Some(SessionStatus::Idle))
        .await;
    let result = tokio::time::timeout(Duration::from_secs(5), turn)
        .await
        .expect("the boundary ends the turn")
        .unwrap();
    result.unwrap();

    // The settled card is a full re-render of the same timeline: the entry is
    // still exactly one, in its place.
    let settled = platform.updated_cards().await.last().cloned().unwrap();
    assert_eq!(
        card_text(&settled).matches("后台任务完成").count(),
        1,
        "the settled re-render keeps exactly one entry: {settled}"
    );
    let entry = body_index(&settled, "后台任务完成").expect("the entry survives the settle");
    let work = body_index(&settled, "CI 通过了。").expect("the work stays below it");
    assert!(
        entry < work,
        "the entry keeps its place on the settled card (entry@{entry}, work@{work}): {settled}"
    );
}

/// Several completions in one read: each Wake leaves its own entry, in the
/// read's own (transcript) order, even when the task that started first
/// finishes last — and each entry joins its OWN task's identity and span.
#[tokio::test]
async fn several_completions_leave_one_entry_each_in_wake_order() {
    let _wd = test_work_dir();
    // The shell run is the pinned `entry_span()` one: launched at 13:50,
    // completed at 14:02 (`shell sh_bg · 14:02 · 12m`). The subagent starts
    // later but finishes first.
    let (base, shell_wake_at) = entry_span();
    let subagent_wake_at = base + 10 * 60_000; // 14:00, 5m after its own launch
    let timeline = |resumed: Vec<TranscriptMessage>| {
        merged_entry_timeline(
            base,
            vec![
                background_shell_launch(base, "call_bg", "sh_bg", "gh run watch"),
                // The subagent started later but finishes first.
                background_subagent_launch(base + 5 * 60_000, "call_sub", "ses_child", "review the diff"),
            ],
            resumed,
        )
    };
    let shell_task = background_shell(base);
    let subagent_task = BackgroundTask {
        tool: ToolIdentity {
            name: "subagent".into(),
            call_id: "call_sub".into(),
        },
        shell_id: None,
        child_id: Some("ses_child".into()),
        started_at: Some(base + 5 * 60_000),
    };
    let live = SessionTranscript::new(timeline(vec![]))
        .with_executions(vec![execution(base + 30_000)])
        .with_background_tasks(vec![shell_task.clone(), subagent_task.clone()]);
    let (_dir, app, backend, platform) = scripted_app(vec![live], Some(SessionStatus::Busy)).await;
    let turn = spawn_turn(&app, ctx("ses_test", "跑一下 CI"));
    wait_for_card_update(&platform, "both live rows", CardUpdates::Any, |card| {
        card_text(card).contains("⏳ 后台任务（2）")
    })
    .await;

    // The read carries the later-starting subagent's Wake FIRST (it finished
    // first), then the shell's.
    script_transcript(
        &backend,
        vec![
            SessionTranscript::new(timeline(vec![assistant(shell_wake_at + 30_000, "都跑完了。")]))
                .with_executions(vec![execution(base + 30_000)])
                .with_wakes(vec![
                    subagent_wake(subagent_wake_at, "review the diff"),
                    shell_wake(shell_wake_at),
                ]),
        ],
    )
    .await;
    wait_for_card_text(&platform, "都跑完了。").await;

    let last = platform.updated_cards().await.last().cloned().unwrap();
    let text = card_text(&last);
    assert_eq!(
        text.matches("后台任务完成").count(),
        1,
        "the shell Wake leaves exactly one shell entry: {last}"
    );
    assert_eq!(
        text.matches("子代理完成").count(),
        1,
        "the subagent Wake leaves exactly one subagent entry: {last}"
    );
    assert!(
        text.contains("shell sh_bg · 14:02 · 12m") && text.contains("子代理 ses_child · 14:00 · 5m"),
        "each entry carries its own identity and span: {last}"
    );
    let subagent = body_index(&last, "子代理完成").expect("the subagent entry renders");
    let shell = body_index(&last, "后台任务完成").expect("the shell entry renders");
    assert!(
        subagent < shell,
        "entries keep the read's order (subagent finished first) (subagent@{subagent}, shell@{shell}): {last}"
    );
    assert!(
        !text.contains("⏳ 后台任务（"),
        "both retired runs leave the live list: {last}"
    );

    // End the turn.
    script_transcript(
        &backend,
        vec![
            SessionTranscript::new(timeline(vec![assistant(shell_wake_at + 30_000, "都跑完了。")]))
                .with_executions(vec![execution(base + 30_000), execution(shell_wake_at + 60_000)])
                .with_wakes(vec![
                    subagent_wake(subagent_wake_at, "review the diff"),
                    shell_wake(shell_wake_at),
                ])
                .with_background_tasks(vec![shell_task, subagent_task]),
        ],
    )
    .await;
    backend
        .set_session_status("ses_test", Some(SessionStatus::Idle))
        .await;
    let result = tokio::time::timeout(Duration::from_secs(5), turn)
        .await
        .expect("the boundary ends the turn")
        .unwrap();
    result.unwrap();
}

/// A subagent Wake merging into a live card leaves an entry whose collapsed
/// title is its own task description, clipped to one short line, and whose fold
/// names the child session.
#[tokio::test]
async fn a_merged_subagent_wake_labels_the_entry_with_its_description() {
    let _wd = test_work_dir();
    let long = "很长的子代理任务描述".repeat(10); // 100 chars: the title clips at 60
    let (started, finished) = entry_span();
    let timeline = |resumed: Vec<TranscriptMessage>| {
        merged_entry_timeline(
            started,
            vec![background_subagent_launch(
                started,
                "call_sub",
                "ses_child",
                &long,
            )],
            resumed,
        )
    };
    let live = SessionTranscript::new(timeline(vec![]))
        .with_executions(vec![execution(started + 30_000)])
        .with_background_tasks(vec![background_shell(started)]);
    let (_dir, app, backend, platform) = scripted_app(vec![live], Some(SessionStatus::Busy)).await;
    let turn = spawn_turn(&app, ctx("ses_test", "跑一下 CI"));
    wait_for_card_text(&platform, "已经交给后台了。").await;

    script_transcript(
        &backend,
        vec![
            SessionTranscript::new(timeline(vec![assistant(finished + 30_000, "子代理跑完了。")]))
                .with_executions(vec![execution(started + 30_000)])
                .with_wakes(vec![subagent_wake(finished, &long)]),
        ],
    )
    .await;
    wait_for_card_text(&platform, "子代理跑完了。").await;

    let last = platform.updated_cards().await.last().cloned().unwrap();
    let clipped = format!("🔔 子代理完成：{}…", long.chars().take(60).collect::<String>());
    assert!(
        card_text(&last).contains(&clipped),
        "the subagent's description is the collapsed title, clipped to 60 chars: {last}"
    );
    assert_eq!(
        card_text(&last).matches("子代理完成").count(),
        1,
        "exactly one entry: {last}"
    );
    // The clip holds for the entry itself: the launch panel below may still
    // carry the full description as its input, the title must not.
    let entry = body_index(&last, "子代理完成").expect("the entry renders");
    let element = &last["body"]["elements"][entry];
    assert_eq!(
        element["tag"], "collapsible_panel",
        "the entry is folded: {element}"
    );
    assert!(
        !element.to_string().contains(&long),
        "the full label must not leak past the clip: {element}"
    );
    assert!(
        card_text(&last).contains("子代理 ses_child · 14:02 · 12m"),
        "the fold names the child session and times: {last}"
    );

    // End the turn: the wake's retirement also releases the background task.
    backend
        .set_session_status("ses_test", Some(SessionStatus::Idle))
        .await;
    script_transcript(
        &backend,
        vec![
            SessionTranscript::new(timeline(vec![assistant(finished + 30_000, "子代理跑完了。")]))
                .with_executions(vec![execution(started + 30_000), execution(finished + 60_000)])
                .with_wakes(vec![subagent_wake(finished, &long)])
                .with_background_tasks(vec![background_shell(started)]),
        ],
    )
    .await;
    let result = tokio::time::timeout(Duration::from_secs(5), turn)
        .await
        .expect("the boundary ends the turn")
        .unwrap();
    result.unwrap();
}

/// The lobby/restart regression (#426, ADR-0066): a RESTART continuation card
/// is sent top-level — it has no reply target at all — and carries the 承接
/// line, which announces its own Wake (so no entry for it). When that card's
/// run yields 「⏳ 等待后台任务」, a later completion Wake resumes IT in place:
/// its entry lands there, before its work, exactly once — where the old split
/// path warned "no reachable reply target" on every pass and the resumed work
/// never rendered. A repeated poll must not duplicate the entry.
#[tokio::test]
async fn a_restart_cards_later_completion_resumes_it_in_place() {
    let _wd = test_work_dir();
    let (started, finished) = entry_span();
    // The first Wake's own work, then the second run's, each with an answered
    // boundary: the first read leaves the follow's task live, the second is the
    // resumed run's true end.
    let timeline = |first: bool, second: bool| {
        let mut resumed = Vec::new();
        if first {
            resumed.push(assistant(started + 120_000, "第一段进展。"));
        }
        if second {
            resumed.push(assistant(finished + 30_000, "第二段进展。"));
        }
        merged_entry_timeline(
            started,
            vec![background_shell_launch(
                started,
                "call_bg",
                "sh_bg",
                "gh run watch",
            )],
            resumed,
        )
    };
    // The restart fixture: no card chain in memory at all.
    let resumed = SessionTranscript::new(timeline(true, false))
        .with_executions(vec![execution(started + 30_000), execution(started + 180_000)])
        .with_wakes(vec![shell_wake(started + 90_000)])
        .with_background_tasks(vec![background_shell(started + 150_000)]);
    let (_dir, app, backend, platform) = scripted_app(vec![resumed], Some(SessionStatus::Idle)).await;
    assert!(
        !Turn::has_card(&app.cards_handle(), "ses_test").await,
        "the restart fixture has no card chain"
    );

    spawn_sync(&app);
    // The restart continuation posts top-level, streams the Wake's work and
    // yields 「⏳ 等待后台任务」 on the task its run left live.
    wait_for_card_update(
        &platform,
        "the restart continuation's waiting card",
        CardUpdates::Latest,
        |card| {
            card_header(card).contains("等待后台任务")
                && card_text(card).contains(WAKE_LEAD)
                && card_text(card).contains("第一段进展。")
        },
    )
    .await;
    let opened = platform.updated_cards().await.last().cloned().unwrap();
    assert!(
        !card_text(&opened).contains("后台任务完成"),
        "the Wake that opened the card is announced by the 承接 line alone: {opened}"
    );
    assert!(
        !Turn::card_is_owned(&app.cards_handle(), "ses_test").await,
        "the yielded card is nobody's to render until the next Wake"
    );
    let cards_posted = posted_cards(&platform).await;

    // The second completion resumes THAT card in place — no reply target
    // exists, and none is needed: the entry and the work land on the card the
    // chain already has.
    script_transcript(
        &backend,
        vec![
            SessionTranscript::new(timeline(true, true))
                .with_executions(vec![execution(started + 30_000), execution(finished + 60_000)])
                .with_wakes(vec![shell_wake(started + 90_000), shell_wake(finished)]),
        ],
    )
    .await;
    wait_for_card_update(
        &platform,
        "the resumed restart card's ending",
        CardUpdates::Latest,
        |card| card_header(card).contains("✅") && card_text(card).contains("第二段进展。"),
    )
    .await;

    assert_eq!(
        posted_cards(&platform).await,
        cards_posted,
        "the later completion posts nothing: the chain resumes its own card: {:?}",
        platform.calls.lock().await
    );
    let last = platform.updated_cards().await.last().cloned().unwrap();
    assert_eq!(
        card_text(&last).matches("后台任务完成").count(),
        1,
        "the resumed completion leaves exactly one entry: {last}"
    );
    assert!(
        card_text(&last).contains("shell sh_bg · 14:02 · 12m"),
        "the entry carries the retired task's identity and timing: {last}"
    );
    let entry = body_index(&last, "后台任务完成").expect("the resumed entry renders");
    let work = body_index(&last, "第二段进展。").expect("the resumed work renders");
    assert!(
        entry < work,
        "the entry precedes its work (entry@{entry}, work@{work}): {last}"
    );

    // Later polls must not duplicate it.
    tokio::time::sleep(Duration::from_millis(80)).await;
    let later = platform.updated_cards().await.last().cloned().unwrap();
    assert_eq!(
        card_text(&later).matches("后台任务完成").count(),
        1,
        "a repeated poll must not duplicate the entry: {later}"
    );
}

/// A Wake that is neither a shell nor a subagent completion (a restart notice,
/// an interruption continuation) keeps today's behavior: no entry.
#[tokio::test]
async fn a_restart_or_interrupt_wake_gets_no_entry() {
    let _wd = test_work_dir();
    let (started, finished) = entry_span();
    let live = SessionTranscript::new(merged_shell_timeline(started, vec![]))
        .with_executions(vec![execution(started + 30_000)])
        .with_background_tasks(vec![background_shell(started)]);
    let (_dir, app, backend, platform) = scripted_app(vec![live], Some(SessionStatus::Busy)).await;
    let turn = spawn_turn(&app, ctx("ses_test", "跑一下 CI"));
    wait_for_card_text(&platform, "已经交给后台了。").await;

    // A restart notice and an interruption continuation (the fixtures keep
    // their shell keys, the source is what the rule reads).
    let variants = || {
        let mut restart = shell_wake(finished);
        restart.source = WakeSource::Restart;
        let mut interrupt = shell_wake(finished + 1_000);
        interrupt.source = WakeSource::Interrupt;
        vec![restart, interrupt]
    };
    let resumed = |text: &str| merged_shell_timeline(started, vec![assistant(finished + 30_000, text)]);
    script_transcript(
        &backend,
        vec![
            SessionTranscript::new(resumed("重启后继续。"))
                .with_executions(vec![execution(started + 30_000)])
                .with_wakes(variants()),
        ],
    )
    .await;
    wait_for_card_text(&platform, "重启后继续。").await;

    let last = platform.updated_cards().await.last().cloned().unwrap();
    assert!(
        !card_text(&last).contains("后台任务完成") && !card_text(&last).contains("子代理完成"),
        "a restart/interrupt notice gets no entry: {last}"
    );

    // End the turn: its Execution idles after the non-completion Wakes.
    script_transcript(
        &backend,
        vec![
            SessionTranscript::new(resumed("重启后继续。"))
                .with_executions(vec![execution(started + 30_000), execution(finished + 60_000)])
                .with_wakes(variants()),
        ],
    )
    .await;
    backend
        .set_session_status("ses_test", Some(SessionStatus::Idle))
        .await;
    let result = tokio::time::timeout(Duration::from_secs(5), turn)
        .await
        .expect("the boundary ends the turn")
        .unwrap();
    result.unwrap();
}

/// An unnamed Wake (no label in its text) still leaves the entry: the bare
/// completion line as its title, identity and timing in the fold.
#[tokio::test]
async fn an_unnamed_merged_wake_renders_the_bare_title() {
    let _wd = test_work_dir();
    let (started, finished) = entry_span();
    let live = SessionTranscript::new(merged_shell_timeline(started, vec![]))
        .with_executions(vec![execution(started + 30_000)])
        .with_background_tasks(vec![background_shell(started)]);
    let (_dir, app, backend, platform) = scripted_app(vec![live], Some(SessionStatus::Busy)).await;
    let turn = spawn_turn(&app, ctx("ses_test", "跑一下 CI"));
    wait_for_card_text(&platform, "已经交给后台了。").await;

    let mut wake = shell_wake(finished);
    wake.label = None;
    script_transcript(
        &backend,
        vec![
            SessionTranscript::new(merged_shell_timeline(
                started,
                vec![assistant(finished + 30_000, "跑完了。")],
            ))
            .with_executions(vec![execution(started + 30_000)])
            .with_wakes(vec![wake.clone()]),
        ],
    )
    .await;
    wait_for_card_text(&platform, "跑完了。").await;

    let last = platform.updated_cards().await.last().cloned().unwrap();
    let text = card_text(&last);
    assert!(
        text.contains("🔔 后台任务完成"),
        "the unnamed Wake renders the bare completion line: {last}"
    );
    assert!(
        !text.contains("🔔 后台任务完成："),
        "a bare title carries no dangling label separator: {last}"
    );
    assert!(
        text.contains("shell sh_bg · 14:02 · 12m"),
        "identity and timing survive an unnamed completion: {last}"
    );
    assert_eq!(
        text.matches("后台任务完成").count(),
        1,
        "exactly one entry: {last}"
    );

    // End the turn.
    script_transcript(
        &backend,
        vec![
            SessionTranscript::new(merged_shell_timeline(
                started,
                vec![assistant(finished + 30_000, "跑完了。")],
            ))
            .with_executions(vec![execution(started + 30_000), execution(finished + 60_000)])
            .with_wakes(vec![wake]),
        ],
    )
    .await;
    backend
        .set_session_status("ses_test", Some(SessionStatus::Idle))
        .await;
    let result = tokio::time::timeout(Duration::from_secs(5), turn)
        .await
        .expect("the boundary ends the turn")
        .unwrap();
    result.unwrap();
}
