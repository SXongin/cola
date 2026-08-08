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
    let Ok(entries) = std::fs::read_dir("/proc") else { return out };
    for entry in entries.flatten() {
        let pid = entry.file_name().to_string_lossy().to_string();
        if !pid.chars().all(|c| c.is_ascii_digit()) {
            continue;
        }
        let cmdline = match std::fs::read(format!("/proc/{pid}/cmdline")) {
            Ok(b) => String::from_utf8_lossy(&b).into_owned(),
            Err(_) => continue,
        };
        let args: Vec<&str> = cmdline.split('\0').collect();
        let is_server = args
            .first()
            .map(|a| a.contains("opencode"))
            .unwrap_or(false)
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
        out.push(ServerCandidate { port, password, uses_default_store });
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
    let eligible: Vec<&ServerCandidate> = candidates
        .iter()
        .filter(|c| c.uses_default_store)
        .collect();
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

#[cfg(test)]
mod tests {
    use super::*;

    fn cand(port: u16, password: &str, uses_default_store: bool) -> ServerCandidate {
        ServerCandidate { port, password: password.into(), uses_default_store }
    }

    #[test]
    fn select_server_prefers_default_store() {
        let custom = cand(4096, "custom", false);
        let def = cand(4001, "default", true);
        let servers = [custom, def];
        let chosen = select_server(&servers, None).unwrap();
        assert_eq!(chosen.port, 4001);
    }

    #[test]
    fn select_server_prefers_configured_port_among_default_store() {
        let a = cand(4001, "x", true);
        let b = cand(4096, "y", true);
        let servers = [a, b];
        let chosen = select_server(&servers, Some(4096)).unwrap();
        assert_eq!(chosen.port, 4096);
    }

    #[test]
    fn select_server_never_hijacks_custom_store() {
        let custom = cand(4096, "custom", false);
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
        assert!(cmd.env.contains(&("OPENCODE_SERVER_PASSWORD".into(), "secret".into())));
        // The server MUST land on the default store so sessions are shared with
        // OpenChamber / the CLI: strip any inherited XDG_DATA_HOME rather than
        // pinning a private one.
        assert!(cmd.remove_env.contains(&"XDG_DATA_HOME".to_string()));
    }
}
