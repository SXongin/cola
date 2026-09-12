//! Pure functions over the OpenCode wire format: parse server payloads into
//! DTOs and build prompt request bodies. No I/O lives here, so every function
//! is unit-testable without a socket; the HTTP transport is in `client`.

use super::types::{ImageInput, ModelInfo, ModelOption, ProviderModels, SessionStatus};

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

/// Parse a `GET /provider` response body into the provider → models map the
/// `/model` and `/think` cards render. Only providers the server reports as
/// `connected` are surfaced (see `Client::list_models`). Pure so it is
/// unit-testable against a captured fixture.
pub(crate) fn parse_provider_models(v: &serde_json::Value) -> Vec<ProviderModels> {
    let Some(all) = v.get("all").and_then(|a| a.as_array()) else {
        return Vec::new();
    };
    let connected: Option<std::collections::HashSet<String>> = v
        .get("connected")
        .and_then(|c| c.as_array())
        .map(|ids| ids.iter().filter_map(|i| i.as_str()).map(String::from).collect());
    all.iter()
        .filter(|prov| {
            connected
                .as_ref()
                .is_none_or(|ids| ids.contains(prov.get("id").and_then(|i| i.as_str()).unwrap_or("")))
        })
        .filter_map(|prov| {
            let id = prov.get("id").and_then(|i| i.as_str())?.to_string();
            let models: Vec<ModelOption> = prov
                .get("models")
                .and_then(|m| m.as_object())
                .map(|m| {
                    m.iter()
                        .map(|(model_id, m_info)| ModelOption {
                            id: model_id.clone(),
                            variants: declared_variants(m_info),
                        })
                        .collect()
                })
                .unwrap_or_default();
            if models.is_empty() {
                None
            } else {
                Some(ProviderModels { provider: id, models })
            }
        })
        .collect()
}

/// The variant names a model declares, per `GET /provider`. The server
/// serializes `model.variants` as a Record keyed by variant id (`{"low": {...},
/// "high": {...}, ...}`); very old servers sent an array of `{id, ...}` objects.
/// Accept both so `/think` and the `/model` auto-clear resolve the same set
/// either way.
fn declared_variants(m_info: &serde_json::Value) -> Vec<String> {
    let Some(variants) = m_info.get("variants") else {
        return Vec::new();
    };
    if let Some(obj) = variants.as_object() {
        return obj.keys().cloned().collect();
    }
    variants
        .as_array()
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.get("id").and_then(|i| i.as_str()).map(String::from))
                .collect()
        })
        .unwrap_or_default()
}

/// The prompt `parts` array: a text part followed by one data-URL `file` part
/// per image. OpenCode decodes the data URL and normalizes the image before
/// handing it to a vision-capable model (FilePartInput). Shared by `prompt`
/// and `prompt_async`.
pub(crate) fn build_parts(text: &str, images: &[ImageInput]) -> Vec<serde_json::Value> {
    let mut parts = vec![serde_json::json!({ "type": "text", "text": text })];
    for img in images {
        parts.push(serde_json::json!({
            "type": "file",
            "mime": img.mime,
            "url": format!("data:{};base64,{}", img.mime, img.data_base64),
        }));
    }
    parts
}

/// Parse one `GET /session/status` map entry (e.g. `{"type":"busy"}`) into a
/// [`SessionStatus`]. `None` for an unrecognised/absent `type` — never guess.
pub(crate) fn parse_session_status_entry(entry: &serde_json::Value) -> Option<SessionStatus> {
    match entry.get("type").and_then(|t| t.as_str()) {
        Some("idle") => Some(SessionStatus::Idle),
        Some("busy") => Some(SessionStatus::Busy),
        Some("retry") => Some(SessionStatus::Retry),
        _ => None,
    }
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

/// Attach the effective model to a prompt body: a per-session override wins
/// over the configured default; when neither is set the server uses its own
/// default. `variant` is independent — it is written whenever set, applying to
/// whatever model the server runs this turn. Shared by `prompt` and
/// `prompt_async`.
pub(crate) fn inject_model(
    body: &mut serde_json::Value,
    override_: Option<&ModelInfo>,
    configured: Option<&ModelInfo>,
    variant: Option<&str>,
) {
    if let Some(model) = override_.or(configured) {
        body["model"] = serde_json::json!({
            "providerID": model.provider_id,
            "modelID": model.id,
        });
    }
    if let Some(v) = variant {
        body["variant"] = serde_json::json!(v);
    }
}

/// Attach the per-session agent override to a prompt body. When unset the
/// server uses the session's own/default agent. Shared by `prompt` and
/// `prompt_async`.
pub(crate) fn inject_agent(body: &mut serde_json::Value, agent: Option<&str>) {
    if let Some(a) = agent {
        body["agent"] = serde_json::json!(a);
    }
}

/// Attach the cola-chosen user-message id to a prompt body (`msg_cola_…`,
/// ADR-0026). When set the server persists that id (idempotent on retries);
/// when None it generates one. Shared by `prompt` and `prompt_async`.
pub(crate) fn inject_message_id(body: &mut serde_json::Value, message_id: Option<&str>) {
    if let Some(mid) = message_id {
        body["messageID"] = serde_json::json!(mid);
    }
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
    fn inject_model_writes_variant_independently_of_model() {
        // A variant is sent even when NO model is set (the server applies it to
        // whatever model runs this turn); when a model IS set, both are written.
        let mut body = serde_json::json!({ "parts": [] });
        inject_model(&mut body, None, None, Some("high"));
        assert_eq!(body["variant"], "high");
        assert!(body.get("model").is_none(), "no model, only variant: {body}");

        let mut body2 = serde_json::json!({ "parts": [] });
        let model = parse_model("opencode-go/deepseek-v4-flash").unwrap();
        inject_model(&mut body2, Some(&model), None, Some("high"));
        assert_eq!(body2["model"]["modelID"], "deepseek-v4-flash");
        assert_eq!(body2["variant"], "high");

        // Unset variant leaves the body free of the field.
        let mut body3 = serde_json::json!({ "parts": [] });
        inject_model(&mut body3, None, None, None);
        assert!(body3.get("variant").is_none());
    }

    #[test]
    fn parses_provider_models_with_object_variants() {
        // Real `GET /provider` shape (captured): `model.variants` is a Record
        // keyed by variant id (`{"low": {...}, "high": {...}, "max": {...}}`),
        // NOT an array of `{id}` objects. Only `connected` providers surface.
        let json = serde_json::json!({
            "all": [
                {
                    "id": "opencode-go",
                    "name": "OpenCode Go",
                    "models": {
                        "deepseek-v4-flash": {
                            "id": "deepseek-v4-flash",
                            "providerID": "opencode-go",
                            "name": "DeepSeek v4 Flash",
                            "variants": {
                                "low": { "reasoningEffort": "low" },
                                "high": { "reasoningEffort": "high" },
                                "max": { "reasoningEffort": "max" }
                            }
                        },
                        "deepseek-v4-pro": {
                            "id": "deepseek-v4-pro",
                            "name": "DeepSeek v4 Pro"
                        }
                    }
                },
                {
                    "id": "not-connected-provider",
                    "models": { "x": { "id": "x", "name": "X" } }
                }
            ],
            "connected": ["opencode-go"]
        });
        let models = parse_provider_models(&json);
        assert_eq!(models.len(), 1, "only connected providers surface: {models:?}");
        assert_eq!(models[0].provider, "opencode-go");
        let flash = models[0]
            .models
            .iter()
            .find(|m| m.id == "deepseek-v4-flash")
            .unwrap();
        assert_eq!(flash.variants, vec!["high", "low", "max"]);
        let pro = models[0]
            .models
            .iter()
            .find(|m| m.id == "deepseek-v4-pro")
            .unwrap();
        assert!(pro.variants.is_empty(), "a model with no variants stays empty");
    }

    #[test]
    fn parses_provider_models_with_legacy_array_variants() {
        // Very old servers sent `variants` as an array of `{id, ...}` objects;
        // both shapes must resolve to the same variant names.
        let json = serde_json::json!({
            "all": [
                {
                    "id": "p",
                    "models": {
                        "m": {
                            "id": "m",
                            "name": "M",
                            "variants": [ { "id": "high", "name": "High" }, { "id": "low", "name": "Low" } ]
                        }
                    }
                }
            ]
        });
        let models = parse_provider_models(&json);
        assert_eq!(models.len(), 1);
        assert_eq!(models[0].models[0].variants, vec!["high", "low"]);
    }

    #[test]
    fn parses_provider_models_without_connected_keeps_all() {
        // No `connected` field (older server): every provider is kept.
        let json = serde_json::json!({
            "all": [
                { "id": "a", "models": { "m": { "id": "m", "name": "M" } } },
                { "id": "b", "models": { "n": { "id": "n", "name": "N" } } }
            ]
        });
        let models = parse_provider_models(&json);
        assert_eq!(models.len(), 2);
    }

    #[test]
    fn parses_session_status_entry_types() {
        assert_eq!(
            parse_session_status_entry(&serde_json::json!({"type": "idle"})),
            Some(SessionStatus::Idle)
        );
        assert_eq!(
            parse_session_status_entry(&serde_json::json!({"type": "busy"})),
            Some(SessionStatus::Busy)
        );
        // The retry entry carries extra fields; cola reads only `type`.
        assert_eq!(
            parse_session_status_entry(&serde_json::json!({
                "type": "retry", "attempt": 1, "message": "boom", "next": 5000
            })),
            Some(SessionStatus::Retry)
        );
        // Unknown/absent type is never guessed.
        assert_eq!(
            parse_session_status_entry(&serde_json::json!({"type": "zombie"})),
            None
        );
        assert_eq!(
            parse_session_status_entry(&serde_json::json!({"no": "type"})),
            None
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
