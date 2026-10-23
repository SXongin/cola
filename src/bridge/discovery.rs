//! Discovery of the OpenCode server cola should attach to, and the command to
//! start its own when none is running.
//!
//! The invariant is the STORE: sessions are shared only when every client
//! (cola, OpenChamber, the CLI) reads the same data directory
//! (`~/.local/share/opencode`). cola therefore attaches to any OpenCode server
//! running on the default store (whoever started it — OpenChamber, a manual
//! `opencode serve`, another tool) and only starts its own when none exists.
//! Among those candidates a V2 Shared Service outranks everything (ADR-0085):
//! it is the process the default clients connect to, so attaching anywhere else
//! breaks the real-time sync they expect.
//!
//! The server's protocol generation is not discovery's call: it is learned by
//! probing at attach time (the adapter's generation probe, spec #364 §2). A V2
//! managed service (`serve --service`) carries no `--port` flag, so its port
//! and private password come from the registration file its state dir holds —
//! the official client's discovery contract.

use async_trait::async_trait;
use sysinfo::{Pid, ProcessRefreshKind, ProcessesToUpdate, System, UpdateKind};

use crate::bridge::attach;
use crate::config::{GenerationOverride, port_from_url};
use crate::opencode::strategy::Generation;
use crate::opencode::transport::Transport;
use crate::state::StateLayout;

/// What a discovered `opencode serve` process is to cola, in selection order.
///
/// A V2 **Shared Service** (`serve --service`) is the machine-wide rendezvous
/// every default OpenCode client (TUI, desktop, WebUI) uses, so it is always
/// the canonical pick when present. A **Coexistent** server is someone else's
/// classic `serve --port` (OpenChamber, a manual CLI). cola's **Owned** server
/// is the one it spawned itself. Selection priority is
/// Service > Coexistent > Owned (ADR-0085).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ServerClass {
    Service,
    Coexistent,
    Owned,
}

impl ServerClass {
    /// The selection rank: lower wins. Explicit rather than derived so
    /// reordering the variants can never silently change the priority.
    fn rank(self) -> u8 {
        match self {
            ServerClass::Service => 0,
            ServerClass::Coexistent => 1,
            ServerClass::Owned => 2,
        }
    }
}

/// An `opencode serve` process discovered on the machine.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServerCandidate {
    pub pid: i32,
    pub port: u16,
    pub password: String,
    /// The server's EFFECTIVE basic-auth username: `OPENCODE_SERVER_USERNAME`
    /// if the server sets it, else the server's own default `"opencode"` (see
    /// opencode `packages/opencode/src/server/auth.ts`). cola must send Basic
    /// auth with BOTH parts or the server answers 401 even when the password
    /// matches (`authorized()` checks username equality too).
    pub username: String,
    /// Whether the server reads the default store (the directory OpenCode
    /// itself resolves as its default data home — `$XDG_DATA_HOME` if set,
    /// else `~/.local/share` — on every platform).
    pub uses_default_store: bool,
    /// What the scan can tell about the process: a `--service` daemon is a
    /// [`ServerClass::Service`], anything else a [`ServerClass::Coexistent`].
    /// Owned-ness is not knowable from argv; [`Self::class_against`] resolves
    /// it against cola's recorded pid.
    pub class: ServerClass,
}

impl ServerCandidate {
    /// The loopback endpoint discovery reached this server on. The one
    /// spelling of a discovered server's URL (attach, reconnect and the
    /// config tiebreaker all go through it).
    pub(crate) fn url(&self) -> String {
        local_url(self.port)
    }

    /// The class for one selection pass: Owned when the recorded pid matches,
    /// otherwise what the scan found. A Shared Service is never Owned — cola
    /// may start and attach it, but it never records or reaps it (ADR-0085),
    /// so a stale/corrupt pid record must not turn the shared rendezvous into
    /// something cola may kill.
    pub fn class_against(&self, self_pid: Option<i32>) -> ServerClass {
        match self.class {
            ServerClass::Service => ServerClass::Service,
            _ if Some(self.pid) == self_pid => ServerClass::Owned,
            _ => ServerClass::Coexistent,
        }
    }

    /// Whether this process is a V2 Shared Service.
    pub fn is_service(&self) -> bool {
        self.class == ServerClass::Service
    }
}

/// The loopback URL of a server port (`http://localhost:4096`).
pub(crate) fn local_url(port: u16) -> String {
    format!("http://localhost:{port}")
}

/// The basic-auth username an `opencode serve` accepts. Mirrors the server's
/// own default (`auth.ts`): `OPENCODE_SERVER_USERNAME` if set, else `opencode`.
pub(crate) const DEFAULT_SERVER_USERNAME: &str = "opencode";

/// The fully-resolved OpenCode server cola talks to: endpoint + credentials.
/// Produced by `resolve_opencode_server` (attach to a discovered shared server,
/// or start cola's own) and consumed by the HTTP client. Unlike the old config
/// fields, this carries BOTH username and password — discovery always supplies
/// both, so the client never silently sends unauthenticated requests (the
/// server 401s without a username even when the password matches).
/// The struct stays nominally `pub` because the `Backend` trait's `reconnect`
/// takes it; its fields are uniformly `pub(crate)` — the whole crate is one
/// binary, so nothing here is an external interface.
#[derive(Debug, Clone)]
pub struct ResolvedServer {
    pub(crate) url: String,
    pub(crate) username: String,
    pub(crate) password: String,
    /// The pid of the attached server, when the attach path knew it: the
    /// neutral identity token the reconnect loop compares to notice a
    /// replacement on the same port (a new generation or a new password).
    pub(crate) pid: Option<i32>,
    /// The generation attach detection resolved for this server, with the
    /// `[opencode] generation` override applied (spec #364 §2). A property of
    /// the attachment — never guessed, never per-session (ADR-0055).
    pub(crate) generation: Generation,
}

/// The data directory OpenCode resolves as its default on every platform.
///
/// OpenCode uses the `xdg-basedir` package, which computes `$XDG_DATA_HOME` if
/// set and `~/.local/share` otherwise — on Linux, macOS AND Windows (macOS and
/// Windows do NOT get platform-conventional paths; a known opencode issue,
/// #8235, that was auto-closed unfixed). cola must mirror this exactly, so
/// `dirs::data_dir()` — which returns `~/Library/Application Support` on macOS
/// — is deliberately NOT used here.
fn default_data_home() -> std::path::PathBuf {
    std::env::var_os("XDG_DATA_HOME")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| {
            dirs::home_dir()
                .unwrap_or_else(|| std::path::PathBuf::from("."))
                .join(".local")
                .join("share")
        })
}

/// The effective basic-auth username of a server, from its environment lines
/// (`KEY=VALUE` pairs as sysinfo returns them). `OPENCODE_SERVER_USERNAME` if
/// set, else the server's own default `"opencode"`.
///
/// The `--port` branch of [`resolve_credentials`] consults this for every
/// lineage, because the generation is not known until the candidate is probed.
/// It is the effective username **only** on V1: a V2 server ignores
/// `OPENCODE_SERVER_USERNAME` and always accepts `opencode` (spec #364 §2), so
/// [`crate::bridge::attach::resolve_probe`] drops an env-derived username once the
/// candidate classifies as V2 — and the probe itself is authenticated with this
/// value first, which is #763.
fn username_from_env(env: &[String]) -> String {
    env.iter()
        .find_map(|kv| kv.strip_prefix("OPENCODE_SERVER_USERNAME="))
        .filter(|u| !u.is_empty())
        .map(|s| s.to_string())
        .unwrap_or_else(|| DEFAULT_SERVER_USERNAME.to_string())
}

/// The state home of a server process, from its environment lines: the
/// server's `XDG_STATE_HOME` if set, else `~/.local/state` (OpenCode's
/// xdg-basedir rule on every platform). Deliberately NOT cola's own
/// `XDG_STATE_HOME`: a managed service registers where the daemon's own
/// environment pointed it.
fn state_home_from_env(env: &[String]) -> std::path::PathBuf {
    env.iter()
        .find_map(|kv| kv.strip_prefix("XDG_STATE_HOME="))
        .filter(|v| !v.is_empty())
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| {
            dirs::home_dir()
                .unwrap_or_else(|| std::path::PathBuf::from("."))
                .join(".local")
                .join("state")
        })
}

/// The path a V2 managed service registers itself at under a state home:
/// `$XDG_STATE_HOME/opencode/service.json` (spec #364 §2). The one spelling of
/// the registration location, for a server's environment and for cola's own.
fn service_registration_path(state_home: &std::path::Path) -> std::path::PathBuf {
    state_home.join("opencode").join("service.json")
}

/// The state home cola's OWN environment resolves — the one a service cola
/// starts inherits, so this is where its registration lands. The same
/// xdg-basedir rule [`state_home_from_env`] applies to a server's environment,
/// applied to ours.
fn own_state_home() -> std::path::PathBuf {
    let env: Vec<String> = std::env::vars_os()
        .map(|(k, v)| format!("{}={}", k.to_string_lossy(), v.to_string_lossy()))
        .collect();
    state_home_from_env(&env)
}

/// Where the Shared Service cola starts (or adopts) registers itself.
fn own_service_registration_path() -> std::path::PathBuf {
    service_registration_path(&own_state_home())
}

/// The config home cola's own environment resolves (xdg-basedir:
/// `$XDG_CONFIG_HOME` if set, else `~/.config`). OpenCode keeps its own
/// `service.json` — the `disabled` flag and the daemon's persisted port and
/// password — there.
fn own_config_home() -> std::path::PathBuf {
    std::env::var_os("XDG_CONFIG_HOME")
        .filter(|v| !v.is_empty())
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| {
            dirs::home_dir()
                .unwrap_or_else(|| std::path::PathBuf::from("."))
                .join(".config")
        })
}

/// OpenCode's own service config: `$XDG_CONFIG_HOME/opencode/service.json`.
fn own_service_config_path() -> std::path::PathBuf {
    own_config_home().join("opencode").join("service.json")
}

/// A V2 managed service's registration file (`{id, version, url, pid,
/// password}`, mode 0600), the official client's discovery contract (spec #364
/// §2). Only the fields discovery needs are parsed; `id`/`version` are ignored.
#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize)]
struct ServiceRegistration {
    url: String,
    password: String,
    #[serde(default)]
    pid: Option<i32>,
}

/// Read a `serve --service` process's registration file and return its port
/// and private password. The file lives at
/// `$XDG_STATE_HOME/opencode/service.json` as the daemon's environment defines
/// it. A registration whose recorded pid is not the process being scanned is
/// stale (the daemon self-heals a replaced registration), so it is ignored
/// rather than attached to through another instance's credentials.
///
/// The username is NOT taken from anywhere: it is always `opencode` (the only
/// one V2 accepts).
fn service_registration_credentials(env: &[String], pid: i32) -> Option<(u16, String)> {
    let path = service_registration_path(&state_home_from_env(env));
    let raw = std::fs::read_to_string(&path).ok()?;
    let registration: ServiceRegistration = serde_json::from_str(&raw).ok()?;
    if registration.pid.is_some_and(|registered| registered != pid) {
        tracing::debug!(
            "ignoring stale V2 service registration {} (pid {:?} != {pid})",
            path.display(),
            registration.pid
        );
        return None;
    }
    let port = port_from_url(&registration.url)?;
    Some((port, registration.password))
}

/// The basic-auth password a classic `serve --port` process carries in its
/// environment. OpenCode's `Env.password` prefers `OPENCODE_PASSWORD` and falls
/// back to the legacy `OPENCODE_SERVER_PASSWORD`, so cola reads the same order:
/// a server started with only the preferred name would otherwise probe with an
/// empty password and 401 forever (issue #812).
fn password_from_env(env: &[String]) -> String {
    env.iter()
        .find_map(|kv| kv.strip_prefix("OPENCODE_PASSWORD="))
        .or_else(|| {
            env.iter()
                .find_map(|kv| kv.strip_prefix("OPENCODE_SERVER_PASSWORD="))
        })
        .unwrap_or("")
        .to_string()
}

/// Resolve one server process's attach credentials — port, username and
/// password — or `None` when discovery cannot reach it.
///
/// A classic `serve --port P` process carries its port in argv and its
/// credentials in the process environment (V1's model; see [`password_from_env`]
/// for the password's name precedence). A V2 managed service
/// (`serve --service`) has no `--port`; its registration file is the fallback
/// attach path (spec #364 §2), and its username is fixed at `opencode` so a
/// poisoned `OPENCODE_SERVER_USERNAME` cannot break the attach.
///
/// `--port 0` (`serve --stdio --port 0`, as `opencode --standalone` and
/// `opencode acp` start) is not an endpoint: the OS assigns the real port and
/// the server publishes it only on stdout, so `localhost:0` can never be
/// attached to. Such a process is not a candidate at all (issue #748).
fn resolve_credentials(args: &[String], env: &[String], pid: i32) -> Option<(u16, String, String)> {
    if let Some(port) = args
        .iter()
        .position(|a| a == "--port")
        .and_then(|idx| args.get(idx + 1))
        .and_then(|p| p.parse::<u16>().ok())
    {
        if port == 0 {
            return None;
        }
        let password = password_from_env(env);
        return Some((port, username_from_env(env), password));
    }
    if !args.iter().any(|a| a == "--service") {
        return None;
    }
    let (port, password) = service_registration_credentials(env, pid)?;
    Some((port, DEFAULT_SERVER_USERNAME.to_string(), password))
}

/// The sysinfo refresh shape for a server scan.
///
/// `ProcessRefreshKind::nothing()` defaults to `tasks: true`, which makes
/// sysinfo enumerate every thread of a process as its own `Process` (keyed by
/// TID, sharing the main thread's cmdline). An `opencode serve` process with N
/// threads would then appear as N identical server candidates, polluting the
/// reconcile's Coexistent detection and causing cola to kill its own Owned
/// Server (yield) thinking a coexistent one appeared. `without_tasks()`
/// keeps only real processes (one per Tgid). Kept as a named function so the
/// shape is unit-testable (`.tasks()` must be false).
fn server_scan_refresh() -> ProcessRefreshKind {
    ProcessRefreshKind::nothing()
        .without_tasks()
        .with_cmd(UpdateKind::Always)
        .with_environ(UpdateKind::Always)
}

/// The class a scan can infer from a server's command line. A V2 managed
/// service is the one with `--service` (its port and password live in the
/// registration file); a classic `serve --port` is Coexistent until the pid
/// record says otherwise.
fn class_from_args(args: &[String]) -> ServerClass {
    if args.iter().any(|a| a == "--service") {
        ServerClass::Service
    } else {
        ServerClass::Coexistent
    }
}

/// How many leading argv tokens may name the `opencode` entrypoint. An
/// interpreter-launched dev server is `node <path>/opencode ... serve` or
/// `bun <path>/opencode serve`, so the path sits at `argv[1]`; three tokens
/// leave room for an interpreter flag without letting an unrelated later
/// argument (say a `--cwd /home/opencode/...` path) stand in for the binary.
const SERVE_ARGV_HEAD: usize = 3;

/// Whether a process's argv is an `opencode … serve` invocation — the ONE
/// server-recognition predicate, shared by [`scan_processes`] and
/// [`is_live_opencode_serve`] so a discovered server and a live one can never
/// disagree (issue #812).
///
/// The entrypoint need not be `argv[0]`: `node /opt/opencode/dist/index.js
/// serve` and `bun …/opencode serve` are real dev launches. A `serve` that is
/// only being described on the command line (`--help` / `-h`) is not a server.
fn is_opencode_serve_args(args: &[String]) -> bool {
    if args.iter().any(|a| a == "--help" || a == "-h") {
        return false;
    }
    let names_opencode = args.iter().take(SERVE_ARGV_HEAD).any(|a| a.contains("opencode"));
    names_opencode && args.iter().any(|a| a == "serve")
}

/// Whether a candidate may be selected/attached at all: it must read the
/// default store, and port 0 is never an endpoint (issue #748).
fn is_eligible(candidate: &ServerCandidate) -> bool {
    candidate.uses_default_store && candidate.port != 0
}

/// Whether a process's environment lines (as sysinfo returns them) say it reads
/// the DEFAULT store: no `XDG_DATA_HOME`, or one equal to the default. Reading
/// another process's env is best-effort (macOS/Windows limit it to same-user
/// processes), so an unreadable env degrades to "assume default store".
fn env_uses_default_store(env: Option<&[String]>, default_store: &str) -> bool {
    match env.and_then(|lines| lines.iter().find_map(|kv| kv.strip_prefix("XDG_DATA_HOME="))) {
        None => true,
        Some(home) => home == default_store,
    }
}

/// Scan for running `opencode serve` processes. Thin I/O — the interesting
/// decisions live in [`select_server`].
///
/// Uses sysinfo so discovery works on every platform (there is no `/proc` on
/// macOS/Windows). cmdline and environ are refreshed on every call (`Always`)
/// so a restarted server's new port/password is picked up.
pub fn scan_processes() -> Vec<ServerCandidate> {
    let default_store = default_data_home().to_string_lossy().into_owned();
    let mut system = System::new();
    let refresh = server_scan_refresh();
    system.refresh_processes_specifics(ProcessesToUpdate::All, true, refresh);

    system
        .processes()
        .values()
        .filter_map(|proc_| {
            let args: Vec<String> = proc_
                .cmd()
                .iter()
                .map(|s| s.to_string_lossy().into_owned())
                .collect();
            let is_server = is_opencode_serve_args(&args);
            if !is_server {
                return None;
            }
            let env: Vec<String> = proc_
                .environ()
                .iter()
                .map(|s| s.to_string_lossy().into_owned())
                .collect();
            let pid = proc_.pid().as_u32() as i32;
            let (port, username, password) = resolve_credentials(&args, &env, pid)?;
            // A server reads the default store when the directory it uses
            // equals the default data home (unset means OpenCode used
            // `~/.local/share`).
            let uses_default_store = env_uses_default_store(Some(&env), &default_store);
            Some(ServerCandidate {
                pid,
                port,
                password,
                username,
                uses_default_store,
                class: class_from_args(&args),
            })
        })
        .collect()
}

/// Pick the server cola should attach to (ADR-0013, ADR-0085).
///
/// Only default-store servers are eligible (a custom-store server is someone
/// else's data — attaching would both break sharing and couple cola to that
/// runtime). A candidate on port 0 is never eligible: that port is not an
/// endpoint (see [`resolve_credentials`]), so it could only ever send the
/// reconnect loop into a probe that cannot succeed (issue #748).
///
/// Eligible candidates are ranked by class: **Shared Service > Coexistent
/// Server > Owned Server**. The V2 managed service is the rendezvous every
/// default client uses, so it wins even over a smaller-port Coexistent server.
/// Within the winning class the configured `[opencode] url` port wins, else the
/// lowest port. Deterministic — the reconnect loop never flaps between two
/// default-store servers.
pub fn pick_server(
    candidates: &[ServerCandidate],
    preferred_port: Option<u16>,
    self_pid: Option<i32>,
) -> Option<&ServerCandidate> {
    let mut eligible: Vec<&ServerCandidate> = candidates.iter().filter(|c| is_eligible(c)).collect();
    // Deterministic across scans: sysinfo enumerates processes in randomized
    // order, so taking `first()` unsorted would make the reconnect loop flap
    // between two servers of the same class (ADR-0013). The pid is the final
    // tiebreaker in the (pathological) case of two candidates on one port.
    eligible.sort_by(|a, b| {
        a.class_against(self_pid)
            .rank()
            .cmp(&b.class_against(self_pid).rank())
            .then_with(|| a.port.cmp(&b.port))
            .then_with(|| a.pid.cmp(&b.pid))
    });
    let winning_class = eligible.first()?.class_against(self_pid);
    let class: Vec<&ServerCandidate> = eligible
        .into_iter()
        .filter(|c| c.class_against(self_pid) == winning_class)
        .collect();
    preferred_port
        .and_then(|p| class.iter().find(|c| c.port == p).copied())
        .or_else(|| class.first().copied())
}

/// The eligible default-store servers the canonical pick leaves out, in
/// deterministic order (selection priority, then port, then pid). The
/// multi-server visibility of `reconcile`, `cola status` and `cola doctor`
/// reads this; when it is empty there is exactly one shared server.
pub fn other_servers<'a>(
    candidates: &'a [ServerCandidate],
    canonical: &ServerCandidate,
) -> Vec<&'a ServerCandidate> {
    let mut others: Vec<&ServerCandidate> = candidates
        .iter()
        .filter(|c| is_eligible(c) && c.pid != canonical.pid)
        .collect();
    others.sort_by(|a, b| {
        a.class
            .rank()
            .cmp(&b.class.rank())
            .then_with(|| a.port.cmp(&b.port))
            .then_with(|| a.pid.cmp(&b.pid))
    });
    others
}

/// Whether any default-store server is live in a fresh scan — whoever started
/// it, cola included. The eager boot uses this to decide "stay serverless"
/// after a start that did not classify: a V2 Shared Service writes no
/// `self-opencode.pid` (it is never owned, ADR-0085), so the old
/// self-pid-based guard would take the bot down at boot whenever a live
/// service's probe was temporarily inconclusive.
pub fn has_live_default_store_server() -> bool {
    scan_processes().iter().any(is_eligible)
}

/// A child process command cola builds before spawning it: the `opencode serve`
/// commands it starts its own servers with, and the `opencode service restart`
/// CLI it hands a service restart to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpawnCommand {
    pub args: Vec<String>,
    /// Env vars to set/override on the child process.
    pub env: Vec<(String, String)>,
    /// Env vars to strip from the inherited environment.
    pub remove_env: Vec<String>,
}

/// Build the `opencode serve` command for cola's own server.
///
/// The server must land on the DEFAULT store so sessions are shared with
/// OpenChamber / the CLI. cola's own process may have inherited an
/// `XDG_DATA_HOME` (e.g. from the launching shell), so the child explicitly
/// strips it instead of pinning a private data directory.
///
/// The child gets the password cola will probe with under the legacy name
/// `OPENCODE_SERVER_PASSWORD`, and the inherited `OPENCODE_PASSWORD` is
/// stripped: OpenCode's `Env.password` prefers `OPENCODE_PASSWORD`, so a cola
/// launched from a shell that exported it would otherwise hand the server the
/// inherited value while cola probes with `cola-secret` — and the attach would
/// 401 forever (#812 precedence; #810's fallback requires the `cola-secret`
/// path to actually work). The two names never both apply to the child: the
/// fallback is set and the preferred one is removed.
pub fn self_start_command(port: u16, password: &str) -> SpawnCommand {
    SpawnCommand {
        args: vec![
            "serve".to_string(),
            "--port".to_string(),
            port.to_string(),
            "--hostname".to_string(),
            "127.0.0.1".to_string(),
        ],
        env: vec![("OPENCODE_SERVER_PASSWORD".to_string(), password.to_string())],
        // The server MUST land on the DEFAULT store so sessions are shared with
        // OpenChamber / the CLI: strip any inherited XDG_DATA_HOME rather than
        // pinning a private one. Likewise strip an inherited username so the
        // server uses its default `opencode` — cola's client sends exactly that,
        // so a mismatched inherited username can't cause 401s against cola's
        // own server. `OPENCODE_PASSWORD` is stripped for the same
        // probe-must-match reason: it outranks the password set above.
        remove_env: vec![
            "XDG_DATA_HOME".to_string(),
            "OPENCODE_SERVER_USERNAME".to_string(),
            "OPENCODE_PASSWORD".to_string(),
        ],
    }
}

/// Build the command that starts the machine-wide V2 **Shared Service**
/// (`opencode serve --service`) — the rendezvous the TUI, the desktop app and
/// the Web UI resolve through the registration file (ADR-0085).
///
/// No `--port`: the daemon honors its own `opencode service set port`. No
/// password either — the service keeps one private credential across restarts
/// in its own config and echoes it in the registration file, which is what cola
/// reads. The child KEEPS `XDG_STATE_HOME` (the registration must land where
/// every client looks for it) and drops `XDG_DATA_HOME` plus the credential
/// variables cola may have inherited, so the daemon lands on the default store
/// with its own auth rather than cola's.
pub fn service_start_command() -> SpawnCommand {
    SpawnCommand {
        args: vec!["serve".to_string(), "--service".to_string()],
        env: Vec::new(),
        remove_env: vec![
            "XDG_DATA_HOME".to_string(),
            "OPENCODE_SERVER_USERNAME".to_string(),
            "OPENCODE_PASSWORD".to_string(),
            "OPENCODE_SERVER_PASSWORD".to_string(),
        ],
    }
}

/// Record the pid of the OpenCode server cola just spawned.
///
/// The record lives at `layout.self_spawned_pid()`. It is written when cola
/// starts its own server and read by `/restart-opencode` to decide whether cola
/// owns the running server and may restart it. Persisted so cola still
/// recognises its own server after a `/restart` of cola itself.
pub fn record_self_spawned(layout: &StateLayout, pid: i32) {
    record_self_spawned_in(&layout.self_spawned_pid(), pid);
}

fn record_self_spawned_in(path: &std::path::Path, pid: i32) {
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    if let Err(e) = std::fs::write(path, pid.to_string()) {
        tracing::warn!("failed to record self-spawned opencode pid {}: {}", pid, e);
    }
}

/// Forget the recorded self-spawned pid (called when that server exits or cola
/// restarts it). Returns the previous pid if one was recorded.
pub fn clear_self_spawned(layout: &StateLayout) -> Option<i32> {
    clear_self_spawned_in(&layout.self_spawned_pid())
}

fn clear_self_spawned_in(path: &std::path::Path) -> Option<i32> {
    let prev = read_self_spawned_pid_in(path);
    let _ = std::fs::remove_file(path);
    prev
}

fn read_self_spawned_pid_in(path: &std::path::Path) -> Option<i32> {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|s| s.trim().parse().ok())
}

/// The recorded pid of cola's Owned Server, if any. Not verified live — use
/// [`is_self_spawned`] when liveness matters (e.g. before deciding to kill).
pub fn self_spawned_pid(layout: &StateLayout) -> Option<i32> {
    read_self_spawned_pid_in(&layout.self_spawned_pid())
}

/// Whether the pid of the server cola is attached to is a server cola itself
/// spawned (and therefore may restart). Only true when the pid file matches a
/// pid that is genuinely a live `opencode serve` — a stale file for a dead or
/// recycled pid is treated as NOT owned, so cola never kills something it can't
/// verify it started.
pub fn is_self_spawned(layout: &StateLayout, pid: i32) -> bool {
    is_self_spawned_record(&layout.self_spawned_pid(), pid) && is_live_opencode_serve(pid)
}

/// Pure record-match check: does the pid file record this pid? (No liveness —
/// the public `is_self_spawned` additionally requires the pid to be a live
/// `opencode serve`.) Kept separate so the ownership logic is unit-testable.
fn is_self_spawned_record(path: &std::path::Path, pid: i32) -> bool {
    read_self_spawned_pid_in(path) == Some(pid)
}

/// Whether a pid is a live `opencode serve` process (not a dead/zombie pid that
/// the OS recycled, and not some other program that happened to reuse the pid).
/// Reads the process cmdline via sysinfo and applies the shared
/// [`is_opencode_serve_args`] predicate, so liveness can never accept a command
/// line a scan would reject.
fn is_live_opencode_serve(pid: i32) -> bool {
    process_cmd(pid)
        .map(|args| is_opencode_serve_args(&args))
        .unwrap_or(false)
}

/// The command-line args of a process, from sysinfo (None if not visible).
pub fn process_cmd(pid: i32) -> Option<Vec<String>> {
    let mut system = System::new();
    system.refresh_processes_specifics(
        ProcessesToUpdate::Some(&[Pid::from_u32(pid as u32)]),
        false,
        ProcessRefreshKind::nothing().with_cmd(UpdateKind::Always),
    );
    system
        .process(Pid::from_u32(pid as u32))
        .map(|p| p.cmd().iter().map(|s| s.to_string_lossy().into_owned()).collect())
}

/// The executable a process is running, from sysinfo (None if not visible or
/// the platform does not report it). `cola restart` uses it to re-exec the lock
/// holder by its own absolute binary path rather than a possibly-relative
/// argv[0] (ADR-0081).
pub fn process_exe(pid: i32) -> Option<std::path::PathBuf> {
    let mut system = System::new();
    system.refresh_processes_specifics(
        ProcessesToUpdate::Some(&[Pid::from_u32(pid as u32)]),
        false,
        ProcessRefreshKind::nothing().with_exe(UpdateKind::Always),
    );
    system
        .process(Pid::from_u32(pid as u32))
        .and_then(|p| p.exe().map(|e| e.to_path_buf()))
}

/// The environment of a process, from sysinfo (`None` if not visible). Used to
/// tell whether a supervisor owns a running cola (ADR-0021): systemd stamps
/// `INVOCATION_ID`/`SYSTEMD_EXEC_PID` into every unit process, launchd sets
/// `XPC_SERVICE_NAME`. Reading another process's env is best-effort — it is
/// limited to same-user processes on macOS/Windows.
pub fn process_environ(pid: i32) -> Option<Vec<String>> {
    let mut system = System::new();
    system.refresh_processes_specifics(
        ProcessesToUpdate::Some(&[Pid::from_u32(pid as u32)]),
        false,
        ProcessRefreshKind::nothing().with_environ(UpdateKind::Always),
    );
    system.process(Pid::from_u32(pid as u32)).map(|p| {
        p.environ()
            .iter()
            .map(|s| s.to_string_lossy().into_owned())
            .collect()
    })
}

/// The working directory of a process, from sysinfo (`None` if not visible).
/// `cola restart` re-execs the replacement in the instance's own directory so a
/// relative `--config` or a `./cola.toml` resolves exactly as it did.
pub fn process_cwd(pid: i32) -> Option<std::path::PathBuf> {
    let mut system = System::new();
    system.refresh_processes_specifics(
        ProcessesToUpdate::Some(&[Pid::from_u32(pid as u32)]),
        false,
        ProcessRefreshKind::nothing().with_cwd(UpdateKind::Always),
    );
    system
        .process(Pid::from_u32(pid as u32))
        .and_then(|p| p.cwd().map(|c| c.to_path_buf()))
}

/// Whether a PID refers to a live process (not a zombie, not missing).
///
/// Conservative on platforms where sysinfo cannot determine zombie state: a
/// process that exists but whose status is unknown is treated as alive, so a
/// lock is never stolen from a process that might still be running.
pub fn process_alive(pid: i32) -> bool {
    let mut system = System::new();
    system.refresh_processes(ProcessesToUpdate::Some(&[Pid::from_u32(pid as u32)]), false);
    match system.process(Pid::from_u32(pid as u32)) {
        Some(p) => p.status() != sysinfo::ProcessStatus::Zombie,
        None => false,
    }
}

/// Terminate a process. On unix this sends SIGTERM (graceful: the process's
/// Drop handlers run, releasing the singleton lock and the Feishu WS); the
/// caller should poll [`process_alive`] and escalate to SIGKILL via
/// [`force_kill`]. On Windows there is no POSIX signal and the graceful
/// `taskkill /PID` first stage is a no-op for a windowless CLI, so we go
/// straight to `taskkill /F`.
pub fn terminate_process(pid: i32) -> anyhow::Result<()> {
    #[cfg(unix)]
    {
        let _ = std::process::Command::new("kill").arg(pid.to_string()).status();
        Ok(())
    }
    #[cfg(windows)]
    {
        let status = std::process::Command::new("taskkill")
            .args(["/F", "/PID", &pid.to_string()])
            .status()?;
        if status.success() {
            Ok(())
        } else {
            anyhow::bail!("taskkill /F failed for pid {}", pid)
        }
    }
}

/// Force-kill a process (SIGKILL on unix). No-op on Windows, where
/// [`terminate_process`] already force-terminated.
pub fn force_kill(pid: i32) -> anyhow::Result<()> {
    #[cfg(unix)]
    {
        let _ = std::process::Command::new("kill")
            .args(["-9", &pid.to_string()])
            .status();
        Ok(())
    }
    #[cfg(windows)]
    {
        let _ = pid;
        Ok(())
    }
}

/// Spawn an `opencode serve` on the default store (same semantics as main's
/// self-start), record its pid, and return it. Used both at cola startup and by
/// `/restart-opencode` to re-raise the server cola owns.
pub fn spawn_self_server(layout: &StateLayout, port: u16, password: &str) -> anyhow::Result<i32> {
    let cmd = self_start_command(port, password);
    let mut child = std::process::Command::new("opencode");
    child.args(&cmd.args).envs(cmd.env.iter().cloned());
    // Removals go LAST on purpose: std/tokio's `Command` lets the last call for
    // a key win, so a stripped name always beats a value set above. Swapping
    // these loops would silently re-add a removed credential — the
    // `OPENCODE_PASSWORD` strip is what keeps the child on the password cola
    // probes with (see `self_start_command`).
    for key in &cmd.remove_env {
        child.env_remove(key);
    }
    // The question tool stays enabled on purpose: cola answers it via Feishu
    // cards (poll GET /question, reply POST /question/{id}/reply), so disabling
    // it would make those cards unreachable.
    let child = child.spawn()?;
    let pid = child.id() as i32;
    record_self_spawned(layout, pid);
    Ok(pid)
}

/// Wait until a TCP port accepts connections (the spawned server is up).
pub async fn wait_for_port(port: u16) -> anyhow::Result<()> {
    for _ in 0..100 {
        if tokio::net::TcpStream::connect(("127.0.0.1", port)).await.is_ok() {
            return Ok(());
        }
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    }
    anyhow::bail!("OpenCode server did not start on port {}", port)
}

/// The startup budget for resolving a freshly spawned server's generation: it
/// accepts TCP a beat before it answers HTTP, and V2 answers 503 while it
/// migrates. Bounded like the Lazy Start readiness wait. On timeout a live
/// child is left recorded (a later reconcile attaches to it by probing again);
/// a child that exited is forgotten immediately and fails the spawn at once.
const SELF_START_PROBE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(20);

/// The startup budget for a fresh Shared Service to publish a registration for
/// a live `serve --service`. A warm daemon lands in well under a second; a cold
/// one has to boot its store. Bounded so Lazy Start's first prompt and cola's
/// eager boot never hang on a service that will not come up.
const SERVICE_START_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(15);

/// Grace after a service attempt exits without a registration. The daemon
/// writes its registration a beat after the spawn returns, and a contender that
/// found an incumbent exits cleanly — in that case the incumbent's registration
/// is already on disk and is adopted, not waited out.
const SERVICE_START_EXIT_GRACE: std::time::Duration = std::time::Duration::from_millis(2000);

/// Grace between SIGTERM and SIGKILL when a failed attempt must be reaped.
const SERVICE_START_KILL_GRACE: std::time::Duration = std::time::Duration::from_secs(5);

/// Why the service-first start did not produce an attachable server.
#[derive(Debug, Clone, PartialEq, Eq)]
enum ServiceStartFailure {
    /// The child could not be spawned at all (no `opencode` on `PATH`, the log
    /// directory unwritable).
    Spawn(String),
    /// No live `serve --service` registration appeared in time; the attempt was
    /// terminated AND confirmed exited. The `serve --port` fallback follows.
    NoRegistration { child_pid: i32 },
    /// A registration appeared, but the process exited before its generation
    /// resolved. Nothing is left to attach to, so the `serve --port` fallback
    /// follows — and the attempt's child was already waited out.
    Exited(String),
    /// A registration appeared and the process is still ALIVE, but it never
    /// classified for the attach. Deliberately NOT a fallback: a live
    /// rendezvous exists, and raising an Owned Server beside it would put two
    /// servers on one store (ADR-0013, ADR-0085). The reconnect loop attaches
    /// once the probe classifies.
    Attach(String),
    /// The attempt never registered and could NOT be confirmed exited: it
    /// ignored SIGTERM and survived SIGKILL. Also raised when a contender lost
    /// the rendezvous to a live incumbent but could not be stopped before that
    /// incumbent was adopted. Deliberately NOT a fallback — a possibly-live
    /// service on the store means raising an Owned Server beside it would put
    /// two servers on one store (ADR-0013, ADR-0085), which is the same hazard
    /// as [`Self::Attach`]. cola stays serverless and the reconnect loop
    /// attaches once the service registers.
    NotReaped { child_pid: i32 },
}

impl std::fmt::Display for ServiceStartFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Spawn(e) => write!(f, "`opencode serve --service` could not be spawned: {e}"),
            Self::NoRegistration { child_pid } => write!(
                f,
                "no live `serve --service` registration appeared within {}s (attempt pid {child_pid})",
                SERVICE_START_TIMEOUT.as_secs()
            ),
            Self::Exited(evidence) => write!(
                f,
                "the shared service exited before its generation resolved: {evidence}"
            ),
            Self::Attach(evidence) => write!(
                f,
                "the shared service registered but its generation never resolved: {evidence}"
            ),
            Self::NotReaped { child_pid } => write!(
                f,
                "the `serve --service` attempt (pid {child_pid}) could not be confirmed exited; not falling back to `serve --port`"
            ),
        }
    }
}

/// Whether a command line really is the V2 Shared Service: an `opencode …
/// serve` invocation — the SAME recognition a scan applies, so an
/// interpreter-launched dev server (`node /opt/opencode/dist/index.js serve`,
/// #812) is the service too — running in `--service` mode. A registration pid
/// that no longer matches this is stale — the pid died and was recycled, or
/// another program owns it now — so it is never attached to, and never killed.
fn is_opencode_service_cmd(args: &[String]) -> bool {
    is_opencode_serve_args(args) && args.iter().any(|a| a == "--service")
}

/// The candidate a registration file describes, when it names a LIVE
/// `serve --service`: the recorded pid must be alive, its command line must
/// really be `opencode ... serve --service`, and `url` must carry a port. Pure
/// over the file's contents and the two process lookups, so the
/// matching-pid-accepted / stale-pid-rejected rule is unit-tested without a
/// filesystem or a process.
///
/// The registration is the service's own identity, not cola's record of it:
/// the class is always [`ServerClass::Service`] and the username is always
/// `opencode` (the only one V2 accepts). The store is NOT taken on trust: the
/// daemon's own `XDG_DATA_HOME` must read the default store, exactly as
/// [`scan_processes`] requires of a discovered server. A registration whose
/// daemon serves a custom store — an incumbent that shares cola's state home
/// but not its data home — is not the canonical service and must never be
/// adopted (spec #809: eligible = `uses_default_store && port != 0`), so the
/// start path keeps waiting and falls back to `serve --port` on the default
/// store.
fn registration_service_candidate(
    raw: &str,
    alive: &dyn Fn(i32) -> bool,
    cmd: &dyn Fn(i32) -> Option<Vec<String>>,
    environ: &dyn Fn(i32) -> Option<Vec<String>>,
) -> Option<ServerCandidate> {
    let registration: ServiceRegistration = serde_json::from_str(raw).ok()?;
    // Without a pid there is nothing to verify liveness against, so the
    // registration cannot be adopted.
    let pid = registration.pid?;
    if !alive(pid) || !cmd(pid).is_some_and(|args| is_opencode_service_cmd(&args)) {
        return None;
    }
    let default_store = default_data_home().to_string_lossy().into_owned();
    if !env_uses_default_store(environ(pid).as_deref(), &default_store) {
        return None;
    }
    let port = port_from_url(&registration.url)?;
    Some(ServerCandidate {
        pid,
        port,
        password: registration.password,
        username: DEFAULT_SERVER_USERNAME.to_string(),
        uses_default_store: true,
        class: ServerClass::Service,
    })
}

/// [`registration_service_candidate`] over the real registration file.
fn live_service_registration(host: &dyn ServiceHost, path: &std::path::Path) -> Option<ServerCandidate> {
    let raw = host.registration(path)?;
    registration_service_candidate(&raw, &|pid| host.alive(pid), &|pid| host.cmd(pid), &|pid| {
        host.environ(pid)
    })
}

/// Whether OpenCode's own config disables the managed service
/// (`{"disabled": true}`). A missing or malformed file means "not disabled" —
/// the default OpenCode itself applies, and a config cola cannot parse must
/// never stop the start path.
fn service_disabled_at(path: &std::path::Path) -> bool {
    std::fs::read_to_string(path)
        .map(|raw| parse_service_disabled(&raw))
        .unwrap_or(false)
}

/// The `disabled` flag of a service config file, pure so the skip is
/// unit-tested without a filesystem.
fn parse_service_disabled(raw: &str) -> bool {
    #[derive(serde::Deserialize)]
    struct ServiceConfig {
        #[serde(default)]
        disabled: bool,
    }
    serde_json::from_str::<ServiceConfig>(raw)
        .map(|c| c.disabled)
        .unwrap_or(false)
}

/// What the start path does first (ADR-0085): raise the Shared Service, or go
/// straight to the Owned `serve --port` server.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ServiceStartPlan {
    TryService,
    /// Skip the service, with the reason for the log.
    PortOnly(ServiceStartSkip),
}

/// Why the service is not attempted at all.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ServiceStartSkip {
    /// `[opencode] generation = "v1"`: V1 has no `serve --service` (spec #809).
    V1Generation,
    /// OpenCode's own `service.json` says `disabled: true`.
    Disabled,
}

impl std::fmt::Display for ServiceStartSkip {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::V1Generation => write!(f, "`[opencode] generation = \"v1\"`"),
            Self::Disabled => write!(f, "OpenCode's own service config says `disabled: true`"),
        }
    }
}

/// The service-first decision — a pure function of the generation override and
/// OpenCode's own `disabled` flag, so both skip branches are unit-tested.
/// Under `auto` the service IS attempted: a V1 binary fails the attempt on its
/// own and falls back, which is exactly the "自然回退" the spec asks for.
fn service_start_plan(generation_override: GenerationOverride, disabled: bool) -> ServiceStartPlan {
    if generation_override == GenerationOverride::V1 {
        return ServiceStartPlan::PortOnly(ServiceStartSkip::V1Generation);
    }
    if disabled {
        return ServiceStartPlan::PortOnly(ServiceStartSkip::Disabled);
    }
    ServiceStartPlan::TryService
}

/// A spawned service attempt's process handle, so the attempt can be waited
/// out. A `Child` that is merely dropped is never waited on: a failed attempt
/// would stay a zombie for as long as cola runs, which is exactly what the
/// spec's "no leftover service child" rules out (ticket #809).
trait ServiceAttempt: Send {
    /// The attempt's pid.
    fn pid(&self) -> i32;
    /// Non-blocking: wait the process out once it has exited, returning whether
    /// it is gone. `false` while it still runs — a service cola only attached
    /// must never be blocked on.
    fn try_reap(&mut self) -> bool;
}

/// The process-side view the Shared Service start path needs. Real code uses
/// [`RealServiceHost`]; a test substitutes every operation, so no process is
/// started and no real registration is read.
trait ServiceHost: Send + Sync {
    /// Spawn `opencode serve --service` detached, its output appended to the
    /// service log; the attempt's pid and process handle.
    fn spawn_service(&self, log_path: &std::path::Path) -> anyhow::Result<Box<dyn ServiceAttempt>>;
    /// The registration file's raw contents, when it exists.
    fn registration(&self, path: &std::path::Path) -> Option<String>;
    /// Whether a pid is a live process.
    fn alive(&self, pid: i32) -> bool;
    /// A pid's command line.
    fn cmd(&self, pid: i32) -> Option<Vec<String>>;
    /// A pid's environment lines, when visible — the store check reads the
    /// daemon's own `XDG_DATA_HOME` from them.
    fn environ(&self, pid: i32) -> Option<Vec<String>>;
    /// Ask a process to exit (SIGTERM on unix).
    fn terminate(&self, pid: i32) -> anyhow::Result<()>;
    /// Kill a process that ignored SIGTERM (SIGKILL on unix).
    fn force_kill(&self, pid: i32) -> anyhow::Result<()>;
}

/// The real operations: a detached `opencode serve --service` child, its
/// registration file, and the machine's process table.
struct RealServiceHost;

impl ServiceHost for RealServiceHost {
    fn spawn_service(&self, log_path: &std::path::Path) -> anyhow::Result<Box<dyn ServiceAttempt>> {
        spawn_service_detached(log_path)
    }

    fn registration(&self, path: &std::path::Path) -> Option<String> {
        std::fs::read_to_string(path).ok()
    }

    fn alive(&self, pid: i32) -> bool {
        process_alive(pid)
    }

    fn cmd(&self, pid: i32) -> Option<Vec<String>> {
        process_cmd(pid)
    }

    fn environ(&self, pid: i32) -> Option<Vec<String>> {
        process_environ(pid)
    }

    fn terminate(&self, pid: i32) -> anyhow::Result<()> {
        terminate_process(pid)
    }

    fn force_kill(&self, pid: i32) -> anyhow::Result<()> {
        force_kill(pid)
    }
}

/// Spawn `opencode serve --service` so the daemon OUTLIVES cola: detached from
/// cola's process group, stdin closed, and stdout/stderr appended to the
/// service log — never a pipe nobody reads, which would block the daemon on its
/// first log line.
///
/// The child keeps its inherited `XDG_STATE_HOME` (its registration must land
/// where the clients look) and has cola's data-home/credential variables
/// stripped by [`service_start_command`].
///
/// The child is a `tokio` one so its handle can be waited on: dropping a bare
/// `std::process::Child` never reaps it, and the attempt's own reap path is the
/// one place a failed start can wait it out (ticket #809).
fn spawn_service_detached(log_path: &std::path::Path) -> anyhow::Result<Box<dyn ServiceAttempt>> {
    let cmd = service_start_command();
    let mut command = tokio::process::Command::new("opencode");
    command.args(&cmd.args).envs(cmd.env.iter().cloned());
    for key in &cmd.remove_env {
        command.env_remove(key);
    }
    if let Some(dir) = log_path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let log = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(log_path)?;
    command
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::from(log.try_clone()?))
        .stderr(std::process::Stdio::from(log));

    #[cfg(unix)]
    {
        // A NEW PROCESS GROUP detaches the daemon from cola's: a signal aimed at
        // cola's group (Ctrl-C at its terminal, a supervisor's group kill) never
        // reaches the service. This is a deliberate deviation from ticket
        // #810's literal `setsid` (recorded in ADR-0085): `setsid` is not
        // reachable here — `pre_exec` is the only std route to it and this
        // crate forbids `unsafe` outside tests
        // (`#![cfg_attr(not(test), forbid(unsafe_code))]`). The difference is
        // the session, and it cannot bite: the child's stdin is closed and its
        // stdout/stderr are files, so it never touches the controlling terminal
        // and is not in the foreground group the terminal's hangup signals
        // (SIGHUP, SIGTTIN/SIGTTOU) are sent to.
        command.process_group(0);
    }
    // Windows has no POSIX session to leave: a new process group plus
    // DETACHED_PROCESS (no console) is the equivalent. The two flags are
    // declared here rather than pulled from a crate for one call.
    #[cfg(windows)]
    {
        const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
        const DETACHED_PROCESS: u32 = 0x0000_0008;
        command.creation_flags(CREATE_NEW_PROCESS_GROUP | DETACHED_PROCESS);
    }

    let child = command.spawn()?;
    let pid = child
        .id()
        .ok_or_else(|| anyhow::anyhow!("`opencode serve --service` spawned without a pid"))?
        as i32;
    Ok(Box::new(RealServiceAttempt { pid, child }))
}

/// The real attempt: the detached child, held so a failed start can wait it
/// out. Dropping it does not kill the daemon — it only hands the child to the
/// runtime's orphan queue, so a service cola merely attached still outlives
/// cola (ADR-0085).
struct RealServiceAttempt {
    pid: i32,
    child: tokio::process::Child,
}

impl ServiceAttempt for RealServiceAttempt {
    fn pid(&self) -> i32 {
        self.pid
    }

    fn try_reap(&mut self) -> bool {
        // An error means the child is already reaped or unwaitable: gone either
        // way, so the caller must not keep waiting on it.
        self.child
            .try_wait()
            .map(|status| status.is_some())
            .unwrap_or(true)
    }
}

/// Start (or adopt) the machine-wide Shared Service and return the candidate
/// its registration describes.
///
/// The daemon writes `service.json` when it starts listening, so the wait is
/// for a registration that names a LIVE `serve --service` — a stale file (dead
/// or recycled pid) keeps the wait going rather than being attached to. A
/// contender that loses the rendezvous to an incumbent already serving the
/// machine is accepted: the incumbent's registration is live, which is all cola
/// needs. Losing does not mean EXITED, though — a contender can still be
/// starting when the first poll sees the incumbent's file — so it is stopped
/// (SIGTERM, a bounded wait, then SIGKILL) before the incumbent is adopted; two
/// services must never write the one store (ADR-0013, ADR-0085). An attempt
/// that never registers is stopped the same way and reported as a failure:
/// [`ServiceStartFailure::NoRegistration`] once it is confirmed exited — which
/// is what sends the caller to the `serve --port` fallback — and
/// [`ServiceStartFailure::NotReaped`] when it is not, so no fallback is raised
/// beside a possibly-live service.
///
/// A CANCELLED wait (the reconnect task dropped at shutdown) is not a normal
/// exit: the [`ServiceAttemptGuard`] then reaps the attempt on drop, so a
/// service child that never registered cannot be leaked.
async fn start_shared_service(
    host: &dyn ServiceHost,
    registration_path: &std::path::Path,
    log_path: &std::path::Path,
) -> Result<ServerCandidate, ServiceStartFailure> {
    let attempt = host
        .spawn_service(log_path)
        .map_err(|e| ServiceStartFailure::Spawn(e.to_string()))?;
    let child_pid = attempt.pid();
    // The attempt rides the guard so a cancelled wait still reaps it; every
    // normal exit below disarms the guard first.
    let mut attempt = ServiceAttemptGuard::new(host, attempt);
    let deadline = tokio::time::Instant::now() + SERVICE_START_TIMEOUT;
    let mut exited_at: Option<tokio::time::Instant> = None;
    loop {
        if let Some(candidate) = live_service_registration(host, registration_path) {
            // A registration naming OUR OWN attempt is the daemon that just
            // came up: adopt it and leave it running.
            if candidate.pid == child_pid {
                attempt.reap_and_disarm();
                return Ok(candidate);
            }
            // Anything else is an incumbent — another client's or a manual
            // `serve --service` — whose live registration is all cola needs.
            // Our contender must be GONE before cola attaches, though: it can
            // still be starting when the first poll sees the incumbent's file,
            // and a second service on the store is the two-server hazard
            // (ADR-0013, ADR-0085). Stop it the way a failed attempt is
            // stopped, and adopt only once it is confirmed exited.
            let gone = reap_failed_service_attempt(host, attempt.attempt_mut()).await;
            attempt.disarm();
            return if gone {
                Ok(candidate)
            } else {
                // The contender ignored SIGTERM and survived SIGKILL: the
                // incumbent is usable, but adopting it beside a contender cola
                // cannot stop would hide the hazard, and the `serve --port`
                // fallback would raise a third server. Stay serverless.
                Err(ServiceStartFailure::NotReaped { child_pid })
            };
        }
        let now = tokio::time::Instant::now();
        if now >= deadline {
            let reaped = reap_failed_service_attempt(host, attempt.attempt_mut()).await;
            attempt.disarm();
            return Err(if reaped {
                ServiceStartFailure::NoRegistration { child_pid }
            } else {
                // The attempt survived SIGKILL: it may still be serving the
                // store, so this must NOT be fallback-eligible (ADR-0013,
                // ADR-0085).
                ServiceStartFailure::NotReaped { child_pid }
            });
        }
        // A child that exits without a live registration failed the attempt (a
        // V1 binary has no `serve --service`) — after the grace the daemon gets
        // to publish the file it writes a beat after the spawn returns.
        if !host.alive(child_pid) {
            let exited = *exited_at.get_or_insert(now);
            if now.duration_since(exited) >= SERVICE_START_EXIT_GRACE {
                reap_failed_service_attempt(host, attempt.attempt_mut()).await;
                attempt.disarm();
                return Err(ServiceStartFailure::NoRegistration { child_pid });
            }
        }
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    }
}

/// Whether a pid is still OUR service attempt: alive AND an
/// `opencode ... serve --service`. A pid that has exited (a V1 binary, or a
/// contender that found an incumbent) or been recycled must never be signalled
/// — the same guard the Owned Server path applies before it kills.
fn is_our_service_attempt(host: &dyn ServiceHost, pid: i32) -> bool {
    host.alive(pid) && host.cmd(pid).is_some_and(|args| is_opencode_service_cmd(&args))
}

/// Terminate a service attempt that must not outlive the start: it never
/// registered, or it lost the rendezvous to a live incumbent that cola is about
/// to adopt. SIGTERM, a bounded wait, then SIGKILL. Nothing is sent unless the
/// pid is still a live `opencode ... serve --service` — an exited pid may have
/// been recycled, and cola must never kill an unrelated process (the same guard
/// the Owned Server path applies before it kills).
///
/// Returns whether the attempt is CONFIRMED gone (exited and waited out, or no
/// longer a live `serve --service`). `false` means cola cannot rule out a live
/// service on the store, so the caller must NOT fall back to `serve --port`:
/// a second server beside it is the two-server hazard ADR-0013/ADR-0085
/// prevent.
///
/// Whichever way it goes, the attempt's own child is waited out: a failed start
/// must leave no service child behind, and an un-waited one lingers as a zombie
/// for as long as cola runs (ticket #809).
async fn reap_failed_service_attempt(host: &dyn ServiceHost, attempt: &mut dyn ServiceAttempt) -> bool {
    let child_pid = attempt.pid();
    if !is_our_service_attempt(host, child_pid) {
        // Nothing of ours to signal: the child already exited (a V1 binary, or
        // a contender that found an incumbent), or the pid is now another
        // program's. Either way a pid the process table reports dead cannot be
        // serving the store; a still-live pid cola cannot identify is not
        // confirmed gone.
        let reaped = attempt.try_reap();
        return reaped || !host.alive(child_pid);
    }
    let _ = host.terminate(child_pid);
    if wait_for_attempt_exit(attempt, SERVICE_START_KILL_GRACE).await {
        return true;
    }
    if host.alive(child_pid) {
        tracing::warn!("OpenCode service attempt (pid {child_pid}) ignored SIGTERM; killing it");
        let _ = host.force_kill(child_pid);
    }
    if wait_for_attempt_exit(attempt, SERVICE_START_KILL_GRACE).await {
        return true;
    }
    tracing::warn!(
        "OpenCode service attempt (pid {child_pid}) could not be confirmed exited; NOT falling back to `opencode serve --port` — a possibly-live service on the store means a second server would be the two-server hazard (ADR-0013, ADR-0085)"
    );
    false
}

/// Poll the attempt's process handle until the child has exited and been waited
/// out, bounded by `grace`. Waiting on the handle — not on a liveness lookup —
/// is what reaps it.
async fn wait_for_attempt_exit(attempt: &mut dyn ServiceAttempt, grace: std::time::Duration) -> bool {
    let deadline = tokio::time::Instant::now() + grace;
    loop {
        if attempt.try_reap() {
            return true;
        }
        if tokio::time::Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
}

/// [`wait_for_attempt_exit`] for a destructor: the same bounded poll, run
/// synchronously because `Drop` cannot await (the pattern the live-server
/// harness already uses for its child).
fn wait_for_attempt_exit_blocking(attempt: &mut dyn ServiceAttempt, grace: std::time::Duration) -> bool {
    let deadline = std::time::Instant::now() + grace;
    loop {
        if attempt.try_reap() {
            return true;
        }
        if std::time::Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
}

/// [`reap_failed_service_attempt`] for a destructor: the same SIGTERM → bounded
/// wait → SIGKILL sequence, run synchronously. Reached only when the wait is
/// CANCELLED before the attempt is adopted or reaped — every normal exit
/// disarms the guard first — so its only job is to leave no service child
/// behind.
fn reap_failed_service_attempt_blocking(host: &dyn ServiceHost, attempt: &mut dyn ServiceAttempt) {
    let child_pid = attempt.pid();
    if !is_our_service_attempt(host, child_pid) {
        // Already exited or recycled: collect the child, never signal.
        attempt.try_reap();
        return;
    }
    let _ = host.terminate(child_pid);
    if wait_for_attempt_exit_blocking(attempt, SERVICE_START_KILL_GRACE) {
        return;
    }
    if host.alive(child_pid) {
        tracing::warn!("cancelled OpenCode service attempt (pid {child_pid}) ignored SIGTERM; killing it");
        let _ = host.force_kill(child_pid);
    }
    wait_for_attempt_exit_blocking(attempt, SERVICE_START_KILL_GRACE);
}

/// The service attempt held across the registration wait, reaping it if the
/// wait future is CANCELLED. The wait owns the attempt so a failed start can
/// wait it out; without this guard a cancelled start (the reconnect task
/// dropped at shutdown) would drop the handle and leave a service child that
/// never registered alive (ticket #809). Every normal exit disarms the guard —
/// the timeout and the lost-rendezvous paths reap through
/// [`reap_failed_service_attempt`], the adopted-daemon path leaves the attempt
/// it just adopted — so only cancellation reaches `Drop`.
struct ServiceAttemptGuard<'h> {
    host: &'h dyn ServiceHost,
    attempt: Option<Box<dyn ServiceAttempt>>,
    armed: bool,
}

impl<'h> ServiceAttemptGuard<'h> {
    fn new(host: &'h dyn ServiceHost, attempt: Box<dyn ServiceAttempt>) -> Self {
        Self {
            host,
            attempt: Some(attempt),
            armed: true,
        }
    }

    /// The attempt, for the wait's own liveness and identity checks.
    fn attempt_mut(&mut self) -> &mut dyn ServiceAttempt {
        self.attempt
            .as_mut()
            .expect("the attempt is held until Drop")
            .as_mut()
    }

    /// Wait the attempt out and stop `Drop` from reaping it — the success path
    /// where the attempt IS the adopted daemon (the registration names it), so
    /// the child must keep running while the handle no longer reaps it.
    fn reap_and_disarm(&mut self) -> bool {
        let reaped = self.attempt_mut().try_reap();
        self.armed = false;
        reaped
    }

    /// The attempt is already reaped or handed over: `Drop` must leave it.
    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for ServiceAttemptGuard<'_> {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        if let Some(attempt) = self.attempt.as_mut() {
            reap_failed_service_attempt_blocking(self.host, attempt.as_mut());
        }
    }
}

/// The two ways cola gets a server when none is running: the machine-wide
/// Shared Service rendezvous (preferred, ADR-0085) and the classic Owned
/// `serve --port` fallback. Behind a trait so the service-first ordering and
/// its fallback are unit-tested without spawning anything.
#[async_trait]
trait ServerStarter: Send + Sync {
    /// Start (or adopt) the V2 Shared Service and resolve the attach to it.
    async fn start_service(
        &self,
        generation_override: GenerationOverride,
    ) -> Result<ResolvedServer, ServiceStartFailure>;

    /// The classic Owned Server path: `serve --port`, wait, resolve.
    async fn start_port(
        &self,
        preferred_port: Option<u16>,
        generation_override: GenerationOverride,
    ) -> anyhow::Result<ResolvedServer>;
}

/// The real legs: the Shared Service through its registration file, the Owned
/// Server through `spawn_self_server`. The host, the two paths and the probe
/// transport are fields — production's values are the real ones — so a test
/// drives the REAL `start_service` (the ownership boundary's own code) against
/// a scripted host (ticket #809).
struct OpencodeStarter<H: ServiceHost> {
    host: H,
    registration_path: std::path::PathBuf,
    log_path: std::path::PathBuf,
    /// How the adopted candidate's probe transport is built. Production passes
    /// [`attach::candidate_transport`]; a test substitutes a loopback transport
    /// so its fake server is reachable without a developer shell's `http_proxy`
    /// in the way (the wire-test pattern, ADR-0031 — production attach
    /// transports keep honoring env proxies by design, ticket 09).
    probe_transport: fn(&ServerCandidate) -> Transport,
    /// The one owner of the state paths this starter touches (the pid record
    /// and the Shared Service's log).
    layout: StateLayout,
}

impl OpencodeStarter<RealServiceHost> {
    fn new(layout: &StateLayout) -> Self {
        Self {
            host: RealServiceHost,
            registration_path: own_service_registration_path(),
            log_path: layout.service_log(),
            probe_transport: attach::candidate_transport,
            layout: layout.clone(),
        }
    }
}

#[async_trait]
impl<H: ServiceHost> ServerStarter for OpencodeStarter<H> {
    async fn start_service(
        &self,
        generation_override: GenerationOverride,
    ) -> Result<ResolvedServer, ServiceStartFailure> {
        let candidate = start_shared_service(&self.host, &self.registration_path, &self.log_path).await?;
        // Resolved through the SAME attach path as any discovered server: the
        // generation is probed, never assumed (#380). The service is NOT
        // recorded as self-spawned — it is never an Owned Server and the yield
        // must never reap it (ADR-0085).
        let transport = (self.probe_transport)(&candidate);
        match resolve_spawned_server_over(
            &self.layout,
            &transport,
            &candidate,
            generation_override,
            SELF_START_PROBE_TIMEOUT,
        )
        .await
        {
            Ok(resolved) => Ok(resolved),
            // A service that registered and then died is GONE: the caller may
            // raise the Owned Server fallback (spec #809 acceptance 6).
            Err(SpawnResolveFailure::Exited(evidence)) => Err(ServiceStartFailure::Exited(evidence)),
            Err(SpawnResolveFailure::Inconclusive(evidence)) => Err(ServiceStartFailure::Attach(evidence)),
        }
    }

    async fn start_port(
        &self,
        preferred_port: Option<u16>,
        generation_override: GenerationOverride,
    ) -> anyhow::Result<ResolvedServer> {
        let candidates = scan_processes();
        let mut port = preferred_port.unwrap_or(4096);
        // Skip ports already held by a custom-store server (someone else's data).
        while candidates.iter().any(|c| c.port == port) {
            port += 1;
        }
        let password = "cola-secret".to_string();
        let pid = spawn_self_server(&self.layout, port, &password)?;
        wait_for_port(port).await?;
        // The Owned Server is described as a candidate and resolved through the
        // SAME attach path as any discovered server: the `opencode` binary's
        // lineage is not fixed (#380), so the generation is probed, never assumed.
        let candidate = ServerCandidate {
            pid,
            port,
            password,
            username: DEFAULT_SERVER_USERNAME.to_string(),
            uses_default_store: true,
            // A spawned `serve --port` is cola's Owned Server by pid; the field
            // only records what the command line says (ADR-0013).
            class: ServerClass::Coexistent,
        };
        let transport = attach::candidate_transport(&candidate);
        resolve_spawned_server_over(
            &self.layout,
            &transport,
            &candidate,
            generation_override,
            SELF_START_PROBE_TIMEOUT,
        )
        .await
        .map_err(|failure| {
            anyhow::anyhow!("spawned OpenCode server's generation could not be resolved: {failure}")
        })
    }
}

/// Start cola's own OpenCode server on the default store. The FIRST step is
/// the machine-wide Shared Service (`opencode serve --service`, ADR-0085): the
/// terminal, the desktop app and the Web UI all resolve that rendezvous from
/// zero arguments, so starting it means every client lands on one server and
/// the real-time sync holds. Only when no service can be raised does cola fall
/// back to the classic Owned `serve --port` path — skipping ports already held
/// by a running server, waiting until it accepts connections, and resolving its
/// generation by probing (the `opencode` binary may be the V1 or the V2 lineage,
/// spec #364 §2).
///
/// Used at boot (`eager`) and by Lazy Start (`auto`); `/restart-opencode`
/// re-raises through [`spawn_self_server`] and the reconnect loop's own
/// re-probe. The result is a [`ResolvedServer`] like any other — a Shared
/// Service and an Owned Server are both just the endpoint cola attaches to
/// (ADR-0013).
pub async fn spawn_own_server(
    layout: &StateLayout,
    preferred_port: Option<u16>,
    generation_override: GenerationOverride,
) -> anyhow::Result<ResolvedServer> {
    let disabled = service_disabled_at(&own_service_config_path());
    spawn_own_server_over(
        preferred_port,
        generation_override,
        disabled,
        &OpencodeStarter::new(layout),
    )
    .await
}

/// [`spawn_own_server`] over an injectable pair of legs. `disabled` is
/// OpenCode's own `disabled` flag, read by the caller so the decision here
/// stays a pure function of it.
async fn spawn_own_server_over(
    preferred_port: Option<u16>,
    generation_override: GenerationOverride,
    disabled: bool,
    starter: &dyn ServerStarter,
) -> anyhow::Result<ResolvedServer> {
    match service_start_plan(generation_override, disabled) {
        ServiceStartPlan::TryService => match starter.start_service(generation_override).await {
            Ok(resolved) => {
                tracing::info!(
                    "started the shared OpenCode service at {} (pid {}) — generation={}",
                    resolved.url,
                    resolved
                        .pid
                        .map(|p| p.to_string())
                        .unwrap_or_else(|| "unknown".into()),
                    resolved.generation.as_str()
                );
                return Ok(resolved);
            }
            // Only "the service is not there" falls back — never started, never
            // registered, or registered and then GONE. A registration whose
            // process is still alive but never classified (`Attach`) and an
            // attempt that survived SIGKILL (`NotReaped`) are both left to the
            // reconnect loop: raising an Owned Server beside a possibly-live
            // service would put two servers on one store, which is exactly what
            // ADR-0013/ADR-0085 exist to prevent.
            Err(
                failure @ (ServiceStartFailure::Spawn(_)
                | ServiceStartFailure::NoRegistration { .. }
                | ServiceStartFailure::Exited(_)),
            ) => {
                tracing::warn!(
                    "could not start the OpenCode service ({failure}); falling back to `opencode serve --port`"
                );
            }
            Err(failure) => return Err(anyhow::anyhow!("{failure}")),
        },
        ServiceStartPlan::PortOnly(skip) => {
            tracing::debug!("skipping the shared OpenCode service: {skip}");
        }
    }
    starter.start_port(preferred_port, generation_override).await
}

/// Why a just-started server's generation never resolved. The distinction is
/// load-bearing: a process that is GONE may be replaced by the `serve --port`
/// fallback, while a live one must not be (a second server on one store is
/// exactly what ADR-0013/ADR-0085 exist to prevent).
#[derive(Debug, Clone, PartialEq, Eq)]
enum SpawnResolveFailure {
    /// The spawned process exited before its generation resolved.
    Exited(String),
    /// The process is still alive, but its probe stayed inconclusive.
    Inconclusive(String),
}

impl std::fmt::Display for SpawnResolveFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Exited(evidence) => write!(
                f,
                "the spawned server exited before its generation resolved: {evidence}"
            ),
            Self::Inconclusive(evidence) => write!(f, "{evidence}"),
        }
    }
}

/// The retry loop around the attach decision for a just-started server (the
/// Shared Service or an Owned Server): the startup window answers nothing, or
/// answers unclassifiably (V2's 503-while-migrating), so inconclusive probes are
/// retried until `timeout` or the child dies. The last probe evidence travels on
/// failure; the credential never does (the attach path's promise).
async fn resolve_spawned_server_over(
    layout: &StateLayout,
    transport: &Transport,
    candidate: &ServerCandidate,
    generation_override: GenerationOverride,
    timeout: std::time::Duration,
) -> Result<ResolvedServer, SpawnResolveFailure> {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        match attach::resolve_candidate_over(transport, candidate, generation_override).await {
            Ok(resolved) => return Ok(resolved),
            Err(evidence) => {
                if !process_alive(candidate.pid) {
                    if self_spawned_pid(layout) == Some(candidate.pid) {
                        clear_self_spawned(layout);
                    }
                    return Err(SpawnResolveFailure::Exited(evidence));
                }
                if tokio::time::Instant::now() >= deadline {
                    return Err(SpawnResolveFailure::Inconclusive(evidence));
                }
                tokio::time::sleep(std::time::Duration::from_millis(200)).await;
            }
        }
    }
}

/// Outcome of `/restart-opencode`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RestartOutcome {
    /// The running server was cola's own and has been restarted.
    Restarted,
    /// The V2 Shared Service was restarted through `opencode service restart`.
    RestartedService,
    /// No OpenCode server is currently running on the default store.
    NoServer,
    /// The running server was started by someone else — cola must not touch it.
    NotOwned,
}

/// What `/restart-opencode` may do, decided purely from a scan. Follows the
/// canonical pick: cola restarts the server it is attached to (or would attach
/// to), never a lower-priority one it is about to yield.
#[derive(Debug, Clone, PartialEq, Eq)]
enum RestartPlan {
    /// Restart the V2 Shared Service via the `opencode service` CLI. `pid` is
    /// the daemon the plan selected — its environment names the state home the
    /// CLI must operate in, and it is the process the wait must see replaced.
    /// `port` identifies WHICH service to wait for — a restart keeps its port,
    /// so a second, unrelated service cannot satisfy the wait.
    Service { pid: i32, port: u16 },
    /// Re-raise cola's Owned Server on the same port and password.
    Owned { pid: i32, port: u16, password: String },
    /// The canonical server is someone else's classic `serve --port`.
    NotOwned,
    /// Nothing eligible on the default store.
    NoServer,
}

/// The restart decision — a pure function so Service / Owned / Coexistent /
/// none are unit-tested without touching a process. `preferred_port` is the
/// `[opencode] url` tiebreaker [`pick_server`] applies, so the plan restarts
/// the SAME canonical server cola is attached to (spec #809: "同类多个：配置的
/// `[opencode] url` 端口优先，否则端口最小"; ADR-0085).
fn restart_plan(
    candidates: &[ServerCandidate],
    preferred_port: Option<u16>,
    self_pid: Option<i32>,
) -> RestartPlan {
    let Some(server) = pick_server(candidates, preferred_port, self_pid) else {
        return RestartPlan::NoServer;
    };
    match server.class_against(self_pid) {
        ServerClass::Service => RestartPlan::Service {
            pid: server.pid,
            port: server.port,
        },
        ServerClass::Owned => RestartPlan::Owned {
            pid: server.pid,
            port: server.port,
            password: server.password.clone(),
        },
        ServerClass::Coexistent => RestartPlan::NotOwned,
    }
}

/// How long `/restart-opencode` waits for the managed service to serve again.
const SERVICE_RESTART_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

/// How long the `opencode service restart` CLI itself may take before it is
/// killed. Bounded so a wedged CLI (a stalled daemon supervisor, a hung lock)
/// cannot hang the handler short of the bounded service wait below or its
/// reply — the restart is a helper, not the thing being waited for.
const SERVICE_RESTART_CLI_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(20);

/// Restart the running OpenCode server according to [`restart_plan`].
///
/// A V2 Shared Service is restarted through its own CLI
/// (`opencode service restart`); cola never kills or re-spawns it with
/// `serve --port`, and never writes `self-opencode.pid` for it (ADR-0085).
/// An Owned Server keeps the old path: terminate, wait for the port to free,
/// re-raise on the same port/password, wait for it to accept. Any other
/// server is someone else's — `NotOwned` tells the caller to ask for a manual
/// restart, because cola has no record of how it was configured.
///
/// `preferred_port` is the `[opencode] url` tiebreaker: the restart follows
/// the canonical pick, so among several servers of one class it restarts the
/// configured one rather than a lower-port sibling (spec #809, ADR-0085).
pub async fn restart_self_spawned_server(
    layout: &StateLayout,
    preferred_port: Option<u16>,
) -> anyhow::Result<RestartOutcome> {
    let candidates = scan_processes();
    match restart_plan(&candidates, preferred_port, self_spawned_pid(layout)) {
        RestartPlan::NoServer => Ok(RestartOutcome::NoServer),
        RestartPlan::NotOwned => Ok(RestartOutcome::NotOwned),
        RestartPlan::Service { pid, port } => {
            restart_managed_service(&RealServiceRestartHost, pid, port).await
        }
        RestartPlan::Owned { pid, port, password } => {
            // Confirm ownership against the live process before killing: the
            // record must still name this pid AND it must be a real
            // `opencode serve` (a recycled pid is never killed).
            if !is_self_spawned(layout, pid) {
                return Ok(RestartOutcome::NotOwned);
            }
            restart_owned_server(layout, pid, port, password).await
        }
    }
}

/// The outside world the Shared Service restart path touches: the daemon's own
/// environment (which names its state home), the `opencode service restart`
/// CLI, and the probe that decides whether the daemon actually changed.
/// Injected so both the state-home choice and the "did it really restart?"
/// check are unit-tested without a process, a socket, or the real binary
/// (ticket #809). Real code uses [`RealServiceRestartHost`].
#[async_trait]
trait ServiceRestartHost: Send + Sync {
    /// A pid's environment lines, when visible.
    fn environ(&self, pid: i32) -> Option<Vec<String>>;
    /// Run the `opencode service restart` command, bounded by
    /// [`SERVICE_RESTART_CLI_TIMEOUT`].
    async fn run_restart_cli(&self, cmd: &SpawnCommand) -> anyhow::Result<()>;
    /// The pid of a live `serve --service` on `port` whose endpoint accepts
    /// connections, when there is one.
    async fn serving_service_pid(&self, port: u16) -> Option<i32>;
}

/// The real operations: [`process_environ`], the real `opencode` binary, and a
/// [`scan_processes`] + TCP probe.
struct RealServiceRestartHost;

#[async_trait]
impl ServiceRestartHost for RealServiceRestartHost {
    fn environ(&self, pid: i32) -> Option<Vec<String>> {
        process_environ(pid)
    }

    async fn run_restart_cli(&self, cmd: &SpawnCommand) -> anyhow::Result<()> {
        run_service_restart_cli("opencode", cmd, SERVICE_RESTART_CLI_TIMEOUT).await
    }

    async fn serving_service_pid(&self, port: u16) -> Option<i32> {
        let live = scan_processes()
            .into_iter()
            .find(|c| c.is_service() && c.port == port)?;
        if tokio::net::TcpStream::connect(("127.0.0.1", port)).await.is_ok() {
            Some(live.pid)
        } else {
            None
        }
    }
}

/// The `opencode service restart` command, with `XDG_STATE_HOME` pinned to the
/// SELECTED service's state home.
///
/// `opencode service` acts on the registration under the CLI's OWN
/// `XDG_STATE_HOME`, so a restart that inherited cola's environment would drive
/// whatever registration lives in cola's state home — a different service
/// whenever the selected one was started under another state home, leaving the
/// selected daemon untouched (ticket #809). The service's own value is set when
/// its environment carries a non-empty `XDG_STATE_HOME`; cola's inherited value
/// is always stripped, so a service with none of its own keeps the default
/// `~/.local/state`, which is where that service actually registered
/// (xdg-basedir, spec #364 §2). The strip is applied before the set — the last
/// write wins — so the service's value is the one the CLI sees.
fn service_restart_command(service_env: Option<&[String]>) -> SpawnCommand {
    let state_home = service_env
        .and_then(|env| env.iter().find_map(|kv| kv.strip_prefix("XDG_STATE_HOME=")))
        .filter(|value| !value.is_empty())
        .map(|value| value.to_string());
    SpawnCommand {
        args: vec!["service".to_string(), "restart".to_string()],
        env: state_home
            .map(|home| vec![("XDG_STATE_HOME".to_string(), home)])
            .unwrap_or_default(),
        remove_env: vec!["XDG_STATE_HOME".to_string()],
    }
}

/// Restart the V2 Shared Service without taking ownership of it. The daemon
/// rewrites its registration file; cola waits, bounded, until a DIFFERENT live
/// `serve --service` accepts connections on `port` again.
///
/// `pid` is the service the plan selected, and it is used twice: its environment
/// names the state home the CLI must run in ([`service_restart_command`]), and
/// it is the daemon the wait must see REPLACED — `opencode service restart` is
/// `stop` + `ensure`, so a successful restart always leaves a new daemon behind.
async fn restart_managed_service(
    host: &dyn ServiceRestartHost,
    pid: i32,
    port: u16,
) -> anyhow::Result<RestartOutcome> {
    tracing::info!("restarting the V2 Shared Service (`opencode service restart`)");
    let cmd = service_restart_command(host.environ(pid).as_deref());
    host.run_restart_cli(&cmd).await?;
    wait_for_live_service(host, pid, port, SERVICE_RESTART_TIMEOUT).await?;
    tracing::info!("shared OpenCode service is serving again");
    Ok(RestartOutcome::RestartedService)
}

/// Run `opencode service restart` and wait for it, bounded by `timeout`.
///
/// The CLI is a child process like any other: if it stalls, an unbounded
/// `.status()`/`wait()` would hold `/restart-opencode` before it ever reaches
/// the bounded service wait (or replies). On timeout the child is killed AND
/// reaped (`Child::kill` waits it out) and a clear error is returned; a
/// non-zero exit is still reported as before. `kill_on_drop` covers the one
/// remaining escape — the enclosing future being cancelled mid-wait.
async fn run_service_restart_cli(
    program: &str,
    cmd: &SpawnCommand,
    timeout: std::time::Duration,
) -> anyhow::Result<()> {
    let mut command = tokio::process::Command::new(program);
    command.args(&cmd.args).kill_on_drop(true);
    for key in &cmd.remove_env {
        command.env_remove(key);
    }
    for (key, value) in &cmd.env {
        command.env(key, value);
    }
    let mut child = command.spawn()?;
    match tokio::time::timeout(timeout, child.wait()).await {
        Ok(status) => {
            let status = status?;
            if !status.success() {
                anyhow::bail!("`opencode service restart` exited with {status}");
            }
            Ok(())
        }
        Err(_) => {
            let _ = child.kill().await;
            anyhow::bail!(
                "`opencode service restart` did not exit within {}s; killed",
                timeout.as_secs()
            )
        }
    }
}

/// Poll until a live `serve --service` on `port` accepts connections AND is not
/// the daemon the restart was meant to replace. `scan_processes` already refuses
/// a stale registration pid, so a hit here is a service the CLI just raised.
///
/// The port is the identity: a restart keeps it, while matching only "some
/// service" would let a second Shared Service on another port satisfy the wait
/// before the restarted one is up.
///
/// The pid is the second half of the identity: `opencode service restart` can
/// exit 0 while `pre_pid` — the daemon selected before the CLI ran — is still
/// the one serving (the CLI drove another state home's registration, or the
/// service ignored the request). Reporting `RestartedService` then would tell
/// the user a restart happened that did not, so the wait keeps polling until
/// the daemon changes and fails with that finding if it never does.
async fn wait_for_live_service(
    host: &dyn ServiceRestartHost,
    pre_pid: i32,
    port: u16,
    timeout: std::time::Duration,
) -> anyhow::Result<()> {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        let serving = host.serving_service_pid(port).await;
        if serving.is_some_and(|pid| pid != pre_pid) {
            return Ok(());
        }
        if tokio::time::Instant::now() >= deadline {
            if serving == Some(pre_pid) {
                anyhow::bail!(
                    "`opencode service restart` returned success, but the OpenCode service on port {port} is still pid {pre_pid}: the daemon did not change, so the restart cannot be confirmed"
                );
            }
            anyhow::bail!(
                "the OpenCode service on port {port} did not accept connections within {}s of restarting",
                timeout.as_secs()
            );
        }
        tokio::time::sleep(std::time::Duration::from_millis(250)).await;
    }
}

/// The Owned Server path: kill our own process, wait for the port to release,
/// re-raise it on the same port/password, and wait until it accepts.
async fn restart_owned_server(
    layout: &StateLayout,
    pid: i32,
    port: u16,
    password: String,
) -> anyhow::Result<RestartOutcome> {
    let _ = clear_self_spawned(layout);
    if let Err(e) = terminate_process(pid) {
        tracing::warn!("terminate self-spawned opencode {} failed: {}", pid, e);
        return Err(e);
    }
    tracing::info!("restarting self-spawned opencode (pid {})", pid);
    // Give the process time to die and the port to free up.
    for _ in 0..50 {
        if !process_alive(pid) {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    for _ in 0..25 {
        if tokio::net::TcpStream::connect(("127.0.0.1", port)).await.is_err() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    }
    let new_pid = spawn_self_server(layout, port, &password)?;
    wait_for_port(port).await?;
    tracing::info!(
        "restarted self-spawned opencode on port {} (pid {})",
        port,
        new_pid
    );
    Ok(RestartOutcome::Restarted)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A layout rooted at `dir`, so a test's pid record and service log never
    /// touch the real `~/.cola`.
    fn test_layout(dir: &std::path::Path) -> StateLayout {
        StateLayout::new(dir.join("sessions.json"), Some(dir.to_path_buf()))
    }

    fn cand(pid: i32, port: u16, password: &str, uses_default_store: bool) -> ServerCandidate {
        ServerCandidate {
            pid,
            port,
            password: password.into(),
            username: DEFAULT_SERVER_USERNAME.into(),
            uses_default_store,
            class: ServerClass::Coexistent,
        }
    }

    fn service_cand(pid: i32, port: u16) -> ServerCandidate {
        ServerCandidate {
            class: ServerClass::Service,
            ..cand(pid, port, "service", true)
        }
    }

    #[test]
    fn server_scan_refresh_excludes_tasks() {
        // Regression (see server_scan_refresh docs): sysinfo's default
        // `tasks: true` turns each thread of an `opencode serve` process into
        // its own candidate, which pollutes Coexistent detection and makes
        // reconcile yield-kill cola's Owned Server. The scan refresh must not
        // enumerate tasks.
        assert!(
            !server_scan_refresh().tasks(),
            "server scan must not treat threads as separate server candidates"
        );
    }

    #[test]
    fn pick_server_prefers_default_store() {
        let custom = cand(1, 4096, "custom", false);
        let def = cand(2, 4001, "default", true);
        let servers = [custom, def];
        let chosen = pick_server(&servers, None, None).unwrap();
        assert_eq!(chosen.port, 4001);
    }

    #[test]
    fn pick_server_prefers_configured_port_among_same_class() {
        let a = cand(1, 4001, "x", true);
        let b = cand(2, 4096, "y", true);
        let servers = [a, b];
        let chosen = pick_server(&servers, Some(4096), None).unwrap();
        assert_eq!(chosen.port, 4096);
    }

    #[test]
    fn pick_server_never_hijacks_custom_store() {
        let custom = cand(1, 4096, "custom", false);
        let servers = [custom];
        assert!(pick_server(&servers, Some(4096), None).is_none());
    }

    #[test]
    fn pick_server_empty_returns_none() {
        assert!(pick_server(&[], Some(4096), None).is_none());
    }

    #[test]
    fn pick_server_prefers_coexistent_over_owned() {
        // cola's Owned Server (pid matches self_pid) loses to a Coexistent
        // Server, even when the Owned one sits on the configured port.
        let owned = cand(1, 4096, "cola-secret", true);
        let coexist = cand(2, 52137, "openchamber", true);
        let servers = [owned, coexist];
        let chosen = pick_server(&servers, Some(4096), Some(1)).unwrap();
        assert_eq!(chosen.pid, 2, "a coexistent server must win over the owned one");
    }

    #[test]
    fn pick_server_keeps_owned_when_no_coexistent() {
        // No coexistent server: cola attaches to its Owned Server, honoring the
        // configured port among the same class.
        let owned = cand(1, 4096, "cola-secret", true);
        let servers = [owned];
        let chosen = pick_server(&servers, Some(4096), Some(1)).unwrap();
        assert_eq!(chosen.pid, 1);
    }

    #[test]
    fn pick_server_owned_not_matched_self_pid_is_coexistent() {
        // self_pid is only the recorded Owned Server; a different pid on the
        // same store is coexist(ent) — it wins.
        let owned = cand(1, 4096, "cola-secret", true);
        let other = cand(9, 4002, "manual", true);
        let servers = [owned, other];
        let chosen = pick_server(&servers, None, Some(1)).unwrap();
        assert_eq!(chosen.pid, 9);
    }

    #[test]
    fn pick_server_is_deterministic_within_a_class() {
        // Two coexistent servers and no preferred port: the lowest port wins
        // regardless of the (randomized) scan order, so the reconnect loop
        // never flaps between them (ADR-0013).
        let a = cand(1, 4096, "x", true);
        let b = cand(2, 52137, "y", true);
        assert_eq!(
            pick_server(&[a.clone(), b.clone()], None, None).unwrap().port,
            4096
        );
        assert_eq!(pick_server(&[b, a], None, None).unwrap().port, 4096);
    }

    #[test]
    fn pick_server_never_picks_a_port_zero_candidate() {
        // Port 0 is never an endpoint: a candidate carrying it must not win
        // over an attachable one (nor be returned alone), even when the
        // configured tiebreaker names it (issue #748).
        let private = cand(1, 0, "stdio", true);
        let healthy = cand(2, 4096, "x", true);
        assert_eq!(
            pick_server(&[private.clone(), healthy.clone()], None, None)
                .unwrap()
                .port,
            4096
        );
        assert_eq!(
            pick_server(&[healthy, private.clone()], Some(0), None)
                .unwrap()
                .port,
            4096
        );
        assert!(pick_server(&[private], None, None).is_none());
    }

    #[test]
    fn scan_classifies_a_service_from_its_argv() {
        let args = |parts: &[&str]| parts.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert_eq!(
            class_from_args(&args(&["opencode", "serve", "--service"])),
            ServerClass::Service
        );
        assert_eq!(
            class_from_args(&args(&["opencode", "serve", "--port", "4096"])),
            ServerClass::Coexistent
        );
    }

    #[test]
    fn pick_server_prefers_service_over_a_smaller_port_coexistent() {
        // The V2 Shared Service is the machine-wide rendezvous every default
        // client uses; an OpenChamber/classic server on a lower port must not
        // steal the attach (ADR-0085).
        let coexist = cand(2, 4001, "openchamber", true);
        let service = service_cand(3, 49374);
        assert_eq!(
            pick_server(&[coexist.clone(), service.clone()], None, None)
                .unwrap()
                .pid,
            3
        );
        assert_eq!(pick_server(&[service, coexist], None, None).unwrap().pid, 3);
    }

    #[test]
    fn pick_server_honors_the_configured_port_within_the_service_class() {
        let a = service_cand(1, 49374);
        let b = service_cand(2, 49375);
        assert_eq!(pick_server(&[a, b], Some(49375), None).unwrap().port, 49375);
    }

    #[test]
    fn pick_server_prefers_service_even_over_the_owned_server() {
        let owned = cand(1, 4096, "cola-secret", true);
        let service = service_cand(2, 49374);
        assert_eq!(
            pick_server(&[owned, service], Some(4096), Some(1)).unwrap().pid,
            2
        );
    }

    #[test]
    fn pick_server_keeps_owned_last_across_scan_orders() {
        // Whatever order sysinfo reports, the Owned Server never outranks a
        // Service or a Coexistent one — even on the configured port.
        let owned = cand(1, 4096, "cola-secret", true);
        let service = service_cand(2, 49374);
        let coexist = cand(3, 4001, "openchamber", true);
        for servers in [
            [owned.clone(), coexist.clone(), service.clone()],
            [service.clone(), owned.clone(), coexist.clone()],
            [coexist.clone(), service.clone(), owned.clone()],
        ] {
            assert_eq!(
                pick_server(&servers, Some(4096), Some(1)).unwrap().pid,
                2,
                "the service is canonical"
            );
        }
        let coexist_only = [owned.clone(), coexist];
        assert_eq!(
            pick_server(&coexist_only, Some(4096), Some(1)).unwrap().pid,
            3,
            "without a service the coexistent still beats the owned"
        );
        assert_eq!(
            pick_server(&[owned], Some(4096), Some(1)).unwrap().pid,
            1,
            "the owned server is the last resort"
        );
    }

    #[test]
    fn other_servers_lists_eligible_non_canonical_servers_deterministically() {
        let service = service_cand(1, 49374);
        let coexist = cand(2, 4001, "x", true);
        let owned = cand(3, 4096, "cola-secret", true);
        let custom = cand(4, 3000, "custom", false);
        let private = cand(5, 0, "stdio", true);
        let all = [private, custom, owned, coexist, service.clone()];
        // Selection priority, then port: coexistent (pid 2) precedes owned
        // (pid 3). Custom-store and port-0 candidates are not servers cola
        // could attach to at all.
        let others = other_servers(&all, &service);
        assert_eq!(others.iter().map(|c| c.pid).collect::<Vec<_>>(), vec![2, 3]);
        assert!(other_servers(std::slice::from_ref(&service), &service).is_empty());
    }

    #[test]
    fn a_service_is_never_owned_even_when_the_pid_record_names_it() {
        // cola never records a service pid, and a corrupt/stale record must
        // not turn the shared rendezvous into something cola may reap: the
        // Service class wins over the pid match (ADR-0085).
        let service = service_cand(1, 49374);
        assert_eq!(service.class_against(Some(1)), ServerClass::Service);
        assert_eq!(pick_server(&[service], None, Some(1)).unwrap().pid, 1);
    }

    #[test]
    fn restart_plan_restarts_the_shared_service() {
        let owned = cand(1, 4096, "cola-secret", true);
        let service = service_cand(2, 49374);
        assert_eq!(
            restart_plan(&[owned, service], None, Some(1)),
            RestartPlan::Service { pid: 2, port: 49374 }
        );
        // The port pins WHICH service the wait targets: a second service on
        // another port cannot satisfy it.
        let a = service_cand(3, 49374);
        let b = service_cand(4, 49375);
        assert_eq!(
            restart_plan(&[b, a], None, None),
            RestartPlan::Service { pid: 3, port: 49374 }
        );
    }

    /// The restart follows the SAME canonical pick as the attach, preferred
    /// port included: with two Shared Services and `[opencode] url` naming the
    /// higher port, `/restart-opencode` must restart the one cola is attached
    /// to, not the lower-port sibling (spec #809: "同类多个：配置的
    /// `[opencode] url` 端口优先，否则端口最小"; ADR-0085).
    #[test]
    fn restart_plan_honors_the_preferred_port_among_services() {
        let low = service_cand(3, 49374);
        let high = service_cand(4, 49375);
        assert_eq!(
            restart_plan(&[low.clone(), high.clone()], Some(49375), None),
            RestartPlan::Service { pid: 4, port: 49375 }
        );
        // No preference keeps the lowest port, unchanged.
        assert_eq!(
            restart_plan(&[high, low], None, None),
            RestartPlan::Service { pid: 3, port: 49374 }
        );
    }

    /// Write an executable `sh` stub for the `opencode service restart` tests.
    /// The stub receives the real `service restart` args and ignores them: these
    /// tests exercise the bound and the exit status, not the CLI's behavior.
    #[cfg(unix)]
    fn stub_cli(dir: &std::path::Path, name: &str, body: &str) -> std::path::PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let path = dir.join(name);
        std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
        let mut perms = std::fs::metadata(&path).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&path, perms).unwrap();
        path
    }

    /// A `service restart` CLI that stalls must be killed and reaped at its
    /// bound, not waited on forever: `/restart-opencode` still owes the caller
    /// the bounded service wait and a reply.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_stalled_service_restart_cli_is_killed_at_its_bound() {
        let dir = tempfile::tempdir().unwrap();
        // `exec` so the killed child IS the sleeper — no orphaned `sleep` left.
        let cli = stub_cli(dir.path(), "opencode", "exec sleep 5");
        let started = std::time::Instant::now();
        let error = run_service_restart_cli(
            cli.to_str().unwrap(),
            &service_restart_command(None),
            std::time::Duration::from_millis(200),
        )
        .await
        .expect_err("a stalling CLI must not be waited on forever");

        assert!(error.to_string().contains("did not exit"), "{error}");
        // The bound — not the child's own 5s sleep — ends the wait.
        assert!(
            started.elapsed() < std::time::Duration::from_secs(3),
            "the CLI bound did not fire: waited {:?}",
            started.elapsed()
        );
    }

    /// A non-zero exit is still a failed restart, and a clean exit succeeds.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_service_restart_cli_reports_its_exit_status() {
        let dir = tempfile::tempdir().unwrap();
        let ok = stub_cli(dir.path(), "opencode-ok", "exit 0");
        run_service_restart_cli(
            ok.to_str().unwrap(),
            &service_restart_command(None),
            std::time::Duration::from_secs(5),
        )
        .await
        .expect("a clean exit is a successful restart");

        let bad = stub_cli(dir.path(), "opencode-bad", "exit 3");
        let error = run_service_restart_cli(
            bad.to_str().unwrap(),
            &service_restart_command(None),
            std::time::Duration::from_secs(5),
        )
        .await
        .expect_err("a non-zero exit is a failed restart");
        assert!(error.to_string().contains("exited with"), "{error}");
    }

    /// The restart CLI acts on the registration under ITS `XDG_STATE_HOME`, so
    /// the command must carry the SELECTED service's own value: a restart that
    /// inherited cola's would drive a different registration and leave the
    /// selected daemon untouched (ticket #809).
    #[test]
    fn service_restart_command_pins_the_services_state_home() {
        let cmd = service_restart_command(Some(&env_lines(&[
            ("XDG_STATE_HOME", "/srv/state"),
            ("PATH", "/usr/bin"),
        ])));
        assert_eq!(cmd.args, argv(&["service", "restart"]));
        assert_eq!(
            cmd.env,
            vec![("XDG_STATE_HOME".to_string(), "/srv/state".to_string())]
        );
        // cola's own inherited value is stripped even when the service has one
        // to set, so only the service's state home can ever reach the CLI.
        assert_eq!(cmd.remove_env, vec!["XDG_STATE_HOME".to_string()]);
    }

    /// A service with no `XDG_STATE_HOME` of its own registered under the
    /// default `~/.local/state`, which is where the CLI looks once cola's value
    /// is stripped — so no replacement is set. An empty value and an unreadable
    /// environment are the same case (xdg-basedir, spec #364 §2).
    #[test]
    fn service_restart_command_falls_back_to_the_default_state_home() {
        let no_value = service_restart_command(Some(&env_lines(&[("PATH", "/usr/bin")])));
        let empty = service_restart_command(Some(&env_lines(&[("XDG_STATE_HOME", "")])));
        let unreadable = service_restart_command(None);
        for cmd in [&no_value, &empty, &unreadable] {
            assert!(cmd.env.is_empty(), "no state home may be set: {:?}", cmd.env);
            assert_eq!(cmd.remove_env, vec!["XDG_STATE_HOME".to_string()]);
            assert_eq!(cmd.args, argv(&["service", "restart"]));
        }
    }

    /// The built command's environment actually reaches the child process: the
    /// service's state home is set, and cola's inherited value never survives.
    /// Without this the choice above could be made and then dropped on the way
    /// to the process.
    #[cfg(unix)]
    #[tokio::test]
    async fn the_restart_cli_receives_the_services_state_home() {
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("state-home.txt");
        let cli = stub_cli(
            dir.path(),
            "opencode",
            &format!("printf '%s' \"${{XDG_STATE_HOME-unset}}\" > {}", out.display()),
        );

        let with_home = service_restart_command(Some(&env_lines(&[("XDG_STATE_HOME", "/srv/state")])));
        run_service_restart_cli(
            cli.to_str().unwrap(),
            &with_home,
            std::time::Duration::from_secs(5),
        )
        .await
        .expect("a clean exit is a successful restart");
        assert_eq!(std::fs::read_to_string(&out).unwrap(), "/srv/state");

        let without = service_restart_command(None);
        run_service_restart_cli(cli.to_str().unwrap(), &without, std::time::Duration::from_secs(5))
            .await
            .expect("a clean exit is a successful restart");
        assert_eq!(
            std::fs::read_to_string(&out).unwrap(),
            "unset",
            "cola's own XDG_STATE_HOME must not reach the CLI"
        );
    }

    /// A restart host with every operation scripted: the daemon's environment,
    /// the CLI's outcome, and the pid (if any) serving afterwards. Nothing is
    /// spawned, no socket is opened, and no real registration is touched.
    struct FakeServiceRestartHost {
        /// Each pid's environment lines; a pid absent here is "unreadable".
        environ: Vec<(i32, Vec<String>)>,
        /// The pid a live `serve --service` on the port reports after the
        /// restart; `None` means nothing is serving.
        serving: Option<i32>,
        /// Every restart command the path ran.
        commands: std::sync::Mutex<Vec<SpawnCommand>>,
    }

    impl FakeServiceRestartHost {
        fn new(serving: Option<i32>) -> Self {
            Self {
                environ: Vec::new(),
                serving,
                commands: std::sync::Mutex::new(Vec::new()),
            }
        }

        fn with_env(mut self, pid: i32, env: Vec<String>) -> Self {
            self.environ.push((pid, env));
            self
        }

        fn commands(&self) -> Vec<SpawnCommand> {
            self.commands.lock().unwrap().clone()
        }
    }

    #[async_trait]
    impl ServiceRestartHost for FakeServiceRestartHost {
        fn environ(&self, pid: i32) -> Option<Vec<String>> {
            self.environ
                .iter()
                .find(|(p, _)| *p == pid)
                .map(|(_, env)| env.clone())
        }

        async fn run_restart_cli(&self, cmd: &SpawnCommand) -> anyhow::Result<()> {
            self.commands.lock().unwrap().push(cmd.clone());
            Ok(())
        }

        async fn serving_service_pid(&self, _port: u16) -> Option<i32> {
            self.serving
        }
    }

    /// The whole restart path runs the CLI in the SELECTED service's state home
    /// — the daemon's own environment decides, not cola's (ticket #809).
    #[tokio::test]
    async fn restarting_the_service_runs_the_cli_in_the_services_state_home() {
        let host = FakeServiceRestartHost::new(Some(99))
            .with_env(42, env_lines(&[("XDG_STATE_HOME", "/srv/state")]));
        let outcome = restart_managed_service(&host, 42, 49374)
            .await
            .expect("a different pid serving on the port is a confirmed restart");

        assert_eq!(outcome, RestartOutcome::RestartedService);
        let commands = host.commands();
        assert_eq!(commands.len(), 1, "exactly one restart CLI run");
        assert_eq!(commands[0].args, argv(&["service", "restart"]));
        assert_eq!(
            commands[0].env,
            vec![("XDG_STATE_HOME".to_string(), "/srv/state".to_string())]
        );
        assert_eq!(commands[0].remove_env, vec!["XDG_STATE_HOME".to_string()]);
    }

    /// A service whose environment carries no `XDG_STATE_HOME` is restarted
    /// with cola's own value stripped and none set, so the CLI keeps the
    /// default `~/.local/state` — where that service registered (ticket #809).
    #[tokio::test]
    async fn restarting_the_service_strips_colas_own_state_home() {
        let host = FakeServiceRestartHost::new(Some(99)).with_env(42, env_lines(&[("PATH", "/usr/bin")]));
        restart_managed_service(&host, 42, 49374)
            .await
            .expect("a different pid serving on the port is a confirmed restart");

        let commands = host.commands();
        assert!(commands[0].env.is_empty(), "{:?}", commands[0].env);
        assert_eq!(commands[0].remove_env, vec!["XDG_STATE_HOME".to_string()]);
    }

    /// `opencode service restart` can exit 0 while the daemon it was meant to
    /// replace is still the one serving — the CLI drove another state home, or
    /// the service ignored the request. Reporting `RestartedService` then tells
    /// the user a restart happened that did not, so the wait requires the
    /// daemon to have CHANGED (ticket #809). Paused time keeps the bounded wait
    /// off the wall clock.
    #[tokio::test(start_paused = true)]
    async fn a_service_that_did_not_change_is_not_reported_as_restarted() {
        let host = FakeServiceRestartHost::new(Some(42));
        let error = restart_managed_service(&host, 42, 49374)
            .await
            .expect_err("the same pid still serving is not a restart");

        let message = error.to_string();
        assert!(message.contains("did not change"), "{message}");
        assert!(message.contains("pid 42"), "{message}");
        assert_eq!(host.commands().len(), 1, "the CLI still ran");
    }

    /// The pid check does not swallow the other failure: a port nothing serves
    /// on still reports the timeout it always did.
    #[tokio::test(start_paused = true)]
    async fn a_service_that_never_comes_back_reports_the_timeout() {
        let host = FakeServiceRestartHost::new(None);
        let error = restart_managed_service(&host, 42, 49374)
            .await
            .expect_err("nothing serving on the port is not a restart");

        assert!(
            error.to_string().contains("did not accept connections"),
            "{error}"
        );
    }

    #[test]
    fn restart_plan_restarts_the_owned_server_on_its_own_port_and_password() {
        let owned = cand(1, 4096, "cola-secret", true);
        assert_eq!(
            restart_plan(&[owned], None, Some(1)),
            RestartPlan::Owned {
                pid: 1,
                port: 4096,
                password: "cola-secret".into()
            }
        );
    }

    #[test]
    fn restart_plan_reports_not_owned_for_a_coexistent_server() {
        let coexist = cand(2, 52137, "openchamber", true);
        assert_eq!(restart_plan(&[coexist], None, None), RestartPlan::NotOwned);
        // The plan follows the canonical pick: an Owned Server that the
        // Coexistent one outranks is about to be yielded, not restarted.
        let owned = cand(1, 4096, "cola-secret", true);
        let coexist = cand(2, 52137, "openchamber", true);
        assert_eq!(
            restart_plan(&[owned, coexist], None, Some(1)),
            RestartPlan::NotOwned
        );
    }

    #[test]
    fn restart_plan_reports_no_server() {
        assert_eq!(restart_plan(&[], None, None), RestartPlan::NoServer);
        let custom = cand(1, 4096, "x", false);
        assert_eq!(restart_plan(&[custom], None, None), RestartPlan::NoServer);
        let private = cand(2, 0, "stdio", true);
        assert_eq!(restart_plan(&[private], None, None), RestartPlan::NoServer);
    }

    #[test]
    fn pick_server_is_deterministic_across_scan_order_for_services() {
        let a = service_cand(1, 49374);
        let b = service_cand(2, 49375);
        assert_eq!(
            pick_server(&[a.clone(), b.clone()], None, None).unwrap().port,
            49374
        );
        assert_eq!(pick_server(&[b, a], None, None).unwrap().port, 49374);
    }

    #[test]
    fn self_start_command_spawns_default_store_server() {
        let cmd = self_start_command(4096, "secret");
        assert_eq!(
            cmd.args,
            vec!["serve", "--port", "4096", "--hostname", "127.0.0.1"]
        );
        assert!(
            cmd.env
                .contains(&("OPENCODE_SERVER_PASSWORD".into(), "secret".into()))
        );
        // The server MUST land on the default store so sessions are shared with
        // OpenChamber / the CLI: strip any inherited XDG_DATA_HOME rather than
        // pinning a private one.
        assert!(cmd.remove_env.contains(&"XDG_DATA_HOME".to_string()));
        // A cola-spawned server must use the DEFAULT username `opencode`, so
        // cola's own client (which sends that) never 401s against it.
        assert!(cmd.remove_env.contains(&"OPENCODE_SERVER_USERNAME".to_string()));
        // The inherited PREFERRED password name must be stripped: cola probes
        // with the value it SET below, so an inherited `OPENCODE_PASSWORD`
        // would shadow it and 401 every attach (#812 precedence).
        assert!(cmd.remove_env.contains(&"OPENCODE_PASSWORD".to_string()));
    }

    /// The spawned Owned Server must authenticate with the password cola probes
    /// with — never one cola merely inherited. OpenCode's `Env.password` prefers
    /// `OPENCODE_PASSWORD` over the `OPENCODE_SERVER_PASSWORD` set here, so
    /// without the strip a cola launched from a shell that exported it would
    /// leave `spawn_own_server_over`'s `serve --port` fallback (#810) failing to
    /// attach (#812 precedence).
    #[test]
    fn self_start_command_cannot_be_poisoned_by_an_inherited_password() {
        let cmd = self_start_command(4096, "cola-secret");
        // Exactly one password name is set: the fallback, with cola's value.
        assert_eq!(
            cmd.env
                .iter()
                .filter(|(k, _)| k.starts_with("OPENCODE_"))
                .cloned()
                .collect::<Vec<_>>(),
            vec![("OPENCODE_SERVER_PASSWORD".to_string(), "cola-secret".to_string())]
        );
        // Exactly the other one is stripped, so the two never both apply.
        assert!(cmd.remove_env.contains(&"OPENCODE_PASSWORD".to_string()));
        assert!(
            !cmd.remove_env.contains(&"OPENCODE_SERVER_PASSWORD".to_string()),
            "the password cola probes with must not be stripped: {:?}",
            cmd.remove_env
        );
        // Model the child environment `spawn_self_server` builds — `env` applied
        // first, then `remove_env` — and resolve it the way OpenCode's
        // `Env.password` does: preferred name, else legacy name. An inherited
        // preferred password resolves to nothing; only cola's own value survives.
        let effective = |key: &str| -> Option<String> {
            if cmd.remove_env.iter().any(|k| k == key) {
                return None;
            }
            cmd.env.iter().find(|(k, _)| k == key).map(|(_, v)| v.clone())
        };
        let resolved = effective("OPENCODE_PASSWORD").or_else(|| effective("OPENCODE_SERVER_PASSWORD"));
        assert_eq!(
            resolved.as_deref(),
            Some("cola-secret"),
            "the Owned Server must resolve to the password cola probes with"
        );
    }

    #[test]
    fn self_spawned_pid_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let layout = test_layout(dir.path());
        let path = layout.self_spawned_pid();
        assert_eq!(read_self_spawned_pid_in(&path), None);
        record_self_spawned(&layout, 4242);
        assert_eq!(read_self_spawned_pid_in(&path), Some(4242));
        assert_eq!(clear_self_spawned(&layout), Some(4242));
        assert_eq!(read_self_spawned_pid_in(&path), None);
    }

    #[test]
    fn self_spawned_ownership_matches_only_recorded_pid() {
        let dir = tempfile::tempdir().unwrap();
        let layout = test_layout(dir.path());
        let path = layout.self_spawned_pid();
        // Nothing recorded → no server is "owned".
        assert!(!is_self_spawned_record(&path, 1));
        record_self_spawned(&layout, 7);
        assert!(is_self_spawned_record(&path, 7));
        assert!(!is_self_spawned_record(&path, 8));
    }

    #[test]
    fn live_opencode_serve_requires_real_serve_process() {
        // A non-existent pid is never a live opencode serve.
        assert!(!is_live_opencode_serve(999_999_999));
        // This test process is alive but not an `opencode serve`.
        let own_pid = std::process::id() as i32;
        assert!(!is_live_opencode_serve(own_pid));
    }

    #[test]
    fn stale_self_spawned_record_not_owned_publicly() {
        // The public `is_self_spawned` requires BOTH a matching record and a
        // live `opencode serve` — a stale record for a dead pid must not grant
        // ownership (the running server may be a recycled pid).
        let dir = tempfile::tempdir().unwrap();
        let layout = test_layout(dir.path());
        let path = layout.self_spawned_pid();
        record_self_spawned_in(&path, 999_999_999);
        // Record matches, but pid is not a live serve → not owned.
        assert!(is_self_spawned_record(&path, 999_999_999));
        // Public check would consult the real pid file (not `path`), so this is
        // the unit-level claim: the record alone is insufficient. The liveness
        // gate is exercised by live_opencode_serve_requires_real_serve_process.
        assert!(!is_live_opencode_serve(999_999_999));
    }

    #[test]
    fn process_alive_detects_own_process() {
        // This test process is alive on every platform.
        assert!(process_alive(std::process::id() as i32));
        // A far-out pid is dead everywhere.
        assert!(!process_alive(i32::MAX - 1));
    }

    #[test]
    fn process_cmd_reads_own_command_line() {
        // The test binary's own cmdline is readable on every platform; it names
        // the crate (cola) so the args are non-empty.
        let cmd = process_cmd(std::process::id() as i32).expect("own cmd readable");
        assert!(!cmd.is_empty());
        // A non-existent pid yields no cmdline.
        assert!(process_cmd(999_999_999).is_none());
    }

    /// Environment lines the way sysinfo returns them (`KEY=VALUE`).
    fn env_lines(pairs: &[(&str, &str)]) -> Vec<String> {
        pairs.iter().map(|(k, v)| format!("{k}={v}")).collect()
    }

    /// A process argv as sysinfo returns it (one `String` per token).
    fn argv(parts: &[&str]) -> Vec<String> {
        parts.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn server_candidate_url_uses_the_one_loopback_spelling() {
        assert_eq!(cand(7, 4096, "x", true).url(), "http://localhost:4096");
        assert_eq!(local_url(49374), "http://localhost:49374");
    }

    #[test]
    fn state_home_prefers_the_servers_own_xdg_state_home() {
        let env = env_lines(&[("XDG_STATE_HOME", "/srv/state"), ("PATH", "/usr/bin")]);
        assert_eq!(state_home_from_env(&env), std::path::PathBuf::from("/srv/state"));

        let default = dirs::home_dir().unwrap().join(".local").join("state");
        assert_eq!(state_home_from_env(&[]), default);
        // An empty value falls back like an unset one (xdg-basedir's rule).
        let empty = env_lines(&[("XDG_STATE_HOME", "")]);
        assert_eq!(state_home_from_env(&empty), default);
    }

    #[test]
    fn port_server_credentials_come_from_argv_and_env() {
        let args: Vec<String> = [
            "/usr/bin/opencode",
            "serve",
            "--port",
            "4096",
            "--hostname",
            "127.0.0.1",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        let env = env_lines(&[
            ("OPENCODE_SERVER_USERNAME", "admin"),
            ("OPENCODE_SERVER_PASSWORD", "secret"),
        ]);
        assert_eq!(
            resolve_credentials(&args, &env, 7),
            Some((4096, "admin".to_string(), "secret".to_string()))
        );
    }

    /// OpenCode's `Env.password` prefers `OPENCODE_PASSWORD` and falls back to
    /// the legacy name; discovery must read the same order or a server started
    /// with only the preferred name probes with an empty password and 401s
    /// (issue #812).
    #[test]
    fn port_server_password_prefers_the_opencode_password_name() {
        let args = argv(&["opencode", "serve", "--port", "4096"]);
        let both = env_lines(&[
            ("OPENCODE_PASSWORD", "preferred"),
            ("OPENCODE_SERVER_PASSWORD", "legacy"),
        ]);
        assert_eq!(resolve_credentials(&args, &both, 7).unwrap().2, "preferred");
        // Either name alone still works.
        assert_eq!(
            resolve_credentials(&args, &env_lines(&[("OPENCODE_PASSWORD", "preferred")]), 7)
                .unwrap()
                .2,
            "preferred"
        );
        assert_eq!(
            resolve_credentials(&args, &env_lines(&[("OPENCODE_SERVER_PASSWORD", "legacy")]), 7)
                .unwrap()
                .2,
            "legacy"
        );
        // Neither set keeps the historical empty password (unauthenticated).
        assert_eq!(resolve_credentials(&args, &[], 7).unwrap().2, "");
    }

    /// The one shared predicate must see interpreter-launched dev servers while
    /// still rejecting anything that merely mentions `serve` — notably a help
    /// invocation that lists the subcommand.
    #[test]
    fn is_opencode_serve_args_recognizes_direct_and_interpreter_launches() {
        assert!(is_opencode_serve_args(&argv(&["/usr/bin/opencode", "serve"])));
        assert!(is_opencode_serve_args(&argv(&[
            "node",
            "/opt/opencode/dist/index.js",
            "serve",
            "--port",
            "4096"
        ])));
        assert!(is_opencode_serve_args(&argv(&[
            "bun",
            "/repo/opencode/src/index.ts",
            "serve"
        ])));

        // Not `opencode`, even though the argv carries a `serve` token.
        assert!(!is_opencode_serve_args(&argv(&[
            "node",
            "/opt/other/server.js",
            "serve"
        ])));
        // No `serve` subcommand at all.
        assert!(!is_opencode_serve_args(&argv(&["opencode"])));
        // A help invocation is not a server.
        assert!(!is_opencode_serve_args(&argv(&["opencode", "--help"])));
        assert!(!is_opencode_serve_args(&argv(&["opencode", "serve", "--help"])));
        assert!(!is_opencode_serve_args(&argv(&["opencode", "serve", "-h"])));
    }

    /// The service's own identity check is the ONE serve-recognition predicate
    /// plus `--service`, so it cannot drift from a scan: a dev-launched service
    /// (`node /opt/opencode/dist/index.js serve --service`) is the service,
    /// while a classic `serve --port` and a help invocation are not (issue
    /// #812, ticket #810 §2).
    #[test]
    fn is_opencode_service_cmd_recognizes_direct_and_dev_launches() {
        assert!(is_opencode_service_cmd(&service_argv()));
        assert!(is_opencode_service_cmd(&dev_service_argv()));
        assert!(is_opencode_service_cmd(&argv(&[
            "/usr/bin/opencode",
            "serve",
            "--service"
        ])));

        // A classic `serve --port` is not the service …
        assert!(!is_opencode_service_cmd(&argv(&[
            "opencode", "serve", "--port", "4096"
        ])));
        // … and neither is a help invocation that merely lists the flags.
        assert!(!is_opencode_service_cmd(&argv(&["opencode", "--help"])));
        assert!(!is_opencode_service_cmd(&argv(&["opencode", "serve", "--help"])));
        // Not an `opencode` server at all, even with the flag.
        assert!(!is_opencode_service_cmd(&argv(&[
            "node",
            "/opt/other/server.js",
            "serve",
            "--service"
        ])));
    }

    /// The dev-launch argv also reaches `resolve_credentials`: port from argv,
    /// password from the environment — no registration file involved.
    #[test]
    fn dev_launched_serve_resolves_port_and_password() {
        let args = argv(&["node", "/opt/opencode/dist/index.js", "serve", "--port", "4096"]);
        let env = env_lines(&[("OPENCODE_PASSWORD", "preferred")]);
        assert_eq!(
            resolve_credentials(&args, &env, 7),
            Some((4096, "opencode".to_string(), "preferred".to_string()))
        );
    }

    /// `opencode --standalone` and `opencode acp` start `serve --stdio --port
    /// 0`: the OS assigns the real port and the server publishes it only on
    /// stdout. The argv port is therefore not an endpoint — `localhost:0` can
    /// never be attached to — so such a process is not a candidate at all
    /// (issue #748).
    #[test]
    fn an_os_assigned_stdio_server_is_not_a_candidate() {
        let args: Vec<String> = ["opencode", "serve", "--stdio", "--port", "0"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        assert_eq!(resolve_credentials(&args, &[], 42), None);
    }

    /// The V2 managed service has no `--port` flag: its registration file is
    /// the attach path, and the username is always `opencode` — both a
    /// poisoned `OPENCODE_SERVER_USERNAME` and a different
    /// `OPENCODE_SERVER_PASSWORD` in the process env must be ignored.
    #[test]
    fn service_credentials_come_from_the_registration_file() {
        let dir = tempfile::tempdir().unwrap();
        let state = dir.path().join("state");
        std::fs::create_dir_all(state.join("opencode")).unwrap();
        std::fs::write(
            state.join("opencode").join("service.json"),
            r#"{"id":"reg","version":"2.0.18","url":"http://127.0.0.1:49374","pid":42,"password":"reg-pw"}"#,
        )
        .unwrap();
        let env = env_lines(&[
            ("XDG_STATE_HOME", state.to_str().unwrap()),
            ("OPENCODE_SERVER_USERNAME", "poison"),
            ("OPENCODE_SERVER_PASSWORD", "env-pw"),
        ]);
        let args: Vec<String> = ["/opt/opencode-v2/opencode", "serve", "--service"]
            .iter()
            .map(|s| s.to_string())
            .collect();

        assert_eq!(
            resolve_credentials(&args, &env, 42),
            Some((49374, "opencode".to_string(), "reg-pw".to_string()))
        );
    }

    #[test]
    fn a_stale_registration_pid_is_ignored() {
        let dir = tempfile::tempdir().unwrap();
        let state = dir.path().join("state");
        std::fs::create_dir_all(state.join("opencode")).unwrap();
        std::fs::write(
            state.join("opencode").join("service.json"),
            r#"{"url":"http://127.0.0.1:49374","pid":99,"password":"reg-pw"}"#,
        )
        .unwrap();
        let env = env_lines(&[("XDG_STATE_HOME", state.to_str().unwrap())]);
        let args: Vec<String> = ["opencode", "serve", "--service"]
            .iter()
            .map(|s| s.to_string())
            .collect();

        assert_eq!(
            resolve_credentials(&args, &env, 42),
            None,
            "pid 99 != scanned pid 42"
        );
    }

    #[test]
    fn a_service_without_a_registration_is_unresolved() {
        let dir = tempfile::tempdir().unwrap();
        let env = env_lines(&[("XDG_STATE_HOME", dir.path().to_str().unwrap())]);
        let args: Vec<String> = ["opencode", "serve", "--service"]
            .iter()
            .map(|s| s.to_string())
            .collect();

        assert_eq!(resolve_credentials(&args, &env, 42), None);
    }

    #[test]
    fn a_serve_without_a_port_or_service_registration_is_unresolved() {
        let args: Vec<String> = ["opencode", "serve"].iter().map(|s| s.to_string()).collect();
        assert_eq!(resolve_credentials(&args, &[], 42), None);
    }

    #[test]
    fn username_defaults_to_opencode_when_unset() {
        // A server started by OpenChamber sets only the password; the username
        // then defaults to `opencode` server-side (auth.ts).
        let env = vec![
            "OPENCODE_SERVER_PASSWORD=secret".to_string(),
            "PATH=/usr/bin".to_string(),
        ];
        assert_eq!(username_from_env(&env), "opencode");
    }

    #[test]
    fn username_from_server_env_when_set() {
        let env = vec![
            "OPENCODE_SERVER_USERNAME=admin".to_string(),
            "OPENCODE_SERVER_PASSWORD=secret".to_string(),
        ];
        assert_eq!(username_from_env(&env), "admin");
    }

    #[test]
    fn username_ignores_empty_env_value() {
        // An empty OPENCODE_SERVER_USERNAME must fall back to the default, not
        // produce an empty credential (Basic with an empty username fails).
        let env = vec!["OPENCODE_SERVER_USERNAME=".to_string()];
        assert_eq!(username_from_env(&env), "opencode");
    }

    #[test]
    fn username_defaults_when_env_unreadable() {
        // macOS/Windows may hide another process's env; an empty env degrades
        // to the default username, matching the server default.
        assert_eq!(username_from_env(&[]), "opencode");
    }

    #[test]
    fn default_data_home_prefers_xdg_when_set() {
        let saved = std::env::var_os("XDG_DATA_HOME");
        // `XDG_DATA_HOME` unset → `$HOME/.local/share`.
        let home = dirs::home_dir().unwrap();
        let expected = home.join(".local").join("share");
        unsafe { std::env::remove_var("XDG_DATA_HOME") };
        assert_eq!(default_data_home(), expected);
        // Explicitly set → used verbatim (mirrors opencode's xdg-basedir).
        unsafe { std::env::set_var("XDG_DATA_HOME", "/custom/data/home") };
        assert_eq!(default_data_home(), std::path::PathBuf::from("/custom/data/home"));
        // Restore so parallel tests aren't affected.
        match saved {
            Some(v) => unsafe { std::env::set_var("XDG_DATA_HOME", v) },
            None => unsafe { std::env::remove_var("XDG_DATA_HOME") },
        }
    }

    /// The retry loop around the spawned-server attach decision: the startup
    /// window's unclassified answers (V2's 503-while-it-settles) are retried
    /// until the info envelope resolves the server. Regression: #380.
    #[tokio::test]
    async fn a_spawned_server_retries_the_startup_window_and_resolves_v2() {
        let server = crate::test_http::TestHttpServer::start().await;
        let attempts = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        server.route_dynamic("GET", "/api/info", {
            let attempts = std::sync::Arc::clone(&attempts);
            move |_| {
                if attempts.fetch_add(1, std::sync::atomic::Ordering::SeqCst) == 0 {
                    crate::test_http::DynamicResponse::new(503, "text/plain", "migrating")
                } else {
                    crate::test_http::DynamicResponse::new(
                        200,
                        "application/json",
                        r#"{"version":"2.0.18","pid":7494,"urls":["http://127.0.0.1:49374"]}"#,
                    )
                }
            }
        });
        // A live stand-in for the spawned child: the test process itself.
        let pid = std::process::id() as i32;
        let candidate = cand(pid, spawn_test_port(&server), "cola-secret", true);
        let transport = spawn_test_transport(&server, "cola-secret");
        let dir = tempfile::tempdir().unwrap();
        let layout = test_layout(dir.path());

        let resolved = resolve_spawned_server_over(
            &layout,
            &transport,
            &candidate,
            GenerationOverride::Auto,
            std::time::Duration::from_secs(5),
        )
        .await
        .expect("the second, classified answer resolves");

        assert_eq!(
            resolved.generation,
            Generation::V2,
            "the spawned server's lineage is probed, not assumed"
        );
        assert_eq!(resolved.username, DEFAULT_SERVER_USERNAME);
        assert_eq!(resolved.pid, Some(pid));
        assert!(
            attempts.load(std::sync::atomic::Ordering::SeqCst) >= 2,
            "the 503 must have been retried"
        );
    }

    /// A spawn whose server never classifies fails with the probe evidence, and
    /// the evidence never carries the credential.
    #[tokio::test]
    async fn a_spawned_server_that_never_classifies_fails_with_evidence() {
        let server = crate::test_http::TestHttpServer::start().await;
        server.route_raw("GET", "/api/info", 503, "text/plain", "migrating");
        let candidate = cand(
            std::process::id() as i32,
            spawn_test_port(&server),
            "spawn-secret",
            true,
        );
        let transport = spawn_test_transport(&server, "spawn-secret");
        let dir = tempfile::tempdir().unwrap();
        let layout = test_layout(dir.path());

        let error = resolve_spawned_server_over(
            &layout,
            &transport,
            &candidate,
            GenerationOverride::Auto,
            std::time::Duration::from_millis(1),
        )
        .await
        .expect_err("an inconclusive spawn must not attach");

        // The pid is still alive, so this is the LIVE-rendezvous case, never
        // the "gone" one a fallback may follow (spec #809 acceptance 6).
        assert!(
            matches!(error, SpawnResolveFailure::Inconclusive(_)),
            "a live process is not gone: {error:?}"
        );
        assert!(error.to_string().contains("503"), "evidence: {error}");
        assert!(
            !error.to_string().contains("spawn-secret"),
            "the credential leaked: {error}"
        );
    }

    /// The V1 leg of the spawn path: V1's 200-`text/html` catch-all resolves V1
    /// through the same loop (the spawn must not assume V2 either).
    #[tokio::test]
    async fn a_spawned_v1_server_resolves_v1_through_the_spawn_path() {
        let server = crate::test_http::TestHttpServer::start().await;
        server.route_raw(
            "GET",
            "/api/info",
            200,
            "text/html",
            "<!doctype html><title>OpenCode</title>",
        );
        let candidate = cand(
            std::process::id() as i32,
            spawn_test_port(&server),
            "cola-secret",
            true,
        );
        let transport = spawn_test_transport(&server, "cola-secret");
        let dir = tempfile::tempdir().unwrap();
        let layout = test_layout(dir.path());

        let resolved = resolve_spawned_server_over(
            &layout,
            &transport,
            &candidate,
            GenerationOverride::Auto,
            std::time::Duration::from_secs(5),
        )
        .await
        .expect("the V1 catch-all resolves");

        assert_eq!(resolved.generation, Generation::V1);
        assert_eq!(resolved.username, DEFAULT_SERVER_USERNAME);
    }

    /// A child that exits during the startup window fails the spawn
    /// immediately, without waiting out the budget.
    #[tokio::test]
    async fn a_spawned_server_that_exited_fails_immediately() {
        let server = crate::test_http::TestHttpServer::start().await;
        server.route_raw("GET", "/api/info", 503, "text/plain", "migrating");
        // A far-out pid is dead everywhere (see `process_alive_detects_own_process`).
        let candidate = cand(i32::MAX - 1, spawn_test_port(&server), "cola-secret", true);
        let transport = spawn_test_transport(&server, "cola-secret");
        let dir = tempfile::tempdir().unwrap();
        let layout = test_layout(dir.path());

        let started = std::time::Instant::now();
        let error = resolve_spawned_server_over(
            &layout,
            &transport,
            &candidate,
            GenerationOverride::Auto,
            std::time::Duration::from_secs(30),
        )
        .await
        .expect_err("a dead child cannot resolve");

        // The failure is typed as "gone", which is what lets the Shared Service
        // start fall back instead of leaving cola serverless (spec #809
        // acceptance 6).
        assert!(
            matches!(error, SpawnResolveFailure::Exited(_)),
            "a dead child is gone, not a live rendezvous: {error:?}"
        );
        assert!(error.to_string().contains("exited"), "evidence: {error}");
        assert!(
            started.elapsed() < std::time::Duration::from_secs(5),
            "a dead child must not wait out the budget"
        );
    }

    /// The fake server's listening port, as the spawn would pick it.
    fn spawn_test_port(server: &crate::test_http::TestHttpServer) -> u16 {
        port_from_url(&server.base_url()).expect("the fake server URL has a port")
    }

    /// The probe transport the real starter is given in tests: loopback, with
    /// the env proxy disabled, so a developer shell's `http_proxy` cannot
    /// intercept the fake server (the wire-test pattern, ADR-0031).
    fn no_proxy_probe(candidate: &ServerCandidate) -> Transport {
        Transport::new_loopback(
            Some(candidate.username.as_str()),
            Some(candidate.password.as_str()),
            candidate.url(),
        )
    }

    /// A transport pointing at the fake server with the env proxy disabled, so
    /// a developer shell's `http_proxy` cannot intercept loopback.
    fn spawn_test_transport(server: &crate::test_http::TestHttpServer, password: &str) -> Transport {
        let transport = Transport::new(Some(DEFAULT_SERVER_USERNAME), Some(password), server.base_url());
        transport.disable_env_proxy(Some(DEFAULT_SERVER_USERNAME), Some(password));
        transport
    }

    // --- Shared Service start (ADR-0085, ticket #810) ---------------------

    /// A scripted [`ServiceHost`]: nothing is spawned, the registration is a
    /// sequence of strings the test controls, liveness/cmdline are lookup
    /// tables, and terminate/kill are recorded. The sequence models a daemon
    /// that registers a beat after the spawn returns (and a stale file it
    /// replaces).
    struct FakeServiceHost {
        spawn: Result<i32, String>,
        /// What each successive `registration()` call sees; the last entry
        /// repeats. Empty means "no file".
        registrations: Vec<Option<String>>,
        reads: std::sync::atomic::AtomicUsize,
        alive: std::sync::Mutex<Vec<i32>>,
        /// Whether a SIGTERM actually ends the attempt (the graceful case).
        dies_on_terminate: bool,
        /// Whether the attempt survives SIGKILL too (a wedged daemon): `alive`
        /// keeps the pid and the process handle never reports exit, so the reap
        /// cannot confirm it gone.
        survives_kill: bool,
        cmd: Vec<(i32, Vec<String>)>,
        /// Each pid's environment lines; a pid absent here is "unreadable",
        /// which degrades to the default store.
        environ: Vec<(i32, Vec<String>)>,
        calls: std::sync::Arc<std::sync::Mutex<Vec<String>>>,
        /// Whether the spawned attempt's process has exited and can be waited
        /// out: SIGTERM/SIGKILL flip it when the test scripts the death, and
        /// `attempt_exited()` scripts a child that exited on its own (the
        /// contender that found an incumbent).
        attempt_exited: std::sync::Arc<std::sync::atomic::AtomicBool>,
    }

    impl FakeServiceHost {
        fn new() -> Self {
            Self {
                spawn: Ok(4242),
                registrations: Vec::new(),
                reads: std::sync::atomic::AtomicUsize::new(0),
                alive: std::sync::Mutex::new(Vec::new()),
                dies_on_terminate: false,
                survives_kill: false,
                cmd: Vec::new(),
                environ: Vec::new(),
                calls: std::sync::Arc::new(std::sync::Mutex::new(Vec::new())),
                attempt_exited: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
            }
        }

        /// The attempt's process exited on its own (nothing was signalled).
        fn attempt_exited(self) -> Self {
            self.attempt_exited
                .store(true, std::sync::atomic::Ordering::SeqCst);
            self
        }

        fn spawn_error(mut self, message: &str) -> Self {
            self.spawn = Err(message.to_string());
            self
        }

        fn registration(mut self, raw: &str) -> Self {
            self.registrations.push(Some(raw.to_string()));
            self
        }

        fn registrations(mut self, raws: &[Option<&str>]) -> Self {
            self.registrations = raws.iter().map(|r| r.map(|raw| raw.to_string())).collect();
            self
        }

        /// A pid the host reports alive with the given command line.
        fn live(mut self, pid: i32, args: &[&str]) -> Self {
            self.alive.lock().unwrap().push(pid);
            self.cmd.push((pid, args.iter().map(|s| s.to_string()).collect()));
            self
        }

        /// The environment lines the host reports for a pid (a daemon's store
        /// comes from its `XDG_DATA_HOME`).
        fn with_env(mut self, pid: i32, env: &[&str]) -> Self {
            self.environ
                .push((pid, env.iter().map(|s| s.to_string()).collect()));
            self
        }

        /// The attempt exits on SIGTERM (the graceful daemon).
        fn dies_on_terminate(mut self) -> Self {
            self.dies_on_terminate = true;
            self
        }

        /// The attempt ignores SIGKILL as well: the process stays alive and the
        /// handle never reports exit, so the reap cannot confirm it gone.
        fn survives_kill(mut self) -> Self {
            self.survives_kill = true;
            self
        }

        fn calls(&self) -> Vec<String> {
            self.calls.lock().unwrap().clone()
        }
    }

    /// The scripted [`ServiceAttempt`]: the process exits when the host's
    /// terminate/kill flips the shared flag (or the test scripts it), and the
    /// reap is recorded in the host's call log — so "the attempt was waited
    /// out" is asserted without a process.
    struct FakeAttempt {
        pid: i32,
        exited: std::sync::Arc<std::sync::atomic::AtomicBool>,
        calls: std::sync::Arc<std::sync::Mutex<Vec<String>>>,
    }

    impl ServiceAttempt for FakeAttempt {
        fn pid(&self) -> i32 {
            self.pid
        }

        fn try_reap(&mut self) -> bool {
            if self.exited.load(std::sync::atomic::Ordering::SeqCst) {
                self.calls.lock().unwrap().push(format!("reap {}", self.pid));
                return true;
            }
            false
        }
    }

    impl ServiceHost for FakeServiceHost {
        fn spawn_service(&self, _log_path: &std::path::Path) -> anyhow::Result<Box<dyn ServiceAttempt>> {
            self.calls.lock().unwrap().push("spawn".to_string());
            match &self.spawn {
                Ok(pid) => Ok(Box::new(FakeAttempt {
                    pid: *pid,
                    exited: std::sync::Arc::clone(&self.attempt_exited),
                    calls: std::sync::Arc::clone(&self.calls),
                })),
                Err(message) => anyhow::bail!("{message}"),
            }
        }

        fn registration(&self, _path: &std::path::Path) -> Option<String> {
            let read = self.reads.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            if self.registrations.is_empty() {
                return None;
            }
            let index = read.min(self.registrations.len() - 1);
            self.registrations[index].clone()
        }

        fn alive(&self, pid: i32) -> bool {
            self.alive.lock().unwrap().contains(&pid)
        }

        fn cmd(&self, pid: i32) -> Option<Vec<String>> {
            self.cmd
                .iter()
                .find(|(p, _)| *p == pid)
                .map(|(_, args)| args.clone())
        }

        fn environ(&self, pid: i32) -> Option<Vec<String>> {
            self.environ
                .iter()
                .find(|(p, _)| *p == pid)
                .map(|(_, env)| env.clone())
        }

        fn terminate(&self, pid: i32) -> anyhow::Result<()> {
            self.calls.lock().unwrap().push(format!("terminate {pid}"));
            if self.dies_on_terminate {
                self.alive.lock().unwrap().retain(|p| *p != pid);
                self.attempt_exited
                    .store(true, std::sync::atomic::Ordering::SeqCst);
            }
            Ok(())
        }

        fn force_kill(&self, pid: i32) -> anyhow::Result<()> {
            self.calls.lock().unwrap().push(format!("kill {pid}"));
            if !self.survives_kill {
                self.alive.lock().unwrap().retain(|p| *p != pid);
                self.attempt_exited
                    .store(true, std::sync::atomic::Ordering::SeqCst);
            }
            Ok(())
        }
    }

    /// A live service's registration file, as the daemon writes it.
    fn service_registration_json(pid: i32, port: u16, password: &str) -> String {
        format!(
            r#"{{"id":"reg","version":"2.0.18","url":"http://127.0.0.1:{port}","pid":{pid},"password":"{password}"}}"#
        )
    }

    fn service_argv() -> Vec<String> {
        ["opencode", "serve", "--service"]
            .iter()
            .map(|s| s.to_string())
            .collect()
    }

    /// A dev launch of the Shared Service: the entrypoint is not `argv[0]`, so
    /// recognition cannot key on the binary name alone (issue #812).
    fn dev_service_argv() -> Vec<String> {
        ["node", "/opt/opencode/dist/index.js", "serve", "--service"]
            .iter()
            .map(|s| s.to_string())
            .collect()
    }

    #[test]
    fn service_start_command_spawns_the_rendezvous_without_a_port_or_password() {
        let cmd = service_start_command();
        // No `--port`: the daemon honors `opencode service set port`. No
        // password: it keeps its own persisted one and echoes it in the
        // registration file cola reads (ADR-0085).
        assert_eq!(cmd.args, vec!["serve", "--service"]);
        assert!(
            cmd.env.is_empty(),
            "the service must not be handed a password: {:?}",
            cmd.env
        );
        // The daemon must land on the DEFAULT store and use its own auth …
        for key in [
            "XDG_DATA_HOME",
            "OPENCODE_SERVER_USERNAME",
            "OPENCODE_PASSWORD",
            "OPENCODE_SERVER_PASSWORD",
        ] {
            assert!(
                cmd.remove_env.contains(&key.to_string()),
                "{key} must be stripped"
            );
        }
        // … while KEEPING the state home: the registration file has to land
        // where every default client looks for it.
        assert!(!cmd.remove_env.contains(&"XDG_STATE_HOME".to_string()));
    }

    #[test]
    fn registration_service_candidate_adopts_a_matching_live_service() {
        let raw = service_registration_json(4242, 49374, "reg-pw");
        let candidate = registration_service_candidate(
            &raw,
            &|pid| pid == 4242,
            &|pid| (pid == 4242).then(service_argv),
            &|_| None,
        )
        .expect("a live registration is attachable");

        assert_eq!(candidate.pid, 4242);
        assert_eq!(candidate.port, 49374);
        assert_eq!(candidate.password, "reg-pw");
        // The username is never taken from anywhere: V2 accepts only `opencode`.
        assert_eq!(candidate.username, DEFAULT_SERVER_USERNAME);
        assert!(candidate.uses_default_store);
        assert_eq!(candidate.class, ServerClass::Service);
        assert!(candidate.is_service());

        // The dev launch is adopted too: the identity check is the shared
        // serve predicate plus `--service`, never `argv[0]` (issue #812).
        let dev = registration_service_candidate(&raw, &|_| true, &|_| Some(dev_service_argv()), &|_| None)
            .expect("a dev-launched service is the service");
        assert_eq!(dev.pid, 4242);
        assert_eq!(dev.class, ServerClass::Service);
    }

    #[test]
    fn registration_service_candidate_rejects_a_stale_registration() {
        let raw = service_registration_json(4242, 49374, "reg-pw");
        // The recorded pid is dead (a leftover registration): never attached to
        // through another instance's credentials.
        assert_eq!(
            registration_service_candidate(&raw, &|_| false, &|_| Some(service_argv()), &|_| None),
            None
        );
        // Alive, but the pid now runs something else (recycled): not the daemon.
        assert_eq!(
            registration_service_candidate(
                &raw,
                &|_| true,
                &|_| Some(vec!["/usr/bin/cola".to_string()]),
                &|_| None
            ),
            None
        );
        // Alive and an `opencode serve`, but NOT the service mode.
        assert_eq!(
            registration_service_candidate(
                &raw,
                &|_| true,
                &|_| Some(
                    ["opencode", "serve", "--port", "4096"]
                        .iter()
                        .map(|s| s.to_string())
                        .collect()
                ),
                &|_| None
            ),
            None
        );
        // No pid to verify liveness against, and a url without a port.
        assert_eq!(
            registration_service_candidate(
                r#"{"url":"http://127.0.0.1:49374","password":"pw"}"#,
                &|_| true,
                &|_| Some(service_argv()),
                &|_| None
            ),
            None
        );
        assert_eq!(
            registration_service_candidate(
                r#"{"url":"http://127.0.0.1","password":"pw","pid":4242}"#,
                &|_| true,
                &|_| Some(service_argv()),
                &|_| None
            ),
            None
        );
    }

    /// The store is part of the service's identity, not a cola assumption: a
    /// registration is only the canonical service when its daemon reads the
    /// DEFAULT store — the same rule [`scan_processes`] applies to every
    /// discovered server. An incumbent on a custom data home that shares cola's
    /// state home must NOT be adopted, or cola would attach to the wrong store
    /// (spec #809: eligible = `uses_default_store && port != 0`).
    #[test]
    fn registration_service_candidate_requires_the_default_store() {
        let raw = service_registration_json(4242, 49374, "reg-pw");
        let live = &|pid: i32| pid == 4242;
        let cmd = &|pid: i32| (pid == 4242).then(service_argv);

        // A custom data home is another store: the incumbent is not adopted.
        assert_eq!(
            registration_service_candidate(&raw, live, cmd, &|_| Some(vec![
                "XDG_DATA_HOME=/custom".to_string()
            ]),),
            None
        );

        // Unset (no variable, or an env cola cannot read) and an explicit
        // default are both the default store: adopted.
        let default = default_data_home().to_string_lossy().into_owned();
        for env in [
            None,
            Some(vec![format!("XDG_DATA_HOME={default}")]),
            Some(vec!["PATH=/usr/bin".to_string()]),
        ] {
            let candidate = registration_service_candidate(&raw, live, cmd, &|_| env.clone())
                .expect("the default store is adoptable");
            assert!(candidate.uses_default_store);
            assert_eq!(candidate.pid, 4242);
        }
    }

    #[test]
    fn service_start_plan_skips_the_service_for_v1_and_for_a_disabled_service() {
        assert_eq!(
            service_start_plan(GenerationOverride::Auto, false),
            ServiceStartPlan::TryService
        );
        assert_eq!(
            service_start_plan(GenerationOverride::V2, false),
            ServiceStartPlan::TryService
        );
        // V1 has no `serve --service` (spec #809) — never attempted, whatever
        // the config says.
        assert_eq!(
            service_start_plan(GenerationOverride::V1, false),
            ServiceStartPlan::PortOnly(ServiceStartSkip::V1Generation)
        );
        assert_eq!(
            service_start_plan(GenerationOverride::V1, true),
            ServiceStartPlan::PortOnly(ServiceStartSkip::V1Generation)
        );
        // OpenCode's own `disabled: true` goes straight to `serve --port`.
        assert_eq!(
            service_start_plan(GenerationOverride::Auto, true),
            ServiceStartPlan::PortOnly(ServiceStartSkip::Disabled)
        );
    }

    /// The registration must land under the STATE home the daemon inherits
    /// (that is where every default client looks for it), and the daemon's
    /// output goes to cola's own state dir — never a pipe.
    #[test]
    fn the_service_registers_under_colas_state_home_and_logs_to_colas_dir() {
        let state = own_state_home();
        if let Some(xdg) = std::env::var_os("XDG_STATE_HOME").filter(|v| !v.is_empty()) {
            assert_eq!(state, std::path::PathBuf::from(xdg));
        }
        if let Some(xdg) = std::env::var_os("XDG_CONFIG_HOME").filter(|v| !v.is_empty()) {
            assert_eq!(own_config_home(), std::path::PathBuf::from(xdg));
        }
        // The registration file is the discovery contract's spelling, under
        // the state home the daemon inherits from cola.
        assert_eq!(
            own_service_registration_path(),
            state.join("opencode").join("service.json")
        );
        assert!(
            own_service_config_path().ends_with("opencode/service.json"),
            "{:?}",
            own_service_config_path()
        );
        assert!(
            StateLayout::default()
                .service_log()
                .ends_with(".cola/opencode-service.log"),
            "{:?}",
            StateLayout::default().service_log()
        );
    }

    #[test]
    fn service_disabled_reads_opencodes_own_flag() {
        assert!(parse_service_disabled(r#"{"disabled":true}"#));
        assert!(!parse_service_disabled(r#"{"disabled":false}"#));
        // A config without the key, an empty object and a malformed file all
        // mean "not disabled": a config cola cannot parse must never stop the
        // start path.
        assert!(!parse_service_disabled(r#"{"port":49374}"#));
        assert!(!parse_service_disabled("{}"));
        assert!(!parse_service_disabled("not json"));

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("opencode").join("service.json");
        // Absent file: not disabled.
        assert!(!service_disabled_at(&path));
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, r#"{"disabled":true}"#).unwrap();
        assert!(service_disabled_at(&path));
    }

    /// A scripted [`ServerStarter`]: records which leg ran and returns canned
    /// results, so the service-first ordering and the fallback are asserted
    /// without a process.
    struct FakeStarter {
        service: Result<ResolvedServer, ServiceStartFailure>,
        port: Result<ResolvedServer, String>,
        calls: std::sync::Mutex<Vec<String>>,
    }

    impl FakeStarter {
        fn new() -> Self {
            Self {
                service: Ok(resolved_server(49374, Generation::V2)),
                port: Ok(resolved_server(4096, Generation::V2)),
                calls: std::sync::Mutex::new(Vec::new()),
            }
        }

        fn service_fails(mut self, failure: ServiceStartFailure) -> Self {
            self.service = Err(failure);
            self
        }

        fn calls(&self) -> Vec<String> {
            self.calls.lock().unwrap().clone()
        }
    }

    #[async_trait]
    impl ServerStarter for FakeStarter {
        async fn start_service(
            &self,
            _generation_override: GenerationOverride,
        ) -> Result<ResolvedServer, ServiceStartFailure> {
            self.calls.lock().unwrap().push("service".to_string());
            self.service.clone()
        }

        async fn start_port(
            &self,
            _preferred_port: Option<u16>,
            _generation_override: GenerationOverride,
        ) -> anyhow::Result<ResolvedServer> {
            self.calls.lock().unwrap().push("port".to_string());
            match &self.port {
                Ok(resolved) => Ok(resolved.clone()),
                Err(message) => anyhow::bail!("{message}"),
            }
        }
    }

    fn resolved_server(port: u16, generation: Generation) -> ResolvedServer {
        ResolvedServer {
            url: local_url(port),
            username: DEFAULT_SERVER_USERNAME.to_string(),
            password: "pw".to_string(),
            pid: Some(7),
            generation,
        }
    }

    #[tokio::test]
    async fn spawn_own_server_prefers_the_shared_service() {
        let starter = FakeStarter::new();
        let resolved = spawn_own_server_over(None, GenerationOverride::Auto, false, &starter)
            .await
            .expect("the service leg resolves");

        assert_eq!(starter.calls(), vec!["service"], "the service is the first step");
        assert_eq!(resolved.url, local_url(49374));
    }

    #[tokio::test]
    async fn spawn_own_server_falls_back_to_the_port_path_when_no_service_registers() {
        let starter =
            FakeStarter::new().service_fails(ServiceStartFailure::NoRegistration { child_pid: 4242 });
        let resolved = spawn_own_server_over(None, GenerationOverride::Auto, false, &starter)
            .await
            .expect("the port leg resolves");

        assert_eq!(starter.calls(), vec!["service", "port"]);
        assert_eq!(resolved.url, local_url(4096));
    }

    #[tokio::test]
    async fn spawn_own_server_falls_back_when_the_service_cannot_be_spawned() {
        let starter = FakeStarter::new().service_fails(ServiceStartFailure::Spawn("no opencode".into()));
        let resolved = spawn_own_server_over(Some(4096), GenerationOverride::Auto, false, &starter)
            .await
            .expect("the port leg resolves");

        assert_eq!(starter.calls(), vec!["service", "port"]);
        assert_eq!(resolved.url, local_url(4096));
    }

    /// A service that registered and then exited leaves no server behind, so
    /// the Owned `serve --port` fallback follows — the counterpart of the
    /// no-registration case, and acceptance 6 of the spec (a start failure
    /// falls back with no leftover service child).
    #[tokio::test]
    async fn spawn_own_server_falls_back_when_the_service_exited_after_registering() {
        let starter =
            FakeStarter::new().service_fails(ServiceStartFailure::Exited("connection refused".into()));
        let resolved = spawn_own_server_over(None, GenerationOverride::Auto, false, &starter)
            .await
            .expect("the port leg resolves");

        assert_eq!(starter.calls(), vec!["service", "port"]);
        assert_eq!(resolved.url, local_url(4096));
    }

    /// A registration that appeared and whose process is STILL ALIVE but never
    /// classified is a LIVE rendezvous: raising an Owned Server beside it would
    /// put two servers on one store (ADR-0013, ADR-0085), so this is the one
    /// failure that does NOT fall back.
    #[tokio::test]
    async fn spawn_own_server_does_not_fall_back_when_a_live_service_never_classifies() {
        let starter = FakeStarter::new().service_fails(ServiceStartFailure::Attach("503".into()));
        let error = spawn_own_server_over(None, GenerationOverride::Auto, false, &starter)
            .await
            .expect_err("a live but unclassified service must not be joined by a second server");

        assert_eq!(starter.calls(), vec!["service"]);
        assert!(error.to_string().contains("never resolved"), "{error}");
    }

    /// An attempt that survived SIGKILL may still be running on the store, so
    /// `NotReaped` is not fallback-eligible either: `serve --port` would put a
    /// second server on the same store (ADR-0013, ADR-0085). The `serve --port`
    /// fallback only runs when the attempt is confirmed gone.
    #[tokio::test]
    async fn spawn_own_server_does_not_fall_back_when_the_attempt_survived_kill() {
        let starter = FakeStarter::new().service_fails(ServiceStartFailure::NotReaped { child_pid: 4242 });
        let error = spawn_own_server_over(None, GenerationOverride::Auto, false, &starter)
            .await
            .expect_err("a possibly-live service must not be joined by a second server");

        assert_eq!(starter.calls(), vec!["service"]);
        assert!(
            error.to_string().contains("could not be confirmed exited"),
            "{error}"
        );
    }

    #[tokio::test]
    async fn spawn_own_server_skips_the_service_for_v1_and_when_disabled() {
        // `generation = "v1"`: V1 has no `serve --service`.
        let v1 = FakeStarter::new();
        let resolved = spawn_own_server_over(None, GenerationOverride::V1, false, &v1)
            .await
            .expect("the port leg resolves");
        assert_eq!(v1.calls(), vec!["port"]);
        assert_eq!(resolved.url, local_url(4096));

        // OpenCode's own `disabled: true`.
        let disabled = FakeStarter::new();
        spawn_own_server_over(None, GenerationOverride::Auto, true, &disabled)
            .await
            .expect("the port leg resolves");
        assert_eq!(disabled.calls(), vec!["port"]);
    }

    #[tokio::test(start_paused = true)]
    async fn the_shared_service_waits_for_a_live_registration() {
        // First read: the daemon has not published yet. Second: its file lands.
        let host = FakeServiceHost::new()
            .registrations(&[None, Some(&service_registration_json(4242, 49374, "reg-pw"))])
            .live(4242, &["opencode", "serve", "--service"]);
        let candidate = start_shared_service(
            &host,
            std::path::Path::new("/state/opencode/service.json"),
            std::path::Path::new("/log/opencode-service.log"),
        )
        .await
        .expect("the second read sees the live service");

        assert_eq!(candidate.pid, 4242);
        assert_eq!(candidate.port, 49374);
        assert_eq!(candidate.class, ServerClass::Service);
        assert_eq!(host.calls(), vec!["spawn"], "a live registration is never reaped");
    }

    /// A registration whose daemon reads a CUSTOM store is not the canonical
    /// service: the wait must not adopt it — it keeps waiting, then falls back
    /// on the default store — so an incumbent that shares cola's state home but
    /// not its data home can never pull cola onto the wrong store (spec #809:
    /// eligible = `uses_default_store && port != 0`).
    #[tokio::test(start_paused = true)]
    async fn a_custom_store_registration_is_not_adopted() {
        let host = FakeServiceHost::new()
            .registration(&service_registration_json(999, 49374, "incumbent-pw"))
            .live(999, &["opencode", "serve", "--service"])
            .with_env(999, &["XDG_DATA_HOME=/custom"])
            .live(4242, &["opencode", "serve", "--service"])
            .dies_on_terminate();
        let failure = start_shared_service(
            &host,
            std::path::Path::new("/state/opencode/service.json"),
            std::path::Path::new("/log/opencode-service.log"),
        )
        .await
        .expect_err("a custom-store incumbent is not the canonical service");

        assert_eq!(failure, ServiceStartFailure::NoRegistration { child_pid: 4242 });
        // The incumbent (999) was never adopted; cola's own attempt was reaped.
        assert_eq!(host.calls(), vec!["spawn", "terminate 4242", "reap 4242"]);
    }

    /// A child that exits because an INCUMBENT service already serves the
    /// machine is not a failure: the incumbent's registration is live and is
    /// adopted (ticket #810 §2).
    #[tokio::test(start_paused = true)]
    async fn an_incumbent_registration_is_adopted_when_the_attempt_exits() {
        let host = FakeServiceHost::new()
            .registration(&service_registration_json(999, 49374, "incumbent-pw"))
            // Our own child (4242) exited; the incumbent (999) is live.
            .attempt_exited()
            .live(999, &["opencode", "serve", "--service"]);
        let candidate = start_shared_service(
            &host,
            std::path::Path::new("/state/opencode/service.json"),
            std::path::Path::new("/log/opencode-service.log"),
        )
        .await
        .expect("the incumbent is accepted");

        assert_eq!(candidate.pid, 999);
        assert_eq!(candidate.password, "incumbent-pw");
        // The adopted attempt is waited out, not dropped: a child left
        // unreaped would linger as a zombie for as long as cola runs.
        assert_eq!(host.calls(), vec!["spawn", "reap 4242"]);
    }

    /// The incumbent can be visible on the FIRST poll, before the contender
    /// has noticed it and exited. Accepting it then would leave the contender
    /// running beside the incumbent — two services writing one store
    /// (ADR-0013, ADR-0085). The contender is stopped first, and only then is
    /// the incumbent adopted (#814).
    #[tokio::test(start_paused = true)]
    async fn an_incumbent_is_adopted_only_after_a_live_contender_is_stopped() {
        let host = FakeServiceHost::new()
            .registration(&service_registration_json(999, 49374, "incumbent-pw"))
            // The contender is still alive and starting when the incumbent's
            // registration is first read.
            .live(4242, &["opencode", "serve", "--service"])
            .dies_on_terminate()
            .live(999, &["opencode", "serve", "--service"]);
        let candidate = start_shared_service(
            &host,
            std::path::Path::new("/state/opencode/service.json"),
            std::path::Path::new("/log/opencode-service.log"),
        )
        .await
        .expect("the incumbent is adopted once the contender is gone");

        assert_eq!(candidate.pid, 999);
        assert_eq!(
            host.calls(),
            vec!["spawn", "terminate 4242", "reap 4242"],
            "the live contender is stopped and waited out before adoption"
        );
    }

    /// A contender cola cannot stop must not be silently left beside the
    /// incumbent: adopting anyway would hide the two-server hazard, and
    /// `serve --port` would add a third server. It reports `NotReaped`, which
    /// is deliberately not fallback-eligible (#814).
    #[tokio::test(start_paused = true)]
    async fn an_unstoppable_contender_is_not_hidden_behind_the_incumbent() {
        let host = FakeServiceHost::new()
            .registration(&service_registration_json(999, 49374, "incumbent-pw"))
            .live(4242, &["opencode", "serve", "--service"])
            .survives_kill()
            .live(999, &["opencode", "serve", "--service"]);
        let failure = start_shared_service(
            &host,
            std::path::Path::new("/state/opencode/service.json"),
            std::path::Path::new("/log/opencode-service.log"),
        )
        .await
        .expect_err("a contender cola cannot stop is not hidden");

        assert_eq!(failure, ServiceStartFailure::NotReaped { child_pid: 4242 });
        assert_eq!(host.calls(), vec!["spawn", "terminate 4242", "kill 4242"]);
    }

    /// No registration in time: the attempt is terminated (SIGTERM) and the
    /// caller falls back to `serve --port`.
    #[tokio::test(start_paused = true)]
    async fn a_service_that_never_registers_is_reaped_and_reported() {
        let host = FakeServiceHost::new()
            .live(4242, &["opencode", "serve", "--service"])
            .dies_on_terminate();
        let failure = start_shared_service(
            &host,
            std::path::Path::new("/state/opencode/service.json"),
            std::path::Path::new("/log/opencode-service.log"),
        )
        .await
        .expect_err("nothing registered");

        assert_eq!(failure, ServiceStartFailure::NoRegistration { child_pid: 4242 });
        // Terminated AND waited out: a failed start leaves no service child.
        assert_eq!(host.calls(), vec!["spawn", "terminate 4242", "reap 4242"]);
        assert!(failure.to_string().contains("serve --service"), "{failure}");
    }

    /// The reap recognizes a dev-launched service too. Rejecting it would leave
    /// the attempt neither adopted nor reaped — the service child still running
    /// while cola falls back to `serve --port` (issue #812, ticket #810 §2).
    #[tokio::test(start_paused = true)]
    async fn a_dev_launched_service_attempt_is_reaped_too() {
        let host = FakeServiceHost::new()
            .live(
                4242,
                &["node", "/opt/opencode/dist/index.js", "serve", "--service"],
            )
            .dies_on_terminate();
        let failure = start_shared_service(
            &host,
            std::path::Path::new("/state/opencode/service.json"),
            std::path::Path::new("/log/opencode-service.log"),
        )
        .await
        .expect_err("nothing registered");

        assert_eq!(failure, ServiceStartFailure::NoRegistration { child_pid: 4242 });
        assert_eq!(host.calls(), vec!["spawn", "terminate 4242", "reap 4242"]);
    }

    /// A child that ignores SIGTERM is force-killed.
    #[tokio::test(start_paused = true)]
    async fn a_service_that_ignores_sigterm_is_force_killed() {
        // `alive` never loses the pid, so the SIGTERM grace runs out.
        let host = FakeServiceHost::new().live(4242, &["opencode", "serve", "--service"]);
        let failure = start_shared_service(
            &host,
            std::path::Path::new("/state/opencode/service.json"),
            std::path::Path::new("/log/opencode-service.log"),
        )
        .await
        .expect_err("nothing registered");

        assert_eq!(failure, ServiceStartFailure::NoRegistration { child_pid: 4242 });
        assert_eq!(
            host.calls(),
            vec!["spawn", "terminate 4242", "kill 4242", "reap 4242"]
        );
    }

    /// An attempt that survives SIGKILL is NOT confirmed gone: reporting
    /// `NoRegistration` would send the caller to `serve --port` while a
    /// possibly-live service still holds the store — two servers on one store
    /// (ADR-0013, ADR-0085). It reports `NotReaped` and starts no fallback.
    #[tokio::test(start_paused = true)]
    async fn an_attempt_that_survives_sigkill_is_not_reported_as_reaped() {
        let host = FakeServiceHost::new()
            .live(4242, &["opencode", "serve", "--service"])
            .survives_kill();
        let failure = start_shared_service(
            &host,
            std::path::Path::new("/state/opencode/service.json"),
            std::path::Path::new("/log/opencode-service.log"),
        )
        .await
        .expect_err("nothing registered");

        assert_eq!(failure, ServiceStartFailure::NotReaped { child_pid: 4242 });
        // SIGKILL was sent, but the process never reported exit — no reap.
        assert_eq!(host.calls(), vec!["spawn", "terminate 4242", "kill 4242"]);
        assert!(failure.to_string().contains("pid 4242"), "{failure}");
    }

    /// A child that exited without registering fails the attempt after a short
    /// grace — it must not wait out the whole budget (a V1 binary answers
    /// `serve --service` with an error at once). A dead pid is never signalled:
    /// it may have been recycled.
    #[tokio::test(start_paused = true)]
    async fn a_service_attempt_that_exited_without_registering_fails_fast() {
        let host = FakeServiceHost::new().attempt_exited();
        let started = std::time::Instant::now();
        let failure = start_shared_service(
            &host,
            std::path::Path::new("/state/opencode/service.json"),
            std::path::Path::new("/log/opencode-service.log"),
        )
        .await
        .expect_err("a dead child cannot register");

        assert_eq!(failure, ServiceStartFailure::NoRegistration { child_pid: 4242 });
        // A dead pid is never signalled, but its child is still collected.
        assert_eq!(host.calls(), vec!["spawn", "reap 4242"]);
        assert!(started.elapsed() < std::time::Duration::from_secs(2));
    }

    #[tokio::test(start_paused = true)]
    async fn a_service_that_cannot_be_spawned_reports_it() {
        let host = FakeServiceHost::new().spawn_error("opencode not found");
        let failure = start_shared_service(
            &host,
            std::path::Path::new("/state/opencode/service.json"),
            std::path::Path::new("/log/opencode-service.log"),
        )
        .await
        .expect_err("no child");

        assert_eq!(failure, ServiceStartFailure::Spawn("opencode not found".into()));
        assert_eq!(host.calls(), vec!["spawn"]);
    }

    /// A wait that is CANCELLED — the reconnect task dropped at shutdown, while
    /// the attempt is still waiting to register — must not leak the attempt:
    /// the guard reaps it on drop, exactly as the timeout path would have.
    /// Without the guard the handle is dropped and a service child that never
    /// registered stays alive (ticket #809).
    #[tokio::test(start_paused = true)]
    async fn a_cancelled_service_start_reaps_the_attempt() {
        let host = FakeServiceHost::new()
            .live(4242, &["opencode", "serve", "--service"])
            .dies_on_terminate();
        // One poll arms the wait (the spawn plus the first registration read)
        // and parks it at the sleep; the elapsed timeout then CANCELS it,
        // dropping the guard while the wait is still armed.
        let _ = tokio::time::timeout(
            std::time::Duration::ZERO,
            start_shared_service(
                &host,
                std::path::Path::new("/state/opencode/service.json"),
                std::path::Path::new("/log/opencode-service.log"),
            ),
        )
        .await;

        assert_eq!(host.calls(), vec!["spawn", "terminate 4242", "reap 4242"]);
    }

    /// The ownership boundary (ADR-0085) lives in `OpencodeStarter::start_service`,
    /// so the REAL start path is driven here against a scripted host: the
    /// service is adopted and its generation resolved (or the start fails), but
    /// cola never records it as its own — `self-opencode.pid` is the Owned
    /// Server's record, and Yield must never reap a machine-wide rendezvous.
    /// A regression that records the service pid anywhere on this path fails
    /// the test.
    #[tokio::test]
    async fn starting_the_shared_service_never_records_a_self_spawned_pid() {
        // A far-out pid is dead everywhere (see
        // `process_alive_detects_own_process`), so the generation never
        // resolves and the start reports `Exited` — the outcome does not
        // matter here, only that the real path ran without recording a pid.
        let dead_pid = i32::MAX - 1;
        let dir = tempfile::tempdir().unwrap();
        let layout = test_layout(dir.path());
        let pid_path = layout.self_spawned_pid();
        let before = std::fs::read(&pid_path).ok();
        let host = FakeServiceHost::new()
            .registration(&service_registration_json(dead_pid, 49374, "reg-pw"))
            .live(dead_pid, &["opencode", "serve", "--service"]);
        let starter = OpencodeStarter {
            host,
            registration_path: std::path::PathBuf::from("/state/opencode/service.json"),
            log_path: std::path::PathBuf::from("/log/opencode-service.log"),
            probe_transport: no_proxy_probe,
            layout: layout.clone(),
        };

        let failure = starter
            .start_service(GenerationOverride::Auto)
            .await
            .expect_err("a dead service cannot be attached");
        assert!(
            matches!(failure, ServiceStartFailure::Exited(_)),
            "the real path must reach the service's resolution: {failure:?}"
        );

        assert_eq!(
            std::fs::read(&pid_path).ok(),
            before,
            "the service must not be written into {}",
            pid_path.display()
        );
        assert!(
            !is_self_spawned_record(&pid_path, dead_pid),
            "a Shared Service is never an Owned Server"
        );
    }

    /// The same boundary on the SUCCESS path: a service that resolves is
    /// adopted and still never recorded (a regression that records only on the
    /// Ok arm fails here). The registration is pointed at the fake HTTP server
    /// so the generation probe classifies V2.
    #[tokio::test]
    async fn an_adopted_shared_service_is_never_recorded_as_an_owned_server() {
        let server = crate::test_http::TestHttpServer::start().await;
        server.route_raw(
            "GET",
            "/api/info",
            200,
            "application/json",
            r#"{"version":"2.0.18","pid":7494,"urls":["http://127.0.0.1:49374"]}"#,
        );
        // A live stand-in for the daemon: the test process itself.
        let pid = std::process::id() as i32;
        let dir = tempfile::tempdir().unwrap();
        let layout = test_layout(dir.path());
        let pid_path = layout.self_spawned_pid();
        let before = std::fs::read(&pid_path).ok();
        let host = FakeServiceHost::new()
            .registration(&service_registration_json(
                pid,
                spawn_test_port(&server),
                "reg-pw",
            ))
            .live(pid, &["opencode", "serve", "--service"]);
        let starter = OpencodeStarter {
            host,
            registration_path: std::path::PathBuf::from("/state/opencode/service.json"),
            log_path: std::path::PathBuf::from("/log/opencode-service.log"),
            probe_transport: no_proxy_probe,
            layout: layout.clone(),
        };

        let resolved = starter
            .start_service(GenerationOverride::Auto)
            .await
            .expect("the live service resolves");
        assert_eq!(resolved.pid, Some(pid));

        assert_eq!(
            std::fs::read(&pid_path).ok(),
            before,
            "the service must not be written into {}",
            pid_path.display()
        );
        assert!(
            !is_self_spawned_record(&pid_path, pid),
            "a Shared Service is never an Owned Server"
        );
    }
}
