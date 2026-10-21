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

/// Rows a `/switch`, `/dir` or `/sub` list card shows per page (ADR-0052): the
/// old row cap becomes the page size. The builders clamp a requested page into
/// `[1, total_pages]` and slice the full filtered list themselves.
pub const CARD_PAGE_SIZE: usize = 6;

/// The window one page of a list card renders: the `[start, end)` slice of the
/// full filtered list, the clamped 1-based page, and the total page count. One
/// place owns the clamp/slice rule (ADR-0052), so a fetch that must read
/// something per rendered row — the `/sub` card's one status read per child —
/// windows exactly the rows the builder renders.
pub(super) struct PageWindow {
    pub(super) start: usize,
    pub(super) end: usize,
    pub(super) page: usize,
    pub(super) total_pages: usize,
}

/// The `[1, total_pages]`-clamped page window over `total` rows for a requested
/// 1-based `page` (missing/garbage reads as 1; an out-of-range page lands on
/// the last page rather than springing back to the first, ADR-0052).
pub(super) fn page_window(total: usize, page: usize) -> PageWindow {
    let total_pages = total.div_ceil(CARD_PAGE_SIZE).max(1);
    let page = page.clamp(1, total_pages);
    let start = (page - 1) * CARD_PAGE_SIZE;
    PageWindow {
        start,
        end: (start + CARD_PAGE_SIZE).min(total),
        page,
        total_pages,
    }
}

/// The list cards' search form (`/switch`, `/dir`, `/sub` and the `/skill`
/// picker; ADR-0051, ADR-0052): an input + a submit button. The routing payload
/// rides in the submit button's `name` (form submits don't always deliver the
/// button `value`) and the typed keyword arrives as `form_value.search`. The
/// submit name is `<prefix>|<chat>|<thread>` plus `|<scope>` when a scope rides
/// along; `default_value` echoes the active keyword (NOT `value`, the passback
/// field) so a re-render never blanks the box — an empty result stays
/// tweakable instead of a retype. `extra_fields` ride the submit's `value`
/// only — the `name` is length-bounded, so a card that needs more routing
/// (`/skill`'s `chat_type`/`reply_message_id`) degrades on the name-only
/// fallback rather than growing the name past Feishu's limit.
#[allow(clippy::too_many_arguments)] // list-card builder: every knob is a first-class card axis
pub(super) fn search_form(
    form_name: &str,
    submit_prefix: &str,
    action: &str,
    thread_key: &crate::config::ThreadKey,
    scope: Option<SwitchScope>,
    placeholder: &str,
    keyword: &str,
    extra_fields: &[(&str, &str)],
) -> serde_json::Value {
    let mut submit_name = format!("{submit_prefix}|{}|{}", thread_key.chat_id, thread_key.thread_id);
    let mut submit_value = json!({
        "action": action,
        "op": "search",
        "chat_id": thread_key.chat_id,
        "thread_id": thread_key.thread_id,
    });
    if let Some(scope) = scope {
        submit_name.push_str(&format!("|{}", scope.as_str()));
        submit_value["scope"] = json!(scope.as_str());
    }
    let submit_value = super::with_callback_field(submit_value, extra_fields);
    json!({
        "tag": "form",
        "name": form_name,
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
                "placeholder": { "tag": "plain_text", "content": placeholder },
                "default_value": keyword,
                "max_length": 100,
                "width": "fill",
            },
            {
                "tag": "button",
                "text": { "tag": "plain_text", "content": "搜索" },
                "type": "primary",
                "form_action_type": "submit",
                "name": submit_name,
                "value": submit_value,
            },
        ],
    })
}

/// The three-column pagination control the list cards share (`/switch`, `/dir`,
/// `/sub` and the `/skill` picker; ADR-0052): a 上一页 button, the
/// 「第 x/y 页 · 共 N 个」 indicator, and a 下一页 button. `page` is the
/// already-clamped current page; the boundary buttons are `disabled` (never
/// hidden, so the layout stays stable). Every button's value carries the
/// routing payload plus the active filter (`keyword`, `scope` when given, and
/// `extra_fields`) and the TARGET page, so a flip rebuilds the same filtered
/// view. `None` when there is at most one page — the caller renders nothing.
#[allow(clippy::too_many_arguments)] // list-card builder: every knob is a first-class card axis
pub(super) fn pager_element(
    action: &str,
    thread_key: &crate::config::ThreadKey,
    keyword: &str,
    scope: Option<SwitchScope>,
    page: usize,
    total_pages: usize,
    total_items: usize,
    extra_fields: &[(&str, &str)],
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
        let value = super::with_callback_field(value, extra_fields);
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
    let window = page_window(total, page);
    let page = window.page;
    let mut elements: Vec<serde_json::Value> = Vec::new();

    // Search form (ADR-0022): keyword-prefilled; the scope rides its submit so
    // a scoped search survives the round trip (ADR-0052).
    elements.push(search_form(
        "switch_search",
        "switchsearch",
        "switch",
        thread_key,
        Some(scope),
        "🔍 搜索标题 / 目录 / ID",
        keyword,
        &[],
    ));

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
        for s in &sessions[window.start..window.end] {
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
        window.total_pages,
        total,
        &[],
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

/// The `/sub` child-session card's run-state labels (spec #344): the server's
/// Busy and Retry both read 运行中 — anything but Idle is a child still doing
/// work — while Idle reads 空闲. A row with no status to report (a failed or
/// unrecognised read) omits the label rather than guessing (ADR-0028's rule).
/// Exported so the bridge tests assert the copy from one source.
pub const CHILD_RUNNING: &str = "⚙️ 运行中";
pub const CHILD_IDLE: &str = "✅ 空闲";

/// The run-state label for one child's status read; `None` = no report.
fn child_state_label(status: Option<crate::opencode::types::SessionStatus>) -> Option<&'static str> {
    match status {
        None => None,
        // Anything but Idle is a child still doing work: `is_live` is Busy
        // (running) and Retry (the next attempt is scheduled).
        Some(status) => Some(if status.is_live() {
            CHILD_RUNNING
        } else {
            CHILD_IDLE
        }),
    }
}

/// One `/sub` row's visible text: `title · agent · id-tail · last-activity ·
/// run-state`, omitting the segments the payload does not carry (an absent
/// agent, server time, or status read) instead of filling them with a guess.
fn child_row_text(
    child: &crate::opencode::types::SessionListInfo,
    status: Option<crate::opencode::types::SessionStatus>,
    now_ms: i64,
) -> String {
    let mut title = crate::bridge::display::title_or_id_tail(child);
    let mut agent = None;
    if let Some(name) = child.agent.as_deref().filter(|a| !a.is_empty()) {
        // The `task` tool names a child `<description> (@<subagent> subagent)`
        // AND records the subagent in `agent` (opencode task.ts), so the
        // standalone agent segment below would otherwise repeat the name. Drop
        // the redundant suffix; a title that was only the suffix falls back to
        // the id tail, like any other empty title.
        if let Some(stripped) = strip_task_agent_suffix(&title, name) {
            title = if stripped.is_empty() {
                crate::bridge::display::id_tail(&child.id)
            } else {
                stripped
            };
        }
        agent = Some(name.to_string());
    }
    let mut parts = vec![title];
    if let Some(agent) = agent {
        parts.push(agent);
    }
    parts.push(format!("`{}`", crate::bridge::display::id_tail(&child.id)));
    if let Some(updated) = child.time.as_ref().map(|t| t.updated) {
        parts.push(crate::bridge::display::relative_time(updated, now_ms));
    }
    if let Some(state) = child_state_label(status) {
        parts.push(state.to_string());
    }
    parts.join(" · ")
}

/// Strip the `task` tool's redundant `(@<agent> subagent)` title suffix when it
/// matches the session's recorded agent exactly. The marker is matched without
/// its leading space because `clean_session_label` normalises whitespace
/// (`strip_mention_tokens`), so a title that is only the marker — an empty
/// description — renders without one. Returns the trimmed description, or
/// `None` when the title does not end with that exact marker — the caller then
/// keeps the title verbatim (e.g. a suffixed title whose session has no `agent`
/// field: no standalone segment, so no duplication and nothing to strip).
fn strip_task_agent_suffix(title: &str, agent: &str) -> Option<String> {
    let marker = format!("(@{agent} subagent)");
    title
        .strip_suffix(&marker)
        .map(|description| description.trim().to_string())
}

/// Build the read-only `/sub` child-session card (spec #344): the Active
/// Session's direct children, one plain text row each — title, id tail, agent,
/// last activity and live run state — with no takeover buttons, no scope
/// toggle and no ＋新建 footer. `children` is the caller's filtered,
/// last-activity-sorted list; `statuses` holds the run state read for the rows
/// on `page` only (one read per rendered row, bounded by the page size), so a
/// missing or `None` entry omits the label. The card reuses the switch card's
/// search box and pager with the same round-trip state (ADR-0052): `keyword`
/// and the clamped `page` ride every button. `now_ms` is the build-time clock
/// the relative last-activity stamps count from.
pub fn build_child_card(
    thread_key: &crate::config::ThreadKey,
    children: &[crate::opencode::types::SessionListInfo],
    statuses: &std::collections::HashMap<String, Option<crate::opencode::types::SessionStatus>>,
    keyword: &str,
    page: usize,
    now_ms: i64,
) -> serde_json::Value {
    let total = children.len();
    let window = page_window(total, page);
    let mut elements: Vec<serde_json::Value> = Vec::new();

    elements.push(search_form(
        "sub_search",
        "subsearch",
        "sub",
        thread_key,
        None,
        "🔍 搜索标题 / 目录 / ID",
        keyword,
        &[],
    ));

    // A conversation with no Active Session, an Active Session with no
    // children, and an empty search all land here: one plain line, never an
    // error (a failure-shaped reply would be indistinguishable from a broken
    // read).
    if children.is_empty() {
        let hint = if keyword.is_empty() {
            "_(没有子会话)_"
        } else {
            "_(无匹配子会话)_"
        };
        elements.push(json!({ "tag": "markdown", "content": hint }));
        return card_shell("🧩 子会话", "blue", elements);
    }

    let header = if keyword.is_empty() {
        "**子会话**".to_string()
    } else {
        format!("**匹配 `{keyword}` 的子会话**")
    };
    elements.push(json!({
        "tag": "markdown",
        "content": crate::feishu::card::sanitize::sanitize_markdown(&header)
    }));
    for child in &children[window.start..window.end] {
        let status = statuses.get(&child.id).copied().flatten();
        elements.push(card_text_row(&child_row_text(child, status, now_ms)));
    }

    // Pagination (ADR-0052) below the rows — the only buttons the card has
    // besides the search submit.
    if let Some(pager) = pager_element(
        "sub",
        thread_key,
        keyword,
        None,
        window.page,
        window.total_pages,
        total,
        &[],
    ) {
        elements.push(pager);
    }

    card_shell("🧩 子会话", "blue", elements)
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
    let window = page_window(total, page);
    let page = window.page;
    let mut elements: Vec<serde_json::Value> = Vec::new();

    // ADR-0078: the entry point to the filesystem browse card — the way to
    // reach a directory the Recent Directories union does not carry.
    elements.push(browse_entry_button(thread_key));

    if total > CARD_PAGE_SIZE || !keyword.is_empty() {
        elements.push(search_form(
            "dir_search",
            "dirsearch",
            "dir",
            thread_key,
            None,
            "🔍 搜索目录路径",
            keyword,
            &[],
        ));
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
        for dir in &dirs[window.start..window.end] {
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
    if let Some(pager) = pager_element(
        "dir",
        thread_key,
        keyword,
        None,
        page,
        window.total_pages,
        total,
        &[],
    ) {
        elements.push(pager);
    }

    card_shell("📂 最近目录", "blue", elements)
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

/// One `/worktree` card row (ADR-0088): a checkout, its branch, whether it is
/// dirty (`None` = the read missed its bound), how many sessions live there,
/// and whether the conversation is already rooted there.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorktreeRow {
    pub path: String,
    pub name: String,
    pub branch: Option<String>,
    pub dirty: Option<bool>,
    pub main: bool,
    pub sessions: usize,
    pub current: bool,
}

/// The `/worktree` picker card (ADR-0088): the project's git checkouts, the
/// main one included and labelled, each with its branch, dirty marker, session
/// count and current marker. Picking a row declares a Pending Session there —
/// the same meaning as the `/dir` card's pick, so it never adopts.
pub fn build_worktree_card(
    thread_key: &crate::config::ThreadKey,
    rows: &[WorktreeRow],
    repo: &str,
) -> serde_json::Value {
    let mut elements: Vec<serde_json::Value> = Vec::new();
    elements.push(json!({
        "tag": "markdown",
        "content": crate::feishu::card::sanitize::sanitize_markdown(&format!(
            "项目 `{repo}` 的 git 检出 · {} 个",
            rows.len()
        )),
    }));
    if rows.is_empty() {
        elements.push(json!({
            "tag": "markdown",
            "content": "_(这个项目还没有 worktree。用 `/worktree new <名字>` 创建。)_",
        }));
    }
    for row in rows {
        let mut title = format!("**{}**", row.name);
        if row.main {
            title.push_str(" · 主检出");
        }
        if row.current {
            title.push_str(" · ✅ 当前");
        }
        let branch = match &row.branch {
            Some(branch) => format!("分支 `{branch}`"),
            None => "detached HEAD".to_string(),
        };
        let dirty = match row.dirty {
            Some(true) => " · ⚠ 有未提交改动",
            Some(false) => "",
            None => " · ? 状态未知",
        };
        let sessions = if row.sessions == 0 {
            String::new()
        } else {
            format!(" · {} 个会话", row.sessions)
        };
        elements.push(card_text_row(&format!("{title}\n{branch}{dirty}{sessions}")));
        elements.push(json!({
            "tag": "column_set",
            "flex_mode": "none",
            "columns": [{
                "tag": "column",
                "width": "auto",
                "vertical_align": "center",
                "elements": [{
                    "tag": "button",
                    "text": {
                        "tag": "plain_text",
                        "content": if row.current { "✅ 当前" } else { "切换到这里" },
                    },
                    "type": "default",
                    "value": {
                        "action": "worktree",
                        "op": "pick",
                        "chat_id": thread_key.chat_id,
                        "thread_id": thread_key.thread_id,
                        "directory": row.path,
                        "name": row.name,
                    },
                }]
            }]
        }));
    }
    card_shell("🌲 worktree", "blue", elements)
}

/// One child row of the `/dir` browse card (ADR-0078, ADR-0083 §3): the display
/// name, the absolute path its button acts on, and what the row is. A directory
/// carries `is_repo` (it holds a `.git` entry — floated to the front and marked
/// 📦) and no `size`; a file carries `size`, the byte count its row names, and
/// opens the **File Viewer** in place.
pub struct BrowseChild {
    pub name: String,
    pub path: String,
    pub is_repo: bool,
    /// `Some` exactly when this row is a file (ADR-0083 §3 lists files beside
    /// directories); `None` marks a directory row.
    pub size: Option<u64>,
}

/// The stateless view state every browse-card control round-trips (ADR-0078):
/// the browsed directory, whether dot-directories show, the child filter and the
/// page. One struct because they always travel together — bundled once here,
/// carried by every button, and read back together by the handler.
#[derive(Clone, Copy)]
pub struct BrowseView<'a> {
    pub dir: &'a str,
    pub show_hidden: bool,
    pub keyword: &'a str,
    pub page: usize,
}

/// The `/dir` browse card (ADR-0078): the filesystem drill-down that reaches a
/// directory not in the Recent Directories union. One screen holds a clickable
/// breadcrumb, the current-directory action row (切换到这里 / 建话题), a
/// path-and-filter form, and one page of child directories — each a full-width
/// drill-down button. Rows descend; selection acts on the directory you are
/// standing in (the classic file-browser model). Schema 2.0: the form is a
/// root-level element (it cannot nest), no v1 `action` container is used, and
/// every rebuild rides `view` (ADR-0078). `notice` carries a fallback
/// explanation — a vanished directory clamped to its nearest existing ancestor,
/// or an unreadable directory.
pub fn build_dir_browse_card(
    thread_key: &crate::config::ThreadKey,
    children: &[BrowseChild],
    current_dir: Option<&str>,
    view: BrowseView<'_>,
    show_filter: bool,
    notice: Option<&str>,
) -> serde_json::Value {
    let total = children.len();
    let window = page_window(total, view.page);
    let page = window.page;
    let view = BrowseView { page, ..view };
    let mut elements: Vec<serde_json::Value> = Vec::new();

    if let Some(notice) = notice {
        elements.push(json!({
            "tag": "markdown",
            "content": crate::feishu::card::sanitize::sanitize_markdown(&format!("⚠️ {notice}")),
        }));
    }
    elements.push(browse_breadcrumb(thread_key, view));
    elements.push(json!({
        "tag": "markdown",
        "content": crate::feishu::card::sanitize::sanitize_markdown(&format!("**当前目录**：`{}`", view.dir)),
    }));
    elements.push(browse_node_actions(thread_key, view, current_dir));
    elements.push(browse_form(thread_key, view, show_filter));

    let header = if view.keyword.is_empty() {
        "**子目录与文件**".to_string()
    } else {
        format!("**匹配 `{}` 的子目录与文件**", view.keyword)
    };
    elements.push(json!({
        "tag": "markdown",
        "content": crate::feishu::card::sanitize::sanitize_markdown(&header),
    }));
    elements.push(browse_dot_toggle(thread_key, view));
    if total == 0 {
        let hint = if view.keyword.is_empty() {
            "_(没有子目录或文件)_"
        } else {
            "_(无匹配的子目录或文件)_"
        };
        elements.push(json!({ "tag": "markdown", "content": hint }));
    } else {
        for child in &children[window.start..window.end] {
            elements.push(match child.size {
                Some(size) => browse_file_button(thread_key, child, size, view),
                None => browse_child_button(thread_key, child, view.show_hidden),
            });
        }
    }

    // The pager rides the extra browse routing (`view`/`dir`/`show_hidden`) so a
    // flip rebuilds THIS card, not the Recent Directories one; the flag is a
    // string because the shared pager only carries string callback fields.
    let show_hidden_str = if view.show_hidden { "true" } else { "false" };
    if let Some(pager) = pager_element(
        "dir",
        thread_key,
        view.keyword,
        None,
        page,
        window.total_pages,
        total,
        &[
            ("view", "browse"),
            ("dir", view.dir),
            ("show_hidden", show_hidden_str),
        ],
    ) {
        elements.push(pager);
    }
    elements.push(browse_recent_button(thread_key));

    card_shell("📁 浏览目录", "blue", elements)
}

/// The routing payload every browse-card control carries: the card identity
/// plus `view`. `view: "browse"` is what routes the callback to the browse
/// handler rather than the Recent Directories one.
fn browse_value(thread_key: &crate::config::ThreadKey, op: &str, view: BrowseView<'_>) -> serde_json::Value {
    json!({
        "action": "dir",
        "view": "browse",
        "op": op,
        "chat_id": thread_key.chat_id,
        "thread_id": thread_key.thread_id,
        "dir": view.dir,
        "show_hidden": view.show_hidden,
        "keyword": view.keyword,
        "page": view.page,
    })
}

/// [`browse_value`] for a card that is not a browse card but still hands the
/// browse handler a navigation (ADR-0083 §3): the **File Viewer**'s
/// 「📂 所在目录」 (`op: "go"` at the file's parent directory) and its
/// 「← 返回目录」 (`op: "restore"`, the listing's whole state so the return is
/// exact). Sharing the builder keeps a viewer's payloads `BrowseView`-shaped
/// and unable to drift from the browse card's own.
pub(crate) fn browse_view_value(
    thread_key: &crate::config::ThreadKey,
    op: &str,
    view: BrowseView<'_>,
) -> serde_json::Value {
    browse_value(thread_key, op, view)
}

/// The routing payload a browse card's **file row** carries (ADR-0083 §3):
/// `action: "file"` — never the browse card's `dir` — dispatches the click to
/// the File Viewer opener, which is what keeps a file row from offering
/// 选择/建话题 (declaring a Session needs a directory). `path` is the file to
/// open; `view` is the listing the row was clicked in, whose whole state
/// (`dir`/`show_hidden`/`keyword`/`page`) rides along so the viewer's
/// 「← 返回目录」 restores it exactly. A viewer opened with no listing to return
/// to (a tool panel's 打开 button, #743) passes `None`.
pub(crate) fn browse_file_value(
    thread_key: &crate::config::ThreadKey,
    path: &str,
    view: Option<BrowseView<'_>>,
) -> serde_json::Value {
    let mut value = file_open_value(path);
    value["chat_id"] = json!(thread_key.chat_id);
    value["thread_id"] = json!(thread_key.thread_id);
    if let Some(view) = view {
        value["dir"] = json!(view.dir);
        value["show_hidden"] = json!(view.show_hidden);
        value["keyword"] = json!(view.keyword);
        value["page"] = json!(view.page);
    }
    value
}

/// The routing payload alone, the half every `action: "file"` click shares
/// (ADR-0083 §3, §8): the action tag and the **verbatim** `path`. The browse
/// card's file row adds its listing and the Chat/Topic
/// ([`browse_file_value`]); a Tool Panel's 「📂 打开」 adds the Session instead
/// ([`tool_file_value`], #743). ONE constructor, so the action a click routes
/// by and the key the path travels in can never drift between the two entry
/// points.
fn file_open_value(path: &str) -> serde_json::Value {
    json!({ "action": "file", "path": path })
}

/// The routing payload a **Tool Panel**'s 「📂 打开」 button carries (ADR-0083
/// §8, #743): the file to open, and the **Session** the panel was rendered for
/// — never the Chat/Topic, which a live Turn card does not know. The click's
/// card-action span context resolves that Session's Chat/Topic from the
/// SessionStore (the same read the span itself makes, and the rule a live
/// card's own section-level buttons follow — see `ledger::cleanup_button`), so
/// the viewer opens with working controls. No listing rides along
/// (`dir`/`keyword`/`page` absent), which is exactly what makes the viewer
/// carry no 「← 返回目录」: a tool panel has no listing to return to.
pub(crate) fn tool_file_value(session_id: &str, path: &str) -> serde_json::Value {
    let mut value = file_open_value(path);
    value["session_id"] = json!(session_id);
    value
}

/// The routing payload a Tool Panel's 「🌐 预览网页」 button carries (#778): the
/// same Session-routed [`tool_file_value`], plus `op: "preview"`. The op is the
/// whole difference: it tells the `action: "file"` dispatch that the click
/// **sends** the file as one File Message and must **not** PATCH the clicked
/// message — the panel rides the agent's Turn card, which a transfer's card
/// PATCH would overwrite (#739's `op: "send"` rewrites the viewer card it was
/// clicked on, and that is exactly what must not happen here).
pub(crate) fn tool_page_value(session_id: &str, path: &str) -> serde_json::Value {
    let mut value = tool_file_value(session_id, path);
    value["op"] = json!("preview");
    value
}

/// The browse card's clickable breadcrumb (ADR-0078): 🏠 home, ⬆ 上一级, then
/// the path's tail segments (at most four, with a leading `…` jumping to the
/// segment just before them) as `column_set`/`flex_mode: "flow"` buttons.
/// Navigating away clears the child filter and returns to page 1.
fn browse_breadcrumb(thread_key: &crate::config::ThreadKey, view: BrowseView<'_>) -> serde_json::Value {
    let nav = |label: &str, target: &str| {
        json!({
            "tag": "column",
            "width": "auto",
            "vertical_align": "center",
            "elements": [{
                "tag": "button",
                "text": { "tag": "plain_text", "content": label },
                "type": "default",
                "value": browse_value(thread_key, "go", BrowseView { dir: target, keyword: "", page: 1, ..view }),
            }],
        })
    };
    let mut columns: Vec<serde_json::Value> = Vec::new();
    if let Some(home) = dirs::home_dir() {
        columns.push(nav("🏠", &home.to_string_lossy()));
    }
    if let Some(parent) = std::path::Path::new(view.dir).parent() {
        let parent = parent.to_string_lossy();
        if !parent.is_empty() && parent != view.dir {
            columns.push(nav("⬆ 上一级", &parent));
        }
    }
    let segments = path_segments(view.dir);
    let cut = segments.len().saturating_sub(4);
    if cut > 0 {
        columns.push(nav("…", &segments[cut - 1].1));
    }
    for (label, path) in &segments[cut..] {
        columns.push(nav(label, path));
    }
    json!({
        "tag": "column_set",
        "flex_mode": "flow",
        "horizontal_spacing": "default",
        "columns": columns,
    })
}

/// The `/`-anchored (label, absolute path) pairs of `dir`, for the breadcrumb.
/// `/a/b/c` yields `("/", "/")`, `("a", "/a")`, `("b", "/a/b")`, `("c",
/// "/a/b/c")`; a relative or empty spelling yields its components as-is.
fn path_segments(dir: &str) -> Vec<(String, String)> {
    let mut acc = std::path::PathBuf::new();
    let mut segments = Vec::new();
    for component in std::path::Path::new(dir).components() {
        acc.push(component.as_os_str());
        let label = match component {
            std::path::Component::RootDir => "/".to_string(),
            other => other.as_os_str().to_string_lossy().to_string(),
        };
        segments.push((label, acc.to_string_lossy().to_string()));
    }
    segments
}

/// The selected-node action row of the browse card: 切换到这里 (or ✅ 当前 when
/// this is already the conversation's directory) and 建话题, both acting on the
/// directory being browsed.
fn browse_node_actions(
    thread_key: &crate::config::ThreadKey,
    view: BrowseView<'_>,
    current_dir: Option<&str>,
) -> serde_json::Value {
    let is_current = current_dir.is_some_and(|current| crate::bridge::command::same_dir(current, view.dir));
    let pick_label = if is_current {
        "✅ 当前"
    } else {
        "切换到这里"
    };
    let column = |op: &str, label: &str| {
        json!({
            "tag": "column",
            "width": "auto",
            "vertical_align": "center",
            "elements": [{
                "tag": "button",
                "text": { "tag": "plain_text", "content": label },
                "type": "default",
                "value": browse_value(thread_key, op, view),
            }],
        })
    };
    json!({
        "tag": "column_set",
        "flex_mode": "bisect",
        "horizontal_spacing": "default",
        "columns": [column("pick", pick_label), column("topic", "建话题")],
    })
}

/// The 显示隐藏 toggle of the browse card (ADR-0078, ADR-0083 §3): shows or
/// hides every dot-prefixed entry — a dot-directory and a dot-file alike. The
/// label states the action and the payload carries the CURRENT `show_hidden`
/// flag — the `show_hidden` op flips it (every other op passes it through
/// unchanged).
fn browse_dot_toggle(thread_key: &crate::config::ThreadKey, view: BrowseView<'_>) -> serde_json::Value {
    let label = if view.show_hidden {
        "隐藏点开头的项"
    } else {
        "显示隐藏项"
    };
    json!({
        "tag": "button",
        "width": "fill",
        "type": "default",
        "text": { "tag": "plain_text", "content": label },
        "value": browse_value(thread_key, "show_hidden", view),
    })
}

/// One child directory of the browse card: a single full-width drill-down
/// button (`width: "fill"`), 📦-marked when it is a repo. `op: "go"` navigates
/// into it, clearing the child filter and returning to page 1.
fn browse_child_button(
    thread_key: &crate::config::ThreadKey,
    child: &BrowseChild,
    show_hidden: bool,
) -> serde_json::Value {
    let label = if child.is_repo {
        format!("📦 {}", child.name)
    } else {
        format!("📁 {}", child.name)
    };
    json!({
        "tag": "button",
        "width": "fill",
        "type": "default",
        "text": { "tag": "plain_text", "content": label },
        "value": browse_value(
            thread_key,
            "go",
            BrowseView { dir: &child.path, show_hidden, keyword: "", page: 1 },
        ),
    })
}

/// One file row of the browse card (ADR-0083 §3): a single full-width button
/// (📄 + the name + its size) that opens the file's **File Viewer** in place —
/// the same ack-patch move as the browse card replacing the Recent Directories
/// card. Its payload carries the whole listing `view`, so the viewer's
/// 「← 返回目录」 comes back to this exact page, filter and toggle.
fn browse_file_button(
    thread_key: &crate::config::ThreadKey,
    child: &BrowseChild,
    size: u64,
    view: BrowseView<'_>,
) -> serde_json::Value {
    let size = crate::backend::file_content::human_size(size);
    json!({
        "tag": "button",
        "width": "fill",
        "type": "default",
        "text": { "tag": "plain_text", "content": format!("📄 {} · {size}", child.name) },
        "value": browse_file_value(thread_key, &child.path, Some(view)),
    })
}

/// The browse card's one form (ADR-0078): a path input with 前往 (navigate) and
/// 选择 (declare a Pending Session there), plus — when the directory has more
/// children than a page or a filter is active — a filter input with 过滤. The
/// submit routing rides the button `name` (form submits do not always deliver
/// the `value`) with `path`/`filter` read from `form_value`; a `name`-only
/// submission of 过滤 loses `view` and falls back to the current project, the
/// accepted degradation of a length-bounded name.
fn browse_form(
    thread_key: &crate::config::ThreadKey,
    view: BrowseView<'_>,
    show_filter: bool,
) -> serde_json::Value {
    let submit = |label: &str, prefix: &str, op: &str, btn_type: &str| {
        json!({
            "tag": "button",
            "text": { "tag": "plain_text", "content": label },
            "type": btn_type,
            "form_action_type": "submit",
            "name": format!("{prefix}|{}|{}", thread_key.chat_id, thread_key.thread_id),
            "value": browse_value(thread_key, op, view),
        })
    };
    let path_row = json!({
        "tag": "column_set",
        "flex_mode": "none",
        "horizontal_spacing": "default",
        "columns": [
            {
                "tag": "column",
                "width": "weighted",
                "weight": 5,
                "vertical_align": "center",
                "elements": [{
                    "tag": "input",
                    "name": "path",
                    "input_type": "multiline_text",
                    "rows": 1,
                    "auto_resize": true,
                    "max_rows": 3,
                    "placeholder": {
                        "tag": "plain_text",
                        "content": "输入或粘贴目录路径（~/work、/root/...）",
                    },
                    "max_length": 1000,
                    "width": "fill",
                }],
            },
            {
                "tag": "column",
                "width": "auto",
                "vertical_align": "center",
                "elements": [submit("前往", "browsego", "go", "default")],
            },
            {
                "tag": "column",
                "width": "auto",
                "vertical_align": "center",
                "elements": [submit("选择", "browsepick", "pick", "primary")],
            },
        ],
    });
    let mut rows = vec![path_row];
    if show_filter {
        rows.push(json!({
            "tag": "column_set",
            "flex_mode": "none",
            "horizontal_spacing": "default",
            "columns": [
                {
                    "tag": "column",
                    "width": "weighted",
                    "weight": 5,
                    "vertical_align": "center",
                    "elements": [{
                        "tag": "input",
                        "name": "filter",
                        "input_type": "multiline_text",
                        "rows": 1,
                        "auto_resize": true,
                        "max_rows": 3,
                        "placeholder": {
                            "tag": "plain_text",
                            "content": "过滤当前目录的子目录与文件名",
                        },
                        "default_value": view.keyword,
                        "max_length": 100,
                        "width": "fill",
                    }],
                },
                {
                    "tag": "column",
                    "width": "auto",
                    "vertical_align": "center",
                    "elements": [submit("过滤", "browsefilter", "filter", "default")],
                },
            ],
        }));
    }
    json!({ "tag": "form", "name": "dir_browse", "elements": rows })
}

/// The 「← 最近目录」 button that swaps the browse card back for the Recent
/// Directories card in place (ADR-0078).
fn browse_recent_button(thread_key: &crate::config::ThreadKey) -> serde_json::Value {
    json!({
        "tag": "button",
        "type": "default",
        "text": { "tag": "plain_text", "content": "← 最近目录" },
        "value": {
            "action": "dir",
            "op": "recent",
            "chat_id": thread_key.chat_id,
            "thread_id": thread_key.thread_id,
        },
    })
}

/// The 「📁 浏览目录」 entry button on the Recent Directories card (ADR-0078):
/// opens the browse card in place.
fn browse_entry_button(thread_key: &crate::config::ThreadKey) -> serde_json::Value {
    json!({
        "tag": "button",
        "width": "fill",
        "type": "primary",
        "text": { "tag": "plain_text", "content": "📁 浏览目录" },
        "value": {
            "action": "dir",
            "op": "browse",
            "chat_id": thread_key.chat_id,
            "thread_id": thread_key.thread_id,
        },
    })
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

    /// The `/sub` card's run-state vocabulary: Busy and Retry both read 运行中,
    /// Idle reads 空闲, and an unknown/failed read reports nothing.
    #[test]
    fn child_state_label_maps_busy_and_retry_to_running_and_idle_to_idle() {
        use crate::opencode::types::SessionStatus;
        assert_eq!(child_state_label(Some(SessionStatus::Busy)), Some(CHILD_RUNNING));
        assert_eq!(child_state_label(Some(SessionStatus::Retry)), Some(CHILD_RUNNING));
        assert_eq!(child_state_label(Some(SessionStatus::Idle)), Some(CHILD_IDLE));
        assert_eq!(child_state_label(None), None);
    }

    /// The `task` tool names a child `<description> (@<subagent> subagent)` and
    /// records the same subagent in `agent` (opencode task.ts), so the row must
    /// not show the name twice: a matching suffix is stripped and the standalone
    /// agent segment keeps it. A suffixed title with no `agent` field, a normal
    /// title, and a title that is only the suffix all behave as documented.
    #[test]
    fn child_row_text_strips_the_redundant_task_agent_suffix() {
        use crate::opencode::types::SessionStatus;
        let now = 1_700_000_000_000;

        // The realistic fixture: title suffix + matching agent.
        let suffixed = child_session(
            "ses_child_task",
            "Research token-saving tools (@general subagent)",
            Some("general"),
            now - 60_000,
        );
        let text = child_row_text(&suffixed, Some(SessionStatus::Busy), now);
        assert!(text.contains("Research token-saving tools"), "{text}");
        assert_eq!(
            text.matches("general").count(),
            1,
            "the agent renders exactly once: {text}"
        );
        assert!(
            !text.contains("(@general subagent)"),
            "the redundant suffix is stripped: {text}"
        );
        assert!(text.contains("`child_t`"), "id tail present: {text}");
        assert!(text.contains(CHILD_RUNNING), "state present: {text}");

        // A stored child whose title carries the suffix but whose `agent` is
        // absent is left verbatim — no standalone segment, so no duplication.
        let agentless = child_session(
            "ses_child_agentless",
            "Research token-saving tools (@general subagent)",
            None,
            now,
        );
        let text = child_row_text(&agentless, None, now);
        assert!(
            text.contains("(@general subagent)"),
            "no agent field means no stripping: {text}"
        );
        assert!(
            !text.contains(" · general · "),
            "no standalone agent segment: {text}"
        );

        // A title that is only the suffix (an empty description: the tool still
        // emits the leading space) falls back to the id tail.
        let bare = child_session("ses_child_bare", " (@general subagent)", Some("general"), now);
        let text = child_row_text(&bare, None, now);
        assert!(!text.contains("(@general subagent)"), "{text}");
        assert!(text.contains("`child_b`"), "falls back to the id tail: {text}");

        // A normal title is untouched.
        let normal = child_session("ses_child_norm", "普通子任务", Some("general"), now);
        assert_eq!(
            child_row_text(&normal, Some(SessionStatus::Idle), now),
            format!("普通子任务 · general · `child_n` · 0s · {CHILD_IDLE}"),
        );
    }

    /// Spec #344: each rendered row carries the child's title, id tail, agent,
    /// last activity and run state — and nothing else. The card is observation
    /// only: no takeover buttons, no scope toggle, no ＋新建 footer.
    #[test]
    fn child_card_rows_carry_title_agent_id_tail_activity_and_state() {
        let key = crate::config::ThreadKey::new("chat_1".into(), "chat_1".into());
        let now = 1_700_000_000_000;
        let children = vec![
            child_session("ses_child_aa", "重构渲染", Some("build"), now - 90_000),
            child_session("ses_child_bb", "跑测试", Some("review"), now - 3 * 3_600_000),
        ];
        let statuses = std::collections::HashMap::from([
            (
                "ses_child_aa".to_string(),
                Some(crate::opencode::types::SessionStatus::Busy),
            ),
            (
                "ses_child_bb".to_string(),
                Some(crate::opencode::types::SessionStatus::Retry),
            ),
        ]);
        let card = build_child_card(&key, &children, &statuses, "", 1, now);
        let text = card.to_string();
        for needle in [
            "重构渲染",
            "build",
            "child_a",
            "1m",
            "跑测试",
            "review",
            "child_b",
            "3h",
        ] {
            assert!(text.contains(needle), "row missing `{needle}`: {text}");
        }
        assert_eq!(
            text.matches(CHILD_RUNNING).count(),
            2,
            "Busy and Retry both read 运行中: {text}"
        );
        let elements = card["body"]["elements"].as_array().unwrap();
        let rows = elements.iter().filter(|e| e["tag"] == "column_set").count();
        assert_eq!(rows, 2, "one text row per child: {text}");
        // Observation only (spec #344): the card's buttons are the search
        // submit and the pager — no row action, no takeover, no ＋新建.
        assert!(
            !text.contains("\"session_id\""),
            "no row buttons on the child card: {text}"
        );
        for op in ["adopt", "topic_adopt", "scope", "new"] {
            assert!(
                !text.contains(&format!("\"op\":\"{op}\"")),
                "no `{op}` control on the child card: {text}"
            );
        }
    }

    /// A child whose payload carries no agent, no server time, or no status
    /// read still renders — the absent segments are omitted, never guessed.
    #[test]
    fn child_card_omits_segments_the_payload_does_not_carry() {
        let key = crate::config::ThreadKey::new("chat_1".into(), "chat_1".into());
        let mut bare = child_session("ses_child_bare", "无元数据", None, 0);
        bare.time = None;
        let card = build_child_card(&key, std::slice::from_ref(&bare), &Default::default(), "", 1, 0);
        let text = card.to_string();
        assert!(text.contains("无元数据"), "title renders: {text}");
        assert!(text.contains("child_b"), "id tail renders: {text}");
        assert!(
            !text.contains(CHILD_RUNNING) && !text.contains(CHILD_IDLE),
            "no state is invented: {text}"
        );
    }

    /// ADR-0052: page two renders the next window of children and the pager
    /// reports the position; every pager button carries the action, the active
    /// keyword and the TARGET page, so the flip rebuilds the same view.
    #[test]
    fn child_card_page_two_windows_the_rows_and_labels_the_pager() {
        let key = crate::config::ThreadKey::new("chat_1".into(), "chat_1".into());
        let now = 1_700_000_000_000;
        let children = child_sessions(13, now);
        let card = build_child_card(&key, &children, &Default::default(), "", 2, now);
        let text = card.to_string();
        for i in 7..=12 {
            assert!(
                text.contains(&format!("子会话{i}")),
                "page 2 shows 子会话{i}: {text}"
            );
        }
        assert!(
            !text.contains("子会话6"),
            "page 1's last row is off page 2: {text}"
        );
        assert!(!text.contains("子会话13"), "page 3's row is off page 2: {text}");
        let pager = child_pager(&card).expect("a multi-page list renders the pager");
        assert_eq!(
            pager["columns"][1]["elements"][0]["content"], "第 2/3 页 · 共 13 个",
            "indicator names the position and the total: {text}"
        );
        let prev = &pager["columns"][0]["elements"][0];
        assert_eq!(prev["value"]["action"], "sub");
        assert_eq!(prev["value"]["op"], "page");
        assert_eq!(prev["value"]["chat_id"], "chat_1");
        assert_eq!(prev["value"]["page"], 1, "上一页 targets page - 1");
        let next = &pager["columns"][2]["elements"][0];
        assert_eq!(next["value"]["page"], 3, "下一页 targets page + 1");
    }

    /// ADR-0052: a stale/out-of-range page clamps to the LAST page, and the
    /// boundary buttons are disabled, never hidden.
    #[test]
    fn child_card_clamps_the_page_and_disables_the_boundary_buttons() {
        let key = crate::config::ThreadKey::new("chat_1".into(), "chat_1".into());
        let now = 1_700_000_000_000;
        let children = child_sessions(13, now);

        let stale = build_child_card(&key, &children, &Default::default(), "", 99, now);
        let text = stale.to_string();
        assert!(
            text.contains("子会话13") && !text.contains("子会话7"),
            "clamped to the last page: {text}"
        );
        let pager = child_pager(&stale).unwrap();
        let columns = pager["columns"].as_array().unwrap();
        assert_eq!(
            columns[1]["elements"][0]["content"], "第 3/3 页 · 共 13 个",
            "the clamped page is reported: {text}"
        );
        assert_eq!(columns[0]["elements"][0]["disabled"], false);
        assert_eq!(
            columns[2]["elements"][0]["disabled"], true,
            "the last page disables 下一页"
        );
    }

    /// Spec #344: an empty child list (or an empty search) is one plain line —
    /// no rows, no pager, nothing that reads like a failure.
    #[test]
    fn child_card_empty_state_is_a_plain_line() {
        let key = crate::config::ThreadKey::new("chat_1".into(), "chat_1".into());
        let empty = build_child_card(&key, &[], &Default::default(), "", 1, 0);
        let text = empty.to_string();
        assert!(text.contains("没有子会话"), "empty hint: {text}");
        assert!(
            !text.contains("\"tag\":\"column_set\""),
            "no rows/pager on the empty card: {text}"
        );

        let filtered = build_child_card(&key, &[], &Default::default(), "渲染", 1, 0);
        let text = filtered.to_string();
        assert!(text.contains("无匹配子会话"), "empty search hint: {text}");
        assert!(
            !text.contains("没有子会话"),
            "an empty search is distinguishable from an empty list: {text}"
        );
    }

    /// The `/sub` search box routes to the `sub` action and echoes the active
    /// keyword in `default_value` (ADR-0051/ADR-0052), so a rebuild never
    /// blanks it.
    #[test]
    fn child_card_search_form_routes_to_sub_and_echoes_the_keyword() {
        let key = crate::config::ThreadKey::new("chat_1".into(), "chat_1".into());
        let now = 1_700_000_000_000;
        let children = child_sessions(1, now);
        let card = build_child_card(&key, &children, &Default::default(), "子会话1", 1, now);
        let elements = card["body"]["elements"].as_array().unwrap();
        let form = elements
            .iter()
            .find(|e| e["tag"] == "form" && e["name"] == "sub_search")
            .expect("child card has a search form");
        let input = form["elements"]
            .as_array()
            .unwrap()
            .iter()
            .find(|e| e["tag"] == "input")
            .expect("search form has an input");
        assert_eq!(input["default_value"], "子会话1");
        // The filter is `matches_keyword` (title + directory + id), so the
        // placeholder carries the same copy as the `/switch` card's.
        assert_eq!(input["placeholder"]["content"], "🔍 搜索标题 / 目录 / ID");
        let submit = form["elements"]
            .as_array()
            .unwrap()
            .iter()
            .find(|e| e["tag"] == "button")
            .expect("search form has a submit");
        assert_eq!(submit["name"], "subsearch|chat_1|chat_1");
        assert_eq!(submit["value"]["action"], "sub");
        assert_eq!(submit["value"]["op"], "search");
        assert!(
            card.to_string().contains("匹配 `子会话1` 的子会话"),
            "header names the filter: {card}"
        );
    }

    /// `n` children of `ses_root`, newest activity first, each with an agent.
    fn child_sessions(n: i64, now: i64) -> Vec<crate::opencode::types::SessionListInfo> {
        (1..=n)
            .map(|i| {
                child_session(
                    &format!("ses_c{i}"),
                    &format!("子会话{i}"),
                    Some("build"),
                    now - i * 1000,
                )
            })
            .collect()
    }

    /// One child session of `ses_root` with the given payload fields.
    fn child_session(
        id: &str,
        title: &str,
        agent: Option<&str>,
        updated: i64,
    ) -> crate::opencode::types::SessionListInfo {
        crate::opencode::types::SessionListInfo {
            id: id.into(),
            title: title.into(),
            directory: "/work/child".into(),
            parent_id: Some("ses_root".into()),
            agent: agent.map(str::to_string),
            model: None,
            time: Some(crate::opencode::types::SessionTime {
                created: updated,
                updated,
                archived: None,
            }),
        }
    }

    /// The `/sub` pager: the three-column `column_set` whose buttons carry
    /// `op: "page"`.
    fn child_pager(card: &serde_json::Value) -> Option<serde_json::Value> {
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

    /// An empty Recent Directories list renders a hint; its only control is the
    /// ADR-0078 browse entry (the way to reach a directory the empty union
    /// cannot offer) — no row or pager buttons.
    #[test]
    fn dir_card_empty_state_is_a_hint_plus_the_browse_entry() {
        let key = crate::config::ThreadKey::new("chat_1".into(), "chat_1".into());
        let card = build_dir_card(&key, &[], None, "", 1);
        let text = card.to_string();
        assert!(text.contains("还没有最近目录"), "empty hint: {text}");
        assert_eq!(
            text.matches("\"tag\":\"button\"").count(),
            1,
            "only the browse entry button remains: {text}"
        );
        assert!(text.contains("\"op\":\"browse\""), "the browse entry: {text}");
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

    // ---- ADR-0078 browse card ----

    fn browse_child(name: &str, repo: bool) -> BrowseChild {
        BrowseChild {
            name: name.to_string(),
            path: format!("/work/{name}"),
            is_repo: repo,
            size: None,
        }
    }

    /// A file row of the browse card, with its byte size.
    fn browse_file(name: &str, size: u64) -> BrowseChild {
        BrowseChild {
            name: name.to_string(),
            path: format!("/work/{name}"),
            is_repo: false,
            size: Some(size),
        }
    }

    /// Every button anywhere in a card (body elements, forms, column sets), for
    /// assertions that don't care about layout.
    fn all_buttons(card: &serde_json::Value) -> Vec<serde_json::Value> {
        fn walk(value: &serde_json::Value, out: &mut Vec<serde_json::Value>) {
            match value {
                serde_json::Value::Object(map) => {
                    if map.get("tag").and_then(|t| t.as_str()) == Some("button") {
                        out.push(value.clone());
                    }
                    for nested in map.values() {
                        walk(nested, out);
                    }
                }
                serde_json::Value::Array(items) => {
                    for item in items {
                        walk(item, out);
                    }
                }
                _ => {}
            }
        }
        let mut out = Vec::new();
        walk(card, &mut out);
        out
    }

    fn bview<'a>(dir: &'a str, show_hidden: bool, keyword: &'a str, page: usize) -> BrowseView<'a> {
        BrowseView {
            dir,
            show_hidden,
            keyword,
            page,
        }
    }

    #[test]
    fn browse_card_carries_the_schema_2_form_and_view_routing() {
        let key = crate::config::ThreadKey::new("chat_1".into(), "chat_1".into());
        let card = build_dir_browse_card(
            &key,
            &[browse_child("alpha", false), browse_child("beta", true)],
            None,
            bview("/work", false, "", 1),
            false,
            None,
        );
        let text = card.to_string();
        assert_eq!(card["schema"], "2.0");
        assert!(text.contains("\"tag\":\"form\""), "one root form: {text}");
        assert!(text.contains("\"name\":\"dir_browse\""), "form name: {text}");
        assert!(
            text.contains("browsego|chat_1|chat_1") && text.contains("browsepick|chat_1|chat_1"),
            "path submit names: {text}"
        );
        assert!(text.contains("📦 beta"), "repo marker: {text}");
        assert!(text.contains("📁 alpha"), "plain dir marker: {text}");
        assert!(text.contains("\"view\":\"browse\""), "view routing: {text}");
        assert!(
            text.contains("\"op\":\"recent\"") && text.contains("← 最近目录"),
            "back-to-recent control: {text}"
        );
        // Schema 2.0: no v1 `action` container anywhere.
        assert!(
            !text.contains("\"tag\":\"action\""),
            "no v1 action container: {text}"
        );
    }

    #[test]
    fn browse_card_shows_the_filter_only_when_asked_and_echoes_the_keyword() {
        let key = crate::config::ThreadKey::new("c".into(), "c".into());
        let without = build_dir_browse_card(&key, &[], None, bview("/work", false, "", 1), false, None);
        assert!(
            !without.to_string().contains("\"name\":\"filter\""),
            "no filter input until asked: {without}"
        );
        let shown = build_dir_browse_card(&key, &[], None, bview("/work", false, "proj", 1), true, None);
        let text = shown.to_string();
        assert!(text.contains("\"name\":\"filter\""), "filter input: {text}");
        assert!(text.contains("browsefilter|c|c"), "filter submit name: {text}");
        assert!(
            text.contains("\"default_value\":\"proj\""),
            "echoes keyword: {text}"
        );
    }

    #[test]
    fn browse_card_dot_toggle_states_the_action_and_carries_the_current_flag() {
        let key = crate::config::ThreadKey::new("c".into(), "c".into());
        let shown = build_dir_browse_card(&key, &[], None, bview("/work", false, "", 1), false, None);
        let toggle = all_buttons(&shown)
            .into_iter()
            .find(|button| button["value"]["op"] == "show_hidden")
            .expect("a dot-toggle");
        assert_eq!(toggle["text"]["content"], "显示隐藏项");
        assert_eq!(toggle["value"]["show_hidden"], false, "carries the current flag");
        let dots = build_dir_browse_card(&key, &[], None, bview("/work", true, "", 1), false, None);
        let toggle = all_buttons(&dots)
            .into_iter()
            .find(|button| button["value"]["op"] == "show_hidden")
            .expect("a dot-toggle");
        assert_eq!(toggle["text"]["content"], "隐藏点开头的项");
        assert_eq!(toggle["value"]["show_hidden"], true);
    }

    #[test]
    fn browse_card_file_rows_unmap_to_the_viewer_and_carry_the_listing() {
        let key = crate::config::ThreadKey::new("chat_1".into(), "chat_1".into());
        let mut children: Vec<BrowseChild> = (1..=7).map(|i| browse_child(&format!("p{i}"), false)).collect();
        children.push(browse_file("notes.md", 1536));
        let card = build_dir_browse_card(&key, &children, None, bview("/work", true, "no", 2), true, None);
        let file = all_buttons(&card)
            .into_iter()
            .find(|button| button["value"]["action"] == "file")
            .unwrap_or_else(|| panic!("a file row: {card}"));
        assert_eq!(file["text"]["content"], "📄 notes.md · 1.5 KB");
        assert_eq!(file["value"]["path"], "/work/notes.md");
        // The listing's whole state rides the row, so 返回目录 restores it.
        assert_eq!(file["value"]["dir"], "/work");
        assert_eq!(file["value"]["show_hidden"], true);
        assert_eq!(file["value"]["keyword"], "no");
        assert_eq!(file["value"]["page"], 2);
        // A file row is never browse-card routing, so it offers no `pick`/`topic`.
        assert!(file["value"].get("op").is_none(), "no op on a file row: {file}");
        assert!(file["value"].get("view").is_none(), "not browse routing: {file}");
        assert!(
            card.to_string().contains("的子目录与文件"),
            "the header names both kinds: {card}"
        );
        // A viewer opened with no listing to return to (#743's tool panel) gets
        // the same payload without the browse state, so no 返回目录 is rendered.
        let bare = browse_file_value(&key, "/work/notes.md", None);
        assert_eq!(bare["path"], "/work/notes.md");
        assert!(bare.get("dir").is_none(), "no listing, no return target: {bare}");
    }

    /// A Tool Panel's 「📂 打开」 payload (ADR-0083 §8, #743): the same
    /// `action: "file"` + verbatim `path` routing a browse file row travels
    /// by — one shared constructor, so the two entry points cannot drift —
    /// with the ROW's Chat/Topic swapped for the panel's Session, which the
    /// click resolves back to a Chat/Topic from the SessionStore. No `op`
    /// (that would route to #739's transfer) and no listing state (a tool
    /// panel has none to return to).
    #[test]
    fn tool_panel_open_payload_shares_the_file_open_routing() {
        let tool = tool_file_value("ses_1", "/work/report.md");
        let row = browse_file_value(
            &crate::config::ThreadKey::new("chat_1".into(), "chat_1".into()),
            "/work/report.md",
            None,
        );
        assert_eq!(tool["action"], row["action"], "one action tag: {tool} vs {row}");
        assert_eq!(tool["path"], row["path"], "one path key: {tool} vs {row}");
        assert_eq!(tool["session_id"], "ses_1");
        assert!(tool.get("op").is_none(), "no op: the click opens, never sends");
        assert!(tool.get("dir").is_none(), "no listing, no return target: {tool}");
        assert!(
            tool.get("chat_id").is_none(),
            "a live Turn card knows no Chat: {tool}"
        );
    }

    /// A Tool Panel's 「🌐 预览网页」 payload (#778) is the open payload plus
    /// `op: "preview"` — the same `action: "file"`, path and Session, so the
    /// transfer routes identically, with the op marking the one difference: the
    /// click sends, and no card is rewritten.
    #[test]
    fn tool_panel_page_payload_marks_the_preview_op() {
        let preview = tool_page_value("ses_1", "/work/index.html");
        let open = tool_file_value("ses_1", "/work/index.html");
        assert_eq!(preview["action"], open["action"], "one action tag: {preview}");
        assert_eq!(preview["path"], open["path"], "one path key: {preview}");
        assert_eq!(preview["session_id"], "ses_1");
        assert_eq!(
            preview["op"], "preview",
            "the op the dispatch routes by: {preview}"
        );
        assert!(open.get("op").is_none(), "the open payload stays op-less: {open}");
        assert!(preview.get("chat_id").is_none(), "no Chat rides along: {preview}");
        assert!(
            preview.get("dir").is_none(),
            "no listing, no return target: {preview}"
        );
    }

    #[test]
    fn browse_card_pager_rides_the_browse_view() {
        let key = crate::config::ThreadKey::new("chat_9".into(), "chat_9".into());
        let children: Vec<BrowseChild> = (1..=13).map(|i| browse_child(&format!("p{i}"), false)).collect();
        let card = build_dir_browse_card(&key, &children, None, bview("/work", true, "k", 2), true, None);
        let pager = all_buttons(&card)
            .into_iter()
            .find(|button| button["value"]["op"] == "page")
            .expect("a pager");
        assert_eq!(pager["value"]["view"], "browse");
        assert_eq!(pager["value"]["dir"], "/work");
        assert_eq!(
            pager["value"]["show_hidden"], "true",
            "the flag rides as a string"
        );
        assert_eq!(pager["value"]["page"], 1, "prev-page target");
    }

    #[test]
    fn browse_breadcrumb_truncates_a_deep_path() {
        let key = crate::config::ThreadKey::new("c".into(), "c".into());
        let card = build_dir_browse_card(&key, &[], None, bview("/a/b/c/d/e/f", false, "", 1), false, None);
        let crumb = card["body"]["elements"]
            .as_array()
            .unwrap()
            .iter()
            .find(|e| e["tag"] == "column_set" && e["flex_mode"] == "flow")
            .expect("breadcrumb");
        let labels: Vec<String> = crumb["columns"]
            .as_array()
            .unwrap()
            .iter()
            .map(|c| c["elements"][0]["text"]["content"].as_str().unwrap().to_string())
            .collect();
        assert!(
            labels.contains(&"…".to_string()),
            "ellipsis for the dropped head: {labels:?}"
        );
        assert!(
            labels.contains(&"e".to_string()) && labels.contains(&"f".to_string()),
            "tail kept: {labels:?}"
        );
        assert!(!labels.contains(&"b".to_string()), "head dropped: {labels:?}");
    }

    #[test]
    fn browse_card_marks_the_current_directory() {
        let key = crate::config::ThreadKey::new("c".into(), "c".into());
        let card = build_dir_browse_card(
            &key,
            &[],
            Some("/work"),
            bview("/work", false, "", 1),
            false,
            None,
        );
        let pick = all_buttons(&card)
            .into_iter()
            .find(|button| button["value"]["op"] == "pick")
            .expect("pick button");
        assert_eq!(pick["text"]["content"], "✅ 当前");
    }
}
