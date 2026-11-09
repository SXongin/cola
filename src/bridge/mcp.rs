//! The read-only `/mcp` command (ADR-0103's **pull surface**): the current
//! directory's MCP server statuses plus **only** its pending Location Forms,
//! and the one control that reopens a dismissed form as a fresh card.
//!
//! It never connects, disconnects, adds, removes or signs in to an MCP server,
//! and it never browses resources. A session-backed question belongs to its
//! Session's card, so only `QuestionRequest::is_location()` rows are listed.
//!
//! The capability is the adapter's: a client without the full surface
//! (statuses **and** Location Forms) answers `None`, and the command says
//! "unavailable" instead of rendering a status-only card.

use std::sync::atomic::Ordering;

use serde_json::Value;

use crate::backend::types::QuestionRequest;
use crate::bridge::handler::CardActionResult;
use crate::bridge::handles::CommandHandles;
use crate::bridge::request::flow::ReopenOutcome;
use crate::bridge::request::kind::PendingRequest;
use crate::bridge::surfaces::LocationFormState;
use crate::config::ThreadKey;
use crate::feishu::card::mcp::{McpFormRow, McpFormState, build_mcp_card};

/// The command's answer on an Agent Client whose Backend reports no full MCP
/// pull surface. V1 publishes a status read but no elicitation-form surface,
/// so the sentence names the missing *pair*, not a missing status read.
pub(crate) fn unavailable_text(client_name: &str) -> String {
    format!(
        "当前 Agent Client（{client_name}）不提供完整的 MCP 状态 + Location Form 读取接口，`/mcp` 不可用。"
    )
}

/// One plain error reply, never an empty card: a failed or timed-out read must
/// be visible to the Host (the Backend rule reserves `None` for "not
/// applicable", never for a failure).
async fn reply_error(
    handles: &CommandHandles,
    message_id: &str,
    what: &str,
    error: crate::error::BridgeError,
) -> crate::error::Result<()> {
    handles
        .flow
        .platform
        .reply_text(message_id, &format!("读取{what}失败：{error}"))
        .await?;
    Ok(())
}

/// Run `/mcp` for one Chat/Topic: the current project directory's statuses and
/// pending Location Forms as one read-only card, or the unavailable text when
/// the adapter has no such surface. Both reads are bounded like every other
/// command-layer network call.
pub(crate) async fn run(
    handles: &CommandHandles,
    thread_key: &ThreadKey,
    message_id: &str,
) -> crate::error::Result<()> {
    let directory = handles.current_project_directory(thread_key).await;
    let timeout_ms = handles.question.list_timeout_ms.load(Ordering::Relaxed);
    let Some(statuses) = crate::bridge::bounded_call(
        "mcp status list",
        timeout_ms,
        handles.flow.backend.mcp_server_statuses(Some(&directory)),
    )
    .await
    else {
        handles
            .flow
            .platform
            .reply_text(message_id, "读取 MCP 状态超时，请稍后重试。")
            .await?;
        return Ok(());
    };
    let statuses = match statuses {
        Ok(Some(statuses)) => statuses,
        Ok(None) => {
            handles
                .flow
                .platform
                .reply_text(message_id, &unavailable_text(handles.flow.backend.client_name()))
                .await?;
            return Ok(());
        }
        Err(error) => return reply_error(handles, message_id, " MCP 状态", error).await,
    };
    let Some(forms) = crate::bridge::bounded_call(
        "mcp form list",
        timeout_ms,
        handles.flow.backend.list_questions(Some(&directory)),
    )
    .await
    else {
        handles
            .flow
            .platform
            .reply_text(message_id, "读取待处理 Location Form 超时，请稍后重试。")
            .await?;
        return Ok(());
    };
    let forms = match forms {
        Ok(forms) => forms,
        Err(error) => return reply_error(handles, message_id, "待处理 Location Form ", error).await,
    };
    // Only directory-owned Location Forms: a session-backed question keeps its
    // own Question card and must never be duplicated on the pull surface.
    let rows: Vec<McpFormRow<'_>> = forms
        .iter()
        .filter(|form| form.is_location())
        .map(|form| McpFormRow {
            request: form,
            state: form_state(handles, form),
        })
        .collect();
    let card = build_mcp_card(thread_key, &directory, &statuses, &rows);
    handles.flow.platform.reply_card(message_id, &card).await?;
    Ok(())
}

/// The durable state a listed form renders: a dismissal is reopenable, a
/// pushed form already has its own card, and a form with no decision yet is
/// waiting for the sweep to push it. One record read drives all three.
fn form_state(handles: &CommandHandles, form: &QuestionRequest) -> McpFormState {
    match handles.question.location_form_state(&form.id) {
        Some(LocationFormState::Dismissed) => McpFormState::Dismissed,
        Some(LocationFormState::Pushed) => McpFormState::Pushed,
        None => McpFormState::Unpushed,
    }
}

/// The truthful toast for each reopen outcome. Only a failed send means the
/// form is still deferred and retryable; a stale card whose form was already
/// reopened says so instead of claiming it stayed dismissed.
fn outcome_toast(outcome: ReopenOutcome) -> &'static str {
    match outcome {
        ReopenOutcome::Reopened => "已重新打开，请在新的卡片上作答。",
        ReopenOutcome::NotDismissed => "该表单已有一张卡片或已重新打开，无需再打开。",
        ReopenOutcome::AlreadyReopening => "正在重新打开，请稍候。",
        ReopenOutcome::NoTarget => "无法重新打开：该目录当前没有已映射的聊天/话题。",
        ReopenOutcome::SendFailed => "重新打开失败，表单仍保持稍后，可再试一次。",
        ReopenOutcome::LostToDismissal => "重新打开时表单又被稍后处理，新卡片已撤回；可再试一次。",
    }
}

/// Handle the `/mcp` card's `op: "reopen"` click. The fresh card is sent by
/// [`crate::bridge::request::flow::RequestFlow::reopen_location_form`] BEFORE
/// the dismissal is cleared, so a failed send leaves the form dismissed and
/// retryable. The payload carries the exact directory whose list produced the
/// form, never a directory re-derived from the clicked chat. The list read
/// shares the callback's own deadline; the reopen's send keeps its internal
/// bound so it cannot be cancelled half-committed.
pub(crate) async fn handle_action(
    handles: &CommandHandles,
    value: &Value,
    deadline: tokio::time::Instant,
) -> Option<CardActionResult> {
    if value.get("op").and_then(Value::as_str) != Some("reopen") {
        return None;
    }
    let request_id = value.get("request_id").and_then(Value::as_str).unwrap_or("");
    let directory = value.get("directory").and_then(Value::as_str).unwrap_or("");
    if request_id.is_empty() || directory.is_empty() {
        return Some(toast("这条重新打开请求缺少表单或目录信息。"));
    }
    // One bounded listing read for both the live request and the repaint: the
    // form list is re-read (the form may have resolved since the card was
    // rendered) and the statuses ride along so a successful reopen can rebuild
    // the clicked card without a second post-action read. Both share the
    // click's deadline.
    let (forms, statuses) = tokio::join!(
        tokio::time::timeout_at(deadline, handles.flow.backend.list_questions(Some(directory))),
        tokio::time::timeout_at(
            deadline,
            handles.flow.backend.mcp_server_statuses(Some(directory)),
        ),
    );
    let forms = match forms {
        Ok(Ok(forms)) => forms,
        Ok(Err(error)) => {
            tracing::warn!("mcp reopen: listing {directory} failed: {error}");
            return Some(toast("读取待处理表单失败，请稍后重试。"));
        }
        Err(_) => {
            tracing::warn!("mcp reopen: listing {directory} timed out");
            return Some(toast("读取待处理表单超时，请稍后重试。"));
        }
    };
    let statuses = match statuses {
        Ok(Ok(Some(statuses))) => Some(statuses),
        Ok(Ok(None)) => None,
        Ok(Err(error)) => {
            tracing::warn!("mcp reopen: status listing {directory} failed: {error}");
            None
        }
        Err(_) => {
            tracing::warn!("mcp reopen: status listing {directory} timed out");
            None
        }
    };
    let Some(form) = forms
        .iter()
        .find(|form| form.id == request_id && form.is_location())
        .cloned()
    else {
        return Some(toast("该 Location Form 已不在待处理列表中。"));
    };
    let outcome = handles
        .question
        .reopen_location_form(&handles.flow, &PendingRequest::Question(form), directory)
        .await;
    let thread_key = ThreadKey::new(
        value
            .get("chat_id")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string(),
        value
            .get("thread_id")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string(),
    );
    // Reopened and NotDismissed both changed the row the clicked card shows;
    // rebuild it from the listing already read. A failed status read leaves
    // the old card in place rather than spending another read after the ack.
    let card = matches!(outcome, ReopenOutcome::Reopened | ReopenOutcome::NotDismissed)
        .then(|| {
            statuses.map(|statuses| {
                let rows: Vec<McpFormRow<'_>> = forms
                    .iter()
                    .filter(|form| form.is_location())
                    .map(|form| McpFormRow {
                        request: form,
                        state: form_state(handles, form),
                    })
                    .collect();
                build_mcp_card(&thread_key, directory, &statuses, &rows)
            })
        })
        .flatten()
        .filter(|_| !thread_key.chat_id.is_empty());
    Some(CardActionResult {
        card,
        toast: Some(outcome_toast(outcome).to_string()),
    })
}

fn toast(text: &str) -> CardActionResult {
    CardActionResult {
        card: None,
        toast: Some(text.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unavailable_names_the_client_and_says_unavailable() {
        let text = unavailable_text("Codex");
        assert!(text.contains("Codex"), "{text}");
        assert!(text.contains("不可用"), "{text}");
        assert!(text.contains("Location Form"), "{text}");
    }
}
