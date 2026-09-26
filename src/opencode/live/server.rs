//! The live suite's child pinned server (ADR-0057).
//!
//! Runs the real `opencode` binary — for V1 the exact pinned artifact
//! `.github/actions/install-opencode-v1` installs for the release-notes, review
//! and live workflows; for V2 the `/opt`-style 2.x install the wrapper
//! (`opencode-v2`) drives — as a child process whose entire world is a temp
//! directory: every XDG tree is relocated, the config points the only provider
//! at the in-process scripted endpoint, and the store is fresh. It can never
//! see the machine's default store, credentials or config (the spec's "never
//! point a test server at the default store").
//!
//! The harness also refuses a binary of the wrong generation: running the live
//! V1 contract on V2 (or the V2 read chain on V1) would assert the wrong
//! protocol.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::json;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::sync::oneshot;

use crate::opencode::strategy::Generation;

use super::provider;

/// The Basic-auth password the child server requires; the test's client sends
/// it with the default `opencode` username (AGENTS.md pitfall #12).
pub const PASSWORD: &str = "live-harness-secret";

/// How long the server gets to print its listening line (the first boot also
/// creates the isolated store, so it is generous).
const STARTUP_TIMEOUT: Duration = Duration::from_secs(90);

/// A running isolated server. Dropping it kills the process and removes the
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
    /// Start a V1 `binary serve` against an isolated store, with
    /// `provider_base_url` (`http://127.0.0.1:port`, no `/v1`) as the only
    /// configured provider.
    pub async fn start(binary: &str, provider_base_url: &str) -> Self {
        Self::launch(Generation::V1, binary, provider_base_url).await
    }

    /// Start a V2 `binary serve` against an isolated store. V2's config shape
    /// is native (`providers`, `permissions`), for the same guarantee: no
    /// default-store, credential or config coupling.
    pub async fn start_v2(binary: &str, provider_base_url: &str) -> Self {
        Self::launch(Generation::V2, binary, provider_base_url).await
    }

    /// The shared launch path: one isolated temp world, the generation's own
    /// config, and the generation's own listening line.
    async fn launch(generation: Generation, binary: &str, provider_base_url: &str) -> Self {
        let root = tempfile::tempdir().expect("create the live harness temp dir");
        let dirs = |name: &str| root.path().join(name);
        for name in ["data", "config", "cache", "state", "home", "tmp", "work"] {
            std::fs::create_dir_all(dirs(name)).expect("create an isolated XDG tree");
        }
        let work_dir = dirs("work");
        match generation {
            Generation::V1 => write_v1_config(&dirs("config"), provider_base_url),
            Generation::V2 => write_v2_config(&dirs("config"), provider_base_url),
        }

        let mut command = tokio::process::Command::new(binary);
        command
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
            .kill_on_drop(true);
        if generation == Generation::V2 {
            // V2's own explicit config-dir override (the wrapper's recipe).
            command.env("OPENCODE_CONFIG_DIR", dirs("config").join("opencode"));
        }
        let mut child = command.spawn().unwrap_or_else(|error| {
            panic!(
                "cannot start the live OpenCode {} binary `{binary}`: {error}\n\
                 set {} to the pinned binary (or put `opencode` on PATH)",
                generation.as_str(),
                live_binary_env(generation)
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
                // never block on a full pipe. V1 prefixes the line with the
                // artifact name, V2 does not.
                if let Some(url) = listen_line(&line)
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
                    "the live {} server did not report its listening address within {}s:\n{}",
                    generation.as_str(),
                    STARTUP_TIMEOUT.as_secs(),
                    stderr.lock().unwrap()
                )
            })
            .unwrap_or_else(|_| {
                panic!(
                    "the live {} server exited before reporting its listening address:\n{}",
                    generation.as_str(),
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
        // `kill_on_drop` guards the process, but the temp tree drops right
        // after this — so reap the child (bounded) before the tree goes away,
        // or a still-writing server could race the cleanup.
        let _ = self.child.start_kill();
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            match self.child.try_wait() {
                Ok(Some(_)) | Err(_) => return,
                Ok(None) => std::thread::sleep(Duration::from_millis(20)),
            }
        }
    }
}

/// Fail fast unless `binary` is a V1 binary, and return the version it
/// reported. The live suite asserts V1's wire contract; a V2 binary would
/// The env var that points a live test at its generation's binary.
fn live_binary_env(generation: Generation) -> &'static str {
    match generation {
        Generation::V1 => "COLA_LIVE_OPENCODE_BIN",
        Generation::V2 => "COLA_LIVE_OPENCODE_V2_BIN",
    }
}

/// The URL inside a server's listening line. V1 prefixes it with the artifact
/// name (`opencode server listening on …`); V2 prints it bare
/// (`server listening on …`), so the marker is matched without its prefix.
fn listen_line(line: &str) -> Option<&str> {
    line.rsplit_once("server listening on ").map(|(_, url)| url)
}

/// Fail fast unless `binary` is a V1 binary, and return the version it
/// reported. The live V1 suite asserts V1's wire contract; a V2 binary would
/// silently test the wrong generation.
///
/// The check is the major generation only, deliberately: the exact version pin
/// lives once in `.github/actions/install-opencode-v1`, and CI runs the binary
/// that action installs. Hardcoding the version here would create the second
/// copy a pin bump must chase (spec #364 §11); the local run just needs to be
/// V1, and the version it used is printed as evidence.
pub async fn ensure_v1_binary(binary: &str) -> String {
    let stdout = binary_version(binary).await;
    assert_eq!(
        binary_major(&stdout),
        "1",
        "the live V1 suite runs only against a V1 binary (reported `{stdout}`); \
         point COLA_LIVE_OPENCODE_BIN at the pinned V1 binary"
    );
    stdout
}

/// Fail fast unless `binary` is a V2 binary, and return the version it
/// reported. The mirror of [`ensure_v1_binary`]: the V2 read slice must run
/// against V2's `/api` surface, never against the V1 compatibility surface.
pub async fn ensure_v2_binary(binary: &str) -> String {
    let stdout = binary_version(binary).await;
    assert_eq!(
        binary_major(&stdout),
        "2",
        "the live V2 suite runs only against a V2 binary (reported `{stdout}`); \
         point COLA_LIVE_OPENCODE_V2_BIN at a 2.x install"
    );
    stdout
}

/// `binary --version`, trimmed — the artifact's own report.
async fn binary_version(binary: &str) -> String {
    let output = tokio::process::Command::new(binary)
        .arg("--version")
        .output()
        .await
        .unwrap_or_else(|error| {
            panic!(
                "cannot run the live OpenCode binary `{binary}`: {error}\n\
                 point COLA_LIVE_OPENCODE_BIN (V1) / COLA_LIVE_OPENCODE_V2_BIN (V2) \
                 at the pinned binary"
            )
        });
    let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
    assert!(output.status.success(), "`{binary} --version` failed: {stderr}");
    stdout
}

/// The major version of `vX.Y.Z` / `X.Y.Z`, empty when the report is garbled.
fn binary_major(report: &str) -> &str {
    report
        .rsplit(' ')
        .next()
        .unwrap_or("")
        .trim_start_matches('v')
        .split('.')
        .next()
        .unwrap_or("")
}

/// Write the isolated V1 global config: one scripted provider, the model it
/// serves, title generation on the same model (so the title call is scripted
/// too), and `bash` gated behind a permission ask.
fn write_v1_config(config_home: &Path, provider_base_url: &str) {
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
    write_config_file(config_home, &config);
}

/// Write the isolated V2 global config in V2's native shape: a provider whose
/// package is the bundled `openai-compatible` entrypoint (a V1-style `npm`
/// spec would send V2 to npm for the AI-SDK package it no longer resolves), and
/// the V2 permissions list. `shell` (V2's renamed `bash`) is allowed outright:
/// the read slice has no permission round-trip yet (S6), and the V2 chain is
/// deliberately not gated on a card that cannot answer it.
fn write_v2_config(config_home: &Path, provider_base_url: &str) {
    let config = json!({
        "$schema": "https://opencode.ai/config.json",
        "model": provider::MODEL_REF,
        "agents": { "title": { "model": provider::MODEL_REF } },
        "autoupdate": false,
        "share": "disabled",
        "snapshot": false,
        "permissions": [ { "action": "shell", "resource": "*", "effect": "allow" } ],
        "providers": {
            provider::PROVIDER: {
                "name": "Scripted live provider",
                "package": "@opencode/ai/providers/openai-compatible",
                "settings": {
                    "baseURL": format!("{provider_base_url}/v1"),
                    "apiKey": "live-harness",
                },
                "models": {
                    provider::MODEL: {
                        "name": "Scripted Model",
                        "limit": { "context": 128_000, "output": 4_096 },
                    },
                },
            },
        },
    });
    write_config_file(config_home, &config);
}

/// Write one isolated `opencode.json`.
fn write_config_file(config_home: &Path, config: &serde_json::Value) {
    let dir = config_home.join("opencode");
    std::fs::create_dir_all(&dir).expect("create the isolated config dir");
    std::fs::write(
        dir.join("opencode.json"),
        serde_json::to_string_pretty(config).expect("serialize the live config"),
    )
    .expect("write the isolated config");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_isolated_v1_config_points_the_provider_at_the_scripted_endpoint() {
        let root = tempfile::tempdir().unwrap();
        write_v1_config(root.path(), "http://127.0.0.1:1234");
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

    /// The V2 config is V2-native: the bundled provider package (never a V1
    /// `npm` spec, which V2 would try to install), a permissions list whose
    /// `shell` action is allowed, and the title agent on the scripted model.
    #[test]
    fn the_isolated_v2_config_declares_the_native_provider_and_shell_permission() {
        let root = tempfile::tempdir().unwrap();
        write_v2_config(root.path(), "http://127.0.0.1:1234");
        let text = std::fs::read_to_string(root.path().join("opencode/opencode.json")).unwrap();
        let config: serde_json::Value = serde_json::from_str(&text).unwrap();

        assert_eq!(
            config["providers"][provider::PROVIDER]["settings"]["baseURL"],
            "http://127.0.0.1:1234/v1"
        );
        assert_eq!(
            config["providers"][provider::PROVIDER]["package"],
            "@opencode/ai/providers/openai-compatible"
        );
        assert!(config["provider"].is_null(), "no V1 provider shape");
        assert_eq!(config["model"], provider::MODEL_REF);
        assert_eq!(config["agents"]["title"]["model"], provider::MODEL_REF);
        assert_eq!(config["permissions"][0]["action"], "shell");
        assert_eq!(config["permissions"][0]["effect"], "allow");
        assert!(
            config["providers"][provider::PROVIDER]["models"][provider::MODEL].is_object(),
            "the scripted model must be declared"
        );
    }

    #[test]
    fn both_listening_line_spellings_yield_the_url() {
        assert_eq!(
            listen_line("opencode server listening on http://127.0.0.1:4096"),
            Some("http://127.0.0.1:4096")
        );
        assert_eq!(
            listen_line("server listening on http://127.0.0.1:49374"),
            Some("http://127.0.0.1:49374")
        );
        assert_eq!(listen_line("some other line"), None);
    }
}
