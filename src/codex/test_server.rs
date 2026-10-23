//! A scripted `codex app-server` for the transport tests (spec #860, ticket
//! #876).
//!
//! It is a real WebSocket server on a real socket — TCP or unix — so the
//! production client (handshake, framing, response correlation) runs against
//! the wire rather than a mock. It answers `initialize` with the fields the
//! pinned protocol requires and counts `initialized` notifications. Compiled
//! only for tests.

#[cfg(unix)]
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::TcpListener;
#[cfg(unix)]
use tokio::net::UnixListener;
use tokio_tungstenite::tungstenite::Message;

/// Quote one value for `sh` with single quotes, escaping an embedded quote as
/// `'\''`. Test-only: a fixture path is interpolated into a fake binary's
/// script, and a temp path containing an apostrophe must not make a test fail
/// for a reason unrelated to the code under test.
#[cfg(unix)]
pub(crate) fn shell_single_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

/// A scripted `codex` CLI for discovery and lifecycle tests.
///
/// It answers the two management commands those paths use, logs every
/// invocation together with the `CODEX_HOME` it was given, and its
/// `app-server daemon start` links the control socket to a socket the test
/// already serves. That keeps a test's "daemon" a real WebSocket server on a
/// real unix socket while no real Codex state is touched.
#[cfg(unix)]
pub(crate) struct FakeCodex {
    pub(crate) binary: std::path::PathBuf,
    pub(crate) log: std::path::PathBuf,
}

#[cfg(unix)]
impl FakeCodex {
    /// Write the fake CLI into `directory`. `real_socket` is what `daemon
    /// start` links the control socket to (`None`: a launcher that reports
    /// success but leaves no socket, the failure this path must name).
    pub(crate) fn write(
        directory: &Path,
        control_socket: &Path,
        real_socket: Option<&Path>,
        daemon_pid_record: Option<&Path>,
        managed_binary: &Path,
    ) -> Self {
        use std::os::unix::fs::PermissionsExt;

        let binary = directory.join("codex");
        let log = directory.join("fake-codex.log");
        let report = json!({
            "status": "running",
            "backend": "pid",
            "managedCodexPath": managed_binary,
            "managedCodexVersion": "0.163.0",
            "socketPath": control_socket,
            "cliVersion": "0.163.0",
            "appServerVersion": "0.163.0",
        })
        .to_string();
        let start = match real_socket {
            Some(real_socket) => format!(
                "mkdir -p {}\nln -sfn {} {}\nprintf '%s\\n' '{{\"status\":\"started\"}}'\n",
                shell_single_quote(
                    &control_socket
                        .parent()
                        .expect("control dir")
                        .display()
                        .to_string()
                ),
                shell_single_quote(&real_socket.display().to_string()),
                shell_single_quote(&control_socket.display().to_string()),
            ),
            None => "printf '%s\\n' '{\"status\":\"started\"}'\n".to_string(),
        };
        // The fake daemon is a detached process of its own, with its own pid
        // record — the shape cola must never adopt as its own. Its stdio is
        // detached so `output()` does not wait on the pipe it inherits.
        let start = match daemon_pid_record {
            Some(record) => format!(
                "{}sleep 30 >/dev/null 2>&1 &\necho $! >> {}\n",
                start,
                shell_single_quote(&record.display().to_string())
            ),
            None => start,
        };
        let script = format!(
            "#!/bin/sh\n\
             printf 'CODEX_HOME=%s ARGS=%s\\n' \"$CODEX_HOME\" \"$*\" >> {log}\n\
             case \"$*\" in\n\
             \"app-server daemon version\")\n\
             \x20 if [ -e {socket} ]; then\n\
             \x20   printf '%s\\n' {report}\n\
             \x20   exit 0\n\
             \x20 fi\n\
             \x20 echo \"Error: failed to connect to {socket}\" >&2\n\
             \x20 exit 1\n\
             \x20 ;;\n\
             \"app-server daemon start\")\n\
             {start}\
             \x20 exit 0\n\
             \x20 ;;\n\
             *)\n\
             \x20 echo \"fake codex: unexpected command: $*\" >&2\n\
             \x20 exit 64\n\
             \x20 ;;\n\
             esac\n",
            log = shell_single_quote(&log.display().to_string()),
            socket = shell_single_quote(&control_socket.display().to_string()),
            report = shell_single_quote(&report),
        );
        std::fs::write(&binary, script).expect("write the fake codex CLI");
        std::fs::set_permissions(&binary, std::fs::Permissions::from_mode(0o755))
            .expect("make the fake codex CLI executable");
        Self { binary, log }
    }

    /// Every invocation, one `CODEX_HOME=… ARGS=…` line each.
    pub(crate) fn invocations(&self) -> Vec<String> {
        std::fs::read_to_string(&self.log)
            .unwrap_or_default()
            .lines()
            .map(str::to_string)
            .collect()
    }

    /// How many times `app-server daemon start` was run.
    pub(crate) fn starts(&self) -> usize {
        self.invocations()
            .iter()
            .filter(|line| line.ends_with("ARGS=app-server daemon start"))
            .count()
    }
}

/// The state one fake server exposes to its test.
#[derive(Default)]
struct State {
    user_agent: String,
    reject_initialize: Option<String>,
    connections: AtomicUsize,
    initialized: AtomicUsize,
}

pub(crate) struct FakeServer {
    state: Arc<State>,
}

impl FakeServer {
    /// Serve a unix socket at `path`. The caller may point a symlink at it —
    /// that is the shipped control-socket shape. Unix-only, like the
    /// transport it exercises.
    #[cfg(unix)]
    pub(crate) async fn unix(path: &Path, user_agent: &str) -> Self {
        let listener = UnixListener::bind(path).expect("bind fake unix app-server");
        let state = Arc::new(State {
            user_agent: user_agent.to_string(),
            ..State::default()
        });
        let serving = Arc::clone(&state);
        tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                accept_connection(stream, Arc::clone(&serving));
            }
        });
        Self { state }
    }

    /// Serve a loopback TCP socket; returns the server and its `ws://` URL.
    pub(crate) async fn tcp(user_agent: &str) -> (Self, String) {
        Self::tcp_with(user_agent, None).await
    }

    /// Serve a loopback TCP socket that answers `initialize` with a JSON-RPC
    /// error, for the handshake's failure path.
    pub(crate) async fn tcp_rejecting(message: &str) -> (Self, String) {
        Self::tcp_with("", Some(message)).await
    }

    async fn tcp_with(user_agent: &str, reject_initialize: Option<&str>) -> (Self, String) {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind fake app-server");
        let address = listener.local_addr().expect("fake app-server address");
        let state = Arc::new(State {
            user_agent: user_agent.to_string(),
            reject_initialize: reject_initialize.map(str::to_string),
            ..State::default()
        });
        let serving = Arc::clone(&state);
        tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                accept_connection(stream, Arc::clone(&serving));
            }
        });
        (Self { state }, format!("ws://{address}"))
    }

    /// Connections accepted so far (one per client handshake).
    pub(crate) fn connections(&self) -> usize {
        self.state.connections.load(Ordering::SeqCst)
    }

    /// `initialized` notifications received so far.
    pub(crate) fn initialized(&self) -> usize {
        self.state.initialized.load(Ordering::SeqCst)
    }

    /// Wait until the server task has observed `expected` `initialized`
    /// notifications; the client's send is asynchronous, so asserting in the
    /// same tick would race the fake server's read.
    pub(crate) async fn wait_for_initialized(&self, expected: usize) {
        wait_until(|| self.initialized(), expected, "initialized notifications").await;
    }

    /// Wait until `expected` connections have been accepted.
    pub(crate) async fn wait_for_connections(&self, expected: usize) {
        wait_until(|| self.connections(), expected, "connections").await;
    }
}

/// Count one accepted connection and serve it on its own task. The accept loop
/// is the only platform-specific part of a fake server; this half is shared by
/// the TCP and unix listeners.
fn accept_connection<S>(stream: S, state: Arc<State>)
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    state.connections.fetch_add(1, Ordering::SeqCst);
    tokio::spawn(async move { serve(stream, state).await });
}

async fn wait_until(count: impl Fn() -> usize, expected: usize, what: &str) {
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(2);
    while count() < expected {
        assert!(
            tokio::time::Instant::now() < deadline,
            "timed out waiting for {expected} {what}"
        );
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
}

async fn serve<S>(stream: S, state: Arc<State>)
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let Ok(mut socket) = tokio_tungstenite::accept_async(stream).await else {
        return;
    };
    while let Some(Ok(message)) = socket.next().await {
        let Message::Text(text) = message else {
            continue;
        };
        let Ok(value) = serde_json::from_str::<Value>(&text) else {
            continue;
        };
        match value.get("method").and_then(Value::as_str) {
            Some("initialize") => {
                if let Some(reject) = &state.reject_initialize {
                    let reply = json!({
                        "jsonrpc": "2.0",
                        "id": value.get("id").cloned().unwrap_or(Value::Null),
                        "error": { "code": -32600, "message": reject },
                    });
                    let _ = socket.send(Message::Text(reply.to_string().into())).await;
                    continue;
                }
                // A broadcast that races the response: the client's
                // correlation loop must skip it, never read it as the reply.
                let _ = socket
                    .send(Message::Text(
                        json!({
                            "method": "remoteControl/status/changed",
                            "params": { "status": "disabled" },
                        })
                        .to_string()
                        .into(),
                    ))
                    .await;
                let reply = json!({
                    "jsonrpc": "2.0",
                    "id": value.get("id").cloned().unwrap_or(Value::Null),
                    "result": {
                        "userAgent": state.user_agent,
                        "codexHome": "/tmp/codex-home",
                        "platformFamily": "unix",
                        "platformOs": "linux",
                    },
                });
                let _ = socket.send(Message::Text(reply.to_string().into())).await;
            }
            Some("initialized") => {
                state.initialized.fetch_add(1, Ordering::SeqCst);
            }
            _ => {}
        }
    }
}
