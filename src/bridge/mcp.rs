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

use serde_json::Value;

use crate::backend::types::QuestionRequest;
use crate::bridge::handler::CardActionResult;
use crate::bridge::handles::CommandHandles;
use crate::bridge::request::kind::PendingRequest;
use crate::config::ThreadKey;
use crate::feishu::card::mcp::{McpFormRow, McpFormState, build_mcp_card};

/// The command's answer on an Agent Client whose Backend reports no MCP pull
/// surface. The client name keeps one shared sentence honest for Codex and
/// OpenCode V1 alike.
pub(crate) fn unavailable_text(client_name: &str) -> String {
    format!("当前 Agent Client（{client_name}）不提供 MCP 状态和 Location Form 的读取接口，`/mcp` 不可用。")
}

/// Run `/mcp` for one Chat/Topic: the current project directory's statuses and
/// pending Location Forms as one read-only card, or the unavailable text when
/// the adapter has no such surface.
pub(crate) async fn run(
    handles: &CommandHandles,
    thread_key: &ThreadKey,
    message_id: &str,
) -> crate::error::Result<()> {
    let directory = handles.current_project_directory(thread_key).await;
    let Some(statuses) = handles.flow.backend.mcp_server_statuses(Some(&directory)).await? else {
        handles
            .flow
            .platform
            .reply_text(message_id, &unavailable_text(handles.flow.backend.client_name()))
            .await?;
        return Ok(());
    };
    let forms = handles.flow.backend.list_questions(Some(&directory)).await?;
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
/// waiting for the sweep to push it.
fn form_state(handles: &CommandHandles, form: &QuestionRequest) -> McpFormState {
    if handles.question.location_form_dismissed(&form.id) {
        McpFormState::Dismissed
    } else if handles.question.location_form_known(&form.id) {
        McpFormState::Pushed
    } else {
        McpFormState::Unpushed
    }
}

/// Handle the `/mcp` card's `op: "reopen"` click. The fresh card is sent by
/// [`crate::bridge::request::flow::RequestFlow::reopen_location_form`] BEFORE
/// the dismissal is cleared, so a failed send leaves the form dismissed and
/// retryable. The payload carries the exact directory whose list produced the
/// form, never a directory re-derived from the clicked chat.
pub(crate) async fn handle_action(handles: &CommandHandles, value: &Value) -> Option<CardActionResult> {
    let request_id = value.get("request_id").and_then(Value::as_str).unwrap_or("");
    let directory = value.get("directory").and_then(Value::as_str).unwrap_or("");
    if request_id.is_empty() || directory.is_empty() {
        return Some(toast("这条重新打开请求缺少表单或目录信息。"));
    }
    // Re-read the directory's current pending list: the form may have resolved
    // since the `/mcp` card was rendered, and the fresh card must be built
    // from the live request.
    let forms = match handles.flow.backend.list_questions(Some(directory)).await {
        Ok(forms) => forms,
        Err(error) => {
            tracing::warn!("mcp reopen: listing {directory} failed: {error}");
            return Some(toast("读取待处理表单失败，请稍后重试。"));
        }
    };
    let Some(form) = forms
        .into_iter()
        .find(|form| form.id == request_id && form.is_location())
    else {
        return Some(toast("该 Location Form 已不在待处理列表中。"));
    };
    let reopened = handles
        .question
        .reopen_location_form(&handles.flow, &PendingRequest::Question(form), directory)
        .await;
    if reopened {
        Some(toast("已重新打开，请在新的卡片上作答。"))
    } else {
        Some(toast("重新打开失败，表单仍保持稍后，可再试一次。"))
    }
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
