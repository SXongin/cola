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

/// Find the running OpenCode server's port and password by scanning /proc.
/// Returns (port, password) if a matching process is found.
fn find_opencode_server(preferred_port: Option<&str>) -> Option<(String, String)> {
    let entries = std::fs::read_dir("/proc").ok()?;
    let mut servers: Vec<(String, String)> = Vec::new();
    for entry in entries.flatten() {
        let pid = entry.file_name().to_string_lossy().to_string();
        if !pid.chars().all(|c| c.is_ascii_digit()) {
            continue;
        }
        let cmdline_path = format!("/proc/{}/cmdline", pid);
        let Ok(cmdline_bytes) = std::fs::read(&cmdline_path) else { continue };
        let cmdline = String::from_utf8_lossy(&cmdline_bytes);
        let args: Vec<&str> = cmdline.split('\0').collect();
        // Binary path contains "opencode" and args contain "serve" subcommand
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
                port = Some(args[idx + 1].to_string());
            }
            idx += 1;
        }
        let port = port?;
        // Read password from env
        let env_path = format!("/proc/{}/environ", pid);
        let Ok(env_bytes) = std::fs::read(&env_path) else { continue };
        let env = String::from_utf8_lossy(&env_bytes);
        let password = env
            .split('\0')
            .find_map(|kv| kv.strip_prefix("OPENCODE_SERVER_PASSWORD="))
            .unwrap_or("")
            .to_string();
        servers.push((port, password));
    }
    // Only match the configured port — cola runs its own dedicated server.
    // Falling back to an arbitrary other OpenCode process would silently
    // hijack another app's server (wrong data dir + password).
    if let Some(pref) = preferred_port {
        servers.into_iter().find(|(p, _)| p == pref)
    } else {
        servers.into_iter().next()
    }
}

/// Update the OpenCode config with the auto-detected port/password.
fn auto_detect_opencode(cfg: &mut config::OpenCodeConfig) {
    let preferred = cfg.url.rsplit(':').next().map(|s| s.to_string());
    if let Some((port, password)) = find_opencode_server(preferred.as_deref()) {
        let detected = format!("http://localhost:{}", port);
        if detected != cfg.url || password != cfg.password.as_deref().unwrap_or("") {
            tracing::info!(
                "Auto-detected OpenCode at {} (was {})",
                detected,
                cfg.url
            );
            cfg.url = detected;
            cfg.password = Some(password);
        }
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {    tracing_subscriber::registry()
        .with(EnvFilter::try_from_default_env().unwrap_or_else(|_| "cola=info".into()))
        .with(tracing_subscriber::fmt::layer())
        .init();

    let cli = Cli::parse();
    let mut cfg = config::load(&cli.config)?;

    // Auto-detect OpenCode server if the configured URL is unreachable
    // (OpenChamber restarts OpenCode on a random port, breaking the config)
    auto_detect_opencode(&mut cfg.opencode);

    tracing::info!(
        "cola starting — OpenCode: {}, Feishu app: {}",
        cfg.opencode.url,
        cfg.feishu.app_id
    );

    let feishu_client = feishu::Client::new(cfg.feishu.clone());

    // Test: send a message to verify the Feishu REST API works
    let user_open_id = "ou_5f48e110fbdcda4a3f1500e74055e42e";
    match feishu_client.send_text("open_id", user_open_id, "cola online ✓").await {
        Ok(msg_id) => tracing::info!("Test message sent: {}", msg_id),
        Err(e) => tracing::error!("Test message failed: {}", e),
    }

    let app = Arc::new(bridge::App::new(
        cfg.clone(),
        opencode::Client::new(cfg.opencode.clone()),
        feishu_client,
    )?);

    app.run().await?;

    Ok(())
}
