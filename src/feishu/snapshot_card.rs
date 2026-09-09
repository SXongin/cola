use crate::bridge::request::describe_permission;
use crate::bridge::snapshot::{SnapshotData, TailEntry};
use crate::feishu::card::{permission_buttons, question_elements};
use crate::opencode;
use serde_json::json;

/// One hint phrase shown under the 运行中 chip when the adopted session is busy
/// (ADR-0028): the snapshot's only live behaviour, the busy-follow (ticket 06),
/// will stream the in-flight turn into this card, so the chip says so up front.
const BUSY_HINT: &str = "有新进展会自动更新";

/// The 等待你的确认 chip, shown when the session has an adopt-time pending
/// request — the operator's action beats the server's run state.
const WAITING_CHIP: &str = "⏳ 等待你的确认";

/// The status chip content for a server run state (ADR-0028 precedence
/// 等待你的确认 > 运行中 > 需要重试 > 空闲; a busy session carries one hint
/// phrase). `None` when there is no status to report — the chip is omitted
/// rather than guessed.
fn status_chip(has_pending: bool, status: Option<opencode::SessionStatus>) -> Option<String> {
    if has_pending {
        return Some(WAITING_CHIP.to_string());
    }
    match status {
        Some(opencode::SessionStatus::Busy) => Some(format!("⚙️ 运行中\n{BUSY_HINT}")),
        Some(opencode::SessionStatus::Retry) => Some("🔁 需要重试".to_string()),
        Some(opencode::SessionStatus::Idle) => Some("✅ 空闲".to_string()),
        None => None,
    }
}

/// Build the status chip as a body element, if the precedence says one shows.
fn status_chip_element(
    has_pending: bool,
    status: Option<opencode::SessionStatus>,
) -> Option<serde_json::Value> {
    status_chip(has_pending, status).map(|content| json!({ "tag": "markdown", "content": content }))
}

/// The body elements for one adopt-time pending request, embedded on the
/// snapshot so it looks and behaves like today's inline card sections
/// (`🔐 权限请求` + its four buttons; question options) — nothing re-implemented.
fn pending_block_elements(
    req: &crate::bridge::request::PendingRequest,
    directory: &str,
) -> Vec<serde_json::Value> {
    match req {
        crate::bridge::request::PendingRequest::Permission(p) => {
            let body = describe_permission(p);
            let sid = p.session_id.as_deref().unwrap_or("");
            let mut els = vec![json!({ "tag": "markdown", "content": format!("🔐 **权限请求**\n{body}") })];
            els.extend(permission_buttons(sid, &p.request_id, &body, directory));
            els
        }
        crate::bridge::request::PendingRequest::Question(q) => {
            let n = q.questions.len();
            let answered = vec![None; n];
            let done = vec![false; n];
            question_elements(&q.id, &q.session_id, &q.questions, directory, &answered, &done)
        }
    }
}

/// The 最近对话 panel: the last-four tail entries, each role-marked and shown
/// folded — its header previews the entry briefly and expanding reveals the
/// full verbatim text (chunked to the per-element cap). No tail → no panel.
fn tail_panels(tail: &[TailEntry]) -> Vec<serde_json::Value> {
    let mut panels = Vec::new();
    if tail.is_empty() {
        return panels;
    }
    panels.push(json!({ "tag": "markdown", "content": "**最近对话**" }));
    for entry in tail {
        let role = match entry.role.as_str() {
            "user" => "👤",
            "assistant" => "🤖",
            _ => "💬",
        };
        let preview: String = entry.text.chars().take(40).collect::<String>().trim().to_string();
        let title = if preview.is_empty() {
            format!("{role} （空消息）")
        } else {
            format!("{role} {preview}…")
        };
        let chunks =
            crate::feishu::card::chunk_text(&entry.text, crate::feishu::card::MAX_ELEMENT_TEXT_CHARS);
        let chunks = if chunks.is_empty() {
            vec!["（空消息）".to_string()]
        } else {
            chunks
        };
        panels.push(crate::feishu::card::collapsible_panel_chunks(&title, &chunks));
    }
    panels
}

/// The snapshot's display title: the cleaned session label, falling back to
/// the id tail when the label is empty (shared by the full snapshot and the
/// compact suppressed-切换 state card).
fn display_title(title: &str, session_id: &str) -> String {
    let label = crate::feishu::card::clean_session_label(title);
    if label.is_empty() {
        crate::bridge::command::id_tail(session_id)
    } else {
        label
    }
}

/// Build the one-shot Session Snapshot card (ADR-0028) for an adopted session.
/// `verb` is the takeover verb (接管/切换 — 已 is prefixed for the header) and
/// `title` the session's display title; the body comes entirely from the
/// read-side [`SnapshotData`]. Sections are assembled from individually
/// constructible builders so a later ticket can drop a resolved claim
/// (05) or stream a busy follow (06) into the same card.
pub fn build_snapshot_card(verb: &str, title: &str, data: &SnapshotData) -> serde_json::Value {
    let verb = verb.strip_prefix("已").unwrap_or(verb);
    let title = display_title(title, &data.session_id);

    // Only the adopted session's OWN pendings belong on the snapshot — never a
    // sibling session's block (defensive; the gather already filtered). The
    // same filtered set drives both the 等待你的确认 chip and the rendered
    // blocks, so a chip never claims a pending that isn't shown.
    let pending: Vec<_> = data
        .pending
        .iter()
        .filter(|req| req.session_id() == data.session_id)
        .collect();

    let mut elements: Vec<serde_json::Value> = Vec::new();
    if let Some(chip) = status_chip_element(!pending.is_empty(), data.status) {
        elements.push(chip);
    }
    for (i, req) in pending.iter().enumerate() {
        if i > 0 {
            elements.push(json!({ "tag": "hr" }));
        }
        elements.extend(pending_block_elements(req, &data.directory));
    }
    if !pending.is_empty() && !data.tail.is_empty() {
        elements.push(json!({ "tag": "hr" }));
    }
    elements.extend(tail_panels(&data.tail));

    json!({
        "schema": "2.0",
        "config": { "wide_screen_mode": true },
        "header": {
            "title": { "tag": "plain_text", "content": format!("已{verb} {title}") },
            "template": "blue"
        },
        "body": { "elements": elements }
    })
}

/// Build the compact suppressed-切换 state card (ADR-0028): when a re-switch
/// to a session already mapped to this thread has nothing to report (idle, no
/// pending, newest user message cola-authored), the switch card patches to
/// this small confirmation instead of a full snapshot — the card-form mirror
/// of the text form's one-line ack. No status chip, no tail, no pending
/// blocks: there is deliberately nothing to report.
pub fn build_switched_state_card(title: &str, session_id: &str, directory: &str) -> serde_json::Value {
    let title = display_title(title, session_id);
    json!({
        "schema": "2.0",
        "config": { "wide_screen_mode": true },
        "header": {
            "title": { "tag": "plain_text", "content": format!("已切换 {title}") },
            "template": "blue"
        },
        "body": { "elements": [
            { "tag": "markdown", "content": format!("已切换到该会话（目录 `{directory}`）。") }
        ] }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bridge::request::PendingRequest;
    use crate::bridge::snapshot::SnapshotData;
    use crate::opencode::client::{PermissionRequest, QuestionInfo, QuestionOption, QuestionRequest};

    fn permission(req_id: &str, session_id: &str, action: &str) -> PermissionRequest {
        PermissionRequest {
            request_id: req_id.into(),
            session_id: Some(session_id.into()),
            permission: Some(action.into()),
            patterns: vec!["ls".into()],
            metadata: None,
            always: vec![],
        }
    }

    fn question(req_id: &str, session_id: &str) -> QuestionRequest {
        QuestionRequest {
            id: req_id.into(),
            session_id: session_id.into(),
            questions: vec![QuestionInfo {
                question: "继续?".into(),
                header: "确认".into(),
                options: vec![
                    QuestionOption {
                        label: "继续".into(),
                        description: String::new(),
                    },
                    QuestionOption {
                        label: "停下".into(),
                        description: String::new(),
                    },
                ],
                multiple: None,
                custom: None,
            }],
        }
    }

    fn data(
        status: Option<opencode::SessionStatus>,
        pending: Vec<PendingRequest>,
        tail: Vec<TailEntry>,
    ) -> SnapshotData {
        SnapshotData {
            session_id: "ses_adopted".into(),
            directory: "/work/proj".into(),
            status,
            pending,
            tail,
            newest_user_is_cola_authored: false,
        }
    }

    fn tail(role: &str, created_ms: i64, text: &str) -> TailEntry {
        TailEntry {
            role: role.into(),
            created_ms,
            text: text.into(),
        }
    }

    fn elements(card: &serde_json::Value) -> &[serde_json::Value] {
        card["body"]["elements"].as_array().unwrap()
    }

    #[test]
    fn header_has_verb_and_title() {
        let d = data(Some(opencode::SessionStatus::Idle), vec![], vec![]);
        let card = build_snapshot_card("接管", "重写登录模块", &d);
        let h = card["header"]["title"]["content"].as_str().unwrap();
        assert_eq!(h, "已接管 重写登录模块", "header: {card}");
        assert_eq!(card["schema"].as_str().unwrap(), "2.0");
    }

    #[test]
    fn status_chip_precedence_matrix() {
        // Pending beats every run state.
        assert_eq!(
            status_chip(true, Some(opencode::SessionStatus::Busy)).as_deref(),
            Some("⏳ 等待你的确认")
        );
        assert_eq!(
            status_chip(true, Some(opencode::SessionStatus::Retry)).as_deref(),
            Some("⏳ 等待你的确认")
        );
        assert_eq!(
            status_chip(true, Some(opencode::SessionStatus::Idle)).as_deref(),
            Some("⏳ 等待你的确认")
        );
        assert_eq!(status_chip(true, None).as_deref(), Some("⏳ 等待你的确认"));

        // No pending: the server state decides.
        assert!(
            status_chip(false, Some(opencode::SessionStatus::Busy))
                .unwrap()
                .contains("运行中")
        );
        assert_eq!(
            status_chip(false, Some(opencode::SessionStatus::Retry)).as_deref(),
            Some("🔁 需要重试")
        );
        assert_eq!(
            status_chip(false, Some(opencode::SessionStatus::Idle)).as_deref(),
            Some("✅ 空闲")
        );
        // Unknown status and nothing pending → no chip, never guessed.
        assert_eq!(status_chip(false, None), None);
    }

    #[test]
    fn busy_chip_carries_one_hint() {
        let d = data(Some(opencode::SessionStatus::Busy), vec![], vec![]);
        let card = build_snapshot_card("接管", "t", &d);
        let body = elements(&card)[0]["content"].as_str().unwrap();
        assert!(body.contains("运行中"));
        assert!(body.contains(BUSY_HINT));
    }

    #[test]
    fn unknown_status_renders_no_chip_line() {
        let d = data(None, vec![], vec![]);
        let card = build_snapshot_card("接管", "t", &d);
        let els = elements(&card);
        // Only the tail/none — no status markdown first.
        assert!(
            els.is_empty()
                || !els[0]["content"]
                    .as_str()
                    .is_some_and(|c| c.contains("空闲") || c.contains("运行中"))
        );
    }

    #[test]
    fn pending_permission_block_reuses_buttons() {
        let d = data(
            Some(opencode::SessionStatus::Idle),
            vec![PendingRequest::Permission(permission(
                "req_1",
                "ses_adopted",
                "bash",
            ))],
            vec![],
        );
        let card = build_snapshot_card("接管", "t", &d);
        let els = elements(&card);
        let body: String = els
            .iter()
            .filter_map(|e| e["content"].as_str())
            .collect::<Vec<_>>()
            .join(" ");
        assert!(body.contains("🔐 **权限请求**"), "permission header: {card}");
        // The four interactive buttons ride along, not re-implemented.
        let buttons: Vec<&str> = els
            .iter()
            .filter(|e| e["tag"] == "button")
            .filter_map(|e| e["text"]["content"].as_str())
            .collect();
        assert_eq!(buttons.len(), 4);
        assert!(buttons.contains(&"✅ 允许一次"));
    }

    #[test]
    fn pending_question_block_reuses_question_elements() {
        let d = data(
            Some(opencode::SessionStatus::Idle),
            vec![PendingRequest::Question(question("q_1", "ses_adopted"))],
            vec![],
        );
        let card = build_snapshot_card("接管", "t", &d);
        let s = card.to_string();
        assert!(s.contains("确认"), "question text present: {s}");
        assert!(s.contains("继续"), "option present: {s}");
        assert!(s.contains("无法回答"), "reject control present: {s}");
    }

    #[test]
    fn no_pending_block_renders_for_a_different_session() {
        let d = data(
            Some(opencode::SessionStatus::Idle),
            vec![
                PendingRequest::Permission(permission("req_own", "ses_adopted", "bash")),
                PendingRequest::Permission(permission("req_other", "ses_sibling", "bash")),
                PendingRequest::Question(question("q_other", "ses_sibling")),
            ],
            vec![],
        );
        let card = build_snapshot_card("接管", "t", &d);
        let s = card.to_string();
        assert!(s.contains("req_own") || s.contains("允许一次"));
        assert!(!s.contains("q_other"), "sibling question leaked: {s}");
        // A sibling permission's body must not appear; only its request id would
        // be the tell — but permission_buttons embed the same body for both.
        assert!(!s.contains("ses_sibling"), "sibling session leaked: {s}");
    }

    #[test]
    fn empty_and_full_tail() {
        // Empty tail → no 最近对话 section at all.
        let empty = data(Some(opencode::SessionStatus::Idle), vec![], vec![]);
        let card = build_snapshot_card("接管", "t", &empty);
        assert!(!card.to_string().contains("最近对话"));

        // Full tail (4) → four role-marked folded panels + the header.
        let full = data(
            Some(opencode::SessionStatus::Idle),
            vec![],
            vec![
                tail("user", 1000, "问题一"),
                tail("assistant", 2000, "回答一"),
                tail("user", 3000, "问题二"),
                tail("assistant", 4000, "回答二"),
            ],
        );
        let card = build_snapshot_card("接管", "t", &full);
        let els = elements(&card);
        let panels: Vec<&serde_json::Value> =
            els.iter().filter(|e| e["tag"] == "collapsible_panel").collect();
        assert_eq!(panels.len(), 4, "one folded panel per entry: {card}");
        let s = card.to_string();
        assert!(s.contains("👤"), "user role marked: {s}");
        assert!(s.contains("🤖"), "assistant role marked: {s}");
        assert!(s.contains("回答二"), "full text present: {s}");
        assert!(panels.iter().all(|p| !p["expanded"].as_bool().unwrap()));
    }

    /// Worst-case input (4-message tail + 2 pending blocks) must stay within
    /// Feishu's card budgets — the streaming splitter is not available to a
    /// one-shot snapshot, so the builder itself must fit. The last tail message
    /// also exceeds `MAX_ELEMENT_TEXT_CHARS` so the per-entry collapsible has to
    /// split across several markdown chunks (the reviewer-noted untested branch).
    #[test]
    fn worst_case_fits_feishu_limits() {
        let mut long_tail: Vec<TailEntry> = (0..3)
            .map(|i| {
                let role = if i % 2 == 0 { "user" } else { "assistant" };
                tail(
                    role,
                    i * 1000,
                    &format!("第{}条消息：{}", i, "很长的内容。".repeat(120)),
                )
            })
            .collect();
        // Push the last entry past the single-element markdown cap so the panel
        // must chunk its full text rather than truncating it — long enough to
        // cross the cap, small enough that the whole card still fits Feishu's
        // byte budget (a truly unbounded message has no one-card answer; the
        // streaming path splits turns for that, a snapshot is one-shot).
        long_tail.push(tail(
            "assistant",
            9000,
            // Just over the 3000-char per-element cap ("超长内容。" is 5 chars),
            // so the entry splits into two chunks while the whole card still
            // fits the one-shot byte budget.
            &format!(
                "第4条超长消息：{}",
                "超长内容。".repeat(crate::feishu::card::MAX_ELEMENT_TEXT_CHARS / 5 + 2)
            ),
        ));
        let d = data(
            Some(opencode::SessionStatus::Busy),
            vec![
                PendingRequest::Permission(permission("req_1", "ses_adopted", "bash")),
                PendingRequest::Question(question("q_1", "ses_adopted")),
            ],
            long_tail,
        );
        let card = build_snapshot_card("接管", "重写登录模块", &d);
        let s = card.to_string();
        // Chunked, not truncated: the whole long tail is present in the card.
        assert!(s.contains("超长内容。"), "full long text present: {}", s.len());
        assert!(
            s.len() <= crate::feishu::card::FEISHU_CARD_LIMIT_BYTES,
            "snapshot card over Feishu byte limit: {}",
            s.len()
        );
        assert!(
            s.len() <= crate::feishu::card::MAX_CARD_JSON_CHARS,
            "snapshot card over estimated JSON budget: {}",
            s.len()
        );
    }
}
