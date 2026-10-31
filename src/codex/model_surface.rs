//! The Codex `/model` surface: what `config/read` declares and what each
//! provider's `/models` endpoint will answer (tickets #908, #931).
//!
//! `/model` is **one page, one 「模型」 scope** (#931). A provider is a config
//! fact, not a pick: Codex runs one provider per process, so switching one is a
//! `$CODEX_HOME/config.toml` edit plus a daemon restart. The page lists only the
//! current channel's models.
//!
//! `model/list` is a **flat, global** catalog compiled into the app-server: it
//! names no provider and is therefore rendered as its own group. Everything
//! else is user configuration:
//!
//! - `[model_providers.<id>]` (from `config/read`'s raw layers — the resolved
//!   `Config` struct is not the source the ticket names, and a layer carries the
//!   raw TOML) supplies the display name and the probe target for the daemon's
//!   own provider;
//! - the top-level `model` becomes the 「配置默认」 row;
//!   `[profiles.*]` is **not** read (ADR-0092, spec #922: Codex 0.134+ no longer
//!   reads it, and `--profile` layers `$CODEX_HOME/<name>.config.toml`);
//! - **Codex runs one provider per process.** The top-level `model_provider` is
//!   the provider the daemon itself is on (`openai` when unset), and a thread
//!   whose `modelProvider` differs from it is created but refuses every turn
//!   (`ModelProviderMismatch`, `config_manager.rs`). The page's callback values
//!   therefore name that one channel, and only when it is unknown (`config/read`
//!   failed, the daemon is not connected) do they fall back to the neutral
//!   `codex/<id>` bucket;
//! - `GET <base_url>/models` is asked for the current channel, using its
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
//! `thread/settings/updated` **is** consumed: the adapter's projection folds
//! its `model`/`modelProvider` into the ordered fact (`Projections::apply_at`),
//! so a server that broadcasts a peer's change moves the card without a read.
//! The broadcast is a bonus, never the only source: a message the pinned daemon
//! does not send cannot be required. `effort` is left out of that fold: the
//! surface never renders it, and the footer's `@variant` already has its own
//! ordered fact.

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

/// The single scope's identity (also the card callback's `level`).
pub(crate) const MODEL_SCOPE: &str = "model";

/// The built-in catalog's group inside the `model` scope. It is deliberately
/// **not** a provider name: `model/list` is global and a catalog entry may run
/// through any configured provider, so rendering it under one would lie.
pub(crate) const CATALOG_GROUP: &str = "内置目录（全局，未校验）";
/// The top-level `model`'s group.
pub(crate) const CONFIG_DEFAULT_GROUP: &str = "配置默认";

/// The config fact the model page states once (#931): Codex runs one provider
/// per process, so changing one is a config edit plus a daemon restart — never
/// a pick — and the page only lists that provider's models.
pub(crate) const PROVIDER_RULE_NOTE: &str = "Codex 一次只用一个 provider；改 `$CODEX_HOME/config.toml` 的 `model_provider` 并重启 daemon；这里只列当前 provider 的模型。";

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

/// The provider Codex's process is on when the config names none: the
/// app-server's own default `model_provider`.
pub(crate) const DEFAULT_MODEL_PROVIDER: &str = "openai";

/// What `config/read` declares that `model/list` cannot tell us.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct CodexModelConfig {
    /// The top-level `model`, shown as 「配置默认」.
    pub(crate) default_model: Option<String>,
    /// The top-level `model_provider`: the one provider this daemon process
    /// runs, and therefore the only one a thread it creates can take turns on.
    /// `None` when the config was read but names none — read
    /// [`Self::configured_provider`] instead of this field, which keeps the
    /// `openai` default.
    pub(crate) model_provider: Option<String>,
    pub(crate) providers: Vec<ConfiguredProvider>,
    /// `config/read` itself failed. An empty provider list is then "unknown",
    /// not "the user configured none", and the card says so.
    pub(crate) read_failed: bool,
}

impl CodexModelConfig {
    /// The provider the Codex process runs, or `None` when the config could
    /// not be read — unknown is not `openai`: a card must not claim a rule it
    /// cannot verify.
    pub(crate) fn configured_provider(&self) -> Option<&str> {
        if self.read_failed {
            None
        } else {
            Some(self.model_provider.as_deref().unwrap_or(DEFAULT_MODEL_PROVIDER))
        }
    }

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

        if let Some(layers) = result.get("layers").and_then(Value::as_array) {
            for (layer_index, layer) in layers.iter().enumerate() {
                let Some(layer_config) = layer_config(layer) else {
                    continue;
                };
                merge_layer(&mut config, &layer_config, &mut providers, layer_index);
            }
        }

        // A server that served `config` but no layers (or a version that keeps
        // `model_providers` only in the resolved object) still contributes.
        if let Some(object) = result.get("config").and_then(Value::as_object) {
            let resolved = Value::Object(object.clone());
            // The resolved object is its own (embedded) source: a URL from a
            // layer and a token from here are different provenance too.
            merge_layer(&mut config, &resolved, &mut providers, usize::MAX);
            // The resolved object is the *effective* config, so when it names
            // the top-level model it outranks a layer: a project layer the
            // daemon refused to trust is listed but not applied, and showing its
            // model as 「配置默认」 would lie.
            if object.contains_key("model") {
                config.default_model = non_empty_str(object.get("model"));
            }
            // Same rule for the process provider: it is a fact about the
            // running daemon, never a layer the daemon might have refused.
            if object.contains_key("model_provider") {
                config.model_provider = non_empty_str(object.get("model_provider"));
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
    layer_index: usize,
) {
    if config.default_model.is_none() {
        config.default_model = non_empty_str(layer.get("model"));
    }
    // The first layer to name the process provider wins, exactly like `model`.
    // A config that names none is the daemon's own `openai` default — see
    // [`CodexModelConfig::configured_provider`].
    if config.model_provider.is_none() {
        config.model_provider = non_empty_str(layer.get("model_provider"));
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
/// It is **one page, one 「模型」 scope** (#931): a provider is a config fact,
/// so there is no provider page and no drill row. Every option value names the
/// **current channel**:
///
/// - a live thread's ordered provider fact (`thread_provider`);
/// - a Pending Session's — or a cold projection's — `config/read` provider;
/// - the neutral `codex` bucket only when neither is known (`config/read`
///   failed, the daemon is not connected).
///
/// `thread_provider` outranks `query.current`'s provider because a picker pick
/// records the neutral `codex` bucket, not the channel the thread really runs
/// on.
pub(crate) fn build_surface(
    catalog: &[crate::backend::types::ProviderModels],
    config: &CodexModelConfig,
    probes: &HashMap<String, ProbeOutcome>,
    thread_provider: Option<&str>,
    query: &ModelSurfaceQuery<'_>,
) -> ModelSurface {
    // A picker pick records the neutral `codex` bucket, so a thread fact of
    // `codex` is not a channel. A thread with no projection yet ("cold") falls
    // back to the config's process provider, exactly like a Pending Session.
    let channel = match thread_provider.filter(|provider| *provider != MODEL_PROVIDER) {
        Some(provider) => Some(provider.to_string()),
        None => config.configured_provider().map(str::to_string),
    };
    ModelSurface {
        current: resolved_current(query.current.as_ref(), thread_provider),
        scopes: vec![model_page(catalog, config, probes, channel.as_deref())],
    }
}

/// The one page (#931): the flat catalog, the top-level `model` as the config
/// default, and the **current channel's** probed models. Every callback value
/// is `<channel>/<model>`; the neutral `codex` bucket is only the fallback when
/// the channel is unknown. No provider rows, pairs or drills — a provider is a
/// config fact, stated once in the note.
fn model_page(
    catalog: &[crate::backend::types::ProviderModels],
    config: &CodexModelConfig,
    probes: &HashMap<String, ProbeOutcome>,
    channel: Option<&str>,
) -> crate::backend::types::ModelScope {
    use crate::backend::types::{ChangeScope, ModelOption, ModelScope, PickKind};

    let channel = channel.unwrap_or(MODEL_PROVIDER);
    let mut options: Vec<ModelOption> = Vec::new();
    for provider in catalog {
        for model in &provider.models {
            options.push(ModelOption {
                id: model.id.clone(),
                variants: model.variants.clone(),
                value: Some(model_ref(channel, &model.id)),
                group: Some(CATALOG_GROUP.to_string()),
                ..Default::default()
            });
        }
    }
    if let Some(model) = config.default_model.as_deref() {
        options.push(ModelOption {
            id: model.to_string(),
            label: Some(format!("{model}（配置默认）")),
            value: Some(model_ref(channel, model)),
            group: Some(CONFIG_DEFAULT_GROUP.to_string()),
            ..Default::default()
        });
    }
    // The current channel's own `/models` answer: the ids it listed are the
    // only ones this daemon can run. Another provider's models are not shown.
    // The neutral `codex` bucket is not a configured channel (ADR-0092), so it
    // never matches a probe.
    if channel != MODEL_PROVIDER
        && let Some(provider) = config.providers.iter().find(|provider| provider.id == channel)
        && let Some(ProbeOutcome::Models(models)) = probes.get(&provider.id)
    {
        let group = format!("{}（/models）", provider.name);
        for model in models {
            options.push(ModelOption {
                id: model.clone(),
                value: Some(model_ref(channel, model)),
                group: Some(group.clone()),
                ..Default::default()
            });
        }
    }
    let mut notes: Vec<&str> = Vec::new();
    // One short status, never a paragraph: the config read itself failed.
    if config.read_failed {
        notes.push("_(读不到 config)_");
    }
    if options.is_empty() {
        notes.push("_(没有可用模型)_");
    }
    // The provider axis is a config fact, stated once here (spec #922's
    // correction: only a config edit + restart changes it).
    notes.push(PROVIDER_RULE_NOTE);
    ModelScope {
        id: MODEL_SCOPE.into(),
        title: "模型".into(),
        changeable: ChangeScope::ThisTurn,
        pick: PickKind::Model,
        options,
        note: Some(notes.join("　")),
        free_text: Some(format!("也可 `/model {}`", model_ref(channel, "<id>"))),
        intro: None,
    }
}

/// The callback value of one model on the model page: the current channel plus
/// the model id, quoted when the provider id needs it (a provider id may
/// contain `/`, issue #908). `codex` is the fallback channel when none is
/// known.
fn model_ref(channel: &str, model: &str) -> String {
    crate::backend::parsing::join_model(channel, model)
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

    /// A config whose process provider is `configured` — the one provider a
    /// Pending Session's page may offer (spec #922 correction).
    fn config_on(configured: &str, providers: Vec<ConfiguredProvider>) -> CodexModelConfig {
        CodexModelConfig {
            default_model: None,
            model_provider: Some(configured.into()),
            providers,
            read_failed: false,
        }
    }

    /// Layers arrive highest-precedence first (project → user → system). The
    /// first occurrence of an id/key wins and the display name falls back to
    /// the id.
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
                    }
                }
            ]
        });

        let config = CodexModelConfig::from_read(&result);

        assert_eq!(config.default_model.as_deref(), Some("project-model"));
        assert_eq!(
            config.model_provider.as_deref(),
            Some("project-provider"),
            "the process provider merges like the default model"
        );
        assert_eq!(config.configured_provider(), Some("project-provider"));
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

    /// The process provider (spec #922 correction, 2026-10-30): the top-level
    /// `model_provider`, `openai` when the config names none, and **unknown**
    /// when the read itself failed.
    #[test]
    fn the_process_provider_is_read_and_defaults_to_openai() {
        let named = CodexModelConfig::from_read(&json!({
            "config": { "model_provider": "opencode-go" }
        }));
        assert_eq!(named.configured_provider(), Some("opencode-go"));

        let unnamed = CodexModelConfig::from_read(&json!({ "config": {}, "layers": [] }));
        assert_eq!(unnamed.model_provider, None);
        assert_eq!(
            unnamed.configured_provider(),
            Some("openai"),
            "no model_provider is the app-server's own openai default"
        );

        // The resolved object is the effective config: a project layer the
        // daemon refused to trust must not become the process provider.
        let layered = CodexModelConfig::from_read(&json!({
            "config": { "model_provider": "user-provider" },
            "layers": [{
                "config": { "model_provider": "project-provider" }
            }]
        }));
        assert_eq!(layered.configured_provider(), Some("user-provider"));

        // A failed read is unknown, never a guessed `openai`.
        let unreadable = CodexModelConfig {
            read_failed: true,
            model_provider: Some("project-provider".into()),
            ..Default::default()
        };
        assert_eq!(unreadable.configured_provider(), None);
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

    /// One page, one scope (#931): no provider drill, page or pair row, even
    /// when a caller still asks for the old `provider` scope. The config fact
    /// is stated exactly once in the note.
    #[test]
    fn the_model_page_is_the_only_scope() {
        let config = config_on("deepseek", vec![provider("deepseek", None, None, None)]);
        let mut probes = HashMap::new();
        probes.insert(
            "deepseek".into(),
            ProbeOutcome::Models(vec!["deepseek-flash".into()]),
        );
        for scope in [None, Some("provider"), Some("model")] {
            let query = ModelSurfaceQuery { scope, ..query() };
            let surface = build_surface(&catalog("gpt-5"), &config, &probes, Some("deepseek"), &query);
            assert_eq!(surface.scopes.len(), 1, "one page, never a second scope");
            let page = &surface.scopes[0];
            assert_eq!(page.id, MODEL_SCOPE);
            assert_eq!(page.title, "模型");
            assert_eq!(page.pick, crate::backend::types::PickKind::Model);
            assert_eq!(page.changeable, ChangeScope::ThisTurn);
            assert!(
                page.options
                    .iter()
                    .all(|option| option.pick.is_none() && !option.disabled),
                "every row is a plain model pick: {:?}",
                page.options
            );
            let note = page.note.as_deref().expect("the config-fact note");
            assert_eq!(
                note.matches(PROVIDER_RULE_NOTE).count(),
                1,
                "the config fact appears once: {note}"
            );
            assert!(note.contains("model_provider") && note.contains("重启 daemon"));
            assert_eq!(
                page.free_text.as_deref(),
                Some("也可 `/model deepseek/<id>`"),
                "the hand-type hint names the current channel"
            );
        }
    }

    /// A live thread's callback values name the **thread provider**, even when
    /// the config's process provider is another one (#931 AC2).
    #[test]
    fn a_live_threads_options_name_the_thread_provider() {
        let mut config = config_on(
            "opencode-go",
            vec![
                provider("deepseek", None, None, None),
                provider("opencode-go", None, None, None),
            ],
        );
        config.default_model = Some("gpt-5-default".into());
        let mut probes = HashMap::new();
        probes.insert(
            "deepseek".into(),
            ProbeOutcome::Models(vec!["deepseek-flash".into()]),
        );
        probes.insert(
            "opencode-go".into(),
            ProbeOutcome::Models(vec!["gpt-6-luna".into()]),
        );
        let surface = build_surface(&catalog("gpt-5"), &config, &probes, Some("deepseek"), &query());
        let page = &surface.scopes[0];
        let values: Vec<&str> = page
            .options
            .iter()
            .map(|option| option.callback_value())
            .collect();
        assert_eq!(
            values,
            [
                "deepseek/gpt-5",
                "deepseek/gpt-5-default",
                "deepseek/deepseek-flash",
            ],
            "catalog, config default and the thread provider's probed models"
        );
        assert!(
            !values.iter().any(|value| value.contains("gpt-6-luna")),
            "another provider's models are not listed: {values:?}"
        );
    }

    /// A Pending Session has no thread fact: its callback values name the
    /// **config** provider (#931 AC2).
    #[test]
    fn a_pending_sessions_options_name_the_config_provider() {
        let mut config = config_on("deepseek", vec![provider("deepseek", None, None, None)]);
        config.default_model = Some("deepseek-v4-pro".into());
        let mut probes = HashMap::new();
        probes.insert(
            "deepseek".into(),
            ProbeOutcome::Models(vec!["deepseek-flash".into()]),
        );
        let pending = ModelSurfaceQuery {
            session_id: None,
            ..ModelSurfaceQuery::default()
        };
        let surface = build_surface(&catalog("gpt-5"), &config, &probes, None, &pending);
        let values: Vec<&str> = surface.scopes[0]
            .options
            .iter()
            .map(|option| option.callback_value())
            .collect();
        assert_eq!(
            values,
            [
                "deepseek/gpt-5",
                "deepseek/deepseek-v4-pro",
                "deepseek/deepseek-flash",
            ]
        );
        assert_eq!(
            surface.scopes[0].free_text.as_deref(),
            Some("也可 `/model deepseek/<id>`")
        );
    }

    /// A live thread whose model projection is still cold has no thread fact
    /// either: the config provider is the honest channel (#931 AC2).
    #[test]
    fn a_cold_projection_uses_the_config_provider() {
        let config = config_on("deepseek", vec![provider("deepseek", None, None, None)]);
        let surface = build_surface(&catalog("gpt-5"), &config, &HashMap::new(), None, &query());
        assert_eq!(surface.scopes[0].options[0].callback_value(), "deepseek/gpt-5");
    }

    /// No channel at all (`config/read` failed and no thread fact) falls back to
    /// the neutral `codex` bucket and still says the config was unreadable
    /// (#931 AC2).
    #[test]
    fn an_unknown_channel_falls_back_to_the_neutral_bucket() {
        let mut unreadable = CodexModelConfig {
            read_failed: true,
            ..Default::default()
        };
        unreadable.default_model = Some("gpt-5-default".into());
        let surface = build_surface(&catalog("gpt-5"), &unreadable, &HashMap::new(), None, &query());
        let page = &surface.scopes[0];
        let values: Vec<&str> = page
            .options
            .iter()
            .map(|option| option.callback_value())
            .collect();
        assert_eq!(values, ["codex/gpt-5", "codex/gpt-5-default"]);
        assert_eq!(
            page.free_text.as_deref(),
            Some("也可 `/model codex/<id>`"),
            "the hand-type hint names the neutral fallback"
        );
        let note = page.note.as_deref().unwrap();
        assert!(note.contains("读不到 config"), "note: {note}");

        // The thread fact outranks the failed config read: the daemon's own
        // projection is the authority (issue #908).
        let surface = build_surface(
            &catalog("gpt-5"),
            &unreadable,
            &HashMap::new(),
            Some("deepseek"),
            &query(),
        );
        assert_eq!(
            surface.scopes[0].options[0].callback_value(),
            "deepseek/gpt-5",
            "a known thread provider survives a failed config read"
        );
        assert!(
            surface.scopes[0]
                .note
                .as_deref()
                .unwrap()
                .contains("读不到 config")
        );

        // An empty catalog leaves the note's other half (and never a blank
        // card).
        let empty_config = CodexModelConfig {
            read_failed: true,
            ..Default::default()
        };
        let empty = build_surface(&[], &empty_config, &HashMap::new(), None, &query());
        let note = empty.scopes[0].note.as_deref().unwrap();
        assert!(note.contains("读不到 config"), "note: {note}");
        assert!(note.contains("没有可用模型"), "note: {note}");
        assert!(empty.scopes[0].free_text.is_some());
    }

    /// Only the current channel's `/models` group is listed; no other provider
    /// appears, and a probed row is a plain model pick (no `provider` field).
    #[test]
    fn only_the_current_providers_models_are_listed() {
        let mut deepseek = provider("deepseek", None, None, None);
        deepseek.name = "DeepSeek".into();
        let mut opencode = provider("opencode-go", None, None, None);
        opencode.name = "OpenCode Go".into();
        let config = config_on("deepseek", vec![deepseek, opencode]);
        let mut probes = HashMap::new();
        probes.insert(
            "deepseek".into(),
            ProbeOutcome::Models(vec!["deepseek-flash".into()]),
        );
        probes.insert(
            "opencode-go".into(),
            ProbeOutcome::Models(vec!["gpt-6-luna".into()]),
        );
        let surface = build_surface(&[], &config, &probes, Some("deepseek"), &query());
        let options = &surface.scopes[0].options;
        assert_eq!(
            options.len(),
            1,
            "only the current provider's models: {options:?}"
        );
        assert_eq!(
            options[0].group.as_deref(),
            Some("DeepSeek（/models）"),
            "the group carries the configured display name"
        );
        assert_eq!(options[0].callback_value(), "deepseek/deepseek-flash");
        assert!(!options[0].disabled);
    }

    /// A thread fact of the neutral `codex` bucket is not a channel: the
    /// current-model line falls back to the caller's resolved provider, and a
    /// picker pick composes the thread's real provider (the same fact the
    /// footer shows).
    #[test]
    fn a_neutral_thread_provider_does_not_rename_the_current_line() {
        let config = config_on("deepseek", vec![provider("deepseek", None, None, None)]);
        let picker_pick = ModelSurfaceQuery {
            current: Some(ModelSelection {
                label: "codex/gpt-custom@high".into(),
                reference: "codex/gpt-custom".into(),
            }),
            ..Default::default()
        };

        // A picker pick records the neutral bucket, so the current line
        // composes the thread's **real** provider.
        let surface = build_surface(&[], &config, &HashMap::new(), Some("deepseek"), &picker_pick);
        let current = surface.current.expect("the current line stays");
        assert_eq!(current.reference, "deepseek/gpt-custom");
        assert_eq!(
            current.label, "deepseek/gpt-custom@high",
            "the label is composed with the real provider too"
        );

        // A failed config read does not hide the thread's real provider: the
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

    /// A failed probe adds no row and no prose: the model page has no provider
    /// status rows any more (#931 AC1).
    #[test]
    fn a_failed_probe_adds_nothing_to_the_model_page() {
        let config = config_on("opencode-go", vec![provider("opencode-go", None, None, None)]);
        let mut probes = HashMap::new();
        probes.insert("opencode-go".into(), ProbeOutcome::Forbidden);
        let surface = build_surface(&[], &config, &probes, Some("opencode-go"), &query());
        let page = &surface.scopes[0];
        assert!(
            page.options.is_empty(),
            "no row for a failed probe: {:?}",
            page.options
        );
        let note = page.note.as_deref().unwrap();
        assert!(!note.contains("无权列出模型"), "no probe prose: {note}");
        assert!(!note.contains("opencode-go"), "no provider row: {note}");
        assert!(note.contains("没有可用模型"), "the page says so: {note}");
    }

    /// #908: a provider id may itself contain `/`. Every ref built from it is
    /// quoted so it round-trips, including the model page's callback values.
    #[test]
    fn a_slashed_provider_id_round_trips_on_the_model_page() {
        let mut slashed = provider("foo/bar", None, None, None);
        slashed.name = "Foo Bar".into();
        let config = config_on("foo/bar", vec![slashed]);
        let mut probes = HashMap::new();
        probes.insert("foo/bar".into(), ProbeOutcome::Models(vec!["gpt-x".into()]));
        let query = ModelSurfaceQuery {
            current: Some(ModelSelection {
                label: "codex/gpt-x".into(),
                reference: "codex/gpt-x".into(),
            }),
            ..query()
        };
        let surface = build_surface(&[], &config, &probes, Some("foo/bar"), &query);
        let option = &surface.scopes[0].options[0];
        assert_eq!(option.callback_value(), "\"foo/bar\"/gpt-x");
        assert_eq!(
            surface.scopes[0].free_text.as_deref(),
            Some("也可 `/model \"foo/bar\"/<id>`")
        );

        let current = surface.current.as_ref().expect("the resolved current pair");
        assert_eq!(current.reference, "\"foo/bar\"/gpt-x");
        assert_eq!(current.label, "\"foo/bar\"/gpt-x");
        let parsed = crate::backend::parsing::parse_model(&current.reference).expect("the pair round-trips");
        assert_eq!(parsed.provider_id, "foo/bar", "not `foo`");
        assert_eq!(parsed.id, "gpt-x");
    }

    /// Spec #922 decision 7: `[profiles.*]` is not a preset source. A layer
    /// carrying one contributes no model row — the table is simply ignored.
    #[test]
    fn a_profiles_table_is_ignored() {
        let result = json!({
            "config": {},
            "layers": [{
                "config": {
                    "model": "top-level-model",
                    "model_provider": "deepseek",
                    "model_providers": {
                        "deepseek": { "name": "DeepSeek", "base_url": "http://127.0.0.1:1/v1" }
                    },
                    "profiles": {
                        "fast": { "model": "deepseek-flash", "model_provider": "deepseek" }
                    }
                }
            }]
        });
        let config = CodexModelConfig::from_read(&result);
        assert_eq!(config.default_model.as_deref(), Some("top-level-model"));
        assert_eq!(config.providers.len(), 1);

        let surface = build_surface(&[], &config, &HashMap::new(), None, &ModelSurfaceQuery::default());
        let page = &surface.scopes[0];
        let labels: Vec<&str> = page.options.iter().map(|option| option.display_label()).collect();
        assert!(
            labels.iter().any(|label| label.contains("top-level-model")),
            "{labels:?}"
        );
        assert!(
            !labels
                .iter()
                .any(|label| label.contains("fast") || label.contains("deepseek-flash")),
            "a profile contributes no model row: {labels:?}"
        );
        assert_eq!(
            page.options[0].callback_value(),
            "deepseek/top-level-model",
            "the config default names the config provider"
        );
    }

    /// The built-in catalog plus a [`ProviderModels`] row per passed id.
    fn catalog(model: &str) -> Vec<crate::backend::types::ProviderModels> {
        vec![crate::backend::types::ProviderModels {
            provider: MODEL_PROVIDER.into(),
            models: vec![crate::backend::types::ModelOption {
                id: model.into(),
                ..Default::default()
            }],
        }]
    }
}
