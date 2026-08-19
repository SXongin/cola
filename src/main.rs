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

impl SingletonLock {
    fn acquire() -> anyhow::Result<Self> {
        let path = lock_file_path();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
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
