//! The Background Task Ledger's live section (ADR-0060, spec #412 ticket
//! #415; restyled by #423): a Session's live Background Tasks ride the newest
//! (live) card's tail like the Todo Panel and the live Tool Panels — one
//! folded-by-default `collapsible_panel` whose title is the pinned count
//! (`⏳ 后台任务（N）`) and whose body is one row per task — bold label joined
//! from the originating tool part's input by `call_id`, start clock; a shell
//! row follows it with its bare elapsed (its only liveness), while a `subagent`
//! row renders its child's activity in the front task panel's own vocabulary
//! instead and never a total elapsed (spec #501) — and a retirement leaves the
//! section on the card's existing render cadence. The launch panel itself
//! (ticket #416) keeps its timeline place and renders 🌙 in its status slot
//! instead of ✅, before and after the retirement.
//!
//! The section stays on the card the tasks lived on (ADR-0066, ticket #488):
//! a shell/subagent completion Wake resumes its yielded card in place, so the
//! live list and the fixed completion entries never leave it. The handover
//! PATCH runs only when the chain genuinely moves — a new Turn takes over a
//! waiting card, or a size overflow, a Supplement split or a restart/interrupt
//! Wake continues the chain — and then the section follows the newest card
//! while the outgoing card keeps the entries of the tasks that lived there; an
//! entry never migrates to a continuation.
//!
//! V1 carries no Background Task facts, so its section never renders; the
//! tests here script the typed transcript reads (the read model already owns
//! the decode, see `opencode::v2::wire`).

use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use super::drain::{
    assistant, ctx, noticed, script_transcript, scripted_app, scripted_app_with, spawn_sync, spawn_turn,
    user, wait_for_card_text,
};
use crate::backend::{
    BackgroundTask, ChildRuntime, ContentBlock, FinishReason, MessageId, MessageRole, Part,
    SessionTranscript, ShellEnd, ShellRuntime, StepFinish, ToolCall, ToolIdentity, ToolOutput, ToolStatus,
    TranscriptMessage, Wake, WakeSource,
};
use crate::bridge::test_support::*;
use crate::bridge::turn::{PromptContext, Turn};
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

/// A shell ledger row's elapsed tail — the row's LAST ` · ` segment, after the
/// type word, the bolded label and the ` · HH:MM` start clock. Only shell rows
/// render an elapsed (a subagent row's liveness is its activity fragment).
/// Panics when the row is missing (the section itself is the test's first
/// assertion).
fn ledger_elapsed(card: &serde_json::Value, prefix: &str) -> String {
    let text = card_text(card);
    let row = text
        .lines()
        .find(|line| line.starts_with(prefix))
        .unwrap_or_else(|| panic!("no ledger row {prefix:?}: {text}"));
    row.rsplit(" · ")
        .next()
        .expect("rsplit always yields one segment")
        .to_string()
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

/// Whether `seg` is a ledger row's `HH:MM` start clock.
fn is_start_clock(seg: &str) -> bool {
    let bytes = seg.as_bytes();
    bytes.len() == 5
        && bytes[2] == b':'
        && bytes[..2].iter().all(u8::is_ascii_digit)
        && bytes[3..].iter().all(u8::is_ascii_digit)
}

/// Whether `seg` is a ledger row's bare TOTAL elapsed (`3m12s` / `1h05m`): one
/// number+unit token with no space. An activity fragment's segments always
/// carry the tool/phase word (or a wait), so they never match — which is how a
/// test tells a shell row's elapsed from a subagent row's fragment age.
fn is_total_elapsed(seg: &str) -> bool {
    !seg.contains(' ') && parsed_task_elapsed(seg).is_some()
}

/// Acceptance 1: a live card with a live Background Task shows the section —
/// a folded panel whose title is the count, whose body is one row labelled
/// from the tool part's input by `call_id`, with the bare elapsed — while the
/// card is still the live one.
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
        card_text(card).contains("⏳ 后台任务（1）")
            && card_text(card).contains("· shell：**npm run build** · ")
    })
    .await;

    let card = platform.updated_cards().await.last().cloned().unwrap();
    assert!(
        !card_header(&card).contains("完成"),
        "the ledger rides the LIVE card, not a settled one: {card}"
    );
    assert_elapsed_shaped(&ledger_elapsed(&card, "· shell：**npm run build** · "));
    // The live list is a folded panel (ADR-0060, #423): the count is its
    // title, the rows its body, and one stable id keeps the reader's fold
    // state across re-renders. An empty ledger renders no panel at all.
    let panels: Vec<&serde_json::Value> = card["body"]["elements"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|e| e["header"]["title"]["content"] == "⏳ 后台任务（1）")
        .collect();
    assert_eq!(panels.len(), 1, "exactly one ledger panel: {card}");
    let panel = panels[0];
    assert_eq!(panel["tag"], "collapsible_panel", "the live list folds: {panel}");
    assert_eq!(panel["expanded"], false, "folded by default: {panel}");
    assert_eq!(
        panel["element_id"], "task_ledger",
        "stable fold identity: {panel}"
    );
    let body = panel["elements"][0]["content"].as_str().unwrap();
    assert!(
        body.starts_with("· shell：**npm run build** · ") && !body.contains("后台任务（"),
        "the rows are the panel body, the count its title: {body}"
    );
    // The launch's own panel keeps its timeline place beside the ledger row.
    assert!(
        card_text(&card).contains("moved to background"),
        "the launching call's panel stays on the card: {card}"
    );
}

/// Acceptance (#416, restyled by #423): a backgrounded launch's panel renders
/// 🌙 in its status slot instead of ✅ — before AND after its run retires (the
/// panel never claims completion; the ledger's entry is the completion record)
/// — while a foreground `shell` call renders ✅ exactly as today.
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
    // Before retirement: the 🌙 marker in the status slot while the run's row
    // rides the ledger.
    wait_for_card_update(&platform, "the backgrounded panel", CardUpdates::Any, |card| {
        let text = card_text(card);
        text.contains("🌙 shell · ") && text.contains("⏳ 后台任务（1）")
    })
    .await;
    let card = platform.updated_cards().await.last().cloned().unwrap();
    let text = card_text(&card);
    assert_eq!(
        text.matches("🌙 shell · ").count(),
        1,
        "the launch's panel carries the pinned marker exactly once: {text}"
    );
    assert_eq!(
        text.matches("✅ shell · ").count(),
        1,
        "only the foreground control claims completion: {text}"
    );

    // The run retires: the ledger's row leaves, the launch's panel keeps its
    // place and STILL reads 🌙 — it never flips to ✅.
    script_transcript(&backend, vec![retired]).await;
    wait_for_card_update(
        &platform,
        "the retired launch's panel",
        CardUpdates::Latest,
        |card| {
            let text = card_text(card);
            !text.contains("后台任务") && text.contains("🌙 shell · ")
        },
    )
    .await;
    let card = platform.updated_cards().await.last().cloned().unwrap();
    let text = card_text(&card);
    assert_eq!(
        text.matches("🌙 shell · ").count(),
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
            && text.contains("· shell：**npm run build**")
            && text.contains("· subagent：**review the diff**")
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
    let shell_row = text.find("· shell：**npm run build**").expect("the shell row");
    let sub_row = text
        .find("· subagent：**review the diff**")
        .expect("the subagent row");
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
    assert_elapsed_shaped(&ledger_elapsed(&card, "· shell：**npm run build** · "));
    // A subagent row never renders a total elapsed (spec #501): after its
    // start clock comes only its child's activity fragment, if any.
    let sub_row = text
        .lines()
        .find(|line| line.starts_with("· subagent：**review the diff** · "))
        .expect("the subagent row");
    assert!(
        !sub_row
            .split(" · ")
            .skip_while(|seg| !is_start_clock(seg))
            .skip(1)
            .any(is_total_elapsed),
        "a subagent row renders no elapsed: {sub_row:?}"
    );
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
        text.contains("· shell：**watch the CI** · "),
        "a shell falls back to its description: {text}"
    );
    assert!(
        text.contains("· subagent：**review the diff** · "),
        "the subagent arm reads its description: {text}"
    );
    assert!(
        text.contains("· shell · "),
        "a task whose input names no label renders bare: {text}"
    );
    assert!(
        !text.contains("· shell：**very long command very long command very long command very long command"),
        "a long label must clip like the completion entry's title in the ROW: {text}"
    );
    let clipped = text
        .lines()
        .find(|line| line.starts_with("· shell：**very long command"))
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
                && text.contains("· shell：**npm run build**")
                && !text.contains("· subagent：")
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

/// The one-card-per-request contract where a live list is involved (#418,
/// rewritten for ADR-0066): the shell retires with resumed work while the
/// subagent stays live, so the completion Wake resumes the yielded card IN
/// PLACE — the retired shell's row goes, its fixed entry arrives beside the
/// REMAINING subagent's row, the resumed work renders on the same card, and
/// the card yields back to 「⏳ 等待后台任务」. The live list never leaves the
/// card, so no handover and no new card exist at all.
#[tokio::test]
async fn a_completion_wake_keeps_the_live_list_on_the_resumed_card() {
    let _wd = test_work_dir();
    let (_dir, app, backend, platform) =
        scripted_app(vec![two_task_waiting()], Some(SessionStatus::Idle)).await;
    yield_the_two_task_card(&app, &platform).await;
    let cards_before = created_cards(&platform).await.len();

    // The shell retires (its Wake at 2_900) while the subagent stays live.
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
        "the resumed card's remaining list",
        CardUpdates::Latest,
        |card| {
            let text = card_text(card);
            card_header(card).contains("等待后台任务")
                && text.contains("🔔 shell 完成：gh run watch")
                && text.contains("CI 通过了。")
                && !text.contains("· shell：**gh run watch**")
        },
    )
    .await;

    // The card is waiting again — the resumed run left the subagent live — and
    // the entry, the work and the remaining list all ride it.
    assert_eq!(
        Turn::card_state(&app.cards_handle(), "ses_test").await,
        Some(CardState::Waiting),
        "the resumed run yields back to ⏳ while a task stays live"
    );
    assert_eq!(
        created_cards(&platform).await.len(),
        cards_before,
        "an in-place resume posts no card: {:?}",
        platform.calls.lock().await
    );
    let resumed = platform.updated_cards().await.last().cloned().unwrap();
    let text = card_text(&resumed);
    assert!(
        card_header(&resumed).contains("等待后台任务"),
        "the card is back to its wait header: {resumed}"
    );
    assert!(
        text.contains("🔔 shell 完成：gh run watch") && text.contains("shell sh_bg · "),
        "the retired task's entry lands on the card that hosted it: {resumed}"
    );
    assert!(
        !text.contains("· shell：**gh run watch**"),
        "the retired task's live row is gone: {resumed}"
    );
    assert!(
        text.contains("⏳ 后台任务（1）") && text.contains("· subagent：**review the diff**"),
        "the remaining live list stays on the same card: {resumed}"
    );
    assert!(
        text.contains("CI 通过了。") && text.contains("已经交给后台了。"),
        "the resumed work joins the request's own timeline: {resumed}"
    );
    assert!(
        !text.contains(WAKE_LEAD),
        "an in-place resume writes no 承接 line: {resumed}"
    );
    assert_eq!(
        text.matches("shell 完成").count(),
        1,
        "exactly one entry: {resumed}"
    );

    // The resume PATCH itself already carried the remaining list: the retired
    // shell's row never appears beside the entry, not even briefly.
    let patches = patches_to(&platform, "om_waiting").await;
    let resume = patches
        .iter()
        .find(|patch| card_text(patch).contains("🔔 shell 完成：gh run watch"))
        .unwrap_or_else(|| panic!("the entry landed on the request's card: {patches:?}"));
    let resume_text = card_text(resume);
    assert!(
        resume_text.contains("⏳ 后台任务（1）") && !resume_text.contains("后台任务（2）"),
        "the entry lands with the remaining list, never the retired row: {resume}"
    );
    // No handover PATCH exists: the request's card never finalizes with the
    // split header — its only transitions are updates — and no continuation
    // card was posted below it (`created_cards` above).
    assert!(
        patches
            .iter()
            .all(|patch| !card_header(patch).contains("部分完成")),
        "an in-place resume hands nothing over: {patches:?}"
    );
}

/// Acceptance 3 (#418), the several-completions case under ADR-0066: both
/// tasks retire in one read — the shell's Wake at 2_900, the subagent's at
/// 3_500 — with the resumed work landing after both, so the SAME resumed card
/// carries both fixed entries in Wake order, the work, and the ✅ ending. No
/// continuation exists to migrate an entry onto: one card per request.
#[tokio::test]
async fn a_resumed_card_carries_every_completion_of_the_read() {
    let _wd = test_work_dir();
    let (_dir, app, backend, platform) =
        scripted_app(vec![two_task_waiting()], Some(SessionStatus::Idle)).await;
    yield_the_two_task_card(&app, &platform).await;
    let cards_before = created_cards(&platform).await.len();

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
        "the resumed card's done state",
        CardUpdates::Latest,
        |card| card_header(card).contains("✅") && card_text(card).contains("都完成了。"),
    )
    .await;

    assert_eq!(
        created_cards(&platform).await.len(),
        cards_before,
        "both completions still post no card: {:?}",
        platform.calls.lock().await
    );
    let resumed = platform.updated_cards().await.last().cloned().unwrap();
    let text = card_text(&resumed);
    assert!(
        !text.contains("后台任务（"),
        "no live list rides a session with nothing live: {resumed}"
    );
    let shell = text
        .find("🔔 shell 完成：gh run watch")
        .unwrap_or_else(|| panic!("the shell's entry lands where the task lived: {resumed}"));
    let subagent = text
        .find("🔔 subagent 完成：review the diff")
        .unwrap_or_else(|| panic!("the subagent's entry lands where the task lived: {resumed}"));
    let work = text
        .find("都完成了。")
        .unwrap_or_else(|| panic!("the resumed work renders: {resumed}"));
    assert!(
        shell < subagent,
        "the entries keep Wake order (shell before subagent): {resumed}"
    );
    assert!(
        subagent < work,
        "both entries precede the work they announce: {resumed}"
    );
    assert!(
        !text.contains(WAKE_LEAD),
        "an in-place resume writes no 承接 line: {resumed}"
    );
}

/// #552: a resumed subagent leaves TWO launch parts for one child. The
/// decoder's merged live list keeps only the newest launch — and the child's
/// completion Wake must render exactly ONE entry, never one per launch part —
/// with no live row left behind.
#[tokio::test]
async fn a_resumed_subagents_wake_renders_one_completion_entry() {
    let _wd = test_work_dir();
    let (_dir, app, backend, platform) =
        scripted_app(vec![resumed_subagent_waiting()], Some(SessionStatus::Idle)).await;

    // The turn idles with the (deduplicated) live child: one row.
    Turn::run(&app.turn_handles(), ctx("ses_test", "审阅一下"))
        .await
        .unwrap();
    assert_eq!(
        Turn::card_state(&app.cards_handle(), "ses_test").await,
        Some(CardState::Waiting),
        "idle with a live task yields waiting"
    );
    let yielded = platform.updated_cards().await.last().cloned().unwrap();
    assert!(
        card_text(&yielded).contains("⏳ 后台任务（1）"),
        "the merged live list shows one row: {yielded}"
    );
    Turn::set_card_message_id(&app.cards_handle(), "ses_test", "om_waiting").await;

    // The resume completes: the child's Wake retires the survivor and the
    // resumed work lands on the same card.
    script_transcript(
        &backend,
        vec![
            SessionTranscript::new(resumed_subagent_timeline())
                .with_executions(vec![execution(2_500), execution(4_000)])
                .with_wakes(vec![resumed_child_wake(3_500)]),
        ],
    )
    .await;
    spawn_sync(&app);
    wait_for_card_update(
        &platform,
        "the resumed card's true end",
        CardUpdates::Latest,
        |card| card_header(card).contains("✅") && card_text(card).contains("审阅完成了。"),
    )
    .await;

    let resumed = platform.updated_cards().await.last().cloned().unwrap();
    let text = card_text(&resumed);
    assert_eq!(
        text.matches("🔔 subagent 完成").count(),
        1,
        "one Wake, one entry — never one per launch part: {resumed}"
    );
    assert!(
        !text.contains("后台任务（"),
        "no live list after the true end: {resumed}"
    );
}

/// The resumed-subagent waiting read: one child launched twice (the earlier
/// run interrupted, the later resume replacing it), carrying the merged live
/// list the decoder produces — the newest launch only (#552).
fn resumed_subagent_waiting() -> SessionTranscript {
    SessionTranscript::new(resumed_subagent_timeline())
        .with_executions(vec![execution(2_500)])
        .with_background_tasks(vec![live_subagent_child(2_600, "call_sub_2", "ses_child")])
}

/// One child, two launches, and the resumed work — the read a completed
/// resume produces (#552).
fn resumed_subagent_timeline() -> Vec<TranscriptMessage> {
    vec![
        user("msg_cola_anchor", 1_000, "审阅一下"),
        subagent_launch(2_000, "call_sub_1", "ses_child"),
        subagent_launch(2_600, "call_sub_2", "ses_child"),
        assistant(3_700, "审阅完成了。"),
    ]
}

/// A settled background `subagent` launch whose metadata names its child
/// session — two of these appear when a run is relaunched on the same child.
fn subagent_launch(created: i64, call_id: &str, child_id: &str) -> TranscriptMessage {
    typed_message(
        &format!("msg_launch_{call_id}"),
        MessageRole::Assistant,
        Some(created),
        vec![
            Part::Tool(ToolCall {
                identity: ToolIdentity {
                    name: "subagent".into(),
                    call_id: call_id.into(),
                },
                status: ToolStatus::Completed,
                started_at: Some(created),
                input: Some(serde_json::json!({ "description": "review the diff", "background": true })),
                metadata: Some(serde_json::json!({ "status": "running", "sessionID": child_id })),
                output: ToolOutput::default(),
            }),
            Part::StepFinish(StepFinish {
                reason: FinishReason::ToolCalls,
            }),
        ],
    )
}

/// The merged live task the decoder keeps for the resumed child (#552).
fn live_subagent_child(started_at: i64, call_id: &str, child_id: &str) -> BackgroundTask {
    BackgroundTask {
        tool: ToolIdentity {
            name: "subagent".into(),
            call_id: call_id.into(),
        },
        shell_id: None,
        child_id: Some(child_id.into()),
        started_at: Some(started_at),
    }
}

/// The resumed child's completion Wake.
fn resumed_child_wake(created_ms: i64) -> Wake {
    Wake {
        id: MessageId::new(format!("msg_wake_sub_{created_ms}")),
        created_ms: Some(created_ms),
        source: WakeSource::Subagent,
        shell_id: None,
        job_id: None,
        child_id: Some("ses_child".into()),
        state: Some("completed".into()),
        label: Some("review the diff".into()),
    }
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
            text.contains("⏳ 后台任务（1）") && text.contains("· subagent：**review the diff**")
        },
    )
    .await;
    let live = platform.updated_cards().await.last().cloned().unwrap();
    assert!(
        !card_text(&live).contains("· shell：**gh run watch**"),
        "the retired task's row does not migrate onto the new Turn's card: {live}"
    );
}

/// The third handover cause (ADR-0066, ticket #488): a Supplement split still
/// moves the chain. The shell's completion resumed the card in place with the
/// subagent still live, so a user message landing below the live resumed card
/// routes as a Supplement (ADR-0043): the request's card finalizes with the
/// standard handoff — keeping the retiring task's entry — and the continuation
/// carries the REMAINING live list. The entry never migrates, and no ledger row
/// is lost or duplicated across the two cards.
#[tokio::test]
async fn a_supplement_split_hands_the_remaining_list_to_the_continuation() {
    let _wd = test_work_dir();
    let (_dir, app, backend, platform) =
        scripted_app(vec![two_task_waiting()], Some(SessionStatus::Idle)).await;
    yield_the_two_task_card(&app, &platform).await;

    // The shell's completion resumes the card in place; the subagent stays
    // live and the resumed run is still going (one boundary only).
    script_transcript(
        &backend,
        vec![
            SessionTranscript::new(two_task_timeline(vec![assistant(3_100, "正在合并。")]))
                .with_executions(vec![execution(2_500)])
                .with_wakes(vec![shell_wake(2_900)])
                .with_background_tasks(vec![live_subagent(2_100, "call_sub")]),
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

    // A message lands below it: the owned chain routes it as a Supplement and
    // splits the chain at the message.
    app.handle_message(incoming(
        "msg_sup".into(),
        "chat_1".into(),
        "p2p".into(),
        None,
        "补充一下，改用方案 B".into(),
        None,
    ))
    .await;

    // The continuation carries the supplement receipt and the REMAINING live
    // list — never the retired row, never the entry (which stays where the
    // task lived).
    let continuation = {
        let calls = platform.calls.lock().await;
        calls
            .iter()
            .find_map(|call| match call {
                PlatformCall::ReplyCard { reply_to, card } if reply_to == "msg_sup" => Some(card.clone()),
                _ => None,
            })
            .unwrap_or_else(|| panic!("the supplement must split the chain at the message: {calls:?}"))
    };
    let continuation_text = card_text(&continuation);
    assert!(
        continuation_text.contains("📨 已收到补充"),
        "the continuation carries the supplement receipt: {continuation}"
    );
    assert!(
        continuation_text.contains("⏳ 后台任务（1）")
            && continuation_text.contains("· subagent：**review the diff**"),
        "the continuation carries the remaining live list: {continuation}"
    );
    assert!(
        !continuation_text.contains("· shell：**gh run watch**"),
        "the retired task's row does not migrate onto the continuation: {continuation}"
    );
    assert!(
        !continuation_text.contains("shell 完成"),
        "the entry never migrates onto the continuation: {continuation}"
    );

    // The request's card finalizes with the standard handoff, keeping its own
    // timeline and the entry — and hands the live list over.
    let finalized = patches_to(&platform, "om_waiting")
        .await
        .pop()
        .expect("the resumed card is finalized at the split");
    let finalized_text = card_text(&finalized);
    assert!(
        card_header(&finalized).contains("部分完成，继续中"),
        "the split finalizes the resumed card with the standard handoff: {finalized}"
    );
    assert!(
        finalized_text.contains("🔔 shell 完成：gh run watch") && finalized_text.contains("正在合并。"),
        "the entry and the work stay on the card that hosted the task: {finalized}"
    );
    assert!(
        !finalized_text.contains("后台任务（"),
        "the finalized card hands the live list over: {finalized}"
    );

    // Every ledger fact renders exactly once across the two cards.
    assert_across_cards([&finalized, &continuation], "shell 完成", 1);
    assert_across_cards([&finalized, &continuation], "· subagent：**review the diff**", 1);
}

// ---------------------------------------------------------------------------
// The yielded card's ledger stays fresh (ADR-0060, tickets #419 and #423): a
// Waiting card has no render loop of its own, so the existing Session Sync
// reads drive its ledger in place — a quiet retirement's row leaves and its
// entry arrives, a row's rendered elapsed advances at second granularity (the
// read's own cadence), and a read that would render the ledger the card
// already shows PATCHes nothing. The carve-out is the ledger alone: the rest
// of the yielded card stays frozen.
//
// The last quiet retirement is also the card's true end (ticket #420): the
// same read settles the host card in place — ✅, or the ❌/⏹ a settled failure
// or a deliberate stop dominates with — and sends the Completion Notice per
// its existing rules. No new card, and only a Waiting card is ever stamped.
// ---------------------------------------------------------------------------

/// The one-task waiting read the in-place tests yield from: the timeline's
/// shell launched at `started_at`. A start near the test's own clock keeps the
/// row's elapsed seconds close to the yield's own read, so a short test window
/// crosses only the boundaries it means to.
fn waiting_shell(started_at: i64) -> SessionTranscript {
    SessionTranscript::new(two_task_timeline(vec![]))
        .with_executions(vec![execution(2_500)])
        .with_background_tasks(vec![live_shell(started_at, "call_bg")])
}

/// The row's rendered elapsed in whole seconds (`XmYYs` under an hour — the only
/// shape these tests reach): a shell row's rendered clock. Panics on the
/// `XhYYm` shape so a test that waits an hour out fails loudly instead of
/// parsing it wrong.
fn elapsed_secs(elapsed: &str) -> u64 {
    let (minutes, seconds) = elapsed
        .trim_end_matches('s')
        .split_once('m')
        .unwrap_or_else(|| panic!("not an XmYYs elapsed: {elapsed:?}"));
    minutes.parse::<u64>().unwrap() * 60 + seconds.parse::<u64>().unwrap()
}

/// A start clock ahead of cola's (`now + 1h`): a shell row's elapsed clamps to
/// `0m00s` forever, so its second-granular clock can never tick (a subagent
/// row's elapsed is never rendered at all). The event-count tests use it for a
/// task that stays live across the event, so
/// "exactly one PATCH for this retirement / collect" is deterministic; the
/// second-granular cadence itself is pinned by the rate tests, where the
/// starts are real.
///
/// ORDERING: a test that counts PATCHes exactly must call `spawn_sync` only
/// AFTER `script_transcript` — the loop must not already be running while a
/// still-ticking row could add a leading PATCH before the event's own, which
/// would over-report the count.
fn frozen_start(now: i64) -> i64 {
    now + 3_600_000
}

/// Assert every one of `patches` is the same wait card with only its rows'
/// elapsed advanced: the header still reads 「⏳ 等待后台任务」, and consecutive
/// patches compare equal modulo the rows' times ([`text_ignoring_elapsed`]).
/// A ledger-only refresh (ADR-0060) has exactly this shape, so a PATCH that
/// smuggled any other change in fails here — the cadence may repeat a patch,
/// never vary what it renders.
fn assert_ledger_only_refreshes(patches: &[serde_json::Value]) {
    assert!(
        patches
            .iter()
            .all(|patch| card_header(patch).contains("等待后台任务")),
        "a ledger-only refresh never restyles the card: {patches:?}"
    );
    let normalized: Vec<String> = patches.iter().map(text_ignoring_elapsed).collect();
    assert!(
        normalized.windows(2).all(|pair| pair[0] == pair[1]),
        "every later PATCH is a ledger-only elapsed advance: {patches:?}"
    );
}

/// A card's visible text with every ledger row's rendered elapsed folded to
/// `<t>` — the "same ledger, a later second" comparison: a patch that only
/// advanced a row's time compares equal to its predecessor, while a row
/// joining/leaving or an entry arriving still differs. Only the elapsed after
/// the rows' ` · ` is folded (`XmYYs` / `XhYYm`); a completion entry's
/// minute-only duration is left verbatim.
fn text_ignoring_elapsed(card: &serde_json::Value) -> String {
    // `·` is two UTF-8 bytes, so the separator's length is what advances the
    // scan (a hardcoded +3 would cut before the trailing space).
    const SEP: &str = " · ";
    let text = card_text(card);
    let mut out = String::with_capacity(text.len());
    let mut rest = text.as_str();
    while let Some(pos) = rest.find(SEP) {
        out.push_str(&rest[..pos + SEP.len()]);
        rest = &rest[pos + SEP.len()..];
        let digits = rest.chars().take_while(char::is_ascii_digit).count();
        let tail = &rest[digits..];
        let elapsed = digits > 0
            && matches!(
                tail.as_bytes(),
                [b'm' | b'h', b'0'..=b'9', b'0'..=b'9', b's' | b'm', ..]
            );
        if elapsed {
            out.push_str("<t>");
            rest = &tail[4..];
        }
    }
    out.push_str(rest);
    out
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

/// Script the live subagent's child read to an activity-less transcript: the
/// mock otherwise serves its DEFAULT turn (with fresh timestamps) for any
/// session, whose `思考中 0s` fragment would tick the yielded refresh every
/// second and disturb a test's PATCH counts. A test that needs a fragment
/// scripts `ses_call_sub` itself after this.
async fn script_child_without_activity(backend: &Arc<MockBackend>) {
    backend
        .given_transcript_after_build("ses_call_sub", vec![SessionTranscript::new(vec![])])
        .await;
}

/// An assistant message that settled the turn as FAILED with no renderable
/// part (a step boundary only), so the Wake step's content diff owes no
/// continuation and the read's own settle decision judges the failure.
fn failed_quiet_assistant(created: i64, error: &str) -> TranscriptMessage {
    let mut message = typed_message(
        &format!("msg_fail_{created}"),
        MessageRole::Assistant,
        Some(created),
        vec![Part::StepFinish(StepFinish {
            reason: FinishReason::Error,
        })],
    );
    message.error = Some(error.to_string());
    message
}

/// Run the turn on `read` to its waiting yield and name the yielded card
/// `om_waiting` (the harness replies every card with one id), so the ledger
/// PATCHes the in-place pass adds can be told apart from every other card's.
async fn yield_waiting_card(app: &Arc<App>, platform: &RecordingPlatform, live_tasks: usize) {
    yield_waiting_card_with(app, platform, live_tasks, ctx("ses_test", "跑一下构建并审阅")).await;
}

/// [`yield_waiting_card`] with the turn's own context (a group turn with a
/// requester, when the test needs the notice rules to be live).
async fn yield_waiting_card_with(
    app: &Arc<App>,
    platform: &RecordingPlatform,
    live_tasks: usize,
    context: PromptContext,
) {
    Turn::run(&app.turn_handles(), context).await.unwrap();
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

/// Run `context`'s turn to its waiting yield on a single live task and name the
/// yielded card `om_waiting` (the harness replies every card with one id, so
/// the rename tells the settle's PATCHes apart from the yield's own).
async fn yield_one_task_card(app: &Arc<App>, platform: &RecordingPlatform, context: PromptContext) {
    Turn::run(&app.turn_handles(), context).await.unwrap();
    assert_eq!(
        Turn::card_state(&app.cards_handle(), "ses_test").await,
        Some(CardState::Waiting),
        "the turn yields on its live task"
    );
    let yielded = platform.updated_cards().await.last().cloned().unwrap();
    assert!(
        card_text(&yielded).contains("⏳ 后台任务（1）"),
        "the waiting card carries its live task: {yielded}"
    );
    Turn::set_card_message_id(&app.cards_handle(), "ses_test", "om_waiting").await;
}

/// Script the read that retires the one live task with nothing to render: the
/// Wake is answered by a later Execution boundary and no resumed work follows,
/// so the read is the true end.
async fn script_quiet_true_end(backend: &Arc<MockBackend>) {
    script_transcript(
        backend,
        vec![
            SessionTranscript::new(two_task_timeline(vec![]))
                .with_executions(vec![execution(2_500), execution(4_000)])
                .with_wakes(vec![shell_wake(2_900)]),
        ],
    )
    .await;
}

/// Await the Completion Notice — sent right after the settle PATCH, so it can
/// trail the card update — or panic after 5 s.
async fn wait_for_notice(platform: &RecordingPlatform) -> Vec<(String, String, Option<String>, String)> {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        let notices = platform.completion_notices().await;
        if !notices.is_empty() {
            return notices;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "no Completion Notice arrived: {:?}",
            platform.calls.lock().await
        );
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

/// Acceptance 1 (#420): the last task retires with nothing to render, so the
/// read settles the WAITING card in place — ✅, terminal, the fixed completion
/// entry kept, no new card — and the Completion Notice fires per its existing
/// group rule.
#[tokio::test]
async fn a_quiet_true_end_settles_the_host_card_in_place() {
    let _wd = test_work_dir();
    let now = chrono::Utc::now().timestamp_millis();
    let live = waiting_shell(now - 5_000);
    let (_dir, app, backend, platform) = scripted_app(vec![live], Some(SessionStatus::Idle)).await;
    // A group turn with a requester: the notice's opt-in default (test_config)
    // notifies, so a missing notice would be the settle's bug.
    let mut context = ctx("ses_test", "跑一下构建并审阅");
    context.is_group = true;
    context.requester_open_id = Some(TEST_HOST.to_string());
    yield_one_task_card(&app, &platform, context).await;
    let cards_before = created_cards(&platform).await.len();

    script_quiet_true_end(&backend).await;
    spawn_sync(&app);
    wait_for_card_update(&platform, "the settled card", CardUpdates::Latest, |card| {
        card_header(card).contains("✅") && card_text(card).contains("🔔 shell 完成：gh run watch")
    })
    .await;

    assert_eq!(
        Turn::card_state(&app.cards_handle(), "ses_test").await,
        Some(CardState::Done),
        "the true end is the card's terminal"
    );
    let patches = patches_to(&platform, "om_waiting").await;
    assert_eq!(patches.len(), 1, "the settle is one in-place PATCH: {patches:?}");
    let settled = &patches[0];
    let text = card_text(settled);
    assert!(
        !text.contains("后台任务（"),
        "the retired task's live list is gone: {settled}"
    );
    assert!(
        text.contains("🔔 shell 完成：gh run watch") && text.contains("shell sh_bg · "),
        "the fixed entry stays on the card that hosted the task: {settled}"
    );
    assert!(
        text.contains("已经交给后台了。") && text.contains("📁"),
        "the settled card keeps its timeline and footer: {settled}"
    );

    // Nothing was posted: the settle is in place.
    assert!(
        continuation_sends(&platform).await.is_empty(),
        "a quiet true end owes no continuation: {:?}",
        platform.calls.lock().await
    );
    assert_eq!(
        created_cards(&platform).await.len(),
        cards_before,
        "no new card is posted for the settle"
    );

    // The notice fires per its existing rules: a group turn with the opt-in
    // replies to the requester with the ✅ copy.
    let notices = wait_for_notice(&platform).await;
    assert_eq!(notices.len(), 1, "exactly one notice: {notices:?}");
    assert_eq!(notices[0].0, "msg_1", "it replies to the prompt");
    assert_eq!(notices[0].1, TEST_HOST);
    assert!(
        notices[0].3.contains("已完成"),
        "unexpected notice text: {}",
        notices[0].3
    );
}

/// Issue #454: a shell the runtime reports ENDED — while its completion Wake
/// never arrives — retires on the same Session Sync read that refreshes the
/// yielded ledger, and the last retirement settles the waiting card in place
/// with the runtime's own ending (「🔔 shell 结束」), never a Wake entry. The
/// transcript still listed the task and keeps listing it; the runtime read is
/// what ends the wait.
#[tokio::test]
async fn a_runtime_confirmed_end_settles_the_waiting_card() {
    let _wd = test_work_dir();
    let now = chrono::Utc::now().timestamp_millis();
    let live = waiting_shell(now - 5_000);
    let (_dir, app, backend, platform) = scripted_app(vec![live], Some(SessionStatus::Idle)).await;
    yield_one_task_card(&app, &platform, ctx("ses_test", "跑一下构建并审阅")).await;
    let cards_before = created_cards(&platform).await.len();

    // The runtime confirms the shell ended (killed) at its own completion
    // time; no Wake will ever retire it. The transcript stays as scripted and
    // keeps listing the task, so only the runtime read can end the wait.
    let finished = now - 1_000;
    backend.task_runtime.lock().unwrap().shells = vec![(
        "sh_call_bg".into(),
        ShellRuntime::Ended {
            end: ShellEnd::Killed,
            completed_at: Some(finished),
        },
    )];
    spawn_sync(&app);
    wait_for_card_update(&platform, "the settled card", CardUpdates::Latest, |card| {
        card_header(card).contains("✅") && card_text(card).contains("🔔 shell 结束：gh run watch")
    })
    .await;

    assert_eq!(
        Turn::card_state(&app.cards_handle(), "ses_test").await,
        Some(CardState::Done),
        "the runtime end is the card's terminal"
    );
    let patches = patches_to(&platform, "om_waiting").await;
    assert_eq!(patches.len(), 1, "the settle is one in-place PATCH: {patches:?}");
    let settled = &patches[0];
    let text = card_text(settled);
    assert!(
        !text.contains("后台任务（"),
        "the runtime-retired task's live list is gone: {settled}"
    );
    assert!(
        text.contains("shell sh_call_bg · ") && !text.contains("shell 已失联"),
        "the runtime entry carries the task's identity and its own ending: {settled}"
    );
    assert!(
        continuation_sends(&platform).await.is_empty()
            && created_cards(&platform).await.len() == cards_before,
        "a runtime settle posts nothing: {:?}",
        platform.calls.lock().await
    );
    // The reconcile asked for exactly this session's task: the runtime read,
    // not a rewritten transcript, is what retired it (#454).
    let calls = backend.task_runtime_calls.lock().await.clone();
    assert!(
        calls
            .iter()
            .any(|(sid, shells, _)| sid == "ses_test" && shells == &vec!["sh_call_bg".to_string()]),
        "the reconcile read named the live shell: {calls:?}"
    );
    // The retirement is overlay-recorded: a later read of the same scripted
    // transcript no longer lists the task (issue #454 review — without this the
    // next live card would resurrect it).
    let later = crate::backend::Backend::transcript(backend.as_ref(), "ses_test")
        .await
        .unwrap();
    assert!(
        later.background_tasks.is_empty(),
        "the retirement overlay filters every later read"
    );
}

/// Issue #454 review: the reconcile observes a retirement only while a card can
/// still receive the ledger. With no card — a settled chain, or right after a
/// restart — observing would record the task and swallow its entry (the real
/// restart retired at 02:25:13 with no card and showed nothing); the next
/// Waiting card reconciles instead, and the task is still there to be seen.
#[tokio::test]
async fn a_runtime_retirement_waits_for_a_waiting_card() {
    let _wd = test_work_dir();
    let now = chrono::Utc::now().timestamp_millis();
    let live = waiting_shell(now - 5_000);
    let (_dir, app, backend, platform) = scripted_app(vec![live], Some(SessionStatus::Idle)).await;
    backend.task_runtime.lock().unwrap().shells = vec![("sh_call_bg".into(), ShellRuntime::Missing)];

    // No card yet: Session Sync runs, but must not observe (or record) the
    // retirement.
    spawn_sync(&app);
    tokio::time::sleep(Duration::from_millis(60)).await;
    assert!(
        backend.task_runtime_calls.lock().await.is_empty(),
        "no waiting card means no runtime observation"
    );

    // A card exists and yields waiting: the same read now observes, renders the
    // entry, and settles.
    yield_one_task_card(&app, &platform, ctx("ses_test", "跑一下构建并审阅")).await;
    wait_for_card_update(
        &platform,
        "the entry after the card exists",
        CardUpdates::Latest,
        |card| card_header(card).contains("✅") && card_text(card).contains("🔔 shell 已失联：gh run watch"),
    )
    .await;
    assert!(
        !backend.task_runtime_calls.lock().await.is_empty(),
        "the waiting card's pass observed the retirement"
    );
}

/// Issue #454 review: a retirement observed by a chain whose anchor postdates
/// the task's launch still renders its entry — after a restart the launching
/// card is gone, and scoping the entry to the observing chain's anchor would
/// silently swallow it (the real reboot settled ✅ with no entry).
#[tokio::test]
async fn a_runtime_retirement_renders_on_a_later_chain() {
    let _wd = test_work_dir();
    // The task launched BEFORE this card's Turn anchor (the fixture timeline's
    // user message is at 1_000): this chain observes the end, the launching
    // chain is not the card being refreshed.
    let live = waiting_shell(500);
    let (_dir, app, backend, platform) = scripted_app(vec![live], Some(SessionStatus::Idle)).await;
    yield_one_task_card(&app, &platform, ctx("ses_test", "跑一下构建并审阅")).await;

    backend.task_runtime.lock().unwrap().shells = vec![("sh_call_bg".into(), ShellRuntime::Missing)];
    spawn_sync(&app);
    wait_for_card_update(&platform, "the later-chain entry", CardUpdates::Latest, |card| {
        card_header(card).contains("✅") && card_text(card).contains("🔔 shell 已失联：gh run watch")
    })
    .await;

    assert_eq!(
        Turn::card_state(&app.cards_handle(), "ses_test").await,
        Some(CardState::Done),
        "the later-chain retirement still settles the wait"
    );
}

/// Issue #454: a shell the runtime no longer knows (its completion record was
/// lost with the process that hosted it) retires as 已失联 — the card settles
/// and the entry says what is known, with no invented completion time.
#[tokio::test]
async fn a_runtime_lost_shell_settles_as_lost() {
    let _wd = test_work_dir();
    let now = chrono::Utc::now().timestamp_millis();
    let live = waiting_shell(now - 5_000);
    let (_dir, app, backend, platform) = scripted_app(vec![live], Some(SessionStatus::Idle)).await;
    yield_one_task_card(&app, &platform, ctx("ses_test", "跑一下构建并审阅")).await;

    backend.task_runtime.lock().unwrap().shells = vec![("sh_call_bg".into(), ShellRuntime::Missing)];
    spawn_sync(&app);
    wait_for_card_update(&platform, "the lost settle", CardUpdates::Latest, |card| {
        card_header(card).contains("✅") && card_text(card).contains("🔔 shell 已失联：gh run watch")
    })
    .await;

    assert_eq!(
        Turn::card_state(&app.cards_handle(), "ses_test").await,
        Some(CardState::Done)
    );
    let patches = patches_to(&platform, "om_waiting").await;
    assert_eq!(patches.len(), 1, "one in-place PATCH: {patches:?}");
    let text = card_text(&patches[0]);
    assert!(
        text.contains("shell sh_call_bg") && !text.contains("shell sh_call_bg · "),
        "a lost entry carries identity but no invented clock: {patches:?}"
    );
}

/// Issue #454: a subagent the runtime reports INACTIVE is evidence, not an
/// ending — the row gains the 待确认 marker, the card stays waiting, and its
/// Wake (or the user's own decision) remains the only retirement.
#[tokio::test]
async fn an_inactive_child_marks_its_row_unconfirmed_without_settling() {
    let _wd = test_work_dir();
    let now = chrono::Utc::now().timestamp_millis();
    let live = waiting_shell_and_subagent(now - 5_000, now - 3_000);
    let (_dir, app, backend, platform) = scripted_app(vec![live], Some(SessionStatus::Idle)).await;
    yield_waiting_card(&app, &platform, 2).await;

    {
        let mut runtime = backend.task_runtime.lock().unwrap();
        runtime.shells = vec![("sh_call_bg".into(), ShellRuntime::Running)];
        runtime.children = vec![("ses_call_sub".into(), ChildRuntime::Inactive)];
    }
    spawn_sync(&app);
    wait_for_card_update(&platform, "the unconfirmed marker", CardUpdates::Latest, |card| {
        card_text(card).contains("⚠️ 状态待确认")
    })
    .await;

    assert_eq!(
        Turn::card_state(&app.cards_handle(), "ses_test").await,
        Some(CardState::Waiting),
        "an unconfirmed row never settles the card"
    );
    let latest = platform.updated_cards().await.last().cloned().unwrap();
    let text = card_text(&latest);
    assert!(
        text.contains("⏳ 后台任务（2 · 1 待确认）"),
        "the title counts the unconfirmed row without unfolding: {latest}"
    );
    assert!(
        text.lines()
            .any(|line| line.contains("subagent：**review the diff**") && line.ends_with("⚠️ 状态待确认")),
        "the marker trails the child's own facts: {latest}"
    );
    // The runtime read asked for exactly this session's tasks.
    let calls = backend.task_runtime_calls.lock().await.clone();
    assert!(
        calls.iter().any(|(sid, shells, children)| sid == "ses_test"
            && shells == &vec!["sh_call_bg".to_string()]
            && children == &vec!["ses_call_sub".to_string()]),
        "the reconcile read named the live tasks: {calls:?}"
    );
}

/// Acceptance 3 (#420): a deliberate `/stop` during the wait dominates the
/// quiet true end — the settle stamps ⏹ 已停止 in place, never ✅ — and the
/// notice follows the card's real terminal.
#[tokio::test]
async fn a_stop_during_the_wait_settles_the_card_as_stopped() {
    let _wd = test_work_dir();
    let now = chrono::Utc::now().timestamp_millis();
    let live = waiting_shell(now - 5_000);
    let (_dir, app, backend, platform) = scripted_app(vec![live], Some(SessionStatus::Idle)).await;
    let mut context = ctx("ses_test", "跑一下构建并审阅");
    context.is_group = true;
    context.requester_open_id = Some(TEST_HOST.to_string());
    yield_one_task_card(&app, &platform, context).await;
    // The user stops while the card waits: the marker is sticky until a Wake
    // clears it (a quiet retirement never does).
    app.stopped_sessions.lock().await.insert("ses_test".to_string());

    script_quiet_true_end(&backend).await;
    spawn_sync(&app);
    wait_for_card_update(&platform, "the stop terminal", CardUpdates::Latest, |card| {
        card_header(card).contains("已停止") && card_text(card).contains("🔔 shell 完成：gh run watch")
    })
    .await;

    assert_eq!(
        Turn::card_state(&app.cards_handle(), "ses_test").await,
        Some(CardState::Stopped),
        "the deliberate stop owns the ending"
    );
    let patches = patches_to(&platform, "om_waiting").await;
    assert_eq!(patches.len(), 1, "the settle is one in-place PATCH: {patches:?}");
    assert!(
        !card_header(&patches[0]).contains("完成"),
        "the ledger path never stamps ✅ over a stop: {}",
        patches[0]
    );
    let notices = wait_for_notice(&platform).await;
    assert_eq!(notices.len(), 1, "exactly one notice: {notices:?}");
    assert!(
        notices[0].3.contains("已停止"),
        "the notice follows the card's terminal: {}",
        notices[0].3
    );
}

/// Acceptance 3 (#420): a read that itself judges a failure over a Waiting
/// card — an answered Wake, no live task left, the turn's settled failure —
/// stamps ❌ with the failure's copy in place, never ✅, and the notice's copy
/// follows the card's real terminal.
#[tokio::test]
async fn a_settled_failure_dominates_the_quiet_true_end() {
    let _wd = test_work_dir();
    let now = chrono::Utc::now().timestamp_millis();
    let live = waiting_shell(now - 5_000);
    let (_dir, app, backend, platform) = scripted_app(vec![live], Some(SessionStatus::Idle)).await;
    // A group turn with a requester: the notice's opt-in default notifies, so
    // its ❌ copy is observable too.
    let mut context = ctx("ses_test", "跑一下构建并审阅");
    context.is_group = true;
    context.requester_open_id = Some(TEST_HOST.to_string());
    yield_one_task_card(&app, &platform, context).await;

    // The task retires and the resumed run FAILED with nothing to render: the
    // read's own settle decision is the failure (a failure dominates the
    // ending, ADR-0059), and the ledger path stamps it instead of ✅.
    script_transcript(
        &backend,
        vec![
            SessionTranscript::new(two_task_timeline(vec![failed_quiet_assistant(3_100, "boom")]))
                .with_executions(vec![execution(2_500), execution(4_000)])
                .with_wakes(vec![shell_wake(2_900)]),
        ],
    )
    .await;
    spawn_sync(&app);
    wait_for_card_update(&platform, "the failed card", CardUpdates::Latest, |card| {
        card_header(card).contains("❌")
    })
    .await;

    assert_eq!(
        Turn::card_state(&app.cards_handle(), "ses_test").await,
        Some(CardState::Error),
        "the settled failure is the card's terminal"
    );
    let patches = patches_to(&platform, "om_waiting").await;
    assert_eq!(patches.len(), 1, "the settle is one in-place PATCH: {patches:?}");
    assert!(
        !card_header(&patches[0]).contains("完成"),
        "the ledger path never stamps ✅ over a failure: {}",
        patches[0]
    );
    let notices = wait_for_notice(&platform).await;
    assert_eq!(notices.len(), 1, "exactly one notice: {notices:?}");
    assert!(
        notices[0].3.contains("出错"),
        "the notice follows the card's terminal: {}",
        notices[0].3
    );
}

/// Acceptance 2 (#420): a retirement that still leaves a Background Task live
/// does not settle — here with the Wake ANSWERED, so the read's own settle
/// decision actually reaches the live-task arm and returns `Waiting` (the
/// existing live-task test's unanswered Wake stops at `Running`). The card
/// gets the ledger refresh in place and keeps waiting: no ✅, no notice.
#[tokio::test]
async fn a_retirement_with_a_live_task_left_never_settles() {
    let _wd = test_work_dir();
    let now = chrono::Utc::now().timestamp_millis();
    // The subagent stays live across the retirement, with a frozen clock: the
    // event's PATCH count below cannot be disturbed by a later rendered second.
    let sub_started = frozen_start(now);
    let live = waiting_shell_and_subagent(now - 5_000, sub_started);
    let (_dir, app, backend, platform) = scripted_app(vec![live], Some(SessionStatus::Idle)).await;
    // The remaining subagent's child read carries no activity, so the frozen
    // start above is the card's only clock.
    script_child_without_activity(&backend).await;
    // A group turn with a requester: a settle would notify, so the silence is
    // evidence, not a missing opt-in.
    let mut context = ctx("ses_test", "跑一下构建并审阅");
    context.is_group = true;
    context.requester_open_id = Some(TEST_HOST.to_string());
    yield_waiting_card_with(&app, &platform, 2, context).await;

    // The shell retires with its Wake ANSWERED (an Execution boundary after
    // it) while the subagent stays live.
    script_transcript(
        &backend,
        vec![
            SessionTranscript::new(two_task_timeline(vec![]))
                .with_executions(vec![execution(2_500), execution(4_000)])
                .with_wakes(vec![shell_wake(2_900)])
                .with_background_tasks(vec![live_subagent(sub_started, "call_sub")]),
        ],
    )
    .await;
    spawn_sync(&app);
    wait_for_card_update(&platform, "the refreshed wait", CardUpdates::Latest, |card| {
        card_text(card).contains("🔔 shell 完成：gh run watch")
    })
    .await;

    assert_eq!(
        Turn::card_state(&app.cards_handle(), "ses_test").await,
        Some(CardState::Waiting),
        "a live Background Task keeps the card waiting"
    );
    // The retirement is exactly ONE in-place PATCH: give the loop several more
    // passes (the injected cadence is 20 ms) and count — the remaining
    // subagent's clock is frozen, so a later read renders the same ledger and
    // PATCHes nothing (ADR-0060's carve-out admits the elapsed refresh, not a
    // PATCH per read).
    tokio::time::sleep(Duration::from_millis(100)).await;
    let patches = patches_to(&platform, "om_waiting").await;
    assert_eq!(
        patches.len(),
        1,
        "the retirement is one in-place PATCH: {patches:?}"
    );
    let text = card_text(&patches[0]);
    assert!(
        card_header(&patches[0]).contains("等待后台任务") && text.contains("⏳ 后台任务（1）"),
        "the refreshed card is still the wait: {}",
        patches[0]
    );
    // The notice would trail the refresh in the same pass: give it the moment,
    // then assert the silence.
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(
        !noticed(&platform).await,
        "a live task keeps the true end (and its notice) away: {:?}",
        platform.calls.lock().await
    );
}

/// Acceptance 3 (#420): a card that already settled as a failure keeps its
/// terminal — the ledger path reads the accumulator's recorded state and never
/// stamps ✅ (or anything else) over it. Nothing is even PATCHed: the card
/// stopped updating at its ending.
#[tokio::test]
async fn a_terminal_card_is_never_restamped_by_a_retirement() {
    let _wd = test_work_dir();
    let now = chrono::Utc::now().timestamp_millis();
    let live = waiting_shell(now - 5_000);
    let (_dir, app, backend, platform) = scripted_app(vec![live], Some(SessionStatus::Idle)).await;
    // A group turn with a requester: if the ledger path stamped over the
    // terminal, the notice would fire and expose it.
    let mut context = ctx("ses_test", "跑一下构建并审阅");
    context.is_group = true;
    context.requester_open_id = Some(TEST_HOST.to_string());
    yield_one_task_card(&app, &platform, context).await;
    // A settled failure (the shape the follow's Error terminal records).
    Turn::set_card_state(&app.cards_handle(), "ses_test", CardState::Error).await;
    let patches_before = patches_to(&platform, "om_waiting").await.len();

    script_quiet_true_end(&backend).await;
    spawn_sync(&app);
    // Several passes at the injected cadence: none may touch the terminal.
    tokio::time::sleep(Duration::from_millis(150)).await;

    assert_eq!(
        Turn::card_state(&app.cards_handle(), "ses_test").await,
        Some(CardState::Error),
        "the settled failure keeps its terminal"
    );
    assert_eq!(
        patches_to(&platform, "om_waiting").await.len(),
        patches_before,
        "a terminal card is not refreshed: {:?}",
        platform.updated_cards().await
    );
    assert!(
        !noticed(&platform).await,
        "a terminal card's retirement sends no notice"
    );
}

/// The notice's p2p rule is reused, not re-invented (#420): a quiet true end
/// that ran past the long-task threshold notifies — the whole run's clock, the
/// turn's start recorded on the card — with no @ mention (p2p's reply is the
/// notification).
#[tokio::test]
async fn a_long_quiet_true_end_notifies_in_p2p() {
    let _wd = test_work_dir();
    let now = chrono::Utc::now().timestamp_millis();
    let live = waiting_shell(now - 5_000);
    let (_dir, app, backend, platform) = scripted_app_with(vec![live], Some(SessionStatus::Idle), |cfg| {
        cfg.bridge.long_task_notice = true;
    })
    .await;
    app.long_task_notice_ms.store(1, Ordering::Relaxed);
    let mut context = ctx("ses_test", "跑一下构建并审阅");
    context.requester_open_id = Some(TEST_HOST.to_string());
    yield_one_task_card(&app, &platform, context).await;
    // The yield and the settle read take real milliseconds, so the threshold
    // above is already past by the time the settle fires.
    tokio::time::sleep(Duration::from_millis(10)).await;

    script_quiet_true_end(&backend).await;
    spawn_sync(&app);
    wait_for_card_update(&platform, "the settled p2p card", CardUpdates::Latest, |card| {
        card_header(card).contains("✅")
    })
    .await;

    let notices = wait_for_notice(&platform).await;
    assert_eq!(notices.len(), 1, "exactly one notice: {notices:?}");
    assert_eq!(notices[0].0, "msg_1", "it replies to the prompt");
    assert_eq!(notices[0].1, TEST_HOST);
    assert_eq!(notices[0].2, None, "p2p needs no @ mention");
    assert!(
        notices[0].3.contains("已完成"),
        "unexpected notice text: {}",
        notices[0].3
    );
}

/// The other half of the p2p rule (#420): a quiet true end under the threshold
/// stays silent, exactly like a short normal turn.
#[tokio::test]
async fn a_short_quiet_true_end_stays_silent_in_p2p() {
    let _wd = test_work_dir();
    let now = chrono::Utc::now().timestamp_millis();
    let live = waiting_shell(now - 5_000);
    let (_dir, app, backend, platform) = scripted_app_with(vec![live], Some(SessionStatus::Idle), |cfg| {
        cfg.bridge.long_task_notice = true;
    })
    .await;
    app.long_task_notice_ms.store(60_000, Ordering::Relaxed);
    let mut context = ctx("ses_test", "跑一下构建并审阅");
    context.requester_open_id = Some(TEST_HOST.to_string());
    yield_one_task_card(&app, &platform, context).await;

    script_quiet_true_end(&backend).await;
    spawn_sync(&app);
    wait_for_card_update(&platform, "the settled p2p card", CardUpdates::Latest, |card| {
        card_header(card).contains("✅")
    })
    .await;
    // The notice would trail the settle PATCH in the same pass: give it the
    // moment, then assert the silence.
    tokio::time::sleep(Duration::from_millis(50)).await;

    assert!(
        platform.completion_notices().await.is_empty(),
        "a short p2p run must not notify: {:?}",
        platform.calls.lock().await
    );
}

/// The true end waits for the Wake's own boundary (the read's settle decision,
/// ADR-0059): a read that already dropped the task but whose Wake has no
/// Execution boundary yet only refreshes the ledger — the resumed run could
/// still render work — and the read that carries the boundary settles.
#[tokio::test]
async fn a_retirement_before_the_wakes_boundary_does_not_settle() {
    let _wd = test_work_dir();
    let now = chrono::Utc::now().timestamp_millis();
    let live = waiting_shell(now - 5_000);
    let (_dir, app, backend, platform) = scripted_app(vec![live], Some(SessionStatus::Idle)).await;
    yield_one_task_card(&app, &platform, ctx("ses_test", "跑一下构建并审阅")).await;

    // The Wake's boundary is still missing: the ledger moves in place (the row
    // leaves, the entry arrives) but the card keeps waiting.
    script_transcript(
        &backend,
        vec![
            SessionTranscript::new(two_task_timeline(vec![]))
                .with_executions(vec![execution(2_500)])
                .with_wakes(vec![shell_wake(2_900)]),
        ],
    )
    .await;
    spawn_sync(&app);
    wait_for_card_update(&platform, "the refreshed wait", CardUpdates::Latest, |card| {
        card_text(card).contains("🔔 shell 完成：gh run watch")
    })
    .await;
    assert_eq!(
        Turn::card_state(&app.cards_handle(), "ses_test").await,
        Some(CardState::Waiting),
        "an unanswered Wake keeps the card waiting"
    );
    assert!(
        card_header(&platform.updated_cards().await.last().cloned().unwrap()).contains("等待后台任务"),
        "the refreshed card is still the wait"
    );

    // The boundary lands: the same ledger, now the true end — settled in place.
    script_transcript(
        &backend,
        vec![
            SessionTranscript::new(two_task_timeline(vec![]))
                .with_executions(vec![execution(2_500), execution(4_000)])
                .with_wakes(vec![shell_wake(2_900)]),
        ],
    )
    .await;
    wait_for_card_update(&platform, "the settled card", CardUpdates::Latest, |card| {
        card_header(card).contains("✅")
    })
    .await;
    assert_eq!(
        Turn::card_state(&app.cards_handle(), "ses_test").await,
        Some(CardState::Done),
        "the read that answers the Wake is the true end"
    );
}

/// The in-place resume's quiet true end (#420, ADR-0066): the resumed card
/// yields 「⏳」 on the task its run left live — an in-place update, so the
/// resume itself never notifies — and THAT card's last retirement settles it
/// ✅ in place and sends the request's ONE notice (a group turn's opt-in, whose
/// clock the card's own Turn start is). No card is posted at either step.
#[tokio::test]
async fn an_in_place_resumes_quiet_true_end_notifies_once() {
    let _wd = test_work_dir();
    let now = chrono::Utc::now().timestamp_millis();
    let sub_started = now - 4_000;
    let (_dir, app, backend, platform) =
        scripted_app(vec![two_task_waiting()], Some(SessionStatus::Idle)).await;
    // A group turn with a requester: a true end notifies, so the notice below
    // is evidence, not a missing opt-in.
    let mut context = ctx("ses_test", "跑一下构建并审阅");
    context.is_group = true;
    context.requester_open_id = Some(TEST_HOST.to_string());
    Turn::run(&app.turn_handles(), context).await.unwrap();
    assert_eq!(
        Turn::card_state(&app.cards_handle(), "ses_test").await,
        Some(CardState::Waiting)
    );
    Turn::set_card_message_id(&app.cards_handle(), "ses_test", "om_waiting").await;
    let cards_before = created_cards(&platform).await.len();
    assert!(
        !noticed(&platform).await,
        "the waiting yield never notifies: {:?}",
        platform.calls.lock().await
    );

    // The shell retires with resumed work: the SAME card resumes in place and
    // yields Waiting again on the remaining subagent — still no notice.
    script_transcript(
        &backend,
        vec![
            SessionTranscript::new(two_task_timeline(vec![assistant(3_100, "CI 通过了。")]))
                .with_executions(vec![execution(2_500), execution(4_000)])
                .with_wakes(vec![shell_wake(2_900)])
                .with_background_tasks(vec![live_subagent(sub_started, "call_sub")]),
        ],
    )
    .await;
    spawn_sync(&app);
    wait_for_card_update(
        &platform,
        "the resumed card waiting on the subagent",
        CardUpdates::Latest,
        |card| {
            card_header(card).contains("等待后台任务")
                && card_text(card).contains("CI 通过了。")
                && !card_text(card).contains("后台任务（2）")
        },
    )
    .await;
    assert!(
        !noticed(&platform).await,
        "an in-place update never notifies: {:?}",
        platform.calls.lock().await
    );

    // The subagent retires quietly: the card reaches its true end in place,
    // and the request's ONE notice follows it.
    script_transcript(
        &backend,
        vec![
            SessionTranscript::new(two_task_timeline(vec![]))
                .with_executions(vec![execution(2_500), execution(4_000)])
                .with_wakes(vec![shell_wake(2_900), subagent_wake(3_500, "review the diff")]),
        ],
    )
    .await;
    wait_for_card_update(
        &platform,
        "the resumed card's true end",
        CardUpdates::Latest,
        |card| card_header(card).contains("✅") && !card_text(card).contains("后台任务（"),
    )
    .await;

    let notices = wait_for_notice(&platform).await;
    assert_eq!(notices.len(), 1, "exactly one notice: {notices:?}");
    assert_eq!(notices[0].0, "msg_1", "it replies to the request's message");
    assert!(
        notices[0].3.contains("已完成"),
        "the true end is what it announces: {:?}",
        notices[0].3
    );
    assert_eq!(
        created_cards(&platform).await.len(),
        cards_before,
        "the whole resume ran in place: {:?}",
        platform.calls.lock().await
    );
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
    // The subagent stays live across the retirement, with a frozen clock: the
    // event's PATCH count below cannot be disturbed by a later rendered second.
    let sub_started = frozen_start(now);
    let live = waiting_shell_and_subagent(now - 5_000, sub_started);
    let (_dir, app, backend, platform) = scripted_app(vec![live], Some(SessionStatus::Idle)).await;
    // The remaining subagent's child read carries no activity, so the frozen
    // start above is the card's only clock.
    script_child_without_activity(&backend).await;
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
        card_text(card).contains("🔔 shell 完成：gh run watch")
            && !card_text(card).contains("· shell：**gh run watch**")
    })
    .await;

    // The retirement is exactly ONE in-place PATCH carrying both facts: give
    // the loop several more passes (the injected cadence is 20 ms) and count —
    // the remaining subagent's clock is frozen, so no later read can PATCH.
    tokio::time::sleep(Duration::from_millis(100)).await;
    let patches = patches_to(&platform, "om_waiting").await;
    assert_eq!(
        patches.len(),
        1,
        "the retirement is one in-place PATCH: {patches:?}"
    );
    let updated = &patches[0];
    let text = card_text(updated);
    assert!(
        !text.contains("· shell：**gh run watch**"),
        "the retired task's row leaves: {updated}"
    );
    assert!(
        text.contains("⏳ 后台任务（1）") && text.contains("· subagent：**review the diff**"),
        "the remaining list rides the same card: {updated}"
    );
    assert!(
        text.contains("🔔 shell 完成：gh run watch") && text.contains("shell sh_bg · "),
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

/// Acceptance 3 (#423): the yielded refresh owes a PATCH only when the ledger
/// it would render differs from the one the card shows — membership, or a
/// row's rendered second advancing. The injected ~20 ms passes stand in for
/// the ~8 s Session Sync reads, so over a 1.2 s window at most two rendered
/// seconds can advance (plus the boundary the yield itself sat on): the card
/// is never PATCHed once per read, and nothing is posted. Every PATCH that
/// does land is the same card with a later second. The control at the end
/// proves the loop was alive for the whole window: a read that changes the
/// ledger PATCHes right away, and that retirement is exactly one PATCH.
#[tokio::test]
async fn repeated_reads_inside_the_same_rendered_second_patch_nothing() {
    let _wd = test_work_dir();
    let now = chrono::Utc::now().timestamp_millis();
    let sub_started = now - 4_000;
    let live = waiting_shell_and_subagent(now - 5_000, sub_started);
    let (_dir, app, backend, platform) = scripted_app(vec![live], Some(SessionStatus::Idle)).await;
    // The only rendered number is the shell row's elapsed: the child read
    // carries no activity, and a subagent row never renders an elapsed, so
    // nothing else can join the rate bound.
    script_child_without_activity(&backend).await;
    yield_waiting_card(&app, &platform, 2).await;
    let yielded = platform.updated_cards().await.last().cloned().unwrap();
    let elapsed = ledger_elapsed(&yielded, "· shell：**gh run watch** · ");
    let cards_before = created_cards(&platform).await.len();

    // ~60 passes at the injected cadence: at most one PATCH per advanced
    // rendered second (at most two in 1.2 s), never one per read.
    spawn_sync(&app);
    tokio::time::sleep(Duration::from_millis(1_200)).await;
    let patches = patches_to(&platform, "om_waiting").await;
    assert!(
        patches.len() <= 5,
        "reads inside a rendered second must not PATCH the yielded card: {patches:?}"
    );
    assert_eq!(
        created_cards(&platform).await.len(),
        cards_before,
        "a ledger refresh posts nothing"
    );
    // Whatever landed only advanced the rendered elapsed: the first seconds
    // are the yield's own read, and the sequence never runs backwards.
    let mut previous = elapsed_secs(&elapsed);
    for patch in &patches {
        let next = elapsed_secs(&ledger_elapsed(patch, "· shell：**gh run watch** · "));
        assert!(
            next >= previous,
            "the rendered elapsed never runs backwards ({previous} -> {next}): {patch}"
        );
        previous = next;
    }
    // Every refresh is the same card with a later second — the wait's header
    // stays, and nothing but the rows' elapsed differs — so the passes can
    // never smuggle another change in under the rate bound.
    assert_ledger_only_refreshes(&patches);

    // Control: the same loop is listening — a read that changes the ledger
    // PATCHes right away. The retirement's transition is ONE PATCH: the
    // remaining subagent's clock is frozen, so no refresh can follow it.
    script_transcript(
        &backend,
        vec![
            SessionTranscript::new(two_task_timeline(vec![]))
                .with_executions(vec![execution(2_500)])
                .with_wakes(vec![shell_wake(2_900)])
                .with_background_tasks(vec![live_subagent(frozen_start(now), "call_sub")]),
        ],
    )
    .await;
    wait_for_card_update(&platform, "the read that moved", CardUpdates::Latest, |card| {
        !card_text(card).contains("· shell：**gh run watch**")
    })
    .await;
    // Several more passes (the injected cadence is 20 ms) must add nothing: the
    // frozen clock renders the same ledger, so the retirement is one PATCH.
    tokio::time::sleep(Duration::from_millis(100)).await;
    let patches = patches_to(&platform, "om_waiting").await;
    let event = patches
        .iter()
        .position(|patch| card_text(patch).contains("🔔 shell 完成：gh run watch"))
        .expect("the retirement PATCHed");
    assert_eq!(
        event,
        patches.len() - 1,
        "the retirement is one PATCH and nothing follows it: {patches:?}"
    );
}

/// Acceptance 3 (#423): elapsed refresh is SECOND-granular and driven by the
/// reads. The row's rendered second advancing owes a PATCH (the ~8 s Session
/// Sync reads spend ≈0.125 QPS per waiting card), while the repeated reads
/// inside one rendered second owe nothing.
#[tokio::test]
async fn a_waiting_cards_elapsed_refreshes_on_the_rendered_second() {
    let _wd = test_work_dir();
    let now = chrono::Utc::now().timestamp_millis();
    let live = waiting_shell(now - 5_000);
    let (_dir, app, backend, platform) = scripted_app(vec![live], Some(SessionStatus::Idle)).await;
    yield_waiting_card(&app, &platform, 1).await;
    let yielded = platform.updated_cards().await.last().cloned().unwrap();
    let yielded_elapsed = elapsed_secs(&ledger_elapsed(&yielded, "· shell：**gh run watch** · "));
    // ORDERING: the loop is not running yet, so the scripted start move below
    // is the card's FIRST PATCH. `spawn_sync` must stay AFTER `script_transcript`
    // (a loop running against the old row could tick in between and over-report
    // the exact count).
    let patches_before = patches_to(&platform, "om_waiting").await.len();

    // The seconds inside the first rendered second do not owe a PATCH: give
    // the passes a moment short of a boundary and count.
    let started = chrono::Utc::now().timestamp_millis() - 55_000;
    let near_boundary = waiting_shell(started);
    script_transcript(&backend, vec![near_boundary]).await;
    spawn_sync(&app);
    wait_for_card_update(&platform, "the moved start", CardUpdates::Latest, |card| {
        ledger_elapsed(card, "· shell：**gh run watch** · ").starts_with("0m5")
    })
    .await;

    // The start move is a membership change: exactly one PATCH so far. The
    // reads that follow inside the same rendered second owe nothing.
    let moved = patches_to(&platform, "om_waiting").await.len();
    assert_eq!(
        moved,
        patches_before + 1,
        "the start move PATCHes once: {:?}",
        platform.updated_cards().await
    );
    tokio::time::sleep(Duration::from_millis(150)).await;
    assert_eq!(
        patches_to(&platform, "om_waiting").await.len(),
        moved,
        "the reads inside that rendered second must not PATCH: {:?}",
        platform.updated_cards().await
    );

    // Over the next 2.5 s every advanced rendered second PATCHes — at most one
    // per second, never one per 20 ms read — and the card's rendered elapsed
    // ends well past the yield's. Every one of those PATCHes is the same card
    // with a later second.
    tokio::time::sleep(Duration::from_millis(2_500)).await;
    let patches = patches_to(&platform, "om_waiting").await;
    let advanced = patches.len() - moved;
    assert!(
        (2..=6).contains(&advanced),
        "one PATCH per advanced rendered second (~2-3 in 2.5 s), not per read: {advanced} in {:?}",
        platform.updated_cards().await
    );
    assert_ledger_only_refreshes(&patches);
    let last = platform.updated_cards().await.last().cloned().unwrap();
    let last_elapsed = elapsed_secs(&ledger_elapsed(&last, "· shell：**gh run watch** · "));
    assert!(
        last_elapsed > yielded_elapsed,
        "the card's elapsed kept moving past the yield's {yielded_elapsed}s: {last}"
    );
}

/// The no-split guard (ADR-0060, ticket #419): a ledger-only refresh must not
/// post a new card even when its entry tips the estimate over the split budget
/// — the corner is reachable, since a completion entry outweighs the row it
/// replaces. The entry here carries a pathological id so that delta alone
/// crosses the budget deterministically; the waiting card's ONE in-place PATCH
/// keeps its own header (never 「部分完成，继续中…」) and nothing is posted.
#[tokio::test]
async fn a_ledger_only_refresh_never_splits_the_waiting_card() {
    let _wd = test_work_dir();
    let now = chrono::Utc::now().timestamp_millis();
    // The subagent stays live across the retirement, with a frozen clock, so
    // the event's PATCH count is deterministic.
    let sub_started = frozen_start(now);
    let live = waiting_shell_and_subagent(now - 5_000, sub_started);
    let (_dir, app, backend, platform) = scripted_app(vec![live], Some(SessionStatus::Idle)).await;
    // The remaining subagent's child read carries no activity, so the frozen
    // start above is the card's only clock.
    script_child_without_activity(&backend).await;
    yield_waiting_card(&app, &platform, 2).await;
    let cards_before = created_cards(&platform).await.len();

    // The retired shell's entry names an id far past one card's split budget:
    // without the guard the flush would finalize the card and continue on a
    // NEW card.
    let mut wake = shell_wake(2_900);
    wake.shell_id = Some(format!("sh_{}", "x".repeat(26_000)));
    wake.job_id = wake.shell_id.clone();
    script_transcript(
        &backend,
        vec![
            SessionTranscript::new(two_task_timeline(vec![]))
                .with_executions(vec![execution(2_500)])
                .with_wakes(vec![wake])
                .with_background_tasks(vec![live_subagent(sub_started, "call_sub")]),
        ],
    )
    .await;
    spawn_sync(&app);
    wait_for_card_update(&platform, "the oversized entry", CardUpdates::Latest, |card| {
        card_text(card).contains("🔔 shell 完成：gh run watch")
    })
    .await;

    // The retirement is exactly ONE in-place PATCH, and the chain did not move:
    // give the loop several more passes (the injected cadence is 20 ms) and
    // count — the remaining subagent's clock is frozen, so no later read can
    // PATCH. The card keeps its waiting header instead of 「部分完成，继续中…」,
    // and no continuation or any other card was posted.
    tokio::time::sleep(Duration::from_millis(100)).await;
    let patches = patches_to(&platform, "om_waiting").await;
    assert_eq!(
        patches.len(),
        1,
        "the oversized entry is one in-place PATCH, never a split: {patches:?}"
    );
    let updated = &patches[0];
    assert!(
        card_header(updated).contains("等待后台任务"),
        "the card keeps its own header: {updated}"
    );
    assert!(
        !card_header(updated).contains("继续中"),
        "a ledger-only refresh never finalizes the card: {updated}"
    );
    let text = card_text(updated);
    assert!(
        text.contains("🔔 shell 完成：gh run watch") && text.contains("⏳ 后台任务（1）"),
        "the entry and the remaining list land in place: {updated}"
    );
    assert!(
        continuation_sends(&platform).await.is_empty(),
        "no continuation is posted: {:?}",
        platform.calls.lock().await
    );
    assert_eq!(
        created_cards(&platform).await.len(),
        cards_before,
        "no new card is posted"
    );
}

/// The retire-then-takeover race (#418 review): a completion that retires
/// between reads is owned by the in-place pass, and the later new-Turn collect
/// — which drops the live list — leaves the entry on the card it happened on.
#[tokio::test]
async fn a_quiet_retirement_survives_a_later_takeover() {
    let _wd = test_work_dir();
    let now = chrono::Utc::now().timestamp_millis();
    // The subagent stays live across the retirement, with a frozen clock, so
    // the retirement and the collect are the card's only PATCHes.
    let sub_started = frozen_start(now);
    let live = waiting_shell_and_subagent(now - 5_000, sub_started);
    let (_dir, app, backend, platform) = scripted_app(vec![live], Some(SessionStatus::Idle)).await;
    // The remaining subagent's child read carries no activity, so the frozen
    // start above is the card's only clock.
    script_child_without_activity(&backend).await;
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
        card_text(card).contains("🔔 shell 完成：gh run watch")
    })
    .await;
    // The retirement is exactly ONE PATCH: several more passes (the injected
    // cadence is 20 ms) with the remaining subagent's clock frozen must add
    // nothing.
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(
        patches_to(&platform, "om_waiting").await.len(),
        1,
        "the retirement is one PATCH: {:?}",
        platform.updated_cards().await
    );

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

    // The collect is the second and LAST PATCH: a few more passes after it
    // (the card is collected, the remaining clock frozen) add nothing.
    tokio::time::sleep(Duration::from_millis(100)).await;
    let patches = patches_to(&platform, "om_waiting").await;
    assert_eq!(
        patches.len(),
        2,
        "the retirement and the collect are the waiting card's PATCHes: {patches:?}"
    );
    let retirement = &patches[0];
    assert!(
        card_text(retirement).contains("🔔 shell 完成：gh run watch")
            && card_header(retirement).contains("等待后台任务"),
        "the retirement is the first PATCH: {retirement}"
    );
    let collected = &patches[1];
    assert!(
        card_header(collected).contains("已由新消息接管"),
        "the collect is the waiting card's last PATCH: {collected}"
    );
    assert!(
        !card_text(collected).contains("后台任务（"),
        "the collect drops the live list: {collected}"
    );
    assert!(
        card_text(collected).contains("🔔 shell 完成：gh run watch"),
        "the entry stays on the card it happened on: {collected}"
    );
}

/// The ledger's elapsed is rendered at card build time: a task whose start
/// clock is ahead (server skew) clamps to `0m00s`, so a row never renders a
/// negative age. The start clock still renders the skewed start's own time.
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
        card_text(card)
            .lines()
            .any(|line| line.starts_with("· shell：**npm run build** · ") && line.ends_with("0m00s"))
    })
    .await;
    // A few more renders of the SAME task must keep rendering the same
    // clamped row, not a drifting one.
    tokio::time::sleep(Duration::from_millis(20)).await;
    let card = platform.updated_cards().await.last().cloned().unwrap();
    assert_eq!(
        ledger_elapsed(&card, "· shell：**npm run build** · "),
        "0m00s",
        "a skewed clock clamps to zero, it does not drift: {card}"
    );
}

// ---------------------------------------------------------------------------
// A live background subagent's child activity on the yielded ledger row (spec
// #501, ticket #503): the Session Sync reads that already keep the waiting
// card's ledger fresh also gather each live subagent child's liveness — one
// transcript light read per distinct child plus its pending-wait query, the
// LIVE task panel's own batch — and the row renders it with the front panel's
// vocabulary, after its elapsed and before the 状态待确认 marker. A failed
// child read keeps the row's last successful fragment (its age keeps growing);
// an unconfirmed row renders no activity; a retirement takes the fragment
// with its row.
// ---------------------------------------------------------------------------

/// A child-session transcript whose newest part is a running tool started at
/// `started_at` — the activity the yielded ledger row shows as `<name> <age>`.
fn child_running_tool(started_at: i64, name: &str) -> SessionTranscript {
    SessionTranscript::new(vec![typed_message(
        "a_child",
        MessageRole::Assistant,
        Some(started_at),
        vec![Part::Tool(ToolCall {
            identity: ToolIdentity {
                name: name.into(),
                call_id: "call_child".into(),
            },
            status: ToolStatus::Running,
            started_at: Some(started_at),
            input: Some(serde_json::json!({ "command": "cargo test" })),
            metadata: None,
            output: ToolOutput::default(),
        })],
    )])
}

/// A child-session transcript whose newest content is streamed reply text: no
/// live tool, so the activity is the phase `回复中` with the age of the newest
/// activity.
fn child_replying(at: i64) -> SessionTranscript {
    SessionTranscript::new(vec![typed_message(
        "a_child",
        MessageRole::Assistant,
        Some(at),
        vec![text_part("working through the diff")],
    )])
}

/// The row starting with `prefix`'s activity fragment — everything it renders
/// after its start clock (`bash 5s`, `思考中 30s · 等待你的授权`), or `None` when
/// the row carries none (a shell row, an unconfirmed row, a subagent row with
/// no stored fragment). A subagent row renders no total elapsed, so the
/// fragment follows the `HH:MM` clock directly; a row with no clock has the
/// fragment after its label.
fn ledger_activity(card: &serde_json::Value, prefix: &str) -> Option<String> {
    let text = card_text(card);
    let row = text.lines().find(|line| line.starts_with(prefix))?;
    let segments: Vec<&str> = row.split(" · ").collect();
    let start = segments
        .iter()
        .position(|seg| is_start_clock(seg))
        .map_or(1, |at| at + 1);
    let activity = segments[start..].join(" · ");
    (!activity.is_empty() && activity != "⚠️ 状态待确认").then_some(activity)
}

/// The seconds a ledger row's elapsed or activity age names (`3m12s`, `1h05m`,
/// `5s`), `None` for a segment that is not one.
fn parsed_task_elapsed(seg: &str) -> Option<u64> {
    if let Some(hm) = seg.strip_suffix('m') {
        let (hours, minutes) = hm.split_once('h')?;
        return Some(hours.parse::<u64>().ok()? * 3_600 + minutes.parse::<u64>().ok()? * 60);
    }
    let ms = seg.strip_suffix('s')?;
    match ms.split_once('m') {
        Some((minutes, seconds)) => Some(minutes.parse::<u64>().ok()? * 60 + seconds.parse::<u64>().ok()?),
        None => ms.parse::<u64>().ok(),
    }
}

/// The seconds a fragment's age names — `bash 5s`, `思考中 1m30s`.
fn activity_age_secs(activity: &str) -> u64 {
    let age = activity
        .split(' ')
        .nth(1)
        .unwrap_or_else(|| panic!("no age in the fragment: {activity:?}"));
    parsed_task_elapsed(age).unwrap_or_else(|| panic!("not an activity age: {age:?}"))
}

/// Acceptance 1 (spec #501): a yielded card's live background subagent row
/// carries its child's running tool and that call's age — the front task
/// panel's own vocabulary — after the row's start clock, and never a total
/// elapsed; the shell row stays exactly as it was (its elapsed, no fragment).
/// The child read rides the same gather the LIVE render uses (ticket #504); the
/// Session Sync passes below are what keep the waiting card's fragment fresh.
#[tokio::test]
async fn a_yielded_subagents_row_shows_its_childs_liveness() {
    let _wd = test_work_dir();
    let now = chrono::Utc::now().timestamp_millis();
    let live = waiting_shell_and_subagent(now - 60_000, now - 30_000);
    let (_dir, app, backend, platform) = scripted_app(vec![live], Some(SessionStatus::Idle)).await;
    let tool_started = now - 5_000;
    backend
        .given_transcript_after_build("ses_call_sub", vec![child_running_tool(tool_started, "bash")])
        .await;
    yield_waiting_card(&app, &platform, 2).await;
    // The yield's own rendering no longer establishes the fragment on its own —
    // since ticket #504 the LIVE render's gather does, before the card yields —
    // and the Session Sync passes below keep it fresh while the card waits.

    spawn_sync(&app);
    wait_for_card_update(
        &platform,
        "the subagent activity fragment",
        CardUpdates::Latest,
        |card| ledger_activity(card, "· subagent：**review the diff**").is_some(),
    )
    .await;

    let card = platform.updated_cards().await.last().cloned().unwrap();
    let activity = ledger_activity(&card, "· subagent：**review the diff**")
        .unwrap_or_else(|| panic!("the subagent row carries its child's activity: {card}"));
    let (name, age) = activity
        .split_once(' ')
        .unwrap_or_else(|| panic!("activity names the tool before its age: {activity:?}"));
    assert_eq!(name, "bash", "the running tool's own name: {activity:?}");
    assert!(
        age.ends_with('s'),
        "the age is second-granular at the yield: {activity:?}"
    );
    // The row reads type, bold label, start clock, then the fragment directly:
    // a subagent row renders no total elapsed.
    let row = card_text(&card)
        .lines()
        .find(|line| line.starts_with("· subagent：**review the diff** · "))
        .expect("the subagent row")
        .to_string();
    assert!(
        row.ends_with(&format!(" · {activity}")),
        "the fragment is appended after the start clock: {row:?}"
    );
    let segments: Vec<&str> = row.split(" · ").collect();
    let clock = segments
        .iter()
        .position(|seg| is_start_clock(seg))
        .expect("the start clock");
    assert!(
        segments.get(clock + 1).is_some_and(|seg| *seg == activity),
        "the fragment follows the start clock directly — no elapsed: {row:?}"
    );
    // The shell row keeps its elapsed and carries no activity fragment.
    let shell_row = card_text(&card)
        .lines()
        .find(|line| line.starts_with("· shell：**gh run watch** · "))
        .expect("the shell row")
        .to_string();
    assert!(
        shell_row.rsplit(" · ").next().is_some_and(is_total_elapsed),
        "a shell row keeps its elapsed and no fragment: {shell_row:?}"
    );
}

/// The phase form (spec #501, acceptance 1): with no tool running, the row
/// shows the child's phase and the age of its newest activity — the front
/// panel's own `回复中 12s` shape.
#[tokio::test]
async fn a_yielded_subagents_row_shows_its_childs_phase() {
    let _wd = test_work_dir();
    let now = chrono::Utc::now().timestamp_millis();
    let live = waiting_shell_and_subagent(now - 60_000, now - 30_000);
    let (_dir, app, backend, platform) = scripted_app(vec![live], Some(SessionStatus::Idle)).await;
    let replied_at = now - 5_000;
    backend
        .given_transcript_after_build("ses_call_sub", vec![child_replying(replied_at)])
        .await;
    yield_waiting_card(&app, &platform, 2).await;

    spawn_sync(&app);
    wait_for_card_update(&platform, "the subagent's phase", CardUpdates::Latest, |card| {
        ledger_activity(card, "· subagent：**review the diff**")
            .is_some_and(|activity| activity.starts_with("回复中 "))
    })
    .await;

    let card = platform.updated_cards().await.last().cloned().unwrap();
    let activity = ledger_activity(&card, "· subagent：**review the diff**").unwrap();
    assert!(
        activity.starts_with("回复中 ") && activity_age_secs(&activity) >= 5,
        "the phase names itself and keeps the newest activity's age: {activity:?}"
    );
}

/// Acceptance 2 (spec #501): one Session Sync pass reads each DISTINCT child at
/// most once — two live subagent tasks naming the same child read it once per
/// pass, never twice; and a pass spends no child read before it can place one
/// (the read count never exceeds the pass count, which the parent's own reads
/// count).
#[tokio::test]
async fn one_pass_reads_each_distinct_child_once() {
    let _wd = test_work_dir();
    let now = chrono::Utc::now().timestamp_millis();
    let shared_child = |started_at: i64, call_id: &str| BackgroundTask {
        tool: ToolIdentity {
            name: "subagent".into(),
            call_id: call_id.into(),
        },
        shell_id: None,
        child_id: Some("ses_shared".into()),
        started_at: Some(started_at),
    };
    let transcript = SessionTranscript::new(vec![
        user("msg_cola_anchor", 1_000, "跑两个审阅"),
        background_launch(
            2_000,
            "subagent",
            "call_sub_a",
            serde_json::json!({ "description": "review the diff" }),
        ),
        background_launch(
            2_100,
            "subagent",
            "call_sub_b",
            serde_json::json!({ "description": "review the tests" }),
        ),
    ])
    .with_executions(vec![execution(2_500)])
    .with_background_tasks(vec![
        shared_child(now - 20_000, "call_sub_a"),
        shared_child(now - 21_000, "call_sub_b"),
    ]);
    let (_dir, app, backend, platform) = scripted_app(vec![transcript], Some(SessionStatus::Idle)).await;
    backend
        .given_transcript_after_build("ses_shared", vec![child_running_tool(now - 5_000, "bash")])
        .await;
    yield_waiting_card(&app, &platform, 2).await;

    spawn_sync(&app);
    wait_for_card_update(
        &platform,
        "both rows' shared activity",
        CardUpdates::Latest,
        |card| {
            ledger_activity(card, "· subagent：**review the diff**").is_some()
                && ledger_activity(card, "· subagent：**review the tests**").is_some()
        },
    )
    .await;

    let calls = backend.transcript_calls.lock().await.clone();
    let passes = calls.iter().filter(|sid| *sid == "ses_test").count();
    let child_reads = calls.iter().filter(|sid| *sid == "ses_shared").count();
    assert!(
        child_reads > 0 && passes > 0,
        "the gather read the parent and the shared child: {calls:?}"
    );
    assert!(
        child_reads <= passes,
        "one pass reads a distinct child at most once (parent reads = passes): {calls:?}"
    );
}

/// Acceptance 2 (spec #501): a session with no live background subagent — a
/// lone shell task, V1's shape — spends no child read at all: the ledger's
/// gather names no child and never calls the backend. (V1 has no Background
/// Task facts at all, so its live list is empty and the same rule holds.)
#[tokio::test]
async fn a_session_without_background_subagents_spends_no_child_read() {
    let _wd = test_work_dir();
    let now = chrono::Utc::now().timestamp_millis();
    let live = waiting_shell(now - 60_000);
    let (_dir, app, backend, platform) = scripted_app(vec![live], Some(SessionStatus::Idle)).await;
    yield_one_task_card(&app, &platform, ctx("ses_test", "跑一下构建并审阅")).await;

    spawn_sync(&app);
    // Several passes with a live shell row: the elapsed ticks, the loop runs.
    wait_for_card_update(&platform, "the shell row ticking", CardUpdates::Latest, |card| {
        card_text(card).contains("· shell：**gh run watch**")
    })
    .await;
    tokio::time::sleep(Duration::from_millis(150)).await;

    let calls = backend.transcript_calls.lock().await.clone();
    assert!(
        calls.len() > 1 && calls.iter().all(|sid| sid == "ses_test"),
        "no task names a child, so only the session itself is read: {calls:?}"
    );
}

/// Acceptance 3 (spec #501): a child transcript read that fails keeps the
/// row's last successful fragment — the same tool and the SAME stored start, so
/// the rendered age keeps growing truthfully — and never ends the wait: no
/// invented value, no settle.
#[tokio::test]
async fn a_failed_child_read_keeps_the_last_fragment_and_grows_its_age() {
    let _wd = test_work_dir();
    let now = chrono::Utc::now().timestamp_millis();
    let live = waiting_shell_and_subagent(now - 120_000, now - 90_000);
    let (_dir, app, backend, platform) = scripted_app(vec![live], Some(SessionStatus::Idle)).await;
    backend
        .given_transcript_after_build("ses_call_sub", vec![child_running_tool(now - 80_000, "bash")])
        .await;
    yield_waiting_card(&app, &platform, 2).await;

    spawn_sync(&app);
    wait_for_card_update(
        &platform,
        "the successful fragment",
        CardUpdates::Latest,
        |card| ledger_activity(card, "· subagent：**review the diff**").is_some(),
    )
    .await;
    let first = ledger_activity(
        &platform.updated_cards().await.last().cloned().unwrap(),
        "· subagent：**review the diff**",
    )
    .unwrap();
    let first_age = activity_age_secs(&first);

    // The child read now fails: the fragment stays (same tool, same stored
    // start) and its age keeps growing with the wall clock.
    backend.fail_transcript_for("ses_call_sub").await;
    tokio::time::sleep(Duration::from_millis(1_200)).await;

    let card = platform.updated_cards().await.last().cloned().unwrap();
    assert!(
        backend
            .transcript_calls
            .lock()
            .await
            .iter()
            .filter(|sid| *sid == "ses_call_sub")
            .count()
            > 1,
        "the failed read was retried, not frozen away"
    );
    let later = ledger_activity(&card, "· subagent：**review the diff**")
        .unwrap_or_else(|| panic!("the failed read keeps the last fragment: {card}"));
    assert_eq!(
        later.split_once(' ').unwrap().0,
        "bash",
        "the fragment keeps the tool the last successful read named: {later:?}"
    );
    assert!(
        activity_age_secs(&later) > first_age,
        "the age keeps growing on the stored start ({first:?} -> {later:?})"
    );
    assert!(
        !card_text(&card).contains("状态待确认"),
        "a failed child read is not a runtime verdict: {card}"
    );
    assert_eq!(
        Turn::card_state(&app.cards_handle(), "ses_test").await,
        Some(CardState::Waiting),
        "a failed child read never ends the wait"
    );

    // The read heals: the next successful gather replaces the stale fragment —
    // here the tool settled and the child reads as thinking.
    backend
        .given_transcript_after_build(
            "ses_call_sub",
            vec![SessionTranscript::new(vec![typed_message(
                "a_child",
                MessageRole::Assistant,
                Some(now - 2_000),
                vec![tool_part(
                    "bash",
                    "call_child",
                    ToolStatus::Completed,
                    serde_json::json!({ "command": "cargo test" }),
                    "test result: ok",
                )],
            )])],
        )
        .await;
    backend.heal_transcript("ses_call_sub").await;
    wait_for_card_update(
        &platform,
        "the fragment after the read heals",
        CardUpdates::Latest,
        |card| {
            ledger_activity(card, "· subagent：**review the diff**")
                .is_some_and(|activity| activity.starts_with("思考中 "))
        },
    )
    .await;
}

/// Acceptance 4 (spec #501): a row the runtime reconciliation could not
/// confirm renders no activity, whatever fragment it stores; once a later
/// read reports the child running again, the fragment returns (stored
/// timestamps untouched, so its age is still truthful).
#[tokio::test]
async fn an_unconfirmed_row_renders_no_activity_and_recovers_it() {
    let _wd = test_work_dir();
    let now = chrono::Utc::now().timestamp_millis();
    let live = waiting_shell_and_subagent(now - 60_000, now - 30_000);
    let (_dir, app, backend, platform) = scripted_app(vec![live], Some(SessionStatus::Idle)).await;
    backend
        .given_transcript_after_build("ses_call_sub", vec![child_running_tool(now - 10_000, "bash")])
        .await;
    yield_waiting_card(&app, &platform, 2).await;

    // The runtime reports the child inactive: its row gains the marker and
    // drops the activity.
    {
        let mut runtime = backend.task_runtime.lock().unwrap();
        runtime.shells = vec![("sh_call_bg".into(), ShellRuntime::Running)];
        runtime.children = vec![("ses_call_sub".into(), ChildRuntime::Inactive)];
    }
    spawn_sync(&app);
    wait_for_card_update(&platform, "the unconfirmed marker", CardUpdates::Latest, |card| {
        card_text(card).contains("⚠️ 状态待确认")
    })
    .await;
    let marked = platform.updated_cards().await.last().cloned().unwrap();
    let row = card_text(&marked)
        .lines()
        .find(|line| line.starts_with("· subagent：**review the diff**"))
        .unwrap_or_else(|| panic!("the subagent row: {marked}"))
        .to_string();
    assert!(
        row.ends_with("⚠️ 状态待确认") && !row.contains("bash"),
        "an unconfirmed row carries the marker and no activity: {row:?}"
    );

    // The next read confirms the child running again: the stored fragment
    // returns.
    backend.task_runtime.lock().unwrap().children = vec![("ses_call_sub".into(), ChildRuntime::Running)];
    wait_for_card_update(&platform, "the restored activity", CardUpdates::Latest, |card| {
        ledger_activity(card, "· subagent：**review the diff**").is_some()
    })
    .await;
    let restored = platform.updated_cards().await.last().cloned().unwrap();
    let activity = ledger_activity(&restored, "· subagent：**review the diff**").unwrap();
    assert!(
        activity.starts_with("bash ") && activity_age_secs(&activity) >= 10,
        "the fragment returns with its age still counting from the stored start: {activity:?}"
    );
}

/// Acceptance 6 (spec #501): a retiring subagent's activity leaves WITH its
/// row — the completion entry appears, and the fragment is nowhere on the card
/// afterwards (no ghost).
#[tokio::test]
async fn a_retired_subagents_activity_leaves_with_its_row() {
    let _wd = test_work_dir();
    let now = chrono::Utc::now().timestamp_millis();
    let live = waiting_shell_and_subagent(now - 60_000, now - 30_000);
    let (_dir, app, backend, platform) = scripted_app(vec![live], Some(SessionStatus::Idle)).await;
    backend
        .given_transcript_after_build("ses_call_sub", vec![child_running_tool(now - 20_000, "bash")])
        .await;
    yield_waiting_card(&app, &platform, 2).await;
    spawn_sync(&app);
    wait_for_card_update(&platform, "the fragment first", CardUpdates::Latest, |card| {
        ledger_activity(card, "· subagent：**review the diff**").is_some()
    })
    .await;

    // The subagent retires with its Wake while the shell stays live.
    script_transcript(
        &backend,
        vec![
            SessionTranscript::new(two_task_timeline(vec![]))
                .with_executions(vec![execution(2_500)])
                .with_wakes(vec![subagent_wake(3_500, "review the diff")])
                .with_background_tasks(vec![live_shell(now - 60_000, "call_bg")]),
        ],
    )
    .await;
    wait_for_card_update(
        &platform,
        "the subagent's completion entry",
        CardUpdates::Latest,
        |card| card_text(card).contains("🔔 subagent 完成：review the diff"),
    )
    .await;

    let card = platform.updated_cards().await.last().cloned().unwrap();
    let text = card_text(&card);
    assert!(
        !text.contains("· subagent：**review the diff**"),
        "the retired row is gone: {card}"
    );
    assert!(
        !text.contains("bash 2") && !text.contains("· bash"),
        "the activity fragment left with its row: {card}"
    );
    assert!(
        text.contains("⏳ 后台任务（1）") && text.contains("· shell：**gh run watch**"),
        "the shell's row and its list stay: {card}"
    );
    assert_eq!(
        Turn::card_state(&app.cards_handle(), "ses_test").await,
        Some(CardState::Waiting),
        "the shell keeps the card waiting"
    );
}

/// Acceptance 5 (spec #501): on the yielded 8 s read the activity age moves by
/// whole rendered seconds — the card is PATCHed when a rendered second turns,
/// never once per read — and the age never runs backwards.
#[tokio::test]
async fn a_yielded_rows_activity_age_advances_on_the_rendered_second() {
    let _wd = test_work_dir();
    let now = chrono::Utc::now().timestamp_millis();
    // The shell's clock is frozen; the child's activity age is the only
    // rendered number that moves (a subagent row renders no elapsed).
    let live = waiting_shell_and_subagent(frozen_start(now), now - 30_000);
    let (_dir, app, backend, platform) = scripted_app(vec![live], Some(SessionStatus::Idle)).await;
    backend
        .given_transcript_after_build("ses_call_sub", vec![child_running_tool(now - 30_000, "bash")])
        .await;
    yield_waiting_card(&app, &platform, 2).await;

    // The yield's own render reads no child; the Session Sync pass lands the
    // fragment as its first PATCH. Measure the rate from there.
    spawn_sync(&app);
    wait_for_card_update(&platform, "the fragment arriving", CardUpdates::Latest, |card| {
        ledger_activity(card, "· subagent：**review the diff**").is_some()
    })
    .await;
    let first_card = platform.updated_cards().await.last().cloned().unwrap();
    let first = activity_age_secs(
        &ledger_activity(&first_card, "· subagent：**review the diff**").expect("the fragment landed"),
    );
    let before = patches_to(&platform, "om_waiting").await.len();

    // ~60 passes at the injected cadence: at most one PATCH per advanced
    // rendered second (at most two in 1.2 s), never one per read.
    tokio::time::sleep(Duration::from_millis(1_200)).await;
    let patches = patches_to(&platform, "om_waiting").await;
    let advanced = &patches[before..];
    assert!(
        advanced.len() <= 4,
        "reads inside a rendered second must not PATCH the yielded card: {advanced:?}"
    );
    let mut previous = first;
    for patch in advanced {
        assert!(
            card_header(patch).contains("等待后台任务"),
            "the refresh never restyles the card: {patch}"
        );
        let age = activity_age_secs(
            &ledger_activity(patch, "· subagent：**review the diff**")
                .unwrap_or_else(|| panic!("the fragment stays across refreshes: {patch}")),
        );
        assert!(
            age >= previous,
            "the rendered activity age never runs backwards ({previous} -> {age}): {patch}"
        );
        previous = age;
    }
    assert!(
        previous > first,
        "the activity age advanced by rendered seconds over the window: {first} -> {previous}"
    );
}

// ---------------------------------------------------------------------------
// A live background subagent's child activity on the LIVE render path (spec
// #501, ticket #504): while the parent turn is still running and the card is
// live, the render poll calls the same shared gather the yielded refresh uses —
// one child read per live subagent and one pending-wait query — so the row is
// as fresh as the card and its age is measured at the render's own clock. The
// live path's whole-minute clock gate still holds: the age's seconds never owe
// a PATCH by themselves, while a typed change does (pinned in `state.rs`), and
// a read that names no child spends no request at all (V1 included).
// ---------------------------------------------------------------------------

/// Acceptance 1 (spec #501): a live card's background subagent row carries its
/// child's running tool and age — the fragment appended after the row's start
/// clock and no total elapsed, never to the shell row — while the parent turn
/// is still running.
#[tokio::test]
async fn a_live_subagents_row_shows_its_childs_liveness() {
    let _wd = test_work_dir();
    let now = chrono::Utc::now().timestamp_millis();
    let transcript = SessionTranscript::new(vec![
        user("msg_cola_anchor", 1_000, "跑一下构建并审阅"),
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
    ])
    .with_background_tasks(vec![
        live_shell(now - 60_000, "call_bg"),
        live_subagent(now - 30_000, "call_sub"),
    ]);
    let (_dir, app, backend, platform) = scripted_app(vec![transcript], Some(SessionStatus::Busy)).await;
    backend
        .given_transcript_after_build("ses_call_sub", vec![child_running_tool(now - 5_000, "bash")])
        .await;

    let _turn = spawn_turn(&app, ctx("ses_test", "跑一下构建并审阅"));
    wait_for_card_update(
        &platform,
        "the live subagent activity",
        CardUpdates::Latest,
        |card| {
            ledger_activity(card, "· subagent：**review the diff**")
                .is_some_and(|activity| activity.starts_with("bash "))
        },
    )
    .await;

    let card = platform.updated_cards().await.last().cloned().unwrap();
    let activity = ledger_activity(&card, "· subagent：**review the diff**")
        .unwrap_or_else(|| panic!("the live subagent row carries its child's activity: {card}"));
    let (name, age) = activity
        .split_once(' ')
        .unwrap_or_else(|| panic!("activity names the tool before its age: {activity:?}"));
    assert_eq!(name, "bash", "the running tool's own name: {activity:?}");
    assert!(
        age.ends_with('s') && activity_age_secs(&activity) >= 5,
        "the live fragment's age is second-granular and true: {activity:?}"
    );
    // The row keeps its start clock, with the fragment directly after it — a
    // subagent row renders no total elapsed.
    let row = card_text(&card)
        .lines()
        .find(|line| line.starts_with("· subagent：**review the diff** · "))
        .expect("the subagent row")
        .to_string();
    let segments: Vec<&str> = row.split(" · ").collect();
    let clock = segments
        .iter()
        .position(|seg| is_start_clock(seg))
        .expect("the start clock");
    assert!(
        segments.get(clock + 1).is_some_and(|seg| *seg == activity),
        "the fragment follows the start clock directly — no elapsed: {row:?}"
    );
    // The shell row keeps its elapsed and no fragment, and the card is still
    // the LIVE one.
    let shell_row = card_text(&card)
        .lines()
        .find(|line| line.starts_with("· shell：**gh run watch** · "))
        .expect("the shell row")
        .to_string();
    assert!(
        shell_row.rsplit(" · ").next().is_some_and(is_total_elapsed),
        "a shell row keeps its elapsed and no fragment: {shell_row:?}"
    );
    assert!(
        !card_header(&card).contains("完成"),
        "the row rides the live card, not a settled one: {card}"
    );
}

/// Acceptance 2 (spec #501): the fragment follows the child on the card's own
/// refresh — a later read of the same row renders the child's new typed
/// activity (its `回复中` phase, then the running tool), each age measured at
/// that render's clock.
#[tokio::test]
async fn a_live_subagents_row_refreshes_to_the_childs_new_activity() {
    let _wd = test_work_dir();
    let now = chrono::Utc::now().timestamp_millis();
    let transcript = SessionTranscript::new(vec![
        user("msg_cola_anchor", 1_000, "跑一下审阅"),
        background_launch(
            2_000,
            "subagent",
            "call_sub",
            serde_json::json!({ "description": "review the diff" }),
        ),
    ])
    .with_background_tasks(vec![live_subagent(now - 30_000, "call_sub")]);
    let (_dir, app, backend, platform) = scripted_app(vec![transcript], Some(SessionStatus::Busy)).await;
    backend
        .given_transcript_after_build("ses_call_sub", vec![child_replying(now - 3_000)])
        .await;

    let _turn = spawn_turn(&app, ctx("ses_test", "跑一下审阅"));
    wait_for_card_update(&platform, "the child's phase", CardUpdates::Latest, |card| {
        ledger_activity(card, "· subagent：**review the diff**")
            .is_some_and(|activity| activity.starts_with("回复中 "))
    })
    .await;

    // The child starts a tool: the next renders carry the typed change.
    backend
        .given_transcript_after_build("ses_call_sub", vec![child_running_tool(now - 1_000, "bash")])
        .await;
    wait_for_card_update(
        &platform,
        "the child's new activity",
        CardUpdates::Latest,
        |card| {
            ledger_activity(card, "· subagent：**review the diff**")
                .is_some_and(|activity| activity.starts_with("bash "))
        },
    )
    .await;
    let card = platform.updated_cards().await.last().cloned().unwrap();
    let activity = ledger_activity(&card, "· subagent：**review the diff**").unwrap();
    assert!(
        activity_age_secs(&activity) >= 1,
        "the new fragment counts from the new tool's start: {activity:?}"
    );
}

/// Acceptance 1 (spec #501): the live row's age is measured at each render's
/// own clock — a later card update of the same read shows it advancing, never
/// frozen on the read that established the fragment.
#[tokio::test]
async fn a_live_subagents_activity_age_advances_with_the_render_clock() {
    let _wd = test_work_dir();
    let now = chrono::Utc::now().timestamp_millis();
    let transcript = SessionTranscript::new(vec![
        user("msg_cola_anchor", 1_000, "跑一下审阅"),
        background_launch(
            2_000,
            "subagent",
            "call_sub",
            serde_json::json!({ "description": "review the diff" }),
        ),
    ])
    .with_background_tasks(vec![live_subagent(now - 30_000, "call_sub")]);
    let (_dir, app, backend, platform) = scripted_app(vec![transcript], Some(SessionStatus::Busy)).await;
    backend
        .given_transcript_after_build("ses_call_sub", vec![child_running_tool(now - 5_000, "bash")])
        .await;

    let _turn = spawn_turn(&app, ctx("ses_test", "跑一下审阅"));
    wait_for_card_update(&platform, "the fragment first", CardUpdates::Latest, |card| {
        ledger_activity(card, "· subagent：**review the diff**").is_some()
    })
    .await;
    let first = activity_age_secs(
        &ledger_activity(
            &platform.updated_cards().await.last().cloned().unwrap(),
            "· subagent：**review the diff**",
        )
        .expect("the fragment landed"),
    );

    wait_for_card_update(
        &platform,
        "the render clock advancing the age",
        CardUpdates::Latest,
        |card| {
            ledger_activity(card, "· subagent：**review the diff**")
                .is_some_and(|activity| activity_age_secs(&activity) > first)
        },
    )
    .await;
}

/// Acceptance 4 (spec #501): a live session whose ledger names no child — a
/// lone shell, or V1 with no Background Task facts at all — spends no child
/// read: the render's gather is empty and only the session itself is read.
#[tokio::test]
async fn a_live_session_without_background_subagents_spends_no_child_read() {
    let _wd = test_work_dir();
    let now = chrono::Utc::now().timestamp_millis();
    let transcript = SessionTranscript::new(vec![
        user("msg_cola_anchor", 1_000, "跑一下构建"),
        background_launch(
            2_000,
            "shell",
            "call_bg",
            serde_json::json!({ "command": "npm run build" }),
        ),
    ])
    .with_background_tasks(vec![live_shell(now - 60_000, "call_bg")]);
    let (_dir, app, backend, platform) = scripted_app(vec![transcript], Some(SessionStatus::Busy)).await;

    let _turn = spawn_turn(&app, ctx("ses_test", "跑一下构建"));
    wait_for_card_update(&platform, "the live shell row", CardUpdates::Latest, |card| {
        card_text(card).contains("· shell：**npm run build**")
    })
    .await;
    // Several more renders at the injected cadence with a live row: the live
    // path still never names a child, so it reads only the session itself.
    tokio::time::sleep(Duration::from_millis(150)).await;

    let calls = backend.transcript_calls.lock().await.clone();
    assert!(
        calls.len() > 1 && calls.iter().all(|sid| sid == "ses_test"),
        "no task names a child, so only the session itself is read: {calls:?}"
    );
}
