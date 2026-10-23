use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

pub fn load(path: &Path) -> anyhow::Result<Config> {
    let content = std::fs::read_to_string(path)?;
    Ok(toml::from_str(&content)?)
}

/// Resolve the config file path using the fallback lookup.
///
/// Order (first existing wins):
/// 1. `explicit` — the user's `--config` flag, used verbatim (missing explicit
///    file is a hard error, never silently ignored).
/// 2. `./cola.toml` in the current directory.
/// 3. `~/.cola/cola.toml` — the same cross-platform dir as logs, session
///    mapping, lock and pid files, so launcher shortcuts without a notion of
///    "current directory" still find it.
///
/// Returns `None` when no fallback file exists.
pub fn resolve_config_path(explicit: Option<&str>, cwd: &Path, home: &Path) -> Option<PathBuf> {
    if let Some(path) = explicit {
        return Some(PathBuf::from(path));
    }
    let cwd_config = cwd.join("cola.toml");
    if cwd_config.exists() {
        return Some(cwd_config);
    }
    let home_config = crate::state::StateLayout::for_home(home).config_file();
    if home_config.exists() {
        return Some(home_config);
    }
    None
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    /// The default OpenCode client's options (ADR-0090 decision 5). This table
    /// is also the implicit `[clients.default] kind = "opencode"`: it is never
    /// renamed, and [`Config::active_client`] resolves it when
    /// `[bridge] default_client` names no other client.
    #[serde(default)]
    pub opencode: OpenCodeConfig,
    pub feishu: FeishuConfig,
    #[serde(default)]
    pub bridge: BridgeConfig,
    #[serde(default)]
    pub review: ReviewConfig,
    #[serde(default)]
    pub worktree: WorktreeConfig,
    /// Every declared agent client by id (ADR-0090 decision 5). The map does
    /// not contain the implicit `default` client — that one is `[opencode]` —
    /// so seeing `default` here is a duplicate and fails to resolve.
    ///
    /// `BTreeMap` rather than a hash map: the configured ids are reported in a
    /// resolution error, and that report must be deterministic.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub clients: BTreeMap<String, ClientEntry>,
}

/// Repository review settings (spec #803, ticket #804): what `/diff` compares
/// the working tree against.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ReviewConfig {
    /// The base ref `/diff` uses when the command names none. `None` leaves the
    /// producer's own resolution (`origin/main`, then `main`). `--base` on the
    /// command always wins over this.
    #[serde(default)]
    pub base: Option<String>,
}

/// Worktree settings (ADR-0088): where `/worktree new` creates its checkouts.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorktreeConfig {
    /// The parent directory for new worktrees. A relative value resolves
    /// against the project's primary checkout (never the session's own
    /// directory, which may itself be a linked worktree); an absolute path is
    /// used as given. Defaults to `.worktrees`, the repository's own
    /// convention.
    #[serde(default = "default_worktree_root")]
    pub root: PathBuf,
}

impl Default for WorktreeConfig {
    fn default() -> Self {
        Self {
            root: default_worktree_root(),
        }
    }
}

fn default_worktree_root() -> PathBuf {
    PathBuf::from(".worktrees")
}

/// When cola starts an `opencode serve` of its own (ADR-0013).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum ServerStartPolicy {
    /// Lazy: attach at boot, spawn an Owned Server only at the moment a prompt
    /// actually needs one and discovery finds no server. The default.
    #[default]
    Auto,
    /// Attach-only: cola never spawns an Owned Server. If no server exists the
    /// bot replies that OpenCode is unavailable.
    Never,
    /// The old behavior: spawn an Owned Server at boot when none is running.
    Eager,
}

impl ServerStartPolicy {
    /// Whether a demand-time lazy spawn is allowed (`Never` forbids it).
    pub fn spawns_when_needed(self) -> bool {
        !matches!(self, Self::Never)
    }
}

/// How cola picks the attached server's protocol generation (spec #364 §2,
/// ADR-0055). The glossary calls this the **Generation Override**.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum GenerationOverride {
    /// Probe `GET /api/info` on every attach/reconnect and trust the result.
    /// An inconclusive probe leaves cola serverless (never guessed). The
    /// default.
    #[default]
    Auto,
    /// Force the V1 strategy even when the probe disagrees; the contradicting
    /// probe is logged at WARN with its evidence.
    V1,
    /// Force the V2 strategy even when the probe disagrees; the contradicting
    /// probe is logged at WARN with its evidence.
    V2,
}

impl std::fmt::Display for GenerationOverride {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Auto => "auto",
            Self::V1 => "v1",
            Self::V2 => "v2",
        })
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OpenCodeConfig {
    /// Preferred/fallback port. Optional: discovery rewrites the effective
    /// endpoint to whatever server it finds on the shared store, so an absent
    /// `url` just means "use the default `http://localhost:4096`". In a
    /// coexistence setup it is only a tiebreaker among servers of the same
    /// class (ADR-0013).
    #[serde(default)]
    pub url: Option<String>,
    /// Default model for new sessions, "provider/model". When unset, cola does
    /// NOT pin a model — the OpenCode server uses its own default model (the
    /// server falls back to `provider.defaultModel()`), so "不写 model" just
    /// means "用 opencode 的默认模型".
    #[serde(default)]
    pub model: Option<String>,
    /// When cola may start its own `opencode serve` (ADR-0013): `auto` (lazy,
    /// the default), `never` (attach-only), `eager` (spawn at boot).
    #[serde(default)]
    pub start_server: ServerStartPolicy,
    /// How the attached server's protocol generation is chosen (spec #364 §2):
    /// `auto` probes `GET /api/info` (default), `v1`/`v2` force the strategy.
    #[serde(default)]
    pub generation: GenerationOverride,
}

impl Default for OpenCodeConfig {
    fn default() -> Self {
        Self {
            url: None,
            model: None,
            start_server: ServerStartPolicy::Auto,
            generation: GenerationOverride::Auto,
        }
    }
}

impl OpenCodeConfig {
    /// The preferred port from `[opencode] url`, if parseable. A tiebreaker in
    /// `pick_server` among servers of the same class (ADR-0013), never an
    /// absolute pin.
    pub fn preferred_port(&self) -> Option<u16> {
        self.url.as_deref().and_then(port_from_url)
    }
}

/// The explicit port of an endpoint URL (`http://localhost:4096`,
/// `http://127.0.0.1:49374/`), `None` when it carries none. The one parser for
/// the config's `[opencode] url` tiebreaker, the V2 registration URL discovery
/// reads, and the attached server URL the ownership/yield check parses — an
/// endpoint port is never parsed twice differently.
pub(crate) fn port_from_url(url: &str) -> Option<u16> {
    url.rsplit(':').next()?.trim_end_matches('/').parse().ok()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FeishuConfig {
    pub app_id: String,
    pub app_secret: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BridgeConfig {
    #[serde(default = "default_session_file")]
    pub session_file: PathBuf,
    /// The Access List file (ADR-0035). A missing or unreadable file means the
    /// bot is unclaimed: only the Claim is served until a Host is recorded.
    #[serde(default = "default_access_file")]
    pub access_file: PathBuf,
    /// Default directory for new sessions. When unset, falls back to the
    /// process working directory. `/dir` overrides per session.
    #[serde(default)]
    pub work_dir: Option<PathBuf>,
    /// New sessions start with Auto-Accept on (`/autoaccept`): cola answers
    /// their pending permission requests automatically instead of showing a
    /// Feishu card. OFF by default — an upgrade must never change permission
    /// behavior without consent. Sessions cola merely ADOPTS keep their own
    /// state; `/autoaccept off` remains the per-session escape hatch.
    #[serde(default)]
    pub default_auto_accept: bool,
    /// In group chats, reply to the requester's message with a short
    /// completion notice (the streaming card is patched in place and so does
    /// not push a new notification). p2p chats don't need it.
    #[serde(default = "default_group_completion_notice")]
    pub group_completion_notice: bool,
    /// In p2p, reply to the requester's message with a completion notice when
    /// a Turn ran past the long-task threshold (5 minutes) — the card patch
    /// pushes no notification and does not bump the conversation, so a long
    /// task's end is otherwise invisible. OFF by default: an upgrade must
    /// never change notification behavior without consent.
    #[serde(default)]
    pub long_task_notice: bool,
    /// Opt-in: pin the Chat or Topic with Feishu's Instant Reminder while a
    /// Permission/Question is pending (ADR-0043). OFF by default — an upgrade
    /// must never change notification behavior without consent.
    #[serde(default)]
    pub instant_reminder: bool,
    /// How many days of rotated daily logs to keep (default 14). Older
    /// `cola-YYYY-MM-DD.log` files are swept on startup and at each rotation.
    #[serde(default = "default_log_days")]
    pub log_days: u32,
    /// The id of the active agent client (ADR-0090 decision 5). Unset means
    /// [`DEFAULT_CLIENT_ID`], the implicit `[opencode]` client. Exactly one
    /// client is active per cola process; switching is this key plus a restart.
    #[serde(default)]
    pub default_client: Option<String>,
}

impl Default for BridgeConfig {
    fn default() -> Self {
        Self {
            session_file: default_session_file(),
            access_file: default_access_file(),
            work_dir: None,
            default_auto_accept: false,
            group_completion_notice: default_group_completion_notice(),
            long_task_notice: false,
            instant_reminder: false,
            log_days: default_log_days(),
            default_client: None,
        }
    }
}

fn default_group_completion_notice() -> bool {
    true
}

fn default_log_days() -> u32 {
    14
}

fn default_session_file() -> PathBuf {
    crate::state::StateLayout::default().session_file().to_path_buf()
}

fn default_access_file() -> PathBuf {
    crate::state::StateLayout::default().access_file()
}

/// The id of the implicit client `[opencode]` declares (ADR-0090 decision 5).
///
/// This spelling is **durable contract**, not display text: the state ticket
/// records the active client's id beside every Session, so `default` is what a
/// legacy file upgrades to. [`Config::active_client`] falls back to it whenever
/// `[bridge] default_client` is unset.
pub const DEFAULT_CLIENT_ID: &str = "default";

/// The adapter a configured agent client selects (ADR-0090 decision 1/2).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClientKind {
    /// The peer OpenCode adapter — the only kind this build constructs.
    Opencode,
    /// Codex over `app-server` (ADR-0090 decision 2); its adapter is spec #860
    /// Phase 3, so this build resolves the client but refuses to start it.
    Codex,
}

impl ClientKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Opencode => "opencode",
            Self::Codex => "codex",
        }
    }

    fn parse(raw: &str) -> Option<Self> {
        match raw {
            "opencode" => Some(Self::Opencode),
            "codex" => Some(Self::Codex),
            _ => None,
        }
    }
}

impl std::fmt::Display for ClientKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The option keys an `opencode` Agent Client accepts: letter for letter what
/// `[opencode]` accepts.
///
/// Kept as an explicit list, not derived from the struct, because the parser
/// must tell a known option from a nested table (`[clients.ada.opencode]`) or a
/// typo — `serde(flatten)` would swallow both silently.
/// `the_option_key_list_matches_the_opencode_config` keeps the list in step
/// with `OpenCodeConfig`'s fields.
const OPENCODE_CLIENT_KEYS: &[&str] = &["url", "model", "start_server", "generation"];

/// One `[clients.<id>]` table: a `kind` plus that kind's options (ADR-0090
/// decision 5).
///
/// Deserialised by hand from the raw table so every key is accounted for:
/// `kind`, the `opencode` option keys above, and — instead of being silently
/// dropped by a `flatten` — whatever is left, which [`Config::active_client`]
/// then rejects naming the table (`clients.<id>.<key>`). `kind` stays raw text
/// so a missing or misspelled value is reported with that same path.
#[derive(Debug, Clone, Default)]
pub struct ClientEntry {
    /// The adapter selector, `opencode` or `codex`. Required; validated on
    /// resolution.
    pub kind: Option<String>,
    /// This Agent Client's OpenCode options — the keys `[opencode]` accepts.
    /// Read only for `kind = "opencode"`; a named Agent Client never inherits
    /// `[opencode]`.
    pub opencode: OpenCodeConfig,
    /// Keys no option list defines (a nested `[clients.<id>.opencode]` table, a
    /// typo such as `model_typo`, a future setting this build predates). Kept
    /// so resolution can fail loudly instead of running different settings
    /// than the file spells.
    unknown: std::collections::BTreeMap<String, toml::Value>,
}

impl<'de> Deserialize<'de> for ClientEntry {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        use serde::de::Error as _;

        let mut table = toml::Table::deserialize(deserializer)?;
        let kind = match table.remove("kind") {
            None => None,
            Some(toml::Value::String(kind)) => Some(kind),
            Some(other) => {
                return Err(D::Error::custom(format!(
                    "`kind` must be a string (`opencode` or `codex`), got {}",
                    other.type_str()
                )));
            }
        };
        let mut options = toml::Table::new();
        for key in OPENCODE_CLIENT_KEYS {
            if let Some(value) = table.remove(*key) {
                options.insert((*key).to_string(), value);
            }
        }
        let opencode = OpenCodeConfig::deserialize(toml::Value::Table(options)).map_err(D::Error::custom)?;
        Ok(Self {
            kind,
            opencode,
            unknown: table.into_iter().collect(),
        })
    }
}

/// `[clients.<id>]` is one table: `kind` plus that kind's options. Only an
/// `opencode` Agent Client has options today, so its keys are serialised only
/// then — a `codex` table must not grow `start_server`/`generation` rows it
/// ignores in `cola config show`. Keys resolution rejects are still printed:
/// `config show` must not hide what the file says.
impl Serialize for ClientEntry {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        use serde::ser::SerializeMap;

        let mut map = serializer.serialize_map(None)?;
        if let Some(kind) = &self.kind {
            map.serialize_entry("kind", kind)?;
        }
        if self.kind.as_deref() == Some(ClientKind::Opencode.as_str()) {
            let options = toml::Value::try_from(&self.opencode)
                .map_err(|error| serde::ser::Error::custom(error.to_string()))?;
            if let toml::Value::Table(options) = options {
                for (key, value) in options {
                    map.serialize_entry(&key, &value)?;
                }
            }
        }
        for (key, value) in &self.unknown {
            map.serialize_entry(key, value)?;
        }
        map.end()
    }
}

impl ClientEntry {
    fn kind_for(&self, id: &str) -> Result<ClientKind, ClientConfigError> {
        let raw = self
            .kind
            .as_deref()
            .ok_or_else(|| ClientConfigError::MissingKind { id: id.to_string() })?;
        ClientKind::parse(raw).ok_or_else(|| ClientConfigError::UnknownKind {
            id: id.to_string(),
            kind: raw.to_string(),
        })
    }

    /// Reject any key the table carried that no option list claims, naming the
    /// client and the setting (`clients.ada.opencode` for a nested table).
    /// `BTreeMap` order makes the reported key deterministic.
    fn unknown_setting(&self, id: &str) -> Result<(), ClientConfigError> {
        match self.unknown.iter().next() {
            Some((key, _)) => Err(ClientConfigError::UnknownSetting {
                id: id.to_string(),
                key: key.clone(),
            }),
            None => Ok(()),
        }
    }
}

/// The one active agent client, resolved from config (ADR-0090 decision 5).
///
/// Produced exactly once per process by [`Config::active_client`] /
/// [`Config::resolve_active_client`]; the `id` is the value durable state
/// records, so renaming it is a state migration, not a config edit.
#[derive(Debug, Clone, PartialEq)]
pub struct ActiveClient {
    /// The configured id: [`DEFAULT_CLIENT_ID`] for the implicit `[opencode]`
    /// client, otherwise the `[clients.<id>]` table's key.
    pub id: String,
    /// The adapter this client selects.
    pub kind: ClientKind,
    /// That kind's settings. `settings` always matches `kind`.
    pub settings: ClientSettings,
}

impl ActiveClient {
    /// This client's OpenCode options when it is an `opencode` client — the
    /// only kind with options modelled so far (ADR-0090 decision 7: the second
    /// kind's settings are earned by its adapter, not designed ahead).
    pub fn opencode(&self) -> Option<&OpenCodeConfig> {
        match &self.settings {
            ClientSettings::Opencode(settings) => Some(settings),
            ClientSettings::Codex => None,
        }
    }
}

/// A resolved client's kind-specific options.
#[derive(Debug, Clone, PartialEq)]
pub enum ClientSettings {
    Opencode(OpenCodeConfig),
    /// Codex has no configurable options yet; #877 adds daemon discovery
    /// settings with the adapter that reads them.
    Codex,
}

/// Why a config cannot resolve its active client. Every message names the
/// exact key to fix, because a client id is durable state and may not be
/// guessed.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ClientConfigError {
    /// `[opencode]` already declares the implicit `default` client, so an
    /// explicit table under that id would be a second one.
    #[error(
        "`[clients.default]` duplicates the implicit client `[opencode]` declares (ADR-0090); \
         give the table another id, or move its options into `[opencode]`"
    )]
    DuplicateDefaultClient,
    /// `[clients.<id>]` exists but carries no `kind`.
    #[error("`clients.{id}.kind` is required; set it to `opencode` or `codex`")]
    MissingKind { id: String },
    /// `[clients.<id>] kind = "<unknown>"`.
    #[error("`clients.{id}.kind = \"{kind}\"` is not a known client kind; expected `opencode` or `codex`")]
    UnknownKind { id: String, kind: String },
    /// `[clients.<id>]` carries a key no option list defines.
    #[error(
        "`clients.{id}.{key}` is not a recognised Agent Client setting; `[clients.{id}]` takes `kind` \
         plus that kind's option keys (`url`, `model`, `start_server`, `generation` for `opencode`), \
         never a nested table"
    )]
    UnknownSetting { id: String, key: String },
    /// `[bridge] default_client` names no configured table.
    #[error("`bridge.default_client = \"{id}\"` names no configured client (configured: {available})")]
    UnknownDefaultClient { id: String, available: String },
}

impl Config {
    /// Resolve the one active agent client — pure, no I/O and no duplicated
    /// meaning (ADR-0090 decision 5).
    ///
    /// The implicit `default`/`opencode` client is always available:
    /// `[opencode]`'s options, all defaults when the table is absent. A
    /// `[clients.default]` table is rejected because that id is already taken.
    /// An unset `[bridge] default_client` means [`DEFAULT_CLIENT_ID`]; a set one
    /// must name a declared table, whose `kind` must be present and known.
    pub fn active_client(&self) -> Result<ActiveClient, ClientConfigError> {
        // `[opencode]` IS `clients.default`, whether or not the table is
        // written. A second table under that id is a collision, never a
        // precedence question.
        if self.clients.contains_key(DEFAULT_CLIENT_ID) {
            return Err(ClientConfigError::DuplicateDefaultClient);
        }
        // Every declared table is read the same way, active or not: a missing
        // or misspelled `kind`, or a key no option list claims, is a config
        // error — reporting it while the table is inert would leave it to break
        // a later switch. `BTreeMap` order makes which one is reported first
        // deterministic.
        for (entry_id, entry) in &self.clients {
            entry.kind_for(entry_id)?;
            entry.unknown_setting(entry_id)?;
        }
        let id = self.bridge.default_client.as_deref().unwrap_or(DEFAULT_CLIENT_ID);
        if id == DEFAULT_CLIENT_ID {
            return Ok(ActiveClient {
                id: DEFAULT_CLIENT_ID.to_string(),
                kind: ClientKind::Opencode,
                settings: ClientSettings::Opencode(self.opencode.clone()),
            });
        }
        let entry = self
            .clients
            .get(id)
            .ok_or_else(|| ClientConfigError::UnknownDefaultClient {
                id: id.to_string(),
                available: std::iter::once(DEFAULT_CLIENT_ID)
                    .chain(self.clients.keys().map(String::as_str))
                    .collect::<Vec<_>>()
                    .join(", "),
            })?;
        let kind = entry.kind_for(id)?;
        let settings = match kind {
            ClientKind::Opencode => ClientSettings::Opencode(entry.opencode.clone()),
            ClientKind::Codex => ClientSettings::Codex,
        };
        Ok(ActiveClient {
            id: id.to_string(),
            kind,
            settings,
        })
    }

    /// Resolve the active client and fold its OpenCode options into
    /// [`Config::opencode`], the field every existing call site already reads.
    ///
    /// This is a process-local **effective view**, never a write to the user's
    /// file: a named `kind = "opencode"` client's `url`/`model`/`start_server`/
    /// `generation` take effect without every caller learning about
    /// `[clients.*]`. The resolved id is returned for the state layer, which
    /// records it (spec #860 phase 1, ticket #874).
    pub fn resolve_active_client(&mut self) -> Result<ActiveClient, ClientConfigError> {
        let active = self.active_client()?;
        if let ClientSettings::Opencode(settings) = &active.settings {
            self.opencode = settings.clone();
        }
        Ok(active)
    }
}

/// A key that uniquely identifies a session context on the Feishu side.
///
/// `thread_id` is the authoritative topic identifier (`omt_...`): present on
/// topic messages (in groups or p2p). For non-topic messages (a group-root or
/// a p2p top-level message) it falls back to the `chat_id`, which makes the
/// whole chat a single "lobby" conversation.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ThreadKey {
    pub chat_id: String,
    #[serde(alias = "root_id")]
    pub thread_id: String,
}

impl ThreadKey {
    pub fn new(chat_id: String, thread_id: String) -> Self {
        Self { chat_id, thread_id }
    }

    /// Derive the conversation key for an incoming message.
    ///
    /// A message that carries a `thread_id` is a topic message → its own key.
    /// Anything else (no `thread_id`) is the chat's top level: a single
    /// "lobby" conversation keyed by `chat_id` itself.
    pub fn from_message(chat_id: &str, thread_id: Option<&str>) -> Self {
        match thread_id {
            Some(tid) if !tid.is_empty() => Self::new(chat_id.to_string(), tid.to_string()),
            _ => Self::new(chat_id.to_string(), chat_id.to_string()),
        }
    }
}

/// How an incoming message maps to a conversation. Encapsulates the
/// group-root/lobby policy: p2p top-level messages are normal conversations;
/// group-root messages are a lobby (with guidance); topic messages (any chat
/// type) are isolated conversations.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConversationKind {
    /// A message inside a topic (group or p2p) → topic-isolated session.
    Topic,
    /// A top-level message in a group → the group's lobby session.
    GroupLobby,
    /// A top-level p2p message → the p2p conversation.
    P2p,
}

impl ConversationKind {
    /// Classify an incoming message. `thread_id` is `Some` iff the message is
    /// inside a topic (findings: a message is a topic message IFF it has one).
    pub fn classify(chat_type: &str, thread_id: Option<&str>) -> Self {
        let has_thread = thread_id.map(|t| !t.is_empty()).unwrap_or(false);
        if has_thread {
            ConversationKind::Topic
        } else if chat_type == "p2p" {
            ConversationKind::P2p
        } else {
            ConversationKind::GroupLobby
        }
    }

    /// The thread key routing a message of this kind.
    pub fn thread_key(&self, chat_id: &str, thread_id: Option<&str>) -> ThreadKey {
        match self {
            ConversationKind::Topic => ThreadKey::from_message(chat_id, thread_id),
            // Non-topic kinds are always the chat's top level: the key is the
            // chat itself, regardless of any stray thread_id.
            _ => ThreadKey::new(chat_id.to_string(), chat_id.to_string()),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionEntry {
    pub thread_key: ThreadKey,
    pub session_id: String,
    pub directory: String,
    /// Per-session agent override set by `/agent`, sent as a per-prompt
    /// `PromptInput.agent` on the next message. Persisted so it survives a cola
    /// restart; when `None` the server's default agent applies.
    #[serde(default)]
    pub agent: Option<String>,
    /// Per-session model override ("provider/model") set by `/model`, sent as a
    /// per-prompt `PromptInput.model` on the next message. Persisted so it
    /// survives a cola restart; when `None` the configured default (or the
    /// server's own default) applies.
    #[serde(default)]
    pub model: Option<String>,
    /// Per-session thinking level ("variant", e.g. "high") set by `/think`,
    /// sent as a per-prompt `PromptInput.variant` on the next message. A model
    /// declares its own variant set — there is no universal scale (ADR-0020);
    /// `None` means the server's default for whatever model runs this turn.
    /// Persisted; cleared automatically when `/model` switches to a model that
    /// doesn't declare the current variant.
    #[serde(default)]
    pub variant: Option<String>,
    /// When true, pending permission requests for this session are answered
    /// automatically (`/autoaccept`) instead of showing a Feishu card.
    #[serde(default)]
    pub auto_accept: bool,
    /// For topic-backed sessions: the `message_id` of a message INSIDE the
    /// topic (recorded when the topic was created via `/topic`). Permission /
    /// question / external cards that must be *sent* (no in-flight card to
    /// reply to) reply to this anchor, which keeps them inside the topic — the
    /// create API rejects `receive_id_type=thread_id`.
    #[serde(default)]
    pub topic_anchor: Option<String>,
    /// For cola-created topics (`/topic`, `/topic --adopt`): the `message_id`
    /// of the command message the Feishu topic was created around — the thread
    /// root (ADR-0023). Together with `topic_anchor` it identifies the topic's
    /// own creation messages, which are never injected as Quoted Context.
    /// Manually-created topics leave it `None`.
    #[serde(default)]
    pub topic_root: Option<String>,
}

impl SessionEntry {
    /// A mapping entry with every optional field at its default; the flow that
    /// creates the mapping sets exactly the extras it carries.
    pub fn new(thread_key: ThreadKey, session_id: impl Into<String>, directory: impl Into<String>) -> Self {
        Self {
            thread_key,
            session_id: session_id.into(),
            directory: directory.into(),
            agent: None,
            model: None,
            variant: None,
            auto_accept: false,
            topic_anchor: None,
            topic_root: None,
        }
    }
}

/// The subcommands of `cola config` (ADR-0081): print where the config came
/// from, print the effective values with secrets masked, or parse-check it.
#[derive(Debug, Clone, Copy, clap::Subcommand)]
pub enum ConfigAction {
    /// Print the resolved config file path (the fallback lookup order).
    Path,
    /// Print the effective config with secrets masked.
    Show,
    /// Parse the config and report whether it is valid.
    Validate,
}

/// Resolve the config path from the ambient environment — the current
/// directory and home — with the documented fallback chain. The lenient
/// counterpart of `main::resolve_config_or_exit`: `None` when no file is found,
/// never an exit. Shared by `cola config` and `cola status`/`doctor`.
pub fn resolve_path_from_env(explicit: Option<&str>) -> Option<PathBuf> {
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let home = dirs::home_dir().unwrap_or_else(|| PathBuf::from("."));
    resolve_config_path(explicit, &cwd, &home)
}

pub fn run(action: ConfigAction, explicit: Option<&str>) -> anyhow::Result<()> {
    let path = resolve_path_from_env(explicit);
    match action {
        ConfigAction::Path => {
            match path {
                Some(p) => println!("{}", p.display()),
                None => println!("未找到配置文件（依次查找 ./cola.toml、~/.cola/cola.toml）"),
            }
            Ok(())
        }
        ConfigAction::Show => {
            let path = path.ok_or_else(|| anyhow::anyhow!("未找到配置文件"))?;
            print!("{}", masked_config_toml(&load(&path)?)?);
            Ok(())
        }
        ConfigAction::Validate => {
            let path = path.ok_or_else(|| anyhow::anyhow!("未找到配置文件"))?;
            // `load` is the same parser the daemon uses, and the active Agent
            // Client must resolve — but this command answers "is the config
            // resolvable", not "can this build attach to it": an active
            // `codex` client is valid config and startup's capability gate is
            // what refuses it until its adapter lands (ADR-0090 §5).
            let cfg = load(&path)?;
            cfg.active_client()?;
            println!("配置有效: {}", path.display());
            Ok(())
        }
    }
}

/// The effective config as TOML with the Feishu app secret masked — what
/// `cola config show` prints. The secret never leaves this function.
pub fn masked_config_toml(cfg: &Config) -> anyhow::Result<String> {
    let mut masked = cfg.clone();
    masked.feishu.app_secret = mask_secret(&masked.feishu.app_secret);
    Ok(toml::to_string_pretty(&masked)?)
}

/// Mask a secret for display: never print it, but say whether one is set and
/// how long it is, which is what a config-support question actually needs.
pub fn mask_secret(secret: &str) -> String {
    if secret.is_empty() {
        return "(未设置)".to_string();
    }
    format!("***（{} 个字符）", secret.chars().count())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mask_secret_hides_the_value() {
        assert_eq!(mask_secret(""), "(未设置)");
        let masked = mask_secret("super-secret-value");
        assert!(!masked.contains("super"), "the value must not leak: {masked}");
        assert!(masked.contains("18"), "the length is reported: {masked}");
    }

    #[test]
    fn config_run_handles_every_action_for_a_valid_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("cola.toml");
        std::fs::write(&path, "[feishu]\napp_id = \"cli_abc\"\napp_secret = \"s3cr3t\"\n").unwrap();
        let explicit = path.to_str();
        run(ConfigAction::Path, explicit).unwrap();
        run(ConfigAction::Show, explicit).unwrap();
        run(ConfigAction::Validate, explicit).unwrap();
    }

    #[test]
    fn config_run_validate_rejects_a_broken_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("cola.toml");
        std::fs::write(&path, "not = valid = toml").unwrap();
        assert!(run(ConfigAction::Validate, path.to_str()).is_err());
    }

    #[test]
    fn config_run_show_without_a_file_is_an_error() {
        // An explicit path that does not exist: `load` fails.
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("nope.toml");
        assert!(run(ConfigAction::Show, missing.to_str()).is_err());
    }

    #[test]
    fn masked_config_toml_hides_the_app_secret() {
        // The production renderer `cola config show` prints: the secret must not
        // survive it. Asserted against `masked_config_toml` (not a re-derived
        // transformation) so removing the masking would fail this test.
        let cfg = Config {
            opencode: OpenCodeConfig::default(),
            feishu: FeishuConfig {
                app_id: "cli_abc".into(),
                app_secret: "s3cr3t".into(),
            },
            bridge: BridgeConfig::default(),
            review: ReviewConfig::default(),
            worktree: WorktreeConfig::default(),
            clients: Default::default(),
        };
        let dumped = masked_config_toml(&cfg).unwrap();
        assert!(dumped.contains("cli_abc"), "the app id is shown: {dumped}");
        assert!(!dumped.contains("s3cr3t"), "secret leaked: {dumped}");
    }

    #[test]
    fn review_base_is_optional_and_parsed() {
        let minimal: Config =
            toml::from_str("[feishu]\napp_id = \"cli_abc\"\napp_secret = \"s3cr3t\"\n").unwrap();
        assert_eq!(minimal.review.base, None, "the whole [review] table is optional");

        let configured: Config = toml::from_str(
            "[feishu]\napp_id = \"cli_abc\"\napp_secret = \"s3cr3t\"\n[review]\nbase = \"upstream/dev\"\n",
        )
        .unwrap();
        assert_eq!(configured.review.base.as_deref(), Some("upstream/dev"));
    }

    /// ADR-0088: the root is optional and defaults to the repository's own
    /// `.worktrees`; an absolute path is kept verbatim.
    #[test]
    fn worktree_root_defaults_and_parses() {
        let minimal: Config =
            toml::from_str("[feishu]\napp_id = \"cli_abc\"\napp_secret = \"s3cr3t\"\n").unwrap();
        assert_eq!(minimal.worktree.root, PathBuf::from(".worktrees"));

        let configured: Config = toml::from_str(
            "[feishu]\napp_id = \"cli_abc\"\napp_secret = \"s3cr3t\"\n[worktree]\nroot = \"/tmp/wt\"\n",
        )
        .unwrap();
        assert_eq!(configured.worktree.root, PathBuf::from("/tmp/wt"));
    }

    #[test]
    fn from_message_uses_thread_id_when_present() {
        let key = ThreadKey::from_message("oc_group_1", Some("omt_topic_1"));
        assert_eq!(key.chat_id, "oc_group_1");
        assert_eq!(key.thread_id, "omt_topic_1");
        assert_ne!(key.thread_id, key.chat_id);
    }

    #[test]
    fn from_message_falls_back_to_chat_id_for_non_topic() {
        let key = ThreadKey::from_message("oc_group_1", None);
        assert_eq!(key.thread_id, "oc_group_1");

        let empty = ThreadKey::from_message("oc_p2p_1", Some(""));
        assert_eq!(empty.thread_id, "oc_p2p_1");
    }

    #[test]
    fn classify_topic_messages_in_group_and_p2p() {
        assert_eq!(
            ConversationKind::classify("group", Some("omt_t_1")),
            ConversationKind::Topic
        );
        assert_eq!(
            ConversationKind::classify("p2p", Some("omt_t_1")),
            ConversationKind::Topic
        );
    }

    #[test]
    fn classify_group_root_is_lobby_p2p_top_is_conversation() {
        assert_eq!(
            ConversationKind::classify("group", None),
            ConversationKind::GroupLobby
        );
        assert_eq!(ConversationKind::classify("p2p", None), ConversationKind::P2p);
    }

    #[test]
    fn classify_topic_key_keeps_thread_id_others_use_chat() {
        let topic = ConversationKind::Topic.thread_key("oc_g1", Some("omt_t_1"));
        assert_eq!(topic.thread_id, "omt_t_1");

        let lobby = ConversationKind::GroupLobby.thread_key("oc_g1", Some("omt_t_1"));
        assert_eq!(lobby.thread_id, "oc_g1");

        let p2p = ConversationKind::P2p.thread_key("oc_p1", None);
        assert_eq!(p2p.thread_id, "oc_p1");
    }

    #[test]
    fn deserializes_legacy_root_id_field() {
        // Old sessions.json entries used `root_id`; the serde alias keeps them loadable.
        let key: ThreadKey = serde_json::from_str(r#"{"chat_id":"oc_1","root_id":"omt_old"}"#).unwrap();
        assert_eq!(key.thread_id, "omt_old");
    }

    #[test]
    fn start_server_policy_deserializes_three_states() {
        let parse = |s: &str| toml::from_str::<OpenCodeConfig>(s).unwrap().start_server;
        assert_eq!(parse(r#"start_server = "auto""#), ServerStartPolicy::Auto);
        assert_eq!(parse(r#"start_server = "never""#), ServerStartPolicy::Never);
        assert_eq!(parse(r#"start_server = "eager""#), ServerStartPolicy::Eager);
    }

    #[test]
    fn start_server_defaults_to_auto() {
        let cfg: OpenCodeConfig = toml::from_str("").unwrap();
        assert_eq!(cfg.start_server, ServerStartPolicy::Auto);
        assert!(cfg.start_server.spawns_when_needed());
        assert!(!ServerStartPolicy::Never.spawns_when_needed());
        assert!(ServerStartPolicy::Eager.spawns_when_needed());
    }

    #[test]
    fn port_from_url_requires_an_explicit_port() {
        assert_eq!(port_from_url("http://localhost:4096"), Some(4096));
        assert_eq!(port_from_url("http://127.0.0.1:49374/"), Some(49374));
        assert_eq!(port_from_url("http://localhost"), None);
        assert_eq!(port_from_url("nonsense"), None);
    }

    #[test]
    fn preferred_port_parses_the_configured_url() {
        let url = |u: &str| {
            toml::from_str::<OpenCodeConfig>(&format!("url = \"{u}\""))
                .unwrap()
                .preferred_port()
        };
        assert_eq!(url("http://localhost:4096"), Some(4096));
        assert_eq!(url("http://localhost:4096/"), Some(4096));
        assert_eq!(url("http://localhost"), None);
    }

    #[test]
    fn generation_override_deserializes_three_states() {
        let parse = |s: &str| toml::from_str::<OpenCodeConfig>(s).unwrap().generation;
        assert_eq!(parse(r#"generation = "auto""#), GenerationOverride::Auto);
        assert_eq!(parse(r#"generation = "v1""#), GenerationOverride::V1);
        assert_eq!(parse(r#"generation = "v2""#), GenerationOverride::V2);
    }

    #[test]
    fn generation_override_defaults_to_auto_and_rejects_unknown() {
        let cfg: OpenCodeConfig = toml::from_str("").unwrap();
        assert_eq!(cfg.generation, GenerationOverride::Auto);
        assert_eq!(cfg.generation.to_string(), "auto");
        // A typo must be a loud config error, never a silent fallback to auto.
        assert!(toml::from_str::<OpenCodeConfig>(r#"generation = "v3""#).is_err());
    }

    /// Instant Reminder is opt-in (ADR-0043): absent or explicit `false` must
    /// mean off, so an upgrade never changes notification behavior; only an
    /// explicit `true` enables the pin lifecycle.
    #[test]
    fn instant_reminder_defaults_to_off_and_requires_an_explicit_true() {
        let absent: BridgeConfig = toml::from_str("").unwrap();
        assert!(
            !absent.instant_reminder,
            "an absent instant_reminder key means off"
        );

        let off: BridgeConfig = toml::from_str("instant_reminder = false").unwrap();
        assert!(!off.instant_reminder);

        let on: BridgeConfig = toml::from_str("instant_reminder = true").unwrap();
        assert!(on.instant_reminder);
    }

    /// Auto-Accept's create-time default is opt-in (#513): absent or explicit
    /// `false` must mean off, so an upgrade never changes permission behavior;
    /// only an explicit `true` marks new sessions.
    #[test]
    fn default_auto_accept_defaults_to_off_and_requires_an_explicit_true() {
        let absent: BridgeConfig = toml::from_str("").unwrap();
        assert!(
            !absent.default_auto_accept,
            "an absent default_auto_accept key means off"
        );

        let off: BridgeConfig = toml::from_str("default_auto_accept = false").unwrap();
        assert!(!off.default_auto_accept);

        let on: BridgeConfig = toml::from_str("default_auto_accept = true").unwrap();
        assert!(on.default_auto_accept);
    }

    #[test]
    fn feishu_only_config_loads() {
        // README's minimal config: only the Feishu credentials. `opencode` must
        // default rather than fail with "missing field".
        let toml_str = r#"
            [feishu]
            app_id = "cli_test"
            app_secret = "secret"
        "#;
        let cfg: Config = toml::from_str(toml_str).unwrap();
        assert_eq!(cfg.opencode.start_server, ServerStartPolicy::Auto);
        assert!(cfg.opencode.url.is_none());
        assert!(cfg.opencode.model.is_none());
        assert_eq!(cfg.feishu.app_id, "cli_test");
        assert_eq!(cfg.feishu.app_secret, "secret");
    }

    #[test]
    fn feishu_plus_opencode_model_config_loads() {
        // The historical issue #02 criterion: `[feishu]` + an `[opencode]` table
        // holding only `model` — the other `[opencode]` fields must default.
        let toml_str = r#"
            [feishu]
            app_id = "cli_test"
            app_secret = "secret"

            [opencode]
            model = "opencode/deepseek-v4-flash"
        "#;
        let cfg: Config = toml::from_str(toml_str).unwrap();
        assert_eq!(cfg.opencode.model.as_deref(), Some("opencode/deepseek-v4-flash"));
        assert!(cfg.opencode.url.is_none());
        assert_eq!(cfg.opencode.start_server, ServerStartPolicy::Auto);
        assert_eq!(cfg.feishu.app_id, "cli_test");
    }

    #[test]
    fn resolve_config_explicit_wins_even_if_missing() {
        // An explicit --config is used verbatim; existence is not checked here
        // (a missing explicit file is a hard error upstream, never a fallback).
        let home = tempfile::tempdir().unwrap();
        let cwd = tempfile::tempdir().unwrap();
        std::fs::write(cwd.path().join("cola.toml"), "").unwrap();
        let p = resolve_config_path(Some("/custom/cola.toml"), cwd.path(), home.path());
        assert_eq!(p.unwrap(), PathBuf::from("/custom/cola.toml"));
    }

    #[test]
    fn resolve_config_prefers_cwd_over_home() {
        let home = tempfile::tempdir().unwrap();
        let cwd = tempfile::tempdir().unwrap();
        std::fs::write(cwd.path().join("cola.toml"), "").unwrap();
        let home_config = crate::state::StateLayout::for_home(home.path()).config_file();
        std::fs::create_dir_all(home_config.parent().unwrap()).unwrap();
        std::fs::write(home_config, "").unwrap();
        let p = resolve_config_path(None, cwd.path(), home.path());
        assert_eq!(p.unwrap(), cwd.path().join("cola.toml"));
    }

    #[test]
    fn resolve_config_falls_back_to_home_when_cwd_missing() {
        let home = tempfile::tempdir().unwrap();
        let cwd = tempfile::tempdir().unwrap();
        let home_config = crate::state::StateLayout::for_home(home.path()).config_file();
        std::fs::create_dir_all(home_config.parent().unwrap()).unwrap();
        std::fs::write(&home_config, "").unwrap();
        let p = resolve_config_path(None, cwd.path(), home.path());
        assert_eq!(p.unwrap(), home_config);
    }

    #[test]
    fn resolve_config_none_when_nothing_exists() {
        let home = tempfile::tempdir().unwrap();
        let cwd = tempfile::tempdir().unwrap();
        assert_eq!(resolve_config_path(None, cwd.path(), home.path()), None);
    }

    // ---- Agent clients (ADR-0090 decision 5) ----

    fn minimal() -> &'static str {
        "[feishu]\napp_id = \"cli_test\"\napp_secret = \"secret\"\n"
    }

    /// The additive rule: `[opencode]` alone is one client named `default`,
    /// kind `opencode`, carrying exactly today's settings.
    #[test]
    fn opencode_only_resolves_to_the_default_opencode_client() {
        let cfg: Config = toml::from_str(&format!(
            "{}\n[opencode]\nurl = \"http://localhost:4111\"\nmodel = \"opencode/x\"\n\
             start_server = \"never\"\ngeneration = \"v2\"\n",
            minimal()
        ))
        .unwrap();
        let active = cfg.active_client().unwrap();
        assert_eq!(active.id, DEFAULT_CLIENT_ID);
        assert_eq!(active.id, "default");
        assert_eq!(active.kind, ClientKind::Opencode);
        assert_eq!(active.settings, ClientSettings::Opencode(cfg.opencode.clone()));
        assert_eq!(active.opencode(), Some(&cfg.opencode));
        assert_eq!(active.opencode().unwrap().preferred_port(), Some(4111));
    }

    /// No `[opencode]` table at all: the implicit client still exists, with the
    /// default settings — exactly what cola does today.
    #[test]
    fn a_config_without_opencode_still_has_the_default_client() {
        let cfg: Config = toml::from_str(minimal()).unwrap();
        let active = cfg.active_client().unwrap();
        assert_eq!(active.id, "default");
        assert_eq!(active.kind, ClientKind::Opencode);
        assert_eq!(active.opencode(), Some(&OpenCodeConfig::default()));
    }

    #[test]
    fn an_explicit_default_client_resolves_to_the_implicit_client() {
        let cfg: Config = toml::from_str(&format!(
            "{}\n[bridge]\ndefault_client = \"default\"\n",
            minimal()
        ))
        .unwrap();
        let active = cfg.active_client().unwrap();
        assert_eq!(active.id, DEFAULT_CLIENT_ID);
        assert_eq!(active.kind, ClientKind::Opencode);
    }

    #[test]
    fn default_client_selects_a_named_codex_client() {
        let cfg: Config = toml::from_str(&format!(
            "{}\n[bridge]\ndefault_client = \"ada\"\n\n[clients.ada]\nkind = \"codex\"\n",
            minimal()
        ))
        .unwrap();
        let active = cfg.active_client().unwrap();
        assert_eq!(active.id, "ada");
        assert_eq!(active.kind, ClientKind::Codex);
        assert_eq!(active.settings, ClientSettings::Codex);
        assert_eq!(
            active.opencode(),
            None,
            "a codex client carries no opencode settings"
        );
    }

    /// A named `opencode` client has its own options; it never inherits
    /// `[opencode]`. Resolving folds them into the process's effective view.
    #[test]
    fn a_named_opencode_client_carries_its_own_options() {
        let mut cfg: Config = toml::from_str(&format!(
            "{}\n[opencode]\nmodel = \"opencode/default\"\n\n[bridge]\n\
             default_client = \"work\"\n\n[clients.work]\nkind = \"opencode\"\n\
             model = \"opencode/work\"\nstart_server = \"never\"\n",
            minimal()
        ))
        .unwrap();
        let active = cfg.active_client().unwrap();
        assert_eq!(active.id, "work");
        assert_eq!(active.kind, ClientKind::Opencode);
        let settings = active.opencode().unwrap();
        assert_eq!(settings.model.as_deref(), Some("opencode/work"));
        assert_eq!(settings.start_server, ServerStartPolicy::Never);

        let resolved = cfg.resolve_active_client().unwrap();
        assert_eq!(resolved, active);
        assert_eq!(
            cfg.opencode.model.as_deref(),
            Some("opencode/work"),
            "the effective view follows the active client"
        );
    }

    /// `[opencode]` is the implicit `default`, so an explicit table under that
    /// id is a duplicate — deterministically an error, never precedence.
    #[test]
    fn clients_default_is_a_duplicate_of_the_implicit_client() {
        let cfg: Config = toml::from_str(&format!(
            "{}\n[opencode]\nmodel = \"opencode/x\"\n\n[clients.default]\nkind = \"opencode\"\n",
            minimal()
        ))
        .unwrap();
        let err = cfg.active_client().unwrap_err();
        assert_eq!(err, ClientConfigError::DuplicateDefaultClient);
        let message = err.to_string();
        assert!(message.contains("clients.default"), "{message}");
        assert!(message.contains("[opencode]"), "{message}");

        // The collision is about the id namespace, not about which client is
        // active: naming another client does not make it resolvable.
        let cfg: Config = toml::from_str(&format!(
            "{}\n[bridge]\ndefault_client = \"ada\"\n\n[clients.default]\nkind = \"opencode\"\n\
             \n[clients.ada]\nkind = \"codex\"\n",
            minimal()
        ))
        .unwrap();
        assert_eq!(
            cfg.active_client().unwrap_err(),
            ClientConfigError::DuplicateDefaultClient
        );
    }

    #[test]
    fn an_unknown_default_client_names_the_key() {
        let cfg: Config =
            toml::from_str(&format!("{}\n[bridge]\ndefault_client = \"ada\"\n", minimal())).unwrap();
        let err = cfg.active_client().unwrap_err();
        assert_eq!(
            err,
            ClientConfigError::UnknownDefaultClient {
                id: "ada".into(),
                available: "default".into(),
            }
        );
        let message = err.to_string();
        assert!(message.contains("bridge.default_client"), "{message}");
        assert!(message.contains("ada"), "{message}");
    }

    #[test]
    fn a_client_without_a_kind_names_the_key() {
        let cfg: Config = toml::from_str(&format!(
            "{}\n[bridge]\ndefault_client = \"ada\"\n\n[clients.ada]\nmodel = \"opencode/x\"\n",
            minimal()
        ))
        .unwrap();
        let err = cfg.active_client().unwrap_err();
        assert_eq!(err, ClientConfigError::MissingKind { id: "ada".into() });
        assert!(
            err.to_string().contains("clients.ada.kind"),
            "the error names the key: {err}"
        );
    }

    /// A table the active client does not name is still validated: an invalid
    /// client would otherwise only fail on the day it is switched to.
    #[test]
    fn an_inert_client_table_is_still_validated() {
        let missing: Config =
            toml::from_str(&format!("{}\n[clients.ada]\nmodel = \"opencode/x\"\n", minimal())).unwrap();
        assert_eq!(
            missing.active_client().unwrap_err(),
            ClientConfigError::MissingKind { id: "ada".into() }
        );

        let unknown: Config =
            toml::from_str(&format!("{}\n[clients.ada]\nkind = \"acp\"\n", minimal())).unwrap();
        assert_eq!(
            unknown.active_client().unwrap_err(),
            ClientConfigError::UnknownKind {
                id: "ada".into(),
                kind: "acp".into(),
            }
        );
    }

    #[test]
    fn an_unknown_client_kind_names_the_key() {
        let cfg: Config = toml::from_str(&format!(
            "{}\n[bridge]\ndefault_client = \"ada\"\n\n[clients.ada]\nkind = \"acp\"\n",
            minimal()
        ))
        .unwrap();
        let err = cfg.active_client().unwrap_err();
        assert_eq!(
            err,
            ClientConfigError::UnknownKind {
                id: "ada".into(),
                kind: "acp".into(),
            }
        );
        let message = err.to_string();
        assert!(message.contains("clients.ada.kind"), "{message}");
        assert!(message.contains("acp"), "{message}");
    }

    /// `[clients.ada.opencode]` is a nested table, not an option: it must fail
    /// naming the key rather than being swallowed by a flatten and leaving
    /// resolution on defaults (review finding on #872).
    #[test]
    fn a_nested_client_options_table_is_rejected() {
        let cfg: Config = toml::from_str(&format!(
            "{}\n[bridge]\ndefault_client = \"ada\"\n\n[clients.ada]\nkind = \"opencode\"\n\
             \n[clients.ada.opencode]\nmodel = \"provider/model\"\n",
            minimal()
        ))
        .unwrap();
        let err = cfg.active_client().unwrap_err();
        assert_eq!(
            err,
            ClientConfigError::UnknownSetting {
                id: "ada".into(),
                key: "opencode".into(),
            }
        );
        let message = err.to_string();
        assert!(message.contains("clients.ada.opencode"), "{message}");
    }

    /// An unknown scalar is rejected too, naming the setting and the client.
    #[test]
    fn an_unknown_client_setting_is_rejected() {
        let cfg: Config = toml::from_str(&format!(
            "{}\n[bridge]\ndefault_client = \"ada\"\n\n[clients.ada]\nkind = \"opencode\"\n\
             model_typo = \"x\"\n",
            minimal()
        ))
        .unwrap();
        let err = cfg.active_client().unwrap_err();
        assert_eq!(
            err,
            ClientConfigError::UnknownSetting {
                id: "ada".into(),
                key: "model_typo".into(),
            }
        );
        assert!(err.to_string().contains("clients.ada.model_typo"), "{err}");
    }

    /// Every `[opencode]` option key is accepted inside a client table, and
    /// lands in that Agent Client's settings.
    #[test]
    fn every_opencode_option_key_is_accepted_in_a_client_table() {
        let cfg: Config = toml::from_str(&format!(
            "{}\n[bridge]\ndefault_client = \"ada\"\n\n[clients.ada]\nkind = \"opencode\"\n\
             url = \"http://localhost:4222\"\nmodel = \"provider/model\"\n\
             start_server = \"never\"\ngeneration = \"v2\"\n",
            minimal()
        ))
        .unwrap();
        let settings = cfg.active_client().unwrap().opencode().cloned().unwrap();
        assert_eq!(settings.url.as_deref(), Some("http://localhost:4222"));
        assert_eq!(settings.model.as_deref(), Some("provider/model"));
        assert_eq!(settings.start_server, ServerStartPolicy::Never);
        assert_eq!(settings.generation, GenerationOverride::V2);
    }

    /// The hand-written key list must match `OpenCodeConfig`'s fields, so a new
    /// option cannot become an "unknown setting" the day it is added.
    #[test]
    fn the_option_key_list_matches_the_opencode_config() {
        use std::collections::BTreeSet;

        let full = OpenCodeConfig {
            url: Some("http://localhost:4222".into()),
            model: Some("provider/model".into()),
            start_server: ServerStartPolicy::Never,
            generation: GenerationOverride::V2,
        };
        let toml::Value::Table(serialised) = toml::Value::try_from(&full).unwrap() else {
            panic!("a config struct serialises to a table");
        };
        let declared: BTreeSet<&str> = OPENCODE_CLIENT_KEYS.iter().copied().collect();
        let fields: BTreeSet<&str> = serialised.keys().map(String::as_str).collect();
        assert_eq!(
            declared, fields,
            "OPENCODE_CLIENT_KEYS must list every OpenCodeConfig field"
        );
    }

    /// Reading resolves but never writes: neither `load` nor resolution may
    /// rewrite the user's hand-maintained file.
    #[test]
    fn reading_a_config_never_rewrites_it() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("cola.toml");
        let original = format!(
            "{}\n[opencode]\nmodel = \"opencode/x\"\n\n[bridge]\ndefault_client = \"ada\"\n\
             \n[clients.ada]\nkind = \"codex\"\n",
            minimal()
        );
        std::fs::write(&path, &original).unwrap();

        let mut cfg = load(&path).unwrap();
        cfg.resolve_active_client().unwrap();

        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            original,
            "reading must leave the file byte-identical"
        );
    }

    /// `cola config validate` parses AND resolves, so a broken client table is
    /// caught by the same command that check-parses the file.
    #[test]
    fn config_run_validate_checks_the_active_client() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("cola.toml");
        std::fs::write(
            &path,
            format!("{}\n[bridge]\ndefault_client = \"nope\"\n", minimal()),
        )
        .unwrap();
        let err = run(ConfigAction::Validate, path.to_str()).unwrap_err();
        assert!(
            err.to_string().contains("bridge.default_client"),
            "expected the resolution error, got: {err}"
        );
    }

    /// `cola config show` renders declared clients (they are not dropped), and
    /// an empty map adds no `[clients]` table to the output.
    #[test]
    fn masked_config_toml_renders_declared_clients() {
        let bare: Config = toml::from_str(minimal()).unwrap();
        let dumped = masked_config_toml(&bare).unwrap();
        assert!(!dumped.contains("[clients"), "no empty table: {dumped}");

        let cfg: Config =
            toml::from_str(&format!("{}\n[clients.ada]\nkind = \"codex\"\n", minimal())).unwrap();
        let dumped = masked_config_toml(&cfg).unwrap();
        let clients = dumped
            .split("[clients.ada]")
            .nth(1)
            .unwrap_or_else(|| panic!("no clients table: {dumped}"));
        assert!(clients.contains("kind = \"codex\""), "{dumped}");
        assert!(
            !clients.contains("start_server") && !clients.contains("generation"),
            "a codex client has no opencode options to print: {dumped}"
        );

        let opencode: Config = toml::from_str(&format!(
            "{}\n[clients.work]\nkind = \"opencode\"\nmodel = \"opencode/work\"\n",
            minimal()
        ))
        .unwrap();
        let dumped = masked_config_toml(&opencode).unwrap();
        assert!(dumped.contains("model = \"opencode/work\""), "{dumped}");

        // A setting resolution rejects is still printed: `config show` must not
        // be the quiet half of the silent-drop bug.
        let nested: Config = toml::from_str(&format!(
            "{}\n[clients.ada]\nkind = \"opencode\"\n\n[clients.ada.opencode]\nmodel = \"p/m\"\n",
            minimal()
        ))
        .unwrap();
        let dumped = masked_config_toml(&nested).unwrap();
        assert!(
            dumped.contains("[clients.ada.opencode]"),
            "unknown settings stay visible: {dumped}"
        );
    }
}
