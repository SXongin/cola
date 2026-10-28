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
//!   failures) are cached per provider for [`PROBE_TTL`], so opening the card
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

use crate::backend::types::{ChangeScope, ModelOption, ModelScope, ModelSurface, ModelSurfaceQuery};

use super::error::Result as CodexResult;
use super::transport::RpcConnection;

/// How long one provider's `/models` answer (success or failure) stays fresh.
/// The picker is re-opened by hand, so a few minutes is enough to make a
/// double-open cheap without hiding a provider that just recovered.
const PROBE_TTL: Duration = Duration::from_secs(180);

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
pub(crate) const CATALOG_GROUP: &str = "内置目录（任何 provider 可试）";
/// The top-level `model`'s group.
pub(crate) const CONFIG_DEFAULT_GROUP: &str = "配置默认";
/// Every `[profiles.<name>]` preset's group.
pub(crate) const PROFILE_GROUP: &str = "Profile 预设";

/// How long a note may be before it is clipped. Notes are rendered verbatim by
/// the card, so the surface owns their one-line budget (the ticket's "one short
/// line" rule).
const NOTE_MAX_CHARS: usize = 64;

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
            for layer in layers {
                let Some(layer_config) = layer_config(layer) else {
                    continue;
                };
                merge_layer(&mut config, &layer_config, &mut providers, &mut profiles);
            }
        }

        // A server that served `config` but no layers (or a version that keeps
        // `model_providers` only in the resolved object) still contributes.
        if let Some(object) = result.get("config").and_then(Value::as_object) {
            let resolved = Value::Object(object.clone());
            merge_layer(&mut config, &resolved, &mut providers, &mut profiles);
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
            .map(|(id, parts)| ConfiguredProvider {
                name: parts.name.unwrap_or_else(|| id.clone()),
                id,
                base_url: parts.base_url,
                token: parts.token,
                env_key: parts.env_key,
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
            set_first(&mut parts.base_url, field("base_url"));
            set_first(&mut parts.token, field("experimental_bearer_token"));
            set_first(&mut parts.env_key, field("env_key"));
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
    /// The one-line reason rendered on the card. No URL, header or token ever
    /// reaches this text.
    pub(crate) fn reason(&self) -> String {
        match self {
            Self::Models(_) => String::new(),
            Self::EnvKey => "用 env_key 取凭据，cola 读不到 daemon 环境，跳过探测".into(),
            Self::NoBaseUrl => "没有 base_url，无法探测".into(),
            Self::BadBaseUrl => "base_url 无效，无法探测".into(),
            Self::UnsupportedScheme => "base_url 不是 http(s)，跳过探测".into(),
            Self::Unauthorized => "需要认证（401）".into(),
            Self::Forbidden => "拒绝访问 /models（403）".into(),
            Self::NotFound => "没有 /models（404）".into(),
            Self::Timeout => "探测超时".into(),
            Self::Network => "网络不可达".into(),
            Self::BadResponse => "响应无法解析".into(),
            Self::Http(status) => format!("返回 HTTP {status}"),
            Self::Empty => "未列出任何模型".into(),
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

/// Drop every entry past [`PROBE_TTL`]. Called under the cache lock before a
/// lookup and on insert, so expired provider/base-URL pairs are removed rather
/// than merely ignored — visiting projects with distinct endpoints cannot grow
/// one connection's map without bound.
fn prune_expired(entries: &mut HashMap<(String, Option<String>), ProbeEntry>) {
    let now = Instant::now();
    entries.retain(|_, entry| now.saturating_duration_since(entry.at) < PROBE_TTL);
}

/// Per-provider `/models` cache (one per attached connection). Clones of a
/// [`super::client::CodexClient`] share it through an `Arc`.
pub(crate) struct ModelProbes {
    entries: Mutex<HashMap<(String, Option<String>), ProbeEntry>>,
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

    /// The cache key is the provider **and** its base URL: one provider id may
    /// be overridden per project layer, and a cached answer for another
    /// project's endpoint would be a lie about this one.
    fn cache_key(provider: &ConfiguredProvider) -> (String, Option<String>) {
        (provider.id.clone(), provider.base_url.clone())
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
/// `provider_switch_on_resume` is the client's `[clients.<id>]` gate (issue
/// #910): it decides whether an existing thread's provider scope says
/// 「需确认」 (`thread/resume`, confirmation first) or 「仅新线程」. A Pending
/// Session has no thread to resume, so its pick is always a new-thread one.
pub(crate) fn build_surface(
    catalog: &[crate::backend::types::ProviderModels],
    config: &CodexModelConfig,
    probes: &HashMap<String, ProbeOutcome>,
    thread_provider: Option<&str>,
    provider_switch_on_resume: bool,
    query: &ModelSurfaceQuery<'_>,
) -> ModelSurface {
    let query_provider = query
        .current
        .as_ref()
        .and_then(|current| current.reference.split_once('/'))
        .map(|(provider, _)| provider.to_string())
        .filter(|provider| !provider.is_empty());
    // A picker pick records the neutral `codex` bucket, so a thread fact of
    // `codex` is not a channel: the caller's resolved provider is the honest
    // marker then. A real thread provider outranks it.
    let current_provider = match thread_provider {
        Some(provider) if provider != MODEL_PROVIDER => Some(provider.to_string()),
        _ => query_provider,
    };

    let mut options: Vec<ModelOption> = Vec::new();
    for provider in catalog {
        for model in &provider.models {
            options.push(ModelOption {
                id: model.id.clone(),
                variants: model.variants.clone(),
                value: Some(codex_ref(&model.id)),
                group: Some(CATALOG_GROUP.to_string()),
                ..ModelOption::default()
            });
        }
    }
    if let Some(model) = config.default_model.as_deref() {
        options.push(ModelOption {
            id: model.to_string(),
            label: Some(format!("{model}（配置默认）")),
            value: Some(codex_ref(model)),
            group: Some(CONFIG_DEFAULT_GROUP.to_string()),
            ..ModelOption::default()
        });
    }
    for profile in &config.profiles {
        let mut label = format!("{}：{}", profile.name, profile.model);
        if let Some(provider) = profile.provider.as_deref() {
            label.push_str(&format!("（{provider}）"));
        }
        // A profile is Codex's own "model + provider" declaration: the pick is a
        // **pair** (issue #910). The provider rides the callback's `provider`
        // field and the value stays the bare model id, so the bridge composes
        // `provider/model` and the path is decided by whether the provider
        // changes. A profile without `model_provider` is model-only.
        let paired = profile.provider.is_some();
        options.push(ModelOption {
            id: profile.model.clone(),
            label: Some(clip(&label, 48)),
            value: Some(if paired {
                profile.model.clone()
            } else {
                codex_ref(&profile.model)
            }),
            group: Some(PROFILE_GROUP.to_string()),
            provider: profile.provider.clone(),
            ..ModelOption::default()
        });
    }
    for provider in &config.providers {
        let Some(ProbeOutcome::Models(models)) = probes.get(&provider.id) else {
            continue;
        };
        let group = format!("{}（/models）", provider.name);
        for model in models {
            // A probed model was listed **by that provider**, so it is a pair:
            // the model is known to exist there (issue #910).
            options.push(ModelOption {
                id: model.clone(),
                value: Some(model.clone()),
                provider: Some(provider.id.clone()),
                group: Some(group.clone()),
                ..ModelOption::default()
            });
        }
    }

    let note =
        probe_note(config, probes).or_else(|| options.is_empty().then(|| "_(没有可用模型)_".to_string()));

    let model_scope = ModelScope {
        id: MODEL_SCOPE.into(),
        title: "模型（每轮可换）".into(),
        changeable: ChangeScope::ThisTurn,
        pick: crate::backend::types::PickKind::Model,
        options,
        note,
        free_text: Some(format!(
            "💡 也可以直接发送 `/model {MODEL_PROVIDER}/<id>` 选择目录之外的模型。"
        )),
        intro: None,
    };

    // A Pending Session has no thread to resume: its pick is applied when the
    // thread is created. A live thread can only take a change through
    // `thread/resume`, and only when the client opted in — the card says which.
    let pending = query.session_id.is_none();
    let resumable = !pending && provider_switch_on_resume;
    let current_model = query
        .current
        .as_ref()
        .and_then(|current| current.reference.split_once('/'))
        .map(|(_, model)| model.to_string());

    // A provider is only half a model: the old model name sent to a new provider
    // is a 404 (issue #910), so **every provider row is a pair**. The known
    // pairs are Codex's own declarations — `[profiles.*]` (`model` +
    // `model_provider`) and the models a provider's `/models` listed. A provider
    // with neither has no valid pick here and is named in the note instead.
    let mut provider_options: Vec<ModelOption> = Vec::new();
    let mut paired_providers: std::collections::HashSet<&str> = std::collections::HashSet::new();
    for provider in &config.providers {
        let mut models: Vec<(String, Option<String>)> = Vec::new();
        for profile in config
            .profiles
            .iter()
            .filter(|profile| profile.provider.as_deref() == Some(provider.id.as_str()))
        {
            models.push((profile.model.clone(), Some(profile.name.clone())));
        }
        if let Some(ProbeOutcome::Models(probed)) = probes.get(&provider.id) {
            for model in probed {
                models.push((model.clone(), None));
            }
        }
        let mut seen = std::collections::HashSet::new();
        models.retain(|(model, _)| seen.insert(model.clone()));
        if models.is_empty() {
            continue;
        }
        paired_providers.insert(provider.id.as_str());
        for (model, profile) in models {
            let same_provider = current_provider.as_deref() == Some(provider.id.as_str());
            let same_pair = same_provider && current_model.as_deref() == Some(model.as_str());
            let mut label = match profile.as_deref() {
                Some(name) => format!("{}：{model}（{name}）", provider.name),
                None => format!("{}：{model}", provider.name),
            };
            if same_pair {
                label.push_str("（当前）");
            }
            // The annotation says what a pick of **this** row does: a row on the
            // thread's own provider is an ordinary model pick, only a different
            // provider needs a confirmation (or a new thread).
            let suffix = if pending {
                "（新线程生效）"
            } else if same_provider {
                ""
            } else if resumable {
                "（需确认）"
            } else {
                "（仅新线程）"
            };
            let room = 48usize.saturating_sub(suffix.chars().count());
            provider_options.push(ModelOption {
                id: format!("{}/{model}", provider.id),
                label: Some(format!("{}{suffix}", clip(&label, room))),
                value: Some(model),
                provider: Some(provider.id.clone()),
                ..ModelOption::default()
            });
        }
    }
    let unpaired: Vec<&str> = config
        .providers
        .iter()
        .filter(|provider| !paired_providers.contains(provider.id.as_str()))
        .map(|provider| provider.name.as_str())
        .collect();
    let mut note = if provider_options.is_empty() {
        if config.read_failed {
            "_(读不到 config，provider / profile 未知)_".to_string()
        } else {
            "_(config 里没有可切换的 provider + model 组合：可在 `[profiles.*]` 声明一对)_".to_string()
        }
    } else if pending {
        "选中的 provider+model 会随第一条消息应用到新线程（`thread/start`）。".to_string()
    } else if resumable {
        "切换会释放并重新加载这条线程：**其他客户端（TUI / VS Code / 桌面端）或另一个共用同一个 \
         `CODEX_HOME` 的 cola 实例也会看到新的 provider**，所以需要确认；模型一起换。"
            .to_string()
    } else {
        "provider 只能在新线程切换；想换渠道请先 `/new`（或在 `[clients.<id>]` 打开 \
         `provider_switch_on_resume`）。"
            .to_string()
    };
    if !unpaired.is_empty() {
        note.push_str(&format!(
            " {} 没有已知模型：请在 `[profiles.*]` 里声明 model + model_provider，或让 `/models` 探测成功。",
            unpaired.join("、")
        ));
    }
    let provider_scope = ModelScope {
        id: PROVIDER_SCOPE.into(),
        title: "Provider（线程级：新线程 / resume）".into(),
        changeable: if resumable {
            ChangeScope::ResumeOnly
        } else {
            ChangeScope::NewThread
        },
        // A thread-level provider **pair**, never a drill-down: the bridge routes
        // it to its switch path (issue #910), where OpenCode's same-named scope
        // opens another surface page.
        pick: crate::backend::types::PickKind::Provider,
        note: Some(note),
        options: provider_options,
        free_text: None,
        intro: None,
    };

    ModelSurface {
        current: query.current.clone(),
        scopes: vec![model_scope, provider_scope],
    }
}

/// The one-line probe note: why a provider contributes no model buttons. One
/// provider's reason fits; many are summarised and clipped, never wrapped.
fn probe_note(config: &CodexModelConfig, probes: &HashMap<String, ProbeOutcome>) -> Option<String> {
    let reasons: Vec<String> = config
        .providers
        .iter()
        .filter_map(|provider| match probes.get(&provider.id) {
            None | Some(ProbeOutcome::Models(_)) => None,
            Some(outcome) => Some(format!("{}：{}", provider.name, outcome.reason())),
        })
        .collect();
    match reasons.as_slice() {
        [] => None,
        [only] => Some(clip(&format!("⚠️ {only}"), NOTE_MAX_CHARS)),
        many => Some(clip(
            &format!(
                "⚠️ {} 个 provider 的 /models 不可用：{}",
                many.len(),
                many.join("；")
            ),
            NOTE_MAX_CHARS,
        )),
    }
}

fn codex_ref(model: &str) -> String {
    format!("{MODEL_PROVIDER}/{model}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::types::{ModelOption, ModelSelection, ProviderModels};

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

    /// A root read for a live thread: the provider scope is a resume then.
    fn live_query() -> ModelSurfaceQuery<'static> {
        query()
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

    /// The surface's shape: the built-in catalog is its own group (never under a
    /// provider), config profiles are `codex/<model>` presets, probed models
    /// group by provider, and the provider list marks the thread's provider.
    #[test]
    fn surface_groups_catalog_presets_and_probed_models() {
        let catalog = vec![ProviderModels {
            provider: "codex".into(),
            models: vec![
                ModelOption {
                    id: "gpt-5.1-codex".into(),
                    variants: vec!["low".into(), "high".into()],
                    ..Default::default()
                },
                ModelOption {
                    id: "gpt-6-luna".into(),
                    ..Default::default()
                },
            ],
        }];
        let mut config = CodexModelConfig {
            default_model: Some("gpt-6-luna".into()),
            ..Default::default()
        };
        config.providers = vec![
            ConfiguredProvider {
                id: "deepseek".into(),
                name: "DeepSeek".into(),
                base_url: Some("http://127.0.0.1:9/v1".into()),
                token: Some(TOKEN.into()),
                env_key: None,
            },
            ConfiguredProvider {
                id: "opencode-go".into(),
                name: "OpenCode Go".into(),
                base_url: Some("http://127.0.0.1:10/v1".into()),
                token: None,
                env_key: Some("OPENCODE_KEY".into()),
            },
        ];
        config.profiles = vec![ProfilePreset {
            name: "fast".into(),
            model: "deepseek-flash".into(),
            provider: Some("deepseek".into()),
        }];
        let mut probes = HashMap::new();
        probes.insert(
            "deepseek".into(),
            ProbeOutcome::Models(vec!["deepseek-flash".into(), "deepseek-v4-pro".into()]),
        );
        probes.insert("opencode-go".into(), ProbeOutcome::EnvKey);

        let surface = build_surface(&catalog, &config, &probes, Some("opencode-go"), false, &query());

        assert_eq!(surface.scopes.len(), 2);
        let model = &surface.scopes[0];
        assert_eq!(model.options[0].id, "gpt-5.1-codex");
        assert_eq!(model.options[0].group.as_deref(), Some(CATALOG_GROUP));
        assert_eq!(model.options[0].callback_value(), "codex/gpt-5.1-codex");
        assert_eq!(model.options[0].variants, ["low", "high"]);

        let config_default = model
            .options
            .iter()
            .find(|option| option.group.as_deref() == Some(CONFIG_DEFAULT_GROUP))
            .expect("the config default is offered");
        assert_eq!(config_default.callback_value(), "codex/gpt-6-luna");
        assert!(config_default.display_label().contains("配置默认"));

        let profile = model
            .options
            .iter()
            .find(|option| option.group.as_deref() == Some(PROFILE_GROUP))
            .expect("the profile is offered");
        assert_eq!(
            profile.callback_value(),
            "deepseek-flash",
            "a profile pick is a **pair**: the bare model id plus its provider"
        );
        assert_eq!(profile.provider.as_deref(), Some("deepseek"));
        assert!(profile.display_label().contains("fast"));
        assert!(profile.display_label().contains("deepseek"));

        let probed: Vec<&ModelOption> = model
            .options
            .iter()
            .filter(|option| option.group.as_deref() == Some("DeepSeek（/models）"))
            .collect();
        assert_eq!(probed.len(), 2);
        assert_eq!(probed[0].callback_value(), "deepseek-flash");
        assert_eq!(
            probed[0].provider.as_deref(),
            Some("deepseek"),
            "a probed model belongs to the provider that listed it"
        );

        // The catalog entry is in no provider's group.
        for option in model.options.iter().filter(|option| {
            matches!(option.id.as_str(), "gpt-5.1-codex" | "gpt-6-luna")
                && option.group.as_deref() != Some(CONFIG_DEFAULT_GROUP)
        }) {
            assert_eq!(option.group.as_deref(), Some(CATALOG_GROUP));
        }

        // The env_key provider contributes a note, no model buttons.
        let note = model.note.as_deref().expect("the skipped provider has a note");
        assert!(note.contains("env_key"), "note: {note}");
        assert!(!note.contains('\n'), "the note stays one line: {note}");

        // The provider scope lists **pairs** only (issue #910): the profile's
        // pair and the two probed ones. `opencode-go` has no known model (its
        // probe was skipped), so it has no row and is named in the note.
        let provider_scope = &surface.scopes[1];
        assert_eq!(provider_scope.id, PROVIDER_SCOPE);
        assert_eq!(provider_scope.changeable, ChangeScope::NewThread);
        assert_eq!(provider_scope.pick, crate::backend::types::PickKind::Provider);
        let pairs: Vec<(&str, &str)> = provider_scope
            .options
            .iter()
            .map(|option| {
                (
                    option.provider.as_deref().unwrap_or_default(),
                    option.callback_value(),
                )
            })
            .collect();
        assert_eq!(
            pairs,
            [("deepseek", "deepseek-flash"), ("deepseek", "deepseek-v4-pro")],
            "one row per known provider+model pair, deduplicated"
        );
        assert!(
            provider_scope.options[0].display_label().contains("fast"),
            "a profile pair names the profile: {:?}",
            provider_scope.options[0].label
        );
        let note = provider_scope.note.as_deref().unwrap();
        assert!(
            note.contains("OpenCode Go"),
            "a provider with no known model is named in the note: {note}"
        );
        assert!(
            !provider_scope
                .options
                .iter()
                .any(|option| option.id.contains("opencode-go")),
            "a bare provider is not selectable"
        );

        // The token never reaches any surface field, a card, or a `{:?}`.
        assert!(
            !format!("{config:?}").contains(TOKEN),
            "the token leaked into a config debug: {config:?}"
        );
        assert!(
            !format!("{:?}", config.providers[0]).contains(TOKEN),
            "the token leaked into a provider debug"
        );
        assert!(
            format!("{:?}", config.providers[0]).contains("<redacted>"),
            "the provider debug redacts the token"
        );
        let rendered = format!(
            "{:?}{:?}",
            surface
                .scopes
                .iter()
                .flat_map(|scope| scope.options.iter())
                .collect::<Vec<_>>(),
            surface
                .scopes
                .iter()
                .map(|scope| (&scope.note, &scope.free_text, &scope.intro))
                .collect::<Vec<_>>()
        );
        assert!(
            !rendered.contains(TOKEN),
            "the token leaked into the surface: {rendered}"
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
        let surface = build_surface(&[], &config, &HashMap::new(), Some("codex"), false, &query);
        let label = surface.scopes[1].options[0].display_label();
        assert!(label.contains("当前"), "label: {label}");
        assert!(
            !label.contains("需确认"),
            "a pair on the thread's own provider is an ordinary model pick: {label}"
        );
    }

    /// A failed probe is a note on the model scope and a short tag on the
    /// provider's own row — never an error, a blank scope, or a model button.
    #[test]
    fn a_failed_probe_surfaces_a_one_line_note() {
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

        let surface = build_surface(&[], &config, &probes, Some("opencode-go"), false, &query());
        let model = &surface.scopes[0];
        assert!(model.options.is_empty(), "no provider models on a 403");
        let note = model.note.as_deref().unwrap();
        assert!(note.contains("403"), "note: {note}");
        assert!(note.contains("opencode-go"), "note names the provider: {note}");
        assert!(!note.contains('\n'));

        // The provider scope offers only pairs; with no profile and a failed
        // probe this provider has none, so it is named in the note instead of
        // being selectable as a bare provider (issue #910, item 2).
        let provider = &surface.scopes[1];
        assert!(provider.options.is_empty(), "no bare provider row");
        let note = provider.note.as_deref().unwrap();
        assert!(note.contains("opencode-go"), "note names it: {note}");
        assert!(note.contains("profiles"), "note says how to pair it: {note}");
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
        let surface = build_surface(
            &[],
            &config,
            &HashMap::new(),
            None,
            false,
            &ModelSurfaceQuery::default(),
        );
        let provider = &surface.scopes[1];
        let note = provider.note.as_deref().unwrap();
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
        let stale = ("old".to_string(), Some("http://old.example".to_string()));
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

    /// Issue #910, items 2 and 3: the provider scope offers **pairs** only, and
    /// each row says what a pick of that row does. A row on the thread's own
    /// provider is an ordinary model pick (no confirmation); a different
    /// provider is a new-thread pick or a confirmed resume, depending on the
    /// gate.
    #[test]
    fn the_provider_scope_offers_pairs_and_says_where_a_pick_takes_effect() {
        let mut config = CodexModelConfig::default();
        config.providers.push(provider("deepseek", None, None, None));
        config.profiles.push(ProfilePreset {
            name: "fast".into(),
            model: "deepseek-flash".into(),
            provider: Some("deepseek".into()),
        });

        // A Pending Session applies the pair when the thread is created.
        let pending = build_surface(
            &[],
            &config,
            &HashMap::new(),
            None,
            true,
            &ModelSurfaceQuery::default(),
        );
        let scope = &pending.scopes[1];
        assert_eq!(scope.changeable, ChangeScope::NewThread, "nothing to resume");
        let label = scope.options[0].display_label();
        assert!(label.contains("新线程生效"), "{label}");
        assert!(
            scope.note.as_deref().unwrap().contains("thread/start"),
            "the note names the creation call: {:?}",
            scope.note
        );

        // A live thread whose provider differs: a confirmed resume when the
        // gate is open.
        let live = live_query();
        let resumable = build_surface(&[], &config, &HashMap::new(), None, true, &live);
        let scope = &resumable.scopes[1];
        assert_eq!(scope.changeable, ChangeScope::ResumeOnly);
        assert!(
            scope.options[0].display_label().contains("需确认"),
            "{:?}",
            scope.options[0].label
        );
        let note = scope.note.as_deref().unwrap();
        assert!(
            note.contains("其他客户端") && note.contains("CODEX_HOME"),
            "both holder kinds are spelled out (ADR-0090): {note}"
        );

        // A live thread with the gate shut: only a new thread can take it.
        let locked = build_surface(&[], &config, &HashMap::new(), None, false, &live);
        let scope = &locked.scopes[1];
        assert_eq!(scope.changeable, ChangeScope::NewThread, "the gate is shut");
        assert!(
            scope.options[0].display_label().contains("仅新线程"),
            "{:?}",
            scope.options[0].label
        );
        assert!(scope.note.as_deref().unwrap().contains("只能在新线程切换"));

        // The same pair on the thread's own provider is an ordinary model
        // pick: no confirmation annotation at all (issue #910, item 3).
        let same = build_surface(&[], &config, &HashMap::new(), Some("deepseek"), true, &live);
        let label = same.scopes[1].options[0].display_label();
        assert!(
            !label.contains("需确认") && !label.contains("仅新线程"),
            "the thread's own provider needs no thread action: {label}"
        );
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
        let surface = build_surface(
            &[],
            &config,
            &HashMap::new(),
            None,
            false,
            &ModelSurfaceQuery::default(),
        );
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
        let surface = build_surface(
            &[],
            &config,
            &HashMap::new(),
            None,
            false,
            &ModelSurfaceQuery::default(),
        );
        assert!(
            surface.scopes[0]
                .options
                .iter()
                .filter(|option| option.group.as_deref() == Some(PROFILE_GROUP))
                .any(|option| option.provider.is_none())
        );
    }
}
