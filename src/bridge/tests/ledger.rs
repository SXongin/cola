//! The Background Task Ledger's live section (ADR-0060, spec #412 ticket
//! #415): a Session's live Background Tasks ride the newest (live) card's tail
//! like the Todo Panel and the live Tool Panels — one row per task, labelled
//! from the originating tool part's input by `call_id`, elapsed bare — and a
//! retirement leaves the section on the card's existing render cadence. The
//! launch panel itself (ticket #416) keeps its timeline place and reads
//! 「已转后台」 instead of ✅, before and after the retirement.
//!
//! The section follows the chain's newest card (ticket #418): when a Wake
//! opens a continuation card or a new Turn takes over a waiting card, the
//! handover PATCH leaves the section on the new card and removes it from the
//! old one — whose own completion entries stay behind, never migrating.
//!
//! V1 carries no Background Task facts, so its section never renders; the
//! tests here script the typed transcript reads (the read model already owns
//! the decode, see `opencode::v2::wire`).

use std::sync::Arc;
use std::time::Duration;

use super::drain::{assistant, ctx, script_transcript, scripted_app, spawn_sync, spawn_turn, user};
use crate::backend::{
    BackgroundTask, ContentBlock, FinishReason, MessageRole, Part, SessionTranscript, StepFinish, ToolCall,
    ToolIdentity, ToolOutput, ToolStatus, TranscriptMessage,
};
use crate::bridge::test_support::*;
use crate::bridge::turn::Turn;
use crate::feishu::card::CardState;
use crate::opencode::types::SessionStatus;

/// A live Background Task of the `shell` kind: the tool call that started it
/// and the shell it runs in.
fn live_shell(started_at: i64, call_id: &str) -> BackgroundTask {
    BackgroundTask {
        tool: ToolIdentity {
            name: "shell".into(),
            call_id: call_id.into(),
        },
        shell_id: Some(format!("sh_{call_id}")),
        child_id: None,
        started_at: Some(started_at),
    }
}

/// A live Background Task of the `subagent` kind, identified by its child
/// session.
fn live_subagent(started_at: i64, call_id: &str) -> BackgroundTask {
    BackgroundTask {
        tool: ToolIdentity {
            name: "subagent".into(),
            call_id: call_id.into(),
        },
        shell_id: None,
        child_id: Some(format!("ses_{call_id}")),
        started_at: Some(started_at),
    }
}

/// An assistant message whose settled tool call launched a backgrounded run:
/// the ledger joins the row's label from this part's `input`, by `call_id`.
fn background_launch(created: i64, name: &str, call_id: &str, input: serde_json::Value) -> TranscriptMessage {
    typed_message(
        &format!("msg_launch_{call_id}"),
        MessageRole::Assistant,
        Some(created),
        vec![
            Part::Tool(ToolCall {
                identity: ToolIdentity {
                    name: name.into(),
                    call_id: call_id.into(),
                },
                status: ToolStatus::Completed,
                started_at: Some(created),
                input: Some(input),
                // The background marker the run's own plugin writes: the call
                // returned the handle while the run is still going (the read
                // model's decode is what turns this into a Background Task).
                metadata: Some(serde_json::json!({ "status": "running" })),
                output: ToolOutput {
                    raw: None,
                    blocks: vec![ContentBlock::Text("moved to background".into())],
                    error: None,
                },
            }),
            Part::StepFinish(StepFinish {
                reason: FinishReason::ToolCalls,
            }),
        ],
    )
}

/// A settled FOREGROUND `shell` call — the control: its run finished inside
/// the call (metadata says `completed`), so its panel reads ✅ as today.
fn foreground_shell(created: i64, call_id: &str) -> TranscriptMessage {
    typed_message(
        &format!("msg_fg_{call_id}"),
        MessageRole::Assistant,
        Some(created),
        vec![
            Part::Tool(ToolCall {
                identity: ToolIdentity {
                    name: "shell".into(),
                    call_id: call_id.into(),
                },
                status: ToolStatus::Completed,
                started_at: Some(created),
                input: Some(serde_json::json!({ "command": "cargo test" })),
                metadata: Some(serde_json::json!({ "status": "completed" })),
                output: ToolOutput {
                    raw: None,
                    blocks: vec![ContentBlock::Text("test result: ok".into())],
                    error: None,
                },
            }),
            Part::StepFinish(StepFinish {
                reason: FinishReason::ToolCalls,
            }),
        ],
    )
}

/// A `todowrite` call: the Todo Panel's fixture, so the ledger's coexistence
/// with the other tail sections is pinned on one card.
fn todowrite(created: i64, call_id: &str) -> TranscriptMessage {
    let todos = serde_json::json!([{ "content": "列清单", "status": "in_progress", "priority": "high" }]);
    typed_message(
        &format!("msg_todo_{call_id}"),
        MessageRole::Assistant,
        Some(created),
        vec![
            Part::Tool(ToolCall {
                identity: ToolIdentity {
                    name: "todowrite".into(),
                    call_id: call_id.into(),
                },
                status: ToolStatus::Completed,
                started_at: Some(created),
                input: Some(serde_json::json!({ "todos": todos.clone() })),
                metadata: None,
                output: ToolOutput {
                    raw: None,
                    blocks: vec![ContentBlock::Text(todos.to_string())],
                    error: None,
                },
            }),
            Part::StepFinish(StepFinish {
                reason: FinishReason::ToolCalls,
            }),
        ],
    )
}

/// The elapsed tail of the row starting with `prefix` — the time after the
/// row's `· `. Panics when the row is missing (the section itself is the
/// test's first assertion).
fn ledger_elapsed(card: &serde_json::Value, prefix: &str) -> String {
    let text = card_text(card);
    let row = text
        .lines()
        .find(|line| line.starts_with(prefix))
        .unwrap_or_else(|| panic!("no ledger row {prefix:?}: {text}"));
    row[prefix.len()..].to_string()
}

/// The elapsed is the read model's own start against the build clock: bare,
/// no Chinese label (ADR-0060), in the `Xm Ys` / `Xh Ym` shape.
fn assert_elapsed_shaped(elapsed: &str) {
    let unit = elapsed.chars().last();
    assert!(
        matches!(unit, Some('s') | Some('m')),
        "the elapsed is bare and unit-suffixed: {elapsed:?}"
    );
    assert!(
        elapsed.chars().next().is_some_and(|c| c.is_ascii_digit()),
        "the elapsed starts with a number: {elapsed:?}"
    );
}

/// Acceptance 1: a live card with a live Background Task shows the section —
/// count plus one row, labelled from the tool part's input by `call_id`, with
/// the bare elapsed — while the card is still the live one.
#[tokio::test]
async fn one_live_task_rides_the_live_cards_tail() {
    let _wd = test_work_dir();
    let transcript = SessionTranscript::new(vec![
        user("msg_cola_anchor", 1_000, "跑一下构建"),
        background_launch(
            2_000,
            "shell",
            "call_bg",
            serde_json::json!({ "command": "npm run build" }),
        ),
    ])
    .with_background_tasks(vec![live_shell(2_100, "call_bg")]);
    let (_dir, app, _backend, platform) = scripted_app(vec![transcript], Some(SessionStatus::Busy)).await;

    let _turn = spawn_turn(&app, ctx("ses_test", "跑一下构建"));
    wait_for_card_update(&platform, "the live ledger", CardUpdates::Any, |card| {
        card_text(card).contains("⏳ 后台任务（1）") && card_text(card).contains("· shell：npm run build · ")
    })
    .await;

    let card = platform.updated_cards().await.last().cloned().unwrap();
    assert!(
        !card_header(&card).contains("完成"),
        "the ledger rides the LIVE card, not a settled one: {card}"
    );
    assert_elapsed_shaped(&ledger_elapsed(&card, "· shell：npm run build · "));
    // The launch's own panel keeps its timeline place beside the ledger row.
    assert!(
        card_text(&card).contains("moved to background"),
        "the launching call's panel stays on the card: {card}"
    );
}

/// Acceptance (#416): a backgrounded launch's panel reads 「已转后台」 instead
/// of ✅ — before AND after its run retires (the panel never claims completion;
/// the ledger's entry is the completion record) — while a foreground `shell`
/// call renders ✅ exactly as today.
#[tokio::test]
async fn a_backgrounded_launchs_panel_never_claims_completion() {
    let _wd = test_work_dir();
    let timeline = || {
        vec![
            user("msg_cola_anchor", 1_000, "跑一下构建并测试"),
            background_launch(
                2_000,
                "shell",
                "call_bg",
                serde_json::json!({ "command": "npm run build" }),
            ),
            foreground_shell(2_100, "call_fg"),
        ]
    };
    let with_task =
        SessionTranscript::new(timeline()).with_background_tasks(vec![live_shell(2_000, "call_bg")]);
    let retired = SessionTranscript::new(timeline());
    let (_dir, app, backend, platform) = scripted_app(vec![with_task], Some(SessionStatus::Busy)).await;

    let _turn = spawn_turn(&app, ctx("ses_test", "跑一下构建并测试"));
    // Before retirement: 「已转后台」 in the status slot while the run's row
    // rides the ledger.
    wait_for_card_update(&platform, "the backgrounded panel", CardUpdates::Any, |card| {
        let text = card_text(card);
        text.contains("已转后台 shell") && text.contains("⏳ 后台任务（1）")
    })
    .await;
    let card = platform.updated_cards().await.last().cloned().unwrap();
    let text = card_text(&card);
    assert_eq!(
        text.matches("已转后台 shell · ").count(),
        1,
        "the launch's panel carries the pinned copy exactly once: {text}"
    );
    assert_eq!(
        text.matches("✅ shell · ").count(),
        1,
        "only the foreground control claims completion: {text}"
    );

    // The run retires: the ledger's row leaves, the launch's panel keeps its
    // place and STILL reads 已转后台 — it never flips to ✅.
    script_transcript(&backend, vec![retired]).await;
    wait_for_card_update(
        &platform,
        "the retired launch's panel",
        CardUpdates::Latest,
        |card| {
            let text = card_text(card);
            !text.contains("后台任务") && text.contains("已转后台 shell")
        },
    )
    .await;
    let card = platform.updated_cards().await.last().cloned().unwrap();
    let text = card_text(&card);
    assert_eq!(
        text.matches("已转后台 shell · ").count(),
        1,
        "the retired launch keeps the marker: {text}"
    );
    assert_eq!(
        text.matches("✅ shell · ").count(),
        1,
        "the foreground control still claims completion: {text}"
    );
}

/// Acceptance 4: several tasks list together in transcript order, coexisting
/// with the Todo Panel and a live Tool Panel instead of replacing them.
#[tokio::test]
async fn several_tasks_list_in_transcript_order_beside_the_other_tail_sections() {
    let _wd = test_work_dir();
    let transcript = SessionTranscript::new(vec![
        user("msg_cola_anchor", 1_000, "跑一下构建并审阅"),
        todowrite(1_200, "call_todo"),
        background_launch(
            2_000,
            "shell",
            "call_bg",
            serde_json::json!({ "command": "npm run build" }),
        ),
        background_launch(
            2_100,
            "subagent",
            "call_sub",
            serde_json::json!({ "description": "review the diff" }),
        ),
        typed_message(
            "msg_live_tool",
            MessageRole::Assistant,
            Some(2_200),
            vec![tool_part(
                "bash",
                "call_live",
                ToolStatus::Running,
                serde_json::json!({ "command": "sleep 600" }),
                "",
            )],
        ),
    ])
    .with_background_tasks(vec![
        live_shell(2_000, "call_bg"),
        live_subagent(2_100, "call_sub"),
    ]);
    let (_dir, app, _backend, platform) = scripted_app(vec![transcript], Some(SessionStatus::Busy)).await;

    let _turn = spawn_turn(&app, ctx("ses_test", "跑一下构建并审阅"));
    wait_for_card_update(&platform, "the full tail", CardUpdates::Any, |card| {
        let text = card_text(card);
        text.contains("⏳ 后台任务（2）")
            && text.contains("· shell：npm run build")
            && text.contains("· 子代理：review the diff")
            && text.contains("todowrite")
    })
    .await;

    let card = platform.updated_cards().await.last().cloned().unwrap();
    let text = card_text(&card);
    assert_eq!(
        text.matches("⏳ 后台任务（2）").count(),
        1,
        "exactly one ledger section: {text}"
    );
    let shell_row = text.find("· shell：npm run build").expect("the shell row");
    let sub_row = text.find("· 子代理：review the diff").expect("the subagent row");
    assert!(
        shell_row < sub_row,
        "rows keep transcript order (shell before subagent): {text}"
    );
    assert!(
        text.contains("todowrite"),
        "the Todo Panel coexists with the ledger: {text}"
    );
    assert!(
        text.contains("⏳ bash"),
        "the live Tool Panel coexists with the ledger: {text}"
    );
    assert_elapsed_shaped(&ledger_elapsed(&card, "· shell：npm run build · "));
    assert_elapsed_shaped(&ledger_elapsed(&card, "· 子代理：review the diff · "));
}

/// Acceptance 2 + 3: the label joins by `call_id` — a shell's `description`
/// stands in for a missing `command`, a subagent reads its `description`, a
/// task whose input names no label renders bare, and a long label clips.
#[tokio::test]
async fn labels_join_by_call_id_and_clip_like_the_entry_title() {
    let _wd = test_work_dir();
    let long = "very long command ".repeat(8);
    let transcript = SessionTranscript::new(vec![
        user("msg_cola_anchor", 1_000, "跑一下"),
        background_launch(
            2_000,
            "shell",
            "call_desc",
            serde_json::json!({ "description": "watch the CI" }),
        ),
        background_launch(
            2_100,
            "subagent",
            "call_sub",
            serde_json::json!({ "description": "review the diff" }),
        ),
        background_launch(2_200, "shell", "call_bare", serde_json::json!({ "timeout": 30 })),
        background_launch(
            2_300,
            "shell",
            "call_long",
            serde_json::json!({ "command": long }),
        ),
    ])
    .with_background_tasks(vec![
        live_shell(2_000, "call_desc"),
        live_subagent(2_100, "call_sub"),
        live_shell(2_200, "call_bare"),
        live_shell(2_300, "call_long"),
    ]);
    let (_dir, app, _backend, platform) = scripted_app(vec![transcript], Some(SessionStatus::Busy)).await;

    let _turn = spawn_turn(&app, ctx("ses_test", "跑一下"));
    wait_for_card_update(&platform, "the labelled rows", CardUpdates::Any, |card| {
        card_text(card).contains("⏳ 后台任务（4）")
    })
    .await;

    let card = platform.updated_cards().await.last().cloned().unwrap();
    let text = card_text(&card);
    assert!(
        text.contains("· shell：watch the CI · "),
        "a shell falls back to its description: {text}"
    );
    assert!(
        text.contains("· 子代理：review the diff · "),
        "the subagent arm reads its description: {text}"
    );
    assert!(
        text.contains("· shell · "),
        "a task whose input names no label renders bare: {text}"
    );
    assert!(
        !text.contains("· shell：very long command very long command very long command very long command"),
        "a long label must clip like the completion entry's title in the ROW: {text}"
    );
    let clipped = text
        .lines()
        .find(|line| line.starts_with("· shell：very long command"))
        .expect("the clipped row");
    assert!(
        clipped.contains('…') && !clipped.contains(long.as_str()),
        "the clip marker renders and the full command is cut: {clipped}"
    );
}

/// Acceptance 3 + 6: a new task appears on the live card and a retirement
/// removes its row within the existing cadence — even out of start order — and
/// the last retirement drops the whole section.
#[tokio::test]
async fn a_task_appears_and_retires_on_the_live_card_in_cadence() {
    let _wd = test_work_dir();
    let launch_only = SessionTranscript::new(vec![
        user("msg_cola_anchor", 1_000, "跑一下构建并审阅"),
        background_launch(
            2_000,
            "shell",
            "call_bg",
            serde_json::json!({ "command": "npm run build" }),
        ),
        background_launch(
            2_100,
            "subagent",
            "call_sub",
            serde_json::json!({ "description": "review the diff" }),
        ),
    ]);
    let both = launch_only.clone().with_background_tasks(vec![
        live_shell(2_000, "call_bg"),
        live_subagent(2_100, "call_sub"),
    ]);
    let retired = SessionTranscript::new(vec![
        user("msg_cola_anchor", 1_000, "跑一下构建并审阅"),
        background_launch(
            2_000,
            "shell",
            "call_bg",
            serde_json::json!({ "command": "npm run build" }),
        ),
    ])
    .with_background_tasks(vec![live_shell(2_000, "call_bg")]);
    let quiet = SessionTranscript::new(vec![user("msg_cola_anchor", 1_000, "跑一下构建并审阅")]);
    let (_dir, app, backend, platform) = scripted_app(vec![launch_only], Some(SessionStatus::Busy)).await;
    let _turn = spawn_turn(&app, ctx("ses_test", "跑一下构建并审阅"));
    // The launches are already on the card, but the read reports no live task:
    // nothing renders the section.
    wait_for_card_update(&platform, "the launch panels", CardUpdates::Latest, |card| {
        card_text(card).contains("moved to background")
    })
    .await;
    assert!(
        !platform
            .updated_cards()
            .await
            .last()
            .is_some_and(|card| card_text(card).contains("后台任务")),
        "no live task, no section"
    );

    // A live task lands with no new part: the ledger alone must owe the flush.
    script_transcript(&backend, vec![both]).await;
    wait_for_card_update(&platform, "both rows", CardUpdates::Latest, |card| {
        card_text(card).contains("⏳ 后台任务（2）")
    })
    .await;

    // The SUBAGENT retires first although the shell started earlier: the row
    // leaves, the shell's row keeps transcript order and stays.
    script_transcript(&backend, vec![retired]).await;
    wait_for_card_update(
        &platform,
        "the subagent's row gone",
        CardUpdates::Latest,
        |card| {
            let text = card_text(card);
            text.contains("⏳ 后台任务（1）")
                && text.contains("· shell：npm run build")
                && !text.contains("子代理")
        },
    )
    .await;

    // The last retirement drops the section entirely.
    script_transcript(&backend, vec![quiet]).await;
    wait_for_card_update(&platform, "no live tasks left", CardUpdates::Latest, |card| {
        !card_text(card).contains("后台任务")
    })
    .await;
    let card = platform.updated_cards().await.last().cloned().unwrap();
    assert!(
        !card_text(&card).contains("后台任务"),
        "an empty ledger renders nothing: {card}"
    );
}

/// Acceptance 5: V1 carries no Background Task facts — a V1 `task` call that
/// looks backgrounded renders its panel and never a ledger section.
#[tokio::test]
async fn a_v1_task_part_is_never_a_ledger_row() {
    let _wd = test_work_dir();
    let transcript = SessionTranscript::new(vec![
        user("msg_cola_anchor", 1_000, "跑一下"),
        // V1's `task` tool is a different generation: its parts carry no
        // background marker the V2 decode would count (the `background` input
        // flag alone is deliberately not enough), so the typed read has no
        // Background Task.
        background_launch(
            2_000,
            "task",
            "call_v1",
            serde_json::json!({ "background": true }),
        ),
    ]);
    let (_dir, app, _backend, platform) = scripted_app(vec![transcript], Some(SessionStatus::Busy)).await;

    let _turn = spawn_turn(&app, ctx("ses_test", "跑一下"));
    wait_for_card_update(&platform, "the V1 panel", CardUpdates::Any, |card| {
        card_text(card).contains("task")
    })
    .await;

    let card = platform.updated_cards().await.last().cloned().unwrap();
    assert!(
        !card_text(&card).contains("后台任务"),
        "V1 has no Background Task facts; the section never renders: {card}"
    );
    // The plain tool panel still renders — only the ledger section is absent.
    assert!(
        card_text(&card).contains("moved to background"),
        "the V1 panel itself is unchanged: {card}"
    );
}

/// The two-task timeline both handover causes share: the anchor prompt, the
/// hand-off line and the two settled launch panels — the shell the handover
/// retires and the subagent that stays live across it, so both sides of one
/// handover are observable on one card. `resumed` appends the read's own later
/// content (the Wake's work, or the next user message).
fn two_task_timeline(resumed: Vec<TranscriptMessage>) -> Vec<TranscriptMessage> {
    let mut messages = vec![
        user("msg_cola_anchor", 1_000, "跑一下构建并审阅"),
        assistant(2_000, "已经交给后台了。"),
        background_launch(
            2_000,
            "shell",
            "call_bg",
            serde_json::json!({ "command": "gh run watch" }),
        ),
        background_launch(
            2_100,
            "subagent",
            "call_sub",
            serde_json::json!({ "description": "review the diff" }),
        ),
    ];
    messages.extend(resumed);
    messages
}

/// The waiting read: both tasks live, the Execution idled at 2_500.
fn two_task_waiting() -> SessionTranscript {
    SessionTranscript::new(two_task_timeline(vec![]))
        .with_executions(vec![execution(2_500)])
        .with_background_tasks(vec![background_shell(2_000), live_subagent(2_100, "call_sub")])
}

/// Run the two-task turn to its waiting yield — the state each handover test
/// starts from — and name the yielded card `om_waiting`, so its handover PATCH
/// can be told apart from the continuation's own updates (the harness replies
/// every card with one id).
async fn yield_the_two_task_card(app: &Arc<App>, platform: &RecordingPlatform) {
    Turn::run(&app.turn_handles(), ctx("ses_test", "跑一下构建并审阅"))
        .await
        .unwrap();
    assert_eq!(
        Turn::card_state(&app.cards_handle(), "ses_test").await,
        Some(CardState::Waiting),
        "idle with live tasks yields waiting"
    );
    let yielded = platform.updated_cards().await.last().cloned().unwrap();
    assert!(
        card_text(&yielded).contains("⏳ 后台任务（2）"),
        "the waiting card carries both live tasks: {yielded}"
    );
    Turn::set_card_message_id(&app.cards_handle(), "ses_test", "om_waiting").await;
}

/// Handover cause 1 (#418): a Wake continuation takes the live list to the new
/// card. The waiting card's handover PATCH loses the section and gains the
/// retired task's fixed completion entry — the entry stays on the card that
/// hosted the task — while the continuation opens with its 承接 line and lists
/// only the REMAINING task. The entry never migrates onto the continuation.
#[tokio::test]
async fn a_wake_continuation_hands_the_live_list_to_the_new_card() {
    let _wd = test_work_dir();
    let (_dir, app, backend, platform) =
        scripted_app(vec![two_task_waiting()], Some(SessionStatus::Idle)).await;
    yield_the_two_task_card(&app, &platform).await;

    // The shell retires (its Wake at 2_900) while the subagent stays live: the
    // chain continues on a new card below the user's message.
    script_transcript(
        &backend,
        vec![
            SessionTranscript::new(two_task_timeline(vec![assistant(3_100, "CI 通过了。")]))
                .with_executions(vec![execution(2_500), execution(4_000)])
                .with_wakes(vec![shell_wake(2_900)])
                .with_background_tasks(vec![live_subagent(2_100, "call_sub")]),
        ],
    )
    .await;
    spawn_sync(&app);
    wait_for_card_update(
        &platform,
        "the continuation's remaining list",
        CardUpdates::Latest,
        |card| {
            let text = card_text(card);
            text.contains(WAKE_LEAD)
                && text.contains("⏳ 后台任务（1）")
                && text.contains("· 子代理：review the diff")
        },
    )
    .await;

    // The continuation's very FIRST payload already carries the remaining
    // list: the handover wrote the read's live set before the split, so the
    // retired shell's row never appears there, not even briefly.
    let first = platform
        .replied_cards()
        .await
        .into_iter()
        .find(|card| card_text(card).contains(WAKE_LEAD))
        .expect("the continuation card is replied to the user's message");
    let first_text = card_text(&first);
    assert!(
        first_text.contains("⏳ 后台任务（1）") && !first_text.contains("后台任务（2）"),
        "the continuation opens with the remaining list, never the retired row: {first}"
    );

    // The newest card carries the remaining task only: the retired shell's row
    // is gone and its completion entry did not migrate onto the continuation.
    let continuation = platform.updated_cards().await.last().cloned().unwrap();
    let text = card_text(&continuation);
    assert!(
        !text.contains("· shell：gh run watch"),
        "the retired task's live row leaves the continuation: {continuation}"
    );
    assert!(
        !text.contains("后台任务完成"),
        "the completion entry stays on the card the task lived on: {continuation}"
    );

    // The outgoing card's handover PATCH: the live list is gone, the entry and
    // the card's own facts (timeline, footer, handoff header) stay.
    let patches = patches_to(&platform, "om_waiting").await;
    assert_eq!(
        patches.len(),
        1,
        "the handover is the old card's last PATCH: {patches:?}"
    );
    let handover = &patches[0];
    assert!(
        !card_text(handover).contains("后台任务（"),
        "the old card shows no live list after the handover: {handover}"
    );
    assert!(
        card_text(handover).contains("🔔 后台任务完成：gh run watch"),
        "the retired task's entry lands on the card that hosted it: {handover}"
    );
    assert!(
        card_text(handover).contains("shell sh_bg · "),
        "the entry's fold body carries the task's identity: {handover}"
    );
    assert!(
        card_text(handover).contains("已经交给后台了。"),
        "the handover PATCH keeps the old card's timeline: {handover}"
    );
    assert!(
        card_text(handover).contains("📁"),
        "the handover PATCH keeps the Turn Footer: {handover}"
    );
    assert!(
        card_header(handover).contains("继续中"),
        "the waiting yield hands over with the standard header: {handover}"
    );
}

/// Acceptance 3 (#418), the several-completions case: both tasks retire in one
/// read — the shell's Wake at 2_900, the subagent's at 3_500 — so each Wake
/// leaves its own fixed entry on the card that hosted it, in Wake order, and
/// the continuation, opening with the newest Wake's 承接 line, renders neither.
#[tokio::test]
async fn a_continuation_never_renders_an_entry_from_an_earlier_card() {
    let _wd = test_work_dir();
    let (_dir, app, backend, platform) =
        scripted_app(vec![two_task_waiting()], Some(SessionStatus::Idle)).await;
    yield_the_two_task_card(&app, &platform).await;

    // Both tasks complete while the card waits, with the resumed work landing
    // after both: the handover owes the old card BOTH entries and the
    // continuation only its 承接 line.
    script_transcript(
        &backend,
        vec![
            SessionTranscript::new(two_task_timeline(vec![assistant(3_600, "都完成了。")]))
                .with_executions(vec![execution(2_500), execution(4_000)])
                .with_wakes(vec![shell_wake(2_900), subagent_wake(3_500, "review the diff")]),
        ],
    )
    .await;
    spawn_sync(&app);
    wait_for_card_update(
        &platform,
        "the continuation's done card",
        CardUpdates::Latest,
        |card| {
            card_header(card).contains("✅")
                && card_text(card).contains(WAKE_LEAD)
                && card_text(card).contains("都完成了。")
        },
    )
    .await;

    // The newest card renders no entry of its own and no live list: the
    // completions belong to the card they happened on.
    let continuation = platform.updated_cards().await.last().cloned().unwrap();
    let text = card_text(&continuation);
    assert!(
        !text.contains("后台任务完成") && !text.contains("子代理完成"),
        "no entry migrates onto the continuation: {continuation}"
    );
    assert!(
        !text.contains("后台任务（"),
        "no live list rides a session with nothing live: {continuation}"
    );

    // The old card's one handover PATCH carries both entries in Wake order.
    let patches = patches_to(&platform, "om_waiting").await;
    assert_eq!(
        patches.len(),
        1,
        "the handover is the old card's last PATCH: {patches:?}"
    );
    let handover = &patches[0];
    let text = card_text(handover);
    assert!(
        !text.contains("后台任务（"),
        "the old card shows no live list after the handover: {handover}"
    );
    let shell = text
        .find("🔔 后台任务完成：gh run watch")
        .unwrap_or_else(|| panic!("the shell's entry lands where the task lived: {handover}"));
    let subagent = text
        .find("🔔 子代理完成：review the diff")
        .unwrap_or_else(|| panic!("the subagent's entry lands where the task lived: {handover}"));
    assert!(
        shell < subagent,
        "the entries keep Wake order (shell before subagent): {handover}"
    );
}

/// Handover cause 2 (#418): a new Turn supersedes a waiting card. The collect
/// PATCH removes the live list from the old card — keeping its collected
/// header, timeline and footer — and the new Turn's own card carries the
/// remaining list, the retired task's row gone.
#[tokio::test]
async fn a_new_turn_takes_the_live_list_over_from_the_waiting_card() {
    let _wd = test_work_dir();
    let (_dir, app, backend, platform) =
        scripted_app(vec![two_task_waiting()], Some(SessionStatus::Idle)).await;
    yield_the_two_task_card(&app, &platform).await;

    // The user posts again: the new Turn collects the waiting card before it
    // starts, and its own read lists only the still-live subagent.
    script_transcript(
        &backend,
        vec![
            SessionTranscript::new(two_task_timeline(vec![user("msg_cola_next", 3_000, "新问题")]))
                .with_executions(vec![execution(2_500)])
                .with_background_tasks(vec![live_subagent(2_100, "call_sub")]),
        ],
    )
    .await;
    let mut second = ctx("ses_test", "新问题");
    second.message_id = "msg_next".into();
    second.cola_message_id = Some("msg_cola_next".into());
    Turn::run(&app.turn_handles(), second).await.unwrap();

    // The collect PATCH: the list is gone, the collected state and the card's
    // own content stay. (The collect is the old card's last PATCH.)
    let patches = patches_to(&platform, "om_waiting").await;
    assert_eq!(
        patches.len(),
        1,
        "the collect is the waiting card's only handover PATCH: {patches:?}"
    );
    let collected = &patches[0];
    assert!(
        !card_text(collected).contains("后台任务（"),
        "the superseded card hands its live list over: {collected}"
    );
    assert!(
        card_header(collected).contains("已由新消息接管"),
        "the collect keeps its own header: {collected}"
    );
    assert!(
        card_text(collected).contains("已经交给后台了。"),
        "the collect keeps the turn's timeline: {collected}"
    );
    assert!(
        card_text(collected).contains("📁"),
        "the collect keeps the Turn Footer: {collected}"
    );

    // The new Turn's own card carries the remaining list.
    wait_for_card_update(
        &platform,
        "the new Turn's remaining list",
        CardUpdates::Latest,
        |card| {
            let text = card_text(card);
            text.contains("⏳ 后台任务（1）") && text.contains("· 子代理：review the diff")
        },
    )
    .await;
    let live = platform.updated_cards().await.last().cloned().unwrap();
    assert!(
        !card_text(&live).contains("· shell：gh run watch"),
        "the retired task's row does not migrate onto the new Turn's card: {live}"
    );
}

// ---------------------------------------------------------------------------
// The yielded card's ledger stays fresh (ADR-0060, ticket #419): a Waiting
// card has no render loop of its own, so the existing Session Sync reads drive
// its ledger in place — a quiet retirement's row leaves and its entry arrives,
// elapsed moves when a whole minute turns, and an unchanged read PATCHes
// nothing. The carve-out is the ledger alone: the rest of the yielded card
// stays frozen.
// ---------------------------------------------------------------------------

/// The one-task waiting read the in-place tests yield from: the timeline's
/// shell launched at `started_at`. A start near the test's own clock keeps the
/// row's elapsed minute far from turning, so a short test window cannot cross
/// it by accident.
fn waiting_shell(started_at: i64) -> SessionTranscript {
    SessionTranscript::new(two_task_timeline(vec![]))
        .with_executions(vec![execution(2_500)])
        .with_background_tasks(vec![live_shell(started_at, "call_bg")])
}

/// The same read with the subagent still live beside the shell: the quiet
/// retirement retires the shell only, so the card stays Waiting (the LAST
/// retirement's settle belongs to a different ticket).
fn waiting_shell_and_subagent(shell_started_at: i64, subagent_started_at: i64) -> SessionTranscript {
    SessionTranscript::new(two_task_timeline(vec![]))
        .with_executions(vec![execution(2_500)])
        .with_background_tasks(vec![
            live_shell(shell_started_at, "call_bg"),
            live_subagent(subagent_started_at, "call_sub"),
        ])
}

/// Run the turn on `read` to its waiting yield and name the yielded card
/// `om_waiting` (the harness replies every card with one id), so the ledger
/// PATCHes the in-place pass adds can be told apart from every other card's.
async fn yield_waiting_card(app: &Arc<App>, platform: &RecordingPlatform, live_tasks: usize) {
    Turn::run(&app.turn_handles(), ctx("ses_test", "跑一下构建并审阅"))
        .await
        .unwrap();
    assert_eq!(
        Turn::card_state(&app.cards_handle(), "ses_test").await,
        Some(CardState::Waiting),
        "idle with live tasks yields waiting"
    );
    let yielded = platform.updated_cards().await.last().cloned().unwrap();
    assert!(
        card_text(&yielded).contains(&format!("⏳ 后台任务（{live_tasks}）")),
        "the waiting card carries its live tasks: {yielded}"
    );
    Turn::set_card_message_id(&app.cards_handle(), "ses_test", "om_waiting").await;
}

/// Every card CREATED (a reply or a top-level send), in call order — the
/// counter for "was a new card posted?" (an in-place PATCH updates one).
async fn created_cards(platform: &RecordingPlatform) -> Vec<serde_json::Value> {
    platform
        .calls
        .lock()
        .await
        .iter()
        .filter_map(|call| match call {
            PlatformCall::ReplyCard { card, .. } | PlatformCall::SendCard { card, .. } => Some(card.clone()),
            _ => None,
        })
        .collect()
}

/// Acceptance 1 (#419): a quiet retirement — the shell's Wake with no resumed
/// work, so no continuation is owed — updates the WAITING card in place: the
/// retired row leaves, its fixed completion entry arrives on the same card,
/// and nothing is posted. The card's non-ledger content (waiting header,
/// timeline, footer) is untouched.
#[tokio::test]
async fn a_quiet_retirement_updates_the_waiting_card_in_place() {
    let _wd = test_work_dir();
    let now = chrono::Utc::now().timestamp_millis();
    let sub_started = now - 4_000;
    let live = waiting_shell_and_subagent(now - 5_000, sub_started);
    let (_dir, app, backend, platform) = scripted_app(vec![live], Some(SessionStatus::Idle)).await;
    yield_waiting_card(&app, &platform, 2).await;
    let cards_before = created_cards(&platform).await.len();

    // The shell retires and its run resumes nothing; the subagent stays live,
    // so the card keeps a ledger but owes no continuation.
    script_transcript(
        &backend,
        vec![
            SessionTranscript::new(two_task_timeline(vec![]))
                .with_executions(vec![execution(2_500)])
                .with_wakes(vec![shell_wake(2_900)])
                .with_background_tasks(vec![live_subagent(sub_started, "call_sub")]),
        ],
    )
    .await;
    spawn_sync(&app);
    wait_for_card_update(&platform, "the quiet retirement", CardUpdates::Latest, |card| {
        card_text(card).contains("🔔 后台任务完成：gh run watch")
            && !card_text(card).contains("· shell：gh run watch")
    })
    .await;

    // Exactly ONE in-place PATCH carries both facts: the row left and the
    // entry arrived, with the remaining list riding along.
    let patches = patches_to(&platform, "om_waiting").await;
    assert_eq!(
        patches.len(),
        1,
        "the retirement is one in-place PATCH: {patches:?}"
    );
    let updated = &patches[0];
    let text = card_text(updated);
    assert!(
        !text.contains("· shell：gh run watch"),
        "the retired task's row leaves: {updated}"
    );
    assert!(
        text.contains("⏳ 后台任务（1）") && text.contains("· 子代理：review the diff"),
        "the remaining list rides the same card: {updated}"
    );
    assert!(
        text.contains("🔔 后台任务完成：gh run watch") && text.contains("shell sh_bg · "),
        "the fixed entry lands on the card the task lived on: {updated}"
    );
    assert!(
        card_header(updated).contains("等待后台任务"),
        "the waiting header is untouched: {updated}"
    );
    assert!(
        text.contains("已经交给后台了。"),
        "the card's timeline is untouched: {updated}"
    );
    assert!(text.contains("📁"), "the Turn Footer is untouched: {updated}");

    // Nothing was posted: no continuation, no new card at all.
    assert!(
        continuation_sends(&platform).await.is_empty(),
        "a quiet retirement owes no continuation: {:?}",
        platform.calls.lock().await
    );
    assert_eq!(
        created_cards(&platform).await.len(),
        cards_before,
        "no new card is posted for a ledger update"
    );
    assert_eq!(
        Turn::card_state(&app.cards_handle(), "ses_test").await,
        Some(CardState::Waiting),
        "the card is still waiting"
    );
}

/// Acceptance 2 (#419): an unchanged read PATCHes nothing — the Session Sync
/// passes run on the injected cadence, but the ledger did not move, so the
/// yielded card keeps the exact bytes the yield left. The control at the end
/// proves the loop was alive for the whole unchanged window: a read that does
/// move the ledger PATCHes immediately after it.
#[tokio::test]
async fn an_unchanged_read_leaves_the_waiting_card_unpatched() {
    let _wd = test_work_dir();
    let now = chrono::Utc::now().timestamp_millis();
    let sub_started = now - 4_000;
    let live = waiting_shell_and_subagent(now - 5_000, sub_started);
    let (_dir, app, backend, platform) = scripted_app(vec![live], Some(SessionStatus::Idle)).await;
    yield_waiting_card(&app, &platform, 2).await;
    let yielded = platform.updated_cards().await.last().cloned().unwrap();
    let elapsed = ledger_elapsed(&yielded, "· shell：gh run watch · ");
    let cards_before = created_cards(&platform).await.len();

    // Many passes at the injected cadence (20 ms each): the ledger is
    // unchanged, so no PATCH and no card.
    spawn_sync(&app);
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(
        patches_to(&platform, "om_waiting").await.is_empty(),
        "an unchanged read must not PATCH the yielded card: {:?}",
        platform.updated_cards().await
    );
    assert_eq!(
        created_cards(&platform).await.len(),
        cards_before,
        "an unchanged read posts nothing"
    );
    let last = platform.updated_cards().await.last().cloned().unwrap();
    assert_eq!(
        ledger_elapsed(&last, "· shell：gh run watch · "),
        elapsed,
        "the frozen card still reads the yield's elapsed: {elapsed:?}"
    );

    // Control: the same loop is listening — a read that changes the ledger
    // PATCHes right away.
    script_transcript(
        &backend,
        vec![
            SessionTranscript::new(two_task_timeline(vec![]))
                .with_executions(vec![execution(2_500)])
                .with_wakes(vec![shell_wake(2_900)])
                .with_background_tasks(vec![live_subagent(sub_started, "call_sub")]),
        ],
    )
    .await;
    wait_for_card_update(&platform, "the read that moved", CardUpdates::Latest, |card| {
        !card_text(card).contains("· shell：gh run watch")
    })
    .await;
}

/// Acceptance 2 (#419): elapsed refresh is minute-granular and driven by the
/// reads. The seconds inside a minute never owe a PATCH (however many reads
/// land), the read that crosses a whole minute PATCHes exactly once with the
/// new minute — and nothing more follows inside that minute.
#[tokio::test]
async fn a_waiting_cards_elapsed_refreshes_on_the_minute() {
    let _wd = test_work_dir();
    let now = chrono::Utc::now().timestamp_millis();
    let live = waiting_shell(now - 5_000);
    let (_dir, app, backend, platform) = scripted_app(vec![live], Some(SessionStatus::Idle)).await;
    yield_waiting_card(&app, &platform, 1).await;

    // A read whose row's elapsed is seconds short of the whole minute: the
    // start moved, so this membership change PATCHes once, deterministically
    // before the boundary.
    let started = chrono::Utc::now().timestamp_millis() - 58_000;
    let near_boundary = waiting_shell(started);
    script_transcript(&backend, vec![near_boundary]).await;
    spawn_sync(&app);
    wait_for_card_update(&platform, "the moved start", CardUpdates::Latest, |card| {
        ledger_elapsed(card, "· shell：gh run watch · ").starts_with("0m5")
    })
    .await;

    // The minute turns over: one more PATCH with the new elapsed, and that is
    // all — if the refresh were second-granular, every read in between would
    // have PATCHed too.
    wait_for_card_update(&platform, "the elapsed minute", CardUpdates::Latest, |card| {
        ledger_elapsed(card, "· shell：gh run watch · ").starts_with("1m")
    })
    .await;
    assert_eq!(
        patches_to(&platform, "om_waiting").await.len(),
        2,
        "one PATCH for the start move and one for the minute turn: {:?}",
        platform.updated_cards().await
    );
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(
        patches_to(&platform, "om_waiting").await.len(),
        2,
        "the seconds inside the new minute do not PATCH: {:?}",
        platform.updated_cards().await
    );
}

/// The retire-then-takeover race (#418 review): a completion that retires
/// between reads is owned by the in-place pass, and the later new-Turn collect
/// — which drops the live list — leaves the entry on the card it happened on.
#[tokio::test]
async fn a_quiet_retirement_survives_a_later_takeover() {
    let _wd = test_work_dir();
    let now = chrono::Utc::now().timestamp_millis();
    let sub_started = now - 4_000;
    let live = waiting_shell_and_subagent(now - 5_000, sub_started);
    let (_dir, app, backend, platform) = scripted_app(vec![live], Some(SessionStatus::Idle)).await;
    yield_waiting_card(&app, &platform, 2).await;

    // The shell retires quietly: the waiting card updates in place, its entry
    // landing where the task lived.
    script_transcript(
        &backend,
        vec![
            SessionTranscript::new(two_task_timeline(vec![]))
                .with_executions(vec![execution(2_500)])
                .with_wakes(vec![shell_wake(2_900)])
                .with_background_tasks(vec![live_subagent(sub_started, "call_sub")]),
        ],
    )
    .await;
    spawn_sync(&app);
    wait_for_card_update(&platform, "the quiet retirement", CardUpdates::Latest, |card| {
        card_text(card).contains("🔔 后台任务完成：gh run watch")
    })
    .await;

    // The user posts again: the new Turn collects the waiting card, dropping
    // its live list — the entry stays.
    script_transcript(
        &backend,
        vec![
            SessionTranscript::new(two_task_timeline(vec![user("msg_cola_next", 3_000, "新问题")]))
                .with_executions(vec![execution(2_500)])
                .with_background_tasks(vec![live_subagent(sub_started, "call_sub")]),
        ],
    )
    .await;
    let mut second = ctx("ses_test", "新问题");
    second.message_id = "msg_next".into();
    second.cola_message_id = Some("msg_cola_next".into());
    Turn::run(&app.turn_handles(), second).await.unwrap();

    let patches = patches_to(&platform, "om_waiting").await;
    assert_eq!(
        patches.len(),
        2,
        "the retirement and the collect are the waiting card's PATCHes: {patches:?}"
    );
    let collected = &patches[1];
    assert!(
        !card_text(collected).contains("后台任务（"),
        "the collect drops the live list: {collected}"
    );
    assert!(
        card_text(collected).contains("🔔 后台任务完成：gh run watch"),
        "the entry stays on the card it happened on: {collected}"
    );
    assert!(
        card_header(collected).contains("已由新消息接管"),
        "the collect keeps its own header: {collected}"
    );
}

/// The ledger's elapsed is rendered at card build time: a task whose start
/// clock is ahead (server skew) clamps to `0m00s`, so a row never renders a
/// negative age.
#[tokio::test]
async fn a_future_start_time_renders_zero_elapsed() {
    let _wd = test_work_dir();
    let now = chrono::Utc::now().timestamp_millis();
    let transcript = SessionTranscript::new(vec![
        user("msg_cola_anchor", 1_000, "跑一下"),
        background_launch(
            2_000,
            "shell",
            "call_bg",
            serde_json::json!({ "command": "npm run build" }),
        ),
    ])
    .with_background_tasks(vec![live_shell(now + 3_600_000, "call_bg")]);
    let (_dir, app, _backend, platform) = scripted_app(vec![transcript], Some(SessionStatus::Busy)).await;

    let _turn = spawn_turn(&app, ctx("ses_test", "跑一下"));
    wait_for_card_update(&platform, "the clamped elapsed", CardUpdates::Any, |card| {
        card_text(card).contains("· shell：npm run build · 0m00s")
    })
    .await;
    // A few more renders of the SAME task must keep rendering the same
    // clamped row, not a drifting one.
    tokio::time::sleep(Duration::from_millis(20)).await;
    let card = platform.updated_cards().await.last().cloned().unwrap();
    assert_eq!(
        ledger_elapsed(&card, "· shell：npm run build · "),
        "0m00s",
        "a skewed clock clamps to zero, it does not drift: {card}"
    );
}
