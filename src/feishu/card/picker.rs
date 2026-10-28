use serde_json::json;

use super::MAX_CARD_JSON_CHARS;
use super::shell::card_shell;
use super::truncate_md;
use super::with_callback_field;

/// One picker option: the button's label, the callback value a click sends, and
/// the provider a **model preset** pairs with (issue #910). The provider is an
/// extra callback field, emitted only when set, so every other card's bytes are
/// unchanged.
#[derive(Clone, Debug, Default)]
struct PickerOption {
    label: String,
    value: String,
    provider: Option<String>,
}

impl From<(String, String)> for PickerOption {
    fn from((label, value): (String, String)) -> Self {
        Self {
            label,
            value,
            provider: None,
        }
    }
}

/// The picker row for one client-declared model option.
fn picker_option(option: &crate::backend::types::ModelOption) -> PickerOption {
    PickerOption {
        label: option.display_label().to_string(),
        value: option.callback_value().to_string(),
        provider: option.provider.clone(),
    }
}

/// A generic option-picker card: one button per option. Shared by the
/// `/agent`, `/model` and `/autoaccept` dual-form cards. Each button carries
/// the given `action` tag + the routing payload (thread_key) + the option
/// value, so the ack routes the choice back to the right thread.
fn option_picker_card(
    header: &str,
    intro: &str,
    thread_key: &crate::config::ThreadKey,
    action: &str,
    options: &[(String, String)],
) -> serde_json::Value {
    let options: Vec<PickerOption> = options.iter().cloned().map(PickerOption::from).collect();
    picker_card(
        header,
        intro,
        thread_key,
        action,
        None,
        false,
        None,
        &options,
        &[],
        None,
        None,
    )
}

/// The `/model` picker-card back button's value: clicking it returns from a
/// provider's model page to the full provider list. A named constant (not a
/// bare string literal) so the handler's comparison cannot typo-silently break
/// navigation.
pub(crate) const PICKER_BACK_TO_PROVIDERS: &str = "__providers__";

/// The callback `level` of the `/model` root page's provider buttons (and of
/// the back button that returns to it) — a **navigation** click: the handler
/// asks the client for the drilled surface. A model page's level is
/// [`PICKER_LEVEL_MODEL`]. A named constant (not a bare string literal) so the
/// handler's comparison cannot typo-silently break navigation.
pub(crate) const PICKER_LEVEL_PROVIDER: &str = "provider";

/// The callback `level` of a **provider selection** button (Codex's
/// thread-level `modelProvider`, issue #910). It is deliberately not
/// [`PICKER_LEVEL_PROVIDER`]: a click here switches a thread's channel, while
/// that one drills into a provider's models — the same scope *name*, two
/// different actions.
pub(crate) const PICKER_LEVEL_PROVIDER_PICK: &str = "provider_switch";

/// The callback `level` of a concrete model-ref button. A fixed tag, never the
/// scope id: a scope's name is its identity, not a command, so no client's
/// naming can turn a model pick into navigation.
pub(crate) const PICKER_LEVEL_MODEL: &str = "model";

/// The callback `level` a scope's option buttons carry. The surface states what
/// a pick does ([`crate::backend::types::PickKind`]); the card maps it to the
/// wire tag the bridge branches on, so a scope id never doubles as a command.
fn scope_level(scope: &crate::backend::types::ModelScope) -> &str {
    use crate::backend::types::PickKind;
    match scope.pick {
        PickKind::DrillDown => PICKER_LEVEL_PROVIDER,
        PickKind::Provider => PICKER_LEVEL_PROVIDER_PICK,
        PickKind::Model => PICKER_LEVEL_MODEL,
    }
}

/// A picker card whose buttons carry an optional `level` (two-step navigation
/// for the `/model` provider → model flow) and an optional leading "back"
/// button. Each button's callback payload is `{action, chat_id, thread_id,
/// [level], value}` plus every `callback_fields` pair. An optional leading clear button
/// (`clear = Some((label, action))`) carries its OWN action tag: "clear/reset"
/// never shares the value namespace with the options, so an option literally
/// named like a clear verb stays selectable (ADR-0020's `--reset` decoupling —
/// clearing is a mechanism, not a value).
#[allow(clippy::too_many_arguments)] // picker builder: every knob is a first-class card axis
fn picker_card(
    header: &str,
    intro: &str,
    thread_key: &crate::config::ThreadKey,
    action: &str,
    level: Option<&str>,
    back: bool,
    clear: Option<(&str, &str)>,
    options: &[PickerOption],
    callback_fields: &[(&str, &str)],
    note: Option<&str>,
    free_text: Option<&str>,
) -> serde_json::Value {
    let mut elements: Vec<serde_json::Value> = vec![json!({
        "tag": "markdown",
        "content": crate::feishu::card::sanitize::sanitize_markdown(intro)
    })];
    if back {
        elements.push(back_button(thread_key, action, callback_fields));
    }
    if let Some((label, clear_action)) = clear {
        elements.push(json!({
            "tag": "button",
            "text": { "tag": "plain_text", "content": label },
            "type": "default",
            "width": "fill",
            "value": with_callback_field(json!({
                "action": clear_action,
                "chat_id": thread_key.chat_id,
                "thread_id": thread_key.thread_id,
                "value": "",
            }), callback_fields),
        }));
    }
    for option in options {
        elements.push(option_button(thread_key, action, option, level, callback_fields));
    }
    if let Some(note) = note {
        elements.push(markdown_element(note));
    }
    if let Some(free_text) = free_text {
        elements.push(markdown_element(free_text));
    }
    card_shell(header, "blue", elements)
}

/// The leading "back to the root page" button of a drilled-in picker page.
fn back_button(
    thread_key: &crate::config::ThreadKey,
    action: &str,
    callback_fields: &[(&str, &str)],
) -> serde_json::Value {
    json!({
        "tag": "button",
        "text": { "tag": "plain_text", "content": "← 返回全部 provider" },
        "type": "default",
        "width": "fill",
        "value": with_callback_field(json!({
            "action": action,
            "chat_id": thread_key.chat_id,
            "thread_id": thread_key.thread_id,
            "level": PICKER_LEVEL_PROVIDER,
            "value": PICKER_BACK_TO_PROVIDERS,
        }), callback_fields),
    })
}

/// One option button: its label, its callback value, the `level` the handler
/// dispatches on, and — for a model preset that pairs a model with a provider
/// (#910) — that provider as an extra `provider` field. The field is omitted
/// when unset, so every card without a pairing keeps its exact bytes.
fn option_button(
    thread_key: &crate::config::ThreadKey,
    action: &str,
    option: &PickerOption,
    level: Option<&str>,
    callback_fields: &[(&str, &str)],
) -> serde_json::Value {
    let mut payload = json!({
        "action": action,
        "chat_id": thread_key.chat_id,
        "thread_id": thread_key.thread_id,
        "value": option.value,
    });
    if let Some(l) = level {
        payload["level"] = json!(l);
    }
    if let Some(provider) = &option.provider {
        payload["provider"] = json!(provider);
    }
    json!({
        "tag": "button",
        "text": { "tag": "plain_text", "content": option.label },
        "type": "default",
        "width": "fill",
        "value": with_callback_field(payload, callback_fields),
    })
}

fn markdown_element(content: &str) -> serde_json::Value {
    json!({
        "tag": "markdown",
        "content": crate::feishu::card::sanitize::sanitize_markdown(content)
    })
}

/// The `/agent` picker card: one button per available agent plus a separate
/// "默认（清除）" button carrying its OWN `agent_clear` action tag — clearing is
/// a mechanism, never a value word, so an agent literally named `default` stays
/// selectable (ADR-0020). The intro shows the CURRENT agent: the per-session
/// override (`override_agent`) when set, else the server's default agent name
/// (`default_agent`, annotated 默认). Falls back to an empty intro when no
/// agents are listed (the backend is unreachable).
///
/// The options are the official picker's set — visible, non-`subagent` mode
/// agents (`packages/tui/src/context/local.tsx` filters `mode !== "subagent"`
/// and `hidden`): a subagent-mode agent is a delegated worker, not something a
/// session should run as.
pub fn build_agent_card(
    thread_key: &crate::config::ThreadKey,
    agents: &[crate::backend::types::AgentInfo],
    override_agent: Option<&str>,
    default_agent: Option<&str>,
) -> serde_json::Value {
    let options: Vec<(String, String)> = agents
        .iter()
        .filter(|a| a.hidden != Some(true) && a.mode.as_deref() != Some("subagent"))
        .map(|a| (a.name.clone(), a.name.clone()))
        .collect();
    let intro = if options.is_empty() {
        "_(没有可用 agent)_".to_string()
    } else {
        let current = match (override_agent, default_agent) {
            (Some(name), _) => format!("**当前 Agent**：`{name}`\n"),
            (None, Some(name)) => format!("**当前 Agent**：`{name}`（默认）\n"),
            (None, None) => String::new(),
        };
        format!("{current}**选择 agent**（下一条消息开始生效）：")
    };
    let empty = options.is_empty();
    picker_card(
        "🤖 选择 Agent",
        &intro,
        thread_key,
        "agent",
        None,
        false,
        if empty {
            None
        } else {
            Some(("默认（清除）", "agent_clear"))
        },
        &options
            .iter()
            .cloned()
            .map(PickerOption::from)
            .collect::<Vec<_>>(),
        &[],
        None,
        None,
    )
}

/// Render one page of a client-declared [`ModelSurface`] as `/model` cards.
///
/// The surface owns the shape: one scope is one titled section (a provider
/// list on OpenCode, a provider's models, Codex's flat catalog). A page with
/// one scope renders a single card whose header is that scope's title; a page
/// with several scopes renders one card with a section per scope, falling
/// back to per-scope cards when the combined page exceeds the card budget.
/// Every scope's options are chunked through [`chunk_picker_cards`], so any
/// provider/model count stays under Feishu's card limits.
///
/// `back` asks for the leading "← 返回全部 provider" button (a drilled-in page
/// of the OpenCode two-level flow); its callback value is
/// [`PICKER_BACK_TO_PROVIDERS`] at level [`PICKER_LEVEL_PROVIDER`].
pub fn build_model_surface_cards(
    thread_key: &crate::config::ThreadKey,
    surface: &crate::backend::types::ModelSurface,
    back: bool,
) -> Vec<serde_json::Value> {
    match surface.scopes.as_slice() {
        [] => vec![picker_card(
            "🎯 选择模型",
            &format!("{}_(没有可用模型)_", current_model_line(surface.current.as_ref())),
            thread_key,
            "model",
            None,
            back,
            None,
            &[],
            &[],
            None,
            None,
        )],
        [scope] => scope_cards(thread_key, surface.current.as_ref(), scope, back),
        scopes => {
            let combined = combined_surface_card(thread_key, surface.current.as_ref(), scopes, back);
            if combined_card_fits(&combined) {
                vec![combined]
            } else {
                scopes
                    .iter()
                    .flat_map(|scope| scope_cards(thread_key, surface.current.as_ref(), scope, back))
                    .collect()
            }
        }
    }
}

/// Whether the combined multi-scope card fits Feishu's budget. Unlike
/// [`chunk_picker_cards`], this path does not chunk, so it must charge section
/// headings, notes, the hand-write hint and the back button — every non-button
/// element — against the same conservative component budget the chunker uses
/// for buttons ([`MAX_PICKER_BUTTONS_PER_CARD`]), as well as the byte ceiling.
/// A surface that fails either check falls back to the per-scope cards, which
/// do chunk.
fn combined_card_fits(card: &serde_json::Value) -> bool {
    let components = card["body"]["elements"]
        .as_array()
        .map(|elements| elements.len())
        .unwrap_or(usize::MAX);
    card.to_string().len() <= MAX_CARD_JSON_CHARS && components <= MAX_PICKER_BUTTONS_PER_CARD
}

/// One scope's card page(s): its intro (with the current-model line on top),
/// the back button, its options, then its note and hand-write hint.
fn scope_cards(
    thread_key: &crate::config::ThreadKey,
    current: Option<&crate::backend::types::ModelSelection>,
    scope: &crate::backend::types::ModelScope,
    back: bool,
) -> Vec<serde_json::Value> {
    let header = format!("🎯 {}", scope.title);
    let mut intro = current_model_line(current);
    if let Some(scope_intro) = &scope.intro {
        intro.push_str(scope_intro);
    }
    let options: Vec<PickerOption> = scope.options.iter().map(picker_option).collect();
    if options.is_empty() {
        return vec![picker_card(
            &header,
            &intro,
            thread_key,
            "model",
            Some(scope_level(scope)),
            back,
            None,
            &[],
            &[],
            scope.note.as_deref(),
            scope.free_text.as_deref(),
        )];
    }
    // A grouped scope (#908: the built-in catalog, config presets and probed
    // providers are separate sections inside one `model` scope) needs the group
    // heading rendered between options, so it cannot ride the flat chunker.
    if scope.options.iter().any(|option| option.group.is_some()) {
        return grouped_scope_cards(thread_key, current, scope, back);
    }
    chunk_picker_cards(
        &header,
        &intro,
        thread_key,
        "model",
        Some(scope_level(scope)),
        back,
        None,
        &options,
        &[],
        scope.note.as_deref(),
        scope.free_text.as_deref(),
    )
}

/// A multi-scope page in one card: a section heading per scope, its options,
/// its note and its hand-write hint. Used when the client declares more than
/// one scope (#908's provider + catalog page).
fn combined_surface_card(
    thread_key: &crate::config::ThreadKey,
    current: Option<&crate::backend::types::ModelSelection>,
    scopes: &[crate::backend::types::ModelScope],
    back: bool,
) -> serde_json::Value {
    let mut elements: Vec<serde_json::Value> = Vec::new();
    let intro = current_model_line(current);
    if !intro.is_empty() {
        elements.push(markdown_element(&intro));
    }
    if back {
        elements.push(back_button(thread_key, "model", &[]));
    }
    for scope in scopes {
        let mut heading = format!("**{}**", scope.title);
        if let Some(scope_intro) = &scope.intro {
            heading.push('\n');
            heading.push_str(scope_intro);
        }
        elements.push(markdown_element(&heading));
        let mut current_group: Option<&str> = None;
        for option in &scope.options {
            if option.group.as_deref() != current_group {
                if let Some(group) = option.group.as_deref() {
                    elements.push(markdown_element(&format!("**{group}**")));
                }
                current_group = option.group.as_deref();
            }
            elements.push(option_button(
                thread_key,
                "model",
                &picker_option(option),
                Some(scope_level(scope)),
                &[],
            ));
        }
        if let Some(note) = &scope.note {
            elements.push(markdown_element(note));
        }
        if let Some(free_text) = &scope.free_text {
            elements.push(markdown_element(free_text));
        }
    }
    card_shell("🎯 选择模型", "blue", elements)
}

/// One grouped scope's card page(s): options keep their group order, every page
/// that starts in the middle of a group repeats that group's heading, and the
/// usual intro / back button / note / hand-write hint are unchanged. Charging
/// the heading as a component too keeps a many-group surface inside Feishu's
/// element ceiling, not just its byte ceiling.
fn grouped_scope_cards(
    thread_key: &crate::config::ThreadKey,
    current: Option<&crate::backend::types::ModelSelection>,
    scope: &crate::backend::types::ModelScope,
    back: bool,
) -> Vec<serde_json::Value> {
    let header = format!("🎯 {}", scope.title);
    let mut intro = current_model_line(current);
    if let Some(scope_intro) = &scope.intro {
        intro.push_str(scope_intro);
    }
    let pages = grouped_page_windows(scope, &intro);
    let n_pages = pages.len();
    pages
        .into_iter()
        .enumerate()
        .map(|(index, page)| {
            let mut intro_text = intro.clone();
            if n_pages > 1 {
                intro_text.push_str(&format!("（第 {} / {} 页）", index + 1, n_pages));
            }
            grouped_picker_card(
                &header,
                &intro_text,
                thread_key,
                Some(scope_level(scope)),
                back,
                &page,
                scope.note.as_deref(),
                scope.free_text.as_deref(),
            )
        })
        .collect()
}

/// Page windows over a grouped scope. An option costs one picker row and its
/// group heading one more (a page that starts mid-group re-renders it), so a
/// page with G headings and O options stays at `G + O` rows and roughly
/// `G + 2O` Feishu components. A single oversized option still gets its own
/// page, exactly as [`chunk_picker_cards`] guarantees.
fn grouped_page_windows<'a>(
    scope: &'a crate::backend::types::ModelScope,
    intro: &str,
) -> Vec<Vec<&'a crate::backend::types::ModelOption>> {
    let mut pages: Vec<Vec<&crate::backend::types::ModelOption>> = Vec::new();
    let mut page: Vec<&crate::backend::types::ModelOption> = Vec::new();
    let base_bytes = intro.len() + 64;
    let mut bytes = base_bytes;
    let mut rows = 0usize;
    let mut current_group: Option<&str> = None;
    for option in &scope.options {
        let option_bytes = 200
            + option.display_label().len() * 2
            + option.callback_value().len()
            + option.provider.as_deref().map_or(0, str::len);
        let mut group_changed = option.group.as_deref() != current_group;
        let mut heading_bytes = if group_changed {
            option.group.as_deref().map_or(0, |group| 100 + group.len())
        } else {
            0
        };
        let mut cost = 1 + usize::from(group_changed && option.group.is_some());
        if !page.is_empty()
            && (rows + cost > MAX_PICKER_BUTTONS_PER_CARD
                || bytes + heading_bytes + option_bytes > MAX_CARD_JSON_CHARS)
        {
            pages.push(std::mem::take(&mut page));
            bytes = base_bytes;
            rows = 0;
            current_group = None;
            group_changed = option.group.as_deref() != current_group;
            heading_bytes = if group_changed {
                option.group.as_deref().map_or(0, |group| 100 + group.len())
            } else {
                0
            };
            cost = 1 + usize::from(group_changed && option.group.is_some());
        }
        bytes += heading_bytes + option_bytes;
        rows += cost;
        if group_changed {
            current_group = option.group.as_deref();
        }
        page.push(option);
    }
    if !page.is_empty() {
        pages.push(page);
    }
    if pages.is_empty() {
        pages.push(Vec::new());
    }
    pages
}

/// One page of a grouped scope: intro, optional back button, the page's options
/// with a bold heading whenever the group changes, then the note and the
/// hand-write hint.
#[allow(clippy::too_many_arguments)] // picker builder: every knob is a first-class card axis
fn grouped_picker_card(
    header: &str,
    intro: &str,
    thread_key: &crate::config::ThreadKey,
    level: Option<&str>,
    back: bool,
    options: &[&crate::backend::types::ModelOption],
    note: Option<&str>,
    free_text: Option<&str>,
) -> serde_json::Value {
    let mut elements: Vec<serde_json::Value> = vec![markdown_element(intro)];
    if back {
        elements.push(back_button(thread_key, "model", &[]));
    }
    let mut current_group: Option<&str> = None;
    for option in options {
        if option.group.as_deref() != current_group {
            if let Some(group) = option.group.as_deref() {
                elements.push(markdown_element(&format!("**{group}**")));
            }
            current_group = option.group.as_deref();
        }
        elements.push(option_button(
            thread_key,
            "model",
            &picker_option(option),
            level,
            &[],
        ));
    }
    if let Some(note) = note {
        elements.push(markdown_element(note));
    }
    if let Some(free_text) = free_text {
        elements.push(markdown_element(free_text));
    }
    card_shell(header, "blue", elements)
}

/// The confirmation card for a **thread-level provider switch** (issue #910).
///
/// A provider is only half a model — the old model name sent to a new provider
/// is a 404 — so the card always names the **pair**, and its 确认 button carries
/// it together with the target **session**, so a card that outlived a
/// `/switch`, a delete or a recreate is refused instead of switching a
/// different thread. 取消 carries its own action tag and changes nothing.
pub fn build_provider_confirm_card(
    thread_key: &crate::config::ThreadKey,
    session_id: &str,
    provider: &str,
    model: &str,
) -> serde_json::Value {
    let intro = format!(
        "**切换为 `{provider}/{model}`？**\n\
         这会释放并重新加载这条线程（`thread/unsubscribe` → `thread/resume` → `thread/read`），\
         把 provider 和模型一起换掉：旧模型名在新 provider 上会 404。\n\
         **同一个线程上的其他客户端（Codex TUI / VS Code / 桌面端），以及另一个共用同一个 \
         `CODEX_HOME` 的 cola 实例，也会看到这次变化**；如果它们仍持有该线程，切换不会生效，\
         cola 会报错而不会假装成功。"
    );
    let cancel = json!({
        "tag": "button",
        "text": { "tag": "plain_text", "content": "取消" },
        "type": "default",
        "width": "fill",
        "value": {
            "action": "model_provider_cancel",
            "chat_id": thread_key.chat_id,
            "thread_id": thread_key.thread_id,
            "value": "",
        },
    });
    let confirm = json!({
        "tag": "button",
        "text": { "tag": "plain_text", "content": "✅ 确认切换" },
        "type": "default",
        "width": "fill",
        "value": {
            "action": "model_provider_confirm",
            "chat_id": thread_key.chat_id,
            "thread_id": thread_key.thread_id,
            "session_id": session_id,
            "provider": provider,
            "value": model,
        },
    });
    card_shell(
        "🎯 确认切换 Provider",
        "blue",
        vec![
            markdown_element(&intro),
            cancel,
            confirm,
            markdown_element("取消不会发出任何请求，线程保持原样。"),
        ],
    )
}

/// The acknowledgement a confirmed switch returns before the RPC starts: the
/// release + resume + read-back runs off the card-ack path, and this card is
/// replaced by the outcome when it lands (issue #910).
pub fn build_provider_switching_card(provider: &str, model: &str) -> serde_json::Value {
    card_shell(
        "🎯 正在切换 Provider",
        "blue",
        vec![markdown_element(&format!(
            "正在把这条线程切换到 `{provider}/{model}`：释放订阅 → `thread/resume` → 回读校验。\n\
             结果会替换这张卡片。"
        ))],
    )
}

/// The card a confirmed provider/model switch ends on. It replaces the
/// confirmation card, so the (now stale) buttons cannot be pressed again.
pub fn build_provider_switched_card(provider: &str, model: &str) -> serde_json::Value {
    card_shell(
        "🎯 Provider 已切换",
        "green",
        vec![markdown_element(&format!(
            "**当前**：`{provider}/{model}`（已由 `thread/read` 回读确认）\n\
             这条线程的模型随 provider 一起换掉了；同一个线程上的其他客户端现在也会使用它。\n\
             再次发送 `/model` 可查看最新状态。"
        ))],
    )
}

/// The thread already ran the pair: the switch was a no-op (a peer had already
/// moved it, or the read-back agrees). Success, but nothing changed.
pub fn build_provider_already_card(provider: &str, model: &str) -> serde_json::Value {
    card_shell(
        "🎯 Provider 已经是这个值",
        "green",
        vec![markdown_element(&format!(
            "这条线程已经在 `{provider}/{model}` 上（`thread/read` 回读确认），不需要切换。"
        ))],
    )
}

/// A failed provider switch: the thread keeps its previous provider/model (no
/// silent fallback), and both the target and the failure are named.
pub fn build_provider_switch_error_card(provider: &str, model: &str, error: &str) -> serde_json::Value {
    card_shell(
        "⚠️ Provider 切换失败",
        "red",
        vec![markdown_element(&format!(
            "线程保持原来的 provider/model，本地记录也没有改动。\n\n\
             **目标**：`{provider}/{model}`\n**错误**：{error}"
        ))],
    )
}

/// The pair could not be recorded, so the switch never started: a click that
/// cannot be saved must not claim success, and the thread is untouched (issue
/// #910). The pair is written **before** the RPC precisely so a prompt racing
/// the switch cannot send the old model name on the new provider.
pub fn build_provider_save_blocked_card(pair: &str, error: &str) -> serde_json::Value {
    card_shell(
        "⚠️ Provider 切换没有开始",
        "orange",
        vec![markdown_element(&format!(
            "无法记录这次选择（`{pair}`）：{error}。\n\
             线程保持原来的 provider/model，也没有发出任何切换请求。"
        ))],
    )
}

/// The leading "current model" line shared by the `/model` picker cards.
/// Empty when there is no session or no rung of the effective-model ladder
/// resolves — the card then shows only its "选择 …" intro, exactly as before.
fn current_model_line(current: Option<&crate::backend::types::ModelSelection>) -> String {
    current
        .map(|c| format!("**当前模型**：`{}`\n", c.label))
        .unwrap_or_default()
}

/// Max picker buttons per card. Feishu counts each button's inner `plain_text`
/// as a separate element against the JSON 2.0 ceiling of 200 elements+components
/// (error 300305 / 11310 "element exceeds the limit" — measured via the cardkit
/// create API at 99 buttons max with an intro markdown). 80 keeps a comfortable
/// margin including the back button's two elements.
pub const MAX_PICKER_BUTTONS_PER_CARD: usize = 80;

/// Build picker cards from `options`, chunking so each card stays under
/// Feishu's component and JSON-size ceilings. Each card beyond the first
/// carries a page hint so the user knows there is more.
#[allow(clippy::too_many_arguments)] // picker builder: every knob is a first-class card axis
fn chunk_picker_cards(
    header: &str,
    intro: &str,
    thread_key: &crate::config::ThreadKey,
    action: &str,
    level: Option<&str>,
    back: bool,
    clear: Option<(&str, &str)>,
    options: &[PickerOption],
    callback_fields: &[(&str, &str)],
    note: Option<&str>,
    free_text: Option<&str>,
) -> Vec<serde_json::Value> {
    let mut pages: Vec<&[PickerOption]> = Vec::new();
    let mut start = 0usize;
    // One button's estimated serialized bytes: ~200 of structure (tags, the
    // routing callback fields) + the label's UTF-8 (shown twice in the button
    // JSON) + the callback VALUE, which for a `/skill` row is the skill's exact
    // id — an unbudgeted id would let the card exceed Feishu's limit (spec #652,
    // ticket #655).
    let button_bytes = |option: &PickerOption| {
        200 + option.label.len() * 2
            + option.value.len()
            + option.provider.as_deref().map_or(0, |provider| provider.len())
    };
    while start < options.len() {
        // Estimate JSON bytes like the streaming splitter; bound the button
        // COUNT by the picker budget, not the streaming 150-component constant —
        // buttons cost ~2 elements each against Feishu's 200-element ceiling.
        let mut bytes = intro.len() + 64;
        let mut end = start;
        while end < options.len()
            && end - start < MAX_PICKER_BUTTONS_PER_CARD
            && bytes + button_bytes(&options[end]) <= MAX_CARD_JSON_CHARS
        {
            bytes += button_bytes(&options[end]);
            end += 1;
        }
        if end == start {
            end = start + 1; // a single oversized option still gets its own card
        }
        pages.push(&options[start..end]);
        start = end;
    }
    let n_pages = pages.len();
    let mut cards: Vec<serde_json::Value> = Vec::with_capacity(n_pages);
    for (i, page) in pages.into_iter().enumerate() {
        let mut intro_text = intro.to_string();
        if n_pages > 1 {
            intro_text.push_str(&format!("（第 {} / {} 页）", i + 1, n_pages));
        }
        cards.push(picker_card(
            header,
            &intro_text,
            thread_key,
            action,
            level,
            back,
            clear,
            page,
            callback_fields,
            note,
            free_text,
        ));
    }
    cards
}

/// The `/autoaccept` toggle card: two buttons showing the current state.
pub fn build_autoaccept_card(thread_key: &crate::config::ThreadKey, current_on: bool) -> serde_json::Value {
    let intro = format!(
        "**当前自动审批：{}**\n自动审批开启后，本会话的权限请求不再弹卡，直接 Allow。",
        if current_on { "🔁 开" } else { "❌ 关" }
    );
    let options = vec![
        ("🔁 开启自动审批".to_string(), "on".to_string()),
        ("❌ 关闭自动审批".to_string(), "off".to_string()),
    ];
    option_picker_card("🔁 自动审批", &intro, thread_key, "autoaccept", &options)
}

/// The `/think` picker card (ADR-0020): the current model, its declared
/// variants (each model's own set — no universal scale), and a separate
/// "默认（清除）" button carrying its OWN `think_clear` action tag. Clearing is
/// a mechanism, never a value word: a variant literally named `default`/`off`/
/// `reset` stays selectable, and nothing depends on the server's `default`
/// sentinel. `current` is the session's active variant (None = server default).
/// Only built when `variants` is non-empty — the caller replies a text message
/// for models that declare none.
pub fn build_think_card(
    thread_key: &crate::config::ThreadKey,
    provider: &str,
    model: &str,
    current: Option<&str>,
    variants: &[String],
) -> serde_json::Value {
    let label = format!("{provider}/{model}");
    let current_label = current
        .map(|v| format!("`{v}`"))
        .unwrap_or_else(|| "默认".to_string());
    let intro =
        format!("**当前模型**：`{label}`\n**当前思考等级**：{current_label}\n选择后下一条消息开始生效：");
    let options: Vec<(String, String)> = variants.iter().map(|v| (v.clone(), v.clone())).collect();
    picker_card(
        "🧠 思考等级",
        &intro,
        thread_key,
        "think",
        None,
        false,
        Some(("默认（清除）", "think_clear")),
        &options
            .iter()
            .cloned()
            .map(PickerOption::from)
            .collect::<Vec<_>>(),
        &[],
        None,
        None,
    )
}

/// How many characters a skill row's button label shows before clipping. A
/// button is a single line, so a long frontmatter description is clipped with a
/// "…" rather than bloating the card; the skill's `name` always leads. The
/// loaded-skill fold reuses it to bound the `name` in its panel title (spec
/// #652, ticket #655).
pub(crate) const SKILL_ROW_LABEL_CHARS: usize = 60;

/// The one-line skill description both the picker row and its text fallback
/// show (spec #652, ticket #656): the skill's `description` trimmed, an
/// empty/whitespace-only value dropped, and a folded/multi-line frontmatter
/// value clipped to its first line (trimmed again). `None` when the skill
/// declares no usable description, so both sites render the name alone — one
/// helper, so the row and the fallback cannot drift.
pub(crate) fn skill_description_line(skill: &crate::backend::SkillInfo) -> Option<&str> {
    skill
        .description
        .as_deref()
        .map(str::trim)
        .filter(|description| !description.is_empty())
        .map(|description| description.lines().next().unwrap_or("").trim())
}

/// Rows a `/skill` picker page shows at most (#664). A skill row is one short
/// line, so more fit per page than the session cards' six; the byte budget can
/// still shrink a page (see [`skill_pages`]), so a pathological id never builds
/// an over-limit card.
pub(crate) const SKILL_PAGE_ROWS: usize = 20;

/// The chrome (intro markdown + search form + pager) charged against the card
/// budget before the rows; generous, since the rows dominate for any realistic
/// catalog.
const SKILL_CARD_CHROME_BYTES: usize = 1_200;

/// The label of one skill's picker row: its `name`, plus its first-line
/// `description` when it declares one, clipped to [`SKILL_ROW_LABEL_CHARS`] so a
/// single button stays one bounded line.
fn skill_row_label(skill: &crate::backend::SkillInfo) -> String {
    match skill_description_line(skill) {
        Some(description) => truncate_md(
            &format!("{} — {}", skill.name, description),
            SKILL_ROW_LABEL_CHARS,
        ),
        None => truncate_md(&skill.name, SKILL_ROW_LABEL_CHARS),
    }
}

/// Estimated serialized bytes of one skill row's button: ~200 of structure
/// (tags, the routing callback fields) + the label's UTF-8, shown twice in the
/// button JSON, + the callback VALUE — the skill's exact id. An unbudgeted id
/// would let the card exceed Feishu's limit (spec #652, ticket #655).
fn skill_row_bytes(label: &str, id: &str) -> usize {
    200 + label.len() * 2 + id.len()
}

/// The page windows over the (already filtered) `skills` (#664). A page holds up
/// to [`SKILL_PAGE_ROWS`] rows while their estimated bytes stay under
/// [`MAX_CARD_JSON_CHARS`]; a single oversized row still gets its own page,
/// exactly as the old chunker guaranteed (spec #652/#655). Always at least one
/// (possibly empty) page, so an empty list still renders a first page.
fn skill_pages(skills: &[crate::backend::SkillInfo]) -> Vec<std::ops::Range<usize>> {
    let labels: Vec<String> = skills.iter().map(skill_row_label).collect();
    let mut pages = Vec::new();
    let mut start = 0usize;
    while start < skills.len() {
        let mut bytes = SKILL_CARD_CHROME_BYTES;
        let mut end = start;
        while end < skills.len()
            && end - start < SKILL_PAGE_ROWS
            && bytes + skill_row_bytes(&labels[end], &skills[end].id) <= MAX_CARD_JSON_CHARS
        {
            bytes += skill_row_bytes(&labels[end], &skills[end].id);
            end += 1;
        }
        if end == start {
            end = start + 1; // a single oversized row still gets its own page
        }
        pages.push(start..end);
        start = end;
    }
    if pages.is_empty() {
        pages.push(0..0);
    }
    pages
}

/// The `/skill` picker's filter (#664): each whitespace token of a
/// case-insensitive keyword is a substring of the skill's `id`, `name` or
/// `description` — the list cards' token-AND rule. An empty keyword keeps the
/// whole list.
pub(crate) fn filter_skills(
    skills: &[crate::backend::SkillInfo],
    keyword: &str,
) -> Vec<crate::backend::SkillInfo> {
    let keyword = keyword.trim().to_lowercase();
    if keyword.is_empty() {
        return skills.to_vec();
    }
    let tokens: Vec<&str> = keyword.split_whitespace().collect();
    skills
        .iter()
        .filter(|skill| {
            let id = skill.id.to_lowercase();
            let name = skill.name.to_lowercase();
            let description = skill.description.as_deref().unwrap_or("").to_lowercase();
            tokens
                .iter()
                .all(|token| id.contains(token) || name.contains(token) || description.contains(token))
        })
        .cloned()
        .collect()
}

/// The `/skill` picker (spec #652, ticket #656; paged and searchable per #664):
/// one button per registered skill — its `name`, plus its first-line
/// `description` when it declares one — so a user who does not know an id can
/// pick one. `skills` is the ALREADY keyword-filtered list; the card windows it
/// into pages of up to [`SKILL_PAGE_ROWS`] rows and renders a keyword search
/// form (when the list outgrows a page, or a keyword is active) and a pager
/// (when there is more than one page), both shared with the other list cards
/// (ADR-0051, ADR-0052).
///
/// The callback value on a row is the skill's generation identity (`id`); a tap
/// re-enters the message pipeline as `#<id>`. `chat_type` and `reply_message_id`
/// ride every row — and the search/pager buttons, so a rebuild re-stamps the
/// fresh rows — reconstructing the
/// [`ConversationKind`](crate::config::ConversationKind) the picker was sent in
/// and replying the loaded-skill card under the ORIGINAL user message, exactly
/// like a typed dispatch (spec #655). `error`, when set, leads the intro so the
/// SAME card answers both a bare `/skill` and an unknown id; a rebuild passes
/// `None`. An empty list opens the no-skills state (`无匹配技能` under a
/// keyword).
#[allow(clippy::too_many_arguments)] // picker builder: every knob is a card axis
pub(crate) fn build_skill_card(
    thread_key: &crate::config::ThreadKey,
    skills: &[crate::backend::SkillInfo],
    keyword: &str,
    page: usize,
    chat_type: &str,
    message_id: &str,
    error: Option<&str>,
) -> serde_json::Value {
    let callback_fields = [("chat_type", chat_type), ("reply_message_id", message_id)];
    let total = skills.len();
    let pages = skill_pages(skills);
    let total_pages = pages.len();
    let page = page.clamp(1, total_pages);
    let window = &pages[page - 1];

    let mut elements: Vec<serde_json::Value> = Vec::new();
    // The search box appears once the list outgrows a page — by the row cap OR
    // the byte budget, both of which drive `pages` — or a keyword is active (so
    // a narrowed result stays refinable and clearable) — ADR-0051's
    // conditional-visibility rule.
    if total_pages > 1 || !keyword.is_empty() {
        elements.push(super::session::search_form(
            "skill_search",
            "skillsearch",
            "skill",
            thread_key,
            None,
            "🔍 搜索技能名称 / ID / 描述",
            keyword,
            &callback_fields,
        ));
    }

    let mut intro = error.map(|e| format!("{e}\n")).unwrap_or_default();
    if total == 0 {
        intro.push_str(if keyword.is_empty() {
            "_(没有可用技能)_"
        } else {
            "_(无匹配技能)_"
        });
    } else if keyword.is_empty() {
        intro.push_str(&format!("**选择技能**（共 {total} 个，点击后以 `#<id>` 发送）："));
    } else {
        intro.push_str(&format!("**匹配 `{keyword}` 的技能**（共 {total} 个）："));
    }
    elements.push(json!({
        "tag": "markdown",
        "content": crate::feishu::card::sanitize::sanitize_markdown(&intro)
    }));

    for skill in &skills[window.start..window.end] {
        elements.push(json!({
            "tag": "button",
            "text": { "tag": "plain_text", "content": skill_row_label(skill) },
            "type": "default",
            "width": "fill",
            "value": with_callback_field(json!({
                "action": "skill",
                "chat_id": thread_key.chat_id,
                "thread_id": thread_key.thread_id,
                "value": skill.id,
            }), &callback_fields),
        }));
    }

    if let Some(pager) = super::session::pager_element(
        "skill",
        thread_key,
        keyword,
        None,
        page,
        total_pages,
        total,
        &callback_fields,
    ) {
        elements.push(pager);
    }

    card_shell("🧩 选择技能", "blue", elements)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::types::{ChangeScope, ModelOption, ModelScope, ModelSelection, ModelSurface};
    use crate::feishu::card::FEISHU_CARD_LIMIT_BYTES;

    fn surface(current: Option<ModelSelection>, scopes: Vec<ModelScope>) -> ModelSurface {
        ModelSurface { current, scopes }
    }

    fn scope(id: &str, title: &str, options: Vec<ModelOption>) -> ModelScope {
        ModelScope {
            id: id.into(),
            title: title.into(),
            changeable: ChangeScope::ThisTurn,
            pick: Default::default(),
            options,
            note: None,
            free_text: None,
            intro: None,
        }
    }

    /// The OpenCode root page: a `provider` scope whose pick drills into that
    /// provider's models (issue #910's explicit routing, not a name match).
    fn provider_scope(id: &str, title: &str, options: Vec<ModelOption>) -> ModelScope {
        ModelScope {
            pick: crate::backend::types::PickKind::DrillDown,
            ..scope(id, title, options)
        }
    }

    /// OpenCode's provider page keeps its hand-built shape: the same header,
    /// the same "选择 provider（共 N 个）" intro (with the current-model line on
    /// top), and the same chunking under Feishu's ceilings.
    #[test]
    fn opencode_provider_page_shape_unchanged_and_chunks() {
        let key = crate::config::ThreadKey::new("chat_1".into(), "chat_1".into());
        let providers: Vec<ModelOption> = (0..212)
            .map(|p| ModelOption::new(format!("provider-{p}")))
            .collect();
        let mut provider_scope = provider_scope("provider", "选择模型", providers.clone());
        provider_scope.intro = Some(format!("**选择 provider**（共 {} 个）：", providers.len()));
        let surface = surface(
            Some(ModelSelection {
                label: "opencode-go/deepseek-v4-flash@low".into(),
                reference: "opencode-go/deepseek-v4-flash".into(),
            }),
            vec![provider_scope],
        );
        let cards = build_model_surface_cards(&key, &surface, false);
        assert!(cards.len() > 1, "huge provider list must split: {}", cards.len());
        let mut total_buttons = 0;
        for card in &cards {
            let elements = card["body"]["elements"].as_array().unwrap();
            let buttons = elements.iter().filter(|e| e["tag"] == "button").count();
            total_buttons += buttons;
            assert!(
                buttons <= MAX_PICKER_BUTTONS_PER_CARD,
                "page over ceiling: {buttons}"
            );
            assert!(
                card.to_string().len() <= FEISHU_CARD_LIMIT_BYTES,
                "page over bytes"
            );
        }
        assert_eq!(total_buttons, providers.len(), "every provider gets a button");
        let first = cards[0].to_string();
        assert!(first.contains("选择模型"), "header: {first}");
        assert!(first.contains("选择 provider"), "intro: {first}");
        assert!(
            first.contains("当前模型") && first.contains("opencode-go/deepseek-v4-flash@low"),
            "current line: {first}"
        );
        let payload = &cards[0]["body"]["elements"]
            .as_array()
            .unwrap()
            .iter()
            .find(|e| e["tag"] == "button")
            .unwrap()["value"];
        assert_eq!(payload["level"].as_str(), Some("provider"));
        assert_eq!(payload["value"].as_str(), Some("provider-0"));
    }

    /// OpenCode's per-provider model page keeps its hand-built shape: the
    /// provider as header, the "选择 model" intro, the back button, and
    /// `provider/model` callback values.
    #[test]
    fn opencode_model_page_shape_unchanged_with_back_button() {
        let key = crate::config::ThreadKey::new("chat_1".into(), "chat_1".into());
        let models: Vec<ModelOption> = (0..400)
            .map(|i| ModelOption {
                id: format!("model-{i}"),
                value: Some(format!("opencode/model-{i}")),
                ..Default::default()
            })
            .collect();
        let mut model_scope = scope("model", "opencode", models);
        model_scope.intro = Some("**选择 model**（下一条消息开始生效）：".into());
        let surface = surface(None, vec![model_scope]);
        let cards = build_model_surface_cards(&key, &surface, true);
        assert!(cards.len() > 1, "huge model list must split: {}", cards.len());
        let first = &cards[0];
        let buttons = first["body"]["elements"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|e| e["tag"] == "button")
            .count();
        // The first card also carries the "back" button, so model buttons are
        // one fewer than the total button count.
        assert!(buttons - 1 <= MAX_PICKER_BUTTONS_PER_CARD);
        let text = first.to_string();
        assert!(text.contains("🎯 opencode"), "header: {text}");
        assert!(text.contains("选择 model"), "intro: {text}");
        assert!(
            text.contains("返回全部 provider"),
            "model picker must offer a way back"
        );
        let payload = &first["body"]["elements"]
            .as_array()
            .unwrap()
            .iter()
            .find(|e| e["tag"] == "button")
            .unwrap()["value"];
        assert_eq!(payload["level"].as_str(), Some("provider"), "back button level");
        assert_eq!(payload["value"].as_str(), Some("__providers__"));
        assert!(text.contains("\"value\":\"opencode/model-0\""), "value: {text}");
        assert!(
            !text.contains("\"content\":\"opencode/model-0\""),
            "the label stays the bare model id: {text}"
        );
        for card in &cards {
            assert!(card.to_string().len() <= FEISHU_CARD_LIMIT_BYTES);
        }
    }

    /// A scope with no options degrades to its note + hand-write hint, never
    /// an empty button group.
    #[test]
    fn empty_scope_renders_note_and_free_text_without_buttons() {
        let key = crate::config::ThreadKey::new("chat_1".into(), "chat_1".into());
        let mut empty = scope("model", "模型（每轮可换）", Vec::new());
        empty.note = Some("_(没有可用模型)_".into());
        empty.free_text = Some("也可以直接发送 `/model codex/<id>`".into());
        let cards = build_model_surface_cards(&key, &surface(None, vec![empty]), false);
        assert_eq!(cards.len(), 1);
        let text = cards[0].to_string();
        assert!(text.contains("没有可用模型"), "note: {text}");
        assert!(text.contains("/model codex/<id>"), "hint: {text}");
        let buttons = cards[0]["body"]["elements"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|e| e["tag"] == "button")
            .count();
        assert_eq!(buttons, 0, "an empty scope has no buttons: {text}");
    }

    /// A client-declared surface with two scopes renders two titled sections
    /// plus each scope's note and hand-write hint, and every button's callback
    /// value is passed through verbatim.
    #[test]
    fn fake_surface_renders_two_sections_and_returns_values() {
        let key = crate::config::ThreadKey::new("chat_1".into(), "chat_1".into());
        let mut catalog = scope(
            "catalog",
            "内置目录",
            vec![ModelOption {
                id: "codex/gpt-5.1-codex".into(),
                label: Some("GPT-5.1 Codex".into()),
                ..Default::default()
            }],
        );
        catalog.note = Some("目录来自 Codex 的内置清单".into());
        catalog.free_text = Some("也可以直接发送 `/model codex/<id>`".into());
        let mut provider = scope(
            "provider",
            "自定义 provider",
            vec![ModelOption {
                id: "custom".into(),
                value: Some("custom/model".into()),
                group: Some("自定义".into()),
                ..Default::default()
            }],
        );
        provider.note = Some("provider 未开放 `/models`，请手写 id".into());
        let cards = build_model_surface_cards(&key, &surface(None, vec![catalog, provider]), false);
        assert_eq!(cards.len(), 1, "a small two-scope surface fits one card");
        let text = cards[0].to_string();
        assert!(text.contains("**内置目录**"), "first section: {text}");
        assert!(text.contains("**自定义 provider**"), "second section: {text}");
        assert!(text.contains("目录来自 Codex 的内置清单"), "note: {text}");
        assert!(text.contains("请手写 id"), "provider note: {text}");
        assert!(text.contains("/model codex/<id>"), "hint: {text}");
        let values: Vec<String> = cards[0]["body"]["elements"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|e| e["tag"] == "button")
            .filter_map(|e| e["value"]["value"].as_str().map(str::to_string))
            .collect();
        assert_eq!(values, ["codex/gpt-5.1-codex", "custom/model"]);
    }

    /// A grouped scope renders a bold heading per group (#908): the built-in
    /// catalog is its own group and never appears under a provider's heading,
    /// and a probed provider's models sit under that provider's group.
    #[test]
    fn grouped_surface_keeps_the_catalog_apart_from_provider_groups() {
        const CATALOG: &str = "内置目录（任何 provider 可试）";
        let key = crate::config::ThreadKey::new("chat_1".into(), "chat_1".into());
        let model = scope(
            "model",
            "模型（每轮可换）",
            vec![
                ModelOption {
                    id: "gpt-5.1-codex".into(),
                    value: Some("codex/gpt-5.1-codex".into()),
                    group: Some(CATALOG.into()),
                    ..Default::default()
                },
                ModelOption {
                    id: "deepseek-flash".into(),
                    value: Some("codex/deepseek-flash".into()),
                    group: Some("DeepSeek（/models）".into()),
                    ..Default::default()
                },
            ],
        );
        let provider = scope(
            "provider",
            "Provider（线程级：新线程 / resume）",
            vec![ModelOption {
                id: "deepseek".into(),
                label: Some("DeepSeek（当前）".into()),
                ..Default::default()
            }],
        );
        let cards = build_model_surface_cards(&key, &surface(None, vec![model, provider]), false);

        // Walk every card and remember the nearest group heading per button.
        let mut seen_catalog = false;
        let mut seen_probed = false;
        for card in &cards {
            let mut group = String::new();
            for element in card["body"]["elements"].as_array().unwrap() {
                if element["tag"] == "markdown" {
                    let content = element["content"].as_str().unwrap();
                    if content.contains(CATALOG) {
                        group = CATALOG.to_string();
                    } else if content.contains("DeepSeek（/models）") {
                        group = "DeepSeek（/models）".to_string();
                    }
                }
                if element["tag"] == "button" {
                    match element["value"]["value"].as_str() {
                        Some("codex/gpt-5.1-codex") => {
                            assert_eq!(
                                group, CATALOG,
                                "the catalog entry must not sit under a provider group"
                            );
                            seen_catalog = true;
                        }
                        Some("codex/deepseek-flash") => {
                            assert_eq!(
                                group, "DeepSeek（/models）",
                                "a probed model sits under its provider's group"
                            );
                            seen_probed = true;
                        }
                        _ => {}
                    }
                }
            }
        }
        assert!(
            seen_catalog && seen_probed,
            "both grouped options rendered: {cards:?}"
        );
    }

    /// A grouped scope larger than one card chunks by page, re-renders a
    /// group's heading when a page starts mid-group, and keeps every card inside
    /// both Feishu budgets.
    #[test]
    fn grouped_scope_pages_repeat_headings_and_stay_within_budget() {
        const GROUPS: [&str; 3] = [
            "内置目录（任何 provider 可试）",
            "DeepSeek（/models）",
            "Profile 预设",
        ];
        let key = crate::config::ThreadKey::new("chat_1".into(), "chat_1".into());
        let mut options = Vec::new();
        for (g, group) in GROUPS.iter().enumerate() {
            for i in 0..40 {
                let id = format!("model-{g}-{i}");
                options.push(ModelOption {
                    id: id.clone(),
                    value: Some(format!("codex/{id}")),
                    group: Some((*group).to_string()),
                    ..Default::default()
                });
            }
        }
        let cards = build_model_surface_cards(
            &key,
            &surface(None, vec![scope("model", "模型（每轮可换）", options)]),
            false,
        );
        assert!(cards.len() > 1, "120 options cannot fit one card");

        for card in &cards {
            assert!(
                card.to_string().len() <= FEISHU_CARD_LIMIT_BYTES,
                "a grouped page exceeded the byte budget"
            );
            let elements = card["body"]["elements"].as_array().unwrap();
            let buttons = elements
                .iter()
                .filter(|element| element["tag"] == "button")
                .count();
            assert!(
                buttons <= MAX_PICKER_BUTTONS_PER_CARD,
                "a grouped page exceeded the component budget: {buttons} buttons"
            );
            // Every button has its group heading on the same page.
            let mut group: Option<String> = None;
            for element in elements {
                if element["tag"] == "markdown" {
                    let content = element["content"].as_str().unwrap();
                    for candidate in GROUPS {
                        if content.contains(candidate) {
                            group = Some(candidate.to_string());
                        }
                    }
                }
                if element["tag"] == "button"
                    && let Some(value) = element["value"]["value"].as_str()
                    && let Some((g, _)) = value
                        .strip_prefix("codex/model-")
                        .and_then(|rest| rest.split_once('-'))
                    && let Ok(index) = g.parse::<usize>()
                {
                    assert_eq!(
                        group.as_deref(),
                        Some(GROUPS[index]),
                        "a page starting mid-group must repeat its heading"
                    );
                }
            }
        }
    }

    /// The `/think` card lists the current model, its declared variants, and a
    /// "默认（清除）" button carrying its OWN `think_clear` action tag — the
    /// value namespace holds only real variants, so a variant literally named
    /// like a clear verb stays selectable (ADR-0020 `--reset` decoupling).
    #[test]
    fn think_card_lists_model_and_variants() {
        let key = crate::config::ThreadKey::new("chat_1".into(), "chat_1".into());
        let card = build_think_card(
            &key,
            "opencode-go",
            "deepseek-v4-flash",
            Some("high"),
            &["low".into(), "high".into()],
        );
        let text = card.to_string();
        assert!(text.contains("思考等级"), "header: {text}");
        assert!(text.contains("opencode-go/deepseek-v4-flash"), "model: {text}");
        assert!(text.contains("当前思考等级"), "current label: {text}");
        assert!(text.contains("默认（清除）"), "default option: {text}");
        assert!(
            text.contains("\"action\":\"think_clear\""),
            "clear action tag: {text}"
        );
        assert!(
            !text.contains("\"value\":\"default\""),
            "clear must not overload a value: {text}"
        );
        assert!(text.contains("\"value\":\"high\""), "variant button: {text}");
        assert!(
            text.contains("\"action\":\"think\""),
            "variant action tag: {text}"
        );
    }

    /// The `/agent` card shows the CURRENT agent (override, or the derived
    /// server default annotated 默认), lists agents as buttons under the `agent`
    /// action, and carries a separate "默认（清除）" button under its OWN
    /// `agent_clear` action — an agent literally named `default` stays a
    /// selectable value (ADR-0020). The empty degrade carries no clear button.
    #[test]
    fn agent_card_lists_current_and_clear_button() {
        let key = crate::config::ThreadKey::new("chat_1".into(), "chat_1".into());
        let agents = vec![
            crate::backend::types::AgentInfo {
                name: "default".into(),
                description: None,
                mode: Some("primary".into()),
                hidden: Some(false),
            },
            crate::backend::types::AgentInfo {
                name: "build".into(),
                description: None,
                mode: Some("primary".into()),
                hidden: Some(true),
            },
            crate::backend::types::AgentInfo {
                name: "explore".into(),
                description: None,
                mode: Some("subagent".into()),
                hidden: Some(false),
            },
        ];
        // No override → the server default `default` is the current agent.
        let card = build_agent_card(&key, &agents, None, Some("default"));
        let text = card.to_string();
        assert!(text.contains("当前 Agent"), "current label: {text}");
        assert!(text.contains("`default`（默认）"), "default current: {text}");
        assert!(
            text.contains("\"action\":\"agent_clear\""),
            "clear action tag: {text}"
        );
        assert!(
            text.contains("\"value\":\"default\""),
            "agent named `default` is selectable: {text}"
        );
        assert!(
            !text.contains("\"value\":\"build\""),
            "hidden agent must not be listed: {text}"
        );
        assert!(
            !text.contains("\"value\":\"explore\""),
            "subagent-mode agent must not be listed: {text}"
        );
        assert!(text.contains("\"action\":\"agent\""), "agent action tag: {text}");

        // Override set → shown without the 默认 annotation.
        let card = build_agent_card(&key, &agents, Some("default"), Some("build"));
        let text = card.to_string();
        assert!(text.contains("`default`"), "override shown: {text}");
        assert!(
            !text.contains("`default`（默认）"),
            "override is not the default annotation: {text}"
        );

        // Empty agents → degrade intro, no clear button.
        let card = build_agent_card(&key, &[], None, None);
        let text = card.to_string();
        assert!(text.contains("没有可用 agent"), "degrade intro: {text}");
        assert!(
            !text.contains("agent_clear"),
            "no clear button when empty: {text}"
        );
    }

    /// A list whose every entry is hidden or subagent-mode degrades exactly
    /// like an empty one: no options, the empty intro, and no clear button.
    #[test]
    fn agent_card_degrades_when_every_agent_is_filtered() {
        let key = crate::config::ThreadKey::new("chat_1".into(), "chat_1".into());
        let agents = vec![
            crate::backend::types::AgentInfo {
                name: "explore".into(),
                description: None,
                mode: Some("subagent".into()),
                hidden: Some(false),
            },
            crate::backend::types::AgentInfo {
                name: "hidden-primary".into(),
                description: None,
                mode: Some("primary".into()),
                hidden: Some(true),
            },
        ];
        let card = build_agent_card(&key, &agents, None, Some("explore"));
        let text = card.to_string();
        assert!(text.contains("没有可用 agent"), "degrade intro: {text}");
        assert!(!text.contains("agent_clear"), "no clear button: {text}");
        assert!(
            !text.contains("\"value\":\"explore\""),
            "no filtered agent leaks into options: {text}"
        );
    }

    /// The `/skill` picker lists EVERY registered skill — a hidden
    /// (`disable-model-invocation`) and a description-less one included — one
    /// row each under the `skill` action. The row label carries the name and,
    /// when present, the description; the callback value is the skill identity,
    /// and the Chat/Topic's `chat_type` rides every button for the tap. Fewer
    /// rows than a page means no search box and no pager (#664).
    #[test]
    fn skill_card_lists_every_entry_with_descriptions_and_routing() {
        let key = crate::config::ThreadKey::new("chat_1".into(), "chat_1".into());
        let skills = [
            skill(
                "implement-spec",
                "Implement Spec",
                Some("Drive a spec to shipped code."),
            ),
            skill("bare", "Bare", None),
            skill(
                "hidden-tool",
                "Hidden Tool",
                Some("Never advertised to the model."),
            ),
        ];
        let card = build_skill_card(&key, &skills, "", 1, "p2p", "msg_src", None);
        let text = card.to_string();
        assert!(text.contains("选择技能"), "header: {text}");
        assert!(text.contains("共 3 个"), "the intro names the total: {text}");
        for entry in &skills {
            assert!(text.contains(&entry.name), "row for {}: {text}", entry.name);
        }
        assert!(
            text.contains("Drive a spec to shipped code."),
            "description shown: {text}"
        );
        assert!(search_form_of(&card).is_none(), "three skills need no search box");
        assert!(pager_of(&card).is_none(), "three skills need no pager");
        let rows = skill_rows(&card);
        assert_eq!(rows.len(), 3, "one row per skill");
        assert_eq!(rows[0]["value"]["action"].as_str(), Some("skill"));
        assert_eq!(rows[0]["value"]["value"].as_str(), Some("implement-spec"));
        assert_eq!(rows[0]["value"]["chat_type"].as_str(), Some("p2p"));
        assert_eq!(
            rows[0]["value"]["reply_message_id"].as_str(),
            Some("msg_src"),
            "the row carries the original user message for the tap's own reply"
        );
        assert!(
            !rows[1]["text"]["content"].as_str().unwrap().contains("Bare —"),
            "a description-less skill shows only its name: {text}"
        );
        assert_eq!(
            rows[2]["value"]["value"].as_str(),
            Some("hidden-tool"),
            "the hidden skill is selectable"
        );
    }

    /// An empty skill list renders the no-skills state (no rows), an `error`
    /// prefix leads the SAME list on an unknown id, and a keyword that matches
    /// nothing shows the no-match copy instead (#664).
    #[test]
    fn skill_card_renders_empty_error_and_no_match_states() {
        let key = crate::config::ThreadKey::new("chat_1".into(), "chat_1".into());
        let empty = build_skill_card(&key, &[], "", 1, "group", "msg_src", None);
        assert!(empty.to_string().contains("没有可用技能"), "no-skills state");
        assert!(skill_rows(&empty).is_empty(), "no rows without skills");

        let no_match = build_skill_card(&key, &[], "zzz", 1, "group", "msg_src", None);
        let text = no_match.to_string();
        assert!(text.contains("无匹配技能"), "no-match state: {text}");
        assert!(
            search_form_of(&no_match).is_some(),
            "an active keyword keeps the box"
        );

        let skills = [skill("implement-spec", "Implement Spec", None)];
        let with_error = build_skill_card(
            &key,
            &skills,
            "",
            1,
            "group",
            "msg_src",
            Some("⚠️ 未找到技能：`nope`"),
        );
        let text = with_error.to_string();
        assert!(text.contains("未找到技能"), "error line: {text}");
        assert!(text.contains("nope"), "error names the id: {text}");
        assert!(text.contains("Implement Spec"), "the list still follows: {text}");
        assert_eq!(skill_rows(&with_error).len(), 1, "the list follows the error");
    }

    /// The one shared description normalizer both the picker row and the text
    /// fallback call (spec #652, ticket #656): a missing, empty or
    /// whitespace-only description is `None`; a multi-line (folded frontmatter)
    /// value is clipped to its first line; surrounding whitespace is trimmed.
    #[test]
    fn skill_description_line_trims_drops_empty_and_takes_the_first_line() {
        let skill = |description: Option<&str>| crate::backend::SkillInfo {
            id: "s".into(),
            name: "S".into(),
            description: description.map(str::to_string),
            content: None,
        };
        assert_eq!(skill_description_line(&skill(None)), None);
        assert_eq!(skill_description_line(&skill(Some(""))), None);
        assert_eq!(skill_description_line(&skill(Some("   \n  "))), None);
        assert_eq!(
            skill_description_line(&skill(Some("  first line\nsecond line  "))),
            Some("first line")
        );
        assert_eq!(
            skill_description_line(&skill(Some("only line"))),
            Some("only line")
        );
    }

    /// An over-long description-less skill name is clipped to the row budget, so
    /// the built card stays under Feishu's byte ceiling — a single row is never
    /// split, so the label itself must be bounded.
    #[test]
    fn skill_card_clips_an_overlong_name_and_stays_within_budget() {
        let key = crate::config::ThreadKey::new("chat_1".into(), "chat_1".into());
        let skills = [crate::backend::SkillInfo {
            id: "long".into(),
            name: "名".repeat(500),
            description: None,
            content: None,
        }];
        let card = build_skill_card(&key, &skills, "", 1, "p2p", "msg_src", None);
        assert!(
            card.to_string().len() <= FEISHU_CARD_LIMIT_BYTES,
            "card over Feishu's byte ceiling: {}",
            card.to_string().len()
        );
        let label = skill_rows(&card)[0]["text"]["content"]
            .as_str()
            .unwrap()
            .to_string();
        assert!(
            label.chars().count() <= SKILL_ROW_LABEL_CHARS + 1,
            "label must be clipped to the row budget: {} chars",
            label.chars().count()
        );
        assert!(label.ends_with('…'), "clipped label keeps its ellipsis: {label}");
    }

    /// A registered skill with a very long CALLBACK VALUE (its exact `id`) must
    /// still produce cards under Feishu's byte ceiling (spec #652, ticket #655):
    /// the page window budgets the button's callback value, not just its label,
    /// so a huge id takes its own page instead of building an oversized card.
    /// The full id stays in the callback so the skill remains selectable.
    #[test]
    fn skill_card_accounts_for_an_overlong_id_and_stays_within_budget() {
        let key = crate::config::ThreadKey::new("chat_1".into(), "chat_1".into());
        let id = "i".repeat(20_000);
        let skills: Vec<crate::backend::SkillInfo> = (0..3)
            .map(|n| crate::backend::SkillInfo {
                id: format!("{n}-{id}"),
                name: format!("skill-{n}"),
                description: None,
                content: None,
            })
            .collect();
        let pages = skill_pages(&skills);
        assert_eq!(pages.len(), 3, "each huge id takes its own page");
        let mut values = Vec::new();
        for (n, _) in pages.iter().enumerate() {
            let card = build_skill_card(&key, &skills, "", n + 1, "p2p", "msg_src", None);
            assert!(
                search_form_of(&card).is_some(),
                "byte-driven pagination still offers the search box"
            );
            assert!(
                card.to_string().len() <= FEISHU_CARD_LIMIT_BYTES,
                "a long callback value pushed a card to {} bytes",
                card.to_string().len()
            );
            values.extend(
                skill_rows(&card)
                    .iter()
                    .map(|row| row["value"]["value"].as_str().unwrap().to_string()),
            );
        }
        for entry in &skills {
            assert!(values.contains(&entry.id), "the exact id must remain selectable");
        }
    }

    /// #664: past a page the picker paginates in ONE card — page 2 windows the
    /// next rows and the pager names the position and the total; a stale page
    /// clamps to the last (ADR-0052) rather than springing back to the first.
    #[test]
    fn skill_card_paginates_and_labels_the_pager() {
        let key = crate::config::ThreadKey::new("chat_1".into(), "chat_1".into());
        let skills = numbered_skills(SKILL_PAGE_ROWS + 5);

        let first = build_skill_card(&key, &skills, "", 1, "p2p", "msg_src", None);
        assert_eq!(skill_rows(&first).len(), SKILL_PAGE_ROWS, "page 1 is full");
        let pager = pager_of(&first).expect("a multi-page list renders the pager");
        assert_eq!(
            pager["columns"][1]["elements"][0]["content"], "第 1/2 页 · 共 25 个",
            "{first}"
        );

        let second = build_skill_card(&key, &skills, "", 2, "p2p", "msg_src", None);
        let rows = skill_rows(&second);
        assert_eq!(rows.len(), 5, "page 2 holds the remainder");
        let want = format!("s{}", SKILL_PAGE_ROWS + 1);
        assert_eq!(rows[0]["value"]["value"].as_str(), Some(want.as_str()));

        let stale = build_skill_card(&key, &skills, "", 99, "p2p", "msg_src", None);
        assert_eq!(
            pager_of(&stale).unwrap()["columns"][1]["elements"][0]["content"],
            "第 2/2 页 · 共 25 个",
            "{stale}"
        );
    }

    /// #664: the search box appears once the list outgrows a page (or a keyword
    /// is active) and routes to the picker; its submit carries the picker's
    /// `chat_type` / `reply_message_id` extras, so a rebuild re-stamps the rows
    /// and a later tap still replies under the original user message.
    #[test]
    fn skill_card_search_form_routes_and_carries_the_picker_extras() {
        let key = crate::config::ThreadKey::new("chat_1".into(), "chat_1".into());
        let small = [skill("a", "A", None)];
        assert!(
            search_form_of(&build_skill_card(&key, &small, "", 1, "p2p", "msg_src", None)).is_none(),
            "a short list needs no search box"
        );

        let long = numbered_skills(SKILL_PAGE_ROWS + 1);
        let card = build_skill_card(&key, &long, "", 1, "p2p", "msg_src", None);
        let form = search_form_of(&card).expect("over a page the search box appears");
        assert_eq!(form["name"], "skill_search");
        let elements = form["elements"].as_array().unwrap();
        let input = elements.iter().find(|e| e["tag"] == "input").unwrap();
        assert_eq!(input["default_value"], "");
        assert_eq!(input["placeholder"]["content"], "🔍 搜索技能名称 / ID / 描述");
        let submit = elements.iter().find(|e| e["tag"] == "button").unwrap();
        assert_eq!(submit["name"], "skillsearch|chat_1|chat_1");
        assert_eq!(submit["value"]["action"], "skill");
        assert_eq!(submit["value"]["op"], "search");
        assert_eq!(submit["value"]["chat_type"], "p2p");
        assert_eq!(submit["value"]["reply_message_id"], "msg_src");

        // An active keyword echoes into the box, keeps it under the cap, and
        // names itself in the header.
        let filtered = filter_skills(&small, "a");
        let card = build_skill_card(&key, &filtered, "a", 1, "p2p", "msg_src", None);
        let form = search_form_of(&card).expect("an active keyword keeps the box");
        assert_eq!(form["name"], "skill_search");
        assert!(
            card.to_string().contains("匹配 `a` 的技能"),
            "the header names the filter: {card}"
        );
    }

    /// #664: the pager buttons carry the same routing payload as the rows —
    /// keyword, target page and the picker's `chat_type` / `reply_message_id`
    /// extras — so a flip rebuilds the same view.
    #[test]
    fn skill_card_pager_carries_the_filter_and_routing() {
        let key = crate::config::ThreadKey::new("chat_1".into(), "chat_1".into());
        let skills = numbered_skills(SKILL_PAGE_ROWS + 5);
        let card = build_skill_card(&key, &skills, "", 2, "p2p", "msg_src", None);
        let pager = pager_of(&card).unwrap();
        let next = &pager["columns"][2]["elements"][0]["value"];
        assert_eq!(next["action"], "skill");
        assert_eq!(next["op"], "page");
        assert_eq!(next["chat_id"], "chat_1");
        assert_eq!(next["thread_id"], "chat_1");
        assert_eq!(next["keyword"], "");
        assert_eq!(next["chat_type"], "p2p");
        assert_eq!(next["reply_message_id"], "msg_src");
    }

    /// #664: the filter matches the skill's `id`, `name` AND `description`,
    /// token-AND, case-insensitively; an empty keyword keeps everything.
    #[test]
    fn filter_skills_matches_id_name_and_description_tokens() {
        let skills = [
            skill(
                "implement-spec",
                "Implement Spec",
                Some("Drive a spec to shipped code."),
            ),
            skill(
                "foreman",
                "Foreman",
                Some("Run a spec's tickets as a serial batch."),
            ),
            skill("bare", "Bare", None),
        ];
        assert_eq!(filter_skills(&skills, "").len(), 3, "empty keyword keeps all");
        let ids = |keyword: &str| -> Vec<String> {
            filter_skills(&skills, keyword)
                .into_iter()
                .map(|s| s.id)
                .collect()
        };
        assert_eq!(ids("foreman"), ["foreman"].map(str::to_string));
        assert_eq!(
            ids("FOREMAN"),
            ["foreman"].map(str::to_string),
            "case-insensitive"
        );
        assert_eq!(
            ids("spec"),
            ["implement-spec", "foreman"].map(str::to_string),
            "an id-less match reaches the name/description"
        );
        assert_eq!(ids("serial batch"), ["foreman"].map(str::to_string), "token-AND");
        assert_eq!(ids("implement shipped"), ["implement-spec"].map(str::to_string));
        assert!(ids("nope").is_empty());
    }

    /// One `SkillInfo` for the skill tests.
    fn skill(id: &str, name: &str, description: Option<&str>) -> crate::backend::SkillInfo {
        crate::backend::SkillInfo {
            id: id.into(),
            name: name.into(),
            description: description.map(str::to_string),
            content: None,
        }
    }

    /// `n` short, description-less skills (`s1`…`sn`).
    fn numbered_skills(n: usize) -> Vec<crate::backend::SkillInfo> {
        (1..=n)
            .map(|i| skill(&format!("s{i}"), &format!("Skill {i}"), None))
            .collect()
    }

    /// The picker's skill ROW buttons (top-level buttons without an `op`; the
    /// search submit carries `op: "search"`, the pager is nested in columns).
    fn skill_rows(card: &serde_json::Value) -> Vec<&serde_json::Value> {
        card["body"]["elements"]
            .as_array()
            .expect("card body elements")
            .iter()
            .filter(|e| e["tag"] == "button" && e["value"].get("op").is_none())
            .collect()
    }

    /// The card's search form, if any.
    fn search_form_of(card: &serde_json::Value) -> Option<&serde_json::Value> {
        card["body"]["elements"]
            .as_array()?
            .iter()
            .find(|e| e["tag"] == "form")
    }

    /// The card's pager (`column_set` containing an `op: "page"` button), if any.
    fn pager_of(card: &serde_json::Value) -> Option<&serde_json::Value> {
        card["body"]["elements"].as_array()?.iter().find(|e| {
            e["tag"] == "column_set"
                && e["columns"]
                    .as_array()
                    .is_some_and(|cols| cols.iter().any(|c| c["elements"][0]["value"]["op"] == "page"))
        })
    }

    fn pinned(json: &str) -> serde_json::Value {
        serde_json::from_str(json).expect("the pinned card JSON parses")
    }

    /// Every button element of one card, in render order.
    fn card_buttons(card: &serde_json::Value) -> Vec<&serde_json::Value> {
        card["body"]["elements"]
            .as_array()
            .map(|elements| elements.iter().filter(|e| e["tag"] == "button").collect())
            .unwrap_or_default()
    }

    /// The OpenCode provider page is pinned byte-for-byte: any drift in the
    /// header, the current-model line, the intro or a button's callback payload
    /// fails, not just the fields one assertion happens to name.
    #[test]
    fn opencode_provider_page_is_pixel_identical() {
        let key = crate::config::ThreadKey::new("chat_1".into(), "chat_1".into());
        let mut provider = provider_scope(
            "provider",
            "选择模型",
            vec![ModelOption::new("opencode"), ModelOption::new("openrouter")],
        );
        provider.intro = Some("**选择 provider**（共 2 个）：".into());
        let surface = surface(
            Some(ModelSelection {
                label: "opencode-go/deepseek-v4-flash@low".into(),
                reference: "opencode-go/deepseek-v4-flash".into(),
            }),
            vec![provider],
        );

        let cards = build_model_surface_cards(&key, &surface, false);

        assert_eq!(cards.len(), 1);
        assert_eq!(
            cards[0],
            pinned(
                r#"{
  "body": {
    "elements": [
      {
        "content": "**当前模型**：`opencode-go/deepseek-v4-flash@low`\n**选择 provider**（共 2 个）：",
        "tag": "markdown"
      },
      {
        "tag": "button",
        "text": { "content": "opencode", "tag": "plain_text" },
        "type": "default",
        "value": {
          "action": "model",
          "chat_id": "chat_1",
          "level": "provider",
          "thread_id": "chat_1",
          "value": "opencode"
        },
        "width": "fill"
      },
      {
        "tag": "button",
        "text": { "content": "openrouter", "tag": "plain_text" },
        "type": "default",
        "value": {
          "action": "model",
          "chat_id": "chat_1",
          "level": "provider",
          "thread_id": "chat_1",
          "value": "openrouter"
        },
        "width": "fill"
      }
    ]
  },
  "config": { "wide_screen_mode": true },
  "header": {
    "template": "blue",
    "title": { "content": "🎯 选择模型", "tag": "plain_text" }
  },
  "schema": "2.0"
}"#
            )
        );
    }

    /// The drilled OpenCode model page is pinned too: the provider header, the
    /// "选择 model" intro, the back button (level `provider`,
    /// `__providers__`) and the bare-label / `provider/model`-value buttons.
    #[test]
    fn opencode_model_page_is_pixel_identical() {
        let key = crate::config::ThreadKey::new("chat_1".into(), "chat_1".into());
        let mut model = scope(
            "model",
            "opencode-go",
            vec![
                ModelOption {
                    id: "deepseek-v4-flash".into(),
                    value: Some("opencode-go/deepseek-v4-flash".into()),
                    ..Default::default()
                },
                ModelOption {
                    id: "deepseek-v4-pro".into(),
                    value: Some("opencode-go/deepseek-v4-pro".into()),
                    ..Default::default()
                },
            ],
        );
        model.intro = Some("**选择 model**（下一条消息开始生效）：".into());
        let surface = surface(None, vec![model]);

        let cards = build_model_surface_cards(&key, &surface, true);

        assert_eq!(cards.len(), 1);
        assert_eq!(
            cards[0],
            pinned(
                r#"{
  "body": {
    "elements": [
      {
        "content": "**选择 model**（下一条消息开始生效）：",
        "tag": "markdown"
      },
      {
        "tag": "button",
        "text": { "content": "← 返回全部 provider", "tag": "plain_text" },
        "type": "default",
        "value": {
          "action": "model",
          "chat_id": "chat_1",
          "level": "provider",
          "thread_id": "chat_1",
          "value": "__providers__"
        },
        "width": "fill"
      },
      {
        "tag": "button",
        "text": { "content": "deepseek-v4-flash", "tag": "plain_text" },
        "type": "default",
        "value": {
          "action": "model",
          "chat_id": "chat_1",
          "level": "model",
          "thread_id": "chat_1",
          "value": "opencode-go/deepseek-v4-flash"
        },
        "width": "fill"
      },
      {
        "tag": "button",
        "text": { "content": "deepseek-v4-pro", "tag": "plain_text" },
        "type": "default",
        "value": {
          "action": "model",
          "chat_id": "chat_1",
          "level": "model",
          "thread_id": "chat_1",
          "value": "opencode-go/deepseek-v4-pro"
        },
        "width": "fill"
      }
    ]
  },
  "config": { "wide_screen_mode": true },
  "header": {
    "template": "blue",
    "title": { "content": "🎯 opencode-go", "tag": "plain_text" }
  },
  "schema": "2.0"
}"#
            )
        );
    }

    /// The empty-scope variants are pinned as well: the provider page keeps the
    /// current-model line above its "no models" note, and the drilled page
    /// keeps its "this provider has no models" note plus the back button.
    #[test]
    fn opencode_empty_pages_are_pixel_identical() {
        let key = crate::config::ThreadKey::new("chat_1".into(), "chat_1".into());

        let mut empty = provider_scope("provider", "选择模型", Vec::new());
        empty.intro = Some("_(没有可用模型)_".into());
        let empty_root = surface(
            Some(ModelSelection {
                label: "opencode-go/deepseek-v4-flash@low".into(),
                reference: "opencode-go/deepseek-v4-flash".into(),
            }),
            vec![empty],
        );
        assert_eq!(
            build_model_surface_cards(&key, &empty_root, false),
            vec![pinned(
                r#"{
  "body": {
    "elements": [
      {
        "content": "**当前模型**：`opencode-go/deepseek-v4-flash@low`\n_(没有可用模型)_",
        "tag": "markdown"
      }
    ]
  },
  "config": { "wide_screen_mode": true },
  "header": {
    "template": "blue",
    "title": { "content": "🎯 选择模型", "tag": "plain_text" }
  },
  "schema": "2.0"
}"#
            )]
        );

        let mut empty_model = scope("model", "opencode-go", Vec::new());
        empty_model.intro = Some("_(该 provider 没有可用模型)_".into());
        assert_eq!(
            build_model_surface_cards(&key, &surface(None, vec![empty_model]), true),
            vec![pinned(
                r#"{
  "body": {
    "elements": [
      {
        "content": "_(该 provider 没有可用模型)_",
        "tag": "markdown"
      },
      {
        "tag": "button",
        "text": { "content": "← 返回全部 provider", "tag": "plain_text" },
        "type": "default",
        "value": {
          "action": "model",
          "chat_id": "chat_1",
          "level": "provider",
          "thread_id": "chat_1",
          "value": "__providers__"
        },
        "width": "fill"
      }
    ]
  },
  "config": { "wide_screen_mode": true },
  "header": {
    "template": "blue",
    "title": { "content": "🎯 opencode-go", "tag": "plain_text" }
  },
  "schema": "2.0"
}"#
            )]
        );
    }

    /// A two-scope surface whose combined card would pass the byte ceiling but
    /// exceed Feishu's component budget falls back to per-scope cards: every
    /// emitted card stays within both budgets and no option is dropped.
    #[test]
    fn combined_surface_respects_the_component_budget() {
        let key = crate::config::ThreadKey::new("chat_1".into(), "chat_1".into());
        let options = |prefix: &str| -> Vec<ModelOption> {
            (0..45)
                .map(|i| ModelOption::new(format!("{prefix}{i}")))
                .collect()
        };
        let surface = surface(
            None,
            vec![
                scope("catalog", "内置目录", options("c")),
                scope("provider", "自定义 provider", options("p")),
            ],
        );

        // The scenario is bytes-fit but components-over: the single-card path
        // cannot be what produced the result.
        let combined = combined_surface_card(&key, None, &surface.scopes, false);
        assert!(
            combined.to_string().len() <= MAX_CARD_JSON_CHARS,
            "scenario needs the byte budget to pass: {}",
            combined.to_string().len()
        );
        assert!(
            combined["body"]["elements"].as_array().unwrap().len() > MAX_PICKER_BUTTONS_PER_CARD,
            "scenario needs the component budget to fail"
        );

        let cards = build_model_surface_cards(&key, &surface, false);
        assert!(
            cards.len() > 1,
            "an over-budget combined page must fall back per scope"
        );
        let mut total_buttons = 0;
        for card in &cards {
            let elements = card["body"]["elements"].as_array().unwrap();
            total_buttons += elements.iter().filter(|e| e["tag"] == "button").count();
            assert!(
                elements.len() <= MAX_PICKER_BUTTONS_PER_CARD,
                "page over the component budget: {}",
                elements.len()
            );
            assert!(
                card.to_string().len() <= FEISHU_CARD_LIMIT_BYTES,
                "page over bytes"
            );
        }
        assert_eq!(total_buttons, 90, "every option keeps a button");
    }

    /// Issue #910: the callback tag follows the surface's declared pick kind,
    /// not the scope id — a provider *selection* is not OpenCode's drill-down,
    /// even though both scopes are named `provider`.
    #[test]
    fn a_provider_selection_is_not_a_provider_drill_down() {
        let key = crate::config::ThreadKey::new("chat_1".into(), "chat_1".into());
        let mut pick = scope(
            "provider",
            "Provider（线程级：新线程 / resume）",
            vec![ModelOption::new("deepseek")],
        );
        pick.pick = crate::backend::types::PickKind::Provider;
        let cards = build_model_surface_cards(&key, &surface(None, vec![pick]), false);
        let button = card_buttons(&cards[0]);
        assert_eq!(button.len(), 1);
        assert_eq!(button[0]["value"]["level"], PICKER_LEVEL_PROVIDER_PICK);
        assert_eq!(button[0]["value"]["value"], "deepseek");

        // The drill-down tag is unchanged, so OpenCode's navigation still works.
        let drill = provider_scope("provider", "选择模型", vec![ModelOption::new("opencode")]);
        let cards = build_model_surface_cards(&key, &surface(None, vec![drill]), false);
        assert_eq!(
            card_buttons(&cards[0])[0]["value"]["level"],
            PICKER_LEVEL_PROVIDER
        );
    }

    /// Issue #910: a profile preset's paired provider rides the callback, and
    /// only there — a model option without a pairing emits no such field, so
    /// every other card's bytes stay identical.
    #[test]
    fn a_paired_provider_rides_the_model_button() {
        let key = crate::config::ThreadKey::new("chat_1".into(), "chat_1".into());
        let model = scope(
            "model",
            "模型（每轮可换）",
            vec![
                ModelOption {
                    id: "deepseek-flash".into(),
                    value: Some("codex/deepseek-flash".into()),
                    provider: Some("deepseek".into()),
                    ..Default::default()
                },
                ModelOption {
                    id: "gpt-5.1".into(),
                    value: Some("codex/gpt-5.1".into()),
                    ..Default::default()
                },
            ],
        );
        let cards = build_model_surface_cards(&key, &surface(None, vec![model]), false);
        let buttons = card_buttons(&cards[0]);
        assert_eq!(buttons[0]["value"]["provider"], "deepseek");
        assert!(
            buttons[1]["value"].get("provider").is_none(),
            "an unpaired model pick carries no provider field"
        );
    }

    /// Issue #910 acceptance 7: the confirmation copy names the **pair**, says
    /// every attached client (and a second cola sharing `CODEX_HOME`) sees the
    /// change, and its buttons carry the pair + target session the bridge
    /// re-checks.
    #[test]
    fn the_provider_confirmation_names_the_pair_peers_and_target() {
        let key = crate::config::ThreadKey::new("chat_1".into(), "chat_1".into());
        let card = build_provider_confirm_card(&key, "ses_1", "deepseek", "deepseek-v4-pro");
        let text = card.to_string();
        for expected in [
            "deepseek/deepseek-v4-pro",
            "其他客户端",
            "TUI",
            "VS Code",
            "CODEX_HOME",
            "cola",
        ] {
            assert!(text.contains(expected), "{expected} missing: {text}");
        }
        let buttons = card_buttons(&card);
        let confirm = buttons
            .iter()
            .find(|button| button["value"]["action"] == "model_provider_confirm")
            .expect("the confirm button");
        assert_eq!(confirm["value"]["value"], "deepseek-v4-pro");
        assert_eq!(confirm["value"]["provider"], "deepseek");
        assert_eq!(confirm["value"]["session_id"], "ses_1");
        assert!(
            buttons
                .iter()
                .any(|button| button["value"]["action"] == "model_provider_cancel"),
            "the cancel button keeps its own action tag (a mechanism, not a value)"
        );
        assert!(
            card.to_string().len() <= MAX_CARD_JSON_CHARS,
            "the confirmation stays inside the card budget"
        );
    }

    /// Issue #910, item 8: the ack is a "switching" card, and the outcome
    /// (success, unchanged, refusal or a failed local save) is a card of its
    /// own — never the ack claiming success.
    #[test]
    fn the_switch_outcome_cards_name_their_state() {
        let switching = build_provider_switching_card("deepseek", "deepseek-v4-pro");
        assert!(switching.to_string().contains("正在"), "{switching}");

        let switched = build_provider_switched_card("deepseek", "deepseek-v4-pro");
        assert!(switched.to_string().contains("已切换"), "{switched}");
        assert!(switched.to_string().contains("thread/read"), "{switched}");

        let already = build_provider_already_card("deepseek", "deepseek-v4-pro");
        assert!(already.to_string().contains("已经在"), "{already}");

        let failed = build_provider_switch_error_card(
            "deepseek",
            "deepseek-v4-pro",
            "thread/resume 之后这条线程仍是 `openai/gpt-5.1-codex`",
        );
        assert!(failed.to_string().contains("切换失败"), "{failed}");
        assert!(failed.to_string().contains("openai/gpt-5.1-codex"), "{failed}");

        let save = build_provider_save_blocked_card("deepseek/deepseek-v4-pro", "disk full");
        let text = save.to_string();
        assert!(text.contains("无法记录"), "{text}");
        assert!(text.contains("disk full"), "{text}");
        assert!(
            text.contains("没有开始") && text.contains("原来的 provider/model"),
            "a failed save never claims the switch happened: {text}"
        );
    }
}
