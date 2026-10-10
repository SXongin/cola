use serde_json::json;

use super::MAX_CARD_JSON_CHARS;
use super::shell::card_shell;
use super::truncate_md;

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

/// Add the extra routing pairs to a button's callback payload (spec #652,
/// ticket #656): the `/skill` picker carries the conversation's `chat_type`
/// alongside `chat_id`/`thread_id` (so a tap reconstructs the exact
/// [`ConversationKind`](crate::config::ConversationKind) the picker was sent in)
/// AND the original user message id the picker replied under, so a tap's own
/// reply (the loaded-skill card) lands under that message rather than the picker
/// card. Every other picker passes an empty slice.
fn with_callback_field(mut payload: serde_json::Value, extra: &[(&str, &str)]) -> serde_json::Value {
    for (key, value) in extra {
        payload[*key] = serde_json::Value::String((*value).to_string());
    }
    payload
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
/// [level], value}` plus every `extra` pair. An optional leading clear button
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
    extra: &[(&str, &str)],
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
            }), extra),
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
            }), extra),
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
            "value": with_callback_field(payload, extra),
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
    extra: &[(&str, &str)],
) -> Vec<serde_json::Value> {
    let mut pages: Vec<&[(String, String)]> = Vec::new();
    let mut start = 0usize;
    while start < options.len() {
        // Estimate JSON bytes like the streaming splitter (a button is ~200
        // bytes + the label's UTF-8); bound the button COUNT by the picker
        // budget, not the streaming 150-component constant — buttons cost ~2
        // elements each against Feishu's 200-element ceiling.
        let mut bytes = intro.len() + 64;
        let mut end = start;
        while end < options.len()
            && end - start < MAX_PICKER_BUTTONS_PER_CARD
            && bytes + 200 + options[end].0.len() * 2 <= MAX_CARD_JSON_CHARS
        {
            bytes += 200 + options[end].0.len() * 2;
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
            extra,
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

/// The `/skill` picker (spec #652, ticket #656): one button per registered
/// skill — its `name`, plus its `description` when it declares one — so a user
/// who does not know an id can pick one. The callback value is the skill's
/// generation identity (`id`); a tap re-enters the message pipeline as
/// `/skill <id>`. `chat_type` rides each button so the tap reconstructs the
/// [`ConversationKind`](crate::config::ConversationKind) the picker was sent in
/// (the same routing payload every card button carries, next to `chat_id` /
/// `thread_id`), and `message_id` — the original user message the picker
/// replied under — rides it too, so the tap's own reply (the loaded-skill card)
/// lands under THAT message, exactly like a typed dispatch, rather than under
/// the picker card. `error`, when set, leads the intro so the SAME card answers
/// both a bare `/skill` and an unknown id. An empty list opens the no-skills
/// state.
pub(crate) fn build_skill_cards(
    thread_key: &crate::config::ThreadKey,
    skills: &[crate::backend::SkillInfo],
    chat_type: &str,
    message_id: &str,
    error: Option<&str>,
) -> Vec<serde_json::Value> {
    let extra = [("chat_type", chat_type), ("reply_message_id", message_id)];
    let mut intro = error.map(|e| format!("{e}\n")).unwrap_or_default();
    if skills.is_empty() {
        intro.push_str("_(没有可用技能)_");
        return vec![picker_card(
            "🧩 选择技能",
            &intro,
            thread_key,
            "skill",
            None,
            false,
            None,
            &[],
            &extra,
        )];
    }
    intro.push_str("**选择技能**（点击后以 `/skill <id>` 发送）：");
    let options: Vec<(String, String)> = skills
        .iter()
        .map(|skill| {
            let label = match skill_description_line(skill) {
                Some(description) => {
                    // A button is one line: the description is already clipped
                    // to its first line, then the whole label to the row budget,
                    // so a newline never reaches Feishu's `plain_text`.
                    truncate_md(
                        &format!("{} — {}", skill.name, description),
                        SKILL_ROW_LABEL_CHARS,
                    )
                }
                // Clip the bare name too: an unbounded label could alone exceed
                // the card budget, and `chunk_picker_cards` cannot split a
                // single oversized option. Every skill row is bounded, so that
                // fallback is unreachable here.
                None => truncate_md(&skill.name, SKILL_ROW_LABEL_CHARS),
            };
            (label, skill.id.clone())
        })
        .collect();
    chunk_picker_cards(
        "🧩 选择技能",
        &intro,
        thread_key,
        "skill",
        None,
        false,
        None,
        &options,
        &extra,
    )
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
    /// and the conversation's `chat_type` rides every button for the tap.
    #[test]
    fn skill_cards_list_every_entry_with_descriptions_and_routing() {
        let key = crate::config::ThreadKey::new("chat_1".into(), "chat_1".into());
        let skills = [
            crate::backend::SkillInfo {
                id: "implement-spec".into(),
                name: "Implement Spec".into(),
                description: Some("Drive a spec to shipped code.".into()),
                content: None,
            },
            crate::backend::SkillInfo {
                id: "bare".into(),
                name: "Bare".into(),
                description: None,
                content: None,
            },
            crate::backend::SkillInfo {
                id: "hidden-tool".into(),
                name: "Hidden Tool".into(),
                description: Some("Never advertised to the model.".into()),
                content: None,
            },
        ];
        let cards = build_skill_cards(&key, &skills, "p2p", "msg_src", None);
        assert_eq!(cards.len(), 1, "three skills fit one card");
        let text = cards[0].to_string();
        assert!(text.contains("选择技能"), "header: {text}");
        for skill in &skills {
            assert!(text.contains(&skill.name), "row for {}: {text}", skill.name);
        }
        assert!(
            text.contains("Drive a spec to shipped code."),
            "description shown: {text}"
        );
        let buttons = cards[0]["body"]["elements"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|e| e["tag"] == "button")
            .collect::<Vec<_>>();
        assert_eq!(buttons.len(), 3, "one row per skill");
        assert_eq!(buttons[0]["value"]["action"].as_str(), Some("skill"));
        assert_eq!(buttons[0]["value"]["value"].as_str(), Some("implement-spec"));
        assert_eq!(buttons[0]["value"]["chat_type"].as_str(), Some("p2p"));
        assert_eq!(
            buttons[0]["value"]["reply_message_id"].as_str(),
            Some("msg_src"),
            "the row carries the original user message for the tap's own reply"
        );
        assert!(
            !buttons[1]["text"]["content"].as_str().unwrap().contains("Bare —"),
            "a description-less skill shows only its name: {text}"
        );
        assert_eq!(
            buttons[2]["value"]["value"].as_str(),
            Some("hidden-tool"),
            "the hidden skill is selectable"
        );
    }

    /// An empty skill list renders the no-skills state (one card, no buttons),
    /// and an `error` prefix leads the SAME list on an unknown id.
    #[test]
    fn skill_cards_render_empty_and_error_states() {
        let key = crate::config::ThreadKey::new("chat_1".into(), "chat_1".into());
        let empty = build_skill_cards(&key, &[], "group", "msg_src", None);
        assert_eq!(empty.len(), 1);
        assert!(empty[0].to_string().contains("没有可用技能"), "no-skills state");

        let skills = [crate::backend::SkillInfo {
            id: "implement-spec".into(),
            name: "Implement Spec".into(),
            description: None,
            content: None,
        }];
        let with_error = build_skill_cards(&key, &skills, "group", "msg_src", Some("⚠️ 未找到技能：`nope`"));
        let text = with_error[0].to_string();
        assert!(text.contains("未找到技能"), "error line: {text}");
        assert!(text.contains("nope"), "error names the id: {text}");
        assert!(text.contains("Implement Spec"), "the list still follows: {text}");
        assert_eq!(
            with_error[0]["body"]["elements"]
                .as_array()
                .unwrap()
                .iter()
                .filter(|e| e["tag"] == "button")
                .count(),
            1,
            "the list follows the error"
        );
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
    /// the built card stays under Feishu's byte ceiling — `chunk_picker_cards`
    /// cannot split one oversized option, so the label itself must be bounded.
    #[test]
    fn skill_card_clips_an_overlong_name_and_stays_within_budget() {
        let key = crate::config::ThreadKey::new("chat_1".into(), "chat_1".into());
        let skills = [crate::backend::SkillInfo {
            id: "long".into(),
            name: "名".repeat(500),
            description: None,
            content: None,
        }];
        let cards = build_skill_cards(&key, &skills, "p2p", "msg_src", None);
        assert_eq!(cards.len(), 1, "one bounded row");
        assert!(
            cards[0].to_string().len() <= FEISHU_CARD_LIMIT_BYTES,
            "card over Feishu's byte ceiling: {}",
            cards[0].to_string().len()
        );
        let label = cards[0]["body"]["elements"]
            .as_array()
            .unwrap()
            .iter()
            .find(|e| e["tag"] == "button")
            .unwrap()["text"]["content"]
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
}
