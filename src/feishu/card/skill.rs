//! The loaded-skill fold (spec #652, ticket #655): one skill a Turn loaded into
//! its user message, rendered as a folded `🧩 已加载技能：<name>` panel whose body
//! is the skill's own instructions. One shared renderer feeds all three sites —
//! the live Turn card, the Session Snapshot's 「最近对话」 tail and the External
//! Message preview — so the `🧩` vocabulary keeps one shape and one size budget.

use crate::backend::MessageSkill;

use super::sanitize::CardMarkdown;
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

/// Feishu's hard platform ceiling for a card's element/component count: a card
/// past it is rejected (`ErrCode 11310`). The live card's own splitter budget
/// ([`super::MAX_CARD_COMPONENTS`], 150) is lower, but a one-shot Session
/// Snapshot or External Message card has no splitter and must still stay under
/// this ceiling.
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

/// The body an attached skill with no instructions renders (its payload named
/// the skill but carried no prepared body, or a body that unwraps to nothing).
/// A collapsible panel needs a non-empty markdown element, so the titled fold
/// still opens rather than vanishing or failing Feishu's content check.
const EMPTY_BODY: &str = "（无说明）";

/// The body a fold renders once the card's shared character budget is spent:
/// its title still names the skill, without an oversized body. A fold within
/// the count budget keeps its title; only the body is dropped.
const OMITTED_BODY: &str = "（内容过长，已省略）";

/// The body the overflow summary fold shows. Constant and bounded: the title
/// names the count, and the body only says why the rest are absent.
const SUMMARY_BODY: &str = "（技能过多，其余已省略）";

/// A card's loaded-skill fold budget: the body characters its folds may still
/// spend AND the individual folds it may still render. Every fold on one card
/// draws from one budget, so a message attaching several large skills cannot
/// push a one-shot snapshot or notification card past Feishu's total limit
/// (AGENTS.md #13), and a pathological skill count cannot push it past the
/// component ceiling ([`SKILL_FOLD_MAX`]). The live Turn card's splitter
/// reserves the same walk through [`loaded_skill_folds_estimate`], so the
/// render and the estimate cannot drift.
pub(crate) struct SkillFolds {
    chars_left: usize,
    /// Individual folds this card may still render; once spent, further skills
    /// are counted in `hidden` and covered by the one summary fold.
    folds_left: usize,
    /// Skills past [`SKILL_FOLD_MAX`] this card has seen — the count its summary
    /// fold names. Accumulates across [`Self::render`] calls, so a card renders
    /// ONE summary however many messages contributed.
    hidden: usize,
}

/// One fold's plan under the card's budget: which skill, and how many body
/// characters its fold may spend (`0` once the budget is spent, so the fold
/// renders its title with the omission body).
struct FoldPlan<'a> {
    skill: &'a MessageSkill,
    allowed: usize,
}

impl SkillFolds {
    /// A fresh card-wide budget.
    pub(crate) fn new() -> Self {
        Self {
            chars_left: SKILL_FOLDS_TOTAL_CHARS,
            folds_left: SKILL_FOLD_MAX,
            hidden: 0,
        }
    }

    /// Plan this call's folds, spending the card's remaining budget: at most
    /// [`Self::folds_left`] individual folds (in order), with every skill past
    /// that counted in `hidden` for the summary. The render and the splitter's
    /// estimate both consume this ONE traversal, so which folds carry a body
    /// and which omit it cannot diverge.
    fn plan<'a>(&mut self, skills: &'a [MessageSkill]) -> Vec<FoldPlan<'a>> {
        let take = skills.len().min(self.folds_left);
        let plans = skills
            .iter()
            .take(take)
            .map(|skill| {
                let allowed = unwrapped_body(skill)
                    .chars()
                    .count()
                    .min(TOOL_OUTPUT_MAX_CHARS)
                    .min(self.chars_left);
                self.chars_left -= allowed;
                FoldPlan { skill, allowed }
            })
            .collect();
        self.folds_left -= take;
        self.hidden += skills.len() - take;
        plans
    }

    /// Render `skills` as folds under this card's remaining budget, in order.
    /// `prefix` names each panel for the reader's fold state (`{prefix}{index}`),
    /// like every other panel's `element_id`. `md` is the CARD's markdown state:
    /// every fold draws from its one table budget and honors its fenced
    /// fallback, so a 6th table anywhere on the card — a fold included —
    /// renders as code. Call once per message on a card, the same budget each
    /// time, so several messages' skills share one allowance. The overflow past
    /// [`SKILL_FOLD_MAX`] is NOT rendered here — call [`Self::summary`] once
    /// after the last message.
    pub(crate) fn render(
        &mut self,
        skills: &[MessageSkill],
        prefix: &str,
        md: &mut CardMarkdown,
    ) -> Vec<serde_json::Value> {
        self.plan(skills)
            .into_iter()
            .enumerate()
            .map(|(i, fold)| build_fold(fold.skill, fold.allowed, &format!("{prefix}{i}"), md))
            .collect()
    }

    /// The single overflow fold this card owes, if any skills were hidden past
    /// [`SKILL_FOLD_MAX`]: one bounded `🧩 已加载技能（等 N 个）` panel naming how
    /// many were collapsed. `None` when every skill got its own fold. Call ONCE
    /// per card, after the last [`Self::render`] — a card renders one summary
    /// however many messages contributed.
    pub(crate) fn summary(&mut self, element_id: &str, md: &mut CardMarkdown) -> Option<serde_json::Value> {
        (self.hidden > 0).then(|| summary_fold(self.hidden, element_id, md))
    }
}

/// [`SkillFolds::render`] followed by [`SkillFolds::summary`] for a card whose
/// folds all belong to one call — the live Turn card's own skills, and the
/// External Message preview. `md` is that card's markdown state, so the folds
/// share its table budget and fenced fallback with the card's other elements.
pub(crate) fn loaded_skill_folds(
    skills: &[MessageSkill],
    prefix: &str,
    md: &mut CardMarkdown,
) -> Vec<serde_json::Value> {
    let mut folds = SkillFolds::new();
    let mut panels = folds.render(skills, prefix, md);
    if let Some(summary) = folds.summary(&format!("{prefix}summary"), md) {
        panels.push(summary);
    }
    panels
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
/// will spend rendering `skills`' folds: the splitter's reserve, consuming the
/// same [`SkillFolds::plan`] walk as [`loaded_skill_folds`]. Each fold — the
/// summary included — is [`FOLD_COMPONENTS`] components, never one, so the
/// reserve cannot under-charge its card. The byte figure is a CJK upper bound
/// (3 bytes per spent character) plus the panel overhead, so the reserve never
/// under-charges its card.
pub(crate) fn loaded_skill_folds_estimate(skills: &[MessageSkill]) -> (usize, usize) {
    let mut budget = SkillFolds::new();
    let plans = budget.plan(skills);
    let summary = budget.hidden > 0;
    let comps = (plans.len() + usize::from(summary)) * FOLD_COMPONENTS;
    let size = plans
        .iter()
        .map(|fold| 400 + fold.skill.name.len() + fold.allowed * 3)
        .sum::<usize>()
        + if summary {
            summary_fold_size(budget.hidden)
        } else {
            0
        };
    (size, comps)
}

/// One fold's panel: the title `🧩 已加载技能：<name>`, the body the skill's
/// instructions (sanitized, capped at `allowed` characters) with the server's
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
    skill: &MessageSkill,
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
    collapsible_panel(&format!("🧩 已加载技能：{}", skill.name), &body, Some(element_id))
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

/// The summary fold's title, shared by its render and its size estimate so the
/// two cannot drift.
fn summary_title(hidden: usize) -> String {
    format!("🧩 已加载技能（等 {hidden} 个）")
}

/// The summary fold's byte estimate, mirroring [`build_fold`]'s 400-byte panel
/// overhead plus its title.
fn summary_fold_size(hidden: usize) -> usize {
    400 + summary_title(hidden).len()
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

    /// `loaded_skill_folds` with a fresh card markdown state — the ordinary
    /// single-call case. Tests that exercise the shared table budget pass their
    /// own [`CardMarkdown`].
    fn folds(skills: &[MessageSkill], prefix: &str) -> Vec<serde_json::Value> {
        loaded_skill_folds(skills, prefix, &mut CardMarkdown::new())
    }

    /// The server's prepared skill body is a `<skill_content>` envelope around
    /// the skill's own markdown: the fold shows the markdown alone, dropping
    /// the wrapper and the sampled `<skill_files>` inventory (spec #652,
    /// ticket #655).
    #[test]
    fn a_loaded_skill_folds_the_unwrapped_body_under_its_title() {
        let skill = prepared("implement-spec", "# Skill: implement-spec\n\nDo the thing.");
        let panels = folds(&[skill], "skill_");

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
            let panels = folds(&[skill], "skill_");
            assert_eq!(panels[0]["header"]["title"]["content"], "🧩 已加载技能：foreman");
            assert_eq!(panels[0]["elements"][0]["content"], "（无说明）");
        }
    }

    /// The element ids keep the caller's prefix, so each site's fold state is
    /// stable across a re-render.
    #[test]
    fn the_fold_ids_carry_the_callers_prefix() {
        let panels = folds(&[prepared("a", "x"), prepared("b", "y")], "snap_0_skill_");
        assert_eq!(panels[0]["element_id"], "snap_0_skill_0");
        assert_eq!(panels[1]["element_id"], "snap_0_skill_1");
    }

    /// The card's shared budget bounds the folds even when a message attaches
    /// several large CJK skills whose per-fold caps would sum far past Feishu's
    /// total card limit (AGENTS.md #13). Every skill still gets its OWN titled
    /// fold below the component cap — the character budget is the size guard —
    /// and a fold past the budget renders its title with the omission body
    /// rather than an oversized body.
    #[test]
    fn the_folds_share_one_body_budget_across_the_card() {
        // 8,000 CJK chars per skill — ten of them are 80,000 raw chars.
        let huge = "很长的技能说明。".repeat(1_000);
        let skills: Vec<MessageSkill> = (0..10).map(|i| prepared(&format!("skill-{i}"), &huge)).collect();

        let panels = folds(&skills, "f_");
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
    /// #655). The estimate charges the same 2-components-per-fold walk, so the
    /// live-card splitter reserves the truth rather than half of it.
    #[test]
    fn a_pathological_skill_count_collapses_into_one_summary_fold() {
        let skills: Vec<MessageSkill> = (0..150)
            .map(|i| prepared(&format!("skill-{i}"), "body"))
            .collect();

        let panels = folds(&skills, "f_");
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

        // The splitter's estimate charges the same components, never one per
        // fold (which is what let 101 skills build a 202-component card).
        let (_, comps) = loaded_skill_folds_estimate(&skills);
        assert_eq!(comps, components);

        // An ordinary message is far below the cap: one fold per skill, no
        // summary.
        let few = folds(&skills[..5], "f_");
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
        let skills = [prepared("one", &body), prepared("two", &body)];

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
        let panels = loaded_skill_folds(&[prepared("a", "Do the thing.")], "f_", &mut md);
        let body = panels[0]["elements"][0]["content"].as_str().unwrap();
        assert!(body.starts_with("```"), "the fold body is fenced: {body}");
        assert!(body.ends_with("```"), "the fence closes: {body}");
        assert!(body.contains("Do the thing."), "{body}");
    }
}
