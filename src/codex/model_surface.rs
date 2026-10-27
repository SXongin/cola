//! The Codex `/model` surface: what `config/read` declares and what each
//! provider's `/models` endpoint will answer (ticket #908).
//!
//! `model/list` is a **flat, global** catalog compiled into the app-server: it
//! names no provider and is therefore rendered as its own group, never under a
//! provider. Everything else is user configuration:
//!
//! - `[model_providers.<id>]` (from `config/read`'s raw layers — the resolved
//!   `Config` struct is not the source the ticket names, and a layer carries the
//!   raw TOML) becomes the `provider` scope, labelled with `name` (or the id);
//! - the top-level `model` and every `[profiles.<name>]`'s `model` become
//!   one-click presets in the `model` scope, because app-server has no "pick a
//!   profile" field — cola applies the profile's model through `turn/start`;
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
//! the card reflects another client's switch on its next open. `thread/settings/
//! updated` is deliberately **not** projected here: measured against the pinned
//! daemon it is not sent for a resume-over-provider to an unsubscribed
//! connection, so it could not be the source of truth; it would be a bonus only
//! if a server did send it. Effort is likewise left out of this ticket's fact
//! merge: the surface never renders it, and the footer's `@variant` already has
//! its own ordered fact — folding `Thread.reasoningEffort` in is a read-model
//! change, not part of the model/provider surface.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use crate::backend::types::{ChangeScope, ModelOption, ModelScope, ModelSurface, ModelSurfaceQuery};
use crate::feishu::card::truncate_md;

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

/// A drilled page is a card **callback**, answered under
/// [`crate::feishu::ws::CARD_ACK_BUDGET`] (2 s). Config gets this slice of it;
/// probes are served from the cache only and the catalog is one websocket
/// round-trip, so the page cannot chain the root page's 2 s config read with a
/// fresh 2 s probe. The root `/model` page is a text command, not a callback,
/// and keeps [`CONFIG_READ_TIMEOUT`] + fresh probes.
pub(crate) const DRILLED_CONFIG_READ_TIMEOUT: Duration =
    crate::feishu::ws::CARD_ACK_BUDGET.saturating_sub(Duration::from_millis(1400));

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

    /// A compact marker for the provider list, where a whole reason would not
    /// fit. `None` for a successful probe (its models are the model scope).
    pub(crate) fn short_tag(&self) -> Option<String> {
        let tag = match self {
            Self::Models(_) => return None,
            Self::EnvKey => "env_key",
            Self::NoBaseUrl => "无 base_url",
            Self::BadBaseUrl => "base_url 无效",
            Self::UnsupportedScheme => "非 http(s)",
            Self::Unauthorized => "401",
            Self::Forbidden => "403",
            Self::NotFound => "404",
            Self::Timeout => "超时",
            Self::Network => "不可达",
            Self::BadResponse => "响应无效",
            Self::Http(status) => return Some(format!("HTTP {status}")),
            Self::Empty => "空列表",
        };
        Some(tag.to_string())
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

    /// Serve this read from the cache only: a drilled card page runs on the
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
pub(crate) fn build_surface(
    catalog: &[crate::backend::types::ProviderModels],
    config: &CodexModelConfig,
    probes: &HashMap<String, ProbeOutcome>,
    thread_provider: Option<&str>,
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
        options.push(ModelOption {
            id: profile.model.clone(),
            label: Some(truncate_md(&label, 48)),
            value: Some(codex_ref(&profile.model)),
            group: Some(PROFILE_GROUP.to_string()),
            ..ModelOption::default()
        });
    }
    for provider in &config.providers {
        let Some(ProbeOutcome::Models(models)) = probes.get(&provider.id) else {
            continue;
        };
        let group = format!("{}（/models）", provider.name);
        for model in models {
            options.push(ModelOption {
                id: model.clone(),
                value: Some(codex_ref(model)),
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
        options,
        note,
        free_text: Some(format!(
            "💡 也可以直接发送 `/model {MODEL_PROVIDER}/<id>` 选择目录之外的模型。"
        )),
        intro: None,
    };

    let provider_options: Vec<ModelOption> = config
        .providers
        .iter()
        .map(|provider| {
            let mut label = provider.name.clone();
            if current_provider.as_deref() == Some(provider.id.as_str()) {
                label.push_str("（当前）");
            }
            if let Some(tag) = probes.get(&provider.id).and_then(ProbeOutcome::short_tag) {
                label.push_str(" · ");
                label.push_str(&tag);
            }
            ModelOption {
                id: provider.id.clone(),
                label: Some(truncate_md(&label, 48)),
                value: Some(provider.id.clone()),
                ..ModelOption::default()
            }
        })
        .collect();
    let provider_scope = ModelScope {
        id: PROVIDER_SCOPE.into(),
        title: "Provider（线程级：新线程 / resume）".into(),
        changeable: ChangeScope::NewThread,
        note: Some(if provider_options.is_empty() {
            if config.read_failed {
                "_(读不到 config，provider / profile 未知)_".to_string()
            } else {
                "_(config 里没有 provider)_".to_string()
            }
        } else {
            "provider 切换需新线程 / resume；本版本仅列出。".to_string()
        }),
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
        [only] => Some(truncate_md(&format!("⚠️ {only}"), NOTE_MAX_CHARS)),
        many => Some(truncate_md(
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

        let surface = build_surface(&catalog, &config, &probes, Some("opencode-go"), &query());

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
            "codex/deepseek-flash",
            "a profile pick is a model-only pick with the neutral bucket"
        );
        assert!(profile.display_label().contains("fast"));
        assert!(profile.display_label().contains("deepseek"));

        let probed: Vec<&ModelOption> = model
            .options
            .iter()
            .filter(|option| option.group.as_deref() == Some("DeepSeek（/models）"))
            .collect();
        assert_eq!(probed.len(), 2);
        assert_eq!(probed[0].callback_value(), "codex/deepseek-flash");

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

        let provider_scope = &surface.scopes[1];
        assert_eq!(provider_scope.id, PROVIDER_SCOPE);
        assert_eq!(provider_scope.changeable, ChangeScope::NewThread);
        let current = provider_scope
            .options
            .iter()
            .find(|option| option.id == "opencode-go")
            .expect("the current provider is listed");
        assert!(current.display_label().contains("当前"), "{:?}", current.label);
        assert!(current.display_label().contains("env_key"));
        assert_eq!(current.callback_value(), "opencode-go");

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
        let card = crate::feishu::card::picker::build_model_surface_cards(
            &crate::config::ThreadKey::new("chat_1".into(), "chat_1".into()),
            &surface,
            false,
        );
        assert!(
            !format!("{card:?}").contains(TOKEN),
            "the token leaked into a card"
        );
    }

    /// A thread fact of the neutral `codex` bucket is not a channel: after a
    /// picker pick the caller's resolved provider is what the provider list
    /// should mark.
    #[test]
    fn a_neutral_thread_provider_falls_back_to_the_selection() {
        let mut config = CodexModelConfig::default();
        config.providers.push(provider(
            "deepseek",
            Some("http://127.0.0.1:1/v1".into()),
            Some(TOKEN),
            None,
        ));
        let query = ModelSurfaceQuery {
            current: Some(ModelSelection {
                label: "deepseek/deepseek-flash".into(),
                reference: "deepseek/deepseek-flash".into(),
            }),
            ..Default::default()
        };
        let surface = build_surface(&[], &config, &HashMap::new(), Some("codex"), &query);
        let label = surface.scopes[1].options[0].display_label();
        assert!(label.contains("当前"), "label: {label}");
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

        let surface = build_surface(&[], &config, &probes, Some("opencode-go"), &query());
        let model = &surface.scopes[0];
        assert!(model.options.is_empty(), "no provider models on a 403");
        let note = model.note.as_deref().unwrap();
        assert!(note.contains("403"), "note: {note}");
        assert!(note.contains("opencode-go"), "note names the provider: {note}");
        assert!(!note.contains('\n'));

        let provider = &surface.scopes[1];
        let label = provider.options[0].display_label();
        assert!(
            label.contains("403"),
            "provider row carries the short tag: {label}"
        );
        assert!(label.contains("当前"));
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
        let surface = build_surface(&[], &config, &HashMap::new(), None, &ModelSurfaceQuery::default());
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
}
