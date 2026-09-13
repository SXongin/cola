//! Compact identity strings for humans: the session-id, directory and model
//! labels cola renders on cards, toasts and text replies. Pure formatting —
//! no platform or backend access — shared by the command dispatcher, the
//! topic transaction and the Feishu card builders.

/// The first 7 characters of a session id with its `ses_` prefix stripped —
/// the compact display hash on cards. `resolve_session` accepts this bare hash
/// as a query, so a copy-pasted card hash resolves without the `ses_` prefix.
pub(crate) fn id_tail(id: &str) -> String {
    id.strip_prefix("ses_").unwrap_or(id).chars().take(7).collect()
}

/// The basename of a working directory, for display (e.g. "cola" for
/// "/root/workspace/dev/cola").
pub(crate) fn dir_basename(dir: &str) -> String {
    std::path::Path::new(dir)
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .filter(|n| !n.is_empty())
        .unwrap_or_else(|| dir.to_string())
}

/// A session's display title, falling back to the id-tail when the title is a
/// meaningless default (`New session - ...`, `sess-<uuid>`, etc.).
pub(crate) fn title_or_id_tail(s: &crate::opencode::SessionListInfo) -> String {
    let cleaned = crate::feishu::card::clean_session_label(&s.title);
    if cleaned.is_empty() {
        id_tail(&s.id)
    } else {
        cleaned
    }
}

/// The display identity of a session's model from the list payload
/// (`providerID/modelID@variant`), matching how cola renders model identity
/// elsewhere. Returns None when the payload carries no model.
pub(crate) fn model_display(model: Option<&serde_json::Value>) -> Option<String> {
    let v = model?;
    let id = match v {
        serde_json::Value::String(s) => return Some(s.clone()),
        _ => v.get("id").and_then(|x| x.as_str())?,
    };
    let mut s = match v.get("providerID").and_then(|x| x.as_str()) {
        Some(p) => format!("{p}/{id}"),
        None => id.to_string(),
    };
    if let Some(variant) = v.get("variant").and_then(|x| x.as_str()) {
        s.push('@');
        s.push_str(variant);
    }
    Some(s)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn model_display_formats_provider_model_and_variant() {
        use serde_json::json;
        // Full identity: providerID/modelID@variant (ADR-0019 model identity).
        assert_eq!(
            model_display(Some(
                &json!({"id": "deepseek-v4-flash", "providerID": "opencode-go", "variant": "low"})
            ))
            .as_deref(),
            Some("opencode-go/deepseek-v4-flash@low")
        );
        // No variant.
        assert_eq!(
            model_display(Some(&json!({"id": "gpt-4o", "providerID": "openai"}))).as_deref(),
            Some("openai/gpt-4o")
        );
        // No provider: bare id.
        assert_eq!(
            model_display(Some(&json!({"id": "claude"}))).as_deref(),
            Some("claude")
        );
        // A bare string payload passes through.
        assert_eq!(
            model_display(Some(&json!("openai/gpt-4o"))).as_deref(),
            Some("openai/gpt-4o")
        );
        // No model at all.
        assert_eq!(model_display(None), None);
    }
}
