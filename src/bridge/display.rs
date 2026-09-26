//! Compact identity strings for humans: the session-id, directory and model
//! labels cola renders on cards, toasts and text replies. Pure formatting —
//! no platform or backend access — shared by the command dispatcher, the
//! topic transaction and the Feishu card builders.

use crate::config::ThreadKey;

/// The Feishu-side label for the current conversation (ADR-0022): a topic is a
/// 本话题, everything else is a 本聊天. Used where cola must name the Feishu
/// side without overloading 会话 (which always means the OpenCode session).
pub(crate) fn feishu_side_label(thread_key: &ThreadKey) -> &'static str {
    if thread_key.thread_id.is_empty() {
        "本聊天"
    } else {
        "本话题"
    }
}

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
pub(crate) fn title_or_id_tail(s: &crate::opencode::types::SessionListInfo) -> String {
    let cleaned = crate::feishu::card::clean_session_label(&s.title);
    if cleaned.is_empty() {
        id_tail(&s.id)
    } else {
        cleaned
    }
}

/// A compact age label for a Session's last activity (its server
/// `time.updated`): `42s`, `5m`, `3h`, `2d`. The `/sub` card's last-activity
/// stamp. `now_ms` is passed in so the age is measured at card build time (the
/// card is a one-shot read, never a ticking one); a clock skewed into the
/// future reads as `0s` instead of a negative age.
pub(crate) fn relative_time(at_ms: i64, now_ms: i64) -> String {
    let secs = ((now_ms - at_ms).max(0) / 1000) as u64;
    if secs < 60 {
        format!("{secs}s")
    } else if secs < 3600 {
        format!("{}m", secs / 60)
    } else if secs < 86_400 {
        format!("{}h", secs / 3600)
    } else {
        format!("{}d", secs / 86_400)
    }
}

/// The display identity of a session's model from the list payload
/// (`providerID/modelID@variant`), matching how cola renders model identity
/// elsewhere. Returns None when the payload carries no model.
///
/// This is the known exception to the transcript seam (spec #332): it reads
/// the public session DTO's raw `model` payload, whose normalization was
/// deliberately left out of scope, rather than the neutral read model.
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

    /// The `/sub` row's last-activity age: one unit, floor-rounded; a future
    /// timestamp (clock skew) clamps to `0s` instead of going negative.
    #[test]
    fn relative_time_labels_each_magnitude() {
        let now = 1_700_000_000_000;
        assert_eq!(relative_time(now, now), "0s");
        assert_eq!(relative_time(now - 42_000, now), "42s");
        assert_eq!(relative_time(now - 83_000, now), "1m");
        assert_eq!(relative_time(now - 3 * 3_600_000, now), "3h");
        assert_eq!(relative_time(now - 2 * 86_400_000, now), "2d");
        assert_eq!(relative_time(now + 5_000, now), "0s");
    }

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
