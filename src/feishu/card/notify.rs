use serde_json::json;

use super::clean_session_label;
use super::sanitize::sanitize_markdown;
use super::shell::card_shell;
use super::skill::loaded_skill_folds;
use crate::backend::MessageSkill;

/// A notification card telling the Feishu side that OpenChamber (or another
/// client on the same store) has posted a new user message to a session.
/// Kept deliberately small and link-free — cola doesn't couple to OpenChamber.
/// The message's attached skills ride the same card as the loaded-skill fold
/// (spec #652, ticket #655), beside the preview text, so a skill loaded from
/// another client is visible in Feishu too.
pub fn build_external_message_card(
    session_name: &str,
    preview: &str,
    skills: &[MessageSkill],
) -> serde_json::Value {
    let session_name = clean_session_label(session_name);
    let mut content = String::new();
    if !session_name.is_empty() {
        content.push_str(&format!("**{}**\n", session_name));
    }
    content.push_str(preview);
    let content = sanitize_markdown(&content);
    let mut elements = vec![json!({ "tag": "markdown", "content": content })];
    elements.extend(loaded_skill_folds(skills, "ext_skill_"));
    card_shell("💬 有新消息", "blue", elements)
}

/// Replacement for a permission/question card whose request was already resolved
/// by another client (e.g. OpenChamber) — so the Feishu card never stays dead.
/// `detail` is the original request text, so the user can see WHAT was handled.
pub fn build_resolved_elsewhere_card(kind: &str, detail: &str) -> serde_json::Value {
    let mut body = format!("该{}请求已在其他端处理，无需操作。", kind);
    if !detail.is_empty() {
        body.push_str(&format!("\n\n{}", detail));
    }
    let body = sanitize_markdown(&body);
    card_shell(
        "✅ 已处理",
        "green",
        vec![json!({ "tag": "markdown", "content": body })],
    )
}

/// Replacement for a permission/question card whose request died with its
/// session's run (an interruption, a location eviction, a restart): nobody
/// answered it, so the card must not claim another client did. `detail` is the
/// original request text, so the user can see WHAT was left unhandled.
pub fn build_interrupted_card(kind: &str, detail: &str) -> serde_json::Value {
    let mut body = format!("会话已中断，该{}请求未处理。", kind);
    if !detail.is_empty() {
        body.push_str(&format!("\n\n{}", detail));
    }
    let body = sanitize_markdown(&body);
    card_shell(
        "⏱ 已随会话中断",
        "orange",
        vec![json!({ "tag": "markdown", "content": body })],
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An External Message whose user message attached a skill renders the
    /// loaded-skill fold beside the preview (spec #652, ticket #655), so a
    /// skill loaded from another client is visible in Feishu.
    #[test]
    fn an_external_message_with_a_skill_renders_the_fold_beside_the_preview() {
        let skills = vec![MessageSkill {
            id: "implement-spec".into(),
            name: "implement-spec".into(),
            instructions: Some(
                "<skill_content name=\"implement-spec\">\nDo the thing.\n\
                 <skill_files>\n<file>/a.md</file>\n</skill_files>\n</skill_content>"
                    .into(),
            ),
        }];
        let card = build_external_message_card("proj", "看一下这个", &skills);
        let s = card.to_string();
        assert!(s.contains("看一下这个"), "{s}");
        assert!(s.contains("🧩 已加载技能：implement-spec"), "{s}");
        assert!(s.contains("Do the thing."), "{s}");
        assert!(!s.contains("skill_files"), "file list stripped: {s}");
    }

    /// A message with no skills renders exactly the preview card as before.
    #[test]
    fn an_external_message_without_skills_renders_only_the_preview() {
        let card = build_external_message_card("proj", "看一下这个", &[]);
        let s = card.to_string();
        assert!(s.contains("看一下这个"), "{s}");
        assert!(!s.contains("已加载技能"), "{s}");
    }

    /// Several large CJK skills must not push the one-shot notification card
    /// past Feishu's total limit: the folds share one card-wide body budget
    /// (spec #652, ticket #655). Every skill still gets its own titled fold.
    #[test]
    fn several_large_skills_stay_within_the_message_card_budget() {
        let huge = "很长的技能说明。".repeat(1_000); // 8,000 CJK chars (24 KB) each
        let skills: Vec<MessageSkill> = (0..10)
            .map(|i| MessageSkill {
                id: format!("s{i}"),
                name: format!("skill-{i}"),
                instructions: Some(huge.clone()),
            })
            .collect();
        let card = build_external_message_card("proj", "看一下这个", &skills);
        let s = card.to_string();
        assert!(s.contains("🧩 已加载技能：skill-0"), "{s}");
        assert!(
            s.contains("🧩 已加载技能：skill-9"),
            "every skill keeps its own fold: {s}"
        );
        let size = s.len();
        assert!(
            size <= crate::feishu::card::FEISHU_CARD_LIMIT_BYTES,
            "the folds pushed the notification to {size} bytes"
        );
    }
}
