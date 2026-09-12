use serde_json::json;

use super::card_shell;

/// One session entry of the `/switch` card: a full-width text row followed by
/// a button row underneath. The text row is its own `column_set` column (not a
/// weighted column squeezed beside the buttons), so the session label and
/// directory wrap naturally instead of being crushed into many narrow lines on
/// mobile. The button row is a `column_set` with `flex_mode: "bisect"` (two
/// equal columns, one per button) so the pair splits evenly on narrow
/// screens instead of overflowing: the primary button switches/adopts into the
/// current thread (`op: "adopt"`); a second "建话题接管" button
/// (`op: "topic_adopt"`) opens a new Feishu topic around the session
/// (ADR-0016). Each button carries the routing payload (action, op, thread_key,
/// target session id).
///
/// Schema 2.0 dropped the v1 `action` container (error 200861, "cards of
/// schema V2 no longer support this capability"), so buttons never live in an
/// `action` element — the button row is a `column_set` with one column per
/// button.
/// A full-width text row (schema-2.0 safe): a single weighted column holding a
/// markdown element. Shared by the `/switch` and `/dir` card rows.
fn card_text_row(text: &str) -> serde_json::Value {
    json!({
        "tag": "column_set",
        "flex_mode": "none",
        "columns": [
            {
                "tag": "column",
                "width": "weighted",
                "weight": 5,
                "vertical_align": "center",
                "elements": [ { "tag": "markdown", "content": text } ]
            }
        ]
    })
}

fn switch_card_row(
    text: &str,
    btn_text: &str,
    thread_key: &crate::config::ThreadKey,
    session_id: &str,
    scope: crate::bridge::command::SwitchScope,
) -> Vec<serde_json::Value> {
    let btn_column = |op: &str, content: &str| {
        json!({
            "tag": "column",
            "width": "auto",
            "vertical_align": "center",
            "elements": [
                {
                    "tag": "button",
                    "text": { "tag": "plain_text", "content": content },
                    "type": "default",
                    "value": {
                        "action": "switch",
                        "op": op,
                        "chat_id": thread_key.chat_id,
                        "thread_id": thread_key.thread_id,
                        "session_id": session_id,
                        "scope": scope.as_str(),
                    },
                }
            ],
        })
    };
    let text_row = card_text_row(text);
    let btn_row = json!({
        "tag": "column_set",
        "flex_mode": "bisect",
        "horizontal_spacing": "default",
        "columns": [ btn_column("adopt", btn_text), btn_column("topic_adopt", "建话题接管") ]
    });
    vec![text_row, btn_row]
}

/// Build the interactive `/switch` session card (ADR-0012, issue 04): a
/// search box, up to `MAX_SWITCH_ROWS` session rows (each with a
/// switch/adopt button), and a "＋new" footer button that creates a fresh
/// session in the current project (equivalent to `/new`). `keyword` is the
/// active filter (empty = all); `active_id`/`mapped_ids` drive the row
/// labels and buttons.
pub const MAX_SWITCH_ROWS: usize = 6;

pub fn build_switch_card(
    thread_key: &crate::config::ThreadKey,
    sessions: &[crate::opencode::client::SessionListInfo],
    keyword: &str,
    scope: crate::bridge::command::SwitchScope,
    current_dir: Option<&str>,
    active_id: Option<&str>,
    mapped_ids: &[String],
) -> serde_json::Value {
    let mut elements: Vec<serde_json::Value> = Vec::new();

    // Search form: an input + a submit button. The routing payload rides in the
    // button's `name` (form submits don't always deliver the button `value`),
    // and the typed keyword arrives as `form_value.search`.
    elements.push(json!({
        "tag": "form",
        "name": "switch_search",
        "elements": [
            {
                "tag": "input",
                "name": "search",
                // Multiline like the question card's custom-answer box: the
                // single-line input types poorly on PC and mobile alike. rows:1
                // starts it at one line for a keyword; auto_resize grows it as
                // the user types. The submitted text is trimmed before
                // filtering (see handler.rs) so stray newlines don't break the
                // match.
                "input_type": "multiline_text",
                "rows": 1,
                "auto_resize": true,
                "max_rows": 4,
                "placeholder": { "tag": "plain_text", "content": "🔍 搜索标题 / 目录 / ID" },
                // `default_value` (NOT `value`) is what prefills the box, so a
                // search that comes back empty keeps the keyword and the user can
                // tweak it instead of retyping. `value` is the passback-data
                // field and is never displayed — echoing keyword there left the
                // input blank on every re-render.
                "default_value": keyword,
                "max_length": 100,
                "width": "fill",
            },
            {
                "tag": "button",
                "text": { "tag": "plain_text", "content": "搜索" },
                "type": "primary",
                "form_action_type": "submit",
                "name": format!(
                    "switchsearch|{}|{}|{}",
                    thread_key.chat_id,
                    thread_key.thread_id,
                    scope.as_str()
                ),
                "value": {
                    "action": "switch",
                    "op": "search",
                    "chat_id": thread_key.chat_id,
                    "thread_id": thread_key.thread_id,
                    "scope": scope.as_str(),
                },
            },
        ],
    }));

    // The scope toggle: in `Directory` view show 全部, in `All` view show
    // 本目录 (ADR-0022). `All` from a fresh conversation has no directory, so
    // the 本目录 button is hidden there. The keyword rides along so a scoped
    // search survives the toggle.
    let toggle_btn = match scope {
        crate::bridge::command::SwitchScope::Directory => json!({
            "tag": "button",
            "text": { "tag": "plain_text", "content": "全部" },
            "type": "default",
            "value": {
                "action": "switch",
                "op": "scope",
                "scope": "all",
                "keyword": keyword,
                "chat_id": thread_key.chat_id,
                "thread_id": thread_key.thread_id,
            },
        }),
        crate::bridge::command::SwitchScope::All => json!({
            "tag": "button",
            "text": { "tag": "plain_text", "content": "本目录" },
            "type": "default",
            "value": {
                "action": "switch",
                "op": "scope",
                "scope": "dir",
                "keyword": keyword,
                "chat_id": thread_key.chat_id,
                "thread_id": thread_key.thread_id,
            },
        }),
    };
    if scope == crate::bridge::command::SwitchScope::Directory || current_dir.is_some() {
        elements.push(toggle_btn);
    }

    if sessions.is_empty() {
        elements.push(json!({ "tag": "markdown", "content": "_(无匹配会话)_" }));
    } else {
        let header = if keyword.is_empty() {
            match scope {
                crate::bridge::command::SwitchScope::Directory => {
                    format!(
                        "**{} 的会话**",
                        current_dir
                            .map(crate::bridge::command::dir_basename)
                            .unwrap_or_default()
                    )
                }
                crate::bridge::command::SwitchScope::All => "**最近会话**".to_string(),
            }
        } else {
            format!("**匹配 `{keyword}` 的会话**")
        };
        elements.push(json!({ "tag": "markdown", "content": header }));
        for s in sessions.iter().take(MAX_SWITCH_ROWS) {
            let label = crate::bridge::command::title_or_id_tail(s);
            // ADR-0022: only the active session is marked; the 本会话 ownership
            // marker on mapped-but-not-active rows is dropped.
            let text = if active_id == Some(s.id.as_str()) {
                format!(
                    "{label} · {} · {}\n_(active)_",
                    s.directory,
                    crate::bridge::command::id_tail(&s.id)
                )
            } else {
                format!(
                    "{label} · {} · {}",
                    s.directory,
                    crate::bridge::command::id_tail(&s.id)
                )
            };
            let btn = if active_id == Some(s.id.as_str()) {
                "✅ 当前"
            } else if mapped_ids.contains(&s.id) {
                "切换"
            } else {
                "接管"
            };
            elements.extend(switch_card_row(&text, btn, thread_key, &s.id, scope));
        }
    }

    // Footer: "＋new" creates a fresh session in the current project.
    elements.push(json!({
        "tag": "button",
        "text": { "tag": "plain_text", "content": "＋ 新建会话" },
        "type": "primary",
        "value": {
            "action": "switch",
            "op": "new",
            "chat_id": thread_key.chat_id,
            "thread_id": thread_key.thread_id,
        },
    }));

    card_shell("📂 会话管理", "blue", elements)
}

/// The confirmation card shown when a `/switch` card op targets a session owned
/// by ANOTHER chat. The row buttons deliberately carry no `--force` (ADR-0016),
/// so the occupied case used to dead-end in a Toast pointing at a text command
/// whose ID the card never displays. This card turns it into one more click:
/// `force_op` re-enters the handler with the full session id and steals the
/// mapping; `back` rebuilds the session list. `force_label` is the op-specific
/// verb (强制接管 / 强制建话题接管).
pub fn build_force_confirm_card(
    thread_key: &crate::config::ThreadKey,
    target: &crate::opencode::client::SessionListInfo,
    owner_name: &str,
    force_op: &str,
    force_label: &str,
    scope: crate::bridge::command::SwitchScope,
) -> serde_json::Value {
    let label = crate::bridge::command::title_or_id_tail(target);
    let back_btn = json!({
        "tag": "button",
        "text": { "tag": "plain_text", "content": "返回列表" },
        "type": "default",
        "value": {
            "action": "switch",
            "op": "back",
            "chat_id": thread_key.chat_id,
            "thread_id": thread_key.thread_id,
            "scope": scope.as_str(),
        },
    });
    let force_btn = json!({
        "tag": "button",
        "text": { "tag": "plain_text", "content": force_label },
        "type": "danger",
        "value": {
            "action": "switch",
            "op": force_op,
            "chat_id": thread_key.chat_id,
            "thread_id": thread_key.thread_id,
            "session_id": target.id,
            "scope": scope.as_str(),
        },
    });
    card_shell(
        "⚠️ 会话已被占用",
        "orange",
        vec![
            json!({
                "tag": "markdown",
                "content": format!(
                    "**{label}** 正被 **{owner_name}** 使用。\n`{}` · `{}`",
                    target.directory,
                    crate::bridge::command::id_tail(&target.id)
                )
            }),
            json!({
                "tag": "markdown",
                "content": "强制接管后，原来的聊天/话题会失去这个会话（需要重新选择）。"
            }),
            json!({
                "tag": "column_set",
                "flex_mode": "bisect",
                "horizontal_spacing": "default",
                "columns": [
                    {
                        "tag": "column",
                        "width": "auto",
                        "vertical_align": "center",
                        "elements": [ force_btn ]
                    },
                    {
                        "tag": "column",
                        "width": "auto",
                        "vertical_align": "center",
                        "elements": [ back_btn ]
                    }
                ]
            }),
        ],
    )
}

/// The `/dir` Recent Directories card (no-arg form): one directory per entry,
/// capped at [`MAX_SWITCH_ROWS`]. Each entry is two rows — a full-width text
/// row (directory path, marked `当前` when it is the thread's active session
/// directory) and a two-button row beneath it, mirroring the `/switch` card
/// layout (ADR-0025): 切换到这里 / ✅ 当前 re-roots the thread into that
/// directory (`op: "pick"`), 建话题 wraps a NEW session in a fresh topic
/// there (`op: "topic"`). Each button carries the routing payload (action,
/// op, thread_key, directory), so the ack routes the choice back to the right
/// thread. Schema-2.0 safe: no v1 `action` container (see `switch_card_row`).
pub fn build_dir_card(
    thread_key: &crate::config::ThreadKey,
    dirs: &[String],
    current_dir: Option<&str>,
) -> serde_json::Value {
    let mut elements: Vec<serde_json::Value> = Vec::new();

    if dirs.is_empty() {
        elements.push(json!({
            "tag": "markdown",
            "content": "_(还没有最近目录。用 `/dir <路径>` 或 `/new` 创建会话。)_"
        }));
    } else {
        elements.push(json!({ "tag": "markdown", "content": "**最近目录**" }));
        for dir in dirs.iter().take(MAX_SWITCH_ROWS) {
            let is_current = current_dir == Some(dir.as_str());
            let text = if is_current {
                format!("`{dir}`\n_(当前)_")
            } else {
                format!("`{dir}`")
            };
            let btn = if is_current {
                "✅ 当前"
            } else {
                "切换到这里"
            };
            elements.extend(dir_card_row(&text, btn, thread_key, dir));
        }
        // Truncated entries aren't lost: `/switch` adopts an EXISTING session
        // by directory, and `/new` then opens a fresh one in that project —
        // the two-step path to a new session in an overflow directory.
        if dirs.len() > MAX_SWITCH_ROWS {
            elements.push(json!({
                "tag": "markdown",
                "content": format!(
                    "_(还有 {} 个最近目录未显示。用 `/switch <路径>` 接管已有会话，再 `/new` 新建。)_",
                    dirs.len() - MAX_SWITCH_ROWS
                )
            }));
        }
    }

    card_shell("📂 最近目录", "blue", elements)
}

/// One `/dir` card entry: a full-width text row plus a two-button row beneath
/// it, mirroring the `/switch` card's row layout (ADR-0025): the left button
/// re-roots the thread into the directory (`op: "pick"`), the right "建话题"
/// button wraps a NEW session in a fresh topic there (`op: "topic"` — the card
/// equivalent of `/topic <dir>`; nested topic creation is rejected by the
/// action handler). Each button carries the routing payload (action, op,
/// thread_key, directory), so the ack routes back to the right thread.
/// Schema-2.0 safe: no v1 `action` container (see `switch_card_row`).
fn dir_card_row(
    text: &str,
    btn_text: &str,
    thread_key: &crate::config::ThreadKey,
    directory: &str,
) -> Vec<serde_json::Value> {
    let text_row = card_text_row(text);
    let btn_column = |op: &str, content: &str, btn_type: &str| {
        json!({
            "tag": "column",
            "width": "auto",
            "vertical_align": "center",
            "elements": [
                {
                    "tag": "button",
                    "text": { "tag": "plain_text", "content": content },
                    "type": btn_type,
                    "value": {
                        "action": "dir",
                        "op": op,
                        "chat_id": thread_key.chat_id,
                        "thread_id": thread_key.thread_id,
                        "directory": directory,
                    },
                }
            ],
        })
    };
    let btn_row = json!({
        "tag": "column_set",
        "flex_mode": "bisect",
        "horizontal_spacing": "default",
        "columns": [
            btn_column("pick", btn_text, "default"),
            btn_column("topic", "建话题", "default"),
        ]
    });
    vec![text_row, btn_row]
}

#[cfg(test)]
mod tests {
    use super::*;

    /// ADR-0025: every Recent Directories row carries both gestures — the
    /// re-root pick and the 建话题 topic opener (each with the routing payload
    /// and the directory).
    #[test]
    fn dir_card_rows_carry_pick_and_topic_buttons() {
        let key = crate::config::ThreadKey::new("chat_1".into(), "chat_1".into());
        let card = build_dir_card(&key, &["/work/a".to_string()], None);
        let s = card.to_string();
        assert!(s.contains("切换到这里"), "left button re-roots: {s}");
        assert!(s.contains("建话题"), "right button opens a topic: {s}");
        assert!(s.contains("\"op\":\"pick\""), "pick op present: {s}");
        assert!(s.contains("\"op\":\"topic\""), "topic op present: {s}");
        assert!(
            s.contains("\"directory\":\"/work/a\""),
            "directory rides along: {s}"
        );
    }

    /// The `/switch` session card must stay schema-V2-compatible: schema 2.0
    /// dropped the v1 `action` container (Feishu rejects the card with ErrCode
    /// 200861, "cards of schema V2 no longer support this capability"), which
    /// killed the `/switch` card (the 400 only landed in the log). Row buttons
    /// render as a nested `column_set` instead, one column per button.
    ///
    /// Each session entry is two rows: a full-width text row, then a button row
    /// beneath it — the text is NOT squeezed into a weighted column beside the
    /// buttons, which crushed the label/directory into many narrow wrapped
    /// lines on mobile.
    #[test]
    fn switch_card_has_no_schema_v2_unsupported_action_container() {
        let key = crate::config::ThreadKey::new("chat_1".into(), "chat_1".into());
        let sessions = vec![crate::opencode::client::SessionListInfo {
            id: "ses_alpha01".into(),
            title: "重写登录".into(),
            directory: "/work/auth".into(),
            parent_id: None,
            agent: None,
            model: None,
            time: None,
        }];
        let card = build_switch_card(
            &key,
            &sessions,
            "",
            crate::bridge::command::SwitchScope::Directory,
            Some("/work/auth"),
            None,
            &[],
        );
        let text = card.to_string();
        assert!(
            !text.contains("\"tag\":\"action\"") && !text.contains("\"tag\": \"action\""),
            "switch card must not use the schema-V2-unsupported action container: {text}"
        );
        let elements = card["body"]["elements"].as_array().unwrap();
        let rows: Vec<&serde_json::Value> = elements.iter().filter(|e| e["tag"] == "column_set").collect();
        assert_eq!(
            rows.len(),
            2,
            "one text row + one button row per session entry: {text}"
        );
        let text_row_columns = rows[0]["columns"].as_array().unwrap();
        assert_eq!(
            text_row_columns.len(),
            1,
            "text row is a single full-width column: {text}"
        );
        assert_eq!(
            text_row_columns[0]["elements"][0]["tag"], "markdown",
            "text row renders the session label/directory: {text}"
        );
        let btn_columns = rows[1]["columns"].as_array().unwrap();
        assert_eq!(btn_columns.len(), 2, "both buttons side by side: {text}");
        assert_eq!(btn_columns[0]["elements"][0]["tag"], "button");
        assert_eq!(btn_columns[1]["elements"][0]["tag"], "button");
    }

    /// A re-rendered `/switch` card after a search must keep the keyword in the
    /// box (`default_value`), so an empty result is tweakable instead of a
    /// retype. The prefill must live in `default_value` — `value` is the
    /// passback field and is never displayed.
    #[test]
    fn switch_card_search_input_echoes_keyword_in_default_value() {
        let key = crate::config::ThreadKey::new("chat_1".into(), "chat_1".into());
        let card = build_switch_card(
            &key,
            &[],
            "重写登录\n任务",
            crate::bridge::command::SwitchScope::Directory,
            Some("/work/auth"),
            None,
            &[],
        );
        let elements = card["body"]["elements"].as_array().unwrap();
        let form = elements
            .iter()
            .find(|e| e["tag"] == "form" && e["name"] == "switch_search")
            .expect("switch card has a search form");
        let input = form["elements"]
            .as_array()
            .unwrap()
            .iter()
            .find(|e| e["tag"] == "input")
            .expect("search form has an input");
        assert_eq!(input["default_value"], "重写登录\n任务");
        assert!(
            input.get("value").is_none(),
            "echo must not use the passback `value` field"
        );
        // Same compact-start contract as the question card's custom-answer
        // box: multiline, one row at rest, growing with the text.
        assert_eq!(input["input_type"], "multiline_text");
        assert_eq!(input["rows"], 1);
        assert_eq!(input["auto_resize"], true);
    }

    /// The `/dir` Recent Directories card must be schema-V2-compatible (no v1
    /// `action` container — ErrCode 200861), with one text row + one button row
    /// per directory, and each button carrying the pick routing payload.
    #[test]
    fn dir_card_has_no_schema_v2_unsupported_action_container() {
        let key = crate::config::ThreadKey::new("chat_1".into(), "chat_1".into());
        let dirs = vec!["/work/auth".to_string(), "/work/billing".to_string()];
        let card = build_dir_card(&key, &dirs, Some("/work/auth"));
        let text = card.to_string();
        assert!(
            !text.contains("\"tag\":\"action\"") && !text.contains("\"tag\": \"action\""),
            "dir card must not use the schema-V2-unsupported action container: {text}"
        );
        let elements = card["body"]["elements"].as_array().unwrap();
        let rows: Vec<&serde_json::Value> = elements.iter().filter(|e| e["tag"] == "column_set").collect();
        assert_eq!(
            rows.len(),
            4,
            "one text row + one button row per directory: {text}"
        );
        // The second entry's button carries the pick payload for its directory.
        let btn = &rows[3]["columns"][0]["elements"][0];
        assert_eq!(btn["tag"], "button");
        assert_eq!(btn["value"]["action"], "dir");
        assert_eq!(btn["value"]["op"], "pick");
        assert_eq!(btn["value"]["directory"], "/work/billing");
        assert_eq!(btn["value"]["chat_id"], "chat_1");
        assert_eq!(btn["value"]["thread_id"], "chat_1");
        // The current directory is marked, and its row reads "当前" not "切换".
        let current_text = rows[0]["columns"][0]["elements"][0]["content"].as_str().unwrap();
        assert!(
            current_text.contains("当前"),
            "current dir marked: {current_text}"
        );
        assert_eq!(
            rows[1]["columns"][0]["elements"][0]["text"]["content"], "✅ 当前",
            "current dir button reads 当前: {text}"
        );
        assert_eq!(
            rows[3]["columns"][0]["elements"][0]["text"]["content"], "切换到这里",
            "non-current dir button reads 切换到这里: {text}"
        );
    }

    /// An empty Recent Directories list renders a hint and no buttons.
    #[test]
    fn dir_card_empty_state_is_buttonless_hint() {
        let key = crate::config::ThreadKey::new("chat_1".into(), "chat_1".into());
        let card = build_dir_card(&key, &[], None);
        let text = card.to_string();
        assert!(text.contains("还没有最近目录"), "empty hint: {text}");
        assert!(
            !text.contains("\"tag\":\"button\"") && !text.contains("\"tag\": \"button\""),
            "empty dir card has no buttons: {text}"
        );
    }

    /// More than `MAX_SWITCH_ROWS` recent directories render only the most
    /// recent six — the rest are silently dropped (same cap as the `/switch`
    /// card).
    #[test]
    fn dir_card_caps_rows_at_max_switch_rows() {
        let key = crate::config::ThreadKey::new("chat_1".into(), "chat_1".into());
        let dirs: Vec<String> = (1..=9).map(|i| format!("/work/proj{i}")).collect();
        let card = build_dir_card(&key, &dirs, None);
        let text = card.to_string();
        let elements = card["body"]["elements"].as_array().unwrap();
        let rows: Vec<&serde_json::Value> = elements.iter().filter(|e| e["tag"] == "column_set").collect();
        assert_eq!(
            rows.len(),
            MAX_SWITCH_ROWS * 2,
            "one text row + one button row for each of the capped six: {text}"
        );
        assert!(text.contains("/work/proj1"), "most recent shown: {text}");
        assert!(text.contains("/work/proj6"), "sixth most recent shown: {text}");
        assert!(
            !text.contains("/work/proj7"),
            "seventh+ directory dropped: {text}"
        );
        assert!(
            text.contains("还有 3 个最近目录未显示"),
            "truncated count hints at the fallback: {text}"
        );
        assert!(
            text.contains("/switch") && text.contains("接管") && text.contains("/new"),
            "hint encodes the switch-then-new flow: {text}"
        );
    }

    /// A Recent Directories card that fits under the cap shows no truncation
    /// hint.
    #[test]
    fn dir_card_under_cap_has_no_truncation_hint() {
        let key = crate::config::ThreadKey::new("chat_1".into(), "chat_1".into());
        let dirs = vec!["/work/auth".to_string(), "/work/billing".to_string()];
        let card = build_dir_card(&key, &dirs, None);
        let text = card.to_string();
        assert!(
            !text.contains("还有") && !text.contains("未显示"),
            "no truncation hint under the cap: {text}"
        );
    }
}
