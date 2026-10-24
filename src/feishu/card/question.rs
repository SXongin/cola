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
    body: &str,
    directory: &str,
    always_supported: bool,
) -> Vec<serde_json::Value> {
    let btn_value = |reply: &str, label: &str, color: &str| {
        json!({
            "action": "perm",
            "reply": reply,
            "session_id": session_id,
            "request_id": request_id,
            "directory": directory,
            "perm_label": label,
            "perm_color": color,
            "perm_body": body,
        })
    };
    let mut elements = Vec::new();
    if always_supported {
        elements.push(json!({ "tag": "markdown", "content": SESSION_ALWAYS_NOTE }));
    }
    elements.push(
        json!({ "tag": "button", "text": { "tag": "plain_text", "content": "✅ 允许一次" }, "type": "primary", "value": btn_value("once", "✅ 已允许一次", "green") }),
    );
    if always_supported {
        elements.push(
            json!({ "tag": "button", "text": { "tag": "plain_text", "content": "🔁 始终允许" }, "type": "default", "value": btn_value("always", "✅ 已始终允许", "green") }),
        );
    }
    elements.extend([
        json!({ "tag": "button", "text": { "tag": "plain_text", "content": "🚫 拒绝" }, "type": "danger", "value": btn_value("reject", "🚫 已拒绝", "red") }),
        json!({ "tag": "button", "text": { "tag": "plain_text", "content": "⚡ 开启自动授权" }, "type": "primary", "value": btn_value("autoaccept", "✅ 已开启自动授权", "blue") }),
    ]);
    elements
}

/// Build the interactive permission card (JSON 2.0). Buttons sit directly in
/// `body.elements` — the v1 `action` container is gone in schema 2.0. Each
/// button carries the request id, session id, owning directory and a
/// description so the card callback (`action: "perm"`) can route the reply
/// back to the right instance and render a result card.
pub fn build_permission_card(
    session_id: &str,
    request_id: &str,
    body: &str,
    directory: &str,
    always_supported: bool,
) -> serde_json::Value {
    let body = super::sanitize::CardMarkdown::new().clean(body);
    let mut elements = vec![json!({ "tag": "markdown", "content": body })];
    elements.extend(permission_buttons(
        session_id,
        request_id,
        &body,
        directory,
        always_supported,
    ));
    card_shell("🔐 权限请求", "orange", elements)
}

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
/// Every button echoes the body and the whole routing triple (`session_id`,
/// `request_id`, `directory`) in its callback value, so each of those appears
/// once as the block's element and once per button. The routing values come
/// from the caller — a long directory is the common case, and a fixed overhead
/// would let the streaming card's split underestimate itself past Feishu's
/// limit.
///
/// The body is estimated **as the card renders it**, not as authored:
/// [`CardMarkdown::element`] is what both the standalone card and the streaming
/// body apply, and it expands `<` to `&#60;` (five bytes for one), so a
/// 150-char reason of `<` would add 600 bytes per copy that a raw-length
/// estimate missed. The result is then charged at its JSON-escaped length
/// ([`json_escaped_len`]). The `BUTTON_OVERHEAD` margin also absorbs the
/// fenced-fallback wrapper (a card whose content Feishu rejected renders the
/// body verbatim inside one fence instead of cleaning it).
pub(crate) fn permission_block_estimate(
    body: &str,
    always_supported: bool,
    request_id: &str,
    session_id: &str,
    directory: &str,
) -> usize {
    // A button's own labels, action, reply, colour and JSON punctuation. CJK
    // labels cost three bytes a character, which is why this is generous.
    const BUTTON_OVERHEAD: usize = 360;
    // Both copies (the element and every button's `perm_body`) carry the
    // cleaned body; `perm_body` is the same text `build_permission_card`
    // passes after cleaning.
    let body = json_escaped_len(&CardMarkdown::new().element(body));
    let routing = json_escaped_len(request_id) + json_escaped_len(session_id) + json_escaped_len(directory);
    let buttons = permission_button_count(always_supported);
    let note = if always_supported {
        json_escaped_len(&CardMarkdown::new().element(SESSION_ALWAYS_NOTE)) + 60
    } else {
        0
    };
    body + buttons * (body + routing + BUTTON_OVERHEAD) + note
}

/// Compact one-line target for a question's Interaction Receipt: each
/// question's header (or a clipped question text), joined, clipped to stay a
/// residue (ADR-0038, rule 4). Shared by the inline block's receipt and the
/// standalone-card path that names the same request.
pub(crate) fn question_target(questions: &[crate::backend::types::QuestionInfo]) -> String {
    super::truncate_md(
        &questions
            .iter()
            .map(|qi| {
                if qi.header.is_empty() {
                    super::truncate_md(&qi.question, 24)
                } else {
                    qi.header.clone()
                }
            })
            .collect::<Vec<_>>()
            .join("、"),
        60,
    )
}

/// A one-line-per-question summary of a `question` request, for the stale card.
pub fn question_summary(questions: &[crate::backend::types::QuestionInfo]) -> String {
    let mut s = String::new();
    for (i, q) in questions.iter().enumerate() {
        s.push_str(&format!("{}. {}\n", i + 1, q.question));
    }
    super::sanitize::CardMarkdown::new().clean(&s)
}

/// The card copy for a question cola cannot collect at all. The neutral model
/// carries only [`UncollectableReason`]; the user-facing wording lives here,
/// in the card copy table, and every rendering path (standalone, inline,
/// snapshot) asks for it.
pub(crate) fn uncollectable_note(reason: UncollectableReason) -> &'static str {
    match reason {
        UncollectableReason::SecretInput => {
            "该提问需要私密输入，cola 无法在卡片上安全收集；请在这里拒绝，必要时请在原始客户端重新发起该提问。"
        }
    }
}

/// The header of the card that records a permission cola auto-declined because
/// its target cannot be rendered in full. Pinned beside the note below, so the
/// decision and its explanation cannot diverge.
pub(crate) const UNSHOWABLE_PERMISSION_HEADER: &str = "🔐 权限请求已自动拒绝";

/// Why such a request is declined, and what the user can do. The permission
/// flow declines the request on the server and asks this function for the copy;
/// the card below renders the same text, so the two never disagree.
pub(crate) fn unshowable_permission_note(budget: usize) -> String {
    format!(
        "该权限请求的目标超过 **{budget}** 字符，卡片无法完整显示。为了不让你批准只看到一部分的内容，\
         cola 已自动拒绝；请在原始客户端中确认后重试。"
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
pub(crate) fn unshowable_permission_retry_note() -> &'static str {
    "该权限请求的目标过长，卡片无法完整显示；cola 暂时无法自动拒绝，稍后会自动重试。\
     这张卡片不提供批准操作。"
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

/// The receipt line an inline permission block collapses to when cola refused
/// it before it could be offered — its target is too long for the card to show
/// in full, so approving it would approve unseen text. Pinned with the refusal
/// header/note above, so the block, the standalone card and the chat notice
/// cannot drift.
pub(crate) const UNSHOWABLE_PERMISSION_RECEIPT: &str = "🚫 已自动拒绝：目标过长，卡片无法完整显示";

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
/// carries it, while inline and snapshot blocks show both here. An
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
) -> Vec<serde_json::Value> {
    use crate::backend::types::FormFieldKind;

    let is_done = |i: usize| -> bool { done.get(i).copied().unwrap_or(false) };
    let selected = |i: usize| -> Option<&Vec<String>> { answered.get(i).and_then(|slot| slot.as_ref()) };
    let mut elements: Vec<serde_json::Value> = Vec::new();
    // Question text comes from the model: sanitize it for the card dialect
    // (one budget for the block; a question rarely carries tables).
    let mut md = super::sanitize::CardMarkdown::new();

    // The request's own title (an elicitation's message, for example) belongs
    // on every surface; the standalone card puts it in the header and passes
    // an empty title here, while inline and snapshot blocks render it here.
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
        for (i, q) in questions.iter().enumerate() {
            let multi = q.is_multi();
            let finalized = is_done(i);

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
            elements.push(json!({ "tag": "markdown", "content": heading }));

            if finalized {
                // Answered / confirmed: no controls, just the 已选 line above.
            } else {
                match q.kind {
                    // An external link is not answerable; acknowledging it is the
                    // only valid answer (the form schema requires the `true`).
                    FormFieldKind::External => {
                        let mut value = question_value(request_id, session_id, directory, i, "answer");
                        value["answer"] = json!("true");
                        elements.push(json!({
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
                            elements.push(json!({
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
                        elements.push(number_answer_form(request_id, session_id, directory, i));
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
                            elements.push(json!({
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
                                elements.push(json!({
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
                                elements.push(json!({
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
                        if q.custom_allowed() || (q.kind == FormFieldKind::String && q.options.is_empty()) {
                            elements.push(custom_answer_form(request_id, session_id, directory, i, multi));
                        }

                        // Multi-select: the commit action sits right under its own
                        // options instead of a far-away bottom submit. Clicking
                        // locks the toggled set (empty allowed — "不选") and
                        // collapses the question to 已选.
                        if multi {
                            elements.push(json!({
                                "tag": "button",
                                "text": { "tag": "plain_text", "content": "✅ 确定该题" },
                                "type": "primary",
                                "value": question_value(request_id, session_id, directory, i, "confirm"),
                            }));
                        }
                    }
                }
            }

            // A divider separates question blocks so options never mix visually.
            if i + 1 < questions.len() {
                elements.push(json!({ "tag": "hr" }));
            }
        }
    }

    // Submit appears only when something is picked while questions remain open
    // ("跳过剩余"). A multi-select question is finalized by its own 确定该题
    // button, so there is no separate bottom submit for the multi-select case.
    let answered_count = done.iter().filter(|d| **d).count();
    if answered_count > 0 && answered_count < questions.len() {
        elements.push(json!({
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
    elements.push(question_reject_button(request_id, session_id, directory));

    elements
}

/// The reject control every question block carries. One builder, so the
/// uncollectable block and an ordinary question cannot drift.
fn question_reject_button(request_id: &str, session_id: &str, directory: &str) -> serde_json::Value {
    json!({
        "tag": "button",
        "text": { "tag": "plain_text", "content": "🚫 无法回答" },
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

/// A custom (free-text) answer input plus its submit button, one per field.
/// Single-select submits a replacement; a multi-select ADDS the typed label to
/// the toggled set. The typed text arrives in `action.form_value` and is
/// injected by ws.rs.
fn custom_answer_form(
    request_id: &str,
    session_id: &str,
    directory: &str,
    index: usize,
    multi: bool,
) -> serde_json::Value {
    let (reply, name_prefix) = if multi {
        ("custom", "submitm")
    } else {
        ("answer", "submit")
    };
    let mut value = question_value(request_id, session_id, directory, index, reply);
    value["question_index"] = json!(index);
    json!({
        "tag": "form",
        "name": format!("form_{}", index),
        "elements": [
            {
                "tag": "input",
                "name": format!("custom_{}", index),
                // Multiline box instead of the default single line: the
                // single-line input is a cramped one-row strip that types
                // poorly on both PC and mobile. rows:1 starts it at one line;
                // auto_resize grows it with the text (Feishu's docs say
                // PC-only, but mobile grows too in practice). Callbacks arrive
                // unchanged via `action.form_value`.
                "input_type": "multiline_text",
                "rows": 1,
                "auto_resize": true,
                "max_rows": 8,
                "placeholder": { "tag": "plain_text", "content": "✍️ 输入自定义答案" },
                "max_length": 500,
                "width": "fill",
            },
            {
                "tag": "button",
                "text": { "tag": "plain_text", "content": "✍️ 自定义" },
                "type": "default",
                "form_action_type": "submit",
                "name": submit_name(name_prefix, request_id, session_id, index),
                "value": value,
            },
        ],
    })
}

/// A numeric field's answer form: one single-line input plus its submit. The
/// typed text is parsed into the field's number at reply time; a value that
/// cannot parse is left unanswered rather than posted as an invalid answer.
fn number_answer_form(
    request_id: &str,
    session_id: &str,
    directory: &str,
    index: usize,
) -> serde_json::Value {
    let value = question_value(request_id, session_id, directory, index, "answer");
    json!({
        "tag": "form",
        "name": format!("form_{}", index),
        "elements": [
            {
                "tag": "input",
                "name": format!("custom_{}", index),
                "input_type": "text",
                "placeholder": { "tag": "plain_text", "content": "✍️ 输入数字" },
                "max_length": 40,
                "width": "fill",
            },
            {
                "tag": "button",
                "text": { "tag": "plain_text", "content": "✅ 确定" },
                "type": "primary",
                "form_action_type": "submit",
                "name": submit_name("submit", request_id, session_id, index),
                "value": value,
            },
        ],
    })
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
pub fn build_question_card(
    title: &str,
    request_id: &str,
    session_id: &str,
    questions: &[crate::backend::types::QuestionInfo],
    directory: &str,
    answered: &[Option<Vec<String>>],
    done: &[bool],
) -> serde_json::Value {
    let elements = question_elements(
        "", None, request_id, session_id, questions, directory, answered, done,
    );
    let header = if title.is_empty() {
        "❓ AI 想问你".to_string()
    } else {
        format!("❓ {title}")
    };
    card_shell(&header, "blue", elements)
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
    let elements = question_elements("", Some(reason), request_id, session_id, &[], directory, &[], &[]);
    card_shell(UNCOLLECTABLE_HEADER, "orange", elements)
}

#[cfg(test)]
mod tests {
    use super::*;

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
        );
        let text = card.to_string();
        assert!(
            text.contains("选择要在哪个目录继续"),
            "question text missing: {}",
            text
        );

        // JSON 2.0: buttons live directly in body.elements (no v1 action row).
        assert_eq!(card["schema"].as_str().unwrap(), "2.0");
        let elements = card["body"]["elements"].as_array().unwrap();
        let buttons: Vec<_> = elements.iter().filter(|e| e["tag"] == "button").collect();
        // one button per option + a reject button
        assert_eq!(buttons.len(), 3);

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
        );
        let elements = card["body"]["elements"].as_array().unwrap();
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
        // No raw option buttons for the collapsed question (the form's submit
        // button is nested inside the form element, not a top-level button).
        let buttons: Vec<_> = elements.iter().filter(|e| e["tag"] == "button").collect();
        assert_eq!(buttons.len(), 1, "only the reject button remains");
        assert_eq!(buttons[0]["value"]["reply"], "reject");
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
        );
        let elements = card["body"]["elements"].as_array().unwrap();
        let form = elements
            .iter()
            .find(|e| e["tag"] == "form")
            .expect("overflow question still gets a custom-answer form");
        assert_eq!(form["name"], "form_0");
        assert!(form.to_string().contains("form_action_type"));
    }

    #[test]
    fn question_card_overflow_custom_disabled_has_no_form() {
        let mut q = many_option_question();
        q.custom = Some(false);
        let card = build_question_card("", "que_1", "ses_1", &[q], "/tmp/proj/lib", &[None], &[false]);
        let elements = card["body"]["elements"].as_array().unwrap();
        assert!(
            elements.iter().any(|e| e["tag"] == "overflow"),
            "overflow picker still rendered"
        );
        assert!(
            elements.iter().all(|e| e["tag"] != "form"),
            "custom-disabled overflow question must not get an input form: {}",
            card
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
        );
        let elements = card["body"]["elements"].as_array().unwrap();
        let form = elements
            .iter()
            .find(|e| e["tag"] == "form")
            .expect("custom-allowed question gets an input form");
        assert_eq!(form["name"], "form_0");
        assert!(form.to_string().contains("input"), "form needs an input");
        assert!(form.to_string().contains("form_action_type"));
        // The custom-answer box is a multiline textarea (better typing on both
        // PC and mobile than the default single-line input). rows:1 starts it
        // compact; auto_resize grows it with the text.
        let input = form["elements"].as_array().unwrap()[0].clone();
        assert_eq!(input["tag"], "input");
        assert_eq!(input["input_type"], "multiline_text");
        assert_eq!(input["rows"], 1);
        assert_eq!(input["auto_resize"], true);
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
        );
        let elements = card["body"]["elements"].as_array().unwrap();
        assert!(
            elements.iter().all(|e| e["tag"] != "form"),
            "custom-disabled question must not get an input form: {}",
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
        );
        let elements = card["body"]["elements"].as_array().unwrap();
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
        // Multi-select now supports a custom answer too (reply "custom" adds it).
        let form = elements
            .iter()
            .find(|e| e["tag"] == "form")
            .expect("multi-select custom-allowed question gets an input form");
        assert!(
            form.to_string().contains("\"reply\":\"custom\""),
            "multi-select custom form must reply with custom: {}",
            form
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
        );
        let elements = card["body"]["elements"].as_array().unwrap();
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
        );
        let elements = card["body"]["elements"].as_array().unwrap();
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
        // A divider separates the two question blocks.
        let elements = card["body"]["elements"].as_array().unwrap();
        let hr = elements.iter().filter(|e| e["tag"] == "hr").count();
        assert_eq!(hr, 1, "one divider between two questions: {}", card);
        // The multi-select's confirm button is grouped right under ITS own
        // options (after Q1's option + custom form) and before the reject
        // button — no separate bottom submit for a multi-select question.
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

    #[test]
    fn a_question_title_renders_in_the_block_for_inline_surfaces() {
        let elements = question_elements("把文件写入工作区？", None, "req_1", "ses_1", &[], "/w", &[], &[]);
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
        let card = build_permission_card("ses_abc", "per_123", "AI 想要执行 bash", "/tmp/proj/lib", true);
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
        assert!(once["perm_body"].as_str().unwrap().contains("bash"));
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

    /// Whole-branch review #883: the command budget is only safe if the card it
    /// produces is. A permission body is rendered once and echoed into every
    /// button's `perm_body`, so it appears six times in the card's JSON; this
    /// pins the worst case — a full-budget CJK command — inside the card's own
    /// budget. Raising [`crate::backend::types::PERMISSION_COMMAND_MAX_CHARS`]
    /// fails here, which is the point.
    #[test]
    fn a_permission_card_at_the_command_budget_fits_the_card_budget() {
        let budget = crate::backend::types::PERMISSION_COMMAND_MAX_CHARS;
        let command = "啊".repeat(budget);
        let body =
            format!("⚡ **执行 Shell 命令**\n**对象**:\n- `{command}`\n\nAI 想要执行这个操作，是否允许？");
        let card = build_permission_card("ses_abc", "n:1", &body, "/tmp/proj", true);
        let len = card.to_string().len();
        assert!(
            len <= crate::feishu::card::MAX_CARD_JSON_CHARS,
            "a full-budget command card is {len} bytes, over the {} budget",
            crate::feishu::card::MAX_CARD_JSON_CHARS
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
        // `&#60;` (5 bytes), so a raw-length estimate under-counts every copy by
        // 600 bytes — the case the cleaning step in the estimate exists for.
        let angle_reason = format!(
            "⚡ **执行 Shell 命令**\n**原因**: {}\n**对象**:\n- `ls`\n\nAI 想要执行这个操作，是否允许？",
            "<".repeat(150)
        );
        for (body, always, directory) in [
            ("⚡ **执行 Shell 命令**\n**对象**:\n- `ls`\n", true, "/d"),
            ("⚡ **执行 Shell 命令**\n**对象**:\n- `cargo test`\n", false, "/d"),
            (&"啊\"\\\n".repeat(250), true, "/d"),
            (
                "⚡ **执行 Shell 命令**\n**对象**:\n- `ls`\n",
                true,
                long_directory.as_str(),
            ),
            (angle_reason.as_str(), true, "/d"),
        ] {
            let card = build_permission_card("ses", "n:1", body, directory, always);
            let elements = card["body"]["elements"].as_array().expect("body elements");
            let rendered: usize = elements.iter().map(|element| element.to_string().len()).sum();
            let estimate = permission_block_estimate(body, always, "n:1", "ses", directory);
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

    /// Whole-branch re-review #883: the estimate must charge the **rendered**
    /// body. `CardMarkdown` expands `<` to `&#60;`, so a reason full of `<` is
    /// five times its authored length; a raw-byte estimate would let the split
    /// reserve 600 bytes too little per copy.
    #[test]
    fn an_angle_bracket_reason_is_charged_at_its_sanitized_size() {
        let reason = "<".repeat(150);
        let body = format!("**原因**: {reason}");
        let buttons = permission_block_components(true) - 1;
        let estimate = permission_block_estimate(&body, true, "n:1", "ses", "/d");
        let raw = permission_block_estimate(&"".repeat(0), true, "n:1", "ses", "/d");
        let sanitized = crate::feishu::card::sanitize::CardMarkdown::new().element(&body);
        assert_eq!(sanitized.len(), body.len() - 150 + 150 * "&#60;".len());
        assert_eq!(
            estimate - raw,
            (1 + buttons) * json_escaped_len(&sanitized),
            "the element and every button carry the cleaned body's bytes"
        );
    }

    /// Whole-branch re-review #883: a long directory is repeated in every
    /// button's callback value, so the block estimate must grow with it — a
    /// fixed overhead would let the streaming card's split exceed its budget.
    #[test]
    fn a_long_directory_is_charged_in_the_permission_block_estimate() {
        let short = permission_block_estimate("body", true, "n:1", "ses", "/d");
        let directory = format!("/work/{}", "a-project-directory/".repeat(12));
        let long = permission_block_estimate("body", true, "n:1", "ses", &directory);
        let buttons = permission_block_components(true) - 1;
        assert_eq!(
            long - short,
            buttons * json_escaped_len(&directory).saturating_sub(2),
            "every button pays for the directory it echoes"
        );
        assert!(long > short, "a longer directory can never estimate less");
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
        let card = build_question_card(
            "Questions",
            "frm_1",
            "ses_1",
            &[string_field, boolean, integer, external],
            "/tmp/proj",
            &[None, None, None, None],
            &[false, false, false, false],
        );
        let text = card.to_string();
        assert_eq!(card["header"]["title"]["content"], "❓ Questions");
        assert!(
            text.contains("https://example.com/x"),
            "external link missing: {text}"
        );

        // The option button displays the LABEL and submits the VALUE.
        let elements = card["body"]["elements"].as_array().unwrap();
        let option = elements
            .iter()
            .find(|e| e["tag"] == "button" && e["text"]["content"] == "目录 A")
            .expect("option button missing");
        assert_eq!(option["value"]["answer"], "/a");

        // Boolean: 是 / 否 with boolean answer values.
        let yes = elements
            .iter()
            .find(|e| e["tag"] == "button" && e["text"]["content"] == "✅ 是")
            .expect("boolean 是 button missing");
        assert_eq!(yes["value"]["answer"], "true");
        assert_eq!(yes["value"]["question_index"], 1);
        let no = elements
            .iter()
            .find(|e| e["tag"] == "button" && e["text"]["content"] == "否")
            .expect("boolean 否 button missing");
        assert_eq!(no["value"]["answer"], "false");

        // Number: an input form whose submit routes as an answer.
        let number_form = elements
            .iter()
            .find(|e| e["tag"] == "form" && e["value"].is_null())
            .expect("numeric form missing");
        assert!(
            number_form.to_string().contains("输入数字"),
            "numeric placeholder missing: {number_form}"
        );
        let number_button = &number_form["elements"][1];
        assert_eq!(number_button["value"]["question_index"], 2);
        assert_eq!(number_button["value"]["reply"], "answer");

        // External: an acknowledgement button answering `true`.
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
        let card = build_question_card("", "frm_1", "ses_1", &[field], "/tmp/proj", &answered, &[false]);
        let text = card.to_string();
        assert!(
            text.contains("已选：苹果、自定 答案"),
            "display labels must replace submitted values: {text}"
        );
        let elements = card["body"]["elements"].as_array().unwrap();
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
}
