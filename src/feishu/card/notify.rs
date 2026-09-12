use serde_json::json;

use super::{card_shell, clean_session_label};

/// A notification card telling the Feishu side that OpenChamber (or another
/// client on the same store) has posted a new user message to a session.
/// Kept deliberately small and link-free — cola doesn't couple to OpenChamber.
pub fn build_external_message_card(session_name: &str, preview: &str) -> serde_json::Value {
    let session_name = clean_session_label(session_name);
    let mut content = String::new();
    if !session_name.is_empty() {
        content.push_str(&format!("**{}**\n", session_name));
    }
    content.push_str(preview);
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
    card_shell(
        "✅ 已处理",
        "green",
        vec![json!({ "tag": "markdown", "content": body })],
    )
}
