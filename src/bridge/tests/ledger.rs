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
    BackgroundTask, ChildEvidence, ChildRuntime, ContentBlock, FinishReason, MessageId, MessageRole, Part,
    SessionTranscript, ShellEnd, ShellRuntime, StepFinish, ToolCall, ToolIdentity, ToolOutput, ToolStatus,
    TranscriptMessage, Wake, WakeSource,
};
use crate::bridge::chain::ChainRecords;
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

/// Acceptance 5 (#589 keeps it under the shared step): V1 carries no
/// Background Task facts — a V1 `task` call that looks backgrounded renders
/// its panel and never a ledger section, and the empty list short-circuits the
/// shared runtime reconcile before any request.
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
    let (_dir, app, backend, platform) = scripted_app(vec![transcript], Some(SessionStatus::Busy)).await;

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
    assert!(
        backend.task_runtime_calls.lock().await.is_empty(),
        "a read with no live task spends no runtime request, whatever the generation"
    );
    // The child-evidence step rides the same guard (#591): with no live task
    // there is no suspect, so no child read is ever issued on V1.
    assert!(
        backend.child_evidence_calls.lock().await.is_empty(),
        "no live task, no child evidence request: {:?}",
        backend.child_evidence_calls.lock().await
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

/// Spec #602, ticket #607: the quiet in-place settle is gated identically. A
/// settle PATCH that failed recoverably is owed as a Pending Card Update, so
/// the notice waits for the drain instead of announcing over an ending that is
/// not yet on the card.
#[tokio::test]
async fn a_quiet_settle_defers_the_notice_until_the_retry_drains() {
    let _wd = test_work_dir();
    let now = chrono::Utc::now().timestamp_millis();
    let live = waiting_shell(now - 5_000);
    let (_dir, app, backend, platform) = scripted_app(vec![live], Some(SessionStatus::Idle)).await;
    let mut context = ctx("ses_test", "跑一下构建并审阅");
    context.is_group = true;
    context.requester_open_id = Some(TEST_HOST.to_string());
    yield_one_task_card(&app, &platform, context).await;

    // The settle PATCH fails recoverably: the ending is owed, so the notice
    // must wait. (BACKOFF_BASE is 5 s, so no sync-tick drain retries it within
    // this test's window — the count is not consumed behind the assertion.)
    platform.fail_update_transport_count.store(100, Ordering::SeqCst);
    script_quiet_true_end(&backend).await;
    spawn_sync(&app);
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while Turn::card_state(&app.cards_handle(), "ses_test").await != Some(CardState::Done) {
        assert!(
            tokio::time::Instant::now() < deadline,
            "the quiet true end never settled the card"
        );
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    // Let the settle's own announce land (it trails the PATCH it gates): with
    // the gate armed nothing is sent, but a send outside the gate would surface.
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(
        !noticed(&platform).await,
        "an owed settle must not announce before it drains: {:?}",
        platform.calls.lock().await
    );

    // Feishu returns: the drain delivers the owed update, and the notice fires.
    platform.fail_update_transport_count.store(0, Ordering::SeqCst);
    app.core.feishu.drain_pending_card_updates(true).await;
    let notices = wait_for_notice(&platform).await;
    assert_eq!(notices.len(), 1, "exactly one notice: {notices:?}");
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

/// Issue #454 review, kept under the shared step (#589): the Session Sync
/// reconcile observes a retirement only while a card can still receive the
/// ledger. With no card — a settled chain, or right after a restart —
/// observing would record the task and swallow its entry (the real restart
/// retired at 02:25:13 with no card and showed nothing). The Turn's own live
/// card is the observer that serves it now, through the drain read: the entry
/// renders there and the turn settles ✅ directly.
#[tokio::test]
async fn a_runtime_retirement_waits_for_a_card_that_can_render_it() {
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
        "no card means no Session Sync observation"
    );

    // The Turn's own card now exists: its drain read observes the retirement,
    // renders the entry, and the settle is ✅ directly.
    Turn::run(&app.turn_handles(), ctx("ses_test", "跑一下构建并审阅"))
        .await
        .unwrap();
    assert_eq!(
        Turn::card_state(&app.cards_handle(), "ses_test").await,
        Some(CardState::Done),
        "the live card's read observed the retirement"
    );
    let final_card = platform.updated_cards().await.last().cloned().unwrap();
    assert!(
        card_text(&final_card).contains("🔔 shell 已失联：gh run watch"),
        "the retirement entry renders on the observing card: {final_card}"
    );
    assert!(
        !backend.task_runtime_calls.lock().await.is_empty(),
        "the live card's read observed the retirement"
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

/// Ticket #589: the runtime reconcile runs on the Turn's own drain read, so a
/// task the runtime confirms ended mid-turn — no Wake ever arrives — loses its
/// row in place, renders its 结束 entry exactly once on the live card, and the
/// settle that follows is ✅ directly. Session Sync used to be the only
/// reconciler and it skips an inflight session, so the turn yielded
/// 「⏳ 等待后台任务」 first and only a later pass retired the task.
#[tokio::test]
async fn a_task_ended_mid_turn_settles_the_turn_directly() {
    let _wd = test_work_dir();
    let now = chrono::Utc::now().timestamp_millis();
    let live = waiting_shell(now - 5_000);
    let (_dir, app, backend, platform) = scripted_app(vec![live], Some(SessionStatus::Idle)).await;
    // The runtime reports the shell killed while the Turn is still live; no
    // Wake will ever retire it. The transcript stays as scripted and keeps
    // listing the task, so only the runtime read can end the wait.
    backend.task_runtime.lock().unwrap().shells = vec![(
        "sh_call_bg".into(),
        ShellRuntime::Ended {
            end: ShellEnd::Killed,
            completed_at: Some(now - 1_000),
        },
    )];

    Turn::run(&app.turn_handles(), ctx("ses_test", "跑一下构建并审阅"))
        .await
        .unwrap();

    assert_eq!(
        Turn::card_state(&app.cards_handle(), "ses_test").await,
        Some(CardState::Done),
        "a task observed ended mid-turn ends the turn directly"
    );
    // No intermediate wait: the ledger never showed 「⏳ 等待后台任务」, and the
    // turn never yielded Waiting on the way to the settle.
    let updates = platform.updated_cards().await;
    assert!(
        updates
            .iter()
            .all(|card| !card_header(card).contains("等待后台任务")),
        "the settle is direct — no waiting detour: {updates:?}"
    );
    let final_card = updates.last().cloned().unwrap();
    let text = card_text(&final_card);
    assert!(
        !text.contains("后台任务（"),
        "the retired task's row left the live ledger: {final_card}"
    );
    assert_eq!(
        text.matches("🔔 shell 结束：gh run watch").count(),
        1,
        "exactly one retirement entry renders on the live card: {final_card}"
    );
    assert!(
        text.contains("shell sh_call_bg · "),
        "the entry carries the task's identity: {final_card}"
    );
    // Review (spec #588 / #589, PR #595): the retirement is committed to the
    // process-local overlay only once the drain's render carried its entry —
    // the shared record-after-flush invariant, pinned on this path.
    assert_eq!(
        backend.overlay.retired_call_ids("ses_test"),
        vec!["call_bg".to_string()],
        "the drain's accepted render records the retirement"
    );
}

/// Review (spec #588, PR #595): the record-after-flush gate reads the real
/// write outcome, not the accumulator's survival. Feishu permanently refuses
/// the flush that carries the retirement entry — the plain attempt AND its
/// fenced retry — so the card suspends and nothing will ever carry the entry.
/// Recording the retirement anyway would hide the task from every later read
/// with no entry ever rendered; the task must stay live, and the next card
/// that CAN render its entry claims it — exactly once.
#[tokio::test]
async fn a_permanently_refused_retirement_write_records_nothing() {
    let _wd = test_work_dir();
    let now = chrono::Utc::now().timestamp_millis();
    let live = waiting_shell(now - 5_000);
    let (_dir, app, backend, platform) = scripted_app(vec![live], Some(SessionStatus::Idle)).await;
    // The runtime confirms the shell ended mid-turn; no Wake will ever retire
    // it. The transcript stays as scripted and keeps listing the task, so only
    // the runtime read can end it.
    backend.task_runtime.lock().unwrap().shells = vec![(
        "sh_call_bg".into(),
        ShellRuntime::Ended {
            end: ShellEnd::Killed,
            completed_at: Some(now - 1_000),
        },
    )];
    // Feishu refuses the content of both the plain write and the fenced retry.
    platform.fail_update_card_content_count.store(2, Ordering::SeqCst);

    Turn::run(&app.turn_handles(), ctx("ses_test", "跑一下构建并审阅"))
        .await
        .unwrap();

    // The card is suspended exactly as before: two attempts, then no more.
    assert!(
        Turn::card_is_suspended(&app.cards_handle(), "ses_test").await,
        "the doubly-refused card suspends"
    );
    assert_eq!(
        patches_to(&platform, "msg_reply").await.len(),
        2,
        "the plain attempt and the fenced one only"
    );
    // Nothing may be recorded: the task stays live on every later read ...
    assert!(
        backend.overlay.retired_call_ids("ses_test").is_empty(),
        "a permanently refused write records nothing: {:?}",
        backend.overlay.retired_call_ids("ses_test")
    );
    let later = crate::backend::Backend::transcript(backend.as_ref(), "ses_test")
        .await
        .unwrap();
    assert_eq!(
        later
            .background_tasks
            .iter()
            .map(|task| task.tool.call_id.as_str())
            .collect::<Vec<_>>(),
        vec!["call_bg"],
        "the task stays live until a card that can render its entry claims it"
    );

    // ... and the next card that CAN render it claims it exactly once: a new
    // Turn on the same session still reads the live task, the runtime still
    // confirms the end, and this accepted write lands the entry.
    platform.fail_update_card_content_count.store(0, Ordering::SeqCst);
    // Name the refused card, so its updates cannot be confused with the
    // successor's (the harness serves one id per send).
    Turn::set_card_message_id(&app.cards_handle(), "ses_test", "om_refused").await;
    script_transcript(
        &backend,
        vec![
            SessionTranscript::new(two_task_timeline(vec![user(
                "msg_cola_second",
                3_000,
                "再来一次",
            )]))
            .with_executions(vec![execution(2_500)])
            .with_background_tasks(vec![live_shell(now - 3_000, "call_bg")]),
        ],
    )
    .await;
    let mut second = ctx("ses_test", "再来一次");
    second.message_id = "msg_2".into();
    second.cola_message_id = Some("msg_cola_second".into());
    spawn_turn(&app, second).await.unwrap().unwrap();

    wait_for_card_update(
        &platform,
        "the successor's retirement entry",
        CardUpdates::Latest,
        |card| card_text(card).contains("🔔 shell 结束：gh run watch"),
    )
    .await;
    let successor = platform.updated_cards().await.last().cloned().unwrap();
    assert_eq!(
        card_text(&successor)
            .matches("🔔 shell 结束：gh run watch")
            .count(),
        1,
        "the entry renders exactly once on the card that can carry it: {successor}"
    );
    assert_eq!(
        backend.overlay.retired_call_ids("ses_test"),
        vec!["call_bg".to_string()],
        "the accepted successor render records the retirement"
    );
}

/// Review (spec #588, PR #595): a recoverable refusal is not a permanent one.
/// The delivery layer queues the failed write and keeps retrying it (ADR-0067),
/// so the carrying payload is accepted: the drain's reconcile pass still
/// records, and the queued write converges the card later.
#[tokio::test]
async fn a_queued_retirement_write_still_records() {
    let _wd = test_work_dir();
    let now = chrono::Utc::now().timestamp_millis();
    let live = waiting_shell(now - 5_000);
    let (_dir, app, backend, platform) = scripted_app(vec![live], Some(SessionStatus::Idle)).await;
    backend.task_runtime.lock().unwrap().shells = vec![(
        "sh_call_bg".into(),
        ShellRuntime::Ended {
            end: ShellEnd::Killed,
            completed_at: Some(now - 1_000),
        },
    )];
    // A transport failure (recoverable): the delivery layer owns the retry.
    platform.fail_update_transport_count.store(1, Ordering::SeqCst);

    Turn::run(&app.turn_handles(), ctx("ses_test", "跑一下构建并审阅"))
        .await
        .unwrap();

    assert_eq!(
        backend.overlay.retired_call_ids("ses_test"),
        vec!["call_bg".to_string()],
        "a queued-for-retry write is accepted: the pass records"
    );
}

/// Review (spec #588, PR #595): the yielded-card refresh reports what really
/// became of its write. Feishu permanently refuses the refresh that carries the
/// retirement entry — the plain attempt AND its fenced retry — so the card
/// suspends and the entry can never land. The pass must not be recorded, the
/// task must stay live for a card that CAN render its entry, and the suspended
/// card must not be PATCHed again; `Refreshed`/`Settled` stay reserved for an
/// accepted write.
#[tokio::test]
async fn a_permanently_refused_yielded_refresh_records_nothing() {
    let _wd = test_work_dir();
    let now = chrono::Utc::now().timestamp_millis();
    // A frozen row clock: no second-granular ledger churn can add a PATCH
    // while the assertion window is open.
    let live = waiting_shell(frozen_start(now));
    let (_dir, app, backend, platform) = scripted_app(vec![live], Some(SessionStatus::Idle)).await;
    yield_one_task_card(&app, &platform, ctx("ses_test", "跑一下构建并审阅")).await;

    // The runtime confirms the end on the next Session Sync pass; the pass's
    // in-place PATCH is refused on both the plain attempt and the fenced one.
    backend.task_runtime.lock().unwrap().shells = vec![(
        "sh_call_bg".into(),
        ShellRuntime::Ended {
            end: ShellEnd::Killed,
            completed_at: Some(now - 1_000),
        },
    )];
    platform.fail_update_card_content_count.store(2, Ordering::SeqCst);
    spawn_sync(&app);

    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while !Turn::card_is_suspended(&app.cards_handle(), "ses_test").await {
        assert!(
            tokio::time::Instant::now() < deadline,
            "the doubly-refused refresh never suspended the card"
        );
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    // The suspended-card handling is exactly as before: the plain attempt and
    // its fenced retry are the card's only writes (both recorded, both
    // refused), and no later pass PATCHes the suspended card again.
    assert_eq!(
        patches_to(&platform, "om_waiting").await.len(),
        2,
        "the plain attempt and its fenced retry only"
    );
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(
        patches_to(&platform, "om_waiting").await.len(),
        2,
        "a suspended card is not PATCHed again"
    );

    // Nothing was recorded: the task stays live, so a later receivable card can
    // still render its entry ...
    assert!(
        backend.overlay.retired_call_ids("ses_test").is_empty(),
        "a permanently refused refresh records nothing: {:?}",
        backend.overlay.retired_call_ids("ses_test")
    );
    let later = crate::backend::Backend::transcript(backend.as_ref(), "ses_test")
        .await
        .unwrap();
    assert_eq!(
        later
            .background_tasks
            .iter()
            .map(|task| task.tool.call_id.as_str())
            .collect::<Vec<_>>(),
        vec!["call_bg"],
        "the task stays live until a card that can render its entry claims it"
    );
}

/// Acceptance (#589): the runtime reconcile is one process-wide verdict per
/// Session per interval, shared by every path — and a waiting card's
/// retirement still lands per ADR-0065 once the cadence admits it. The Turn's
/// own drain read spends the first attempt; with the interval pinned wide, no
/// number of Session Sync passes may spend another, and the card keeps its
/// wait. Admitting the next attempt (the injectable knob) retires the task,
/// renders its entry, and settles the card in place with ONE PATCH.
#[tokio::test]
async fn the_shared_throttle_bounds_the_waiting_cards_reconcile() {
    let _wd = test_work_dir();
    let now = chrono::Utc::now().timestamp_millis();
    // A start ahead of cola's clock: the row's elapsed clamps to 0m00s, so no
    // second-granular clock churn can add a PATCH while the throttle holds.
    let live = waiting_shell(frozen_start(now));
    let (_dir, app, backend, platform) = scripted_app(vec![live], Some(SessionStatus::Idle)).await;
    // The turn's drain read spends its attempt on the (empty) runtime and the
    // card yields waiting — one live task, no verdict.
    yield_one_task_card(&app, &platform, ctx("ses_test", "跑一下构建并审阅")).await;

    // The runtime now reports the shell ended, but the interval is pinned
    // wide: the drain already spent this Session's verdict, so no Session Sync
    // pass may spend another — the shared gate bounds both paths together.
    backend.task_runtime_calls.lock().await.clear();
    backend.task_runtime.lock().unwrap().shells = vec![(
        "sh_call_bg".into(),
        ShellRuntime::Ended {
            end: ShellEnd::Killed,
            completed_at: Some(now - 1_000),
        },
    )];
    app.runtime_reconcile
        .interval_ms
        .store(60_000, std::sync::atomic::Ordering::Relaxed);
    spawn_sync(&app);
    tokio::time::sleep(Duration::from_millis(150)).await;
    assert!(
        backend.task_runtime_calls.lock().await.is_empty(),
        "the shared throttle bounds every path's runtime read"
    );
    assert_eq!(
        Turn::card_state(&app.cards_handle(), "ses_test").await,
        Some(CardState::Waiting),
        "no verdict means no retirement: the wait keeps its card"
    );

    // The cadence admits the next attempt: the deferred retirement retires the
    // task, renders its entry, and settles the waiting card ✅ in place.
    app.runtime_reconcile
        .interval_ms
        .store(0, std::sync::atomic::Ordering::Relaxed);
    wait_for_card_update(&platform, "the settled card", CardUpdates::Latest, |card| {
        card_header(card).contains("✅") && card_text(card).contains("🔔 shell 结束：gh run watch")
    })
    .await;
    assert_eq!(
        Turn::card_state(&app.cards_handle(), "ses_test").await,
        Some(CardState::Done),
        "the admitted verdict settles the waiting card"
    );
    let patches = patches_to(&platform, "om_waiting").await;
    assert_eq!(
        patches.len(),
        1,
        "the deferred retirement is still one in-place PATCH: {patches:?}"
    );
    assert!(
        !card_text(&patches[0]).contains("后台任务（"),
        "the retired task's live list is gone: {:?}",
        patches[0]
    );
}

/// Issue #454, extended by #591: a subagent the runtime reports INACTIVE is
/// evidence, not an ending — the row gains the 待确认 marker, the card stays
/// waiting, and only its own terminal child transcript (#591), its Wake, the
/// cleanup button or the user's own decision can retire it. Its evidence read
/// is spent on the suspect alone and yields nothing here.
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
    // The suspect's own transcript was read for evidence — on the suspect
    // alone — and yielded none, so the marker stands.
    let evidence_reads = backend.child_evidence_calls.lock().await.clone();
    assert!(
        !evidence_reads.is_empty() && evidence_reads.iter().all(|child| child == "ses_call_sub"),
        "the child evidence read was spent on the suspect alone: {evidence_reads:?}"
    );
}

/// Spec #588 / #591: a subagent the runtime no longer reports active is
/// retirable on its OWN child transcript's evidence — the terminal step finish
/// (with its completion stamp) its newest assistant message carries, which is
/// exactly the read a missing Wake leaves as the only way out. One light read
/// per suspect per cycle: the row leaves, the card records
/// 「🔔 subagent 结束：<描述>」 exactly once and settles ✅ in place, and the
/// retirement is overlay-recorded so no later read resurrects the task.
#[tokio::test]
async fn a_terminal_child_settles_the_waiting_card() {
    let _wd = test_work_dir();
    let now = chrono::Utc::now().timestamp_millis();
    let live = SessionTranscript::new(two_task_timeline(vec![]))
        .with_executions(vec![execution(2_500)])
        .with_background_tasks(vec![live_subagent(now - 3_000, "call_sub")]);
    let (_dir, app, backend, platform) = scripted_app(vec![live], Some(SessionStatus::Idle)).await;
    script_child_without_activity(&backend).await;
    yield_one_task_card(&app, &platform, ctx("ses_test", "跑一下构建并审阅")).await;
    let cards_before = created_cards(&platform).await.len();

    // The runtime no longer lists the child active; the child's own newest
    // assistant message shows the run finished at `finished`.
    let finished = now - 1_000;
    backend.task_runtime.lock().unwrap().children = vec![("ses_call_sub".into(), ChildRuntime::Inactive)];
    backend
        .with_child_evidence(
            "ses_call_sub",
            ChildEvidence::Terminal {
                completed_at: finished,
            },
        )
        .await;
    spawn_sync(&app);
    wait_for_card_update(&platform, "the settled card", CardUpdates::Latest, |card| {
        card_header(card).contains("✅") && card_text(card).contains("🔔 subagent 结束：review the diff")
    })
    .await;

    assert_eq!(
        Turn::card_state(&app.cards_handle(), "ses_test").await,
        Some(CardState::Done),
        "the child's own terminal evidence is the card's true end"
    );
    let patches = patches_to(&platform, "om_waiting").await;
    let settled = patches.last().cloned().unwrap();
    let text = card_text(&settled);
    assert!(
        !text.contains("后台任务（") && !text.contains("状态待确认"),
        "the retired suspect's row is gone: {settled}"
    );
    assert_eq!(
        text.matches("🔔 subagent 结束：review the diff").count(),
        1,
        "exactly one retirement entry renders: {settled}"
    );
    assert!(
        text.contains("subagent ses_call_sub · "),
        "the entry carries the child's identity and its clock: {settled}"
    );
    assert!(
        continuation_sends(&platform).await.is_empty()
            && created_cards(&platform).await.len() == cards_before,
        "an evidence settle posts nothing: {:?}",
        platform.calls.lock().await
    );
    // Exactly one light read, on the one suspect — never a transcript read.
    assert_eq!(
        backend.child_evidence_calls.lock().await.as_slice(),
        ["ses_call_sub".to_string()],
        "one evidence read per suspected child per cycle"
    );
    // The retirement is overlay-recorded: a later read of the same scripted
    // transcript no longer lists the task (#591 review — without this the next
    // live card would resurrect it).
    let later = crate::backend::Backend::transcript(backend.as_ref(), "ses_test")
        .await
        .unwrap();
    assert!(
        later.background_tasks.is_empty(),
        "the retirement overlay filters every later read"
    );
}

/// Spec #588 / #591: a child session the server no longer knows (404) is
/// evidence too — the suspect retires as 「🔔 subagent 已失联：<描述>」 with no
/// invented clock, and the last task's retirement settles the wait ✅ in place.
#[tokio::test]
async fn a_gone_child_retires_as_lost() {
    let _wd = test_work_dir();
    let now = chrono::Utc::now().timestamp_millis();
    let live = SessionTranscript::new(two_task_timeline(vec![]))
        .with_executions(vec![execution(2_500)])
        .with_background_tasks(vec![live_subagent(now - 3_000, "call_sub")]);
    let (_dir, app, backend, platform) = scripted_app(vec![live], Some(SessionStatus::Idle)).await;
    script_child_without_activity(&backend).await;
    yield_one_task_card(&app, &platform, ctx("ses_test", "跑一下构建并审阅")).await;

    backend.task_runtime.lock().unwrap().children = vec![("ses_call_sub".into(), ChildRuntime::Inactive)];
    backend
        .with_child_evidence("ses_call_sub", ChildEvidence::Gone)
        .await;
    spawn_sync(&app);
    wait_for_card_update(&platform, "the lost settle", CardUpdates::Latest, |card| {
        card_header(card).contains("✅") && card_text(card).contains("🔔 subagent 已失联：review the diff")
    })
    .await;

    assert_eq!(
        Turn::card_state(&app.cards_handle(), "ses_test").await,
        Some(CardState::Done),
        "a gone child cannot hold the wait"
    );
    let patches = patches_to(&platform, "om_waiting").await;
    let settled = patches.last().cloned().unwrap();
    let text = card_text(&settled);
    assert!(
        text.contains("subagent ses_call_sub") && !text.contains("subagent ses_call_sub · "),
        "a lost entry carries identity but no invented clock: {settled}"
    );
}

/// Acceptance (#591): a failed child-evidence read is no verdict — the row
/// keeps its ⚠️ 状态待确认 marker, no entry renders, and nothing settles; the
/// next cycle retries the read.
#[tokio::test]
async fn a_failed_child_evidence_read_keeps_the_row_unconfirmed() {
    let _wd = test_work_dir();
    let now = chrono::Utc::now().timestamp_millis();
    let live = SessionTranscript::new(two_task_timeline(vec![]))
        .with_executions(vec![execution(2_500)])
        .with_background_tasks(vec![live_subagent(now - 3_000, "call_sub")]);
    let (_dir, app, backend, platform) = scripted_app(vec![live], Some(SessionStatus::Idle)).await;
    script_child_without_activity(&backend).await;
    yield_one_task_card(&app, &platform, ctx("ses_test", "跑一下构建并审阅")).await;

    backend.task_runtime.lock().unwrap().children = vec![("ses_call_sub".into(), ChildRuntime::Inactive)];
    backend
        .fail_child_evidence_reads
        .store(usize::MAX, Ordering::SeqCst);
    spawn_sync(&app);
    wait_for_card_update(&platform, "the unconfirmed marker", CardUpdates::Latest, |card| {
        card_text(card).contains("⚠️ 状态待确认")
    })
    .await;

    assert_eq!(
        Turn::card_state(&app.cards_handle(), "ses_test").await,
        Some(CardState::Waiting),
        "an unreadable child never settles the card"
    );
    let latest = platform.updated_cards().await.last().cloned().unwrap();
    let text = card_text(&latest);
    assert!(
        text.contains("⏳ 后台任务（1 · 1 待确认）") && !text.contains("🔔 subagent"),
        "the marker stands and no entry renders: {latest}"
    );
    let calls = backend.child_evidence_calls.lock().await.clone();
    assert!(
        !calls.is_empty() && calls.iter().all(|child| child == "ses_call_sub"),
        "the read was attempted, on the suspect alone: {calls:?}"
    );
}

/// Acceptance (#591): the child evidence works on the live card too — a
/// subagent the runtime no longer reports active, whose own transcript shows
/// a terminal finish, ends the turn ✅ directly on the drain read: no
/// 「⏳ 等待后台任务」 detour, the row gone, one 「🔔 subagent 结束」 entry.
#[tokio::test]
async fn a_terminal_child_ends_the_turn_directly() {
    let _wd = test_work_dir();
    let now = chrono::Utc::now().timestamp_millis();
    let live = SessionTranscript::new(two_task_timeline(vec![]))
        .with_executions(vec![execution(2_500)])
        .with_background_tasks(vec![live_subagent(now - 3_000, "call_sub")]);
    let (_dir, app, backend, platform) = scripted_app(vec![live], Some(SessionStatus::Idle)).await;
    script_child_without_activity(&backend).await;
    backend.task_runtime.lock().unwrap().children = vec![("ses_call_sub".into(), ChildRuntime::Inactive)];
    backend
        .with_child_evidence(
            "ses_call_sub",
            ChildEvidence::Terminal {
                completed_at: now - 1_000,
            },
        )
        .await;

    Turn::run(&app.turn_handles(), ctx("ses_test", "跑一下构建并审阅"))
        .await
        .unwrap();

    assert_eq!(
        Turn::card_state(&app.cards_handle(), "ses_test").await,
        Some(CardState::Done),
        "the child's terminal evidence ends the turn directly"
    );
    let updates = platform.updated_cards().await;
    assert!(
        updates
            .iter()
            .all(|card| !card_header(card).contains("等待后台任务")),
        "the settle is direct — no waiting detour: {updates:?}"
    );
    let final_card = updates.last().cloned().unwrap();
    let text = card_text(&final_card);
    assert!(
        !text.contains("后台任务（") && !text.contains("状态待确认"),
        "the retired suspect's row left the live ledger: {final_card}"
    );
    assert_eq!(
        text.matches("🔔 subagent 结束：review the diff").count(),
        1,
        "exactly one retirement entry renders on the live card: {final_card}"
    );
    assert_eq!(
        backend.child_evidence_calls.lock().await.as_slice(),
        ["ses_call_sub".to_string()],
        "one evidence read for the one suspect"
    );
}

/// Acceptance (#591): at most ONE evidence read per suspected child per
/// reconcile cycle — and a child the runtime still confirms RUNNING is never
/// read at all. The suspect's terminal evidence retires it, and the retirement
/// overlay drops it from every later read, so no cycle can read it twice.
#[tokio::test]
async fn one_evidence_read_is_spent_per_suspected_child_per_cycle() {
    let _wd = test_work_dir();
    let now = chrono::Utc::now().timestamp_millis();
    let live = SessionTranscript::new(two_task_timeline(vec![]))
        .with_executions(vec![execution(2_500)])
        .with_background_tasks(vec![
            live_subagent(now - 3_000, "call_sub"),
            live_subagent(now - 2_000, "call_run"),
        ]);
    let (_dir, app, backend, platform) = scripted_app(vec![live], Some(SessionStatus::Idle)).await;
    script_child_without_activity(&backend).await;
    yield_waiting_card(&app, &platform, 2).await;

    backend.task_runtime.lock().unwrap().children = vec![
        ("ses_call_sub".into(), ChildRuntime::Inactive),
        ("ses_call_run".into(), ChildRuntime::Running),
    ];
    backend
        .with_child_evidence(
            "ses_call_sub",
            ChildEvidence::Terminal {
                completed_at: now - 1_000,
            },
        )
        .await;
    spawn_sync(&app);
    wait_for_card_update(&platform, "the suspect's entry", CardUpdates::Latest, |card| {
        card_text(card).contains("🔔 subagent 结束：review the diff")
    })
    .await;

    assert_eq!(
        backend.child_evidence_calls.lock().await.as_slice(),
        ["ses_call_sub".to_string()],
        "one read for the suspect, none for the running child"
    );
    assert_eq!(
        Turn::card_state(&app.cards_handle(), "ses_test").await,
        Some(CardState::Waiting),
        "the running child keeps the wait"
    );
    let latest = platform.updated_cards().await.last().cloned().unwrap();
    let text = card_text(&latest);
    assert!(
        text.contains("⏳ 后台任务（1）") && !text.contains("状态待确认"),
        "the running child keeps a confirmed row: {latest}"
    );
}

/// Spec #588 / #590: the waiting card's 「清理待确认任务」 click clears the
/// unconfirmed subagent — the click re-derives the runtime verdict itself, so
/// only the row the runtime cannot confirm is cleared — records its call id in
/// the retirement overlay, renders one 🧹 entry, and settles the last task's
/// wait ✅ in place: one in-place PATCH, no new card, no continuation, and the
/// task never re-enters the live list within the process.
#[tokio::test]
async fn a_cleanup_click_clears_the_unconfirmed_row_and_settles_in_place() {
    let _wd = test_work_dir();
    let now = chrono::Utc::now().timestamp_millis();
    let live = SessionTranscript::new(two_task_timeline(vec![]))
        .with_executions(vec![execution(2_500)])
        .with_background_tasks(vec![live_subagent(now - 3_000, "call_sub")]);
    let (_dir, app, backend, platform) = scripted_app(vec![live], Some(SessionStatus::Idle)).await;
    // The child read must not tick (no fragment): the settle's PATCH count
    // below is the cleanup's alone.
    script_child_without_activity(&backend).await;
    yield_one_task_card(&app, &platform, ctx("ses_test", "跑一下构建并审阅")).await;

    // The runtime reports the child inactive with no concluding evidence: the
    // row gains the marker — and the waiting card the cleanup button.
    backend.task_runtime.lock().unwrap().children = vec![("ses_call_sub".into(), ChildRuntime::Inactive)];
    spawn_sync(&app);
    wait_for_card_update(&platform, "the unconfirmed marker", CardUpdates::Latest, |card| {
        card_text(card).contains("⚠️ 状态待确认")
    })
    .await;

    let marked = platform.updated_cards().await.last().cloned().unwrap();
    assert!(
        card_text(&marked).contains("清理待确认任务"),
        "the waiting card offers the cleanup exit: {marked}"
    );
    assert!(
        marked["body"]["elements"]
            .as_array()
            .unwrap()
            .iter()
            .any(|el| { el["value"]["action"] == "cleanup" && el["value"]["session_id"] == "ses_test" }),
        "the button carries the session it clears: {marked}"
    );

    // The click acks immediately (claim + spawn + toast), off the pipeline.
    let patches_before = patches_to(&platform, "om_waiting").await.len();
    let created_before = created_cards(&platform).await.len();
    let ack = app
        .host_action(serde_json::json!({ "action": "cleanup", "session_id": "ses_test" }))
        .await
        .expect("the cleanup click acks");
    assert!(
        ack.card.is_none(),
        "the ack keeps the card: the pipeline PATCHes it"
    );
    assert_eq!(ack.toast.as_deref(), Some("正在清理..."));

    wait_for_card_update(&platform, "the cleaned settle", CardUpdates::Latest, |card| {
        card_header(card).contains("✅")
            && card_text(card).contains("🧹 subagent 已清理：review the diff（人工）")
    })
    .await;

    assert_eq!(
        Turn::card_state(&app.cards_handle(), "ses_test").await,
        Some(CardState::Done),
        "clearing the last task settles the wait"
    );
    let patches = patches_to(&platform, "om_waiting").await;
    assert_eq!(
        patches.len(),
        patches_before + 1,
        "the cleanup settles in one in-place PATCH after the marker: {patches:?}"
    );
    let settled = patches.last().unwrap();
    let text = card_text(settled);
    assert!(
        !text.contains("后台任务（"),
        "the cleared row's live list is gone: {settled}"
    );
    assert_eq!(
        text.matches("🧹 subagent 已清理：review the diff（人工）")
            .count(),
        1,
        "one entry per cleared row, exactly once: {settled}"
    );
    assert!(
        text.contains("subagent ses_call_sub · "),
        "the entry keeps the task's identity and clock: {settled}"
    );
    assert!(
        !text.contains("清理待确认任务"),
        "the button leaves with the last unconfirmed row: {settled}"
    );
    assert_eq!(
        created_cards(&platform).await.len(),
        created_before,
        "no new card: the settle is in place"
    );
    assert!(
        continuation_sends(&platform).await.is_empty(),
        "a cleanup settle owes no continuation: {:?}",
        platform.calls.lock().await
    );

    // The overlay keeps the cleared task out of every later read: within this
    // process life it never re-enters a live list.
    let later = crate::backend::Backend::transcript(backend.as_ref(), "ses_test")
        .await
        .unwrap();
    assert!(
        later.background_tasks.is_empty(),
        "the cleared task never re-enters the live list within the process"
    );
}

/// Spec #588 / #590: one click clears EVERY unconfirmed row — each gets its own
/// 🧹 entry — and the positively running shell's row keeps the card waiting.
#[tokio::test]
async fn a_cleanup_click_clears_every_unconfirmed_row_at_once() {
    let _wd = test_work_dir();
    let now = chrono::Utc::now().timestamp_millis();
    let live = SessionTranscript::new(two_task_timeline(vec![]))
        .with_executions(vec![execution(2_500)])
        .with_background_tasks(vec![
            live_shell(frozen_start(now), "call_bg"),
            live_subagent(now - 3_000, "call_sub"),
            live_subagent_child(now - 2_000, "call_sub2", "ses_child2"),
        ]);
    let (_dir, app, backend, platform) = scripted_app(vec![live], Some(SessionStatus::Idle)).await;
    script_child_without_activity(&backend).await;
    yield_waiting_card(&app, &platform, 3).await;

    {
        let mut runtime = backend.task_runtime.lock().unwrap();
        runtime.shells = vec![("sh_call_bg".into(), ShellRuntime::Running)];
        runtime.children = vec![
            ("ses_call_sub".into(), ChildRuntime::Inactive),
            ("ses_child2".into(), ChildRuntime::Inactive),
        ];
    }
    spawn_sync(&app);
    wait_for_card_update(
        &platform,
        "the two unconfirmed rows",
        CardUpdates::Latest,
        |card| card_text(card).contains("⏳ 后台任务（3 · 2 待确认）"),
    )
    .await;

    app.host_action(serde_json::json!({ "action": "cleanup", "session_id": "ses_test" }))
        .await
        .expect("the cleanup click acks");
    wait_for_card_update(&platform, "the multi-row cleanup", CardUpdates::Latest, |card| {
        card_text(card).contains("🧹 subagent 已清理：review the diff（人工）")
            && card_text(card).contains("🧹 subagent 已清理（人工）")
    })
    .await;

    let refreshed = platform.updated_cards().await.last().cloned().unwrap();
    let text = card_text(&refreshed);
    assert_eq!(
        text.matches("🧹 subagent 已清理").count(),
        2,
        "one 🧹 entry per cleared row: {refreshed}"
    );
    assert!(
        text.contains("⏳ 后台任务（1）") && text.contains("**gh run watch**"),
        "the running shell's row keeps the wait: {refreshed}"
    );
    assert_eq!(
        Turn::card_state(&app.cards_handle(), "ses_test").await,
        Some(CardState::Waiting),
        "a live task keeps the card waiting"
    );
    assert!(
        !text.contains("清理待确认任务"),
        "the button leaves with the last unconfirmed row: {refreshed}"
    );
}

/// Spec #588 / #590: cleanup clears ONLY the unconfirmed rows — a positively
/// running task's row is untouched and the card keeps waiting — the 🧹 entry
/// names just the cleared task, the button disappears with the last unconfirmed
/// row, and a later click finds nothing to claim.
#[tokio::test]
async fn a_cleanup_click_keeps_a_running_task_and_the_wait() {
    let _wd = test_work_dir();
    let now = chrono::Utc::now().timestamp_millis();
    // The running shell's start is frozen (future) so its rendered elapsed can
    // never tick: the PATCH count is the marker's and the cleanup's alone.
    let live = waiting_shell_and_subagent(frozen_start(now), now - 3_000);
    let (_dir, app, backend, platform) = scripted_app(vec![live], Some(SessionStatus::Idle)).await;
    script_child_without_activity(&backend).await;
    // A group turn with a requester: a settle would notify, so the silence is
    // evidence rather than a missing opt-in.
    let mut context = ctx("ses_test", "跑一下构建并审阅");
    context.is_group = true;
    context.requester_open_id = Some(TEST_HOST.to_string());
    yield_waiting_card_with(&app, &platform, 2, context).await;

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

    let patches_before = patches_to(&platform, "om_waiting").await.len();
    app.host_action(serde_json::json!({ "action": "cleanup", "session_id": "ses_test" }))
        .await
        .expect("the cleanup click acks");

    wait_for_card_update(&platform, "the cleaned wait", CardUpdates::Latest, |card| {
        card_text(card).contains("🧹 subagent 已清理：review the diff（人工）")
    })
    .await;

    assert_eq!(
        Turn::card_state(&app.cards_handle(), "ses_test").await,
        Some(CardState::Waiting),
        "a positively running task keeps the wait"
    );
    let patches = patches_to(&platform, "om_waiting").await;
    assert_eq!(
        patches.len(),
        patches_before + 1,
        "the cleanup refreshes the waiting card once: {patches:?}"
    );
    let refreshed = patches.last().unwrap();
    let text = card_text(refreshed);
    assert!(
        card_header(refreshed).contains("等待后台任务"),
        "the card keeps its waiting header: {refreshed}"
    );
    assert!(
        text.contains("⏳ 后台任务（1）") && text.contains("**gh run watch**"),
        "the running task's row survives, now the only one: {refreshed}"
    );
    assert!(
        text.matches("🧹 subagent 已清理：review the diff（人工）")
            .count()
            == 1,
        "one 🧹 entry for the cleared row: {refreshed}"
    );
    assert!(
        !text.contains("清理待确认任务"),
        "the button disappears once no unconfirmed row remains: {refreshed}"
    );
    assert!(
        platform.completion_notices().await.is_empty(),
        "a wait that keeps running notifies nothing: {:?}",
        platform.calls.lock().await
    );

    // The overlay filtered only the cleared child; the running shell stays.
    let later = crate::backend::Backend::transcript(backend.as_ref(), "ses_test")
        .await
        .unwrap();
    assert_eq!(
        later
            .background_tasks
            .iter()
            .map(|task| task.tool.call_id.as_str())
            .collect::<Vec<_>>(),
        vec!["call_bg"],
        "only the unconfirmed task is gone from later reads"
    );
    // Review (spec #588): the marker rides the overlay, so a cleared task's
    // marker must not survive as live state either — the overlay only re-applies
    // markers to tasks still live in the read.
    assert!(
        !later.unconfirmed_tasks.contains("call_sub"),
        "the cleared task's marker does not come back: {later:?}"
    );

    // A second click finds no claim (the card no longer carries an unconfirmed
    // row), so the stale button can clear nothing.
    assert!(
        app.host_action(serde_json::json!({ "action": "cleanup", "session_id": "ses_test" }))
            .await
            .is_none(),
        "a click with no unconfirmed row is refused"
    );
}

/// Review (spec #588 / #590, PR #595): the cleanup click's own reconcile pass
/// is the card's refresh point. When that pass positively reports the child
/// RUNNING, it resolves the carried ⚠️ 状态待确认 marker — nothing is cleared —
/// and the same click must re-render the card: marker and button gone, the
/// running row kept, the wait still waiting. Without the click's refresh the
/// stale button would survive until a later Session Sync read. Session Sync is
/// parked behind another active session on the thread (the harness's ordinary
/// inactive-session state, ADR-0017) so the click's pass is the card's only
/// writer and its refresh cannot be masked by the poll loop.
#[tokio::test]
async fn a_cleanup_click_that_finds_the_child_running_refreshes_the_card() {
    let _wd = test_work_dir();
    let now = chrono::Utc::now().timestamp_millis();
    // The shell's start is frozen (future) so its rendered elapsed can never
    // tick: the PATCH count is the marker's and the click's alone.
    let live = waiting_shell_and_subagent(frozen_start(now), now - 3_000);
    let (_dir, app, backend, platform) = scripted_app(vec![live], Some(SessionStatus::Idle)).await;
    script_child_without_activity(&backend).await;
    yield_waiting_card(&app, &platform, 2).await;

    // An admitted reconcile marks the inactive child: the row marker, the
    // title count and the cleanup button land together.
    {
        let mut runtime = backend.task_runtime.lock().unwrap();
        runtime.shells = vec![("sh_call_bg".into(), ShellRuntime::Running)];
        runtime.children = vec![("ses_call_sub".into(), ChildRuntime::Inactive)];
    }
    spawn_sync(&app);
    wait_for_card_update(&platform, "the unconfirmed marker", CardUpdates::Latest, |card| {
        card_text(card).contains("⚠️ 状态待确认") && card_text(card).contains("清理待确认任务")
    })
    .await;

    // Park Session Sync: the click's pass must be the card's only writer, so a
    // refresh the click never does cannot be covered by a later poll.
    seed_session(&app, "ses_parked", "/work").await;

    // The runtime now positively reports the child running: the click's own
    // reconcile resolves the marker without clearing anything.
    backend.task_runtime.lock().unwrap().children = vec![("ses_call_sub".into(), ChildRuntime::Running)];
    let patches_before = patches_to(&platform, "om_waiting").await.len();
    let ack = app
        .host_action(serde_json::json!({ "action": "cleanup", "session_id": "ses_test" }))
        .await
        .expect("the cleanup click acks");
    assert!(
        ack.card.is_none(),
        "the ack keeps the card: the pipeline PATCHes it"
    );
    assert_eq!(ack.toast.as_deref(), Some("正在清理..."));

    wait_for_card_update(&platform, "the resolved ledger", CardUpdates::Latest, |card| {
        let text = card_text(card);
        text.contains("⏳ 后台任务（2）") && !text.contains("待确认") && !text.contains("清理待确认任务")
    })
    .await;

    assert_eq!(
        Turn::card_state(&app.cards_handle(), "ses_test").await,
        Some(CardState::Waiting),
        "the positively running child's row keeps the wait"
    );
    let patches = patches_to(&platform, "om_waiting").await;
    assert_eq!(
        patches.len(),
        patches_before + 1,
        "the click's own pass re-renders the waiting card exactly once: {patches:?}"
    );
    let refreshed = patches.last().unwrap();
    let text = card_text(refreshed);
    assert!(
        text.contains("subagent：**review the diff**"),
        "the running child's row remains: {refreshed}"
    );
    assert!(
        !text.contains("⚠️ 状态待确认") && !text.contains("清理待确认任务"),
        "the resolved marker and its button leave in the click's refresh: {refreshed}"
    );
    assert!(
        !text.contains("🧹"),
        "a run the click did not clear invents no cleanup entry: {refreshed}"
    );

    // The click's verdict owns the marker state for later reads too: resolved
    // means resolved, and nothing was cleared.
    let later = crate::backend::Backend::transcript(backend.as_ref(), "ses_test")
        .await
        .unwrap();
    assert!(
        !later.unconfirmed_tasks.contains("call_sub"),
        "the resolved marker does not ride later reads: {later:?}"
    );
    assert_eq!(
        later
            .background_tasks
            .iter()
            .map(|task| task.tool.call_id.as_str())
            .collect::<Vec<_>>(),
        vec!["call_bg", "call_sub"],
        "the click cleared nothing"
    );
}

/// Review (spec #588, #590): the cleanup click is never throttled — a recent
/// poll's verdict must not make it no-op. Even with the shared interval pinned
/// wide right after a sync pass stamped it, the click spends its own runtime
/// read and clears the row.
#[tokio::test]
async fn a_cleanup_click_is_not_throttled_by_a_recent_poll() {
    let _wd = test_work_dir();
    let now = chrono::Utc::now().timestamp_millis();
    let live = SessionTranscript::new(two_task_timeline(vec![]))
        .with_executions(vec![execution(2_500)])
        .with_background_tasks(vec![live_subagent(now - 3_000, "call_sub")]);
    let (_dir, app, backend, platform) = scripted_app(vec![live], Some(SessionStatus::Idle)).await;
    script_child_without_activity(&backend).await;
    yield_one_task_card(&app, &platform, ctx("ses_test", "跑一下构建并审阅")).await;

    // The sync pass reconciles (stamping the shared throttle) and the row
    // gains its marker.
    backend.task_runtime.lock().unwrap().children = vec![("ses_call_sub".into(), ChildRuntime::Inactive)];
    spawn_sync(&app);
    wait_for_card_update(&platform, "the unconfirmed marker", CardUpdates::Latest, |card| {
        card_text(card).contains("⚠️ 状态待确认")
    })
    .await;

    // Pin the shared interval wide: any reconcile behind the throttle would
    // now no-op, so only a direct, unthrottled read can clear the row.
    app.runtime_reconcile.interval_ms.store(60_000, Ordering::Relaxed);
    backend.task_runtime_calls.lock().await.clear();
    app.host_action(serde_json::json!({ "action": "cleanup", "session_id": "ses_test" }))
        .await
        .expect("the cleanup click acks");
    wait_for_card_update(&platform, "the cleaned settle", CardUpdates::Latest, |card| {
        card_header(card).contains("✅")
            && card_text(card).contains("🧹 subagent 已清理：review the diff（人工）")
    })
    .await;

    assert!(
        !backend.task_runtime_calls.lock().await.is_empty(),
        "the click spends its own runtime read, never a poll's throttled one"
    );
}

/// ADR-0073 (spec #588, #590): a cleanup click whose own runtime read fails
/// re-derives nothing, so it clears nothing — the carried markers stay on the
/// card, the button stays usable, and no 🧹 entry is invented.
#[tokio::test]
async fn a_cleanup_click_with_a_failed_verdict_read_clears_nothing() {
    let _wd = test_work_dir();
    let now = chrono::Utc::now().timestamp_millis();
    let live = SessionTranscript::new(two_task_timeline(vec![]))
        .with_executions(vec![execution(2_500)])
        .with_background_tasks(vec![live_subagent(now - 3_000, "call_sub")]);
    let (_dir, app, backend, platform) = scripted_app(vec![live], Some(SessionStatus::Idle)).await;
    script_child_without_activity(&backend).await;
    yield_one_task_card(&app, &platform, ctx("ses_test", "跑一下构建并审阅")).await;

    backend.task_runtime.lock().unwrap().children = vec![("ses_call_sub".into(), ChildRuntime::Inactive)];
    spawn_sync(&app);
    wait_for_card_update(&platform, "the unconfirmed marker", CardUpdates::Latest, |card| {
        card_text(card).contains("⚠️ 状态待确认")
    })
    .await;

    // The click's own verdict read fails: it must not clear the carried row.
    backend.fail_task_runtime_reads(usize::MAX);
    app.host_action(serde_json::json!({ "action": "cleanup", "session_id": "ses_test" }))
        .await
        .expect("the cleanup click acks");
    tokio::time::sleep(Duration::from_millis(100)).await;

    assert_eq!(
        Turn::card_state(&app.cards_handle(), "ses_test").await,
        Some(CardState::Waiting),
        "a failed verdict read cannot settle the wait"
    );
    let latest = platform.updated_cards().await.last().cloned().unwrap();
    let text = card_text(&latest);
    assert!(
        text.contains("⚠️ 状态待确认") && text.contains("清理待确认任务"),
        "the carried row and its button stay: {latest}"
    );
    assert!(
        !text.contains("🧹"),
        "a failed read invents no cleanup entry: {latest}"
    );
}

/// Review (spec #588 / #589 / #590, PR #595): the cleanup's overlay record is
/// gated on its refresh landing — every part of it. The click's own reconcile
/// retires a runtime-ended task and clears the unconfirmed residue; when a new
/// Turn takes the chain while its reads are in flight, the waiting card stops
/// admitting the write, and recording any of it anyway would hide the tasks
/// from every later transcript read without ever rendering their entries
/// (🔔 结束/已失联, 🧹 已清理) — which no later read can reconstruct.
///
/// The arrangement is deterministic, no wall-clock margin decides:
/// - the click's reads complete while the test holds the session's card-write
///   lock, so its refresh is parked ahead of the write admission;
/// - Session Sync's verdict path is frozen out (the shared throttle pinned
///   wide) behind a decoy active session, so the click's read counters are
///   the only ones that can move — and a straggler pass can neither retire
///   the shell nor race the click;
/// - the new Turn's supersede collects the waiting card while the refresh is
///   parked, and the lock is released only after the collect actually landed
///   on the old card.
///
/// The assertions are properties — nothing recorded, nothing hidden, no entry
/// half-rendered, the collect the old card's last write — never exact patch
/// counts, so a benign extra ledger refresh cannot fail them.
#[tokio::test]
async fn a_cleanup_click_whose_card_is_replaced_never_hides_the_tasks() {
    let _wd = test_work_dir();
    let now = chrono::Utc::now().timestamp_millis();
    let live = SessionTranscript::new(two_task_timeline(vec![]))
        .with_executions(vec![execution(2_500)])
        .with_background_tasks(vec![
            live_shell(now - 3_000, "call_bg"),
            live_subagent(now - 3_000, "call_sub"),
        ]);
    let (_dir, app, backend, platform) = scripted_app(vec![live], Some(SessionStatus::Idle)).await;
    script_child_without_activity(&backend).await;
    // The lock below parks the click's reads on purpose; a wide read bound
    // keeps a loaded suite from abandoning them before the test releases it.
    app.turn_follow_read_timeout_ms.store(10_000, Ordering::Relaxed);
    yield_waiting_card(&app, &platform, 2).await;

    // The runtime cannot confirm the child (the shell stays unjudged yet), so
    // the waiting card grows its marker and its cleanup button.
    backend.task_runtime.lock().unwrap().children = vec![("ses_call_sub".into(), ChildRuntime::Inactive)];
    spawn_sync(&app);
    wait_for_card_update_within(
        &platform,
        "the unconfirmed marker",
        Duration::from_secs(10),
        CardUpdates::Latest,
        |card| card_text(card).contains("⚠️ 状态待确认") && card_text(card).contains("清理待确认任务"),
    )
    .await;
    // Park Session Sync behind a decoy active session (the harness's ordinary
    // inactive-session state, ADR-0017) and freeze the shared throttle wide:
    // from here on no throttled caller can spend a runtime read on this
    // session, so the click's counters below are its own.
    seed_session(&app, "ses_parked", "/work").await;
    app.runtime_reconcile
        .interval_ms
        .store(600_000, Ordering::Relaxed);
    // The runtime now misses the shell: the click's own, unthrottled reconcile
    // is the only read that may end it.
    backend.task_runtime.lock().unwrap().shells = vec![("sh_call_bg".into(), ShellRuntime::Missing)];
    // Both the click's and the successor's reads serve the two live tasks.
    script_transcript(
        &backend,
        vec![
            SessionTranscript::new(two_task_timeline(vec![user("msg_cola_next", 3_000, "新问题")]))
                .with_executions(vec![execution(2_500)])
                .with_background_tasks(vec![
                    live_shell(now - 3_000, "call_bg"),
                    live_subagent(now - 3_000, "call_sub"),
                ]),
        ],
    )
    .await;

    // Hold the session's card-write lock across the click: its reads
    // (transcript, runtime verdict, child evidence) complete, then its refresh
    // parks before the write admission.
    let write_lock = app.cards_handle().write_lock("ses_test").await;
    let guard = write_lock.lock().await;
    let runtime_reads = backend.task_runtime_calls.lock().await.len();
    let evidence_reads = backend.child_evidence_calls.lock().await.len();
    let ack = app
        .host_action(serde_json::json!({ "action": "cleanup", "session_id": "ses_test" }))
        .await
        .expect("the cleanup click acks");
    assert_eq!(ack.toast.as_deref(), Some("正在清理..."));
    // The click's own pass ran: only the click's verdict path is unthrottled,
    // so these counters can have moved for it alone — and everything it
    // records happens here, before its refresh reaches the held lock.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    while backend.task_runtime_calls.lock().await.len() <= runtime_reads
        || backend.child_evidence_calls.lock().await.len() <= evidence_reads
    {
        assert!(
            tokio::time::Instant::now() < deadline,
            "the cleanup pipeline never spent its own reads"
        );
        tokio::time::sleep(Duration::from_millis(5)).await;
    }

    // The chain moves under the parked refresh: the new Turn's supersede
    // collects the waiting card (the mark, `CardState::Superseded`, never
    // admits a ledger write).
    let second = {
        let mut second = ctx("ses_test", "新问题");
        second.message_id = "msg_next".into();
        second.cola_message_id = Some("msg_cola_next".into());
        second
    };
    let next_turn = spawn_turn(&app, second);
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    while Turn::card_state(&app.cards_handle(), "ses_test").await != Some(CardState::Superseded) {
        assert!(
            tokio::time::Instant::now() < deadline,
            "the new Turn never superseded the waiting card"
        );
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    drop(guard);
    // The refused refresh submits nothing, so the collect is the old card's
    // only write left: wait for it to land — a condition, never a count or a
    // sleep — before freezing the world again.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    while !patches_to(&platform, "om_waiting")
        .await
        .iter()
        .any(|card| card_header(card).contains("已由新消息接管"))
    {
        assert!(
            tokio::time::Instant::now() < deadline,
            "the takeover's collect never landed on the old card"
        );
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    // Every earlier writer has finished (the write lock is FIFO), so the
    // assertions below read a still world.
    let guard2 = write_lock.lock().await;

    // Nothing of the click's pass was recorded — neither the runtime
    // retirement of the shell nor the dismissal of the unconfirmed child — so
    // the tasks stay live on every later read and the successor's own
    // admitted paths can render their entries.
    assert!(
        backend.overlay.retired_call_ids("ses_test").is_empty(),
        "a refused refresh records nothing: {:?}",
        backend.overlay.retired_call_ids("ses_test")
    );
    assert_eq!(
        backend.overlay.unconfirmed_call_ids("ses_test"),
        vec!["call_sub".to_string()],
        "the carried marker state rides the refused refresh untouched"
    );
    let later = crate::backend::Backend::transcript(backend.as_ref(), "ses_test")
        .await
        .unwrap();
    assert_eq!(
        later
            .background_tasks
            .iter()
            .map(|task| task.tool.call_id.as_str())
            .collect::<Vec<_>>(),
        vec!["call_bg", "call_sub"],
        "a refresh that never landed must not hide the task: it would have no entry"
    );
    assert!(
        later.unconfirmed_tasks.contains("call_sub"),
        "the carried marker survives for the successor's button: {later:?}"
    );
    // No card carries a half-rendered retirement or cleanup entry: the
    // click's local transcript never reached a card, and the takeover
    // rendered its own collect instead.
    let mut cards = platform.updated_cards().await;
    cards.extend(platform.replied_cards().await);
    cards.extend(platform.sent_cards().await);
    assert!(
        cards
            .iter()
            .all(|card| !card_text(card).contains("🔔") && !card_text(card).contains("🧹")),
        "a refused refresh renders no entry anywhere: {cards:?}"
    );
    // The takeover's collect is the old card's last write; a benign extra
    // ledger refresh may precede it, nothing may follow, and no payload may
    // carry an entry.
    let patches = patches_to(&platform, "om_waiting").await;
    assert!(
        patches
            .last()
            .is_some_and(|card| card_header(card).contains("已由新消息接管")),
        "the collect is the old card's last write: {patches:?}"
    );
    assert!(
        patches
            .iter()
            .all(|card| !card_text(card).contains("🔔") && !card_text(card).contains("🧹")),
        "the old card received no retirement or cleanup entry: {patches:?}"
    );
    drop(guard2);

    // The successor's turn runs to its waiting yield: with nothing recorded,
    // the live tasks, their marker and the cleanup button are still there.
    next_turn.await.unwrap().unwrap();
    wait_for_card_update_within(
        &platform,
        "the successor's waiting card",
        Duration::from_secs(10),
        CardUpdates::Latest,
        |card| card_text(card).contains("⚠️ 状态待确认") && card_text(card).contains("清理待确认任务"),
    )
    .await;
    let successor_id = Turn::card_message_id(&app.cards_handle(), "ses_test")
        .await
        .expect("the successor carries its card id");

    // A later admitted path — Session Sync, unfrozen and re-activated — ends
    // the shell: its row leaves and exactly one 已失联 entry renders, while
    // the unconfirmed child keeps its row, marker and button.
    seed_session(&app, "ses_test", "/work").await;
    app.runtime_reconcile.interval_ms.store(0, Ordering::Relaxed);
    wait_for_card_update_within(
        &platform,
        "the shell's 已失联 entry",
        Duration::from_secs(10),
        CardUpdates::Latest,
        |card| {
            card_header(card).contains("⏳")
                && card_text(card).contains("🔔 shell 已失联：gh run watch")
                && card_text(card).contains("清理待确认任务")
        },
    )
    .await;
    let updates = patches_to(&platform, &successor_id).await;
    assert!(
        updates
            .iter()
            .all(|card| card_text(card).matches("🔔 shell 已失联").count() <= 1),
        "the shell's entry never doubles within one render: {updates:?}"
    );
    assert!(
        updates
            .iter()
            .any(|card| card_text(card).contains("🔔 shell 已失联")),
        "the runtime retirement renders on the successor: {updates:?}"
    );
    assert!(
        updates
            .last()
            .is_some_and(|card| card_text(card).contains("🔔 shell 已失联")),
        "the entry's own refresh is the newest write: {updates:?}"
    );

    // The successor's own click clears the child and renders its 🧹 entry
    // exactly once; both retirements are now recorded (their refreshes
    // landed), so every later read keeps the tasks out.
    let ack = app
        .host_action(serde_json::json!({ "action": "cleanup", "session_id": "ses_test" }))
        .await
        .expect("the cleanup click on the successor acks");
    assert_eq!(ack.toast.as_deref(), Some("正在清理..."));
    wait_for_card_update_within(
        &platform,
        "the successor's cleaned settle",
        Duration::from_secs(10),
        CardUpdates::Latest,
        |card| {
            card_header(card).contains("✅")
                && card_text(card).contains("🧹 subagent 已清理：review the diff（人工）")
        },
    )
    .await;
    let updates = patches_to(&platform, &successor_id).await;
    assert!(
        updates
            .iter()
            .all(|card| card_text(card).matches("🧹 subagent 已清理").count() <= 1),
        "the cleanup entry never doubles within one render: {updates:?}"
    );
    // The settled card carries each retirement exactly once: the shell's
    // entry (rendered by the earlier admitted refresh) re-renders with the
    // card, never duplicated, and the click adds its own 🧹 entry once.
    let settled = updates.last().expect("the successor's settle is a write");
    assert!(
        card_header(settled).contains("✅"),
        "the last task gone settles the card: {settled}"
    );
    assert_eq!(
        card_text(settled).matches("🧹 subagent 已清理").count(),
        1,
        "one cleanup entry on the settled card: {settled}"
    );
    assert_eq!(
        card_text(settled).matches("🔔 shell 已失联").count(),
        1,
        "the shell's entry stays exactly one on the settled card: {settled}"
    );
    assert_eq!(
        backend.overlay.retired_call_ids("ses_test"),
        vec!["call_bg".to_string(), "call_sub".to_string()],
        "both landed refreshes recorded their retirements"
    );
    assert!(
        backend.overlay.unconfirmed_call_ids("ses_test").is_empty(),
        "the landed cleanup clears the marker set"
    );
    let later = crate::backend::Backend::transcript(backend.as_ref(), "ses_test")
        .await
        .unwrap();
    assert!(
        later.background_tasks.is_empty(),
        "accepted retirements keep the tasks out of later reads: {later:?}"
    );
}

/// Await the External Message notification (the 有新消息 card) or panic after
/// 10 s — the condition the supersede below is observed by, never a sleep.
async fn wait_for_external_notify(platform: &RecordingPlatform) -> serde_json::Value {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        let mut cards = platform.sent_cards().await;
        cards.extend(platform.replied_cards().await);
        if let Some(card) = cards
            .into_iter()
            .find(|card| card_text(card).contains("有新消息"))
        {
            return card;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "no external-message notification arrived: {:?}",
            platform.calls.lock().await
        );
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

/// Review (spec #588 / #589, PR #595): Session Sync commits its reconcile
/// pass's overlay record only after the flush that carried its entries was
/// accepted — the same invariant the cleanup click obeys for its own pass.
///
/// A newer External Message in the same read as the reconcile supersedes the
/// waiting card: the yielded-card write admission passes (the card is still
/// the live, no-handoff Waiting one), the runtime retires the shell, and only
/// THEN does the chain-held check reject the card — the notification path
/// collects it and arms the fresh renderer, so this read's retirement entry has
/// no card to land on. Recording the retirement anyway would hide the task from
/// every later read (the launch record never flips) with no entry ever
/// rendered; the task must stay live until a card that can carry its entry
/// receives one.
///
/// Deterministic, no wall-clock margin decides: the pass's transcript read is
/// parked on the mock's gate while the test scripts BOTH facts it will read
/// (the newer message and the missing-shell verdict), then released; the
/// notification's landing is awaited as a condition, and the reads behind it
/// stay parked while the assertions run.
#[tokio::test]
async fn an_external_supersede_records_no_runtime_retirement() {
    let _wd = test_work_dir();
    let now = chrono::Utc::now().timestamp_millis();
    let (_dir, app, backend, platform) =
        scripted_app(vec![waiting_shell(now - 5_000)], Some(SessionStatus::Idle)).await;
    yield_one_task_card(&app, &platform, ctx("ses_test", "跑一下构建并审阅")).await;
    // The notify path only fires for a message newer than the watermark: seed
    // the anchor's own message as already read (the parked pass below would
    // otherwise be the session's first observation, which never notifies).
    app.external
        .last_user_msg_epoch
        .lock()
        .await
        .insert("ses_test".into(), 1_000);

    // Park Session Sync's first read, then move BOTH facts the pass will read:
    // the transcript now carries a newer EXTERNAL message, and the runtime no
    // longer knows the shell.
    let gate = backend.hold_transcripts();
    spawn_sync(&app);
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    while backend.transcript_gate_entered.load(Ordering::SeqCst) == 0 {
        assert!(
            tokio::time::Instant::now() < deadline,
            "Session Sync never reached the parked read"
        );
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    backend.task_runtime.lock().unwrap().shells = vec![("sh_call_bg".into(), ShellRuntime::Missing)];
    script_transcript(
        &backend,
        vec![
            SessionTranscript::new(two_task_timeline(vec![
                user("msg_ext_next", 3_000, "新问题"),
                assistant(3_500, "答复。"),
            ]))
            .with_executions(vec![execution(2_500), execution(4_000)])
            .with_background_tasks(vec![live_shell(now - 3_000, "call_bg")]),
        ],
    )
    .await;
    gate.add_permits(1);

    // The pass runs: the yielded-card admission passes, the runtime retires the
    // shell, and the chain-held check then rejects the superseded card — the
    // notify path collects it and arms the fresh renderer. The next reads stay
    // parked on the same gate, so the world is frozen at the supersede while
    // the assertions below read it.
    let notify = wait_for_external_notify(&platform).await;
    assert!(
        card_text(&notify).contains("新问题"),
        "the notification previews the newer message: {notify}"
    );
    assert!(
        backend.overlay.retired_call_ids("ses_test").is_empty(),
        "a superseded pass records nothing: {:?}",
        backend.overlay.retired_call_ids("ses_test")
    );
    assert!(
        backend.overlay.unconfirmed_call_ids("ses_test").is_empty(),
        "a superseded pass records no marker state either: {:?}",
        backend.overlay.unconfirmed_call_ids("ses_test")
    );
    let mut cards = platform.updated_cards().await;
    cards.extend(platform.replied_cards().await);
    cards.extend(platform.sent_cards().await);
    assert!(
        cards
            .iter()
            .all(|card| !card_text(card).contains("🔔") && !card_text(card).contains("🧹")),
        "a superseded pass half-renders no entry anywhere: {cards:?}"
    );

    // Freeze the shared throttle wide, THEN release the readers parked behind
    // the notification: no later pass can spend a verdict, so the world stays
    // frozen and the task provably stays live on every read.
    app.runtime_reconcile
        .interval_ms
        .store(600_000, Ordering::Relaxed);
    *backend.transcript_gate.lock().unwrap() = None;
    gate.add_permits(64);
    let later = crate::backend::Backend::transcript(backend.as_ref(), "ses_test")
        .await
        .unwrap();
    assert_eq!(
        later
            .background_tasks
            .iter()
            .map(|task| task.tool.call_id.as_str())
            .collect::<Vec<_>>(),
        vec!["call_bg"],
        "the task stays live until a card that can render its entry takes the chain"
    );

    // A later receivable card renders exactly one entry: the external render's
    // own waiting yield (the run completed with the shell still live), then
    // Session Sync's next admitted pass retires the shell and settles the card.
    app.runtime_reconcile.interval_ms.store(0, Ordering::Relaxed);
    wait_for_card_update_within(
        &platform,
        "the shell's 已失联 entry on the successor",
        Duration::from_secs(10),
        CardUpdates::Latest,
        |card| card_header(card).contains("✅") && card_text(card).contains("🔔 shell 已失联：gh run watch"),
    )
    .await;
    assert_eq!(
        backend.overlay.retired_call_ids("ses_test"),
        vec!["call_bg".to_string()],
        "the accepted refresh records the retirement"
    );
    let mut cards = platform.updated_cards().await;
    cards.extend(platform.replied_cards().await);
    cards.extend(platform.sent_cards().await);
    assert!(
        cards
            .iter()
            .all(|card| card_text(card).matches("🔔 shell 已失联").count() <= 1),
        "the entry renders exactly once on the card that carried it: {cards:?}"
    );
}

/// Review (spec #588, #590): the unconfirmed marker and its cleanup button are
/// process-local state the reconcile owns. A Session Sync read within the
/// shared throttle (no verdict) must not drop them — the read's transcript is
/// fresh and carries no marker of its own, so the state has to ride the
/// backend adapter's overlay until a verdict resolves it. A throttled read is
/// not evidence that the child is running.
#[tokio::test]
async fn a_throttled_read_keeps_the_unconfirmed_marker_and_its_button() {
    let _wd = test_work_dir();
    let now = chrono::Utc::now().timestamp_millis();
    let live = waiting_shell_and_subagent(frozen_start(now), now - 3_000);
    let (_dir, app, backend, platform) = scripted_app(vec![live], Some(SessionStatus::Idle)).await;
    script_child_without_activity(&backend).await;
    yield_waiting_card(&app, &platform, 2).await;

    // An admitted reconcile marks the inactive child: the row marker, the
    // title count and the cleanup button land together.
    {
        let mut runtime = backend.task_runtime.lock().unwrap();
        runtime.shells = vec![("sh_call_bg".into(), ShellRuntime::Running)];
        runtime.children = vec![("ses_call_sub".into(), ChildRuntime::Inactive)];
    }
    spawn_sync(&app);
    wait_for_card_update(&platform, "the unconfirmed marker", CardUpdates::Latest, |card| {
        card_text(card).contains("⚠️ 状态待确认") && card_text(card).contains("清理待确认任务")
    })
    .await;

    // Pin the shared throttle wide: every later read is a read the runtime was
    // not asked about, so nothing positive confirms the child.
    app.runtime_reconcile.interval_ms.store(60_000, Ordering::Relaxed);
    backend.transcript_calls.lock().await.clear();
    wait_for_transcript_reads(&backend, "ses_test", 1).await;
    // Let the throttled read's refresh land: it must owe no PATCH.
    tokio::time::sleep(Duration::from_millis(100)).await;

    let latest = platform.updated_cards().await.last().cloned().unwrap();
    let text = card_text(&latest);
    assert!(
        text.contains("⏳ 后台任务（2 · 1 待确认）"),
        "the title keeps counting the unconfirmed row across a throttled read: {latest}"
    );
    assert!(
        text.contains("⚠️ 状态待确认"),
        "the marker survives a read the runtime was not asked about: {latest}"
    );
    assert!(
        text.contains("清理待确认任务"),
        "the cleanup button stays while the unconfirmed row does: {latest}"
    );
}

/// Review (spec #588): a verdict that positively reports the child RUNNING
/// resolves the marker — the live row stays, the title count and the cleanup
/// button leave. With the marker carried across reads, only evidence may clear
/// it; a stale marker must not outlive the verdict that resolves it.
#[tokio::test]
async fn a_running_verdict_resolves_the_unconfirmed_marker_and_its_button() {
    let _wd = test_work_dir();
    let now = chrono::Utc::now().timestamp_millis();
    let live = waiting_shell_and_subagent(frozen_start(now), now - 3_000);
    let (_dir, app, backend, platform) = scripted_app(vec![live], Some(SessionStatus::Idle)).await;
    script_child_without_activity(&backend).await;
    yield_waiting_card(&app, &platform, 2).await;

    backend.task_runtime.lock().unwrap().children = vec![("ses_call_sub".into(), ChildRuntime::Inactive)];
    spawn_sync(&app);
    wait_for_card_update(&platform, "the unconfirmed marker", CardUpdates::Latest, |card| {
        card_text(card).contains("⚠️ 状态待确认")
    })
    .await;

    // The runtime now positively reports the child running: the marker and its
    // button leave on the ledger refresh, with the row itself still live.
    backend.task_runtime.lock().unwrap().children = vec![("ses_call_sub".into(), ChildRuntime::Running)];
    wait_for_card_update(&platform, "the resolved ledger", CardUpdates::Latest, |card| {
        let text = card_text(card);
        text.contains("⏳ 后台任务（2）") && !text.contains("待确认")
    })
    .await;
    let latest = platform.updated_cards().await.last().cloned().unwrap();
    assert!(
        !card_text(&latest).contains("清理待确认任务"),
        "the button leaves with the resolved row: {latest}"
    );
}

/// Review (spec #588): a failed verdict read clears nothing — the marker and
/// its cleanup button stay until positive evidence resolves them. The attempt
/// is spent (and the throttle counts it), but a read that answered nothing is
/// no evidence; the carried marker must survive it.
#[tokio::test]
async fn a_failed_verdict_read_keeps_the_unconfirmed_marker() {
    let _wd = test_work_dir();
    let now = chrono::Utc::now().timestamp_millis();
    let live = waiting_shell_and_subagent(frozen_start(now), now - 3_000);
    let (_dir, app, backend, platform) = scripted_app(vec![live], Some(SessionStatus::Idle)).await;
    script_child_without_activity(&backend).await;
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

    // Every later verdict read fails: no verdict, no marker change.
    backend.fail_task_runtime_reads(usize::MAX);
    backend.task_runtime_calls.lock().await.clear();
    backend.transcript_calls.lock().await.clear();
    wait_for_transcript_reads(&backend, "ses_test", 1).await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(
        !backend.task_runtime_calls.lock().await.is_empty(),
        "the failing verdict read was attempted"
    );

    let latest = platform.updated_cards().await.last().cloned().unwrap();
    let text = card_text(&latest);
    assert!(
        text.contains("⚠️ 状态待确认"),
        "a failed verdict read keeps the marker: {latest}"
    );
    assert!(
        text.contains("清理待确认任务"),
        "a failed verdict read keeps the cleanup button: {latest}"
    );
}

/// Review (spec #588): a verdict that does not answer for the child — an empty
/// runtime read, an id the active map could not place — keeps the marker
/// exactly as the read carried it. No answer is no evidence either way.
#[tokio::test]
async fn a_verdict_that_does_not_answer_for_the_child_keeps_its_marker() {
    let _wd = test_work_dir();
    let now = chrono::Utc::now().timestamp_millis();
    let live = waiting_shell_and_subagent(frozen_start(now), now - 3_000);
    let (_dir, app, backend, platform) = scripted_app(vec![live], Some(SessionStatus::Idle)).await;
    script_child_without_activity(&backend).await;
    yield_waiting_card(&app, &platform, 2).await;

    backend.task_runtime.lock().unwrap().children = vec![("ses_call_sub".into(), ChildRuntime::Inactive)];
    spawn_sync(&app);
    wait_for_card_update(&platform, "the unconfirmed marker", CardUpdates::Latest, |card| {
        card_text(card).contains("⚠️ 状态待确认")
    })
    .await;

    // The runtime read now names no task at all: no verdict for the child, so
    // the carried marker stands.
    backend.task_runtime.lock().unwrap().children = vec![];
    backend.transcript_calls.lock().await.clear();
    wait_for_transcript_reads(&backend, "ses_test", 1).await;
    tokio::time::sleep(Duration::from_millis(100)).await;

    let latest = platform.updated_cards().await.last().cloned().unwrap();
    let text = card_text(&latest);
    assert!(
        text.contains("⏳ 后台任务（2 · 1 待确认）"),
        "no answer for the child keeps the title count: {latest}"
    );
    assert!(
        text.contains("⚠️ 状态待确认"),
        "no answer for the child keeps the marker: {latest}"
    );
    assert!(
        text.contains("清理待确认任务"),
        "no answer for the child keeps the cleanup button: {latest}"
    );
}

/// Spec #588 / #590: a late Wake after a cleanup is untouched — the cleaned
/// task's completion still resumes the chain (the split continuation opens with
/// its own work) and the Wake's completion entry still renders on the card
/// whose chain observed it. The cleanup suppresses nothing.
#[tokio::test]
async fn a_late_wake_after_a_cleanup_still_resumes_the_chain() {
    let _wd = test_work_dir();
    let now = chrono::Utc::now().timestamp_millis();
    let live = SessionTranscript::new(two_task_timeline(vec![]))
        .with_executions(vec![execution(2_500)])
        .with_background_tasks(vec![live_subagent(now - 3_000, "call_sub")]);
    let (_dir, app, backend, platform) = scripted_app(vec![live], Some(SessionStatus::Idle)).await;
    script_child_without_activity(&backend).await;
    yield_one_task_card(&app, &platform, ctx("ses_test", "跑一下构建并审阅")).await;

    backend.task_runtime.lock().unwrap().children = vec![("ses_call_sub".into(), ChildRuntime::Inactive)];
    spawn_sync(&app);
    wait_for_card_update(&platform, "the unconfirmed marker", CardUpdates::Latest, |card| {
        card_text(card).contains("⚠️ 状态待确认")
    })
    .await;
    app.host_action(serde_json::json!({ "action": "cleanup", "session_id": "ses_test" }))
        .await
        .expect("the cleanup click acks");
    wait_for_card_update(&platform, "the cleaned settle", CardUpdates::Latest, |card| {
        card_header(card).contains("✅")
    })
    .await;
    let posts_before = created_cards(&platform).await.len();

    // The lost Wake now arrives with the subagent's resumed work: nothing the
    // cleanup did suppresses it — the chain continues on a new card with the
    // resumed work, and the Wake's own completion entry lands where the chain
    // observed it.
    script_transcript(
        &backend,
        vec![
            SessionTranscript::new(two_task_timeline(vec![assistant(3_100, "审阅完成。")]))
                .with_executions(vec![execution(2_500), execution(4_000)])
                .with_wakes(vec![Wake {
                    id: MessageId::new("msg_wake_sub_2900"),
                    created_ms: Some(2_900),
                    source: WakeSource::Subagent,
                    shell_id: None,
                    job_id: None,
                    child_id: Some("ses_call_sub".into()),
                    state: Some("completed".into()),
                    label: Some("review the diff".into()),
                }]),
        ],
    )
    .await;
    wait_for_card_update(
        &platform,
        "the continuation's true end",
        CardUpdates::Latest,
        |card| card_header(card).contains("✅") && card_text(card).contains("审阅完成。"),
    )
    .await;

    assert!(
        created_cards(&platform).await.len() > posts_before
            || !continuation_sends(&platform).await.is_empty(),
        "the late Wake still continues the chain: {:?}",
        platform.calls.lock().await
    );
    // The handover PATCH on the card that hosted the task carries the Wake's
    // own completion entry — nothing the cleanup did suppresses it — and the
    // cleanup's 🧹 entry stays there exactly once.
    let handover = patches_to(&platform, "om_waiting")
        .await
        .last()
        .cloned()
        .expect("the handover PATCH lands on the hosted card");
    assert!(
        card_text(&handover).contains("🔔 subagent 完成：review the diff"),
        "the late Wake's completion entry is never suppressed: {handover}"
    );
    assert_eq!(
        card_text(&handover)
            .matches("🧹 subagent 已清理：review the diff（人工）")
            .count(),
        1,
        "the cleanup entry stays exactly once on the card that hosted the task: {handover}"
    );
    // The continuation opens with the resumed work and never replays the
    // cleanup entry: entries never migrate to a continuation.
    let continuation = platform.updated_cards().await.last().cloned().unwrap();
    assert!(
        card_text(&continuation).contains("审阅完成。"),
        "the resumed work lands on the continuation: {continuation}"
    );
    assert!(
        !card_text(&continuation).contains("🧹 subagent 已清理"),
        "the cleanup entry stays on its own card: {continuation}"
    );
}

/// Spec #588 / #590, review PR #595: the cleanup's synthetic 🧹 retirement —
/// like every runtime/evidence retirement — keeps exactly-once through the
/// chain's in-memory announce set alone and never advances the durable Wake
/// Watermark (ADR-0061, which is Wake-scoped). The cleanup overlay is
/// process-local; the watermark is not. If the cleanup's own clock staged the
/// mark, a restart would read a genuinely un-announced late Wake at or below
/// that clock as already announced, `fresh()` would keep, and the
/// continuation would be suppressed — #590's "a late Wake for a cleared task
/// behaves as before; nothing is suppressed". Two lives: life 1 cleans up
/// (its card write is confirmed, draining whatever the stage holds), the
/// persisted sidecar is reloaded by a fresh process, and the late Wake still
/// resumes the chain.
#[tokio::test]
async fn a_cleanup_never_advances_the_durable_watermark_for_a_late_wake() {
    let _wd = test_work_dir();
    let now = chrono::Utc::now().timestamp_millis();
    let live = SessionTranscript::new(two_task_timeline(vec![]))
        .with_executions(vec![execution(2_500)])
        .with_background_tasks(vec![live_subagent(now - 3_000, "call_sub")]);
    let (dir, app, backend, platform) = scripted_app(vec![live], Some(SessionStatus::Idle)).await;
    script_child_without_activity(&backend).await;
    yield_one_task_card(&app, &platform, ctx("ses_test", "跑一下构建并审阅")).await;

    // Life 1: the child reads inactive, the user cleans it up, and the
    // settle's card write is confirmed — its stage would drain right here.
    backend.task_runtime.lock().unwrap().children = vec![("ses_call_sub".into(), ChildRuntime::Inactive)];
    spawn_sync(&app);
    wait_for_card_update(&platform, "the unconfirmed marker", CardUpdates::Latest, |card| {
        card_text(card).contains("⚠️ 状态待确认")
    })
    .await;
    app.host_action(serde_json::json!({ "action": "cleanup", "session_id": "ses_test" }))
        .await
        .expect("the cleanup click acks");
    wait_for_card_update(&platform, "the cleaned settle", CardUpdates::Latest, |card| {
        card_header(card).contains("✅")
    })
    .await;

    // The persisted sidecar, read as a fresh process reads it: the cleanup's
    // synthetic clock is not a Wake and must not be there.
    let session_file = dir.path().join("sessions.json");
    let persisted = ChainRecords::load(session_file.with_file_name("chain_records.json"));
    assert_eq!(
        persisted.announced("ses_test"),
        None,
        "a synthetic cleanup entry never advances the durable Wake Watermark"
    );

    // Life 2: a fresh process over the same files. The late Wake arrives with
    // the subagent's resumed work, its server clock at or before the cleanup
    // click's — and nothing ever announced it.
    let late = SessionTranscript::new(two_task_timeline(vec![assistant(3_100, "审阅完成。")]))
        .with_executions(vec![execution(2_500), execution(4_000)])
        .with_wakes(vec![Wake {
            id: MessageId::new("msg_wake_sub_2900"),
            created_ms: Some(2_900),
            source: WakeSource::Subagent,
            shell_id: None,
            job_id: None,
            child_id: Some("ses_call_sub".into()),
            state: Some("completed".into()),
            label: Some("review the diff".into()),
        }]);
    let (restarted, restarted_platform) = restarted_app_with_transcript(&session_file, late).await;
    assert!(
        restarted.cards_handle().chains.get("ses_test").is_none(),
        "life 1's settled card left no durable record: the Fresh gate decides"
    );

    spawn_sync(&restarted);
    wait_for_card_update(
        &restarted_platform,
        "the late Wake's continuation",
        CardUpdates::Latest,
        |card| card_header(card).contains("✅") && card_text(card).contains("审阅完成。"),
    )
    .await;
    assert!(
        restarted_platform
            .sent_cards()
            .await
            .iter()
            .any(|card| card_text(card).contains(WAKE_LEAD)),
        "the late Wake still resumes the chain after the restart: {:?}",
        restarted_platform.calls.lock().await
    );
}

/// The second life of a restart test (spec #588, review PR #595): a fresh app
/// over `session_file` with `transcript` scripted — no in-memory chain, only
/// what the sidecar and the session's own reads carry.
async fn restarted_app_with_transcript(
    session_file: &std::path::Path,
    transcript: SessionTranscript,
) -> (Arc<App>, Arc<RecordingPlatform>) {
    let mut backend = MockBackend::new(realistic_parts());
    backend.given_transcript("ses_test", vec![transcript]);
    backend.with_session_status("ses_test", Some(SessionStatus::Idle));
    let platform = Arc::new(RecordingPlatform::new());
    let app = Arc::new(
        App::new(test_config(session_file), Arc::new(backend), platform.clone())
            .expect("the restarted app builds"),
    );
    seed_session(&app, "ses_test", "/work").await;
    (app, platform)
}

/// Spec #588 / #590, review PR #595: the runtime-retirement class of the same
/// invariant — an evidence/runtime 结束 entry is synthetic too, so its clock
/// never advances the durable Wake Watermark. Life 1's Session Sync observes
/// the runtime's own end of the live shell (its card write is confirmed); the
/// restart reloads the sidecar, and a genuinely un-announced late Wake at or
/// before the runtime's `finished_at` still resumes the chain.
#[tokio::test]
async fn a_runtime_retirement_never_advances_the_durable_watermark_for_a_late_wake() {
    let _wd = test_work_dir();
    let now = chrono::Utc::now().timestamp_millis();
    let live = waiting_shell(now - 5_000);
    let (dir, app, backend, platform) = scripted_app(vec![live], Some(SessionStatus::Idle)).await;
    yield_one_task_card(&app, &platform, ctx("ses_test", "跑一下构建并审阅")).await;

    // Life 1: the runtime confirms the shell ended at its own completion time
    // — no Wake will ever arrive — and the settle's card write is confirmed.
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

    let session_file = dir.path().join("sessions.json");
    let persisted = ChainRecords::load(session_file.with_file_name("chain_records.json"));
    assert_eq!(
        persisted.announced("ses_test"),
        None,
        "a synthetic runtime retirement never advances the durable Wake Watermark"
    );

    // Life 2: the lost Wake now arrives at or before the runtime's own end —
    // never announced — and still resumes the chain after the restart.
    let late = SessionTranscript::new(two_task_timeline(vec![assistant(3_100, "构建完成。")]))
        .with_executions(vec![execution(2_500), execution(4_000)])
        .with_wakes(vec![shell_wake(2_900)]);
    let (restarted, restarted_platform) = restarted_app_with_transcript(&session_file, late).await;
    assert!(
        restarted.cards_handle().chains.get("ses_test").is_none(),
        "life 1's settled card left no durable record: the Fresh gate decides"
    );

    spawn_sync(&restarted);
    wait_for_card_update(
        &restarted_platform,
        "the late Wake's continuation",
        CardUpdates::Latest,
        |card| card_header(card).contains("✅") && card_text(card).contains("构建完成。"),
    )
    .await;
    assert!(
        restarted_platform
            .sent_cards()
            .await
            .iter()
            .any(|card| card_text(card).contains(WAKE_LEAD)),
        "the late Wake still resumes the chain after the restart: {:?}",
        restarted_platform.calls.lock().await
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

// ---------------------------------------------------------------------------
// A running shell's output window (spec #588, ticket #592): the shared
// runtime reconcile's own cycle reads the record's tail, and the ledger
// renders it under the shell's row — labelled, clipped when truncated, and
// refreshed at the shared cadence. Display-only: a failed or vanished read
// omits the window, and nothing ever prompts, retires or settles.
// ---------------------------------------------------------------------------

/// Script the live shell `sh_call_bg`'s output window (the shell id
/// [`live_shell`] derives from its `call_bg` call id).
fn given_shell_output(backend: &Arc<MockBackend>, text: &str, clipped: bool, captured_ms: i64) {
    backend.shell_outputs.lock().unwrap().insert(
        "sh_call_bg".into(),
        Some(crate::backend::ShellOutputWindow {
            text: text.into(),
            clipped,
            captured_ms,
        }),
    );
}

/// Acceptance 1/2 (spec #588, #592): a running shell's row carries its output
/// window on the LIVE card, and the window refreshes only on the shared
/// reconcile cycle — many drain ticks inside one cycle spend one tail read per
/// live shell.
#[tokio::test]
async fn a_live_turns_shell_window_renders_on_the_shared_cadence() {
    let _wd = test_work_dir();
    let now = chrono::Utc::now().timestamp_millis();
    let live = SessionTranscript::new(two_task_timeline(vec![]))
        .with_executions(vec![execution(2_500)])
        .with_background_tasks(vec![
            live_shell(now - 5_000, "call_bg"),
            live_shell(now - 3_000, "call_bg2"),
        ]);
    let (_dir, app, backend, platform) = scripted_app(vec![live], Some(SessionStatus::Busy)).await;
    // One cycle, pinned wide before the turn starts: its first drain tick
    // spends it, every later tick in the window must not.
    app.runtime_reconcile.interval_ms.store(60_000, Ordering::Relaxed);
    given_shell_output(&backend, "step 1\nstep 2", false, now);
    backend.shell_outputs.lock().unwrap().insert(
        "sh_call_bg2".into(),
        Some(crate::backend::ShellOutputWindow {
            text: "other 1\nother 2".into(),
            clipped: true,
            captured_ms: now,
        }),
    );

    let _turn = spawn_turn(&app, ctx("ses_test", "跑一下构建并审阅"));
    wait_for_card_update(&platform, "the live output window", CardUpdates::Latest, |card| {
        let text = card_text(card);
        text.contains("· shell：**gh run watch**")
            && text.contains("截至于")
            && text.contains("step 1")
            && text.contains("other 2")
    })
    .await;

    // Many more ticks inside the same cycle: one tail read per live shell.
    tokio::time::sleep(Duration::from_millis(150)).await;
    assert_eq!(
        backend.shell_output_calls.lock().await.clone(),
        vec!["sh_call_bg".to_string(), "sh_call_bg2".to_string()],
        "at most one tail read per live shell per shared cycle"
    );

    // The cadence admits the next attempt: the changed tail lands.
    given_shell_output(&backend, "step 3\nstep 4", false, now + 60_000);
    app.runtime_reconcile.interval_ms.store(0, Ordering::Relaxed);
    wait_for_card_update(&platform, "the refreshed window", CardUpdates::Latest, |card| {
        card_text(card).contains("step 4")
    })
    .await;
    assert_eq!(
        backend.prompt_calls.lock().await.len(),
        1,
        "the window is a pure read: the turn's own prompt is the card's only one"
    );
}

/// Acceptance 1/3/4 (spec #588, #592): the yielded (waiting) card carries the
/// window too; a failed read takes it away entirely — never a stale window or
/// an empty panel — and a later successful read brings the fresh one back.
/// The wait itself is untouched by any of it.
#[tokio::test]
async fn a_failed_window_read_omits_it_and_a_later_read_restores_it() {
    let _wd = test_work_dir();
    let now = chrono::Utc::now().timestamp_millis();
    let live = waiting_shell(now - 5_000);
    let (_dir, app, backend, platform) = scripted_app(vec![live], Some(SessionStatus::Idle)).await;
    given_shell_output(&backend, "build ok", false, now);
    yield_one_task_card(&app, &platform, ctx("ses_test", "跑一下构建并审阅")).await;

    // The yield already carried the window (the drain's shared cycle read it).
    let yielded = platform.updated_cards().await.last().cloned().unwrap();
    assert!(
        card_text(&yielded).contains("截至于") && card_text(&yielded).contains("build ok"),
        "the waiting card shows its running shell's tail: {yielded}"
    );
    assert_eq!(
        backend.shell_output_calls.lock().await.len(),
        1,
        "the yield's cycle spent one tail read"
    );

    // The next reads fail: the window leaves the card, and stays gone while
    // the reads keep failing — no placeholder, no stale window.
    backend
        .fail_shell_output_reads
        .store(1_000, std::sync::atomic::Ordering::SeqCst);
    spawn_sync(&app);
    wait_for_card_update(&platform, "the window leaving", CardUpdates::Latest, |card| {
        !card_text(card).contains("截至于")
    })
    .await;
    tokio::time::sleep(Duration::from_millis(150)).await;
    let latest = platform.updated_cards().await.last().cloned().unwrap();
    assert!(
        !card_text(&latest).contains("截至于") && !card_text(&latest).contains("build ok"),
        "a failed read never keeps or invents a window: {latest}"
    );

    // The read recovers: the fresh window returns on the next cycle.
    backend
        .fail_shell_output_reads
        .store(0, std::sync::atomic::Ordering::SeqCst);
    given_shell_output(&backend, "build done", false, now + 120_000);
    wait_for_card_update(&platform, "the window returning", CardUpdates::Latest, |card| {
        card_text(card).contains("build done")
    })
    .await;

    // A vanished record is the same no-output case (a successful read that
    // answers nothing): the window leaves again, never an empty panel.
    backend.shell_outputs.lock().unwrap().remove("sh_call_bg");
    wait_for_card_update(&platform, "the vanished window", CardUpdates::Latest, |card| {
        !card_text(card).contains("截至于")
    })
    .await;

    assert_eq!(
        Turn::card_state(&app.cards_handle(), "ses_test").await,
        Some(CardState::Waiting),
        "display-only: the failed and refreshed windows never settle or liveness"
    );
    assert_eq!(
        backend.prompt_calls.lock().await.len(),
        1,
        "the window is a pure read: the yield's own prompt is the card's only one"
    );
}

/// Acceptance 6 (spec #588, #592): V1 carries no Background Task facts, so the
/// window read is never issued either — a V1 card shows nothing.
#[tokio::test]
async fn a_v1_read_spends_no_output_window_request() {
    let _wd = test_work_dir();
    let transcript = SessionTranscript::new(vec![
        user("msg_cola_anchor", 1_000, "跑一下构建"),
        typed_message(
            "msg_launch",
            MessageRole::Assistant,
            Some(2_000),
            vec![Part::Tool(ToolCall {
                identity: ToolIdentity {
                    name: "shell".into(),
                    call_id: "call_bg".into(),
                },
                status: ToolStatus::Running,
                started_at: Some(2_000),
                input: Some(serde_json::json!({ "command": "npm run build" })),
                metadata: Some(serde_json::json!({ "background": true })),
                output: ToolOutput {
                    raw: None,
                    blocks: vec![ContentBlock::Text("moved to background".into())],
                    error: None,
                },
            })],
        ),
    ]);
    let (_dir, app, backend, platform) = scripted_app(vec![transcript], Some(SessionStatus::Busy)).await;

    let _turn = spawn_turn(&app, ctx("ses_test", "跑一下构建"));
    wait_for_card_update(&platform, "the V1 panel", CardUpdates::Latest, |card| {
        card_text(card).contains("moved to background")
    })
    .await;
    tokio::time::sleep(Duration::from_millis(150)).await;

    assert!(
        backend.shell_output_calls.lock().await.is_empty()
            && backend.task_runtime_calls.lock().await.is_empty(),
        "a read with no live shell spends no window or runtime request, whatever the generation"
    );
    let latest = platform.updated_cards().await.last().cloned().unwrap();
    assert!(
        !card_text(&latest).contains("截至于"),
        "a V1 card never shows a window: {latest}"
    );
}

// ---------------------------------------------------------------------------
// One admitted reconcile cycle, ONE read budget (spec #588, review PR #595):
// the runtime verdict read, the child-evidence reads and the shell-window
// reads all spend one `read_timeout_ms` window. Each later read gets only the
// remainder; a read the spent budget cannot fund is skipped — no request at
// all — and the next cycle retries it. A stalled runtime or capture can never
// stack another full timeout per task.
// ---------------------------------------------------------------------------

/// Review PR #595: the runtime retires one shell while two surviving shells'
/// window reads hang — the retirement is still observed and rendered, and the
/// whole cycle stays within ONE read budget. The first hung window read spends
/// the remainder; the second is skipped (never issued, no second timeout
/// stacked), and both omitted windows render as no window at all — never a
/// placeholder. The retiree ends 已失联 (identity-only): its entry spends no
/// output read of its own, so the cycle's reads are exactly the two windows.
#[tokio::test]
async fn a_retirement_cycle_stays_within_one_read_budget() {
    let _wd = test_work_dir();
    let now = chrono::Utc::now().timestamp_millis();
    let live = SessionTranscript::new(two_task_timeline(vec![]))
        .with_executions(vec![execution(2_500)])
        .with_background_tasks(vec![
            live_shell(now - 5_000, "call_bg"),
            live_shell(now - 4_000, "call_bg2"),
            live_shell(now - 3_000, "call_bg3"),
        ]);
    let (_dir, app, backend, platform) = scripted_app(vec![live], Some(SessionStatus::Idle)).await;
    backend.task_runtime.lock().unwrap().shells = vec![("sh_call_bg".into(), ShellRuntime::Missing)];
    backend.hang_shell_output_reads(usize::MAX);
    let budget = 2_000;
    app.turn_drain_timeout_ms.store(budget, Ordering::Relaxed);

    let started = std::time::Instant::now();
    Turn::run(&app.turn_handles(), ctx("ses_test", "跑一下构建并审阅"))
        .await
        .unwrap();
    let elapsed = started.elapsed();

    // The first hung window read spends the cycle's remaining budget; the
    // second survivor's read is skipped — never issued, so no timeout stacks.
    let window_reads = backend.shell_output_calls.lock().await.clone();
    assert!(
        window_reads.len() <= 1,
        "one window read at most may be issued in the cycle: {window_reads:?}"
    );
    if let Some(first) = window_reads.first() {
        assert_eq!(
            first, "sh_call_bg2",
            "the cycle reads the first live shell: {window_reads:?}"
        );
    }
    assert!(
        elapsed < Duration::from_millis(budget + budget / 2),
        "the cycle must stay within one read budget, not one per shell: {elapsed:?}"
    );

    // The retirement rendered on the very cycle that observed it...
    let card = platform.updated_cards().await.last().cloned().unwrap();
    let text = card_text(&card);
    assert!(
        text.contains("🔔 shell 已失联：gh run watch"),
        "the retirement entry renders after the bounded cycle: {card}"
    );
    // ...the skipped windows are omitted, and the two live shells keep the
    // wait: nothing was guessed dead.
    assert!(
        !text.contains("截至于"),
        "a skipped window read renders no window at all: {card}"
    );
    assert!(
        text.contains("⏳ 后台任务（2）"),
        "the two surviving shells keep the wait: {card}"
    );
    assert_eq!(
        Turn::card_state(&app.cards_handle(), "ses_test").await,
        Some(CardState::Waiting),
        "the bounded cycle never ends the wait by itself"
    );
}

/// Review PR #595: several suspects' evidence reads hang — the cycle stays
/// within ONE read budget and every marker stays unconfirmed. The first hung
/// read spends the remainder; the later suspects are skipped (no request, no
/// extra timeout), and with no evidence applied nothing is guessed dead:
/// every row keeps ⚠️ 状态待确认 and the card keeps its wait.
#[tokio::test]
async fn hanging_evidence_reads_stay_within_one_read_budget() {
    let _wd = test_work_dir();
    let now = chrono::Utc::now().timestamp_millis();
    let live = SessionTranscript::new(two_task_timeline(vec![]))
        .with_executions(vec![execution(2_500)])
        .with_background_tasks(vec![
            live_subagent(now - 5_000, "call_sub1"),
            live_subagent(now - 4_000, "call_sub2"),
            live_subagent(now - 3_000, "call_sub3"),
        ]);
    let (_dir, app, backend, platform) = scripted_app(vec![live], Some(SessionStatus::Idle)).await;
    backend.task_runtime.lock().unwrap().children = vec![
        ("ses_call_sub1".into(), ChildRuntime::Inactive),
        ("ses_call_sub2".into(), ChildRuntime::Inactive),
        ("ses_call_sub3".into(), ChildRuntime::Inactive),
    ];
    backend.hang_child_evidence_reads(usize::MAX);
    let budget = 2_000;
    app.turn_drain_timeout_ms.store(budget, Ordering::Relaxed);

    let started = std::time::Instant::now();
    Turn::run(&app.turn_handles(), ctx("ses_test", "跑一下构建并审阅"))
        .await
        .unwrap();
    let elapsed = started.elapsed();

    // The first hung evidence read spends the cycle's remaining budget; every
    // later suspect is skipped — never issued, so no timeout stacks.
    let reads = backend.child_evidence_calls.lock().await.clone();
    assert!(
        reads.len() <= 1,
        "one evidence read at most may be issued in the cycle: {reads:?}"
    );
    if let Some(first) = reads.first() {
        assert_eq!(
            first, "ses_call_sub1",
            "the cycle reads the first suspect: {reads:?}"
        );
    }
    assert!(
        elapsed < Duration::from_millis(budget + budget / 2),
        "the cycle must stay within one read budget, not one per suspect: {elapsed:?}"
    );

    // No evidence means no guess: every suspect keeps its marker, no entry
    // renders, and the wait stands.
    let card = platform.updated_cards().await.last().cloned().unwrap();
    let text = card_text(&card);
    assert!(
        text.contains("⏳ 后台任务（3 · 3 待确认）"),
        "every hung suspect keeps its marker: {card}"
    );
    assert!(
        !text.contains("🔔 subagent"),
        "a hung evidence read never guesses an ending: {card}"
    );
    assert_eq!(
        Turn::card_state(&app.cards_handle(), "ses_test").await,
        Some(CardState::Waiting),
        "no evidence means no ending: the card keeps its wait"
    );
}

// ---------------------------------------------------------------------------
// A completion entry's output tail (spec #588, ticket #593): when a shell
// ends — a Wake's 完成/取消/失败 or the runtime's 结束 — its folded body
// carries the result, read once at the entry's own render. A record the read
// cannot answer for says 「输出已不可用」; the 已失联 and 🧹 endings stay
// identity-only, and a subagent entry never spends a shell read.
// ---------------------------------------------------------------------------

/// Script one shell's output window (spec #588, #593) under an arbitrary shell
/// id: the entry fixtures retire `sh_bg`/`sh_call_bg` alike, while
/// [`given_shell_output`] names the live-row fixture alone.
fn given_shell_tail(backend: &Arc<MockBackend>, shell_id: &str, text: &str, clipped: bool, captured_ms: i64) {
    backend.shell_outputs.lock().unwrap().insert(
        shell_id.into(),
        Some(crate::backend::ShellOutputWindow {
            text: text.into(),
            clipped,
            captured_ms,
        }),
    );
}

/// Acceptance 1 (spec #588, #593): a runtime-retired shell's completion entry
/// carries its output tail on the LIVE card — the 已截断 label when clipped,
/// never the live row's 截至于 read clock — and spends exactly one read, at the
/// entry's own render. The retirement read
/// spends no live-window read (the shell leaves the live list first), and no
/// later pass re-reads the retired shell.
#[tokio::test]
async fn a_live_retirements_entry_carries_the_output_tail_once() {
    let _wd = test_work_dir();
    let now = chrono::Utc::now().timestamp_millis();
    let live = waiting_shell(now - 5_000);
    let (_dir, app, backend, platform) = scripted_app(vec![live], Some(SessionStatus::Idle)).await;
    given_shell_tail(&backend, "sh_call_bg", "step 1\nstep 2", true, now);
    backend.task_runtime.lock().unwrap().shells = vec![(
        "sh_call_bg".into(),
        ShellRuntime::Ended {
            end: ShellEnd::Killed,
            completed_at: Some(now - 1_000),
        },
    )];

    Turn::run(&app.turn_handles(), ctx("ses_test", "跑一下构建并审阅"))
        .await
        .unwrap();

    assert_eq!(
        Turn::card_state(&app.cards_handle(), "ses_test").await,
        Some(CardState::Done),
        "the runtime end still settles the turn directly"
    );
    let final_card = platform.updated_cards().await.last().cloned().unwrap();
    let text = card_text(&final_card);
    assert!(
        text.contains("🔔 shell 结束：gh run watch"),
        "the ending's entry renders: {final_card}"
    );
    assert!(
        !text.contains("截至于")
            && text.contains("仅最后 2 行 · 已截断")
            && text.contains("step 1")
            && text.contains("step 2"),
        "the entry's fold body carries the truncated tail, no read clock: {final_card}"
    );
    assert_eq!(
        backend.shell_output_calls.lock().await.clone(),
        vec!["sh_call_bg".to_string()],
        "one output read for the entry render, no live-window read"
    );
    assert_eq!(
        backend.prompt_calls.lock().await.len(),
        1,
        "the tail is a pure read: the turn's own prompt is the card's only one"
    );
}

/// Spec #588/#593 (review): the turn's FINAL-render path carries the tail too
/// — the Wake that retires the shell lands on the finalization read after the
/// drain's last read, so its entry is first rendered there; it still spends
/// its one output read, like every other venue, never a bare entry.
#[tokio::test]
async fn a_finalization_entry_carries_the_output_tail() {
    let _wd = test_work_dir();
    let now = chrono::Utc::now().timestamp_millis();
    let settled = SessionTranscript::new(vec![
        user("msg_cola_anchor", 1_000, "跑一下 CI"),
        assistant(2_000, "已经交给后台了。"),
    ]);
    let waked = SessionTranscript::new(vec![
        user("msg_cola_anchor", 1_000, "跑一下 CI"),
        assistant(2_000, "已经交给后台了。"),
        assistant(3_100, "CI 通过了。"),
    ])
    .with_executions(vec![execution(2_500), execution(4_000)])
    .with_wakes(vec![shell_wake(2_900)]);
    let (_dir, app, backend, platform) = scripted_app(vec![settled, waked], Some(SessionStatus::Idle)).await;
    given_shell_tail(&backend, "sh_bg", "step 1\nstep 2", true, now);

    Turn::run(&app.turn_handles(), ctx("ses_test", "跑一下 CI"))
        .await
        .unwrap();

    let final_card = platform.updated_cards().await.last().cloned().unwrap();
    let text = card_text(&final_card);
    assert!(
        text.contains("🔔 shell 完成：gh run watch"),
        "the final read's entry renders: {final_card}"
    );
    assert!(
        !text.contains("截至于")
            && text.contains("仅最后 2 行 · 已截断")
            && text.contains("step 1")
            && text.contains("step 2"),
        "the final-render entry carries the truncated tail, no read clock: {final_card}"
    );
    assert_eq!(
        backend.shell_output_calls.lock().await.clone(),
        vec!["sh_bg".to_string()],
        "one output read, spent by the final-render entry itself"
    );
}

/// Spec #588/#593 (review): an unreadable record on the final-render path gets
/// the same honest copy as every other venue — 「输出已不可用」, exactly one
/// read.
#[tokio::test]
async fn a_finalization_entry_with_no_record_says_output_unavailable() {
    let _wd = test_work_dir();
    let settled = SessionTranscript::new(vec![
        user("msg_cola_anchor", 1_000, "跑一下 CI"),
        assistant(2_000, "已经交给后台了。"),
    ]);
    let waked = SessionTranscript::new(vec![
        user("msg_cola_anchor", 1_000, "跑一下 CI"),
        assistant(2_000, "已经交给后台了。"),
        assistant(3_100, "CI 通过了。"),
    ])
    .with_executions(vec![execution(2_500), execution(4_000)])
    .with_wakes(vec![shell_wake(2_900)]);
    let (_dir, app, backend, platform) = scripted_app(vec![settled, waked], Some(SessionStatus::Idle)).await;

    Turn::run(&app.turn_handles(), ctx("ses_test", "跑一下 CI"))
        .await
        .unwrap();

    let final_card = platform.updated_cards().await.last().cloned().unwrap();
    let text = card_text(&final_card);
    assert!(
        text.contains("🔔 shell 完成：gh run watch") && text.contains("输出已不可用"),
        "the final-render entry says the record is unavailable: {final_card}"
    );
    assert!(!text.contains("截至于"), "never an empty panel: {final_card}");
    assert_eq!(
        backend.shell_output_calls.lock().await.clone(),
        vec!["sh_bg".to_string()],
        "one output read, spent by the final-render entry itself"
    );
}

/// Acceptance 1 (spec #588, #593): the same tail lands on the WAITING card's
/// quiet true end — the entry is planned, read and committed through the
/// yielded refresh's own site, still exactly one read.
#[tokio::test]
async fn a_waiting_cards_completion_entry_carries_the_output_tail() {
    let _wd = test_work_dir();
    let now = chrono::Utc::now().timestamp_millis();
    let live = waiting_shell(now - 5_000);
    let (_dir, app, backend, platform) = scripted_app(vec![live], Some(SessionStatus::Idle)).await;
    yield_one_task_card(&app, &platform, ctx("ses_test", "跑一下构建并审阅")).await;

    // The entry's own render is the only read this test counts: clear the
    // yield's live-window reads and script the tail afterwards.
    given_shell_tail(&backend, "sh_bg", "build ok", true, now);
    backend.shell_output_calls.lock().await.clear();
    script_quiet_true_end(&backend).await;
    spawn_sync(&app);
    wait_for_card_update(&platform, "the settled card", CardUpdates::Latest, |card| {
        card_header(card).contains("✅") && card_text(card).contains("🔔 shell 完成：gh run watch")
    })
    .await;

    let settled = platform.updated_cards().await.last().cloned().unwrap();
    let text = card_text(&settled);
    assert!(
        !text.contains("截至于") && text.contains("仅最后 1 行 · 已截断") && text.contains("build ok"),
        "the waiting card's entry carries the truncated tail, no read clock: {settled}"
    );
    assert_eq!(
        backend.shell_output_calls.lock().await.clone(),
        vec!["sh_bg".to_string()],
        "exactly one output read, spent by the entry's own render"
    );
}

/// Acceptance 2 (spec #588, #593): a record that cannot be read says
/// 「输出已不可用」 — a failed read and a vanished record alike — never an
/// empty panel posing as output.
#[tokio::test]
async fn an_unreadable_entry_record_says_output_unavailable() {
    let _wd = test_work_dir();
    let now = chrono::Utc::now().timestamp_millis();

    // A failed read: every output read fails, so the entry's own read fails
    // whichever cycle reaches it first.
    let live = waiting_shell(now - 5_000);
    let (_dir, app, backend, platform) = scripted_app(vec![live], Some(SessionStatus::Idle)).await;
    yield_one_task_card(&app, &platform, ctx("ses_test", "跑一下构建并审阅")).await;
    backend.fail_shell_output_reads.store(1_000, Ordering::SeqCst);
    script_quiet_true_end(&backend).await;
    spawn_sync(&app);
    wait_for_card_update(&platform, "the settled card", CardUpdates::Latest, |card| {
        card_header(card).contains("✅") && card_text(card).contains("🔔 shell 完成：gh run watch")
    })
    .await;
    let settled = platform.updated_cards().await.last().cloned().unwrap();
    let text = card_text(&settled);
    assert!(
        text.contains("输出已不可用"),
        "a failed read says the record is unavailable: {settled}"
    );
    assert!(!text.contains("截至于"), "never a stale tail: {settled}");

    // A vanished record: the read succeeds and answers nothing (the runtime
    // no longer keeps it), which is the same no-output case.
    let live = waiting_shell(now - 5_000);
    let (_dir2, app2, backend2, platform2) = scripted_app(vec![live], Some(SessionStatus::Idle)).await;
    yield_one_task_card(&app2, &platform2, ctx("ses_test", "跑一下构建并审阅")).await;
    script_quiet_true_end(&backend2).await;
    spawn_sync(&app2);
    wait_for_card_update(&platform2, "the settled card", CardUpdates::Latest, |card| {
        card_header(card).contains("✅") && card_text(card).contains("🔔 shell 完成：gh run watch")
    })
    .await;
    let settled = platform2.updated_cards().await.last().cloned().unwrap();
    let text = card_text(&settled);
    assert!(
        text.contains("输出已不可用"),
        "a vanished record says the same: {settled}"
    );
    assert!(!text.contains("截至于"), "never an empty panel: {settled}");
}

/// Spec #588/#593 (review): a readable-but-EMPTY capture is not the
/// unavailable copy — the record answered, it just holds nothing — so the
/// live row shows no window and the completion entry renders no output
/// section at all, while still spending its one read.
#[tokio::test]
async fn an_empty_capture_renders_no_window_and_no_output_section() {
    let _wd = test_work_dir();
    let now = chrono::Utc::now().timestamp_millis();
    let live = waiting_shell(now - 5_000);
    let (_dir, app, backend, platform) = scripted_app(vec![live], Some(SessionStatus::Idle)).await;
    // The record answers an empty capture — a successful read with nothing to
    // show — so the live row's window is omitted, never a placeholder.
    given_shell_tail(&backend, "sh_call_bg", "", false, now);
    yield_one_task_card(&app, &platform, ctx("ses_test", "跑一下构建并审阅")).await;
    let yielded = platform.updated_cards().await.last().cloned().unwrap();
    assert!(
        !card_text(&yielded).contains("截至于"),
        "an empty capture renders no live window: {yielded}"
    );

    // The entry's own read answers the same empty capture: the fold body stays
    // identity-only — 「输出已不可用」 belongs to a gone or unreadable record.
    given_shell_tail(&backend, "sh_bg", "", false, now);
    backend.shell_output_calls.lock().await.clear();
    script_quiet_true_end(&backend).await;
    spawn_sync(&app);
    wait_for_card_update(&platform, "the settled card", CardUpdates::Latest, |card| {
        card_header(card).contains("✅") && card_text(card).contains("🔔 shell 完成：gh run watch")
    })
    .await;

    let settled = platform.updated_cards().await.last().cloned().unwrap();
    let text = card_text(&settled);
    assert!(
        !text.contains("输出已不可用"),
        "an empty capture is readable, not unavailable: {settled}"
    );
    assert!(
        !text.contains("截至于"),
        "nothing to show: no output section: {settled}"
    );
    assert_eq!(
        backend.shell_output_calls.lock().await.clone(),
        vec!["sh_bg".to_string()],
        "the entry's read is still spent exactly once"
    );
}

/// Acceptance 2 (spec #588, #593): the 已失联 entry carries identity only —
/// there is no record to read, so it must not spend an output read nor invent
/// one, even when a window is scripted for its shell id.
#[tokio::test]
async fn a_lost_entry_stays_identity_only_with_no_output_read() {
    let _wd = test_work_dir();
    let now = chrono::Utc::now().timestamp_millis();
    let live = waiting_shell(now - 5_000);
    let (_dir, app, backend, platform) = scripted_app(vec![live], Some(SessionStatus::Idle)).await;
    yield_one_task_card(&app, &platform, ctx("ses_test", "跑一下构建并审阅")).await;

    // A window exists for the shell, but the lost ending must never show it.
    given_shell_tail(&backend, "sh_call_bg", "should never render", false, now);
    backend.shell_output_calls.lock().await.clear();
    backend.task_runtime.lock().unwrap().shells = vec![("sh_call_bg".into(), ShellRuntime::Missing)];
    spawn_sync(&app);
    wait_for_card_update(&platform, "the lost settle", CardUpdates::Latest, |card| {
        card_header(card).contains("✅") && card_text(card).contains("🔔 shell 已失联：gh run watch")
    })
    .await;

    let settled = platform.updated_cards().await.last().cloned().unwrap();
    let text = card_text(&settled);
    assert!(
        text.contains("shell sh_call_bg") && !text.contains("shell sh_call_bg · "),
        "the lost entry keeps identity and no invented clock: {settled}"
    );
    assert!(
        !text.contains("输出已不可用") && !text.contains("截至于") && !text.contains("should never render"),
        "the lost entry stays identity-only: {settled}"
    );
    assert!(
        backend.shell_output_calls.lock().await.is_empty(),
        "the lost entry spends no output read"
    );
}

/// Acceptance 2 (spec #588, #593): a subagent's runtime/evidence ending has no
/// shell output — the entry stays identity-only and spends no output read,
/// even with a window scripted for its child id.
#[tokio::test]
async fn a_subagent_entry_spends_no_output_read() {
    let _wd = test_work_dir();
    let now = chrono::Utc::now().timestamp_millis();
    let live = SessionTranscript::new(two_task_timeline(vec![]))
        .with_executions(vec![execution(2_500)])
        .with_background_tasks(vec![live_subagent(now - 3_000, "call_sub")]);
    let (_dir, app, backend, platform) = scripted_app(vec![live], Some(SessionStatus::Idle)).await;
    script_child_without_activity(&backend).await;
    given_shell_tail(&backend, "ses_call_sub", "should never render", false, now);
    backend.task_runtime.lock().unwrap().children = vec![("ses_call_sub".into(), ChildRuntime::Inactive)];
    backend
        .with_child_evidence(
            "ses_call_sub",
            ChildEvidence::Terminal {
                completed_at: now - 1_000,
            },
        )
        .await;

    Turn::run(&app.turn_handles(), ctx("ses_test", "跑一下构建并审阅"))
        .await
        .unwrap();

    let final_card = platform.updated_cards().await.last().cloned().unwrap();
    let text = card_text(&final_card);
    assert_eq!(
        text.matches("🔔 subagent 结束：review the diff").count(),
        1,
        "the subagent's evidence ending renders: {final_card}"
    );
    assert!(
        !text.contains("输出已不可用") && !text.contains("截至于") && !text.contains("should never render"),
        "a subagent entry has no shell output: {final_card}"
    );
    assert!(
        backend.shell_output_calls.lock().await.is_empty(),
        "no shell read is ever spent on a subagent entry"
    );
}

/// Acceptance 2 (spec #588, #590/#593): the user's own cleanup entry never
/// carries an output tail — it is not the shell's ending — so its render
/// spends no read, whatever window exists for the task's id.
#[tokio::test]
async fn a_cleaned_entry_spends_no_output_read() {
    let _wd = test_work_dir();
    let now = chrono::Utc::now().timestamp_millis();
    let live = SessionTranscript::new(two_task_timeline(vec![]))
        .with_executions(vec![execution(2_500)])
        .with_background_tasks(vec![live_subagent(now - 3_000, "call_sub")]);
    let (_dir, app, backend, platform) = scripted_app(vec![live], Some(SessionStatus::Idle)).await;
    script_child_without_activity(&backend).await;
    yield_one_task_card(&app, &platform, ctx("ses_test", "跑一下构建并审阅")).await;

    // The runtime reports the child inactive with no concluding evidence: the
    // row gains the marker — and the waiting card the cleanup button.
    backend.task_runtime.lock().unwrap().children = vec![("ses_call_sub".into(), ChildRuntime::Inactive)];
    spawn_sync(&app);
    wait_for_card_update(&platform, "the unconfirmed marker", CardUpdates::Latest, |card| {
        card_text(card).contains("⚠️ 状态待确认")
    })
    .await;

    given_shell_tail(&backend, "ses_call_sub", "should never render", false, now);
    backend.shell_output_calls.lock().await.clear();
    app.host_action(serde_json::json!({ "action": "cleanup", "session_id": "ses_test" }))
        .await
        .expect("the cleanup click acks");
    wait_for_card_update(&platform, "the cleaned settle", CardUpdates::Latest, |card| {
        card_header(card).contains("✅")
            && card_text(card).contains("🧹 subagent 已清理：review the diff（人工）")
    })
    .await;

    let settled = platform.updated_cards().await.last().cloned().unwrap();
    let text = card_text(&settled);
    assert!(
        !text.contains("输出已不可用") && !text.contains("截至于") && !text.contains("should never render"),
        "the 🧹 entry is identity-only: {settled}"
    );
    assert!(
        backend.shell_output_calls.lock().await.is_empty(),
        "the cleanup ending spends no output read"
    );
}
