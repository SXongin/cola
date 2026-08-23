/// Discovery of the OpenCode server cola should attach to, and the command to
/// start its own when none is running.
///
/// The invariant is the STORE: sessions are shared only when every client
/// (cola, OpenChamber, the CLI) reads the same data directory
/// (`~/.local/share/opencode`). cola therefore attaches to any OpenCode server
/// running on the default store (whoever started it — OpenChamber, a manual
/// `opencode serve`, another tool) and only starts its own when none exists.

/// An `opencode serve` process discovered on the machine.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServerCandidate {
    pub pid: i32,
    pub port: u16,
    pub password: String,
    /// Whether the server reads the default store (`XDG_DATA_HOME` unset or
    /// equal to the default `~/.local/share/opencode`).
    pub uses_default_store: bool,
}

/// Scan `/proc` for running `opencode serve` processes. Thin I/O — the
/// interesting decisions live in [`select_server`].
pub fn scan_processes() -> Vec<ServerCandidate> {
    let default_data_home = dirs::data_dir().map(|p| p.to_string_lossy().to_string());
    let mut out = Vec::new();
    let Ok(entries) = std::fs::read_dir("/proc") else {
        return out;
    };
    for entry in entries.flatten() {
        let pid = entry.file_name().to_string_lossy().to_string();
        if !pid.chars().all(|c| c.is_ascii_digit()) {
            continue;
        }
        let pid_num: i32 = pid.parse().ok().unwrap_or(0);
        let cmdline = match std::fs::read(format!("/proc/{pid}/cmdline")) {
            Ok(b) => String::from_utf8_lossy(&b).into_owned(),
            Err(_) => continue,
        };
        let args: Vec<&str> = cmdline.split('\0').collect();
        let is_server = args.first().map(|a| a.contains("opencode")).unwrap_or(false)
            && args.iter().skip(1).any(|a| *a == "serve");
        if !is_server {
            continue;
        }
        let mut port = None;
        let mut idx = 0;
        while idx < args.len() {
            if args[idx] == "--port" && idx + 1 < args.len() {
                port = args[idx + 1].parse().ok();
            }
            idx += 1;
        }
        let Some(port) = port else { continue };
        let env = match std::fs::read(format!("/proc/{pid}/environ")) {
            Ok(b) => String::from_utf8_lossy(&b).into_owned(),
            Err(_) => continue,
        };
        let password = env
            .split('\0')
            .find_map(|kv| kv.strip_prefix("OPENCODE_SERVER_PASSWORD="))
            .unwrap_or("")
            .to_string();
        let xdg = env
            .split('\0')
            .find_map(|kv| kv.strip_prefix("XDG_DATA_HOME="))
            .map(|s| s.to_string());
        let uses_default_store = match xdg {
            None => true,
            Some(home) => Some(home) == default_data_home,
        };
        out.push(ServerCandidate {
            pid: pid_num,
            port,
            password,
            uses_default_store,
        });
    }
    out
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
/// Reads `/proc/<pid>/cmdline` and checks the `serve` subcommand.
fn is_live_opencode_serve(pid: i32) -> bool {
    let cmdline = match std::fs::read(format!("/proc/{pid}/cmdline")) {
        Ok(b) => String::from_utf8_lossy(&b).into_owned(),
        Err(_) => return false,
    };
    let args: Vec<&str> = cmdline.split('\0').collect();
    args.first().map(|a| a.contains("opencode")).unwrap_or(false)
        && args.iter().skip(1).any(|a| *a == "serve")
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
    let Some(server) = select_server(&candidates, None).map(|c| c.clone()) else {
        return Ok(RestartOutcome::NoServer);
    };
    if !is_self_spawned(server.pid) {
        return Ok(RestartOutcome::NotOwned);
    }
    // Kill our own server, wait for the port to release, re-raise it on the same
    // port/password, and wait until it accepts connections again.
    let _ = clear_self_spawned();
    match std::process::Command::new("kill")
        .arg(server.pid.to_string())
        .status()
    {
        Ok(_) => tracing::info!("restarting self-spawned opencode (pid {})", server.pid),
        Err(e) => {
            tracing::warn!("kill self-spawned opencode {} failed: {}", server.pid, e);
            return Err(e.into());
        }
    }
    // Give the port time to free up.
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
}
