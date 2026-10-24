//! `cola status` and `cola doctor`: read-only introspection of a cola
//! installation (ADR-0081).
//!
//! The CLI's job is lifecycle and introspection; the Feishu slash commands own
//! session control. `status` is a fast, config-light snapshot — it resolves what
//! it can and reports the rest as absent instead of failing — while `doctor`
//! runs the environment checks a support request needs (config, Feishu
//! credentials, the client binary and runtime, the singleton lock, the
//! autostart binary path) and exits non-zero when a check fails.
//!
//! Both report the **active Agent Client** through the neutral
//! [`crate::backend::ServerLifecycle`] seam (ADR-0090 decisions 3/5): its id and
//! kind from the config, and its endpoint, attachment, capabilities and local
//! clients from the adapter's own [`crate::backend::ServerLifecycle::client_report`].
//! The one check that writes to a runtime cola does not own — a real turn
//! proving the configured provider works — is opt-in via `doctor --check-turn`.
//!
//! Neither command takes the Singleton Lock: they must work while cola runs, and
//! against a broken install where cola cannot run at all.

use std::path::{Path, PathBuf};

use crate::backend::Backend;
use crate::backend::lifecycle::{ClientReport, ClientState, TurnProbe, UserClient, UserClientState};
use crate::config::{self, Config};
use crate::update::InstallChannel;

/// Everything `cola status` prints, gathered from the machine. The pure
/// renderers take this by reference; gathering is the process/OS boundary.
#[derive(Debug, Clone, PartialEq)]
pub struct Status {
    pub version: String,
    pub channel: String,
    /// PID of the live cola daemon (the Singleton Lock holder), if any.
    pub running_pid: Option<i32>,
    pub autostart: AutostartStatus,
    /// Whether the bot is claimed; `None` when the access file could not be
    /// inspected (no config).
    pub claimed: Option<bool>,
    pub host: Option<String>,
    /// The access file's declared version when this build refused it. Reported
    /// instead of "unclaimed": the file may still name a Host, and the process
    /// must not write over it.
    pub access_unsupported: Option<String>,
    pub config_path: Option<PathBuf>,
    /// Whether the resolved config file parsed. `false` with a path present is
    /// a broken config; `false` with no path is "no config found".
    pub config_ok: bool,
    pub log_path: PathBuf,
    /// The `[opencode] generation` override spelling (`auto`/`v1`/`v2`),
    /// reported even when no server is running so a script can read it.
    pub generation_override: String,
    /// The active Agent Client (ADR-0090 decision 5): its configured id and
    /// kind, plus what this process could diagnose about it. `None` only when
    /// the config cannot resolve an active client.
    pub client: Option<ClientStatus>,
    pub server: Option<ServerStatus>,
}

/// The client-scoped rows `cola status` reports (ADR-0090 decision 3, ticket
/// #883): which client is active, which attachment was found, and what the
/// connection advertised. Neutral — no adapter type is named here.
#[derive(Debug, Clone, PartialEq)]
pub struct ClientStatus {
    /// The configured id (`default` for the implicit `[opencode]` client).
    pub id: String,
    /// The adapter `kind` (`opencode` / `codex`).
    pub kind: String,
    /// The endpoint the client targets, even when it is absent.
    pub endpoint: String,
    /// `attached` / `unattached` / `absent` / `unknown`, for scripts.
    pub state: String,
    /// The evidence for `state` — a daemon's report, or why it is absent.
    pub detail: String,
    /// The binary the runtime runs, when it can be learned.
    pub runtime_binary: Option<String>,
    /// The client binary a spawn would use, when it can be found.
    pub client_binary: Option<String>,
    pub version: Option<String>,
    pub platform: Option<String>,
    pub surface_source: Option<String>,
    pub capabilities: Vec<ClientCapability>,
    pub user_clients: Vec<ClientProcess>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ClientCapability {
    pub name: String,
    pub value: String,
}

/// One local process using the active client, and what is proven about it.
#[derive(Debug, Clone, PartialEq)]
pub struct ClientProcess {
    pub pid: i32,
    pub binary: String,
    /// `attached` / `embedded` / `unverified`.
    pub state: String,
    pub detail: String,
}

/// The registered boot-time launcher, as far as it can be read.
#[derive(Debug, Clone, PartialEq)]
pub struct AutostartStatus {
    pub registered: bool,
    pub active: bool,
    /// The binary the installed supervisor would start.
    pub binary: Option<PathBuf>,
}

/// The shared OpenCode server cola would attach to.
#[derive(Debug, Clone, PartialEq)]
pub struct ServerStatus {
    pub url: String,
    pub pid: i32,
    /// The generation the attach probe resolved (`v1`/`v2`), `None` when the
    /// probe was inconclusive (never guessed).
    pub generation: Option<String>,
    /// The probe's evidence — the detail line when the generation is unknown.
    pub evidence: String,
    /// Other eligible default-store servers cola is *not* attached to, in
    /// deterministic order. Non-empty means more than one shared server is
    /// running; cola picks one canonical and never aggregates (ADR-0085).
    pub other_servers: Vec<OtherServer>,
}

/// One eligible default-store server that is not the canonical pick.
#[derive(Debug, Clone, PartialEq)]
pub struct OtherServer {
    pub url: String,
    pub pid: i32,
}

/// The install channel as a stable CLI spelling.
fn channel_label(channel: InstallChannel) -> &'static str {
    match channel {
        InstallChannel::GitHub => "github",
        InstallChannel::Cargo => "cargo",
    }
}

/// The config's resolved path plus its parse result — the pair every
/// introspection command needs. Bundled so `gather`/`doctor_checks` take one
/// argument instead of three that always travel together.
pub struct ConfigState {
    pub path: Option<PathBuf>,
    loaded: Option<anyhow::Result<Config>>,
}

impl ConfigState {
    /// Resolve and load from the ambient environment (`cola config`'s lookup).
    /// A missing file is `loaded: None`; a broken one is `loaded: Some(Err)` —
    /// neither is an error here.
    pub fn from_env(explicit: Option<&str>) -> Self {
        let path = config::resolve_path_from_env(explicit);
        let loaded = path.as_deref().map(config::load);
        Self { path, loaded }
    }

    /// The parsed config, when one loaded cleanly.
    pub fn cfg(&self) -> Option<&Config> {
        self.loaded.as_ref().and_then(|r| r.as_ref().ok())
    }

    /// The active Agent Client, resolved leniently: `None` when the config
    /// cannot resolve one (broken table, unknown kind). The report narrows in
    /// that case; it never fails the command.
    pub fn active_client(&self) -> Option<config::ActiveClient> {
        self.cfg()?.active_client().ok()
    }

    /// The active Agent Client's effective OpenCode settings — the same fold
    /// [`Config::resolve_active_client`] applies at boot.
    ///
    /// Deliberately lenient: `None` when the config cannot resolve an active
    /// client (broken table, unknown kind) or the active kind is not OpenCode.
    /// `status`/`doctor` narrow their report in that case rather than failing,
    /// and they must never probe the raw `[opencode]` table when a named
    /// `kind = "opencode"` client is active: its `url`/`generation` are the
    /// ones boot would use (spec #860, ticket #872/#875).
    pub fn effective_opencode(&self) -> Option<config::OpenCodeConfig> {
        self.active_client()?.opencode().cloned()
    }

    /// Whether a config file was found AND parsed.
    pub fn ok(&self) -> bool {
        matches!(self.loaded, Some(Ok(_)))
    }
}

/// Gather the status snapshot. Its absence only narrows what can be reported
/// (claim state, the generation override), never fails the command.
///
/// `client` is the active client built by the composition root (`src/main.rs`,
/// which may name the adapter); the report reads its attachment through the
/// neutral [`ServerLifecycle::attachment_status`] seam, so `cola status` never
/// spells an adapter type (spec #860, ticket #875).
pub async fn gather(state: &ConfigState, log_path: &Path, client: Option<&dyn Backend>) -> Status {
    let cfg = state.cfg();
    let channel = crate::update::current_exe_path()
        .map(|exe| channel_label(crate::update::detect_install_channel(&exe)))
        .unwrap_or("unknown");
    let autostart = AutostartStatus {
        registered: crate::autostart::is_registered(),
        active: crate::autostart::supervisor_is_active(),
        binary: crate::autostart::supervisor_binary_path(),
    };
    let (claimed, host, access_unsupported) = match cfg {
        Some(cfg) => {
            let list = crate::bridge::access::AccessList::load_read_only(&cfg.bridge.access_file);
            match list.unsupported_version() {
                Some(declared) => (None, None, Some(declared.to_string())),
                None => (Some(list.is_claimed()), list.host().map(str::to_string), None),
            }
        }
        None => (None, None, None),
    };
    let server = probe_server(client).await;
    let generation_override = effective_generation_override(state);
    let active_client = gather_client(state, client).await;

    Status {
        version: crate::version::display_version(),
        channel: channel.to_string(),
        running_pid: crate::running_daemon_pid(),
        autostart,
        claimed,
        host,
        access_unsupported,
        config_path: state.path.clone(),
        config_ok: state.ok(),
        log_path: log_path.to_path_buf(),
        generation_override,
        client: active_client,
        server,
    }
}

/// The active client's row, from the config id/kind plus whatever the neutral
/// [`Backend::client_report`] could diagnose. No client built (a broken config,
/// or a kind this build cannot construct) still reports the id and kind the
/// config resolved, with the endpoint as `unknown` — the row is the point.
async fn gather_client(state: &ConfigState, client: Option<&dyn Backend>) -> Option<ClientStatus> {
    let active = state.active_client()?;
    let report = match client {
        Some(client) => client.client_report().await,
        None => None,
    };
    let (endpoint, state_label, detail) = match &report {
        Some(report) => (
            report.endpoint.clone(),
            client_state_label(&report.state).to_string(),
            report.detail.clone(),
        ),
        None => (
            "unknown".to_string(),
            "unknown".to_string(),
            "no report (no client could be built)".to_string(),
        ),
    };
    let capabilities = report
        .as_ref()
        .map(|report| {
            report
                .capabilities
                .iter()
                .map(|capability| ClientCapability {
                    name: capability.name.clone(),
                    value: capability.value.clone(),
                })
                .collect()
        })
        .unwrap_or_default();
    let user_clients = report
        .as_ref()
        .map(|report| report.user_clients.iter().map(client_process).collect())
        .unwrap_or_default();
    Some(ClientStatus {
        id: active.id,
        kind: active.kind.as_str().to_string(),
        endpoint,
        state: state_label,
        detail,
        runtime_binary: report.as_ref().and_then(|report| report.runtime_binary.clone()),
        client_binary: report.as_ref().and_then(|report| report.client_binary.clone()),
        version: report.as_ref().and_then(|report| report.version.clone()),
        platform: report.as_ref().and_then(|report| report.platform.clone()),
        surface_source: report.as_ref().and_then(|report| report.surface_source.clone()),
        capabilities,
        user_clients,
    })
}

fn client_state_label(state: &ClientState) -> &'static str {
    match state {
        ClientState::Attached => "attached",
        ClientState::Unattached => "unattached",
        ClientState::Absent => "absent",
    }
}

fn user_client_state_label(state: UserClientState) -> &'static str {
    match state {
        UserClientState::Attached => "attached",
        UserClientState::Embedded => "embedded",
        UserClientState::Unverified => "unverified",
    }
}

fn client_process(client: &UserClient) -> ClientProcess {
    ClientProcess {
        pid: client.pid,
        binary: client.binary.clone(),
        state: user_client_state_label(client.state).to_string(),
        detail: client.detail.clone(),
    }
}

/// Read the client's attachment for the report. `None` when there is nothing
/// to report or no client could be built (no config, another kind).
async fn probe_server(client: Option<&dyn Backend>) -> Option<ServerStatus> {
    let status = client?.attachment_status().await?;
    let other_servers = status
        .other_servers
        .into_iter()
        .map(|c| OtherServer {
            url: c.url,
            pid: c.pid,
        })
        .collect();
    Some(ServerStatus {
        url: status.url,
        pid: status.pid,
        generation: status.generation,
        evidence: status.evidence,
        other_servers,
    })
}

/// The generation label `cola status` prints (`auto`/`v1`/`v2`) for the
/// **active** Agent Client. A config that cannot resolve one falls back to the
/// raw `[opencode]` table, preserving the old lenient display.
fn effective_generation_override(state: &ConfigState) -> String {
    state
        .effective_opencode()
        .or_else(|| state.cfg().map(|cfg| cfg.opencode.clone()))
        .map(|settings| settings.generation)
        .unwrap_or_default()
        .to_string()
}

/// The human-readable `cola status` report.
pub fn render(s: &Status) -> String {
    let mut out = String::new();
    out.push_str(&format!("{}  ({} 通道)\n", s.version, s.channel));
    out.push_str(&format!("运行: {}\n", running_line(s)));
    out.push_str(&format!("自启动: {}\n", autostart_line(&s.autostart)));
    out.push_str(&format!("认领: {}\n", claimed_line(s)));
    out.push_str(&format!(
        "配置: {}\n",
        match &s.config_path {
            Some(p) if s.config_ok => p.display().to_string(),
            Some(p) => format!("{}（解析失败）", p.display()),
            None => "未找到（见 `cola.toml.example`）".to_string(),
        }
    ));
    out.push_str(&format!("日志: {}\n", s.log_path.display()));
    if let Some(client) = &s.client {
        out.push_str(&format!("客户端: {}\n", client_line(client)));
        if !client.capabilities.is_empty() {
            out.push_str(&format!("能力: {}\n", capability_line(&client.capabilities)));
        }
        if !client.user_clients.is_empty() {
            out.push_str(&format!("本地客户端: {}\n", process_list(&client.user_clients)));
        }
    }
    // The OpenCode row predates the client-scoped rows; keep it exactly as it
    // was for an OpenCode install, and omit it when another client is active
    // (printing "no shared OpenCode server" for a Codex client would be noise).
    if s.client.as_ref().is_none_or(|client| client.kind == "opencode") {
        out.push_str(&format!(
            "OpenCode: {}\n",
            server_line(s.server.as_ref(), &s.generation_override)
        ));
    }
    out
}

/// The client row: id, kind, endpoint and state.
fn client_line(client: &ClientStatus) -> String {
    let state = match client.state.as_str() {
        "attached" => "已连接",
        "unattached" => "未连接",
        "absent" => "未运行",
        _ => "未知",
    };
    format!("{} ({}) → {} [{state}]", client.id, client.kind, client.endpoint)
}

fn capability_line(capabilities: &[ClientCapability]) -> String {
    capabilities
        .iter()
        .map(|capability| format!("{}={}", capability.name, capability.value))
        .collect::<Vec<_>>()
        .join(", ")
}

fn process_list(clients: &[ClientProcess]) -> String {
    clients
        .iter()
        .map(|client| {
            let state = match client.state.as_str() {
                "attached" => "已连接",
                "embedded" => "内嵌服务器",
                _ => "未验证",
            };
            format!("{} (PID {}, {state})", client.binary, client.pid)
        })
        .collect::<Vec<_>>()
        .join(", ")
}

fn running_line(s: &Status) -> String {
    match s.running_pid {
        Some(pid) => format!("✅ 运行中 (PID {pid})"),
        None => "✗ 未运行".to_string(),
    }
}

fn autostart_line(a: &AutostartStatus) -> String {
    if !a.registered {
        return "✗ 未注册".to_string();
    }
    let state = if a.active { "active" } else { "inactive" };
    match &a.binary {
        Some(p) => format!("✅ 已注册 ({state}) → {}", p.display()),
        None => format!("✅ 已注册 ({state})"),
    }
}

fn claimed_line(s: &Status) -> String {
    if let Some(version) = &s.access_unsupported {
        return format!("⚠ 不支持的状态文件版本 {version}（access.json 未读取，且不会写入）");
    }
    match s.claimed {
        Some(true) => match &s.host {
            Some(h) => format!("✅ 机主 = {h}"),
            None => "✅ 已认领".to_string(),
        },
        Some(false) => "✗ 未认领（认领码在启动日志里）".to_string(),
        None => "? 未知（无配置）".to_string(),
    }
}

fn server_line(server: Option<&ServerStatus>, generation_override: &str) -> String {
    match server {
        None => format!("未发现共享服务器（LAZY 启动或 attach-only），覆盖 {generation_override}"),
        Some(s) => {
            let canonical = match &s.generation {
                Some(g) => format!(
                    "{} (PID {}, 世代 {}，覆盖 {})",
                    s.url, s.pid, g, generation_override
                ),
                None => format!(
                    "{} (PID {}, 世代未知：{}；覆盖 {})",
                    s.url, s.pid, s.evidence, generation_override
                ),
            };
            if s.other_servers.is_empty() {
                canonical
            } else {
                format!(
                    "{canonical}\n其他 OpenCode 服务器（未连接）: {}",
                    server_list(&s.other_servers)
                )
            }
        }
    }
}

/// `url (PID n), …` for the servers cola is not attached to.
fn server_list(servers: &[OtherServer]) -> String {
    servers
        .iter()
        .map(|o| format!("{} (PID {})", o.url, o.pid))
        .collect::<Vec<_>>()
        .join(", ")
}

/// The `cola status --json` payload — the same facts, stable keys for scripts.
pub fn render_json(s: &Status) -> serde_json::Value {
    serde_json::json!({
        "version": s.version,
        "channel": s.channel,
        "running": s.running_pid,
        "autostart": {
            "registered": s.autostart.registered,
            "active": s.autostart.active,
            "binary": s.autostart.binary.as_ref().map(|p| p.display().to_string()),
        },
        "claimed": s.claimed,
        "host": s.host,
        "access_unsupported_version": s.access_unsupported,
        "config_path": s.config_path.as_ref().map(|p| p.display().to_string()),
        "config_ok": s.config_ok,
        "log_path": s.log_path.display().to_string(),
        "generation_override": s.generation_override,
        "client": s.client.as_ref().map(|client| serde_json::json!({
            "id": client.id,
            "kind": client.kind,
            "endpoint": client.endpoint,
            "state": client.state,
            "detail": client.detail,
            "runtime_binary": client.runtime_binary,
            "client_binary": client.client_binary,
            "version": client.version,
            "platform": client.platform,
            "surface_source": client.surface_source,
            "capabilities": client.capabilities.iter().map(|capability| serde_json::json!({
                "name": capability.name,
                "value": capability.value,
            })).collect::<Vec<_>>(),
            "user_clients": client.user_clients.iter().map(|user| serde_json::json!({
                "pid": user.pid,
                "binary": user.binary,
                "state": user.state,
                "detail": user.detail,
            })).collect::<Vec<_>>(),
        })),
        "opencode": s.server.as_ref().map(|s| serde_json::json!({
            "url": s.url,
            "pid": s.pid,
            "generation": s.generation,
            "evidence": s.evidence,
            "other_servers": s.other_servers.iter().map(|o| serde_json::json!({
                "url": o.url,
                "pid": o.pid,
            })).collect::<Vec<_>>(),
        })),
    })
}

/// One `cola doctor` check.
#[derive(Debug, Clone, PartialEq)]
pub struct Check {
    pub label: String,
    pub status: CheckStatus,
    pub detail: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CheckStatus {
    Pass,
    Warn,
    Fail,
}

impl Check {
    fn pass(label: impl Into<String>, detail: impl Into<String>) -> Self {
        Self {
            label: label.into(),
            status: CheckStatus::Pass,
            detail: detail.into(),
        }
    }
    fn warn(label: impl Into<String>, detail: impl Into<String>) -> Self {
        Self {
            label: label.into(),
            status: CheckStatus::Warn,
            detail: detail.into(),
        }
    }
    fn fail(label: impl Into<String>, detail: impl Into<String>) -> Self {
        Self {
            label: label.into(),
            status: CheckStatus::Fail,
            detail: detail.into(),
        }
    }
}

/// Whether the report has no failing check (warnings do not fail the run).
pub fn doctor_ok(checks: &[Check]) -> bool {
    !checks.iter().any(|c| c.status == CheckStatus::Fail)
}

/// The human-readable `cola doctor` report, with a summary line.
pub fn render_doctor(checks: &[Check]) -> String {
    let mut out = String::from("cola doctor — 环境检查\n\n");
    for c in checks {
        let mark = match c.status {
            CheckStatus::Pass => "✅",
            CheckStatus::Warn => "⚠️",
            CheckStatus::Fail => "❌",
        };
        out.push_str(&format!("{mark} {}: {}\n", c.label, c.detail));
    }
    let pass = checks.iter().filter(|c| c.status == CheckStatus::Pass).count();
    let warn = checks.iter().filter(|c| c.status == CheckStatus::Warn).count();
    let fail = checks.iter().filter(|c| c.status == CheckStatus::Fail).count();
    out.push_str(&format!(
        "\n{} 项检查，{} 通过，{} 警告，{} 失败\n",
        checks.len(),
        pass,
        warn,
        fail
    ));
    out
}

/// Run `cola doctor`: gather the checks, print the report, and return whether
/// every check passed. `check_turn` is the explicit opt-in for the one check
/// that writes to the user's runtime (ADR-0090 decision 3, ticket #883).
pub async fn run_doctor(
    state: &ConfigState,
    log_path: &Path,
    client: Option<&dyn Backend>,
    check_turn: bool,
) -> anyhow::Result<bool> {
    let checks = doctor_checks(state, log_path, client, check_turn).await;
    print!("{}", render_doctor(&checks));
    Ok(doctor_ok(&checks))
}

/// Assemble the doctor checks from the resolved config state.
async fn doctor_checks(
    state: &ConfigState,
    log_path: &Path,
    client: Option<&dyn Backend>,
    check_turn: bool,
) -> Vec<Check> {
    let mut checks = Vec::new();
    let cfg = state.cfg();

    match (&state.path, &state.loaded) {
        (Some(p), Some(Ok(_))) => checks.push(Check::pass("配置文件", p.display().to_string())),
        (Some(p), Some(Err(e))) => {
            checks.push(Check::fail("配置文件", format!("{} 无法解析：{e}", p.display())))
        }
        _ => checks.push(Check::fail(
            "配置文件",
            "未找到（依次查找 ./cola.toml、~/.cola/cola.toml）".to_string(),
        )),
    }

    match cfg {
        Some(cfg) => {
            let client = crate::feishu::client::Client::new(cfg.feishu.clone());
            checks.push(feishu_check(&client, &cfg.feishu.app_id).await);
        }
        None => checks.push(Check::fail("飞书凭证", "无配置，无法检查".to_string())),
    }

    let active = state.active_client();
    if let Some(active) = &active {
        checks.push(Check::pass(
            "Agent 客户端",
            format!("{} (kind = {})", active.id, active.kind.as_str()),
        ));
    }
    match active.as_ref().map(|active| active.kind) {
        Some(config::ClientKind::Codex) => {
            let report = match client {
                Some(client) => client.client_report().await,
                None => None,
            };
            let turn = if check_turn {
                match client {
                    Some(client) => Some(client.probe_turn().await),
                    None => Some(TurnProbe::NotAttempted {
                        detail: "no client could be built".to_string(),
                    }),
                }
            } else {
                None
            };
            checks.extend(codex_client_checks(report.as_ref(), turn.as_ref()));
        }
        // The OpenCode rows are unchanged (ADR-0081): the client-scoped rows
        // are additive, and a broken/unsupported config keeps its old report.
        _ => {
            if check_turn {
                checks.push(Check::warn(
                    "回合检查",
                    "`--check-turn` 只适用于 Codex 客户端；OpenCode 的服务器探测已覆盖其可用性".to_string(),
                ));
            }
            checks.push(opencode_binary_check());

            let server = probe_server(client).await;
            match &server {
                Some(s) => match &s.generation {
                    Some(g) => checks.push(Check::pass(
                        "共享 OpenCode 服务器",
                        format!("{} (世代 {g})", s.url),
                    )),
                    None => checks.push(Check::warn(
                        "共享 OpenCode 服务器",
                        format!("{} 世代未知：{}", s.url, s.evidence),
                    )),
                },
                None => checks.push(Check::warn(
                    "共享 OpenCode 服务器",
                    "未发现（federated 服务器未运行；LAZY 启动或 attach-only）".to_string(),
                )),
            }
            checks.push(multiple_servers_check(server.as_ref()));
        }
    }

    checks.push(lock_check(crate::lock_state()));
    checks.push(log_check(log_path));

    match crate::autostart::supervisor_binary_path() {
        Some(bin) => match crate::update::current_exe_path() {
            Ok(current) if paths_equal(&bin, &current) => {
                checks.push(Check::pass("自启动二进制", bin.display().to_string()))
            }
            Ok(current) => checks.push(Check::warn(
                "自启动二进制",
                format!(
                    "注册指向 {}，当前为 {}——重新运行 `cola autostart enable`",
                    bin.display(),
                    current.display()
                ),
            )),
            // The registered path exists, but our own binary is not on disk (a
            // replaced/unlinked install): the comparison cannot be made, so
            // report a warning rather than dropping the check.
            Err(_) => checks.push(Check::warn(
                "自启动二进制",
                format!("注册指向 {}，但当前二进制不在磁盘上，无法比较", bin.display()),
            )),
        },
        // A registration exists but its executable could not be read (a
        // malformed unit/plist/Run value): the requested check must still
        // report, as a warning, rather than vanish from the list.
        None if crate::autostart::is_registered() => checks.push(Check::warn(
            "自启动二进制",
            "已注册，但无法从注册中解析出可执行路径".to_string(),
        )),
        None => {}
    }

    checks
}

/// The Codex client's checks (ADR-0090 decision 3, ticket #883): the client
/// binary exists, a daemon was found, this process is attached, the user's own
/// client is attached to it, and — only when explicitly requested — the
/// configured provider can complete a turn.
///
/// A proven embedded client is a **named failure**; an unverified sharing state
/// is a warning that says exactly what could not be established, never a pass.
fn codex_client_checks(report: Option<&ClientReport>, turn: Option<&TurnProbe>) -> Vec<Check> {
    let mut checks = Vec::new();
    let Some(report) = report else {
        checks.push(Check::warn(
            "Codex 二进制",
            "无法构建 Codex 客户端（缺少 CODEX_HOME 或 home 目录）".to_string(),
        ));
        checks.push(Check::warn("Codex daemon", "未知：没有客户端报告"));
        checks.push(Check::warn("Codex 连接", "未知：没有客户端报告"));
        checks.push(Check::warn("Codex 本地客户端", "未知：没有客户端报告"));
        checks.push(turn_check(turn));
        return checks;
    };

    match &report.client_binary {
        Some(binary) => checks.push(Check::pass("Codex 二进制", binary.clone())),
        None => checks.push(Check::warn(
            "Codex 二进制",
            "PATH 中未找到 `codex`（attach 到已有 daemon 不受影响；启动 daemon 或本地 TUI 需要它）"
                .to_string(),
        )),
    }

    let daemon_running = match &report.state {
        ClientState::Attached | ClientState::Unattached => true,
        ClientState::Absent => false,
    };
    let daemon_detail = match report.runtime_binary.as_deref() {
        Some(binary) => format!("{}；运行时二进制 {binary}", report.detail),
        None => report.detail.clone(),
    };
    checks.push(if daemon_running {
        Check::pass("Codex daemon", daemon_detail)
    } else {
        Check::warn(
            "Codex daemon",
            format!("未发现（LAZY 启动或 attach-only）：{}", report.detail),
        )
    });

    match &report.state {
        ClientState::Attached => checks.push(Check::pass(
            "Codex 连接",
            format!(
                "{}；{}",
                report
                    .version
                    .as_deref()
                    .map(|version| format!("app-server {version}"))
                    .unwrap_or_else(|| "版本未知".to_string()),
                report.surface_source.as_deref().unwrap_or("surface 未报告")
            ),
        )),
        ClientState::Unattached => checks.push(Check::fail(
            "Codex 连接",
            format!("daemon 存在但无法连接：{}", report.detail),
        )),
        ClientState::Absent => checks.push(Check::warn(
            "Codex 连接",
            format!("无 daemon 可连接：{}", report.detail),
        )),
    }

    checks.push(local_client_check(report));
    checks.push(turn_check(turn));
    checks
}

/// The user's own client: a proven embedded one is a failure; a proven
/// attached one passes; an unverifiable one is a warning that names the gap.
fn local_client_check(report: &ClientReport) -> Check {
    let embedded: Vec<&UserClient> = report
        .user_clients
        .iter()
        .filter(|client| client.state == UserClientState::Embedded)
        .collect();
    if !embedded.is_empty() {
        let listed = embedded
            .iter()
            .map(|client| format!("PID {} ({})：{}", client.pid, client.binary, client.detail))
            .collect::<Vec<_>>()
            .join("；");
        return Check::fail(
            "Codex 本地客户端",
            format!(
                "{listed}。它运行在自己的内嵌服务器上，与共享 daemon 不共享线程；请启动 \
                 `codex app-server daemon start` 并用 `--remote` 把你的客户端指向该 daemon"
            ),
        );
    }
    let unverified: Vec<&UserClient> = report
        .user_clients
        .iter()
        .filter(|client| client.state == UserClientState::Unverified)
        .collect();
    if !unverified.is_empty() {
        let listed = unverified
            .iter()
            .map(|client| format!("PID {} ({})", client.pid, client.binary))
            .collect::<Vec<_>>()
            .join("；");
        return Check::warn(
            "Codex 本地客户端",
            format!(
                "{listed} 未在 argv 中命名 daemon，无法从进程表证明它已连接到共享 daemon；\
                 用 `codex --remote unix://{}` 启动可被证实",
                report
                    .endpoint
                    .strip_prefix("unix://")
                    .unwrap_or(&report.endpoint)
            ),
        );
    }
    let attached: Vec<&UserClient> = report
        .user_clients
        .iter()
        .filter(|client| client.state == UserClientState::Attached)
        .collect();
    if !attached.is_empty() {
        let listed = attached
            .iter()
            .map(|client| format!("PID {} ({})", client.pid, client.binary))
            .collect::<Vec<_>>()
            .join("；");
        return Check::pass(
            "Codex 本地客户端",
            format!("{listed} 已连接到 {}", report.endpoint),
        );
    }
    Check::warn(
        "Codex 本地客户端",
        format!(
            "未发现本地 Codex 客户端；无法确认你的客户端已连接到 {}",
            report.endpoint
        ),
    )
}

/// The provider's real turn. Without `--check-turn` the check states the
/// opt-in rather than guessing, because the probe is a write on a daemon cola
/// does not own.
fn turn_check(turn: Option<&TurnProbe>) -> Check {
    match turn {
        None => Check::warn(
            "Codex 回合",
            "未检查；运行 `cola doctor --check-turn` 验证配置的 provider 能否完成一次真实回合".to_string(),
        ),
        Some(TurnProbe::Passed { detail }) => Check::pass("Codex 回合", detail.clone()),
        Some(TurnProbe::Failed { detail }) => Check::fail("Codex 回合", detail.clone()),
        Some(TurnProbe::NotAttempted { detail }) => Check::warn("Codex 回合", detail.clone()),
    }
}

/// Whether two binary paths resolve to the same file (symlinks followed).
fn paths_equal(a: &Path, b: &Path) -> bool {
    match (std::fs::canonicalize(a), std::fs::canonicalize(b)) {
        (Ok(x), Ok(y)) => x == y,
        _ => a == b,
    }
}

/// How long either Feishu probe may take before `doctor` calls the check a
/// failure. Without this, a service that accepts the connection but never
/// responds would hang the command instead of reporting (ADR-0081).
const FEISHU_CHECK_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

/// Verify the Feishu credentials by exchanging them for a tenant access token
/// and reading the bot's identity, each bounded by [`FEISHU_CHECK_TIMEOUT`].
/// Never reports the secret.
async fn feishu_check(client: &crate::feishu::client::Client, app_id: &str) -> Check {
    match tokio::time::timeout(FEISHU_CHECK_TIMEOUT, client.get_access_token()).await {
        Err(_) => Check::fail("飞书凭证", format!("app {app_id} 认证超时")),
        Ok(Err(e)) => Check::fail("飞书凭证", format!("app {app_id} 认证失败：{e}")),
        Ok(Ok(_)) => match tokio::time::timeout(FEISHU_CHECK_TIMEOUT, client.bot_open_id()).await {
            Err(_) => Check::fail("飞书凭证", format!("app {app_id} 读取 bot 信息超时")),
            Ok(Ok(bot)) => Check::pass("飞书凭证", format!("app {app_id}，bot {bot}")),
            // Both halves are the credential check (ADR-0081): a token that
            // works but a bot identity that does not means the app cannot read
            // its own bot — a failure, not a warning.
            Ok(Err(e)) => Check::fail(
                "飞书凭证",
                format!("app {app_id} 可取 token，但读取 bot 信息失败：{e}"),
            ),
        },
    }
}

/// Report whether more than one eligible default-store server is running.
/// cola attaches to exactly one (the canonical pick); the rest are invisible
/// to it, so a multi-server machine gets a WARNING naming both sides rather
/// than silently splitting real-time sync across processes (ADR-0085).
fn multiple_servers_check(server: Option<&ServerStatus>) -> Check {
    match server {
        None => Check::pass("多个默认存储服务器", "未发现共享服务器"),
        Some(s) if s.other_servers.is_empty() => {
            Check::pass("多个默认存储服务器", format!("仅 {} (PID {})", s.url, s.pid))
        }
        Some(s) => Check::warn(
            "多个默认存储服务器",
            format!(
                "canonical {} (PID {})；忽略 {}",
                s.url,
                s.pid,
                server_list(&s.other_servers)
            ),
        ),
    }
}

/// Whether `opencode` is available where cola would spawn it — the PATH an
/// autostart supervisor snapshots (ADR-0013).
fn opencode_binary_check() -> Check {
    match which_opencode() {
        Some(p) => Check::pass("opencode 二进制", p.display().to_string()),
        None => Check::warn(
            "opencode 二进制",
            "PATH 中未找到 `opencode`（eager/lazy 启动会失败；attach 到已有服务器不受影响）".to_string(),
        ),
    }
}

/// Locate an executable named `opencode` on `PATH` (plus `.exe` on Windows).
fn which_opencode() -> Option<PathBuf> {
    which_opencode_in(std::env::var_os("PATH").as_deref())
}

/// [`which_opencode`] over an explicit PATH string, so the lookup is unit-testable
/// without mutating the process environment (which would race sibling tests).
fn which_opencode_in(path: Option<&std::ffi::OsStr>) -> Option<PathBuf> {
    for dir in std::env::split_paths(path?) {
        for name in OPENCODE_BIN_NAMES {
            let candidate = dir.join(name);
            if crate::update::is_executable_file(&candidate) {
                return Some(candidate);
            }
        }
    }
    None
}

/// The executable names to look for on `PATH`. Windows resolves executables by
/// extension, so only `opencode.exe` counts there — an extensionless file named
/// `opencode` is not runnable and must not shadow it.
#[cfg(windows)]
const OPENCODE_BIN_NAMES: &[&str] = &["opencode.exe"];
#[cfg(not(windows))]
const OPENCODE_BIN_NAMES: &[&str] = &["opencode"];

/// Report the log file's state: present (with size), not yet created in an
/// existing directory, or in a directory that does not exist yet (created on
/// the next start).
fn log_check(path: &Path) -> Check {
    match std::fs::metadata(path) {
        // A directory at the log path can never be opened for appending — the
        // daemon would fail to start logging. A misconfigured `--log-file` is a
        // failure, not a pass.
        Ok(m) if m.is_dir() => Check::fail(
            "日志文件",
            format!("{} 是目录，无法作为日志文件写入", path.display()),
        ),
        Ok(m) => Check::pass("日志文件", format!("{}（{} 字节）", path.display(), m.len())),
        Err(_) if path.parent().is_some_and(Path::exists) => {
            Check::pass("日志文件", format!("{}（尚未创建）", path.display()))
        }
        Err(_) => Check::warn(
            "日志文件",
            format!("{} 所在目录不存在（启动时会创建）", path.display()),
        ),
    }
}

/// Report the Singleton Lock state (ADR-0081). A live cola holder is healthy;
/// a stale or foreign lock is a WARNING, not a silent pass — a stale file means
/// the last instance died uncleanly (the next start reclaims it), and a
/// non-cola owner is the PID-reuse hazard the takeover guards against.
fn lock_check(state: crate::LockState) -> Check {
    match state {
        crate::LockState::Absent => Check::pass("单例锁", "无运行实例".to_string()),
        crate::LockState::Held(pid) => Check::pass("单例锁", format!("由 PID {pid} 持有")),
        crate::LockState::Stale(pid) => {
            Check::warn("单例锁", format!("陈旧锁（PID {pid} 已不存在），下次启动会回收"))
        }
        crate::LockState::Foreign(pid) => {
            Check::warn("单例锁", format!("锁文件指向的 PID {pid} 不是 cola 进程"))
        }
        // The file exists but is unusable: a failure, because a start would have
        // to reclaim it and an operator must know the lock is corrupt.
        crate::LockState::Malformed => Check::fail(
            "单例锁",
            "锁文件内容无法解析（不是 PID）；启动会拒绝并要求手动删除该文件".to_string(),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `cola status` still reports URL, pid, generation and the other servers
    /// for an OpenCode attachment — read through the neutral lifecycle seam, so
    /// this module never names the adapter (spec #860, ticket #875).
    #[tokio::test]
    async fn probe_reads_the_attachment_through_the_client_seam() {
        let mut mock = crate::bridge::test_support::MockBackend::new(Vec::new());
        mock.attachment = Some(crate::backend::lifecycle::AttachmentStatus {
            url: "http://localhost:49374".into(),
            pid: 42,
            generation: Some("v2".into()),
            evidence: String::new(),
            other_servers: vec![crate::backend::lifecycle::OtherServer {
                url: "http://localhost:4096".into(),
                pid: 7,
            }],
        });

        let server = probe_server(Some(&mock)).await.expect("the attachment reports");

        assert_eq!(server.url, "http://localhost:49374");
        assert_eq!(server.pid, 42);
        assert_eq!(server.generation.as_deref(), Some("v2"));
        assert_eq!(
            server.other_servers,
            vec![OtherServer {
                url: "http://localhost:4096".into(),
                pid: 7,
            }]
        );
    }

    fn sample_status() -> Status {
        Status {
            version: "cola 0.13.1-dev main@abc1234".into(),
            channel: "github".into(),
            running_pid: Some(4321),
            autostart: AutostartStatus {
                registered: true,
                active: true,
                binary: Some(PathBuf::from("/usr/local/bin/cola")),
            },
            claimed: Some(true),
            host: Some("ou_abc".into()),
            access_unsupported: None,
            config_path: Some(PathBuf::from("/home/u/cola.toml")),
            config_ok: true,
            log_path: PathBuf::from("/home/u/.cola/cola.log"),
            generation_override: "auto".into(),
            client: Some(ClientStatus {
                id: "default".into(),
                kind: "opencode".into(),
                endpoint: "http://localhost:4096".into(),
                state: "attached".into(),
                detail: "attached (generation v2)".into(),
                runtime_binary: None,
                client_binary: None,
                version: None,
                platform: None,
                surface_source: None,
                capabilities: vec![ClientCapability {
                    name: "generation".into(),
                    value: "v2".into(),
                }],
                user_clients: Vec::new(),
            }),
            server: Some(ServerStatus {
                url: "http://localhost:4096".into(),
                pid: 99,
                generation: Some("v2".into()),
                evidence: String::new(),
                other_servers: Vec::new(),
            }),
        }
    }

    #[test]
    fn render_reports_every_fact() {
        let text = render(&sample_status());
        assert!(text.contains("cola 0.13.1-dev main@abc1234"));
        assert!(text.contains("github 通道"));
        assert!(text.contains("✅ 运行中 (PID 4321)"));
        assert!(text.contains("✅ 已注册 (active) → /usr/local/bin/cola"));
        assert!(text.contains("✅ 机主 = ou_abc"));
        assert!(text.contains("/home/u/cola.toml"));
        assert!(text.contains("/home/u/.cola/cola.log"));
        assert!(text.contains("http://localhost:4096 (PID 99, 世代 v2，覆盖 auto)"));
    }

    #[test]
    fn render_handles_absent_facts() {
        let s = Status {
            running_pid: None,
            autostart: AutostartStatus {
                registered: false,
                active: false,
                binary: None,
            },
            claimed: Some(false),
            host: None,
            config_path: None,
            config_ok: false,
            server: None,
            ..sample_status()
        };
        let text = render(&s);
        assert!(text.contains("✗ 未运行"));
        assert!(text.contains("✗ 未注册"));
        assert!(text.contains("✗ 未认领"));
        assert!(text.contains("未找到"));
        assert!(text.contains("未发现共享服务器"));
    }

    #[tokio::test]
    async fn gather_surfaces_an_unsupported_access_file() {
        let dir = tempfile::tempdir().unwrap();
        let access_path = dir.path().join("access.json");
        std::fs::write(&access_path, r#"{"version":3,"host":"ou_host"}"#).unwrap();
        let config_path = dir.path().join("cola.toml");
        std::fs::write(
            &config_path,
            format!(
                "[feishu]\napp_id = \"a\"\napp_secret = \"b\"\n\n[bridge]\naccess_file = \"{}\"\n",
                access_path.display().to_string().replace('\\', "/")
            ),
        )
        .unwrap();
        let state = ConfigState::from_env(config_path.to_str());
        let status = gather(&state, &dir.path().join("cola.log"), None).await;
        assert_eq!(status.access_unsupported.as_deref(), Some("3"));
        assert_eq!(status.claimed, None, "not silently unclaimed");
        assert_eq!(status.host, None);
    }

    #[test]
    fn render_surfaces_an_unsupported_access_file() {
        let s = Status {
            claimed: None,
            host: None,
            access_unsupported: Some("3".into()),
            ..sample_status()
        };
        let text = render(&s);
        assert!(
            text.contains("不支持的状态文件版本 3"),
            "the report names the unsupported version: {text}"
        );
        assert!(!text.contains("✗ 未认领"), "not silently unclaimed: {text}");
        let v = render_json(&s);
        assert_eq!(v["access_unsupported_version"], "3");
        assert!(v["claimed"].is_null());
    }

    #[test]
    fn render_shows_a_broken_config_path() {
        let s = Status {
            config_path: Some(PathBuf::from("/bad/cola.toml")),
            config_ok: false,
            ..sample_status()
        };
        assert!(render(&s).contains("/bad/cola.toml（解析失败）"));
    }

    #[test]
    fn render_reports_an_inconclusive_generation_with_evidence() {
        let s = Status {
            server: Some(ServerStatus {
                url: "http://localhost:49374".into(),
                pid: 7,
                generation: None,
                evidence: "GET /api/info -> 503 (migrating)".into(),
                other_servers: Vec::new(),
            }),
            ..sample_status()
        };
        let text = render(&s);
        assert!(text.contains("世代未知"));
        assert!(text.contains("503"));
    }

    #[test]
    fn render_json_carries_the_same_facts() {
        let v = render_json(&sample_status());
        assert_eq!(v["running"], 4321);
        assert_eq!(v["autostart"]["registered"], true);
        assert_eq!(v["autostart"]["binary"], "/usr/local/bin/cola");
        assert_eq!(v["claimed"], true);
        assert_eq!(v["generation_override"], "auto");
        assert_eq!(v["opencode"]["generation"], "v2");
        assert_eq!(v["opencode"]["pid"], 99);
    }

    #[test]
    fn render_json_nulls_absent_facts() {
        let v = render_json(&Status {
            running_pid: None,
            claimed: None,
            host: None,
            config_path: None,
            server: None,
            ..sample_status()
        });
        assert!(v["running"].is_null());
        assert!(v["claimed"].is_null());
        assert!(v["config_path"].is_null());
        assert!(v["opencode"].is_null());
        // The override is still readable with no server running.
        assert_eq!(v["generation_override"], "auto");
    }

    #[test]
    fn render_lists_other_default_store_servers() {
        let s = Status {
            server: Some(ServerStatus {
                url: "http://localhost:49374".into(),
                pid: 7,
                generation: Some("v2".into()),
                evidence: String::new(),
                other_servers: vec![OtherServer {
                    url: "http://localhost:4096".into(),
                    pid: 9,
                }],
            }),
            ..sample_status()
        };
        let text = render(&s);
        assert!(text.contains("http://localhost:49374 (PID 7, 世代 v2"));
        assert!(text.contains("其他 OpenCode 服务器（未连接）"));
        assert!(text.contains("http://localhost:4096 (PID 9)"));
    }

    #[test]
    fn render_json_carries_other_servers() {
        let s = Status {
            server: Some(ServerStatus {
                url: "http://localhost:49374".into(),
                pid: 7,
                generation: Some("v2".into()),
                evidence: String::new(),
                other_servers: vec![OtherServer {
                    url: "http://localhost:4096".into(),
                    pid: 9,
                }],
            }),
            ..sample_status()
        };
        let v = render_json(&s);
        assert_eq!(v["opencode"]["other_servers"][0]["url"], "http://localhost:4096");
        assert_eq!(v["opencode"]["other_servers"][0]["pid"], 9);
        // One server is still the normal case: an empty list, not a missing key.
        let one = render_json(&sample_status());
        assert_eq!(one["opencode"]["other_servers"], serde_json::json!([]));
    }

    /// The client-scoped rows (ticket #883) name the active client, its
    /// endpoint and the capabilities the connection advertised.
    fn codex_client_status() -> ClientStatus {
        ClientStatus {
            id: "ada".into(),
            kind: "codex".into(),
            endpoint: "unix:///home/u/.codex/app-server-control/app-server-control.sock".into(),
            state: "attached".into(),
            detail: "attached to the app-server daemon (version 0.163.0, unix/linux)".into(),
            runtime_binary: Some("/home/u/.codex/packages/app-server-daemon/current/bin/codex".into()),
            client_binary: Some("/usr/local/bin/codex".into()),
            version: Some("0.163.0".into()),
            platform: Some("unix/linux".into()),
            surface_source: Some("pinned 0.163.0".into()),
            capabilities: vec![
                ClientCapability {
                    name: "background_tasks".into(),
                    value: "unsupported".into(),
                },
                ClientCapability {
                    name: "surface".into(),
                    value: "pinned 0.163.0".into(),
                },
            ],
            user_clients: vec![ClientProcess {
                pid: 77,
                binary: "codex".into(),
                state: "attached".into(),
                detail: "shares the daemon".into(),
            }],
        }
    }

    /// A full `ClientReport` for the doctor checks, with one local client.
    fn codex_report(state: ClientState, user_clients: Vec<UserClient>) -> ClientReport {
        ClientReport {
            endpoint: "unix:///tmp/codex-home/app-server-control/app-server-control.sock".into(),
            state,
            detail: "daemon at /tmp/codex-home/app-server-control/app-server-control.sock".into(),
            runtime_binary: Some("/managed/bin/codex".into()),
            version: Some("0.163.0".into()),
            platform: Some("unix/linux".into()),
            surface_source: Some("pinned 0.163.0".into()),
            capabilities: Vec::new(),
            user_clients,
            client_binary: Some("/usr/bin/codex".into()),
        }
    }

    fn attached_user(pid: i32) -> UserClient {
        UserClient {
            pid,
            binary: "codex".into(),
            state: UserClientState::Attached,
            detail: "shares the daemon through unix:///tmp/codex-home/app-server-control.sock".into(),
        }
    }

    #[test]
    fn render_reports_the_active_client_and_its_capabilities() {
        let s = Status {
            client: Some(codex_client_status()),
            server: None,
            ..sample_status()
        };
        let text = render(&s);
        assert!(
            text.contains(
                "客户端: ada (codex) → unix:///home/u/.codex/app-server-control/app-server-control.sock [已连接]"
            ),
            "{text}"
        );
        assert!(text.contains("能力: background_tasks=unsupported"), "{text}");
        assert!(text.contains("本地客户端: codex (PID 77, 已连接)"), "{text}");
        assert!(
            !text.contains("OpenCode:"),
            "a Codex client does not print the OpenCode row: {text}"
        );
    }

    /// The OpenCode rows stay exactly as they were: the client row is additive.
    #[test]
    fn render_keeps_the_opencode_row_for_an_opencode_client() {
        let text = render(&sample_status());
        assert!(text.contains("客户端: default (opencode)"), "{text}");
        assert!(text.contains("OpenCode: http://localhost:4096"), "{text}");
    }

    #[test]
    fn render_json_names_the_active_client_kind_endpoint_and_capabilities() {
        let s = Status {
            client: Some(codex_client_status()),
            server: None,
            ..sample_status()
        };
        let v = render_json(&s);
        assert_eq!(v["client"]["id"], "ada");
        assert_eq!(v["client"]["kind"], "codex");
        assert_eq!(
            v["client"]["endpoint"],
            "unix:///home/u/.codex/app-server-control/app-server-control.sock"
        );
        assert_eq!(v["client"]["state"], "attached");
        assert_eq!(v["client"]["version"], "0.163.0");
        assert_eq!(v["client"]["surface_source"], "pinned 0.163.0");
        assert_eq!(v["client"]["capabilities"][0]["name"], "background_tasks");
        assert_eq!(v["client"]["capabilities"][0]["value"], "unsupported");
        assert_eq!(v["client"]["user_clients"][0]["state"], "attached");
        assert!(v["opencode"].is_null(), "the absent OpenCode row is intact");
    }

    #[tokio::test]
    async fn gather_names_the_active_client_without_a_probe() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("cola.toml");
        std::fs::write(
            &path,
            r#"
[feishu]
app_id = "a"
app_secret = "b"

[bridge]
default_client = "ada"

[clients.ada]
kind = "codex"
"#,
        )
        .unwrap();
        let state = ConfigState::from_env(path.to_str());
        let status = gather(&state, &dir.path().join("cola.log"), None).await;
        let client = status
            .client
            .as_ref()
            .expect("the config's active client is named");
        assert_eq!(client.id, "ada");
        assert_eq!(client.kind, "codex");
        assert_eq!(client.endpoint, "unknown");
        assert_eq!(client.state, "unknown");
        let v = render_json(&status);
        assert_eq!(v["client"]["id"], "ada");
        assert_eq!(v["client"]["kind"], "codex");
    }

    #[tokio::test]
    async fn gather_reads_the_client_report_through_the_seam() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("cola.toml");
        std::fs::write(
            &path,
            r#"
[feishu]
app_id = "a"
app_secret = "b"

[bridge]
default_client = "ada"

[clients.ada]
kind = "codex"
"#,
        )
        .unwrap();
        let state = ConfigState::from_env(path.to_str());
        let mut mock = crate::bridge::test_support::MockBackend::new(Vec::new());
        mock.client_report = Some(codex_report(ClientState::Attached, vec![attached_user(77)]));
        let status = gather(&state, &dir.path().join("cola.log"), Some(&mock)).await;
        let client = status.client.expect("the seam's report is used");
        assert_eq!(client.id, "ada");
        assert_eq!(client.kind, "codex");
        assert_eq!(
            client.endpoint,
            "unix:///tmp/codex-home/app-server-control/app-server-control.sock"
        );
        assert_eq!(client.state, "attached");
        assert_eq!(client.version.as_deref(), Some("0.163.0"));
        assert_eq!(client.user_clients[0].state, "attached");
    }

    /// A proven embedded client is a named FAIL (ADR-0090 decision 3): the
    /// user's TUI shares nothing, so cola would silently mirror nothing.
    #[test]
    fn an_embedded_codex_client_fails_doctor() {
        let report = codex_report(
            ClientState::Attached,
            vec![UserClient {
                pid: 9,
                binary: "codex".into(),
                state: UserClientState::Embedded,
                detail: "runs with `--no-daemon`, so its app-server is in-process".into(),
            }],
        );
        let checks = codex_client_checks(Some(&report), Some(&TurnProbe::Passed { detail: "ok".into() }));
        assert!(!doctor_ok(&checks), "{checks:?}");
        let local = checks
            .iter()
            .find(|check| check.label == "Codex 本地客户端")
            .expect("the local-client check");
        assert_eq!(local.status, CheckStatus::Fail);
        assert!(local.detail.contains("内嵌服务器"), "{}", local.detail);
        assert!(local.detail.contains("--remote"), "{}", local.detail);
    }

    /// An unverified sharing state is never a pass: it says exactly what the
    /// process table could not establish.
    #[test]
    fn an_unverified_codex_client_is_a_warning() {
        let report = codex_report(
            ClientState::Attached,
            vec![UserClient {
                pid: 9,
                binary: "codex".into(),
                state: UserClientState::Unverified,
                detail: "names no daemon".into(),
            }],
        );
        let checks = codex_client_checks(Some(&report), Some(&TurnProbe::Passed { detail: "ok".into() }));
        let local = checks
            .iter()
            .find(|check| check.label == "Codex 本地客户端")
            .expect("the local-client check");
        assert_eq!(local.status, CheckStatus::Warn);
        assert!(local.detail.contains("无法从进程表证明"), "{}", local.detail);
    }

    /// The acceptance case that a login-shaped check would miss: the provider
    /// fails every turn as a retryable `error`, so doctor must fail.
    #[test]
    fn a_retryable_provider_error_fails_doctor() {
        let report = codex_report(ClientState::Attached, vec![attached_user(77)]);
        let failed = TurnProbe::Failed {
            detail: "the configured provider cannot complete a turn (the server will retry): 401 \
                     Unauthorized"
                .into(),
        };
        let checks = codex_client_checks(Some(&report), Some(&failed));
        assert!(!doctor_ok(&checks), "{checks:?}");
        let turn = checks
            .iter()
            .find(|check| check.label == "Codex 回合")
            .expect("the turn check");
        assert_eq!(turn.status, CheckStatus::Fail);
        assert!(turn.detail.contains("will retry"), "{}", turn.detail);
    }

    /// The passing path: a daemon-attached client and a working provider are
    /// fully green, with no ChatGPT account state read anywhere.
    #[test]
    fn a_daemon_attached_client_with_a_working_provider_passes() {
        let report = codex_report(ClientState::Attached, vec![attached_user(77)]);
        let checks = codex_client_checks(
            Some(&report),
            Some(&TurnProbe::Passed {
                detail: "the configured provider completed a turn (thread thread-1)".into(),
            }),
        );
        assert!(doctor_ok(&checks), "{checks:?}");
        assert!(
            checks.iter().all(|check| check.status == CheckStatus::Pass),
            "every Codex check passes: {checks:?}"
        );
    }

    #[test]
    fn the_turn_check_states_its_opt_in_when_not_requested() {
        let report = codex_report(ClientState::Attached, vec![attached_user(77)]);
        let checks = codex_client_checks(Some(&report), None);
        let turn = checks
            .iter()
            .find(|check| check.label == "Codex 回合")
            .expect("the turn check");
        assert_eq!(turn.status, CheckStatus::Warn);
        assert!(turn.detail.contains("--check-turn"), "{}", turn.detail);
    }

    #[test]
    fn multiple_servers_check_warns_only_above_one() {
        assert_eq!(multiple_servers_check(None).status, CheckStatus::Pass);
        assert_eq!(
            multiple_servers_check(Some(&sample_status().server.unwrap())).status,
            CheckStatus::Pass
        );
        let many = ServerStatus {
            url: "http://localhost:49374".into(),
            pid: 7,
            generation: Some("v2".into()),
            evidence: String::new(),
            other_servers: vec![OtherServer {
                url: "http://localhost:4096".into(),
                pid: 9,
            }],
        };
        let check = multiple_servers_check(Some(&many));
        assert_eq!(check.status, CheckStatus::Warn);
        assert!(check.detail.contains("canonical http://localhost:49374 (PID 7)"));
        assert!(check.detail.contains("http://localhost:4096 (PID 9)"));
    }

    #[test]
    fn config_state_reports_missing_broken_and_valid() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("none.toml");
        let s = ConfigState::from_env(missing.to_str());
        assert_eq!(s.path.as_deref(), Some(missing.as_path()));
        assert!(s.cfg().is_none());
        assert!(!s.ok());

        let broken = dir.path().join("broken.toml");
        std::fs::write(&broken, "not = valid = toml").unwrap();
        let s = ConfigState::from_env(broken.to_str());
        assert!(s.cfg().is_none());
        assert!(!s.ok(), "a path that fails to parse is not ok");

        let good = dir.path().join("good.toml");
        std::fs::write(&good, "[feishu]\napp_id = \"a\"\napp_secret = \"b\"\n").unwrap();
        let s = ConfigState::from_env(good.to_str());
        assert!(s.cfg().is_some());
        assert!(s.ok());
    }

    /// The report's generation override follows the **active** Agent Client
    /// (#872/#875): with `default_client = "ada"`, `[opencode] generation =
    /// "v1"` must not leak into the line — `ada`'s `v2` is what boot and the
    /// probe use.
    #[test]
    fn the_reported_generation_override_is_the_active_clients() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("cola.toml");
        std::fs::write(
            &path,
            r#"
[feishu]
app_id = "cli_test"
app_secret = "secret"

[bridge]
default_client = "ada"

[opencode]
url = "http://localhost:1111"
generation = "v1"

[clients.ada]
kind = "opencode"
url = "http://localhost:2222"
generation = "v2"
"#,
        )
        .unwrap();

        let state = ConfigState::from_env(path.to_str());
        assert_eq!(effective_generation_override(&state), "v2");
    }

    #[test]
    fn lock_check_never_silently_passes_a_stale_or_foreign_lock() {
        assert_eq!(lock_check(crate::LockState::Absent).status, CheckStatus::Pass);
        assert_eq!(lock_check(crate::LockState::Held(42)).status, CheckStatus::Pass);
        // A dead PID must not read as a healthy pass.
        assert_eq!(lock_check(crate::LockState::Stale(42)).status, CheckStatus::Warn);
        assert_eq!(
            lock_check(crate::LockState::Foreign(42)).status,
            CheckStatus::Warn
        );
        // A malformed lock file is a failure, not a healthy pass.
        assert_eq!(lock_check(crate::LockState::Malformed).status, CheckStatus::Fail);
    }

    #[test]
    fn log_check_reports_present_and_missing() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("cola.log");
        std::fs::write(&file, b"hello\n").unwrap();
        assert_eq!(log_check(&file).status, CheckStatus::Pass);

        let never = dir.path().join("never.log");
        assert_eq!(log_check(&never).status, CheckStatus::Pass); // parent exists

        let gone = dir.path().join("gone").join("cola.log");
        assert_eq!(log_check(&gone).status, CheckStatus::Warn); // parent missing

        // A directory at the log path can never be appended to: a failure, not
        // a pass.
        let as_dir = dir.path().join("logs-dir");
        std::fs::create_dir(&as_dir).unwrap();
        assert_eq!(log_check(&as_dir).status, CheckStatus::Fail);
    }

    #[test]
    fn doctor_ok_fails_only_on_a_failing_check() {
        let pass = Check::pass("a", "fine");
        let warn = Check::warn("b", "meh");
        let fail = Check::fail("c", "broken");
        assert!(doctor_ok(&[pass.clone(), warn.clone()]));
        assert!(!doctor_ok(&[pass, fail]));
    }

    #[test]
    fn render_doctor_summarizes_all_three_kinds() {
        let checks = [
            Check::pass("配置", "/x/cola.toml"),
            Check::warn("服务器", "未发现"),
            Check::fail("飞书", "认证失败"),
        ];
        let text = render_doctor(&checks);
        assert!(text.contains("✅ 配置: /x/cola.toml"));
        assert!(text.contains("⚠️ 服务器: 未发现"));
        assert!(text.contains("❌ 飞书: 认证失败"));
        assert!(text.contains("3 项检查，1 通过，1 警告，1 失败"));
    }

    #[test]
    fn paths_equal_follows_existing_files_and_falls_back() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("cola");
        std::fs::write(&file, b"x").unwrap();
        assert!(paths_equal(&file, &file));
        assert!(!paths_equal(&file, &dir.path().join("other")));
    }

    #[test]
    fn which_opencode_finds_an_executable_on_path() {
        let dir = tempfile::tempdir().unwrap();
        let bin = dir.path().join(OPENCODE_BIN_NAMES[0]);
        std::fs::write(&bin, b"#!/bin/sh\n").unwrap();
        make_executable(&bin);
        assert_eq!(which_opencode_in(Some(dir.path().as_os_str())), Some(bin));
    }

    #[cfg(unix)]
    #[test]
    fn which_opencode_skips_a_non_executable_file() {
        // A same-named file without an execute bit cannot be spawned, so PATH
        // lookup must not report it as found.
        let dir = tempfile::tempdir().unwrap();
        let bin = dir.path().join(OPENCODE_BIN_NAMES[0]);
        std::fs::write(&bin, b"#!/bin/sh\n").unwrap();
        let mut perms = std::fs::metadata(&bin).unwrap().permissions();
        use std::os::unix::fs::PermissionsExt;
        perms.set_mode(0o644);
        std::fs::set_permissions(&bin, perms).unwrap();
        assert_eq!(which_opencode_in(Some(dir.path().as_os_str())), None);
    }

    #[test]
    fn which_opencode_returns_none_when_absent() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(which_opencode_in(Some(dir.path().as_os_str())), None);
        assert_eq!(which_opencode_in(None), None);
    }

    /// `chmod +x` where it exists; a no-op elsewhere.
    fn make_executable(path: &Path) {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = std::fs::metadata(path).unwrap().permissions();
            perms.set_mode(0o755);
            std::fs::set_permissions(path, perms).unwrap();
        }
        #[cfg(not(unix))]
        let _ = path;
    }
}
