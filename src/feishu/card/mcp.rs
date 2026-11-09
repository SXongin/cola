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

use super::question::location_form_summary_raw;
use super::sanitize::{CardMarkdown, serialized_len};
use super::shell::card_shell;
use super::{MAX_CARD_COMPONENTS, MAX_CARD_JSON_CHARS, MAX_ELEMENT_TEXT_CHARS, truncate_md};

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

/// One row's raw text clip. A row is one short line; keeping it well under
/// [`MAX_ELEMENT_TEXT_CHARS`] leaves room for the sanitizer's `<` expansion
/// (five characters per `<`) and the surrounding markdown.
const MCP_ROW_CHARS: usize = 400;

/// Conservative per-element JSON overhead (tag, braces, quoting, nesting).
const MCP_ELEMENT_OVERHEAD: usize = 160;

/// A reopen button's extra JSON structure beyond its text.
const MCP_BUTTON_OVERHEAD: usize = 300;

/// Elements reserved for the fixed intro/headers/footer and the omission note.
const MCP_ELEMENT_RESERVE: usize = 8;

/// Bytes reserved for the card shell, the fixed sections and the omission
/// note, so a listing never consumes the whole [`MAX_CARD_JSON_CHARS`].
const MCP_BYTE_RESERVE: usize = 4_000;

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
    let state = match status.state {
        McpServerState::Connected => "✅ 已连接",
        McpServerState::Pending => "⏳ 连接中",
        McpServerState::Disabled => "⏸️ 已禁用",
        McpServerState::Failed => "❌ 连接失败",
        McpServerState::NeedsAuth => "🔐 需要登录授权（needs_auth）",
        McpServerState::Unknown => "❔ 状态未知",
    };
    format!("{state}{detail}")
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

/// One card body under BOTH budgets: one shared [`CardMarkdown`] for the
/// whole-card table allowance, and running element/estimated-JSON caps so a
/// directory with many servers or forms cannot build a card Feishu rejects.
struct BoundedBody {
    elements: Vec<serde_json::Value>,
    bytes: usize,
    markdown: CardMarkdown,
}

impl BoundedBody {
    fn new() -> Self {
        Self {
            elements: Vec::new(),
            bytes: 0,
            markdown: CardMarkdown::new(),
        }
    }

    /// Push one markdown row and, when given, its control as one unit: a
    /// dismissed form never loses its reopen button to the byte budget. A row
    /// that does not fit is omitted (the caller reports the count).
    fn push(&mut self, text: &str) -> bool {
        self.push_with_button(text, None)
    }

    fn push_with_button(&mut self, text: &str, button: Option<serde_json::Value>) -> bool {
        self.push_inner(text, button, true)
    }

    /// A fixed tail (the omission note) that may use the element reserve the
    /// listing left free. It still keeps the byte margin.
    fn push_tail(&mut self, text: &str) -> bool {
        self.push_inner(text, None, false)
    }

    fn push_inner(&mut self, text: &str, button: Option<serde_json::Value>, reserve: bool) -> bool {
        let clipped = truncate_md(text, MCP_ROW_CHARS);
        let mut cleaned = self.markdown.clean(&clipped);
        if cleaned.chars().count() > MAX_ELEMENT_TEXT_CHARS {
            // Defensive: the raw clip already keeps the sanitizer's `<`
            // expansion under this cap, but a future row builder must not
            // inherit an unbounded element.
            cleaned = truncate_md(&cleaned, MAX_ELEMENT_TEXT_CHARS);
        }
        let element_cap = if reserve {
            MAX_CARD_COMPONENTS - MCP_ELEMENT_RESERVE
        } else {
            MAX_CARD_COMPONENTS
        };
        let byte_cap = if reserve {
            MAX_CARD_JSON_CHARS - MCP_BYTE_RESERVE
        } else {
            MAX_CARD_JSON_CHARS - 1_000
        };
        let mut cost = serialized_len(&cleaned) + MCP_ELEMENT_OVERHEAD;
        let mut count = 1;
        if let Some(button) = &button {
            cost += button.to_string().len() + MCP_BUTTON_OVERHEAD;
            count += 1;
        }
        if self.elements.len() + count > element_cap || self.bytes + cost > byte_cap {
            return false;
        }
        self.elements.push(json!({
            "tag": "markdown",
            "content": cleaned,
        }));
        self.bytes += serialized_len(&cleaned) + MCP_ELEMENT_OVERHEAD;
        if let Some(button) = button {
            self.elements.push(button);
            self.bytes += cost - (serialized_len(&cleaned) + MCP_ELEMENT_OVERHEAD);
        }
        true
    }
}

/// Build the `/mcp` card. The body is read-only. Location Forms come BEFORE
/// the server statuses so the recovery controls survive a cap: a dismissed
/// form's reopen button is the one thing this surface exists to offer, while a
/// status row can be dropped when the directory is extreme. Within the form
/// section, dismissed rows sort first for the same reason. A session-backed
/// form is never rendered, even if a caller passes one in — only
/// `is_location()` rows reach the body.
pub(crate) fn build_mcp_card(
    thread_key: &ThreadKey,
    directory: &str,
    statuses: &[McpServerStatus],
    forms: &[McpFormRow<'_>],
) -> serde_json::Value {
    let mut body = BoundedBody::new();
    body.push(&format!(
        "**目录**：`{directory}`

只读：不会连接/断开、增删 MCP 服务器，也不会登录或浏览资源。"
    ));

    let mut omitted = 0;
    body.push("**待处理的 Location Form**");
    let mut location_forms: Vec<&McpFormRow<'_>> =
        forms.iter().filter(|form| form.request.is_location()).collect();
    // Stable sort: a dismissed form's control outranks every status and every
    // non-dismissed row when the card budget runs out.
    location_forms.sort_by_key(|form| match form.state {
        McpFormState::Dismissed => 0,
        McpFormState::Unpushed => 1,
        McpFormState::Pushed => 2,
    });
    if location_forms.is_empty() {
        body.push("当前目录没有待处理的 Location Form。会话自有的提问不会列在这里。");
    } else {
        body.push("重新打开会把表单作为一张新卡片发到该目录当前映射的聊天/话题。");
        for form in location_forms {
            let summary = location_form_summary_raw(form.request);
            let state = match form.state {
                McpFormState::Dismissed => "已稍后，可重新打开",
                McpFormState::Pushed => "已推送，见原卡片",
                McpFormState::Unpushed => "等待推送",
            };
            let button = (form.state == McpFormState::Dismissed)
                .then(|| reopen_button(thread_key, &form.request.id, directory));
            if !body.push_with_button(&format!("- {summary}（{state}）"), button) {
                omitted += 1;
            }
        }
    }

    body.push("**MCP 服务器**");
    if statuses.is_empty() {
        body.push("当前目录没有配置 MCP 服务器。");
    } else {
        for status in statuses {
            let row = format!("- **{}** · {}", status.name, status_label(status));
            if !body.push(&row) {
                omitted += 1;
            }
        }
    }
    if omitted > 0 {
        // The element/byte reserve above leaves room for this note; if a
        // future change exhausts it too, the listing is already truncated as
        // far as it can be and dropping the note is the only remaining option.
        let _ = body.push_tail(&format!(
            "…列表过长，还有 {omitted} 项未显示；可直接处理上方已列出的项。"
        ));
    }

    card_shell("🔌 MCP", "blue", body.elements)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::types::{FormFieldKind, QuestionInfo};
    use crate::bridge::test_support::{card_buttons, card_text};
    use crate::feishu::card::session::card_components;

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
            title: "Questions".into(),
            ..Default::default()
        }
    }

    fn status(name: &str, state: McpServerState, error: Option<&str>) -> McpServerStatus {
        McpServerStatus {
            name: name.into(),
            state,
            error: error.map(str::to_string),
        }
    }

    fn reopen(card: &serde_json::Value) -> Option<&serde_json::Value> {
        card_buttons(card).into_iter().find(|button| {
            let value = &button["value"];
            value.get("action").and_then(|v| v.as_str()) == Some("mcp")
                && value.get("op").and_then(|v| v.as_str()) == Some("reopen")
        })
    }

    #[test]
    fn the_card_lists_statuses_forms_and_a_needs_auth_indication() {
        let thread_key = ThreadKey::new("chat_1".into(), "chat_1".into());
        let statuses = vec![
            status("codegraph", McpServerState::Connected, None),
            status("context7", McpServerState::NeedsAuth, Some("no credential")),
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
        let text = card_text(&card);
        assert!(text.contains("/work"), "{text}");
        assert!(text.contains("codegraph") && text.contains("已连接"), "{text}");
        assert!(text.contains("context7"), "{text}");
        assert!(
            text.contains("needs_auth"),
            "the auth indication is explicit: {text}"
        );
        assert!(text.contains("context7：请先登录 Context7 再继续"), "{text}");
        let reopen = reopen(&card).expect("a dismissed form gets a reopen control");
        assert_eq!(
            reopen["value"]["directory"].as_str(),
            Some("/work"),
            "the control carries the exact directory"
        );
    }

    #[test]
    fn every_wire_state_renders_its_own_label() {
        let thread_key = ThreadKey::new("chat_1".into(), "chat_1".into());
        let statuses = vec![
            status("c", McpServerState::Connected, None),
            status("p", McpServerState::Pending, None),
            status("d", McpServerState::Disabled, None),
            status("f", McpServerState::Failed, Some("spawn failed")),
            status("a", McpServerState::NeedsAuth, Some("sign in")),
            status("u", McpServerState::Unknown, None),
        ];
        let text = card_text(&build_mcp_card(&thread_key, "/work", &statuses, &[]));
        for expected in [
            "✅ 已连接",
            "⏳ 连接中",
            "⏸️ 已禁用",
            "❌ 连接失败：spawn failed",
            "🔐 需要登录授权（needs_auth）：sign in",
            "❔ 状态未知",
        ] {
            assert!(text.contains(expected), "missing {expected}: {text}");
        }
    }

    #[test]
    fn a_pushed_or_unpushed_form_has_no_reopen_control() {
        let thread_key = ThreadKey::new("chat_1".into(), "chat_1".into());
        let pushed = location_form("frm_pushed");
        let unpushed = location_form("frm_unpushed");
        let card = build_mcp_card(
            &thread_key,
            "/work",
            &[],
            &[
                McpFormRow {
                    request: &pushed,
                    state: McpFormState::Pushed,
                },
                McpFormRow {
                    request: &unpushed,
                    state: McpFormState::Unpushed,
                },
            ],
        );
        let text = card_text(&card);
        assert!(text.contains("已推送") && text.contains("等待推送"), "{text}");
        assert!(reopen(&card).is_none(), "only a dismissed form reopens: {card}");

        let empty = build_mcp_card(&thread_key, "/work", &[], &[]);
        let text = card_text(&empty);
        assert!(text.contains("没有配置 MCP 服务器"), "{text}");
        assert!(text.contains("没有待处理的 Location Form"), "{text}");
    }

    #[test]
    fn a_session_backed_question_passed_in_is_never_rendered() {
        let thread_key = ThreadKey::new("chat_1".into(), "chat_1".into());
        let session = session_form("frm_session");
        let card = build_mcp_card(
            &thread_key,
            "/work",
            &[],
            &[McpFormRow {
                request: &session,
                state: McpFormState::Unpushed,
            }],
        );
        let text = card_text(&card);
        assert!(!text.contains("Questions"), "{text}");
        assert!(!text.contains("frm_session"), "{text}");
        assert!(text.contains("没有待处理的 Location Form"), "{text}");
    }

    /// One shared table budget: six servers whose error text each carries a
    /// markdown table must not make the card's sixth table native. One shared
    /// element/JSON budget: a directory with hundreds of servers must still
    /// build a card Feishu accepts, with the overflow reported.
    #[test]
    fn the_card_threads_one_table_budget_and_caps_its_listing() {
        let thread_key = ThreadKey::new("chat_1".into(), "chat_1".into());
        let table = "| a | b |\n|---|---|\n| 1 | 2 |";
        let statuses: Vec<_> = (0..200)
            .map(|index| status(&format!("server-{index}"), McpServerState::Failed, Some(table)))
            .collect();
        let forms_owned: Vec<_> = (0..20)
            .map(|index| {
                let mut form = location_form(&format!("frm_{index}"));
                form.message = Some(table.into());
                form
            })
            .collect();
        let forms: Vec<_> = forms_owned
            .iter()
            .map(|request| McpFormRow {
                request,
                state: McpFormState::Dismissed,
            })
            .collect();
        let card = build_mcp_card(&thread_key, "/work", &statuses, &forms);
        let text = card_text(&card);
        assert!(
            card.to_string().len() <= MAX_CARD_JSON_CHARS,
            "the card must stay under the JSON budget: {}",
            card.to_string().len()
        );
        assert!(
            card_components(&card) <= MAX_CARD_COMPONENTS,
            "the card must stay under the component cap: {}",
            card_components(&card)
        );
        assert!(
            text.matches("```").count() >= 2,
            "tables past the one shared budget are fenced, not native: {text}"
        );
        assert!(text.contains("列表过长"), "the omitted rows are reported: {text}");
        assert!(
            reopen(&card).is_some(),
            "a dismissed form's recovery control survives the cap: {text}"
        );
        assert!(
            text.find("已稍后").unwrap_or(usize::MAX) < text.find("server-0").unwrap_or(usize::MAX),
            "the form section precedes the droppable status rows: {text}"
        );
        assert!(
            text.chars().count() < 100_000,
            "the listing is truncated, not fully rendered"
        );
    }
}
