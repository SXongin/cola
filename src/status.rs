//! `cola status` and `cola doctor`: read-only introspection of a cola
//! installation (ADR-0081).
//!
//! The CLI's job is lifecycle and introspection; the Feishu slash commands own
//! session control. `status` is a fast, config-light snapshot — it resolves what
//! it can and reports the rest as absent instead of failing — while `doctor`
//! runs the environment checks a support request needs (config, Feishu
//! credentials, the OpenCode binary and server, the singleton lock, the
//! autostart binary path) and exits non-zero when a check fails.
//!
//! Neither command takes the Singleton Lock: they must work while cola runs, and
//! against a broken install where cola cannot run at all.

use std::path::{Path, PathBuf};

use crate::bridge::attach;
use crate::bridge::discovery;
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
    pub config_path: Option<PathBuf>,
    /// Whether the resolved config file parsed. `false` with a path present is
    /// a broken config; `false` with no path is "no config found".
    pub config_ok: bool,
    pub log_path: PathBuf,
    /// The `[opencode] generation` override spelling (`auto`/`v1`/`v2`),
    /// reported even when no server is running so a script can read it.
    pub generation_override: String,
    pub server: Option<ServerStatus>,
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

    /// Whether a config file was found AND parsed.
    pub fn ok(&self) -> bool {
        matches!(self.loaded, Some(Ok(_)))
    }
}

/// Gather the status snapshot. Its absence only narrows what can be reported
/// (claim state, the generation override), never fails the command.
pub async fn gather(state: &ConfigState, log_path: &Path) -> Status {
    let cfg = state.cfg();
    let channel = crate::update::current_exe_path()
        .map(|exe| channel_label(crate::update::detect_install_channel(&exe)))
        .unwrap_or("unknown");
    let autostart = AutostartStatus {
        registered: crate::autostart::is_registered(),
        active: crate::autostart::supervisor_is_active(),
        binary: crate::autostart::supervisor_binary_path(),
    };
    let (claimed, host) = match cfg {
        Some(cfg) => {
            let list = crate::bridge::access::AccessList::load(&cfg.bridge.access_file);
            (Some(list.is_claimed()), list.host().map(str::to_string))
        }
        None => (None, None),
    };
    let server = probe_server(cfg).await;
    let generation_override = cfg.map(|c| c.opencode.generation).unwrap_or_default().to_string();

    Status {
        version: crate::version::display_version(),
        channel: channel.to_string(),
        running_pid: crate::running_daemon_pid(),
        autostart,
        claimed,
        host,
        config_path: state.path.clone(),
        config_ok: state.ok(),
        log_path: log_path.to_path_buf(),
        generation_override,
        server,
    }
}

/// Scan for a shared OpenCode server and probe its generation. `None` when
/// nothing is running on the shared store; a probe that cannot classify still
/// returns the candidate with `generation: None` and the evidence.
async fn probe_server(cfg: Option<&Config>) -> Option<ServerStatus> {
    let preferred_port = cfg.and_then(|c| c.opencode.preferred_port());
    let generation_override = cfg.map(|c| c.opencode.generation).unwrap_or_default();
    let candidates = discovery::scan_processes();
    let candidate = discovery::pick_server(&candidates, preferred_port, discovery::self_spawned_pid())?;
    let other_servers: Vec<OtherServer> = discovery::other_servers(&candidates, candidate)
        .into_iter()
        .map(|c| OtherServer {
            url: c.url(),
            pid: c.pid,
        })
        .collect();
    // The probe carries Basic credentials and the endpoint is always loopback,
    // so it must bypass any env proxy (ADR-0081) — `new_loopback`, not the
    // production attach transport.
    let transport = crate::opencode::transport::Transport::new_loopback(
        Some(&candidate.username),
        Some(&candidate.password),
        candidate.url(),
    );
    let (url, generation, evidence) =
        match attach::resolve_candidate_over(&transport, candidate, generation_override).await {
            Ok(resolved) => (
                resolved.url,
                Some(resolved.generation.as_str().to_string()),
                String::new(),
            ),
            Err(evidence) => (candidate.url(), None, evidence),
        };
    Some(ServerStatus {
        url,
        pid: candidate.pid,
        generation,
        evidence,
        other_servers,
    })
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
    out.push_str(&format!(
        "OpenCode: {}\n",
        server_line(s.server.as_ref(), &s.generation_override)
    ));
    out
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
        "config_path": s.config_path.as_ref().map(|p| p.display().to_string()),
        "config_ok": s.config_ok,
        "log_path": s.log_path.display().to_string(),
        "generation_override": s.generation_override,
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
/// every check passed.
pub async fn run_doctor(explicit_config: Option<&str>, log_path: &Path) -> anyhow::Result<bool> {
    let state = ConfigState::from_env(explicit_config);
    let checks = doctor_checks(&state, log_path).await;
    print!("{}", render_doctor(&checks));
    Ok(doctor_ok(&checks))
}

/// Assemble the doctor checks from the resolved config state.
async fn doctor_checks(state: &ConfigState, log_path: &Path) -> Vec<Check> {
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

    checks.push(opencode_binary_check());

    let server = probe_server(cfg).await;
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
            config_path: Some(PathBuf::from("/home/u/cola.toml")),
            config_ok: true,
            log_path: PathBuf::from("/home/u/.cola/cola.log"),
            generation_override: "auto".into(),
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
