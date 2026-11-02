//! The app-server transport: JSON-RPC 2.0 over one WebSocket, one message per
//! text frame (spec #860, tickets #876/#878/#949).
//!
//! The daemon's control endpoint is a **unix-domain socket file on every
//! platform**, and stable Rust cannot reach one on Windows. So the production
//! path never dials it directly: cola spawns `codex app-server proxy
//! --sock <path>`, a Codex subcommand that relays raw bytes between its
//! stdio and the control socket, and runs the normal WebSocket handshake over
//! the child's stdin/stdout. `client_async` over a boxed `AsyncRead +
//! AsyncWrite` keeps one client type for every byte stream underneath.
//!
//! A **daemon** close is detected from the control path, not from stdout: the
//! real proxy's two copy directions are joined, so after the daemon socket
//! reaches EOF it keeps stdout open while it waits for stdin EOF. The watcher
//! polls the control path, and a path that disappears, names a different
//! socket, or no longer accepts a connection closes the connection and kills
//! and reaps the child (a killed daemon leaves its socket file behind, so the
//! last check is a real connect, not a metadata comparison). A reader that
//! does see EOF still closes the connection. The reverse direction is explicit
//! as well: closing the connection (the last handle dropping, a protocol
//! fault, an overflowed decision queue) terminates and reaps the child. A
//! child that exits on its own also closes the connection. Either way every
//! waiter fails by name and [`ServerEvent::Closed`] reaches the decision
//! stream.
//!
//! `Endpoint::WebSocket` remains for the hermetic tests only; the live suites
//! dial `Endpoint::Unix`, and the daemon cannot serve a WebSocket listener.
//!
//! The shipped control socket under `$CODEX_HOME/app-server-control/` is a
//! **symlink** into `/tmp/codex-daemon-<uid>/<hash>` on Unix (spec #948). The path
//! is handed to the proxy verbatim and never canonicalised: the OS follows the
//! link at connect time, so a daemon that re-points it is picked up on the
//! next attach.
//!
//! On Windows the production dial is that same proxy child over the same
//! AF_UNIX path; only the hermetic fixture substitutes a TCP target for its
//! fake daemon, because stable Rust cannot bind an AF_UNIX socket there. The
//! real AF_UNIX-on-Windows proof belongs to the tier-1 job (#953).
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
//!   to the one waiter registered for its id, a notification goes to
//!   [`RpcConnection::inbound`]'s broadcast, and the server-request protocol
//!   (a request, or its `serverRequest/resolved`) goes to the connection's
//!   lossless [`RpcConnection::take_server_events`] stream — those are
//!   decisions a server or a card is blocked on, so they cannot share the
//!   broadcast's lossy lag.
//!
//! The connection is therefore safe to share: [`RpcConnection`] is a cheap
//! handle over the tasks (`Clone`), and the tasks stop when the last handle is
//! dropped. Correlation, framing and push traffic stay in this layer; which
//! notification belongs to which thread is the adapter's business.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::time::Duration;

use futures_util::stream::SplitStream;
use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite};
use tokio::net::TcpStream;
use tokio::process::{Child, ChildStderr, ChildStdin, ChildStdout, Command};
use tokio::sync::{broadcast, mpsc, oneshot, watch};
use tokio_tungstenite::WebSocketStream;
use tokio_tungstenite::tungstenite::Message;
use url::Url;

use super::discovery;
use super::error::{CodexError, Result};

/// How many inbound notifications a subscriber may fall behind before the
/// broadcast drops the oldest. A push-shaped runtime can outpace a slow
/// consumer; the adapter sees [`broadcast::error::RecvError::Lagged`] and
/// reports it instead of silently rendering a hole. The server-request
/// protocol is not subject to this bound — see [`ServerEvent`].
const INBOUND_CAPACITY: usize = 1024;

/// How many server decisions may wait between the reader loop and the approval
/// record.
///
/// This is a **memory** bound, not a rate limit: a peer is free to send more
/// requests than it should, and the queue must not grow without limit. One
/// tool call blocks on one decision at a time, so a backlog this size means the
/// decision pipeline is not running — not that the peer is busy. Overflow is
/// therefore treated as loss of trust: the connection is closed, every pending
/// decision fails loudly, and the server clears its own pending set because the
/// peer it waited on is gone. A decision is never dropped to make room.
const SERVER_EVENT_CAPACITY: usize = 64;

/// How many outgoing frames may sit between a caller and the writer. Bounded on
/// purpose: a peer that stops reading must apply backpressure to the callers
/// (whose own timeouts — the handshake's, the liveness probe's — bound them)
/// instead of letting an unbounded queue grow. A closed socket fails the send,
/// which closes the connection and fails every waiter.
const OUTGOING_CAPACITY: usize = 64;

/// How long a server-initiated request's response may wait for room in that
/// queue. A reply is not a fire-and-forget notification: a stalled writer must
/// not hold a Feishu callback open, and the request must remain answerable, so
/// this budget fails the send by name and lets the adapter restore it.
pub(crate) const RESPONSE_WRITE_TIMEOUT: Duration = Duration::from_secs(2);

/// How often the proxy's watcher checks whether the control path still names
/// the daemon it dialled. The real proxy keeps stdout open after the daemon
/// side reaches EOF, so this poll — not the child's exit — is what notices a
/// daemon that is gone. One poll is a metadata read plus a real connect (the
/// only way to tell a live socket from a killed daemon's stale file), so the
/// cadence is slower than the cheap metadata-only check it replaced.
const CONTROL_POLL_INTERVAL: Duration = Duration::from_secs(1);

/// The most of one proxy stderr line kept for the log. The proxy's diagnostics
/// are short; a newline-free torrent must not grow a buffer without limit, so
/// the rest of the line is drained and replaced by a marker.
const MAX_STDERR_LINE: usize = 8 * 1024;

/// How to reach an app-server.
// `ws://` endpoints are how the hermetic tests dial a TCP fake server; the
// live suites and the daemon path use `Unix`, because the daemon serves no
// WebSocket listener (ticket #954).
#[allow(dead_code)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Endpoint {
    /// `ws://HOST:PORT` — a hermetic-test listener, never the daemon's control
    /// socket.
    WebSocket(String),
    /// `unix://PATH` — a filesystem socket, usually the daemon control socket
    /// (a symlink; followed by the OS, not by string canonicalisation). The
    /// dial spawns `codex app-server proxy --sock PATH` and speaks WebSocket
    /// over the child's stdio, so this works on every platform.
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

/// One notification this connection did not request, fanned out to every
/// consumer of the connection's push stream (the thread dispatcher, the status
/// watcher, doctor's turn probe).
///
/// This stream is a **broadcast**: a consumer that falls behind loses the
/// oldest events, which every reader here is prepared for (a thread dispatcher
/// marks its subscriptions lagged and the status watcher clears its cache).
/// Server-initiated requests deliberately do *not* ride it — see
/// [`ServerEvent`].
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
    /// The socket is gone: no more responses or push traffic will arrive, and
    /// every subscription's stream ends. Sent once, by whichever task noticed
    /// first.
    Closed,
}

/// One frame of the server-request protocol: a request the server is waiting on
/// (an id the server owns) or the notification that ends one.
///
/// These ride a dedicated, **lossless** stream
/// ([`RpcConnection::take_server_events`]) rather than the notification
/// broadcast. Both directions are decisions a server is blocked on, so a lost
/// frame is a hang rather than a gap: a dropped request leaves the server
/// waiting forever with no card, and a dropped resolution leaves a card
/// clickable for a request nobody waits on any more. "Lossless" means no frame
/// is discarded *silently*: the stream is bounded by
/// [`SERVER_EVENT_CAPACITY`] for memory safety, and overflowing it closes the
/// connection (a loud failure every waiter sees) instead of dropping a
/// decision.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum ServerEvent {
    /// A request the server initiated; answer it with
    /// [`RpcConnection::respond`].
    Request {
        id: Value,
        method: String,
        params: Value,
    },
    /// [`SERVER_REQUEST_RESOLVED`]: the server no longer waits on `requestId`
    /// (another client answered it, or the turn ended).
    Resolved { params: Value },
    /// The socket is gone: no answer can reach the server, so nothing may stay
    /// pending.
    Closed,
}

/// The notification the server sends when a request leaves its pending set,
/// whether cola answered it, another client did, or the turn ended. Routed to
/// [`ServerEvent`] so a lost resolution cannot leave a stale clickable card.
pub(crate) const SERVER_REQUEST_RESOLVED: &str = "serverRequest/resolved";

/// The one byte stream a `WebSocketStream` is built over.
trait ByteStream: AsyncRead + AsyncWrite + Unpin + Send {}
impl<T: AsyncRead + AsyncWrite + Unpin + Send> ByteStream for T {}

type BoxedStream = Box<dyn ByteStream>;
type Socket = WebSocketStream<BoxedStream>;

/// The request target the WebSocket handshake uses over the proxy child: the
/// daemon serves one endpoint and ignores the host, but the handshake still
/// needs a valid request target.
const PROXY_HANDSHAKE_URL: &str = "ws://localhost/";

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
    /// The lossless server-request stream's sender. Requests and their
    /// resolutions are sent here from the reader loop; the receiver is handed
    /// to the approval consumer once, by [`RpcConnection::take_server_events`].
    ///
    /// **Bounded** ([`SERVER_EVENT_CAPACITY`]): "one frame per tool call" is a
    /// shape, not a rate a peer must honour, and an unbounded queue would let a
    /// flood of server requests grow cola's memory until OOM. A connection
    /// whose caller never takes the stream (doctor's attach, a liveness probe)
    /// buffers at most that many and drops them with the connection.
    server_events: mpsc::Sender<ServerEvent>,
    server_events_rx: Mutex<Option<mpsc::Receiver<ServerEvent>>>,
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
    /// Set when this connection was dialled through `codex app-server proxy`:
    /// firing it makes the child's watcher kill and reap the process. No
    /// `Endpoint::WebSocket` connection has one.
    proxy_terminate: Option<Arc<tokio::sync::Notify>>,
    /// The proxy child's pid, for the tests that prove it is gone after the
    /// last handle drops.
    #[cfg(test)]
    proxy_pid: Option<u32>,
    tasks: Mutex<Vec<tokio::task::JoinHandle<()>>>,
}

impl Drop for ConnectionInner {
    fn drop(&mut self) {
        // The proxy child must not outlive its last handle: the watcher owns
        // the `Child`, so notify it to kill and reap. A `Notify` permit is
        // remembered even when the watcher has not polled yet, so this cannot
        // race itself away while the runtime is still alive.
        if let Some(terminate) = &self.proxy_terminate {
            terminate.notify_one();
        }
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
    ///
    /// The test shorthand for [`Self::open_with_proxy`] with no proxy binary:
    /// it dials a `ws://` endpoint, and pins the named failure a `unix://`
    /// dial without a binary must produce. Production paths always supply the
    /// binary `discovery::resolve_proxy_binary` resolved.
    #[cfg(test)]
    pub(crate) async fn open(endpoint: &Endpoint, timeout: Duration) -> Result<Self> {
        Self::open_with_proxy(endpoint, timeout, None).await
    }

    /// [`Self::open`] with the `codex` binary to spawn as `app-server proxy`
    /// for a `unix://` endpoint. Ignored for `ws://`.
    pub(crate) async fn open_with_proxy(
        endpoint: &Endpoint,
        timeout: Duration,
        proxy_binary: Option<&Path>,
    ) -> Result<Self> {
        match tokio::time::timeout(timeout, Self::open_inner(endpoint, proxy_binary)).await {
            Err(_) => Err(CodexError::Timeout(timeout)),
            Ok(result) => result,
        }
    }

    async fn open_inner(endpoint: &Endpoint, proxy_binary: Option<&Path>) -> Result<Self> {
        Ok(match endpoint {
            Endpoint::Unix(path) => {
                let binary = proxy_binary.ok_or_else(|| CodexError::ProxyBinary {
                    tried: format!(
                        "dialling `{}` needs a codex binary, and this call supplied none",
                        path.display()
                    ),
                })?;
                // Capture the control path's identity *before* the child is
                // spawned, so a daemon replaced during the handshake is still
                // noticed by the watcher.
                let watch = ProxyWatch::capture(path);
                let (io, child) = spawn_proxy(binary, path).await?;
                // Cancellation anywhere in the handshake (the outer timeout,
                // a caller dropping the future) drops this guard, which kills
                // the child instead of leaking a process nobody owns.
                let mut guard = ProxyGuard::new(child);
                let socket = Self::handshake(PROXY_HANDSHAKE_URL, Box::new(io)).await?;
                let child = guard.take();
                Self::start_with_proxy(socket, Some(child), Some(watch))
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
                Self::start_with_proxy(Self::handshake(raw, Box::new(stream)).await?, None, None)
            }
        })
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
    fn start_with_proxy(socket: Socket, child: Option<Child>, watch: Option<ProxyWatch>) -> Self {
        let (sink, stream) = socket.split();
        let (outgoing, outgoing_rx) = mpsc::channel(OUTGOING_CAPACITY);
        let (inbound, _) = broadcast::channel(INBOUND_CAPACITY);
        let (server_events, server_events_rx) = mpsc::channel(SERVER_EVENT_CAPACITY);
        let (closing, closing_rx) = watch::channel(false);
        let proxy_terminate = child.as_ref().map(|_| Arc::new(tokio::sync::Notify::new()));

        let inner = Arc::new(ConnectionInner {
            outgoing,
            next_id: AtomicU64::new(1),
            pending: Mutex::new(HashMap::new()),
            inbound,
            server_events,
            server_events_rx: Mutex::new(Some(server_events_rx)),
            inbound_seq: AtomicU64::new(0),
            closed: std::sync::atomic::AtomicBool::new(false),
            closing,
            proxy_terminate: proxy_terminate.clone(),
            #[cfg(test)]
            proxy_pid: child.as_ref().and_then(Child::id),
            tasks: Mutex::new(Vec::new()),
        });

        let writer = tokio::spawn(write_loop(sink, outgoing_rx, closing_rx, Arc::downgrade(&inner)));
        let reader = tokio::spawn(read_loop(stream, Arc::downgrade(&inner)));
        inner
            .tasks
            .lock()
            .expect("connection tasks")
            .extend([writer, reader]);
        if let (Some(child), Some(terminate), Some(watch)) = (child, proxy_terminate, watch) {
            // Deliberately not in `tasks`: aborting the connection's I/O tasks
            // must not prevent the child from being killed and reaped.
            watch_proxy(child, terminate, Arc::downgrade(&inner), watch);
        }

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
        self.respond_within(id, result, RESPONSE_WRITE_TIMEOUT).await
    }

    /// [`Self::respond`] with an explicit enqueue budget. The caller gets
    /// [`CodexError::ResponseTimeout`] when the writer's queue stays full, and
    /// the request is still unanswered (the frame was never queued).
    pub(crate) async fn respond_within(&self, id: &Value, result: Value, timeout: Duration) -> Result<()> {
        if self.closed() {
            return Err(CodexError::NoResponse(format!("server request {id}")));
        }
        let frame = json!({
            "jsonrpc": "2.0",
            "id": id,
            "result": result,
        });
        match tokio::time::timeout(timeout, self.send(&frame)).await {
            Ok(result) => result,
            Err(_) => Err(CodexError::ResponseTimeout {
                id: id.to_string(),
                timeout,
            }),
        }
    }

    /// Refuse one server-initiated request by name. Used where cola cannot
    /// express an answer at all (a request kind it does not implement): the
    /// server then sees a refusal instead of waiting forever, and the failure
    /// is never guessed at.
    pub(crate) async fn respond_error(&self, id: &Value, code: i64, message: &str) -> Result<()> {
        self.respond_error_within(id, code, message, RESPONSE_WRITE_TIMEOUT)
            .await
    }

    /// [`Self::respond_error`] with an explicit enqueue budget.
    pub(crate) async fn respond_error_within(
        &self,
        id: &Value,
        code: i64,
        message: &str,
        timeout: Duration,
    ) -> Result<()> {
        if self.closed() {
            return Err(CodexError::NoResponse(format!("server request {id}")));
        }
        let frame = json!({
            "jsonrpc": "2.0",
            "id": id,
            "error": { "code": code, "message": message },
        });
        match tokio::time::timeout(timeout, self.send(&frame)).await {
            Ok(result) => result,
            Err(_) => Err(CodexError::ResponseTimeout {
                id: id.to_string(),
                timeout,
            }),
        }
    }

    /// Subscribe to everything the server pushed that was not a response to
    /// one of this connection's requests. A broadcast, so a slow consumer loses
    /// old events — the server-request protocol is *not* here; see
    /// [`Self::take_server_events`].
    pub(crate) fn inbound(&self) -> broadcast::Receiver<Inbound> {
        self.inner.inbound.subscribe()
    }

    /// Take the connection's one lossless server-request stream.
    ///
    /// Only one consumer may own it (the approval record), so it is taken once
    /// and `None` afterwards. Unlike [`Self::inbound`] no frame here is dropped
    /// silently: an approval request or its resolution is a decision a server
    /// (or a card) is blocked on, not a notification whose loss a reader can
    /// repair from a later read. The stream is bounded for memory safety, so
    /// overflow closes the connection instead (see [`SERVER_EVENT_CAPACITY`]).
    pub(crate) fn take_server_events(&self) -> Option<mpsc::Receiver<ServerEvent>> {
        self.inner.server_events_rx.lock().expect("server events").take()
    }

    /// The connection's close signal.
    ///
    /// A consumer of [`Self::take_server_events`] must watch this beside its
    /// stream: when the stream is *full*, the `Closed` frame cannot be queued,
    /// so the close signal is the only edge that wakes it to clear its state.
    pub(crate) fn closing(&self) -> watch::Receiver<bool> {
        self.inner.closing.subscribe()
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
    /// senders. Test-only; the approval tests also build one to exercise the
    /// decision stream's bound without a socket.
    #[cfg(test)]
    pub(crate) fn test_connection(capacity: usize) -> (Self, mpsc::Receiver<Message>, watch::Receiver<bool>) {
        let (outgoing, outgoing_rx) = mpsc::channel(capacity);
        let (inbound, _) = broadcast::channel(INBOUND_CAPACITY);
        let (server_events, server_events_rx) = mpsc::channel(SERVER_EVENT_CAPACITY);
        let (closing, closing_rx) = watch::channel(false);
        let inner = Arc::new(ConnectionInner {
            outgoing,
            next_id: AtomicU64::new(1),
            pending: Mutex::new(HashMap::new()),
            inbound,
            server_events,
            server_events_rx: Mutex::new(Some(server_events_rx)),
            inbound_seq: AtomicU64::new(0),
            closed: std::sync::atomic::AtomicBool::new(false),
            closing,
            proxy_terminate: None,
            proxy_pid: None,
            tasks: Mutex::new(Vec::new()),
        });
        (Self { inner }, outgoing_rx, closing_rx)
    }

    #[cfg(test)]
    fn pending_len(&self) -> usize {
        self.inner.pending.lock().expect("pending requests").len()
    }

    /// The proxy child's pid, when this connection was dialled through one.
    /// Test-only: the lifetime tests assert the process is gone after the last
    /// handle drops.
    #[cfg(test)]
    pub(crate) fn proxy_pid(&self) -> Option<u32> {
        self.inner.proxy_pid
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

/// Spawn `codex app-server proxy --sock PATH` and hand back its stdio as one
/// byte stream.
///
/// The proxy is Codex's own relay to the control socket (spec #948): cola never
/// speaks the unix domain socket itself, which is why this transport works on
/// Windows too. Stdout is the byte stream; stderr carries the proxy's own
/// diagnostics (a rejected `--sock`, a connect failure) and is drained to the
/// log so the operator is not left with only an exit status.
async fn spawn_proxy(binary: &Path, socket: &Path) -> Result<(ChildIo, Child)> {
    let mut command = Command::new(binary);
    command
        .arg("app-server")
        .arg("proxy")
        .arg("--sock")
        .arg(socket)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    // A failure here is a configuration fault (a binary the OS cannot start),
    // never evidence that the daemon vanished: it deliberately has its own
    // variant so the attach path cannot turn it into `Unreachable`.
    let mut child = command.spawn().map_err(|error| CodexError::ProxySpawn {
        binary: binary.display().to_string(),
        detail: error.to_string(),
    })?;
    let stdin = child.stdin.take().expect("the proxy child's stdin is piped");
    let stdout = child.stdout.take().expect("the proxy child's stdout is piped");
    if let Some(stderr) = child.stderr.take() {
        watch_proxy_stderr(stderr);
    }
    Ok((ChildIo { stdin, stdout }, child))
}

/// Drain the proxy child's stderr to the log. Its only writer is `codex`'s own
/// diagnostics on a failure; without this the operator sees a bare exit code.
///
/// Fixed-size chunks are decoded lossily and split into lines here rather than
/// with a line reader: a reader stops at the first non-UTF-8 line and leaves
/// the pipe undrained, blocking the child mid-relay. A single line is capped
/// at [`MAX_STDERR_LINE`], with the remainder drained and marked, so a
/// newline-free stream cannot grow the buffer without limit.
fn watch_proxy_stderr(stderr: ChildStderr) {
    tokio::spawn(async move {
        let mut stderr = stderr;
        let mut buffer = [0u8; 4096];
        let mut line = Vec::with_capacity(256);
        let mut truncated = false;
        loop {
            let read = match stderr.read(&mut buffer).await {
                Ok(0) | Err(_) => break,
                Ok(read) => read,
            };
            for &byte in &buffer[..read] {
                if let Some(truncated_line) = push_stderr_byte(&mut line, &mut truncated, byte) {
                    log_stderr_line(&line, truncated_line);
                    line.clear();
                }
            }
        }
        if !line.is_empty() || truncated {
            log_stderr_line(&line, truncated);
        }
    });
}

fn push_stderr_byte(line: &mut Vec<u8>, truncated: &mut bool, byte: u8) -> Option<bool> {
    if byte == b'\n' {
        let ended = *truncated;
        *truncated = false;
        return Some(ended);
    }
    if line.len() < MAX_STDERR_LINE {
        line.push(byte);
    } else {
        *truncated = true;
    }
    None
}

fn log_stderr_line(line: &[u8], truncated: bool) {
    let text = String::from_utf8_lossy(line);
    if truncated {
        tracing::warn!("codex app-server proxy: {} [line truncated]", text.trim_end());
    } else {
        tracing::warn!("codex app-server proxy: {}", text.trim_end());
    }
}

/// The proxy child's stdin/stdout as one duplex byte stream.
struct ChildIo {
    stdin: ChildStdin,
    stdout: ChildStdout,
}

impl AsyncRead for ChildIo {
    fn poll_read(
        self: std::pin::Pin<&mut Self>,
        context: &mut std::task::Context<'_>,
        buffer: &mut tokio::io::ReadBuf<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::pin::Pin::new(&mut self.get_mut().stdout).poll_read(context, buffer)
    }
}

impl AsyncWrite for ChildIo {
    fn poll_write(
        self: std::pin::Pin<&mut Self>,
        context: &mut std::task::Context<'_>,
        buffer: &[u8],
    ) -> std::task::Poll<std::io::Result<usize>> {
        std::pin::Pin::new(&mut self.get_mut().stdin).poll_write(context, buffer)
    }

    fn poll_flush(
        self: std::pin::Pin<&mut Self>,
        context: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::pin::Pin::new(&mut self.get_mut().stdin).poll_flush(context)
    }

    fn poll_shutdown(
        self: std::pin::Pin<&mut Self>,
        context: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::pin::Pin::new(&mut self.get_mut().stdin).poll_shutdown(context)
    }
}

/// Owns the proxy child until the connection takes it over. Dropping it kills
/// and reaps the process, which is what must happen when the handshake fails,
/// times out, or is cancelled: no [`ConnectionInner`] exists yet to own it.
///
/// This is deliberately the **only** kill mechanism before the handshake
/// completes — no `kill_on_drop` backstop — so a test can prove the guard is
/// what reaps a child stranded mid-handshake.
struct ProxyGuard(Option<Child>);

impl ProxyGuard {
    fn new(child: Child) -> Self {
        Self(Some(child))
    }

    fn take(&mut self) -> Child {
        self.0.take().expect("the proxy child is taken once")
    }

    fn child(&mut self) -> &mut Child {
        self.0.as_mut().expect("the proxy child is owned")
    }

    /// Kill and reap now. Used by the watcher's terminate edge.
    async fn kill_and_reap(&mut self) {
        if let Some(mut child) = self.0.take() {
            let _ = child.start_kill();
            if let Err(error) = child.wait().await {
                tracing::warn!("codex app-server proxy could not be reaped: {error}");
            }
        }
    }
}

impl Drop for ProxyGuard {
    fn drop(&mut self) {
        let Some(mut child) = self.0.take() else {
            return;
        };
        // `start_kill` is synchronous and needs no runtime; reaping does. When
        // the runtime is already gone there is nothing left that could wait,
        // but the signal itself still guarantees the child is not left
        // running.
        let _ = child.start_kill();
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            runtime.spawn(async move {
                let _ = child.wait().await;
            });
        }
    }
}

/// What the proxy's peer looks like from cola's side, captured before the
/// child is spawned.
///
/// The production proxy relays to a filesystem control socket. A clean daemon
/// exit removes (or re-points) the path and the metadata check notices, but a
/// daemon killed without unlinking its socket leaves the same file behind with
/// the same identity — so a path that still looks unchanged is not proof of
/// life. The poll therefore also connects to the socket: a daemon that is gone
/// refuses the connection even though its path survives. The Windows hermetic
/// fixture cannot bind an AF_UNIX socket in stable Rust, so its fake daemon is
/// a TCP listener and the poll checks that listener instead; that target never
/// comes from production.
#[derive(Debug)]
enum ProxyWatch {
    Path {
        path: PathBuf,
        identity: Option<discovery::SocketIdentity>,
    },
    #[cfg(not(unix))]
    Tcp { address: String },
}

impl ProxyWatch {
    fn capture(path: &Path) -> Self {
        #[cfg(not(unix))]
        if let Some(address) = path.to_str().and_then(|path| path.strip_prefix("tcp://")) {
            return Self::Tcp {
                address: address.to_string(),
            };
        }
        Self::Path {
            path: path.to_path_buf(),
            identity: discovery::socket_identity(path).ok().flatten(),
        }
    }

    /// Whether the daemon this path named is gone: the path is missing, it now
    /// names a different socket, or the socket it names no longer accepts a
    /// connection. The last case is the one a SIGKILL leaves behind — the path
    /// and its `(dev,ino)` survive but nothing listens — so an unchanged
    /// identity is only a precondition for probing, never proof of life. A
    /// metadata read error is not evidence of death, so it is ignored rather
    /// than closing a healthy connection.
    async fn lost(&self) -> bool {
        match self {
            Self::Path { path, identity } => {
                let current = match discovery::socket_identity(path) {
                    Ok(current) => current,
                    Err(_) => return false,
                };
                match current {
                    None => identity.is_some(),
                    Some(current) if identity.is_some_and(|baseline| baseline != current) => true,
                    // A liveness check on the daemon, not the adapter's
                    // transport: the proxy child stays the only RPC carrier.
                    Some(_) => !discovery::control_socket_accepts(path).await,
                }
            }
            #[cfg(not(unix))]
            Self::Tcp { address } => {
                use std::net::SocketAddr;

                let Ok(address) = address.parse::<SocketAddr>() else {
                    return false;
                };
                // The fixture's fake daemon is this suite's only TCP peer. Once
                // its listener is gone every connect attempt fails — refused,
                // timed out, or whatever kind this platform's stack reports —
                // and any of those means the fixture is dead. Only a completed
                // connect is evidence of life.
                match tokio::time::timeout(discovery::CONTROL_PROBE_TIMEOUT, TcpStream::connect(address))
                    .await
                {
                    Ok(Ok(stream)) => {
                        drop(stream);
                        false
                    }
                    _ => true,
                }
            }
        }
    }
}

/// Watch the proxy child: its exit of **any** kind is the connection's close,
/// the terminate edge (last handle dropped, or the connection closed for a
/// protocol error) kills and reaps it, and a control path that no longer names
/// the daemon it dialled is a dead daemon — the real proxy keeps stdout open
/// waiting for stdin EOF, so that is the only edge a `read_loop` cannot see.
/// Owning the child through [`ProxyGuard`] also covers the watcher being
/// cancelled at runtime shutdown.
fn watch_proxy(
    child: Child,
    terminate: Arc<tokio::sync::Notify>,
    inner: Weak<ConnectionInner>,
    watch: ProxyWatch,
) {
    tokio::spawn(async move {
        let mut child = ProxyGuard::new(child);
        let mut poll = tokio::time::interval(CONTROL_POLL_INTERVAL);
        poll.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        tokio::select! {
            biased;
            _ = terminate.notified() => child.kill_and_reap().await,
            status = child.child().wait() => {
                match status {
                    Ok(status) if status.success() => {
                        tracing::debug!("codex app-server proxy exited: {status}");
                    }
                    Ok(status) => {
                        tracing::warn!("codex app-server proxy exited unsuccessfully: {status}");
                    }
                    Err(error) => {
                        tracing::warn!("codex app-server proxy wait failed: {error}");
                    }
                }
            }
            _ = wait_for_control_path(&watch, &mut poll) => {
                // The daemon is gone. Fail every waiter before the reap so a
                // close cannot be delayed by the child's own shutdown.
                if let Some(inner) = inner.upgrade() {
                    close(&inner);
                }
                child.kill_and_reap().await;
            }
        }
        if let Some(inner) = inner.upgrade() {
            close(&inner);
        }
    });
}

/// Poll the control path until it is gone. The first interval tick completes
/// immediately; it is harmless (the captured identity is still current, and
/// the handshake just proved a listener is there) and keeps a daemon that
/// vanished during the handshake from going unnoticed.
async fn wait_for_control_path(watch: &ProxyWatch, poll: &mut tokio::time::Interval) {
    loop {
        poll.tick().await;
        if watch.lost().await {
            return;
        }
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
    // A closed connection owns nothing: the proxy child is killed and reaped
    // by its watcher even while other handles are still alive.
    if let Some(terminate) = &inner.proxy_terminate {
        terminate.notify_one();
    }
    let pending = std::mem::take(&mut *inner.pending.lock().expect("pending requests"));
    for (_, pending) in pending {
        let _ = pending.reply.send(Err(RpcFailure::Closed));
    }
    // The server-request stream ends too: the approval record clears its
    // pending set on this frame, because no answer can reach the server. The
    // queue may be full (that is one way to get here), in which case the frame
    // cannot be queued — the approval record's watch on `closing` is the edge
    // that wakes it.
    let _ = inner.server_events.try_send(ServerEvent::Closed);
    let _ = inner.inbound.send(Inbound::Closed);
}

/// Put one server decision on the lossless stream, or take the connection down.
///
/// A full queue means nothing is draining the decision pipeline, so the
/// connection's decisions can no longer be honoured. Closing is the loud
/// failure the peer and every waiter see; dropping the event would leave a
/// server waiting forever (the exact failure this stream exists to prevent).
fn send_server_event(inner: &Arc<ConnectionInner>, event: ServerEvent) {
    match inner.server_events.try_send(event) {
        Ok(()) => {}
        Err(mpsc::error::TrySendError::Full(_)) => {
            tracing::error!(
                "the codex connection's server-request stream is full \
                 ({SERVER_EVENT_CAPACITY} decisions unread); closing the connection rather than \
                 dropping a decision"
            );
            close(inner);
        }
        // The receiver is gone: the connection is closing anyway.
        Err(mpsc::error::TrySendError::Closed(_)) => {}
    }
}

/// Route one inbound value.
fn dispatch(inner: &Arc<ConnectionInner>, value: Value) {
    let method = value.get("method").and_then(Value::as_str);
    let id = value.get("id").cloned();
    match (method, id) {
        (Some(method), Some(id)) => {
            let params = value.get("params").cloned().unwrap_or(Value::Null);
            // A server request is a server blocked on an answer: it rides the
            // lossless stream, never the notification broadcast.
            send_server_event(
                inner,
                ServerEvent::Request {
                    id,
                    method: method.to_string(),
                    params,
                },
            );
        }
        (Some(method), None) => {
            let params = value.get("params").cloned().unwrap_or(Value::Null);
            if method == SERVER_REQUEST_RESOLVED {
                // The end of a server request belongs to the same lossless
                // stream: losing it strands a clickable card.
                send_server_event(inner, ServerEvent::Resolved { params });
                return;
            }
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
    use crate::codex::test_server::{FakeServer, ProxyDaemon};

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

    #[test]
    fn a_proxy_stderr_line_is_capped_and_the_rest_drained() {
        let mut line = Vec::new();
        let mut truncated = false;
        for _ in 0..MAX_STDERR_LINE + 32 {
            assert_eq!(push_stderr_byte(&mut line, &mut truncated, b'x'), None);
        }
        assert_eq!(line.len(), MAX_STDERR_LINE, "the line is capped");
        assert!(truncated, "the excess is recorded as a marker");
        assert_eq!(
            push_stderr_byte(&mut line, &mut truncated, b'\n'),
            Some(true),
            "the rest of the oversized line was still drained to its newline"
        );
        assert!(!truncated);
        line.clear();
        assert!(push_stderr_byte(&mut line, &mut truncated, b'y').is_none());
        assert_eq!(push_stderr_byte(&mut line, &mut truncated, b'\n'), Some(false));
    }

    /// The hermetic acceptance path (#949): a real `codex app-server proxy`
    /// child connects to the fake daemon, the WebSocket handshake completes
    /// over its stdio, a request gets its response, and a server-initiated
    /// request is answered with the server's own id.
    ///
    /// On unix the fake daemon is a real AF_UNIX socket behind a symlinked
    /// control path. On Windows stable Rust cannot bind AF_UNIX, so the relay
    /// is handed a `tcp://` target instead (see [`ProxyDaemon`]); that is the
    /// same child/handshake seam, not the AF_UNIX-on-Windows proof (#953).
    #[tokio::test]
    async fn the_transport_handshakes_and_correlates_through_a_real_proxy_child() {
        let daemon = ProxyDaemon::start("codex-tui/0.166.0 (test)").await;
        let connection = daemon.connection().await;
        let pid = connection.proxy_pid().expect("a proxy child was spawned");
        assert!(crate::process::alive(pid as i32), "the proxy child runs");

        let result = connection
            .request(
                "initialize",
                json!({ "clientInfo": { "name": "cola", "version": "test" } }),
            )
            .await
            .expect("initialize over the proxy's stdio");
        assert_eq!(result["userAgent"], "codex-tui/0.166.0 (test)");
        connection.notify("initialized", None).await.expect("initialized");
        daemon.server.wait_for_initialized(1).await;

        let mut server_events = connection
            .take_server_events()
            .expect("the connection's one server-request stream");
        let id = daemon
            .server
            .request_to_all("test/approval", json!({ "threadId": "t" }));
        let event = tokio::time::timeout(Duration::from_secs(2), server_events.recv())
            .await
            .expect("the server request arrives through the proxy")
            .expect("the stream stays open");
        let ServerEvent::Request {
            id: received, method, ..
        } = event
        else {
            panic!("expected a server request, got {event:?}");
        };
        assert_eq!(received, id);
        assert_eq!(method, "test/approval");
        connection
            .respond(&received, json!({ "decision": "accept" }))
            .await
            .expect("the answer is written");
        daemon.server.wait_for_server_request_replies(1).await;
        assert_eq!(
            daemon.server.server_request_replies()[0].result,
            Some(json!({ "decision": "accept" }))
        );
    }

    /// Returning from the dial with a handshaken connection means the child is
    /// still there; dropping the last handle kills and reaps it.
    #[tokio::test]
    async fn dropping_the_last_handle_reaps_the_proxy_child() {
        let daemon = ProxyDaemon::start("codex-tui/0.166.0 (test)").await;
        let connection = daemon.connection().await;
        let pid = connection.proxy_pid().expect("a proxy child was spawned");
        assert!(crate::process::alive(pid as i32));

        let second_handle = connection.clone();
        drop(second_handle);
        assert!(
            crate::process::alive(pid as i32),
            "one surviving handle keeps the child alive"
        );

        drop(connection);
        wait_for_pid_to_disappear(pid).await;
    }

    /// The daemon going away is the connection's close even though the proxy
    /// child survives: the real proxy joins both copy directions, so after the
    /// socket side reaches EOF it keeps stdout open until stdin EOF. A pending
    /// waiter must fail, the decision stream must end with `Closed`, and the
    /// child must be reaped — all driven by the control path disappearing, not
    /// by the child exiting.
    #[tokio::test]
    async fn a_dead_daemon_closes_the_proxy_connection_and_reaps_the_child() {
        let daemon = ProxyDaemon::start("codex-tui/0.166.0 (test)").await;
        let connection = daemon.connection().await;
        let pid = connection.proxy_pid().expect("a proxy child was spawned");
        let mut server_events = connection
            .take_server_events()
            .expect("the connection's one server-request stream");

        // The fake server answers no method it does not implement, so this
        // request is genuinely pending when the daemon dies.
        let pending = tokio::spawn({
            let connection = connection.clone();
            async move { connection.request("test/never", json!({})).await }
        });
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(!pending.is_finished(), "the request is still pending");

        daemon.die().await;

        let failed = tokio::time::timeout(Duration::from_secs(5), pending)
            .await
            .expect("a dead daemon must not leave the waiter hanging")
            .expect("the request task did not panic")
            .expect_err("the request cannot be answered over a dead daemon");
        assert!(
            matches!(failed, CodexError::NoResponse(_)),
            "the waiter fails by name: {failed}"
        );
        assert!(connection.closed());

        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        loop {
            match tokio::time::timeout(Duration::from_secs(1), server_events.recv()).await {
                Ok(Some(ServerEvent::Closed)) => break,
                Ok(Some(other)) => panic!("only `Closed` can follow a dead daemon: {other:?}"),
                Ok(None) => break,
                Err(_) if tokio::time::Instant::now() < deadline => {}
                Err(_) => panic!("the decision stream never reported the close"),
            }
        }
        wait_for_pid_to_disappear(pid).await;
    }

    /// A daemon killed **without** unlinking its control socket is still dead:
    /// a SIGKILL leaves the socket file and its `(dev,ino)` identity behind, so
    /// the metadata comparison sees no change. Only a real connect can tell
    /// that stale path from a live daemon, so a pending waiter must fail,
    /// `Closed` must arrive, and the proxy child must be reaped. Removing the
    /// liveness probe makes this test time out, which is the point.
    #[cfg(target_family = "unix")]
    #[tokio::test]
    async fn a_sigkilled_daemon_closes_the_proxy_connection_through_its_stale_socket() {
        let daemon = ProxyDaemon::start("codex-tui/0.166.0 (test)").await;
        let connection = daemon.connection().await;
        let pid = connection.proxy_pid().expect("a proxy child was spawned");
        let mut server_events = connection
            .take_server_events()
            .expect("the connection's one server-request stream");
        let Endpoint::Unix(control) = &daemon.endpoint else {
            panic!("the unix fake daemon publishes a control path");
        };
        let identity = discovery::socket_identity(control)
            .expect("the control path is readable")
            .expect("the control path names the live socket");

        let pending = tokio::spawn({
            let connection = connection.clone();
            async move { connection.request("test/never", json!({})).await }
        });
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(!pending.is_finished(), "the request is still pending");

        daemon.die_keeping_socket().await;
        assert_eq!(
            discovery::socket_identity(control).expect("the stale path is readable"),
            Some(identity),
            "a SIGKILL leaves the control path and its identity behind"
        );

        let failed = tokio::time::timeout(Duration::from_secs(5), pending)
            .await
            .expect("a stale control socket must not leave the waiter hanging")
            .expect("the request task did not panic")
            .expect_err("the request cannot be answered over a dead daemon");
        assert!(
            matches!(failed, CodexError::NoResponse(_)),
            "the waiter fails by name: {failed}"
        );
        assert!(connection.closed());

        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        loop {
            match tokio::time::timeout(Duration::from_secs(1), server_events.recv()).await {
                Ok(Some(ServerEvent::Closed)) => break,
                Ok(Some(other)) => panic!("only `Closed` can follow a dead daemon: {other:?}"),
                Ok(None) => break,
                Err(_) if tokio::time::Instant::now() < deadline => {}
                Err(_) => panic!("the decision stream never reported the close"),
            }
        }
        wait_for_pid_to_disappear(pid).await;
    }

    /// A control path re-pointed at another daemon's socket is the same close:
    /// the identity captured before the spawn no longer matches, so the
    /// connection cannot be trusted to name the server it was dialled for.
    #[cfg(target_family = "unix")]
    #[tokio::test]
    async fn a_replaced_control_socket_closes_the_proxy_connection() {
        let daemon = ProxyDaemon::start("codex-tui/0.166.0 (test)").await;
        let connection = daemon.connection().await;
        let pid = connection.proxy_pid().expect("a proxy child was spawned");
        let pending = tokio::spawn({
            let connection = connection.clone();
            async move { connection.request("test/never", json!({})).await }
        });
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(!pending.is_finished(), "the request is still pending");

        let crate::codex::transport::Endpoint::Unix(link) = &daemon.endpoint else {
            panic!("the unix fake daemon publishes a control path");
        };
        let replacement = link.with_file_name("replacement.sock");
        let listener =
            std::os::unix::net::UnixListener::bind(&replacement).expect("bind a replacement daemon");
        // Replace the published link atomically: no window in which the path is
        // missing, so only the identity comparison can notice.
        let staged = link.with_file_name("staged-link");
        std::os::unix::fs::symlink(&replacement, &staged).expect("stage the replacement link");
        std::fs::rename(&staged, link).expect("re-point the control path");

        let failed = tokio::time::timeout(Duration::from_secs(5), pending)
            .await
            .expect("a replaced daemon must not leave the waiter hanging")
            .expect("the request task did not panic")
            .expect_err("the request cannot be answered over a replaced daemon");
        assert!(
            matches!(failed, CodexError::NoResponse(_)),
            "the waiter fails by name: {failed}"
        );
        wait_for_pid_to_disappear(pid).await;
        drop(listener);
    }

    /// The proxy spawns from the binary the caller resolved; a `unix://` dial
    /// with none is the named failure, never a silent search of the host.
    #[tokio::test]
    async fn a_unix_dial_without_a_proxy_binary_fails_by_name() {
        let error = RpcConnection::open(
            &Endpoint::Unix(PathBuf::from("/tmp/app-server-control.sock")),
            Duration::from_secs(1),
        )
        .await
        .err()
        .expect("no binary can be spawned");
        assert!(
            matches!(error, CodexError::ProxyBinary { .. }),
            "the dial names the missing proxy binary: {error}"
        );
    }

    /// Closing the connection while the daemon stays healthy is the one path
    /// where only `close()`'s terminate edge can reap the child: the real
    /// proxy waits for both stdin EOF *and* socket EOF, and the daemon's socket
    /// never ends here. A non-JSON frame closes the connection, and the child
    /// must be gone even though the `RpcConnection` handle is still alive.
    #[tokio::test]
    async fn a_protocol_fault_kills_the_proxy_child_while_the_daemon_lives() {
        let daemon = ProxyDaemon::start("codex-tui/0.166.0 (test)").await;
        let connection = daemon.connection().await;
        let pid = connection.proxy_pid().expect("a proxy child was spawned");

        let failed = connection
            .request("test/malformed", json!({}))
            .await
            .expect_err("a non-JSON frame fails the request");
        assert!(
            matches!(failed, CodexError::NoResponse(_) | CodexError::Transport { .. }),
            "the waiter fails by name: {failed}"
        );
        assert!(connection.closed(), "the connection is closed");

        // Deliberately keep `connection` alive: the terminate edge, not the
        // last-handle drop, is what must reap the child.
        wait_for_pid_to_disappear(pid).await;
        drop(connection);
    }

    /// A handshake cancelled mid-flight strands a real child: no
    /// `ConnectionInner` exists yet, and the real proxy keeps running while its
    /// socket side stays open. The handshake-failure guard must kill and reap
    /// it.
    ///
    /// The dial gets a generous budget and is **aborted once the child has been
    /// observed**, rather than racing a short timeout against a process-table
    /// scan on a slow runner. Aborting drops the same future a timeout would,
    /// so the drop path under test is unchanged. The fake daemon accepts and
    /// never answers, and the relay fixture does not exit on its own, so
    /// removing the guard leaves the pid alive and fails this test.
    #[tokio::test]
    async fn a_cancelled_handshake_kills_and_reaps_the_proxy_child() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind a silent daemon");
        let address = listener.local_addr().expect("address");
        let target = PathBuf::from(format!("tcp://{address}"));
        let (release, held) = tokio::sync::oneshot::channel::<()>();
        tokio::spawn(async move {
            // Accept and hold the connection open: a fixture that saw EOF would
            // exit by itself and hide a missing reap.
            if let Ok((stream, _)) = listener.accept().await {
                let _ = held.await;
                drop(stream);
            }
        });

        let dial = {
            let target = target.clone();
            tokio::spawn(async move {
                RpcConnection::open_with_proxy(
                    &Endpoint::Unix(target),
                    Duration::from_secs(5),
                    Some(&crate::codex::test_server::proxy_helper()),
                )
                .await
            })
        };
        let pid = wait_for_proxy_child(&target).await;
        assert!(crate::process::alive(pid as i32), "the proxy child runs");

        dial.abort();
        let joined = dial.await;
        assert!(
            joined.is_err_and(|error| error.is_cancelled()),
            "the dial task was cancelled mid-handshake"
        );
        wait_for_pid_to_disappear(pid).await;
        let _ = release.send(());
    }

    /// A proxy that exits before the handshake is a transport failure. This
    /// says nothing about reaping (the fixture exits itself); the cancellation
    /// test above is the one that proves the guard.
    #[tokio::test]
    async fn an_immediately_exiting_proxy_cannot_handshake() {
        let error = RpcConnection::open_with_proxy(
            &Endpoint::Unix(PathBuf::from("exit")),
            Duration::from_secs(5),
            Some(&crate::codex::test_server::proxy_helper()),
        )
        .await
        .err()
        .expect("a child with no daemon cannot complete the handshake");
        assert!(
            matches!(error, CodexError::Transport { .. } | CodexError::Timeout(_)),
            "the failure is the transport's: {error}"
        );
    }

    /// The fixture must behave like the real proxy: a daemon-side EOF ends only
    /// the socket→stdout pump, and the process stays alive with stdout open
    /// until stdin EOF. Without this the dead-daemon test would pass even if
    /// the transport's control-path watcher were removed.
    #[tokio::test]
    async fn the_proxy_fixture_survives_a_daemon_side_eof() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind a fake daemon");
        let address = listener.local_addr().expect("address");
        let mut child = Command::new(crate::codex::test_server::proxy_helper())
            .arg("--sock")
            .arg(format!("tcp://{address}"))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .expect("spawn the relay fixture");
        let stdin = child.stdin.take().expect("the fixture's stdin is piped");

        let (stream, _) = listener.accept().await.expect("the fixture connects");
        // The daemon side closes: the socket→stdout pump ends, but the process
        // must keep waiting for stdin instead of exiting.
        drop(stream);
        tokio::time::sleep(Duration::from_millis(500)).await;
        assert!(
            child.try_wait().expect("poll the fixture").is_none(),
            "the fixture exited on the daemon-side EOF, unlike the real proxy"
        );

        drop(stdin);
        let status = tokio::time::timeout(Duration::from_secs(5), child.wait())
            .await
            .expect("stdin EOF ends the fixture")
            .expect("wait for the fixture");
        assert!(status.success(), "the fixture exits cleanly: {status}");
    }

    async fn wait_for_proxy_child(target: &Path) -> u32 {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        loop {
            if let Some(pid) = crate::codex::test_server::find_proxy_child(target) {
                return pid as u32;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "the proxy fixture never started for {}",
                target.display()
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    async fn wait_for_pid_to_disappear(pid: u32) {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        while crate::process::alive(pid as i32) {
            assert!(
                tokio::time::Instant::now() < deadline,
                "the proxy child {pid} survived its connection"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        #[cfg(target_os = "linux")]
        {
            // `alive` already treats a zombie as gone; the reaping half of the
            // contract is that `/proc/<pid>` is gone too.
            while Path::new(&format!("/proc/{pid}")).exists() {
                assert!(
                    tokio::time::Instant::now() < deadline,
                    "the proxy child {pid} was killed but never reaped"
                );
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        }
    }

    /// Two clients attach independently: a second connection's handshake does
    /// not disturb the first, and each gets its own response.
    #[tokio::test]
    async fn a_second_connection_opens_independently() {
        let (server, url) = FakeServer::tcp("codex-tui/0.166.0 (test)").await;
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
        let (_, url) = FakeServer::tcp("codex-tui/0.166.0 (test)").await;
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
        assert_eq!(result["userAgent"], "codex-tui/0.166.0 (test)");

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
        let (server, url) = FakeServer::tcp("codex-tui/0.166.0 (test)").await;
        let endpoint = Endpoint::parse(&url).expect("endpoint");
        let connection = RpcConnection::open(&endpoint, Duration::from_secs(5))
            .await
            .expect("connection");
        let mut server_events = connection
            .take_server_events()
            .expect("the connection's one server-request stream");

        let id = server.request_to_all("test/approval", json!({ "threadId": "t" }));
        let event = tokio::time::timeout(Duration::from_secs(2), server_events.recv())
            .await
            .expect("the server request arrives")
            .expect("the stream stays open");
        match event {
            ServerEvent::Request {
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
        assert_eq!(result["userAgent"], "codex-tui/0.166.0 (test)");
    }

    /// Whole-branch review #883: the server-request protocol is **lossless**.
    /// The notification broadcast drops the oldest events for a consumer that
    /// falls behind, so a request followed by a burst of notifications used to
    /// be gone by the time the approval record read its stream — the server
    /// then waits forever with no card. The dedicated stream keeps the request
    /// (and its resolution) regardless of what the broadcast does.
    #[tokio::test]
    async fn a_server_request_survives_a_lagged_notification_burst() {
        let (connection, _frames, _closing) = RpcConnection::test_connection(1);
        let mut server_events = connection
            .take_server_events()
            .expect("the connection's one server-request stream");
        // A broadcast consumer that never drains: after more than
        // INBOUND_CAPACITY events behind it, it is lagged — the state the
        // approval record used to read requests from.
        let _slow = connection.inbound();

        dispatch(
            &connection.inner,
            json!({
                "jsonrpc": "2.0",
                "id": 7,
                "method": "item/commandExecution/requestApproval",
                "params": { "threadId": "t" },
            }),
        );
        for seq in 0..=INBOUND_CAPACITY {
            dispatch(
                &connection.inner,
                json!({
                    "jsonrpc": "2.0",
                    "method": "item/updated",
                    "params": { "seq": seq },
                }),
            );
        }

        let event = tokio::time::timeout(Duration::from_secs(2), server_events.recv())
            .await
            .expect("the request is not dropped with the notifications")
            .expect("the stream is open");
        match event {
            ServerEvent::Request { id, method, .. } => {
                assert_eq!(id, json!(7));
                assert_eq!(method, "item/commandExecution/requestApproval");
            }
            other => panic!("expected the request first, got {other:?}"),
        }

        // The resolution rides the same stream, so a burst cannot leave a card
        // clickable for a request the server no longer waits on.
        dispatch(
            &connection.inner,
            json!({
                "jsonrpc": "2.0",
                "method": SERVER_REQUEST_RESOLVED,
                "params": { "requestId": 7 },
            }),
        );
        assert!(matches!(
            server_events.recv().await,
            Some(ServerEvent::Resolved { params }) if params["requestId"] == json!(7)
        ));
    }

    /// The lossless stream has exactly one owner, and the connection's close
    /// ends it with `Closed` — the frame the approval record clears its pending
    /// set on, because no answer can reach a closed server.
    #[tokio::test]
    async fn the_server_stream_is_taken_once_and_ends_with_the_connection() {
        let (connection, _frames, _closing) = RpcConnection::test_connection(1);
        let mut events = connection.take_server_events().expect("the first take");
        assert!(
            connection.take_server_events().is_none(),
            "a second taker would get an empty stream and record nothing"
        );
        close(&connection.inner);
        assert_eq!(events.recv().await, Some(ServerEvent::Closed));
        drop(connection);
        assert_eq!(events.recv().await, None, "the stream ends with the connection");
    }

    /// The decision stream is bounded for memory safety, and overflow is loss of
    /// trust rather than a dropped decision: the connection closes, every
    /// waiter fails loudly, and the peer sees the socket go (so it clears its
    /// pending set). The already-queued decisions stay readable, and the close
    /// signal is set even though the `Closed` frame no longer fits.
    #[tokio::test]
    async fn a_full_server_event_queue_closes_the_connection() {
        let (connection, _frames, _closing) = RpcConnection::test_connection(1);
        let mut events = connection.take_server_events().expect("the stream");

        for id in 0..=SERVER_EVENT_CAPACITY {
            dispatch(
                &connection.inner,
                json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "method": "test/approval",
                    "params": {},
                }),
            );
        }

        assert!(connection.closed(), "overflow closes the connection");
        assert!(
            *connection.closing().borrow(),
            "the consumer's wake edge is set even though `Closed` could not be queued"
        );

        // Every queued decision is still readable — the overflow request is the
        // one refused, no earlier frame is lost — and the stream then stalls on
        // the live sender, which is why the close signal exists.
        let mut drained = 0;
        while let Ok(Some(event)) = tokio::time::timeout(Duration::from_millis(50), events.recv()).await {
            assert!(
                matches!(event, ServerEvent::Request { .. }),
                "only decisions precede the close: {event:?}"
            );
            drained += 1;
        }
        assert_eq!(
            drained, SERVER_EVENT_CAPACITY,
            "the queue held exactly its capacity; the overflow frame closed instead of growing it"
        );
    }

    /// Ticket #882's write-side bound: a stalled writer (a full queue with no
    /// reader) cannot hold `respond` open indefinitely. The timeout is named,
    /// the frame is not queued, and draining the queue lets the same response
    /// succeed afterwards.
    #[tokio::test]
    async fn a_response_send_is_bounded_by_the_writer_queue() {
        let (connection, mut frames, _closing) = RpcConnection::test_connection(1);
        connection
            .notify("filler", None)
            .await
            .expect("fill the writer queue");

        let error = connection
            .respond_within(
                &json!(7),
                json!({ "decision": "accept" }),
                Duration::from_millis(20),
            )
            .await
            .expect_err("a full queue must time out");
        assert!(
            matches!(error, CodexError::ResponseTimeout { .. }),
            "the timeout is named: {error}"
        );

        let filler = frames.recv().await.expect("the filler frame");
        assert!(matches!(filler, Message::Text(_)));
        connection
            .respond_within(
                &json!(7),
                json!({ "decision": "accept" }),
                Duration::from_millis(200),
            )
            .await
            .expect("the retry is queued once there is room");
        let response = frames.recv().await.expect("the response frame");
        let Message::Text(text) = response else {
            panic!("expected a text frame");
        };
        let value: Value = serde_json::from_str(&text).expect("the response is JSON");
        assert_eq!(value["id"], json!(7));
        assert_eq!(value["result"], json!({ "decision": "accept" }));
        assert!(
            frames.try_recv().is_err(),
            "a timed-out frame is never silently queued"
        );
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
        let (_, url) = FakeServer::tcp("codex-tui/0.166.0 (test)").await;
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
        let (server, url) = FakeServer::tcp("codex-tui/0.166.0 (test)").await;
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
        assert_eq!(result["userAgent"], "codex-tui/0.166.0 (test)");
    }
}
