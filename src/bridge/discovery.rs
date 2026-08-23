//! Discovery of the OpenCode server cola should attach to, and the command to
//! start its own when none is running.
//!
//! The invariant is the STORE: sessions are shared only when every client
//! (cola, OpenChamber, the CLI) reads the same data directory
//! (`~/.local/share/opencode`). cola therefore attaches to any OpenCode server
//! running on the default store (whoever started it — OpenChamber, a manual
//! `opencode serve`, another tool) and only starts its own when none exists.

use sysinfo::{Pid, ProcessRefreshKind, ProcessesToUpdate, System, UpdateKind};

/// An `opencode serve` process discovered on the machine.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServerCandidate {
    pub pid: i32,
    pub port: u16,
    pub password: String,
    /// Whether the server reads the default store (the directory OpenCode
    /// itself resolves as its default data home — `$XDG_DATA_HOME` if set,
    /// else `~/.local/share` — on every platform).
    pub uses_default_store: bool,
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

/// Scan for running `opencode serve` processes. Thin I/O — the interesting
/// decisions live in [`select_server`].
///
/// Uses sysinfo so discovery works on every platform (there is no `/proc` on
/// macOS/Windows). cmdline and environ are refreshed on every call (`Always`)
/// so a restarted server's new port/password is picked up.
pub fn scan_processes() -> Vec<ServerCandidate> {
    let default_store = default_data_home().to_string_lossy().into_owned();
    let mut system = System::new();
    let refresh = ProcessRefreshKind::nothing()
        .with_cmd(UpdateKind::Always)
        .with_environ(UpdateKind::Always);
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
            let port = args
                .iter()
                .position(|a| a == "--port")
                .and_then(|idx| args.get(idx + 1))
                .and_then(|p| p.parse().ok())?;
            let env: Vec<String> = proc_
                .environ()
                .iter()
                .map(|s| s.to_string_lossy().into_owned())
                .collect();
            let password = env
                .iter()
                .find_map(|kv| kv.strip_prefix("OPENCODE_SERVER_PASSWORD="))
                .unwrap_or("")
                .to_string();
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
                pid: proc_.pid().as_u32() as i32,
                port,
                password,
                uses_default_store,
            })
        })
        .collect()
}

/// Pick the server cola should attach to.
///
/// Only default-store servers are eligible (a custom-store server is someone
/// else's data — attaching would both break sharing and couple cola to a
/// foreign runtime). Among eligible servers, the configured port wins; the
/// first eligible otherwise.
pub fn select_server(
    candidates: &[ServerCandidate],
    preferred_port: Option<u16>,
) -> Option<&ServerCandidate> {
    let eligible: Vec<&ServerCandidate> = candidates.iter().filter(|c| c.uses_default_store).collect();
    preferred_port
        .and_then(|p| eligible.iter().find(|c| c.port == p).copied())
        .or_else(|| eligible.first().copied())
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
        remove_env: vec!["XDG_DATA_HOME".to_string()],
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
    // If the launching environment doesn't pin OpenCode config, disable the
    // interactive question tool: cola answers questions via Feishu cards, so a
    // blocked question must never hang a session.
    if std::env::var_os("OPENCODE_CONFIG_CONTENT").is_none() {
        child.env("OPENCODE_CONFIG_CONTENT", r#"{"tools":{"question":false}}"#);
    }
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
    let Some(server) = select_server(&candidates, None).cloned() else {
        return Ok(RestartOutcome::NoServer);
    };
    if !is_self_spawned(server.pid) {
        return Ok(RestartOutcome::NotOwned);
    }
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
            uses_default_store,
        }
    }

    #[test]
    fn select_server_prefers_default_store() {
        let custom = cand(1, 4096, "custom", false);
        let def = cand(2, 4001, "default", true);
        let servers = [custom, def];
        let chosen = select_server(&servers, None).unwrap();
        assert_eq!(chosen.port, 4001);
    }

    #[test]
    fn select_server_prefers_configured_port_among_default_store() {
        let a = cand(1, 4001, "x", true);
        let b = cand(2, 4096, "y", true);
        let servers = [a, b];
        let chosen = select_server(&servers, Some(4096)).unwrap();
        assert_eq!(chosen.port, 4096);
    }

    #[test]
    fn select_server_never_hijacks_custom_store() {
        let custom = cand(1, 4096, "custom", false);
        let servers = [custom];
        assert!(select_server(&servers, Some(4096)).is_none());
    }

    #[test]
    fn select_server_empty_returns_none() {
        assert!(select_server(&[], Some(4096)).is_none());
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
}
