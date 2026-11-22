//! The Terminal Card's clicks (ADR-0086 decisions 4–9, ticket #1123).
//!
//! `action: "terminal"` carries five ops, so this module registers four verbs:
//! the three op-selected ones (`full_output`, `ask_ai`, `rerun`) and the
//! default arm (`op` absent or unknown) that handles `kill`/`continue`. The
//! dispatch selects the entry; only the default arm parses `op` further, to
//! tell `kill` from `continue`.
//!
//! The ack budget is part of the contract: `full_output` and `ask_ai` answer a
//! toast immediately and run their bounded work off the callback, so a 30 MB
//! upload or a whole Turn never runs inside the ack. `rerun` keeps its record
//! check synchronously — cheap — and spawns the run; `kill`/`continue` only
//! enqueue a signal for the run loop that owns the process group.

use std::sync::Arc;

use tokio::time::Instant;

use crate::bridge::card_action::VerbFuture;
use crate::bridge::handler::{
    App, CardActionResult, TerminalAskAiRequest, TerminalCardRequest, thread_key_from_value,
};

/// The Terminal Card's 「📄 完整输出」 click (ADR-0086 decision 7) — the
/// Terminal Command's counterpart of the viewer's 「发为文件」. The card
/// callback must ack inside Feishu's budget, and a 30 MB upload cannot fit it,
/// so the click claims the card, spawns the transfer and answers a toast
/// immediately. The spawned step reads a bounded copy of the **Terminal Log**,
/// sends it as exactly ONE File Message under the clicked card, and PATCHes
/// that card with the outcome — so a large upload neither dies with the
/// callback nor leaves the clicker without a result.
///
/// It reuses the viewer's path on purpose: the same `send_file_message` lane
/// (content-hash upload cache, one bounded send), the same
/// `SharedCore::viewer_transfer_timeout_ms` per-call bound and the same
/// per-card in-flight claim. A second upload implementation is exactly what
/// this must not grow.
pub(crate) fn run_full_output<'a>(
    app: &'a Arc<App>,
    value: &'a serde_json::Value,
    _deadline: Instant,
    _callback_started: Instant,
) -> VerbFuture<'a> {
    Box::pin(async move {
        let Some(request) = TerminalCardRequest::from_value(value) else {
            return Some(CardActionResult {
                card: None,
                toast: Some("无效操作".to_string()),
            });
        };
        let thread_key = thread_key_from_value(value);
        let Some(in_flight_guard) = app.claim_viewer_action(&thread_key, &request.message_id) else {
            return Some(CardActionResult {
                card: None,
                toast: Some("正在发送完整输出…".to_string()),
            });
        };
        let app = Arc::clone(app);
        tokio::spawn(async move {
            let _in_flight_guard = in_flight_guard;
            app.run_terminal_full_output(request).await;
        });
        Some(CardActionResult {
            card: None,
            toast: Some("正在发送完整输出…".to_string()),
        })
    })
}

/// The Terminal Card's 「问 AI」 click (ADR-0086 decision 9) — the ONE explicit
/// door by which a Terminal Command's output reaches the model. No Terminal
/// Command ever submits its own output, so this click is the only path into the
/// Session Transcript.
///
/// The payload is self-describing: the command, the output window and the run
/// id it was rendered for. The submission therefore needs neither a live
/// process nor the **Terminal Log** on disk, so it works on a run the startup
/// sweep marked lost (ADR-0086 decision 5).
///
/// The click claims the run's one submission before anything is spawned, so a
/// redelivered or double-clicked button cannot submit the same output twice,
/// and a click naming a run a newer command replaced is refused as stale
/// (ADR-0086 decision 4) — refused by state, never hidden.
///
/// The callback must ack inside Feishu's budget, and the normal prompt pipeline
/// may ensure a server and wait on a whole Turn, so the click acknowledges and
/// spawns it. The card is PATCHed with what happened to the *submission* — a
/// new Turn or a Supplement — never with a claim that the model answered; the
/// answer belongs to the Turn's own card.
pub(crate) fn run_ask_ai<'a>(
    app: &'a Arc<App>,
    value: &'a serde_json::Value,
    _deadline: Instant,
    _callback_started: Instant,
) -> VerbFuture<'a> {
    Box::pin(async move {
        let Some(request) = TerminalAskAiRequest::from_value(value) else {
            return Some(CardActionResult {
                card: None,
                toast: Some("无效操作".to_string()),
            });
        };
        let thread_key = thread_key_from_value(value);
        let (guard, toast) = match crate::bridge::terminal::AskAiGuard::claim(
            &app.terminal_runs,
            &app.terminal_records,
            &thread_key,
            &request.card.run_id,
        ) {
            crate::bridge::terminal::AskAiClaim::Accepted(guard) => (guard, "🤖 正在把终端输出提交给 AI…"),
            crate::bridge::terminal::AskAiClaim::InFlight => {
                return Some(CardActionResult {
                    card: None,
                    toast: Some("正在提交给 AI…".to_string()),
                });
            }
            crate::bridge::terminal::AskAiClaim::AlreadySubmitted => {
                return Some(CardActionResult {
                    card: None,
                    toast: Some("这份输出已经提交给 AI 了。".to_string()),
                });
            }
            crate::bridge::terminal::AskAiClaim::Stale => {
                return Some(CardActionResult {
                    card: None,
                    toast: Some("这张终端卡片的操作已过期。".to_string()),
                });
            }
            // The durable authority is unreadable and this process never ran
            // the command: nothing can prove the card current, so refuse rather
            // than hand old output to the model (#827 review, finding 1). The
            // toast says which failure it is, so the reader knows to re-run.
            crate::bridge::terminal::AskAiClaim::Unverifiable => {
                return Some(CardActionResult {
                    card: None,
                    toast: Some(
                        "⚠️ 找不到这次运行的记录，无法确认它是否仍是最新的，因此没有提交（避免把旧输出交给 AI）。"
                            .to_string(),
                    ),
                });
            }
        };
        let app = Arc::clone(app);
        tokio::spawn(async move {
            app.run_terminal_ask_ai(request, guard).await;
        });
        Some(CardActionResult {
            card: None,
            toast: Some(toast.to_string()),
        })
    })
}

/// Handle a 「↻ 重跑」 click (ADR-0086 decisions 4 and 5). The claim and the
/// stale check happen together, synchronously, inside the callback — cheap, so
/// the ack is never held — and the run itself is spawned off it. The command,
/// cwd and timeout come from the **Terminal Run Record**, which is what makes
/// the button work after a restart and what a multi-kilobyte command is kept
/// out of the card's JSON for.
pub(crate) fn run_rerun<'a>(
    app: &'a Arc<App>,
    value: &'a serde_json::Value,
    _deadline: Instant,
    _callback_started: Instant,
) -> VerbFuture<'a> {
    Box::pin(async move {
        let thread_key = thread_key_from_value(value);
        if thread_key.chat_id.is_empty() {
            return Some(CardActionResult {
                card: None,
                toast: Some("这张卡片缺少会话信息，无法操作。".to_string()),
            });
        }
        let run_id = value.get("run_id").and_then(|v| v.as_str()).unwrap_or("");
        // The clicked card is this run's new reply target: a rerun posts under
        // the card it was clicked on, which is where its reader is looking.
        let Some(message_id) = value.get("open_message_id").and_then(|v| v.as_str()) else {
            return Some(CardActionResult {
                card: None,
                toast: Some("这条操作缺少消息位置，无法重跑。".to_string()),
            });
        };
        let message_id = message_id.to_string();
        let requester = value
            .get("operator_open_id")
            .and_then(|v| v.as_str())
            .map(str::to_string);
        let claim = crate::bridge::terminal::claim_rerun(&app.command_handles(), &thread_key, run_id);
        let toast = match claim {
            crate::bridge::terminal::RerunClaim::Stale => "这张终端卡片的操作已过期。",
            // The slot is held (the command this card names, or a successor,
            // is still running): the record is untouched, so the card's own id
            // stays current for a later click.
            crate::bridge::terminal::RerunClaim::Busy => {
                "这个会话已有终端命令在运行，请先点击运行卡片的「⏹ 终止」。"
            }
            crate::bridge::terminal::RerunClaim::Accepted(claimed) => {
                let app = Arc::clone(app);
                tokio::spawn(async move {
                    if let Err(e) = crate::bridge::terminal::rerun(
                        &app.command_handles(),
                        claimed,
                        crate::bridge::terminal::TerminalRequest {
                            message_id: &message_id,
                            requester_open_id: requester.as_deref(),
                            // Replaced from the record inside `rerun`: a
                            // button payload carries no chat type, and the
                            // record does.
                            is_group: false,
                        },
                    )
                    .await
                    {
                        tracing::error!("Terminal rerun: {}", e);
                    }
                });
                "↻ 正在重跑…"
            }
        };
        Some(CardActionResult {
            card: None,
            toast: Some(toast.to_string()),
        })
    })
}

/// Route a Terminal Card control (ADR-0086 decision 6). The handler never
/// touches the process group: it looks the run up by the button's run id,
/// refuses a stale id, and enqueues the signal for the run loop. A control is
/// refused by state, not hidden, so a redelivered click is a no-op.
///
/// This is also the default arm: an `op` that names no op-keyed verb —
/// `kill`, `continue`, an absent op, or an unknown one — lands here. Only
/// `kill`/`continue` are actionable; the rest are refused as unknown.
pub(crate) fn run_control<'a>(
    app: &'a Arc<App>,
    value: &'a serde_json::Value,
    _deadline: Instant,
    _callback_started: Instant,
) -> VerbFuture<'a> {
    Box::pin(async move {
        let thread_key = thread_key_from_value(value);
        if thread_key.chat_id.is_empty() {
            return Some(CardActionResult {
                card: None,
                toast: Some("这张卡片缺少会话信息，无法操作。".to_string()),
            });
        }
        let run_id = value.get("run_id").and_then(|v| v.as_str()).unwrap_or("");
        let op = value
            .get("op")
            .and_then(|v| v.as_str())
            .and_then(crate::bridge::terminal::TerminalOp::parse);
        let Some(op) = op else {
            return Some(CardActionResult {
                card: None,
                toast: Some("未知的终端操作。".to_string()),
            });
        };
        let outcome = {
            let runs = app
                .terminal_runs
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            runs.control(&thread_key, run_id, op)
        };
        let toast = match outcome {
            crate::bridge::terminal::TerminalControlOutcome::Accepted => match op {
                crate::bridge::terminal::TerminalOp::Kill => "⏹ 正在终止…",
                crate::bridge::terminal::TerminalOp::Continue => "⏱ 已延长时限，继续跟踪输出…",
            },
            crate::bridge::terminal::TerminalControlOutcome::Stale => "这张终端卡片的操作已过期。",
            crate::bridge::terminal::TerminalControlOutcome::NotPastCap => "命令尚未到达时限。",
        };
        Some(CardActionResult {
            card: None,
            toast: Some(toast.to_string()),
        })
    })
}
