//! The app-server transport: JSON-RPC 2.0 over one WebSocket, one message per
//! text frame (spec #860, ticket #876).
//!
//! `codex app-server --listen` takes `ws://IP:PORT` or `unix://PATH` (stdio is
//! the remaining form and is not used: a peer attachment cannot own the
//! agent's stdin/stdout). Both are the same WebSocket protocol, so the only
//! difference is the byte stream underneath — a TCP socket or a unix socket.
//! `client_async` over a boxed `AsyncRead + AsyncWrite` keeps one client type
//! for both.
//!
//! The shipped control socket under `$CODEX_HOME/app-server-control/` is a
//! **symlink** into `/tmp/codex-daemon-0/<hash>` (ADR-0090's amendment,
//! question 4). The path is used verbatim and never canonicalised: the OS
//! follows the link at connect time, so a daemon that re-points it is picked
//! up on the next attach.

use std::path::PathBuf;
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::{TcpStream, UnixStream};
use tokio_tungstenite::WebSocketStream;
use tokio_tungstenite::tungstenite::Message;
use url::Url;

use super::error::{CodexError, Result};

/// How to reach an app-server.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Endpoint {
    /// `ws://HOST:PORT` — the loopback listener Codex starts for a TUI.
    WebSocket(String),
    /// `unix://PATH` — a filesystem socket, usually the daemon control socket
    /// (a symlink; followed by the OS, not by string canonicalisation).
    Unix(PathBuf),
}

impl Endpoint {
    /// Parse the form `codex app-server --listen` prints. `stdio://` is
    /// rejected with the endpoint named so a misconfiguration is obvious.
    pub(crate) fn parse(raw: &str) -> Result<Self> {
        if let Some(path) = raw.strip_prefix("unix://") {
            if path.is_empty() {
                return Err(CodexError::Endpoint(raw.to_string()));
            }
            return Ok(Self::Unix(PathBuf::from(path)));
        }
        if raw.starts_with("ws://") {
            return Ok(Self::WebSocket(raw.to_string()));
        }
        Err(CodexError::Endpoint(raw.to_string()))
    }

    /// Human-readable form for errors and logs.
    pub(crate) fn describe(&self) -> String {
        match self {
            Self::WebSocket(url) => url.clone(),
            Self::Unix(path) => format!("unix://{}", path.display()),
        }
    }
}

/// The one byte stream a `WebSocketStream` is built over.
trait ByteStream: AsyncRead + AsyncWrite + Unpin + Send {}
impl<T: AsyncRead + AsyncWrite + Unpin + Send> ByteStream for T {}

type BoxedStream = Box<dyn ByteStream>;

/// The HTTP request line the WebSocket handshake uses over a unix socket: the
/// daemon serves one endpoint and ignores the host, but the handshake still
/// needs a valid request target.
const UNIX_HANDSHAKE_URL: &str = "ws://localhost/";

/// One live app-server connection. The handshake and the contract check are
/// this ticket's whole use; later tickets add the read/subscription loop on
/// top of {@link request} and {@link notify}.
pub(crate) struct RpcConnection {
    socket: WebSocketStream<BoxedStream>,
    next_id: u64,
}

impl RpcConnection {
    /// Dial the endpoint and complete the WebSocket handshake, bounded by
    /// `timeout` so a wedged server cannot hold cola's boot.
    pub(crate) async fn open(endpoint: &Endpoint, timeout: Duration) -> Result<Self> {
        match tokio::time::timeout(timeout, Self::open_inner(endpoint)).await {
            Err(_) => Err(CodexError::Timeout(timeout)),
            Ok(result) => result,
        }
    }

    async fn open_inner(endpoint: &Endpoint) -> Result<Self> {
        match endpoint {
            Endpoint::Unix(path) => {
                let stream = UnixStream::connect(path)
                    .await
                    .map_err(|error| CodexError::Transport {
                        context: format!("connect to unix socket {}", path.display()),
                        detail: error.to_string(),
                    })?;
                let socket = Self::handshake(UNIX_HANDSHAKE_URL, Box::new(stream)).await?;
                Ok(Self { socket, next_id: 1 })
            }
            Endpoint::WebSocket(raw) => {
                let url = Url::parse(raw).map_err(|error| CodexError::Transport {
                    context: format!("parse endpoint {raw}"),
                    detail: error.to_string(),
                })?;
                let host = url.host_str().ok_or_else(|| CodexError::Transport {
                    context: format!("parse endpoint {raw}"),
                    detail: "no host".to_string(),
                })?;
                let port = url.port_or_known_default().unwrap_or(80);
                let stream =
                    TcpStream::connect((host, port))
                        .await
                        .map_err(|error| CodexError::Transport {
                            context: format!("connect to {raw}"),
                            detail: error.to_string(),
                        })?;
                let socket = Self::handshake(raw, Box::new(stream)).await?;
                Ok(Self { socket, next_id: 1 })
            }
        }
    }

    async fn handshake(url: &str, stream: BoxedStream) -> Result<WebSocketStream<BoxedStream>> {
        let (socket, _) = tokio_tungstenite::client_async(url, stream)
            .await
            .map_err(|error| CodexError::Transport {
                context: format!("WebSocket handshake with {url}"),
                detail: error.to_string(),
            })?;
        Ok(socket)
    }

    /// Send one JSON-RPC request and wait for the response with its id,
    /// skipping server notifications and server-initiated requests (later
    /// tickets own their dispatch; the handshake must not choke on a broadcast
    /// that races it).
    pub(crate) async fn request(&mut self, method: &str, params: Value) -> Result<Value> {
        let id = self.next_id;
        self.next_id += 1;
        self.send(&json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        }))
        .await?;

        loop {
            let message = self
                .next_message()
                .await?
                .ok_or_else(|| CodexError::NoResponse(method.to_string()))?;
            if message.get("id").and_then(Value::as_u64) != Some(id) {
                // A notification (no id) or another request's traffic; the
                // correlation id is the only contract this layer needs.
                continue;
            }
            if let Some(error) = message.get("error") {
                let detail = error
                    .get("message")
                    .and_then(Value::as_str)
                    .map(str::to_string)
                    .unwrap_or_else(|| error.to_string());
                return Err(CodexError::Rpc {
                    method: method.to_string(),
                    detail,
                });
            }
            return Ok(message.get("result").cloned().unwrap_or(Value::Null));
        }
    }

    /// Send one JSON-RPC notification (no id, no response).
    pub(crate) async fn notify(&mut self, method: &str, params: Option<Value>) -> Result<()> {
        let mut message = json!({ "jsonrpc": "2.0", "method": method });
        if let Some(params) = params {
            message["params"] = params;
        }
        self.send(&message).await
    }

    async fn send(&mut self, message: &Value) -> Result<()> {
        let text = serde_json::to_string(message).map_err(|error| CodexError::Transport {
            context: "encode a JSON-RPC frame".to_string(),
            detail: error.to_string(),
        })?;
        self.socket
            .send(Message::Text(text.into()))
            .await
            .map_err(|error| CodexError::Transport {
                context: "send a JSON-RPC frame".to_string(),
                detail: error.to_string(),
            })
    }

    /// The next JSON value on a text frame; `None` when the peer closed.
    /// Binary frames are not part of this protocol and are ignored, as are
    /// control frames the WebSocket layer owns.
    async fn next_message(&mut self) -> Result<Option<Value>> {
        loop {
            match self.socket.next().await {
                None => return Ok(None),
                Some(Err(error)) => {
                    return Err(CodexError::Transport {
                        context: "read a JSON-RPC frame".to_string(),
                        detail: error.to_string(),
                    });
                }
                Some(Ok(Message::Text(text))) => {
                    let value: Value =
                        serde_json::from_str(&text).map_err(|error| CodexError::Transport {
                            context: "decode a JSON-RPC frame".to_string(),
                            detail: error.to_string(),
                        })?;
                    return Ok(Some(value));
                }
                Some(Ok(Message::Ping(payload))) => {
                    self.socket.send(Message::Pong(payload)).await.map_err(|error| {
                        CodexError::Transport {
                            context: "answer a WebSocket ping".to_string(),
                            detail: error.to_string(),
                        }
                    })?;
                }
                Some(Ok(Message::Close(_))) => return Ok(None),
                Some(Ok(_)) => continue,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use serde_json::json;

    use super::*;
    use crate::codex::test_server::FakeServer;

    #[test]
    fn endpoints_parse_the_listen_forms() {
        assert_eq!(
            Endpoint::parse("ws://127.0.0.1:4501").unwrap(),
            Endpoint::WebSocket("ws://127.0.0.1:4501".to_string())
        );
        assert_eq!(
            Endpoint::parse("unix:///tmp/control.sock").unwrap(),
            Endpoint::Unix(PathBuf::from("/tmp/control.sock"))
        );
        assert!(Endpoint::parse("stdio://").is_err());
        assert!(Endpoint::parse("unix://").is_err());
        assert!(Endpoint::parse("http://127.0.0.1:4501").is_err());
    }

    /// The shipped control socket is a symlink (ADR-0090 amendment, question
    /// 4). A real symlink to a real unix-socket server must connect: the path
    /// is handed to the OS, never canonicalised first.
    #[tokio::test]
    async fn the_unix_transport_follows_a_symlinked_socket() {
        let dir = tempfile::tempdir().expect("tempdir");
        let real = dir.path().join("app-server.sock");
        let link = dir.path().join("app-server-control.sock");
        let server = FakeServer::unix(&real, "codex-tui/0.163.0 (test)").await;
        std::os::unix::fs::symlink(&real, &link).expect("symlink the control socket");

        let endpoint = Endpoint::parse(&format!("unix://{}", link.display())).expect("endpoint");
        let mut connection = RpcConnection::open(&endpoint, Duration::from_secs(5))
            .await
            .expect("connect through the symlink");

        let result = connection
            .request(
                "initialize",
                json!({ "clientInfo": { "name": "cola", "version": "test" } }),
            )
            .await
            .expect("initialize");
        assert_eq!(result["userAgent"], "codex-tui/0.163.0 (test)");
        connection.notify("initialized", None).await.expect("initialized");
        server.wait_for_initialized(1).await;
    }

    /// Two clients attach independently: a second connection's handshake does
    /// not disturb the first, and each gets its own response.
    #[tokio::test]
    async fn a_second_connection_opens_independently() {
        let (server, url) = FakeServer::tcp("codex-tui/0.163.0 (test)").await;
        let endpoint = Endpoint::parse(&url).expect("endpoint");

        let mut first = RpcConnection::open(&endpoint, Duration::from_secs(5))
            .await
            .expect("first connection");
        let first_result = first
            .request(
                "initialize",
                json!({ "clientInfo": { "name": "cola", "version": "test" } }),
            )
            .await
            .expect("first initialize");
        first
            .notify("initialized", None)
            .await
            .expect("first initialized");

        let mut second = RpcConnection::open(&endpoint, Duration::from_secs(5))
            .await
            .expect("second connection");
        let second_result = second
            .request(
                "initialize",
                json!({ "clientInfo": { "name": "cola", "version": "test" } }),
            )
            .await
            .expect("second initialize");
        second
            .notify("initialized", None)
            .await
            .expect("second initialized");

        assert_eq!(first_result["userAgent"], second_result["userAgent"]);
        server.wait_for_connections(2).await;
        server.wait_for_initialized(2).await;
        assert_eq!(server.connections(), 2);
        assert_eq!(server.initialized(), 2);
    }

    /// A broadcast notification that races the response is skipped, not
    /// mistaken for the reply.
    #[tokio::test]
    async fn a_racing_notification_is_skipped() {
        let (_, url) = FakeServer::tcp("codex-tui/0.163.0 (test)").await;
        let endpoint = Endpoint::parse(&url).expect("endpoint");
        let mut connection = RpcConnection::open(&endpoint, Duration::from_secs(5))
            .await
            .expect("connection");
        let result = connection
            .request(
                "initialize",
                json!({ "clientInfo": { "name": "cola", "version": "test" } }),
            )
            .await
            .expect("initialize");
        assert_eq!(result["userAgent"], "codex-tui/0.163.0 (test)");
    }
}
