//! Discovery of the OpenCode server cola should attach to, and the command to
//! start its own when none is running.
//!
//! The invariant is the STORE: sessions are shared only when every client
//! (cola, OpenChamber, the CLI) reads the same data directory
//! (`~/.local/share/opencode`). cola therefore attaches to any OpenCode server
//! running on the default store (whoever started it — OpenChamber, a manual
//! `opencode serve`, another tool) and only starts its own when none exists.
//!
//! The server's protocol generation is not discovery's call: it is learned by
//! probing at attach time (`crate::opencode::generation`, spec #364 §2). A V2
//! managed service (`serve --service`) carries no `--port` flag, so its port
//! and private password come from the registration file its state dir holds —
//! the official client's discovery contract.

use sysinfo::{Pid, ProcessRefreshKind, ProcessesToUpdate, System, UpdateKind};

use crate::bridge::attach;
use crate::config::{GenerationOverride, port_from_url};
use crate::opencode::strategy::Generation;
use crate::opencode::transport::Transport;

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
}

impl ServerCandidate {
    /// The loopback endpoint discovery reached this server on. The one
    /// spelling of a discovered server's URL (attach, reconnect and the
    /// config tiebreaker all go through it).
    pub(crate) fn url(&self) -> String {
        local_url(self.port)
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
/// V1 only: a V2 server ignores `OPENCODE_SERVER_USERNAME` and always accepts
/// the username `opencode` (spec #364 §2), so V2 candidates never consult this.
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
    let path = state_home_from_env(env).join("opencode").join("service.json");
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

/// Resolve one server process's attach credentials — port, username and
/// password — or `None` when discovery cannot reach it.
///
/// A classic `serve --port P` process carries its port in argv and its
/// credentials in the process environment (V1's model). A V2 managed service
/// (`serve --service`) has no `--port`; its registration file is the fallback
/// attach path (spec #364 §2), and its username is fixed at `opencode` so a
/// poisoned `OPENCODE_SERVER_USERNAME` cannot break the attach.
fn resolve_credentials(args: &[String], env: &[String], pid: i32) -> Option<(u16, String, String)> {
    if let Some(port) = args
        .iter()
        .position(|a| a == "--port")
        .and_then(|idx| args.get(idx + 1))
        .and_then(|p| p.parse::<u16>().ok())
    {
        let password = env
            .iter()
            .find_map(|kv| kv.strip_prefix("OPENCODE_SERVER_PASSWORD="))
            .unwrap_or("")
            .to_string();
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
            let is_server = args.first().map(|a| a.contains("opencode")).unwrap_or(false)
                && args.iter().skip(1).any(|a| a == "serve");
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
            let xdg = env
                .iter()
                .find_map(|kv| kv.strip_prefix("XDG_DATA_HOME="))
                .map(|s| s.to_string());
            // A server reads the default store when the directory it uses
            // equals the default data home. Unset XDG_DATA_HOME means OpenCode
            // used `~/.local/share`; reading another process's env is
            // best-effort (macOS/Windows limit it to same-user processes), so
            // an unreadable env degrades to "assume default store".
            let uses_default_store = match xdg {
                None => true,
                Some(home) => home == default_store,
            };
            Some(ServerCandidate {
                pid,
                port,
                password,
                username,
                uses_default_store,
            })
        })
        .collect()
}

/// Pick the server cola should attach to (ADR-0013).
///
/// Only default-store servers are eligible (a custom-store server is someone
/// else's data — attaching would both break sharing and couple cola to that
/// runtime). Among eligible servers a Coexistent Server (pid != `self_pid`)
/// always wins over cola's Owned Server (pid == `self_pid`), so cola steps
/// aside as soon as OpenChamber (or a manual `opencode serve`) raises one;
/// within the winning class the configured port wins, else the lowest port.
/// Deterministic — the reconnect loop never flaps between two default-store
/// servers.
pub fn pick_server(
    candidates: &[ServerCandidate],
    preferred_port: Option<u16>,
    self_pid: Option<i32>,
) -> Option<&ServerCandidate> {
    let mut eligible: Vec<&ServerCandidate> = candidates.iter().filter(|c| c.uses_default_store).collect();
    // Deterministic across scans: sysinfo enumerates processes in randomized
    // order, so taking `first()` unsorted would make the reconnect loop flap
    // between two servers of the same class (ADR-0013).
    eligible.sort_by_key(|c| c.port);
    let coexist: Vec<&ServerCandidate> = eligible
        .iter()
        .copied()
        .filter(|c| Some(c.pid) != self_pid)
        .collect();
    let class = if coexist.is_empty() { eligible } else { coexist };
    preferred_port
        .and_then(|p| class.iter().find(|c| c.port == p).copied())
        .or_else(|| class.first().copied())
}

/// The command cola runs to start its own OpenCode server on the default store.
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
        // own server.
        remove_env: vec![
            "XDG_DATA_HOME".to_string(),
            "OPENCODE_SERVER_USERNAME".to_string(),
        ],
    }
}

/// The file recording the pid of the OpenCode server cola itself spawned.
/// Written when cola starts its own server; read by `/restart-opencode` to
/// decide whether cola owns the running server and may restart it. Persisted so
/// cola still recognises its own server after a `/restart` of cola itself.
fn self_spawned_pid_path_in(state_dir: &std::path::Path) -> std::path::PathBuf {
    state_dir.join("self-opencode.pid")
}

fn self_spawned_pid_path() -> std::path::PathBuf {
    let state_dir = dirs::home_dir()
        .unwrap_or_else(|| std::path::PathBuf::from("."))
        .join(".cola");
    self_spawned_pid_path_in(&state_dir)
}

/// Record the pid of the OpenCode server cola just spawned.
pub fn record_self_spawned(pid: i32) {
    record_self_spawned_in(&self_spawned_pid_path(), pid);
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
pub fn clear_self_spawned() -> Option<i32> {
    clear_self_spawned_in(&self_spawned_pid_path())
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
pub fn self_spawned_pid() -> Option<i32> {
    read_self_spawned_pid_in(&self_spawned_pid_path())
}

/// Whether the pid of the server cola is attached to is a server cola itself
/// spawned (and therefore may restart). Only true when the pid file matches a
/// pid that is genuinely a live `opencode serve` — a stale file for a dead or
/// recycled pid is treated as NOT owned, so cola never kills something it can't
/// verify it started.
pub fn is_self_spawned(pid: i32) -> bool {
    is_self_spawned_record(&self_spawned_pid_path(), pid) && is_live_opencode_serve(pid)
}

/// Pure record-match check: does the pid file record this pid? (No liveness —
/// the public `is_self_spawned` additionally requires the pid to be a live
/// `opencode serve`.) Kept separate so the ownership logic is unit-testable.
fn is_self_spawned_record(path: &std::path::Path, pid: i32) -> bool {
    read_self_spawned_pid_in(path) == Some(pid)
}

/// Whether a pid is a live `opencode serve` process (not a dead/zombie pid that
/// the OS recycled, and not some other program that happened to reuse the pid).
/// Reads the process cmdline via sysinfo and checks the `serve` subcommand.
fn is_live_opencode_serve(pid: i32) -> bool {
    process_cmd(pid)
        .map(|args| {
            args.first().map(|a| a.contains("opencode")).unwrap_or(false)
                && args.iter().skip(1).any(|a| a == "serve")
        })
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
pub fn spawn_self_server(port: u16, password: &str) -> anyhow::Result<i32> {
    let cmd = self_start_command(port, password);
    let mut child = std::process::Command::new("opencode");
    child.args(&cmd.args).envs(cmd.env.iter().cloned());
    for key in &cmd.remove_env {
        child.env_remove(key);
    }
    // The question tool stays enabled on purpose: cola answers it via Feishu
    // cards (poll GET /question, reply POST /question/{id}/reply), so disabling
    // it would make those cards unreachable.
    let child = child.spawn()?;
    let pid = child.id() as i32;
    record_self_spawned(pid);
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

/// Start cola's own `opencode serve` on the default store, skipping ports
/// already held by a running server, wait until it accepts connections, and
/// resolve its generation by probing — the `opencode` binary may be the V1 or
/// the V2 lineage (spec #364 §2). Used at boot (`eager`) and by Lazy Start
/// (`auto`); `/restart-opencode` re-raises through [`spawn_self_server`] and
/// the reconnect loop's own re-probe. The result is a [`ResolvedServer`] like
/// any other — an Owned Server is just the endpoint cola attaches to
/// (ADR-0013).
pub async fn spawn_own_server(
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
    let pid = spawn_self_server(port, &password)?;
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
    };
    let transport = attach::candidate_transport(&candidate);
    resolve_spawned_server_over(
        &transport,
        &candidate,
        generation_override,
        SELF_START_PROBE_TIMEOUT,
    )
    .await
    .map_err(|evidence| {
        anyhow::anyhow!("spawned OpenCode server's generation could not be resolved: {evidence}")
    })
}

/// The retry loop around the attach decision for a just-spawned Owned Server:
/// the startup window answers nothing, or answers unclassifiably (V2's
/// 503-while-migrating), so inconclusive probes are retried until `timeout` or
/// the child dies. The last probe evidence travels on failure; the credential
/// never does (the attach path's promise).
async fn resolve_spawned_server_over(
    transport: &Transport,
    candidate: &ServerCandidate,
    generation_override: GenerationOverride,
    timeout: std::time::Duration,
) -> Result<ResolvedServer, String> {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        match attach::resolve_candidate_over(transport, candidate, generation_override).await {
            Ok(resolved) => return Ok(resolved),
            Err(evidence) => {
                if !process_alive(candidate.pid) {
                    if self_spawned_pid() == Some(candidate.pid) {
                        clear_self_spawned();
                    }
                    return Err(format!(
                        "the spawned server exited before its generation resolved: {evidence}"
                    ));
                }
                if tokio::time::Instant::now() >= deadline {
                    return Err(evidence);
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
    /// No OpenCode server is currently running on the default store.
    NoServer,
    /// The running server was started by someone else — cola must not touch it.
    NotOwned,
}

/// Restart the running OpenCode server, but ONLY if it is one cola spawned.
///
/// cola never restarts a server it doesn't own (e.g. one launched by another
/// tool): it has no record of how that server was configured, and killing it
/// could take down another application's runtime. `NotOwned` tells the caller
/// to reply that the server needs a manual restart.
pub async fn restart_self_spawned_server() -> anyhow::Result<RestartOutcome> {
    let candidates = scan_processes();
    let eligible: Vec<&ServerCandidate> = candidates.iter().filter(|c| c.uses_default_store).collect();
    if eligible.is_empty() {
        return Ok(RestartOutcome::NoServer);
    }
    // A coexistent server is running but cola owns nothing — never touch it.
    let Some(server) = eligible.iter().find(|c| is_self_spawned(c.pid)).copied() else {
        return Ok(RestartOutcome::NotOwned);
    };
    // Kill our own server, wait for the port to release, re-raise it on the same
    // port/password, and wait until it accepts connections again.
    let _ = clear_self_spawned();
    if let Err(e) = terminate_process(server.pid) {
        tracing::warn!("terminate self-spawned opencode {} failed: {}", server.pid, e);
        return Err(e);
    }
    tracing::info!("restarting self-spawned opencode (pid {})", server.pid);
    // Give the process time to die and the port to free up.
    for _ in 0..50 {
        if !process_alive(server.pid) {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    for _ in 0..25 {
        if tokio::net::TcpStream::connect(("127.0.0.1", server.port))
            .await
            .is_err()
        {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    }
    let pid = spawn_self_server(server.port, &server.password)?;
    wait_for_port(server.port).await?;
    tracing::info!(
        "restarted self-spawned opencode on port {} (pid {})",
        server.port,
        pid
    );
    Ok(RestartOutcome::Restarted)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cand(pid: i32, port: u16, password: &str, uses_default_store: bool) -> ServerCandidate {
        ServerCandidate {
            pid,
            port,
            password: password.into(),
            username: DEFAULT_SERVER_USERNAME.into(),
            uses_default_store,
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
    }

    #[test]
    fn self_spawned_pid_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let path = self_spawned_pid_path_in(dir.path());
        assert_eq!(read_self_spawned_pid_in(&path), None);
        record_self_spawned_in(&path, 4242);
        assert_eq!(read_self_spawned_pid_in(&path), Some(4242));
        assert_eq!(clear_self_spawned_in(&path), Some(4242));
        assert_eq!(read_self_spawned_pid_in(&path), None);
    }

    #[test]
    fn self_spawned_ownership_matches_only_recorded_pid() {
        let dir = tempfile::tempdir().unwrap();
        let path = self_spawned_pid_path_in(dir.path());
        // Nothing recorded → no server is "owned".
        assert!(!is_self_spawned_record(&path, 1));
        record_self_spawned_in(&path, 7);
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
        let path = self_spawned_pid_path_in(dir.path());
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

        let resolved = resolve_spawned_server_over(
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

        let error = resolve_spawned_server_over(
            &transport,
            &candidate,
            GenerationOverride::Auto,
            std::time::Duration::from_millis(1),
        )
        .await
        .expect_err("an inconclusive spawn must not attach");

        assert!(error.contains("503"), "evidence: {error}");
        assert!(!error.contains("spawn-secret"), "the credential leaked: {error}");
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

        let resolved = resolve_spawned_server_over(
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

        let started = std::time::Instant::now();
        let error = resolve_spawned_server_over(
            &transport,
            &candidate,
            GenerationOverride::Auto,
            std::time::Duration::from_secs(30),
        )
        .await
        .expect_err("a dead child cannot resolve");

        assert!(error.contains("exited"), "evidence: {error}");
        assert!(
            started.elapsed() < std::time::Duration::from_secs(5),
            "a dead child must not wait out the budget"
        );
    }

    /// The fake server's listening port, as the spawn would pick it.
    fn spawn_test_port(server: &crate::test_http::TestHttpServer) -> u16 {
        port_from_url(&server.base_url()).expect("the fake server URL has a port")
    }

    /// A transport pointing at the fake server with the env proxy disabled, so
    /// a developer shell's `http_proxy` cannot intercept loopback.
    fn spawn_test_transport(server: &crate::test_http::TestHttpServer, password: &str) -> Transport {
        let transport = Transport::new(Some(DEFAULT_SERVER_USERNAME), Some(password), server.base_url());
        transport.disable_env_proxy(Some(DEFAULT_SERVER_USERNAME), Some(password));
        transport
    }
}
