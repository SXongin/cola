//! The Codex `/model` surface: what `config/read` declares and what each
//! provider's `/models` endpoint will answer (ticket #908).
//!
//! `model/list` is a **flat, global** catalog compiled into the app-server: it
//! names no provider and is therefore rendered as its own group, never under a
//! provider. Everything else is user configuration:
//!
//! - `[model_providers.<id>]` (from `config/read`'s raw layers — the resolved
//!   `Config` struct is not the source the ticket names, and a layer carries the
//!   raw TOML) becomes the `provider` scope, and every row there is a
//!   **provider + model pair** (issue #910): the old model name on a new
//!   provider is a 404, so a bare provider is never selectable. The pairs are
//!   what `[profiles.*]` declares for that provider plus whatever its
//!   `/models` listed;
//! - the top-level `model` and every `[profiles.<name>]`'s `model` become
//!   one-click presets in the `model` scope, because app-server has no "pick a
//!   profile" field — a profile's model and provider travel as one pair;
//! - `GET <base_url>/models` is asked per provider, using that provider's
//!   `experimental_bearer_token`. A provider that only declares `env_key` is
//!   skipped: the daemon's environment is not cola's. Results (including
//!   failures) are cached per provider for [`ProbeOutcome::cache_ttl`], so opening the card
//!   twice does not hammer a provider.
//!
//! Credentials only ever go into the outbound `Authorization` header: no note,
//! log line or card value carries a token. The URL comes from the user's own
//! config (the same trust domain as Codex), but its scheme is restricted to
//! http(s) and only the host is logged — never the path, query or headers.
//!
//! # Read-primary model/provider facts
//!
//! The surface trusts reads, not pushes. A root `/model` open already resolves
//! `current` through the bridge's own thread read, and the provider marker comes
//! from the adapter's ordered model projection (falling back to `current`), so
//! the card reflects another client's switch on its next open.
//!
//! `thread/settings/updated` **is** consumed now (issue #910): the adapter's
//! projection folds its `model`/`modelProvider` into the ordered fact
//! (`Projections::apply_at`), so a server that broadcasts a change moves the
//! card without a read. The measurement still stands — the pinned daemon does
//! not send it for a resume-over-provider to a connection that just
//! resubscribed — so a switch never depends on it: the `thread/read` read-back
//! is the verdict, and the broadcast is a bonus when a server does send it.
//! `effort` is left out of that fold: the surface never renders it, and the
//! footer's `@variant` already has its own ordered fact.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use crate::backend::types::{ModelSelection, ModelSurface, ModelSurfaceQuery};

use super::error::Result as CodexResult;
use super::transport::RpcConnection;

/// How long one provider's `/models` answer (success or failure) stays fresh.
/// The picker is re-opened by hand, so a few minutes is enough to make a
/// double-open cheap without hiding a provider that just recovered.
const PROBE_TTL: Duration = Duration::from_secs(180);

/// How long a **transient** probe failure is cached (issue #908's owner
/// review): a one-off timeout, a transport error, a `5xx`/`429` or an empty
/// list should retry on the next open, not hide the group for three minutes.
#[cfg(not(test))]
const PROBE_TRANSIENT_TTL: Duration = Duration::from_secs(20);
#[cfg(test)]
const PROBE_TRANSIENT_TTL: Duration = Duration::from_millis(200);

/// How long a **definitive** probe failure is cached: a `401`/`403`/`404`, an
/// `env_key`-only provider, a layer mix or a bad URL cannot change without the
/// config or the credential changing — and both are part of the cache key, so
/// a fix starts a fresh entry. Re-asking every open would only cost a request.
#[cfg(not(test))]
const PROBE_DEFINITIVE_TTL: Duration = Duration::from_secs(180);
#[cfg(test)]
const PROBE_DEFINITIVE_TTL: Duration = Duration::from_millis(600);

/// One probe's own deadline. A provider that does not answer within this is
/// "timeout", not a hung card.
#[cfg(not(test))]
const PROBE_TIMEOUT: Duration = Duration::from_secs(2);
#[cfg(test)]
const PROBE_TIMEOUT: Duration = Duration::from_millis(80);

/// `config/read` is a local app-server read, but a wedged connection must not
/// hold `/model` open. On expiry the surface degrades to the catalog.
#[cfg(not(test))]
pub(crate) const CONFIG_READ_TIMEOUT: Duration = Duration::from_secs(2);
#[cfg(test)]
pub(crate) const CONFIG_READ_TIMEOUT: Duration = Duration::from_millis(200);

/// What a callback's budget must leave for the card build and the ack
/// response. The remainder is the slice **both** the config read and the
/// catalog read may use; they run concurrently, so the surface returns by
/// about that slice, not their sum.
const CALLBACK_READ_MARGIN: Duration = Duration::from_millis(1400);

/// The slice a callback read may give each of its two concurrent reads
/// (config and catalog): `budget - CALLBACK_READ_MARGIN` — 600 ms out of a 2 s
/// budget. `None` means this is not a callback read.
pub(crate) fn callback_read_slice(ack_budget: Option<Duration>) -> Option<Duration> {
    ack_budget.map(|budget| budget.saturating_sub(CALLBACK_READ_MARGIN))
}

/// The config-read timeout for one surface read.
///
/// A callback read (`ack_budget = Some`) is answered inside a card-ack budget;
/// it must not also run a fresh probe, so its config read gets
/// [`callback_read_slice`] (600 ms out of a 2 s budget) and probes come from
/// the cache. A non-callback read (`None`) keeps [`CONFIG_READ_TIMEOUT`] and
/// fresh probes — it is a command's page, not a callback.
pub(crate) fn config_read_timeout(ack_budget: Option<Duration>) -> Duration {
    callback_read_slice(ack_budget).unwrap_or(CONFIG_READ_TIMEOUT)
}

/// The `model` scope's identity (also the card callback's `level`).
pub(crate) const MODEL_SCOPE: &str = "model";
/// The `provider` scope's identity. The bridge's provider navigation keys on
/// this name, so it is the same neutral word OpenCode uses.
pub(crate) const PROVIDER_SCOPE: &str = "provider";

/// The built-in catalog's group inside the `model` scope. It is deliberately
/// **not** a provider name: `model/list` is global and a catalog entry may run
/// through any configured provider, so rendering it under one would lie.
pub(crate) const CATALOG_GROUP: &str = "内置目录（全局，未校验）";
/// The top-level `model`'s group.
pub(crate) const CONFIG_DEFAULT_GROUP: &str = "配置默认";
/// Every `[profiles.<name>]` preset's group.
pub(crate) const PROFILE_GROUP: &str = "Profile 预设";

/// The neutral provider bucket the picker's `codex/<id>` callback value uses.
const MODEL_PROVIDER: &str = "codex";

/// One configured provider's probe-relevant fields. The bearer token is the
/// only secret here; nothing else in this module formats it.
#[derive(Clone, PartialEq, Eq)]
pub(crate) struct ConfiguredProvider {
    pub(crate) id: String,
    pub(crate) name: String,
    pub(crate) base_url: Option<String>,
    pub(crate) token: Option<String>,
    pub(crate) env_key: Option<String>,
    /// The base URL and the credential (token or `env_key`) came from
    /// **different** config layers (issue #908): the layers are not equally
    /// trusted, so no probe is made — a refused project layer must not receive
    /// a user layer's token.
    pub(crate) mixed_layers: bool,
}

/// The token and every URL part beyond scheme+host are redacted, so a `{:?}`
/// of a config (or a provider) can never print a credential into a panic, a
/// debug log or a test artefact. A URL can carry a secret in its userinfo or
/// query, exactly where the module's logging rule already refuses to look.
impl std::fmt::Debug for ConfiguredProvider {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ConfiguredProvider")
            .field("id", &self.id)
            .field("name", &self.name)
            .field("base_url", &safe_base_url(self.base_url.as_deref()))
            .field("token", &self.token.as_ref().map(|_| "<redacted>"))
            .field("env_key", &self.env_key)
            .finish()
    }
}

/// The most a provider URL may show in a debug surface: scheme + host, never
/// userinfo, path, query or fragment. An unparseable URL is `<invalid>` for the
/// same reason — the raw string could be the only place a secret lives.
fn safe_base_url(base_url: Option<&str>) -> Option<String> {
    let base_url = base_url?;
    match reqwest::Url::parse(base_url) {
        // Only the schemes the probe itself accepts are shown; anything else
        // (including a no-host parse of a secret-looking string) is opaque.
        Ok(url) if matches!(url.scheme(), "http" | "https") => Some(format!(
            "{}://{}",
            url.scheme(),
            url.host_str().unwrap_or("<invalid>")
        )),
        Ok(_) | Err(_) => Some("<invalid>".to_string()),
    }
}

/// Clip a surface label/note to `max_chars` characters, adding an ellipsis.
/// The surface owns its one-line budget; a backend adapter does not reach into
/// the card layer for it.
fn clip(text: &str, max_chars: usize) -> String {
    if text.chars().count() <= max_chars {
        text.to_string()
    } else {
        format!("{}…", text.chars().take(max_chars).collect::<String>())
    }
}

/// One `[profiles.<name>]` preset. Only `model` + `model_provider` are read:
/// the other profile keys are not part of this card (and cola does not apply
/// them).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ProfilePreset {
    pub(crate) name: String,
    pub(crate) model: String,
    pub(crate) provider: Option<String>,
}

/// What `config/read` declares that `model/list` cannot tell us.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct CodexModelConfig {
    /// The top-level `model`, shown as 「配置默认」.
    pub(crate) default_model: Option<String>,
    pub(crate) providers: Vec<ConfiguredProvider>,
    pub(crate) profiles: Vec<ProfilePreset>,
    /// `config/read` itself failed. An empty provider list is then "unknown",
    /// not "the user configured none", and the card says so.
    pub(crate) read_failed: bool,
}

impl CodexModelConfig {
    /// Parse a `config/read` result. Layers are ordered highest-precedence
    /// first (project → user → system in the pinned server), so the first
    /// layer to set a **field** wins — a higher layer may declare only
    /// `base_url` and keep a lower layer's token, exactly as Codex's own
    /// table merge does. The resolved `config` object is a fallback for a
    /// server that omits layers or carries a provider only there.
    pub(crate) fn from_read(result: &Value) -> Self {
        let mut config = Self::default();
        let mut providers: std::collections::BTreeMap<String, ProviderParts> =
            std::collections::BTreeMap::new();
        let mut profiles: std::collections::BTreeMap<String, ProfileParts> =
            std::collections::BTreeMap::new();

        if let Some(layers) = result.get("layers").and_then(Value::as_array) {
            for (layer_index, layer) in layers.iter().enumerate() {
                let Some(layer_config) = layer_config(layer) else {
                    continue;
                };
                merge_layer(
                    &mut config,
                    &layer_config,
                    &mut providers,
                    &mut profiles,
                    layer_index,
                );
            }
        }

        // A server that served `config` but no layers (or a version that keeps
        // `model_providers` only in the resolved object) still contributes.
        if let Some(object) = result.get("config").and_then(Value::as_object) {
            let resolved = Value::Object(object.clone());
            // The resolved object is its own (embedded) source: a URL from a
            // layer and a token from here are different provenance too.
            merge_layer(&mut config, &resolved, &mut providers, &mut profiles, usize::MAX);
            // The resolved object is the *effective* config, so when it names
            // the top-level model it outranks a layer: a project layer the
            // daemon refused to trust is listed but not applied, and showing its
            // model as 「配置默认」 would lie.
            if object.contains_key("model") {
                config.default_model = non_empty_str(object.get("model"));
            }
        }

        config.providers = providers
            .into_iter()
            .map(|(id, parts)| {
                // One credential, from the highest layer that named one: an
                // `env_key` a project layer trusts must not be shadowed by a
                // user layer's token (and the reverse). A lower layer's
                // credential is dropped, never paired with another layer's
                // URL (issue #908).
                let mut token = parts.token;
                let mut env_key = parts.env_key;
                match (parts.token_layer, parts.env_key_layer) {
                    (Some(token_layer), Some(env_key_layer)) if env_key_layer < token_layer => token = None,
                    (Some(token_layer), Some(env_key_layer)) if token_layer < env_key_layer => env_key = None,
                    _ => {}
                }
                let credential_layer = if token.is_some() {
                    parts.token_layer
                } else if env_key.is_some() {
                    parts.env_key_layer
                } else {
                    None
                };
                ConfiguredProvider {
                    name: parts.name.unwrap_or_else(|| id.clone()),
                    id,
                    base_url: parts.base_url,
                    mixed_layers: parts.base_url_layer.is_some()
                        && credential_layer.is_some()
                        && parts.base_url_layer != credential_layer,
                    token,
                    env_key,
                }
            })
            .collect();
        config.profiles = profiles
            .into_iter()
            .filter_map(|(name, parts)| {
                parts.model.map(|model| ProfilePreset {
                    name,
                    model,
                    provider: parts.provider,
                })
            })
            .collect();
        config
    }
}

#[derive(Default)]
struct ProviderParts {
    name: Option<String>,
    base_url: Option<String>,
    token: Option<String>,
    env_key: Option<String>,
    /// Which layer supplied each field (issue #908). A probe may only use a
    /// URL and a credential that came from the **same** layer: the layers are
    /// not equally trusted (a project layer may be refused by the daemon), and
    /// merging them per field would let a project URL receive a user token.
    /// The two credential kinds are tracked apart: a higher layer naming
    /// `env_key` must not keep a lower layer's token (and the reverse).
    base_url_layer: Option<usize>,
    token_layer: Option<usize>,
    env_key_layer: Option<usize>,
}

#[derive(Default)]
struct ProfileParts {
    model: Option<String>,
    provider: Option<String>,
}

/// Fill only the fields a higher-precedence layer has not named yet: a table
/// key is merged per field, not replaced wholesale.
fn set_first(slot: &mut Option<String>, value: Option<String>) {
    if slot.is_none() {
        *slot = value;
    }
}

/// [`set_first`] plus the layer that won, for the fields a probe pairs up.
fn set_first_at(
    slot: &mut Option<String>,
    layer_slot: &mut Option<usize>,
    value: Option<String>,
    layer: usize,
) {
    if slot.is_none()
        && let Some(value) = value
    {
        *slot = Some(value);
        *layer_slot = Some(layer);
    }
}

/// A layer's config is JSON today, but the schema types it as `true`; a server
/// that hands back the raw TOML text is accepted too.
fn layer_config(layer: &Value) -> Option<Value> {
    let config = layer.get("config")?;
    match config {
        Value::Object(_) => Some(config.clone()),
        Value::String(text) => {
            let parsed: toml::Value = toml::from_str(text).ok()?;
            serde_json::to_value(parsed).ok()
        }
        _ => None,
    }
}

fn merge_layer(
    config: &mut CodexModelConfig,
    layer: &Value,
    providers: &mut std::collections::BTreeMap<String, ProviderParts>,
    profiles: &mut std::collections::BTreeMap<String, ProfileParts>,
    layer_index: usize,
) {
    if config.default_model.is_none() {
        config.default_model = non_empty_str(layer.get("model"));
    }

    if let Some(layer_providers) = layer.get("model_providers").and_then(Value::as_object) {
        for (id, provider) in layer_providers {
            let provider = provider.as_object();
            let field = |name: &str| {
                provider
                    .and_then(|provider| provider.get(name))
                    .and_then(Value::as_str)
                    .map(str::trim)
                    .filter(|value| !value.is_empty())
                    .map(str::to_string)
            };
            let parts = providers.entry(id.clone()).or_default();
            set_first(&mut parts.name, field("name"));
            set_first_at(
                &mut parts.base_url,
                &mut parts.base_url_layer,
                field("base_url"),
                layer_index,
            );
            set_first_at(
                &mut parts.token,
                &mut parts.token_layer,
                field("experimental_bearer_token"),
                layer_index,
            );
            set_first_at(
                &mut parts.env_key,
                &mut parts.env_key_layer,
                field("env_key"),
                layer_index,
            );
        }
    }

    if let Some(layer_profiles) = layer.get("profiles").and_then(Value::as_object) {
        for (name, profile) in layer_profiles {
            let profile = profile.as_object();
            let field = |key: &str| {
                profile
                    .and_then(|profile| profile.get(key))
                    .and_then(Value::as_str)
                    .map(str::trim)
                    .filter(|value| !value.is_empty())
                    .map(str::to_string)
            };
            let parts = profiles.entry(name.clone()).or_default();
            set_first(&mut parts.model, field("model"));
            set_first(&mut parts.provider, field("model_provider"));
        }
    }
}

fn non_empty_str(value: Option<&Value>) -> Option<String> {
    value
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
}

/// What one provider's `/models` read concluded. Failure variants are kept
/// distinct so the card can say *why* there are no buttons (403 vs 404 vs a
/// timeout), and a skipped provider says so rather than looking empty.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ProbeOutcome {
    Models(Vec<String>),
    /// Only `env_key` credentials: the daemon's environment is not cola's.
    EnvKey,
    /// The base URL and the credential came from different config layers: a
    /// lower layer's token must never be sent to a layer that may have been
    /// refused (issue #908).
    LayerMismatch,
    NoBaseUrl,
    BadBaseUrl,
    UnsupportedScheme,
    Unauthorized,
    Forbidden,
    NotFound,
    Timeout,
    Network,
    BadResponse,
    Http(u16),
    Empty,
}

impl ProbeOutcome {
    /// How long this answer stays cached, by kind: a successful list is worth
    /// [`PROBE_TTL`], a **transient** failure expires quickly so the next open
    /// retries, and a **definitive** one (a config/credential verdict, or the
    /// provider saying it has no such endpoint) is kept long — its cache key
    /// includes the URL and credential fingerprint, so fixing either one starts
    /// a fresh entry.
    fn cache_ttl(&self) -> Duration {
        match self {
            Self::Models(_) => PROBE_TTL,
            Self::Timeout | Self::Network | Self::BadResponse | Self::Empty => PROBE_TRANSIENT_TTL,
            Self::Http(code) => {
                if *code == 429 || *code >= 500 {
                    PROBE_TRANSIENT_TTL
                } else {
                    PROBE_DEFINITIVE_TTL
                }
            }
            Self::EnvKey
            | Self::LayerMismatch
            | Self::NoBaseUrl
            | Self::BadBaseUrl
            | Self::UnsupportedScheme
            | Self::Unauthorized
            | Self::Forbidden
            | Self::NotFound => PROBE_DEFINITIVE_TTL,
        }
    }

    /// A three-or-so-character tag for the provider row (#908's owner review):
    /// no sentence, no URL, no header.
    pub(crate) fn short_tag(&self) -> String {
        match self {
            Self::Models(_) => String::new(),
            Self::EnvKey => "env_key".into(),
            Self::NoBaseUrl => "无 URL".into(),
            Self::BadBaseUrl => "URL 无效".into(),
            Self::UnsupportedScheme => "非 http(s)".into(),
            Self::Unauthorized => "401".into(),
            Self::Forbidden => "403".into(),
            Self::NotFound => "404".into(),
            Self::Timeout => "超时".into(),
            Self::Network => "网络".into(),
            Self::BadResponse => "响应无效".into(),
            Self::Http(status) => format!("HTTP {status}"),
            Self::Empty => "无模型".into(),
            Self::LayerMismatch => "凭据跨层".into(),
        }
    }
}

/// One probe's cache entry. The instant is when the answer arrived; failures
/// are cached exactly like successes so a 403 does not cost 2s on every open.
#[derive(Debug, Clone)]
struct ProbeEntry {
    at: Instant,
    outcome: ProbeOutcome,
}

/// The probe cache key: provider id, base URL, the `env_key` identity and a
/// fingerprint of the token (issue #910). Never the token itself.
type ProbeCacheKey = (String, Option<String>, Option<String>, Option<u64>);

/// A non-reversible fingerprint of a bearer token for [`ProbeCacheKey`]. The
/// token never enters the key, a log line or a `Debug` surface.
fn token_fingerprint(token: Option<&str>) -> Option<u64> {
    token.map(|token| {
        use std::hash::{Hash as _, Hasher as _};
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        token.hash(&mut hasher);
        hasher.finish()
    })
}

/// Drop every entry past **its own** TTL ([`ProbeOutcome::cache_ttl`]: the
/// success/definitive [`PROBE_TTL`], the shorter [`PROBE_TRANSIENT_TTL`]).
/// Called under the cache lock before a lookup and on insert, so expired
/// provider/base-URL pairs are removed rather than merely ignored — visiting
/// projects with distinct endpoints cannot grow one connection's map without
/// bound.
fn prune_expired(entries: &mut HashMap<ProbeCacheKey, ProbeEntry>) {
    let now = Instant::now();
    entries.retain(|_, entry| now.saturating_duration_since(entry.at) < entry.outcome.cache_ttl());
}

/// Per-provider `/models` cache (one per attached connection). Clones of a
/// [`super::client::CodexClient`] share it through an `Arc`.
pub(crate) struct ModelProbes {
    entries: Mutex<HashMap<ProbeCacheKey, ProbeEntry>>,
    client: reqwest::Client,
}

impl ModelProbes {
    pub(crate) fn new() -> Self {
        // Tests point this at a loopback fake; a developer's env proxy must not
        // intercept it (the same rule as `test_http::no_proxy_transport`).
        #[cfg(test)]
        let builder = reqwest::Client::builder().no_proxy();
        #[cfg(not(test))]
        let builder = reqwest::Client::builder();
        Self {
            entries: Mutex::new(HashMap::new()),
            client: builder.build().expect("build the codex /models probe client"),
        }
    }

    /// Probe every configured provider concurrently, returning one outcome per
    /// provider id. Cached answers (fresh within [`PROBE_TTL`]) never touch the
    /// network.
    pub(crate) async fn probe_all(&self, providers: &[ConfiguredProvider]) -> HashMap<String, ProbeOutcome> {
        let probed = futures_util::future::join_all(
            providers
                .iter()
                .map(|provider| async { (provider.id.clone(), self.outcome(provider).await) }),
        )
        .await;
        probed.into_iter().collect()
    }

    /// The cache key is the provider, its base URL **and its credentials**: one
    /// provider id may be overridden per project layer, and a cached 401 from a
    /// stale token must not outlive the fix for a whole TTL (issue #910). The
    /// token is folded to a fingerprint, never stored.
    fn cache_key(provider: &ConfiguredProvider) -> ProbeCacheKey {
        (
            provider.id.clone(),
            provider.base_url.clone(),
            provider.env_key.clone(),
            token_fingerprint(provider.token.as_deref()),
        )
    }

    /// Serve this read from the cache only: a card-callback read runs on the
    /// card-ack budget and must never start a fresh probe. Expired entries are
    /// pruned here too, so a cold cache stays small rather than growing with
    /// every project that names a provider.
    pub(crate) fn cached(&self, providers: &[ConfiguredProvider]) -> HashMap<String, ProbeOutcome> {
        let mut entries = self.entries.lock().expect("codex model probes");
        prune_expired(&mut entries);
        providers
            .iter()
            .filter_map(|provider| {
                entries
                    .get(&Self::cache_key(provider))
                    .map(|entry| (provider.id.clone(), entry.outcome.clone()))
            })
            .collect()
    }

    async fn outcome(&self, provider: &ConfiguredProvider) -> ProbeOutcome {
        let key = Self::cache_key(provider);
        {
            let mut entries = self.entries.lock().expect("codex model probes");
            prune_expired(&mut entries);
            if let Some(entry) = entries.get(&key) {
                return entry.outcome.clone();
            }
        }
        let outcome = self.probe(provider).await;
        let mut entries = self.entries.lock().expect("codex model probes");
        // Prune on insert as well: a probe for a new provider must not let an
        // unrelated expired entry linger until the next read names it again.
        prune_expired(&mut entries);
        entries.insert(
            key,
            ProbeEntry {
                at: Instant::now(),
                outcome: outcome.clone(),
            },
        );
        outcome
    }

    async fn probe(&self, provider: &ConfiguredProvider) -> ProbeOutcome {
        if provider.mixed_layers {
            // A refused project layer's base_url paired with a lower layer's
            // bearer token: never send that token there (issue #908).
            return ProbeOutcome::LayerMismatch;
        }
        if provider.token.is_none() && provider.env_key.is_some() {
            tracing::debug!(
                provider = %provider.id,
                "codex /models probe skipped: env_key credentials are not readable from cola"
            );
            return ProbeOutcome::EnvKey;
        }
        let Some(base_url) = provider.base_url.as_deref() else {
            return ProbeOutcome::NoBaseUrl;
        };
        let Ok(mut url) = reqwest::Url::parse(base_url) else {
            return ProbeOutcome::BadBaseUrl;
        };
        if !matches!(url.scheme(), "http" | "https") {
            return ProbeOutcome::UnsupportedScheme;
        }
        let path = format!("{}/models", url.path().trim_end_matches('/'));
        url.set_path(&path);
        let host = url.host_str().unwrap_or_default().to_string();
        let mut request = self
            .client
            .get(url)
            .timeout(PROBE_TIMEOUT)
            .header(reqwest::header::ACCEPT, "application/json");
        if let Some(token) = provider.token.as_deref() {
            request = request.bearer_auth(token);
        }
        match request.send().await {
            Ok(response) => {
                let status = response.status();
                if !status.is_success() {
                    tracing::debug!(
                        provider = %provider.id,
                        host = %host,
                        status = status.as_u16(),
                        "codex /models probe refused"
                    );
                    return match status.as_u16() {
                        401 => ProbeOutcome::Unauthorized,
                        403 => ProbeOutcome::Forbidden,
                        404 => ProbeOutcome::NotFound,
                        other => ProbeOutcome::Http(other),
                    };
                }
                match response.json::<Value>().await {
                    Ok(body) => {
                        let models = model_ids(&body);
                        tracing::debug!(
                            provider = %provider.id,
                            host = %host,
                            status = status.as_u16(),
                            models = models.len(),
                            "codex /models probe answered"
                        );
                        if models.is_empty() {
                            ProbeOutcome::Empty
                        } else {
                            ProbeOutcome::Models(models)
                        }
                    }
                    Err(_) => {
                        tracing::debug!(
                            provider = %provider.id,
                            host = %host,
                            status = status.as_u16(),
                            "codex /models probe answered with an undecodable body"
                        );
                        ProbeOutcome::BadResponse
                    }
                }
            }
            Err(error) if error.is_timeout() => {
                // Log the class only: an error's Display can carry the URL.
                tracing::debug!(
                    provider = %provider.id,
                    host = %host,
                    "codex /models probe timed out"
                );
                ProbeOutcome::Timeout
            }
            Err(_) => {
                tracing::debug!(
                    provider = %provider.id,
                    host = %host,
                    "codex /models probe could not reach the provider"
                );
                ProbeOutcome::Network
            }
        }
    }
}

impl Default for ModelProbes {
    fn default() -> Self {
        Self::new()
    }
}

/// Read the model ids out of the shapes a `/models` endpoint may answer:
/// OpenAI's `{"data":[{"id":…}]}`, a `{"models":[…]}` variant, or a bare array
/// of strings/objects. Order is the server's; duplicates are dropped so a
/// paginated-looking duplicate cannot double a button.
fn model_ids(body: &Value) -> Vec<String> {
    let rows = body
        .get("data")
        .and_then(Value::as_array)
        .or_else(|| body.get("models").and_then(Value::as_array))
        .or_else(|| body.as_array());
    let Some(rows) = rows else {
        return Vec::new();
    };
    let mut seen = std::collections::HashSet::new();
    let mut models = Vec::new();
    for row in rows {
        let id = match row {
            Value::String(id) => Some(id.as_str()),
            _ => row.get("id").and_then(Value::as_str),
        };
        let Some(id) = id.map(str::trim).filter(|id| !id.is_empty()) else {
            continue;
        };
        if seen.insert(id.to_string()) {
            models.push(id.to_string());
        }
    }
    models
}

/// Call `config/read`. Best-effort: a server that does not serve it (or a
/// timeout) leaves the catalog-only surface intact, and the reason is a debug
/// line — `/model` never fails because config could not be read.
pub(crate) async fn read_config(
    connection: &RpcConnection,
    directory: Option<&str>,
    timeout: Duration,
) -> CodexResult<CodexModelConfig> {
    let mut params = json!({ "includeLayers": true });
    if let Some(directory) = directory.filter(|directory| !directory.is_empty()) {
        params["cwd"] = json!(directory);
    }
    let result = tokio::time::timeout(timeout, connection.request("config/read", params)).await;
    match result {
        Ok(Ok(result)) => Ok(CodexModelConfig::from_read(&result)),
        Ok(Err(error)) => Err(error),
        Err(_) => Err(super::error::CodexError::Timeout(timeout)),
    }
}

/// Build the Codex surface from the built-in catalog, the config declaration
/// and this open's probe answers.
///
/// `thread_provider` is the thread's own provider when the caller knows it
/// (the projection's ordered model fact); it outranks `query.current`'s
/// provider because a picker pick records the neutral `codex` bucket, not the
/// channel the thread really runs on.
///
/// A provider is creation-only (spec #922): a Pending Session's provider page
/// offers the selectable pairs that ride `thread/start`, while a live thread's
/// page is a read-only current value with the `/new` / `/fork` note. A Pending
/// Session has no thread yet, so its pick is always a creation one.
pub(crate) fn build_surface(
    catalog: &[crate::backend::types::ProviderModels],
    config: &CodexModelConfig,
    probes: &HashMap<String, ProbeOutcome>,
    thread_provider: Option<&str>,
    query: &ModelSurfaceQuery<'_>,
) -> ModelSurface {
    // Parsed, not string-split: a provider id may itself contain `/` (issue
    // #908), and the parser knows the quoted form.
    let query_provider = query
        .current
        .as_ref()
        .and_then(|current| crate::backend::parsing::parse_model(&current.reference))
        .map(|model| model.provider_id);
    // A picker pick records the neutral `codex` bucket, so a thread fact of
    // `codex` is not a channel: the caller's resolved provider is the honest
    // marker then. A real thread provider outranks it.
    let current_provider = match thread_provider {
        Some(provider) if provider != MODEL_PROVIDER => Some(provider.to_string()),
        _ => query_provider,
    };
    let current_model = query
        .current
        .as_ref()
        .and_then(|current| crate::backend::parsing::parse_model(&current.reference))
        .map(|model| model.id);
    let pending = query.session_id.is_none();

    // Two pages, one scope each (#908's owner review): the root is the **model**
    // page, and `切换 provider…` drills into the provider page. A drilled page
    // carries the back button from the bridge.
    let scope = if query.scope == Some(PROVIDER_SCOPE) {
        provider_page(
            config,
            probes,
            current_provider.as_deref(),
            current_model.as_deref(),
            pending,
        )
    } else {
        model_page(catalog, config, probes, current_provider.as_deref())
    };
    ModelSurface {
        current: resolved_current(query.current.as_ref(), thread_provider),
        scopes: vec![scope],
    }
}

/// The root page (#908): the flat catalog, the config's presets and the
/// **current** provider's probed models — everything the next turn can take
/// without changing the channel — plus the drill into the provider page. No
/// probe-failure notes here: a broken provider is page 2's status row.
fn model_page(
    catalog: &[crate::backend::types::ProviderModels],
    config: &CodexModelConfig,
    probes: &HashMap<String, ProbeOutcome>,
    current_provider: Option<&str>,
) -> crate::backend::types::ModelScope {
    let mut options: Vec<crate::backend::types::ModelOption> = Vec::new();
    for provider in catalog {
        for model in &provider.models {
            options.push(crate::backend::types::ModelOption {
                id: model.id.clone(),
                variants: model.variants.clone(),
                value: Some(codex_ref(&model.id)),
                group: Some(CATALOG_GROUP.to_string()),
                ..Default::default()
            });
        }
    }
    if let Some(model) = config.default_model.as_deref() {
        options.push(crate::backend::types::ModelOption {
            id: model.to_string(),
            label: Some(format!("{model}（配置默认）")),
            value: Some(codex_ref(model)),
            group: Some(CONFIG_DEFAULT_GROUP.to_string()),
            ..Default::default()
        });
    }
    for profile in &config.profiles {
        let mut label = format!("{}：{}", profile.name, profile.model);
        // `codex` is reserved for the neutral namespace (ADR-0092): a profile
        // naming it cannot be a real pair, so it is offered as the neutral
        // catalog ref (a model-only pick).
        let reserved = profile.provider.as_deref() == Some(MODEL_PROVIDER);
        if reserved {
            label.push_str("（codex 保留名）");
        } else if let Some(provider) = profile.provider.as_deref() {
            label.push_str(&format!("（{provider}）"));
        }
        let paired = profile.provider.is_some() && !reserved;
        options.push(crate::backend::types::ModelOption {
            id: profile.model.clone(),
            label: Some(clip(&label, 48)),
            value: Some(if paired {
                profile.model.clone()
            } else {
                codex_ref(&profile.model)
            }),
            group: Some(PROFILE_GROUP.to_string()),
            provider: if paired { profile.provider.clone() } else { None },
            ..Default::default()
        });
    }
    // The current provider's own `/models` answer: the ids that provider listed
    // are pairs (it named them itself). No other provider's models on this page.
    if let Some(current) = current_provider
        && current != MODEL_PROVIDER
        && let Some(provider) = config.providers.iter().find(|provider| provider.id == current)
        && let Some(ProbeOutcome::Models(models)) = probes.get(&provider.id)
    {
        let group = format!("{}（/models）", provider.name);
        for model in models {
            options.push(crate::backend::types::ModelOption {
                id: model.clone(),
                value: Some(model.clone()),
                provider: Some(provider.id.clone()),
                group: Some(group.clone()),
                ..Default::default()
            });
        }
    }
    // The drill-down into the provider page (a client with a provider axis: this
    // is Codex's own surface).
    options.push(crate::backend::types::ModelOption {
        id: PROVIDER_SCOPE.into(),
        label: Some("切换 provider…".to_string()),
        value: Some(PROVIDER_SCOPE.to_string()),
        pick: Some(crate::backend::types::PickKind::DrillDown),
        ..Default::default()
    });
    let only_drill = options
        .iter()
        .all(|option| option.pick == Some(crate::backend::types::PickKind::DrillDown));
    // One short line when `config/read` failed (ADR-0092 decision 8): the
    // catalog is known, the provider/profile data is not — the provider page
    // says so too, but the root page must not hide it.
    let note = if config.read_failed {
        Some("配置读取失败：只显示内置目录".to_string())
    } else if only_drill {
        Some("_(没有可用模型)_".to_string())
    } else {
        None
    };
    crate::backend::types::ModelScope {
        id: MODEL_SCOPE.into(),
        title: "模型（每轮可换）".into(),
        changeable: crate::backend::types::ChangeScope::ThisTurn,
        pick: crate::backend::types::PickKind::Model,
        options,
        note,
        free_text: Some(format!("也可 `/model {MODEL_PROVIDER}/<id>`")),
        intro: None,
    }
}

/// Page 2 (#908): on a **Pending Session**, every configured provider as
/// **pairs** (`provider/model`), the current pair marked, a probe failure as a
/// short tag, a pairless provider as a disabled row — the pair rides
/// `thread/start`. On a **live thread** a provider is creation-only (spec
/// #922): the page is a read-only current value with the `/new` / `/fork` note,
/// and it offers no rows at all.
fn provider_page(
    config: &CodexModelConfig,
    probes: &HashMap<String, ProbeOutcome>,
    current_provider: Option<&str>,
    current_model: Option<&str>,
    pending: bool,
) -> crate::backend::types::ModelScope {
    use crate::backend::types::{ChangeScope, ModelOption, ModelScope, PickKind};
    if !pending {
        // The neutral `codex` bucket names no channel: only a real thread
        // provider is worth showing.
        let current = current_provider.filter(|provider| *provider != MODEL_PROVIDER);
        let note = match current {
            Some(provider) => {
                format!("当前 provider：`{provider}`。provider 只能在新会话中更换：请用 `/new` 或 `/fork`。")
            }
            None => "provider 只能在新会话中更换：请用 `/new` 或 `/fork`。".to_string(),
        };
        return ModelScope {
            id: PROVIDER_SCOPE.into(),
            title: "Provider（仅新会话）".into(),
            changeable: ChangeScope::NewThread,
            pick: PickKind::Provider,
            options: Vec::new(),
            note: Some(note),
            free_text: None,
            intro: Some("provider 是会话创建时的选择".to_string()),
        };
    }
    let mut options: Vec<ModelOption> = Vec::new();
    let mut paired_providers: std::collections::HashSet<&str> = std::collections::HashSet::new();
    for provider in &config.providers {
        if provider.id == MODEL_PROVIDER {
            // Reserved for the neutral catalog namespace (ADR-0092): a pair
            // through it could never address the configured channel.
            paired_providers.insert(provider.id.as_str());
            options.push(ModelOption {
                id: provider.id.clone(),
                label: Some(clip(&format!("{}：内置目录保留名，不可选", provider.name), 48)),
                disabled: true,
                ..Default::default()
            });
            continue;
        }
        let mut models: Vec<String> = Vec::new();
        for profile in config
            .profiles
            .iter()
            .filter(|profile| profile.provider.as_deref() == Some(provider.id.as_str()))
        {
            models.push(profile.model.clone());
        }
        if let Some(ProbeOutcome::Models(probed)) = probes.get(&provider.id) {
            models.extend(probed.iter().cloned());
        }
        let mut seen = std::collections::HashSet::new();
        models.retain(|model| seen.insert(model.clone()));
        if models.is_empty() {
            continue;
        }
        paired_providers.insert(provider.id.as_str());
        for model in models {
            let same_provider = current_provider == Some(provider.id.as_str());
            // #908 AC1: the row shows the configured display name (its id when
            // the config names none); the callback still carries id + model.
            let mut label = crate::backend::parsing::join_model(&provider.name, &model);
            // The current marker is the provider fact (#908 AC1): the exact pair
            // says 「当前」, another model of the same provider says so too.
            if same_provider && current_model == Some(model.as_str()) {
                label.push_str("（当前）");
            } else if same_provider {
                label.push_str("（当前 provider）");
            }
            options.push(ModelOption {
                id: format!("{}/{model}", provider.id),
                label: Some(clip(&label, 48)),
                value: Some(model),
                provider: Some(provider.id.clone()),
                ..Default::default()
            });
        }
    }
    // Status rows: a short failure tag, and a pairless provider listed (never a
    // bare provider button).
    for provider in &config.providers {
        let current_marker = if current_provider == Some(provider.id.as_str()) {
            "（当前）"
        } else {
            ""
        };
        let failed = probes
            .get(&provider.id)
            .filter(|outcome| !matches!(outcome, ProbeOutcome::Models(_)));
        if paired_providers.contains(provider.id.as_str()) {
            if let Some(outcome) = failed {
                options.push(ModelOption {
                    id: provider.id.clone(),
                    label: Some(clip(
                        &format!("{}：{}{current_marker}", provider.name, outcome.short_tag()),
                        48,
                    )),
                    disabled: true,
                    ..Default::default()
                });
            }
            continue;
        }
        let label = match failed {
            Some(outcome) => format!("{}：{}{current_marker}", provider.name, outcome.short_tag()),
            None => format!("{}：没有已知模型{current_marker}", provider.name),
        };
        options.push(ModelOption {
            id: provider.id.clone(),
            label: Some(clip(&label, 48)),
            disabled: true,
            ..Default::default()
        });
    }
    ModelScope {
        id: PROVIDER_SCOPE.into(),
        title: "切换 provider".into(),
        changeable: ChangeScope::NewThread,
        pick: PickKind::Provider,
        // One short status, never a paragraph: the config read itself failed.
        note: config.read_failed.then(|| "_(读不到 config)_".to_string()),
        options,
        free_text: None,
        // A Pending Session's pick is a creation fact (spec #922).
        intro: Some("只在新会话生效".to_string()),
    }
}

/// The current-model line the card shows (#910). A picker pick records the
/// neutral `codex` bucket, so on a thread whose real provider is known **and
/// configured**, the line must name that provider — the same fact the footer
/// reads — instead of `codex/<id>`. Anything else is left exactly as the caller
/// resolved it.
fn resolved_current(
    current: Option<&ModelSelection>,
    thread_provider: Option<&str>,
) -> Option<ModelSelection> {
    let current = current?;
    // Parsed, not split: a provider id may itself contain `/` (issue #908).
    let Some(parsed) = crate::backend::parsing::parse_model(&current.reference) else {
        return Some(current.clone());
    };
    if parsed.provider_id != MODEL_PROVIDER {
        return Some(current.clone());
    }
    let Some(real) = thread_provider else {
        return Some(current.clone());
    };
    // The **thread projection** is the authority here, independent of the
    // config read (issue #908): a failed `config/read` (or a provider the
    // config no longer declares) must not leave a neutral `codex/<id>` on the
    // card while the footer names the real channel.
    if real == MODEL_PROVIDER {
        return Some(current.clone());
    }
    let reference = crate::backend::parsing::join_model(real, &parsed.id);
    let label = match current.label.strip_prefix(&format!("{MODEL_PROVIDER}/")) {
        Some(rest) => {
            let (model, variant) = match rest.split_once('@') {
                Some((model, variant)) => (model, Some(variant)),
                None => (rest, None),
            };
            match variant {
                Some(variant) => format!("{}@{variant}", crate::backend::parsing::join_model(real, model)),
                None => crate::backend::parsing::join_model(real, model),
            }
        }
        None => reference.clone(),
    };
    Some(ModelSelection { label, reference })
}

fn codex_ref(model: &str) -> String {
    format!("{MODEL_PROVIDER}/{model}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::types::{ChangeScope, ModelSelection};

    const TOKEN: &str = "sentinel-probe-token-DO-NOT-LEAK";

    /// A callback's budget is sliced for config and leaves the rest for the
    /// catalog round-trip, card build and ack; no budget means the root page's
    /// full config timeout.
    #[test]
    fn a_callback_budget_slices_the_config_read_and_leaves_a_margin() {
        assert_eq!(
            callback_read_slice(Some(Duration::from_secs(2))),
            Some(Duration::from_millis(600))
        );
        assert_eq!(callback_read_slice(None), None);
        assert_eq!(
            config_read_timeout(Some(Duration::from_secs(2))),
            Duration::from_millis(600)
        );
        assert_eq!(config_read_timeout(None), CONFIG_READ_TIMEOUT);
        assert_eq!(
            config_read_timeout(Some(Duration::from_millis(100))),
            Duration::ZERO,
            "a budget below the margin leaves config no time, never a negative one"
        );
    }

    fn provider(
        id: &str,
        base_url: Option<String>,
        token: Option<&str>,
        env_key: Option<&str>,
    ) -> ConfiguredProvider {
        ConfiguredProvider {
            id: id.into(),
            name: id.into(),
            base_url,
            token: token.map(str::to_string),
            env_key: env_key.map(str::to_string),
            mixed_layers: false,
        }
    }

    fn query() -> ModelSurfaceQuery<'static> {
        ModelSurfaceQuery {
            session_id: Some("thread-1"),
            directory: Some("/work/cola"),
            current: Some(ModelSelection {
                label: "opencode-go/gpt-6-luna".into(),
                reference: "opencode-go/gpt-6-luna".into(),
            }),
            ..Default::default()
        }
    }

    /// Layers arrive highest-precedence first (project → user → system). The
    /// first occurrence of an id/key wins, the display name falls back to the
    /// id, and a profile without a model contributes no preset.
    #[test]
    fn config_read_merges_layers_best_layer_first() {
        let result = json!({
            "config": {},
            "layers": [
                {
                    "name": { "type": "project" },
                    "version": "v1",
                    "config": {
                        "model": "project-model",
                        "model_provider": "project-provider",
                        "model_providers": {
                            "deepseek": {
                                "name": "DeepSeek Project",
                                "base_url": "http://127.0.0.1:1/v1",
                                "experimental_bearer_token": "project-token"
                            },
                            "no-name": { "base_url": "http://127.0.0.1:2" }
                        },
                        "profiles": {
                            "fast": { "model": "deepseek-flash", "model_provider": "deepseek" },
                            "no-model": { "model_provider": "deepseek" }
                        }
                    }
                },
                {
                    "name": { "type": "user" },
                    "version": "v2",
                    "config": {
                        "model": "user-model",
                        "model_providers": {
                            "deepseek": { "name": "DeepSeek User", "base_url": "http://user" },
                            "user-only": { "name": "User Only", "env_key": "USER_KEY" }
                        },
                        "profiles": {
                            "fast": { "model": "user-model" }
                        }
                    }
                }
            ]
        });

        let config = CodexModelConfig::from_read(&result);

        assert_eq!(config.default_model.as_deref(), Some("project-model"));
        assert_eq!(
            config.providers.iter().map(|p| p.id.as_str()).collect::<Vec<_>>(),
            ["deepseek", "no-name", "user-only"],
            "sorted, one per id, first layer wins"
        );
        assert_eq!(config.providers[0].name, "DeepSeek Project");
        assert_eq!(
            config.providers[0].token.as_deref(),
            Some("project-token"),
            "the lower layer must not overwrite the token"
        );
        assert_eq!(config.providers[1].name, "no-name", "no name means the id");
        assert_eq!(config.providers[2].env_key.as_deref(), Some("USER_KEY"));
        assert_eq!(config.profiles.len(), 1, "a model-less profile is not a preset");
        assert_eq!(config.profiles[0].name, "fast");
        assert_eq!(config.profiles[0].model, "deepseek-flash");
        assert_eq!(config.profiles[0].provider.as_deref(), Some("deepseek"));
    }

    /// A higher layer may name only one provider field; the lower layer's
    /// token/name still applies, exactly as a TOML table merge works.
    #[test]
    fn config_read_merges_provider_fields_across_layers() {
        let result = json!({
            "layers": [
                {
                    "config": {
                        "model_providers": {
                            "deepseek": { "base_url": "http://high/v1" }
                        }
                    }
                },
                {
                    "config": {
                        "model_providers": {
                            "deepseek": {
                                "name": "DeepSeek",
                                "base_url": "http://low/v1",
                                "experimental_bearer_token": "low-token"
                            }
                        }
                    }
                }
            ]
        });
        let config = CodexModelConfig::from_read(&result);
        assert_eq!(config.providers.len(), 1);
        let provider = &config.providers[0];
        assert_eq!(provider.base_url.as_deref(), Some("http://high/v1"));
        assert_eq!(provider.name, "DeepSeek");
        assert_eq!(provider.token.as_deref(), Some("low-token"));
    }

    /// The resolved `config` is a fallback when a server serves no layers (or
    /// carries the provider table only there). A raw-TOML string layer parses
    /// too — the schema types a layer's config as `true`.
    #[test]
    fn config_read_falls_back_to_resolved_config_and_raw_toml() {
        let resolved_only = json!({
            "config": {
                "model": "gpt-5.1-codex",
                "model_providers": { "local": { "name": "Local", "base_url": "http://127.0.0.1:3" } }
            }
        });
        let config = CodexModelConfig::from_read(&resolved_only);
        assert_eq!(config.default_model.as_deref(), Some("gpt-5.1-codex"));
        assert_eq!(config.providers.len(), 1);
        assert_eq!(config.providers[0].name, "Local");

        let raw = json!({
            "layers": [{
                "name": { "type": "user" },
                "version": "v1",
                "config": "model = \"raw\"\n[model_providers.x]\nname = \"X\"\nbase_url = \"http://127.0.0.1:4\"\n"
            }]
        });
        let config = CodexModelConfig::from_read(&raw);
        assert_eq!(config.default_model.as_deref(), Some("raw"));
        assert_eq!(config.providers[0].id, "x");
        assert_eq!(config.providers[0].name, "X");
    }

    /// A provider's `Debug` shows at most scheme+host. A URL can carry a
    /// credential in userinfo or a query, and an unparseable one may be the only
    /// place a secret lives, so neither its path/query nor a raw invalid string
    /// may appear — nor can the bearer token.
    #[test]
    fn provider_debug_shows_only_scheme_and_host() {
        let provider = ConfiguredProvider {
            id: "deepseek".into(),
            name: "DeepSeek".into(),
            base_url: Some(
                "https://sentinel-user:sentinel-pass@example.test/v1?token=sentinel-query#fragment".into(),
            ),
            token: Some(TOKEN.into()),
            env_key: None,
            mixed_layers: false,
        };
        let debugged = format!("{provider:?}");
        assert!(debugged.contains("https://example.test"), "{debugged}");
        for secret in [
            "sentinel-user",
            "sentinel-pass",
            "sentinel-query",
            "/v1",
            "fragment",
            TOKEN,
        ] {
            assert!(
                !debugged.contains(secret),
                "provider Debug leaked {secret:?}: {debugged}"
            );
        }

        let config = CodexModelConfig {
            providers: vec![provider],
            ..Default::default()
        };
        let debugged = format!("{config:?}");
        for secret in ["sentinel-user", "sentinel-pass", "sentinel-query", TOKEN] {
            assert!(
                !debugged.contains(secret),
                "config Debug leaked {secret:?}: {debugged}"
            );
        }

        let invalid = ConfiguredProvider {
            id: "bad".into(),
            name: "Bad".into(),
            base_url: Some("sentinel-user:sentinel-pass@not a url".into()),
            token: None,
            env_key: None,
            mixed_layers: false,
        };
        let debugged = format!("{invalid:?}");
        assert!(debugged.contains("<invalid>"), "{debugged}");
        assert!(!debugged.contains("sentinel"), "invalid URL leaked: {debugged}");
    }

    /// The token rides the Authorization header, never a note or a log line,
    /// and a second probe inside the TTL does not reach the provider.
    #[tokio::test]
    async fn a_probe_uses_the_bearer_token_caches_and_never_logs_it() {
        let server = crate::test_http::TestHttpServer::start().await;
        server.route(
            "GET",
            "/v1/models",
            200,
            r#"{"object":"list","data":[{"id":"deepseek-flash"},{"id":"deepseek-v4-pro"},{"id":"deepseek-flash"}]}"#,
        );
        let probes = ModelProbes::new();
        const URL_SECRET: &str = "sentinel-url-query";
        let provider = ConfiguredProvider {
            id: "deepseek".into(),
            name: "DeepSeek".into(),
            base_url: Some(format!("{}/v1?token={URL_SECRET}", server.base_url())),
            token: Some(TOKEN.into()),
            env_key: None,
            mixed_layers: false,
        };

        let (first, logs) = crate::bridge::test_support::capture_logs(async {
            probes.probe_all(std::slice::from_ref(&provider)).await
        })
        .await;
        assert_eq!(
            first.get("deepseek"),
            Some(&ProbeOutcome::Models(vec![
                "deepseek-flash".into(),
                "deepseek-v4-pro".into()
            ])),
            "the endpoint's ids are listed once each, in server order"
        );
        let requests = server.requests();
        assert_eq!(requests.len(), 1);
        assert_eq!(
            requests[0].header("authorization"),
            Some(format!("Bearer {TOKEN}").as_str())
        );
        assert!(
            requests[0].query.contains(URL_SECRET),
            "the query was really sent, so a leak would be possible"
        );
        assert!(!logs.contains(TOKEN), "the token leaked into a log: {logs}");
        assert!(
            !logs.contains(URL_SECRET),
            "a URL query leaked into a log: {logs}"
        );
        assert!(!logs.contains("deepseek-flash"), "the answer body leaked too");
        assert!(
            !format!("{first:?}").contains(TOKEN),
            "the token leaked into a probe outcome"
        );

        // Within the TTL the answer is served from the cache: still one request.
        let second = probes.probe_all(std::slice::from_ref(&provider)).await;
        assert_eq!(first, second);
        assert_eq!(server.request_count(), 1, "a cached probe makes no request");
    }

    /// Issue #908 (owner review): a **transient** failure (a timeout) is cached
    /// for the short TTL and retried on the next open, while a definitive one
    /// (`403`) is still cached at that moment.
    #[tokio::test]
    async fn a_timed_out_probe_retries_after_the_short_ttl() {
        let server = crate::test_http::TestHttpServer::start().await;
        server.route("GET", "/forbidden", 403, "{}");
        server.route("GET", "/broken", 500, "{}");
        server.route_delayed("GET", "/slow", 200, "{}", Duration::from_millis(400));
        let probes = ModelProbes::new();
        let forbidden = provider(
            "forbidden",
            Some(format!("{}/forbidden", server.base_url())),
            Some(TOKEN),
            None,
        );
        let broken = provider(
            "broken",
            Some(format!("{}/broken", server.base_url())),
            Some(TOKEN),
            None,
        );
        let slow = provider(
            "slow",
            Some(format!("{}/slow", server.base_url())),
            Some(TOKEN),
            None,
        );
        let outcomes = probes
            .probe_all(&[forbidden.clone(), broken.clone(), slow.clone()])
            .await;
        assert_eq!(outcomes.get("slow"), Some(&ProbeOutcome::Timeout));
        assert_eq!(outcomes.get("broken"), Some(&ProbeOutcome::Http(500)));
        assert_eq!(outcomes.get("forbidden"), Some(&ProbeOutcome::Forbidden));
        assert_eq!(server.request_count(), 3);

        // Age the **transient** entries past their short TTL, but leave the
        // 403 inside its definitive TTL: only the transient ones retry.
        {
            let mut entries = probes.entries.lock().unwrap();
            for provider in [&slow, &broken] {
                let key = ModelProbes::cache_key(provider);
                entries.get_mut(&key).unwrap().at =
                    Instant::now() - PROBE_TRANSIENT_TTL - Duration::from_millis(10);
            }
        }
        let outcomes = probes
            .probe_all(&[forbidden.clone(), broken.clone(), slow.clone()])
            .await;
        assert_eq!(
            outcomes.get("forbidden"),
            Some(&ProbeOutcome::Forbidden),
            "a definitive failure keeps its long TTL"
        );
        assert_eq!(server.request_count(), 5, "both transient failures retried");

        // A definitive entry does not survive its own (longer) TTL.
        {
            let mut entries = probes.entries.lock().unwrap();
            let key = ModelProbes::cache_key(&forbidden);
            entries.get_mut(&key).unwrap().at =
                Instant::now() - PROBE_DEFINITIVE_TTL - Duration::from_millis(10);
        }
        let outcomes = probes.probe_all(std::slice::from_ref(&forbidden)).await;
        assert_eq!(outcomes.get("forbidden"), Some(&ProbeOutcome::Forbidden));
        assert_eq!(
            server.request_count(),
            6,
            "an expired definitive answer is re-probed"
        );
    }

    /// 403, 404, a timeout and an `env_key`-only provider are distinct outcomes;
    /// failures are cached too, and the skipped provider is never dialled.
    #[tokio::test]
    async fn probe_failures_are_distinct_and_cached() {
        let server = crate::test_http::TestHttpServer::start().await;
        server.route("GET", "/forbidden", 403, "{}");
        server.route("GET", "/missing", 404, "{}");
        server.route_delayed("GET", "/slow", 200, "{}", Duration::from_millis(400));

        let probes = ModelProbes::new();
        let forbidden = provider(
            "forbidden",
            Some(format!("{}/forbidden", server.base_url())),
            Some(TOKEN),
            None,
        );
        let missing = provider(
            "missing",
            Some(format!("{}/missing", server.base_url())),
            Some(TOKEN),
            None,
        );
        let slow = provider(
            "slow",
            Some(format!("{}/slow", server.base_url())),
            Some(TOKEN),
            None,
        );
        let env = provider("env", Some(server.base_url()), None, Some("PROVIDER_KEY"));
        let no_url = provider("none", None, Some(TOKEN), None);
        let ftp = provider("ftp", Some("ftp://example.invalid/v1".into()), Some(TOKEN), None);
        let bad = provider("bad", Some("not a url".into()), Some(TOKEN), None);
        let providers = vec![forbidden, missing, slow, env, no_url, ftp, bad];

        let outcomes = probes.probe_all(&providers).await;
        assert_eq!(outcomes.get("forbidden"), Some(&ProbeOutcome::Forbidden));
        assert_eq!(outcomes.get("missing"), Some(&ProbeOutcome::NotFound));
        assert_eq!(outcomes.get("slow"), Some(&ProbeOutcome::Timeout));
        assert_eq!(outcomes.get("env"), Some(&ProbeOutcome::EnvKey));
        assert_eq!(outcomes.get("none"), Some(&ProbeOutcome::NoBaseUrl));
        assert_eq!(outcomes.get("ftp"), Some(&ProbeOutcome::UnsupportedScheme));
        assert_eq!(outcomes.get("bad"), Some(&ProbeOutcome::BadBaseUrl));
        assert_eq!(
            server.request_count(),
            3,
            "only http(s) providers are dialled; the rest are skipped"
        );

        // A failure is cached: re-probing the same providers adds no requests.
        let again = probes.probe_all(&providers).await;
        assert_eq!(outcomes, again);
        assert_eq!(server.request_count(), 3, "failures are cached within the TTL");
    }

    /// Issue #908 (blocking): a base URL from one config layer and a token from
    /// another are never paired for a probe — a refused project layer must not
    /// receive a user layer's bearer token.
    #[tokio::test]
    async fn a_layer_mixed_url_and_token_is_never_probed() {
        let server = crate::test_http::TestHttpServer::start().await;
        server.route("GET", "/models", 200, r#"{"data":[{"id":"leak"}]}"#);
        let mixed = serde_json::json!({
            "config": {},
            "layers": [
                {
                    "name": { "type": "project" },
                    "version": "v1",
                    "config": {
                        "model_providers": {
                            "evil": { "base_url": server.base_url() }
                        }
                    }
                },
                {
                    "name": { "type": "user" },
                    "version": "v1",
                    "config": {
                        "model_providers": {
                            "evil": { "experimental_bearer_token": "user-secret" }
                        }
                    }
                }
            ]
        });
        let config = CodexModelConfig::from_read(&mixed);
        let evil = config
            .providers
            .iter()
            .find(|provider| provider.id == "evil")
            .expect("the pair is still listed (display)");
        assert!(
            evil.mixed_layers,
            "the URL and the token come from different layers"
        );
        assert_eq!(
            evil.token.as_deref(),
            Some("user-secret"),
            "the fields still merge for display"
        );

        let probe = ModelProbes::new();
        let outcomes = probe.probe_all(&config.providers).await;
        assert_eq!(
            outcomes.get("evil"),
            Some(&ProbeOutcome::LayerMismatch),
            "no probe is attempted"
        );
        assert_eq!(
            server.request_count(),
            0,
            "the user token never reaches the project layer's URL"
        );

        // The same layer for both is probed normally.
        let together = serde_json::json!({
            "config": {},
            "layers": [{
                "name": { "type": "user" },
                "version": "v1",
                "config": {
                    "model_providers": {
                        "ok": {
                            "base_url": server.base_url(),
                            "experimental_bearer_token": "user-secret"
                        }
                    }
                }
            }]
        });
        let config = CodexModelConfig::from_read(&together);
        let outcomes = ModelProbes::new().probe_all(&config.providers).await;
        assert!(
            matches!(outcomes.get("ok"), Some(ProbeOutcome::Models(_))),
            "one layer's URL and token are fine: {outcomes:?}"
        );
        assert_eq!(server.request_count(), 1);
    }

    /// Issue #908 (gate follow-up): a higher layer that names `env_key` wins the
    /// credential, and the lower layer's token is **dropped** — never sent to
    /// the higher layer's URL.
    #[tokio::test]
    async fn an_env_key_above_a_token_is_not_sent() {
        let server = crate::test_http::TestHttpServer::start().await;
        server.route("GET", "/models", 200, r#"{"data":[{"id":"leak"}]}"#);
        let result = serde_json::json!({
            "config": {},
            "layers": [
                {
                    "name": { "type": "project" },
                    "version": "v1",
                    "config": {
                        "model_providers": {
                            "evil": {
                                "base_url": server.base_url(),
                                "env_key": "PROJECT_KEY"
                            }
                        }
                    }
                },
                {
                    "name": { "type": "user" },
                    "version": "v1",
                    "config": {
                        "model_providers": {
                            "evil": { "experimental_bearer_token": "user-secret" }
                        }
                    }
                }
            ]
        });
        let config = CodexModelConfig::from_read(&result);
        let evil = config
            .providers
            .iter()
            .find(|provider| provider.id == "evil")
            .expect("merged");
        assert!(
            evil.token.is_none(),
            "the lower layer's token is dropped when a higher layer names env_key"
        );
        assert_eq!(evil.env_key.as_deref(), Some("PROJECT_KEY"));
        assert!(
            !evil.mixed_layers,
            "the URL and the winning credential share a layer"
        );

        let outcomes = ModelProbes::new().probe_all(&config.providers).await;
        assert_eq!(
            outcomes.get("evil"),
            Some(&ProbeOutcome::EnvKey),
            "an env_key provider is never dialled"
        );
        assert_eq!(server.request_count(), 0, "no request reaches the project URL");
    }

    /// Issue #910: the probe cache key is the provider, its URL **and** its
    /// credentials, so a cached 401 from a stale token cannot outlive the fix
    /// for a whole TTL. The token itself never enters the key or its Debug.
    #[tokio::test]
    async fn a_changed_token_forces_a_new_probe() {
        let server = crate::test_http::TestHttpServer::start().await;
        server.route("GET", "/models", 200, r#"{"data":[{"id":"one"}]}"#);
        let probes = ModelProbes::new();
        let base = Some(server.base_url());
        let stale = provider("p", base.clone(), Some("stale-token"), None);
        let first = probes.probe_all(std::slice::from_ref(&stale)).await;
        assert_eq!(first.get("p"), Some(&ProbeOutcome::Models(vec!["one".into()])));
        assert_eq!(server.request_count(), 1);

        // Same provider and base URL, a fixed token: a new probe.
        let fixed = provider("p", base.clone(), Some("fixed-token"), None);
        let second = probes.probe_all(std::slice::from_ref(&fixed)).await;
        assert_eq!(second, first);
        assert_eq!(
            server.request_count(),
            2,
            "a changed token is a new probe, not the stale cached answer"
        );

        // The same fixed token is cached again.
        probes.probe_all(std::slice::from_ref(&fixed)).await;
        assert_eq!(server.request_count(), 2, "an unchanged token stays cached");

        // The key itself never carries the credential.
        assert_ne!(ModelProbes::cache_key(&stale), ModelProbes::cache_key(&fixed));
        let debug = format!("{:?}", ModelProbes::cache_key(&stale));
        assert!(!debug.contains("stale-token"), "token leaked: {debug}");
        let env_a = provider("p", base.clone(), None, Some("KEY_A"));
        let env_b = provider("p", base, None, Some("KEY_B"));
        assert_ne!(
            ModelProbes::cache_key(&env_a),
            ModelProbes::cache_key(&env_b),
            "the env_key identity is part of the key"
        );
    }

    /// A thread fact of the neutral `codex` bucket is not a channel: after a
    /// picker pick the caller's resolved provider is what the provider list
    /// should mark (and the pair that matches it is 「当前」).
    #[test]
    fn a_neutral_thread_provider_falls_back_to_the_selection() {
        let mut config = CodexModelConfig::default();
        config.providers.push(provider(
            "deepseek",
            Some("http://127.0.0.1:1/v1".into()),
            Some(TOKEN),
            None,
        ));
        config.profiles.push(ProfilePreset {
            name: "fast".into(),
            model: "deepseek-flash".into(),
            provider: Some("deepseek".into()),
        });
        let query = ModelSurfaceQuery {
            current: Some(ModelSelection {
                label: "deepseek/deepseek-flash".into(),
                reference: "deepseek/deepseek-flash".into(),
            }),
            ..Default::default()
        };
        let surface = build_surface(&[], &config, &HashMap::new(), Some("codex"), &query);
        assert_eq!(
            surface.current.as_ref().map(|current| current.reference.as_str()),
            Some("deepseek/deepseek-flash"),
            "a neutral thread fact does not rename the caller's selection"
        );
        let mut providers_query = query;
        providers_query.scope = Some(PROVIDER_SCOPE);
        providers_query.session_id = None;
        let providers = build_surface(&[], &config, &HashMap::new(), Some("codex"), &providers_query);
        let label = providers.scopes[0].options[0].display_label();
        assert!(label.contains("当前"), "label: {label}");

        // Issue #910: a picker pick records the neutral bucket, so the current
        // line composes the thread's **real** provider (the same fact the footer
        // shows) when that provider is configured.
        let picker_pick = ModelSurfaceQuery {
            current: Some(ModelSelection {
                label: "codex/gpt-custom@high".into(),
                reference: "codex/gpt-custom".into(),
            }),
            ..Default::default()
        };
        let surface = build_surface(&[], &config, &HashMap::new(), Some("deepseek"), &picker_pick);
        let current = surface.current.expect("the current line stays");
        assert_eq!(current.reference, "deepseek/gpt-custom");
        assert_eq!(
            current.label, "deepseek/gpt-custom@high",
            "the label is composed with the real provider too"
        );

        // A **failed config read** does not hide the thread's real provider: the
        // projection is the authority, not the config (issue #908).
        let unreadable = CodexModelConfig {
            read_failed: true,
            ..Default::default()
        };
        let surface = build_surface(&[], &unreadable, &HashMap::new(), Some("deepseek"), &picker_pick);
        assert_eq!(
            surface.current.unwrap().reference,
            "deepseek/gpt-custom",
            "the config read's failure must not leave the neutral ref"
        );

        // The projection is the authority even for a provider the config no
        // longer declares (it was renamed or removed while the thread runs).
        let surface = build_surface(&[], &config, &HashMap::new(), Some("renamed"), &picker_pick);
        assert_eq!(
            surface.current.unwrap().reference,
            "renamed/gpt-custom",
            "the footer's provider is the current line's provider"
        );

        // A neutral thread fact is not a channel: the line stays as resolved.
        let surface = build_surface(&[], &config, &HashMap::new(), Some("codex"), &picker_pick);
        assert_eq!(surface.current.unwrap().reference, "codex/gpt-custom");
    }

    /// A failed probe is a note on the model scope and a short tag on the
    /// provider's own row — never an error, a blank scope, or a model button.
    #[test]
    fn a_failed_probe_is_a_short_tag_on_the_provider_page() {
        let config = CodexModelConfig {
            providers: vec![provider(
                "opencode-go",
                Some("http://127.0.0.1:1/v1".into()),
                Some(TOKEN),
                None,
            )],
            ..Default::default()
        };
        let mut probes = HashMap::new();
        probes.insert("opencode-go".into(), ProbeOutcome::Forbidden);

        // The root page has no probe prose: only the catalog/current provider.
        let surface = build_surface(&[], &config, &probes, Some("opencode-go"), &query());
        let model = &surface.scopes[0];
        assert!(
            model
                .note
                .as_deref()
                .is_none_or(|note| !note.contains("403") && !note.contains("opencode-go")),
            "no probe prose on the model page: {:?}",
            model.note
        );
        assert!(
            !model.options.iter().any(|option| option.disabled),
            "no probe rows here either"
        );

        // The provider page lists it as a disabled row with a short tag, and
        // marks it as the current provider (#908 AC1).
        let pending_provider = ModelSurfaceQuery {
            session_id: None,
            ..provider_query()
        };
        let provider_page = build_surface(&[], &config, &probes, Some("opencode-go"), &pending_provider);
        let provider = &provider_page.scopes[0];
        assert_eq!(provider.id, PROVIDER_SCOPE);
        assert_eq!(provider.options.len(), 1, "the provider is listed");
        assert!(provider.options[0].disabled, "never a bare provider button");
        assert_eq!(provider.options[0].provider, None);
        let label = provider.options[0].display_label();
        assert!(
            label.contains("opencode-go") && label.contains("403"),
            "the row carries the short tag: {label}"
        );
        assert!(
            label.contains("当前"),
            "the thread's provider is marked even without a known model: {label}"
        );
    }

    fn provider_query() -> ModelSurfaceQuery<'static> {
        let mut query = query();
        query.scope = Some(PROVIDER_SCOPE);
        query
    }

    /// When `config/read` itself fails, an empty provider list means "unknown",
    /// not "the user configured none": the card says so instead of claiming an
    /// empty config.
    #[test]
    fn an_unreadable_config_says_unknown_instead_of_empty() {
        let config = CodexModelConfig {
            read_failed: true,
            ..Default::default()
        };
        // The root page names the failure in one line, alongside the catalog.
        let root = build_surface(&[], &config, &HashMap::new(), None, &query());
        assert_eq!(
            root.scopes[0].note.as_deref(),
            Some("配置读取失败：只显示内置目录"),
            "the model page says the provider/profile data is unknown"
        );

        let pending_provider = ModelSurfaceQuery {
            session_id: None,
            ..provider_query()
        };
        let surface = build_surface(&[], &config, &HashMap::new(), None, &pending_provider);
        let note = surface.scopes[0].note.as_deref().unwrap();
        assert!(note.contains("读不到 config"), "note: {note}");
        assert!(!note.contains("没有 provider"), "note: {note}");
    }

    /// An expired answer is re-probed **and** dropped from the cache: a drilled
    /// click's cache-only read prunes it, and a subsequent probe only replaces
    /// the requested provider's entry. Without pruning, one connection would
    /// accumulate every provider/base-URL pair a project ever named.
    #[tokio::test]
    async fn an_expired_probe_is_dropped_and_reprobed() {
        let server = crate::test_http::TestHttpServer::start().await;
        server.route("GET", "/models", 200, r#"{"data":[{"id":"one"}]}"#);
        let probes = ModelProbes::new();
        let provider = provider("p", Some(server.base_url()), None, None);
        let key = ModelProbes::cache_key(&provider);
        assert_eq!(
            probes.probe_all(std::slice::from_ref(&provider)).await.get("p"),
            Some(&ProbeOutcome::Models(vec!["one".into()]))
        );

        // `PROBE_TTL` is minutes in production; expiry is simulated by moving
        // the instants, which is the same state a later read would see. The
        // unrelated key stands for an old project's endpoint.
        let stale: ProbeCacheKey = (
            "old".to_string(),
            Some("http://old.example".to_string()),
            None,
            None,
        );
        {
            let mut entries = probes.entries.lock().unwrap();
            entries.insert(
                stale.clone(),
                ProbeEntry {
                    at: Instant::now() - PROBE_TTL - Duration::from_secs(1),
                    outcome: ProbeOutcome::Forbidden,
                },
            );
            entries.get_mut(&key).unwrap().at = Instant::now() - PROBE_TTL - Duration::from_secs(1);
        }

        // The drilled page's cache-only read prunes both expired entries and
        // returns nothing fresh.
        assert!(
            probes.cached(std::slice::from_ref(&provider)).is_empty(),
            "an expired answer is not served"
        );
        assert!(
            !probes.entries.lock().unwrap().contains_key(&stale),
            "the cache-only read drops an unrelated expired entry"
        );

        probes.probe_all(std::slice::from_ref(&provider)).await;
        assert_eq!(server.request_count(), 2, "a stale entry probes again");
        let entries = probes.entries.lock().unwrap();
        assert!(entries.contains_key(&key), "the re-probed entry is fresh");
        assert_eq!(entries.len(), 1, "nothing expired survives the insert");
    }

    /// Spec #922: a Pending Session's provider page offers the **pairs** that
    /// ride `thread/start`; a live thread's page is read-only — the current
    /// provider plus the `/new` / `/fork` note, with no rows to click.
    #[test]
    fn the_provider_page_is_selectable_only_on_a_pending_session() {
        let mut config = CodexModelConfig::default();
        config.providers.push(provider("deepseek", None, None, None));
        config.profiles.push(ProfilePreset {
            name: "fast".into(),
            model: "deepseek-flash".into(),
            provider: Some("deepseek".into()),
        });

        // A Pending Session: the pair rides creation, no suffix on the row.
        let pending_query = ModelSurfaceQuery {
            session_id: None,
            ..provider_query()
        };
        let pending = build_surface(&[], &config, &HashMap::new(), None, &pending_query);
        let scope = &pending.scopes[0];
        assert_eq!(scope.changeable, ChangeScope::NewThread);
        assert_eq!(scope.options[0].display_label(), "deepseek/deepseek-flash");
        assert_eq!(scope.intro.as_deref(), Some("只在新会话生效"));
        assert!(scope.note.is_none(), "no long copy");

        // A live thread: the current provider is stated, the rows are gone, and
        // the only remedy named is `/new` / `/fork`.
        let live = provider_query();
        let read_only = build_surface(&[], &config, &HashMap::new(), Some("deepseek"), &live);
        let scope = &read_only.scopes[0];
        assert_eq!(scope.changeable, ChangeScope::NewThread);
        assert!(scope.options.is_empty(), "no clickable rows: {:?}", scope.options);
        let note = scope.note.as_deref().expect("the read-only note");
        assert!(
            note.contains("当前 provider") && note.contains("deepseek"),
            "{note}"
        );
        assert!(note.contains("/new") && note.contains("/fork"), "{note}");
    }

    /// #908 AC1: a pair row labels the provider's configured **display name**
    /// (its id when the config names none); the callback keeps the id + model
    /// separately, so the label is presentation only.
    #[test]
    fn a_pair_row_shows_the_providers_display_name() {
        let mut config = CodexModelConfig::default();
        let mut deepseek = provider("deepseek", None, None, None);
        deepseek.name = "DeepSeek".into();
        config.providers.push(deepseek);
        config.profiles.push(ProfilePreset {
            name: "fast".into(),
            model: "deepseek-flash".into(),
            provider: Some("deepseek".into()),
        });

        let pending_query = ModelSurfaceQuery {
            session_id: None,
            ..provider_query()
        };
        let surface = build_surface(&[], &config, &HashMap::new(), None, &pending_query);
        let option = &surface.scopes[0].options[0];
        assert_eq!(option.display_label(), "DeepSeek/deepseek-flash");
        assert_eq!(
            option.provider.as_deref(),
            Some("deepseek"),
            "the callback carries the id, not the display name"
        );
        assert_eq!(
            option.value.as_deref(),
            Some("deepseek-flash"),
            "the callback carries the bare model id"
        );
    }

    /// #908: a provider id may itself contain `/`. Every reference cola
    /// composes for the current pair must quote it (issue #910's gate finding),
    /// or `parse_model` reads the wrong provider — and the current marker lands
    /// on the wrong row.
    #[test]
    fn a_slashed_provider_id_resolves_its_current_pair() {
        let mut config = CodexModelConfig::default();
        let mut slashed = provider("foo/bar", None, None, None);
        slashed.name = "Foo Bar".into();
        config.providers.push(slashed);
        config.profiles.push(ProfilePreset {
            name: "fast".into(),
            model: "gpt-x".into(),
            provider: Some("foo/bar".into()),
        });
        let query = ModelSurfaceQuery {
            current: Some(ModelSelection {
                label: "codex/gpt-x".into(),
                reference: "codex/gpt-x".into(),
            }),
            ..provider_query()
        };

        // The thread runs `foo/bar`, the pick was the neutral `codex` bucket:
        // the resolved current pair names the real provider, quoted.
        let pending = ModelSurfaceQuery {
            session_id: None,
            ..query
        };
        let surface = build_surface(&[], &config, &HashMap::new(), Some("foo/bar"), &pending);
        let option = &surface.scopes[0].options[0];
        assert_eq!(option.display_label(), "Foo Bar/gpt-x（当前）");
        assert_eq!(option.provider.as_deref(), Some("foo/bar"));
        assert_eq!(option.value.as_deref(), Some("gpt-x"));

        let current = surface.current.as_ref().expect("the resolved current pair");
        assert_eq!(
            current.reference, "\"foo/bar\"/gpt-x",
            "a slashed provider is quoted so the pair round-trips"
        );
        assert_eq!(current.label, "\"foo/bar\"/gpt-x");
        let parsed = crate::backend::parsing::parse_model(&current.reference).expect("the pair round-trips");
        assert_eq!(parsed.provider_id, "foo/bar", "not `foo`");
        assert_eq!(parsed.id, "gpt-x");
    }

    /// Issue #910: a profile preset is Codex's own model + provider declaration,
    /// so the option carries the provider it pairs with — the card forwards it,
    /// which is how a Pending Session applies both at `thread/start`.
    #[test]
    fn a_profile_preset_carries_its_paired_provider() {
        let mut config = CodexModelConfig::default();
        config.profiles.push(ProfilePreset {
            name: "fast".into(),
            model: "deepseek-flash".into(),
            provider: Some("deepseek".into()),
        });
        let surface = build_surface(&[], &config, &HashMap::new(), None, &ModelSurfaceQuery::default());
        let profile = surface.scopes[0]
            .options
            .iter()
            .find(|option| option.group.as_deref() == Some(PROFILE_GROUP))
            .expect("the preset is offered");
        assert_eq!(profile.provider.as_deref(), Some("deepseek"));

        // A preset without `model_provider` pairs with nothing.
        config.profiles.push(ProfilePreset {
            name: "plain".into(),
            model: "gpt-5.1".into(),
            provider: None,
        });
        let surface = build_surface(&[], &config, &HashMap::new(), None, &ModelSurfaceQuery::default());
        assert!(
            surface.scopes[0]
                .options
                .iter()
                .filter(|option| option.group.as_deref() == Some(PROFILE_GROUP))
                .any(|option| option.provider.is_none())
        );
    }
}
