//! Generation-neutral cola-side helpers (spec #860, #870).
//!
//! Only the pieces that are cola's own contract live here, not a protocol
//! generation's: the self-identifying user-message id (ADR-0026) and the
//! `provider/model` spec parser behind `/model` and the effective-model
//! resolution. They were the OpenCode adapter's because it was the only
//! client; a second adapter needs the same two facts, so they sit with the
//! contract. Protocol payload shapes live in the generation strategies (V1
//! under `opencode::v1`), never in this module.

use crate::backend::types::ModelInfo;

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

/// Join a provider id and a model id into the one ref cola stores and shows
/// (`provider/model`).
///
/// A provider id is an opaque TOML key and may itself contain `/`; the plain
/// convention would then split it wrongly (`foo/bar` + `gpt` reads back as
/// `foo` + `bar/gpt`). Such a provider is **quoted** — `"foo/bar"/gpt` — and
/// [`parse_model`] reads the quoted form too, so a pair built from two known
/// fields always round-trips. Model ids stay unquoted (they may contain
/// slashes, and the parser keeps the whole remainder).
pub(crate) fn join_model(provider: &str, model: &str) -> String {
    if provider.contains('/') || provider.contains('"') || provider.contains('\\') {
        let escaped = provider.replace('\\', "\\\\").replace('"', "\\\"");
        return format!("\"{escaped}\"/{model}");
    }
    format!("{provider}/{model}")
}

/// Parse "provider/model" into ModelInfo. A two-part split: model IDs may
/// themselves contain slashes (gateway models like `openrouter/openai/o3`), so
/// the provider is only the FIRST segment and the whole remainder is the model
/// id. The variant is never part of this string — it lives in a separate
/// SessionEntry field (ADR-0020) to keep the text form unambiguous. A provider
/// that itself contains `/` is written quoted ([`join_model`]) and read here.
pub(crate) fn parse_model(spec: &str) -> Option<ModelInfo> {
    let (provider, id) = if spec.starts_with('"') {
        quoted_provider(spec)?
    } else {
        let (provider, id) = spec.split_once('/')?;
        if provider.is_empty() || id.is_empty() {
            return None;
        }
        (provider.to_string(), id)
    };
    if provider.is_empty() || id.is_empty() {
        return None;
    }
    Some(ModelInfo {
        id: id.to_string(),
        provider_id: provider,
        variant: None,
    })
}

/// Re-express `spec` through the neutral provider when its validation fact has
/// expired. The boolean says whether normalization occurred, letting callers
/// preserve that generation decision beside a pending mirror write.
pub(crate) fn model_spec_for_provider_generation(
    spec: &str,
    expected_generation: Option<u64>,
    current_generation: u64,
    neutral_provider: Option<&str>,
) -> (String, bool) {
    let stale = expected_generation.is_some_and(|expected| expected != current_generation);
    if stale
        && let Some(neutral_provider) = neutral_provider
        && let Some(model) = parse_model(spec)
    {
        return (join_model(neutral_provider, &model.id), true);
    }
    (spec.to_string(), false)
}

/// The quoted-provider form of [`join_model`]: `"foo/bar"/gpt`. `\\` and `\"`
/// are the only escapes (a provider id is a config key, not free text).
fn quoted_provider(spec: &str) -> Option<(String, &str)> {
    let mut chars = spec.char_indices();
    let (_, quote) = chars.next()?;
    debug_assert_eq!(quote, '"');
    let mut provider = String::new();
    let mut escaped = false;
    for (index, ch) in chars {
        if escaped {
            provider.push(ch);
            escaped = false;
            continue;
        }
        match ch {
            '\\' => escaped = true,
            '"' => {
                let rest = &spec[index + ch.len_utf8()..];
                let model = rest.strip_prefix('/')?;
                if provider.is_empty() || model.is_empty() {
                    return None;
                }
                return Some((provider, model));
            }
            other => provider.push(other),
        }
    }
    None
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

    /// A provider id may itself contain `/` (a quoted TOML key): the pair built
    /// from the two known fields round-trips instead of splitting into
    /// `foo` + `bar/gpt`.
    #[test]
    fn join_and_parse_round_trip_a_slashed_provider() {
        let joined = join_model("foo/bar", "gpt");
        assert_eq!(joined, "\"foo/bar\"/gpt");
        let parsed = parse_model(&joined).expect("the quoted provider parses");
        assert_eq!(parsed.provider_id, "foo/bar");
        assert_eq!(parsed.id, "gpt");

        // The plain convention is untouched, and so is a slashed model id.
        assert_eq!(join_model("openai", "gpt-5"), "openai/gpt-5");
        let plain = parse_model(&join_model("openrouter", "openai/o3")).unwrap();
        assert_eq!(plain.provider_id, "openrouter");
        assert_eq!(plain.id, "openai/o3");

        // A quote or a backslash in the id is escaped, not lost.
        let odd = join_model("a\"b", "m");
        let parsed = parse_model(&odd).expect("an escaped quote parses");
        assert_eq!(parsed.provider_id, "a\"b");

        assert!(parse_model("\"unterminated/x").is_none());
        assert!(parse_model("\"/x").is_none());
    }

    #[test]
    fn stale_provider_fact_keeps_the_model_in_the_neutral_bucket() {
        assert_eq!(
            model_spec_for_provider_generation("openai/gpt-5.1", Some(3), 4, Some("codex")),
            ("codex/gpt-5.1".to_string(), true)
        );
        assert_eq!(
            model_spec_for_provider_generation("\"foo/bar\"/gpt", Some(3), 4, Some("codex")),
            ("codex/gpt".to_string(), true),
            "a quoted provider is dropped without losing the model id"
        );
        assert_eq!(
            model_spec_for_provider_generation("openai/gpt-5.1", Some(3), 3, Some("codex")),
            ("openai/gpt-5.1".to_string(), false)
        );
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
