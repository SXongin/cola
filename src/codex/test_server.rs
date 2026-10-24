//! A scripted `codex app-server` for the transport and thread tests (spec #860,
//! tickets #876/#878).
//!
//! It is a real WebSocket server on a real socket — TCP or unix — so the
//! production client (handshake, framing, correlation, subscriptions) runs
//! against the wire rather than a mock. It answers `initialize` with the fields
//! the pinned protocol requires, counts `initialized` notifications, and
//! scripts the thread surface: `thread/list` pages a seeded store, `thread/read`
//! returns one thread, `thread/resume` subscribes **only a thread that has
//! run**, `thread/name/set` updates the stored name and `thread/unsubscribe`
//! drops one connection's subscription without touching another's. It can push
//! `item/*` traffic to the subscribers of a thread, which is the only way to
//! prove the client's reader loop routes push traffic (ticket #878).
//!
//! Compiled only for tests.

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
use tokio::sync::mpsc;
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
    /// The file `daemon start` reads its link target from, so a test can point a
    /// later start at another socket **without rewriting the executable** (which
    /// can hit `ETXTBSY` while the previous instance is still mapped).
    target: Option<std::path::PathBuf>,
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
        let (start, target) = match real_socket {
            Some(real_socket) => {
                let target = directory.join("link-target");
                std::fs::write(&target, real_socket.display().to_string())
                    .expect("write the fake link target");
                (
                    format!(
                        "mkdir -p {}\nln -sfn \"$(cat {})\" {}\nprintf '%s\\n' '{{\"status\":\"started\"}}'\n",
                        shell_single_quote(
                            &control_socket
                                .parent()
                                .expect("control dir")
                                .display()
                                .to_string()
                        ),
                        shell_single_quote(&target.display().to_string()),
                        shell_single_quote(&control_socket.display().to_string()),
                    ),
                    Some(target),
                )
            }
            None => ("printf '%s\\n' '{\"status\":\"started\"}'\n".to_string(), None),
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
        Self { binary, log, target }
    }

    /// Point the next `daemon start` at another socket, without rewriting the
    /// script. Only meaningful when the CLI was written with a target.
    pub(crate) fn point_at(&self, socket: &Path) {
        let target = self
            .target
            .as_ref()
            .expect("this fake CLI was written without a link target");
        std::fs::write(target, socket.display().to_string()).expect("re-point the fake link target");
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

/// One thread in the scripted app-server's store.
///
/// `has_history` is the spike's discriminator (ADR-0090's amendment, question
/// 2): `thread/start` creates a thread, but `thread/resume` answers `-32600 no
/// rollout found` until it has run a turn. A fixture with `has_history: false`
/// is exactly that never-run thread.
#[derive(Debug, Clone)]
pub(crate) struct ScriptedThread {
    pub(crate) id: String,
    /// The session tree root. Deliberately independent of `id`, so a client
    /// that derives it from the thread id instead of reading it fails a test.
    pub(crate) session_id: String,
    pub(crate) name: Option<String>,
    /// Set for a sub-agent thread; mapped onto the neutral list row's parent.
    pub(crate) parent_thread_id: Option<String>,
    pub(crate) preview: String,
    pub(crate) cwd: String,
    pub(crate) source: String,
    pub(crate) created_at: i64,
    pub(crate) updated_at: i64,
    pub(crate) has_history: bool,
    /// The persisted turns `thread/read` and `thread/resume` carry. A real
    /// `thread/read` page is bounded, so a test can cap it with
    /// [`Self::read_turn_limit`] while `thread/turns/list` still pages the
    /// whole history.
    pub(crate) turns: Vec<Value>,
    /// How many turns a `thread/read`/`thread/resume` response carries. `None`
    /// carries every stored turn.
    pub(crate) read_turn_limit: Option<usize>,
    /// Answer `thread/turns/list` without the required `data` field, so a
    /// malformed page is a decode error rather than an empty success.
    pub(crate) malformed_turns_page: bool,
    /// Return a repeating `nextCursor` from `thread/turns/list`, so the client
    /// must refuse to loop and keep the projection incomplete.
    pub(crate) repeat_turns_cursor: bool,
}

impl ScriptedThread {
    /// A thread that has run, so it can be adopted.
    pub(crate) fn adopted(id: &str, session_id: &str, cwd: &str, name: &str) -> Self {
        Self {
            id: id.to_string(),
            session_id: session_id.to_string(),
            name: Some(name.to_string()),
            parent_thread_id: None,
            preview: name.to_string(),
            cwd: cwd.to_string(),
            source: "cli".to_string(),
            created_at: 1_700_000_000,
            updated_at: 1_700_000_000,
            has_history: true,
            turns: Vec::new(),
            read_turn_limit: None,
            malformed_turns_page: false,
            repeat_turns_cursor: false,
        }
    }

    /// A freshly `thread/start`-ed thread that has not run a turn: resume must
    /// fail with the documented no-rollout error.
    pub(crate) fn never_run(id: &str, session_id: &str, cwd: &str) -> Self {
        Self {
            has_history: false,
            name: None,
            ..Self::adopted(id, session_id, cwd, "")
        }
    }
}

/// One connection stream, tagged with its connection id so a release removes
/// exactly one subscriber.
type Stream = (usize, mpsc::UnboundedSender<Message>);

/// The state one fake server exposes to its test.
struct State {
    user_agent: String,
    reject_initialize: Option<String>,
    connections: AtomicUsize,
    initialized: AtomicUsize,
    /// `thread/loaded/list` calls: the lifecycle's liveness probe.
    liveness: AtomicUsize,
    /// Per-connection serving tasks, so a test can make the daemon *die*
    /// (break its live connections) while leaving its socket path on disk.
    tasks: std::sync::Mutex<Vec<tokio::task::JoinHandle<()>>>,
    threads: std::sync::Mutex<Vec<ScriptedThread>>,
    /// Per-thread subscribers, each tagged with the connection's id so an
    /// unsubscribe removes exactly one stream.
    subscribers: std::sync::Mutex<std::collections::HashMap<String, Vec<Stream>>>,
    /// Every live connection, for a server-wide broadcast.
    live: std::sync::Mutex<Vec<Stream>>,
    next_connection: AtomicUsize,
    list_calls: AtomicUsize,
    list_params: std::sync::Mutex<Vec<Value>>,
    resume_calls: AtomicUsize,
    turns_calls: AtomicUsize,
    turns_params: std::sync::Mutex<Vec<Value>>,
    unsubscribe_calls: AtomicUsize,
    /// Answer `thread/resume` without a response, so a test can cancel a
    /// pending adopt.
    silent_resume: std::sync::atomic::AtomicBool,
    /// Fail this many `thread/turns/list` calls before answering normally.
    fail_turns_pages: AtomicUsize,
    /// The page size `thread/list` uses when the caller sends no `limit`.
    page_size: usize,
}

impl Default for State {
    fn default() -> Self {
        Self {
            user_agent: String::new(),
            reject_initialize: None,
            connections: AtomicUsize::new(0),
            initialized: AtomicUsize::new(0),
            liveness: AtomicUsize::new(0),
            tasks: std::sync::Mutex::new(Vec::new()),
            threads: std::sync::Mutex::new(Vec::new()),
            subscribers: std::sync::Mutex::new(std::collections::HashMap::new()),
            live: std::sync::Mutex::new(Vec::new()),
            next_connection: AtomicUsize::new(1),
            list_calls: AtomicUsize::new(0),
            list_params: std::sync::Mutex::new(Vec::new()),
            resume_calls: AtomicUsize::new(0),
            turns_calls: AtomicUsize::new(0),
            turns_params: std::sync::Mutex::new(Vec::new()),
            unsubscribe_calls: AtomicUsize::new(0),
            silent_resume: std::sync::atomic::AtomicBool::new(false),
            fail_turns_pages: AtomicUsize::new(0),
            page_size: 2,
        }
    }
}

pub(crate) struct FakeServer {
    state: Arc<State>,
    /// Set on a server that can be stopped; `None` for the TCP constructors,
    /// which no test kills.
    shutdown: Option<Arc<tokio::sync::Notify>>,
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
        let shutdown = Arc::new(tokio::sync::Notify::new());
        let signal = Arc::clone(&shutdown);
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    _ = signal.notified() => break,
                    accepted = listener.accept() => {
                        let Ok((stream, _)) = accepted else { break };
                        accept_connection(stream, Arc::clone(&serving));
                    }
                }
            }
            // Dropping the listener closes the socket, but the *path* stays on
            // disk: exactly a crashed daemon's stale control socket.
        });
        Self {
            state,
            shutdown: Some(shutdown),
        }
    }

    /// Make the fake daemon die: stop accepting, break every live connection,
    /// and leave the socket path behind. Unix only, matching [`Self::unix`].
    ///
    /// `tokio`'s `UnixListener` does not unlink on drop, so a later dial gets
    /// `ECONNREFUSED` at a path that still exists — the stale-socket shape the
    /// lifecycle must not read as a live daemon.
    #[cfg(unix)]
    pub(crate) async fn die(&self) {
        if let Some(shutdown) = &self.shutdown {
            shutdown.notify_one();
        }
        let tasks: Vec<_> = self
            .state
            .tasks
            .lock()
            .expect("connection tasks")
            .drain(..)
            .collect();
        for task in tasks {
            task.abort();
        }
        // Give the aborted tasks a tick to actually drop their sockets.
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
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
        (
            Self {
                state,
                shutdown: None,
            },
            format!("ws://{address}"),
        )
    }

    /// Add one thread to the stored list. The list is served in insertion
    /// order, which keeps paging assertions deterministic.
    pub(crate) fn seed_thread(&self, thread: ScriptedThread) {
        self.state.threads.lock().expect("scripted threads").push(thread);
    }

    /// Send a notification to every live connection, whether it subscribed or
    /// not (the `thread/started` / `thread/status/changed` shape).
    pub(crate) fn push_notification(&self, method: &str, params: Value) {
        let message = Message::Text(
            json!({ "jsonrpc": "2.0", "method": method, "params": params })
                .to_string()
                .into(),
        );
        self.state
            .live
            .lock()
            .expect("live connections")
            .retain(|(_, connection)| connection.send(message.clone()).is_ok());
    }

    /// Send one notification to the subscribers of `thread_id` only (the
    /// `turn/*` and `item/*` shape). Returns how many streams received it.
    pub(crate) fn push_to_thread(&self, thread_id: &str, method: &str, params: Value) -> usize {
        let message = Message::Text(
            json!({ "jsonrpc": "2.0", "method": method, "params": params })
                .to_string()
                .into(),
        );
        let mut subscribers = self.state.subscribers.lock().expect("subscribers");
        let Some(streams) = subscribers.get_mut(thread_id) else {
            return 0;
        };
        streams.retain(|(_, stream)| stream.send(message.clone()).is_ok());
        streams.len()
    }

    /// Push one completed agent-message item to a thread's subscribers.
    pub(crate) fn push_item(&self, thread_id: &str, item_id: &str, text: &str) -> usize {
        self.push_to_thread(
            thread_id,
            "item/completed",
            json!({
                "threadId": thread_id,
                "turnId": "turn-1",
                "completedAtMs": 1_700_000_001_000_i64,
                "item": { "type": "agentMessage", "id": item_id, "text": text },
            }),
        )
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

    /// `thread/loaded/list` calls received so far (the liveness probe).
    pub(crate) fn liveness_probes(&self) -> usize {
        self.state.liveness.load(Ordering::SeqCst)
    }

    /// Wait until `expected` liveness probes have been answered.
    pub(crate) async fn wait_for_liveness(&self, expected: usize) {
        wait_until(|| self.liveness_probes(), expected, "liveness probes").await;
    }

    /// `thread/list` calls received so far.
    pub(crate) fn list_calls(&self) -> usize {
        self.state.list_calls.load(Ordering::SeqCst)
    }

    /// Every `thread/list` params object, in call order.
    pub(crate) fn list_params(&self) -> Vec<Value> {
        self.state.list_params.lock().expect("list params").clone()
    }

    /// Leave the next `thread/resume` unanswered (the connection stays open),
    /// so a test can cancel the adopt future mid-await. One-shot: the resume
    /// after it answers normally.
    pub(crate) fn silence_next_resume(&self) {
        self.state.silent_resume.store(true, Ordering::SeqCst);
    }

    /// `thread/resume` calls received so far.
    pub(crate) fn resume_calls(&self) -> usize {
        self.state.resume_calls.load(Ordering::SeqCst)
    }

    /// Wait until `expected` `thread/resume` calls have arrived.
    pub(crate) async fn wait_for_resume_calls(&self, expected: usize) {
        wait_until(|| self.resume_calls(), expected, "resume calls").await;
    }

    /// `thread/turns/list` calls received so far.
    pub(crate) fn turns_calls(&self) -> usize {
        self.state.turns_calls.load(Ordering::SeqCst)
    }

    /// Every `thread/turns/list` params object, in call order.
    pub(crate) fn turns_params(&self) -> Vec<Value> {
        self.state.turns_params.lock().expect("turns params").clone()
    }

    /// `thread/unsubscribe` calls received so far.
    pub(crate) fn unsubscribe_calls(&self) -> usize {
        self.state.unsubscribe_calls.load(Ordering::SeqCst)
    }

    /// How many connection streams are currently subscribed to `thread_id`.
    pub(crate) fn subscribers(&self, thread_id: &str) -> usize {
        self.state
            .subscribers
            .lock()
            .expect("subscribers")
            .get(thread_id)
            .map(Vec::len)
            .unwrap_or(0)
    }

    /// Wait until `expected` streams subscribe to `thread_id`.
    pub(crate) async fn wait_for_subscribers(&self, thread_id: &str, expected: usize) {
        wait_until(|| self.subscribers(thread_id), expected, "thread subscribers").await;
    }

    /// The name the server currently stores for `thread_id`.
    pub(crate) fn thread_name(&self, thread_id: &str) -> Option<String> {
        self.state
            .threads
            .lock()
            .expect("scripted threads")
            .iter()
            .find(|thread| thread.id == thread_id)
            .and_then(|thread| thread.name.clone())
    }

    /// Give a never-run thread a rollout, as the user's own client's first turn
    /// would. The next `thread/resume` then succeeds.
    pub(crate) fn mark_history(&self, thread_id: &str) {
        if let Some(thread) = self
            .state
            .threads
            .lock()
            .expect("scripted threads")
            .iter_mut()
            .find(|thread| thread.id == thread_id)
        {
            thread.has_history = true;
        }
    }

    /// Fail the next `count` `thread/turns/list` calls with a malformed page,
    /// so a retry path can be exercised.
    pub(crate) fn fail_turns_pages(&self, count: usize) {
        self.state.fail_turns_pages.store(count, Ordering::SeqCst);
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
    let connection_id = state.next_connection.fetch_add(1, Ordering::SeqCst);
    let (outgoing, incoming) = mpsc::unbounded_channel::<Message>();
    state
        .live
        .lock()
        .expect("live connections")
        .push((connection_id, outgoing.clone()));
    let serving = Arc::clone(&state);
    let cleanup = Arc::clone(&state);
    let handle = tokio::spawn(async move {
        serve(stream, serving, connection_id, outgoing, incoming).await;
        deregister(&cleanup, connection_id);
    });
    // Registered so a test can make the daemon die mid-life (`FakeServer::die`)
    // and break the live connection, not only the listener.
    state.tasks.lock().expect("connection tasks").push(handle);
}

/// Drop every trace of a connection that ended (or died).
fn deregister(state: &State, connection_id: usize) {
    state
        .live
        .lock()
        .expect("live connections")
        .retain(|(id, _)| *id != connection_id);
    for streams in state.subscribers.lock().expect("subscribers").values_mut() {
        streams.retain(|(id, _)| *id != connection_id);
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

async fn serve<S>(
    stream: S,
    state: Arc<State>,
    connection_id: usize,
    outgoing: mpsc::UnboundedSender<Message>,
    mut pushed: mpsc::UnboundedReceiver<Message>,
) where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let Ok(socket) = tokio_tungstenite::accept_async(stream).await else {
        return;
    };
    let (mut sink, mut incoming) = socket.split();
    loop {
        tokio::select! {
            message = incoming.next() => {
                let Some(Ok(message)) = message else { break };
                let Message::Text(text) = message else { continue };
                let Ok(value) = serde_json::from_str::<Value>(&text) else { continue };
                if !handle_call(&state, connection_id, &outgoing, &mut sink, value).await {
                    break;
                }
            }
            message = pushed.recv() => {
                let Some(message) = message else { break };
                if sink.send(message).await.is_err() {
                    break;
                }
            }
        }
    }
}

/// Answer one client call. Returns `false` when the connection should stop.
async fn handle_call<S>(
    state: &Arc<State>,
    connection_id: usize,
    outgoing: &mpsc::UnboundedSender<Message>,
    sink: &mut futures_util::stream::SplitSink<tokio_tungstenite::WebSocketStream<S>, Message>,
    value: Value,
) -> bool
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let id = value.get("id").cloned().unwrap_or(Value::Null);
    let params = value.get("params").cloned().unwrap_or(Value::Null);
    match value.get("method").and_then(Value::as_str) {
        Some("initialize") => {
            if let Some(reject) = &state.reject_initialize {
                return reply_error(sink, id, -32600, reject).await;
            }
            // A broadcast that races the response: the client's correlation
            // loop must skip it, never read it as the reply.
            let broadcast = json!({
                "method": "remoteControl/status/changed",
                "params": { "status": "disabled" },
            });
            if send(sink, broadcast).await.is_err() {
                return false;
            }
            let result = json!({
                "userAgent": state.user_agent,
                "codexHome": "/tmp/codex-home",
                "platformFamily": "unix",
                "platformOs": "linux",
            });
            reply(sink, id, result).await
        }
        Some("initialized") => {
            state.initialized.fetch_add(1, Ordering::SeqCst);
            true
        }
        Some("thread/loaded/list") => {
            state.liveness.fetch_add(1, Ordering::SeqCst);
            reply(sink, id, json!({ "data": [] })).await
        }
        Some("thread/list") => {
            state.list_calls.fetch_add(1, Ordering::SeqCst);
            state
                .list_params
                .lock()
                .expect("list params")
                .push(params.clone());
            replying(sink, id, list_threads(state, &params)).await
        }
        Some("thread/read") => {
            let thread_id = params.get("threadId").and_then(Value::as_str).unwrap_or("");
            match find_thread(state, thread_id) {
                Some(thread) => replying(sink, id, json!({ "thread": thread_json(&thread) })).await,
                None => reply_error(sink, id, -32600, &format!("thread not found: {thread_id}")).await,
            }
        }
        Some("thread/turns/list") => {
            state.turns_calls.fetch_add(1, Ordering::SeqCst);
            state
                .turns_params
                .lock()
                .expect("turns params")
                .push(params.clone());
            let thread_id = params.get("threadId").and_then(Value::as_str).unwrap_or("");
            let Some(thread) = find_thread(state, thread_id) else {
                return reply_error(sink, id, -32600, &format!("thread not found: {thread_id}")).await;
            };
            if thread.malformed_turns_page {
                return replying(sink, id, json!({ "nextCursor": null, "backwardsCursor": null })).await;
            }
            let remaining = state.fail_turns_pages.load(Ordering::SeqCst);
            if remaining > 0 {
                state.fail_turns_pages.fetch_sub(1, Ordering::SeqCst);
                return replying(sink, id, json!({ "nextCursor": null, "backwardsCursor": null })).await;
            }
            let offset = params
                .get("cursor")
                .and_then(Value::as_str)
                .and_then(|cursor| cursor.parse::<usize>().ok())
                .unwrap_or(0)
                .min(thread.turns.len());
            let limit = params
                .get("limit")
                .and_then(Value::as_u64)
                .map(|limit| limit as usize)
                .filter(|limit| *limit > 0)
                .unwrap_or(2);
            let end = (offset + limit).min(thread.turns.len());
            let next = if thread.repeat_turns_cursor {
                Some(json!("0"))
            } else {
                (end < thread.turns.len()).then(|| json!(end.to_string()))
            };
            replying(
                sink,
                id,
                json!({
                    "data": thread.turns[offset..end],
                    "nextCursor": next,
                    "backwardsCursor": null,
                }),
            )
            .await
        }
        Some("test/malformed") => {
            // A broken peer: a text frame that is not JSON. The client must
            // close the connection and fail the request instead of waiting.
            return send_text(sink, "this frame is not json").await;
        }
        Some("thread/resume") => {
            state.resume_calls.fetch_add(1, Ordering::SeqCst);
            if state.silent_resume.swap(false, Ordering::SeqCst) {
                return true;
            }
            let thread_id = params.get("threadId").and_then(Value::as_str).unwrap_or("");
            let Some(thread) = find_thread(state, thread_id) else {
                return reply_error(sink, id, -32600, &format!("thread not found: {thread_id}")).await;
            };
            if !thread.has_history {
                // The spike's exact shape (ADR-0090 amendment, question 2).
                return reply_error(
                    sink,
                    id,
                    -32600,
                    &format!("no rollout found for thread id {thread_id}"),
                )
                .await;
            }
            {
                let mut subscribers = state.subscribers.lock().expect("subscribers");
                let streams = subscribers.entry(thread_id.to_string()).or_default();
                // Resuming twice on one connection is the same subscription,
                // not a second stream (the spike re-resumed successfully).
                if !streams.iter().any(|(id, _)| *id == connection_id) {
                    streams.push((connection_id, outgoing.clone()));
                }
            }
            let result = json!({
                "approvalPolicy": "never",
                "approvalsReviewer": "user",
                "cwd": thread.cwd,
                "model": "gpt-5",
                "modelProvider": "openai",
                "sandbox": { "type": "readOnly", "networkAccess": false },
                "thread": thread_json(&thread),
            });
            reply(sink, id, result).await
        }
        Some("thread/unsubscribe") => {
            state.unsubscribe_calls.fetch_add(1, Ordering::SeqCst);
            let thread_id = params.get("threadId").and_then(Value::as_str).unwrap_or("");
            let status = {
                let mut subscribers = state.subscribers.lock().expect("subscribers");
                let streams = subscribers.entry(thread_id.to_string()).or_default();
                let before = streams.len();
                // Only this connection leaves; another subscriber's stream is
                // untouched (the spike's question 2).
                streams.retain(|(id, _)| *id != connection_id);
                if streams.len() < before {
                    "unsubscribed"
                } else {
                    "notSubscribed"
                }
            };
            reply(sink, id, json!({ "status": status })).await
        }
        Some("thread/name/set") => {
            let thread_id = params.get("threadId").and_then(Value::as_str).unwrap_or("");
            let name = params.get("name").and_then(Value::as_str).unwrap_or("");
            let renamed = {
                let mut threads = state.threads.lock().expect("scripted threads");
                match threads.iter_mut().find(|thread| thread.id == thread_id) {
                    Some(thread) => {
                        thread.name = Some(name.to_string());
                        true
                    }
                    None => false,
                }
            };
            if renamed {
                reply(sink, id, json!({})).await
            } else {
                reply_error(sink, id, -32600, &format!("thread not found: {thread_id}")).await
            }
        }
        _ => true,
    }
}

fn find_thread(state: &State, thread_id: &str) -> Option<ScriptedThread> {
    state
        .threads
        .lock()
        .expect("scripted threads")
        .iter()
        .find(|thread| thread.id == thread_id)
        .cloned()
}

/// The page `thread/list` answers: cwd/source filters, an opaque cursor as an
/// offset, and `nextCursor` only while another page remains.
fn list_threads(state: &State, params: &Value) -> Value {
    let cwd = params.get("cwd");
    let sources = params
        .get("sourceKinds")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let matching: Vec<ScriptedThread> = state
        .threads
        .lock()
        .expect("scripted threads")
        .iter()
        .filter(|thread| match cwd {
            Some(Value::String(path)) => thread.cwd == *path,
            Some(Value::Array(paths)) => paths
                .iter()
                .filter_map(Value::as_str)
                .any(|path| thread.cwd == path),
            _ => true,
        })
        .filter(|thread| {
            sources.is_empty()
                || sources
                    .iter()
                    .filter_map(Value::as_str)
                    .any(|source| thread.source == source)
        })
        .cloned()
        .collect();

    let offset = params
        .get("cursor")
        .and_then(Value::as_str)
        .and_then(|cursor| cursor.parse::<usize>().ok())
        .unwrap_or(0)
        .min(matching.len());
    let limit = params
        .get("limit")
        .and_then(Value::as_u64)
        .map(|limit| limit as usize)
        .filter(|limit| *limit > 0)
        .unwrap_or(state.page_size);
    let end = (offset + limit).min(matching.len());
    let data: Vec<Value> = matching[offset..end].iter().map(thread_json).collect();
    let next = (end < matching.len()).then(|| json!(end.to_string()));
    json!({ "data": data, "nextCursor": next })
}

/// One thread in the wire shape `0.163.0` defines. The full required set is
/// present so the client decodes the real contract, not a relaxed fixture.
fn thread_json(thread: &ScriptedThread) -> Value {
    let turns = match thread.read_turn_limit {
        Some(limit) => &thread.turns[..thread.turns.len().min(limit)],
        None => &thread.turns[..],
    };
    json!({
        "id": thread.id,
        "sessionId": thread.session_id,
        "name": thread.name,
        "parentThreadId": thread.parent_thread_id,
        "preview": thread.preview,
        "cwd": thread.cwd,
        "createdAt": thread.created_at,
        "updatedAt": thread.updated_at,
        "cliVersion": "0.163.0",
        "modelProvider": "openai",
        "ephemeral": false,
        "projectId": null,
        "source": thread.source,
        "status": { "type": "notLoaded" },
        "turns": turns,
    })
}

async fn send_text<S>(
    sink: &mut futures_util::stream::SplitSink<tokio_tungstenite::WebSocketStream<S>, Message>,
    text: &str,
) -> bool
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    sink.send(Message::Text(text.to_string().into())).await.is_ok()
}

async fn send<S>(
    sink: &mut futures_util::stream::SplitSink<tokio_tungstenite::WebSocketStream<S>, Message>,
    value: Value,
) -> std::result::Result<(), ()>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    sink.send(Message::Text(value.to_string().into()))
        .await
        .map_err(|_| ())
}

async fn replying<S>(
    sink: &mut futures_util::stream::SplitSink<tokio_tungstenite::WebSocketStream<S>, Message>,
    id: Value,
    result: Value,
) -> bool
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    reply(sink, id, result).await
}

async fn reply<S>(
    sink: &mut futures_util::stream::SplitSink<tokio_tungstenite::WebSocketStream<S>, Message>,
    id: Value,
    result: Value,
) -> bool
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    send(sink, json!({ "jsonrpc": "2.0", "id": id, "result": result }))
        .await
        .is_ok()
}

async fn reply_error<S>(
    sink: &mut futures_util::stream::SplitSink<tokio_tungstenite::WebSocketStream<S>, Message>,
    id: Value,
    code: i64,
    message: &str,
) -> bool
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    send(
        sink,
        json!({
            "jsonrpc": "2.0",
            "id": id,
            "error": { "code": code, "message": message },
        }),
    )
    .await
    .is_ok()
}
