//! The live suite's child pinned-V1 server (ADR-0057).
//!
//! Runs the real `opencode serve` binary — the exact pinned 1.18.31 artifact
//! the release-notes and review workflows install — as a child process whose
//! entire world is a temp directory: every XDG tree is relocated, the config
//! points the only provider at the in-process scripted endpoint, and the store
//! is fresh. It can never see the machine's default store, credentials or
//! config (the spec's "never point a test server at the default store").
//!
//! The harness also refuses a V2 binary: running the live V1 contract on a
//! different generation would assert the wrong protocol.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::json;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::sync::oneshot;

use super::provider;

/// The Basic-auth password the child server requires; the test's client sends
/// it with the default `opencode` username (AGENTS.md pitfall #12).
pub const PASSWORD: &str = "live-harness-secret";

/// How long the server gets to print its listening line (the first boot also
/// creates the isolated store, so it is generous).
const STARTUP_TIMEOUT: Duration = Duration::from_secs(90);

/// A running isolated V1 server. Dropping it kills the process and removes the
/// temp tree (`kill_on_drop` plus the `TempDir`).
pub struct LiveServer {
    child: tokio::process::Child,
    /// Held so the isolated trees (and their contents) live exactly as long as
    /// the server does.
    _root: tempfile::TempDir,
    base_url: String,
    work_dir: PathBuf,
    stderr: Arc<Mutex<String>>,
}

impl LiveServer {
    /// Start `binary serve` against an isolated store, with `provider_base_url`
    /// (`http://127.0.0.1:port`, no `/v1`) as the only configured provider.
    pub async fn start(binary: &str, provider_base_url: &str) -> Self {
        let root = tempfile::tempdir().expect("create the live harness temp dir");
        let dirs = |name: &str| root.path().join(name);
        for name in ["data", "config", "cache", "state", "home", "tmp", "work"] {
            std::fs::create_dir_all(dirs(name)).expect("create an isolated XDG tree");
        }
        let work_dir = dirs("work");
        write_config(&dirs("config"), provider_base_url);

        let mut child = tokio::process::Command::new(binary)
            .arg("serve")
            .arg("--hostname")
            .arg("127.0.0.1")
            .arg("--port")
            .arg("0")
            .current_dir(&work_dir)
            // A clean, proxy-free environment: the only network the server may
            // reach is the loopback provider (models.dev fetch is disabled, so
            // no credentials or external services are involved at all).
            .env_clear()
            .env("HOME", dirs("home"))
            .env("PATH", "/usr/local/bin:/usr/bin:/bin")
            .env("XDG_DATA_HOME", dirs("data"))
            .env("XDG_CONFIG_HOME", dirs("config"))
            .env("XDG_CACHE_HOME", dirs("cache"))
            .env("XDG_STATE_HOME", dirs("state"))
            .env("TMPDIR", dirs("tmp"))
            .env("OPENCODE_SERVER_PASSWORD", PASSWORD)
            .env("OPENCODE_DISABLE_MODELS_FETCH", "1")
            .env("OPENCODE_DISABLE_AUTOUPDATE", "1")
            .env("NO_PROXY", "127.0.0.1,localhost")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .unwrap_or_else(|error| {
                panic!(
                    "cannot start the live OpenCode binary `{binary}`: {error}\n\
                     set COLA_LIVE_OPENCODE_BIN to the pinned V1 1.18.31 binary \
                     (or put `opencode` on PATH)"
                )
            });

        let stderr = Arc::new(Mutex::new(String::new()));
        if let Some(pipe) = child.stderr.take() {
            let buffer = Arc::clone(&stderr);
            tokio::spawn(async move {
                let mut lines = BufReader::new(pipe).lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    let mut collected = buffer.lock().unwrap();
                    collected.push_str(&line);
                    collected.push('\n');
                }
            });
        }

        let stdout = child.stdout.take().expect("the server stdout is piped");
        let (listen_tx, listen_rx) = oneshot::channel::<String>();
        tokio::spawn(async move {
            let mut lines = BufReader::new(stdout).lines();
            let mut listen_tx = Some(listen_tx);
            while let Ok(Some(line)) = lines.next_line().await {
                // Keep draining after the handshake so a chatty server can
                // never block on a full pipe.
                if let Some(url) = line.strip_prefix("opencode server listening on ")
                    && let Some(tx) = listen_tx.take()
                {
                    let _ = tx.send(url.trim().to_string());
                }
            }
        });

        let listen = tokio::time::timeout(STARTUP_TIMEOUT, listen_rx)
            .await
            .unwrap_or_else(|_| {
                panic!(
                    "the pinned V1 server did not report its listening address within {}s:\n{}",
                    STARTUP_TIMEOUT.as_secs(),
                    stderr.lock().unwrap()
                )
            })
            .unwrap_or_else(|_| {
                panic!(
                    "the pinned V1 server exited before reporting its listening address:\n{}",
                    stderr.lock().unwrap()
                )
            });

        Self {
            child,
            _root: root,
            base_url: listen,
            work_dir,
            stderr,
        }
    }

    /// The server's base URL (`http://127.0.0.1:port`), from its own output.
    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    /// The isolated working directory handed to every Session — the instance
    /// directory cola routes reads and permission replies with.
    pub fn work_dir(&self) -> String {
        self.work_dir.to_string_lossy().into_owned()
    }

    /// Everything the server wrote to stderr so far, for failure messages.
    pub fn stderr(&self) -> String {
        self.stderr.lock().unwrap().clone()
    }
}

impl Drop for LiveServer {
    fn drop(&mut self) {
        // `kill_on_drop` already guards the process; killing eagerly here keeps
        // the temp-tree removal from racing a still-writing server.
        let _ = self.child.start_kill();
    }
}

/// Fail fast unless `binary` is a V1 binary. The live suite asserts V1's wire
/// contract; a V2 binary would silently test the wrong generation.
pub async fn ensure_v1_binary(binary: &str) {
    let output = tokio::process::Command::new(binary)
        .arg("--version")
        .output()
        .await
        .unwrap_or_else(|error| {
            panic!(
                "cannot run the live OpenCode binary `{binary}`: {error}\n\
                 set COLA_LIVE_OPENCODE_BIN to the pinned V1 1.18.31 binary \
                 (or put `opencode` on PATH)"
            )
        });
    let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
    assert!(output.status.success(), "`{binary} --version` failed: {stderr}");
    let major = stdout
        .rsplit(' ')
        .next()
        .unwrap_or("")
        .trim_start_matches('v')
        .split('.')
        .next()
        .unwrap_or("");
    assert_eq!(
        major, "1",
        "the live V1 suite runs only against a V1 binary (reported `{stdout}`); \
         point COLA_LIVE_OPENCODE_BIN at the pinned 1.18.31 binary"
    );
}

/// Write the isolated global config: one scripted provider, the model it
/// serves, title generation on the same model (so the title call is scripted
/// too), and `bash` gated behind a permission ask.
fn write_config(config_home: &Path, provider_base_url: &str) {
    let config = json!({
        "$schema": "https://opencode.ai/config.json",
        "model": provider::MODEL_REF,
        "small_model": provider::MODEL_REF,
        "autoupdate": false,
        "share": "disabled",
        "snapshot": false,
        "permission": { "bash": "ask" },
        "provider": {
            provider::PROVIDER: {
                "npm": "@ai-sdk/openai-compatible",
                "name": "Scripted live provider",
                "options": {
                    "baseURL": format!("{provider_base_url}/v1"),
                    "apiKey": "live-harness",
                },
                "models": {
                    provider::MODEL: {
                        "name": "Scripted Model",
                        "tool_call": true,
                        "limit": { "context": 128_000, "output": 4_096 },
                    },
                },
            },
        },
    });
    let dir = config_home.join("opencode");
    std::fs::create_dir_all(&dir).expect("create the isolated config dir");
    std::fs::write(
        dir.join("opencode.json"),
        serde_json::to_string_pretty(&config).expect("serialize the live config"),
    )
    .expect("write the isolated config");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_isolated_config_points_the_provider_at_the_scripted_endpoint() {
        let root = tempfile::tempdir().unwrap();
        write_config(root.path(), "http://127.0.0.1:1234");
        let text = std::fs::read_to_string(root.path().join("opencode/opencode.json")).unwrap();
        let config: serde_json::Value = serde_json::from_str(&text).unwrap();

        assert_eq!(
            config["provider"][provider::PROVIDER]["options"]["baseURL"],
            "http://127.0.0.1:1234/v1"
        );
        assert_eq!(config["model"], provider::MODEL_REF);
        assert_eq!(config["small_model"], provider::MODEL_REF);
        assert_eq!(config["permission"]["bash"], "ask");
        assert!(
            config["provider"][provider::PROVIDER]["models"][provider::MODEL].is_object(),
            "the scripted model must be declared"
        );
    }
}
