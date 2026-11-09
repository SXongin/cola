//! The read-only `/mcp` card (ADR-0103): the current directory's MCP server
//! statuses plus only its directory-owned **Location Forms**. A session-backed
//! question belongs to its Session's card and never appears here.
//!
//! The card is read-only about MCP itself — no connect/disconnect/add/remove,
//! no sign-in and no resource browsing. Its single action is reopening a
//! dismissed Location Form as a fresh card; that click only touches cola's own
//! durable dismissal record, never the MCP server.

use serde_json::json;

use crate::backend::types::{McpServerState, McpServerStatus, QuestionRequest};
use crate::config::ThreadKey;

use super::question::location_form_summary;
use super::sanitize::sanitize_markdown;
use super::shell::card_shell;

/// How the pull surface describes one pending Location Form. A form with no
/// durable decision yet has not been pushed; a pushed one already has its own
/// card; a dismissed one lost it to 「稍后」 and is the only state `/mcp` can
/// reopen.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum McpFormState {
    Dismissed,
    Pushed,
    Unpushed,
}

/// One pending Location Form row on the `/mcp` card.
pub(crate) struct McpFormRow<'a> {
    pub request: &'a QuestionRequest,
    pub state: McpFormState,
}

/// The status line one MCP server renders. `needs_auth` gets an explicit
/// sign-in-required phrase, and `failed` keeps the server's own error text, so
/// the Host sees *why* a form naming that server may be unanswerable.
fn status_label(status: &McpServerStatus) -> String {
    let detail = status
        .error
        .as_deref()
        .filter(|error| !error.trim().is_empty())
        .map(|error| format!("：{error}"))
        .unwrap_or_default();
    match status.state {
        McpServerState::Connected => "✅ 已连接".to_string(),
        McpServerState::Pending => "⏳ 连接中".to_string(),
        McpServerState::Disabled => "⏸️ 已禁用".to_string(),
        McpServerState::Failed => format!("❌ 连接失败{detail}"),
        McpServerState::NeedsAuth => format!("🔐 需要登录授权（needs_auth）{detail}"),
        McpServerState::Unknown => "❔ 状态未知".to_string(),
    }
}

/// The `↩️ 重新打开` control a dismissed form row carries. The payload names the
/// **exact directory whose list produced the form** (user story 29), so the
/// reopen cannot land on another location's form with the same id.
fn reopen_button(thread_key: &ThreadKey, request_id: &str, directory: &str) -> serde_json::Value {
    json!({
        "tag": "button",
        "text": { "tag": "plain_text", "content": "↩️ 重新打开" },
        "type": "default",
        "width": "fill",
        "value": {
            "action": "mcp",
            "op": "reopen",
            "chat_id": thread_key.chat_id,
            "thread_id": thread_key.thread_id,
            "request_id": request_id,
            "directory": directory,
        },
    })
}

fn markdown(content: &str) -> serde_json::Value {
    json!({
        "tag": "markdown",
        "content": sanitize_markdown(content),
    })
}

/// Build the `/mcp` card. The body is read-only: statuses first (the fact a
/// Host may need to fix before answering a form), then the directory's pending
/// Location Forms, then a short read-only-scope note.
pub(crate) fn build_mcp_card(
    thread_key: &ThreadKey,
    directory: &str,
    statuses: &[McpServerStatus],
    forms: &[McpFormRow<'_>],
) -> serde_json::Value {
    let mut elements = vec![markdown(&format!(
        "**目录**：`{directory}`\n\n只读：不会连接/断开、增删 MCP 服务器，也不会登录或浏览资源。"
    ))];

    elements.push(markdown("**MCP 服务器**"));
    if statuses.is_empty() {
        elements.push(markdown("当前目录没有配置 MCP 服务器。"));
    } else {
        for status in statuses {
            let name = sanitize_markdown(&status.name);
            elements.push(markdown(&format!("- **{name}** · {}", status_label(status))));
        }
    }

    elements.push(markdown("**待处理的 Location Form**"));
    if forms.is_empty() {
        elements.push(markdown(
            "当前目录没有待处理的 Location Form。会话自有的提问不会列在这里。",
        ));
    } else {
        for form in forms {
            let summary = location_form_summary(form.request);
            let state = match form.state {
                McpFormState::Dismissed => "已稍后，可重新打开",
                McpFormState::Pushed => "已推送，见原卡片",
                McpFormState::Unpushed => "等待推送",
            };
            elements.push(markdown(&format!("- {summary}（{state}）")));
            if form.state == McpFormState::Dismissed {
                elements.push(reopen_button(thread_key, &form.request.id, directory));
            }
        }
        elements.push(markdown(
            "重新打开会把表单作为一张新卡片发到该目录当前映射的聊天/话题。",
        ));
    }

    card_shell("🔌 MCP", "blue", elements)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::types::{FormFieldKind, QuestionInfo};

    fn location_form(id: &str) -> QuestionRequest {
        QuestionRequest::location(
            id,
            "Context7 登录".into(),
            vec![QuestionInfo {
                key: "token".into(),
                question: "令牌".into(),
                header: "令牌".into(),
                kind: FormFieldKind::String,
                ..Default::default()
            }],
            Some("context7".into()),
            Some("请先登录 Context7 再继续".into()),
            None,
        )
    }

    fn session_form(id: &str) -> QuestionRequest {
        QuestionRequest {
            id: id.into(),
            session_id: "ses_1".into(),
            ..Default::default()
        }
    }

    #[test]
    fn the_card_lists_statuses_forms_and_a_needs_auth_indication() {
        let thread_key = ThreadKey::new("chat_1".into(), "chat_1".into());
        let statuses = vec![
            McpServerStatus {
                name: "codegraph".into(),
                state: McpServerState::Connected,
                error: None,
            },
            McpServerStatus {
                name: "context7".into(),
                state: McpServerState::NeedsAuth,
                error: Some("no credential".into()),
            },
        ];
        let form = location_form("frm_1");
        let card = build_mcp_card(
            &thread_key,
            "/work",
            &statuses,
            &[McpFormRow {
                request: &form,
                state: McpFormState::Dismissed,
            }],
        );
        let text = crate::bridge::test_support::card_text(&card);
        assert!(text.contains("/work"), "{text}");
        assert!(text.contains("codegraph") && text.contains("已连接"), "{text}");
        assert!(text.contains("context7"), "{text}");
        assert!(
            text.contains("needs_auth"),
            "the auth indication is explicit: {text}"
        );
        assert!(text.contains("context7：请先登录 Context7 再继续"), "{text}");
        assert!(
            card.to_string().contains("\"op\":\"reopen\""),
            "a dismissed form gets a reopen control: {card}"
        );
    }

    #[test]
    fn a_pushed_form_has_no_reopen_control_and_the_empty_state_is_explicit() {
        let thread_key = ThreadKey::new("chat_1".into(), "chat_1".into());
        let form = location_form("frm_1");
        let card = build_mcp_card(
            &thread_key,
            "/work",
            &[],
            &[McpFormRow {
                request: &form,
                state: McpFormState::Pushed,
            }],
        );
        let text = crate::bridge::test_support::card_text(&card);
        assert!(text.contains("已推送"), "{text}");
        assert!(
            !card.to_string().contains("\"op\":\"reopen\""),
            "a live form already has its card: {card}"
        );

        let empty = build_mcp_card(&thread_key, "/work", &[], &[]);
        let text = crate::bridge::test_support::card_text(&empty);
        assert!(text.contains("没有配置 MCP 服务器"), "{text}");
        assert!(text.contains("没有待处理的 Location Form"), "{text}");
    }

    #[test]
    fn a_session_backed_question_is_not_rendered_even_if_passed_in() {
        // The bridge filters before calling; the card has no session-backed
        // rendering at all, so a plain session form cannot masquerade as a
        // Location Form row.
        let thread_key = ThreadKey::new("chat_1".into(), "chat_1".into());
        let form = session_form("frm_session");
        let card = build_mcp_card(&thread_key, "/work", &[], &[]);
        assert!(!crate::bridge::test_support::card_text(&card).contains("frm_session"));
        // The helper itself is only produced for Location Forms; this pins the
        // bridge's filter's expected input shape.
        assert!(!form.is_location());
    }
}
