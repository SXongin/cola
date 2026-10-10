//! The loaded-skill fold (spec #652, ticket #655): one skill a Turn loaded into
//! its user message, rendered as a folded `🧩 已加载技能：<name>` panel whose body
//! is the skill's own instructions. One shared renderer feeds all three sites —
//! the live Turn card, the Session Snapshot's 「最近对话」 tail and the External
//! Message preview — so the `🧩` vocabulary keeps one shape and one size budget.

use crate::backend::MessageSkill;

use super::sanitize::sanitize_markdown;
use super::shell::collapsible_panel;
use super::tool_render::{TOOL_OUTPUT_MAX_CHARS, parse_skill_envelope};
use super::truncate_md;

/// The body characters one card's loaded-skill folds may spend in total
/// (spec #652, ticket #655). Feishu rejects a card whose serialized JSON
/// exceeds 30 KB (AGENTS.md pitfall #13), and a message can attach several
/// skills whose bodies are CJK (3 bytes per char), so the per-fold
/// [`TOOL_OUTPUT_MAX_CHARS`] cap alone cannot bound a one-shot snapshot or
/// notification card, which has no size splitter. 4,000 chars is at most
/// 12 KB of CJK body text, leaving room for the rest of the card.
pub(crate) const SKILL_FOLDS_TOTAL_CHARS: usize = 4_000;

/// The most loaded-skill folds one card renders: a message's skill list is
/// user-controlled and Feishu caps a card at 200 components, so the count is
/// bounded too. Additional skills are named in one summary fold rather than
/// dropped silently.
pub(crate) const MAX_SKILL_FOLDS: usize = 8;

/// The body an attached skill with no instructions renders (its payload named
/// the skill but carried no prepared body, or a body that unwraps to nothing).
/// A collapsible panel needs a non-empty markdown element, so the titled fold
/// still opens rather than vanishing or failing Feishu's content check.
const EMPTY_BODY: &str = "（无说明）";

/// The body a fold renders once the card's shared character budget is spent:
/// its title still names the skill, without an oversized body.
const OMITTED_BODY: &str = "（内容过长，已省略）";

/// The body characters the one summary fold that names skills past
/// [`MAX_SKILL_FOLDS`] may spend.
const SUMMARY_BODY_CHARS: usize = 200;

/// A card's loaded-skill fold budget: how many more folds it may render and how
/// many more body characters they may spend. Every fold on one card draws from
/// one budget, so a message attaching several large skills cannot push a
/// one-shot snapshot or notification card past Feishu's total limit. The live
/// Turn card's splitter estimates the same walk through
/// [`loaded_skill_folds_estimate`], so the two cannot drift.
pub(crate) struct SkillFolds {
    chars_left: usize,
    folds_left: usize,
}

impl SkillFolds {
    /// A fresh card-wide budget.
    pub(crate) fn new() -> Self {
        Self {
            chars_left: SKILL_FOLDS_TOTAL_CHARS,
            folds_left: MAX_SKILL_FOLDS,
        }
    }

    /// Render `skills` as folds under this card's remaining budget, in order.
    /// `prefix` names each panel for the reader's fold state (`{prefix}{index}`),
    /// like every other panel's `element_id`; the overflow summary uses
    /// `{prefix}more`. Call once per message on a card, the same budget each
    /// time, so several messages' skills share one allowance.
    pub(crate) fn render(&mut self, skills: &[MessageSkill], prefix: &str) -> Vec<serde_json::Value> {
        let mut panels = Vec::new();
        let mut overflow_from = None;
        for (i, skill) in skills.iter().enumerate() {
            let Some(allowed) = self.take(skill) else {
                overflow_from = Some(i);
                break;
            };
            panels.push(build_fold(skill, allowed, &format!("{prefix}{i}")));
        }
        if let Some(from) = overflow_from {
            // The overflow is named, not dropped: one bounded fold lists the rest.
            panels.push(build_summary(
                &summary_names(&skills[from..]),
                skills.len() - from,
                &format!("{prefix}more"),
            ));
        }
        panels
    }

    /// Spend one fold's budget and return the body characters it may use (the
    /// per-fold cap intersected with what is left). `None` once the count cap
    /// is spent, so the caller summarizes the remaining skills in one fold.
    fn take(&mut self, skill: &MessageSkill) -> Option<usize> {
        if self.folds_left == 0 {
            return None;
        }
        self.folds_left -= 1;
        let allowed = unwrapped_body(skill)
            .chars()
            .count()
            .min(TOOL_OUTPUT_MAX_CHARS)
            .min(self.chars_left);
        self.chars_left -= allowed;
        Some(allowed)
    }
}

/// [`SkillFolds::render`] for a card whose folds all belong to one call — the
/// live Turn card's own skills.
pub(crate) fn loaded_skill_folds(skills: &[MessageSkill], prefix: &str) -> Vec<serde_json::Value> {
    SkillFolds::new().render(skills, prefix)
}

/// The skill's own instructions with the server's `<skill_content>` envelope and
/// sampled `<skill_files>` list stripped, trimmed — the body a fold shows.
fn unwrapped_body(skill: &MessageSkill) -> String {
    skill
        .instructions
        .as_deref()
        .map(|text| parse_skill_envelope(text).unwrap_or_else(|| text.to_string()))
        .unwrap_or_default()
        .trim()
        .to_string()
}

/// The serialized size (bytes) and component count a fresh card-wide budget
/// will spend rendering `skills`' folds: the splitter's reserve, walking the
/// same budget as [`loaded_skill_folds`]. The byte figure is a CJK upper bound
/// (3 bytes per spent character) plus the panel overhead, so the reserve never
/// under-charges its card.
pub(crate) fn loaded_skill_folds_estimate(skills: &[MessageSkill]) -> (usize, usize) {
    let mut budget = SkillFolds::new();
    let mut comps = 0usize;
    let mut size = 0usize;
    let mut overflow_from = None;
    for (i, skill) in skills.iter().enumerate() {
        let Some(allowed) = budget.take(skill) else {
            overflow_from = Some(i);
            break;
        };
        comps += 1;
        size += 400 + skill.name.len() + allowed * 3;
    }
    if let Some(from) = overflow_from {
        comps += 1;
        size += 400 + SUMMARY_BODY_CHARS.min(summary_names(&skills[from..]).chars().count()) * 3;
    }
    (size, comps)
}

/// One fold's panel: the title `🧩 已加载技能：<name>`, the body the skill's
/// instructions (sanitized, capped at `allowed` characters) with the server's
/// `<skill_content>` envelope and sampled `<skill_files>` list stripped — the
/// same unwrapping the `skill` tool panel uses (spec #652, ticket #655). An
/// empty body renders its titled fold with a marker instead of an empty panel;
/// a fold whose budget is spent renders the omission marker.
fn build_fold(skill: &MessageSkill, allowed: usize, element_id: &str) -> serde_json::Value {
    let body = unwrapped_body(skill);
    let body = if body.is_empty() {
        EMPTY_BODY.to_string()
    } else if allowed == 0 {
        OMITTED_BODY.to_string()
    } else {
        truncate_md(&sanitize_markdown(&body), allowed)
    };
    collapsible_panel(&format!("🧩 已加载技能：{}", skill.name), &body, Some(element_id))
}

/// The `、`-joined names the overflow summary folds in.
fn summary_names(skills: &[MessageSkill]) -> String {
    skills
        .iter()
        .map(|skill| skill.name.as_str())
        .collect::<Vec<_>>()
        .join("、")
}

/// The one summary fold naming the skills past the count cap.
fn build_summary(names: &str, count: usize, element_id: &str) -> serde_json::Value {
    collapsible_panel(
        &format!("🧩 已加载技能（等 {count} 个）"),
        &truncate_md(names, SUMMARY_BODY_CHARS),
        Some(element_id),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One prepared skill body: a `<skill_content>` envelope around the skill's
    /// own markdown.
    fn prepared(name: &str, body: &str) -> MessageSkill {
        MessageSkill {
            id: name.into(),
            name: name.into(),
            instructions: Some(format!(
                "<skill_content name=\"{name}\">\n{body}\n\
                 <skill_files>\n<file>/root/skills/a.md</file>\n</skill_files>\n</skill_content>"
            )),
        }
    }

    /// The server's prepared skill body is a `<skill_content>` envelope around
    /// the skill's own markdown: the fold shows the markdown alone, dropping
    /// the wrapper and the sampled `<skill_files>` inventory (spec #652,
    /// ticket #655).
    #[test]
    fn a_loaded_skill_folds_the_unwrapped_body_under_its_title() {
        let skill = prepared("implement-spec", "# Skill: implement-spec\n\nDo the thing.");
        let panels = loaded_skill_folds(&[skill], "skill_");

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
    }

    /// A skill the payload attached by identity alone (no prepared body), or
    /// one whose body unwraps to nothing, still renders its titled fold — never
    /// an empty panel Feishu would reject.
    #[test]
    fn an_empty_instruction_body_still_renders_a_titled_fold() {
        for instructions in [None, Some(String::new())] {
            let skill = MessageSkill {
                id: "foreman".into(),
                name: "foreman".into(),
                instructions,
            };
            let panels = loaded_skill_folds(&[skill], "skill_");
            assert_eq!(panels[0]["header"]["title"]["content"], "🧩 已加载技能：foreman");
            assert_eq!(panels[0]["elements"][0]["content"], "（无说明）");
        }
    }

    /// The element ids keep the caller's prefix, so each site's fold state is
    /// stable across a re-render.
    #[test]
    fn the_fold_ids_carry_the_callers_prefix() {
        let panels = loaded_skill_folds(&[prepared("a", "x"), prepared("b", "y")], "snap_0_skill_");
        assert_eq!(panels[0]["element_id"], "snap_0_skill_0");
        assert_eq!(panels[1]["element_id"], "snap_0_skill_1");
    }

    /// The card's shared budget bounds the folds even when a message attaches
    /// several large CJK skills whose per-fold caps would sum far past Feishu's
    /// total card limit (AGENTS.md #13): later folds are truncated and finally
    /// omitted, and the over-count overflow is named in one summary fold rather
    /// than dropped.
    #[test]
    fn the_folds_share_one_body_budget_across_the_card() {
        // 8,000 CJK chars per skill — ten of them are 80,000 raw chars.
        let huge = "很长的技能说明。".repeat(1_000);
        let skills: Vec<MessageSkill> = (0..10).map(|i| prepared(&format!("skill-{i}"), &huge)).collect();

        let panels = loaded_skill_folds(&skills, "f_");
        assert_eq!(
            panels.len(),
            MAX_SKILL_FOLDS + 1,
            "eight folds plus the overflow summary"
        );

        let body_chars: usize = panels
            .iter()
            .map(|panel| panel["elements"][0]["content"].as_str().unwrap().chars().count())
            .sum();
        assert!(
            body_chars < 5_000,
            "the folds spent {body_chars} chars, not the raw 80,000"
        );

        // The later folds are titled but omit their bodies once the budget is
        // spent; the summary names the two skills past the count cap.
        assert_eq!(panels[2]["elements"][0]["content"], OMITTED_BODY);
        assert!(
            panels[MAX_SKILL_FOLDS]["header"]["title"]["content"]
                .as_str()
                .is_some_and(|title| title.contains("等 2 个")),
            "{:?}",
            panels[MAX_SKILL_FOLDS]
        );
    }
}
