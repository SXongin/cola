use serde_json::json;

use super::MAX_CARD_JSON_CHARS;
use super::shell::card_shell;
use super::truncate_md;
use super::with_callback_field;

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
    picker_card(header, intro, thread_key, action, None, false, None, options, &[])
}

/// The `/model` picker-card back button's value: clicking it returns from a
/// provider's model page to the full provider list. A named constant (not a
/// bare string literal) so the handler's comparison cannot typo-silently break
/// navigation.
pub(crate) const PICKER_BACK_TO_PROVIDERS: &str = "__providers__";

/// The two levels of the `/model` picker card (ADR-0012 issue 05): a
/// `Provider` button on the provider-list page (its `value` is a provider id,
/// or [`PICKER_BACK_TO_PROVIDERS`] to go back); a `Model` button records the
/// per-session override. Serialized as the callback's `level` field.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PickerLevel {
    Provider,
    Model,
}

impl PickerLevel {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            PickerLevel::Provider => "provider",
            PickerLevel::Model => "model",
        }
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
    level: Option<PickerLevel>,
    back: bool,
    clear: Option<(&str, &str)>,
    options: &[(String, String)],
    callback_fields: &[(&str, &str)],
) -> serde_json::Value {
    let mut elements: Vec<serde_json::Value> = vec![json!({
        "tag": "markdown",
        "content": crate::feishu::card::sanitize::sanitize_markdown(intro)
    })];
    if back {
        elements.push(json!({
            "tag": "button",
            "text": { "tag": "plain_text", "content": "← 返回全部 provider" },
            "type": "default",
            "width": "fill",
            "value": with_callback_field(json!({
                "action": action,
                "chat_id": thread_key.chat_id,
                "thread_id": thread_key.thread_id,
                "level": PickerLevel::Provider.as_str(),
                "value": PICKER_BACK_TO_PROVIDERS,
            }), callback_fields),
        }));
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
    for (label, value) in options {
        let mut payload = json!({
            "action": action,
            "chat_id": thread_key.chat_id,
            "thread_id": thread_key.thread_id,
            "value": value,
        });
        if let Some(l) = level {
            payload["level"] = json!(l.as_str());
        }
        elements.push(json!({
            "tag": "button",
            "text": { "tag": "plain_text", "content": label },
            "type": "default",
            "width": "fill",
            "value": with_callback_field(payload, callback_fields),
        }));
    }
    card_shell(header, "blue", elements)
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
    agents: &[crate::opencode::types::AgentInfo],
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
        &options,
        &[],
    )
}

/// The `/model` picker, step 1: one card page per set of `provider` buttons.
/// Falls back to an empty intro when no providers are listed (the backend is
/// unreachable).
///
/// A shared OpenCode server advertises providers across every model source it
/// knows (gateways included) — hundreds of providers and thousands of models,
/// which in a single button-per-model card blows past Feishu's 30KB / component
/// ceilings and the send fails silently (the `/model` command "does nothing").
/// The flow is therefore two-step: pick a provider here, then one of its
/// models ([`build_model_picker_cards`]); every card is chunked to stay under
/// the card budgets, and any model stays selectable via the
/// `/model <provider/model>` text form. `current` is the model the next turn
/// will run (session override → configured default → server-recorded) — shown
/// so the user can see what a pick would replace.
pub fn build_model_provider_cards(
    thread_key: &crate::config::ThreadKey,
    providers: &[crate::opencode::types::ProviderModels],
    current: Option<&str>,
) -> Vec<serde_json::Value> {
    let options: Vec<(String, String)> = providers
        .iter()
        .map(|p| (p.provider.clone(), p.provider.clone()))
        .collect();
    if options.is_empty() {
        return vec![picker_card(
            "🎯 选择模型",
            &format!("{}_(没有可用模型)_", current_model_line(current)),
            thread_key,
            "model",
            Some(PickerLevel::Provider),
            false,
            None,
            &[],
            &[],
        )];
    }
    chunk_picker_cards(
        "🎯 选择模型",
        &format!(
            "{}**选择 provider**（共 {} 个）：",
            current_model_line(current),
            options.len()
        ),
        thread_key,
        "model",
        Some(PickerLevel::Provider),
        false,
        None,
        &options,
        &[],
    )
}

/// The leading "current model" line shared by the `/model` picker cards.
/// `current` is the caller's display label (`provider/model@variant`); empty
/// when there is no session or no rung of the effective-model ladder resolves
/// — the card then shows only its "选择 …" intro, exactly as before.
fn current_model_line(current: Option<&str>) -> String {
    current
        .map(|c| format!("**当前模型**：`{c}`\n"))
        .unwrap_or_default()
}

/// The `/model` picker, step 2: one card page per set of `provider/model`
/// buttons for a single chosen provider. Carries a leading "← 返回全部
/// provider" button so the user can back out of a wrong provider.
pub fn build_model_picker_cards(
    thread_key: &crate::config::ThreadKey,
    provider: &str,
    models: &[crate::opencode::types::ModelOption],
) -> Vec<serde_json::Value> {
    // The provider is already chosen (step 1), so the button LABEL shows just
    // the model name — a `provider/model` label truncates in Feishu's button
    // width. The callback VALUE keeps the full `provider/model` the handler
    // records as the override.
    let options: Vec<(String, String)> = models
        .iter()
        .map(|m| (m.id.clone(), format!("{provider}/{}", m.id)))
        .collect();
    if options.is_empty() {
        return vec![picker_card(
            &format!("🎯 {provider}"),
            "_(该 provider 没有可用模型)_",
            thread_key,
            "model",
            Some(PickerLevel::Model),
            true,
            None,
            &[],
            &[],
        )];
    }
    chunk_picker_cards(
        &format!("🎯 {provider}"),
        "**选择 model**（下一条消息开始生效）：",
        thread_key,
        "model",
        Some(PickerLevel::Model),
        true,
        None,
        &options,
        &[],
    )
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
    level: Option<PickerLevel>,
    back: bool,
    clear: Option<(&str, &str)>,
    options: &[(String, String)],
    callback_fields: &[(&str, &str)],
) -> Vec<serde_json::Value> {
    let mut pages: Vec<&[(String, String)]> = Vec::new();
    let mut start = 0usize;
    // One button's estimated serialized bytes: ~200 of structure (tags, the
    // routing callback fields) + the label's UTF-8 (shown twice in the button
    // JSON) + the callback VALUE, which for a `/skill` row is the skill's exact
    // id — an unbudgeted id would let the card exceed Feishu's limit (spec #652,
    // ticket #655).
    let button_bytes = |option: &(String, String)| 200 + option.0.len() * 2 + option.1.len();
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
        &options,
        &[],
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
    use crate::feishu::card::FEISHU_CARD_LIMIT_BYTES;
    /// A provider list of any size must never build a card over Feishu's
    /// ceilings: the `/model` picker chunks into pages under the byte budget.
    #[test]
    fn provider_cards_chunk_without_exceeding_feishu_limits() {
        let key = crate::config::ThreadKey::new("chat_1".into(), "chat_1".into());
        // A shared server advertising hundreds of providers.
        let providers: Vec<crate::opencode::types::ProviderModels> = (0..212)
            .map(|p| crate::opencode::types::ProviderModels {
                provider: format!("provider-{p}"),
                models: vec![crate::opencode::types::ModelOption {
                    id: "m".into(),
                    variants: Vec::new(),
                }],
            })
            .collect();
        let cards = build_model_provider_cards(&key, &providers, None);
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
    }

    /// The model picker for one provider carries a back button and honors the
    /// card budgets even for a provider with hundreds of models.
    #[test]
    fn model_picker_chunks_and_has_back_button() {
        let key = crate::config::ThreadKey::new("chat_1".into(), "chat_1".into());
        let models: Vec<crate::opencode::types::ModelOption> = (0..400)
            .map(|i| crate::opencode::types::ModelOption {
                id: format!("model-{i}"),
                variants: Vec::new(),
            })
            .collect();
        let cards = build_model_picker_cards(&key, "opencode", &models);
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
        assert!(
            first.to_string().contains("返回全部 provider"),
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
        assert!(first.to_string().contains("opencode/model-0"));
        for card in &cards {
            assert!(card.to_string().len() <= FEISHU_CARD_LIMIT_BYTES);
        }
    }

    /// An empty provider list degrades to a single "no models" card, not zero;
    /// a resolvable current model still renders its line on top.
    #[test]
    fn provider_cards_degrade_to_empty_intro() {
        let key = crate::config::ThreadKey::new("chat_1".into(), "chat_1".into());
        let cards = build_model_provider_cards(&key, &[], Some("opencode-go/deepseek-v4-flash@low"));
        assert_eq!(cards.len(), 1);
        let text = cards[0].to_string();
        assert!(text.contains("没有可用模型"), "degrade intro: {text}");
        assert!(text.contains("当前模型"), "current label: {text}");
        assert!(
            text.contains("opencode-go/deepseek-v4-flash@low"),
            "current model: {text}"
        );
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
            crate::opencode::types::AgentInfo {
                name: "default".into(),
                description: None,
                mode: Some("primary".into()),
                hidden: Some(false),
            },
            crate::opencode::types::AgentInfo {
                name: "build".into(),
                description: None,
                mode: Some("primary".into()),
                hidden: Some(true),
            },
            crate::opencode::types::AgentInfo {
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
            crate::opencode::types::AgentInfo {
                name: "explore".into(),
                description: None,
                mode: Some("subagent".into()),
                hidden: Some(false),
            },
            crate::opencode::types::AgentInfo {
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
}
