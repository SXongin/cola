//! The app-server transport: JSON-RPC 2.0 over one WebSocket, one message per
//! text frame (spec #860, tickets #876/#878).
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
//!
//! # One reader loop, fanned out
//!
//! app-server is push-shaped: a subscribed thread delivers `item/*` and
//! `turn/*` notifications on the connection that resumed it (ADR-0090's
//! amendment), and those arrive *while* ordinary requests are in flight. A
//! synchronous "read until my id" loop would discard exactly that traffic, so
//! the socket is split after the handshake:
//!
//! - a **writer** task owns the sink and serialises every outgoing frame;
//! - a **reader** task owns the stream and fans each frame out: a response goes
//!   to the one waiter registered for its id, a notification or server request
//!   goes to [`RpcConnection::inbound`]'s broadcast.
//!
//! The connection is therefore safe to share: [`RpcConnection`] is a cheap
//! handle over the tasks (`Clone`), and the tasks stop when the last handle is
//! dropped. Correlation, framing and push traffic stay in this layer; which
//! notification belongs to which thread is the adapter's business.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::time::Duration;

use futures_util::stream::SplitStream;
use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::TcpStream;
#[cfg(unix)]
use tokio::net::UnixStream;
use tokio::sync::{broadcast, mpsc, oneshot, watch};
use tokio_tungstenite::WebSocketStream;
use tokio_tungstenite::tungstenite::Message;
use url::Url;

use super::error::{CodexError, Result};

/// How many inbound notifications/requests a subscriber may fall behind before
/// the broadcast drops the oldest. A push-shaped runtime can outpace a slow
/// consumer; the adapter sees [`broadcast::error::RecvError::Lagged`] and
/// reports it instead of silently rendering a hole.
const INBOUND_CAPACITY: usize = 1024;

/// How many outgoing frames may sit between a caller and the writer. Bounded on
/// purpose: a peer that stops reading must apply backpressure to the callers
/// (whose own timeouts — the handshake's, the liveness probe's — bound them)
/// instead of letting an unbounded queue grow. A closed socket fails the send,
/// which closes the connection and fails every waiter.
const OUTGOING_CAPACITY: usize = 64;

/// How to reach an app-server.
// `ws://` endpoints are how a TUI's own listener is addressed (and how every
// hermetic test dials the fake server); the daemon path uses `Unix`.
#[allow(dead_code)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Endpoint {
    /// `ws://HOST:PORT` — the loopback listener Codex starts for a TUI.
    WebSocket(String),
    /// `unix://PATH` — a filesystem socket, usually the daemon control socket
    /// (a symlink; followed by the OS, not by string canonicalisation). The
    /// variant parses on every platform; connecting to it is unix-only.
    Unix(PathBuf),
}

impl Endpoint {
    /// Parse the form `codex app-server --listen` prints. `stdio://` is
    /// rejected with the endpoint named so a misconfiguration is obvious.
    // Used by the live suite and every test endpoint; the daemon builds `Unix`
    // directly.
    #[allow(dead_code)]
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

    /// The neutral base-URL spelling of this endpoint (`unix://PATH` or
    /// `ws://…`), for `Backend::base_url`.
    pub(crate) fn display_url(&self) -> String {
        match self {
            Self::WebSocket(url) => url.clone(),
            Self::Unix(path) => format!("unix://{}", path.display()),
        }
    }
}

/// One message this connection did not request: a server notification (no id)
/// or a server-initiated request (an id the server owns).
///
/// Both are fanned out here, not dispatched: notifications feed the adapter's
/// thread subscriptions (#878), and server requests are the approval/question
/// surface the adapter records and answers with [`RpcConnection::respond`]
/// (#882).
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Inbound {
    Notification {
        /// The order this connection received the notification, assigned in
        /// the reader loop. Two independent consumers (the thread dispatcher
        /// and the status watcher) need one receipt order to resolve a stale
        /// event against a later history page.
        seq: u64,
        method: String,
        params: Value,
    },
    Request {
        id: Value,
        method: String,
        params: Value,
    },
    /// The socket is gone: no more responses or push traffic will arrive, and
    /// every subscription's stream ends. Sent once, by whichever task noticed
    /// first.
    Closed,
}

/// The one byte stream a `WebSocketStream` is built over.
trait ByteStream: AsyncRead + AsyncWrite + Unpin + Send {}
impl<T: AsyncRead + AsyncWrite + Unpin + Send> ByteStream for T {}

type BoxedStream = Box<dyn ByteStream>;
type Socket = WebSocketStream<BoxedStream>;

/// The HTTP request line the WebSocket handshake uses over a unix socket: the
/// daemon serves one endpoint and ignores the host, but the handshake still
/// needs a valid request target.
#[cfg(unix)]
const UNIX_HANDSHAKE_URL: &str = "ws://localhost/";

/// One live app-server connection, shared by the handshake, the lifecycle's
/// liveness probe and the thread adapter (ticket #878). Cloning is cheap: every
/// clone addresses the same socket and the same reader/writer tasks, and the
/// socket closes when the last one is dropped.
#[derive(Clone)]
pub(crate) struct RpcConnection {
    inner: Arc<ConnectionInner>,
}

/// Why a waiter's request ended without a result.
enum RpcFailure {
    /// The server answered with a JSON-RPC `error` member.
    Protocol { code: Option<i64>, detail: String },
    /// The connection ended before a response arrived.
    Closed,
}

struct Pending {
    reply: oneshot::Sender<std::result::Result<Value, RpcFailure>>,
}

/// Removes one request's pending entry on **every** exit path — including a
/// caller cancelling the future while the send is blocked on a full queue or
/// while the response is awaited. Without it the map would grow with every
/// cancelled call even though the frame queue is bounded.
struct PendingEntry {
    inner: Arc<ConnectionInner>,
    id: u64,
}

impl Drop for PendingEntry {
    fn drop(&mut self) {
        self.inner
            .pending
            .lock()
            .expect("pending requests")
            .remove(&self.id);
    }
}

struct ConnectionInner {
    outgoing: mpsc::Sender<Message>,
    next_id: AtomicU64,
    pending: Mutex<HashMap<u64, Pending>>,
    inbound: broadcast::Sender<Inbound>,
    /// The receipt order of inbound notifications. Assigned in the reader loop
    /// (before fan-out), so every consumer sees one order even when two tasks
    /// process the same broadcast at different speeds.
    inbound_seq: AtomicU64,
    /// Set by whichever task first sees the end. A request that arrives after
    /// this fails immediately instead of waiting on a response that cannot
    /// come.
    closed: std::sync::atomic::AtomicBool,
    /// Ends the writer when the connection closes. A watch (state-bearing, not
    /// a one-shot notify) so a close that races the writer's select cannot be
    /// missed: the writer sees the value even when it checks after the send.
    closing: watch::Sender<bool>,
    tasks: Mutex<Vec<tokio::task::JoinHandle<()>>>,
}

impl Drop for ConnectionInner {
    fn drop(&mut self) {
        // The tasks capture a `Weak` (the reader) or only a channel (the
        // writer), so they cannot keep this alive; aborting them here closes
        // the socket promptly instead of waiting for the peer's own close.
        let tasks = self.tasks.get_mut().expect("connection tasks");
        for task in tasks.drain(..) {
            task.abort();
        }
    }
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
        let socket = match endpoint {
            Endpoint::Unix(path) => {
                #[cfg(unix)]
                {
                    let stream = UnixStream::connect(path)
                        .await
                        .map_err(|error| CodexError::Transport {
                            context: format!("connect to unix socket {}", path.display()),
                            detail: error.to_string(),
                        })?;
                    Self::handshake(UNIX_HANDSHAKE_URL, Box::new(stream)).await?
                }
                // A unix-domain socket is not part of this platform's socket
                // support. Say so by name; the caller can still use `ws://`.
                #[cfg(not(unix))]
                {
                    return Err(CodexError::UnsupportedPlatform {
                        path: path.display().to_string(),
                        platform: std::env::consts::OS,
                    });
                }
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
                Self::handshake(raw, Box::new(stream)).await?
            }
        };

        Ok(Self::start(socket))
    }

    async fn handshake(url: &str, stream: BoxedStream) -> Result<Socket> {
        let (socket, _) = tokio_tungstenite::client_async(url, stream)
            .await
            .map_err(|error| CodexError::Transport {
                context: format!("WebSocket handshake with {url}"),
                detail: error.to_string(),
            })?;
        Ok(socket)
    }

    /// Split the socket and start the writer and reader tasks.
    fn start(socket: Socket) -> Self {
        let (sink, stream) = socket.split();
        let (outgoing, outgoing_rx) = mpsc::channel(OUTGOING_CAPACITY);
        let (inbound, _) = broadcast::channel(INBOUND_CAPACITY);
        let (closing, closing_rx) = watch::channel(false);

        let inner = Arc::new(ConnectionInner {
            outgoing,
            next_id: AtomicU64::new(1),
            pending: Mutex::new(HashMap::new()),
            inbound,
            inbound_seq: AtomicU64::new(0),
            closed: std::sync::atomic::AtomicBool::new(false),
            closing,
            tasks: Mutex::new(Vec::new()),
        });

        let writer = tokio::spawn(write_loop(sink, outgoing_rx, closing_rx, Arc::downgrade(&inner)));
        let reader = tokio::spawn(read_loop(stream, Arc::downgrade(&inner)));
        inner
            .tasks
            .lock()
            .expect("connection tasks")
            .extend([writer, reader]);

        Self { inner }
    }

    /// Send one JSON-RPC request and wait for the response with its id. Other
    /// traffic is not read here: notifications and server requests go to
    /// [`Self::inbound`], so a subscription's stream survives a request.
    pub(crate) async fn request(&self, method: &str, params: Value) -> Result<Value> {
        if self.closed() {
            return Err(CodexError::NoResponse(method.to_string()));
        }
        let id = self.inner.next_id.fetch_add(1, Ordering::SeqCst);
        let (reply, response) = oneshot::channel();
        self.inner
            .pending
            .lock()
            .expect("pending requests")
            .insert(id, Pending { reply });
        // Armed immediately: cancellation anywhere below (blocked on a full
        // queue, waiting for the response) must not leave this entry behind.
        let _pending = PendingEntry {
            inner: Arc::clone(&self.inner),
            id,
        };

        let frame = json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        });
        self.send(&frame).await?;
        // The connection may have ended between the check above and the send;
        // the close drains whatever is pending at that moment, and `_pending`
        // covers the remaining window.
        if self.closed() {
            return Err(CodexError::NoResponse(method.to_string()));
        }

        match response.await {
            Ok(Ok(value)) => Ok(value),
            Ok(Err(RpcFailure::Protocol { code, detail })) => Err(CodexError::Rpc {
                method: method.to_string(),
                code,
                detail,
            }),
            Ok(Err(RpcFailure::Closed)) | Err(_) => Err(CodexError::NoResponse(method.to_string())),
        }
    }

    /// Send one JSON-RPC notification (no id, no response).
    pub(crate) async fn notify(&self, method: &str, params: Option<Value>) -> Result<()> {
        if self.closed() {
            return Err(CodexError::NoResponse(method.to_string()));
        }
        let mut message = json!({ "jsonrpc": "2.0", "method": method });
        if let Some(params) = params {
            message["params"] = params;
        }
        self.send(&message).await
    }

    /// Answer one server-initiated request, addressed by the id the server
    /// chose. This is the response half of [`Inbound::Request`]: approvals and
    /// questions are not client calls, so they are answered on this same
    /// serialised writer rather than through [`Self::request`]. A closed
    /// connection refuses the frame by name, exactly like a call.
    pub(crate) async fn respond(&self, id: &Value, result: Value) -> Result<()> {
        if self.closed() {
            return Err(CodexError::NoResponse(format!("server request {id}")));
        }
        self.send(&json!({
            "jsonrpc": "2.0",
            "id": id,
            "result": result,
        }))
        .await
    }

    /// Refuse one server-initiated request by name. Used where cola cannot
    /// express an answer at all (a request kind it does not implement): the
    /// server then sees a refusal instead of waiting forever, and the failure
    /// is never guessed at.
    pub(crate) async fn respond_error(&self, id: &Value, code: i64, message: &str) -> Result<()> {
        if self.closed() {
            return Err(CodexError::NoResponse(format!("server request {id}")));
        }
        self.send(&json!({
            "jsonrpc": "2.0",
            "id": id,
            "error": { "code": code, "message": message },
        }))
        .await
    }

    /// Subscribe to everything the server pushed that was not a response to
    /// one of this connection's requests.
    pub(crate) fn inbound(&self) -> broadcast::Receiver<Inbound> {
        self.inner.inbound.subscribe()
    }

    /// The receipt sequence of the newest inbound notification fanned out by
    /// this connection. A history page read now reflects every status received
    /// at or below this value, so a status carrying it or an older one is
    /// stale against the page.
    pub(crate) fn inbound_seq(&self) -> u64 {
        self.inner.inbound_seq.load(Ordering::SeqCst)
    }

    fn closed(&self) -> bool {
        self.inner.closed.load(Ordering::SeqCst)
    }

    /// A connection with no socket and no writer task: the caller owns the
    /// frame receiver, so it can hold the queue full and observe blocked
    /// senders. Test-only.
    #[cfg(test)]
    fn test_connection(capacity: usize) -> (Self, mpsc::Receiver<Message>, watch::Receiver<bool>) {
        let (outgoing, outgoing_rx) = mpsc::channel(capacity);
        let (inbound, _) = broadcast::channel(INBOUND_CAPACITY);
        let (closing, closing_rx) = watch::channel(false);
        let inner = Arc::new(ConnectionInner {
            outgoing,
            next_id: AtomicU64::new(1),
            pending: Mutex::new(HashMap::new()),
            inbound,
            inbound_seq: AtomicU64::new(0),
            closed: std::sync::atomic::AtomicBool::new(false),
            closing,
            tasks: Mutex::new(Vec::new()),
        });
        (Self { inner }, outgoing_rx, closing_rx)
    }

    #[cfg(test)]
    fn pending_len(&self) -> usize {
        self.inner.pending.lock().expect("pending requests").len()
    }

    async fn send(&self, message: &Value) -> Result<()> {
        let text = serde_json::to_string(message).map_err(|error| CodexError::Transport {
            context: "encode a JSON-RPC frame".to_string(),
            detail: error.to_string(),
        })?;
        self.inner
            .outgoing
            .send(Message::Text(text.into()))
            .await
            .map_err(|_| CodexError::NoResponse("the connection writer".to_string()))
    }
}

/// Serialise every outgoing frame; the sink is the only writer.
///
/// Selects on the connection's close signal in both states — waiting for a
/// frame and sending one — so a close ends the writer even while it is wedged
/// on a peer that stopped reading. Dropping the receiver before returning is
/// what wakes callers blocked in `send()` on the bounded queue: they see a
/// send error and fail their request instead of waiting for a writer that no
/// longer exists.
async fn write_loop<S>(
    mut sink: S,
    mut outgoing: mpsc::Receiver<Message>,
    mut closing: watch::Receiver<bool>,
    inner: Weak<ConnectionInner>,
) where
    S: futures_util::Sink<Message> + Unpin,
    S::Error: std::fmt::Debug,
{
    let mut close_while_sending = closing.clone();
    loop {
        tokio::select! {
            biased;
            _ = closing.changed() => break,
            message = outgoing.recv() => {
                let Some(message) = message else { break };
                tokio::select! {
                    biased;
                    _ = close_while_sending.changed() => break,
                    result = sink.send(message) => {
                        if result.is_err() {
                            break;
                        }
                    }
                }
            }
        }
    }
    // Wake every caller blocked on the queue's capacity *before* the graceful
    // close: on a teardown a partial frame does not matter, and a caller must
    // never wait for a writer that is exiting.
    drop(outgoing);
    if !*closing.borrow() {
        // Dropping the sink completes the WebSocket close handshake as far as
        // this half can; the reader sees the peer's close next.
        let _ = sink.close().await;
    }
    if let Some(inner) = inner.upgrade() {
        close(&inner);
    }
}

/// Read every frame and fan it out. A frame with a `method` is push traffic;
/// a frame with only an `id` is a response to one of this connection's calls.
async fn read_loop(mut stream: SplitStream<Socket>, inner: Weak<ConnectionInner>) {
    while let Some(message) = stream.next().await {
        let Some(inner) = inner.upgrade() else {
            return;
        };
        match message {
            Ok(Message::Text(text)) => match serde_json::from_str::<Value>(&text) {
                Ok(value) => dispatch(&inner, value),
                Err(error) => {
                    // A frame that is not JSON cannot be correlated or routed.
                    // If it was a response, its waiter would otherwise hang
                    // forever (thread calls carry no per-call timeout), and a
                    // stream with unknown gaps cannot be trusted — so the
                    // connection is closed and every waiter fails by name.
                    tracing::warn!(
                        "codex app-server sent a non-JSON text frame; closing the connection: {error}"
                    );
                    close(&inner);
                    break;
                }
            },
            Ok(Message::Ping(payload)) => {
                // A ping answer must not stall the reader behind a busy writer;
                // the WebSocket layer can retry or the peer can close.
                let _ = inner.outgoing.try_send(Message::Pong(payload));
            }
            Ok(Message::Close(_)) => break,
            Ok(_) => continue,
            Err(error) => {
                tracing::debug!("codex app-server connection read failed: {error}");
                break;
            }
        }
    }

    // The peer closed or the socket failed: every request still waiting can
    // never be answered on this connection.
    if let Some(inner) = inner.upgrade() {
        close(&inner);
    }
}

/// Mark the connection closed exactly once: fail every waiter and tell the
/// subscribers. No response or push frame can follow.
fn close(inner: &ConnectionInner) {
    if inner.closed.swap(true, Ordering::SeqCst) {
        return;
    }
    // End the writer first: it drops the frame receiver, which wakes callers
    // blocked on the bounded queue (their send fails instead of hanging).
    let _ = inner.closing.send(true);
    let pending = std::mem::take(&mut *inner.pending.lock().expect("pending requests"));
    for (_, pending) in pending {
        let _ = pending.reply.send(Err(RpcFailure::Closed));
    }
    let _ = inner.inbound.send(Inbound::Closed);
}

/// Route one inbound value.
fn dispatch(inner: &Arc<ConnectionInner>, value: Value) {
    let method = value.get("method").and_then(Value::as_str);
    let id = value.get("id").cloned();
    match (method, id) {
        (Some(method), Some(id)) => {
            let params = value.get("params").cloned().unwrap_or(Value::Null);
            let _ = inner.inbound.send(Inbound::Request {
                id,
                method: method.to_string(),
                params,
            });
        }
        (Some(method), None) => {
            let params = value.get("params").cloned().unwrap_or(Value::Null);
            let seq = inner.inbound_seq.fetch_add(1, Ordering::SeqCst) + 1;
            let _ = inner.inbound.send(Inbound::Notification {
                seq,
                method: method.to_string(),
                params,
            });
        }
        (None, Some(id)) => {
            let Some(id) = id.as_u64() else {
                tracing::debug!("codex app-server answered with a non-numeric id: {id}");
                return;
            };
            let Some(pending) = inner.pending.lock().expect("pending requests").remove(&id) else {
                // A response to a call this connection no longer waits for
                // (its caller timed out or was cancelled).
                return;
            };
            let result = match value.get("error") {
                Some(error) => Err(RpcFailure::Protocol {
                    code: error.get("code").and_then(Value::as_i64),
                    detail: error
                        .get("message")
                        .and_then(Value::as_str)
                        .map(str::to_string)
                        .unwrap_or_else(|| error.to_string()),
                }),
                None => Ok(value.get("result").cloned().unwrap_or(Value::Null)),
            };
            let _ = pending.reply.send(result);
        }
        (None, None) => {
            tracing::debug!("codex app-server sent a frame that is neither a call nor a response");
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use futures_util::Sink;
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
    #[cfg(unix)]
    #[tokio::test]
    async fn the_unix_transport_follows_a_symlinked_socket() {
        let dir = tempfile::tempdir().expect("tempdir");
        let real = dir.path().join("app-server.sock");
        let link = dir.path().join("app-server-control.sock");
        let server = FakeServer::unix(&real, "codex-tui/0.163.0 (test)").await;
        std::os::unix::fs::symlink(&real, &link).expect("symlink the control socket");

        let endpoint = Endpoint::parse(&format!("unix://{}", link.display())).expect("endpoint");
        let connection = RpcConnection::open(&endpoint, Duration::from_secs(5))
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

        let first = RpcConnection::open(&endpoint, Duration::from_secs(5))
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

        let second = RpcConnection::open(&endpoint, Duration::from_secs(5))
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

    /// A broadcast notification that races the response is skipped by the
    /// correlation loop **and delivered to `inbound()`**. The old synchronous
    /// loop discarded it, so asserting delivery is what makes this test fail
    /// without the fan-out (the scripted server emits this notification before
    /// the `initialize` response).
    #[tokio::test]
    async fn a_racing_notification_is_delivered_not_mistaken_for_the_reply() {
        let (_, url) = FakeServer::tcp("codex-tui/0.163.0 (test)").await;
        let endpoint = Endpoint::parse(&url).expect("endpoint");
        let connection = RpcConnection::open(&endpoint, Duration::from_secs(5))
            .await
            .expect("connection");
        let mut inbound = connection.inbound();

        let result = connection
            .request(
                "initialize",
                json!({ "clientInfo": { "name": "cola", "version": "test" } }),
            )
            .await
            .expect("initialize");
        assert_eq!(result["userAgent"], "codex-tui/0.163.0 (test)");

        let event = tokio::time::timeout(Duration::from_secs(2), inbound.recv())
            .await
            .expect("the racing notification is not lost")
            .expect("inbound stays open");
        match event {
            Inbound::Notification { method, params, .. } => {
                assert_eq!(method, "remoteControl/status/changed");
                assert_eq!(params["status"], "disabled");
            }
            other => panic!("expected the initialize-time broadcast, got {other:?}"),
        }
    }

    /// A frame that is not JSON cannot be correlated: the connection closes and
    /// the pending waiter fails by name instead of hanging forever (thread
    /// calls carry no per-call timeout).
    /// A caller cancelled while its frame waits on the bounded queue must not
    /// leave a pending entry behind; otherwise the map grows without bound even
    /// though the queue does not.
    #[tokio::test]
    async fn a_cancelled_request_removes_its_pending_entry() {
        let (connection, _frames, _closing) = RpcConnection::test_connection(1);
        connection
            .inner
            .outgoing
            .try_send(Message::Text("filler".into()))
            .expect("fill the queue");
        let request = tokio::spawn({
            let connection = connection.clone();
            async move { connection.request("thread/list", json!({})).await }
        });

        // Let the request register its id and block on the full queue.
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert_eq!(connection.pending_len(), 1);
        assert!(!request.is_finished(), "the send is blocked on the full queue");

        request.abort();
        let _ = request.await;
        assert_eq!(
            connection.pending_len(),
            0,
            "cancellation must remove the pending entry"
        );
    }

    /// Ticket #882's transport half: a server-initiated request stays a request
    /// (it is fanned out, never mistaken for a response), and `respond` writes
    /// the answer with the server's own id — the reader keeps serving other
    /// traffic in between.
    #[tokio::test]
    async fn a_server_request_is_answered_with_the_servers_id() {
        let (server, url) = FakeServer::tcp("codex-tui/0.163.0 (test)").await;
        let endpoint = Endpoint::parse(&url).expect("endpoint");
        let connection = RpcConnection::open(&endpoint, Duration::from_secs(5))
            .await
            .expect("connection");
        let mut inbound = connection.inbound();

        let id = server.request_to_all("test/approval", json!({ "threadId": "t" }));
        let event = tokio::time::timeout(Duration::from_secs(2), inbound.recv())
            .await
            .expect("the server request arrives")
            .expect("inbound stays open");
        match event {
            Inbound::Request {
                id: received, method, ..
            } => {
                assert_eq!(received, id);
                assert_eq!(method, "test/approval");
                connection
                    .respond(&received, json!({ "decision": "accept" }))
                    .await
                    .expect("the answer is written");
            }
            other => panic!("expected a server request, got {other:?}"),
        }

        server.wait_for_server_request_replies(1).await;
        let replies = server.server_request_replies();
        assert_eq!(replies[0].id, id);
        assert_eq!(replies[0].result, Some(json!({ "decision": "accept" })));

        // The connection is still usable after answering a server request.
        let result = connection
            .request(
                "initialize",
                json!({ "clientInfo": { "name": "cola", "version": "test" } }),
            )
            .await
            .expect("initialize still answers");
        assert_eq!(result["userAgent"], "codex-tui/0.163.0 (test)");
    }

    /// A close must end the writer and drop the frame receiver, waking a
    /// caller whose `send()` is blocked on a full queue. The stalled sink is
    /// the "peer stopped reading" case.
    #[tokio::test]
    async fn close_releases_a_sender_blocked_on_a_full_queue() {
        let (connection, frames, closing) = RpcConnection::test_connection(1);
        connection
            .inner
            .outgoing
            .try_send(Message::Text("first".into()))
            .expect("fill the queue");

        let started = Arc::new(tokio::sync::Notify::new());
        let writer = tokio::spawn(write_loop(
            StalledSink {
                started: Arc::clone(&started),
            },
            frames,
            closing,
            Arc::downgrade(&connection.inner),
        ));
        // The writer took the first frame and is now wedged in `send`.
        started.notified().await;
        connection
            .inner
            .outgoing
            .try_send(Message::Text("second".into()))
            .expect("fill the queue again");

        let request = tokio::spawn({
            let connection = connection.clone();
            async move { connection.request("thread/list", json!({})).await }
        });
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert_eq!(connection.pending_len(), 1);
        assert!(
            !request.is_finished(),
            "the send is blocked while the writer is stalled"
        );

        close(&connection.inner);

        let result = tokio::time::timeout(Duration::from_secs(2), request)
            .await
            .expect("close releases the blocked sender")
            .expect("the request task did not panic");
        assert!(result.is_err(), "a closed writer fails the send: {result:?}");
        assert_eq!(connection.pending_len(), 0);
        tokio::time::timeout(Duration::from_secs(2), writer)
            .await
            .expect("the writer exits on close")
            .expect("the writer task did not panic");
    }

    /// A sink that accepts no frame: its first `poll_ready` says it started and
    /// then stays pending, exactly a peer that stopped reading.
    struct StalledSink {
        started: Arc<tokio::sync::Notify>,
    }

    impl Sink<Message> for StalledSink {
        type Error = ();

        fn poll_ready(
            self: std::pin::Pin<&mut Self>,
            _context: &mut std::task::Context<'_>,
        ) -> std::task::Poll<std::result::Result<(), Self::Error>> {
            self.started.notify_one();
            std::task::Poll::Pending
        }

        fn start_send(
            self: std::pin::Pin<&mut Self>,
            _message: Message,
        ) -> std::result::Result<(), Self::Error> {
            Ok(())
        }

        fn poll_flush(
            self: std::pin::Pin<&mut Self>,
            _context: &mut std::task::Context<'_>,
        ) -> std::task::Poll<std::result::Result<(), Self::Error>> {
            std::task::Poll::Pending
        }

        fn poll_close(
            self: std::pin::Pin<&mut Self>,
            _context: &mut std::task::Context<'_>,
        ) -> std::task::Poll<std::result::Result<(), Self::Error>> {
            std::task::Poll::Pending
        }
    }

    #[tokio::test]
    async fn a_malformed_frame_fails_the_pending_request() {
        let (_, url) = FakeServer::tcp("codex-tui/0.163.0 (test)").await;
        let endpoint = Endpoint::parse(&url).expect("endpoint");
        let connection = RpcConnection::open(&endpoint, Duration::from_secs(5))
            .await
            .expect("connection");

        let failed = tokio::time::timeout(
            Duration::from_secs(2),
            connection.request("test/malformed", json!({})),
        )
        .await
        .expect("the waiter must not hang on an unroutable frame")
        .expect_err("a malformed frame fails the request");
        assert!(
            matches!(failed, CodexError::NoResponse(_) | CodexError::Transport { .. }),
            "{failed}"
        );

        // The connection is closed, so a later call fails fast too.
        let after = tokio::time::timeout(
            Duration::from_secs(2),
            connection.request("initialize", json!({})),
        )
        .await
        .expect("a closed connection fails fast")
        .expect_err("nothing is served after the malformed frame");
        assert!(matches!(after, CodexError::NoResponse(_)), "{after}");
    }

    /// Ticket #878's seam: a notification the server pushes while a request is
    /// in flight reaches `inbound()`, and the request still gets its own
    /// response. The old synchronous correlation loop dropped it.
    #[tokio::test]
    async fn a_pushed_notification_reaches_inbound_while_requests_are_in_flight() {
        let (server, url) = FakeServer::tcp("codex-tui/0.163.0 (test)").await;
        let endpoint = Endpoint::parse(&url).expect("endpoint");
        let connection = RpcConnection::open(&endpoint, Duration::from_secs(5))
            .await
            .expect("connection");
        let mut inbound = connection.inbound();

        server.push_notification(
            "thread/status/changed",
            json!({ "threadId": "thread-1", "status": { "type": "idle" } }),
        );
        let event = tokio::time::timeout(Duration::from_secs(2), inbound.recv())
            .await
            .expect("a pushed notification arrives")
            .expect("inbound stays open");
        match event {
            Inbound::Notification { method, params, .. } => {
                assert_eq!(method, "thread/status/changed");
                assert_eq!(params["threadId"], "thread-1");
            }
            other => panic!("expected a notification, got {other:?}"),
        }

        let result = connection
            .request(
                "initialize",
                json!({ "clientInfo": { "name": "cola", "version": "test" } }),
            )
            .await
            .expect("initialize still answers");
        assert_eq!(result["userAgent"], "codex-tui/0.163.0 (test)");
    }
}
