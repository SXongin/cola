use serde_json::json;

use super::card_shell;

/// Feishu rejects JSON 2.0 cards with more than 200 total components/elements
/// (ErrCode 11310 "element exceeds the limit"). A single collapsible panel
/// counts as multiple components (panel + header title + icon + nested
/// markdown), so cap the number of tool panels rendered in one card.
/// A question with more options than this collapses them into a folding/// `overflow` group instead of a tall stack of buttons.
const MAX_VISIBLE_OPTIONS: usize = 3;

/// Display cap for a Custom Answer button label. Long or multiline answers
/// collapse and truncate in the button only; the stored answer and the click
/// value keep the raw text.
const CUSTOM_ANSWER_LABEL_CHARS: usize = 20;

/// The four permission buttons (Allow Once / Allow Always / Deny / Auto-Accept),
/// as body elements. Shared by the standalone permission card and the inline
/// section on the streaming card. The fourth turns on the session's Auto-Accept
/// (cola-side, `/autoaccept`), distinct from "Allow Always" which is a
/// backend-side per-type rule.
pub fn permission_buttons(
    session_id: &str,
    request_id: &str,
    body: &str,
    directory: &str,
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
    vec![
        json!({ "tag": "button", "text": { "tag": "plain_text", "content": "✅ 允许一次" }, "type": "primary", "value": btn_value("once", "✅ 已允许一次", "green") }),
        json!({ "tag": "button", "text": { "tag": "plain_text", "content": "🔁 始终允许" }, "type": "default", "value": btn_value("always", "✅ 已始终允许", "green") }),
        json!({ "tag": "button", "text": { "tag": "plain_text", "content": "🚫 拒绝" }, "type": "danger", "value": btn_value("reject", "🚫 已拒绝", "red") }),
        json!({ "tag": "button", "text": { "tag": "plain_text", "content": "⚡ 开启自动授权" }, "type": "primary", "value": btn_value("autoaccept", "✅ 已开启自动授权", "blue") }),
    ]
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
) -> serde_json::Value {
    let mut elements = vec![json!({ "tag": "markdown", "content": body })];
    elements.extend(permission_buttons(session_id, request_id, body, directory));
    card_shell("🔐 权限请求", "orange", elements)
}

/// A one-line-per-question summary of a `question` request, for the stale card.
pub fn question_summary(questions: &[crate::opencode::client::QuestionInfo]) -> String {
    let mut s = String::new();
    for (i, q) in questions.iter().enumerate() {
        s.push_str(&format!("{}. {}\n", i + 1, q.question));
    }
    s
}

/// Build the interactive question card (JSON 2.0). Each question renders as its
/// own block: a markdown heading (number, text, status) followed by its
/// controls, with a divider between blocks so single- and multi-select options
/// never mix. Controls are option buttons (or a folding `overflow` group when a
/// single-select has many options) plus — when `custom` is allowed (default
/// true) — an input form; a multi-select question (`multiple`) renders toggle
/// buttons whose clicks add/remove labels in the running selection ("已选"),
/// committed by a per-question "确定该题" button right under its own options.
/// Custom Answers (user-typed entries) render as selected toggle buttons too —
/// clicking one removes it. `done[i]` marks a finalized question (a
/// single-select answered by a click, a multi-select confirmed) — it renders as
/// a static "✅ … 已选" line instead of controls, so answering one question
/// never silently submits the others.
/// `answered[i]` is the DISPLAY selection: the locked answer for done questions,
/// or the in-progress toggles of an open multi-select. A submit button appears
/// when some (but not all) questions are answered (skip remaining); a reject
/// button sits at the bottom. The card callback (`action: "question"`) posts
/// the answer back to the session.
/// The body elements of a question prompt. Shared by the standalone question
/// card and the inline section on the streaming card.
pub fn question_elements(
    request_id: &str,
    session_id: &str,
    questions: &[crate::opencode::client::QuestionInfo],
    directory: &str,
    answered: &[Option<Vec<String>>],
    done: &[bool],
) -> Vec<serde_json::Value> {
    // A multi-select question (`multiple`) is NEVER finalized by clicking an
    // option: clicks toggle labels in its answer set until the user hits the
    // per-question "确定该题" button. Single-select questions are finalized the
    // moment an option is clicked.
    let is_multi = |i: usize| -> bool { questions.get(i).is_some_and(|q| q.multiple == Some(true)) };
    let is_done = |i: usize| -> bool { done.get(i).copied().unwrap_or(false) };
    let mut elements: Vec<serde_json::Value> = Vec::new();

    for (i, q) in questions.iter().enumerate() {
        let multi = is_multi(i);
        let finalized = is_done(i);

        // Per-question heading: number + text + status, option bullets while
        // the question is still open (so the full option list reads together
        // with the controls right below), and the running selection.
        let mut heading = String::new();
        if finalized {
            heading.push_str(&format!("✅ **{}. {}**", i + 1, q.question));
        } else {
            let title = if q.header.is_empty() {
                q.question.clone()
            } else {
                q.header.clone()
            };
            heading.push_str(&format!(
                "**{}. {}{}**",
                i + 1,
                title,
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
        }
        if let Some(Some(labels)) = answered.get(i) {
            heading.push_str(&format!("\n👉 已选：{}", labels.join("、")));
        }
        elements.push(json!({ "tag": "markdown", "content": heading }));

        if finalized {
            // Answered / confirmed: no controls, just the 已选 line above.
        } else {
            // Option picker: raw buttons when the set is small; a single-select
            // with many options collapses into an `overflow` group (the heading
            // above keeps the full option list visible). Multi-select always
            // stays on buttons so clicks toggle the running set.
            if !multi && q.options.len() > MAX_VISIBLE_OPTIONS {
                let options: Vec<serde_json::Value> = q
                    .options
                    .iter()
                    .map(|opt| {
                        json!({
                            "text": { "tag": "plain_text", "content": opt.label },
                            "value": format!("{}|{}", i, opt.label),
                        })
                    })
                    .collect();
                elements.push(json!({
                    "tag": "overflow",
                    "width": "fill",
                    "options": options,
                    "value": {
                        "action": "question",
                        "reply": "answer",
                        "request_id": request_id,
                        "session_id": session_id,
                        "directory": directory,
                    },
                }));
            } else {
                for opt in &q.options {
                    // Multi-select buttons show their selected state so the user
                    // can see what's already picked while still toggling.
                    let selected = multi
                        && answered
                            .get(i)
                            .and_then(|a| a.as_ref())
                            .is_some_and(|labels| labels.iter().any(|l| l == &opt.label));
                    elements.push(json!({
                        "tag": "button",
                        "text": {
                            "tag": "plain_text",
                            "content": if selected {
                                format!("✅ {}", opt.label)
                            } else {
                                opt.label.clone()
                            },
                        },
                        "type": if selected { "primary" } else { "default" },
                        "value": {
                            "action": "question",
                            "reply": "answer",
                            "request_id": request_id,
                            "session_id": session_id,
                            "directory": directory,
                            "question_index": i,
                            "answer": opt.label,
                        },
                    }));
                }
            }

            // Custom Answers: user-typed entries in the selection, rendered as
            // selected buttons exactly like a picked option — a click removes
            // the entry through the same toggle path (`reply: "answer"`). The
            // button label is display-only (whitespace collapsed, long text
            // truncated); the value carries the raw answer verbatim.
            if multi && let Some(Some(labels)) = answered.get(i) {
                for label in labels {
                    if q.options.iter().any(|opt| &opt.label == label) {
                        continue;
                    }
                    elements.push(json!({
                        "tag": "button",
                        "text": {
                            "tag": "plain_text",
                            "content": format!("✅ {}", custom_answer_label(label)),
                        },
                        "type": "primary",
                        "value": {
                            "action": "question",
                            "reply": "answer",
                            "request_id": request_id,
                            "session_id": session_id,
                            "directory": directory,
                            "question_index": i,
                            "answer": label,
                        },
                    }));
                }
            }

            // Free-text answer (OpenCode questions allow custom by default). The
            // typed text arrives in `action.form_value`; ws.rs injects it into
            // the answer payload. Single-select submits a replacement
            // (`reply: "answer"`); a multi-select ADDS the typed label to the
            // toggled set (`reply: "custom"`), so the two never collide. Rendered
            // for the overflow path too: a many-option single-select still needs
            // a way to type an answer outside the listed set.
            if q.custom.unwrap_or(true) {
                let (reply, name_prefix) = if multi {
                    ("custom", "submitm")
                } else {
                    ("answer", "submit")
                };
                let input_name = format!("custom_{}", i);
                elements.push(json!({
                    "tag": "form",
                    "name": format!("form_{}", i),
                    "elements": [
                        {
                            "tag": "input",
                            "name": input_name,
                            // Multiline box instead of the default single line:
                            // the single-line input is a cramped one-row strip
                            // that types poorly on both PC and mobile. rows:1
                            // starts it at one line; auto_resize grows it with
                            // the text (Feishu's docs say PC-only, but mobile
                            // grows too in practice). Callbacks arrive unchanged
                            // via `action.form_value`.
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
                            // Form submit callbacks don't always carry the button
                            // `value`, so the routing payload is ALSO encoded in the
                            // `name` ("submit|req|ses|qi", or "submitm|…" for a
                            // multi-select custom addition) — ws.rs rebuilds the
                            // value from it when `action.value` is absent. The
                            // directory is deliberately NOT in the name: Feishu
                            // caps `name` at 100 chars and `submit|req|ses|qi|dir`
                            // overflows on deep paths, killing the whole card update
                            // (ErrCode 11310 "name exceed the default maximum 100").
                            // The handler re-resolves the directory from the store /
                            // request flow when the fallback fires.
                            "name": format!("{}|{}|{}|{}", name_prefix, request_id, session_id, i),
                            "value": {
                                "action": "question",
                                "reply": reply,
                                "request_id": request_id,
                                "session_id": session_id,
                                "directory": directory,
                                "question_index": i,
                            },
                        },
                    ],
                }));
            }

            // Multi-select: the commit action sits right under its own options
            // instead of a far-away bottom submit. Clicking locks the toggled
            // set (empty allowed — "不选") and collapses the question to 已选.
            if multi {
                elements.push(json!({
                    "tag": "button",
                    "text": { "tag": "plain_text", "content": "✅ 确定该题" },
                    "type": "primary",
                    "value": {
                        "action": "question",
                        "reply": "confirm",
                        "request_id": request_id,
                        "session_id": session_id,
                        "directory": directory,
                        "question_index": i,
                    },
                }));
            }
        }

        // A divider separates question blocks so options never mix visually.
        if i + 1 < questions.len() {
            elements.push(json!({ "tag": "hr" }));
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
    elements.push(json!({
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
    }));

    elements
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
/// body elements from `question_elements`.
pub fn build_question_card(
    request_id: &str,
    session_id: &str,
    questions: &[crate::opencode::client::QuestionInfo],
    directory: &str,
    answered: &[Option<Vec<String>>],
    done: &[bool],
) -> serde_json::Value {
    let elements = question_elements(request_id, session_id, questions, directory, answered, done);
    card_shell("❓ AI 想问你", "blue", elements)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn question_card_has_option_buttons_with_answer_payload() {
        let questions = vec![crate::opencode::client::QuestionInfo {
            question: "选择要在哪个目录继续".into(),
            header: "目录".into(),
            options: vec![
                crate::opencode::client::QuestionOption {
                    label: "/a".into(),
                    description: "dir a".into(),
                },
                crate::opencode::client::QuestionOption {
                    label: "/b".into(),
                    description: String::new(),
                },
            ],
            multiple: None,
            custom: None,
        }];
        let card = build_question_card("que_1", "ses_1", &questions, "/tmp/proj/lib", &[None], &[false]);
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
    fn many_option_question() -> crate::opencode::client::QuestionInfo {
        let options = (0..5)
            .map(|i| crate::opencode::client::QuestionOption {
                label: format!("/a{}", i),
                description: String::new(),
            })
            .collect();
        crate::opencode::client::QuestionInfo {
            question: "选一个目录".into(),
            header: "目录".into(),
            options,
            multiple: None,
            custom: None,
        }
    }

    #[test]
    fn question_card_many_options_collapse_into_overflow() {
        let card = build_question_card(
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
        let card = build_question_card("que_1", "ses_1", &[q], "/tmp/proj/lib", &[None], &[false]);
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
        let questions = vec![crate::opencode::client::QuestionInfo {
            question: "q".into(),
            header: "h".into(),
            options: vec![crate::opencode::client::QuestionOption {
                label: "/a".into(),
                description: String::new(),
            }],
            multiple: None,
            custom: None, // default: custom answers allowed
        }];
        let card = build_question_card("que_1", "ses_1", &questions, "/tmp/proj/lib", &[None], &[false]);
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
        let questions = vec![crate::opencode::client::QuestionInfo {
            question: "q".into(),
            header: "h".into(),
            options: vec![crate::opencode::client::QuestionOption {
                label: "/a".into(),
                description: String::new(),
            }],
            multiple: None,
            custom: Some(false),
        }];
        let card = build_question_card("que_1", "ses_1", &questions, "/tmp/proj/lib", &[None], &[false]);
        let elements = card["body"]["elements"].as_array().unwrap();
        assert!(
            elements.iter().all(|e| e["tag"] != "form"),
            "custom-disabled question must not get an input form: {}",
            card
        );
    }

    #[test]
    fn multi_select_question_gets_confirm_button_and_custom_form() {
        let questions = vec![crate::opencode::client::QuestionInfo {
            question: "选择水果".into(),
            header: "水果".into(),
            options: vec![crate::opencode::client::QuestionOption {
                label: "苹果".into(),
                description: String::new(),
            }],
            multiple: Some(true),
            custom: None,
        }];
        let card = build_question_card("que_1", "ses_1", &questions, "/tmp/proj/lib", &[None], &[false]);
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
        let questions = vec![crate::opencode::client::QuestionInfo {
            question: "选择水果".into(),
            header: "水果".into(),
            options: vec![crate::opencode::client::QuestionOption {
                label: "苹果".into(),
                description: String::new(),
            }],
            multiple: Some(true),
            custom: None,
        }];
        // The selection holds an option and a multiline Custom Answer; the
        // custom label is not an option label, so it gets its own chip.
        let answered = vec![Some(vec!["苹果".to_string(), "自定\n答案".to_string()])];
        let card = build_question_card("que_1", "ses_1", &questions, "/tmp/proj/lib", &answered, &[false]);
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
        let questions = vec![crate::opencode::client::QuestionInfo {
            question: "选择目录".into(),
            header: "目录".into(),
            options: vec![crate::opencode::client::QuestionOption {
                label: "/a".into(),
                description: String::new(),
            }],
            multiple: None,
            custom: None,
        }];
        let answered = vec![Some(vec!["自定".to_string()])];
        let card = build_question_card("que_1", "ses_1", &questions, "/tmp/proj/lib", &answered, &[false]);
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
        let questions = vec![crate::opencode::client::QuestionInfo {
            question: "选择水果".into(),
            header: "水果".into(),
            options: vec![crate::opencode::client::QuestionOption {
                label: "苹果".into(),
                description: String::new(),
            }],
            multiple: Some(true),
            custom: None,
        }];
        // Confirmed (done) multi-select: no option buttons, no confirm, no form.
        let card = build_question_card(
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
        let mk = |q: &str, h: &str, multi: bool| crate::opencode::client::QuestionInfo {
            question: q.into(),
            header: h.into(),
            options: vec![crate::opencode::client::QuestionOption {
                label: "选项".into(),
                description: String::new(),
            }],
            multiple: if multi { Some(true) } else { None },
            custom: None,
        };
        let questions = vec![mk("选目录", "目录", false), mk("选水果", "水果", true)];
        let card = build_question_card(
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
    fn permission_card_carries_reply_payload() {
        let card = build_permission_card("ses_abc", "per_123", "AI 想要执行 bash", "/tmp/proj/lib");
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
        // The Auto-Accept toggle carries the session id so it can flip the flag.
        let autoaccept = values.iter().find(|v| v["reply"] == "autoaccept").unwrap();
        assert_eq!(autoaccept["session_id"], "ses_abc");
        assert_eq!(autoaccept["request_id"], "per_123");
    }
}
