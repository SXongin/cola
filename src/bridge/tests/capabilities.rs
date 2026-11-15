//! Agent capabilities gate every optional surface (ADR-0106, spec #1057 P5,
//! ticket #1062).
//!
//! Each case declares one axis absent on the mock client and asserts the
//! **surface is not in the card** (or the command answers unsupported) — never
//! that a renderer returned an empty string. Every case also drives the
//! full-capability control, so a broken fixture cannot pass by rendering
//! nothing.
//!
//! The fake client's declaration is a construction fact
//! ([`MockBackend::capabilities`]), so a case scripts it before the App is
//! built, exactly as a real attachment answers it once.

use super::drain::{assistant, ctx, scripted_app_prepared, user};
use crate::backend::types::{
    FormFieldKind, QuestionInfo, QuestionOption, QuestionRequest, RequestScope, SessionStatus,
};
use crate::backend::{
    AgentCapabilities, BackgroundTask, MessageId, MessageRole, Part, TodoList, TodoStatus, TodoStep,
    TokenUsage, ToolCall, ToolIdentity, ToolOutput, ToolStatus, TranscriptMessage, TurnDiff, TurnDiffFile,
};
use crate::bridge::test_support::*;
use crate::bridge::turn::Turn;
use crate::feishu::card::CardState;

/// [`AgentCapabilities::full`] with the named axes cleared.
fn caps_with(configure: impl FnOnce(&mut AgentCapabilities)) -> AgentCapabilities {
    let mut capabilities = AgentCapabilities::full();
    configure(&mut capabilities);
    capabilities
}

/// One server-change diff, so the turn-diff index has counts to render.
fn one_file_diff() -> TurnDiff {
    TurnDiff(vec![TurnDiffFile {
        path: "src/lib.rs".into(),
        patch: "@@ -1 +1 @@\n-old\n+new\n".into(),
        additions: 1,
        deletions: 1,
        status: "modified".into(),
    }])
}

/// The card surfaces' fixture: a settled turn that carries a plan (a Todo
/// List), a Compaction row, token usage and a one-file diff — every card
/// surface the capability table can omit.
fn surface_transcript() -> crate::backend::SessionTranscript {
    let assistant = TranscriptMessage {
        tokens: Some(TokenUsage {
            input: 100,
            output: 50,
            total: 42_000,
            cache_read: 0,
            cache_write: 0,
        }),
        ..assistant(2_000, "答案。")
    };
    crate::backend::SessionTranscript::new(vec![
        user("msg_cola_anchor", 1_000, "做点活"),
        compaction_message("msg_cmp", 1_500, "压缩摘要"),
        assistant,
    ])
    .with_todo_list(
        MessageId::new("msg_cola_anchor"),
        TodoList {
            explanation: None,
            steps: vec![TodoStep {
                text: "检查边界".into(),
                status: TodoStatus::InProgress,
            }],
        },
    )
}

/// Run the surface transcript to a settled card under `capabilities`.
async fn card_with(capabilities: AgentCapabilities) -> serde_json::Value {
    let (_dir, app, _backend, platform) = scripted_app_prepared(
        vec![surface_transcript()],
        Some(SessionStatus::Idle),
        |_| {},
        move |backend| {
            backend.capabilities = capabilities;
            backend.given_session_diff(one_file_diff());
        },
    )
    .await;
    Turn::run(&app.turn_handles(), ctx("ses_test", "做点活"))
        .await
        .unwrap();
    platform
        .updated_cards()
        .await
        .last()
        .cloned()
        .expect("the settled turn writes its card")
}

/// The card surfaces follow the ADR-0106 table: each absent axis removes its
/// own surface from the card, and the full client keeps it. The assertion is
/// on the card bytes, never on a renderer's return value.
#[tokio::test]
async fn card_surfaces_follow_the_capability_table() {
    struct Case {
        axis: &'static str,
        absent: fn(&mut AgentCapabilities),
        marker: &'static str,
    }
    // A marker is text/element identity that only its surface writes: the plan
    // step lives in the todo panel, the compaction word in its row, 📊 in the
    // footer, and the button copy in the change index.
    let cases = [
        Case {
            axis: "plan",
            absent: |caps| caps.plan = crate::backend::PlanMode::None,
            marker: "检查边界",
        },
        Case {
            axis: "compaction",
            absent: |caps| caps.compaction = false,
            marker: "压缩摘要",
        },
        Case {
            axis: "usage",
            absent: |caps| caps.usage = crate::backend::UsageMode::None,
            marker: "📊",
        },
        Case {
            axis: "diffs",
            absent: |caps| caps.diffs = false,
            marker: "查看完整变化",
        },
    ];
    for case in cases {
        let full = card_text(&card_with(AgentCapabilities::full()).await);
        assert!(
            full.contains(case.marker),
            "{}: the full-capability control renders its surface: {full}",
            case.axis
        );
        let absent = card_text(&card_with(caps_with(case.absent)).await);
        assert!(
            !absent.contains(case.marker),
            "{}: an absent surface must not be in the card: {absent}",
            case.axis
        );
    }
}

/// One session-backed question the sweep can surface.
fn session_question(id: &str) -> QuestionRequest {
    QuestionRequest {
        id: id.into(),
        session_id: "ses_test".into(),
        questions: vec![QuestionInfo {
            question: "继续吗？".into(),
            header: "确认".into(),
            options: vec![QuestionOption {
                label: "继续".into(),
                description: String::new(),
                ..Default::default()
            }],
            kind: FormFieldKind::String,
            custom: None,
            ..Default::default()
        }],
        ..Default::default()
    }
}

/// One directory-owned Location Form: no Session, the directory is the owner.
fn location_question(id: &str) -> QuestionRequest {
    QuestionRequest {
        id: id.into(),
        session_id: String::new(),
        questions: vec![QuestionInfo {
            question: "选个目录".into(),
            header: "目录".into(),
            options: Vec::new(),
            kind: FormFieldKind::String,
            custom: None,
            ..Default::default()
        }],
        scope: RequestScope::Location,
        ..Default::default()
    }
}

/// The question axis gates both the card and the Location Form at their source:
/// `None` surfaces nothing at all, `Session` drops a Location Form before it
/// can be listed, and the full client posts the question.
#[tokio::test]
async fn question_surfaces_follow_the_question_axis() {
    async fn sweep_and_post_count(capabilities: AgentCapabilities, question: QuestionRequest) -> usize {
        let (_dir, app, _backend, platform) = scripted_app_prepared(
            vec![crate::backend::SessionTranscript::new(vec![user(
                "msg_cola_anchor",
                1_000,
                "做点活",
            )])],
            Some(SessionStatus::Idle),
            |_| {},
            move |backend| {
                backend.capabilities = capabilities;
                backend.ask_question(question);
            },
        )
        .await;
        let mut seen = std::collections::HashSet::new();
        app.question.sweep(&app.flow_handles(), &mut seen).await;
        posted_cards(&platform)
            .await
            .iter()
            .filter(|card| card_text(card).contains("继续吗？") || card_text(card).contains("选个目录"))
            .count()
    }

    assert_eq!(
        sweep_and_post_count(AgentCapabilities::full(), session_question("q_full")).await,
        1,
        "the full client surfaces its session question"
    );
    assert_eq!(
        sweep_and_post_count(
            caps_with(|caps| caps.questions = crate::backend::QuestionMode::None),
            session_question("q_none")
        )
        .await,
        0,
        "no question surface means no card"
    );
    assert_eq!(
        sweep_and_post_count(
            caps_with(|caps| caps.questions = crate::backend::QuestionMode::Session),
            location_question("q_loc"),
        )
        .await,
        0,
        "a session-only client never surfaces a Location Form"
    );
    assert_eq!(
        sweep_and_post_count(AgentCapabilities::full(), location_question("q_loc_full")).await,
        1,
        "the full client surfaces the Location Form"
    );
}

/// The command surfaces report an unsupported capability instead of rendering
/// an empty card: `/mcp`, `/compact` and `/agent`.
#[tokio::test]
async fn command_surfaces_follow_their_axis() {
    // `/mcp` on a client without MCP statuses: the command answers 不可用 and
    // posts no card (never a status-only or empty card).
    let (_dir, app, _backend, platform) = scripted_app_prepared(
        vec![crate::backend::SessionTranscript::new(vec![user(
            "msg_cola_anchor",
            1_000,
            "做点活",
        )])],
        Some(SessionStatus::Idle),
        |_| {},
        |backend| {
            backend.capabilities = caps_with(|caps| caps.mcp = false);
            backend.with_mcp_statuses_in("/work", Vec::new());
        },
    )
    .await;
    send_command(&app, "/mcp", "msg_mcp_absent").await;
    let reply = last_reply_text(&platform).await.expect("a reply");
    assert!(reply.contains("不可用"), "/mcp reports unsupported: {reply}");
    assert!(
        !platform_calls(&platform).await.iter().any(|call| matches!(
            call,
            PlatformCall::ReplyCard { .. } | PlatformCall::SendCard { .. }
        )),
        "no empty /mcp card is posted"
    );

    // `/compact` on a client without compaction: unsupported, and no compact
    // call is made.
    let (_dir, app, backend, platform) = scripted_app_prepared(
        vec![crate::backend::SessionTranscript::new(vec![user(
            "msg_cola_anchor",
            1_000,
            "做点活",
        )])],
        Some(SessionStatus::Idle),
        |_| {},
        |backend| backend.capabilities = caps_with(|caps| caps.compaction = false),
    )
    .await;
    send_command(&app, "/compact", "msg_compact_absent").await;
    let reply = last_reply_text(&platform).await.expect("a reply");
    assert_eq!(
        reply, "当前 Agent Client（OpenCode）不提供上下文压缩，`/compact` 不可用。",
        "the shared sentence keeps /compact's exact copy"
    );
    assert!(
        backend.compact_calls.lock().await.is_empty(),
        "an unsupported /compact spends no compact call"
    );

    // `/agent` on a client with no agent selection: unsupported, no picker.
    let (_dir, app, _backend, platform) = scripted_app_prepared(
        vec![crate::backend::SessionTranscript::new(vec![user(
            "msg_cola_anchor",
            1_000,
            "做点活",
        )])],
        Some(SessionStatus::Idle),
        |_| {},
        |backend| backend.capabilities = caps_with(|caps| caps.modes = false),
    )
    .await;
    send_command(&app, "/agent", "msg_agent_absent").await;
    let reply = last_reply_text(&platform).await.expect("a reply");
    assert_eq!(
        reply, "当前 Agent Client（OpenCode）不提供 Agent 选择，`/agent` 不可用。",
        "the shared sentence restores the space before the Latin noun"
    );
}

/// The panel liveness line follows `child_agents`: a client with no child-agent
/// concept still renders the launch panel and the ledger row, but spends no
/// child read and shows no activity fragment.
#[tokio::test]
async fn panel_liveness_follows_the_child_agent_axis() {
    let now = chrono::Utc::now().timestamp_millis();
    let transcript =
        crate::backend::SessionTranscript::new(vec![user("msg_cola_anchor", 1_000, "跑一下构建并审阅")])
            .with_background_tasks(vec![BackgroundTask {
                tool: ToolIdentity {
                    name: "subagent".into(),
                    call_id: "call_sub".into(),
                },
                shell_id: None,
                child_id: Some("ses_call_sub".into()),
                started_at: Some(now - 30_000),
            }]);
    let (_dir, app, backend, platform) = scripted_app_prepared(
        vec![transcript],
        Some(SessionStatus::Idle),
        |_| {},
        |backend| {
            backend.capabilities = caps_with(|caps| caps.child_agents = crate::backend::ChildAgentMode::None);
        },
    )
    .await;
    backend
        .given_transcript_after_build("ses_call_sub", vec![child_running_tool(now - 5_000, "bash")])
        .await;

    Turn::run(&app.turn_handles(), ctx("ses_test", "跑一下构建并审阅"))
        .await
        .unwrap();

    let card = platform
        .calls
        .lock()
        .await
        .iter()
        .rev()
        .find_map(|call| match call {
            PlatformCall::SendCard { card, .. }
            | PlatformCall::ReplyCard { card, .. }
            | PlatformCall::UpdateMessage { card, .. } => Some(card.clone()),
            _ => None,
        })
        .expect("the turn writes a card");
    assert!(
        !backend
            .transcript_calls
            .lock()
            .await
            .iter()
            .any(|id| id == "ses_call_sub"),
        "no child-agent axis means no child read"
    );
    // The launch's ledger row is still there — only its liveness fragment is
    // gated, not the run's existence.
    assert!(
        card_text(&card).contains("后台任务（1）"),
        "the ledger row still renders under the plain panel: {card}"
    );
    assert!(
        card_text(&card).contains(crate::feishu::card::vocab::TOOL_SUBAGENT),
        "the row names its run kind: {card}"
    );
    // The ledger row still renders (the client reports live runs through child
    // evidence), but carries no activity fragment: the child's running `bash`
    // never reaches the card.
    assert!(
        !card_text(&card).contains("bash "),
        "the plain row carries no liveness fragment: {card}"
    );
}

/// The ledger follows `reports_live_runs()` (`background_runs || child_evidence`):
/// a child-only client with no live run renders no section, while a
/// background-only client shows its launch. A claim with no launch (the test
/// above) renders no section either.
#[tokio::test]
async fn ledger_follows_reports_live_runs() {
    // Child-only (Codex's shape): live runs are possible, but this read has
    // none — so no empty ledger.
    let (_dir, app, _backend, platform) = scripted_app_prepared(
        vec![crate::backend::SessionTranscript::new(vec![
            user("msg_cola_anchor", 1_000, "做点活"),
            assistant(2_000, "答案。"),
        ])],
        Some(SessionStatus::Idle),
        |_| {},
        |backend| {
            backend.with_child_only_runs();
        },
    )
    .await;
    Turn::run(&app.turn_handles(), ctx("ses_test", "做点活"))
        .await
        .unwrap();
    let card = platform.updated_cards().await.last().cloned().unwrap();
    assert!(
        !card_text(&card).contains("后台任务"),
        "a child-only client with no live run renders no section: {card}"
    );

    // Background-only: the launch it reports has its row.
    let now = chrono::Utc::now().timestamp_millis();
    let transcript = crate::backend::SessionTranscript::new(vec![
        user("msg_cola_anchor", 1_000, "跑一下构建"),
        assistant(2_000, "已经交给后台。"),
    ])
    .with_background_tasks(vec![BackgroundTask {
        tool: ToolIdentity {
            name: "shell".into(),
            call_id: "call_bg".into(),
        },
        shell_id: Some("sh_bg".into()),
        child_id: None,
        started_at: Some(now - 5_000),
    }]);
    let (_dir, app, _backend, platform) = scripted_app_prepared(
        vec![transcript],
        Some(SessionStatus::Idle),
        |_| {},
        |backend| {
            backend.capabilities = caps_with(|caps| caps.child_evidence = false);
        },
    )
    .await;
    Turn::run(&app.turn_handles(), ctx("ses_test", "跑一下构建"))
        .await
        .unwrap();
    let card = platform.updated_cards().await.last().cloned().unwrap();
    assert!(
        card_text(&card).contains("后台任务（1）"),
        "a background-only client shows its launch: {card}"
    );
}

/// The Session Card (`/card`) reads the compaction axis: a client without
/// compaction carries no Compaction marker on its static read either.
#[tokio::test]
async fn the_session_card_omits_compaction_when_absent() {
    let transcript = crate::backend::SessionTranscript::new(vec![
        user("msg_cola_anchor", 1_000, "做点活"),
        compaction_message("msg_cmp", 1_500, "压缩摘要"),
        assistant(2_000, "答案。"),
    ]);
    let (_dir, app, _backend, platform) = scripted_app_prepared(
        vec![transcript],
        Some(SessionStatus::Idle),
        |_| {},
        |backend| backend.capabilities.compaction = false,
    )
    .await;
    send_command(&app, "/card", "msg_card").await;
    let cards = posted_cards(&platform).await;
    let card = cards.last().expect("the Session Card is posted");
    assert!(
        !card_text(card).contains("压缩摘要"),
        "a client without compaction's /card carries no marker: {card}"
    );
}

/// A client that claims `background_runs` but reports no launch renders no
/// ledger rows — the honest empty answer, never an invented task — and logs the
/// unsubstantiated claim once (ADR-0106's diagnosable failure). The empty read
/// is the only reachable shape of that claim: the transcript carries neither a
/// Background Task nor a run, so there is no launch fact to draw from.
#[tokio::test]
async fn a_claimed_background_surface_without_launches_renders_no_ledger_rows() {
    let transcript = crate::backend::SessionTranscript::new(vec![
        user("msg_cola_anchor", 1_000, "做点活"),
        assistant(2_000, "答案。"),
    ]);
    let (_dir, app, _backend, platform) = scripted_app_prepared(
        vec![transcript],
        Some(SessionStatus::Idle),
        |_| {},
        |backend| {
            backend.capabilities = caps_with(|caps| {
                caps.background_runs = true;
                caps.child_evidence = false;
            });
        },
    )
    .await;
    // The claim's only reachable shape is an empty read: the transcript names
    // no Background Task and no run, so there is no launch to render. Pin the
    // one-shot diagnostic the gate now emits (pre-change there was none) and
    // the honest empty card.
    let ((), logs) = capture_logs(async {
        Turn::run(&app.turn_handles(), ctx("ses_test", "做点活"))
            .await
            .unwrap();
    })
    .await;
    assert_eq!(
        level_count(
            &logs,
            "claims background runs but this read names no launch",
            "DEBUG"
        ),
        1,
        "the unsubstantiated claim is logged exactly once, not per render:\n{logs}"
    );
    let card = platform.updated_cards().await.last().cloned().unwrap();
    assert!(
        !card_text(&card).contains("后台任务"),
        "a claim with no launch renders no section: {card}"
    );
    assert!(
        !card_text(&card).contains("状态待确认"),
        "it never invents a task row: {card}"
    );
    assert_eq!(
        Turn::card_state(&app.cards_handle(), "ses_test").await,
        Some(CardState::Done),
        "the turn settles rather than yielding on an invented run"
    );
}

/// Every card posted as a NEW message — the surface a sweep creates.
async fn posted_cards(platform: &RecordingPlatform) -> Vec<serde_json::Value> {
    platform
        .calls
        .lock()
        .await
        .iter()
        .filter_map(|call| match call {
            PlatformCall::SendCard { card, .. } | PlatformCall::ReplyCard { card, .. } => Some(card.clone()),
            _ => None,
        })
        .collect()
}

/// A child transcript whose newest content is one running tool — the activity
/// a live row would render when the client has a child-agent surface.
fn child_running_tool(started_at: i64, name: &str) -> crate::backend::SessionTranscript {
    crate::backend::SessionTranscript::new(vec![TranscriptMessage {
        id: MessageId::new("a_child"),
        role: MessageRole::Assistant,
        time: None,
        model: None,
        tokens: None,
        error: None,
        parts: vec![Part::Tool(ToolCall {
            identity: ToolIdentity {
                name: name.into(),
                call_id: "call_child".into(),
            },
            status: ToolStatus::Running,
            started_at: Some(started_at),
            input: Some(serde_json::json!({ "command": "cargo test" })),
            metadata: None,
            output: ToolOutput::default(),
            ..ToolCall::default()
        })],
    }])
}

/// One plain-text reply on the platform, if the command posted one.
async fn last_reply_text(platform: &RecordingPlatform) -> Option<String> {
    platform_calls(platform)
        .await
        .iter()
        .rev()
        .find_map(|call| match call {
            PlatformCall::ReplyText { text, .. } => Some(text.clone()),
            _ => None,
        })
}

/// Every platform call, cloned for assertions.
async fn platform_calls(platform: &RecordingPlatform) -> Vec<PlatformCall> {
    platform.calls.lock().await.clone()
}
