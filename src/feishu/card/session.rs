use serde_json::json;

use super::shell::card_shell;

/// The `/switch` card's list scope (ADR-0022). `Directory` filters the list to
/// the active session's directory (the card's default view); `All` shows the
/// whole shared store. The scope round-trips through the card's button values.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SwitchScope {
    Directory,
    All,
}

impl SwitchScope {
    /// The payload string (`"dir"` / `"all"`) carried on card buttons.
    pub fn as_str(self) -> &'static str {
        match self {
            SwitchScope::Directory => "dir",
            SwitchScope::All => "all",
        }
    }

    /// Parse a payload string; anything other than `"dir"` reads as `All`.
    pub fn parse(s: &str) -> SwitchScope {
        if s == "dir" {
            SwitchScope::Directory
        } else {
            SwitchScope::All
        }
    }
}

/// The list state a `/switch`-card adoption carries onto its Session Snapshot
/// (ADR-0052): the list's thread plus its active keyword/scope/page. The
/// snapshot's optional 「返回列表」 button rebuilds exactly this filtered
/// window, so an adoption that turns out wrong returns to the page it came
/// from. Every other snapshot source has no list to return to and passes
/// `None`.
#[derive(Clone)]
pub struct BackToList {
    pub thread_key: crate::config::ThreadKey,
    pub keyword: String,
    pub scope: SwitchScope,
    pub page: usize,
}

/// The 「返回列表」 button (ADR-0052) shared by the force-confirm card and the
/// adopt snapshot: a standalone schema-2.0-safe button whose `op: "back"`
/// rebuilds the `/switch` list with this filter and page.
pub(crate) fn back_to_list_button(back: &BackToList) -> serde_json::Value {
    json!({
        "tag": "button",
        "text": { "tag": "plain_text", "content": "返回列表" },
        "type": "default",
        "value": {
            "action": "switch",
            "op": "back",
            "chat_id": back.thread_key.chat_id,
            "thread_id": back.thread_key.thread_id,
            "scope": back.scope.as_str(),
            "keyword": back.keyword,
            "page": back.page,
        },
    })
}

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
/// target session id) plus the active `keyword`/`scope` and the clamped `page`,
/// so a row action rebuilds the same filtered window (ADR-0052).
///
/// Schema 2.0 dropped the v1 `action` container (error 200861, "cards of
/// schema V2 no longer support this capability"), so buttons never live in an
/// `action` element — the button row is a `column_set` with one column per
/// button.
/// A full-width text row (schema-2.0 safe): a single weighted column holding a
/// markdown element. Shared by the `/switch` and `/dir` card rows.
fn card_text_row(text: &str) -> serde_json::Value {
    let text = crate::feishu::card::sanitize::sanitize_markdown(text);
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
    scope: SwitchScope,
    keyword: &str,
    page: usize,
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
                        "keyword": keyword,
                        "page": page,
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

/// Rows a `/switch` or `/dir` list card shows per page (ADR-0052): the old
/// row cap becomes the page size. The builders clamp a requested page into
/// `[1, total_pages]` and slice the full filtered list themselves.
pub const CARD_PAGE_SIZE: usize = 6;

/// The three-column pagination control both list cards share (ADR-0052): a
/// 上一页 button, the 「第 x/y 页 · 共 N 个」 indicator, and a 下一页 button.
/// `page` is the already-clamped current page; the boundary buttons are
/// `disabled` (never hidden, so the layout stays stable). Every button's value
/// carries the routing payload plus the active filter (`keyword`, and `scope`
/// when given) and the TARGET page, so a flip rebuilds the same filtered view.
/// `None` when there is at most one page — the caller renders nothing.
fn pager_element(
    action: &str,
    thread_key: &crate::config::ThreadKey,
    keyword: &str,
    scope: Option<SwitchScope>,
    page: usize,
    total_pages: usize,
    total_items: usize,
) -> Option<serde_json::Value> {
    if total_pages <= 1 {
        return None;
    }
    let page_button = |content: &str, target: usize, disabled: bool| {
        let mut value = json!({
            "action": action,
            "op": "page",
            "chat_id": thread_key.chat_id,
            "thread_id": thread_key.thread_id,
            "keyword": keyword,
            "page": target,
        });
        if let Some(scope) = scope {
            value["scope"] = json!(scope.as_str());
        }
        json!({
            "tag": "column",
            "width": "auto",
            "vertical_align": "center",
            "elements": [
                {
                    "tag": "button",
                    "text": { "tag": "plain_text", "content": content },
                    "type": "default",
                    "disabled": disabled,
                    "value": value,
                }
            ],
        })
    };
    let indicator = json!({
        "tag": "column",
        "width": "weighted",
        "weight": 5,
        "vertical_align": "center",
        "elements": [
            {
                "tag": "markdown",
                "content": format!("第 {page}/{total_pages} 页 · 共 {total_items} 个"),
            }
        ],
    });
    Some(json!({
        "tag": "column_set",
        "flex_mode": "none",
        "horizontal_spacing": "default",
        "columns": [
            page_button("上一页", page - 1, page == 1),
            indicator,
            page_button("下一页", page + 1, page == total_pages),
        ],
    }))
}

/// Build the interactive `/switch` session card (ADR-0012, issue 04): a
/// search box, up to one page (`CARD_PAGE_SIZE`) of session rows (each with a
/// switch/adopt button), a pagination control below the rows when the filter
/// spans more than a page, and a "＋new" footer button that creates a fresh
/// session in the current project (equivalent to `/new`). `keyword` is the
/// active filter (empty = all); `page` is 1-based and clamped into
/// `[1, total_pages]`, so a stale page lands on the last page instead of
/// springing back to the first (ADR-0052); `active_id`/`mapped_ids` drive the
/// row labels and buttons.
#[allow(clippy::too_many_arguments)] // card builder: every knob is a first-class card axis
pub fn build_switch_card(
    thread_key: &crate::config::ThreadKey,
    sessions: &[crate::opencode::types::SessionListInfo],
    keyword: &str,
    scope: SwitchScope,
    page: usize,
    current_dir: Option<&str>,
    active_id: Option<&str>,
    mapped_ids: &[String],
) -> serde_json::Value {
    let total = sessions.len();
    let total_pages = total.div_ceil(CARD_PAGE_SIZE).max(1);
    let page = page.clamp(1, total_pages);
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
        SwitchScope::Directory => json!({
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
        SwitchScope::All => json!({
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
    if scope == SwitchScope::Directory || current_dir.is_some() {
        elements.push(toggle_btn);
    }

    if sessions.is_empty() {
        elements.push(json!({ "tag": "markdown", "content": "_(无匹配会话)_" }));
    } else {
        let header = if keyword.is_empty() {
            match scope {
                SwitchScope::Directory => {
                    format!(
                        "**{} 的会话**",
                        current_dir
                            .map(crate::bridge::display::dir_basename)
                            .unwrap_or_default()
                    )
                }
                SwitchScope::All => "**最近会话**".to_string(),
            }
        } else {
            format!("**匹配 `{keyword}` 的会话**")
        };
        elements.push(json!({
            "tag": "markdown",
            "content": crate::feishu::card::sanitize::sanitize_markdown(&header)
        }));
        let start = (page - 1) * CARD_PAGE_SIZE;
        for s in sessions.iter().skip(start).take(CARD_PAGE_SIZE) {
            let label = crate::bridge::display::title_or_id_tail(s);
            // ADR-0022: only the active session is marked; the 本会话 ownership
            // marker on mapped-but-not-active rows is dropped.
            let text = if active_id == Some(s.id.as_str()) {
                format!(
                    "{label} · {} · {}\n_(active)_",
                    s.directory,
                    crate::bridge::display::id_tail(&s.id)
                )
            } else {
                format!(
                    "{label} · {} · {}",
                    s.directory,
                    crate::bridge::display::id_tail(&s.id)
                )
            };
            let btn = if active_id == Some(s.id.as_str()) {
                "✅ 当前"
            } else if mapped_ids.contains(&s.id) {
                "切换"
            } else {
                "接管"
            };
            elements.extend(switch_card_row(
                &text, btn, thread_key, &s.id, scope, keyword, page,
            ));
        }
    }

    // Pagination (ADR-0052) below the rows and above the ＋new footer.
    if let Some(pager) = pager_element(
        "switch",
        thread_key,
        keyword,
        Some(scope),
        page,
        total_pages,
        total,
    ) {
        elements.push(pager);
    }

    // Footer: "＋new" creates a fresh session in the current project. It keeps
    // the active filter (ADR-0052), so the refreshed list stays where it was.
    elements.push(json!({
        "tag": "button",
        "text": { "tag": "plain_text", "content": "＋ 新建会话" },
        "type": "primary",
        "value": {
            "action": "switch",
            "op": "new",
            "chat_id": thread_key.chat_id,
            "thread_id": thread_key.thread_id,
            "keyword": keyword,
            "scope": scope.as_str(),
            "page": page,
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
/// verb (强制接管 / 强制建话题接管). Both buttons carry the active
/// `keyword`/`scope` and clamped `page` (ADR-0052), so the round trip lands
/// back on the same filtered window.
#[allow(clippy::too_many_arguments)] // card builder: every knob is a first-class card axis
pub fn build_force_confirm_card(
    thread_key: &crate::config::ThreadKey,
    target: &crate::opencode::types::SessionListInfo,
    owner_name: &str,
    force_op: &str,
    force_label: &str,
    scope: SwitchScope,
    keyword: &str,
    page: usize,
) -> serde_json::Value {
    let label = crate::bridge::display::title_or_id_tail(target);
    let back_btn = back_to_list_button(&BackToList {
        thread_key: thread_key.clone(),
        keyword: keyword.to_string(),
        scope,
        page,
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
            "keyword": keyword,
            "page": page,
        },
    });
    card_shell(
        "⚠️ 会话已被占用",
        "orange",
        vec![
            json!({
                "tag": "markdown",
                "content": crate::feishu::card::sanitize::sanitize_markdown(&format!(
                    "**{label}** 正被 **{owner_name}** 使用。\n`{}` · `{}`",
                    target.directory,
                    crate::bridge::display::id_tail(&target.id)
                ))
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

/// The `/dir` Recent Directories card (no-arg form): one page of directories —
/// [`CARD_PAGE_SIZE`] entries — with a pagination control below the rows
/// (ADR-0052). `page` is 1-based and clamped into `[1, total_pages]`, so a
/// stale page lands on the last page instead of springing back to the first.
/// Each entry is two rows — a full-width text row (directory path, marked
/// `当前` when it is the thread's active session directory) and a two-button
/// row beneath it, mirroring the `/switch` card layout (ADR-0025): 切换到这里 /
/// ✅ 当前 re-roots the thread into that directory (`op: "pick"`), 建话题 wraps
/// a NEW session in a fresh topic there (`op: "topic"`). Each button carries
/// the routing payload (action, op, thread_key, directory) plus the active
/// `keyword` and clamped `page`, so a row action rebuilds the same filtered
/// view (ADR-0052). Schema-2.0 safe: no v1 `action` container (see
/// `switch_card_row`).
///
/// ADR-0051: a search form over the paths renders when the list outgrows a
/// page or `keyword` is non-empty (so a narrowed result stays refinable); the
/// header and the empty state follow the keyword.
pub fn build_dir_card(
    thread_key: &crate::config::ThreadKey,
    dirs: &[String],
    current_dir: Option<&str>,
    keyword: &str,
    page: usize,
) -> serde_json::Value {
    let total = dirs.len();
    let total_pages = total.div_ceil(CARD_PAGE_SIZE).max(1);
    let page = page.clamp(1, total_pages);
    let mut elements: Vec<serde_json::Value> = Vec::new();

    if total > CARD_PAGE_SIZE || !keyword.is_empty() {
        elements.push(dir_search_form(thread_key, keyword));
    }

    if dirs.is_empty() {
        let hint = if keyword.is_empty() {
            "_(还没有最近目录。用 `/dir <路径>` 或 `/new` 创建会话。)_"
        } else {
            "_(无匹配目录)_"
        };
        elements.push(json!({ "tag": "markdown", "content": hint }));
    } else {
        let header = if keyword.is_empty() {
            "**最近目录**".to_string()
        } else {
            format!("**匹配 `{keyword}` 的目录**")
        };
        elements.push(json!({
            "tag": "markdown",
            "content": crate::feishu::card::sanitize::sanitize_markdown(&header)
        }));
        let start = (page - 1) * CARD_PAGE_SIZE;
        for dir in dirs.iter().skip(start).take(CARD_PAGE_SIZE) {
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
            elements.extend(dir_card_row(&text, btn, thread_key, dir, keyword, page));
        }
    }

    // Pagination (ADR-0052) below the rows — the old overflow hints are gone.
    if let Some(pager) = pager_element("dir", thread_key, keyword, None, page, total_pages, total) {
        elements.push(pager);
    }

    card_shell("📂 最近目录", "blue", elements)
}

/// The `/dir` card's search form (ADR-0051), mirroring the `/switch` card's:
/// the routing payload rides in the submit button's `name` (form submits don't
/// always deliver the button `value`), and the typed keyword arrives as
/// `form_value.search`. `default_value` echoes the active keyword so a
/// re-render never blanks the box.
fn dir_search_form(thread_key: &crate::config::ThreadKey, keyword: &str) -> serde_json::Value {
    json!({
        "tag": "form",
        "name": "dir_search",
        "elements": [
            {
                "tag": "input",
                "name": "search",
                // Multiline like the switch card's search box and the question
                // card's custom answer: one row at rest, growing with the text.
                "input_type": "multiline_text",
                "rows": 1,
                "auto_resize": true,
                "max_rows": 4,
                "placeholder": { "tag": "plain_text", "content": "🔍 搜索目录路径" },
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
                    "dirsearch|{}|{}",
                    thread_key.chat_id,
                    thread_key.thread_id
                ),
                "value": {
                    "action": "dir",
                    "op": "search",
                    "chat_id": thread_key.chat_id,
                    "thread_id": thread_key.thread_id,
                },
            },
        ],
    })
}

/// One `/dir` card entry: a full-width text row plus a two-button row beneath
/// it, mirroring the `/switch` card's row layout (ADR-0025): the left button
/// re-roots the thread into the directory (`op: "pick"`), the right "建话题"
/// button wraps a NEW session in a fresh topic there (`op: "topic"` — the card
/// equivalent of `/topic <dir>`; nested topic creation is rejected by the
/// action handler). Each button carries the routing payload (action, op,
/// thread_key, directory) plus the active `keyword` and clamped `page`, so a
/// row action rebuilds the same filtered view (ADR-0052). Schema-2.0 safe: no
/// v1 `action` container (see `switch_card_row`).
fn dir_card_row(
    text: &str,
    btn_text: &str,
    thread_key: &crate::config::ThreadKey,
    directory: &str,
    keyword: &str,
    page: usize,
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
                        "keyword": keyword,
                        "page": page,
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
        let card = build_dir_card(&key, &["/work/a".to_string()], None, "", 1);
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
        let sessions = vec![crate::opencode::types::SessionListInfo {
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
            SwitchScope::Directory,
            1,
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
            SwitchScope::Directory,
            1,
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

    /// ADR-0052: the second page renders the next window of sessions, the
    /// pager reports the position, and it sits between the row list and the
    /// ＋新建 footer.
    #[test]
    fn switch_card_page_two_windows_the_rows_and_labels_the_pager() {
        let key = crate::config::ThreadKey::new("chat_1".into(), "chat_1".into());
        let sessions = switch_sessions(13);
        let card = build_switch_card(&key, &sessions, "", SwitchScope::All, 2, None, None, &[]);
        let text = card.to_string();
        for i in 2..=7 {
            assert!(
                text.contains(&format!("项目{i} ·")),
                "page 2 shows 项目{i}: {text}"
            );
        }
        assert!(
            !text.contains("项目8 ·"),
            "page 1's last row is off page 2: {text}"
        );
        assert!(!text.contains("项目1 ·"), "page 3's row is off page 2: {text}");
        let pager = switch_pager(&card).expect("a multi-page list renders the pager");
        assert_eq!(
            pager["columns"][1]["elements"][0]["content"], "第 2/3 页 · 共 13 个",
            "indicator names the position and the total: {text}"
        );
        let elements = card["body"]["elements"].as_array().unwrap();
        let pager_idx = elements
            .iter()
            .position(|e| e["columns"][0]["elements"][0]["value"]["op"] == "page")
            .expect("pager is a top-level element");
        let new_idx = elements
            .iter()
            .position(|e| e["tag"] == "button" && e["value"]["op"] == "new")
            .expect("＋新建 footer is a top-level element");
        let last_row_idx = elements
            .iter()
            .rposition(|e| {
                e["tag"] == "column_set"
                    && e["columns"]
                        .as_array()
                        .is_some_and(|cols| cols.iter().any(|c| c["elements"][0]["value"]["op"] == "adopt"))
            })
            .expect("a session row is a top-level element");
        assert!(
            last_row_idx < pager_idx && pager_idx < new_idx,
            "pager sits between the rows and the footer: {text}"
        );
    }

    /// ADR-0052: the boundary button is disabled, never hidden — 上一页 on the
    /// first page, 下一页 on the last.
    #[test]
    fn switch_card_pager_disables_the_boundary_buttons() {
        let key = crate::config::ThreadKey::new("chat_1".into(), "chat_1".into());
        let sessions = switch_sessions(13);

        let first = switch_pager(&build_switch_card(
            &key,
            &sessions,
            "",
            SwitchScope::All,
            1,
            None,
            None,
            &[],
        ))
        .unwrap();
        let columns = first["columns"].as_array().unwrap();
        assert_eq!(columns[0]["elements"][0]["text"]["content"], "上一页");
        assert_eq!(
            columns[0]["elements"][0]["disabled"], true,
            "page 1 disables 上一页"
        );
        assert_eq!(
            columns[2]["elements"][0]["disabled"], false,
            "page 1 keeps 下一页 live"
        );

        let last = switch_pager(&build_switch_card(
            &key,
            &sessions,
            "",
            SwitchScope::All,
            3,
            None,
            None,
            &[],
        ))
        .unwrap();
        let columns = last["columns"].as_array().unwrap();
        assert_eq!(
            columns[0]["elements"][0]["disabled"], false,
            "the last page keeps 上一页 live"
        );
        assert_eq!(
            columns[2]["elements"][0]["disabled"], true,
            "the last page disables 下一页"
        );
    }

    /// ADR-0052: a single page (and the empty list) renders no pager at all.
    #[test]
    fn switch_card_single_page_renders_no_pager() {
        let key = crate::config::ThreadKey::new("chat_1".into(), "chat_1".into());
        let six = switch_sessions(6);
        assert!(
            switch_pager(&build_switch_card(
                &key,
                &six,
                "",
                SwitchScope::All,
                1,
                None,
                None,
                &[]
            ))
            .is_none(),
            "exactly one page hides the pager"
        );
        let seven = switch_sessions(7);
        assert!(
            switch_pager(&build_switch_card(
                &key,
                &seven,
                "",
                SwitchScope::All,
                1,
                None,
                None,
                &[]
            ))
            .is_some(),
            "over one page the pager appears"
        );
        assert!(
            switch_pager(&build_switch_card(
                &key,
                &[],
                "",
                SwitchScope::All,
                1,
                None,
                None,
                &[]
            ))
            .is_none(),
            "the empty list has nothing to page through"
        );
    }

    /// ADR-0052: a stale/out-of-range page is clamped to the last page (never
    /// sprung back to the first); page 0 is the first page.
    #[test]
    fn switch_card_clamps_an_out_of_range_page_into_range() {
        let key = crate::config::ThreadKey::new("chat_1".into(), "chat_1".into());
        let sessions = switch_sessions(13);

        let stale = build_switch_card(&key, &sessions, "", SwitchScope::All, 99, None, None, &[]);
        let text = stale.to_string();
        assert!(
            text.contains("项目1 ·") && !text.contains("项目7 ·"),
            "clamped to the last page: {text}"
        );
        let pager = switch_pager(&stale).unwrap();
        assert_eq!(
            pager["columns"][1]["elements"][0]["content"], "第 3/3 页 · 共 13 个",
            "the clamped page is reported: {text}"
        );

        let zero = build_switch_card(&key, &sessions, "", SwitchScope::All, 0, None, None, &[]);
        let text = zero.to_string();
        assert!(
            text.contains("项目13 ·") && !text.contains("项目7 ·"),
            "page 0 reads as page 1: {text}"
        );
        let pager = switch_pager(&zero).unwrap();
        assert_eq!(
            pager["columns"][1]["elements"][0]["content"], "第 1/3 页 · 共 13 个",
            "page 0 reports as page 1: {text}"
        );
    }

    /// ADR-0052: every row button carries the active keyword, scope and the
    /// clamped page, so adopt/topic_adopt rebuild the same filtered window.
    #[test]
    fn switch_card_row_buttons_carry_the_filter_and_page() {
        let key = crate::config::ThreadKey::new("chat_1".into(), "chat_1".into());
        let sessions = switch_sessions(13);
        let card = build_switch_card(&key, &sessions, "proj", SwitchScope::All, 2, None, None, &[]);
        let row = switch_row(&card, "ses_p7").expect("page 2 holds the seventh session");
        for (i, op) in ["adopt", "topic_adopt"].iter().enumerate() {
            let btn = &row["columns"][i]["elements"][0];
            assert_eq!(btn["value"]["op"], *op, "left adopt / right topic_adopt: {btn}");
            assert_eq!(btn["value"]["session_id"], "ses_p7");
            assert_eq!(btn["value"]["keyword"], "proj");
            assert_eq!(btn["value"]["scope"], "all");
            assert_eq!(btn["value"]["page"], 2);
        }
    }

    /// ADR-0052: the pager buttons carry the routing payload, the active
    /// keyword/scope and the TARGET page (clamped).
    #[test]
    fn switch_card_pager_buttons_carry_the_filter_and_target_page() {
        let key = crate::config::ThreadKey::new("chat_1".into(), "chat_1".into());
        let sessions = switch_sessions(13);
        let card = build_switch_card(&key, &sessions, "proj", SwitchScope::All, 2, None, None, &[]);
        let pager = switch_pager(&card).unwrap();
        let columns = pager["columns"].as_array().unwrap();

        let prev = &columns[0]["elements"][0];
        assert_eq!(prev["value"]["action"], "switch");
        assert_eq!(prev["value"]["op"], "page");
        assert_eq!(prev["value"]["chat_id"], "chat_1");
        assert_eq!(prev["value"]["thread_id"], "chat_1");
        assert_eq!(prev["value"]["keyword"], "proj");
        assert_eq!(prev["value"]["scope"], "all");
        assert_eq!(prev["value"]["page"], 1, "上一页 targets page - 1");
        let next = &columns[2]["elements"][0];
        assert_eq!(next["value"]["keyword"], "proj");
        assert_eq!(next["value"]["scope"], "all");
        assert_eq!(next["value"]["page"], 3, "下一页 targets page + 1");
    }

    /// ADR-0052: the ＋新建 footer keeps the active keyword/scope/page, so the
    /// refreshed list stays where it was.
    #[test]
    fn switch_card_new_button_carries_the_filter_and_page() {
        let key = crate::config::ThreadKey::new("chat_1".into(), "chat_1".into());
        let sessions = switch_sessions(13);
        let card = build_switch_card(&key, &sessions, "proj", SwitchScope::All, 2, None, None, &[]);
        let new_btn = card["body"]["elements"]
            .as_array()
            .unwrap()
            .iter()
            .find(|e| e["tag"] == "button" && e["value"]["op"] == "new")
            .expect("switch card has a ＋新建 footer");
        assert_eq!(new_btn["value"]["keyword"], "proj");
        assert_eq!(new_btn["value"]["scope"], "all");
        assert_eq!(new_btn["value"]["page"], 2);
    }

    /// ADR-0052: both force-confirm buttons (强制接管 / 返回列表) carry the
    /// active keyword/scope/page so the round trip lands on the same window.
    #[test]
    fn force_confirm_card_buttons_carry_the_filter_and_page() {
        let key = crate::config::ThreadKey::new("chat_1".into(), "chat_1".into());
        let sessions = switch_sessions(1);
        let card = build_force_confirm_card(
            &key,
            &sessions[0],
            "隔壁群",
            "force_adopt",
            "强制接管",
            SwitchScope::All,
            "proj",
            2,
        );
        let columns = card["body"]["elements"]
            .as_array()
            .unwrap()
            .iter()
            .find(|e| e["tag"] == "column_set")
            .expect("force-confirm card has the button row")["columns"]
            .as_array()
            .unwrap();
        for (col, op) in [(&columns[0], "force_adopt"), (&columns[1], "back")] {
            let value = &col["elements"][0]["value"];
            assert_eq!(value["op"], op);
            assert_eq!(value["keyword"], "proj");
            assert_eq!(value["scope"], "all");
            assert_eq!(value["page"], 2);
        }
    }

    /// `n` sessions 项目N rooted in `/work/projN`, in display order (most
    /// recently active first) — the builder slices what its caller hands it.
    fn switch_sessions(n: i64) -> Vec<crate::opencode::types::SessionListInfo> {
        (1..=n)
            .rev()
            .map(|i| crate::opencode::types::SessionListInfo {
                id: format!("ses_p{i}"),
                title: format!("项目{i}"),
                directory: format!("/work/proj{i}"),
                parent_id: None,
                agent: None,
                model: None,
                time: None,
            })
            .collect()
    }

    /// The `/switch` pager: the three-column `column_set` whose buttons carry
    /// `op: "page"` (the row button rows carry adopt/topic_adopt instead).
    fn switch_pager(card: &serde_json::Value) -> Option<serde_json::Value> {
        card["body"]["elements"]
            .as_array()?
            .iter()
            .find(|e| {
                e["tag"] == "column_set"
                    && e["columns"]
                        .as_array()
                        .is_some_and(|cols| cols.iter().any(|c| c["elements"][0]["value"]["op"] == "page"))
            })
            .cloned()
    }

    /// The two-column button row whose left button targets `session_id`.
    fn switch_row(card: &serde_json::Value, session_id: &str) -> Option<serde_json::Value> {
        card["body"]["elements"]
            .as_array()?
            .iter()
            .find(|e| {
                e["tag"] == "column_set"
                    && e["columns"][0]["elements"][0]["value"]["session_id"] == session_id
            })
            .cloned()
    }

    /// The `/dir` Recent Directories card must be schema-V2-compatible (no v1
    /// `action` container — ErrCode 200861), with one text row + one button row
    /// per directory, and each button carrying the pick routing payload.
    #[test]
    fn dir_card_has_no_schema_v2_unsupported_action_container() {
        let key = crate::config::ThreadKey::new("chat_1".into(), "chat_1".into());
        let dirs = vec!["/work/auth".to_string(), "/work/billing".to_string()];
        let card = build_dir_card(&key, &dirs, Some("/work/auth"), "", 1);
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
        let card = build_dir_card(&key, &[], None, "", 1);
        let text = card.to_string();
        assert!(text.contains("还没有最近目录"), "empty hint: {text}");
        assert!(
            !text.contains("\"tag\":\"button\"") && !text.contains("\"tag\": \"button\""),
            "empty dir card has no buttons: {text}"
        );
    }

    /// ADR-0052: a second page renders the next window of directories, and the
    /// pager reports the position (`第 x/y 页 · 共 N 个`).
    #[test]
    fn dir_card_page_two_windows_the_rows_and_labels_the_pager() {
        let key = crate::config::ThreadKey::new("chat_1".into(), "chat_1".into());
        let dirs: Vec<String> = (1..=13).map(|i| format!("/work/proj{i}")).collect();
        let card = build_dir_card(&key, &dirs, None, "", 2);
        let text = card.to_string();
        for i in 7..=12 {
            assert!(
                text.contains(&format!("/work/proj{i}")),
                "page 2 shows proj{i}: {text}"
            );
        }
        assert!(
            !text.contains("/work/proj6"),
            "page 1's last row is off page 2: {text}"
        );
        assert!(
            !text.contains("/work/proj13"),
            "page 3's row is off page 2: {text}"
        );
        let pager = dir_pager(&card).expect("a multi-page list renders the pager");
        let columns = pager["columns"].as_array().unwrap();
        assert_eq!(
            columns[1]["elements"][0]["content"], "第 2/3 页 · 共 13 个",
            "indicator names the position and the total: {text}"
        );
    }

    /// ADR-0052: the boundary button is disabled, never hidden — 上一页 on the
    /// first page, 下一页 on the last.
    #[test]
    fn dir_card_pager_disables_the_boundary_buttons() {
        let key = crate::config::ThreadKey::new("chat_1".into(), "chat_1".into());
        let dirs: Vec<String> = (1..=13).map(|i| format!("/work/proj{i}")).collect();

        let first = dir_pager(&build_dir_card(&key, &dirs, None, "", 1)).unwrap();
        let columns = first["columns"].as_array().unwrap();
        assert_eq!(columns[0]["elements"][0]["text"]["content"], "上一页");
        assert_eq!(
            columns[0]["elements"][0]["disabled"], true,
            "page 1 disables 上一页"
        );
        assert_eq!(
            columns[2]["elements"][0]["disabled"], false,
            "page 1 keeps 下一页 live"
        );

        let last = dir_pager(&build_dir_card(&key, &dirs, None, "", 3)).unwrap();
        let columns = last["columns"].as_array().unwrap();
        assert_eq!(
            columns[0]["elements"][0]["disabled"], false,
            "the last page keeps 上一页 live"
        );
        assert_eq!(
            columns[2]["elements"][0]["disabled"], true,
            "the last page disables 下一页"
        );
    }

    /// ADR-0052: a single page (and the empty list) renders no pager at all.
    #[test]
    fn dir_card_single_page_renders_no_pager() {
        let key = crate::config::ThreadKey::new("chat_1".into(), "chat_1".into());
        let six: Vec<String> = (1..=6).map(|i| format!("/work/p{i}")).collect();
        assert!(
            dir_pager(&build_dir_card(&key, &six, None, "", 1)).is_none(),
            "exactly one page hides the pager"
        );
        let seven: Vec<String> = (1..=7).map(|i| format!("/work/p{i}")).collect();
        assert!(
            dir_pager(&build_dir_card(&key, &seven, None, "", 1)).is_some(),
            "over one page the pager appears"
        );
        assert!(
            dir_pager(&build_dir_card(&key, &[], None, "", 1)).is_none(),
            "the empty list has nothing to page through"
        );
    }

    /// ADR-0052: a stale/out-of-range page is clamped to the last page (never
    /// sprung back to the first); page 0 is the first page.
    #[test]
    fn dir_card_clamps_an_out_of_range_page_into_range() {
        let key = crate::config::ThreadKey::new("chat_1".into(), "chat_1".into());
        let dirs: Vec<String> = (1..=13).map(|i| format!("/work/proj{i}")).collect();

        let stale = build_dir_card(&key, &dirs, None, "", 99);
        let text = stale.to_string();
        assert!(
            text.contains("/work/proj13") && !text.contains("/work/proj12"),
            "clamped to the last page: {text}"
        );
        let pager = dir_pager(&stale).unwrap();
        assert_eq!(
            pager["columns"][1]["elements"][0]["content"], "第 3/3 页 · 共 13 个",
            "the clamped page is reported: {text}"
        );

        let zero = build_dir_card(&key, &dirs, None, "", 0);
        let text = zero.to_string();
        assert!(
            text.contains("/work/proj1") && !text.contains("/work/proj7"),
            "page 0 reads as page 1: {text}"
        );
        let pager = dir_pager(&zero).unwrap();
        assert_eq!(
            pager["columns"][1]["elements"][0]["content"], "第 1/3 页 · 共 13 个",
            "page 0 reports as page 1: {text}"
        );
    }

    /// ADR-0052: every row button carries the active keyword and the clamped
    /// page, so pick/topic rebuild the same filtered window.
    #[test]
    fn dir_card_row_buttons_carry_the_keyword_and_page() {
        let key = crate::config::ThreadKey::new("chat_1".into(), "chat_1".into());
        let dirs: Vec<String> = (1..=7).map(|i| format!("/work/proj{i}")).collect();
        let card = build_dir_card(&key, &dirs, None, "proj", 2);
        let row = dir_row(&card, "/work/proj7").expect("page 2 holds the seventh directory");
        for (i, op) in ["pick", "topic"].iter().enumerate() {
            let btn = &row["columns"][i]["elements"][0];
            assert_eq!(btn["value"]["op"], *op, "left pick / right topic: {btn}");
            assert_eq!(btn["value"]["directory"], "/work/proj7");
            assert_eq!(btn["value"]["keyword"], "proj");
            assert_eq!(btn["value"]["page"], 2);
        }
    }

    /// ADR-0052: the pager buttons carry the routing payload, the active
    /// keyword and the TARGET page (clamped).
    #[test]
    fn dir_card_pager_buttons_carry_the_filter_and_target_page() {
        let key = crate::config::ThreadKey::new("chat_1".into(), "chat_1".into());
        let dirs: Vec<String> = (1..=13).map(|i| format!("/work/proj{i}")).collect();
        let card = build_dir_card(&key, &dirs, None, "proj", 2);
        let pager = dir_pager(&card).unwrap();
        let columns = pager["columns"].as_array().unwrap();

        let prev = &columns[0]["elements"][0];
        assert_eq!(prev["value"]["action"], "dir");
        assert_eq!(prev["value"]["op"], "page");
        assert_eq!(prev["value"]["chat_id"], "chat_1");
        assert_eq!(prev["value"]["thread_id"], "chat_1");
        assert_eq!(prev["value"]["keyword"], "proj");
        assert_eq!(prev["value"]["page"], 1, "上一页 targets page - 1");
        let next = &columns[2]["elements"][0];
        assert_eq!(next["value"]["keyword"], "proj");
        assert_eq!(next["value"]["page"], 3, "下一页 targets page + 1");
    }

    /// ADR-0052 replaced the old 「还有 N 个…」 overflow hints with the pager;
    /// neither the unfiltered nor the filtered copy renders anymore.
    #[test]
    fn dir_card_paginates_instead_of_hinting_at_overflow() {
        let key = crate::config::ThreadKey::new("chat_1".into(), "chat_1".into());
        let dirs: Vec<String> = (1..=13).map(|i| format!("/work/proj{i}")).collect();
        let unfiltered = build_dir_card(&key, &dirs, None, "", 1).to_string();
        assert!(
            !unfiltered.contains("还有") && !unfiltered.contains("未显示"),
            "the overflow hint is gone: {unfiltered}"
        );
        let filtered = build_dir_card(&key, &dirs, None, "work", 1).to_string();
        assert!(
            !filtered.contains("请细化关键词"),
            "the refine hint is gone: {filtered}"
        );
        assert!(
            !unfiltered.contains("/switch") && !filtered.contains("/switch"),
            "the old switch-then-new fallback is gone"
        );
    }

    /// A Recent Directories card that fits under the cap shows no truncation
    /// hint and no search form.
    #[test]
    fn dir_card_under_cap_has_no_truncation_hint() {
        let key = crate::config::ThreadKey::new("chat_1".into(), "chat_1".into());
        let dirs = vec!["/work/auth".to_string(), "/work/billing".to_string()];
        let card = build_dir_card(&key, &dirs, None, "", 1);
        let text = card.to_string();
        assert!(
            !text.contains("还有") && !text.contains("未显示"),
            "no truncation hint under the cap: {text}"
        );
        assert!(
            dir_search_form(&card).is_none(),
            "a short list stays form-free: {text}"
        );
    }

    /// ADR-0051: the search form appears only when the list outgrows the row
    /// budget — or a keyword is already active, so a narrowed result stays
    /// refinable and clearable.
    #[test]
    fn dir_card_search_form_appears_over_the_cap_or_with_a_keyword() {
        let key = crate::config::ThreadKey::new("chat_1".into(), "chat_1".into());
        let short: Vec<String> = (1..=2).map(|i| format!("/work/p{i}")).collect();
        let long: Vec<String> = (1..=7).map(|i| format!("/work/p{i}")).collect();
        assert!(
            dir_search_form(&build_dir_card(&key, &short, None, "", 1)).is_none(),
            "two directories do not need a search box"
        );
        assert!(
            dir_search_form(&build_dir_card(&key, &long, None, "", 1)).is_some(),
            "over the cap the search box appears"
        );
        assert!(
            dir_search_form(&build_dir_card(&key, &short, None, "p1", 1)).is_some(),
            "an active keyword keeps the box under the cap"
        );
    }

    /// The dir search input echoes the keyword in `default_value` (never the
    /// passback `value`), and the submit button's `name` encodes the routing
    /// the WS extractor rebuilds when `value` is missing.
    #[test]
    fn dir_card_search_input_echoes_keyword_and_encodes_routing() {
        let key = crate::config::ThreadKey::new("chat_1".into(), "chat_1".into());
        let card = build_dir_card(&key, &["/work/auth".to_string()], None, "auth\nwork", 1);
        let form = dir_search_form(&card).expect("keyword keeps the form");
        assert_eq!(form["name"], "dir_search");
        let input = form["elements"]
            .as_array()
            .unwrap()
            .iter()
            .find(|e| e["tag"] == "input")
            .expect("search form has an input");
        assert_eq!(input["default_value"], "auth\nwork");
        assert!(
            input.get("value").is_none(),
            "echo must not use the passback `value` field"
        );
        assert_eq!(input["input_type"], "multiline_text");
        assert_eq!(input["rows"], 1);
        assert_eq!(input["auto_resize"], true);
        let submit = form["elements"]
            .as_array()
            .unwrap()
            .iter()
            .find(|e| e["tag"] == "button")
            .expect("search form has a submit button");
        assert_eq!(submit["name"], "dirsearch|chat_1|chat_1");
        assert_eq!(submit["value"]["action"], "dir");
        assert_eq!(submit["value"]["op"], "search");
        assert_eq!(submit["value"]["chat_id"], "chat_1");
        assert_eq!(submit["value"]["thread_id"], "chat_1");
    }

    /// A keyword that matches nothing shows the no-match hint (not the
    /// first-run hint); a matching keyword names itself in the header.
    #[test]
    fn dir_card_search_empty_state_and_header() {
        let key = crate::config::ThreadKey::new("chat_1".into(), "chat_1".into());
        let empty = build_dir_card(&key, &[], None, "nope", 1);
        let text = empty.to_string();
        assert!(text.contains("无匹配目录"), "no-match hint: {text}");
        assert!(
            !text.contains("还没有最近目录"),
            "the first-run hint is not reused for a search: {text}"
        );

        let hit = build_dir_card(&key, &["/work/auth".to_string()], None, "auth", 1);
        let text = hit.to_string();
        assert!(
            text.contains("匹配 `auth` 的目录"),
            "header names the keyword: {text}"
        );
        assert!(
            !text.contains("最近目录**"),
            "the plain header is replaced while searching: {text}"
        );
    }

    fn dir_search_form(card: &serde_json::Value) -> Option<serde_json::Value> {
        card["body"]["elements"]
            .as_array()?
            .iter()
            .find(|e| e["tag"] == "form" && e["name"] == "dir_search")
            .cloned()
    }

    /// The `/dir` pager: the three-column `column_set` whose buttons carry
    /// `op: "page"` (the row button rows carry pick/topic instead).
    fn dir_pager(card: &serde_json::Value) -> Option<serde_json::Value> {
        card["body"]["elements"]
            .as_array()?
            .iter()
            .find(|e| {
                e["tag"] == "column_set"
                    && e["columns"]
                        .as_array()
                        .is_some_and(|cols| cols.iter().any(|c| c["elements"][0]["value"]["op"] == "page"))
            })
            .cloned()
    }

    /// The two-column button row whose left button targets `dir`.
    fn dir_row(card: &serde_json::Value, dir: &str) -> Option<serde_json::Value> {
        card["body"]["elements"]
            .as_array()?
            .iter()
            .find(|e| e["tag"] == "column_set" && e["columns"][0]["elements"][0]["value"]["directory"] == dir)
            .cloned()
    }
}
