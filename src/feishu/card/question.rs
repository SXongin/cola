use serde_json::json;

use crate::backend::types::UncollectableReason;

use super::sanitize::CardMarkdown;
use super::shell::card_shell;
use super::tool_render::json_escaped_len;

/// A single-select question with more options than this collapses them all
/// into a folding `overflow` group instead of a tall stack of buttons (the
/// heading still lists every option). Multi-selects always stay on buttons so
/// clicks toggle the running selection.
const MAX_VISIBLE_OPTIONS: usize = 3;

/// The most characters the free-text answer box accepts. The client enforces
/// it; the bridge caps a stored draft at the same number, because
/// `max_length` is a client-side rule.
pub(crate) const CUSTOM_INPUT_MAX_CHARS: usize = 500;

/// The most characters the numeric box accepts (a number is short).
pub(crate) const NUMERIC_INPUT_MAX_CHARS: usize = 40;

/// Display cap for a Custom Answer button label. Long or multiline answers
/// collapse and truncate in the button only; the stored answer and the click
/// value keep the raw text.
const CUSTOM_ANSWER_LABEL_CHARS: usize = 20;

/// The copy that explains what the session-scoped Always button means. Pinned
/// here, beside the button, so the card cannot describe a scope it does not
/// offer — and an adapter never writes UI copy.
pub(crate) const SESSION_ALWAYS_NOTE: &str = "🔁 始终允许：本会话内后续同类请求将自动批准";

/// The permission controls, as body elements: Allow Once / Allow Always (only
/// when the backend advertised a session-scoped grant) / Deny / Auto-Accept.
/// Shared by the standalone permission card and the inline section on the
/// streaming card. The fourth turns on the session's Auto-Accept (cola-side,
/// `/autoaccept`), distinct from "Allow Always" which is a backend-side
/// session rule.
pub fn permission_buttons(
    session_id: &str,
    request_id: &str,
    directory: &str,
    always_supported: bool,
    target_digest: Option<u64>,
) -> Vec<serde_json::Value> {
    // The callback carries routing plus the decision and its display labels —
    // never the body. The body is the largest part of the block, and echoing it
    // into every button multiplied the card's JSON by the button count; a click
    // rebuilds the result body from what it already has (the verified pending
    // request, or the block's recorded target).
    let summary_digest = target_digest.map(target_digest_hex);
    permission_control_elements(always_supported, |reply, _decision, label, color| {
        let mut value = json!({
            "action": "perm",
            "reply": reply,
            "session_id": session_id,
            "request_id": request_id,
            "directory": directory,
            "perm_label": label,
            "perm_color": color,
        });
        if let Some(digest) = &summary_digest {
            // A summary surface proves itself again at the click: the marker
            // and the digest let the guard reject a request whose target
            // changed after the companion was delivered.
            value["summary"] = json!(true);
            value["target_digest"] = json!(digest);
        }
        value
    })
}

/// The four permission controls in their shared order: Allow Once / Allow
/// Always (only when the backend advertised a session-scoped grant) / Deny /
/// Auto-Accept. `value` supplies each button's callback payload, so the
/// ordinary card and the fallback card share every label, type and ordering
/// decision and differ only in what a click means.
fn permission_control_elements(
    always_supported: bool,
    mut value: impl FnMut(&str, Option<&str>, &str, &str) -> serde_json::Value,
) -> Vec<serde_json::Value> {
    let mut elements = Vec::new();
    if always_supported {
        elements.push(json!({ "tag": "markdown", "content": SESSION_ALWAYS_NOTE }));
    }
    elements.push(json!({
        "tag": "button",
        "text": { "tag": "plain_text", "content": "✅ 允许一次" },
        "type": "primary",
        "value": value("once", Some("once"), "✅ 已允许一次", "green"),
    }));
    if always_supported {
        elements.push(json!({
            "tag": "button",
            "text": { "tag": "plain_text", "content": "🔁 始终允许" },
            "type": "default",
            "value": value("always", Some("always"), "✅ 已始终允许", "green"),
        }));
    }
    elements.extend([
        json!({
            "tag": "button",
            "text": { "tag": "plain_text", "content": "🚫 拒绝" },
            "type": "danger",
            "value": value("reject", None, "🚫 已拒绝", "red"),
        }),
        json!({
            "tag": "button",
            "text": { "tag": "plain_text", "content": "⚡ 开启自动授权" },
            "type": "primary",
            "value": value("autoaccept", None, "✅ 已开启自动授权", "blue"),
        }),
    ]);
    elements
}

/// Build the interactive permission card (JSON 2.0). Buttons sit directly in
/// `body.elements` — the v1 `action` container is gone in schema 2.0. Each
/// button carries the request id, session id and owning directory so the card
/// callback (`action: "perm"`) can route the reply back to the right instance;
/// the body is rendered once above the controls, never echoed into them.
pub fn build_permission_card(
    session_id: &str,
    request_id: &str,
    body: &str,
    directory: &str,
    always_supported: bool,
    target_digest: Option<u64>,
) -> serde_json::Value {
    let body = super::sanitize::CardMarkdown::new().clean(body);
    let mut elements = vec![json!({ "tag": "markdown", "content": body })];
    elements.extend(permission_buttons(
        session_id,
        request_id,
        directory,
        always_supported,
        target_digest,
    ));
    card_shell("🔐 权限请求", "orange", elements)
}

/// The risk paragraph the fallback card and its confirmation step share when
/// the companion definitely failed (ADR-0102 / #1013). It is deliberately
/// explicit that approval approves the command the card could not show, and
/// points at the original client where the full request can be read.
pub(crate) fn fallback_permission_risk_note() -> &'static str {
    "完整目标未能送达；批准即批准未能在卡片上完整显示的完整命令。\
     建议先在原始客户端查看详情。"
}

/// The risk paragraph for the cause at hand: the definite wording above, or a
/// neutral wording when the send may have landed. The unknown case must not
/// claim a failure the companion cannot prove.
fn fallback_permission_risk_note_for(loss: FallbackLoss) -> &'static str {
    match loss {
        FallbackLoss::NotDelivered => fallback_permission_risk_note(),
        FallbackLoss::OutcomeUnknown => {
            "完整目标未能确认送达；批准即批准未能在卡片上完整显示的完整命令。\
             建议先在原始客户端查看详情。"
        }
        FallbackLoss::PreviousDelivered => {
            "完整目标已变化；批准即批准未能在卡片上完整显示的完整命令。\
             建议先在原始客户端查看详情。"
        }
    }
}

/// The header of the standalone card shown when neither companion lane could
/// deliver a permission's full target.
pub(crate) const FALLBACK_PERMISSION_HEADER: &str = "🔐 权限请求（目标未完整展示）";

/// The header of the second-step confirmation card. The risk is named in the
/// body by [`fallback_permission_risk_note_for`].
pub(crate) const FALLBACK_PERMISSION_CONFIRM_HEADER: &str = "⚠️ 确认批准（目标未完整展示）";

/// Why the full target is absent from the fallback card. The distinction is
/// the companion outcome's own: a definite failure is not the same claim as an
/// unknown send, and the fallback copy must not turn one into the other
/// (ADR-0102).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) enum FallbackLoss {
    /// No lane delivered; the complete target definitely did not land.
    #[default]
    NotDelivered,
    /// A send may have landed; there is no delivery proof.
    OutcomeUnknown,
    /// A confirmed companion carried the *previous* target; the target has
    /// since changed, so the current one is not on the card.
    PreviousDelivered,
}

impl FallbackLoss {
    /// The callback key the fallback card carries so the confirmation and the
    /// result card keep the same claim.
    pub(crate) fn key(self) -> &'static str {
        match self {
            Self::NotDelivered => "not_delivered",
            Self::OutcomeUnknown => "outcome_unknown",
            Self::PreviousDelivered => "previous_delivered",
        }
    }

    /// Parse a callback key. Only the explicit `not_delivered` key may claim a
    /// definite failure; a missing or malformed key is treated as unknown, so a
    /// legacy or malformed card cannot turn an ambiguous send into a false
    /// claim that nothing arrived.
    pub(crate) fn from_key(key: Option<&str>) -> Self {
        match key {
            Some("not_delivered") => Self::NotDelivered,
            Some("previous_delivered") => Self::PreviousDelivered,
            _ => Self::OutcomeUnknown,
        }
    }

    /// The content line under the target head. The risk paragraph follows both.
    fn content_line(self) -> &'static str {
        match self {
            Self::NotDelivered => "未能送达。",
            Self::OutcomeUnknown => "发送结果未知，可能已送达。",
            Self::PreviousDelivered => "之前的目标已完整送达；当前目标已变化，未能完整展示。",
        }
    }
}

/// The fallback body WITHOUT the ask/final confirmation line, so the card
/// builders own the one place that appends their prompt. The target head comes
/// from the bridge ([`crate::bridge::request::companion::describe_permission_target_summary`]);
/// this copy table adds the content claim, the risk and — when the user comes
/// back after a digest mismatch — the changed-target note.
pub(crate) fn fallback_permission_summary(target_summary: &str, loss: FallbackLoss, changed: bool) -> String {
    let mut summary = format!(
        "{target_summary}\n**完整内容**: {}\n\n**风险**: {}",
        loss.content_line(),
        fallback_permission_risk_note_for(loss)
    );
    if changed {
        summary.push_str(concat!(
            "\n\n**目标已变化**: 在你确认之后目标发生了变化，原来的确认不再有效；",
            "请重新查看并再次确认。"
        ));
    }
    summary
}

/// The fallback card's controls: allow-once / allow-always are FIRST-STEP
/// buttons that only open [`build_fallback_permission_confirm_card`]; deny and
/// Auto-Accept stay one click. Each button carries the routing triple, the
/// loss cause, the risk digest of the request's patterns (as a hex string, so
/// a 64-bit value survives Feishu's JSON round-trip), and `fallback: true` so
/// the click handler never treats the ack as a streaming-card update.
fn fallback_permission_buttons(
    session_id: &str,
    request_id: &str,
    directory: &str,
    always_supported: bool,
    target_digest: u64,
    loss: FallbackLoss,
) -> Vec<serde_json::Value> {
    let digest = target_digest_hex(target_digest);
    permission_control_elements(always_supported, |reply, decision, label, color| {
        let first_step = matches!(reply, "once" | "always");
        // The allow buttons are a first step, not a decision: the click opens
        // the confirmation card and never reaches the backend.
        let callback_reply = if first_step { "confirm" } else { reply };
        // Deny and Auto-Accept keep their ordinary wording; the three
        // decisions that touch the unseen target name it.
        let label = if first_step || reply == "reject" {
            format!("{label}（目标未完整展示）")
        } else {
            label.to_string()
        };
        let mut value = json!({
            "action": "perm",
            "reply": callback_reply,
            "session_id": session_id,
            "request_id": request_id,
            "directory": directory,
            "perm_label": label,
            "perm_color": color,
            "fallback": true,
            "always_supported": always_supported,
            "target_digest": digest,
            "loss": loss.key(),
        });
        if let Some(decision) = decision {
            value["decision"] = json!(decision);
        }
        value
    })
}

/// A digest's callback spelling: lowercase hex, fixed width. `value` is only
/// guaranteed to be a JSON object, and a `u64` above 2^53 does not survive a
/// double/string round-trip; the guard parses this back, and a parse failure is
/// a mismatch, never a panic.
pub(crate) fn target_digest_hex(digest: u64) -> String {
    format!("{digest:016x}")
}

/// Build the standalone fallback card for a permission whose complete target
/// no companion lane delivered. `summary` is
/// [`fallback_permission_summary`], so the risk and the target summary cannot
/// drift from the confirmation or result.
///
/// It is standalone by construction: the click ack replaces *this* card, never
/// a streaming card that happens to be live for the same session.
pub(crate) fn build_fallback_permission_card(
    session_id: &str,
    request_id: &str,
    directory: &str,
    summary: &str,
    always_supported: bool,
    target_digest: u64,
    loss: FallbackLoss,
) -> serde_json::Value {
    let body = format!(
        "{}\n\nAI 想要执行这个操作，是否允许？",
        super::sanitize::CardMarkdown::new().clean(summary)
    );
    let mut elements = vec![json!({ "tag": "markdown", "content": body })];
    elements.extend(fallback_permission_buttons(
        session_id,
        request_id,
        directory,
        always_supported,
        target_digest,
        loss,
    ));
    card_shell(FALLBACK_PERMISSION_HEADER, "orange", elements)
}

/// Build the second-step confirmation card: the same summary and risk, then
/// 确认批准 (which carries the original decision, `risk_confirmed`, and the
/// target digest) and 取消 (which returns to the fallback card without a reply).
#[allow(clippy::too_many_arguments)] // the routing triple, decision and copy context
pub(crate) fn build_fallback_permission_confirm_card(
    session_id: &str,
    request_id: &str,
    directory: &str,
    summary: &str,
    target_digest: u64,
    decision: &str,
    always_supported: bool,
    loss: FallbackLoss,
) -> serde_json::Value {
    let body = format!(
        "{}\n\n确认批准将执行上面这个未能在卡片上完整显示的目标。",
        super::sanitize::CardMarkdown::new().clean(summary)
    );
    let digest = target_digest_hex(target_digest);
    let perm_label = if decision == "always" {
        "✅ 已始终允许（目标未完整展示）"
    } else {
        "✅ 已允许一次（目标未完整展示）"
    };
    let confirm = json!({
        "action": "perm",
        "reply": decision,
        "session_id": session_id,
        "request_id": request_id,
        "directory": directory,
        "perm_label": perm_label,
        "perm_color": "green",
        "fallback": true,
        "risk_confirmed": true,
        "target_digest": digest,
        "always_supported": always_supported,
        "loss": loss.key(),
    });
    let cancel = json!({
        "action": "perm",
        "reply": "cancel_confirm",
        "session_id": session_id,
        "request_id": request_id,
        "directory": directory,
        "perm_label": "已取消",
        "perm_color": "grey",
        "fallback": true,
        "target_digest": digest,
        "always_supported": always_supported,
        "loss": loss.key(),
    });
    card_shell(
        FALLBACK_PERMISSION_CONFIRM_HEADER,
        "red",
        vec![
            json!({ "tag": "markdown", "content": body }),
            json!({
                "tag": "button",
                "text": { "tag": "plain_text", "content": "✅ 确认批准" },
                "type": "danger",
                "value": confirm,
            }),
            json!({
                "tag": "button",
                "text": { "tag": "plain_text", "content": "取消" },
                "type": "default",
                "value": cancel,
            }),
        ],
    )
}

/// The receipt an inline block collapses to when its request moves to the
/// standalone fallback card. The request is still pending; this line only
/// stops the old block's controls from lingering on the streaming card.
pub(crate) const FALLBACK_PERMISSION_MOVED_RECEIPT: &str = "🔄 已改为单独的风险确认卡片";

/// How many body elements one permission block adds: its description element
/// plus every element [`permission_buttons`] renders (the scope note when the
/// session decision is advertised, then the buttons).
///
/// The streaming card's split estimate charges this beside the build, so a
/// near-budget card reserves room for the block instead of appending it past
/// Feishu's limit.
pub(crate) fn permission_block_components(always_supported: bool) -> usize {
    1 + permission_button_count(always_supported)
}

/// How many elements [`permission_buttons`] renders: `once`, `always` (only
/// when advertised), `reject` and `autoaccept`, plus the scope note.
fn permission_button_count(always_supported: bool) -> usize {
    3 + 2 * usize::from(always_supported)
}

/// The serialized-JSON size estimate of one permission block.
///
/// The body is rendered **once**, as the block's own markdown element; the
/// buttons no longer echo it, so it is charged once. Every button still carries
/// the whole routing triple (`session_id`, `request_id`, `directory`) in its
/// callback value, so each of those appears once as the block's element and
/// once per button. The routing values come from the caller — a long directory
/// is the common case, and a fixed overhead would let the streaming card's
/// split underestimate itself past Feishu's limit.
///
/// The body is estimated **as the card renders it**, not as authored:
/// [`CardMarkdown::element`] is what both the standalone card and the streaming
/// body apply, and it expands `<` to `&#60;` (five bytes for one), so a
/// 150-char reason of `<` adds 600 bytes to the element that a raw-length
/// estimate would miss. The result is then charged at its JSON-escaped length
/// ([`json_escaped_len`]), taking the larger of the cleaned and fenced element
/// so the content-rejection fallback is covered too.
pub(crate) fn permission_block_estimate(
    body: &str,
    always_supported: bool,
    summary: bool,
    request_id: &str,
    session_id: &str,
    directory: &str,
) -> usize {
    // A button's own labels, action, reply, colour and JSON punctuation. CJK
    // labels cost three bytes a character, which is why this is generous.
    const BUTTON_OVERHEAD: usize = 360;
    // A summary block's buttons also carry the surface marker and the
    // hex digest. Charge them explicitly so a split estimate cannot drift
    // behind the rendered callback.
    let summary_keys = if summary {
        json_escaped_len("\"summary\":true,\"target_digest\":\"0123456789abcdef\",")
    } else {
        0
    };
    // The element is rendered either cleaned (normal) or fenced (the card's
    // content-rejection fallback, which wraps the body in a fence sized to its
    // longest backtick run — a 3,000-backtick command gains ~6 KB). Charge the
    // larger of the two so neither path can outgrow the reserve.
    let cleaned = json_escaped_len(&CardMarkdown::new().element(body));
    let fenced = json_escaped_len(body) + crate::feishu::card::fenced_block_overhead(body);
    let element = cleaned.max(fenced);
    let routing = json_escaped_len(request_id) + json_escaped_len(session_id) + json_escaped_len(directory);
    let buttons = permission_button_count(always_supported);
    let note = if always_supported {
        json_escaped_len(&CardMarkdown::new().element(SESSION_ALWAYS_NOTE)) + 60
    } else {
        0
    };
    element + buttons * (routing + BUTTON_OVERHEAD + summary_keys) + note
}

/// Compact one-line target for a question's Interaction Receipt: each
/// question's own text, joined, clipped to stay a residue (ADR-0038, rule 4).
/// Shared by the inline block's receipt and the standalone-card path that names
/// the same request.
///
/// The text is what the question actually asked. The header is only Codex's
/// own 12-character UI label (a word like `Color`), so a receipt built from it
/// leaves the reader unable to tell what they answered — the question's
/// sentence is the only part that can.
pub(crate) fn question_target(questions: &[crate::backend::types::QuestionInfo]) -> String {
    super::truncate_md(
        &questions
            .iter()
            .map(|qi| question_label(qi, 24))
            .collect::<Vec<_>>()
            .join("、"),
        60,
    )
}

/// One question's display label for a residue: the question it asked, with its
/// short header as the fallback when the text is empty.
pub(crate) fn question_label(question: &crate::backend::types::QuestionInfo, max_chars: usize) -> String {
    let text = if question.question.is_empty() {
        &question.header
    } else {
        &question.question
    };
    let label = one_line_label(text, max_chars);
    if label.is_empty() {
        // The capped window held only whitespace: the header is still a usable
        // one-word target, through the same sanitizer.
        one_line_label(&question.header, max_chars)
    } else {
        label
    }
}

/// The most input characters a receipt label scans. A card callback must stay
/// cheap and bounded, and a residue is at most `max_chars`; this window leaves
/// room for a whitespace run or markdown decoration without ever walking a
/// model-sized question. A question (or header) longer than it is clipped to
/// the window, which is far past the visible budget's needs.
const LABEL_SCAN_CHARS: usize = 512;

/// One markdown line from model-authored text: a window-bounded scan, newlines
/// collapsed, backticks flattened to apostrophes (an unmatched one would break
/// the residue's inline formatting), then clipped to the visible budget.
fn one_line_label(text: &str, max_chars: usize) -> String {
    let window: String = text.chars().take(LABEL_SCAN_CHARS).collect();
    let flattened = window.split_whitespace().collect::<Vec<_>>().join(" ");
    let safe = flattened.replace('`', "'");
    super::truncate_md(&safe, max_chars)
}

/// A one-line-per-question summary of a `question` request, for the stale card.
pub fn question_summary(questions: &[crate::backend::types::QuestionInfo]) -> String {
    super::sanitize::CardMarkdown::new().clean(&question_summary_text(questions))
}

/// [`question_summary`] before markdown hygiene, for a caller that threads one
/// card-wide [`super::sanitize::CardMarkdown`].
fn question_summary_text(questions: &[crate::backend::types::QuestionInfo]) -> String {
    let mut s = String::new();
    for (i, q) in questions.iter().enumerate() {
        s.push_str(&format!("{}. {}\n", i + 1, q.question));
    }
    s
}

/// The card copy for a question cola cannot collect at all. The neutral model
/// carries only [`UncollectableReason`]; the user-facing wording lives here,
/// in the card copy table, and every rendering path (standalone, inline) asks
/// for it.
pub(crate) fn uncollectable_note(reason: UncollectableReason) -> &'static str {
    match reason {
        UncollectableReason::SecretInput => {
            "该提问需要私密输入，cola 无法在卡片上安全收集；请在这里拒绝，必要时请在原始客户端重新发起该提问。"
        }
    }
}

/// Why an over-budget request is refused instead of taking the companion lane:
/// there is no usable card target at all (neither a message to reply under nor
/// a chat). Shared by the reason copy, the retry copy and the receipt, so they
/// cannot drift.
pub(crate) const NO_COMPANION_TARGET: &str = "没有可用的聊天目标，无法送达完整内容";

/// The header of the card that records a permission cola auto-declined because
/// its target cannot be rendered in full and no chat target can carry the
/// companion. Pinned beside the note below, so the decision and its explanation
/// cannot diverge.
pub(crate) const UNSHOWABLE_PERMISSION_HEADER: &str = "🔐 权限请求已自动拒绝";

/// Why such a request is declined, and what the user can do. The permission
/// flow declines the request on the server and asks this function for the copy;
/// the card below renders the same text, so the two never disagree.
pub(crate) fn unshowable_permission_note(budget: usize) -> String {
    format!(
        "该权限请求在卡片上需要显示的内容超过 **{budget}** 字符，卡片无法完整显示，\
         完整内容也未能送达。为了不让你批准只看到一部分的内容，\
         cola 已自动拒绝，该请求在服务端已结清——其他客户端也无法再批准。\
         请让 AI 把这条操作拆成更短的步骤后重试；或先开启自动授权（`/autoaccept on`）再让它执行，\
         自动授权不需要逐条查看。"
    )
}

/// The card that explains an auto-declined, unshowable permission. It carries
/// no controls: the decision is already made, and the point is to say so rather
/// than to offer a partial target.
pub(crate) fn build_unshowable_permission_card(budget: usize) -> serde_json::Value {
    card_shell(
        UNSHOWABLE_PERMISSION_HEADER,
        "grey",
        vec![json!({ "tag": "markdown", "content": unshowable_permission_note(budget) })],
    )
}

/// The receipt line an inline permission block collapses to when cola **could
/// not land** the refusal it wanted (a failed server reply). It claims nothing:
/// the request is still pending and the sweep retries, so the block must not
/// look like a decision either way.
pub(crate) const UNSHOWABLE_PERMISSION_RETRY_RECEIPT: &str = "⏳ 目标无法完整显示，稍后重试";

/// The header of the card that replaces a standalone permission card while
/// cola retries a refusal it could not land. Pinned with the retry note below.
pub(crate) const UNSHOWABLE_PERMISSION_RETRY_HEADER: &str = "⏳ 权限请求待重试";

/// Why that card carries no controls.
pub(crate) fn unshowable_permission_retry_note() -> String {
    format!(
        "该权限请求的目标过长，卡片无法完整显示，{}；\
         cola 暂时无法自动拒绝，稍后会自动重试。这张卡片不提供批准操作。",
        NO_COMPANION_TARGET
    )
}

/// The card that replaces a standalone permission card while the refusal is
/// retried: non-interactive, and claiming neither a decision nor another
/// client's.
pub(crate) fn build_unshowable_permission_retry_card() -> serde_json::Value {
    card_shell(
        UNSHOWABLE_PERMISSION_RETRY_HEADER,
        "grey",
        vec![json!({ "tag": "markdown", "content": unshowable_permission_retry_note() })],
    )
}

/// The receipt an inline over-budget block collapses to when its old card is
/// superseded by a fresh process. The request itself is reprocessed (Auto-
/// Accept or a new companion card); this card only stops being interactive.
pub(crate) const SUPERSEDED_PERMISSION_RECEIPT: &str = "🔄 已重新处理：旧卡片已失效";

/// The header of a standalone card that is superseded by a fresh process.
pub(crate) const SUPERSEDED_PERMISSION_HEADER: &str = "🔄 权限请求正在重新处理";

/// Why a recovered card carries no controls.
pub(crate) fn superseded_permission_note() -> &'static str {
    "这张卡片来自旧的进程，已不再提供操作；该请求会由当前进程重新处理——\
     自动授权会话会直接批准，其他会话会重新送达完整内容并提供新的权限卡片。"
}

/// The non-interactive card that replaces a recovered card the new process
/// cannot re-adopt (ADR-0101, ADR-0102).
pub(crate) fn build_superseded_permission_card() -> serde_json::Value {
    card_shell(
        SUPERSEDED_PERMISSION_HEADER,
        "grey",
        vec![json!({ "tag": "markdown", "content": superseded_permission_note() })],
    )
}

/// The receipt an inline block shows while its superseded withdrawal is
/// retried: the old card is no longer valid, but the request is being
/// REPROCESSED, not auto-refused.
pub(crate) const SUPERSEDED_PERMISSION_PREVIEW_RECEIPT: &str = "🔄 旧卡片已失效，正在重新处理";

/// The header of the standalone preview card shown while the superseded
/// withdrawal is retried.
pub(crate) const SUPERSEDED_PERMISSION_PREVIEW_HEADER: &str = "🔄 权限请求正在重新处理";

/// Why the preview card carries no controls.
pub(crate) fn superseded_permission_preview_note() -> &'static str {
    "这张卡片来自旧的进程，已不再提供操作；该请求正在由当前进程重新处理，\
     稍后会以自动授权或新的权限卡片呈现。"
}

/// The non-interactive preview card shown while the superseded withdrawal is
/// retried.
pub(crate) fn build_superseded_permission_preview_card() -> serde_json::Value {
    card_shell(
        SUPERSEDED_PERMISSION_PREVIEW_HEADER,
        "grey",
        vec![json!({
            "tag": "markdown",
            "content": superseded_permission_preview_note()
        })],
    )
}

/// The header of the card that explains a permission whose companion send
/// outcome is unknown. An unconfirmed delivery is not a completeness proof, so
/// the request was declined and no interactive approval is offered.
pub(crate) const UNCERTAIN_PERMISSION_HEADER: &str = "🚫 权限请求已拒绝（完整内容送达结果未知）";

/// Why the request was declined when the companion's outcome is unknown.
pub(crate) fn uncertain_permission_note() -> &'static str {
    "该权限请求的完整内容超过卡片显示预算，cola 已尝试通过伴随消息发送，但发送结果未知——它可能已送达，也可能没有。\
     在无法确认完整内容已送达之前，不能提供批准操作；该请求已按拒绝处理，在服务端已结清。\
     如需继续，请让 AI 重新发起该操作；也可以先开启自动授权（`/autoaccept on`）再执行。"
}

/// The non-interactive card that explains an unknown companion outcome.
pub(crate) fn build_uncertain_permission_card() -> serde_json::Value {
    card_shell(
        UNCERTAIN_PERMISSION_HEADER,
        "grey",
        vec![json!({ "tag": "markdown", "content": uncertain_permission_note() })],
    )
}

/// The receipt an inline over-budget block collapses to when its companion
/// outcome is unknown and the request was declined.
pub(crate) const UNCERTAIN_PERMISSION_RECEIPT: &str = "🚫 已拒绝：完整内容送达结果未知";

/// The label of the result card when a click is refused because the companion
/// outcome is unknown.
pub(crate) const UNCERTAIN_PERMISSION_CLICK_LABEL: &str = "🚫 已拒绝（完整内容送达结果未知）";

/// The label of the result card cola shows when its click finds no usable
/// confirmed companion delivery. It is a CLICK decision, not an automatic
/// refusal of the request, so the wording says the click was declined. It
/// deliberately does not claim the content was never sent: a stale record may
/// have delivered the *previous* target, and the full note below names that
/// possibility.
pub(crate) const MISSING_PERMISSION_CLICK_LABEL: &str = "🚫 已拒绝（没有匹配的完整内容）";

/// The receipt a clicked block collapses to when no usable confirmed companion
/// exists for it: the user's click was refused, nobody auto-declined the
/// request, and the bound may be the rendered body rather than a target length.
pub(crate) const MISSING_PERMISSION_RECEIPT: &str = "🚫 已拒绝：没有匹配的完整内容";

/// Why the click guard refuses an over-budget approval whose confirmed
/// companion delivery is missing or stale (ADR-0102): no delivered content
/// proves this exact request, so there is nothing the user could have reviewed
/// to approve. The click itself rejects the request, so it is settled
/// server-side.
pub(crate) fn missing_permission_click_note() -> String {
    "该权限请求的完整内容超过卡片显示预算，且没有找到已送达的伴随消息（或内容已变化）。\
     为了不让你批准没有完整看到的内容，本次点击已拒绝，该请求在服务端已结清。\
     如需继续，请让 AI 重新发起该操作；也可以先开启自动授权（`/autoaccept on`）再执行。"
        .to_string()
}

/// The receipt line an inline permission block collapses to when cola refused
/// it before it could be offered — its target is too long for the card to show
/// in full, so approving it would approve unseen text. Pinned with the refusal
/// header/note above, so the block, the standalone card and the chat notice
/// cannot drift.
pub(crate) fn unshowable_permission_receipt() -> String {
    format!("🚫 已自动拒绝：目标过长，{NO_COMPANION_TARGET}")
}

/// The header of a standalone card whose request cannot be collected at all.
pub(crate) const UNCOLLECTABLE_HEADER: &str = "❓ 无法在卡片上作答";

/// The body elements of a form prompt (spec #364, S6). Each typed field renders
/// as its own block — a markdown heading (number, title, description, status)
/// followed by the controls its kind demands, with a divider between blocks:
///
/// - a `String` with options renders option buttons (or a folding `overflow`
///   group when a single-select has many), displaying each option's label while
///   submitting its value; a free-text input is added when `custom` allows (or
///   when the field has no options at all);
/// - a `Multiselect` renders toggle buttons whose clicks add/remove values in
///   the running selection ("已选"), committed by a per-question "确定该题"
///   button; typed custom answers render as removable chips kept verbatim;
/// - a `Boolean` renders 是/否 buttons, a `Number`/`Integer` a typed input, and
///   an `External` field its link plus an acknowledgement button.
///
/// `done[i]` marks a finalized field (a single-select/boolean answered by a
/// click, a multi-select confirmed) — it renders as a static "✅ … 已选" line
/// instead of controls, so answering one field never silently submits the
/// others. `answered[i]` is the finalized answer's submitted VALUES (or an open
/// multi-select's live toggles); the heading maps them to display labels. A
/// submit button appears when some (but not all) fields are answered (skip
/// remaining); a reject/cancel button sits at the bottom. The card callback
/// (`action: "question"`) posts the keyed answer back to the session. Shared by
/// the standalone question card and the inline section on the streaming card.
///
/// `title` (an elicitation's message) and `uncollectable` render at the top of
/// the block: the standalone card passes an empty title because its header
/// carries it, while an inline block shows both here. An
/// `uncollectable` request renders its reason and a decline control only —
/// never a field an answer could be typed into.
#[allow(clippy::too_many_arguments)] // title, reason, routing, fields, display state
pub fn question_elements(
    title: &str,
    uncollectable: Option<UncollectableReason>,
    request_id: &str,
    session_id: &str,
    questions: &[crate::backend::types::QuestionInfo],
    directory: &str,
    answered: &[Option<Vec<String>>],
    done: &[bool],
    open: usize,
    drafts: &[String],
) -> Vec<serde_json::Value> {
    let mut md = super::sanitize::CardMarkdown::new();
    // The public question card keeps the semantic "cannot answer"; the
    // Location Form passes 「取消」, its explicit decline.
    question_elements_with(
        &mut md,
        "🚫 无法回答",
        title,
        uncollectable,
        request_id,
        session_id,
        questions,
        directory,
        answered,
        done,
        open,
        drafts,
    )
}

/// [`question_elements`] on a sanitizer the caller already owns, for a card
/// that builds more than one model-text element: the [`CardMarkdown`] table
/// budget is per CARD, not per element, so a Location Form's context and its
/// question body must share one instance.
#[allow(clippy::too_many_arguments)]
fn question_elements_with(
    md: &mut super::sanitize::CardMarkdown,
    decline_label: &str,
    title: &str,
    uncollectable: Option<UncollectableReason>,
    request_id: &str,
    session_id: &str,
    questions: &[crate::backend::types::QuestionInfo],
    directory: &str,
    answered: &[Option<Vec<String>>],
    done: &[bool],
    open: usize,
    drafts: &[String],
) -> Vec<serde_json::Value> {
    use crate::backend::types::FormFieldKind;

    // Defence in depth: even a caller that forgot to mark the request
    // uncollectable must never render a secret field as an input. The neutral
    // model carries the marker on the field, and this renderer is the one place
    // an answer could be typed.
    let uncollectable = uncollectable.or_else(|| {
        questions
            .iter()
            .any(|question| question.secret)
            .then_some(UncollectableReason::SecretInput)
    });
    let is_done = |i: usize| -> bool { done.get(i).copied().unwrap_or(false) };
    let selected = |i: usize| -> Option<&Vec<String>> { answered.get(i).and_then(|slot| slot.as_ref()) };
    let mut elements: Vec<serde_json::Value> = Vec::new();
    // Question text comes from the model: sanitize it for the card dialect
    // through the caller's card-wide budget (the context and the block share
    // one sanitizer, or a sixth table across them reaches Feishu).

    // The request's own title (an elicitation's message, for example) belongs
    // on every surface; the standalone card puts it in the header and passes
    // an empty title here, while an inline block renders it here.
    if !title.is_empty() {
        elements.push(json!({
            "tag": "markdown",
            "content": md.clean(&format!("**{}**", title)),
        }));
    }
    if let Some(reason) = uncollectable {
        elements.push(json!({
            "tag": "markdown",
            "content": md.clean(uncollectable_note(reason)),
        }));
    }

    if uncollectable.is_none() {
        // Issue #917: the Feishu Android client scrolls the message's bottom to
        // the keyboard's top when a card input takes focus, so an input with
        // more than about half a screen of content BELOW it is pushed off the
        // top of the screen — unreadable, and the typed text stays invisible.
        // Exactly one question is open at a time: it renders its controls, and
        // every other unanswered one folds into a collapsible panel, whose
        // folded cost is its header line (measured: folded content does not
        // push the input away). A single-question ask — the common case — is
        // unchanged.
        //
        // One root-level form owns the whole block (issue #917 part 3), so a
        // switch between questions is a form SUBMIT: the text typed into the
        // open question's box rides along in `form_value` and becomes that
        // question's draft, instead of being wiped by the repaint that opens
        // another one. Feishu rejects a `form` nested in a `collapsible_panel`
        // (ErrCode 200621), so the panel cannot own the switch: the form wraps
        // the panels (the legal direction) and the switch button—a submit—lives
        // inside the folded panel's body.
        let current = open_index(questions.len(), done, open);
        let mut form: Vec<serde_json::Value> = Vec::new();
        for (i, q) in questions.iter().enumerate() {
            let multi = q.is_multi();
            let finalized = is_done(i);
            let block_start = form.len();

            // Per-question heading: number + text + status, option bullets while
            // the question is still open (so the full option list reads together
            // with the controls right below), and the running selection.
            let mut heading = String::new();
            if finalized {
                // The finished block keeps the full question text (falling back to
                // the short title when the field has no description).
                let label = if q.question.is_empty() {
                    q.title()
                } else {
                    q.question.as_str()
                };
                heading.push_str(&format!("✅ **{}. {}**", i + 1, label));
            } else {
                heading.push_str(&format!(
                    "**{}. {}{}**",
                    i + 1,
                    q.title(),
                    if multi { "（可多选）" } else { "" }
                ));
                if !q.header.is_empty() && q.header != q.question {
                    heading.push_str(&format!("\n{}", q.question));
                }
                for opt in &q.options {
                    if opt.description.is_empty() {
                        heading.push_str(&format!("\n- {}", opt.label));
                    } else {
                        heading.push_str(&format!("\n- {} ({})", opt.label, opt.description));
                    }
                }
                if let Some(url) = &q.url {
                    heading.push_str(&format!("\n🔗 {url}"));
                }
            }
            if let Some(values) = selected(i).filter(|values| !values.is_empty()) {
                heading.push_str(&format!("\n👉 已选：{}", q.display_values(values).join("、")));
            }
            let heading = md.clean(&heading);
            form.push(json!({ "tag": "markdown", "content": heading }));

            // Only the open question renders a typed box: it is the card's one
            // input (ADR-0093), and a single input keeps `form_value` — and so
            // the switch's draft — unambiguous.
            let is_open = Some(i) == current;
            if finalized {
                // Answered / confirmed: no controls, just the 已选 line above.
            } else {
                match q.kind {
                    // An external link is not answerable; acknowledging it is the
                    // only valid answer (the form schema requires the `true`).
                    FormFieldKind::External => {
                        let mut value = question_value(request_id, session_id, directory, i, "answer");
                        value["answer"] = json!("true");
                        form.push(json!({
                            "tag": "button",
                            "text": { "tag": "plain_text", "content": "✅ 我已了解" },
                            "type": "primary",
                            "value": value,
                        }));
                    }
                    // A boolean finalizes on one click, like a single-select: the
                    // answer value is the JSON boolean the form schema expects.
                    FormFieldKind::Boolean => {
                        for (label, answer, button_type) in
                            [("✅ 是", "true", "primary"), ("否", "false", "default")]
                        {
                            let mut value = question_value(request_id, session_id, directory, i, "answer");
                            value["answer"] = json!(answer);
                            form.push(json!({
                                "tag": "button",
                                "text": { "tag": "plain_text", "content": label },
                                "type": button_type,
                                "value": value,
                            }));
                        }
                    }
                    // A numeric field has no options: a typed input plus its
                    // submit is its only control, and the text is parsed into the
                    // field's number at reply time.
                    FormFieldKind::Number | FormFieldKind::Integer => {
                        if is_open {
                            form.extend(number_answer_elements(
                                request_id,
                                session_id,
                                directory,
                                i,
                                draft_of(drafts, i),
                            ));
                        }
                    }
                    // String / Multiselect: an option picker when the field
                    // declares options (the question tool always does) plus the
                    // custom-answer input; a String with none is just the input.
                    FormFieldKind::String | FormFieldKind::Multiselect => {
                        if !multi && q.options.len() > MAX_VISIBLE_OPTIONS {
                            let options: Vec<serde_json::Value> = q
                                .options
                                .iter()
                                .map(|opt| {
                                    json!({
                                        "text": { "tag": "plain_text", "content": opt.label },
                                        "value": format!("{}|{}", i, opt.answer_value()),
                                    })
                                })
                                .collect();
                            form.push(json!({
                                "tag": "overflow",
                                "width": "fill",
                                "options": options,
                                "value": question_value(request_id, session_id, directory, i, "answer"),
                            }));
                        } else {
                            for opt in &q.options {
                                // Multi-select buttons show their selected state so
                                // the user can see what's already picked while
                                // still toggling. Selection matches the submitted
                                // VALUE; the label is display only.
                                let is_selected = multi
                                    && selected(i).is_some_and(|values| {
                                        values.iter().any(|value| value == opt.answer_value())
                                    });
                                let mut value =
                                    question_value(request_id, session_id, directory, i, "answer");
                                value["answer"] = json!(opt.answer_value());
                                form.push(json!({
                                    "tag": "button",
                                    "text": {
                                        "tag": "plain_text",
                                        "content": if is_selected {
                                            format!("✅ {}", opt.label)
                                        } else {
                                            opt.label.clone()
                                        },
                                    },
                                    "type": if is_selected { "primary" } else { "default" },
                                    "value": value,
                                }));
                            }
                        }

                        // Custom Answers: user-typed entries in the selection,
                        // rendered as selected buttons exactly like a picked option
                        // — a click removes the entry through the same toggle path
                        // (`reply: "answer"`). The button label is display-only
                        // (whitespace collapsed, long text truncated); the value
                        // carries the raw answer verbatim.
                        if multi && let Some(values) = selected(i) {
                            for value in values {
                                if q.options.iter().any(|opt| opt.answer_value() == value) {
                                    continue;
                                }
                                let mut payload =
                                    question_value(request_id, session_id, directory, i, "answer");
                                payload["answer"] = json!(value);
                                form.push(json!({
                                    "tag": "button",
                                    "text": {
                                        "tag": "plain_text",
                                        "content": format!("✅ {}", custom_answer_label(value)),
                                    },
                                    "type": "primary",
                                    "value": payload,
                                }));
                            }
                        }

                        // Free-text answer (V1 questions allow custom by default).
                        // The typed text arrives in `action.form_value`; ws.rs
                        // injects it into the answer payload. Single-select submits
                        // a replacement (`reply: "answer"`); a multi-select ADDS
                        // the typed label to the toggled set (`reply: "custom"`),
                        // so the two never collide. A String field with no options
                        // always renders it — otherwise the field has no control.
                        if is_open && renders_typed_input(q) {
                            form.extend(custom_answer_elements(
                                request_id,
                                session_id,
                                directory,
                                i,
                                multi,
                                draft_of(drafts, i),
                            ));
                        }

                        // Multi-select: the commit action sits right under its own
                        // options instead of a far-away bottom submit. Clicking
                        // locks the toggled set (empty allowed — "不选") and
                        // collapses the question to 已选.
                        if multi {
                            form.push(json!({
                                "tag": "button",
                                "text": { "tag": "plain_text", "content": "✅ 确定该题" },
                                "type": "primary",
                                "value": question_value(request_id, session_id, directory, i, "confirm"),
                            }));
                        }
                    }
                }
            }

            // Fold every unanswered question but the open one (issue #917).
            // The block built above is exactly what the expanded question shows,
            // except the typed box, which only the open question renders. The
            // panel's body carries the switch: a form SUBMIT inside the panel
            // (legal — the panel, not the form, may be the nested one), so the
            // open question's typed text travels with it.
            let mut block: Vec<serde_json::Value> = form.split_off(block_start);
            if !finalized && !is_open {
                // A fold only exists while some question is open, so the
                // resolved open question is the box this switch rides.
                block.push(switch_button(request_id, session_id, i, current.unwrap_or(open)));
                form.push(super::shell::collapsible_panel_elements(
                    &format!("{}. {}", i + 1, question_label(q, 40)),
                    std::mem::take(&mut block),
                    Some(&format!("question_{i}")),
                ));
            } else {
                form.append(&mut block);
            }

            // A divider separates question blocks so options never mix visually.
            if i + 1 < questions.len() {
                form.push(json!({ "tag": "hr" }));
            }
        }

        // Submit appears only when something is picked while questions remain
        // open ("跳过剩余"). A multi-select question is finalized by its own
        // 确定该题 button, so there is no separate bottom submit for it.
        let answered_count = done.iter().filter(|d| **d).count();
        if answered_count > 0 && answered_count < questions.len() {
            form.push(json!({
                "tag": "button",
                "text": { "tag": "plain_text", "content": "✅ 提交（跳过剩余）" },
                "type": "primary",
                "value": {
                    "action": "question",
                    "reply": "submit",
                    "request_id": request_id,
                    "session_id": session_id,
                    "directory": directory,
                },
            }));
        }
        form.push(question_reject_button(
            decline_label,
            request_id,
            session_id,
            directory,
        ));
        elements.push(json!({
            "tag": "form",
            "name": format!("form_{request_id}"),
            "elements": form,
        }));
    } else {
        // Nothing to collect: the reason's copy above and the decline control.
        // No form — there is no field an answer could be typed into.
        elements.push(question_reject_button(
            decline_label,
            request_id,
            session_id,
            directory,
        ));
    }

    elements
}

/// The question the card renders open: `chosen` while it is still unanswered,
/// otherwise the first unanswered one; `None` when every question is finalized.
/// The renderer and the typed-answer freeze read this ONE predicate, so they
/// cannot disagree about which question is open (issue #917).
pub(crate) fn open_index(n: usize, done: &[bool], chosen: usize) -> Option<usize> {
    let is_done = |i: usize| done.get(i).copied().unwrap_or(false);
    if chosen < n && !is_done(chosen) {
        return Some(chosen);
    }
    (0..n).find(|i| !is_done(*i))
}

/// The unsubmitted text recorded for question `index`, or an empty seed.
fn draft_of(drafts: &[String], index: usize) -> &str {
    drafts.get(index).map(String::as_str).unwrap_or("")
}

/// The switch control a folded question carries: a form SUBMIT that names the
/// question to open. It rides the button `name` ("open|req|ses|qi|from") because
/// a form submit does not deliver the button `value`; `from` is the question
/// whose box the switch rode, which the guard needs even when an empty box
/// delivers no `form_value`. The directory stays out, exactly like
/// `submit_name` (Feishu caps `name` at 100 chars).
fn switch_button(request_id: &str, session_id: &str, index: usize, from: usize) -> serde_json::Value {
    json!({
        "tag": "button",
        "text": { "tag": "plain_text", "content": "▸ 作答这道题" },
        "type": "default",
        "form_action_type": "submit",
        // `from` rides the name: a submit may deliver no `form_value` at all
        // (an empty box), and the guard that drops a superseded switch needs to
        // know which box it rode. Still short of the 100-char `name` cap.
        "name": format!("open|{request_id}|{session_id}|{index}|{from}"),
        "value": {
            "action": "question",
            "reply": "open",
            "request_id": request_id,
            "session_id": session_id,
            "question_index": index,
            // Also in the value: a client that delivers `action.value` and no
            // name must still say which box the switch rode.
            "from_index": from,
        },
    })
}

/// The reject control every question block carries. One builder, so the
/// uncollectable block and an ordinary question cannot drift.
fn question_reject_button(
    decline_label: &str,
    request_id: &str,
    session_id: &str,
    directory: &str,
) -> serde_json::Value {
    json!({
        "tag": "button",
        "text": { "tag": "plain_text", "content": decline_label },
        "type": "danger",
        "value": {
            "action": "question",
            "reply": "reject",
            "request_id": request_id,
            "session_id": session_id,
            "directory": directory,
        },
    })
}

/// The routing payload every per-question control carries: which request, which
/// session (V2's session-scoped reply), the owning directory (ADR-0010) and
/// which field the click answers.
fn question_value(
    request_id: &str,
    session_id: &str,
    directory: &str,
    index: usize,
    reply: &str,
) -> serde_json::Value {
    json!({
        "action": "question",
        "reply": reply,
        "request_id": request_id,
        "session_id": session_id,
        "directory": directory,
        "question_index": index,
    })
}

/// The submit action's `name`: form submit callbacks do not always carry the
/// button `value`, so the routing payload is ALSO encoded here ("submit|req|
/// ses|qi", or "submitm|…" for a multi-select custom addition) and ws.rs
/// rebuilds the value from it when `action.value` is absent. The directory is
/// deliberately NOT in the name: Feishu caps `name` at 100 chars and
/// `submit|req|ses|qi|dir` overflows on deep paths, killing the whole card
/// update (ErrCode 11310 "name exceed the default maximum 100"). The handler
/// re-resolves the directory from the store / request flow when the fallback
/// fires.
fn submit_name(name_prefix: &str, request_id: &str, session_id: &str, index: usize) -> String {
    format!("{name_prefix}|{request_id}|{session_id}|{index}")
}

/// Whether this field renders a free-text input the Host can be typing
/// into: the custom-answer box, or a numeric field's form.
///
/// The render and the freeze that protects it (issue #917) read this ONE
/// predicate. A card that shows such an input may carry unsubmitted text the
/// client alone holds: Feishu rebuilds `body.elements` on every card update,
/// so an input comes back empty after any PATCH — a repaint while the Host
/// types eats their answer. The bridge therefore never repaints that card for
/// background progress ([`crate::bridge::turn::state::StreamAccumulator::awaits_typed_answer`]).
pub(crate) fn renders_typed_input(q: &crate::backend::types::QuestionInfo) -> bool {
    use crate::backend::types::FormFieldKind;
    match q.kind {
        // No options at all: the input is the field's only control.
        FormFieldKind::Number | FormFieldKind::Integer => true,
        // The custom-answer form: allowed by the field (V1 leaves `custom`
        // unset, which reads as allowed), or the field has no option to click.
        FormFieldKind::String | FormFieldKind::Multiselect => {
            q.custom_allowed() || (q.kind == FormFieldKind::String && q.options.is_empty())
        }
        FormFieldKind::Boolean | FormFieldKind::External => false,
    }
}

/// A custom (free-text) answer input plus its submit button, one per question.
/// Single-select submits a replacement; a multi-select ADDS the typed label to
/// the toggled set. The typed text arrives in `action.form_value` and is
/// injected by ws.rs.
///
/// Both elements belong to the card's ONE root-level form (issue #917 part 3):
/// the input and its submit are siblings there, not a form of their own. The
/// input is seeded with the question's recorded draft, so a switch between
/// questions does not lose text the Host typed.
fn custom_answer_elements(
    request_id: &str,
    session_id: &str,
    directory: &str,
    index: usize,
    multi: bool,
    draft: &str,
) -> Vec<serde_json::Value> {
    let (reply, name_prefix) = if multi {
        ("custom", "submitm")
    } else {
        ("answer", "submit")
    };
    let value = question_value(request_id, session_id, directory, index, reply);
    let input = answer_input(
        &format!("custom_{index}"),
        "✍️ 输入自定义答案",
        CUSTOM_INPUT_MAX_CHARS,
        draft,
    );
    vec![
        input,
        json!({
            "tag": "button",
            "text": { "tag": "plain_text", "content": "✍️ 自定义" },
            "type": "default",
            "form_action_type": "submit",
            "name": submit_name(name_prefix, request_id, session_id, index),
            "value": value,
        }),
    ]
}

/// One answer box, seeded with a draft when there is one. Multiline instead of
/// the default single line: the single-line input is a cramped one-row strip
/// that types poorly on both PC and mobile — the card therefore renders a
/// free-text answer as `multiline_text`. `rows:1` starts it at one line;
/// `auto_resize` grows it with the text (Feishu's docs say PC-only, but mobile
/// grows too in practice). Callbacks arrive unchanged via `action.form_value`.
///
/// A NUMBER/INTEGER field does not use this: its value is parsed as a number,
/// and a multiline box makes a trailing newline (Return) an ordinary edit, so
/// it keeps the single-line `text` input (issue #917 part 3 review).
fn answer_input(name: &str, placeholder: &str, max_length: usize, draft: &str) -> serde_json::Value {
    let mut input = json!({
        "tag": "input",
        "name": name,
        "input_type": "multiline_text",
        "rows": 1,
        "auto_resize": true,
        "max_rows": 8,
        "placeholder": { "tag": "plain_text", "content": placeholder },
        "max_length": max_length,
        "width": "fill",
    });
    if !draft.is_empty() {
        input["default_value"] = json!(draft);
    }
    input
}

/// A numeric field's answer box: single-line `text`, never `multiline_text`, so
/// Return cannot put a newline into a value that is parsed as a number. It still
/// seeds a draft, so a switch does not lose a half-typed number.
fn number_input(name: &str, draft: &str) -> serde_json::Value {
    let mut input = json!({
        "tag": "input",
        "name": name,
        "input_type": "text",
        "placeholder": { "tag": "plain_text", "content": "✍️ 输入数字" },
        "max_length": NUMERIC_INPUT_MAX_CHARS,
        "width": "fill",
    });
    if !draft.is_empty() {
        input["default_value"] = json!(draft);
    }
    input
}

/// A numeric field's answer box plus its submit: the typed text is parsed into
/// the field's number at reply time, and a value that cannot parse is left
/// unanswered rather than posted as an invalid answer.
fn number_answer_elements(
    request_id: &str,
    session_id: &str,
    directory: &str,
    index: usize,
    draft: &str,
) -> Vec<serde_json::Value> {
    let value = question_value(request_id, session_id, directory, index, "answer");
    vec![
        number_input(&format!("custom_{index}"), draft),
        json!({
            "tag": "button",
            "text": { "tag": "plain_text", "content": "✅ 确定" },
            "type": "primary",
            "form_action_type": "submit",
            "name": submit_name("submit", request_id, session_id, index),
            "value": value,
        }),
    ]
}

/// Display label for a Custom Answer button: whitespace (newlines included)
/// collapses to single spaces and long text truncates with an ellipsis. The
/// stored answer and the callback value keep the raw text.
fn custom_answer_label(raw: &str) -> String {
    let collapsed = raw.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut chars = collapsed.chars();
    let head: String = chars.by_ref().take(CUSTOM_ANSWER_LABEL_CHARS).collect();
    if chars.next().is_some() {
        format!("{head}…")
    } else {
        head
    }
}

/// Build the interactive question card (JSON 2.0): a header plus the question
/// body elements from `question_elements`. V2 forms carry their own title; V1
/// questions have none and get the generic header.
#[allow(clippy::too_many_arguments)] // routing, fields, and the paint it renders
pub fn build_question_card(
    title: &str,
    request_id: &str,
    session_id: &str,
    questions: &[crate::backend::types::QuestionInfo],
    directory: &str,
    answered: &[Option<Vec<String>>],
    done: &[bool],
    open: usize,
    drafts: &[String],
) -> serde_json::Value {
    let elements = question_elements(
        "", None, request_id, session_id, questions, directory, answered, done, open, drafts,
    );
    let header = if title.is_empty() {
        "❓ AI 想问你".to_string()
    } else {
        format!("❓ {title}")
    };
    card_shell(&header, "blue", elements)
}

/// The header of a directory-owned Location Form card (ADR-0103).
pub(crate) const LOCATION_FORM_HEADER: &str = "📍 目录级 MCP 请求";

/// The shared-connection caveat every Location Form card carries: the wire has
/// no caller marker, so cola cannot tell a caller-attributed form from a
/// no-caller one and must put the wider consent scope on every card.
pub(crate) const LOCATION_FORM_SHARED_CAVEAT: &str =
    "⚠️ 这个回答会代表整个目录提交；若多个目录共用同一条 MCP 连接，也可能一并提交其他目录的同类请求。";

/// The context block of a Location Form: the asking server, its message and
/// the directory the answer commits.
fn location_form_context(
    req: &crate::backend::types::QuestionRequest,
    directory: &str,
    md: &mut super::sanitize::CardMarkdown,
) -> String {
    let mut text = format!("**📍 目录**：`{directory}`");
    if let Some(server) = req.server.as_deref().filter(|s| !s.is_empty()) {
        text.push_str(&format!("\n**🔌 MCP 服务器**：{server}"));
    }
    if let Some(message) = req.message.as_deref().filter(|m| !m.is_empty()) {
        text.push_str(&format!("\n\n{message}"));
    }
    text.push_str(&format!("\n\n{LOCATION_FORM_SHARED_CAVEAT}"));
    md.clean(&text)
}

/// The 「稍后」 control: withdraws the card without declining the request
/// (ADR-0103). It carries only routing — no session, because a Location Form
/// has none.
fn location_defer_button(request_id: &str, directory: &str) -> serde_json::Value {
    json!({
        "tag": "button",
        "text": { "tag": "plain_text", "content": "💤 稍后" },
        "type": "default",
        "value": {
            "action": "question",
            "reply": "defer",
            "request_id": request_id,
            "session_id": "",
            "directory": directory,
            "location_form": true,
        },
    })
}

/// Stamp `location_form: true` onto every callback value the card carries, so
/// a plain click routes as a Location Form even before the flow reads its
/// remembered owner (a form submit whose routing is rebuilt from the button
/// name has no value and falls back to that owner).
fn mark_location_controls(value: &mut serde_json::Value) {
    match value {
        serde_json::Value::Object(map) => {
            if map.get("action").and_then(|v| v.as_str()) == Some("question") {
                map.insert("location_form".into(), serde_json::Value::Bool(true));
            }
            for child in map.values_mut() {
                mark_location_controls(child);
            }
        }
        serde_json::Value::Array(items) => {
            for child in items {
                mark_location_controls(child);
            }
        }
        _ => {}
    }
}

/// Build the standalone card for a **Location Form** (ADR-0103): the asking
/// server, its message, the committed directory and the shared-connection
/// caveat, then the ordinary typed fields (or the uncollectable reason), the
/// explicit decline, and 「稍后」. A typed input always stays at the card's end
/// (the `card-inputs` rule), so the defer control is inserted before the form.
#[allow(clippy::too_many_arguments)] // routing, fields, and the paint it renders
pub fn build_location_form_card(
    req: &crate::backend::types::QuestionRequest,
    directory: &str,
    answered: &[Option<Vec<String>>],
    done: &[bool],
    open: usize,
    drafts: &[String],
) -> serde_json::Value {
    // One card build owns one table budget: the context and the question body
    // must share this sanitizer, or five tables across them (six in total)
    // reach Feishu and the card is rejected on every retry.
    let mut md = super::sanitize::CardMarkdown::new();
    let mut elements = vec![json!({
        "tag": "markdown",
        "content": location_form_context(req, directory, &mut md),
    })];
    // The renderer enforces the secret refusal independently of the adapter's
    // request-level marker; the Location card's controls must read the same
    // decision, or a defensive caller's field-only secret would still get
    // 「稍后」.
    let uncollectable = req.uncollectable.or_else(|| {
        req.questions
            .iter()
            .any(|question| question.secret)
            .then_some(UncollectableReason::SecretInput)
    });
    let mut body = question_elements_with(
        &mut md,
        "🚫 取消",
        "",
        uncollectable,
        &req.id,
        "",
        &req.questions,
        directory,
        answered,
        done,
        open,
        drafts,
    );
    // A form cola cannot collect at all (a secret field) is decline-only: the
    // ADR-0103 rule is that the only decision is an explicit 「取消」, so it
    // never offers 「稍后」 (a withdrawal would leave an unanswerable form with
    // no way back except `/mcp`).
    if uncollectable.is_none() {
        let defer = location_defer_button(&req.id, directory);
        match body
            .iter()
            .position(|element| element.get("tag").and_then(|tag| tag.as_str()) == Some("form"))
        {
            Some(form_at) => body.insert(form_at, defer),
            None => body.push(defer),
        }
    }
    elements.extend(body);
    let header = if uncollectable.is_some() {
        UNCOLLECTABLE_HEADER.to_string()
    } else if req.title.is_empty() {
        LOCATION_FORM_HEADER.to_string()
    } else {
        format!("📍 {}", req.title)
    };
    let mut card = card_shell(&header, "orange", elements);
    mark_location_controls(&mut card);
    card
}

/// The one-line summary a stale Location Form card carries: the asking server
/// and message (or the form's title), so the "handled elsewhere" card still
/// says what the form was about.
pub fn location_form_summary(req: &crate::backend::types::QuestionRequest) -> String {
    super::sanitize::CardMarkdown::new().clean(&location_form_summary_raw(req))
}

/// [`location_form_summary`] before markdown hygiene. **The result is raw
/// transcript text and must be cleaned before it reaches a card** — a caller
/// building a multi-element card threads one [`super::sanitize::CardMarkdown`]
/// and cleans this itself, so the card's one table budget is not spent by a
/// per-summary instance. The `_raw` suffix is the contract warning.
pub(crate) fn location_form_summary_raw(req: &crate::backend::types::QuestionRequest) -> String {
    let mut text = String::new();
    if let Some(server) = req.server.as_deref().filter(|s| !s.is_empty()) {
        text.push_str(&format!("{server}："));
    }
    let what = req
        .message
        .as_deref()
        .filter(|m| !m.is_empty())
        .or_else(|| Some(req.title.as_str()).filter(|t| !t.is_empty()));
    match what {
        Some(what) => text.push_str(what),
        None => text.push_str(&question_summary_text(&req.questions)),
    }
    text
}

/// The card a Location Form's 「稍后」 click leaves in place of the withdrawn
/// one: the request is still pending, no decision was taken.
pub fn build_location_form_deferred_card(server: Option<&str>, message: Option<&str>) -> serde_json::Value {
    let mut detail = String::new();
    if let Some(server) = server.filter(|s| !s.is_empty()) {
        detail.push_str(&format!("**🔌 MCP 服务器**：{server}\n\n"));
    }
    if let Some(message) = message.filter(|m| !m.is_empty()) {
        detail.push_str(message);
        detail.push_str("\n\n");
    }
    detail.push_str("该请求仍然待处理；稍后可在 `/mcp` 中重新打开，再作答即可。");
    card_shell(
        "💤 已稍后处理",
        "grey",
        vec![json!({
            "tag": "markdown",
            "content": super::sanitize::CardMarkdown::new().clean(&detail),
        })],
    )
}

/// The standalone card for a request cola cannot collect at all: the reason's
/// copy and the ordinary reject control, never a field the answer could be
/// typed into.
pub fn build_uncollectable_card(
    reason: UncollectableReason,
    request_id: &str,
    session_id: &str,
    directory: &str,
) -> serde_json::Value {
    let elements = question_elements(
        "",
        Some(reason),
        request_id,
        session_id,
        &[],
        directory,
        &[],
        &[],
        0,
        &[],
    );
    card_shell(UNCOLLECTABLE_HEADER, "orange", elements)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The reason card is itself a surface, so it names the undelivered
    /// companion without claiming there is no chat target; the retry note and
    /// receipt keep the shared no-target clause.
    #[test]
    fn unshowable_permission_copy_distinguishes_the_reason_from_the_no_target_clause() {
        let note = unshowable_permission_note(crate::backend::PERMISSION_TARGET_MAX_CHARS);
        assert!(
            !note.contains(NO_COMPANION_TARGET),
            "the reason card is shown on a surface: {note}"
        );
        assert!(note.contains("完整内容也未能送达"), "{note}");
        let retry = unshowable_permission_retry_note();
        assert!(retry.contains(NO_COMPANION_TARGET), "{retry}");
        let receipt = unshowable_permission_receipt();
        assert!(receipt.contains(NO_COMPANION_TARGET), "{receipt}");
    }

    #[test]
    fn question_card_has_option_buttons_with_answer_payload() {
        let questions = vec![crate::backend::types::QuestionInfo {
            question: "选择要在哪个目录继续".into(),
            header: "目录".into(),
            options: vec![
                crate::backend::types::QuestionOption {
                    label: "/a".into(),
                    description: "dir a".into(),
                    ..Default::default()
                },
                crate::backend::types::QuestionOption {
                    label: "/b".into(),
                    description: String::new(),
                    ..Default::default()
                },
            ],
            kind: crate::backend::types::FormFieldKind::String,
            custom: None,
            ..Default::default()
        }];
        let card = build_question_card(
            "",
            "que_1",
            "ses_1",
            &questions,
            "/tmp/proj/lib",
            &[None],
            &[false],
            0,
            &[],
        );
        let text = card.to_string();
        assert!(
            text.contains("选择要在哪个目录继续"),
            "question text missing: {}",
            text
        );

        // JSON 2.0: the block is ONE root-level form (issue #917 part 3), so the
        // option buttons, the open question's input and its submit are siblings
        // inside it.
        assert_eq!(card["schema"].as_str().unwrap(), "2.0");
        assert_eq!(
            card["body"]["elements"][0]["tag"], "form",
            "the whole question block is one form: {card}"
        );
        let elements = form_elements(&card);
        assert!(
            elements
                .iter()
                .any(|e| e["tag"] == "input" && e["name"] == "custom_0"),
            "the open question's box is inside the form: {card}"
        );
        let buttons: Vec<_> = elements.iter().filter(|e| e["tag"] == "button").collect();
        // one button per option + the custom submit + a reject button
        assert_eq!(buttons.len(), 4);

        let a_btn = buttons.iter().find(|b| b["text"]["content"] == "/a").unwrap();
        let value = &a_btn["value"];
        assert_eq!(value["action"], "question");
        assert_eq!(value["reply"], "answer");
        assert_eq!(value["request_id"], "que_1");
        assert_eq!(value["session_id"], "ses_1");
        assert_eq!(value["question_index"], 0);
        assert_eq!(value["answer"], "/a");

        let reject = buttons.iter().find(|b| b["value"]["reply"] == "reject").unwrap();
        assert_eq!(reject["value"]["request_id"], "que_1");
    }

    /// A single-select question with 5 options — past `MAX_VISIBLE_OPTIONS`,
    /// so its controls take the `overflow` path rather than raw buttons.
    fn many_option_question() -> crate::backend::types::QuestionInfo {
        let options = (0..5)
            .map(|i| crate::backend::types::QuestionOption {
                label: format!("/a{}", i),
                description: String::new(),
                ..Default::default()
            })
            .collect();
        crate::backend::types::QuestionInfo {
            question: "选一个目录".into(),
            header: "目录".into(),
            options,
            kind: crate::backend::types::FormFieldKind::String,
            custom: None,
            ..Default::default()
        }
    }

    #[test]
    fn question_card_many_options_collapse_into_overflow() {
        let card = build_question_card(
            "",
            "que_1",
            "ses_1",
            &[many_option_question()],
            "/tmp/proj/lib",
            &[None],
            &[false],
            0,
            &[],
        );
        let elements = form_elements(&card);
        let overflow = elements
            .iter()
            .find(|e| e["tag"] == "overflow")
            .expect("many options collapse into an overflow");
        let options = overflow["options"].as_array().unwrap();
        assert_eq!(options.len(), 5);
        // Each option encodes "qi|label"; ws.rs decodes it back to an answer.
        assert_eq!(options[0]["value"].as_str().unwrap(), "0|/a0");
        assert_eq!(options[4]["value"].as_str().unwrap(), "0|/a4");
        // The overflow carries the routing payload for the reply.
        assert_eq!(overflow["value"]["request_id"], "que_1");
        assert_eq!(overflow["value"]["reply"], "answer");
        // No raw option buttons for the collapsed question: only its custom
        // submit and the reject remain.
        let buttons: Vec<_> = elements.iter().filter(|e| e["tag"] == "button").collect();
        assert_eq!(buttons.len(), 2, "custom submit + reject: {card}");
        let reject = buttons
            .iter()
            .find(|b| b["value"]["reply"] == "reject")
            .expect("reject button stays");
        assert_eq!(reject["value"]["request_id"], "que_1");
    }

    #[test]
    fn question_card_overflow_path_keeps_custom_answer_form() {
        let card = build_question_card(
            "",
            "que_1",
            "ses_1",
            &[many_option_question()],
            "/tmp/proj/lib",
            &[None],
            &[false],
            0,
            &[],
        );
        let form = &card["body"]["elements"][0];
        assert_eq!(form["tag"], "form", "one root-level form: {card}");
        assert_eq!(form["name"], "form_que_1");
        assert!(form.to_string().contains("form_action_type"));
        assert!(
            form.to_string().contains("\"tag\":\"input\""),
            "the overflow question still gets its answer box: {card}"
        );
    }

    #[test]
    fn question_card_overflow_custom_disabled_has_no_form() {
        let mut q = many_option_question();
        q.custom = Some(false);
        let card = build_question_card(
            "",
            "que_1",
            "ses_1",
            &[q],
            "/tmp/proj/lib",
            &[None],
            &[false],
            0,
            &[],
        );
        let elements = form_elements(&card);
        assert!(
            elements.iter().any(|e| e["tag"] == "overflow"),
            "overflow picker still rendered"
        );
        assert!(
            elements.iter().all(|e| e["tag"] != "input"),
            "custom-disabled overflow question must not get an input: {}",
            card
        );
    }

    /// Issue #917: the predicate that RENDERS the free-text input and the one
    /// that freezes the card's clock are the same, so a card cannot carry an
    /// unprotected input (or be frozen with no input to protect).
    #[test]
    fn renders_typed_input_matches_the_fields_that_render_a_form() {
        use crate::backend::types::{FormFieldKind, QuestionInfo, QuestionOption};
        let q = |kind, options: usize, custom: Option<bool>| QuestionInfo {
            question: "q".into(),
            header: "h".into(),
            options: (0..options)
                .map(|i| QuestionOption {
                    label: format!("/a{i}"),
                    ..Default::default()
                })
                .collect(),
            kind,
            custom,
            ..Default::default()
        };
        assert!(
            renders_typed_input(&q(FormFieldKind::String, 0, None)),
            "an optionless String renders only the input"
        );
        assert!(
            renders_typed_input(&q(FormFieldKind::String, 2, Some(true))),
            "custom allowed renders the input beside options"
        );
        assert!(renders_typed_input(&q(FormFieldKind::Multiselect, 2, Some(true))));
        assert!(
            !renders_typed_input(&q(FormFieldKind::String, 2, Some(false))),
            "custom disabled: options only"
        );
        assert!(!renders_typed_input(&q(
            FormFieldKind::Multiselect,
            2,
            Some(false)
        )));
        assert!(
            !renders_typed_input(&q(FormFieldKind::Boolean, 0, None)),
            "yes/no buttons, no input"
        );
        assert!(!renders_typed_input(&q(FormFieldKind::External, 0, None)));
        assert!(renders_typed_input(&q(FormFieldKind::Number, 0, None)));
        assert!(
            renders_typed_input(&q(FormFieldKind::Integer, 2, Some(false))),
            "a numeric field is its form whatever its options"
        );
    }

    #[test]
    fn question_card_custom_answer_form_added_when_custom_allowed() {
        let questions = vec![crate::backend::types::QuestionInfo {
            question: "q".into(),
            header: "h".into(),
            options: vec![crate::backend::types::QuestionOption {
                label: "/a".into(),
                description: String::new(),
                ..Default::default()
            }],
            kind: crate::backend::types::FormFieldKind::String,
            custom: None, // default: custom answers allowed,
            ..Default::default()
        }];
        let card = build_question_card(
            "",
            "que_1",
            "ses_1",
            &questions,
            "/tmp/proj/lib",
            &[None],
            &[false],
            0,
            &[],
        );
        let form = &card["body"]["elements"][0];
        assert_eq!(form["tag"], "form", "one root-level form: {card}");
        assert_eq!(form["name"], "form_que_1");
        let input = form_elements(&card)
            .iter()
            .find(|e| e["tag"] == "input")
            .expect("custom-allowed question gets an input inside the form")
            .clone();
        // The custom-answer box is a multiline textarea (better typing on both
        // PC and mobile than the default single-line input). rows:1 starts it
        // compact; auto_resize grows it with the text.
        assert_eq!(input["input_type"], "multiline_text");
        assert_eq!(input["rows"], 1);
        assert_eq!(input["auto_resize"], true);
        assert!(
            form_elements(&card)
                .iter()
                .any(|e| e["form_action_type"] == "submit"),
            "the box has its submit beside it: {card}"
        );
    }

    #[test]
    fn question_card_custom_disabled_has_no_input_form() {
        let questions = vec![crate::backend::types::QuestionInfo {
            question: "q".into(),
            header: "h".into(),
            options: vec![crate::backend::types::QuestionOption {
                label: "/a".into(),
                description: String::new(),
                ..Default::default()
            }],
            kind: crate::backend::types::FormFieldKind::String,
            custom: Some(false),
            ..Default::default()
        }];
        let card = build_question_card(
            "",
            "que_1",
            "ses_1",
            &questions,
            "/tmp/proj/lib",
            &[None],
            &[false],
            0,
            &[],
        );
        let elements = form_elements(&card);
        assert!(
            elements.iter().all(|e| e["tag"] != "input"),
            "custom-disabled question must not get an input: {}",
            card
        );
    }

    #[test]
    fn multi_select_question_gets_confirm_button_and_custom_form() {
        let questions = vec![crate::backend::types::QuestionInfo {
            question: "选择水果".into(),
            header: "水果".into(),
            options: vec![crate::backend::types::QuestionOption {
                label: "苹果".into(),
                description: String::new(),
                ..Default::default()
            }],
            kind: crate::backend::types::FormFieldKind::Multiselect,
            custom: None,
            ..Default::default()
        }];
        let card = build_question_card(
            "",
            "que_1",
            "ses_1",
            &questions,
            "/tmp/proj/lib",
            &[None],
            &[false],
            0,
            &[],
        );
        let elements = form_elements(&card);
        // The per-question 确定该题 button commits the toggled selection.
        let confirm = elements
            .iter()
            .find(|e| e["tag"] == "button" && e["text"]["content"] == "✅ 确定该题")
            .expect("multi-select must get a 确定该题 button");
        assert_eq!(confirm["value"]["reply"], "confirm");
        assert_eq!(confirm["value"]["question_index"], 0);
        // A multi-select keeps its option toggle buttons.
        let opt = elements
            .iter()
            .find(|e| e["tag"] == "button" && e["value"]["answer"] == "苹果")
            .expect("option toggle button missing");
        assert_eq!(opt["value"]["reply"], "answer");
        // Multi-select supports a custom answer too (reply "custom" adds it):
        // the input and its submit sit in the same root form.
        assert!(
            elements
                .iter()
                .any(|e| e["tag"] == "button" && e["value"]["reply"] == "custom"),
            "multi-select custom submit must reply with custom: {card}"
        );
        assert!(
            elements
                .iter()
                .any(|e| e["tag"] == "input" && e["name"] == "custom_0"),
            "multi-select custom box missing: {card}"
        );
    }

    #[test]
    fn multi_select_custom_answers_render_as_removable_buttons() {
        let questions = vec![crate::backend::types::QuestionInfo {
            question: "选择水果".into(),
            header: "水果".into(),
            options: vec![crate::backend::types::QuestionOption {
                label: "苹果".into(),
                description: String::new(),
                ..Default::default()
            }],
            kind: crate::backend::types::FormFieldKind::Multiselect,
            custom: None,
            ..Default::default()
        }];
        // The selection holds an option and a multiline Custom Answer; the
        // custom label is not an option label, so it gets its own chip.
        let answered = vec![Some(vec!["苹果".to_string(), "自定\n答案".to_string()])];
        let card = build_question_card(
            "",
            "que_1",
            "ses_1",
            &questions,
            "/tmp/proj/lib",
            &answered,
            &[false],
            0,
            &[],
        );
        let elements = form_elements(&card);
        let chip = elements
            .iter()
            .find(|e| e["tag"] == "button" && e["value"]["answer"] == "自定\n答案")
            .expect("Custom Answer chip missing");
        // The chip re-enters as a normal toggle; the RAW text rides in `value`.
        assert_eq!(chip["value"]["reply"], "answer");
        assert_eq!(chip["value"]["question_index"], 0);
        assert_eq!(chip["type"], "primary");
        // Display label is collapsed; the value keeps the raw newline.
        assert_eq!(chip["text"]["content"], "✅ 自定 答案");
        // The option renders exactly one button — the custom walk skips it.
        assert_eq!(
            elements
                .iter()
                .filter(|e| e["tag"] == "button" && e["value"]["answer"] == "苹果")
                .count(),
            1
        );
    }

    #[test]
    fn custom_answer_button_label_collapses_and_truncates() {
        // Whitespace (newlines included) collapses to single spaces.
        assert_eq!(custom_answer_label("  a\n\nb  c "), "a b c");
        // At the cap: no ellipsis.
        let exact = "二".repeat(CUSTOM_ANSWER_LABEL_CHARS);
        assert_eq!(custom_answer_label(&exact), exact);
        // Over the cap: truncated with a single ellipsis.
        let long = "一".repeat(CUSTOM_ANSWER_LABEL_CHARS + 5);
        let label = custom_answer_label(&long);
        assert_eq!(label.chars().count(), CUSTOM_ANSWER_LABEL_CHARS + 1);
        assert!(label.ends_with('…'));
    }

    #[test]
    fn single_select_never_renders_custom_answer_buttons() {
        // Defensive: a single-select custom answer replaces and finalizes on
        // submit, so a non-option label never gets a removable chip (the chip
        // path is multi-only). Rendered with done=false to exercise the guard.
        let questions = vec![crate::backend::types::QuestionInfo {
            question: "选择目录".into(),
            header: "目录".into(),
            options: vec![crate::backend::types::QuestionOption {
                label: "/a".into(),
                description: String::new(),
                ..Default::default()
            }],
            kind: crate::backend::types::FormFieldKind::String,
            custom: None,
            ..Default::default()
        }];
        let answered = vec![Some(vec!["自定".to_string()])];
        let card = build_question_card(
            "",
            "que_1",
            "ses_1",
            &questions,
            "/tmp/proj/lib",
            &answered,
            &[false],
            0,
            &[],
        );
        let elements = form_elements(&card);
        assert!(
            elements
                .iter()
                .all(|e| !(e["tag"] == "button" && e["value"]["answer"] == "自定")),
            "single-select must not grow a Custom Answer chip: {}",
            card
        );
    }

    #[test]
    fn question_card_done_multi_select_collapses_to_selection_line() {
        let questions = vec![crate::backend::types::QuestionInfo {
            question: "选择水果".into(),
            header: "水果".into(),
            options: vec![crate::backend::types::QuestionOption {
                label: "苹果".into(),
                description: String::new(),
                ..Default::default()
            }],
            kind: crate::backend::types::FormFieldKind::Multiselect,
            custom: None,
            ..Default::default()
        }];
        // Confirmed (done) multi-select: no option buttons, no confirm, no form.
        let card = build_question_card(
            "",
            "que_1",
            "ses_1",
            &questions,
            "/tmp/proj/lib",
            &[Some(vec!["苹果".to_string()])],
            &[true],
            0,
            &[],
        );
        let text = card.to_string();
        assert!(text.contains("已选：苹果"), "selection missing: {}", text);
        assert!(
            !text.contains("确定该题"),
            "done multi-select must not show the confirm button: {}",
            text
        );
        assert!(
            !text.contains("✍️ 自定义"),
            "done multi-select must not show the custom form: {}",
            text
        );
        assert!(
            !text.contains("\"reply\":\"answer\""),
            "done multi-select must not show option toggles: {}",
            text
        );
    }

    #[test]
    fn multi_question_card_groups_by_header_with_dividers() {
        let mk = |q: &str, h: &str, multi: bool| crate::backend::types::QuestionInfo {
            question: q.into(),
            header: h.into(),
            options: vec![crate::backend::types::QuestionOption {
                label: "选项".into(),
                description: String::new(),
                ..Default::default()
            }],
            kind: if multi {
                crate::backend::types::FormFieldKind::Multiselect
            } else {
                crate::backend::types::FormFieldKind::String
            },
            custom: None,
            ..Default::default()
        };
        let questions = vec![mk("选目录", "目录", false), mk("选水果", "水果", true)];
        let card = build_question_card(
            "",
            "que_1",
            "ses_1",
            &questions,
            "/tmp/proj/lib",
            &[None, None],
            &[false, false],
            0,
            &[],
        );
        let text = card.to_string();
        // The header is used as the per-question title so blocks read distinctly.
        assert!(text.contains("**1. 目录**"), "q1 title missing: {}", text);
        assert!(text.contains("选目录"), "q1 question body missing: {}", text);
        assert!(
            text.contains("**2. 水果（可多选）**"),
            "q2 title missing: {}",
            text
        );
        assert!(text.contains("选水果"), "q2 question body missing: {}", text);
        // A divider separates the two question blocks (inside the one form).
        let hr = form_elements(&card).iter().filter(|e| e["tag"] == "hr").count();
        assert_eq!(hr, 1, "one divider between two questions: {}", card);
        // The multi-select's confirm button is grouped right under ITS own
        // options and before the reject button — no separate bottom submit for
        // a multi-select question. Question 2 is folded (issue #917), so both
        // controls live inside its panel; the panel sits before the reject.
        let elements = all_elements(&card);
        let opt1 = elements
            .iter()
            .position(|e| {
                e["tag"] == "button" && e["value"]["question_index"] == 1 && e["value"]["answer"] == "选项"
            })
            .expect("Q1 option toggle missing");
        let confirm = elements
            .iter()
            .position(|e| e["tag"] == "button" && e["text"]["content"] == "✅ 确定该题")
            .expect("confirm button missing");
        let reject = elements
            .iter()
            .position(|e| e["tag"] == "button" && e["text"]["content"] == "🚫 无法回答")
            .expect("reject button missing");
        assert!(
            opt1 < confirm && confirm < reject,
            "multi-select confirm must sit with its options, before reject: {}",
            card
        );
        assert!(
            !elements
                .iter()
                .any(|e| e["tag"] == "button" && e["text"]["content"].to_string().contains("提交")),
            "no bottom submit button when questions remain open: {}",
            card
        );
    }

    /// A one-option String question — the shape the fold tests need.
    fn string_question(n: usize) -> crate::backend::types::QuestionInfo {
        crate::backend::types::QuestionInfo {
            question: format!("问题 {n}"),
            header: format!("h{n}"),
            options: vec![crate::backend::types::QuestionOption {
                label: format!("/a{n}"),
                ..Default::default()
            }],
            kind: crate::backend::types::FormFieldKind::String,
            custom: None,
            ..Default::default()
        }
    }

    /// Every element the card renders, form and folded panel bodies included,
    /// in document order, so a test can assert on a control wherever the
    /// structure put it (issue #917: one root-level form, one open question,
    /// the rest folded).
    fn all_elements(card: &serde_json::Value) -> Vec<serde_json::Value> {
        fn walk(value: &serde_json::Value, out: &mut Vec<serde_json::Value>) {
            if let Some(items) = value.as_array() {
                for item in items {
                    walk(item, out);
                }
                return;
            }
            if value["tag"].is_string() {
                out.push(value.clone());
            }
            if let Some(inner) = value["elements"].as_array() {
                for item in inner {
                    walk(item, out);
                }
            }
        }
        let mut out = Vec::new();
        walk(&card["body"]["elements"], &mut out);
        out
    }

    /// The card's ONE root-level form's elements: the whole question block lives
    /// inside it (issue #917 part 3), so "where the controls are" is always here.
    fn form_elements(card: &serde_json::Value) -> &Vec<serde_json::Value> {
        card["body"]["elements"]
            .as_array()
            .expect("card body")
            .iter()
            .find(|e| e["tag"] == "form")
            .expect("the question block is one root-level form")["elements"]
            .as_array()
            .expect("form elements")
    }

    /// How many elements with `tag` the card JSON holds anywhere — the simplest
    /// way to say "exactly one input on this card".
    fn count_tag(value: &serde_json::Value, tag: &str) -> usize {
        match value {
            serde_json::Value::Object(map) => {
                usize::from(map.get("tag").and_then(|t| t.as_str()) == Some(tag))
                    + map.values().map(|v| count_tag(v, tag)).sum::<usize>()
            }
            serde_json::Value::Array(items) => items.iter().map(|v| count_tag(v, tag)).sum(),
            _ => 0,
        }
    }

    #[test]
    fn an_uncollectable_question_renders_its_reason_and_a_decline() {
        let elements = question_elements(
            "",
            Some(UncollectableReason::SecretInput),
            "req_1",
            "ses_1",
            &[],
            "/w",
            &[],
            &[],
            0,
            &[],
        );
        let text = serde_json::to_string(&elements).expect("elements serialize");
        assert!(text.contains("私密输入"), "the reason is shown: {text}");
        assert!(
            text.contains("重新发起"),
            "the copy says how to retry safely: {text}"
        );
        assert!(
            !elements.iter().any(|element| element["tag"] == "input"),
            "no answer can be typed: {text}"
        );
        assert!(
            elements
                .iter()
                .any(|element| element["tag"] == "button" && element["value"]["reply"] == "reject"),
            "the only control is the decline: {text}"
        );
    }

    /// A callers' field-only secret marker (no request-level uncollectable
    /// reason) must still make a Location Form decline-only: the renderer
    /// already refuses the input, so the card must not offer 「稍后」 either.
    #[test]
    fn a_field_only_secret_location_form_has_no_defer_control() {
        let request = crate::backend::types::QuestionRequest::location(
            "frm_defensive_secret",
            "登录".into(),
            vec![crate::backend::types::QuestionInfo {
                key: "token".into(),
                question: "API token".into(),
                header: "Token".into(),
                kind: crate::backend::types::FormFieldKind::String,
                secret: true,
                ..Default::default()
            }],
            Some("vault".into()),
            Some("Sign in".into()),
            None,
        );
        let card = build_location_form_card(&request, "/work", &[], &[], 0, &[]);
        let text = card.to_string();
        assert!(text.contains("私密输入"), "the secret reason is shown: {text}");
        assert!(
            !text.contains("\"reply\":\"defer\""),
            "a field-only secret is decline-only: {text}"
        );
        assert!(
            text.contains("\"reply\":\"reject\""),
            "the explicit decline remains: {text}"
        );
    }

    /// The context and the question body share ONE `CardMarkdown` budget: a
    /// sixth table across the two is fenced, so Feishu never rejects the whole
    /// card (`230099/11310`).
    #[test]
    fn a_location_form_context_and_body_share_one_table_budget() {
        fn table(n: usize) -> String {
            format!("| t{n} | b |\n|---|---|\n| 1 | 2 |")
        }
        fn collect(value: &serde_json::Value, out: &mut String) {
            match value {
                serde_json::Value::Object(map) => {
                    if map.get("tag").and_then(|tag| tag.as_str()) == Some("markdown")
                        && let Some(content) = map.get("content").and_then(|c| c.as_str())
                    {
                        out.push_str(content);
                        out.push('\n');
                    }
                    for child in map.values() {
                        collect(child, out);
                    }
                }
                serde_json::Value::Array(items) => {
                    for child in items {
                        collect(child, out);
                    }
                }
                _ => {}
            }
        }

        let request = crate::backend::types::QuestionRequest::location(
            "frm_tables",
            "Tables".into(),
            vec![crate::backend::types::QuestionInfo {
                key: "choice".into(),
                question: format!("Pick\n\n{}\n\n{}\n\n{}", table(4), table(5), table(6)),
                header: "Pick".into(),
                kind: crate::backend::types::FormFieldKind::String,
                options: vec![crate::backend::types::QuestionOption {
                    value: "yes".into(),
                    label: "Yes".into(),
                    description: String::new(),
                }],
                ..Default::default()
            }],
            Some("context7".into()),
            Some(format!("{}\n\n{}\n\n{}", table(1), table(2), table(3))),
            None,
        );
        let card = build_location_form_card(&request, "/w", &[], &[], 0, &[]);
        let mut content = String::new();
        collect(&card, &mut content);
        assert!(
            content.contains("| t5 | b |\n|---|---|"),
            "the fifth table renders natively: {content}"
        );
        assert!(
            content.contains("```\n| t6 | b |\n|---|---|\n| 1 | 2 |\n```"),
            "the sixth table (across context and body) is fenced: {content}"
        );
    }

    #[test]
    fn a_question_title_renders_in_the_block_for_inline_surfaces() {
        let elements = question_elements(
            "把文件写入工作区？",
            None,
            "req_1",
            "ses_1",
            &[],
            "/w",
            &[],
            &[],
            0,
            &[],
        );
        let text = serde_json::to_string(&elements).expect("elements serialize");
        assert!(
            text.contains("把文件写入工作区？"),
            "an elicitation's message renders inline: {text}"
        );
    }

    #[test]
    fn a_standalone_uncollectable_card_uses_the_copy_table() {
        let card = build_uncollectable_card(UncollectableReason::SecretInput, "req_1", "ses_1", "/w");
        assert_eq!(
            card["header"]["title"]["content"].as_str().unwrap(),
            UNCOLLECTABLE_HEADER
        );
        let text = card.to_string();
        assert!(text.contains("私密输入"), "{text}");
        assert!(!text.contains("\"tag\":\"input\""), "{text}");
    }

    #[test]
    fn permission_card_carries_reply_payload() {
        let card = build_permission_card(
            "ses_abc",
            "per_123",
            "AI 想要执行 bash",
            "/tmp/proj/lib",
            true,
            None,
        );
        assert_eq!(
            card["header"]["title"]["content"].as_str().unwrap(),
            "🔐 权限请求"
        );
        // JSON 2.0: body.elements hold the markdown + buttons directly (no v1
        // action row).
        assert_eq!(card["schema"].as_str().unwrap(), "2.0");
        let elements = card["body"]["elements"].as_array().unwrap();
        // element 0 = description markdown, rest = buttons
        assert!(elements[0]["content"].as_str().unwrap().contains("AI 想要执行"));
        let buttons: Vec<_> = elements.iter().filter(|e| e["tag"] == "button").collect();
        assert_eq!(buttons.len(), 4);
        let values: Vec<_> = buttons.iter().map(|a| &a["value"]).collect();
        // Each button must carry the reply + request_id + session_id so the
        // card callback can route the answer back.
        let once = values.iter().find(|v| v["reply"] == "once").unwrap();
        assert_eq!(once["request_id"], "per_123");
        assert_eq!(once["session_id"], "ses_abc");
        assert_eq!(once["perm_label"], "✅ 已允许一次");
        assert_eq!(once["perm_color"], "green");
        // The body is rendered once as the block element; a callback carries
        // routing and the display labels only, never the body.
        assert!(
            once.get("perm_body").is_none(),
            "the button must not echo the body: {once}"
        );
        assert_eq!(once["directory"], "/tmp/proj/lib");
        let reject = values.iter().find(|v| v["reply"] == "reject").unwrap();
        assert_eq!(reject["perm_color"], "red");
        let always = values.iter().find(|v| v["reply"] == "always").unwrap();
        assert!(always["perm_label"].as_str().unwrap().contains("始终允许"));
        // The session-scoped decision says what it means, beside the button.
        assert!(
            elements
                .iter()
                .any(|element| element["content"] == SESSION_ALWAYS_NOTE),
            "the session scope copy is on the card"
        );
        // The Auto-Accept toggle carries the session id so it can flip the flag.
        let autoaccept = values.iter().find(|v| v["reply"] == "autoaccept").unwrap();
        assert_eq!(autoaccept["session_id"], "ses_abc");
        assert_eq!(autoaccept["request_id"], "per_123");
    }

    /// A request whose backend did not advertise a session-scoped decision
    /// offers neither the button nor its explanation — no silent
    /// approximation of a session-scoped decision the wire cannot carry.
    #[test]
    fn permission_card_hides_the_session_decision_when_not_advertised() {
        let card = build_permission_card(
            "ses_abc",
            "per_123",
            "AI 想要申请额外权限",
            "/tmp/proj/lib",
            false,
            None,
        );
        let elements = card["body"]["elements"].as_array().unwrap();
        let buttons: Vec<_> = elements.iter().filter(|e| e["tag"] == "button").collect();
        assert_eq!(buttons.len(), 3);
        assert!(
            !buttons.iter().any(|button| button["value"]["reply"] == "always"),
            "no session-scoped button: {card}"
        );
        assert!(
            !elements
                .iter()
                .any(|element| element["content"] == SESSION_ALWAYS_NOTE),
            "no scope copy without the decision: {card}"
        );
    }

    /// The shared target budget is only safe if the card it produces is. This
    /// pins the worst case — a full-budget CJK command, the smallest per-element
    /// ceiling the platform measured — inside the card's own JSON budget, with
    /// the body rendered once. Raising
    /// [`crate::backend::types::PERMISSION_TARGET_MAX_CHARS`] fails here, which
    /// is the point.
    #[test]
    fn a_permission_card_at_the_target_budget_fits_the_card_budget() {
        let budget = crate::backend::types::PERMISSION_TARGET_MAX_CHARS;
        let command = "啊".repeat(budget);
        let body =
            format!("⚡ **执行 Shell 命令**\n**对象**:\n- `{command}`\n\nAI 想要执行这个操作，是否允许？");
        let card = build_permission_card("ses_abc", "n:1", &body, "/tmp/proj", true, None);
        let len = card.to_string().len();
        assert!(
            len <= crate::feishu::card::MAX_CARD_JSON_CHARS,
            "a full-budget command card is {len} bytes, over the {} budget",
            crate::feishu::card::MAX_CARD_JSON_CHARS
        );
    }

    /// ADR-0102's two budgets on one page: a 3,000-character CJK target still
    /// renders inside the card's JSON budget (the worst case the target budget
    /// admits), while a 5,000-character over-budget request takes the summary
    /// form, whose card is far smaller than the full target could ever be.
    #[test]
    fn a_cjk_target_card_and_a_five_thousand_char_summary_stay_in_budget() {
        use crate::backend::types::{PERMISSION_TARGET_MAX_CHARS, PermissionRequest};
        use crate::bridge::surfaces::CompanionLane;

        let request = |command: String| PermissionRequest {
            request_id: "per_long".into(),
            session_id: Some("ses".into()),
            permission: Some("bash".into()),
            patterns: vec![command],
            metadata: None,
            always: Vec::new(),
            always_supported: true,
        };

        // 3,000 CJK characters: 3 bytes each, ~9 KB of JSON plus the card's
        // chrome. The target budget's worst case must still serialize.
        let cjk = request("啊".repeat(PERMISSION_TARGET_MAX_CHARS));
        let full_body = crate::bridge::request::kind::describe_permission(&cjk);
        let full_card = build_permission_card("ses", "n:1", &full_body, "/tmp/proj", true, None);
        assert!(
            full_card.to_string().len() <= crate::feishu::card::MAX_CARD_JSON_CHARS,
            "a full-budget CJK card is {} bytes",
            full_card.to_string().len()
        );

        // 5,000 characters is over budget, so the surface is the summary and
        // the full target rides the companion. The card carries no full target.
        let long = request("x".repeat(5_000));
        let delivery = crate::bridge::surfaces::CompanionDelivery {
            message_id: "om_post".into(),
            lane: CompanionLane::RichText,
            target_digest: 0,
        };
        let summary = crate::bridge::request::companion::describe_permission_summary(&long, &delivery);
        let summary_digest = crate::bridge::request::companion::patterns_target_digest(&long);
        let summary_card =
            build_permission_card("ses", "n:1", &summary, "/tmp/proj", true, Some(summary_digest));
        let len = summary_card.to_string().len();
        assert!(
            len <= crate::feishu::card::MAX_CARD_JSON_CHARS,
            "the summary card is {len} bytes"
        );
        assert!(!summary_card.to_string().contains(&"x".repeat(1_000)));
        assert!(summary.contains("目标长度"));
        assert!(
            summary_card.to_string().contains("\"summary\":true"),
            "a summary card's buttons carry the surface marker"
        );
        assert!(summary.contains("Rich Text Message"));
    }

    /// The JSON worst case the render invariant bounds: a body at
    /// [`crate::backend::types::PERMISSION_BODY_MAX_CHARS`] **sanitized**
    /// characters where every character is a JSON control escape (`\u0000`,
    /// six bytes). The card still serializes under its own budget. A raw `<`
    /// is far cheaper: the proof counts sanitized characters, and a target sits
    /// inside an inline code span where `<` stays one byte.
    #[test]
    fn a_permission_card_at_the_json_escape_worst_case_fits_the_card_budget() {
        let body = "\u{0}".repeat(crate::backend::types::PERMISSION_BODY_MAX_CHARS);
        let card = build_permission_card("ses_abc", "n:1", &body, "/tmp/proj", true, None);
        let len = card.to_string().len();
        assert!(
            len <= crate::feishu::card::MAX_CARD_JSON_CHARS,
            "the JSON control-escape worst case is {len} bytes, over the {} budget",
            crate::feishu::card::MAX_CARD_JSON_CHARS
        );
        assert!(
            len >= 18_000,
            "the six-byte JSON escape case is ~18 KB, got {len}"
        );
    }

    /// The block estimate the streaming card's split reserves must be an upper
    /// bound on what the block's elements actually serialize to — otherwise a
    /// near-full card would still append it past Feishu's limit. The routing
    /// triple is charged for every button, so a long directory counts too.
    #[test]
    fn the_permission_block_estimate_covers_the_rendered_block() {
        let long_directory = format!("/work/{}", "a-project-directory/".repeat(12));
        // A 150-character `<` reason: `CardMarkdown::clean` expands each `<` to
        // `&#60;` (5 bytes), so a raw-length estimate under-counts the element
        // by 600 bytes — the case the cleaning step in the estimate exists for.
        let angle_reason = format!(
            "⚡ **执行 Shell 命令**\n**原因**: {}\n**对象**:\n- `ls`\n\nAI 想要执行这个操作，是否允许？",
            "<".repeat(150)
        );
        for (body, always, directory, digest) in [
            ("⚡ **执行 Shell 命令**\n**对象**:\n- `ls`\n", true, "/d", None),
            (
                "⚡ **执行 Shell 命令**\n**对象**:\n- `cargo test`\n",
                false,
                "/d",
                None,
            ),
            (&"啊\"\\\n".repeat(250), true, "/d", None),
            (
                "⚡ **执行 Shell 命令**\n**对象**:\n- `ls`\n",
                true,
                long_directory.as_str(),
                None,
            ),
            (angle_reason.as_str(), true, "/d", None),
            // A summary block's buttons carry the surface marker and digest;
            // the estimate must still cover the rendered JSON.
            (
                "⚡ **执行 Shell 命令**\n**对象**:\n- `ls`\n",
                true,
                "/d",
                Some(42),
            ),
            (
                "⚡ **执行 Shell 命令**\n**对象**:\n- `ls`\n",
                true,
                long_directory.as_str(),
                Some(42),
            ),
        ] {
            let card = build_permission_card("ses", "n:1", body, directory, always, digest);
            let elements = card["body"]["elements"].as_array().expect("body elements");
            let rendered: usize = elements.iter().map(|element| element.to_string().len()).sum();
            let estimate = permission_block_estimate(body, always, digest.is_some(), "n:1", "ses", directory);
            assert!(
                estimate >= rendered,
                "estimate {estimate} under the block's {rendered} bytes (directory {directory:?})"
            );
            assert_eq!(
                permission_block_components(always),
                elements.len(),
                "the component count is exactly what the block renders"
            );
        }
    }

    /// A summary block's extra callback keys are charged explicitly, so the
    /// split estimate follows the rendered buttons instead of relying on the
    /// fixed per-button slack.
    #[test]
    fn a_summary_block_estimate_charges_its_callback_keys() {
        let plain = permission_block_estimate("body", true, false, "n:1", "ses", "/d");
        let summary = permission_block_estimate("body", true, true, "n:1", "ses", "/d");
        let per_button = json_escaped_len("\"summary\":true,\"target_digest\":\"0123456789abcdef\",");
        assert_eq!(
            summary - plain,
            permission_button_count(true) * per_button,
            "every summary button charges the marker and digest"
        );
    }

    /// Whole-branch re-review #883: the estimate must charge the **rendered**
    /// body. `CardMarkdown` expands `<` to `&#60;`, so a reason full of `<` is
    /// five times its authored length; charging the authored bytes leaves 600
    /// bytes per copy unaccounted, which the rendered card disproves.
    #[test]
    fn an_angle_bracket_reason_is_charged_at_its_sanitized_size() {
        let body = format!("**原因**: {}", "<".repeat(150));
        let cleaned = crate::feishu::card::sanitize::CardMarkdown::new().element(&body);
        assert_eq!(
            cleaned.len(),
            body.len() - 150 + 150 * "&#60;".len(),
            "the sanitizer expands every '<'"
        );
        let card = build_permission_card("ses", "n:1", &body, "/d", true, None);
        let elements = card["body"]["elements"].as_array().expect("body elements");
        let rendered: usize = elements.iter().map(|element| element.to_string().len()).sum();
        let estimate = permission_block_estimate(&body, true, false, "n:1", "ses", "/d");
        assert!(
            estimate >= rendered,
            "estimate {estimate} under the rendered block's {rendered} bytes"
        );
        // The authored bytes alone would have been 4 bytes short per '<' on
        // the element, so the estimate must charge the sanitized size.
        assert!(
            json_escaped_len(&cleaned) - json_escaped_len(&body) >= 600,
            "the expansion the raw charge would miss"
        );
    }

    /// Whole-branch re-review #883: the content-rejection fallback renders the
    /// body inside a fence sized to its longest backtick run, so a
    /// backtick-heavy command adds up to twice the run around the element. The
    /// estimate must charge that fence, or a near-limit streaming card can
    /// carry the tail past Feishu's limit.
    #[test]
    fn a_backtick_heavy_body_is_charged_with_its_fence() {
        let body = format!(
            "⚡ **执行 Shell 命令**\n**对象**:\n- `{}`\n",
            "`".repeat(crate::backend::types::PERMISSION_TARGET_MAX_CHARS)
        );
        let estimate = permission_block_estimate(&body, false, false, "n:1", "ses", "/d");
        // What the fallback writes: the fenced element plus the routing-only
        // buttons (the body is not echoed into them).
        let fenced = crate::feishu::card::fenced_code(&body, None);
        let element = serde_json::json!({ "tag": "markdown", "content": fenced })
            .to_string()
            .len();
        let echoed = serde_json::json!({
            "tag": "button",
            "text": { "tag": "plain_text", "content": "✅ 允许一次" },
            "value": {
                "action": "perm",
                "reply": "once",
                "session_id": "ses",
                "request_id": "n:1",
                "directory": "/d",
                "perm_label": "✅ 已允许一次",
                "perm_color": "green",
            }
        })
        .to_string()
        .len();
        let buttons = permission_block_components(false) - 1;
        assert!(
            estimate >= element + buttons * echoed,
            "estimate {estimate} under the fallback's {} bytes",
            element + buttons * echoed
        );
    }

    /// Whole-branch re-review #883: a long directory is repeated in every
    /// button's callback value, so the block estimate must grow with it — a
    /// fixed overhead would let the streaming card's split exceed its budget.
    #[test]
    fn a_long_directory_is_charged_in_the_permission_block_estimate() {
        let short = permission_block_estimate("body", true, false, "n:1", "ses", "/d");
        let directory = format!("/work/{}", "a-project-directory/".repeat(12));
        let long = permission_block_estimate("body", true, false, "n:1", "ses", &directory);
        let buttons = permission_block_components(true) - 1;
        assert_eq!(
            long - short,
            buttons * json_escaped_len(&directory).saturating_sub(2),
            "every button pays for the directory it echoes"
        );
        assert!(long > short, "a longer directory can never estimate less");
    }

    /// Defence in depth: a caller that forgot to mark the request
    /// uncollectable must still never render a secret field as an input.
    #[test]
    fn a_secret_field_never_renders_an_input() {
        use crate::backend::types::{FormFieldKind, QuestionInfo};
        let secret = QuestionInfo {
            key: "token".into(),
            question: "API token".into(),
            header: "Token".into(),
            kind: FormFieldKind::String,
            secret: true,
            ..Default::default()
        };
        let card = build_question_card("", "req", "ses", &[secret], "/work", &[], &[], 0, &[]);
        let json = card.to_string();
        assert!(
            !json.contains("\"tag\":\"input\""),
            "a secret field is never an input: {json}"
        );
        assert!(
            json.contains("私密输入"),
            "the secret refusal is rendered: {json}"
        );
    }

    /// A V2 form's typed fields render as their own controls: a string field's
    /// options show the display label but submit the option value; a boolean
    /// gets 是/否 buttons whose values are JSON booleans; a number gets a typed
    /// input form; an external field shows its link and an acknowledgement
    /// button.
    #[test]
    fn form_card_renders_typed_fields() {
        use crate::backend::types::{FormFieldKind, QuestionInfo, QuestionOption};
        let string_field = QuestionInfo {
            key: "q0".into(),
            question: "选哪个目录？".into(),
            header: "目录".into(),
            kind: FormFieldKind::String,
            options: vec![QuestionOption {
                value: "/a".into(),
                label: "目录 A".into(),
                description: "第一个".into(),
            }],
            custom: Some(false),
            required: true,
            url: None,
            secret: false,
        };
        let boolean = QuestionInfo {
            key: "q1".into(),
            question: "确定吗".into(),
            header: "确认".into(),
            kind: FormFieldKind::Boolean,
            ..Default::default()
        };
        let integer = QuestionInfo {
            key: "q2".into(),
            question: "几个".into(),
            header: "数量".into(),
            kind: FormFieldKind::Integer,
            ..Default::default()
        };
        let external = QuestionInfo {
            key: "q3".into(),
            question: "打开链接".into(),
            header: "链接".into(),
            kind: FormFieldKind::External,
            url: Some("https://example.com/x".into()),
            ..Default::default()
        };
        // One card per field: issue #917 folds every question but the first
        // unanswered one, and Feishu rejects a form nested in a folded panel, so
        // only the open field's controls render on a multi-field card.
        let one = |field: QuestionInfo| {
            build_question_card(
                "Questions",
                "frm_1",
                "ses_1",
                &[field],
                "/tmp/proj",
                &[None],
                &[false],
                0,
                &[],
            )
        };

        // The option button displays the LABEL and submits the VALUE.
        let card = one(string_field);
        assert_eq!(card["header"]["title"]["content"], "❓ Questions");
        let elements = form_elements(&card);
        let option = elements
            .iter()
            .find(|e| e["tag"] == "button" && e["text"]["content"] == "目录 A")
            .expect("option button missing");
        assert_eq!(option["value"]["answer"], "/a");

        // Boolean: 是 / 否 with boolean answer values.
        let card = one(boolean);
        let elements = form_elements(&card);
        let yes = elements
            .iter()
            .find(|e| e["tag"] == "button" && e["text"]["content"] == "✅ 是")
            .expect("boolean 是 button missing");
        assert_eq!(yes["value"]["answer"], "true");
        assert_eq!(yes["value"]["question_index"], 0);
        let no = elements
            .iter()
            .find(|e| e["tag"] == "button" && e["text"]["content"] == "否")
            .expect("boolean 否 button missing");
        assert_eq!(no["value"]["answer"], "false");

        // Number: an input plus its submit inside the root form.
        let card = one(integer);
        assert!(
            card.to_string().contains("输入数字"),
            "numeric placeholder missing: {card}"
        );
        let elements = form_elements(&card);
        let number_input = elements
            .iter()
            .find(|e| e["tag"] == "input")
            .expect("numeric input missing");
        assert_eq!(
            number_input["input_type"], "text",
            "a numeric field keeps the single-line input: a multiline box makes \
             Return an ordinary edit and '42\\n' would not parse: {card}"
        );
        let number_button = elements
            .iter()
            .find(|e| e["tag"] == "button" && e["form_action_type"] == "submit")
            .expect("numeric submit missing");
        assert_eq!(number_button["value"]["question_index"], 0);
        assert_eq!(number_button["value"]["reply"], "answer");

        // External: the link, and an acknowledgement button answering `true`.
        let card = one(external);
        let text = card.to_string();
        assert!(
            text.contains("https://example.com/x"),
            "external link missing: {text}"
        );
        let elements = form_elements(&card);
        let ack = elements
            .iter()
            .find(|e| e["tag"] == "button" && e["text"]["content"] == "✅ 我已了解")
            .expect("external ack missing");
        assert_eq!(ack["value"]["answer"], "true");
    }

    /// A submitted answer VALUE renders as its option's display LABEL while
    /// the payload keeps the raw value; a custom (non-option) value renders
    /// collapsed but is carried verbatim in the click payload.
    #[test]
    fn form_card_maps_values_to_labels_and_keeps_custom_verbatim() {
        use crate::backend::types::{FormFieldKind, QuestionInfo, QuestionOption};
        let field = QuestionInfo {
            key: "q0".into(),
            question: "选水果".into(),
            header: "水果".into(),
            kind: FormFieldKind::Multiselect,
            options: vec![QuestionOption {
                value: "apple".into(),
                label: "苹果".into(),
                description: String::new(),
            }],
            ..Default::default()
        };
        let answered = vec![Some(vec!["apple".to_string(), "自定\n答案".to_string()])];
        let card = build_question_card(
            "",
            "frm_1",
            "ses_1",
            &[field],
            "/tmp/proj",
            &answered,
            &[false],
            0,
            &[],
        );
        let text = card.to_string();
        assert!(
            text.contains("已选：苹果、自定 答案"),
            "display labels must replace submitted values: {text}"
        );
        let elements = form_elements(&card);
        let custom = elements
            .iter()
            .find(|e| e["tag"] == "button" && e["value"]["answer"] == "自定\n答案")
            .expect("custom chip must carry the raw value");
        assert_eq!(custom["text"]["content"], "✅ 自定 答案");
        // Clicking the option chip toggles off the option VALUE, not the label.
        let option = elements
            .iter()
            .find(|e| e["tag"] == "button" && e["text"]["content"] == "✅ 苹果")
            .expect("selected option button missing");
        assert_eq!(option["value"]["answer"], "apple");
    }

    /// A receipt is ONE markdown line built from model text: newlines collapse
    /// and backticks are flattened, so a multiline or code-quoting question
    /// cannot break the residue. The header is only the fallback for a field
    /// whose question text is empty.
    #[test]
    fn a_receipt_label_is_one_safe_line() {
        let question = |text: &str, header: &str| crate::backend::types::QuestionInfo {
            question: text.into(),
            header: header.into(),
            ..Default::default()
        };
        assert_eq!(
            question_label(&question("Which\ncolor?", "Color"), 40),
            "Which color?"
        );
        assert_eq!(
            question_label(&question("Use `red` or `blue`?", "Color"), 40),
            "Use 'red' or 'blue'?"
        );
        assert_eq!(question_label(&question("", "Color"), 40), "Color");
        assert_eq!(
            question_target(&[question("Which color?", "Color"), question("", "Size")]),
            "Which color?、Size"
        );

        // A model-sized question is capped before flattening: the label stays a
        // residue instead of walking the whole body on a card callback.
        let clipped = question_label(&question(&"长".repeat(10_000), "选择"), 40);
        assert!(
            clipped.chars().count() <= 41,
            "the label must stay clipped: {}",
            clipped.chars().count()
        );
        // Leading whitespace must not consume the whole capped window and
        // leave the receipt with no question at all.
        let padded = question_label(&question(&format!("{}  问什么？", " ".repeat(400)), "选择"), 40);
        assert_eq!(padded, "问什么？");
        // Past the bounded window there is nothing left to flatten: the header
        // answers, through the same one-line sanitizer.
        let buried = question_label(&question(&format!("{}问什么？", " ".repeat(600)), "`选择`"), 40);
        assert_eq!(buried, "'选择'");
    }

    /// Issue #917: a card with several questions renders ONE open question and
    /// folds the rest. The client scrolls the message's bottom to the keyboard's
    /// top on focus, so a card holding more than about half a screen below an
    /// input loses that input off the top of the screen; a folded panel costs
    /// only its header line.
    #[test]
    fn a_multi_question_card_folds_every_question_but_the_open_one() {
        let card = build_question_card(
            "",
            "que_1",
            "ses_1",
            &[string_question(1), string_question(2), string_question(3)],
            "/tmp/proj/lib",
            &[None, None, None],
            &[false, false, false],
            0,
            &[],
        );
        let elements = form_elements(&card);
        let panels: Vec<_> = elements
            .iter()
            .filter(|e| e["tag"] == "collapsible_panel")
            .collect();
        assert_eq!(panels.len(), 2, "the two closed questions fold: {card}");
        assert!(panels.iter().all(|p| p["expanded"] == json!(false)));
        assert_eq!(panels[0]["header"]["title"]["content"], "2. 问题 2");
        assert_eq!(panels[1]["header"]["title"]["content"], "3. 问题 3");
        // The whole block is ONE root-level form (issue #917 part 3), so the
        // panels — which may not hold a form themselves (ErrCode 200621) — sit
        // inside it, and the card carries exactly one input: the open
        // question's box.
        assert_eq!(
            card["body"]["elements"][0]["tag"], "form",
            "the block is one form: {card}"
        );
        assert_eq!(
            count_tag(&card, "input"),
            1,
            "only the open question renders a box: {card}"
        );
        assert!(
            elements
                .iter()
                .any(|e| e["tag"] == "input" && e["name"] == "custom_0"),
            "the open question has its input: {card}"
        );
        for panel in &panels {
            assert!(
                !panel.to_string().contains("\"tag\":\"form\""),
                "Feishu rejects a form nested in a collapsible panel (200621): {panel}"
            );
            assert!(
                !panel.to_string().contains("\"tag\":\"input\""),
                "a folded question carries no box: {panel}"
            );
            // The switch is a form SUBMIT, so the open question's draft rides
            // along with it.
            let switch = panel["elements"]
                .as_array()
                .unwrap()
                .iter()
                .find(|e| e["tag"] == "button" && e["value"]["reply"] == "open")
                .expect("a folded question offers its switch");
            assert_eq!(switch["form_action_type"], "submit");
            assert_eq!(
                switch["name"],
                format!("open|que_1|ses_1|{}|0", switch["value"]["question_index"]),
                "the name carries the target AND the box the switch rode"
            );
        }
        // The reject control stays last, below the open question.
        assert_eq!(elements.last().unwrap()["value"]["reply"], "reject");
    }

    /// A numeric field folded behind the open question renders no form (Feishu
    /// rejects a form inside a panel, 200621) and points at the open one, while
    /// the open question keeps the card's only input.
    #[test]
    fn a_folded_numeric_field_renders_no_form() {
        let numeric = |n: usize| crate::backend::types::QuestionInfo {
            question: format!("数量 {n}"),
            header: format!("h{n}"),
            kind: crate::backend::types::FormFieldKind::Integer,
            ..Default::default()
        };
        let card = build_question_card(
            "",
            "que_1",
            "ses_1",
            &[string_question(1), numeric(2)],
            "/tmp/proj/lib",
            &[None, None],
            &[false, false],
            0,
            &[],
        );
        let elements = form_elements(&card);
        let panel = elements
            .iter()
            .find(|e| e["tag"] == "collapsible_panel")
            .expect("the numeric field folds");
        assert!(
            !panel.to_string().contains("\"tag\":\"form\""),
            "no form nested in a panel: {panel}"
        );
        assert!(
            !panel.to_string().contains("\"tag\":\"input\""),
            "a folded numeric field carries no box: {panel}"
        );
        assert!(
            panel["elements"].as_array().unwrap().iter().any(|e| {
                e["tag"] == "button" && e["value"]["reply"] == "open" && e["value"]["question_index"] == 1
            }),
            "the fold says how to reach it: {panel}"
        );
        assert!(
            elements
                .iter()
                .any(|e| e["tag"] == "input" && e["name"] == "custom_0"),
            "the open string question keeps the only input: {card}"
        );
    }

    /// A folded question whose only control would be a typed input renders a
    /// pointer instead: its form cannot live inside the panel (Feishu 200621)
    /// and the open question owns the card's single input (issue #917).
    #[test]
    fn a_folded_input_only_question_points_at_the_open_one() {
        let question = |n: usize| crate::backend::types::QuestionInfo {
            question: format!("问题 {n}"),
            header: format!("h{n}"),
            kind: crate::backend::types::FormFieldKind::String,
            ..Default::default()
        };
        let card = build_question_card(
            "",
            "que_1",
            "ses_1",
            &[question(1), question(2)],
            "/tmp/proj/lib",
            &[None, None],
            &[false, false],
            0,
            &[],
        );
        let elements = form_elements(&card);
        let panel = elements
            .iter()
            .find(|e| e["tag"] == "collapsible_panel")
            .expect("the second question folds");
        assert!(
            !panel.to_string().contains("\"tag\":\"form\""),
            "no form nested in a panel: {panel}"
        );
        assert!(
            panel["elements"]
                .as_array()
                .unwrap()
                .iter()
                .any(|e| { e["tag"] == "button" && e["value"]["reply"] == "open" }),
            "a control-less fold offers its switch: {panel}"
        );
        assert!(
            elements
                .iter()
                .any(|e| e["tag"] == "input" && e["name"] == "custom_0"),
            "the open question still renders its input: {card}"
        );
        assert_eq!(
            count_tag(&card, "input"),
            1,
            "the folded question renders no second box: {card}"
        );
    }

    /// The open question's form keeps ITS index in the routing payload even when
    /// earlier questions are already answered and off the card: the submit's
    /// `question_index` is what ws.rs echoes back as the answered field.
    #[test]
    fn the_open_questions_form_keeps_its_index() {
        let numeric = |n: usize| crate::backend::types::QuestionInfo {
            question: format!("数量 {n}"),
            header: format!("h{n}"),
            kind: crate::backend::types::FormFieldKind::Integer,
            ..Default::default()
        };
        let card = build_question_card(
            "",
            "que_1",
            "ses_1",
            &[string_question(1), numeric(2)],
            "/tmp/proj/lib",
            &[Some(vec!["/a1".into()]), None],
            &[true, false],
            0,
            &[],
        );
        let elements = form_elements(&card);
        let submit = elements
            .iter()
            .find(|e| e["tag"] == "button" && e["form_action_type"] == "submit")
            .expect("the open question has its submit");
        assert_eq!(submit["name"], "submit|que_1|ses_1|1");
        assert_eq!(submit["value"]["question_index"], 1);
        assert_eq!(submit["value"]["reply"], "answer");
        assert!(
            elements
                .iter()
                .any(|e| e["tag"] == "input" && e["name"] == "custom_1"),
            "the box is named for ITS question: {card}"
        );
    }

    /// Answering the open question moves the fold: the next unanswered question
    /// opens, the answered one becomes its one-line receipt.
    #[test]
    fn the_open_question_is_the_first_unanswered_one() {
        let card = build_question_card(
            "",
            "que_1",
            "ses_1",
            &[string_question(1), string_question(2), string_question(3)],
            "/tmp/proj/lib",
            &[Some(vec!["/a1".into()]), None, None],
            &[true, false, false],
            0,
            &[],
        );
        let elements = form_elements(&card);
        let panels: Vec<_> = elements
            .iter()
            .filter(|e| e["tag"] == "collapsible_panel")
            .collect();
        assert_eq!(panels.len(), 1, "only question 3 folds: {card}");
        assert_eq!(panels[0]["header"]["title"]["content"], "3. 问题 3");
        assert!(
            elements
                .iter()
                .any(|e| e["tag"] == "input" && e["name"] == "custom_1"),
            "question 2 is open and renders its input: {card}"
        );
        assert!(
            elements.iter().any(|e| {
                e["tag"] == "markdown" && e["content"].as_str().is_some_and(|c| c.contains("✅ **1."))
            }),
            "the answered question stays a one-line receipt: {card}"
        );
    }

    /// Issue #917 part 3: the reader picks the open question, and the text
    /// typed into the previous one is seeded back into its box.
    #[test]
    fn a_switch_opens_the_chosen_question_and_seeds_its_draft() {
        let card = build_question_card(
            "",
            "que_1",
            "ses_1",
            &[string_question(1), string_question(2)],
            "/tmp/proj/lib",
            &[None, None],
            &[false, false],
            1,
            &["草稿一".to_string(), "草稿二".to_string()],
        );
        let elements = form_elements(&card);
        let input = elements
            .iter()
            .find(|e| e["tag"] == "input")
            .expect("the open question renders its box");
        assert_eq!(input["name"], "custom_1", "the box belongs to question 2");
        assert_eq!(
            input["default_value"], "草稿二",
            "the draft is seeded back into the box"
        );
        // Question 1 folded, and its panel offers the way back.
        let panel = elements
            .iter()
            .find(|e| e["tag"] == "collapsible_panel")
            .expect("question 1 folds");
        assert_eq!(panel["header"]["title"]["content"], "1. 问题 1");
        let switch = panel["elements"]
            .as_array()
            .unwrap()
            .iter()
            .find(|e| e["tag"] == "button" && e["value"]["reply"] == "open")
            .expect("the folded question offers its switch");
        assert_eq!(switch["value"]["question_index"], 0);
        assert_eq!(
            switch["name"], "open|que_1|ses_1|0|1",
            "target 0, riding question 2's open box"
        );
        assert_eq!(
            switch["form_action_type"], "submit",
            "the switch must submit the form so the draft travels with it"
        );
    }

    /// An empty draft is not a `default_value`: the box renders its
    /// placeholder instead of an empty seeded value.
    #[test]
    fn an_empty_draft_renders_no_default_value() {
        let card = build_question_card(
            "",
            "que_1",
            "ses_1",
            &[string_question(1)],
            "/tmp/proj/lib",
            &[None],
            &[false],
            0,
            &[String::new()],
        );
        let input = form_elements(&card)
            .iter()
            .find(|e| e["tag"] == "input")
            .expect("the open question renders its box")
            .clone();
        assert!(
            input.get("default_value").is_none(),
            "no draft, no seeded value: {input}"
        );
    }

    /// Issue #917 part 3, ADR-0094 decision 3: only the two intentional submits
    /// (the open question's answer box, and a folded question's switch) may
    /// carry the form action. An option, a multi-select confirm, the skip submit
    /// or the reject button that accidentally submitted the form would let
    /// `form_value` overwrite an option's answer in `ws.rs`.
    #[test]
    fn only_the_answer_box_and_the_switch_submit_the_form() {
        let card = build_question_card(
            "",
            "que_1",
            "ses_1",
            &[
                crate::backend::types::QuestionInfo {
                    question: "选水果".into(),
                    header: "水果".into(),
                    options: vec![crate::backend::types::QuestionOption {
                        label: "苹果".into(),
                        ..Default::default()
                    }],
                    kind: crate::backend::types::FormFieldKind::Multiselect,
                    custom: None,
                    ..Default::default()
                },
                // Answered, so the skip submit renders too.
                string_question(2),
                many_option_question(),
            ],
            "/tmp/proj/lib",
            &[None, Some(vec!["/a2".into()]), None],
            &[false, true, false],
            0,
            &[String::new(), String::new(), String::new()],
        );
        let mut submits: Vec<String> = all_elements(&card)
            .iter()
            .filter(|e| e["tag"] == "button" && e["form_action_type"] == "submit")
            .map(|e| e["text"]["content"].as_str().unwrap_or("").to_string())
            .collect();
        submits.sort();
        let mut expected = vec!["✍️ 自定义".to_string(), "▸ 作答这道题".to_string()];
        expected.sort();
        assert_eq!(
            submits, expected,
            "only the answer box and the switch may submit the form: {card}"
        );
        // The controls that must stay non-submit are all present on this card.
        let elements = all_elements(&card);
        let labels: Vec<&str> = elements
            .iter()
            .filter_map(|e| e["text"]["content"].as_str())
            .collect();
        for required in ["苹果", "✅ 确定该题", "✅ 提交（跳过剩余）", "🚫 无法回答"] {
            assert!(
                labels.contains(&required),
                "the card must show the non-submit control {required}: {card}"
            );
        }
        // And none of them is a submit.
        for element in all_elements(&card) {
            let label = element["text"]["content"].as_str().unwrap_or("");
            if ["苹果", "✅ 确定该题", "✅ 提交（跳过剩余）", "🚫 无法回答"].contains(&label)
            {
                assert!(
                    element.get("form_action_type").is_none(),
                    "{label} must not submit the form: {element}"
                );
            }
        }
    }

    /// A numeric field's box is seeded from its draft like any other, so a
    /// switch away and back does not lose a half-typed number — and it stays
    /// single-line while doing it.
    #[test]
    fn a_numeric_field_seeds_its_draft_into_a_single_line_box() {
        let numeric = crate::backend::types::QuestionInfo {
            question: "数量".into(),
            header: "h".into(),
            kind: crate::backend::types::FormFieldKind::Integer,
            ..Default::default()
        };
        let card = build_question_card(
            "",
            "que_1",
            "ses_1",
            &[numeric],
            "/tmp/proj/lib",
            &[None],
            &[false],
            0,
            &["12".to_string()],
        );
        let input = form_elements(&card)
            .iter()
            .find(|e| e["tag"] == "input")
            .expect("the numeric field renders a box")
            .clone();
        assert_eq!(input["input_type"], "text", "still single-line: {card}");
        assert_eq!(input["default_value"], "12", "the draft is seeded: {card}");
    }

    /// One question — the common ask — keeps today's flat layout: no fold, no
    /// switch, no extra tap.
    #[test]
    fn a_single_question_card_does_not_fold() {
        let card = build_question_card(
            "",
            "que_1",
            "ses_1",
            &[many_option_question()],
            "/tmp/proj/lib",
            &[None],
            &[false],
            0,
            &[],
        );
        let elements = card["body"]["elements"].as_array().unwrap();
        assert!(
            !all_elements(&card)
                .iter()
                .any(|e| e["tag"] == "collapsible_panel"),
            "a one-question card has nothing to fold: {card}"
        );
        assert!(elements.iter().any(|e| e["tag"] == "form"));
    }
}
