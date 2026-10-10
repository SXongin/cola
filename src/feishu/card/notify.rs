use serde_json::json;

use super::clean_session_label;
use super::sanitize::sanitize_markdown;
use super::shell::card_shell;

/// A notification card telling the Feishu side that OpenChamber (or another
/// client on the same store) has posted a new user message to a session.
/// Kept deliberately small and link-free — cola doesn't couple to OpenChamber.
/// The preview is text only: a skill attached from another client is NOT
/// rendered here (the loaded-skill fold lives only on the dedicated
/// `/skill` card, ADR-0077's acceptance reversal).
pub fn build_external_message_card(session_name: &str, preview: &str) -> serde_json::Value {
    let session_name = clean_session_label(session_name);
    let mut content = String::new();
    if !session_name.is_empty() {
        content.push_str(&format!("**{}**\n", session_name));
    }
    content.push_str(preview);
    let content = sanitize_markdown(&content);
    card_shell(
        "💬 有新消息",
        "blue",
        vec![json!({ "tag": "markdown", "content": content })],
    )
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

    /// An External Message renders the preview text only (spec #652, ticket
    /// #655, acceptance reversal): a skill attached from another client is NOT
    /// rendered here — no loaded-skill fold.
    #[test]
    fn an_external_message_renders_only_the_preview() {
        let card = build_external_message_card("proj", "看一下这个");
        let s = card.to_string();
        assert!(s.contains("看一下这个"), "{s}");
        assert!(!s.contains("已加载技能"), "no skill fold on the preview: {s}");
    }
}
