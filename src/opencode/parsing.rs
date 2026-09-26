//! Generation-neutral cola-side helpers for the OpenCode adapter.
//!
//! Only the pieces that are cola's own contract live here, not a protocol
//! generation's: the self-identifying user-message id (ADR-0026) and the
//! `provider/model` spec parser behind `/model` and the effective-model
//! resolution. Protocol payload shapes live in the generation strategies (V1
//! under [`super::v1`]), never in this module.

use super::types::ModelInfo;

/// The id prefix cola assigns to every user message it submits (ADR-0026).
/// cola picks the message's id (`PromptInput.messageID`) at send time and the
/// server persists it, so the `msg_cola_` prefix self-identifies a message as
/// cola-authored — the external-message sync uses it instead of a timestamp
/// baseline that goes stale when the server dies mid-turn.
const COLA_MESSAGE_ID_PREFIX: &str = "msg_cola_";

/// Generate a fresh self-identifying user-message id for a prompt cola is about
/// to send (ADR-0026). The server validates ids by `msg` prefix and persists
/// the supplied id, and re-posting the same id is idempotent — so a retry that
/// reuses the id never duplicates the user message.
pub(crate) fn cola_message_id() -> String {
    format!("{}{}", COLA_MESSAGE_ID_PREFIX, uuid::Uuid::new_v4().simple())
}

/// Whether a stored user message was authored by cola (ADR-0026). Any user
/// message whose id carries the `msg_cola_` prefix was submitted by cola on
/// behalf of Feishu; everything else in a cola-mapped session is a candidate
/// for the external-message sync.
pub(crate) fn is_cola_message_id(id: &str) -> bool {
    id.starts_with(COLA_MESSAGE_ID_PREFIX)
}

/// Parse "provider/model" into ModelInfo. A two-part split: model IDs may
/// themselves contain slashes (gateway models like `openrouter/openai/o3`), so
/// the provider is only the FIRST segment and the whole remainder is the model
/// id. The variant is never part of this string — it lives in a separate
/// SessionEntry field (ADR-0020) to keep the text form unambiguous.
pub(crate) fn parse_model(spec: &str) -> Option<ModelInfo> {
    let (provider, id) = spec.split_once('/')?;
    if provider.is_empty() || id.is_empty() {
        return None;
    }
    Some(ModelInfo {
        id: id.to_string(),
        provider_id: provider.to_string(),
        variant: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_model_two_part_split_keeps_slashed_model_ids() {
        // Model ids may contain slashes (gateway models like
        // `openrouter/openai/o3`): the provider is the FIRST segment and the
        // whole remainder is the model. The variant is never part of this
        // string (ADR-0020) — it lives in a separate session field.
        let m = parse_model("openrouter/openai/o3").unwrap();
        assert_eq!(m.provider_id, "openrouter");
        assert_eq!(m.id, "openai/o3");
        assert!(m.variant.is_none());

        let simple = parse_model("opencode-go/deepseek-v4-flash").unwrap();
        assert_eq!(simple.provider_id, "opencode-go");
        assert_eq!(simple.id, "deepseek-v4-flash");

        assert!(parse_model("no-slash").is_none());
        assert!(parse_model("").is_none());
    }

    #[test]
    fn cola_message_id_self_identifies_and_is_unique() {
        let a = cola_message_id();
        let b = cola_message_id();
        assert!(a.starts_with("msg_cola_"), "prefix missing: {a}");
        assert_ne!(a, b, "ids must be unique");
        assert!(is_cola_message_id(&a));
        assert!(!is_cola_message_id("msg_serverside"));
        assert!(!is_cola_message_id("msg_cola")); // no trailing separator
    }
}
