//! The loaded-skill fold (spec #652, ticket #655): one skill a Turn loaded into
//! its user message, rendered as a folded `🧩 已加载技能：<name>` panel whose body
//! is the skill's own instructions. One shared renderer feeds all three sites —
//! the live Turn card, the Session Snapshot's 「最近对话」 tail and the External
//! Message preview — so the `🧩` vocabulary keeps one shape.

use crate::backend::MessageSkill;

use super::sanitize::sanitize_markdown;
use super::shell::collapsible_panel;
use super::tool_render::{TOOL_OUTPUT_MAX_CHARS, parse_skill_envelope};
use super::truncate_md;

/// The body an attached skill with no instructions renders (its payload named
/// the skill but carried no prepared body, or a body that unwraps to nothing).
/// A collapsible panel needs a non-empty markdown element, so the titled fold
/// still opens rather than vanishing or failing Feishu's content check.
const EMPTY_BODY: &str = "（无说明）";

/// One loaded skill as a folded collapsible panel: the title
/// `🧩 已加载技能：<name>`, the body the skill's instructions with the server's
/// `<skill_content>` envelope and its sampled `<skill_files>` list stripped —
/// the same unwrapping the `skill` tool panel uses (spec #652, ticket #655).
/// `element_id` keeps the reader's fold state across a re-render, like every
/// other panel's.
pub(crate) fn loaded_skill_panel(skill: &MessageSkill, element_id: Option<&str>) -> serde_json::Value {
    let body = skill
        .instructions
        .as_deref()
        .map(|text| parse_skill_envelope(text).unwrap_or_else(|| text.to_string()))
        .unwrap_or_default();
    let body = truncate_md(&body, TOOL_OUTPUT_MAX_CHARS);
    let body = if body.trim().is_empty() {
        EMPTY_BODY.to_string()
    } else {
        sanitize_markdown(&body)
    };
    collapsible_panel(&format!("🧩 已加载技能：{}", skill.name), &body, element_id)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The server's prepared skill body is a `<skill_content>` envelope around
    /// the skill's own markdown: the fold shows the markdown alone, dropping
    /// the wrapper and the sampled `<skill_files>` inventory (spec #652,
    /// ticket #655).
    #[test]
    fn a_loaded_skill_folds_the_unwrapped_body_under_its_title() {
        let skill = MessageSkill {
            id: "implement-spec".into(),
            name: "implement-spec".into(),
            instructions: Some(
                "<skill_content name=\"implement-spec\">\n# Skill: implement-spec\n\n\
                 Do the thing.\n\n<skill_files>\n<file>/root/skills/a.md</file>\n\
                 </skill_files>\n</skill_content>"
                    .into(),
            ),
        };
        let panel = loaded_skill_panel(&skill, Some("skill_0"));

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
            let panel = loaded_skill_panel(&skill, None);
            assert_eq!(panel["header"]["title"]["content"], "🧩 已加载技能：foreman");
            assert_eq!(panel["elements"][0]["content"], "（无说明）");
        }
    }
}
