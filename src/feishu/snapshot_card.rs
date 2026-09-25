use crate::backend::{MessageRole, TailEntry};
use crate::bridge::snapshot::{ElsewherePending, SnapshotData};
use crate::feishu::card::session::{BackToList, back_to_list_button};
use crate::opencode;
use serde_json::json;

/// One hint phrase shown under the 运行中 chip when the adopted session is busy
/// (ADR-0028): the snapshot's only live behaviour, the busy-follow (ticket 06),
/// will stream the in-flight turn into this card, so the chip says so up front.
pub(crate) const BUSY_HINT: &str = "有新进展会自动更新";

/// The busy run-state chip (ADR-0028). Tests assert this constant so a copy
/// tweak lives in one place.
pub(crate) const BUSY_CHIP: &str = "⚙️ 运行中";

/// The retry run-state chip.
pub(crate) const RETRY_CHIP: &str = "🔁 需要重试";

/// The idle run-state chip.
pub(crate) const IDLE_CHIP: &str = "✅ 空闲";

/// The 等待你的确认 chip, shown when the session has an adopt-time pending
/// request — the operator's action beats the server's run state.
pub(crate) const WAITING_CHIP: &str = "⏳ 等待你的确认";

/// The 等待你的确认 chip when the session's pendings are NOT embedded because
/// they are already hosted by another card, and Message Pin is on: the pointer
/// names the card and its pin (ADR-0028 update 2026-09-25).
pub(crate) const WAITING_ELSEWHERE_PINNED_CHIP: &str = "⏳ 等待你的确认（见置顶的原卡片）";

/// The pointer chip with Message Pin off: names the original card without
/// promising a pin, since the opt-in gates the pin itself.
pub(crate) const WAITING_ELSEWHERE_CHIP: &str = "⏳ 等待你的确认（见原卡片）";

/// The status chip content for a server run state (ADR-0028 precedence
/// 等待你的确认 > 运行中 > 需要重试 > 空闲; a busy session carries one hint
/// phrase). A pending already surfaced elsewhere keeps the operator action
/// ahead of the server state too, pointing at its hosting card instead. `None`
/// when there is no status to report — the chip is omitted rather than
/// guessed.
fn status_chip(
    has_pending: bool,
    pending_elsewhere: Option<ElsewherePending>,
    status: Option<opencode::types::SessionStatus>,
) -> Option<String> {
    if has_pending {
        return Some(WAITING_CHIP.to_string());
    }
    if let Some(elsewhere) = pending_elsewhere {
        return Some(
            match elsewhere {
                ElsewherePending::Pinned => WAITING_ELSEWHERE_PINNED_CHIP,
                ElsewherePending::Unpinned => WAITING_ELSEWHERE_CHIP,
            }
            .to_string(),
        );
    }
    match status {
        Some(opencode::types::SessionStatus::Busy) => Some(format!("{BUSY_CHIP}\n{BUSY_HINT}")),
        Some(opencode::types::SessionStatus::Retry) => Some(RETRY_CHIP.to_string()),
        Some(opencode::types::SessionStatus::Idle) => Some(IDLE_CHIP.to_string()),
        None => None,
    }
}

/// Build the status chip as a body element, if the precedence says one shows.
fn status_chip_element(
    has_pending: bool,
    pending_elsewhere: Option<ElsewherePending>,
    status: Option<opencode::types::SessionStatus>,
) -> Option<serde_json::Value> {
    status_chip(has_pending, pending_elsewhere, status)
        .map(|content| json!({ "tag": "markdown", "content": content }))
}
/// The live answer state of one claimed question block on the snapshot
/// (ADR-0028): request_id → state. Rebuilds after a button interaction pass
/// the flow's current state so the block mirrors the standalone question
/// card's 已选/✅ markers; the initial adopt-time snapshot passes nothing (all
/// questions open).
#[derive(Default, Clone)]
pub struct QuestionBlockState {
    /// The displayed answers per question slot (locked answers + live
    /// multi-select toggles).
    pub display: Vec<Option<Vec<String>>>,
    /// Whether each question slot is finalized (answered).
    pub done: Vec<bool>,
}

pub type SnapshotQuestionState = std::collections::HashMap<String, QuestionBlockState>;

/// The 最近对话 panel: the last-four tail entries, each role-marked and shown
/// folded — its header previews the entry briefly and expanding reveals the
/// full verbatim text (chunked to cola's per-element budget). No tail → no panel.
fn tail_panels(tail: &[TailEntry]) -> Vec<serde_json::Value> {
    let mut panels = Vec::new();
    if tail.is_empty() {
        return panels;
    }
    panels.push(json!({ "tag": "markdown", "content": "**最近对话**" }));
    // Transcript text is model/user-authored: sanitize it for the card dialect
    // (one budget for the card's tail).
    let mut md = crate::feishu::card::sanitize::CardMarkdown::new();
    for (i, entry) in tail.iter().enumerate() {
        let (role, preview) = tail_preview(entry);
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
        let chunks: Vec<String> = chunks.iter().map(|c| md.clean(c)).collect();
        // The snapshot is sent once, but SnapshotClaims rebuilds it in place on
        // a claim ack and on remote resolution — so its panels take stable ids
        // too (the tail's order is fixed, so the entry index is stable).
        panels.push(crate::feishu::card::shell::collapsible_panel_chunks(
            &title,
            &chunks,
            Some(&format!("snap_{i}")),
        ));
    }
    panels
}

/// The emoji marker for a tail entry's role. Shared by the static snapshot's
/// collapsible panels and the busy-adopt follow's static text (ADR-0028), so
/// the two presentations cannot disagree.
pub(crate) fn role_marker(role: &MessageRole) -> &'static str {
    match role {
        MessageRole::User => "👤",
        MessageRole::Assistant => "🤖",
        _ => "💬",
    }
}

/// One tail entry's role marker + short preview, shared by the static
/// snapshot's collapsible panels and the busy-adopt follow's static text
/// (ADR-0028).
pub(crate) fn tail_preview(entry: &TailEntry) -> (&'static str, String) {
    let preview: String = entry.text.chars().take(40).collect::<String>().trim().to_string();
    (role_marker(&entry.role), preview)
}

/// The snapshot's display title: the cleaned session label, falling back to
/// the id tail when the label is empty (shared by the full snapshot, the
/// compact suppressed-切换 state card, and the busy-adopt follow's static
/// prefix).
pub(crate) fn display_title(title: &str, session_id: &str) -> String {
    let label = crate::feishu::card::clean_session_label(title);
    if label.is_empty() {
        crate::bridge::display::id_tail(session_id)
    } else {
        label
    }
}

/// Build the one-shot Session Snapshot card (ADR-0028) for an adopted session.
/// `verb` is the takeover verb (接管/切换 — 已 is prefixed for the header) and
/// `title` the session's display title; the body comes entirely from the
/// read-side [`SnapshotData`]. Sections are assembled from individually
/// constructible builders so a later ticket can drop a resolved claim
/// (05) or stream a busy follow (06) into the same card. `back` is the
/// `/switch`-list state the adoption came from, when it came from the list
/// card: it renders the optional 「返回列表」 button (ADR-0052); every other
/// snapshot source passes `None`.
pub fn build_snapshot_card(
    verb: &str,
    title: &str,
    data: &SnapshotData,
    back: Option<&BackToList>,
) -> serde_json::Value {
    build_snapshot_card_with_state(verb, title, data, &SnapshotQuestionState::new(), &[], back)
}

/// [`build_snapshot_card`] with live question state: after an interaction on a
/// claimed question block, the rebuild passes the flow's current answers/done
/// flags so the embedded block mirrors the standalone question card.
/// `receipts` are Interaction Receipt lines for claimed blocks resolved by
/// another client (#175, ADR-0038 rule 4): one markdown line each, rendered
/// under the live blocks so a resolved block leaves a visible residue.
pub fn build_snapshot_card_with_state(
    verb: &str,
    title: &str,
    data: &SnapshotData,
    question_state: &SnapshotQuestionState,
    receipts: &[String],
    back: Option<&BackToList>,
) -> serde_json::Value {
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
    if let Some(chip) = status_chip_element(!pending.is_empty(), data.pending_elsewhere, data.status) {
        elements.push(chip);
    }
    for (i, req) in pending.iter().enumerate() {
        if i > 0 {
            elements.push(json!({ "tag": "hr" }));
        }
        elements.extend(req.snapshot_block_elements(&data.directory, question_state));
    }
    for line in receipts {
        elements.push(json!({ "tag": "markdown", "content": line }));
    }
    if (!pending.is_empty() || !receipts.is_empty()) && !data.tail.is_empty() {
        elements.push(json!({ "tag": "hr" }));
    }
    elements.extend(tail_panels(&data.tail));

    // The `/switch`-list adoption's way back to the page it came from
    // (ADR-0052), at the card's end where the list footer used to be.
    if let Some(back) = back {
        elements.push(back_to_list_button(back));
    }

    crate::feishu::card::shell::card_shell(&format!("已{verb} {title}"), "blue", elements)
}

/// Build the compact suppressed-切换 state card (ADR-0028): when a re-switch
/// to a session already mapped to this thread has nothing to report (idle, no
/// pending, newest user message cola-authored), the switch card patches to
/// this small confirmation instead of a full snapshot — the card-form mirror
/// of the text form's one-line ack. No status chip, no tail, no pending
/// blocks: there is deliberately nothing to report. `back` is the
/// `/switch`-list state the adoption came from, when it came from the list
/// card: it renders the optional 「返回列表」 button (ADR-0052) with the same
/// behavior as the full snapshot's.
pub fn build_switched_state_card(
    title: &str,
    session_id: &str,
    directory: &str,
    back: Option<&BackToList>,
) -> serde_json::Value {
    let title = display_title(title, session_id);
    let mut elements = vec![json!({
        "tag": "markdown",
        "content": format!("已切换到该会话（目录 `{directory}`）。")
    })];
    if let Some(back) = back {
        elements.push(back_to_list_button(back));
    }
    crate::feishu::card::shell::card_shell(&format!("已切换 {title}"), "blue", elements)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bridge::request::kind::PendingRequest;
    use crate::bridge::snapshot::SnapshotData;
    use crate::opencode::types::{PermissionRequest, QuestionInfo, QuestionOption, QuestionRequest};

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
        status: Option<opencode::types::SessionStatus>,
        pending: Vec<PendingRequest>,
        tail: Vec<TailEntry>,
    ) -> SnapshotData {
        SnapshotData {
            session_id: "ses_adopted".into(),
            directory: "/work/proj".into(),
            status,
            pending,
            pending_elsewhere: None,
            tail,
            newest_user_epoch: None,
            newest_user_is_cola_authored: false,
        }
    }

    fn tail(role: MessageRole, created_ms: i64, text: &str) -> TailEntry {
        TailEntry {
            role,
            created_ms,
            text: text.into(),
        }
    }

    fn elements(card: &serde_json::Value) -> &[serde_json::Value] {
        card["body"]["elements"].as_array().unwrap()
    }

    #[test]
    fn header_has_verb_and_title() {
        let d = data(Some(opencode::types::SessionStatus::Idle), vec![], vec![]);
        let card = build_snapshot_card("接管", "重写登录模块", &d, None);
        let h = card["header"]["title"]["content"].as_str().unwrap();
        assert_eq!(h, "已接管 重写登录模块", "header: {card}");
        assert_eq!(card["schema"].as_str().unwrap(), "2.0");
    }

    #[test]
    fn status_chip_precedence_matrix() {
        // Pending beats every run state.
        assert_eq!(
            status_chip(true, None, Some(opencode::types::SessionStatus::Busy)).as_deref(),
            Some(WAITING_CHIP)
        );
        assert_eq!(
            status_chip(true, None, Some(opencode::types::SessionStatus::Retry)).as_deref(),
            Some(WAITING_CHIP)
        );
        assert_eq!(
            status_chip(true, None, Some(opencode::types::SessionStatus::Idle)).as_deref(),
            Some(WAITING_CHIP)
        );
        assert_eq!(status_chip(true, None, None).as_deref(), Some(WAITING_CHIP));

        // An embedded pending beats a pending surfaced elsewhere: the pointer
        // is only for pendings the snapshot does NOT show.
        assert_eq!(
            status_chip(
                true,
                Some(ElsewherePending::Unpinned),
                Some(opencode::types::SessionStatus::Busy)
            )
            .as_deref(),
            Some(WAITING_CHIP)
        );

        // A pending surfaced elsewhere beats every run state too (ADR-0028
        // update 2026-09-25): the pointer copy depends on Message Pin.
        assert_eq!(
            status_chip(
                false,
                Some(ElsewherePending::Pinned),
                Some(opencode::types::SessionStatus::Busy)
            )
            .as_deref(),
            Some(WAITING_ELSEWHERE_PINNED_CHIP)
        );
        assert_eq!(
            status_chip(
                false,
                Some(ElsewherePending::Unpinned),
                Some(opencode::types::SessionStatus::Idle)
            )
            .as_deref(),
            Some(WAITING_ELSEWHERE_CHIP)
        );

        // No pending: the server state decides.
        assert!(
            status_chip(false, None, Some(opencode::types::SessionStatus::Busy))
                .unwrap()
                .contains(BUSY_CHIP)
        );
        assert_eq!(
            status_chip(false, None, Some(opencode::types::SessionStatus::Retry)).as_deref(),
            Some(RETRY_CHIP)
        );
        assert_eq!(
            status_chip(false, None, Some(opencode::types::SessionStatus::Idle)).as_deref(),
            Some(IDLE_CHIP)
        );
        // Unknown status and nothing pending → no chip, never guessed.
        assert_eq!(status_chip(false, None, None), None);
    }

    /// ADR-0028 update (2026-09-25): a pending already hosted by another card
    /// is not re-embedded, but the chip must say the session waits on the
    /// operator — never fall back to the server run state, never carry the
    /// busy hint.
    #[test]
    fn pending_elsewhere_points_at_the_host_card_without_the_busy_state() {
        let mut d = data(Some(opencode::types::SessionStatus::Busy), vec![], vec![]);
        d.pending_elsewhere = Some(ElsewherePending::Unpinned);
        let card = build_snapshot_card("接管", "t", &d, None);
        let body = elements(&card)[0]["content"].as_str().unwrap();
        assert_eq!(body, WAITING_ELSEWHERE_CHIP);
        let s = card.to_string();
        assert!(!s.contains(BUSY_CHIP), "no server run state: {s}");
        assert!(!s.contains(BUSY_HINT), "no busy hint: {s}");
    }

    /// The pointer promises 置顶 only when Message Pin is on (the same
    /// `[bridge] instant_reminder` opt-in gates the pin itself).
    #[test]
    fn pending_elsewhere_promises_the_pin_only_when_message_pin_is_on() {
        let mut d = data(None, vec![], vec![]);
        d.pending_elsewhere = Some(ElsewherePending::Pinned);
        let card = build_snapshot_card("接管", "t", &d, None);
        let body = elements(&card)[0]["content"].as_str().unwrap();
        assert_eq!(body, WAITING_ELSEWHERE_PINNED_CHIP);
        assert!(body.contains("置顶"), "{body}");
        assert!(!card.to_string().contains(BUSY_CHIP));
    }

    /// An embedded pending keeps the plain chip (no pointer), even when other
    /// pendings of the same session are surfaced elsewhere.
    #[test]
    fn embedded_pending_keeps_the_plain_waiting_chip() {
        let mut d = data(
            Some(opencode::types::SessionStatus::Busy),
            vec![PendingRequest::Permission(permission(
                "req_1",
                "ses_adopted",
                "bash",
            ))],
            vec![],
        );
        d.pending_elsewhere = Some(ElsewherePending::Pinned);
        let card = build_snapshot_card("接管", "t", &d, None);
        let body = elements(&card)[0]["content"].as_str().unwrap();
        assert_eq!(body, WAITING_CHIP);
        assert!(!card.to_string().contains("原卡片"), "{card}");
    }

    #[test]
    fn busy_chip_carries_one_hint() {
        let d = data(Some(opencode::types::SessionStatus::Busy), vec![], vec![]);
        let card = build_snapshot_card("接管", "t", &d, None);
        let body = elements(&card)[0]["content"].as_str().unwrap();
        assert!(body.contains(BUSY_CHIP));
        assert!(body.contains(BUSY_HINT));
    }

    #[test]
    fn unknown_status_renders_no_chip_line() {
        let d = data(None, vec![], vec![]);
        let card = build_snapshot_card("接管", "t", &d, None);
        let els = elements(&card);
        // Only the tail/none — no status markdown first.
        assert!(
            els.is_empty()
                || !els[0]["content"]
                    .as_str()
                    .is_some_and(|c| c.contains(IDLE_CHIP) || c.contains(BUSY_CHIP))
        );
    }

    #[test]
    fn pending_permission_block_reuses_buttons() {
        let d = data(
            Some(opencode::types::SessionStatus::Idle),
            vec![PendingRequest::Permission(permission(
                "req_1",
                "ses_adopted",
                "bash",
            ))],
            vec![],
        );
        let card = build_snapshot_card("接管", "t", &d, None);
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
            Some(opencode::types::SessionStatus::Idle),
            vec![PendingRequest::Question(question("q_1", "ses_adopted"))],
            vec![],
        );
        let card = build_snapshot_card("接管", "t", &d, None);
        let s = card.to_string();
        assert!(s.contains("确认"), "question text present: {s}");
        assert!(s.contains("继续"), "option present: {s}");
        assert!(s.contains("无法回答"), "reject control present: {s}");
    }

    #[test]
    fn no_pending_block_renders_for_a_different_session() {
        let d = data(
            Some(opencode::types::SessionStatus::Idle),
            vec![
                PendingRequest::Permission(permission("req_own", "ses_adopted", "bash")),
                PendingRequest::Permission(permission("req_other", "ses_sibling", "bash")),
                PendingRequest::Question(question("q_other", "ses_sibling")),
            ],
            vec![],
        );
        let card = build_snapshot_card("接管", "t", &d, None);
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
        let empty = data(Some(opencode::types::SessionStatus::Idle), vec![], vec![]);
        let card = build_snapshot_card("接管", "t", &empty, None);
        assert!(!card.to_string().contains("最近对话"));

        // Full tail (4) → four role-marked folded panels + the header.
        let full = data(
            Some(opencode::types::SessionStatus::Idle),
            vec![],
            vec![
                tail(MessageRole::User, 1000, "问题一"),
                tail(MessageRole::Assistant, 2000, "回答一"),
                tail(MessageRole::User, 3000, "问题二"),
                tail(MessageRole::Assistant, 4000, "回答二"),
            ],
        );
        let card = build_snapshot_card("接管", "t", &full, None);
        let els = elements(&card);
        let panels: Vec<&serde_json::Value> =
            els.iter().filter(|e| e["tag"] == "collapsible_panel").collect();
        assert_eq!(panels.len(), 4, "one folded panel per entry: {card}");
        // Stable ids, so a claim rebuild keeps each panel's fold state.
        let ids: Vec<&str> = panels
            .iter()
            .map(|p| p["element_id"].as_str().expect("panel element_id"))
            .collect();
        assert_eq!(ids, ["snap_0", "snap_1", "snap_2", "snap_3"], "{card}");
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
                let role = if i % 2 == 0 {
                    MessageRole::User
                } else {
                    MessageRole::Assistant
                };
                tail(
                    role,
                    i * 1000,
                    &format!("第{}条消息：{}", i, "很长的内容。".repeat(120)),
                )
            })
            .collect();
        // Push the last entry past cola's single-element budget so the panel
        // chunks its full text rather than truncating it — long enough to
        // cross the budget, small enough that the whole card still fits
        // Feishu's byte budget (a truly unbounded message has no one-card
        // answer; the streaming path splits turns for that, a snapshot is
        // one-shot).
        long_tail.push(tail(
            MessageRole::Assistant,
            9000,
            // Just over the 3000-char element budget ("超长内容。" is 5 chars),
            // so the entry splits into two chunks while the whole card still
            // fits the one-shot byte budget.
            &format!(
                "第4条超长消息：{}",
                "超长内容。".repeat(crate::feishu::card::MAX_ELEMENT_TEXT_CHARS / 5 + 2)
            ),
        ));
        let d = data(
            Some(opencode::types::SessionStatus::Busy),
            vec![
                PendingRequest::Permission(permission("req_1", "ses_adopted", "bash")),
                PendingRequest::Question(question("q_1", "ses_adopted")),
            ],
            long_tail,
        );
        let card = build_snapshot_card("接管", "重写登录模块", &d, None);
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

    /// ADR-0052: only an adoption from the `/switch` list carries the
    /// 「返回列表」 button on its full snapshot, and the button holds the
    /// list's keyword/scope/page.
    #[test]
    fn full_snapshot_back_button_renders_only_with_the_list_state() {
        let back = BackToList {
            thread_key: crate::config::ThreadKey::new("chat_1".into(), "chat_1".into()),
            keyword: "proj".into(),
            scope: crate::feishu::card::session::SwitchScope::All,
            page: 2,
        };
        let d = data(Some(opencode::types::SessionStatus::Idle), vec![], vec![]);

        let card = build_snapshot_card("接管", "t", &d, Some(&back));
        let button = elements(&card)
            .iter()
            .find(|e| e["tag"] == "button" && e["text"]["content"] == "返回列表")
            .expect("a list adoption renders the back button");
        assert_eq!(button["value"]["action"], "switch");
        assert_eq!(button["value"]["op"], "back");
        assert_eq!(button["value"]["keyword"], "proj");
        assert_eq!(button["value"]["scope"], "all");
        assert_eq!(button["value"]["page"], 2);
        assert_eq!(button["value"]["chat_id"], "chat_1");
        assert_eq!(button["value"]["thread_id"], "chat_1");

        // A snapshot from any other source (text /attach, /topic --adopt)
        // passes no list state and renders no button.
        let card = build_snapshot_card("接管", "t", &d, None);
        assert!(!card.to_string().contains("返回列表"), "{card}");
    }

    /// ADR-0052: the suppressed compact 已切换 state card carries the same
    /// back button when the adoption came from the list.
    #[test]
    fn switched_state_back_button_renders_only_with_the_list_state() {
        let back = BackToList {
            thread_key: crate::config::ThreadKey::new("chat_1".into(), "chat_1".into()),
            keyword: "proj".into(),
            scope: crate::feishu::card::session::SwitchScope::Directory,
            page: 3,
        };

        let card = build_switched_state_card("t", "ses_adopted", "/work/proj", Some(&back));
        let button = elements(&card)
            .iter()
            .find(|e| e["tag"] == "button" && e["text"]["content"] == "返回列表")
            .expect("the suppressed 已切换 card renders the back button");
        assert_eq!(button["value"]["op"], "back");
        assert_eq!(button["value"]["keyword"], "proj");
        assert_eq!(button["value"]["scope"], "dir");
        assert_eq!(button["value"]["page"], 3);

        let card = build_switched_state_card("t", "ses_adopted", "/work/proj", None);
        assert!(!card.to_string().contains("返回列表"), "{card}");
    }
}
