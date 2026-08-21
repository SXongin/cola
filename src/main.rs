mod bridge;
mod config;
mod error;
mod feishu;
mod opencode;

use clap::Parser;
use std::sync::Arc;
use tracing_subscriber::{EnvFilter, layer::SubscriberExt, util::SubscriberInitExt};

#[derive(Parser)]
#[command(name = "cola", about = "Bridge bot connecting OpenCode to Feishu")]
struct Cli {
    #[arg(short, long, default_value = "cola.toml")]
    config: String,
}

/// Resolve which OpenCode server cola talks to, mutating the config.
///
/// The invariant is the STORE: sessions are shared only when cola, OpenChamber
/// and the CLI read the same data directory (`~/.local/share/opencode`). So
/// cola attaches to whatever `opencode serve` is already running on the default
/// store (OpenChamber's managed server, a manual one — whoever started it) and
/// only starts its own when none exists. A server on a custom store is never
/// touched.
async fn resolve_opencode_server(cfg: &mut config::OpenCodeConfig) -> anyhow::Result<()> {
    let preferred_port = cfg.url.rsplit(':').next().and_then(|s| s.parse().ok());
    let candidates = bridge::discovery::scan_processes();

    if let Some(server) = bridge::discovery::select_server(&candidates, preferred_port).cloned() {
        cfg.url = format!("http://localhost:{}", server.port);
        cfg.password = Some(server.password);
        tracing::info!("Attached to OpenCode server at {}", cfg.url);
        return Ok(());
    }

    // No shared server running — start our own on the default store.
    let port = {
        let mut p = preferred_port.unwrap_or(4096);
        // Skip ports already held by a foreign (custom-store) server.
        while candidates.iter().any(|c| c.port == p) {
            p += 1;
        }
        p
    };
    let password = cfg.password.clone().unwrap_or_else(|| "cola-secret".to_string());
    tracing::info!("No shared OpenCode server found; starting one on port {}", port);
    spawn_self_server(port, &password)?;
    wait_for_port(port).await?;
    cfg.url = format!("http://localhost:{}", port);
    cfg.password = Some(password);
    Ok(())
}

/// Spawn cola's own `opencode serve` (default store, so sessions stay shared).
fn spawn_self_server(port: u16, password: &str) -> anyhow::Result<()> {
    let cmd = bridge::discovery::self_start_command(port, password);
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
    child.spawn()?;
    Ok(())
}

/// Wait until the self-started server accepts connections.
async fn wait_for_port(port: u16) -> anyhow::Result<()> {
    for _ in 0..100 {
        if tokio::net::TcpStream::connect(("127.0.0.1", port)).await.is_ok() {
            return Ok(());
        }
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    }
    anyhow::bail!("OpenCode server did not start on port {}", port)
}

/// Path of the singleton lock file — created atomically at startup so a second
/// cola process refuses to start and stays out of the Feishu WS + permission
/// flow (two instances double-answer permission cards and double-send events).
fn lock_file_path() -> std::path::PathBuf {
    dirs::home_dir()
        .unwrap_or_else(|| std::path::PathBuf::from("."))
        .join(".cola")
        .join("cola.lock")
}

/// Hold the singleton lock for the lifetime of the process. Fails fast if
/// another cola is already running (its PID is in the lock file). The lock file
/// is cleaned up on drop.
struct SingletonLock(std::path::PathBuf);

/// Whether a PID refers to a live process. On Linux this checks `/proc/{pid}`;
/// elsewhere it conservatively assumes the PID is alive so a lock is never
/// stolen from a real process (a stale lock on a non-Linux host is a manual
/// delete, same as before).
///
/// A zombie ('Z') is treated as DEAD: it can no longer handle a permission or
/// post an event, so a lock owned by one is stale and reclaimable. This is what
/// lets a `/restart` child start while the old process lingers as a zombie
/// until its parent (the interactive shell) reaps it — without this, the child
/// sees the old PID in /proc, refuses to start ("Another cola instance"), and
/// exits, leaving nothing running.
fn pid_alive(pid: i32) -> bool {
    #[cfg(target_os = "linux")]
    {
        let Ok(stat) = std::fs::read_to_string(format!("/proc/{pid}/stat")) else {
            return false;
        };
        // /proc/<pid>/stat is "pid (comm) state ppid ..." — the comm may itself
        // contain ')', so take the part after the LAST ')' and read its first
        // char (field 3 = state). Unknown/unparseable is treated as alive (the
        // conservative direction: never steal a lock we can't verify is dead).
        !matches!(
            stat.rsplit(')')
                .next()
                .and_then(|s| s.trim_start().chars().next()),
            Some('Z')
        )
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = pid;
        true
    }
}

impl SingletonLock {
    fn acquire() -> anyhow::Result<Self> {
        let path = lock_file_path();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        // A lock file whose PID is no longer alive is a stale leftover from a
        // crashed/killed process — reclaim it instead of failing forever. A
        // zombie counts as dead (see `pid_alive`), which is also what lets a
        // `/restart` child start while the old process lingers as a zombie.
        if let Some(pid) = stale_lock_owner(&path) {
            tracing::warn!("Removing stale singleton lock (owner PID {} is not alive)", pid);
            let _ = std::fs::remove_file(&path);
        }
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
        {
            Ok(mut file) => {
                use std::io::Write;
                writeln!(file, "{}", std::process::id())?;
                tracing::info!("Acquired singleton lock at {}", path.display());
                Ok(SingletonLock(path))
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                let owner = std::fs::read_to_string(&path).unwrap_or_default();
                anyhow::bail!(
                    "Another cola instance is already running (lock {} owned by PID {}). \
                     Refusing to start a duplicate that would double-handle Feishu events.",
                    path.display(),
                    owner.trim()
                )
            }
            Err(e) => Err(e.into()),
        }
    }
}

/// Whether the PID recorded in a lock file is stale (dead or a zombie) and the
/// lock can be reclaimed. `None` means "not reclaimable" — either the file
/// can't be read, the PID can't be parsed, or the owner is genuinely alive.
fn stale_lock_owner(path: &std::path::Path) -> Option<i32> {
    let owner = std::fs::read_to_string(path).ok()?;
    let pid = owner.trim().parse::<i32>().ok()?;
    if pid_alive(pid) { None } else { Some(pid) }
}

impl Drop for SingletonLock {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::registry()
        .with(EnvFilter::try_from_default_env().unwrap_or_else(|_| "cola=info".into()))
        .with(tracing_subscriber::fmt::layer())
        .init();

    let cli = Cli::parse();
    let mut cfg = config::load(&cli.config)?;

    // Grab the singleton lock BEFORE touching the network or the store — a
    // duplicate cola must die before it can double-connect to the Feishu WS.
    let _lock = SingletonLock::acquire()?;

    resolve_opencode_server(&mut cfg.opencode).await?;

    tracing::info!(
        "cola starting — OpenCode: {}, Feishu app: {}",
        cfg.opencode.url,
        cfg.feishu.app_id
    );

    let feishu_client = feishu::Client::new(cfg.feishu.clone());

    let opencode_client = opencode::Client::new(cfg.opencode.clone());
    let app = Arc::new(bridge::App::new(
        cfg.clone(),
        Arc::new(opencode_client),
        Arc::new(feishu_client),
    )?);

    app.run().await?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pid_alive_sees_own_process_as_alive() {
        assert!(pid_alive(std::process::id() as i32));
    }

    #[test]
    fn pid_alive_missing_pid_is_dead() {
        // PID 1 is the init process and is always alive on Linux.
        assert!(pid_alive(1));
        // A far-out PID is almost certainly nonexistent.
        assert!(!pid_alive(i32::MAX - 1));
    }

    /// The core restart bug: a zombie process's /proc entry persists until its
    /// parent reaps it, so the old `pid_alive` (mere /proc existence) treated a
    /// zombie as alive. A `/restart` child therefore refused to start while the
    /// old cola lingered as a zombie, and the new process never came up.
    #[test]
    fn pid_alive_treats_zombie_as_dead() {
        let mut child = std::process::Command::new("sh")
            .arg("-c")
            .arg("exit 0")
            .spawn()
            .expect("spawn zombie child");
        let pid = child.id() as i32;
        // Let the child exit; we must NOT wait() yet, so it stays a zombie.
        std::thread::sleep(std::time::Duration::from_millis(300));
        let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).expect("zombie stat readable");
        let state = stat
            .rsplit(')')
            .next()
            .and_then(|s| s.trim_start().chars().next());
        assert_eq!(state, Some('Z'), "child should be a zombie: {stat}");
        assert!(!pid_alive(pid), "a zombie must not be considered alive");
        let _ = child.wait(); // reap
    }

    #[test]
    fn singleton_lock_reclaimed_from_zombie_owner() {
        let home = tempfile::tempdir().unwrap();
        let lock = home.path().join("cola.lock");

        // Simulate the old cola: it acquires the lock and exits, leaving its
        // PID in the file as a zombie.
        let mut owner = std::process::Command::new("sh")
            .arg("-c")
            .arg(format!("echo $$ > '{}'; exit 0", lock.display()))
            .spawn()
            .expect("spawn lock owner");
        let owner_pid = owner.id() as i32;
        std::thread::sleep(std::time::Duration::from_millis(300));
        assert!(!pid_alive(owner_pid), "owner should be a zombie");

        // A new cola can now reclaim the lock instead of refusing to start:
        // the stale-owner check must see the zombie as dead.
        assert_eq!(
            stale_lock_owner(&lock),
            Some(owner_pid),
            "a lock owned by a zombie must be stale/reclaimable"
        );
        let _ = owner.wait(); // reap the zombie
    }

    #[test]
    fn singleton_lock_with_live_owner_is_not_stale() {
        let home = tempfile::tempdir().unwrap();
        let lock = home.path().join("cola.lock");
        std::fs::write(&lock, std::process::id().to_string()).unwrap();
        // Our own process is alive — the lock must NOT be reclaimable.
        assert_eq!(stale_lock_owner(&lock), None);
    }
}
