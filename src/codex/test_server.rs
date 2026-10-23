//! A scripted `codex app-server` for the transport tests (spec #860, ticket
//! #876).
//!
//! It is a real WebSocket server on a real socket — TCP or unix — so the
//! production client (handshake, framing, response correlation) runs against
//! the wire rather than a mock. It answers `initialize` with the fields the
//! pinned protocol requires and counts `initialized` notifications. Compiled
//! only for tests.

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::{TcpListener, UnixListener};
use tokio_tungstenite::tungstenite::Message;

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
    /// that is the shipped control-socket shape.
    pub(crate) async fn unix(path: &Path, user_agent: &str) -> Self {
        let listener = UnixListener::bind(path).expect("bind fake unix app-server");
        let state = Arc::new(State {
            user_agent: user_agent.to_string(),
            ..State::default()
        });
        let serving = Arc::clone(&state);
        tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                serving.connections.fetch_add(1, Ordering::SeqCst);
                let state = Arc::clone(&serving);
                tokio::spawn(async move { serve(stream, state).await });
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
                serving.connections.fetch_add(1, Ordering::SeqCst);
                let state = Arc::clone(&serving);
                tokio::spawn(async move { serve(stream, state).await });
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
