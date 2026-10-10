//! The loaded-skill fold (spec #652, ticket #655): one skill a `/skill <id>`
//! dispatch loaded into the prompt, rendered as a folded
//! `🧩 已加载技能：<name>` panel whose body is the skill's own markdown from the
//! skill-list read's `content`. Its ONLY caller is the dedicated loaded-skill
//! card that replies under the user's `/skill` message — the live Turn card,
//! the Session Snapshot's 「最近对话」 tail and the External Message preview
//! render no skill fold (the acceptance reversal recorded in ADR-0077).

use crate::backend::SkillInfo;
use serde_json::json;

use super::picker::SKILL_ROW_LABEL_CHARS;
use super::sanitize::CardMarkdown;
use super::shell::{card_shell, collapsible_panel};
use super::tool_render::{TOOL_OUTPUT_MAX_CHARS, parse_skill_envelope};
use super::truncate_md;

/// The body characters one card's loaded-skill folds may spend in total
/// (spec #652, ticket #655). Feishu rejects a card whose serialized JSON
/// exceeds 30 KB (AGENTS.md pitfall #13), and one `/skill` dispatch can load
/// several skills whose bodies are CJK (3 bytes per char), so the per-fold
/// [`TOOL_OUTPUT_MAX_CHARS`] cap alone cannot bound the one-shot loaded-skill
/// card, which has no size splitter. 4,000 chars is at most 12 KB of CJK body
/// text, leaving room for the rest of the card.
pub(crate) const SKILL_FOLDS_TOTAL_CHARS: usize = 4_000;

/// Feishu's hard platform ceiling for a card's element/component count: a card
/// past it is rejected (`ErrCode 11310`). The loaded-skill card has no
/// splitter and must stay under this ceiling.
const FEISHU_MAX_COMPONENTS: usize = 200;

/// Components one loaded-skill fold spends: its `collapsible_panel` plus the
/// single markdown element nested inside it ([`collapsible_panel`]).
const FOLD_COMPONENTS: usize = 2;

/// The most individual skill folds one CARD renders before the overflow
/// collapses into one summary fold (spec #652, ticket #655). Each fold spends
/// [`FOLD_COMPONENTS`] components, so this spends at most HALF of Feishu's
/// [`FEISHU_MAX_COMPONENTS`] ceiling — the other half stays for the card's own
/// elements and the summary fold. It is a platform-limit guard for
/// pathological input (a `/skill a` token pasted a hundred times), NOT a count
/// cap on ordinary messages: a message with a handful of skills is far below
/// it and still renders one fold per skill, with its body.
pub(crate) const SKILL_FOLD_MAX: usize = FEISHU_MAX_COMPONENTS / (2 * FOLD_COMPONENTS);

/// The body an attached skill with no instructions renders (the list route
/// carried no `content`, or one that unwraps to nothing). A collapsible panel
/// needs a non-empty markdown element, so the titled fold still opens rather
/// than vanishing or failing Feishu's content check.
const EMPTY_BODY: &str = "（无说明）";

/// The body a fold renders once the card's shared character budget is spent:
/// its title still names the skill, without an oversized body. A fold within
/// the count budget keeps its title; only the body is dropped.
const OMITTED_BODY: &str = "（内容过长，已省略）";

/// The body the overflow summary fold shows. Constant and bounded: the title
/// names the count, and the body only says why the rest are absent.
const SUMMARY_BODY: &str = "（技能过多，其余已省略）";

/// One card's loaded-skill folds: one folded `🧩 已加载技能：<name>` panel per
/// distinct skill, in order, under the card's SHARED budget — the body
/// characters its folds may spend AND the individual folds it may render. Every
/// fold on the card draws from that one budget, so a dispatch loading several
/// large skills cannot push the one-shot loaded-skill card past Feishu's total
/// limit (AGENTS.md #13), and a pathological skill count cannot push it past the
/// component ceiling ([`SKILL_FOLD_MAX`]). `prefix` names each panel for the
/// reader's fold state (`{prefix}{index}`), like every other panel's
/// `element_id`; `md` is the CARD's markdown state, so the folds share its one
/// table budget and fenced fallback with the card's other elements. The overflow
/// past [`SKILL_FOLD_MAX`] is collapsed into ONE bounded summary fold naming the
/// count; a fold past the character budget keeps its title with an omission body.
pub(crate) fn loaded_skill_folds(
    skills: &[SkillInfo],
    prefix: &str,
    md: &mut CardMarkdown,
) -> Vec<serde_json::Value> {
    let take = skills.len().min(SKILL_FOLD_MAX);
    let mut chars_left = SKILL_FOLDS_TOTAL_CHARS;
    let mut panels: Vec<serde_json::Value> = skills[..take]
        .iter()
        .enumerate()
        .map(|(i, skill)| {
            let allowed = unwrapped_body(skill)
                .chars()
                .count()
                .min(TOOL_OUTPUT_MAX_CHARS)
                .min(chars_left);
            chars_left -= allowed;
            build_fold(skill, allowed, &format!("{prefix}{i}"), md)
        })
        .collect();
    let hidden = skills.len() - take;
    if hidden > 0 {
        panels.push(summary_fold(hidden, &format!("{prefix}summary"), md));
    }
    panels
}

/// The dedicated loaded-skill card (spec #652, ticket #655, acceptance
/// reversal): the small card that replies under a resolved `/skill <id>`
/// message, carrying one `🧩 已加载技能：<name>` fold per distinct loaded skill.
/// The fold body is the skill's own markdown from the skill-list read's
/// `content` (no `<skill_content>` envelope to strip — the unwrap is harmless
/// on raw content). One card-wide budget bounds the folds, so a dispatch loading
/// several large skills cannot build a card Feishu rejects. `error`, when set,
/// leads the card — a mixed dispatch that resolved SOME ids but not others names
/// the unknown ones here, so nothing is dropped silently.
pub(crate) fn build_loaded_skill_card(skills: &[SkillInfo], error: Option<&str>) -> serde_json::Value {
    let mut md = CardMarkdown::new();
    let mut elements = Vec::new();
    if let Some(error) = error {
        elements.push(json!({ "tag": "markdown", "content": md.element(error) }));
    }
    elements.extend(loaded_skill_folds(skills, "loaded_skill_", &mut md));
    card_shell("🧩 已加载技能", "green", elements)
}

/// One fold's title: `🧩 已加载技能：<name>`. The name is clipped to the same
/// budget the picker's row label uses ([`SKILL_ROW_LABEL_CHARS`]) — the fold's
/// body budget never bounds its title, so an unbounded name could alone push the
/// card past Feishu's 30 KB limit (AGENTS.md #13).
fn fold_title(name: &str) -> String {
    format!("🧩 已加载技能：{}", truncate_md(name, SKILL_ROW_LABEL_CHARS))
}

/// The skill's own markdown, with any server `<skill_content>` envelope and
/// sampled `<skill_files>` list stripped, trimmed — the body a fold shows. The
/// list route serves raw markdown, so the unwrap is usually a no-op; keeping it
/// keeps the fold identical for a payload that does carry the envelope.
fn unwrapped_body(skill: &SkillInfo) -> String {
    skill
        .content
        .as_deref()
        .map(|text| parse_skill_envelope(text).unwrap_or_else(|| text.to_string()))
        .unwrap_or_default()
        .trim()
        .to_string()
}

/// The plain-text body one skill contributes to the degraded text fallback
/// (spec #652, ticket #655): the same unwrapped markdown a fold shows, clipped
/// to `allowed` characters — or empty when the skill carries no content or the
/// shared budget is spent. Shared so the fallback and the folds unwrap the same
/// way, and the fallback keeps the fold body the acceptance promises instead of
/// degrading to a title-only list.
pub(crate) fn fallback_body(skill: &SkillInfo, allowed: usize) -> String {
    let body = unwrapped_body(skill);
    if body.is_empty() || allowed == 0 {
        String::new()
    } else {
        truncate_md(&body, allowed)
    }
}

/// One fold's panel: the title `🧩 已加载技能：<name>`, the body the skill's
/// markdown (sanitized, capped at `allowed` characters) with any server
/// `<skill_content>` envelope and sampled `<skill_files>` list stripped — the
/// same unwrapping the `skill` tool panel uses (spec #652, ticket #655). An
/// empty body renders its titled fold with a marker instead of an empty panel;
/// a fold whose body is past the budget renders the omission marker.
///
/// `md` is the CARD's markdown state, threaded in rather than a fresh
/// one-shot: the fold draws from the card's single table budget, so a 6th table
/// anywhere on the card — this fold included — renders as code, exactly like
/// every other element ([`CardMarkdown`]'s own warning). The raw body is clipped
/// BEFORE `md.element` because that call may wrap an over-budget table in a
/// fence — clipping the fenced result could cut the closing fence (the `skill`
/// tool panel clips first for the same reason).
fn build_fold(
    skill: &SkillInfo,
    allowed: usize,
    element_id: &str,
    md: &mut CardMarkdown,
) -> serde_json::Value {
    let body = unwrapped_body(skill);
    let body = if body.is_empty() {
        EMPTY_BODY.to_string()
    } else if allowed == 0 {
        OMITTED_BODY.to_string()
    } else {
        truncate_md(&body, allowed)
    };
    let body = md.element(&body);
    collapsible_panel(&fold_title(&skill.name), &body, Some(element_id))
}

/// The overflow summary fold: one bounded panel naming how many skills a
/// pathological message attached past [`SKILL_FOLD_MAX`], so every skill is
/// accounted for without building a card past Feishu's component ceiling.
fn summary_fold(hidden: usize, element_id: &str, md: &mut CardMarkdown) -> serde_json::Value {
    collapsible_panel(
        &summary_title(hidden),
        &md.element(SUMMARY_BODY),
        Some(element_id),
    )
}

/// The summary fold's title, so its render and the tests name the same shape.
fn summary_title(hidden: usize) -> String {
    format!("🧩 已加载技能（等 {hidden} 个）")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One skill as the list route serves it: a name and its own markdown body.
    fn skill(name: &str, content: Option<&str>) -> SkillInfo {
        SkillInfo {
            id: name.into(),
            name: name.into(),
            description: None,
            content: content.map(str::to_string),
        }
    }

    /// The server's prepared skill body is a `<skill_content>` envelope around
    /// the skill's own markdown: the fold shows the markdown alone, dropping
    /// the wrapper and the sampled `<skill_files>` inventory (spec #652,
    /// ticket #655). The list route usually serves raw markdown, so the same
    /// unwrap is a harmless no-op there.
    #[test]
    fn a_loaded_skill_folds_the_unwrapped_body_under_its_title() {
        let enveloped = skill(
            "implement-spec",
            Some(
                "<skill_content name=\"implement-spec\">\n# Skill: implement-spec\n\nDo the thing.\n\
                 <skill_files>\n<file>/root/skills/a.md</file>\n</skill_files>\n</skill_content>",
            ),
        );
        let panels = loaded_skill_folds(&[enveloped], "skill_", &mut CardMarkdown::new());

        assert_eq!(panels.len(), 1);
        let panel = &panels[0];
        assert_eq!(panel["tag"], "collapsible_panel");
        assert_eq!(panel["expanded"], false, "the fold starts collapsed");
        assert_eq!(panel["element_id"], "skill_0");
        assert_eq!(
            panel["header"]["title"]["content"],
            "🧩 已加载技能：implement-spec"
        );
        let body = panel["elements"][0]["content"].as_str().unwrap();
        assert!(body.contains("Do the thing."), "{body}");
        assert!(!body.contains("<skill_content"), "envelope stripped: {body}");
        assert!(!body.contains("skill_files"), "file list stripped: {body}");
        assert!(
            !body.contains("/root/skills/a.md"),
            "sampled file dropped: {body}"
        );

        // Raw content (the list route's own shape) passes through unchanged.
        let raw = skill("foreman", Some("# Foreman\n\nRun the batch."));
        let panels = loaded_skill_folds(&[raw], "skill_", &mut CardMarkdown::new());
        let body = panels[0]["elements"][0]["content"].as_str().unwrap();
        assert!(body.contains("Run the batch."), "{body}");
    }

    /// A skill the list read carried no `content` for (or one whose body
    /// unwraps to nothing) still renders its titled fold — never an empty panel
    /// Feishu would reject.
    #[test]
    fn an_empty_instruction_body_still_renders_a_titled_fold() {
        for content in [None, Some("")] {
            let panels = loaded_skill_folds(&[skill("foreman", content)], "skill_", &mut CardMarkdown::new());
            assert_eq!(panels[0]["header"]["title"]["content"], "🧩 已加载技能：foreman");
            assert_eq!(panels[0]["elements"][0]["content"], "（无说明）");
        }
    }

    /// The element ids keep the caller's prefix, so the fold state is stable
    /// across a re-render.
    #[test]
    fn the_fold_ids_carry_the_callers_prefix() {
        let panels = loaded_skill_folds(
            &[skill("a", Some("x")), skill("b", Some("y"))],
            "loaded_skill_",
            &mut CardMarkdown::new(),
        );
        assert_eq!(panels[0]["element_id"], "loaded_skill_0");
        assert_eq!(panels[1]["element_id"], "loaded_skill_1");
    }

    /// The card's shared budget bounds the folds even when a dispatch loads
    /// several large CJK skills whose per-fold caps would sum far past Feishu's
    /// total card limit (AGENTS.md #13). Every skill still gets its OWN titled
    /// fold below the component cap — the character budget is the size guard —
    /// and a fold past the budget renders its title with the omission body
    /// rather than an oversized body.
    #[test]
    fn the_folds_share_one_body_budget_across_the_card() {
        // 8,000 CJK chars per skill — ten of them are 80,000 raw chars.
        let huge = "很长的技能说明。".repeat(1_000);
        let skills: Vec<SkillInfo> = (0..10)
            .map(|i| skill(&format!("skill-{i}"), Some(&huge)))
            .collect();

        let panels = loaded_skill_folds(&skills, "f_", &mut CardMarkdown::new());
        assert_eq!(panels.len(), 10, "one fold per skill, below the component cap");
        for (i, panel) in panels.iter().enumerate() {
            let expected = format!("🧩 已加载技能：skill-{i}");
            assert_eq!(
                panel["header"]["title"]["content"].as_str(),
                Some(expected.as_str())
            );
        }

        let body_chars: usize = panels
            .iter()
            .map(|panel| panel["elements"][0]["content"].as_str().unwrap().chars().count())
            .sum();
        assert!(
            body_chars < 5_000,
            "the folds spent {body_chars} chars, not the raw 80,000"
        );

        // The first folds carry a (capped) body; once the 4,000-char budget is
        // spent the rest keep their titles with the omission body.
        assert_ne!(panels[0]["elements"][0]["content"], OMITTED_BODY);
        assert_eq!(panels[2]["elements"][0]["content"], OMITTED_BODY);
    }

    /// A pathological skill count — a `/skill a` token pasted 150 times — must
    /// not build a card Feishu rejects on component count: the first
    /// [`SKILL_FOLD_MAX`] skills keep their own fold and the rest collapse into
    /// ONE bounded summary fold naming how many were hidden (spec #652, ticket
    /// #655).
    #[test]
    fn a_pathological_skill_count_collapses_into_one_summary_fold() {
        let skills: Vec<SkillInfo> = (0..150)
            .map(|i| skill(&format!("skill-{i}"), Some("body")))
            .collect();

        let panels = loaded_skill_folds(&skills, "f_", &mut CardMarkdown::new());
        assert_eq!(panels.len(), SKILL_FOLD_MAX + 1, "the cap plus one summary");
        assert!(
            panels[..SKILL_FOLD_MAX]
                .iter()
                .all(|panel| panel["tag"] == "collapsible_panel"),
            "every fold under the cap is its own panel"
        );
        let summary = &panels[SKILL_FOLD_MAX];
        assert_eq!(
            summary["header"]["title"]["content"],
            "🧩 已加载技能（等 100 个）"
        );
        assert_eq!(summary["element_id"], "f_summary");

        // A fold is a panel plus its nested markdown element: 2 components.
        let components: usize = panels
            .iter()
            .map(|panel| 1 + panel["elements"].as_array().unwrap().len())
            .sum();
        assert_eq!(components, 2 * (SKILL_FOLD_MAX + 1));
        assert!(
            components <= FEISHU_MAX_COMPONENTS,
            "the folds spent {components} components, past Feishu's ceiling"
        );

        // An ordinary message is far below the cap: one fold per skill, no
        // summary.
        let few = loaded_skill_folds(&skills[..5], "f_", &mut CardMarkdown::new());
        assert_eq!(few.len(), 5);
        assert!(few.iter().all(|panel| panel["tag"] == "collapsible_panel"));
    }

    /// The folds SHARE the card's table budget (spec #652, ticket #655): two
    /// skills carrying three tables each are six tables, one past Feishu's
    /// per-card `MAX_CARD_TABLES` (a 6th fails with `230099/11310 card table
    /// number over limit`). Threading the card's [`CardMarkdown`] through the
    /// render — not a fresh one-shot per fold — makes the sixth render as code.
    #[test]
    fn the_folds_share_one_table_budget_across_the_card() {
        let table = "| a | b |\n|---|---|\n| 1 | 2 |";
        let body = format!("{table}\n\n{table}\n\n{table}");
        let skills = [skill("one", Some(&body)), skill("two", Some(&body))];

        let mut md = CardMarkdown::new();
        let panels = loaded_skill_folds(&skills, "f_", &mut md);
        let text: String = panels
            .iter()
            .map(|panel| panel["elements"][0]["content"].as_str().unwrap().to_string())
            .collect::<Vec<_>>()
            .join("\n");

        // All six tables are present, but exactly ONE is wrapped in a fence:
        // the card-wide budget let five render natively.
        assert_eq!(text.matches("|---|---|").count(), 6, "six tables present: {text}");
        assert_eq!(
            text.matches("```").count(),
            2,
            "exactly the sixth table is fenced: {text}"
        );
        assert_eq!(
            text.matches("```").count() / 2,
            6 - crate::feishu::card::sanitize::MAX_CARD_TABLES,
            "one table past the budget is fenced"
        );
    }

    /// The card's fenced fallback (`230099`: Feishu refused the card once)
    /// applies to a fold's body too, exactly like every other element.
    #[test]
    fn the_fenced_fallback_fences_a_folds_body() {
        let mut md = CardMarkdown::fenced();
        let panels = loaded_skill_folds(&[skill("a", Some("Do the thing."))], "f_", &mut md);
        let body = panels[0]["elements"][0]["content"].as_str().unwrap();
        assert!(body.starts_with("```"), "the fold body is fenced: {body}");
        assert!(body.ends_with("```"), "the fence closes: {body}");
        assert!(body.contains("Do the thing."), "{body}");
    }

    /// The dedicated card (spec #652, ticket #655): one fold per distinct
    /// loaded skill, titled, with the body from the list `content`. An empty
    /// body still renders a titled fold; a leading error line, when given, is
    /// the card's first element.
    #[test]
    fn the_loaded_skill_card_lists_one_titled_fold_per_skill() {
        let card = build_loaded_skill_card(
            &[
                skill("implement-spec", Some("# Implement Spec\n\nDrive it.")),
                skill("foreman", None),
            ],
            None,
        );
        assert_eq!(card["header"]["title"]["content"], "🧩 已加载技能");
        let panels: Vec<&serde_json::Value> = card["body"]["elements"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|e| e["tag"] == "collapsible_panel")
            .collect();
        assert_eq!(panels.len(), 2, "{card}");
        assert_eq!(
            panels[0]["header"]["title"]["content"],
            "🧩 已加载技能：implement-spec"
        );
        assert!(
            panels[0]["elements"][0]["content"]
                .as_str()
                .unwrap()
                .contains("Drive it."),
            "{card}"
        );
        assert_eq!(panels[1]["header"]["title"]["content"], "🧩 已加载技能：foreman");
        assert_eq!(panels[1]["elements"][0]["content"], "（无说明）");

        // A mixed dispatch's error line leads the card, before the folds.
        let with_error = build_loaded_skill_card(&[skill("foreman", None)], Some("⚠️ 未找到技能：`nope`"));
        let first = &with_error["body"]["elements"][0];
        assert_eq!(first["tag"], "markdown");
        assert!(
            first["content"].as_str().unwrap().contains("nope"),
            "{with_error}"
        );
        assert_eq!(
            with_error["body"]["elements"][1]["header"]["title"]["content"],
            "🧩 已加载技能：foreman"
        );
    }

    /// A very long skill name is clipped in the fold title to the picker's
    /// row-label budget — the body budget never bounds the title (spec #652,
    /// ticket #655).
    #[test]
    fn an_overlong_skill_name_is_clipped_in_the_fold_title() {
        let card = build_loaded_skill_card(&[skill(&"名".repeat(500), Some("body"))], None);
        let title = card["body"]["elements"][0]["header"]["title"]["content"]
            .as_str()
            .unwrap();
        assert!(
            title.chars().count() <= SKILL_ROW_LABEL_CHARS + "🧩 已加载技能：".chars().count() + 1,
            "title clipped: {} chars",
            title.chars().count()
        );
        assert!(title.ends_with('…'), "{title}");
    }
}
